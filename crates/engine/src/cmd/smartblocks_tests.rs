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
