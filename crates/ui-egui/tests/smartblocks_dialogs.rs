//! The Smart Blocks dialogs and the Blocks palette: BCONVERT typed or from a menu opens Convert to
//! Block and OK runs the engine's command; BREPLACE opens Replace Block listing the most similar
//! block first; the Blocks palette suggests blocks for the selection and inserts with placement
//! suggestion. JSON calls and -BCONVERT never open a dialog.

use cadcraft_engine::doc::geom::Vec3;
use cadcraft_engine::doc::{Circle, EntityKind, Handle, Line};
use cadcraft_engine::geom::Vec2;
use cadcraft_engine::{Input, Session};
use cadcraft_ui_egui::{CadApp, Services, blocks_palette, docking::Panel};
use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

fn harness(mut app: CadApp) -> Harness<'static, CadApp> {
    app.ui.in_window_menu = false;
    let mut h = Harness::builder().with_size(vec2(1400.0, 900.0)).build_ui_state(
        |ui, app: &mut CadApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    h
}

fn pt(p: Vec2, deg: f64, k: f64, at: (f64, f64)) -> Vec3 {
    (p.rotate(deg.to_radians()) * k + Vec2::new(at.0, at.1)).to3(0.0)
}

/// Two lines and a circle, turned by `deg`, scaled by `k` and moved to `at`.
fn shape(app: &mut CadApp, deg: f64, k: f64, at: (f64, f64)) -> Vec<Handle> {
    let v = Vec2::new;
    let mut hs = Vec::new();
    for (a, b) in [(v(0.0, 0.0), v(4.0, 0.0)), (v(4.0, 0.0), v(4.0, 2.0))] {
        hs.push(app.session.add_entity(EntityKind::Line(Line { a: pt(a, deg, k, at), b: pt(b, deg, k, at) })).unwrap());
    }
    hs.push(app.session.add_entity(EntityKind::Circle(Circle { center: pt(v(1.0, 1.0), deg, k, at), radius: 0.5 * k })).unwrap());
    hs
}

fn hexes(hs: &[Handle]) -> Vec<String> {
    hs.iter().map(|h| h.hex()).collect()
}

fn references(app: &CadApp) -> Vec<(String, Vec2, f64)> {
    let d = app.session.doc().unwrap();
    d.model
        .iter()
        .filter_map(|e| if let EntityKind::Insert(i) = &e.kind { Some((i.block.clone(), i.insert.xy(), i.rotation)) } else { None })
        .collect()
}

/// Two copies of a shape on layer WALLS, the first selected.
fn app_with_two_copies() -> (CadApp, Vec<Handle>) {
    let mut app = CadApp::new(Session::new(), Services::default());
    let a = shape(&mut app, 0.0, 1.0, (0.0, 0.0));
    let b = shape(&mut app, 90.0, 1.0, (30.0, 10.0));
    app.session.execute("layer.new", &json!({ "name": "WALLS" })).unwrap();
    for h in a.iter().chain(&b) {
        app.session.doc_mut().unwrap().modify_entity(*h, |e| e.common.layer = "WALLS".into()).unwrap();
    }
    app.session.set_selection(a.clone());
    (app, a)
}

#[test]
fn typed_bconvert_opens_the_dialog_and_ok_converts_every_instance() {
    let (mut app, _) = app_with_two_copies();
    // JSON calls and -BCONVERT never open it.
    let mut probe = CadApp::new(Session::new(), Services::default());
    let a = shape(&mut probe, 0.0, 1.0, (0.0, 0.0));
    probe.session.set_selection(a);
    assert!(probe.cmdline("-bconvert").is_none());
    assert!(probe.ui.dialog.is_none() && probe.session.prompting(), "the command-line form asks on the command line");
    probe.session.input(Input::Cancel).unwrap();
    let r = probe.run("bconvert", json!({ "name": "VIA_JSON" })).unwrap();
    assert_eq!(r["block"], "VIA_JSON");
    assert!(probe.ui.dialog.is_none() && probe.smart.convert.is_none());

    assert!(app.cmdline("BCONVERT").is_none());
    assert_eq!(app.ui.dialog.as_deref(), Some("bconvert"));
    let mut h = harness(app);
    h.get_by_label("Convert to Block");
    assert_eq!(h.state().smart.convert.as_ref().unwrap().found, Some((2, false)), "the selection and its rotated copy");
    h.get_by_label("Instances found: 2");
    h.get_by_label("Lower left of the objects");
    h.state_mut().smart.convert.as_mut().unwrap().name = "PAIR".into();
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.dialog, None);
    assert!(h.state().smart.convert.is_none());
    let refs = references(h.state());
    assert_eq!(refs.len(), 2, "six objects became two references");
    assert!(refs.iter().all(|r| r.0 == "PAIR"));
    assert_eq!(h.state().session.doc().unwrap().model.len(), 2);
    assert_eq!(h.state().session.doc().unwrap().block("PAIR").unwrap().entities.len(), 3);
}

#[test]
fn cancel_and_a_refused_name_leave_the_drawing_alone() {
    let (app, _) = app_with_two_copies();
    let mut h = harness(app);
    h.state_mut().cmdline("bconvert");
    h.run_steps(3);
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert_eq!((h.state().ui.dialog.clone(), h.state().session.doc().unwrap().model.len()), (None, 6));
    assert!(h.state().smart.convert.is_none());

    h.state_mut().cmdline("bconvert");
    h.run_steps(3);
    h.state_mut().smart.convert.as_mut().unwrap().name = "a*b".into();
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(3);
    let c = h.state().smart.convert.as_ref().expect("the dialog stays open");
    assert!(c.error.is_some(), "a bad name is refused in the dialog");
    assert_eq!(h.state().ui.dialog.as_deref(), Some("bconvert"));
    assert_eq!(h.state().session.doc().unwrap().model.len(), 6);
    // A good name then goes through, with objects on layer 0.
    {
        let c = h.state_mut().smart.convert.as_mut().unwrap();
        c.name = "ZERO".into();
        c.layer0 = true;
    }
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(3);
    let d = h.state().session.doc().unwrap();
    assert!(d.block("ZERO").unwrap().entities.iter().all(|e| e.common.layer == "0"));
}

#[test]
fn the_count_follows_include_existing_blocks() {
    let (mut app, a) = app_with_two_copies();
    // A third copy that is already a block reference: the shape at the block's base, turned by the reference.
    let c = shape(&mut app, 0.0, 1.0, (-40.0, 5.0));
    app.session.execute("-block", &json!({ "name": "OLD", "base": [-40, 5], "handles": hexes(&c) })).unwrap();
    app.session.execute("insert", &json!({ "name": "OLD", "at": [-40, 5], "rotation": 200 })).unwrap();
    app.session.set_selection(a);
    app.cmdline("bconvert");
    let mut h = harness(app);
    assert_eq!(h.state().smart.convert.as_ref().unwrap().found, Some((2, false)));
    h.get_by_label("Include existing blocks").click();
    h.run_steps(3);
    assert_eq!(h.state().smart.convert.as_ref().unwrap().found, Some((3, false)));
    h.get_by_label("Instances found: 3");
    h.get_by_label("OK").click();
    h.run_steps(3);
    assert!(references(h.state()).iter().all(|r| r.0 != "OLD"), "the existing reference took the new block");
    assert_eq!(references(h.state()).len(), 3);
}

#[test]
fn the_base_point_is_picked_on_the_drawing_and_a_cancelled_pick_keeps_the_dialog() {
    let (mut app, _) = app_with_two_copies();
    app.cmdline("bconvert");
    let mut h = harness(app);
    h.get_by_label("Pick point").click();
    h.run_steps(2);
    assert!(h.state().session.running.is_some(), "the point prompt is on the command line");
    assert!(h.state().session.prompt_text().contains("Specify base point"), "{}", h.state().session.prompt_text());
    assert_eq!(h.state().ui.dialog, None, "the dialog steps aside");
    h.state_mut().session.input(Input::Cancel).unwrap();
    h.run_steps(3);
    assert_eq!(h.state().ui.dialog.as_deref(), Some("bconvert"));
    assert_eq!(h.state().smart.convert.as_ref().unwrap().base, None, "Esc changes nothing");

    h.get_by_label("Pick point").click();
    h.run_steps(2);
    h.state_mut().session.input(Input::Point(Vec2::new(2.0, 1.0))).unwrap();
    h.run_steps(3);
    assert_eq!(h.state().ui.dialog.as_deref(), Some("bconvert"));
    assert_eq!(h.state().smart.convert.as_ref().unwrap().base, Some(Vec2::new(2.0, 1.0)));
    h.get_by_label("2.0000, 1.0000");
    // Esc on a later pick keeps the point already picked.
    h.get_by_label("Pick point").click();
    h.run_steps(2);
    h.state_mut().session.input(Input::Cancel).unwrap();
    h.run_steps(3);
    assert_eq!(h.state().ui.dialog.as_deref(), Some("bconvert"));
    assert_eq!(h.state().smart.convert.as_ref().unwrap().base, Some(Vec2::new(2.0, 1.0)));
    h.get_by_label("OK").click();
    h.run_steps(3);
    let at = references(h.state()).into_iter().map(|r| r.1).find(|at| at.near(Vec2::new(2.0, 1.0), 1e-9));
    assert!(at.is_some(), "the first reference sits at the picked base point");
}

#[test]
fn bconvert_with_nothing_selected_asks_for_objects_first() {
    let (mut app, a) = app_with_two_copies();
    app.session.set_selection(Vec::new());
    app.cmdline("bconvert");
    assert!(app.session.running.is_some() && app.ui.dialog.is_none(), "SELECT first");
    let mut h = harness(app);
    h.state_mut().session.input(Input::Pick(a)).unwrap();
    h.state_mut().session.input(Input::Enter).unwrap();
    h.run_steps(3);
    assert_eq!(h.state().ui.dialog.as_deref(), Some("bconvert"), "the dialog follows the selection");
    assert_eq!(h.state().smart.convert.as_ref().unwrap().handles.len(), 3);
    // Cancelling the selection opens nothing.
    h.state_mut().ui.dialog = None;
    h.state_mut().smart.convert = None;
    h.state_mut().session.set_selection(Vec::new());
    h.state_mut().cmdline("bconvert");
    h.state_mut().session.input(Input::Cancel).unwrap();
    h.run_steps(3);
    assert_eq!(h.state().ui.dialog, None);
}

fn define(app: &mut CadApp, name: &str, hs: &[Handle]) {
    app.session.execute("-block", &json!({ "name": name, "base": [0, 0], "handles": hexes(hs) })).unwrap();
}

/// Blocks: SAMPLE (the shape), BIG (the shape three times the size, same look), PART (without
/// its circle) and FAR (a lone circle); a reference to SAMPLE is selected.
fn app_with_blocks() -> (CadApp, Handle) {
    let mut app = CadApp::new(Session::new(), Services::default());
    let a = shape(&mut app, 0.0, 1.0, (0.0, 0.0));
    define(&mut app, "SAMPLE", &a);
    let b = shape(&mut app, 25.0, 3.0, (0.0, 0.0));
    define(&mut app, "BIG", &b);
    let p = shape(&mut app, 0.0, 1.0, (0.0, 0.0));
    app.session.doc_mut().unwrap().remove_entity(p[2]);
    define(&mut app, "PART", &p[..2]);
    let c = app.session.add_entity(EntityKind::Circle(Circle { center: Vec3::ZERO, radius: 2.0 })).unwrap();
    define(&mut app, "FAR", &[c]);
    let r = app.session.execute("insert", &json!({ "name": "SAMPLE", "at": [10, 20], "rotation": 40, "scale": 1.5 })).unwrap();
    let h = Handle::parse_hex(r["handle"].as_str().unwrap()).unwrap();
    app.session.set_selection(vec![h]);
    (app, h)
}

#[test]
fn breplace_lists_the_most_similar_block_first_and_replace_swaps_the_reference() {
    let (app, r) = app_with_blocks();
    let mut app = app;
    assert!(app.cmdline("breplace").is_none());
    assert_eq!(app.ui.dialog.as_deref(), Some("breplace"));
    let mut h = harness(app);
    h.get_by_label("Replace Block");
    let names: Vec<String> = h.state().smart.replace.as_ref().unwrap().ranking.iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(names, ["BIG", "PART", "FAR"], "the same shape at another size first");
    assert_eq!(h.state().smart.replace.as_ref().unwrap().chosen.as_deref(), Some("BIG"), "the most similar is preselected");
    h.get_by_label("Replace 1 block reference(s) with the most similar block:");
    // The choice can be changed; Replace uses it and keeps the placement.
    h.get_by_label("FAR").click();
    h.run_steps(2);
    assert_eq!(h.state().smart.replace.as_ref().unwrap().chosen.as_deref(), Some("FAR"));
    h.get_by_label("BIG").click();
    h.run_steps(2);
    h.get_by_label("Replace").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.dialog, None);
    let e = h.state().session.doc().unwrap().entity(r).unwrap().clone();
    let EntityKind::Insert(i) = &e.kind else { panic!("{e:?}") };
    assert_eq!(i.block, "BIG");
    assert!((i.insert.x - 10.0).abs() < 1e-9 && (i.scale.x - 1.5).abs() < 1e-9 && (i.rotation - 40f64.to_radians()).abs() < 1e-9);
}

#[test]
fn breplace_dialog_cancel_changes_nothing_and_without_references_it_does_not_open() {
    let (mut app, r) = app_with_blocks();
    app.session.set_selection(Vec::new());
    // Objects that are not references: the engine's message, no dialog.
    let loose = shape(&mut app, 0.0, 1.0, (100.0, 100.0));
    app.session.set_selection(loose);
    app.cmdline("breplace");
    assert_eq!(app.ui.dialog, None);
    app.session.set_selection(vec![r]);
    let json_call = app.run("breplace", json!({ "handles": [r.hex()] })).unwrap();
    assert!(json_call["ranking"].is_array());
    assert_eq!(app.ui.dialog, None, "JSON calls never open it");
    app.cmdline("breplace");
    let mut h = harness(app);
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.dialog, None);
    let EntityKind::Insert(i) = &h.state().session.doc().unwrap().entity(r).unwrap().kind else { panic!() };
    assert_eq!(i.block, "SAMPLE");
}

#[test]
fn ui_dialog_blocks_opens_the_palette_whatever_the_params() {
    for params in [Value::Null, json!({})] {
        let (mut app, _) = app_with_blocks();
        app.run("ui.dialog.blocks", params).unwrap();
        assert!(app.ui.docking.layout.contains(&Panel::Blocks));
        assert_eq!(app.ui.dialog, None);
    }
}

#[test]
fn the_blocks_palette_suggests_blocks_for_the_selection_most_similar_first() {
    let (mut app, _) = app_with_blocks();
    // The selection: the shape again, turned and at another size (not a reference).
    let sel = shape(&mut app, 70.0, 2.0, (200.0, 100.0));
    app.session.set_selection(sel);
    assert!(app.cmdline("blockspalette").is_none());
    assert!(app.ui.docking.layout.contains(&Panel::Blocks));
    let mut h = harness(app);
    h.get_by_label("Suggested");
    h.get_by_label("All blocks");
    let (_, ranking) = h.state().smart.suggested.clone().expect("suggestions were computed");
    let names: Vec<&str> = ranking.iter().map(|(n, _)| n.as_str()).collect();
    // BIG and SAMPLE are the shape at another size and turn: both first, in either order.
    let mut first_two = names[..2].to_vec();
    first_two.sort_unstable();
    assert_eq!(first_two, ["BIG", "SAMPLE"], "{names:?}");
    assert!(ranking[0].1 < 1e-2, "{ranking:?}");
    let at = |n: &str| ranking.iter().position(|(m, _)| m == n);
    assert!(at("PART") < at("FAR"), "the lines alone are nearer than a lone circle: {names:?}");
    // Nothing selected: no Suggested section.
    h.state_mut().session.set_selection(Vec::new());
    h.run_steps(3);
    assert!(h.state().smart.suggested.as_ref().is_some_and(|(_, r)| r.is_empty()));
    assert!(h.query_by_label("Suggested").is_none());
}

#[test]
fn inserting_from_the_palette_follows_the_placement_suggestion_when_it_is_on() {
    let mut app = CadApp::new(Session::new(), Services::default());
    let door = shape(&mut app, 0.0, 1.0, (0.0, 0.0));
    define(&mut app, "DOOR", &door);
    for (a, b) in [([0, 0], [10, 0]), ([0, 5], [10, 5])] {
        app.session.execute("line", &json!({ "points": [a, b] })).unwrap();
    }
    for x in [2.0, 6.0] {
        app.session.execute("insert", &json!({ "name": "DOOR", "at": [x, 0.25], "rotation": 90 })).unwrap();
    }
    app.session.set_selection(Vec::new());
    app.cmdline("blockspalette");
    let mut h = harness(app);
    // On first: the learned relation to the nearest wall.
    h.get_by_label("Suggest placement").click();
    h.run_steps(2);
    assert!(h.state().smart.suggest_placement);
    h.get_all_by_label("DOOR").next().unwrap().click();
    h.run_steps(2);
    assert!(h.state().session.running.as_ref().is_some_and(|r| r.id == "insert"));
    h.state_mut().session.input(Input::Point(Vec2::new(4.4, 5.9))).unwrap();
    for _ in 0..3 {
        h.state_mut().session.input(Input::Enter).unwrap();
    }
    let snapped = references(h.state()).pop().unwrap();
    assert!(snapped.1.near(Vec2::new(4.4, 5.25), 1e-9), "{snapped:?}");
    assert!((snapped.2 - 90f64.to_radians()).abs() < 1e-9, "{snapped:?}");
    // Off: the block goes where the point is.
    h.get_by_label("Suggest placement").click();
    h.run_steps(2);
    assert!(!h.state().smart.suggest_placement);
    h.get_all_by_label("DOOR").next().unwrap().click();
    h.run_steps(2);
    h.state_mut().session.input(Input::Point(Vec2::new(1.3, 3.1))).unwrap();
    for _ in 0..3 {
        h.state_mut().session.input(Input::Enter).unwrap();
    }
    let plain = references(h.state()).pop().unwrap();
    assert!(plain.1.near(Vec2::new(1.3, 3.1), 1e-9) && plain.2.abs() < 1e-9, "{plain:?}");
}

/// Walls y = 0 and y = 5 and the block DOOR; with `teach`, two doors on the lower wall, turned 90°.
fn walls_and_door(app: &mut CadApp, teach: bool) {
    let door = shape(app, 0.0, 1.0, (50.0, 50.0));
    define(app, "DOOR", &door);
    for (a, b) in [([0, 0], [10, 0]), ([0, 5], [10, 5])] {
        app.session.execute("line", &json!({ "points": [a, b] })).unwrap();
    }
    if teach {
        for x in [2.0, 6.0] {
            app.session.execute("insert", &json!({ "name": "DOOR", "at": [x, 0.25], "rotation": 90 })).unwrap();
        }
    }
}

fn suggested_door(app: &mut CadApp) -> Value {
    app.session.execute("insert", &json!({ "name": "DOOR", "at": [4.4, 5.9], "suggest": true })).unwrap()
}

#[test]
fn learned_placements_survive_a_prefs_round_trip_and_drive_a_suggestion_in_a_new_drawing() {
    let mut app = CadApp::new(Session::new(), Services::default());
    walls_and_door(&mut app, true);
    let prefs = app.prefs_json();
    assert!(serde_json::from_str::<Value>(&prefs).unwrap()["smartBlocks"].is_array());

    // The next run: loaded prefs, a new drawing with the block and walls but no doors.
    let mut next = CadApp::new(Session::new(), Services::default());
    next.load_prefs(&prefs);
    assert_eq!(next.session.recent_blocks(5), ["DOOR"]);
    walls_and_door(&mut next, false);
    let r = suggested_door(&mut next);
    assert_eq!(r["suggested"], true);
    let snapped = references(&next).pop().unwrap();
    assert!(snapped.1.near(Vec2::new(4.4, 5.25), 1e-9) && (snapped.2 - 90f64.to_radians()).abs() < 1e-9, "{snapped:?}");

    // Without the prefs nothing is known.
    let mut blank = CadApp::new(Session::new(), Services::default());
    walls_and_door(&mut blank, false);
    assert_eq!(suggested_door(&mut blank)["suggested"], false);
    // Malformed stored data is ignored.
    blank.load_prefs(r#"{"smartBlocks": {"x": [1, 2, 3]}}"#);
    assert!(blank.session.recent_blocks(5).is_empty());
}

#[test]
fn forget_learned_placements_clears_them_from_the_session_and_the_prefs() {
    let mut app = CadApp::new(Session::new(), Services::default());
    walls_and_door(&mut app, true);
    let prefs = app.prefs_json();
    let mut next = CadApp::new(Session::new(), Services::default());
    next.load_prefs(&prefs);
    walls_and_door(&mut next, false);
    next.cmdline("blockspalette");
    let mut h = harness(next);
    h.get_by_label("Forget learned placements").click();
    h.run_steps(3);
    assert!(h.state().session.recent_blocks(5).is_empty());
    assert!(serde_json::from_str::<Value>(&h.state().prefs_json()).unwrap().get("smartBlocks").is_none());
    assert_eq!(suggested_door(h.state_mut())["suggested"], false, "the drawing has no doors and nothing is remembered");
}

#[test]
fn recently_inserted_blocks_rank_nearer_in_the_palette_suggestions() {
    let (mut app, _) = app_with_blocks();
    let sel = shape(&mut app, 70.0, 2.0, (200.0, 100.0));
    app.session.set_selection(sel);
    let before = blocks_palette::suggestions(&mut app);
    let far = before.iter().find(|(n, _)| n == "FAR").map(|(_, d)| *d).unwrap();
    // FAR was used in another drawing.
    app.session.load_placements(&json!([{ "name": "far" }]));
    let after = blocks_palette::suggestions(&mut app);
    let boosted = after.iter().find(|(n, _)| n == "FAR").map(|(_, d)| *d).unwrap();
    assert!(boosted < far && (boosted - far * 0.75).abs() < 1e-12, "{before:?} {after:?}");
    assert!(after.windows(2).all(|w| w[0].1 <= w[1].1), "still best first: {after:?}");
}
