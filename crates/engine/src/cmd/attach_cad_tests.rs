use cadcraft_doc::{EntityKind, Image, Space, UnderlayKind};
use cadcraft_geom::Vec2;
use cadcraft_render::raster::{RasterOptions, View, render};
use serde_json::{Value, json};

use crate::{Input, Session};

/// A W2D stream: a red diagonal across a 1000 × 500 sheet.
fn w2d() -> Vec<u8> {
    let mut b = b"(W2D V06.00)".to_vec();
    b.extend(b"(Bounds 0,0 1000,500)\n(Color 255,0,0,255)\n(LineWeight 30)\nL 0,0 1000,500\n(EndOfDWF)");
    b
}

/// A file of its own per call: the tests run in parallel.
fn tmp(name: &str, bytes: &[u8]) -> String {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("cadcraft-attach-cad-{}-{n}-{name}", std::process::id()));
    std::fs::write(&p, bytes).unwrap();
    p.to_string_lossy().into_owned()
}

fn b64(bytes: &[u8]) -> String {
    crate::cmd::file::base64_encode(bytes)
}

fn images(s: &Session) -> Vec<Image> {
    s.doc().unwrap().model.iter().filter_map(|e| if let EntityKind::Image(i) = &e.kind { Some(i.clone()) } else { None }).collect()
}

/// Whether any pixel within 3 of screen pixel (x, y) of the model drawn 100 × 100 around (50, 50) is not white (a thin red line shrunk to the view).
fn red_near(s: &Session, x: u32, y: u32) -> bool {
    let list = cadcraft_render::build(s.doc().unwrap(), &Space::Model, &cadcraft_render::Options::default());
    let view = View { center: Vec2::new(50.0, 50.0), scale: 1.0, width: 100, height: 100 };
    let o = RasterOptions { background: cadcraft_color::Rgb(255, 255, 255), antialias: false, ..Default::default() };
    let r = render(&list, &view, &o).unwrap();
    (x - 3..=x + 3).any(|px| {
        (y - 3..=y + 3).any(|py| {
            let p = r.pixel(px, py).unwrap();
            p.red() > 200 && p.green() < 235 && p.blue() < 235
        })
    })
}

#[test]
fn dwfattach_places_a_sheet_that_shows_its_drawing() {
    let mut s = Session::new();
    let r = s.execute("dwfattach", &json!({"path": "mem:a.dwf", "data": b64(&w2d()), "at": [0, 0], "width": 100, "layer": "DWF"})).unwrap();
    assert_eq!((r["sheet"].as_str(), r["page"].as_u64(), r["sheets"].as_array().map(Vec::len)), (Some("Sheet 1"), Some(1), Some(1)));
    assert_eq!(r["width"].as_f64(), Some(100.0));
    let i = images(&s).remove(0);
    assert_eq!((i.kind, i.page, i.sheet.as_str(), i.path.as_str()), (UnderlayKind::Dwf, 1, "Sheet 1", "mem:a.dwf"));
    // Natural size is in points; 100 units wide: u is one point.
    assert!((i.u.x * i.size.x - 100.0).abs() < 1e-6, "{:?} {:?}", i.u, i.size);
    // The diagonal crosses the sheet's centre: (50, 25) in the drawing is screen pixel (50, 75).
    assert!(red_near(&s, 50, 75), "the red diagonal is drawn at the sheet centre");
    assert!(!red_near(&s, 50, 20), "nothing above the sheet");
}

#[test]
fn attach_picks_the_command_by_extension() {
    let mut s = Session::new();
    s.execute("attach", &json!({"path": "mem:b.dwf", "data": b64(&w2d()), "width": 80})).unwrap();
    let kinds: Vec<UnderlayKind> = images(&s).iter().map(|i| i.kind).collect();
    assert_eq!(kinds, [UnderlayKind::Dwf]);
    assert!(s.execute("attach", &json!({"path": "mem:b.xyz", "data": b64(&w2d())})).unwrap_err().to_string().contains("DWF"));
}

#[test]
fn the_wrong_kind_and_bad_sheets_are_errors() {
    let mut s = Session::new();
    // Not a DWF file.
    assert!(s.execute("dwfattach", &json!({"path": "mem:c.dwf", "data": "QUJD"})).is_err());
    for p in [json!({"sheet": 2}), json!({"sheet": 0}), json!({"sheet": "Nope"}), json!({"sheet": true})] {
        let mut p = p;
        p["path"] = json!("mem:c.dwf");
        p["data"] = json!(b64(&w2d()));
        assert!(s.execute("dwfattach", &p).is_err(), "{p}");
    }
    // The sheet may be named or numbered.
    s.execute("dwfattach", &json!({"path": "mem:c.dwf", "data": b64(&w2d()), "sheet": "sheet 1"})).unwrap();
    s.execute("dwfattach", &json!({"path": "mem:c.dwf", "data": b64(&w2d()), "sheet": 1})).unwrap();
    assert_eq!(images(&s).len(), 2);
}

#[test]
fn a_dwf_underlay_survives_dxf_save_and_load() {
    let mut s = Session::new();
    s.execute("dwfattach", &json!({"path": "mem:d.dwf", "data": b64(&w2d()), "at": [5, 6], "scale": 2, "rotation": 30})).unwrap();
    let text = cadcraft_io::write_dxf(s.doc().unwrap());
    for name in ["DWFUNDERLAY", "DWFDEFINITION", "ACAD_DWFDEFINITIONS"] {
        assert!(text.contains(name), "{name}");
    }
    assert!(!text.contains("PDFUNDERLAY"));
    let back = cadcraft_io::read_dxf(text.as_bytes()).unwrap();
    let got: Vec<Image> = back.model.iter().filter_map(|e| if let EntityKind::Image(i) = &e.kind { Some(i.clone()) } else { None }).collect();
    let before = images(&s);
    assert_eq!(got.len(), 1);
    for (a, b) in before.iter().zip(&got) {
        assert_eq!((a.kind, a.page, &a.path, &a.sheet), (b.kind, b.page, &b.path, &b.sheet));
        assert!((a.u - b.u).xy().len() < 1e-9 && (a.insert - b.insert).xy().len() < 1e-9);
    }
    // A second round trip is stable.
    let again = cadcraft_io::write_dxf(&back);
    assert_eq!(again.matches("DWFDEFINITION").count(), text.matches("DWFDEFINITION").count());
}

#[test]
fn the_interactive_form_prompts_for_the_sheet_and_the_rest() {
    let mut s = Session::new();
    let dwf = tmp("i.dwf", &w2d());
    s.start("dwfattach").unwrap();
    s.input(Input::Text(dwf)).unwrap();
    assert!(s.current_prompt().is_some_and(|p| p.message.contains("sheet")));
    s.input(Input::Text("Sheet 1".into())).unwrap();
    s.input(Input::Point(Vec2::new(5.0, 6.0))).unwrap();
    s.input(Input::Text("2".into())).unwrap();
    s.input(Input::Text("0".into())).unwrap();
    assert!(s.current_prompt().is_none());
    let i = &images(&s)[0];
    // Scale 2 is two drawing units per inch of the sheet: 2/72 per point.
    assert_eq!((i.kind, i.insert.x, i.insert.y), (UnderlayKind::Dwf, 5.0, 6.0));
    assert!((i.u.x - 2.0 / 72.0).abs() < 1e-12, "{:?}", i.u);
}

#[test]
fn hostile_params_and_files_never_panic() {
    let mut s = Session::new();
    let good = b64(&w2d());
    for cmd in ["dwfattach", "attach"] {
        for p in [
            Value::Null,
            json!(5),
            json!({}),
            json!({"path": ""}),
            json!({"path": "mem:x.dwf", "data": 5}),
            json!({"path": "mem:x.dwf", "data": "!!"}),
            json!({"path": "mem:x.dwf", "data": good, "sheet": [1]}),
            json!({"path": "mem:x.dwf", "data": good, "sheet": 1e300}),
            json!({"path": "mem:x.dwf", "data": good, "scale": 1e300}),
            json!({"path": "mem:x.dwf", "data": good, "at": [1e300, 0]}),
            json!({"path": "D:/definitely/not/here.dwf"}),
            json!({"path": "//198.51.100.1/share/a.dwf"}),
        ] {
            let _ = s.execute(cmd, &p);
        }
    }
    // Truncated and bit-flipped files through the command.
    let bytes = w2d();
    for n in (0..bytes.len()).step_by(7) {
        let _ = s.execute("attach", &json!({"path": "mem:h.dwf", "data": b64(&bytes[..n])}));
        let mut flipped = bytes.clone();
        flipped[n.min(bytes.len() - 1)] ^= 0xFF;
        let _ = s.execute("dwfattach", &json!({"path": "mem:h.dwf", "data": b64(&flipped)}));
    }
}
