use cadcraft_color::Color;
use cadcraft_doc::{EntityKind, Handle};
use serde_json::{Value, json};

use crate::Session;

const OBJS: [&str; 7] = [
    "<< /Type /Catalog /Pages 2 0 R >>",
    "<< /Type /Pages /Count 1 /Kids [3 0 R] /MediaBox [0 0 200 100] >>",
    "<< /Type /Page /Parent 2 0 R /Annots [4 0 R 5 0 R 6 0 R 7 0 R] >>",
    "<< /Type /Annot /Subtype /FreeText /Rect [20 60 120 90] /Contents (Check this) /DA (0 0 1 rg /Helv 10 Tf) /C [1 0 0] >>",
    "<< /Type /Annot /Subtype /Square /Rect [10 10 60 40] /BE << /S /C /I 1 >> >>",
    "<< /Type /Annot /Subtype /Ink /InkList [[10 10 20 20 30 10]] >>",
    "<< /Type /Annot /Subtype /Line /L [100 20 150 20] /LE [/None /OpenArrow] /Contents (See here) >>",
];

/// A file of its own per call: the tests run in parallel.
fn tmp(name: &str, bytes: &[u8]) -> String {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("cadcraft-markup-{}-{n}-{name}", std::process::id()));
    std::fs::write(&p, bytes).unwrap();
    p.to_string_lossy().into_owned()
}

fn pdf() -> String {
    let mut f = b"%PDF-1.4\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in OBJS.iter().enumerate() {
        offs.push(f.len());
        f.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
    }
    let xref = f.len();
    f.extend(format!("xref\n0 {}\n0000000000 65535 f \n", OBJS.len() + 1).bytes());
    for o in offs {
        f.extend(format!("{o:010} 00000 n \n").bytes());
    }
    f.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", OBJS.len() + 1).bytes());
    tmp("review.pdf", &f)
}

/// A PNG header (the markup importer reads only the size).
fn png(w: u32, h: u32) -> String {
    let mut b = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    tmp("review.png", &b)
}

fn model(s: &Session) -> Vec<(Handle, String, EntityKind)> {
    // The paper under the markups is checked on its own (see `markup_paper_tests`).
    s.doc()
        .unwrap()
        .model
        .iter()
        .filter(|e| !matches!(e.kind, EntityKind::Image(_)))
        .map(|e| (e.handle, e.common.layer.clone(), e.kind.clone()))
        .collect()
}

fn imported(s: &mut Session) -> Value {
    s.execute("markupimport", &json!({"path": pdf(), "at": [1000, 500], "width": 400})).unwrap()
}

#[test]
fn pdf_import_scales_the_page_to_width_at_the_point() {
    let mut s = Session::new();
    let r = imported(&mut s);
    assert_eq!(r["count"], 4, "{r}");
    assert_eq!(r["skipped"], json!([]));
    let objs = model(&s);
    assert!(objs.iter().all(|(_, l, _)| l == "MARKUP"));
    assert_eq!(s.doc().unwrap().layer("MARKUP").unwrap().color, Color::Index(1));
    // The 200 x 100 pt page is 400 x 200 units, its lower-left corner at (1000, 500): the text's
    // top left (0.1, 0.1) is at (1040, 680), and its height 0.1 of the page is 20.
    let EntityKind::MText(m) = &objs[0].2 else { panic!("{:?}", objs[0].2) };
    assert_eq!((m.insert.x, m.insert.y, m.height, m.contents.as_str()), (1040.0, 680.0, 20.0, "Check this"));
    let e = s.doc().unwrap().entity(objs[0].0).unwrap();
    assert_eq!(super::tagged(e), Some(("text".into(), "Check this".into())));
    assert!(matches!(&e.common.color, Color::True(_)), "the annotation's own colour is kept");
    let EntityKind::LwPolyline(p) = &objs[2].2 else { panic!("{:?}", objs[2].2) };
    assert_eq!((p.closed, p.vertices.len(), p.vertices[0].p.x, p.vertices[0].p.y), (false, 3, 1020.0, 520.0));
    s.undo().unwrap();
    assert!(model(&s).is_empty(), "one undo step takes the import back");
}

#[test]
fn default_width_is_the_page_size_in_points() {
    let mut s = Session::new();
    s.execute("markupimport", &json!({"path": pdf(), "layer": "REVIEW"})).unwrap();
    let EntityKind::MText(m) = &model(&s)[0].2 else { panic!() };
    assert_eq!((m.insert.x, m.insert.y, m.height), (20.0, 90.0, 10.0));
    assert!(model(&s).iter().all(|(_, l, _)| l == "REVIEW"));
}

#[test]
fn images_need_supplied_markups() {
    let mut s = Session::new();
    let path = png(640, 480);
    let e = s.execute("markupimport", &json!({"path": path})).unwrap_err().to_string();
    assert!(e.contains("need recognition"), "{e}");
    let r = s
        .execute(
            "markupimport",
            &json!({"path": path, "markups": [
                {"kind": "text", "text": "Move door", "at": [0.5, 0.5], "height": 0.1},
                {"kind": "ellipse", "points": [[0.0, 0.0], [0.5, 1.0]]},
                {"kind": "sparkle"},
                {"kind": "cloud", "points": [[0, 0], [1, 1]]},
            ]}),
        )
        .unwrap();
    assert_eq!(r["count"], 2);
    assert_eq!(r["skipped"].as_array().unwrap().len(), 2, "{r}");
    let objs = model(&s);
    let EntityKind::MText(m) = &objs[0].2 else { panic!() };
    // The image is 640 x 480 pixels, one unit each.
    assert_eq!((m.insert.x, m.insert.y, m.height), (320.0, 240.0, 48.0));
    let EntityKind::Ellipse(el) = &objs[1].2 else { panic!() };
    assert_eq!((el.center.x, el.center.y, el.major.x, el.ratio), (160.0, 240.0, 0.0, 160.0 / 240.0));
}

#[test]
fn assist_lists_then_converts_and_removes_the_markups() {
    let mut s = Session::new();
    imported(&mut s);
    let list = s.execute("markupassist", &json!({})).unwrap();
    let kinds: Vec<&str> = list["markups"].as_array().unwrap().iter().map(|m| m["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["text", "cloud", "polyline", "leader"]);
    assert_eq!(list["markups"][3]["text"], "See here");
    assert_eq!(model(&s).len(), 4, "listing changes nothing");

    let r = s.execute("markupassist", &json!({"all": true, "as": "mtext", "layer": "NOTES"})).unwrap();
    assert_eq!(r["converted"], 4, "{r}");
    let objs = model(&s);
    assert_eq!(objs.len(), 4, "the markups were replaced, not kept");
    assert!(objs.iter().all(|(_, l, _)| l == "NOTES"));
    assert!(s.doc().unwrap().model.iter().all(|e| super::tagged(e).is_none()));
    let EntityKind::MText(m) = &objs[0].2 else { panic!("{:?}", objs[0].2) };
    assert_eq!(m.contents, "Check this");
    let EntityKind::LwPolyline(cloud) = &objs[1].2 else { panic!() };
    assert!(cloud.closed && cloud.vertices.len() > 4 && cloud.vertices.iter().all(|v| v.bulge != 0.0), "a revision cloud");
    let EntityKind::MLeader(ml) = &objs[3].2 else { panic!("{:?}", objs[3].2) };
    assert_eq!(ml.text.as_ref().map(|t| t.contents.as_str()), Some("See here"));
    // Arrow at the line's end: the leader starts at its page point (150, 20), (0.75, 0.8).
    assert_eq!((ml.leaders[0][0].x, ml.leaders[0][0].y), (1300.0, 540.0));

    s.undo().unwrap();
    let again = s.execute("markupassist", &json!({})).unwrap();
    assert_eq!(again["count"], 4, "one undo step brings the markups back");
    assert_eq!(model(&s).len(), 4);
}

#[test]
fn assist_text_as_text_and_mleader_and_keep() {
    let mut s = Session::new();
    imported(&mut s);
    let h = s.execute("markupassist", &json!({})).unwrap()["markups"][0]["handle"].as_str().unwrap().to_string();
    s.execute("markupassist", &json!({"handles": [h], "as": "text", "keep": true})).unwrap();
    let objs = model(&s);
    assert_eq!(objs.len(), 5, "kept markup plus the new text");
    assert!(matches!(&objs[4].2, EntityKind::Text(t) if t.value == "Check this" && t.height == 20.0));
    assert_eq!(objs[4].1, "0", "the current layer");
    s.execute("markupassist", &json!({"handles": [h], "as": "mleader"})).unwrap();
    let objs = model(&s);
    assert!(matches!(&objs.last().unwrap().2, EntityKind::MLeader(m) if m.leaders.is_empty() && m.text.is_some()));
    assert!(objs.iter().all(|(x, ..)| x.hex() != h), "the markup was removed");
}

#[test]
fn hostile_params_never_panic() {
    let mut s = Session::new();
    let junk = [
        json!(null),
        json!([]),
        json!({"path": 5}),
        json!({"path": ""}),
        json!({"path": "nope.pdf"}),
        json!({"path": "x.txt"}),
        json!({"path": pdf(), "page": -1}),
        json!({"path": pdf(), "page": 1e30}),
        json!({"path": pdf(), "page": 9}),
        json!({"path": pdf(), "at": "x"}),
        json!({"path": pdf(), "at": [1e300, 0]}),
        json!({"path": pdf(), "width": -3}),
        json!({"path": pdf(), "width": 1e300}),
        json!({"path": pdf(), "width": "wide"}),
        json!({"path": pdf(), "layer": "a/b"}),
        json!({"path": png(0, 0), "markups": []}),
        json!({"path": png(1, 1), "markups": [1, "a", null, {}, {"kind": 5}, {"kind": "text"}, {"kind": "text", "text": "t", "at": "q"},
            {"kind": "cloud", "points": [[null]]}, {"kind": "rect", "points": [[1e300, 1e300], [-1e300, 0]]},
            {"kind": "ellipse", "points": [[0, 0], [0, 0]]}, {"kind": "leader", "points": "x"}, {"kind": "text", "text": "t", "at": [0, 0], "height": -1}]}),
        json!({"path": png(1, 1), "markups": "no"}),
        json!({"path": png(1, 1), "width": 1e9, "markups": [{"kind": "polyline", "points": [[1e6, 1e6], [0, 0]]}]}),
    ];
    for p in &junk {
        let _ = s.execute("markupimport", p);
    }
    imported(&mut s);
    let assist = [
        json!(null),
        json!({"handles": "x"}),
        json!({"handles": [1, "zz", null]}),
        json!({"handles": ["ffffffff"], "as": "mtext"}),
        json!({"all": true, "as": "bogus"}),
        json!({"all": true, "layer": 5}),
        json!({"all": true, "layer": "a|b"}),
        json!({"all": true, "arcLength": -1}),
        json!({"handle": 12}),
        json!({"handle": []}),
        json!({"all": true, "arcLength": 1e-300, "keep": true}),
    ];
    for p in &assist {
        let _ = s.execute("markupassist", p);
    }
    // Objects that are not markups are never converted.
    s.execute("line", &json!({"points": [[0, 0], [1, 1]]})).unwrap();
    let line = model(&s).last().unwrap().0.hex();
    let r = s.execute("markupassist", &json!({"handles": [line]})).unwrap();
    assert_eq!(r["converted"], 0);
}

#[test]
fn interactive_import_asks_path_point_and_width() {
    use crate::Input;
    let mut s = Session::new();
    s.start("markupimport").unwrap();
    s.input(Input::Text(pdf())).unwrap();
    s.input(Input::Point(cadcraft_geom::Vec2::new(10.0, 20.0))).unwrap();
    s.input(Input::Text("400".into())).unwrap();
    assert!(s.current_prompt().is_none(), "the command is done");
    let EntityKind::MText(m) = &model(&s)[0].2 else { panic!() };
    assert_eq!((m.insert.x, m.insert.y), (50.0, 200.0));
}

// ---------------------------------------------------------------- markup paper

fn papers(s: &Session) -> Vec<(String, cadcraft_doc::Image)> {
    s.doc()
        .unwrap()
        .model
        .iter()
        .filter_map(|e| if let EntityKind::Image(i) = &e.kind { Some((e.common.layer.clone(), i.clone())) } else { None })
        .collect()
}

/// The colour at screen pixel (x, y) of the model drawn 400 × 200 around (1200, 600).
fn sample(s: &Session, x: u32, y: u32) -> [u8; 3] {
    use cadcraft_render::raster::{RasterOptions, View, render};
    let list = cadcraft_render::build(s.doc().unwrap(), &cadcraft_doc::Space::Model, &cadcraft_render::Options::default());
    let view = View { center: cadcraft_geom::Vec2::new(1200.0, 550.0), scale: 1.0, width: 400, height: 200 };
    let o = RasterOptions { background: cadcraft_color::Rgb(255, 255, 255), antialias: false, ..Default::default() };
    let p = render(&list, &view, &o).unwrap().pixel(x, y).unwrap();
    [p.red(), p.green(), p.blue()]
}

#[test]
fn a_pdf_import_attaches_the_page_as_paper_under_the_markups() {
    let mut s = Session::new();
    let r = imported(&mut s);
    assert_eq!(r["count"], 4);
    let paper = r["paper"].as_str().unwrap().to_string();
    let all: Vec<_> = s.doc().unwrap().model.iter().collect();
    // First in draw order: under every markup.
    assert_eq!(all[0].handle.hex(), paper);
    assert_eq!(all.len(), 5);
    let (layer, img) = papers(&s).remove(0);
    assert_eq!((layer.as_str(), img.page, img.size), ("MARKUP-PAPER", 1, cadcraft_geom::Vec2::new(200.0, 100.0)));
    // The same `at` and `width` as the markups: the 200 pt page is 400 units wide at (1000, 500).
    assert_eq!((img.insert.x, img.insert.y, img.u.x, img.v.y), (1000.0, 500.0, 2.0, 2.0));
    assert!(img.path.ends_with("review.pdf"), "{}", img.path);
    // MARKUPPAPERDISPLAY and MARKUPPAPERTRANSPARENCY are real system variables.
    assert_eq!(s.execute("getvar", &json!({"name": "MARKUPPAPERDISPLAY"})).unwrap()["value"], 1);
    assert!(s.execute("setvar", &json!({"name": "MARKUPPAPERTRANSPARENCY", "value": 95})).is_err());
    assert!(s.execute("setvar", &json!({"name": "MARKUPPAPERDISPLAY", "value": 2})).is_err());
}

#[test]
fn an_image_import_attaches_the_image_as_paper_and_paper_false_skips_it() {
    let mut s = Session::new();
    let path = png(640, 480);
    let m = json!([{"kind": "rect", "points": [[0.1, 0.1], [0.2, 0.2]]}]);
    let r = s.execute("markupimport", &json!({"path": path, "markups": m, "at": [5, 6], "width": 320})).unwrap();
    assert!(r["paper"].is_string());
    let (layer, img) = papers(&s).remove(0);
    assert_eq!(
        (layer.as_str(), img.page, img.size, img.insert.x, img.insert.y, img.u.x),
        ("MARKUP-PAPER", 0, cadcraft_geom::Vec2::new(640.0, 480.0), 5.0, 6.0, 0.5)
    );
    let mut s = Session::new();
    let r = s.execute("markupimport", &json!({"path": png(640, 480), "markups": m, "paper": false})).unwrap();
    assert!(r["paper"].is_null() && papers(&s).is_empty());
}

#[test]
fn markup_paper_display_hides_and_transparency_fades_the_paper() {
    use cadcraft_render::images::{Raster, png_data_uri};
    let mut s = Session::new();
    // A real red 2 × 1 image as the paper, 400 × 200 units at (1000, 500).
    let r = Raster { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 255, 0, 0, 255] };
    let b64 = png_data_uri(&r).unwrap().strip_prefix("data:image/png;base64,").unwrap().to_string();
    let file = tmp("paper.png", &crate::cmd::file::base64_decode(&b64).unwrap());
    s.execute("markupimport", &json!({"path": file, "at": [1000, 500], "width": 400, "markups": []})).unwrap();
    // Screen (200, 100) is world (1200, 550): inside the paper.
    assert_eq!(sample(&s, 200, 100), [255, 0, 0]);
    s.execute("setvar", &json!({"name": "MARKUPPAPERTRANSPARENCY", "value": 50})).unwrap();
    let p = sample(&s, 200, 100);
    assert!(p[0] == 255 && (120..140).contains(&p[1]), "half transparent: {p:?}");
    s.execute("setvar", &json!({"name": "MARKUPPAPERDISPLAY", "value": 0})).unwrap();
    assert_eq!(sample(&s, 200, 100), [255, 255, 255], "hidden");
    s.execute("setvar", &json!({"name": "MARKUPPAPERDISPLAY", "value": 1})).unwrap();
    assert!(sample(&s, 200, 100)[1] > 100, "shown again");
}

#[test]
fn hostile_paper_params_never_panic() {
    let mut s = Session::new();
    for p in [json!({"paper": "yes"}), json!({"paper": null}), json!({"paper": 5}), json!({"paper": true, "page": 7})] {
        let mut q = json!({"path": pdf()});
        if let (Some(o), Some(extra)) = (q.as_object_mut(), p.as_object()) {
            o.extend(extra.clone());
        }
        let _ = s.execute("markupimport", &q);
    }
    // A missing image under valid markups: the import still succeeds and the paper draws its frame.
    let r = s.execute("markupimport", &json!({"path": "D:/gone/plan.png", "markups": []}));
    assert!(r.is_err(), "the size can't be read");
}
