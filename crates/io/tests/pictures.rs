//! Raster images: DXF round trip and the picture in SVG and PDF plots. The files are generated
//! here; nothing is read from disk.

use std::io::Cursor;

use cadcraft_doc::{Common, Drawing, EntityKind, Image, Space};
use cadcraft_geom::{Vec2, Vec3};
use cadcraft_render::images;

fn png() -> Vec<u8> {
    let img = image::RgbaImage::from_fn(2, 1, |x, _| if x == 0 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) });
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
    out
}

fn add(i: Image) -> Drawing {
    let mut d = Drawing::new_imperial();
    d.add(&Space::Model, Common::default(), EntityKind::Image(i)).unwrap();
    d
}

#[test]
fn images_survive_dxf_save_and_reload() {
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
    d.add(&Space::Model, Common::default(), EntityKind::Image(raster.clone())).unwrap();
    let text = cadcraft_io::write_dxf(&d);
    let back = cadcraft_io::read_dxf(text.as_bytes()).unwrap();
    let imgs: Vec<&Image> = back.model.iter().filter_map(|e| if let EntityKind::Image(i) = &e.kind { Some(i) } else { None }).collect();
    assert_eq!(imgs.len(), 1);
    let r = imgs[0];
    assert_eq!((r.path.as_str(), r.fade, r.insert), ("pics/logo.png", 20, raster.insert));
    assert!((r.u - raster.u).len() < 1e-9 && (r.v - raster.v).len() < 1e-9, "scale and rotation survive: {:?} {:?}", r.u, r.v);
    // And a second save of what was read is stable (no duplicated definitions).
    let again = cadcraft_io::write_dxf(&back);
    assert_eq!(again.matches("IMAGEDEF\n").count(), text.matches("IMAGEDEF\n").count());
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
