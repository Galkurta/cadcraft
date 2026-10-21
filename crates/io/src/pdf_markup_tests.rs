use serde_json::{Value, json};

use super::*;

/// Annotations of a 200 x 100 point page: a FreeText, a Square with a cloud border and a
/// comment, an Ink stroke and a Line with an arrow at its end.
const OBJS: [&str; 7] = [
    "<< /Type /Catalog /Pages 2 0 R >>",
    "<< /Type /Pages /Count 1 /Kids [3 0 R] /MediaBox [0 0 200 100] >>",
    "<< /Type /Page /Parent 2 0 R /Annots [4 0 R 5 0 R 6 0 R 7 0 R] >>",
    "<< /Type /Annot /Subtype /FreeText /Rect [20 60 120 90] /Contents (Check this) /DA (0 0 1 rg /Helv 10 Tf) /C [1 0 0] >>",
    "<< /Type /Annot /Subtype /Square /Rect [10 10 60 40] /BE << /S /C /I 1 >> /Contents (Revise) /C [0 0 1] >>",
    "<< /Type /Annot /Subtype /Ink /InkList [[10 10 20 20 30 10]] >>",
    "<< /Type /Annot /Subtype /Line /L [100 20 150 20] /LE [/None /OpenArrow] /Contents (See here) >>",
];

fn classic(objs: &[&str]) -> Vec<u8> {
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

/// The same objects inside an object stream, found through a compressed xref stream with a PNG
/// "Up" predictor.
fn compressed(objs: &[&str]) -> Vec<u8> {
    let (mut head, mut body) = (String::new(), String::new());
    for (i, o) in objs.iter().enumerate() {
        head.push_str(&format!("{} {} ", i + 1, body.len()));
        body.push_str(o);
        body.push('\n');
    }
    let first = head.len();
    let objstm = miniz_oxide::deflate::compress_to_vec_zlib(format!("{head}{body}").as_bytes(), 6);
    let mut f = b"%PDF-1.5\n".to_vec();
    let stm_off = f.len();
    let n = objs.len();
    f.extend(format!("{} 0 obj\n<< /Type /ObjStm /N {n} /First {first} /Filter /FlateDecode /Length {} >>\nstream\n", n + 1, objstm.len()).bytes());
    f.extend(&objstm);
    f.extend(b"\nendstream\nendobj\n");
    let xref_off = f.len();
    // Rows of [type, field 2 (two bytes), field 3].
    let mut rows: Vec<[u8; 4]> = vec![[0, 0, 0, 0]];
    for i in 0..n {
        rows.push([2, 0, (n + 1) as u8, i as u8]);
    }
    rows.push([1, (stm_off >> 8) as u8, stm_off as u8, 0]);
    rows.push([1, (xref_off >> 8) as u8, xref_off as u8, 0]);
    let mut raw = Vec::new();
    let mut prev = [0u8; 4];
    for r in &rows {
        raw.push(2);
        raw.extend(r.iter().zip(prev).map(|(a, b)| a.wrapping_sub(b)));
        prev = *r;
    }
    let data = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
    f.extend(
        format!(
            "{} 0 obj\n<< /Type /XRef /Size {} /W [1 2 1] /Root 1 0 R /Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 4 >> /Length {} >>\nstream\n",
            n + 2,
            n + 3,
            data.len()
        )
        .bytes(),
    );
    f.extend(&data);
    f.extend(format!("\nendstream\nendobj\nstartxref\n{xref_off}\n%%EOF\n").bytes());
    f
}

fn near(v: &Value, want: &[f64]) {
    let got: Vec<f64> =
        v.as_array().unwrap().iter().flat_map(|p| p.as_array().cloned().unwrap_or_else(|| vec![p.clone()])).map(|x| x.as_f64().unwrap()).collect();
    assert_eq!(got.len(), want.len(), "{v}");
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() < 1e-9, "{v} vs {want:?}");
    }
}

fn check(m: &PdfMarkups) {
    assert_eq!((m.width, m.height, m.page_count), (200.0, 100.0, 1));
    let kinds: Vec<&str> = m.markups.iter().map(|v| v["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["text", "cloud", "text", "polyline", "leader"]);
    let t = &m.markups[0];
    assert_eq!((t["text"].clone(), t["color"].clone()), (json!("Check this"), json!("#ff0000")));
    near(&t["at"], &[0.1, 0.1]);
    assert!((t["height"].as_f64().unwrap() - 0.1).abs() < 1e-9);
    // Corners, top left clockwise; the comment sits at the top left of the cloud.
    near(&m.markups[1]["points"], &[0.05, 0.6, 0.3, 0.6, 0.3, 0.9, 0.05, 0.9]);
    assert_eq!(m.markups[1]["color"], json!("#0000ff"));
    assert_eq!(m.markups[2]["text"], json!("Revise"));
    near(&m.markups[2]["at"], &[0.05, 0.6]);
    near(&m.markups[3]["points"], &[0.05, 0.9, 0.1, 0.8, 0.15, 0.9]);
    // The arrow is at the end of the line, so the leader starts there.
    near(&m.markups[4]["points"], &[0.75, 0.8, 0.5, 0.8]);
    assert_eq!(m.markups[4]["text"], json!("See here"));
}

#[test]
fn classic_xref_file() {
    check(&pdf_markups(&classic(&OBJS), 1).unwrap());
}

#[test]
fn compressed_xref_and_object_stream() {
    check(&pdf_markups(&compressed(&OBJS), 1).unwrap());
}

#[test]
fn page_rotation_turns_the_markups() {
    let mut o = OBJS;
    o[2] = "<< /Type /Page /Parent 2 0 R /Rotate 90 /Annots [4 0 R] >>";
    let m = pdf_markups(&classic(&o), 1).unwrap();
    assert_eq!((m.width, m.height), (100.0, 200.0));
    // The box spans u 0.1..0.6, v 0.1..0.4 unturned; a quarter turn clockwise maps it to
    // u 0.6..0.9, v 0.1..0.6, so its top left is (0.6, 0.1).
    near(&m.markups[0]["at"], &[0.6, 0.1]);
}

#[test]
fn missing_page_and_non_pdf_are_errors() {
    assert!(pdf_markups(&classic(&OBJS), 2).unwrap_err().to_string().contains("1 page"));
    assert!(pdf_markups(&classic(&OBJS), 0).is_err());
    assert!(pdf_markups(b"not a pdf", 1).is_err());
}

#[test]
fn other_annotations_are_noted_not_errors() {
    let mut o = OBJS;
    o[3] = "<< /Type /Annot /Subtype /Highlight /Rect [0 0 1 1] >>";
    let m = pdf_markups(&classic(&o), 1).unwrap();
    assert_eq!(m.notes, ["skipped a /Highlight annotation"]);
    assert_eq!(m.markups.len(), 4);
}

#[test]
fn hostile_input_never_panics() {
    for file in [classic(&OBJS), compressed(&OBJS)] {
        for n in 0..file.len() {
            let _ = pdf_markups(&file[..n], 1);
        }
        for i in 0..file.len() {
            for x in [0xFFu8, 0x20, 0x01] {
                let mut f = file.clone();
                f[i] ^= x;
                let _ = pdf_markups(&f, 1);
            }
        }
    }
    // A page tree that contains itself, a /Prev chain that loops, a deeply nested array.
    let cyc = classic(&["<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [2 0 R 2 0 R] >>"]);
    assert!(pdf_markups(&cyc, 1).is_err());
    let s = String::from_utf8_lossy(&classic(&OBJS)).replace("/Root 1 0 R", "/Root 1 0 R /Prev 9");
    let _ = pdf_markups(s.as_bytes(), 1);
    let deep = classic(&[&format!("<< /Type /Catalog /Pages {} >>", "[".repeat(5000))]);
    assert!(pdf_markups(&deep, 1).is_err());
    // A flate bomb is refused, not inflated.
    let bomb = miniz_oxide::deflate::compress_to_vec_zlib(&vec![0u8; 40 << 20], 1);
    let mut f = b"%PDF-1.5\n1 0 obj\n<< /Filter /FlateDecode >>\nstream\n".to_vec();
    f.extend(bomb);
    f.extend(b"\nendstream\nendobj\nstartxref\n9\n%%EOF");
    assert!(pdf_markups(&f, 1).is_err());
}

#[test]
fn image_sizes() {
    let mut png = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
    png.extend(640u32.to_be_bytes());
    png.extend(480u32.to_be_bytes());
    assert_eq!(image_size(&png), Some((640, 480)));
    let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 11, 8, 1, 0xE0, 2, 0x80, 1, 1, 0x11, 0];
    assert_eq!(image_size(&jpg), Some((640, 480)));
    for n in 0..png.len() {
        assert_eq!(image_size(&png[..n]), None);
    }
    for n in 0..jpg.len() {
        let _ = image_size(&jpg[..n]);
    }
    assert_eq!(image_size(b"GIF89a"), None);
}
