fn close(a: [u8; 4], b: [u8; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3)
}

use cadcraft_render::images::Raster;

use super::*;

fn l32(b: &mut Vec<u8>, v: i32) {
    b.extend(v.to_le_bytes());
}

fn u32le(b: &mut Vec<u8>, v: u32) {
    b.extend(v.to_le_bytes());
}

fn header() -> Vec<u8> {
    b"(W2D V06.00)".to_vec()
}

/// An extended binary block: `{`, length, opcode, data, `}`.
fn block(b: &mut Vec<u8>, op: u16, data: &[u8]) {
    b.push(b'{');
    u32le(b, (data.len() + 3) as u32);
    b.extend(op.to_le_bytes());
    b.extend(data);
    b.push(b'}');
}

/// The mapping of a `(Bounds 0,0 1000,1000)` sheet at its natural size, rendered 1 px per point:
/// logical (x, y) is at pixel (15.84 + 0.76032 x, 15.84 + 0.76032 (1000 - y)).
fn at(x: f64, y: f64) -> (u32, u32) {
    ((15.84 + 0.76032 * x) as u32, (15.84 + 0.76032 * (1000.0 - y)) as u32)
}

fn px(r: &Raster, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * r.width + x) * 4) as usize;
    r.rgba.get(i..i + 4).and_then(|s| <[u8; 4]>::try_from(s).ok()).unwrap_or([0; 4])
}

fn fixture() -> Vec<u8> {
    let mut b = header();
    b.extend(b"(DrawingInfo (Description 'x (y') (Bounds 0,0 1000,1000) (Scale 1 1 undefined))\n");
    b.extend(b"(Layer 1 'Walls')\n(Comment 'a ) tricky (')\n(Frobnicate 1 2)\n");
    // A filled red square, then blue line and ring.
    b.extend(b"(Color 255,0,0,255) F P 4 100,100 400,100 400,400 100,400 f\n");
    b.extend(b"(LineWeight 40) (Color 0,0,255,255) P 2 600,100 900,100\n");
    b.extend(b"R 500,800,100\n");
    // Binary colour, origin, an unknown extended block (skipped), then a relative line.
    b.push(0x03);
    b.extend([0, 128, 0, 255]);
    b.push(b'O');
    u32le(&mut b, 700);
    u32le(&mut b, 700);
    block(&mut b, 0x7777, b"abc");
    b.push(b'l');
    for v in [0, 0, 200, 0] {
        l32(&mut b, v);
    }
    // A colour map override and an indexed colour, then a relative polyline.
    b.extend(b"(ColorMap 2 10,10,10,255 255,128,0,255)");
    b.extend([b'c', 1]);
    b.push(b'p');
    b.push(3);
    for v in [-500, -200, 100, 100, 100, -100] {
        l32(&mut b, v);
    }
    // Text: "HI" at (100, 900), 100 logical units high.
    b.extend(b"(Color 0,0,0,255)");
    b.push(0x18);
    for v in [0, 100, -500, 400] {
        l32(&mut b, v);
    }
    b.push(2);
    for c in "HI".encode_utf16() {
        b.extend(c.to_le_bytes());
    }
    b.extend(b"(EndOfDWF)garbage after the trailer");
    b
}

#[test]
fn renders_primitives_where_the_mapping_puts_them() {
    let sc = scene(&fixture()).unwrap();
    assert_eq!((sc.width, sc.height), (792.0, 792.0));
    assert_eq!(sc.skipped, 2, "the unknown extended ASCII and binary opcodes");
    let r = sc.render(792).unwrap();
    assert_eq!((r.width, r.height), (792, 792));
    let at_px = |x, y| {
        let (cx, cy) = at(x, y);
        px(&r, cx, cy)
    };
    assert_eq!(at_px(250.0, 250.0), [255, 0, 0, 255], "filled square");
    assert_eq!(at_px(750.0, 100.0), [0, 0, 255, 255], "blue polyline");
    assert_eq!(at_px(600.0, 800.0), [0, 0, 255, 255], "ring");
    assert_eq!(at_px(500.0, 800.0), [255, 255, 255, 255], "ring centre is empty");
    assert_eq!(at_px(800.0, 700.0), [0, 128, 0, 255], "relative binary line after O");
    assert_eq!(at_px(500.0, 600.0), [255, 128, 0, 255], "indexed colour from the overridden map");
    assert_eq!(px(&r, 5, 5), [255, 255, 255, 255]);
    let dark = (28..96).flat_map(|y| (88..200).map(move |x| (x, y))).filter(|&(x, y)| px(&r, x, y)[0] < 100).count();
    assert!(dark > 30, "text drew {dark} dark pixels");
}

#[test]
fn reads_the_trailer_and_stops() {
    let mut b = header();
    b.extend(b"(Bounds 0,0 10,10)(Color 255,0,0,255) L 0,0 10,10 (EndOfDWF) L 0,10 10,0");
    let sc = scene(&b).unwrap();
    assert_eq!(sc.ops.len(), 1);
}

#[test]
fn default_colour_map_is_the_published_one() {
    let m = default_cmap();
    assert_eq!(m.len(), 256);
    assert_eq!(m.get(1), Some(&[255, 0, 0, 255]));
    assert_eq!(m.get(7), Some(&[0, 0, 0, 255]));
    assert_eq!(m.get(10), Some(&[255, 0, 0, 255]));
    assert_eq!(m.get(30), Some(&[255, 127, 0, 255]));
    assert_eq!(m.get(250), Some(&[84, 84, 84, 255]));
    assert_eq!(m.get(255), Some(&[255, 255, 255, 255]));
    // Every hue family's full-strength entry sits at 10 + 10 k.
    assert_eq!(m.get(50), Some(&[255, 255, 0, 255]));
}

#[test]
fn default_map_colours_index_through_c() {
    let mut b = header();
    b.extend(b"(Bounds 0,0 100,100) F C 1 P 3 0,0 100,0 0,100 f c");
    b.push(3);
    b.extend(b"C 5 L 0,0 100,100");
    let sc = scene(&b).unwrap();
    assert_eq!(sc.ops.len(), 2);
    assert!(matches!(sc.ops.first().and_then(|o| o.fill.as_ref()).map(|f| &f.0), Some(Brush::Solid([255, 0, 0, 255]))));
}

#[test]
fn scale_sets_the_paper() {
    let mut b = header();
    b.extend(b"(Bounds 0,0 100,50)(Scale 1 1 inch) L 0,0 100,50");
    let sc = scene(&b).unwrap();
    // 100 in = 7200 pt for the drawing, plus the 2 % margins on each side.
    assert!((sc.width - 7500.0).abs() < 1e-6, "{}", sc.width);
    assert!((sc.height - 3750.0).abs() < 1e-6, "{}", sc.height);
    let mut b = header();
    b.extend(b"(Bounds 0,0 100,50)(Scale 1 1 meter) L 0,0 100,50");
    let sc = scene(&b).unwrap();
    assert!((sc.width - 14400.0).abs() < 1e-6, "the paper is capped: {}", sc.width);
    let r = scene_with_size(&b, Some((300.0, 200.0))).unwrap();
    assert_eq!((r.width, r.height), (300.0, 200.0));
}

#[test]
fn empty_sheet_is_an_error() {
    let mut b = header();
    b.extend(b"(Bounds 0,0 10,10)");
    assert_eq!(scene(&b).err().as_deref(), Some("the sheet is empty"));
    assert!(scene(b"(DWF V06.00)").is_err());
    assert!(scene(b"nope").is_err());
}

#[test]
fn unknown_single_byte_opcode_stops_but_keeps_the_drawing() {
    let mut b = header();
    b.extend(b"(Bounds 0,0 10,10)(Color 255,0,0,255) L 0,0 10,10 ");
    b.push(0x01);
    b.extend(b"L 0,10 10,0");
    let sc = scene(&b).unwrap();
    assert_eq!(sc.ops.len(), 1);
    assert_eq!(sc.skipped, 1);
}

#[test]
fn clip_hides_what_is_outside() {
    let mut b = header();
    b.extend(b"(Bounds 0,0 1000,1000)(Clip 0,0 500,1000)(Color 255,0,0,255) F P 4 0,0 1000,0 1000,1000 0,1000");
    let r = scene(&b).unwrap().render(792).unwrap();
    let (lx, ly) = at(250.0, 500.0);
    let (rx, ry) = at(750.0, 500.0);
    assert_eq!(px(&r, lx, ly), [255, 0, 0, 255]);
    assert_eq!(px(&r, rx, ry), [255, 255, 255, 255]);
}

#[test]
fn invisible_geometry_is_not_drawn() {
    let mut b = header();
    b.extend(b"(Bounds 0,0 10,10)(Color 255,0,0,255) v L 0,0 10,10 V L 0,10 10,0");
    assert_eq!(scene(&b).unwrap().ops.len(), 1);
}

#[test]
fn arcs_wedges_ellipses_beziers_contours_triangles_markers() {
    let mut b = header();
    b.extend(b"(Bounds 0,0 1000,1000)(Color 255,0,0,255)");
    // Quarter arc (outline), filled wedge, tilted ellipse.
    b.extend(b"R 500,500,300 0,16384\n");
    b.extend(b"F R 500,500,100 0,16384 f\n");
    b.extend(b"E 500,500 300,100 0,0 8192\n");
    b.extend(b"(Bezier 1 0,0 0,500 500,500 500,0)\n");
    b.extend(b"(ContourSet 2 4 4 0,0 400,0 400,400 0,400 100,100 300,100 300,300 100,300)\n");
    // Binary: marker, polytriangle, 16-bit forms.
    b.extend(b"S 40 G 1 M 2 100,100 900,900\n");
    b.push(b'O');
    u32le(&mut b, 500);
    u32le(&mut b, 500);
    b.push(0x12);
    for v in [0i16, 0, 50] {
        b.extend(v.to_le_bytes());
    }
    b.extend(b"F");
    b.push(0x14);
    b.push(4);
    for v in [10i16, 10, 100, 0, -100, 100, 100, 0] {
        b.extend(v.to_le_bytes());
    }
    b.push(0x07);
    b.push(2);
    for (x, y, c) in [(0i16, 0i16, [255u8, 0, 0, 255]), (100, 0, [0, 255, 0, 255]), (-50, 100, [0, 0, 255, 255]), (100, 0, [255, 255, 255, 255])] {
        b.extend(x.to_le_bytes());
        b.extend(y.to_le_bytes());
        b.extend(c);
    }
    let sc = scene(&b).unwrap();
    assert_eq!(sc.skipped, 0);
    // arc, wedge, ellipse, bezier, contours, 2 markers... + circle + 2 + 2 triangles.
    assert!(sc.ops.len() >= 10, "{}", sc.ops.len());
    sc.render(512).unwrap();
}

#[test]
fn binary_colour_map_and_images() {
    let mut b = header();
    b.extend(b"(Bounds 0,0 1000,1000)");
    // Binary colour map of 2 entries, then a 2x2 RGB image from (100,100) to (500,500).
    let mut cm = vec![2u8];
    cm.extend([9, 9, 9, 255, 255, 0, 255, 255]);
    block(&mut b, 1, &cm);
    let mut img = Vec::new();
    img.extend(2u16.to_le_bytes());
    img.extend(2u16.to_le_bytes());
    for v in [100i32, 100, 400, 400, 7] {
        img.extend(v.to_le_bytes());
    }
    img.extend(12u32.to_le_bytes());
    img.extend([255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0]);
    block(&mut b, 6, &img);
    // A 1-bit image from cur (500,500): the colour map above is its own local map.
    let mut bit = Vec::new();
    bit.extend(8u16.to_le_bytes());
    bit.extend(1u16.to_le_bytes());
    for v in [100i32, 100, 100, 100, 8] {
        bit.extend(v.to_le_bytes());
    }
    bit.push(2);
    bit.extend([0, 0, 0, 255, 255, 255, 255, 255]);
    bit.extend(1u32.to_le_bytes());
    bit.push(0b1010_1010);
    block(&mut b, 2, &bit);
    // Group3X is not read.
    block(&mut b, 3, &[0; 8]);
    let sc = scene(&b).unwrap();
    assert_eq!(sc.skipped, 1);
    assert_eq!(sc.ops.len(), 2);
    let r = sc.render(792).unwrap();
    let (x, y) = at(200.0, 400.0);
    assert!(close(px(&r, x, y), [255, 0, 0, 255]), "top-left pixel {:?}", px(&r, x, y));
    let (x, y) = at(400.0, 200.0);
    assert!(close(px(&r, x, y), [255, 255, 0, 255]), "bottom-right pixel {:?}", px(&r, x, y));
}

#[test]
fn text_and_font_opcodes() {
    let mut b = header();
    b.extend(b"(Bounds 0,0 1000,1000)(Color 0,0,0,255)");
    b.push(0x06);
    b.push(5);
    b.extend(b"Arial");
    b.push(1);
    u32le(&mut b, 0);
    u32le(&mut b, 0);
    // Advanced text: width scale 1.5, oblique 10 degrees, rotated 90 degrees.
    b.push(b'x');
    b.extend(1536u16.to_le_bytes());
    b.extend(1024u16.to_le_bytes());
    l32(&mut b, 0);
    b.extend(1820u16.to_le_bytes());
    b.push(1);
    b.push(1);
    l32(&mut b, 16384);
    l32(&mut b, 100);
    l32(&mut b, 200);
    l32(&mut b, 200);
    for _ in 0..8 {
        l32(&mut b, 0);
    }
    b.push(3);
    for c in "abc".encode_utf16() {
        b.extend(c.to_le_bytes());
    }
    let sc = scene(&b).unwrap();
    assert!(!sc.ops.is_empty());
    assert_eq!(sc.skipped, 0);
}

#[test]
fn hostile_truncation_and_bit_flips_never_panic() {
    let f = fixture();
    for n in 0..f.len() {
        let _ = scene(f.get(..n).unwrap_or(&[]));
    }
    for i in 0..f.len() {
        for x in [0xFFu8, 0x55, 0xAA, 0x80, 0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x01] {
            let mut g = f.clone();
            if let Some(v) = g.get_mut(i) {
                *v ^= x;
            }
            let _ = scene(&g).and_then(|s| s.render(256));
        }
    }
}

#[test]
fn hostile_counts_and_sizes_stay_bounded() {
    let cases: Vec<Vec<u8>> = vec![
        // Extended counts with no data behind them.
        [&b"p"[..], &[0, 255, 255]].concat(),
        [&b"P 999999999999 1,1 2,2"[..]].concat(),
        [&b"P 2000000 1,1 2,2"[..]].concat(),
        [&b"{"[..], &[0xFF; 4], b"xx"].concat(),
        [&b"{"[..], &[0; 4]].concat(),
        [&b"{"[..], &[1, 0, 0, 0]].concat(),
        [&b"(Foo "[..], &[b'{', 0xFF, 0xFF, 0xFF, 0x7F]].concat(),
        [&b"(((((((((("[..]].concat(),
        [&b"(Foo 'unterminated"[..]].concat(),
        [&b"k"[..], &[0, 255, 255, 0, 255, 255]].concat(),
        [&b"(ContourSet 99999999 1 2 3)"[..]].concat(),
        [&b"(Gouraud 99999999 1,1)"[..]].concat(),
        [&b"x"[..], &[0xFF; 40]].concat(),
        [&b"g"[..], &[0, 255, 255]].concat(),
        [&b"(Bezier 999999999 0,0)"[..]].concat(),
        [&b"R 0,0,99999999999999999999 -9223372036854775807,9223372036854775807"[..]].concat(),
        [&b"E 0,0 1,1 -9223372036854775807,9223372036854775807 5"[..]].concat(),
    ];
    for c in cases {
        let mut b = header();
        b.extend(b"(Bounds 0,0 10,10)L 0,0 10,10 ");
        b.extend(c);
        let sc = scene(&b).unwrap();
        assert!(sc.ops.len() <= 10);
        sc.render(256).unwrap();
    }
    // 5 million tiny opcodes.
    let mut b = header();
    b.extend(b"(Bounds 0,0 10,10)(Color 255,0,0,255)");
    for _ in 0..2_500_000 {
        b.extend(b"Vv");
    }
    b.extend(b"V L 0,0 10,10");
    assert_eq!(scene(&b).unwrap().ops.len(), 1);
    // More lines than are kept.
    let mut b = header();
    b.extend(b"(Bounds 0,0 10,10)(Color 255,0,0,255)");
    for _ in 0..(MAX_PRIMS + 5000) {
        b.extend(b"L 0,0 1,1");
    }
    let sc = scene(&b).unwrap();
    assert_eq!(sc.ops.len(), MAX_PRIMS);
    assert!(sc.skipped >= 5000);
}
