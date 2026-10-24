//! A markup's tag (xdata of the application `CADCRAFT_MARKUP`) survives saving the drawing and
//! opening it again, so MARKUPASSIST still lists the markups of an imported PDF.

use cadcraft_doc::Drawing;
use cadcraft_dxf::Version;
use cadcraft_engine::Session;
use cadcraft_engine::cmd::file::{IoHooks, SaveAs, set_io, set_write_as};
use serde_json::{Value, json};

fn read(bytes: &[u8], name: &str) -> Result<Drawing, String> {
    cadcraft_io::read(bytes, name).map_err(|e| e.to_string())
}

fn write(d: &Drawing, name: &str) -> Result<Vec<u8>, String> {
    cadcraft_io::write(d, name).map_err(|e| e.to_string())
}

fn write_as(d: &Drawing, name: &str, o: &SaveAs) -> Result<Vec<u8>, String> {
    cadcraft_io::write_with(d, name, &cadcraft_io::SaveOptions { version: o.version, binary: o.binary }).map_err(|e| e.to_string())
}

fn install() {
    set_io(IoHooks { read, write, plot: None });
    set_write_as(write_as);
}

/// A long comment, with characters outside ASCII: the tag stores it in several pieces.
fn long_text() -> String {
    (0..60).map(|i| format!("Revise \u{412}\u{456}\u{434}\u{43f}\u{43e}\u{432}\u{456}\u{434}\u{44c} #{i} (see \\detail) ")).collect()
}

fn pdf(dir: &std::path::Path) -> String {
    let long = long_text();
    // PDF text strings: UTF-16BE with a byte order mark, as a hex string.
    let hex: String = [0xFEFFu16].into_iter().chain(long.encode_utf16()).map(|u| format!("{u:04X}")).collect();
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] /MediaBox [0 0 200 100] >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Annots [4 0 R 5 0 R 6 0 R] >>".to_string(),
        "<< /Type /Annot /Subtype /FreeText /Rect [20 60 120 90] /Contents (Check this) /DA (0 0 1 rg /Helv 10 Tf) /C [1 0 0] >>".to_string(),
        format!("<< /Type /Annot /Subtype /Square /Rect [10 10 60 40] /BE << /S /C /I 1 >> /Contents <{hex}> /C [0 0 1] >>"),
        "<< /Type /Annot /Subtype /Line /L [100 20 150 20] /LE [/None /OpenArrow] /Contents (See here) >>".to_string(),
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
    let p = dir.join("review.pdf");
    std::fs::write(&p, f).unwrap();
    p.to_string_lossy().into_owned()
}

/// What MARKUPASSIST lists: kind and text of every markup (the handles are the drawing's).
fn listed(s: &mut Session) -> Vec<(String, String)> {
    let r: Value = s.execute("markupassist", &json!({})).unwrap();
    r["markups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["kind"].as_str().unwrap().to_string(), m["text"].as_str().unwrap_or_default().to_string()))
        .collect()
}

fn round_trip(ext: &str, opts: Value) {
    install();
    let dir = std::env::temp_dir().join(format!("cadcraft-markup-roundtrip-{}-{ext}-{}", std::process::id(), opts.to_string().len()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = Session::new();
    s.execute("markupimport", &json!({"path": pdf(&dir), "width": 400})).unwrap();
    let before = listed(&mut s);
    assert_eq!(before.iter().map(|m| m.0.as_str()).collect::<Vec<_>>(), ["text", "cloud", "text", "leader"]);
    assert_eq!(before[2].1, long_text(), "the long comment is read whole");

    let path = dir.join(format!("marked.{ext}")).to_string_lossy().into_owned();
    let mut p = json!({ "path": path });
    p.as_object_mut().unwrap().extend(opts.as_object().unwrap().clone());
    s.execute("saveas", &p).unwrap();

    let mut t = Session::new();
    t.execute("open", &json!({ "path": path })).unwrap();
    assert_eq!(listed(&mut t), before, "{ext} {opts}");
    // The reopened markups still convert.
    let r = t.execute("markupassist", &json!({"all": true})).unwrap();
    assert_eq!(r["converted"], 4, "{r}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn markup_tags_survive_a_dxf_round_trip() {
    round_trip("dxf", json!({}));
}

#[test]
fn markup_tags_survive_every_dxf_version_and_binary_dxf() {
    for v in [Version::R2000, Version::R2004, Version::R2007, Version::R2010, Version::R2013, Version::R2018] {
        round_trip("dxf", json!({ "version": v.year() }));
    }
    round_trip("dxf", json!({ "binary": true }));
    round_trip("dxf", json!({ "binary": true, "version": "2000" }));
}

#[test]
fn markup_tags_survive_a_dwg_round_trip() {
    round_trip("dwg", json!({}));
}
