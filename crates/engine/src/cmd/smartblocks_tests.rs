use cadcraft_doc::{Arc as DocArc, Circle, EntityKind, Handle, Line, LwPolyline};
use cadcraft_geom::{PolyVertex, Vec2, Vec3};
use serde_json::{Value, json};

use super::place::{learn, suggest};
use super::shape::{Feat, feat_of};
use crate::Session;

/// How a copy of the sample is made: rotated, scaled, shifted; or mirrored / distorted.
#[derive(Clone, Copy)]
struct Place {
    k: f64,
    deg: f64,
    t: Vec2,
    mirror: bool,
    distort: bool,
}

const AT: fn(f64, f64) -> Place = |x, y| Place { k: 1.0, deg: 0.0, t: Vec2::new(x, y), mirror: false, distort: false };

fn xf(c: &Place, p: Vec2) -> Vec3 {
    let p = if c.mirror { Vec2::new(-p.x, p.y) } else { p };
    (p.rotate(c.deg.to_radians()) * c.k + c.t).to3(0.0)
}

/// Lines (0,0)-(5,0), (5,0)-(5,2), (5,2)-(0,4) and the arc from (0,0) to (0,4) round (0,2): a
/// connected, asymmetric shape of four objects.
fn connected(s: &mut Session, c: Place) -> Vec<Handle> {
    let l = |s: &mut Session, a: (f64, f64), b: (f64, f64)| {
        let (a, b) = (Vec2::new(a.0, a.1), Vec2::new(b.0, b.1));
        s.add_entity(EntityKind::Line(Line { a: xf(&c, a), b: xf(&c, b) })).unwrap()
    };
    let mut hs = vec![l(s, (0.0, 0.0), (5.0, 0.0)), l(s, (5.0, 0.0), (5.0, 2.0)), l(s, (5.0, 2.0), (if c.distort { 0.3 } else { 0.0 }, 4.0))];
    let (mut a0, mut a1) = (-std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2);
    if c.mirror {
        (a0, a1) = (std::f64::consts::PI - a1, std::f64::consts::PI - a0);
    }
    let rot = c.deg.to_radians();
    hs.push(
        s.add_entity(EntityKind::Arc(DocArc { center: xf(&c, Vec2::new(0.0, 2.0)), radius: 2.0 * c.k, start: a0 + rot, end: a1 + rot })).unwrap(),
    );
    hs
}

/// Two lines, a circle and an arc that touch nothing.
fn loose(s: &mut Session, c: Place) -> Vec<Handle> {
    let line = |s: &mut Session, a: (f64, f64), b: (f64, f64)| {
        s.add_entity(EntityKind::Line(Line { a: xf(&c, Vec2::new(a.0, a.1)), b: xf(&c, Vec2::new(b.0, b.1)) })).unwrap()
    };
    let cx = if c.distort { 1.3 } else { 1.0 };
    let mut hs = vec![line(s, (0.0, 0.0), (4.0, 0.0)), line(s, (4.0, 0.0), (4.0, 2.0))];
    hs.push(s.add_entity(EntityKind::Circle(Circle { center: xf(&c, Vec2::new(cx, 1.0)), radius: 0.5 * c.k })).unwrap());
    let (mut a0, mut a1) = (0.3, 2.0);
    if c.mirror {
        (a0, a1) = (std::f64::consts::PI - a1, std::f64::consts::PI - a0);
    }
    let rot = c.deg.to_radians();
    hs.push(
        s.add_entity(EntityKind::Arc(DocArc { center: xf(&c, Vec2::new(3.0, 1.0)), radius: 0.8 * c.k, start: a0 + rot, end: a1 + rot })).unwrap(),
    );
    hs
}

fn hexes(hs: &[Handle]) -> Vec<String> {
    hs.iter().map(|h| h.hex()).collect()
}

fn feats(s: &Session) -> Vec<Feat> {
    s.doc().unwrap().model.iter().filter_map(|e| feat_of(&e.kind)).collect()
}

fn sorted(mut v: Vec<Feat>) -> Vec<Feat> {
    v.sort_by(|a, b| (a.tag as u8, a.center().x, a.center().y).partial_cmp(&(b.tag as u8, b.center().x, b.center().y)).unwrap());
    v
}

fn same(a: &[Feat], b: &[Feat]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.tag == y.tag
                && x.pts.len() == y.pts.len()
                && x.pts.iter().zip(&y.pts).all(|(p, q)| p.near(*q, 1e-9))
                && x.rad.iter().zip(&y.rad).all(|(p, q)| (p - q).abs() < 1e-9)
        })
}

/// Every block reference's objects, placed in the drawing the way the reference places them.
fn expanded(s: &Session) -> Vec<Feat> {
    let d = s.doc().unwrap();
    let mut out = Vec::new();
    for e in d.model.iter() {
        if let EntityKind::Insert(i) = &e.kind {
            let b = d.block(&i.block).unwrap();
            let k = i.scale.x;
            let base = b.base.xy();
            for be in b.entities.iter() {
                let f = feat_of(&be.kind).unwrap();
                let at = |p: Vec2| i.insert.xy() + (p - base).rotate(i.rotation) * k;
                out.push(Feat { pts: f.pts.iter().map(|p| at(*p)).collect(), rad: f.rad.iter().map(|r| r * k).collect(), ..f });
            }
        }
    }
    out
}

#[test]
fn bconvert_turns_three_rotated_copies_into_one_block_and_three_references() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    loose(&mut s, Place { deg: 37.0, ..AT(20.0, 5.0) });
    loose(&mut s, Place { deg: 200.0, ..AT(-30.0, 10.0) });
    let before = sorted(feats(&s));
    let steps = s.state().unwrap().undo.len();

    let r = s.execute("bconvert", &json!({ "handles": hexes(&a), "name": "THING" })).unwrap();
    assert_eq!((r["block"].as_str(), r["converted"].as_u64()), (Some("THING"), Some(3)), "{r}");
    let d = s.doc().unwrap();
    assert_eq!(d.model.len(), 3, "twelve objects became three references");
    assert!(d.model.iter().all(|e| matches!(&e.kind, EntityKind::Insert(i) if i.block == "THING")));
    assert_eq!(d.block("THING").unwrap().entities.len(), 4);
    assert!(same(&sorted(expanded(&s)), &before), "the references reproduce the three copies exactly");
    assert_eq!(s.state().unwrap().undo.len(), steps + 1, "one undo step");

    s.undo().unwrap();
    assert_eq!(s.doc().unwrap().model.len(), 12);
    assert!(s.doc().unwrap().block("THING").is_none());
}

#[test]
fn bconvert_leaves_copies_that_differ_and_honours_scaled_and_layer0() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    s.execute("layer.new", &json!({"name": "WALLS"})).unwrap();
    for h in &a {
        s.doc_mut().unwrap().modify_entity(*h, |e| e.common.layer = "WALLS".into()).unwrap();
    }
    loose(&mut s, Place { k: 2.0, ..AT(20.0, 0.0) });
    loose(&mut s, Place { mirror: true, ..AT(40.0, 0.0) });
    loose(&mut s, Place { distort: true, ..AT(60.0, 0.0) });
    let r = s.execute("bconvert", &json!({ "handles": hexes(&a), "layer0": true })).unwrap();
    assert_eq!(r["converted"], 1, "scaled copies convert only on request; mirrored and distorted ones never: {r}");
    let name = r["block"].as_str().unwrap().to_string();
    assert!(s.doc().unwrap().block(&name).unwrap().entities.iter().all(|e| e.common.layer == "0"));
    assert!(
        s.doc().unwrap().model.iter().any(|e| matches!(e.kind, EntityKind::Insert(_)) && e.common.layer == "WALLS"),
        "the reference keeps the layer"
    );
    assert!(s.execute("bconvert", &json!({ "handles": hexes(&a), "name": name })).is_err(), "the block exists");

    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    loose(&mut s, Place { k: 2.0, deg: 90.0, ..AT(20.0, 0.0) });
    let r = s.execute("bconvert", &json!({ "handles": hexes(&a), "scaled": true })).unwrap();
    assert_eq!(r["converted"], 2);
    let scales: Vec<f64> =
        s.doc().unwrap().model.iter().filter_map(|e| if let EntityKind::Insert(i) = &e.kind { Some(i.scale.x) } else { None }).collect();
    assert!(scales.iter().any(|k| (k - 2.0).abs() < 1e-9) && scales.iter().any(|k| (k - 1.0).abs() < 1e-9), "{scales:?}");
}

#[test]
fn bconvert_with_include_existing_converts_references_of_other_blocks() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    let other = loose(&mut s, AT(50.0, 50.0));
    s.execute("-block", &json!({ "name": "OLD", "base": [50, 50], "handles": hexes(&other) })).unwrap();
    s.execute("insert", &json!({ "name": "OLD", "at": [10, 0], "rotation": 30 })).unwrap();
    // Without the option the reference is another block's, left alone.
    let mut plain = Session::new();
    let pa = loose(&mut plain, AT(0.0, 0.0));
    let po = loose(&mut plain, AT(50.0, 50.0));
    plain.execute("-block", &json!({ "name": "OLD", "base": [50, 50], "handles": hexes(&po) })).unwrap();
    plain.execute("insert", &json!({ "name": "OLD", "at": [10, 0], "rotation": 30 })).unwrap();
    assert_eq!(plain.execute("bconvert", &json!({ "handles": hexes(&pa) })).unwrap()["converted"], 1);

    let r = s.execute("bconvert", &json!({ "handles": hexes(&a), "includeExisting": true, "name": "NEW" })).unwrap();
    assert_eq!(r["converted"], 2, "{r}");
    assert!(s.doc().unwrap().model.iter().all(|e| matches!(&e.kind, EntityKind::Insert(i) if i.block == "NEW")));
}

#[test]
fn bsearch_finds_scaled_and_rotated_copies_but_not_mirrored_or_distorted_ones() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    let scaled = loose(&mut s, Place { k: 2.5, deg: 90.0, ..AT(20.0, 0.0) });
    let rotated = loose(&mut s, Place { deg: -123.0, ..AT(40.0, 7.0) });
    let mirrored = loose(&mut s, Place { mirror: true, ..AT(60.0, 0.0) });
    let distorted = loose(&mut s, Place { distort: true, ..AT(80.0, 0.0) });
    let n = s.doc().unwrap().model.len();

    let r = s.execute("bsearch", &json!({ "handles": hexes(&a) })).unwrap();
    assert_eq!(r["count"], 3, "{r}");
    let found: Vec<String> = r["handles"].as_array().unwrap().iter().map(|h| h.as_str().unwrap().to_string()).collect();
    for h in a.iter().chain(&scaled).chain(&rotated) {
        assert!(found.contains(&h.hex()), "{h:?} found");
    }
    for h in mirrored.iter().chain(&distorted) {
        assert!(!found.contains(&h.hex()), "{h:?} must not match");
    }
    assert_eq!(s.selection().len(), 12);
    let scaled_hit = r["instances"].as_array().unwrap().iter().find(|i| (i["scale"].as_f64().unwrap() - 2.5).abs() < 1e-9).unwrap();
    assert!((scaled_hit["rotation"].as_f64().unwrap() - 90.0).abs() < 1e-7, "{scaled_hit}");
    assert_eq!(s.doc().unwrap().model.len(), n, "the drawing is not changed");

    let r = s.execute("bsearch", &json!({ "handles": hexes(&a), "scaled": false })).unwrap();
    assert_eq!(r["count"], 2, "without scaling the 2.5x copy is out: {r}");
}

#[test]
fn bdetect_finds_the_repeated_group_among_noise_and_converts_it() {
    let mut s = Session::new();
    connected(&mut s, AT(0.0, 0.0));
    connected(&mut s, Place { deg: 61.0, ..AT(100.0, 40.0) });
    connected(&mut s, Place { deg: 245.0, ..AT(-80.0, 90.0) });
    // Noise: scattered single lines and circles, and one pair of touching lines that occurs once.
    let mut seed = 12345u64;
    let mut rnd = |m: f64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 33) as f64 / (1u64 << 31) as f64) * m
    };
    for _ in 0..30 {
        let (x, y) = (rnd(400.0) - 200.0, rnd(400.0) - 200.0);
        s.add_entity(EntityKind::Line(Line { a: Vec3::new(x, y, 0.0), b: Vec3::new(x + rnd(9.0), y + rnd(9.0), 0.0) })).unwrap();
        s.add_entity(EntityKind::Circle(Circle { center: Vec3::new(rnd(400.0) - 200.0, rnd(400.0) - 200.0, 0.0), radius: 1.0 + rnd(3.0) })).unwrap();
    }
    s.add_entity(EntityKind::Line(Line { a: Vec3::new(300.0, 300.0, 0.0), b: Vec3::new(310.0, 300.0, 0.0) })).unwrap();
    s.add_entity(EntityKind::Line(Line { a: Vec3::new(310.0, 300.0, 0.0), b: Vec3::new(310.0, 305.0, 0.0) })).unwrap();
    let n = s.doc().unwrap().model.len();

    let r = s.execute("bdetect", &json!({})).unwrap();
    let groups = r["groups"].as_array().unwrap();
    assert_eq!((groups[0]["count"].as_u64(), groups[0]["size"].as_u64()), (Some(3), Some(4)), "{r}");
    assert_eq!(s.doc().unwrap().model.len(), n, "detecting changes nothing");

    let r = s.execute("bdetect", &json!({ "convert": true, "name": "UNIT" })).unwrap();
    assert_eq!(r["converted"]["converted"], 3, "{r}");
    let d = s.doc().unwrap();
    assert_eq!(d.model.len(), n - 12 + 3);
    assert_eq!(d.block("UNIT").unwrap().entities.len(), 4);
    let refs: Vec<_> = d.model.iter().filter(|e| matches!(e.kind, EntityKind::Insert(_))).collect();
    assert_eq!(refs.len(), 3);
}

/// Defines block `name` from a fresh copy of `shape`.
fn define(s: &mut Session, name: &str, hs: &[Handle]) {
    s.execute("-block", &json!({ "name": name, "base": [0, 0], "handles": hexes(hs) })).unwrap();
}

#[test]
fn breplace_ranks_the_most_similar_block_first_and_keeps_the_placement() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    define(&mut s, "SAMPLE", &a);
    // The same shape three times the size, the shape without its arc and a lone circle.
    let big = loose(&mut s, Place { k: 3.0, ..AT(0.0, 0.0) });
    define(&mut s, "BIG", &big);
    let part = loose(&mut s, AT(0.0, 0.0));
    s.doc_mut().unwrap().remove_entity(part[3]);
    define(&mut s, "PART", &part[..3]);
    let c = s.add_entity(EntityKind::Circle(Circle { center: Vec3::ZERO, radius: 2.0 })).unwrap();
    define(&mut s, "FAR", &[c]);
    let r = s.execute("insert", &json!({ "name": "SAMPLE", "at": [10, 20], "rotation": 40, "scale": 1.5 })).unwrap();
    let h = r["handle"].as_str().unwrap().to_string();
    s.execute("layer.new", &json!({"name": "DOORS"})).unwrap();
    s.doc_mut().unwrap().modify_entity(Handle::parse_hex(&h).unwrap(), |e| e.common.layer = "DOORS".into()).unwrap();

    let r = s.execute("breplace", &json!({ "handles": [h] })).unwrap();
    let names: Vec<&str> = r["ranking"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["BIG", "PART", "FAR"], "{r}");
    assert!(r["ranking"][0]["distance"].as_f64().unwrap() < 1e-9);

    let r = s.execute("breplace", &json!({ "handles": [h], "with": "big" })).unwrap();
    assert_eq!(r["replaced"], 1);
    let e = s.doc().unwrap().entity(Handle::parse_hex(&h).unwrap()).unwrap().clone();
    let EntityKind::Insert(i) = &e.kind else { panic!("{e:?}") };
    assert_eq!(i.block, "BIG");
    assert_eq!((i.insert.x, i.insert.y, i.scale.x), (10.0, 20.0, 1.5));
    assert!((i.rotation - 40f64.to_radians()).abs() < 1e-12, "rotation kept");
    assert_eq!(e.common.layer, "DOORS", "layer kept");
    assert!(s.execute("breplace", &json!({ "handles": [h], "with": "NOPE" })).is_err());
    let before = s.doc().unwrap().model.len();
    assert!(s.execute("breplace", &json!({ "handles": [a[0].hex()] })).is_err(), "not a reference");
    assert_eq!(s.doc().unwrap().model.len(), before);
}

#[test]
fn breplace_prompt_lists_ranked_names_and_replaces() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    define(&mut s, "SAMPLE", &a);
    let b = loose(&mut s, Place { k: 2.0, ..AT(0.0, 0.0) });
    define(&mut s, "BIG", &b);
    let c = s.add_entity(EntityKind::Circle(Circle { center: Vec3::ZERO, radius: 2.0 })).unwrap();
    define(&mut s, "FAR", &[c]);
    let h = s.execute("insert", &json!({ "name": "SAMPLE", "at": [0, 0] })).unwrap()["handle"].as_str().unwrap().to_string();
    s.start("breplace").unwrap();
    s.input(crate::Input::Pick(vec![Handle::parse_hex(&h).unwrap()])).unwrap();
    s.cmdline("").unwrap();
    let p = s.current_prompt().unwrap();
    assert_eq!(p.keywords, ["BIG", "FAR"]);
    s.cmdline("far").unwrap();
    assert!(s.running.is_none());
    let EntityKind::Insert(i) = &s.doc().unwrap().entity(Handle::parse_hex(&h).unwrap()).unwrap().kind else { panic!() };
    assert_eq!(i.block, "FAR");
}

#[test]
fn dash_bconvert_asks_for_the_name() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    loose(&mut s, Place { deg: 10.0, ..AT(30.0, 0.0) });
    s.start("-bconvert").unwrap();
    s.input(crate::Input::Pick(a)).unwrap();
    s.cmdline("").unwrap();
    assert!(s.prompt_text().contains("Enter block name"), "{}", s.prompt_text());
    s.cmdline("PAIR").unwrap();
    assert!(s.running.is_none());
    assert!(s.doc().unwrap().block("PAIR").is_some());
    assert_eq!(s.doc().unwrap().model.len(), 2);
}

#[test]
fn placement_reproduces_the_learned_door_on_wall_offset() {
    let v = Vec2::new;
    // A wall drawn left to right, one right to left, one vertical drawn downwards.
    let segs = [(v(0.0, 0.0), v(10.0, 0.0)), (v(10.0, 5.0), v(0.0, 5.0)), (v(20.0, 10.0), v(20.0, 0.0))];
    let quarter = std::f64::consts::FRAC_PI_2;
    // Doors sit 0.15 off the left of a wall read towards +X (+Y when vertical), turned with it; one stray door.
    let refs = [(v(3.0, 0.15), 0.0), (v(7.0, 0.15), 0.0), (v(19.85, 6.0), quarter), (v(5.0, 2.3), 1.0)];
    let learned = learn(&refs, &segs);
    assert_eq!(learned.len(), 4);
    let g = suggest(v(5.0, 5.4), &segs, &learned, 1e-6).unwrap();
    assert_eq!(g.segment, 1);
    assert!(g.at.near(v(5.0, 5.15), 1e-9) && g.rotation.abs() < 1e-9 && g.support == 3, "{g:?}");
    let g = suggest(v(20.3, 2.0), &segs, &learned, 1e-6).unwrap();
    assert!(g.at.near(v(19.85, 2.0), 1e-9) && (g.rotation - quarter).abs() < 1e-9, "{g:?}");
    assert!((g.relation.offset - 0.15).abs() < 1e-9 && g.relation.angle.abs() < 1e-9, "{:?}", g.relation);
    // Nothing learned, no walls: no suggestion.
    assert!(suggest(v(0.0, 0.0), &segs, &[], 1e-6).is_none());
    assert!(suggest(v(0.0, 0.0), &[], &learned, 1e-6).is_none());
}

#[test]
fn insert_suggest_snaps_to_the_learned_relation() {
    let mut s = Session::new();
    let door = loose(&mut s, AT(0.0, 0.0));
    define(&mut s, "DOOR", &door);
    for (a, b) in [([0, 0], [10, 0]), ([0, 5], [10, 5])] {
        s.execute("line", &json!({ "points": [a, b] })).unwrap();
    }
    for x in [2.0, 6.0] {
        s.execute("insert", &json!({ "name": "DOOR", "at": [x, 0.25], "rotation": 90 })).unwrap();
    }
    let r = s.execute("insert", &json!({ "name": "DOOR", "at": [4.4, 5.9], "suggest": true })).unwrap();
    assert_eq!(r["suggested"], true);
    let EntityKind::Insert(i) = &s.doc().unwrap().entity(Handle::parse_hex(r["handle"].as_str().unwrap()).unwrap()).unwrap().kind else { panic!() };
    assert!((i.insert.x - 4.4).abs() < 1e-9 && (i.insert.y - 5.25).abs() < 1e-9 && (i.rotation - 90f64.to_radians()).abs() < 1e-9, "{i:?}");
    // An explicit rotation wins; with nothing learned the point is used as given.
    let r = s.execute("insert", &json!({ "name": "DOOR", "at": [1, 4], "rotation": 10, "suggest": true })).unwrap();
    let EntityKind::Insert(i) = &s.doc().unwrap().entity(Handle::parse_hex(r["handle"].as_str().unwrap()).unwrap()).unwrap().kind else { panic!() };
    assert!((i.rotation - 10f64.to_radians()).abs() < 1e-12 && (i.insert.y - 5.25).abs() < 1e-9);
    let circle = s.execute("circle", &json!({"center": [0, 0], "radius": 1})).unwrap()["handle"].clone();
    s.execute("-block", &json!({ "name": "ALONE", "base": [0, 0], "handles": [circle] })).unwrap();
    let r = s.execute("insert", &json!({ "name": "ALONE", "at": [7, 7], "suggest": true })).unwrap();
    assert_eq!(r["suggested"], false);
}

#[test]
fn hostile_params_never_panic() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    loose(&mut s, Place { deg: 10.0, ..AT(30.0, 0.0) });
    // Objects with NaN / enormous coordinates and long polylines.
    for x in [f64::NAN, f64::INFINITY, 1e300, -1e300] {
        let _ = s.add_entity(EntityKind::Line(Line { a: Vec3::new(x, 0.0, 0.0), b: Vec3::new(1.0, x, 0.0) }));
        let _ = s.add_entity(EntityKind::Circle(Circle { center: Vec3::new(x, x, 0.0), radius: x }));
        let _ = s.add_entity(EntityKind::Arc(DocArc { center: Vec3::ZERO, radius: x, start: x, end: 1.0 }));
    }
    let vertices: Vec<PolyVertex> = (0..2000).map(|i| PolyVertex { bulge: f64::NAN, ..PolyVertex::new(Vec2::new(i as f64, 0.0)) }).collect();
    let _ = s.add_entity(EntityKind::LwPolyline(LwPolyline { vertices, closed: true, const_width: 0.0, elevation: 0.0, plinegen: false }));
    // Hatches, splines and dimensions with hostile numbers, counts and shapes.
    let nan_loop = |n: usize, x: f64| HatchLoop {
        vertices: (0..n).map(|i| PolyVertex { bulge: x, start_width: x, ..PolyVertex::new(Vec2::new(i as f64, x)) }).collect(),
        outer: true,
        sources: Vec::new(),
    };
    for (loops, x) in [
        (vec![nan_loop(3, f64::NAN)], 0.0),
        (vec![nan_loop(5000, 1.0)], 1.0),
        ((0..40).map(|_| nan_loop(3, 1.0)).collect(), 1.0),
        (Vec::new(), 1.0),
        (vec![nan_loop(1, 1e300), nan_loop(0, 0.0)], 1e300),
    ] {
        let mut h = hatch("ANSI31", x, x);
        h.loops = loops;
        let _ = s.add_entity(EntityKind::Hatch(h));
    }
    for x in [f64::NAN, 1e300] {
        let mut sp = Spline::from_control(vec![Vec2::new(0.0, 0.0), Vec2::new(1.0, x), Vec2::new(2.0, 0.0), Vec2::new(3.0, 1.0)], 3);
        sp.weights = vec![x];
        sp.knots = vec![x, 0.0];
        let _ = s.add_entity(EntityKind::Spline(sp));
    }
    let mut huge = Spline::from_control((0..600).map(|i| Vec2::new(i as f64, 0.0)).collect(), 3);
    huge.fit = vec![Vec2::ZERO; 600];
    let _ = s.add_entity(EntityKind::Spline(huge));
    for x in [f64::NAN, 1e300] {
        if let EntityKind::Dimension(mut d) = linear_dim("", "Standard", (x, 1.0)) {
            d.kind = DimKind::Linear { rotation: x };
            let _ = s.add_entity(EntityKind::Dimension(d));
        }
    }
    let bad: Vec<Value> = vec![
        json!(null),
        json!({}),
        json!({"handles": 5}),
        json!({"handles": ["zz", null, -1, 1e40]}),
        json!({"handles": hexes(&a), "tolerance": "x"}),
        json!({"handles": hexes(&a), "tolerance": -1}),
        json!({"handles": hexes(&a), "tolerance": 1e300}),
        json!({"handles": hexes(&a), "tolerance": 0}),
        json!({"handles": hexes(&a), "name": "a*b"}),
        json!({"handles": hexes(&a), "base": "nope"}),
        json!({"handles": hexes(&a), "base": [1e308, -1e308], "scaled": true}),
        json!({"handles": hexes(&a), "name": 5, "includeExisting": "yes"}),
        json!({"maxSize": -3, "minSize": 1e30, "gap": 1e300, "convert": true, "index": 1e30}),
        json!({"convert": true, "index": 99}),
        json!({"gap": 0.0}),
        json!({"gap": -1}),
        json!({"with": 5, "handles": hexes(&a)}),
        json!({"suggest": true}),
        json!({"handles": hexes(&a), "dryRun": true, "name": 5, "base": [f64::NAN, 1]}),
        json!({"handles": hexes(&a), "dryRun": "yes", "shapes": "yes"}),
        json!({"handles": hexes(&a), "shapes": true}),
        json!({"handles": hexes(&a), "shapes": true, "with": "NOPE"}),
        json!({"handles": [], "shapes": true}),
    ];
    for id in ["bconvert", "-bconvert", "bsearch", "bdetect", "breplace"] {
        for p in &bad {
            let _ = s.execute(id, p);
        }
    }
    let _ = s.execute("insert", &json!({"name": "NOPE", "at": [1e308, 0], "suggest": true}));
    let _ = s.execute("insert", &json!({"name": "NOPE", "at": [f64::NAN, 0], "suggest": 1}));
    // The whole drawing as a sample is over the cap: an error, not a hang.
    let all: Vec<String> = s.doc().unwrap().model.iter().map(|e| e.handle.hex()).collect();
    assert!(s.execute("bsearch", &json!({ "handles": all })).is_err());
    // Placement on hostile segments and relations.
    let nan = Vec2::new(f64::NAN, 1.0);
    let _ = suggest(Vec2::ZERO, &[(nan, Vec2::ZERO), (Vec2::ZERO, Vec2::ZERO)], &learn(&[(nan, f64::NAN)], &[(Vec2::ZERO, Vec2::X)]), f64::NAN);
}

#[test]
fn many_unrelated_lines_stay_within_budget() {
    let mut s = Session::new();
    for i in 0..3000 {
        let x = f64::from(i) * 3.0;
        s.add_entity(EntityKind::Line(Line { a: Vec3::new(x, 0.0, 0.0), b: Vec3::new(x + 1.0, f64::from(i % 7), 0.0) })).unwrap();
    }
    let r = s.execute("bdetect", &json!({})).unwrap();
    assert!(r["groups"].is_array());
}

// ---------- hatches, splines, dimensions, polylines with bulges and widths ----------

use std::collections::BTreeSet;

use cadcraft_doc::{DimKind, DimMarks, Dimension, Hatch, HatchLoop};
use cadcraft_geom::{Mat3, Spline};

/// Scale by `k`, turn by `deg`, move by `t`; mirrored across the Y axis first when `mirror`.
fn xform(k: f64, deg: f64, t: (f64, f64), mirror: bool) -> Mat3 {
    let m = if mirror { Mat3::mirror(Vec2::ZERO, Vec2::new(0.0, 1.0)) } else { Mat3::scale(1.0, 1.0) };
    m.then(Mat3::scale(k, k)).then(Mat3::rotate(deg.to_radians())).then(Mat3::translate(Vec2::new(t.0, t.1)))
}

/// A copy of `kind` carried by `m`, as MOVE / ROTATE / SCALE / MIRROR carry it.
fn put(s: &mut Session, kind: &EntityKind, m: &Mat3) -> Handle {
    let mut k = kind.clone();
    k.transform(m);
    s.add_entity(k).unwrap()
}

fn handle_set(v: &Value) -> BTreeSet<String> {
    v.as_array().unwrap().iter().map(|h| h.as_str().unwrap().to_string()).collect()
}

/// BSEARCH from `sample` finds its copies (turned, moved, scaled and `also`: the same shape
/// built another way), never a mirrored copy or any of the `different` ones; BCONVERT turns
/// exactly those into references.
fn assert_copies_found(sample: EntityKind, different: &[EntityKind], also: &[EntityKind]) {
    let mut s = Session::new();
    let h = s.add_entity(sample.clone()).unwrap();
    let mut want =
        vec![h, put(&mut s, &sample, &xform(1.0, 33.0, (200.0, 50.0), false)), put(&mut s, &sample, &xform(2.0, -100.0, (-300.0, 400.0), false))];
    for (i, a) in also.iter().enumerate() {
        want.push(put(&mut s, a, &xform(1.0, 10.0, (500.0 + 100.0 * i as f64, 500.0), false)));
    }
    let mut other = vec![put(&mut s, &sample, &xform(1.0, 0.0, (800.0, 0.0), true))];
    for (i, d) in different.iter().enumerate() {
        other.push(put(&mut s, d, &xform(1.0, 70.0, (900.0, 900.0 + 100.0 * i as f64), false)));
    }
    let r = s.execute("bsearch", &json!({ "handles": [h.hex()], "scaled": true })).unwrap();
    let found = handle_set(&r["handles"]);
    assert_eq!(found, want.iter().map(|h| h.hex()).collect::<BTreeSet<_>>(), "{r}");
    assert_eq!(r["count"], want.len());

    let r = s.execute("bconvert", &json!({ "handles": [h.hex()], "name": "SHAPE", "scaled": true })).unwrap();
    assert_eq!(r["converted"], want.len(), "{r}");
    let d = s.doc().unwrap();
    assert_eq!(d.model.len(), want.len() + other.len());
    assert_eq!(d.model.iter().filter(|e| matches!(&e.kind, EntityKind::Insert(i) if i.block == "SHAPE")).count(), want.len());
    for o in other {
        assert!(d.entity(o).is_some_and(|e| !matches!(e.kind, EntityKind::Insert(_))), "an unlike object stays as drawn");
    }
}

fn pv(x: f64, y: f64, bulge: f64, w: (f64, f64)) -> PolyVertex {
    PolyVertex { p: Vec2::new(x, y), bulge, start_width: w.0, end_width: w.1 }
}

fn lw(vertices: Vec<PolyVertex>, closed: bool) -> EntityKind {
    EntityKind::LwPolyline(LwPolyline { vertices, closed, const_width: 0.0, elevation: 0.0, plinegen: false })
}

#[test]
fn bulge_and_width_polylines_match_by_shape_and_never_mirrored() {
    let verts = vec![pv(0.0, 0.0, 0.5, (0.2, 0.4)), pv(4.0, 0.0, -0.3, (0.1, 0.1)), pv(5.0, 3.0, 0.0, (0.0, 0.0))];
    let sample = lw(verts.clone(), false);
    // A different bulge, a different width, a closed one, and the same polyline drawn from its other end.
    let mut bulge = verts.clone();
    bulge[0].bulge = 0.6;
    let mut width = verts.clone();
    width[1].end_width = 0.3;
    let reversed = lw(vec![pv(5.0, 3.0, 0.3, (0.1, 0.1)), pv(4.0, 0.0, -0.5, (0.4, 0.2)), pv(0.0, 0.0, 0.0, (0.0, 0.0))], false);
    assert_copies_found(sample, &[lw(bulge, false), lw(width, false), lw(verts, true)], &[reversed]);
}

fn hatch(pattern: &str, scale: f64, angle: f64) -> Hatch {
    let outer = [(0.0, 0.0), (6.0, 0.0), (6.0, 2.0), (2.0, 4.0), (0.0, 3.0)];
    let inner = [(1.0, 0.5), (2.0, 0.5), (2.0, 1.5), (1.0, 1.5)];
    let l = |pts: &[(f64, f64)], outer: bool| HatchLoop {
        vertices: pts.iter().map(|p| PolyVertex::new(Vec2::new(p.0, p.1))).collect(),
        outer,
        sources: Vec::new(),
    };
    Hatch {
        pattern: pattern.into(),
        solid: false,
        loops: vec![l(&outer, true), l(&inner, false)],
        scale,
        angle,
        associative: false,
        style: 0,
        elevation: 0.0,
        gradient: None,
        origin: Vec2::ZERO,
        background: None,
        pattern_lines: Vec::new(),
    }
}

#[test]
fn hatches_match_by_loops_pattern_scale_and_angle_relative_to_the_transform() {
    let sample = hatch("ANSI31", 1.5, 0.3);
    let other_pattern = hatch("ANSI37", 1.5, 0.3);
    let other_scale = hatch("ANSI31", 1.0, 0.3);
    let other_angle = hatch("ANSI31", 1.5, 0.9);
    let mut moved_loop = hatch("ANSI31", 1.5, 0.3);
    moved_loop.loops[1].vertices[2].p = Vec2::new(2.0, 1.2);
    let mut one_loop = hatch("ANSI31", 1.5, 0.3);
    one_loop.loops.truncate(1);
    // The inner loop listed first and read from another vertex is the same hatch.
    let mut shuffled = hatch("ANSI31", 1.5, 0.3);
    shuffled.loops.reverse();
    shuffled.loops[1].vertices.rotate_left(2);
    let kinds = |h: Hatch| EntityKind::Hatch(h);
    assert_copies_found(kinds(sample), &[kinds(other_pattern), kinds(other_scale), kinds(other_angle), kinds(moved_loop), kinds(one_loop)], &[]);
    // The shuffled loop order has another first loop, so it is not found from the sample; searching from it finds the sample.
    let mut s = Session::new();
    let a = s.add_entity(kinds(hatch("ANSI31", 1.5, 0.3))).unwrap();
    let b = s.add_entity(kinds(shuffled)).unwrap();
    let r = s.execute("bsearch", &json!({ "handles": [b.hex()] })).unwrap();
    assert!(handle_set(&r["handles"]).contains(&b.hex()), "{r}");
    let _ = a;
}

#[test]
fn splines_match_by_control_points_knots_and_fit_points_from_either_end() {
    let control = vec![Vec2::new(0.0, 0.0), Vec2::new(1.0, 2.0), Vec2::new(3.0, 3.0), Vec2::new(5.0, 1.0), Vec2::new(6.0, 0.0)];
    let sample = Spline::from_control(control.clone(), 3);
    let mut moved = sample.clone();
    moved.control[2] = Vec2::new(3.0, 3.5);
    let mut degree2 = Spline::from_control(control.clone(), 2);
    degree2.fit.clear();
    let mut weighted = sample.clone();
    weighted.weights = vec![1.0, 1.0, 2.0, 1.0, 1.0];
    let mut fit = sample.clone();
    fit.fit = vec![Vec2::new(0.0, 0.0), Vec2::new(6.0, 0.0)];
    let mut backwards = control.clone();
    backwards.reverse();
    let reversed = Spline::from_control(backwards, 3);
    let k = EntityKind::Spline;
    assert_copies_found(k(sample), &[k(moved), k(degree2), k(weighted), k(fit)], &[k(reversed)]);
}

fn linear_dim(text: &str, style: &str, defpt: (f64, f64)) -> EntityKind {
    EntityKind::Dimension(Dimension {
        kind: DimKind::Linear { rotation: 0.0 },
        defpt: Vec3::new(defpt.0, defpt.1, 0.0),
        text_mid: Vec3::ZERO,
        p13: Vec3::new(0.0, 0.0, 0.0),
        p14: Vec3::new(5.0, 0.0, 0.0),
        p15: Vec3::ZERO,
        p16: Vec3::ZERO,
        text: text.into(),
        style: style.into(),
        measurement: 5.0,
        text_rotation: 0.0,
        user_text_pos: false,
        block: None,
        overrides: serde_json::Map::new(),
        assoc: Vec::new(),
        marks: DimMarks::default(),
    })
}

#[test]
fn dimensions_match_by_definition_points_type_style_and_text() {
    let sample = linear_dim("", "Standard", (5.0, 2.0));
    let mut overrides = linear_dim("", "Standard", (5.0, 2.0));
    if let EntityKind::Dimension(d) = &mut overrides {
        d.overrides.insert("dimtxt".into(), json!(2.5));
    }
    let mut aligned = linear_dim("", "Standard", (5.0, 2.0));
    if let EntityKind::Dimension(d) = &mut aligned {
        d.kind = DimKind::Aligned;
    }
    assert_copies_found(
        sample,
        &[
            linear_dim("<> typ.", "Standard", (5.0, 2.0)),
            linear_dim("", "Architectural", (5.0, 2.0)),
            linear_dim("", "Standard", (5.0, 3.0)),
            overrides,
            aligned,
        ],
        &[],
    );
}

// ---------- T-junctions ----------

#[test]
fn bdetect_links_a_wall_ending_on_another_walls_side() {
    let mut s = Session::new();
    let line = |s: &mut Session, m: &Mat3, a: (f64, f64), b: (f64, f64)| {
        let k = EntityKind::Line(Line { a: Vec3::new(a.0, a.1, 0.0), b: Vec3::new(b.0, b.1, 0.0) });
        put(s, &k, m)
    };
    // A T: a bar and a stem whose end lies on the middle of the bar; twice, the second turned.
    let mut want = BTreeSet::new();
    for m in [xform(1.0, 0.0, (0.0, 0.0), false), xform(1.0, 90.0, (100.0, 40.0), false)] {
        want.insert(line(&mut s, &m, (0.0, 0.0), (10.0, 0.0)).hex());
        want.insert(line(&mut s, &m, (4.0, 0.0), (4.0, 6.0)).hex());
    }
    // Noise that touches nothing, and a lone bar.
    for i in 0..5 {
        let x = 300.0 + 20.0 * f64::from(i);
        s.execute("line", &json!({ "points": [[x, 0], [x + 3.0, 7.0 + f64::from(i)]] })).unwrap();
    }
    let r = s.execute("bdetect", &json!({})).unwrap();
    let g = &r["groups"][0];
    assert_eq!((g["count"].as_u64(), g["size"].as_u64()), (Some(2), Some(2)), "{r}");
    let found: BTreeSet<String> = g["instances"].as_array().unwrap().iter().flat_map(|i| handle_set(&i["handles"])).collect();
    assert_eq!(found, want);
    // The gap is what decides: a stem that stops short of the bar is no T.
    let mut s = Session::new();
    s.execute("line", &json!({ "points": [[0, 0], [10, 0]] })).unwrap();
    s.execute("line", &json!({ "points": [[4, 0.5], [4, 6]] })).unwrap();
    s.execute("line", &json!({ "points": [[50, 0], [60, 0]] })).unwrap();
    s.execute("line", &json!({ "points": [[54, 0.5], [54, 6]] })).unwrap();
    let r = s.execute("bdetect", &json!({})).unwrap();
    assert_eq!(r["groups"], json!([]), "{r}");
    let r = s.execute("bdetect", &json!({ "gap": 1.0 })).unwrap();
    assert_eq!(r["groups"][0]["count"], 2, "{r}");
}

// ---------- placement along the wall ----------

#[test]
fn placement_reproduces_a_door_learned_300_from_a_wall_corner() {
    let v = Vec2::new;
    let quarter = std::f64::consts::FRAC_PI_2;
    // Walls in millimetres: two meeting at the origin, and a third one apart.
    let segs = [(v(0.0, 0.0), v(5000.0, 0.0)), (v(0.0, 0.0), v(0.0, 4000.0)), (v(6000.0, 0.0), v(6000.0, 4000.0))];
    // A door 300 from the corner along each wall, 100 to the left of the wall read towards +X (+Y).
    let refs = [(v(300.0, 100.0), 0.0), (v(-100.0, 300.0), quarter)];
    let learned = learn(&refs, &segs);
    assert!(learned.iter().all(|r| (r.along - 300.0).abs() < 1e-9 && (r.offset - 100.0).abs() < 1e-9), "{learned:?}");
    // Near the new wall's start the door sits 300 from it; near its far end, 300 from that end.
    let g = suggest(v(6050.0, 900.0), &segs, &learned, 1e-6).unwrap();
    assert!(g.anchored && g.at.near(v(5900.0, 300.0), 1e-9) && (g.rotation - quarter).abs() < 1e-9, "{g:?}");
    let g = suggest(v(6050.0, 3500.0), &segs, &learned, 1e-6).unwrap();
    assert!(g.at.near(v(5900.0, 3700.0), 1e-9), "{g:?}");
    // Doors that sit at different distances from the corners follow the cursor along the wall.
    let loose = learn(&[(v(300.0, 100.0), 0.0), (v(900.0, 100.0), 0.0)], &segs);
    let g = suggest(v(2000.0, 40.0), &segs, &loose, 1e-6).unwrap();
    assert!(!g.anchored && g.at.near(v(2000.0, 100.0), 1e-9), "{g:?}");
}

#[test]
fn placement_measures_from_a_wall_that_meets_the_wall_in_the_middle() {
    let v = Vec2::new;
    // A long wall, and a partition meeting it at x = 4000.
    let segs = [(v(0.0, 0.0), v(8000.0, 0.0)), (v(4000.0, 0.0), v(4000.0, 3000.0))];
    let learned = learn(&[(v(4300.0, 100.0), 0.0)], &segs);
    assert!((learned[0].along - 300.0).abs() < 1e-9, "{learned:?}");
    // Either side of the partition is 300 away from it; the cursor's side wins.
    let g = suggest(v(3800.0, 20.0), &segs, &learned, 1e-6).unwrap();
    assert!(g.at.near(v(3700.0, 100.0), 1e-9), "{g:?}");
    let g = suggest(v(4200.0, 20.0), &segs, &learned, 1e-6).unwrap();
    assert!(g.at.near(v(4300.0, 100.0), 1e-9), "{g:?}");
}

#[test]
fn placement_picks_the_learned_relation_closest_to_the_cursor() {
    let v = Vec2::new;
    let segs = [(v(0.0, 0.0), v(10.0, 0.0))];
    // Doors on both sides of the wall, two on the left and two on the right; one odd one on the left.
    let refs =
        [(v(2.0, 0.2), 0.0), (v(4.0, 0.2), 0.0), (v(3.0, -0.3), std::f64::consts::PI), (v(6.0, -0.3), std::f64::consts::PI), (v(8.0, 1.5), 0.5)];
    let learned = learn(&refs, &segs);
    let above = suggest(v(5.0, 0.6), &segs, &learned, 1e-6).unwrap();
    assert!((above.relation.offset - 0.2).abs() < 1e-9 && above.at.near(v(5.0, 0.2), 1e-9), "{above:?}");
    let below = suggest(v(5.0, -0.7), &segs, &learned, 1e-6).unwrap();
    assert!((below.relation.offset + 0.3).abs() < 1e-9 && below.at.near(v(5.0, -0.3), 1e-9) && below.support == 2, "{below:?}");
    // The one odd door is as common as half the most common relation, so it counts; alone, it fixes its distance from a wall end (2 or 8 from either end).
    let odd = suggest(v(2.1, 1.4), &segs, &learned, 1e-6).unwrap();
    assert!((odd.relation.offset - 1.5).abs() < 1e-9 && odd.support == 1 && odd.anchored, "{odd:?}");
    assert!(odd.at.near(v(2.0, 1.5), 1e-9) || odd.at.near(v(8.0, 1.5), 1e-9), "{odd:?}");
}

// ---------- rotation-invariant ranking ----------

#[test]
fn breplace_ranking_does_not_change_when_the_sample_is_turned() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    define(&mut s, "SAMPLE", &a);
    let turned = loose(&mut s, Place { deg: 37.0, ..AT(0.0, 0.0) });
    define(&mut s, "TURNED", &turned);
    let part = loose(&mut s, AT(0.0, 0.0));
    s.doc_mut().unwrap().remove_entity(part[3]);
    define(&mut s, "PART", &part[..3]);
    let c = s.add_entity(EntityKind::Circle(Circle { center: Vec3::ZERO, radius: 2.0 })).unwrap();
    define(&mut s, "FAR", &[c]);
    let h = s.execute("insert", &json!({ "name": "SAMPLE", "at": [10, 20] })).unwrap()["handle"].as_str().unwrap().to_string();
    let h2 = s.execute("insert", &json!({ "name": "TURNED", "at": [10, 40] })).unwrap()["handle"].as_str().unwrap().to_string();
    let r = s.execute("breplace", &json!({ "handles": [h] })).unwrap();
    let names: Vec<&str> = r["ranking"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names[0], "TURNED", "{r}");
    assert!(r["ranking"][0]["distance"].as_f64().unwrap() < 1e-6, "a turned copy is the same shape: {r}");
    // Seen from the turned copy the sample is as near as it was.
    let r2 = s.execute("breplace", &json!({ "handles": [h2] })).unwrap();
    assert_eq!(r2["ranking"][0]["name"], "SAMPLE", "{r2}");
    let dist = |r: &Value, n: &str| r["ranking"].as_array().unwrap().iter().find(|e| e["name"] == n).unwrap()["distance"].as_f64().unwrap();
    // SAMPLE and TURNED are one shape: every other block is as far from the one as from the other.
    for n in ["PART", "FAR"] {
        let (from_sample, from_turned) = (dist(&r, n), dist(&r2, n));
        assert!((from_sample - from_turned).abs() < 0.01, "{n}: {from_sample} vs {from_turned}");
    }
}

#[test]
fn breplace_shapes_ranks_blocks_by_the_selected_objects() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    define(&mut s, "LOOSE", &a);
    let c = s.add_entity(EntityKind::Circle(Circle { center: Vec3::ZERO, radius: 2.0 })).unwrap();
    define(&mut s, "RING", &[c]);
    let sel = loose(&mut s, Place { deg: 50.0, k: 2.0, ..AT(100.0, 100.0) });
    let r = s.execute("breplace", &json!({ "handles": hexes(&sel), "shapes": true })).unwrap();
    assert_eq!(r["ranking"][0]["name"], "LOOSE", "{r}");
    assert_eq!(r["ranking"][1]["name"], "RING", "{r}");
    // Without `shapes` objects that are not references are refused, as before; so are `with` and no references.
    assert!(s.execute("breplace", &json!({ "handles": hexes(&sel) })).is_err());
    assert!(s.execute("breplace", &json!({ "handles": hexes(&sel), "shapes": true, "with": "RING" })).is_err());
    assert!(s.execute("breplace", &json!({ "handles": [], "shapes": true })).is_err());
}

#[test]
fn bconvert_dry_run_counts_without_changing_the_drawing() {
    let mut s = Session::new();
    let a = loose(&mut s, AT(0.0, 0.0));
    loose(&mut s, Place { deg: 37.0, ..AT(20.0, 5.0) });
    let steps = s.state().unwrap().undo.len();
    let before = s.doc().unwrap().model.len();
    let r = s.execute("bconvert", &json!({ "handles": hexes(&a), "dryRun": true })).unwrap();
    assert_eq!((r["dryRun"].as_bool(), r["instances"].as_u64()), (Some(true), Some(2)), "{r}");
    // The name need not be valid for a count.
    assert!(s.execute("bconvert", &json!({ "handles": hexes(&a), "dryRun": true, "name": "a*b" })).is_ok());
    assert_eq!((s.doc().unwrap().model.len(), s.state().unwrap().undo.len()), (before, steps));
    assert!(s.doc().unwrap().blocks.values().all(|b| b.anonymous || b.name.starts_with('*')));
}
