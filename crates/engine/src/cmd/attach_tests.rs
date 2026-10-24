use cadcraft_doc::{EntityKind, Image, Space};
use cadcraft_geom::Vec2;
use cadcraft_render::images::{Raster, png_data_uri};
use cadcraft_render::raster::{RasterOptions, View, render};
use serde_json::{Value, json};

use crate::{Input, Session};

/// Base64 of a 2×1 PNG: red left, blue right.
fn png_b64() -> String {
    let r = Raster { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 0, 0, 255, 255] };
    png_data_uri(&r).unwrap().strip_prefix("data:image/png;base64,").unwrap().to_string()
}

fn images(s: &Session) -> Vec<(String, Image)> {
    s.doc()
        .unwrap()
        .model
        .iter()
        .filter_map(|e| if let EntityKind::Image(i) = &e.kind { Some((e.common.layer.clone(), i.clone())) } else { None })
        .collect()
}

/// The colour at screen pixel (x, y) of the model drawn 100 × 100 around (50, 50).
fn pixel(s: &Session, x: u32, y: u32) -> [u8; 3] {
    let list = cadcraft_render::build(s.doc().unwrap(), &Space::Model, &cadcraft_render::Options::default());
    let view = View { center: Vec2::new(50.0, 50.0), scale: 1.0, width: 100, height: 100 };
    let o = RasterOptions { background: cadcraft_color::Rgb(255, 255, 255), antialias: false, ..Default::default() };
    let p = render(&list, &view, &o).unwrap().pixel(x, y).unwrap();
    [p.red(), p.green(), p.blue()]
}

/// A file of its own per call: the tests run in parallel.
fn tmp(name: &str, bytes: &[u8]) -> String {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("cadcraft-attach-{}-{n}-{name}", std::process::id()));
    std::fs::write(&p, bytes).unwrap();
    p.to_string_lossy().into_owned()
}

#[test]
fn imageattach_places_the_picture_at_the_scale_and_rotation() {
    let mut s = Session::new();
    let r = s.execute("imageattach", &json!({"path": "mem:a.png", "data": png_b64(), "at": [10, 20], "width": 80, "layer": "PICS"})).unwrap();
    assert_eq!((r["width"].as_f64(), r["height"].as_f64(), &r["pixels"]), (Some(80.0), Some(40.0), &json!([2.0, 1.0])));
    let (layer, i) = images(&s).remove(0);
    assert_eq!((layer.as_str(), i.path.as_str(), i.size), ("PICS", "mem:a.png", Vec2::new(2.0, 1.0)));
    // 80 units over 2 pixels: 40 per pixel.
    assert_eq!((i.insert.x, i.insert.y, i.u.x, i.v.y), (10.0, 20.0, 40.0, 40.0));
    // Scale and a quarter turn.
    s.execute("imageattach", &json!({"path": "mem:a.png", "data": png_b64(), "scale": 2, "rotation": 90})).unwrap();
    let i = &images(&s)[1].1;
    assert!(i.u.x.abs() < 1e-9 && (i.u.y - 2.0).abs() < 1e-9 && (i.v.x + 2.0).abs() < 1e-9, "{:?} {:?}", i.u, i.v);
    // Without width or scale a picture is one unit per pixel.
    s.execute("imageattach", &json!({"path": "mem:a.png", "data": png_b64()})).unwrap();
    assert_eq!(images(&s)[2].1.u.x, 1.0);
}

#[test]
fn an_attached_png_shows_its_pixels_and_survives_dxf_save_and_load() {
    let mut s = Session::new();
    s.execute("imageattach", &json!({"path": "mem:two.png", "data": png_b64(), "at": [0, 0], "width": 100})).unwrap();
    // 100 units wide, 50 high (2 × 1 pixels): the lower half of the 100 × 100 view.
    assert_eq!(pixel(&s, 25, 75), [255, 0, 0], "left pixel red");
    assert_eq!(pixel(&s, 75, 75), [0, 0, 255], "right pixel blue");
    assert_eq!(pixel(&s, 25, 25), [255, 255, 255], "above the picture: background");
    let back = cadcraft_io::read_dxf(cadcraft_io::write_dxf(s.doc().unwrap()).as_bytes()).unwrap();
    let imgs: Vec<&Image> = back.model.iter().filter_map(|e| if let EntityKind::Image(i) = &e.kind { Some(i) } else { None }).collect();
    assert_eq!(imgs.len(), 1);
    let before = &images(&s)[0].1;
    assert_eq!((imgs[0].path.as_str(), imgs[0].size, imgs[0].u, imgs[0].v), ("mem:two.png", before.size, before.u, before.v));
}

#[test]
fn imageattach_refuses_bad_parameters_without_changing_the_drawing() {
    let mut s = Session::new();
    let good = png_b64();
    let cases = [
        json!({}),
        json!({"path": ""}),
        json!({"path": "mem:x.png", "data": "!!not base64!!"}),
        json!({"path": "mem:x.png", "data": 5}),
        json!({"path": "D:/definitely/not/here.png"}),
        json!({"path": "mem:y.png", "data": good, "width": 10, "scale": 2}),
        json!({"path": "mem:y.png", "data": good, "width": -1}),
        json!({"path": "mem:y.png", "data": good, "scale": 0}),
        json!({"path": "mem:y.png", "data": good, "scale": 1e300}),
        json!({"path": "mem:y.png", "data": good, "scale": 1e-300}),
        json!({"path": "mem:y.png", "data": good, "at": [1e300, 0]}),
        json!({"path": "mem:y.png", "data": good, "at": "nowhere"}),
        json!({"path": "mem:y.png", "data": good, "rotation": "x"}),
        json!({"path": "mem:y.png", "data": good, "layer": "bad<name>"}),
        json!({"path": "//198.51.100.1/share/a.png"}),
        json!({"path": "\\\\198.51.100.1\\share\\a.png"}),
        json!({"path": "mem:z.png", "data": "QUJD"}),
    ];
    for p in cases {
        assert!(s.execute("imageattach", &p).is_err(), "{p}");
    }
    assert!(images(&s).is_empty());
}

#[test]
fn oversized_and_corrupt_pictures_are_errors() {
    let mut s = Session::new();
    // A PNG header claiming 8000 × 8000 pixels (over the 50 megapixel cap) and no data.
    let big = {
        let mut b = Vec::new();
        // A PNG's signature, IHDR (8000 × 8000, 8-bit gray) and nothing else: the header alone is checked first.
        b.extend([0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        b.extend(8000u32.to_be_bytes());
        b.extend(8000u32.to_be_bytes());
        b.extend([8, 0, 0, 0, 0]);
        b
    };
    let b64 = cadcraft_engine_b64(&big);
    let e = s.execute("imageattach", &json!({"path": "mem:big.png", "data": b64})).unwrap_err().to_string();
    assert!(!e.is_empty(), "refused: {e}");
    let junk = tmp("junk.png", b"\x89PNG\r\n\x1a\ngarbage garbage");
    assert!(s.execute("imageattach", &json!({"path": junk})).is_err());
    let jpg = tmp("junk.jpg", b"\xFF\xD8\xFF\xE0 truncated");
    assert!(s.execute("attach", &json!({"path": jpg})).is_err());
    assert!(images(&s).is_empty());
}

fn cadcraft_engine_b64(b: &[u8]) -> String {
    crate::cmd::file::base64_encode(b)
}

#[test]
fn attach_picks_by_extension() {
    let mut s = Session::new();
    let png = tmp("one.png", &crate::cmd::file::base64_decode(&png_b64()).unwrap());
    let r = s.execute("attach", &json!({"path": png, "width": 10})).unwrap();
    assert_eq!(r["width"].as_f64(), Some(10.0));
    assert_eq!(images(&s).len(), 1);
    let e = s.execute("attach", &json!({"path": "plan.dwg"})).unwrap_err().to_string();
    assert!(e.contains("XATTACH"), "{e}");
    // A PDF isn't a picture this command reads.
    let pdf = tmp("sheet.pdf", b"%PDF-1.4 not really");
    assert!(s.execute("attach", &json!({"path": pdf})).is_err());
    assert!(s.execute("imageattach", &json!({"path": pdf})).is_err());
    assert_eq!(images(&s).len(), 1);
}

#[test]
fn the_interactive_form_prompts_for_file_point_scale_and_rotation() {
    let mut s = Session::new();
    let png = tmp("ia.png", &crate::cmd::file::base64_decode(&png_b64()).unwrap());
    s.start("imageattach").unwrap();
    s.input(Input::Text(png)).unwrap();
    s.input(Input::Point(Vec2::new(5.0, 6.0))).unwrap();
    s.input(Input::Text("3".into())).unwrap();
    s.input(Input::Text("0".into())).unwrap();
    assert!(s.current_prompt().is_none());
    let i = &images(&s)[0].1;
    assert_eq!((i.insert.x, i.insert.y, i.u.x), (5.0, 6.0, 3.0));
    // A bad scale is an error and the command is still waiting.
    s.start("imageattach").unwrap();
    s.input(Input::Text("x.png".into())).unwrap();
    s.input(Input::Enter).unwrap();
    let _ = s.input(Input::Text("zero".into()));
    assert!(s.current_prompt().is_some_and(|p| p.message.contains("scale")), "still asking for the scale");
    s.input(Input::Cancel).unwrap();
    assert_eq!(images(&s).len(), 1);
}

#[test]
fn hostile_attach_params_never_panic() {
    let mut s = Session::new();
    for cmd in ["attach", "imageattach"] {
        for p in [
            Value::Null,
            json!(5),
            json!([]),
            json!({"path": null}),
            json!({"path": 5}),
            json!({"path": "a\u{0}b.png"}),
            json!({"path": "mem:q.png", "data": null}),
            json!({"path": "mem:q.png", "data": ""}),
        ] {
            let _ = s.execute(cmd, &p);
        }
    }
}
