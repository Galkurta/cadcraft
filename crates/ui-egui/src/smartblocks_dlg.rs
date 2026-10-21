//! The Smart Blocks dialogs. BCONVERT typed or from a menu opens Convert to Block (name, base
//! point, include existing blocks, layer 0, how many identical instances there are); BREPLACE
//! opens Replace Block (the drawing's other blocks, most similar first, each with a thumbnail).
//! OK and Replace run the engine's commands with JSON parameters; the dialogs themselves never
//! change the drawing, and JSON calls and `-BCONVERT` skip them.
//!
//! This file also holds what the Blocks palette shares with them ([`Thumbs`]).

use std::collections::HashMap;
use std::sync::Arc;

use cadcraft_color::Rgb;
use cadcraft_doc::Drawing;
use cadcraft_geom::Vec2;
use cadcraft_render::DisplayList;
use egui::{RichText, Stroke, vec2};
use serde_json::{Value, json};

use crate::CadApp;
use crate::canvas::Xf;
use crate::theme::Tokens;

/// Thumbnail size in points.
pub const THUMB: egui::Vec2 = egui::vec2(56.0, 42.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Convert,
    Replace,
}

/// The Smart Blocks UI state: the dialogs while open, and what the Blocks palette keeps.
#[derive(Default)]
pub struct State {
    pub convert: Option<Convert>,
    pub replace: Option<Replace>,
    /// BCONVERT or BREPLACE started with nothing selected: its dialog opens once the selection
    /// has been made.
    waiting: Option<Kind>,
    /// A base point pick (BPICK) is under way; [`resume`] takes its point and reopens the dialog.
    picking: bool,
    pub(crate) thumbs: Thumbs,
    /// The Blocks palette's "Suggest placement" switch: INSERT from the palette snaps to how the
    /// block's other references sit on walls.
    pub suggest_placement: bool,
    /// The Blocks palette's suggestions for (drawing, revision, selection).
    pub suggested: Option<(Key, Vec<(String, f64)>)>,
}

/// (drawing uid, revision, selection) a cached result is for.
pub type Key = (u64, u64, Vec<String>);

/// The Convert to Block dialog.
#[derive(Clone, Debug, Default)]
pub struct Convert {
    pub handles: Vec<String>,
    /// Empty: the engine's default name.
    pub name: String,
    /// `None`: the lower left of the objects.
    pub base: Option<Vec2>,
    pub include_existing: bool,
    pub layer0: bool,
    pub error: Option<String>,
    /// The name BCONVERT would give by default.
    pub default_name: String,
    /// Instances found (the selection included), and whether the search was cut short.
    pub found: Option<(usize, bool)>,
    counted: Option<(u64, u64, bool)>,
}

/// The Replace Block dialog.
#[derive(Clone, Debug, Default)]
pub struct Replace {
    pub handles: Vec<String>,
    /// Every other block, most similar first, with its distance (0 = the same shape).
    pub ranking: Vec<(String, f64)>,
    pub chosen: Option<String>,
    pub error: Option<String>,
}

/// Block previews, built once per drawing revision and only for the rows shown.
#[derive(Default)]
pub struct Thumbs {
    key: (u64, u64),
    lists: HashMap<String, Option<Arc<DisplayList>>>,
}

impl Thumbs {
    /// The preview of block `name` in `d`.
    pub fn get(&mut self, uid: u64, revision: u64, d: &Drawing, name: &str) -> Option<Arc<DisplayList>> {
        if self.key != (uid, revision) {
            self.key = (uid, revision);
            self.lists.clear();
        }
        self.lists
            .entry(name.to_ascii_uppercase())
            .or_insert_with(|| {
                let b = d.block(name)?;
                let rough = cadcraft_render::build_entities(d, b.entities.iter().map(|e| e.as_ref()), &cadcraft_render::Options::default());
                let size = rough.bounds.width().max(rough.bounds.height());
                let opts = cadcraft_render::Options { tolerance: (size / 100.0).max(1e-9), ..Default::default() };
                let list =
                    if rough.bounds.is_empty() { rough } else { cadcraft_render::build_entities(d, b.entities.iter().map(|e| e.as_ref()), &opts) };
                Some(Arc::new(list))
            })
            .clone()
    }
}

/// Draw `list` fitted into a thumbnail-sized box.
pub fn paint_thumb(ui: &mut egui::Ui, list: Option<&DisplayList>) {
    let t = Tokens::get();
    let (rect, _) = ui.allocate_exact_size(THUMB, egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 3.0, t.canvas);
    p.rect_stroke(rect, 3.0, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
    if let Some(l) = list.filter(|l| !l.bounds.is_empty() && !l.prims.is_empty()) {
        let b = l.bounds;
        let inner = rect.shrink(4.0);
        let (w, h) = (b.width().max(1e-9), b.height().max(1e-9));
        let scale = (f64::from(inner.width()) / w).min(f64::from(inner.height()) / h);
        let xf = Xf { rect, center: Vec2::new((b.min.x + b.max.x) / 2.0, (b.min.y + b.max.y) / 2.0), scale };
        crate::canvas::draw_list(&p, &xf, l, Rgb(t.canvas.r(), t.canvas.g(), t.canvas.b()), false);
    }
}

/// BCONVERT and BREPLACE typed or chosen from a menu (no parameters) open their dialogs; JSON
/// calls and `-BCONVERT` run the commands as they are.
pub fn route(app: &mut CadApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    if !params.is_null() {
        return None;
    }
    let kind = match cadcraft_engine::cmd::resolve_alias(id).as_str() {
        "bconvert" => Kind::Convert,
        "breplace" => Kind::Replace,
        _ => return None,
    };
    app.session.state().ok()?;
    Some(open(app, kind))
}

fn open(app: &mut CadApp, kind: Kind) -> Result<Value, String> {
    let selection = app.session.selection();
    if selection.is_empty() {
        // Ask for the objects first (SELECT); the dialog follows when that ends.
        app.smart.waiting = Some(kind);
        app.session.start("select").map_err(|e| e.to_string())?;
        return Ok(Value::Null);
    }
    open_with(app, kind, &selection);
    Ok(Value::Null)
}

fn open_with(app: &mut CadApp, kind: Kind, selection: &[cadcraft_engine::doc::Handle]) {
    let handles: Vec<String> = selection.iter().map(|h| h.hex()).collect();
    match kind {
        Kind::Convert => {
            let include_existing = app.session.doc().is_ok_and(|d| d.header.i64("BCONVERTINCLUDEEXISTINGBLOCKS", 0) != 0);
            let layer0 = app.session.doc().is_ok_and(|d| d.header.i64("BCONVERTLAYER", 0) != 0);
            app.smart.convert = Some(Convert { handles, include_existing, layer0, ..Convert::default() });
            app.ui.dialog = Some("bconvert".into());
        }
        Kind::Replace => {
            let ranking: Vec<(String, f64)> = match app.session.execute("breplace", &json!({ "handles": handles })) {
                Ok(r) => r["ranking"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|e| Some((e["name"].as_str()?.to_string(), e["distance"].as_f64()?))).collect())
                    .unwrap_or_default(),
                Err(e) => {
                    app.session.echo(e.to_string());
                    return;
                }
            };
            let chosen = ranking.first().map(|(n, _)| n.clone());
            app.smart.replace = Some(Replace { handles, ranking, chosen, error: None });
            app.ui.dialog = Some("breplace".into());
        }
    }
}

/// Every frame: once the SELECT a dialog asked for, or its base point pick, has ended, bring the
/// dialog back (with the picked point; unchanged when the pick was cancelled).
pub fn resume(app: &mut CadApp) {
    if app.session.running.is_some() {
        return;
    }
    if std::mem::take(&mut app.smart.picking) {
        let picked = app.session.smart_pick.take();
        if let (Some(p), Some(c)) = (picked, app.smart.convert.as_mut()) {
            c.base = Some(p);
        }
        if app.smart.convert.is_some() {
            app.ui.dialog = Some("bconvert".into());
        }
    }
    if let Some(kind) = app.smart.waiting.take() {
        let selection = app.session.selection();
        if !selection.is_empty() {
            open_with(app, kind, &selection);
        }
    }
}

/// Pick the base point on the drawing (BPICK: "Specify base point" with snaps and tracking); the
/// dialog hides meanwhile.
fn pick_base(app: &mut CadApp) {
    app.smart.picking = true;
    app.session.smart_pick = None;
    app.ui.dialog = None;
    app.start("bpick");
}

/// How many instances BCONVERT would convert, refreshed when the drawing or the choice changes.
fn count(app: &mut CadApp, c: &mut Convert) {
    let Ok(st) = app.session.state() else { return };
    let key = (st.uid, st.revision, c.include_existing);
    if c.counted == Some(key) {
        return;
    }
    c.counted = Some(key);
    match app.session.execute("bconvert", &json!({ "handles": c.handles, "includeExisting": c.include_existing, "dryRun": true })) {
        Ok(r) => {
            c.found = r["instances"].as_u64().map(|n| (usize::try_from(n).unwrap_or(usize::MAX), r["truncated"].as_bool().unwrap_or(false)));
            c.default_name = r["block"].as_str().unwrap_or_default().to_string();
        }
        Err(e) => {
            c.found = None;
            c.error = Some(e.to_string());
        }
    }
}

/// OK: run BCONVERT with the dialog's choices. A refusal (a bad or taken name) stays in the
/// dialog.
pub fn convert_ok(app: &mut CadApp, c: &mut Convert) -> bool {
    let mut p = json!({ "handles": c.handles, "includeExisting": c.include_existing, "layer0": c.layer0 });
    if !c.name.trim().is_empty() {
        p["name"] = json!(c.name.trim());
    }
    if let Some(b) = c.base {
        p["base"] = json!([b.x, b.y]);
    }
    match app.run("bconvert", p) {
        Ok(_) => true,
        Err(e) => {
            c.error = Some(e);
            false
        }
    }
}

/// The Convert to Block window.
pub fn convert_dialog(app: &mut CadApp, ctx: &egui::Context, open: &mut bool) {
    let Some(mut c) = app.smart.convert.take() else {
        *open = false;
        return;
    };
    count(app, &mut c);
    let t = Tokens::get();
    let (mut ok, mut cancel, mut pick) = (false, false, false);
    egui::Window::new(crate::tl!("Convert to Block"))
        .id(egui::Id::new("Convert to Block"))
        .open(open)
        .collapsible(false)
        .resizable(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            egui::Grid::new("convert-grid").num_columns(2).spacing(vec2(8.0, 6.0)).show(ui, |ui| {
                ui.label(crate::tl!("Block name"));
                let edit = ui.add(egui::TextEdit::singleline(&mut c.name).hint_text(c.default_name.clone()).desired_width(220.0));
                if edit.changed() {
                    c.error = None;
                }
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    ok = true;
                }
                ui.end_row();
                ui.label(crate::tl!("Base point"));
                ui.horizontal(|ui| {
                    match c.base {
                        Some(b) => ui.label(format!("{:.4}, {:.4}", b.x, b.y)),
                        None => ui.label(RichText::new(crate::tl!("Lower left of the objects")).color(t.text_dim)),
                    };
                    if ui.button(crate::tl!("Pick point")).clicked() {
                        pick = true;
                    }
                });
                ui.end_row();
            });
            ui.add_space(4.0);
            if ui.checkbox(&mut c.include_existing, crate::tl!("Include existing blocks")).changed() {
                c.error = None;
            }
            ui.checkbox(&mut c.layer0, crate::tl!("Convert objects to layer 0"));
            ui.add_space(4.0);
            match c.found {
                Some((n, cut)) => {
                    ui.label(crate::tf!("Instances found: {n}", n = n));
                    if cut {
                        ui.label(RichText::new(crate::tl!("The search was cut short; there may be more.")).color(t.text_dim));
                    }
                }
                None => {
                    ui.label(RichText::new(crate::tl!("Instances found: none")).color(t.text_dim));
                }
            }
            if let Some(e) = &c.error {
                ui.colored_label(t.warn, e);
            }
            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(crate::tl!("Cancel")).clicked() {
                    cancel = true;
                }
                if ui.add_enabled(c.found.is_some(), egui::Button::new(crate::tl!("OK"))).clicked() {
                    ok = true;
                }
            });
        });
    if pick {
        app.smart.convert = Some(c);
        pick_base(app);
        return;
    }
    if cancel || (ok && convert_ok(app, &mut c)) {
        *open = false;
        return;
    }
    // The window's own close button: nothing was converted.
    if *open {
        app.smart.convert = Some(c);
    }
}

/// Replace: run BREPLACE with the chosen block.
pub fn replace_ok(app: &mut CadApp, r: &mut Replace) -> bool {
    let Some(with) = r.chosen.clone() else { return false };
    match app.run("breplace", json!({ "handles": r.handles, "with": with })) {
        Ok(_) => true,
        Err(e) => {
            r.error = Some(e);
            false
        }
    }
}

/// The Replace Block window.
pub fn replace_dialog(app: &mut CadApp, ctx: &egui::Context, open: &mut bool) {
    let Some(mut r) = app.smart.replace.take() else {
        *open = false;
        return;
    };
    let Ok((uid, revision, doc)) = app.session.state().map(|s| (s.uid, s.revision, s.doc.clone())) else {
        *open = false;
        return;
    };
    let t = Tokens::get();
    let mut thumbs = std::mem::take(&mut app.smart.thumbs);
    let (mut go, mut cancel) = (false, false);
    egui::Window::new(crate::tl!("Replace Block"))
        .id(egui::Id::new("Replace Block"))
        .open(open)
        .collapsible(false)
        .resizable(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            if r.ranking.is_empty() {
                ui.label(crate::tl!("There is no other block to replace with."));
            } else {
                ui.label(crate::tf!("Replace {n} block reference(s) with the most similar block:", n = r.handles.len()));
                ui.add_space(4.0);
                egui::ScrollArea::vertical().id_salt("replace-blocks").max_height(300.0).show(ui, |ui| {
                    ui.set_min_width(300.0);
                    for (name, distance) in &r.ranking {
                        ui.horizontal(|ui| {
                            let list = thumbs.get(uid, revision, &doc, name);
                            paint_thumb(ui, list.as_deref());
                            let row = ui.selectable_label(r.chosen.as_deref() == Some(name.as_str()), name);
                            if row.clicked() {
                                r.chosen = Some(name.clone());
                                r.error = None;
                            }
                            if row.double_clicked() {
                                go = true;
                            }
                            ui.label(RichText::new(format!("{:.0}%", 100.0 / (1.0 + distance))).small().color(t.text_dim));
                        });
                    }
                });
            }
            if let Some(e) = &r.error {
                ui.colored_label(t.warn, e);
            }
            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(crate::tl!("Cancel")).clicked() {
                    cancel = true;
                }
                if ui.add_enabled(r.chosen.is_some(), egui::Button::new(crate::tl!("Replace"))).clicked() {
                    go = true;
                }
            });
        });
    app.smart.thumbs = thumbs;
    if cancel || (go && replace_ok(app, &mut r)) {
        *open = false;
        return;
    }
    if *open {
        app.smart.replace = Some(r);
    }
}
