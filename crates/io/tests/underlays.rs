//! DWF and DGN underlays: the DXF round trip (DWFUNDERLAY / DGNUNDERLAY with their definitions
//! and dictionaries), the pictures in raster, SVG and PDF output, and hostile input. The files
//! are generated here; nothing is read from disk.

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

/// A DGN V7 2D file: TCB and one red line of 100 × 50 master units.
fn dgn() -> Vec<u8> {
    let pdp = |v: i32| {
        let b = v.to_le_bytes();
        [b[2], b[3], b[0], b[1]]
    };
    let mut f = vec![0u8; 1536];
    f[0] = 0x08;
    f[1] = 0x09;
    f[2..4].copy_from_slice(&766u16.to_le_bytes());
    f[1112..1116].copy_from_slice(&pdp(10));
    f[1116..1120].copy_from_slice(&pdp(100));
    let mut line = vec![0u8; 52];
    line[0] = 0x01;
    line[1] = 0x03;
    line[2..4].copy_from_slice(&24u16.to_le_bytes());
    line[34..36].copy_from_slice(&((3u16 << 8) | (3 << 3)).to_le_bytes());
    for (at, v) in [(36, 0), (40, 0), (44, 100_000), (48, 50_000)] {
        line[at..at + 4].copy_from_slice(&pdp(v));
    }
    f.extend(line);
    f.extend([0xFF, 0xFF]);
    f
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
fn dwf_and_dgn_underlays_round_trip_through_dxf() {
    let mut d = Drawing::new_imperial();
    for (path, kind, sheet, page) in [
        ("D:/a/plan.dwf", UnderlayKind::Dwf, "Floor 1", 1),
        ("D:/a/plan.dwf", UnderlayKind::Dwf, "Floor 2", 2),
        ("D:/a/site.dgn", UnderlayKind::Dgn, "Default", 1),
    ] {
        let i = Image { page, insert: Vec3::new(3.0, 4.0, 0.0), ..underlay(path, kind, sheet, 800.0, 600.0) };
        d.add(&Space::Model, Common::default(), EntityKind::Image(i)).unwrap();
    }
    // A PDF next to them keeps its own entity and dictionary.
    d.add(&Space::Model, Common::default(), EntityKind::Image(underlay("D:/a/x.pdf", UnderlayKind::Pdf, "", 595.0, 842.0))).unwrap();
    let text = cadcraft_io::write_dxf(&d).replace("\r\n", "\n");
    for name in [
        "DWFUNDERLAY",
        "DWFDEFINITION",
        "ACAD_DWFDEFINITIONS",
        "DGNUNDERLAY",
        "DGNDEFINITION",
        "ACAD_DGNDEFINITIONS",
        "PDFUNDERLAY",
        "PDFDEFINITION",
        "ACAD_PDFDEFINITIONS",
        "AcDbDwfReference",
        "AcDbDgnReference",
    ] {
        assert!(text.contains(name), "{name}");
    }
    assert_eq!(text.matches("0\nDWFDEFINITION\n").count(), 2, "two sheets of one file are two definitions");
    let back = cadcraft_io::read_dxf(text.as_bytes()).unwrap();
    let (before, after) = (images_of(&d), images_of(&back));
    assert_eq!(after.len(), 4);
    for (a, b) in before.iter().zip(&after) {
        assert_eq!((a.kind, &a.path), (b.kind, &b.path));
        assert!((a.u.x - b.u.x).abs() < 1e-9 && (a.insert.x - b.insert.x).abs() < 1e-9);
    }
    // The sheet names survive (the files can't be read here, so the number comes from the name).
    assert_eq!(after[0].sheet, "Floor 1");
    assert_eq!(after[1].sheet, "Floor 2");
    assert_eq!((after[2].kind, after[2].page), (UnderlayKind::Dgn, 1));
    assert_eq!(after[3].kind, UnderlayKind::Pdf);
    // Writing again gives the same objects.
    let again = cadcraft_io::write_dxf(&back).replace("\r\n", "\n");
    for name in ["0\nDWFUNDERLAY\n", "0\nDWFDEFINITION\n", "0\nDGNUNDERLAY\n", "0\nDGNDEFINITION\n", "0\nPDFUNDERLAY\n"] {
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
fn the_underlays_draw_in_the_raster_svg_and_pdf_outputs() {
    cadcraft_io::register_image_decoders();
    images::put_bytes("test:out.dwf", w2d()).unwrap();
    images::put_bytes("test:out.dgn", dgn()).unwrap();
    for (path, kind) in [("test:out.dwf", UnderlayKind::Dwf), ("test:out.dgn", UnderlayKind::Dgn)] {
        let (w, h) = images::natural_size(path, 1).unwrap();
        let mut i = underlay(path, kind, "", w, h);
        i.u = Vec3::new(100.0 / w, 0.0, 0.0);
        i.v = Vec3::new(0.0, 100.0 / w, 0.0);
        let d = add(i);
        // The sheet is the lower part of the view and its centre is on the red diagonal.
        let centre_y = 100 - (50.0 * h / w) as u32;
        let red = (47..=53)
            .any(|x| (centre_y.saturating_sub(3)..=centre_y + 3).any(|y| matches!(sample(&d, x, y), [r, g, b] if r > 200 && g < 235 && b < 235)));
        assert!(red, "{path}: the diagonal is drawn through the sheet centre");
        assert_eq!(sample(&d, 50, 2), [255, 255, 255], "{path}: nothing above the sheet");
        let svg = cadcraft_io::svg::export(&d, &Space::Model);
        assert!(svg.contains("<image") && svg.contains("data:image/png;base64,"), "{path}");
        let pdf = cadcraft_io::pdf(&d, &Space::Model, &cadcraft_io::PdfOptions::default()).unwrap();
        assert!(String::from_utf8_lossy(&pdf).contains("/Subtype /Image"), "{path}");
    }
}

#[test]
fn hostile_underlay_files_and_dxf_never_panic() {
    cadcraft_io::register_image_decoders();
    for base in [w2d(), dgn()] {
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
    }
    // The OLE signature of a DGN V8 file.
    let mut v8 = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    v8.resize(1024, 0);
    assert_eq!(cadcraft_io::underlay_sheets(&v8).unwrap_err(), "DGN V8 is not supported: no public specification");
    assert_eq!(cadcraft_io::underlay_kind(&v8).unwrap_err(), "DGN V8 is not supported: no public specification");
    images::put_bytes("test:v8.dgn", v8).unwrap();
    assert_eq!(images::natural_size("test:v8.dgn", 1).unwrap_err(), "DGN V8 is not supported: no public specification");
    // Hostile placements and definitions through the DXF reader.
    for kind in ["DWF", "DGN"] {
        let text = format!(
            "0\nSECTION\n2\nENTITIES\n0\n{kind}UNDERLAY\n5\n20\n100\nAcDbUnderlayReference\n340\nFFFF\n10\nnan\n41\n1e999\n50\n1e999\n0\nENDSEC\n\
             0\nSECTION\n2\nOBJECTS\n0\n{kind}DEFINITION\n5\nFFFF\n1\n\n2\n\n0\nENDSEC\n0\nEOF\n"
        );
        let _ = cadcraft_io::read_dxf(text.as_bytes());
    }
}
