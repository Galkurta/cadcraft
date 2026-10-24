use super::*;
use crate::underlay;
use cadcraft_render::images::Raster;

// ---- a tiny ISFF writer ----------------------------------------------------------------

fn p32(v: i32) -> [u8; 4] {
    let u = v as u32;
    let (hi, lo) = ((u >> 16) as u16, u as u16);
    let (h, l) = (hi.to_le_bytes(), lo.to_le_bytes());
    [h[0], h[1], l[0], l[1]]
}

fn vaxb(v: f64) -> [u8; 8] {
    if v == 0.0 {
        return [0; 8];
    }
    let a = v.abs();
    let mut e = a.log2().floor() as i32 + 1;
    let mut m = a / 2f64.powi(e);
    while m >= 1.0 {
        m /= 2.0;
        e += 1;
    }
    while m < 0.5 {
        m *= 2.0;
        e -= 1;
    }
    let frac = ((m - 0.5) * 2f64.powi(56)) as u64;
    let bits = (u64::from(v < 0.0) << 63) | (((e + 128) as u64) << 55) | frac;
    let b = bits.to_le_bytes();
    [b[6], b[7], b[4], b[5], b[2], b[3], b[0], b[1]]
}

fn el(ty: u8, level: u8, complex: bool, rest: &[u8]) -> Vec<u8> {
    let mut rest = rest.to_vec();
    if rest.len() % 2 == 1 {
        rest.push(0);
    }
    let w0 = u16::from(level) | (u16::from(complex) << 7) | (u16::from(ty) << 8);
    let mut v = w0.to_le_bytes().to_vec();
    v.extend(((rest.len() / 2) as u16).to_le_bytes());
    v.extend(rest);
    v
}

fn sym(colour: u16, weight: u16, style: u16) -> u16 {
    (colour << 8) | (weight << 3) | style
}

/// A displayable element: range block, display header, then `data` (from byte 36).
fn std(ty: u8, level: u8, complex: bool, attindx: u16, props: u16, symb: u16, data: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; 24];
    r.extend([0, 0]);
    r.extend(attindx.to_le_bytes());
    r.extend(props.to_le_bytes());
    r.extend(symb.to_le_bytes());
    r.extend(data);
    el(ty, level, complex, &r)
}

fn ints(v: &[i32]) -> Vec<u8> {
    v.iter().flat_map(|x| p32(*x)).collect()
}

/// A TCB 1500 bytes long: 10 UORs per subunit, 10 subunits per master unit (1 master = 100 raw).
fn tcb(three_d: bool, mask: Option<[u8; 8]>) -> Vec<u8> {
    let mut r = vec![0u8; 1496];
    let mut put = |at: usize, b: &[u8]| r[at - 4..at - 4 + b.len()].copy_from_slice(b);
    put(1112, &p32(10));
    put(1116, &p32(10));
    if three_d {
        put(1214, &[0x40]);
    }
    if let Some(m) = mask {
        put(48, &m);
    }
    el(9, 8, false, &r)
}

fn colour_table(entries: &[(usize, [u8; 3])]) -> Vec<u8> {
    let mut r = vec![0u8; 24 + 8 + 2 + 768];
    for (i, c) in entries {
        r[34 + 3 * i..34 + 3 * i + 3].copy_from_slice(c);
    }
    el(5, 1, false, &r)
}

fn line(level: u8, complex: bool, colour: u16, a: (i32, i32), b: (i32, i32)) -> Vec<u8> {
    std(3, level, complex, 0, 0, sym(colour, 0, 0), &ints(&[a.0, a.1, b.0, b.1]))
}

fn end(mut v: Vec<u8>) -> Vec<u8> {
    v.extend([0xFF, 0xFF]);
    v
}

const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];
const GREEN: [u8; 3] = [0, 200, 0];
const MAGENTA: [u8; 3] = [200, 0, 200];
const ORANGE: [u8; 3] = [255, 200, 0];
const TEAL: [u8; 3] = [0, 150, 150];

/// Master units (100 raw): bounds are exactly 0..100 in both axes.
fn fixture() -> Vec<u8> {
    let mut f = tcb(false, Some([0xEF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]));
    // Colour c is table entry c + 1.
    f.extend(colour_table(&[(0, [255, 255, 255]), (2, RED), (3, BLUE), (4, GREEN), (5, MAGENTA), (6, ORANGE), (7, TEAL)]));
    // Red line along the bottom edge.
    f.extend(line(1, false, 1, (0, 0), (10000, 0)));
    // Shape 10..40 with blue outline and orange fill linkage.
    let mut shape = vec![5, 0];
    shape.extend(ints(&[1000, 1000, 4000, 1000, 4000, 4000, 1000, 4000, 1000, 1000]));
    shape.extend([0x07, 0x10, 0x41, 0x00, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0]);
    f.extend(std(6, 1, false, 3 + 4 * 5, 0x0800, sym(2, 31, 0), &shape));
    // Arc: centre (70,70), radius 20, 0..90 degrees counter-clockwise, green.
    let mut arc = ints(&[0, 90 * 360_000]);
    arc.extend(vaxb(2000.0));
    arc.extend(vaxb(2000.0));
    arc.extend(p32(0));
    arc.extend(vaxb(7000.0));
    arc.extend(vaxb(7000.0));
    f.extend(std(16, 1, false, 0, 0, sym(3, 0, 0), &arc));
    // Text "HELLO" at (50,60), 6 master units high, blue.
    let mut t = vec![1, 0];
    t.extend(ints(&[100_000, 100_000, 0, 5000, 6000]));
    t.extend([5, 0]);
    t.extend(b"HELLO");
    f.extend(std(17, 1, false, 0, 0, sym(2, 0, 0), &t));
    // Cell with two lines along the top, magenta.
    let mut cell = vec![0u8; 62];
    cell[0] = 2;
    f.extend(std(2, 1, false, 0, 0, 0, &cell));
    f.extend(line(1, true, 4, (500, 10000), (4500, 10000)));
    f.extend(line(1, true, 4, (5000, 10000), (9000, 10000)));
    // Complex chain: (60,10) -> (60,30) -> (90,30), teal.
    f.extend(std(12, 1, false, 0, 0, sym(6, 0, 0), &[0, 0, 2, 0]));
    f.extend(line(1, true, 0, (6000, 1000), (6000, 3000)));
    f.extend(line(1, true, 0, (6000, 3000), (9000, 3000)));
    // Level 5 is off in the view mask: must not draw.
    f.extend(line(5, false, 2, (0, 5000), (3000, 5000)));
    end(f)
}

// ---- sampling --------------------------------------------------------------------------

const S: f64 = 512.0 / 102.0;

fn px(img: &Raster, x: i64, y: i64) -> [u8; 3] {
    let (w, h) = (i64::from(img.width), i64::from(img.height));
    if x < 0 || y < 0 || x >= w || y >= h {
        return [255, 255, 255];
    }
    let i = ((y * w + x) * 4) as usize;
    [img.rgba[i], img.rgba[i + 1], img.rgba[i + 2]]
}

/// Pixels in the box (master units, y up) matching `pred`.
fn count(img: &Raster, x: (f64, f64), y: (f64, f64), pred: impl Fn([u8; 3]) -> bool) -> usize {
    let (x0, x1) = (((x.0 + 1.0) * S) as i64, ((x.1 + 1.0) * S) as i64);
    let (y0, y1) = (((101.0 - y.1) * S) as i64, ((101.0 - y.0) * S) as i64);
    let mut n = 0;
    for yy in y0..=y1 {
        for xx in x0..=x1 {
            if pred(px(img, xx, yy)) {
                n += 1;
            }
        }
    }
    n
}

fn near(img: &Raster, x: f64, y: f64, pred: impl Fn([u8; 3]) -> bool) -> bool {
    count(img, (x - 0.7, x + 0.7), (y - 0.7, y + 0.7), pred) > 0
}

/// `p` is `c` blended towards white (thin lines are anti-aliased): every channel that differs
/// from white moved by the same fraction of the way, at least a third, and the rest stays white.
fn is(c: [u8; 3]) -> impl Fn([u8; 3]) -> bool {
    move |p| {
        let mut fr = Vec::new();
        for (a, b) in p.iter().zip(c) {
            if b == 255 {
                if *a < 235 {
                    return false;
                }
            } else {
                fr.push((255.0 - f64::from(*a)) / (255.0 - f64::from(b)));
            }
        }
        let mean = fr.iter().sum::<f64>() / fr.len() as f64;
        mean >= 0.33 && fr.iter().all(|f| (f - mean).abs() < 0.2)
    }
}

fn nonwhite(p: [u8; 3]) -> bool {
    p.iter().map(|v| u32::from(*v)).sum::<u32>() < 720
}

// ---- tests -----------------------------------------------------------------------------

#[test]
fn reads_vax_and_pdp_numbers() {
    for v in [1.0, -1.0, 0.5, 2000.0, 7000.25, -123456.789, 1e-3, 3.0e9] {
        let got = vax(&vaxb(v), 0).unwrap();
        assert!((got - v).abs() <= v.abs() * 1e-12, "{v} -> {got}");
    }
    assert_eq!(vax(&[0; 8], 0), Some(0.0));
    for v in [0, 1, -1, 70000, -70000, i32::MAX, i32::MIN] {
        assert_eq!(i32p(&p32(v), 0), Some(v));
    }
    assert_eq!(i32p(&[1, 2, 3], 0), None);
}

#[test]
fn curve_through_collinear_points_stays_on_the_line() {
    let pts: Vec<(f64, f64)> = (0..7).map(|i| (f64::from(i), 0.0)).collect();
    let c = curve(&pts);
    assert_eq!(c.first(), Some(&(2.0, 0.0)));
    assert!(c.last().is_some_and(|p| (p.0 - 4.0).abs() < 1e-9));
    assert!(c.iter().all(|p| p.1.abs() < 1e-9));
    // Too few points or coincident points: kept as given.
    assert_eq!(curve(&pts[..3]).len(), 3);
}

#[test]
fn detects_v7() {
    let f = fixture();
    assert!(is_v7(&f));
    assert_eq!(underlay::sheets(&f).unwrap(), vec!["Model".to_string()]);
    let mut seed = 12345u32;
    for _ in 0..20 {
        let junk: Vec<u8> = (0..4000)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 24) as u8
            })
            .collect();
        assert!(!is_v7(&junk));
    }
    assert!(!is_v7(&[]));
    assert!(!is_v7(&f[..100]));
}

#[test]
fn v8_is_refused() {
    let mut f = OLE.to_vec();
    f.extend(vec![0u8; 600]);
    assert_eq!(scene(&f).err().as_deref(), Some("DGN V8 is not supported: no public specification"));
    assert_eq!(underlay::sheets(&f).err().as_deref(), Some("DGN V8 is not supported: no public specification"));
}

#[test]
fn renders_the_drawing() {
    let f = fixture();
    let sc = scene(&f).unwrap();
    assert!((sc.width - 102.0).abs() < 1e-6 && (sc.height - 102.0).abs() < 1e-6, "{}x{}", sc.width, sc.height);
    assert_eq!(sc.skipped, 0);
    let (w, h) = underlay::page_size(&f, 1).unwrap();
    assert!((w - 102.0).abs() < 1e-6 && (h - 102.0).abs() < 1e-6);
    let img = underlay::render(&f, 1, 512).unwrap();
    assert_eq!((img.width, img.height), (512, 512));

    // Red line along the bottom.
    assert!(near(&img, 50.0, 0.0, is(RED)));
    // Filled shape: orange inside, blue outline on the left edge.
    assert!(near(&img, 25.0, 25.0, is(ORANGE)));
    assert!(near(&img, 10.0, 25.0, is(BLUE)));
    // Arc: green at 45 degrees and at both ends, nothing in the centre.
    let d = 20.0 * std::f64::consts::FRAC_1_SQRT_2;
    assert!(near(&img, 70.0 + d, 70.0 + d, is(GREEN)));
    assert!(near(&img, 90.0, 70.0, is(GREEN)));
    assert!(near(&img, 70.0, 90.0, is(GREEN)));
    assert_eq!(count(&img, (66.0, 72.0), (66.0, 72.0), nonwhite), 0);
    // Text sits right of its origin, above its baseline, inside 50..80 x 60..66.
    assert!(count(&img, (50.0, 80.0), (60.0, 66.0), is(BLUE)) > 20);
    assert_eq!(count(&img, (41.0, 48.0), (56.0, 70.0), nonwhite), 0);
    assert_eq!(count(&img, (50.0, 80.0), (67.0, 69.0), nonwhite), 0);
    // Cell lines along the top.
    assert!(near(&img, 25.0, 100.0, is(MAGENTA)));
    assert!(near(&img, 70.0, 100.0, is(MAGENTA)));
    // Complex chain, both legs.
    assert!(near(&img, 60.0, 20.0, is(TEAL)));
    assert!(near(&img, 75.0, 30.0, is(TEAL)));
    // The line on the disabled level is absent.
    assert_eq!(count(&img, (0.0, 30.0), (48.0, 52.0), nonwhite), 0);
}

#[test]
fn a_cleared_mask_bit_is_the_only_hidden_level() {
    // Same drawing with every level on: the level 5 line shows.
    let mut f = fixture();
    f[48..56].copy_from_slice(&[0xFF; 8]);
    let img = underlay::render(&f, 1, 512).unwrap();
    assert!(near(&img, 15.0, 50.0, is(BLUE)));
}

#[test]
fn three_d_files_draw_the_xy_plane() {
    let mut f = tcb(true, None);
    f.extend(colour_table(&[(2, RED), (3, BLUE)]));
    f.extend(std(3, 1, false, 0, 0, sym(1, 0, 0), &ints(&[0, 0, 500, 10000, 0, 900])));
    f.extend(std(3, 1, false, 0, 0, sym(2, 0, 0), &ints(&[0, 0, 0, 0, 10000, -300])));
    let f = end(f);
    let img = underlay::render(&f, 1, 512).unwrap();
    assert!(near(&img, 50.0, 0.0, is(RED)));
    assert!(near(&img, 0.0, 50.0, is(BLUE)));
}

#[test]
fn empty_drawings_and_bad_sheets_are_errors() {
    let f = end(tcb(false, None));
    assert_eq!(scene(&f).err().as_deref(), Some("the DGN file draws nothing"));
    assert!(underlay::render(&fixture(), 2, 256).is_err());
    assert!(underlay::render(&fixture(), 0, 256).is_err());
    assert_eq!(scene(b"nope").err().as_deref(), Some("not a DGN V7 file"));
}

#[test]
fn default_colours_and_deleted_elements() {
    let mut f = tcb(false, None);
    // No colour table: colour 3 is red; the deleted blue line must not draw.
    f.extend(line(1, false, 3, (0, 0), (10000, 0)));
    f.extend(line(1, false, 1, (0, 5000), (10000, 5000)));
    let n = f.len();
    f[n - 52..n - 50].copy_from_slice(&0x8301u16.to_le_bytes());
    let f = end(f);
    let img = underlay::render(&f, 1, 512).unwrap();
    // Only the red line counts towards the bounds (the deleted line would make the sheet square).
    assert!(img.height < img.width / 4, "{}x{}", img.width, img.height);
    assert!(img.rgba.chunks(4).any(|p| is(RED)([p[0], p[1], p[2]])));
}

#[test]
fn unsupported_elements_are_counted() {
    let mut f = tcb(false, None);
    f.extend(line(1, false, 3, (0, 0), (10000, 0)));
    f.extend(std(35, 1, false, 0, 0, 0, &[0; 40]));
    f.extend(std(18, 1, false, 0, 0, 0, &[0; 10]));
    f.extend(line(1, true, 3, (0, 0), (0, 10000)));
    f.extend(std(22, 1, false, 0, 0, 0, &[0; 10]));
    let sc = scene(&end(f)).unwrap();
    // 35, 18 (its complex component is skipped silently), 22.
    assert_eq!(sc.skipped, 3);
    assert_eq!(sc.ops.len(), 1);
}

#[test]
fn hostile_files_never_panic() {
    let f = fixture();
    for n in 0..f.len() {
        let _ = scene(&f[..n]);
    }
    for i in 0..f.len() {
        let mut g = f.clone();
        g[i] ^= 0xFF;
        let _ = scene(&g);
        g[i] = 0x7F;
        let _ = scene(&g);
    }
    // Bogus lengths and counts.
    let mut g = tcb(false, None);
    g.extend([0x04, 0x04, 0xFF, 0xFF]);
    let _ = scene(&g);
    let mut g = tcb(false, None);
    g.extend(std(12, 1, false, 0, 0, 0, &[0xFF, 0xFF, 0xFF, 0xFF]));
    g.extend(line(1, true, 1, (0, 0), (1, 1)));
    let _ = scene(&g);
    let mut g = tcb(false, None);
    g.extend(std(4, 1, false, 0, 0, 0, &[0xFF, 0xFF, 1, 2, 3, 4]));
    let _ = scene(&g);
    let mut g = tcb(false, None);
    let mut arc = ints(&[i32::MIN, i32::MAX]);
    arc.extend(vaxb(1e300));
    arc.extend([0xFF; 8]);
    arc.extend(ints(&[i32::MAX]));
    arc.extend([0xFF; 16]);
    g.extend(std(16, 1, false, 0, 0, 0, &arc));
    let _ = scene(&g);
}

#[test]
fn millions_of_empty_elements_stay_bounded() {
    let mut f = tcb(false, None);
    f.extend(vec![0u8; 4 * 5_200_000]);
    assert_eq!(scene(&f).err().as_deref(), Some("the DGN file draws nothing"));
}
