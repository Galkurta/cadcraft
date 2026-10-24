//! DWF underlays: the DXF round trip (DWFUNDERLAY with its definitions and dictionary), the
//! scale and display flags it shares with PDF underlays, the pictures in raster, SVG and PDF
//! output, and hostile input. The files are generated here; nothing is read from disk.

use cadcraft_color::Rgb;
use cadcraft_doc::{Common, Drawing, EntityKind, Image, Space, UnderlayKind};
use cadcraft_geom::{Vec2, Vec3};
use cadcraft_render::images;
use cadcraft_render::raster::{RasterOptions, View, render};

/// A W2D stream: a red diagonal across a 1000 × 500 sheet.
fn w2d() -> Vec<u8> {
    let mut b = b"(W2D V06.00)".to_vec();
    b.extend(b"(Bounds 0,0 1000,500)\n(Color 255,0,0,255)\n(LineWeight 30)\nL 0,0 1000,500\n(EndOfDWF)");
    b
}

fn underlay(path: &str, kind: UnderlayKind, sheet: &str, w: f64, h: f64) -> Image {
    Image {
        u: Vec3::new(100.0 / w, 0.0, 0.0),
        v: Vec3::new(0.0, 100.0 / w, 0.0),
        size: Vec2::new(w, h),
        path: path.into(),
        page: 1,
        kind,
        sheet: sheet.into(),
        ..Image::default()
    }
}

fn add(i: Image) -> Drawing {
    let mut d = Drawing::new_imperial();
    d.add(&Space::Model, Common::default(), EntityKind::Image(i)).unwrap();
    d
}

fn images_of(d: &Drawing) -> Vec<Image> {
    d.model.iter().filter_map(|e| if let EntityKind::Image(i) = &e.kind { Some(i.clone()) } else { None }).collect()
}

#[test]
fn dwf_underlays_round_trip_through_dxf() {
    let mut d = Drawing::new_imperial();
    for (path, kind, sheet, page) in [("D:/a/plan.dwf", UnderlayKind::Dwf, "Floor 1", 1), ("D:/a/plan.dwf", UnderlayKind::Dwf, "Floor 2", 2)] {
        let i = Image { page, insert: Vec3::new(3.0, 4.0, 0.0), ..underlay(path, kind, sheet, 800.0, 600.0) };
        d.add(&Space::Model, Common::default(), EntityKind::Image(i)).unwrap();
    }
    // A PDF next to them keeps its own entity and dictionary.
    d.add(&Space::Model, Common::default(), EntityKind::Image(underlay("D:/a/x.pdf", UnderlayKind::Pdf, "", 595.0, 842.0))).unwrap();
    let text = cadcraft_io::write_dxf(&d).replace("\r\n", "\n");
    for name in ["DWFUNDERLAY", "DWFDEFINITION", "ACAD_DWFDEFINITIONS", "PDFUNDERLAY", "PDFDEFINITION", "ACAD_PDFDEFINITIONS", "AcDbDwfReference"] {
        assert!(text.contains(name), "{name}");
    }
    assert_eq!(text.matches("0\nDWFDEFINITION\n").count(), 2, "two sheets of one file are two definitions");
    let back = cadcraft_io::read_dxf(text.as_bytes()).unwrap();
    let (before, after) = (images_of(&d), images_of(&back));
    assert_eq!(after.len(), 3);
    for (a, b) in before.iter().zip(&after) {
        assert_eq!((a.kind, &a.path), (b.kind, &b.path));
        assert!((a.u.x - b.u.x).abs() < 1e-9 && (a.insert.x - b.insert.x).abs() < 1e-9);
    }
    // The sheet names survive (the files can't be read here, so the number comes from the name).
    assert_eq!(after[0].sheet, "Floor 1");
    assert_eq!(after[1].sheet, "Floor 2");
    assert_eq!(after[2].kind, UnderlayKind::Pdf);
    // Writing again gives the same objects.
    let again = cadcraft_io::write_dxf(&back).replace("\r\n", "\n");
    for name in ["0\nDWFUNDERLAY\n", "0\nDWFDEFINITION\n", "0\nPDFUNDERLAY\n"] {
        assert_eq!(again.matches(name).count(), text.matches(name).count(), "{name}");
    }
}

#[test]
fn a_dwf_definition_finds_its_sheet_in_the_file_when_it_can_read_it() {
    cadcraft_io::register_image_decoders();
    images::put_bytes("test:named.dwf", w2d()).unwrap();
    let d = add(underlay("test:named.dwf", UnderlayKind::Dwf, "Sheet 1", 100.0, 50.0));
    let back = cadcraft_io::read_dxf(cadcraft_io::write_dxf(&d).as_bytes()).unwrap();
    let i = images_of(&back).remove(0);
    assert_eq!((i.kind, i.page, i.sheet.as_str()), (UnderlayKind::Dwf, 1, "Sheet 1"));
}

/// A DXF with one DWFUNDERLAY of sheet "Sheet 1" of `path` whose scale groups are all `scale` and
/// whose display flags (group 280) are `flags`, clipped to a square.
fn dwf_dxf(path: &str, scale: &str, flags: i64) -> String {
    format!(
        "0\nSECTION\n2\nENTITIES\n0\nDWFUNDERLAY\n5\n20\n100\nAcDbEntity\n8\n0\n100\nAcDbUnderlayReference\n340\n30\n10\n0\n20\n0\n30\n0\n\
         41\n{scale}\n42\n{scale}\n43\n{scale}\n50\n0\n280\n{flags}\n281\n100\n282\n0\n11\n0\n21\n0\n11\n10\n21\n10\n0\nENDSEC\n\
         0\nSECTION\n2\nOBJECTS\n0\nDWFDEFINITION\n5\n30\n100\nAcDbUnderlayDefinition\n1\n{path}\n2\nSheet 1\n0\nENDSEC\n0\nEOF\n"
    )
}

/// The group `code` of the first DWFUNDERLAY entity in a written DXF.
fn group(dxf: &str, code: &str) -> String {
    let lines: Vec<&str> = dxf.lines().map(str::trim).collect();
    let pairs: Vec<(&str, &str)> = lines.chunks(2).filter_map(|c| Some((*c.first()?, *c.get(1)?))).collect();
    let start = pairs.iter().position(|p| *p == ("0", "DWFUNDERLAY")).unwrap();
    pairs.iter().skip(start + 1).take_while(|p| p.0 != "0").find(|p| p.0 == code).unwrap().1.to_string()
}

#[test]
fn a_dwf_underlay_shares_the_inch_scale_and_the_display_flags_of_a_pdf_underlay() {
    cadcraft_io::register_image_decoders();
    images::put_bytes("test:inch.dwf", w2d()).unwrap();
    let (w, h) = images::natural_size("test:inch.dwf", 1).unwrap();
    // Scale 1 is one drawing unit per inch of the sheet; the model counts per point.
    let d = cadcraft_io::read_dxf(dwf_dxf("test:inch.dwf", "1", 3).as_bytes()).unwrap();
    let i = images_of(&d).remove(0);
    assert_eq!((i.kind, i.page, i.sheet.as_str()), (UnderlayKind::Dwf, 1, "Sheet 1"));
    assert!((i.u.x - 1.0 / 72.0).abs() < 1e-12 && (i.v.y - 1.0 / 72.0).abs() < 1e-12, "{:?} {:?}", i.u, i.v);
    let list = cadcraft_render::build(&d, &Space::Model, &cadcraft_render::Options::default());
    assert!((list.images[0].u.len() - w / 72.0).abs() < 1e-9 && (list.images[0].v.len() - h / 72.0).abs() < 1e-9);
    assert_eq!(group(&cadcraft_io::write_dxf(&d), "41").parse::<f64>().unwrap(), 1.0);
    // Monochrome (4), adjust for the background (8) and inverted clip (16) round trip.
    let d = cadcraft_io::read_dxf(dwf_dxf("test:inch.dwf", "1", 31).as_bytes()).unwrap();
    let i = images_of(&d).remove(0);
    assert!(i.clipping && i.monochrome && i.adjust_for_background && i.clip_inverted);
    assert_eq!(group(&cadcraft_io::write_dxf(&d), "280").parse::<i64>().unwrap(), 31);
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
fn a_dwf_underlay_draws_in_the_raster_svg_and_pdf_outputs() {
    cadcraft_io::register_image_decoders();
    images::put_bytes("test:out.dwf", w2d()).unwrap();
    let path = "test:out.dwf";
    let (w, h) = images::natural_size(path, 1).unwrap();
    let mut i = underlay(path, UnderlayKind::Dwf, "", w, h);
    i.u = Vec3::new(100.0 / w, 0.0, 0.0);
    i.v = Vec3::new(0.0, 100.0 / w, 0.0);
    let d = add(i);
    // The sheet is the lower part of the view and its centre is on the red diagonal.
    let centre_y = 100 - (50.0 * h / w) as u32;
    let red = (47..=53)
        .any(|x| (centre_y.saturating_sub(3)..=centre_y + 3).any(|y| matches!(sample(&d, x, y), [r, g, b] if r > 200 && g < 235 && b < 235)));
    assert!(red, "the diagonal is drawn through the sheet centre");
    assert_eq!(sample(&d, 50, 2), [255, 255, 255], "nothing above the sheet");
    let svg = cadcraft_io::svg::export(&d, &Space::Model);
    assert!(svg.contains("<image") && svg.contains("data:image/png;base64,"));
    let pdf = cadcraft_io::pdf(&d, &Space::Model, &cadcraft_io::PdfOptions::default()).unwrap();
    assert!(String::from_utf8_lossy(&pdf).contains("/Subtype /Image"));
}

#[test]
fn hostile_underlay_files_and_dxf_never_panic() {
    cadcraft_io::register_image_decoders();
    let base = w2d();
    for cut in (0..base.len()).step_by(11) {
        let name = "test:hostile.bin";
        let _ = images::put_bytes(name, base[..cut].to_vec());
        let _ = images::natural_size(name, 1);
        let _ = images::load(name, 1, 512);
        let _ = images::load(name, 2, 512);
        let mut flipped = base.clone();
        if let Some(b) = flipped.get_mut(cut) {
            *b ^= 0xFF;
        }
        let _ = cadcraft_io::underlay_sheets(&flipped);
        let _ = cadcraft_io::underlay_kind(&flipped);
        let _ = images::put_bytes("test:hostile.flip", flipped);
        let _ = images::load("test:hostile.flip", 1, 512);
    }
    // Hostile placements and definitions through the DXF reader.
    let text = "0\nSECTION\n2\nENTITIES\n0\nDWFUNDERLAY\n5\n20\n100\nAcDbUnderlayReference\n340\nFFFF\n10\nnan\n41\n1e999\n50\n1e999\n0\nENDSEC\n\
                0\nSECTION\n2\nOBJECTS\n0\nDWFDEFINITION\n5\nFFFF\n1\n\n2\n\n0\nENDSEC\n0\nEOF\n";
    let _ = cadcraft_io::read_dxf(text.as_bytes());
}
