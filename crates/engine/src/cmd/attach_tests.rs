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

/// A one-page PDF with a red square covering the left half of a 100 × 50 page, and a second page.
fn pdf_bytes() -> Vec<u8> {
    let content = "1 0 0 rg 0 0 50 50 re f";
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 100 50] >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Page /Parent 2 0 R >>".to_string(),
    ];
    let mut f = b"%PDF-1.4\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(f.len());
        f.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
    }
    let xref = f.len();
    f.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
    for o in offs {
        f.extend(format!("{o:010} 00000 n \n").bytes());
    }
    f.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).bytes());
    f
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
    assert_eq!((layer.as_str(), i.path.as_str(), i.page, i.size), ("PICS", "mem:a.png", 0, Vec2::new(2.0, 1.0)));
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
    let pdf = tmp("junk.pdf", b"%PDF-1.4 not really");
    assert!(s.execute("pdfattach", &json!({"path": pdf})).is_err());
    assert!(s.execute("pdfattach", &json!({"path": "mem:no.pdf", "data": "QUJD"})).is_err());
    assert!(images(&s).is_empty());
}

fn cadcraft_engine_b64(b: &[u8]) -> String {
    crate::cmd::file::base64_encode(b)
}

#[test]
fn pdfattach_places_a_page_and_attach_picks_by_extension() {
    let mut s = Session::new();
    let path = tmp("sheet.pdf", &pdf_bytes());
    let r = s.execute("pdfattach", &json!({"path": path, "at": [0, 0], "width": 100})).unwrap();
    assert_eq!((r["page"].as_u64(), r["pages"].as_u64(), r["width"].as_f64(), r["height"].as_f64()), (Some(1), Some(2), Some(100.0), Some(50.0)));
    let (_, i) = images(&s).remove(0);
    assert_eq!((i.page, i.size, i.u.x), (1, Vec2::new(100.0, 50.0), 1.0));
    assert_eq!(pixel(&s, 25, 75), [255, 0, 0], "the page's red half");
    assert_eq!(pixel(&s, 75, 75), [255, 255, 255], "paper");
    // Page 2 exists (blank, 100 × 50 by inheritance); page 3 doesn't.
    s.execute("pdfattach", &json!({"path": path, "page": 2, "scale": 2})).unwrap();
    assert_eq!(images(&s)[1].1.page, 2);
    for page in [json!(3), json!(0), json!(-1), json!("one"), json!(1.5)] {
        assert!(s.execute("pdfattach", &json!({"path": path, "page": page})).is_err(), "{page}");
    }
    // ATTACH picks by extension.
    s.execute("attach", &json!({"path": path})).unwrap();
    assert_eq!(images(&s)[2].1.page, 1);
    let png = tmp("one.png", &crate::cmd::file::base64_decode(&png_b64()).unwrap());
    s.execute("attach", &json!({"path": png})).unwrap();
    assert_eq!(images(&s)[3].1.page, 0);
    let e = s.execute("attach", &json!({"path": "plan.dwg"})).unwrap_err().to_string();
    assert!(e.contains("XATTACH"), "{e}");
    // A PDF isn't an image.
    assert!(s.execute("imageattach", &json!({"path": path})).is_err());
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
    for cmd in ["attach", "imageattach", "pdfattach"] {
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
