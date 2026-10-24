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

/// A DGN V7 2D file: TCB (1000 UORs per master unit) and one red line of 100 × 50 master units.
fn dgn() -> Vec<u8> {
    // DGN long integers: bytes B2 B3 B0 B1.
    let pdp = |v: i32| {
        let b = v.to_le_bytes();
        [b[2], b[3], b[0], b[1]]
    };
    let mut tcb = vec![0u8; 1536];
    tcb[0] = 0x08;
    tcb[1] = 0x09;
    tcb[2..4].copy_from_slice(&766u16.to_le_bytes());
    tcb[1112..1116].copy_from_slice(&pdp(10));
    tcb[1116..1120].copy_from_slice(&pdp(100));
    let mut line = vec![0u8; 52];
    line[0] = 0x01;
    line[1] = 0x03;
    line[2..4].copy_from_slice(&24u16.to_le_bytes());
    // Colour 3 (red in the default colour table), weight 3.
    line[34..36].copy_from_slice(&((3u16 << 8) | (3 << 3)).to_le_bytes());
    for (at, v) in [(36, 0), (40, 0), (44, 100_000), (48, 50_000)] {
        line[at..at + 4].copy_from_slice(&pdp(v));
    }
    let mut f = tcb;
    f.extend(line);
    f.extend([0xFF, 0xFF]);
    f
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
fn dgnattach_places_a_design_in_master_units() {
    let mut s = Session::new();
    let r = s.execute("dgnattach", &json!({"path": "mem:a.dgn", "data": b64(&dgn()), "scale": 1})).unwrap();
    assert_eq!(r["page"].as_u64(), Some(1));
    let i = images(&s).remove(0);
    assert_eq!((i.kind, i.page), (UnderlayKind::Dgn, 1));
    assert!(!i.sheet.is_empty());
    // A 100 × 50 master-unit design (plus its margin) at one drawing unit per master unit.
    let (w, h) = (r["width"].as_f64().unwrap(), r["height"].as_f64().unwrap());
    assert!((100.0..110.0).contains(&w) && (50.0..56.0).contains(&h), "{w} x {h}");
    assert!(red_near(&s, 50, 75), "the red line is drawn through the middle");
}

#[test]
fn attach_picks_the_command_by_extension() {
    let mut s = Session::new();
    s.execute("attach", &json!({"path": "mem:b.dwf", "data": b64(&w2d()), "width": 80})).unwrap();
    s.execute("attach", &json!({"path": "mem:b.dgn", "data": b64(&dgn())})).unwrap();
    let kinds: Vec<UnderlayKind> = images(&s).iter().map(|i| i.kind).collect();
    assert_eq!(kinds, [UnderlayKind::Dwf, UnderlayKind::Dgn]);
    assert!(s.execute("attach", &json!({"path": "mem:b.xyz", "data": b64(&w2d())})).unwrap_err().to_string().contains("DGN"));
}

#[test]
fn dgn_v8_is_refused_with_the_exact_reason() {
    let mut s = Session::new();
    let mut v8 = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    v8.resize(2048, 0);
    for cmd in ["dgnattach", "attach"] {
        let e = s.execute(cmd, &json!({"path": "mem:v8.dgn", "data": b64(&v8)})).unwrap_err().to_string();
        assert!(e.contains("DGN V8 is not supported: no public specification"), "{cmd}: {e}");
    }
    assert!(images(&s).is_empty());
}

#[test]
fn the_wrong_kind_and_bad_sheets_are_errors() {
    let mut s = Session::new();
    let e = s.execute("dwfattach", &json!({"path": "mem:c.dgn", "data": b64(&dgn())})).unwrap_err().to_string();
    assert!(e.contains("DGNATTACH"), "{e}");
    let e = s.execute("dgnattach", &json!({"path": "mem:c.dwf", "data": b64(&w2d())})).unwrap_err().to_string();
    assert!(e.contains("DWFATTACH"), "{e}");
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
fn underlays_survive_dxf_save_and_load() {
    let mut s = Session::new();
    s.execute("dwfattach", &json!({"path": "mem:d.dwf", "data": b64(&w2d()), "at": [5, 6], "scale": 2, "rotation": 30})).unwrap();
    s.execute("dgnattach", &json!({"path": "mem:d.dgn", "data": b64(&dgn()), "at": [1, 2]})).unwrap();
    let text = cadcraft_io::write_dxf(s.doc().unwrap());
    for name in ["DWFUNDERLAY", "DWFDEFINITION", "ACAD_DWFDEFINITIONS", "DGNUNDERLAY", "DGNDEFINITION", "ACAD_DGNDEFINITIONS"] {
        assert!(text.contains(name), "{name}");
    }
    assert!(!text.contains("PDFUNDERLAY"));
    let back = cadcraft_io::read_dxf(text.as_bytes()).unwrap();
    let got: Vec<Image> = back.model.iter().filter_map(|e| if let EntityKind::Image(i) = &e.kind { Some(i.clone()) } else { None }).collect();
    let before = images(&s);
    assert_eq!(got.len(), 2);
    for (a, b) in before.iter().zip(&got) {
        assert_eq!((a.kind, a.page, &a.path, &a.sheet), (b.kind, b.page, &b.path, &b.sheet));
        assert!((a.u - b.u).xy().len() < 1e-9 && (a.insert - b.insert).xy().len() < 1e-9);
    }
    // A second round trip is stable.
    let again = cadcraft_io::write_dxf(&back);
    assert_eq!(again.matches("DWFDEFINITION").count(), text.matches("DWFDEFINITION").count());
}

#[test]
fn the_interactive_forms_prompt_for_the_sheet_and_the_rest() {
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
    // DGN has no sheet prompt.
    let dgn_file = tmp("i.dgn", &dgn());
    s.start("dgnattach").unwrap();
    s.input(Input::Text(dgn_file)).unwrap();
    assert!(s.current_prompt().is_some_and(|p| p.message.contains("insertion")));
    s.input(Input::Cancel).unwrap();
    assert_eq!(images(&s).len(), 1);
}

#[test]
fn hostile_params_and_files_never_panic() {
    let mut s = Session::new();
    let good = b64(&w2d());
    for cmd in ["dwfattach", "dgnattach", "attach"] {
        for p in [
            Value::Null,
            json!(5),
            json!({}),
            json!({"path": ""}),
            json!({"path": "mem:x.dwf", "data": 5}),
            json!({"path": "mem:x.dwf", "data": "!!"}),
            json!({"path": "mem:x.dwf", "data": good, "sheet": [1]}),
            json!({"path": "mem:x.dwf", "data": good, "sheet": 1e300}),
            json!({"path": "mem:x.dwf", "data": good, "model": "x"}),
            json!({"path": "mem:x.dwf", "data": good, "scale": 1e300}),
            json!({"path": "mem:x.dwf", "data": good, "at": [1e300, 0]}),
            json!({"path": "mem:x.dgn", "data": "QUJD"}),
            json!({"path": "D:/definitely/not/here.dwf"}),
            json!({"path": "//198.51.100.1/share/a.dgn"}),
        ] {
            let _ = s.execute(cmd, &p);
        }
    }
    // Truncated and bit-flipped files through the command.
    for bytes in [w2d(), dgn()] {
        for n in (0..bytes.len()).step_by(7) {
            let _ = s.execute("attach", &json!({"path": "mem:h.dgn", "data": b64(&bytes[..n])}));
            let mut flipped = bytes.clone();
            flipped[n.min(bytes.len() - 1)] ^= 0xFF;
            let _ = s.execute("dwfattach", &json!({"path": "mem:h.dwf", "data": b64(&flipped)}));
        }
    }
}
