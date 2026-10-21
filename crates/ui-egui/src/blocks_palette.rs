//! The Blocks palette (BLOCKSPALETTE, Window ▸ Blocks): the drawing's blocks with a preview each,
//! INSERT by clicking one, and a Suggested section that ranks the blocks by how closely their
//! shape resembles the current selection (BREPLACE's ranking). With "Suggest placement" on, a
//! block inserted from here follows how its other references sit on walls (INSERT's `Suggest`
//! option). A thin view: it reads the drawing and acts through [`CadApp::start`] and the
//! session's prompts.

use cadcraft_doc::Drawing;
use egui::RichText;
use serde_json::{Value, json};

use crate::CadApp;
use crate::docking::Panel;
use crate::theme::Tokens;

/// Suggested blocks shown.
const SUGGESTED: usize = 5;

/// BLOCKSPALETTE (any parameters) and Window ▸ Blocks open the palette.
pub fn route(app: &mut CadApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    match id {
        "blockspalette" => {}
        "ui.dialog.blocks" if params.is_null() => {}
        _ => return None,
    }
    Some(crate::docking::open_floating(app, Panel::Blocks, 300.0))
}

/// The blocks a drawing offers: named blocks that are not external references, sorted by name.
pub fn block_names(d: &Drawing) -> Vec<String> {
    let mut names: Vec<String> =
        d.blocks.values().filter(|b| !b.anonymous && !b.name.starts_with('*') && b.xref_path.is_none()).map(|b| b.name.clone()).collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// How many of the most recently inserted blocks (in any drawing) rank higher.
const RECENT: usize = 10;
/// The distance of a recently inserted block is scaled by this: it ranks nearer.
const RECENT_BOOST: f64 = 0.75;

/// The blocks most like the selection, best first (empty when nothing is selected); blocks
/// inserted recently, in this or another drawing, rank nearer. BREPLACE's ranking is cached per
/// drawing revision and selection.
pub fn suggestions(app: &mut CadApp) -> Vec<(String, f64)> {
    let Ok((uid, revision)) = app.session.state().map(|s| (s.uid, s.revision)) else { return Vec::new() };
    let selection: Vec<String> = app.session.selection().iter().map(|h| h.hex()).collect();
    let key = (uid, revision, selection);
    let cached = app.smart.suggested.as_ref().filter(|(k, _)| *k == key).map(|(_, r)| r.clone());
    let mut ranking = cached.unwrap_or_else(|| {
        let ranking: Vec<(String, f64)> = if key.2.is_empty() {
            Vec::new()
        } else {
            app.session
                .execute("breplace", &json!({ "handles": key.2, "shapes": true }))
                .ok()
                .and_then(|r| r["ranking"].as_array().cloned())
                .map(|a| a.iter().filter_map(|e| Some((e["name"].as_str()?.to_string(), e["distance"].as_f64()?))).collect())
                .unwrap_or_default()
        };
        app.smart.suggested = Some((key, ranking.clone()));
        ranking
    });
    let recent = app.session.recent_blocks(RECENT);
    for (name, d) in &mut ranking {
        if recent.iter().any(|r| r.eq_ignore_ascii_case(name)) {
            *d *= RECENT_BOOST;
        }
    }
    ranking.sort_by(|a, b| a.1.total_cmp(&b.1));
    ranking.truncate(SUGGESTED);
    ranking
}

/// Start INSERT on `name`, with placement suggestion on when `suggest`.
pub fn insert(app: &mut CadApp, name: &str, suggest: bool) {
    crate::dialogs::insert_block(app, name);
    if suggest
        && app.session.running.as_ref().is_some_and(|r| r.id == "insert")
        && let Err(e) = app.session.input(cadcraft_engine::Input::Keyword("Suggest".into()))
    {
        app.session.echo(e.to_string());
    }
}

/// The palette body.
pub fn show(app: &mut CadApp, ui: &mut egui::Ui) {
    let t = Tokens::get();
    let Ok((uid, revision, doc)) = app.session.state().map(|s| (s.uid, s.revision, s.doc.clone())) else {
        ui.label(crate::tl!("No drawing is open."));
        return;
    };
    let names = block_names(&doc);
    let near = suggestions(app);
    let learned = !app.session.recent_blocks(1).is_empty();
    let mut forget = false;
    let mut thumbs = std::mem::take(&mut app.smart.thumbs);
    let mut pick: Option<String> = None;
    let mut row = |ui: &mut egui::Ui, name: &str, note: Option<String>| {
        ui.horizontal(|ui| {
            let list = thumbs.get(uid, revision, &doc, name);
            crate::smartblocks_dlg::paint_thumb(ui, list.as_deref());
            if ui.selectable_label(false, name).on_hover_text(crate::tl!("Insert this block")).clicked() {
                pick = Some(name.to_string());
            }
            if let Some(n) = note {
                ui.label(RichText::new(n).small().color(t.text_dim));
            }
        });
    };
    egui::ScrollArea::vertical().id_salt("dock-blocks").auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(4.0);
        ui.checkbox(&mut app.smart.suggest_placement, crate::tl!("Suggest placement"))
            .on_hover_text(crate::tl!("Insert follows how the block's other references sit on the walls."));
        if ui
            .add_enabled(learned, egui::Button::new(crate::tl!("Forget learned placements")).small())
            .on_hover_text(crate::tl!("Drop the placements and recently used blocks learned in all drawings."))
            .clicked()
        {
            forget = true;
        }
        if !near.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new(crate::tl!("Suggested")).strong());
            for (name, distance) in &near {
                row(ui, name, Some(format!("{:.0}%", 100.0 / (1.0 + distance))));
            }
        }
        ui.add_space(4.0);
        ui.label(RichText::new(crate::tl!("All blocks")).strong());
        if names.is_empty() {
            ui.label(RichText::new(crate::tl!("No blocks defined in this drawing.")).color(t.text_dim));
        }
        for name in &names {
            row(ui, name, None);
        }
    });
    app.smart.thumbs = thumbs;
    if forget && let Err(e) = app.run("bforget", json!({})) {
        app.session.echo(e);
    }
    if let Some(name) = pick {
        let suggest = app.smart.suggest_placement;
        insert(app, &name, suggest);
    }
}
