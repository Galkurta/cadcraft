//! Raster images in the display list and the CPU rasteriser: placement, clip, fade, transparency,
//! missing and hostile files.

use std::io::Cursor;

use cadcraft_color::Rgb;
use cadcraft_doc::{Common, Drawing, EntityKind, Image, Space};
use cadcraft_geom::{Vec2, Vec3};
use cadcraft_render::raster::{RasterOptions, View, render};
use cadcraft_render::{DisplayList, Kind, Options, build, images};

/// A 2×1 PNG: red left, blue right.
fn png() -> Vec<u8> {
    let img = image::RgbaImage::from_fn(2, 1, |x, _| if x == 0 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) });
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
    out
}

/// A 10 × 10 unit picture at the origin showing `name`.
fn picture(name: &str) -> Image {
    Image {
        insert: Vec3::ZERO,
        u: Vec3::new(5.0, 0.0, 0.0),
        v: Vec3::new(0.0, 10.0, 0.0),
        size: Vec2::new(2.0, 1.0),
        path: name.into(),
        ..Image::default()
    }
}

fn drawing(i: Image, layer: &str) -> Drawing {
    let mut d = Drawing::new_imperial();
    d.ensure_layer(layer);
    d.add(&Space::Model, Common { layer: layer.into(), ..Common::default() }, EntityKind::Image(i)).unwrap();
    d
}

fn pixel(list: &DisplayList, x: u32, y: u32) -> [u8; 3] {
    let view = View { center: Vec2::new(5.0, 5.0), scale: 10.0, width: 100, height: 100 };
    let o = RasterOptions { background: Rgb(255, 255, 255), antialias: false, ..Default::default() };
    let p = render(list, &view, &o).unwrap().pixel(x, y).unwrap();
    [p.red(), p.green(), p.blue()]
}

fn list(d: &Drawing) -> DisplayList {
    build(d, &Space::Model, &Options::default())
}

#[test]
fn image_pixels_land_where_the_image_is_placed() {
    images::put_bytes("test:place.png", png()).unwrap();
    let l = list(&drawing(picture("test:place.png"), "0"));
    assert_eq!(pixel(&l, 25, 50), [255, 0, 0], "left half is red");
    assert_eq!(pixel(&l, 75, 50), [0, 0, 255], "right half is blue");
    assert_eq!(l.prims.iter().filter(|p| matches!(p.kind, Kind::Image(_))).count(), 1);
}

#[test]
fn image_offset_and_rotation_move_the_pixels() {
    images::put_bytes("test:move.png", png()).unwrap();
    // Rotated a quarter turn about its corner: u points up, v points left; at (10, 0) it covers x 0..10, y 0..10.
    let i = Image { insert: Vec3::new(10.0, 0.0, 0.0), u: Vec3::new(0.0, 5.0, 0.0), v: Vec3::new(-10.0, 0.0, 0.0), ..picture("test:move.png") };
    let l = list(&drawing(i, "0"));
    // Red is the first pixel along u: the lower half (screen y 75), blue the upper (25).
    assert_eq!(pixel(&l, 50, 75), [255, 0, 0]);
    assert_eq!(pixel(&l, 50, 25), [0, 0, 255]);
}

#[test]
fn fade_and_clip_change_what_is_drawn() {
    images::put_bytes("test:fade.png", png()).unwrap();
    let faded = Image { fade: 50, ..picture("test:fade.png") };
    let p = pixel(&list(&drawing(faded, "0")), 25, 50);
    assert!(p[0] == 255 && (120..140).contains(&p[1]) && (120..140).contains(&p[2]), "half way to white: {p:?}");
    // Clipped to the left pixel column: the right half shows the background.
    let clipped = Image { clipping: true, clip: vec![Vec2::new(-0.5, -0.5), Vec2::new(0.5, 0.5)], ..picture("test:fade.png") };
    let l = list(&drawing(clipped, "0"));
    assert_eq!(pixel(&l, 25, 50), [255, 0, 0]);
    assert_eq!(pixel(&l, 75, 50), [255, 255, 255]);
}

#[test]
fn monochrome_grays_the_picture_and_an_inverted_clip_cuts_a_hole() {
    images::put_bytes("test:style.png", png()).unwrap();
    let gray = Image { monochrome: true, ..picture("test:style.png") };
    let l = list(&drawing(gray, "0"));
    let (red, blue) = (pixel(&l, 25, 50), pixel(&l, 75, 50));
    assert!(red[0] == red[1] && red[1] == red[2] && (70..82).contains(&red[0]), "red in gray: {red:?}");
    assert!(blue[0] == blue[1] && blue[1] == blue[2] && (25..35).contains(&blue[0]), "blue in gray: {blue:?}");
    // The boundary covers the left pixel column; inverted, that column is the hole.
    let boundary = vec![Vec2::new(-0.5, -0.5), Vec2::new(0.5, 0.5)];
    let inside = Image { clipping: true, clip: boundary.clone(), ..picture("test:style.png") };
    let hole = Image { clipping: true, clip_inverted: true, clip: boundary, ..picture("test:style.png") };
    let l = list(&drawing(inside, "0"));
    assert_eq!((pixel(&l, 25, 50), pixel(&l, 75, 50)), ([255, 0, 0], [255, 255, 255]));
    let l = list(&drawing(hole, "0"));
    assert_eq!((pixel(&l, 25, 50), pixel(&l, 75, 50)), ([255, 255, 255], [0, 0, 255]), "the clipped-out column is cut out, the rest stays");
}

#[test]
fn object_transparency_blends_with_the_background() {
    images::put_bytes("test:alpha.png", png()).unwrap();
    let mut d = drawing(picture("test:alpha.png"), "0");
    let h = d.model.iter().next().unwrap().handle;
    d.model.modify(h, |e| e.common.transparency = cadcraft_doc::Transparency::Percent(50));
    let on = Options { transparency: true, ..Options::default() };
    let p = {
        let l = build(&d, &Space::Model, &on);
        pixel(&l, 25, 50)
    };
    assert!(p[0] == 255 && (120..140).contains(&p[1]), "{p:?}");
    // Without Options::transparency the image stays opaque.
    assert_eq!(pixel(&list(&d), 25, 50), [255, 0, 0]);
}

#[test]
fn a_missing_file_draws_frame_cross_and_name() {
    let l = list(&drawing(picture("D:/nowhere/plan-sheet.png"), "0"));
    assert!(!l.prims.iter().any(|p| matches!(p.kind, Kind::Image(_))));
    let polylines = l.prims.iter().filter(|p| p.kind == Kind::Polyline).count();
    assert!(polylines > 3, "frame, two diagonals and name strokes: {polylines}");
    // Frame edge pixels are drawn; the render does not fail.
    assert_eq!(pixel(&l, 50, 0)[0], 0, "top edge of the frame is black");
}

#[test]
fn hostile_pictures_are_errors_not_panics() {
    images::put_bytes("test:junk.png", b"\x89PNG\r\n\x1a\nnot really".to_vec()).unwrap();
    images::put_bytes("test:empty.png", Vec::new()).unwrap();
    let l = list(&drawing(picture("test:junk.png"), "0"));
    assert_eq!(pixel(&l, 50, 50), [255, 255, 255], "an undecodable picture draws nothing");
    assert!(images::load("test:junk.png", 0, 0).is_err());
    assert!(images::load("test:empty.png", 0, 0).is_err());
    assert!(images::load("test:none.png", 0, 0).is_err());
    // A valid 8000 × 8000 PNG (64 megapixels) is refused before any pixel is allocated.
    let mut big = Vec::new();
    image::GrayImage::new(8000, 8000).write_to(&mut Cursor::new(&mut big), image::ImageFormat::Png).unwrap();
    images::put_bytes("test:huge.png", big).unwrap();
    assert!(images::load("test:huge.png", 0, 0).unwrap_err().contains("pixels"));
    // PDF pages without a reader installed, and NaN placements.
    assert!(images::load("test:place.png", 1, 512).is_err());
    let nan = Image { insert: Vec3::new(f64::NAN, 0.0, 0.0), u: Vec3::new(f64::INFINITY, 0.0, 0.0), ..picture("test:junk.png") };
    let l = list(&drawing(nan, "0"));
    assert!(l.prims.is_empty());
    let zero = Image { size: Vec2::new(0.0, f64::NAN), ..picture("test:junk.png") };
    let _ = list(&drawing(zero, "0"));
}
