//! Tests of the page interpreter on hand-built PDF files (no xref: the reader rebuilds it).

use super::*;

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    v.extend(data);
    v.extend(b"\nendstream");
    v
}

fn flate(dict: &str, data: &[u8]) -> Vec<u8> {
    stream(&format!("{dict} /Filter /FlateDecode"), &miniz_oxide::deflate::compress_to_vec_zlib(data, 6))
}

/// A 100 × 100 pt page with `content`, extra page entries and extra objects (numbered from 5).
fn pdf(page: &str, content: &str, extra: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    let mut out = b"%PDF-1.5\n".to_vec();
    let mut put = |n: u32, body: &[u8]| {
        out.extend(format!("{n} 0 obj\n").bytes());
        out.extend(body);
        out.extend(b"\nendobj\n");
    };
    put(1, b"<< /Type /Catalog /Pages 2 0 R >>");
    put(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    put(3, format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R {page} >>").as_bytes());
    put(4, &stream("", content.as_bytes()));
    for (n, body) in extra {
        put(n, &body);
    }
    out
}

/// The page drawn 256 px wide (2.56 px per point).
fn draw(bytes: &[u8]) -> Raster {
    render(bytes, 1, 256).unwrap()
}

/// The pixel at point (x, y) of the page (y up).
fn at(r: &Raster, x: f64, y: f64) -> [u8; 4] {
    let s = f64::from(r.width) / 100.0;
    let (px, py) = ((x * s) as usize, ((100.0 - y) * s) as usize);
    let i = (py * r.width as usize + px) * 4;
    [r.rgba[i], r.rgba[i + 1], r.rgba[i + 2], r.rgba[i + 3]]
}

fn near(p: [u8; 4], want: [u8; 3], tol: i32) -> bool {
    (0..3).all(|i| (i32::from(p[i]) - i32::from(want[i])).abs() <= tol)
}

/// The bounding box in points of every non-white pixel: `(x0, y0, x1, y1)`.
fn ink(r: &Raster) -> Option<(f64, f64, f64, f64)> {
    let s = f64::from(r.width) / 100.0;
    let mut bb: Option<(f64, f64, f64, f64)> = None;
    for y in 0..r.height as usize {
        for x in 0..r.width as usize {
            let i = (y * r.width as usize + x) * 4;
            if r.rgba[i] < 200 || r.rgba[i + 1] < 200 || r.rgba[i + 2] < 200 {
                let (px, py) = (x as f64 / s, 100.0 - (y as f64 + 1.0) / s);
                bb = Some(match bb {
                    None => (px, py, px + 1.0 / s, py + 1.0 / s),
                    Some((a, b, c, d)) => (a.min(px), b.min(py), c.max(px + 1.0 / s), d.max(py + 1.0 / s)),
                });
            }
        }
    }
    bb
}

/// A TrueType font of one glyph (gid 1, a 500 × 700 square at 1000 units per em) reached from
/// character 65 through a Macintosh cmap, with advance 600.
fn tiny_ttf() -> Vec<u8> {
    let be16 = |v: u16| v.to_be_bytes().to_vec();
    let be32 = |v: u32| v.to_be_bytes().to_vec();
    let mut head = vec![0u8; 54];
    head[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes());
    head[18..20].copy_from_slice(&1000u16.to_be_bytes());
    head[36..44].copy_from_slice(&[0, 0, 0, 0, 1, 0xF4, 2, 0xBC]);
    let mut hhea = vec![0u8; 36];
    hhea[34..36].copy_from_slice(&2u16.to_be_bytes());
    let mut maxp = vec![0u8; 32];
    maxp[..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    maxp[4..6].copy_from_slice(&2u16.to_be_bytes());
    let hmtx = [be16(600), be16(0), be16(600), be16(0)].concat();
    let mut glyph = [be16(1), be16(0), be16(0), be16(500), be16(700), be16(3), be16(0)].concat();
    glyph.extend([1, 1, 1, 1]);
    for dx in [0i16, 500, 0, -500] {
        glyph.extend(dx.to_be_bytes());
    }
    for dy in [0i16, 0, 700, 0] {
        glyph.extend(dy.to_be_bytes());
    }
    let glen = glyph.len();
    let loca = [be16(0), be16(0), be16((glen / 2) as u16)].concat();
    // cmap: one (1, 0) subtable, format 0.
    let mut sub = [be16(0), be16(262), be16(0)].concat();
    let mut map = [0u8; 256];
    map[65] = 1;
    sub.extend(map);
    let cmap = [be16(0), be16(1), be16(1), be16(0), be32(12), sub].concat();
    let tables: Vec<(&[u8; 4], Vec<u8>)> =
        vec![(b"cmap", cmap), (b"glyf", glyph), (b"head", head), (b"hhea", hhea), (b"hmtx", hmtx), (b"loca", loca), (b"maxp", maxp)];
    let mut out = [be32(0x0001_0000), be16(tables.len() as u16), be16(64), be16(2), be16(48)].concat();
    let mut off = 12 + 16 * tables.len();
    for (tag, d) in &tables {
        out.extend(*tag);
        out.extend(be32(0));
        out.extend(be32(off as u32));
        out.extend(be32(d.len() as u32));
        off += d.len().div_ceil(4) * 4;
    }
    for (_, d) in &tables {
        out.extend(d);
        out.resize(out.len().div_ceil(4) * 4, 0);
    }
    out
}

fn ttf_page(content: &str, font_extra: &str) -> Vec<u8> {
    pdf(
        "/Resources << /Font << /F1 5 0 R >> >>",
        content,
        vec![
            (5, format!("<< /Type /Font /Subtype /TrueType /BaseFont /AAAAAA+Tiny /FirstChar 65 /LastChar 65 /Widths [600] /Encoding /WinAnsiEncoding /FontDescriptor 6 0 R {font_extra} >>").into_bytes()),
            (6, b"<< /Type /FontDescriptor /FontName /AAAAAA+Tiny /Flags 32 /FontFile2 7 0 R >>".to_vec()),
            (7, flate("", &tiny_ttf())),
        ],
    )
}

#[test]
fn a_non_embedded_helvetica_draws_glyphs_where_the_text_matrix_puts_them() {
    let f = pdf("/Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >>", "BT /F1 40 Tf 10 30 Td (H) Tj ET", vec![]);
    let r = draw(&f);
    let (x0, y0, x1, y1) = ink(&r).expect("glyph ink");
    // Cap height ~0.72 em = 29 pt on the baseline y = 30; the letter starts near x = 10 + side bearing.
    assert!((9.0..18.0).contains(&x0) && x1 > 25.0 && x1 < 45.0, "x range {x0}..{x1}");
    assert!((28.0..33.0).contains(&y0) && y1 > 54.0 && y1 < 62.0, "y range {y0}..{y1}");
    // A second line moved with T* / TL lands lower by the leading.
    let f2 = pdf(
        "/Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >>",
        "BT /F1 20 Tf 22 TL 10 70 Td (H) Tj T* (H) Tj ET",
        vec![],
    );
    let (_, y0, _, y1) = ink(&draw(&f2)).unwrap();
    assert!(y0 < 50.0 && y1 > 80.0, "two lines span {y0}..{y1}");
}

#[test]
fn an_embedded_truetype_glyph_is_filled_at_its_place_with_the_declared_advance() {
    let r = draw(&ttf_page("BT /F1 50 Tf 10 10 Td (AA) Tj ET", ""));
    // 500 × 700 units at 50 pt: 25 × 35 pt squares, advance 600 units = 30 pt.
    assert!(near(at(&r, 20.0, 25.0), [0, 0, 0], 8), "first square");
    assert!(near(at(&r, 50.0, 25.0), [0, 0, 0], 8), "second square at x = 40..65");
    assert!(near(at(&r, 37.5, 25.0), [255, 255, 255], 8), "gap between them");
    assert!(near(at(&r, 20.0, 50.0), [255, 255, 255], 8), "above the glyph");
    let (x0, y0, x1, y1) = ink(&r).unwrap();
    assert!((x0 - 10.0).abs() < 0.8 && (x1 - 65.0).abs() < 0.8 && (y0 - 10.0).abs() < 0.8 && (y1 - 45.0).abs() < 0.8, "{x0} {y0} {x1} {y1}");
    // Character spacing, word spacing and horizontal scaling move the second glyph.
    let r = draw(&ttf_page("BT /F1 50 Tf 10 10 Td 10 Tc 50 Tz (AA) Tj ET", ""));
    // Advance = (30 + 10) * 0.5 = 20 pt: second square at 30..42.5 (squares are 12.5 wide).
    assert!(near(at(&r, 36.0, 25.0), [0, 0, 0], 8), "second square after Tc / Tz");
    // Render mode 3 draws nothing; mode 1 strokes only the outline.
    assert!(ink(&draw(&ttf_page("BT /F1 50 Tf 10 10 Td 3 Tr (A) Tj ET", ""))).is_none());
    let r = draw(&ttf_page("BT /F1 50 Tf 10 10 Td 1 Tr 1 w (A) Tj ET", ""));
    assert!(near(at(&r, 22.0, 27.0), [255, 255, 255], 8), "the inside stays open");
    assert!(near(at(&r, 10.0, 27.0), [0, 0, 0], 60), "the left edge is stroked");
    // TJ adjustments: -1000 shifts the next glyph right by one font size.
    let r = draw(&ttf_page("BT /F1 20 Tf 10 10 Td [(A) -1000 (A)] TJ ET", ""));
    assert!(near(at(&r, 12.0, 15.0), [0, 0, 0], 8) && near(at(&r, 46.0, 15.0), [0, 0, 0], 8), "A, then A 32 pt on");
}

#[test]
fn a_type0_font_with_identity_h_maps_codes_to_glyphs() {
    let f = pdf(
        "/Resources << /Font << /F1 5 0 R >> >>",
        "BT /F1 50 Tf 10 10 Td <0001> Tj ET",
        vec![
            (5, b"<< /Type /Font /Subtype /Type0 /BaseFont /Tiny /Encoding /Identity-H /DescendantFonts [8 0 R] >>".to_vec()),
            (
                8,
                b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Tiny /CIDToGIDMap /Identity /DW 600 /FontDescriptor 6 0 R /W [1 [600]] >>"
                    .to_vec(),
            ),
            (6, b"<< /Type /FontDescriptor /FontName /Tiny /Flags 4 /FontFile2 7 0 R >>".to_vec()),
            (7, flate("", &tiny_ttf())),
        ],
    );
    let r = draw(&f);
    assert!(near(at(&r, 20.0, 25.0), [0, 0, 0], 8));
    assert!(near(at(&r, 40.0, 25.0), [255, 255, 255], 8));
}

#[test]
fn a_type1_font_file_is_decoded_and_drawn() {
    let f = pdf(
        "/Resources << /Font << /F1 5 0 R >> >>",
        "BT /F1 100 Tf 10 10 Td (A) Tj ET",
        vec![
            (5, b"<< /Type /Font /Subtype /Type1 /BaseFont /Test /FontDescriptor 6 0 R /FirstChar 65 /LastChar 65 /Widths [600] >>".to_vec()),
            (6, b"<< /Type /FontDescriptor /FontName /Test /Flags 4 /FontFile 7 0 R >>".to_vec()),
            (7, flate("", &type1::tests::raw_font(false, false))),
        ],
    );
    // The test font's A is a 100-unit square at (50, 0), FontMatrix 0.002: 20 pt at 100 pt size.
    let (x0, y0, x1, y1) = ink(&draw(&f)).unwrap();
    assert!((x0 - 20.0).abs() < 1.0 && (x1 - 40.0).abs() < 1.0 && (y0 - 10.0).abs() < 1.0 && (y1 - 30.0).abs() < 1.0, "{x0} {y0} {x1} {y1}");
}

#[test]
fn a_type3_font_runs_its_glyph_procedures() {
    let f = pdf(
        "/Resources << /Font << /F1 5 0 R >> >>",
        "0 0 1 rg BT /F1 40 Tf 10 10 Td (AA) Tj ET",
        vec![
            (5, b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 750 750] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /sq 6 0 R >> /Encoding << /Type /Encoding /Differences [65 /sq] >> /FirstChar 65 /LastChar 65 /Widths [1000] >>".to_vec()),
            (6, stream("", b"1000 0 0 0 750 750 d1 0 0 750 750 re f")),
        ],
    );
    let r = draw(&f);
    assert!(near(at(&r, 20.0, 25.0), [0, 0, 255], 8), "first 30 pt square, in the text colour");
    assert!(near(at(&r, 60.0, 25.0), [0, 0, 255], 8), "second one 40 pt on");
    assert!(near(at(&r, 42.0, 25.0), [255, 255, 255], 8));
}

fn image_page(content: &str, objs: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    pdf("/Resources << /XObject << /Im1 5 0 R >> >>", content, objs)
}

#[test]
fn an_rgb_flate_image_draws_with_its_colours() {
    // 2 × 2: red green / blue yellow (row 0 on top).
    let px = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
    let f = image_page(
        "q 50 0 0 50 25 25 cm /Im1 Do Q",
        vec![(5, flate("/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8", &px))],
    );
    let r = draw(&f);
    assert!(near(at(&r, 37.5, 62.5), [255, 0, 0], 6), "top left");
    assert!(near(at(&r, 62.5, 62.5), [0, 255, 0], 6), "top right");
    assert!(near(at(&r, 37.5, 37.5), [0, 0, 255], 6), "bottom left");
    assert!(near(at(&r, 62.5, 37.5), [255, 255, 0], 6), "bottom right");
    assert!(near(at(&r, 10.0, 10.0), [255, 255, 255], 2), "paper around it");
}

#[test]
fn a_dct_image_is_decoded_as_jpeg() {
    let mut jpg = Vec::new();
    let img = ::image::RgbImage::from_pixel(16, 16, ::image::Rgb([220, 20, 30]));
    ::image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 95).encode_image(&img).unwrap();
    let f = image_page(
        "q 60 0 0 60 20 20 cm /Im1 Do Q",
        vec![(5, stream("/Type /XObject /Subtype /Image /Width 16 /Height 16 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode", &jpg))],
    );
    let r = draw(&f);
    assert!(near(at(&r, 50.0, 50.0), [220, 20, 30], 14), "{:?}", at(&r, 50.0, 50.0));
    // A broken JPEG draws nothing and reports it.
    let f = image_page(
        "q 60 0 0 60 20 20 cm /Im1 Do Q",
        vec![(
            5,
            stream("/Type /XObject /Subtype /Image /Width 16 /Height 16 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode", &jpg[..40]),
        )],
    );
    assert!(ink(&draw(&f)).is_none());
}

#[test]
fn an_image_mask_takes_the_fill_colour_and_inline_images_work() {
    // 8 × 1 stencil 00001111: the left half (sample 0) is painted.
    let f = pdf("", "0 0 1 rg q 80 0 0 40 10 10 cm BI /W 8 /H 1 /IM true /BPC 1 ID \x0F\nEI Q", vec![]);
    let r = draw(&f);
    assert!(near(at(&r, 30.0, 30.0), [0, 0, 255], 6), "painted half");
    assert!(near(at(&r, 70.0, 30.0), [255, 255, 255], 6), "unpainted half");
    // /Decode [1 0] flips it.
    let f = pdf("", "0 0 1 rg q 80 0 0 40 10 10 cm BI /W 8 /H 1 /IM true /BPC 1 /D [1 0] ID \x0F\nEI Q", vec![]);
    assert!(near(at(&draw(&f), 70.0, 30.0), [0, 0, 255], 6));
    // An inline RGB image with abbreviations, ASCIIHex-filtered.
    let f = pdf("", "q 50 0 0 50 25 25 cm BI /W 1 /H 1 /CS /RGB /BPC 8 /F /AHx ID 00FF00> EI Q", vec![]);
    assert!(near(at(&draw(&f), 50.0, 50.0), [0, 255, 0], 4));
}

#[test]
fn soft_masks_colour_keys_indexed_and_decode_arrays() {
    // A red pixel through a 50 % gray soft mask over the white paper.
    let f = image_page(
        "q 50 0 0 50 25 25 cm /Im1 Do Q",
        vec![
            (5, flate("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /SMask 6 0 R", &[255, 0, 0])),
            (6, flate("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", &[128])),
        ],
    );
    let p = at(&draw(&f), 50.0, 50.0);
    assert!(p[0] > 250 && (120..=135).contains(&p[1]) && (120..=135).contains(&p[2]), "{p:?}");
    // Colour key: the red pixel of [red, green] is masked out.
    let f = image_page(
        "q 80 0 0 40 10 10 cm /Im1 Do Q",
        vec![(
            5,
            flate(
                "/Type /XObject /Subtype /Image /Width 2 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Mask [255 255 0 0 0 0]",
                &[255, 0, 0, 0, 255, 0],
            ),
        )],
    );
    let r = draw(&f);
    assert!(near(at(&r, 25.0, 30.0), [255, 255, 255], 6), "keyed out");
    assert!(near(at(&r, 75.0, 30.0), [0, 255, 0], 6));
    // Indexed with a 2-bit palette and a /Decode-d gray image.
    let f = image_page(
        "q 80 0 0 40 10 10 cm /Im1 Do Q",
        vec![(
            5,
            flate(
                "/Type /XObject /Subtype /Image /Width 2 /Height 1 /ColorSpace [/Indexed /DeviceRGB 3 <FF0000 00FF00 0000FF 000000>] /BitsPerComponent 2",
                &[0b0110_0000],
            ),
        )],
    );
    let r = draw(&f);
    assert!(near(at(&r, 25.0, 30.0), [0, 255, 0], 6) && near(at(&r, 75.0, 30.0), [0, 0, 255], 6));
    let f = image_page(
        "q 80 0 0 40 10 10 cm /Im1 Do Q",
        vec![(5, flate("/Type /XObject /Subtype /Image /Width 2 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /Decode [1 0]", &[0, 255]))],
    );
    let r = draw(&f);
    assert!(near(at(&r, 25.0, 30.0), [255, 255, 255], 6) && near(at(&r, 75.0, 30.0), [0, 0, 0], 6));
}

fn shading_page(content: &str, shading: &str, extra: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    let mut objs = vec![(5, shading.as_bytes().to_vec())];
    objs.extend(extra);
    pdf("/Resources << /Shading << /Sh1 5 0 R >> /Pattern << /P1 << /PatternType 2 /Shading 5 0 R >> >> >>", content, objs)
}

#[test]
fn an_axial_shading_runs_from_one_end_colour_to_the_other() {
    let sh = "<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 100 0] /Extend [true true] /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >>";
    let r = draw(&shading_page("/Sh1 sh", sh, vec![]));
    assert!(near(at(&r, 2.0, 50.0), [250, 0, 5], 8), "{:?}", at(&r, 2.0, 50.0));
    assert!(near(at(&r, 98.0, 50.0), [5, 0, 250], 8));
    assert!(near(at(&r, 50.0, 50.0), [128, 0, 128], 8));
    // As a shading pattern filling a rectangle only.
    let r = draw(&shading_page("/Pattern cs /P1 scn 0 0 50 100 re f", sh, vec![]));
    assert!(near(at(&r, 2.0, 50.0), [250, 0, 5], 8));
    assert!(near(at(&r, 75.0, 50.0), [255, 255, 255], 2), "outside the filled rectangle");
    // Without /Extend the area beyond the axis stays unpainted.
    let sh =
        "<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [20 0 80 0] /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >>";
    let r = draw(&shading_page("/Sh1 sh", sh, vec![]));
    assert!(near(at(&r, 10.0, 50.0), [255, 255, 255], 2) && near(at(&r, 90.0, 50.0), [255, 255, 255], 2));
}

#[test]
fn a_postscript_function_shading_and_a_radial_one() {
    // t → (1 − t, t, 0): red to green through the calculator.
    let sh = "<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 100 0] /Extend [true true] /Function 6 0 R >>";
    let f = shading_page("/Sh1 sh", sh, vec![(6, stream("/FunctionType 4 /Domain [0 1] /Range [0 1 0 1 0 1]", b"{ dup 1 exch sub exch 0 }"))]);
    let r = draw(&f);
    assert!(near(at(&r, 1.0, 50.0), [252, 3, 0], 8), "{:?}", at(&r, 1.0, 50.0));
    assert!(near(at(&r, 99.0, 50.0), [3, 252, 0], 8));
    // Sampled (type 0) and stitching (type 3) functions.
    let stitch = "<< /FunctionType 3 /Domain [0 1] /Bounds [0.5] /Encode [0 1 0 1] /Functions [<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 1 0] /N 1 >> << /FunctionType 2 /Domain [0 1] /C0 [0 1 0] /C1 [0 0 1] /N 1 >>] >>";
    let sh = format!("<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 100 0] /Extend [true true] /Function {stitch} >>");
    let r = draw(&shading_page("/Sh1 sh", &sh, vec![]));
    assert!(
        near(at(&r, 1.0, 50.0), [252, 3, 0], 8) && near(at(&r, 50.0, 50.0), [0, 255, 0], 10) && near(at(&r, 99.0, 50.0), [0, 3, 252], 8),
        "{:?}",
        at(&r, 50.0, 50.0)
    );
    let samples = [255u8, 0, 0, 0, 0, 255];
    let sh = "<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 100 0] /Extend [true true] /Function 6 0 R >>";
    let f = shading_page("/Sh1 sh", sh, vec![(6, stream("/FunctionType 0 /Domain [0 1] /Range [0 1 0 1 0 1] /Size [2] /BitsPerSample 8", &samples))]);
    let r = draw(&f);
    assert!(near(at(&r, 1.0, 50.0), [252, 0, 3], 8) && near(at(&r, 99.0, 50.0), [3, 0, 252], 8));
    // Radial: red centre, blue rim of a circle of radius 40.
    let sh = "<< /ShadingType 3 /ColorSpace /DeviceRGB /Coords [50 50 0 50 50 40] /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >>";
    let r = draw(&shading_page("/Sh1 sh", sh, vec![]));
    assert!(
        near(at(&r, 50.0, 50.0), [250, 0, 5], 12) && near(at(&r, 88.0, 50.0), [12, 0, 243], 14),
        "{:?} {:?}",
        at(&r, 50.0, 50.0),
        at(&r, 88.0, 50.0)
    );
    assert!(near(at(&r, 95.0, 50.0), [255, 255, 255], 2), "beyond the rim without /Extend");
    // A mesh shading is skipped with a note.
    let f = shading_page("/Sh1 sh", "<< /ShadingType 4 /ColorSpace /DeviceRGB >>", vec![]);
    let (_, notes) = render_notes(&f, 1, 256).unwrap();
    assert!(notes.iter().any(|n| n.contains("mesh")), "{notes:?}");
}

#[test]
fn a_form_xobject_is_transformed_and_clipped_to_its_box() {
    let f = pdf(
        "/Resources << /XObject << /Fm1 5 0 R >> >>",
        "/Fm1 Do",
        vec![(5, stream("/Type /XObject /Subtype /Form /BBox [0 0 20 20] /Matrix [2 0 0 2 10 10]", b"1 0 0 rg 0 0 100 100 re f"))],
    );
    let r = draw(&f);
    assert!(near(at(&r, 30.0, 30.0), [255, 0, 0], 4));
    assert!(near(at(&r, 48.0, 48.0), [255, 0, 0], 4), "BBox 20 × 2 = 40 pt reaches x = 50");
    assert!(near(at(&r, 55.0, 30.0), [255, 255, 255], 2), "clipped to the box");
    assert!(near(at(&r, 5.0, 30.0), [255, 255, 255], 2));
    // The form's own resources are used, and a form that shows itself ends.
    let f = pdf(
        "/Resources << /XObject << /Fm1 5 0 R >> >>",
        "/Fm1 Do",
        vec![
            (
                5,
                stream(
                    "/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << /XObject << /Fm1 5 0 R /Fm2 6 0 R >> >>",
                    b"/Fm2 Do /Fm1 Do 1 1 98 98 re 1 w S",
                ),
            ),
            (6, stream("/Type /XObject /Subtype /Form /BBox [0 0 100 100]", b"0 1 0 rg 40 40 20 20 re f")),
        ],
    );
    let r = draw(&f);
    assert!(near(at(&r, 50.0, 50.0), [0, 255, 0], 4));
}

#[test]
fn a_form_used_many_times_is_decoded_once() {
    // Each decode counts against the inflate budget; the last use (green over black) must still draw.
    let mut form = vec![b' '; 1 << 20];
    form.extend(b"40 40 20 20 re f");
    let f = pdf(
        "/Resources << /XObject << /Fm1 5 0 R >> >>",
        &format!("{}0 1 0 rg /Fm1 Do", "/Fm1 Do ".repeat(4999)),
        vec![(5, flate("/Type /XObject /Subtype /Form /BBox [0 0 100 100]", &form))],
    );
    let r = draw(&f);
    assert!(near(at(&r, 50.0, 50.0), [0, 255, 0], 4));
}

#[test]
fn huge_font_numbers_do_not_overflow() {
    let f = pdf(
        "/Resources << /Font << /F1 5 0 R /F2 6 0 R >> >>",
        "BT /F1 20 Tf (AB) Tj /F2 20 Tf <0001> Tj ET",
        vec![
            (5, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /FirstChar 1e30 /Widths [500 500] >>".to_vec()),
            (6, b"<< /Type /Font /Subtype /Type0 /BaseFont /Tiny /Encoding /Identity-H /DescendantFonts [7 0 R] >>".to_vec()),
            (7, b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Tiny /W [1e30 [1 2 3] 1e30 1e30 5] >>".to_vec()),
        ],
    );
    let _ = draw(&f);
}

#[test]
fn graphics_state_alpha_and_line_parameters() {
    let f = pdf("/Resources << /ExtGState << /G1 << /ca 0.5 /CA 0.5 /LW 10 /D [[4 4] 0] >> >> >>", "/G1 gs 0 g 10 10 80 80 re f", vec![]);
    let p = at(&draw(&f), 50.0, 50.0);
    assert!(near(p, [128, 128, 128], 4), "{p:?}");
    // The stroked line takes /LW 10 from the state and is dashed.
    let f = pdf("/Resources << /ExtGState << /G1 << /LW 10 /D [[10 10] 0] >> >> >>", "/G1 gs 0 50 m 100 50 l S", vec![]);
    let r = draw(&f);
    assert!(near(at(&r, 5.0, 50.0), [0, 0, 0], 8) && near(at(&r, 15.0, 50.0), [255, 255, 255], 8) && near(at(&r, 25.0, 52.0), [0, 0, 0], 8));
}

#[test]
fn a_tiling_pattern_repeats_its_cell() {
    let f = pdf(
        "/Resources << /Pattern << /P1 5 0 R >> >>",
        "/Pattern cs /P1 scn 0 0 100 100 re f",
        vec![(
            5,
            stream(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>",
                b"1 0 0 rg 0 0 5 5 re f",
            ),
        )],
    );
    let r = draw(&f);
    for (x, y) in [(2.5, 2.5), (12.5, 2.5), (52.5, 72.5)] {
        assert!(near(at(&r, x, y), [255, 0, 0], 12), "({x}, {y}): {:?}", at(&r, x, y));
    }
    for (x, y) in [(7.5, 7.5), (17.5, 2.5), (57.5, 77.5)] {
        assert!(near(at(&r, x, y), [255, 255, 255], 8), "({x}, {y}): {:?}", at(&r, x, y));
    }
    // An uncoloured pattern takes the colour given with scn.
    let f = pdf(
        "/Resources << /ColorSpace << /Cs1 [/Pattern /DeviceRGB] >> /Pattern << /P1 5 0 R >> >>",
        "/Cs1 cs 0 0 1 /P1 scn 0 0 100 100 re f",
        vec![(
            5,
            stream(
                "/Type /Pattern /PatternType 1 /PaintType 2 /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>",
                b"1 0 0 rg 0 0 5 5 re f",
            ),
        )],
    );
    assert!(near(at(&draw(&f), 2.5, 2.5), [0, 0, 255], 12));
}

#[test]
fn colour_spaces_and_clipping() {
    let f = pdf(
        "/Resources << /ColorSpace << /S1 [/Separation /Spot /DeviceRGB << /FunctionType 2 /Domain [0 1] /C0 [1 1 1] /C1 [0 0 1] /N 1 >>] >> >>",
        "/S1 cs 1 sc 0 0 50 100 re f 1 1 0 0 k 50 0 50 100 re f",
        vec![],
    );
    let r = draw(&f);
    assert!(near(at(&r, 25.0, 50.0), [0, 0, 255], 4) && near(at(&r, 75.0, 50.0), [0, 0, 255], 4));
    // W n clips later painting to the rectangle.
    let f = pdf("", "20 20 30 30 re W n 1 0 0 rg 0 0 100 100 re f", vec![]);
    let r = draw(&f);
    assert!(near(at(&r, 30.0, 30.0), [255, 0, 0], 4) && near(at(&r, 60.0, 30.0), [255, 255, 255], 2));
}

#[test]
fn hostile_content_resources_and_fonts_never_panic() {
    // A file that uses everything, damaged every which way.
    let mut ttf = tiny_ttf();
    let good = {
        let mut objs = vec![
            (5, b"<< /Type /Font /Subtype /TrueType /BaseFont /Tiny /FirstChar 65 /LastChar 65 /Widths [600] /FontDescriptor 6 0 R >>".to_vec()),
            (6, b"<< /Type /FontDescriptor /Flags 32 /FontFile2 7 0 R >>".to_vec()),
            (7, flate("", &ttf)),
            (8, flate("/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /SMask 9 0 R", &[1; 12])),
            (9, flate("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", &[9])),
            (10, b"<< /ShadingType 3 /ColorSpace /DeviceRGB /Coords [50 50 0 50 50 40] /Extend [true true] /Function 11 0 R >>".to_vec()),
            (11, stream("/FunctionType 4 /Domain [0 1] /Range [0 1 0 1 0 1]", b"{ dup dup 1 exch sub }")),
            (12, stream("/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << /XObject << /F 12 0 R >> >>", b"/F Do 0 0 5 5 re f")),
            (
                13,
                stream(
                    "/Type /Pattern /PatternType 1 /PaintType 1 /BBox [0 0 4 4] /XStep 4 /YStep 4 /Resources << /Pattern << /P 13 0 R >> >>",
                    b"/Pattern cs /P scn 0 0 2 2 re f",
                ),
            ),
        ];
        objs.sort_by_key(|o| o.0);
        pdf(
            "/Resources << /Font << /F1 5 0 R /F2 << /Type /Font /Subtype /Type0 /Encoding /Identity-H /DescendantFonts [<< /Subtype /CIDFontType2 /W [1 [5 5 5] 5 9 100] >>] >> >> /XObject << /Im 8 0 R /Fm 12 0 R >> /Shading << /S 10 0 R >> /Pattern << /P 13 0 R >> >>",
            "q BT /F1 30 Tf 10 10 Td (AAA) Tj /F2 12 Tf <00010002> Tj ET /Im Do /S sh /Fm Do /Pattern cs /P scn 0 0 90 90 re f BI /W 2 /H 2 /BPC 8 /CS /G ID abcd EI 0 0 m 5 5 l S Q",
            objs,
        )
    };
    let _ = render_notes(&good, 1, 256).unwrap();
    for n in (0..good.len()).step_by(7) {
        let _ = render_notes(&good[..n], 1, 256);
    }
    for i in (0..good.len()).step_by(5) {
        let mut f = good.clone();
        f[i] ^= 0xFF;
        let _ = render_notes(&f, 1, 256);
        f[i] = f[i].wrapping_add(31);
        let _ = render_notes(&f, 1, 256);
    }
    // Damaged font programs: truncated and flipped inside the (uncompressed) TTF.
    for n in (0..ttf.len()).step_by(11) {
        let f = pdf(
            "/Resources << /Font << /F1 5 0 R >> >>",
            "BT /F1 30 Tf 10 10 Td (A) Tj ET",
            vec![
                (5, b"<< /Type /Font /Subtype /TrueType /FirstChar 65 /Widths [600] /FontDescriptor 6 0 R >>".to_vec()),
                (6, b"<< /FontFile2 7 0 R >>".to_vec()),
                (7, stream("", &ttf[..n])),
            ],
        );
        let _ = render_notes(&f, 1, 256);
    }
    for i in (0..ttf.len()).step_by(3) {
        ttf[i] ^= 0xA5;
        let f = pdf(
            "/Resources << /Font << /F1 5 0 R >> >>",
            "BT /F1 30 Tf 10 10 Td (A) Tj ET",
            vec![
                (5, b"<< /Type /Font /Subtype /TrueType /FirstChar 65 /Widths [600] /FontDescriptor 6 0 R >>".to_vec()),
                (6, b"<< /FontFile2 7 0 R >>".to_vec()),
                (7, stream("", &ttf)),
            ],
        );
        let _ = render_notes(&f, 1, 256);
        ttf[i] ^= 0xA5;
    }
    // Hostile content: absurd operands, unbalanced q, endless nesting of arrays, a huge image,
    // huge dashes and sizes.
    let nasty = [
        "1e30 0 0 1e30 0 0 cm 0 0 1 1 re f",
        "0 0 m 1e38 1e38 l 1e-38 1e-38 l S",
        &"q ".repeat(100_000),
        &"[".repeat(100_000),
        &"BT ".repeat(50_000),
        "BT /F1 1e30 Tf (x) Tj 1e30 Tz 1e30 Tc (x) Tj ET",
        "BI /W 100000 /H 100000 /BPC 8 /CS /RGB ID x EI",
        "BI /W 4 /H 4 /BPC 16 /CS /CMYK /F /Fl ID xx EI",
        "[1e-30 1e-30] 0 d 0 0 m 100 100 l S",
        "[0 0] 0 d 0 0 m 100 100 l S",
        "/Pattern cs /Nope scn 0 0 10 10 re f /Nope sh /Nope Do",
        "1 0 0 1 NaN NaN cm 0 0 10 10 re f",
        "(unterminated",
        "<< /a (b",
        "BI /W 1 /H 1 ID",
    ];
    for c in nasty {
        let f = pdf("/Resources << /Font << /F1 5 0 R >> >>", c, vec![(5, ttf_font_obj())]);
        let _ = render_notes(&f, 1, 256);
    }
}

fn ttf_font_obj() -> Vec<u8> {
    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec()
}

/// A bare CFF font: glyph 1 is named "A" (SID 34) and is a 500 × 700 square.
fn tiny_cff() -> Vec<u8> {
    let index = |items: &[&[u8]]| -> Vec<u8> {
        let mut out = vec![0, items.len() as u8, 1, 1];
        let mut end = 1u8;
        for it in items {
            end += it.len() as u8;
            out.push(end);
        }
        for it in items {
            out.extend(*it);
        }
        out
    };
    let glyph: Vec<u8> = vec![139, 139, 21, 248, 136, 139, 5, 139, 249, 80, 5, 252, 136, 139, 5, 14];
    let charstrings = index(&[&[14], &glyph]);
    let charset = vec![0u8, 0, 34];
    let header = [1u8, 0, 4, 1];
    let names = index(&[b"Test"]);
    let strings = vec![0u8, 0];
    let gsubrs = vec![0u8, 0];
    // The Top DICT has a fixed size: two 4-byte entries and the Private entry (size, offset: 7).
    let top_len = 4 + 4 + 7;
    let top_index_len = 4 + 1 + top_len;
    let base = header.len() + names.len() + top_index_len + strings.len() + gsubrs.len();
    let (charset_at, cs_at) = (base, base + charset.len());
    let private_at = cs_at + charstrings.len();
    let int = |v: usize| [28, (v >> 8) as u8, v as u8];
    let mut top = Vec::new();
    top.extend(int(charset_at));
    top.push(15);
    top.extend(int(cs_at));
    top.push(17);
    top.extend(int(0));
    top.extend(int(private_at));
    top.push(18);
    let mut out = Vec::new();
    out.extend(header);
    out.extend(names);
    out.extend(index(&[&top]));
    out.extend(strings);
    out.extend(gsubrs);
    out.extend(charset);
    out.extend(charstrings);
    out
}

#[test]
fn bare_cff_fonts_draw_simple_and_cid_keyed_text() {
    let cff = tiny_cff();
    let info = cff::parse(&cff).expect("the test CFF parses");
    assert_eq!(info.n_glyphs, 2);
    let f = pdf(
        "/Resources << /Font << /F1 5 0 R >> >>",
        "BT /F1 50 Tf 10 10 Td (A) Tj ET",
        vec![
            (5, b"<< /Type /Font /Subtype /Type1 /BaseFont /Test /FirstChar 65 /LastChar 65 /Widths [600] /Encoding /WinAnsiEncoding /FontDescriptor 6 0 R >>".to_vec()),
            (6, b"<< /Type /FontDescriptor /Flags 32 /FontFile3 7 0 R >>".to_vec()),
            (7, stream("/Subtype /Type1C", &cff)),
        ],
    );
    let (x0, y0, x1, y1) = ink(&draw(&f)).expect("the CFF glyph is drawn");
    assert!((x0 - 10.0).abs() < 0.8 && (x1 - 35.0).abs() < 0.8 && (y0 - 10.0).abs() < 0.8 && (y1 - 45.0).abs() < 0.8, "{x0} {y0} {x1} {y1}");
    let f = pdf(
        "/Resources << /Font << /F1 5 0 R >> >>",
        "BT /F1 50 Tf 10 10 Td <0001> Tj ET",
        vec![
            (5, b"<< /Type /Font /Subtype /Type0 /BaseFont /Test /Encoding /Identity-H /DescendantFonts [8 0 R] >>".to_vec()),
            (8, b"<< /Type /Font /Subtype /CIDFontType0 /BaseFont /Test /DW 600 /FontDescriptor 6 0 R >>".to_vec()),
            (6, b"<< /Type /FontDescriptor /Flags 4 /FontFile3 7 0 R >>".to_vec()),
            (7, stream("/Subtype /CIDFontType0C", &cff)),
        ],
    );
    assert!(near(at(&draw(&f), 20.0, 25.0), [0, 0, 0], 8));
}
