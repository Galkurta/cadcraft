//! Raster images and PDF underlays: DXF round trip, PDF pages as pictures, and the picture in
//! SVG and PDF plots. The files are generated here; nothing is read from disk.

use std::io::Cursor;

use cadcraft_color::Rgb;
use cadcraft_doc::{Common, Drawing, EntityKind, Image, Space};
use cadcraft_geom::{Vec2, Vec3};
use cadcraft_render::images;
use cadcraft_render::raster::{RasterOptions, View, render};

fn png() -> Vec<u8> {
    let img = image::RgbaImage::from_fn(2, 1, |x, _| if x == 0 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) });
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
    out
}

/// A one-page PDF of `w` × `h` points (rotated by `rotate`) drawing `content`.
fn pdf(w: u32, h: u32, rotate: u32, content: &str) -> Vec<u8> {
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Rotate {rotate} /Contents 4 0 R >>"),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
    for o in &offsets {
        out.extend(format!("{o:010} 00000 n \n").bytes());
    }
    out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).bytes());
    out
}

fn underlay(path: &str, w: f64, h: f64) -> Image {
    Image { u: Vec3::new(1.0, 0.0, 0.0), v: Vec3::new(0.0, 1.0, 0.0), size: Vec2::new(w, h), path: path.into(), page: 1, ..Image::default() }
}

fn add(i: Image) -> Drawing {
    let mut d = Drawing::new_imperial();
    d.add(&Space::Model, Common::default(), EntityKind::Image(i)).unwrap();
    d
}

/// The colour at screen pixel (x, y) of a 100 × 100 view of the 0..100 square.
fn sample(d: &Drawing, x: u32, y: u32) -> [u8; 3] {
    cadcraft_io::register_image_decoders();
    let list = cadcraft_render::build(d, &Space::Model, &cadcraft_render::Options::default());
    let view = View { center: Vec2::new(50.0, 50.0), scale: 1.0, width: 100, height: 100 };
    let o = RasterOptions { background: Rgb(255, 255, 255), antialias: false, ..Default::default() };
    let p = render(&list, &view, &o).unwrap().pixel(x, y).unwrap();
    [p.red(), p.green(), p.blue()]
}

#[test]
fn images_and_pdf_underlays_survive_dxf_save_and_reload() {
    let mut d = Drawing::new_imperial();
    let raster = Image {
        insert: Vec3::new(3.0, 4.0, 0.0),
        u: Vec3::new(0.5, 0.0, 0.0),
        v: Vec3::new(0.0, 0.5, 0.0),
        size: Vec2::new(2.0, 1.0),
        path: "pics/logo.png".into(),
        fade: 20,
        ..Image::default()
    };
    let pdf = Image {
        insert: Vec3::new(10.0, 20.0, 0.0),
        u: Vec3::new(0.0, 0.25, 0.0),
        v: Vec3::new(-0.25, 0.0, 0.0),
        path: "sheets/plan.pdf".into(),
        page: 3,
        clipping: true,
        clip: vec![Vec2::new(10.0, 10.0), Vec2::new(300.0, 400.0)],
        fade: 30,
        contrast: 70,
        ..underlay("", 0.0, 0.0)
    };
    d.add(&Space::Model, Common::default(), EntityKind::Image(raster.clone())).unwrap();
    d.add(&Space::Model, Common::default(), EntityKind::Image(pdf.clone())).unwrap();
    let text = cadcraft_io::write_dxf(&d);
    assert!(text.contains("PDFUNDERLAY") && text.contains("PDFDEFINITION") && text.contains("ACAD_PDFDEFINITIONS"));
    let back = cadcraft_io::read_dxf(text.as_bytes()).unwrap();
    let imgs: Vec<&Image> = back.model.iter().filter_map(|e| if let EntityKind::Image(i) = &e.kind { Some(i) } else { None }).collect();
    assert_eq!(imgs.len(), 2);
    let r = imgs.iter().find(|i| i.page == 0).unwrap();
    assert_eq!((r.path.as_str(), r.fade, r.insert), ("pics/logo.png", 20, raster.insert));
    let p = imgs.iter().find(|i| i.page > 0).unwrap();
    assert_eq!((p.path.as_str(), p.page, p.fade, p.contrast, p.clipping), ("sheets/plan.pdf", 3, 30, 70, true));
    assert_eq!(p.clip, pdf.clip);
    assert_eq!(p.insert, pdf.insert);
    assert!((p.u - pdf.u).len() < 1e-9 && (p.v - pdf.v).len() < 1e-9, "scale and rotation survive: {:?} {:?}", p.u, p.v);
    // And a second save of what was read is stable (no duplicated definitions).
    let again = cadcraft_io::write_dxf(&back);
    assert_eq!(again.matches("PDFDEFINITION").count(), text.matches("PDFDEFINITION").count());
}

#[test]
fn a_pdf_page_is_drawn_under_the_underlay_placement() {
    // Left half of a 100 × 50 pt page is red, the right half blue-stroked.
    images::put_bytes("test:page.pdf", pdf(100, 50, 0, "1 0 0 rg 0 0 50 50 re f 0 0 1 rg 50 0 50 25 re f")).unwrap();
    let d = add(underlay("test:page.pdf", 100.0, 50.0));
    // The page sits at the origin, 100 × 50 units: screen y 50..100 shows it.
    assert_eq!(sample(&d, 25, 75), [255, 0, 0], "red left half");
    assert_eq!(sample(&d, 75, 88), [0, 0, 255], "blue lower right");
    assert_eq!(sample(&d, 75, 60), [255, 255, 255], "paper white upper right");
    assert_eq!(sample(&d, 50, 20), [255, 255, 255], "above the page: background");
}

#[test]
fn a_rotated_page_is_drawn_rotated() {
    // /Rotate 90 (clockwise) of a 100 × 50 page shows 50 × 100: the user-space left half (red) is on top.
    images::put_bytes("test:rot.pdf", pdf(100, 50, 90, "1 0 0 rg 0 0 50 50 re f")).unwrap();
    let d = add(underlay("test:rot.pdf", 0.0, 0.0));
    assert_eq!(sample(&d, 25, 25), [255, 0, 0], "top is red");
    assert_eq!(sample(&d, 25, 75), [255, 255, 255], "bottom is paper");
}

#[test]
fn a_missing_page_or_pdf_draws_only_the_frame() {
    images::put_bytes("test:two.pdf", pdf(100, 100, 0, "0 g 0 0 100 100 re f")).unwrap();
    let mut i = underlay("test:two.pdf", 100.0, 100.0);
    i.page = 2;
    let d = add(i);
    // Page 2 doesn't exist: nothing but the (drawn) frame; the render succeeds.
    assert_eq!(sample(&d, 50, 50), [255, 255, 255]);
    let gone = add(underlay("D:/gone/plan.pdf", 100.0, 100.0));
    assert_eq!(sample(&gone, 25, 50), [255, 255, 255], "frame and name only");
}

#[test]
fn hostile_pdf_content_never_panics() {
    cadcraft_io::register_image_decoders();
    let streams = [
        "",
        "Q Q Q Q 1 0 0 rg",
        "q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q q Q",
        "1e400 0 0 1e400 0 0 cm 0 0 1e30 1e30 re f 0 0 m 1e300 1e300 l S",
        "0 0 m 10 10 l (unterminated string S",
        "BI /W 1 /H 1 ID \u{0001}\u{0002} no end",
        "[ 1 2 3 d 0 0 m 5 5 l S [ 0 0 ] 0 d 0 0 m 5 5 l S",
        "-1 w NaN w 0 0 m 1 1 l S 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 re f",
        "0 0 100 100 re W n 0 0 100 100 re W* n 0 0 0 0 re W n 10 10 m 20 20 l f",
        "<< /A << /B (c) >> >> 0 0 m << unterminated",
        "0 0 0 0 0 0 cm 0 0 10 10 re f 1 0 0 1 0 0 cm",
    ];
    for (n, s) in streams.iter().enumerate() {
        let name = format!("test:hostile{n}.pdf");
        images::put_bytes(&name, pdf(100, 100, 0, s)).unwrap();
        let _ = images::load(&name, 1, 512);
    }
    // Damaged and wrong files, and absurd page boxes.
    for (n, bytes) in
        [b"%PDF-1.4 garbage".to_vec(), b"not a pdf".to_vec(), pdf(0, 0, 0, "0 0 m"), pdf(100000, 100000, 0, "0 0 10 10 re f"), pdf(100, 100, 45, "")]
            .into_iter()
            .enumerate()
    {
        let name = format!("test:bad{n}.pdf");
        images::put_bytes(&name, bytes).unwrap();
        let _ = images::load(&name, 1, 512);
        let _ = images::natural_size(&name, 1);
        let _ = images::load(&name, 0, 512);
        let _ = images::load(&name, u32::MAX, 512);
    }
    // A hostile placement through the DXF reader.
    let text =
        "0\nSECTION\n2\nENTITIES\n0\nPDFUNDERLAY\n5\n20\n100\nAcDbUnderlayReference\n340\nFFFF\n10\nnan\n41\n1e999\n50\n1e999\n0\nENDSEC\n0\nEOF\n";
    let _ = cadcraft_io::read_dxf(text.as_bytes());
}

#[test]
fn svg_and_pdf_plots_embed_the_picture() {
    images::put_bytes("test:plot.png", png()).unwrap();
    let i = Image {
        insert: Vec3::ZERO,
        u: Vec3::new(5.0, 0.0, 0.0),
        v: Vec3::new(0.0, 10.0, 0.0),
        size: Vec2::new(2.0, 1.0),
        path: "test:plot.png".into(),
        ..Image::default()
    };
    let d = add(i);
    let svg = cadcraft_io::svg::export(&d, &Space::Model);
    assert!(svg.contains("<image") && svg.contains("data:image/png;base64,iVBOR"), "{}", &svg[..svg.len().min(400)]);
    let pdf = cadcraft_io::pdf(&d, &Space::Model, &cadcraft_io::PdfOptions::default()).unwrap();
    let text = String::from_utf8_lossy(&pdf);
    // Streams are compressed by default, but the image dictionary and the resource entry are plain.
    assert!(text.contains("/Subtype /Image") && text.contains("/XObject <<") && text.contains("/DeviceRGB"));
    // A missing file adds nothing to embed.
    let gone = add(Image { path: "D:/none/x.png".into(), ..Image::default() });
    let pdf = cadcraft_io::pdf(&gone, &Space::Model, &cadcraft_io::PdfOptions::default()).unwrap();
    assert!(!String::from_utf8_lossy(&pdf).contains("/Subtype /Image"));
}
