//! The Assistant palette: a chat with a model on one OpenAI-compatible endpoint (`cadcraft-ai`).
//! The palette drives the conversation as a state machine ([`cadcraft_ai::Driver`]): every frame
//! it collects what the request in flight has streamed in, runs the tool calls the model asked for
//! on the UI thread through the control dispatcher ([`crate::control::handle`], exactly like a
//! request from the control channel or MCP) and starts the next request. The request itself runs
//! on a worker thread (native) or the browser's `fetch` (web), so the window never waits.
//!
//! Image markups (MARKUPIMPORT on a PNG/JPG) are read by the model on a worker thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use cadcraft_ai::keystore::ApiKey;
use cadcraft_ai::{Agent, Backend, Config, Driver, Event, Vision};
use egui::{RichText, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::theme::Tokens;
use crate::{CadApp, ControlRequest};

/// Transcript lines kept.
const MAX_LINES: usize = 500;
/// A message longer than this many characters is cut short until "Show more".
const SHOW_CHARS: usize = 600;
/// The one-line summary of a tool call is cut to this many characters.
const BRIEF_CHARS: usize = 90;
/// The first thing the status line says while an image is read.
const READING: &str = "Reading the markups with the AI model";

/// The endpoint settings. An empty field falls back to the environment
/// (`CADCRAFT_AI_BASE_URL`, `CADCRAFT_AI_MODEL`, `CADCRAFT_AI_KEY` / `OPENAI_API_KEY`,
/// `CADCRAFT_AI_VISION`). Saved with the app's preferences (`prefs_json`, key `assistant`), never
/// in a drawing. The API key is the exception: it lives in the OS credential store and reaches the
/// preferences file only when there is none; `api_key` here is the key being edited, and is read
/// from older preferences files to move it into the store.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub base_url: String,
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// `Auto` here means "the environment's choice, else Auto".
    pub vision: Vision,
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
        if self.vision != Vision::Auto {
            c.vision = self.vision;
        }
        c
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    User,
    Assistant,
    /// A tool call: `Line::tool` is set.
    Tool,
    Note,
    Error,
}

/// A tool call and, once it ran, its answer.
struct ToolLine {
    name: String,
    args: String,
    result: Option<String>,
    error: bool,
}

struct Line {
    /// Keeps a tool call's open/closed state when older lines scroll out.
    id: u64,
    kind: Kind,
    text: String,
    tool: Option<ToolLine>,
    /// A long text shown in full.
    expanded: bool,
}

/// A turn running: polled every frame by [`pump`].
struct Run {
    driver: Driver,
    cancel: Arc<AtomicBool>,
}

/// An image being read for MARKUPIMPORT.
struct MarkupJob {
    path: String,
    done: Receiver<Result<Vec<Value>, String>>,
}

pub struct State {
    pub settings: Settings,
    /// Where the API key lives. Tests put a fake store here.
    pub keys: ApiKey,
    lines: Vec<Line>,
    next_id: u64,
    /// The last line is the answer being streamed: more text continues it.
    open_text: bool,
    input: String,
    /// The conversation between turns (`None` before the first one and after Clear).
    agent: Option<Agent>,
    run: Option<Run>,
    /// The endpoint refused images this session (survives Clear).
    no_vision: bool,
    markup: Option<MarkupJob>,
    /// Lets worker threads wake the UI; set every frame by [`pump`].
    ctx: Option<egui::Context>,
}

impl Default for State {
    fn default() -> Self {
        State {
            settings: Settings::default(),
            keys: ApiKey::os(),
            lines: Vec::new(),
            next_id: 0,
            open_text: false,
            input: String::new(),
            agent: None,
            run: None,
            no_vision: false,
            markup: None,
            ctx: None,
        }
    }
}

impl State {
    fn push(&mut self, kind: Kind, text: impl Into<String>, tool: Option<ToolLine>) {
        self.next_id += 1;
        self.open_text = false;
        self.lines.push(Line { id: self.next_id, kind, text: text.into(), tool, expanded: false });
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..self.lines.len() - MAX_LINES);
        }
    }

    fn note(&mut self, kind: Kind, text: impl Into<String>) {
        self.push(kind, text, None);
    }

    /// One thing the driver reported.
    fn event(&mut self, e: Event) {
        match e {
            Event::Text(t) => match self.lines.last_mut() {
                Some(l) if self.open_text && l.kind == Kind::Assistant => l.text.push_str(&t),
                _ => {
                    self.push(Kind::Assistant, t, None);
                    self.open_text = true;
                }
            },
            Event::ToolCall { name, args } => self.push(Kind::Tool, "", Some(ToolLine { name, args, result: None, error: false })),
            Event::ToolResult { name, summary, error } => {
                self.open_text = false;
                let waiting = self.lines.iter_mut().rev().filter_map(|l| l.tool.as_mut()).find(|t| t.result.is_none() && t.name == name);
                match waiting {
                    Some(t) => (t.result, t.error) = (Some(summary), error),
                    None => self.push(Kind::Tool, "", Some(ToolLine { name, args: String::new(), result: Some(summary), error })),
                }
            }
            Event::Note(t) => self.note(Kind::Note, t),
            Event::Error(t) => self.note(Kind::Error, t),
        }
    }

    pub fn running(&self) -> bool {
        self.run.is_some()
    }

    /// Settings loaded from the preferences: a plain-text key in them moves into the credential store.
    pub fn load_settings(&mut self, settings: Settings) {
        self.settings = settings;
        self.keys.adopt(&self.settings.api_key);
    }

    /// What the preferences file keeps of the settings: the key only when there is no credential store.
    pub fn prefs_value(&self) -> Option<Value> {
        let mut s = self.settings.clone();
        s.api_key = self.keys.for_prefs(&s.api_key).to_string();
        (s != Settings::default()).then(|| json!(s))
    }

    /// Bring the credential store and the edited key together: first the stored key, then every change.
    fn sync_key(&mut self) {
        if let Some(stored) = self.keys.load().filter(|_| self.settings.api_key.is_empty()) {
            self.settings.api_key = stored;
        }
        self.keys.save(&self.settings.api_key);
    }

    /// The endpoint to talk to, with the key as it is now.
    fn config(&mut self) -> Config {
        self.sync_key();
        self.settings.config()
    }
}

/// What the agent calls tools through: each call is a control-channel request the app answers
/// right here, on the UI thread.
struct AppBackend<'a> {
    app: &'a mut CadApp,
    ctx: &'a egui::Context,
}

impl Backend for AppBackend<'_> {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let (req, _pending) = ControlRequest::new(method, params);
        let r = match crate::control::handle(self.app, self.ctx, &req) {
            crate::control::Outcome::Done(v) => v,
            crate::control::Outcome::Screenshot { .. } => return Err("no screenshots here; use render".into()),
        };
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
    let cfg = app.assistant.config();
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

/// Every frame: remember the context, advance the turn (collect the reply, run its tool calls,
/// start the next request) and collect the markup recognition that finished.
pub fn pump(app: &mut CadApp, ctx: &egui::Context) {
    app.assistant.ctx = Some(ctx.clone());
    if let Some(mut run) = app.assistant.run.take() {
        let alive = run.driver.poll(&mut AppBackend { app: &mut *app, ctx });
        for e in run.driver.take_events() {
            app.assistant.event(e);
        }
        if alive {
            app.assistant.run = Some(run);
            // Data arriving wakes the window; this is only the safety net.
            ctx.request_repaint_after(Duration::from_millis(250));
        } else {
            let agent = run.driver.into_agent();
            app.assistant.no_vision |= agent.no_vision;
            app.assistant.agent = Some(agent);
            ctx.request_repaint();
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
    let cfg = st.config();
    let mut agent = st.agent.take().unwrap_or_else(|| Agent::new(cfg.clone()));
    agent.config = cfg;
    agent.no_vision |= st.no_vision;
    st.note(Kind::User, text.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let wake = ctx.clone();
    let driver = Driver::start(agent, &text, false, cancel.clone(), Arc::new(move || wake.request_repaint()));
    st.run = Some(Run { driver, cancel });
    ctx.request_repaint();
}

/// `text` cut to [`SHOW_CHARS`] characters: the shown part, and whether something was cut.
fn cut(text: &str) -> (&str, bool) {
    match text.char_indices().nth(SHOW_CHARS) {
        Some((i, _)) => (text.get(..i).unwrap_or(text), true),
        None => (text, false),
    }
}

/// `command_line circle 0,0 5 — ok`: the tool, what it was asked (a command line as typed, other
/// arguments as compact JSON) and how it went, on one line.
fn tool_header(t: &ToolLine) -> String {
    let parsed = serde_json::from_str::<Value>(&t.args).ok();
    let asked = match (&parsed, t.name.as_str()) {
        (Some(v), "command_line") if v["text"].is_string() => v["text"].as_str().unwrap_or_default().to_string(),
        (Some(Value::Object(m)), _) if m.is_empty() => String::new(),
        (Some(v), _) => v.to_string(),
        (None, _) => t.args.clone(),
    };
    let asked: String = asked.split_whitespace().collect::<Vec<_>>().join(" ");
    let asked = match asked.char_indices().nth(BRIEF_CHARS) {
        Some((i, _)) => format!("{}…", asked.get(..i).unwrap_or(&asked)),
        None => asked,
    };
    let status = match (&t.result, t.error) {
        (None, _) => "…".to_string(),
        (Some(_), false) => "ok".to_string(),
        (Some(r), true) => {
            let first = r.lines().next().unwrap_or_default();
            let short: String = first.chars().take(BRIEF_CHARS).collect();
            format!("error: {short}{}", if first.chars().nth(BRIEF_CHARS).is_some() { "…" } else { "" })
        }
    };
    format!("{} {asked} — {status}", t.name).replace("  ", " ")
}

/// The full arguments (pretty-printed when they are JSON) and answer of a tool call.
fn tool_body(t: &ToolLine) -> String {
    let args = serde_json::from_str::<Value>(&t.args).and_then(|v| serde_json::to_string_pretty(&v)).unwrap_or_else(|_| t.args.clone());
    match &t.result {
        Some(r) => format!("{args}\n→ {r}"),
        None => args,
    }
}

fn show_line(ui: &mut egui::Ui, t: &Tokens, line: &mut Line) {
    if let Some(tool) = &line.tool {
        let color = if tool.error { t.warn } else { t.text_dim };
        egui::CollapsingHeader::new(RichText::new(tool_header(tool)).color(color).font(crate::theme::small()))
            .id_salt(("assistant-tool", line.id))
            .default_open(false)
            .show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(tool_body(tool)).color(t.text_dim).font(crate::theme::mono())).wrap().selectable(true));
            });
        return;
    }
    let (color, font) = match line.kind {
        Kind::User => (t.accent, crate::theme::body()),
        Kind::Assistant => (t.text, crate::theme::body()),
        Kind::Note | Kind::Tool => (t.text_dim, crate::theme::small()),
        Kind::Error => (t.warn, crate::theme::small()),
    };
    let (head, cut_off) = if line.expanded { (line.text.as_str(), false) } else { cut(&line.text) };
    ui.horizontal_wrapped(|ui| {
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(head).color(color).font(font.clone())).wrap());
    });
    if cut_off || line.expanded && cut(&line.text).1 {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            let label = if line.expanded { crate::tl!("Show less") } else { crate::tl!("Show more") };
            if ui.small_button(label).clicked() {
                line.expanded = !line.expanded;
            }
        });
    }
}

fn vision_label(v: Vision) -> &'static str {
    match v {
        Vision::Auto => crate::tl!("Auto"),
        Vision::On => crate::tl!("On"),
        Vision::Off => crate::tl!("Off"),
    }
}

pub fn show(app: &mut CadApp, ui: &mut egui::Ui) {
    let t = Tokens::get();
    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
    let running = app.assistant.running();
    let mut action = None;
    // The input and its buttons stay at the bottom; the transcript takes the rest.
    egui::Panel::bottom("assistant-input").frame(egui::Frame::new().inner_margin(6.0)).show(ui, |ui| {
        egui::CollapsingHeader::new(crate::tl!("Settings")).id_salt("assistant-settings").default_open(false).show(ui, |ui| {
            let env = Config::from_env();
            let st = &mut app.assistant;
            let mut key_focused = false;
            egui::Grid::new("assistant-settings-grid").num_columns(2).spacing(vec2(6.0, 4.0)).show(ui, |ui| {
                let s = &mut st.settings;
                ui.label(crate::tl!("Base URL"));
                ui.add(egui::TextEdit::singleline(&mut s.base_url).hint_text(&env.base_url).desired_width(f32::INFINITY));
                ui.end_row();
                ui.label(crate::tl!("Model"));
                ui.add(egui::TextEdit::singleline(&mut s.model).hint_text(&env.model).desired_width(f32::INFINITY));
                ui.end_row();
                ui.label(crate::tl!("API key"));
                let key = egui::TextEdit::singleline(&mut s.api_key).password(true).hint_text(if env.api_key.is_empty() { "" } else { "••••" }).desired_width(f32::INFINITY);
                key_focused = ui.add(key).has_focus();
                ui.end_row();
                ui.label(crate::tl!("Image input"));
                egui::ComboBox::from_id_salt("assistant-vision").selected_text(vision_label(s.vision)).show_ui(ui, |ui| {
                    for v in [Vision::Auto, Vision::On, Vision::Off] {
                        ui.selectable_value(&mut s.vision, v, vision_label(v));
                    }
                });
                ui.end_row();
            });
            if ui.button(crate::tl!("Use environment")).clicked() {
                st.settings = Settings::default();
            }
            // The key goes to the credential store once the field is left, not on every keystroke.
            if !key_focused {
                st.sync_key();
            }
            if let Some(reason) = st.keys.warning().filter(|_| cfg!(not(target_arch = "wasm32"))) {
                ui.label(RichText::new(crate::tf!("The credential store is unavailable ({reason}); the key is kept in your preferences file.", reason = reason)).color(t.warn).font(crate::theme::small()));
            }
            ui.label(
                RichText::new(crate::tl!("Empty fields use CADCRAFT_AI_BASE_URL, CADCRAFT_AI_MODEL, CADCRAFT_AI_KEY (or OPENAI_API_KEY) and CADCRAFT_AI_VISION. Any OpenAI-compatible endpoint works: Ollama, LM Studio, OpenAI, OpenRouter, vLLM. The API key is kept in your system's credential store; the rest is saved with your preferences on this computer, never in a drawing."))
                    .color(t.text_faint)
                    .font(crate::theme::small()),
            );
            ui.label(
                RichText::new(crate::tl!("Image input: Auto sends images and stops sending them if the model refuses; Off never sends them (marked-up images can't be read then)."))
                    .color(t.text_faint)
                    .font(crate::theme::small()),
            );
            if cfg!(target_arch = "wasm32") {
                ui.label(
                    RichText::new(crate::tl!("In the browser the endpoint must allow cross-origin requests (CORS) from this page; a local Ollama needs OLLAMA_ORIGINS set to this page's address. The key is saved with your preferences in this browser."))
                        .color(t.text_faint)
                        .font(crate::theme::small()),
                );
            }
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
                st.note(Kind::Note, crate::tl!("Stopping…"));
                ui.ctx().request_repaint();
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
        for line in &mut app.assistant.lines {
            show_line(ui, &t, line);
        }
    });
    if action.is_some() {
        let text = std::mem::take(&mut app.assistant.input);
        send(app, text.trim().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, args: &str, result: Option<&str>, error: bool) -> ToolLine {
        ToolLine { name: name.into(), args: args.into(), result: result.map(str::to_string), error }
    }

    #[test]
    fn a_tool_call_is_one_line() {
        assert_eq!(tool_header(&tool("command_line", r#"{"text":"circle 0,0 5"}"#, Some("ok: done"), false)), "command_line circle 0,0 5 — ok");
        assert_eq!(tool_header(&tool("render", "{}", None, false)), "render — …");
        assert_eq!(tool_header(&tool("query_entities", r#"{"type":"Circle"}"#, Some("x"), false)), r#"query_entities {"type":"Circle"} — ok"#);
        assert_eq!(
            tool_header(&tool("command_line", r#"{"text":"foo"}"#, Some("unknown command `foo`\nsecond line"), true)),
            "command_line foo — error: unknown command `foo`"
        );
        // Cut arguments (not JSON any more) and very long ones stay on one short line.
        let long = format!(r#"{{"text":"{}"#, "x".repeat(500));
        let h = tool_header(&tool("command_line", &long, Some("e"), true));
        assert!(h.chars().count() < 160 && !h.contains('\n'), "{h}");
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        assert_eq!(cut("short"), ("short", false));
        let long = "я".repeat(SHOW_CHARS + 5);
        let (head, cut_off) = cut(&long);
        assert!(cut_off && head.chars().count() == SHOW_CHARS);
        assert!(!cut(&"a".repeat(SHOW_CHARS)).1);
    }

    #[test]
    fn streamed_text_continues_one_line_and_results_find_their_call() {
        let mut st = State::default();
        for e in [
            Event::Text("Hel".into()),
            Event::Text("lo".into()),
            Event::ToolCall { name: "a".into(), args: "{}".into() },
            Event::ToolCall { name: "b".into(), args: "{}".into() },
            Event::ToolResult { name: "a".into(), summary: "ra".into(), error: false },
            Event::ToolResult { name: "b".into(), summary: "rb".into(), error: true },
            Event::Text("Bye".into()),
        ] {
            st.event(e);
        }
        let texts: Vec<_> = st.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["Hello", "", "", "Bye"]);
        let results: Vec<_> = st.lines.iter().filter_map(|l| l.tool.as_ref()).map(|t| (t.result.as_deref(), t.error)).collect();
        assert_eq!(results, [(Some("ra"), false), (Some("rb"), true)]);
    }
}
