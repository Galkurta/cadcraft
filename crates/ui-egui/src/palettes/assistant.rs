//! The Assistant palette: a chat with a model on one OpenAI-compatible endpoint (`cadcraft-ai`).
//! The conversation runs on a worker thread, so the window never waits for the network; the tools
//! the model calls are sent back here and run on the UI thread through the control dispatcher
//! ([`crate::control::handle`]), exactly like a request from the control channel or MCP.
//!
//! Image markups (MARKUPIMPORT on a PNG/JPG) are read by the model on a worker thread the same way.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use cadcraft_ai::{Agent, Backend, Config, Event};
use egui::{RichText, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::theme::Tokens;
use crate::{CadApp, ControlRequest};

/// Transcript lines kept.
const MAX_LINES: usize = 500;
/// How long a tool call may wait for the UI thread.
const TOOL_WAIT: Duration = Duration::from_secs(120);
/// The first thing the status line says while an image is read.
const READING: &str = "Reading the markups with the AI model";

/// The endpoint settings. An empty field falls back to the environment
/// (`CADCRAFT_AI_BASE_URL`, `CADCRAFT_AI_MODEL`, `CADCRAFT_AI_KEY` / `OPENAI_API_KEY`). Saved with
/// the app's preferences (`prefs_json`, key `assistant`), never in a drawing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl Settings {
    /// The environment's endpoint with this window's non-empty fields on top.
    pub fn config(&self) -> Config {
        let mut c = Config::from_env();
        for (field, value) in [(&mut c.base_url, &self.base_url), (&mut c.model, &self.model), (&mut c.api_key, &self.api_key)] {
            if !value.trim().is_empty() {
                *field = value.trim().to_string();
            }
        }
        c
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    User,
    Assistant,
    Tool,
    Error,
}

enum Msg {
    Event(Event),
    /// The turn is over; the conversation comes back.
    Done(Agent),
}

/// A turn running on its worker thread.
struct Run {
    events: Receiver<Msg>,
    /// Tool calls waiting for the UI thread.
    tools: Receiver<ControlRequest>,
    cancel: Arc<AtomicBool>,
}

/// An image being read for MARKUPIMPORT.
struct MarkupJob {
    path: String,
    done: Receiver<Result<Vec<Value>, String>>,
}

#[derive(Default)]
pub struct State {
    pub settings: Settings,
    lines: Vec<(Kind, String)>,
    input: String,
    /// The conversation between turns (`None` before the first one and after Clear).
    agent: Option<Agent>,
    run: Option<Run>,
    markup: Option<MarkupJob>,
    /// Lets worker threads wake the UI; set every frame by [`pump`].
    ctx: Option<egui::Context>,
}

impl State {
    fn note(&mut self, kind: Kind, text: impl Into<String>) {
        self.lines.push((kind, text.into()));
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..self.lines.len() - MAX_LINES);
        }
    }

    pub fn running(&self) -> bool {
        self.run.is_some()
    }
}

/// What the worker thread calls tools through: each call is a control-channel request the UI
/// thread answers.
struct UiBackend {
    tools: Sender<ControlRequest>,
    ctx: egui::Context,
}

impl Backend for UiBackend {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let (req, pending) = ControlRequest::new(method, params);
        self.tools.send(req).map_err(|_| "the window is gone".to_string())?;
        self.ctx.request_repaint();
        let r = pending.wait(TOOL_WAIT);
        if r.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(r.get("result").cloned().unwrap_or(Value::Null));
        }
        let error = r.get("error").and_then(Value::as_str).unwrap_or("failed");
        Err(match r.get("state").and_then(Value::as_str) {
            Some(state) => format!("{error} ({state})"),
            None => error.to_string(),
        })
    }
    /// The Assistant has no screenshots (the window is not an image the model can ask for); it has `render`.
    fn has_ui(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        "the CADCraft window".into()
    }
}

/// Open the palette. The first time it floats by the right edge of the drawing; afterwards it
/// returns where it was closed.
pub fn open(app: &mut CadApp) -> Result<Value, String> {
    crate::docking::open_floating(app, crate::docking::Panel::Assistant, 340.0)
}

/// `assistantopen`, `assistantclose` and the UI side of `markupimport`. `None` when `id` is not one.
pub fn route(app: &mut CadApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    match id {
        "assistantopen" => Some(open(app)),
        "assistantclose" => Some(crate::docking::command(app, false, &json!({"operation": "close", "panel": crate::docking::Panel::Assistant.id()}))),
        // Typed or from a menu (no parameters): choose the file. JSON calls go to the command.
        "markupimport" if params.is_null() && cfg!(not(target_arch = "wasm32")) => {
            let pick = app.services.pick_open.as_ref()?;
            let Some(path) = pick() else { return Some(Ok(Value::Null)) };
            Some(import_markup(app, path))
        }
        _ => None,
    }
}

fn is_image(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg"].iter().any(|e| p.ends_with(e))
}

/// MARKUPIMPORT of a file the user chose: a PDF at once; an image after the model has read it.
fn import_markup(app: &mut CadApp, path: String) -> Result<Value, String> {
    if !is_image(&path) {
        return app.run("markupimport", json!({ "path": path }));
    }
    if app.assistant.markup.is_some() {
        return Err("a markup image is already being read".into());
    }
    let ctx = app.assistant.ctx.clone();
    let cfg = app.assistant.settings.config();
    let (tx, done) = channel();
    let file = path.clone();
    std::thread::Builder::new()
        .name("markup-recognition".into())
        .spawn(move || {
            let result = read_image(&cfg, &file);
            let _ = tx.send(result);
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        })
        .map_err(|e| e.to_string())?;
    app.assistant.markup = Some(MarkupJob { path, done });
    app.set_status(format!("{}…", crate::i18n::t(READING)));
    Ok(json!({"reading": true}))
}

fn read_image(cfg: &Config, path: &str) -> Result<Vec<Value>, String> {
    let mime = if path.to_ascii_lowercase().ends_with(".png") { "image/png" } else { "image/jpeg" };
    let len = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?.len();
    if len > 8 << 20 {
        return Err(format!("{path}: larger than 8 MB"));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    cadcraft_ai::recognize_image_markups(cfg, &bytes, mime).map_err(|e| e.to_string())
}

/// Every frame: remember the context, answer the worker's tool calls, collect its events and the
/// markup recognition that finished.
pub fn pump(app: &mut CadApp, ctx: &egui::Context) {
    app.assistant.ctx = Some(ctx.clone());
    if let Some(run) = app.assistant.run.take() {
        while let Ok(req) = run.tools.try_recv() {
            // Timed out while queued: the worker was told it did not run.
            if !req.begin() {
                continue;
            }
            let reply = req.reply.clone();
            let _ = match crate::control::handle(app, ctx, &req) {
                crate::control::Outcome::Done(v) => reply.send(v),
                crate::control::Outcome::Screenshot { .. } => reply.send(json!({"ok": false, "error": "no screenshots here; use render"})),
            };
        }
        let mut finished = false;
        loop {
            match run.events.try_recv() {
                Ok(Msg::Event(e)) => {
                    let (kind, text) = match e {
                        Event::Text(t) => (Kind::Assistant, t),
                        Event::ToolCall { name, args } => (Kind::Tool, format!("{name} {args}")),
                        Event::ToolResult { name, summary, error } => (if error { Kind::Error } else { Kind::Tool }, format!("{name}: {summary}")),
                        Event::Error(e) => (Kind::Error, e),
                    };
                    app.assistant.note(kind, text);
                }
                Ok(Msg::Done(agent)) => {
                    app.assistant.agent = Some(agent);
                    finished = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // The worker died without handing the conversation back.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    app.assistant.note(Kind::Error, "the Assistant stopped unexpectedly");
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            ctx.request_repaint();
        } else {
            app.assistant.run = Some(run);
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
    if let Some(job) = app.assistant.markup.take() {
        match job.done.try_recv() {
            Ok(Ok(markups)) => {
                let r = app.run("markupimport", json!({"path": job.path, "markups": markups}));
                match r {
                    Ok(v) => {
                        let n = v.get("count").and_then(Value::as_u64).unwrap_or(0);
                        app.set_status(format!("MARKUPIMPORT: {n} markups imported"));
                    }
                    Err(e) => app.set_status(e),
                }
            }
            Ok(Err(e)) => {
                app.session.echo(e.clone());
                app.set_status(e);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // Keep the status line up while the model reads.
                app.set_status(format!("{}…", crate::i18n::t(READING)));
                app.assistant.markup = Some(job);
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => app.set_status("MARKUPIMPORT: reading the image failed"),
        }
    }
}

/// Start a turn with `text`.
fn send(app: &mut CadApp, text: String) {
    let st = &mut app.assistant;
    let Some(ctx) = st.ctx.clone() else { return };
    let cfg = st.settings.config();
    let mut agent = st.agent.take().unwrap_or_else(|| Agent::new(cfg.clone()));
    agent.config = cfg;
    st.note(Kind::User, text.clone());
    let (events, events_rx) = channel();
    let (tools, tools_rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let spawned = std::thread::Builder::new().name("assistant".into()).spawn(move || {
        let mut backend = UiBackend { tools, ctx: ctx.clone() };
        let sink = events.clone();
        let wake = ctx.clone();
        agent.send(&mut backend, &text, &stop, &mut |e| {
            let _ = sink.send(Msg::Event(e.clone()));
            wake.request_repaint();
        });
        let _ = events.send(Msg::Done(agent));
        ctx.request_repaint();
    });
    match spawned {
        Ok(_) => st.run = Some(Run { events: events_rx, tools: tools_rx, cancel }),
        Err(e) => st.note(Kind::Error, e.to_string()),
    }
}

pub fn show(app: &mut CadApp, ui: &mut egui::Ui) {
    let t = Tokens::get();
    if cfg!(target_arch = "wasm32") {
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.add_space(6.0);
            ui.label(RichText::new(crate::tl!("The Assistant is not available yet in the web build.")).color(t.text_dim));
        });
        return;
    }
    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
    let running = app.assistant.running();
    let mut action = None;
    // The input and its buttons stay at the bottom; the transcript takes the rest.
    egui::Panel::bottom("assistant-input").frame(egui::Frame::new().inner_margin(6.0)).show(ui, |ui| {
        egui::CollapsingHeader::new(crate::tl!("Settings")).id_salt("assistant-settings").default_open(false).show(ui, |ui| {
            let env = Config::from_env();
            let s = &mut app.assistant.settings;
            egui::Grid::new("assistant-settings-grid").num_columns(2).spacing(vec2(6.0, 4.0)).show(ui, |ui| {
                ui.label(crate::tl!("Base URL"));
                ui.add(egui::TextEdit::singleline(&mut s.base_url).hint_text(&env.base_url).desired_width(f32::INFINITY));
                ui.end_row();
                ui.label(crate::tl!("Model"));
                ui.add(egui::TextEdit::singleline(&mut s.model).hint_text(&env.model).desired_width(f32::INFINITY));
                ui.end_row();
                ui.label(crate::tl!("API key"));
                ui.add(egui::TextEdit::singleline(&mut s.api_key).password(true).hint_text(if env.api_key.is_empty() { "" } else { "••••" }).desired_width(f32::INFINITY));
                ui.end_row();
            });
            if ui.button(crate::tl!("Use environment")).clicked() {
                *s = Settings::default();
            }
            ui.label(
                RichText::new(crate::tl!("Empty fields use CADCRAFT_AI_BASE_URL, CADCRAFT_AI_MODEL and CADCRAFT_AI_KEY (or OPENAI_API_KEY). Any OpenAI-compatible endpoint works: Ollama, LM Studio, OpenAI, OpenRouter, vLLM. Saved with your preferences on this computer, never in a drawing."))
                    .color(t.text_faint)
                    .font(crate::theme::small()),
            );
        });
        let st = &mut app.assistant;
        let field = egui::TextEdit::singleline(&mut st.input)
            .hint_text(crate::tl!("Ask the Assistant to draw or change something"))
            .desired_width(f32::INFINITY);
        let r = ui.add_enabled(!running, field);
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.horizontal(|ui| {
            let can_send = !running && !st.input.trim().is_empty();
            if ui.add_enabled(can_send, egui::Button::new(crate::tl!("Send"))).clicked() || (enter && can_send) {
                action = Some(true);
                r.request_focus();
            }
            if ui.add_enabled(running, egui::Button::new(crate::tl!("Stop"))).clicked() {
                if let Some(run) = &st.run {
                    run.cancel.store(true, Ordering::Relaxed);
                }
                st.note(Kind::Tool, crate::tl!("Stopping after the current request…"));
            }
            if ui.add_enabled(!running, egui::Button::new(crate::tl!("Clear"))).clicked() {
                st.lines.clear();
                st.agent = None;
            }
            if running {
                ui.label(RichText::new(crate::tl!("Working…")).color(t.text_dim));
            }
        });
    });
    egui::ScrollArea::vertical().id_salt("dock-assistant").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        for (kind, text) in &app.assistant.lines {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(6.0);
                let (color, font) = match kind {
                    Kind::User => (t.accent, crate::theme::body()),
                    Kind::Assistant => (t.text, crate::theme::body()),
                    Kind::Tool => (t.text_dim, crate::theme::small()),
                    Kind::Error => (t.warn, crate::theme::small()),
                };
                ui.add(egui::Label::new(RichText::new(text).color(color).font(font)).wrap());
            });
        }
    });
    if action.is_some() {
        let text = std::mem::take(&mut app.assistant.input);
        send(app, text.trim().to_string());
    }
}
