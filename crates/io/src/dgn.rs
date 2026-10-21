//! DGN V7 (ISFF) design files, read from the public element-structure documentation: lines,
//! line strings, shapes, curves, arcs, ellipses, complex chains/shapes, cells and text become a
//! [`Scene`] of the design plane (Z is ignored in 3D files). Level display follows view 1's
//! level mask, colours the file's colour table (or our own default table).
//!
//! Units: coordinates are converted to master units (raw / UORs per subunit / subunits per
//! master unit) and one master unit is one point; the long side is clamped to [1e-3, 1e7]
//! points (the drawing is scaled to fit). The sheet is the drawing's bounds plus a 1% margin.
//!
//! The file is hostile input: every read is bounds-checked, loops are capped, nothing panics.

use std::f64::consts::PI;

use tiny_skia::{FillRule, LineCap, LineJoin, Transform};

use crate::underlay::{Brush, Op, Scene, StrokeStyle, polyline};

pub(crate) const MODEL_NAME: &str = "Model";

/// Most elements read from one file.
const MAX_ELEMENTS: usize = 5_000_000;
/// Most points kept in one complex chain.
const MAX_CHAIN_POINTS: usize = 200_000;
/// Smallest element that can hold the TCB fields we read (up to the 3D flag at byte 1214).
const MIN_TCB: usize = 1216;
const OLE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

fn arr<const N: usize>(b: &[u8], o: usize) -> Option<[u8; N]> {
    b.get(o..o.checked_add(N)?)?.try_into().ok()
}

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    arr::<2>(b, o).map(u16::from_le_bytes)
}

/// A 32-bit integer in the file's PDP-endian order (high word first, each word little endian).
fn i32p(b: &[u8], o: usize) -> Option<i32> {
    let [b0, b1, b2, b3] = arr::<4>(b, o)?;
    let v = (u32::from(u16::from_le_bytes([b0, b1])) << 16) | u32::from(u16::from_le_bytes([b2, b3]));
    Some(v as i32)
}

/// A VAX D-float: file bytes [B6 B7 B4 B5 B2 B3 B0 B1]; sign, 8-bit exponent, 55-bit fraction
/// with a hidden leading 0.1b: value = ±2^(exp-128) * (0.5 + frac / 2^56).
fn vax(b: &[u8], o: usize) -> Option<f64> {
    let [f0, f1, f2, f3, f4, f5, f6, f7] = arr::<8>(b, o)?;
    let bits = u64::from_le_bytes([f6, f7, f4, f5, f2, f3, f0, f1]);
    let exp = ((bits >> 55) & 0xFF) as i32;
    if exp == 0 {
        return Some(0.0);
    }
    let frac = (bits & ((1u64 << 55) - 1)) as f64;
    let v = 2f64.powi(exp - 128) * (0.5 + frac / 72_057_594_037_927_936.0);
    Some(if bits >> 63 == 1 { -v } else { v })
}

fn good(v: f64) -> bool {
    v.is_finite() && v.abs() < 1e12
}

/// The first element: its type word and whole bytes, when it fits the file.
fn first_element(bytes: &[u8]) -> Option<(u8, &[u8])> {
    let w0 = u16le(bytes, 0)?;
    let len = 4 + 2 * usize::from(u16le(bytes, 2)?);
    Some((((w0 >> 8) & 0x7F) as u8, bytes.get(..len)?))
}

pub(crate) fn is_v7(bytes: &[u8]) -> bool {
    let Some((ty, el)) = first_element(bytes) else { return false };
    match ty {
        9 => el.len() >= MIN_TCB,
        // Old files start with a digitizer setup or a cell-library header (type 5): no magic,
        // so also require a plausible header size and a next element that fits (or the end).
        5 | 8 => {
            el.len() >= 28 && {
                let rest = bytes.get(el.len()..).unwrap_or(&[]);
                rest.is_empty() || u16le(rest, 0) == Some(0xFFFF) || first_element(rest).is_some()
            }
        }
        _ => false,
    }
}

// ---- file-wide state -------------------------------------------------------------------

struct Meta {
    three_d: bool,
    /// Master units per raw unit.
    scale: f64,
    /// Levels shown (bit n-1 = level n); `None` = all.
    mask: Option<[u8; 8]>,
    /// 256 RGB entries; entry 0 is the background, entry c+1 is element colour c.
    table: Option<Vec<[u8; 3]>>,
    tcb_seen: bool,
    skipped: usize,
}

impl Meta {
    fn new() -> Self {
        Meta { three_d: false, scale: 1.0, mask: None, table: None, tcb_seen: false, skipped: 0 }
    }

    /// Level 0 counts as level 1.
    fn shown(&self, level: u8) -> bool {
        let n = usize::from(level.max(1)) - 1;
        self.mask.is_none_or(|m| m.get(n / 8).is_some_and(|b| b >> (n % 8) & 1 == 1))
    }

    fn tcb(&mut self, el: &[u8]) {
        if self.tcb_seen || el.len() < MIN_TCB {
            return;
        }
        self.tcb_seen = true;
        self.three_d = el.get(1214).is_some_and(|b| b & 0x40 != 0);
        if let (Some(ups), Some(spm)) = (i32p(el, 1112), i32p(el, 1116))
            && ups > 0
            && spm > 0
        {
            self.scale = 1.0 / (f64::from(ups) * f64::from(spm));
        }
        self.mask = arr::<8>(el, 48).filter(|m| m.iter().any(|b| *b != 0));
    }

    fn colour_table(&mut self, el: &[u8]) {
        if self.table.is_some() {
            return;
        }
        let Some(rgb) = el.get(38..38 + 768) else { return };
        self.table = Some(rgb.as_chunks::<3>().0.to_vec());
    }

    fn rgb(&self, colour: u8) -> [u8; 3] {
        let i = if colour == 255 { 0 } else { usize::from(colour) + 1 };
        let own = self.table.as_ref().and_then(|t| t.get(i).copied());
        own.or_else(|| default_table().get(i).copied()).unwrap_or([0, 0, 0])
    }

    fn pt(&self, el: &[u8], o: usize) -> Option<(f64, f64)> {
        Some((f64::from(i32p(el, o)?) * self.scale, f64::from(i32p(el, o + 4)?) * self.scale))
    }
}

/// Our own table (not any CAD vendor's): element colours 0..=9 are white, blue, green, red,
/// yellow, magenta, orange, cyan, dark gray, light gray; the rest is an HSV ramp.
/// ponytail: the ramp is arbitrary; real files nearly always carry their own colour table.
fn default_table() -> Vec<[u8; 3]> {
    let mut t = vec![
        [255, 255, 255],
        [255, 255, 255],
        [0, 0, 255],
        [0, 255, 0],
        [255, 0, 0],
        [255, 255, 0],
        [255, 0, 255],
        [255, 128, 0],
        [0, 255, 255],
        [96, 96, 96],
        [192, 192, 192],
    ];
    for c in 10..=254u32 {
        let v = [1.0, 0.75, 0.5][(c % 3) as usize];
        t.push(hsv(f64::from(c - 10) * 300.0 / 245.0, 1.0, v));
    }
    t.push([255, 255, 255]);
    t
}

fn hsv(h: f64, s: f64, v: f64) -> [u8; 3] {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let q = |f: f64| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [q(r), q(g), q(b)]
}

// ---- primitives ------------------------------------------------------------------------

enum Geo {
    Poly {
        pts: Vec<(f64, f64)>,
        closed: bool,
    },
    /// `at` is the lower-left corner, `size` the extent (width, height) before rotation.
    Text {
        at: (f64, f64),
        cap: f64,
        wf: f64,
        rot: f64,
        size: (f64, f64),
        s: String,
    },
}

struct Prim {
    geo: Geo,
    colour: u8,
    weight: u8,
    style: u8,
    fill: Option<u8>,
}

/// (properties, attribute index, (style, weight, colour)) of a displayable element.
fn head(el: &[u8]) -> Option<(u16, u16, (u8, u8, u8))> {
    let sym = u16le(el, 34)?;
    Some((u16le(el, 32)?, u16le(el, 30)?, ((sym & 7) as u8, ((sym >> 3) & 31) as u8, (sym >> 8) as u8)))
}

/// The shape-fill colour (linkage 0x0041), unless the element is a hole.
fn fill_of(el: &[u8], props: u16, attindx: u16) -> Option<u8> {
    if props & 0x0800 == 0 || props & 0x8000 != 0 {
        return None;
    }
    let mut off = 32 + 2 * usize::from(attindx);
    for _ in 0..64 {
        let (h0, h1) = (*el.get(off)?, *el.get(off + 1)?);
        if h1 & 0x10 != 0 {
            if u16le(el, off + 2) == Some(0x0041) {
                return el.get(off + 8).copied();
            }
            off += 2 + 2 * usize::from(h0);
        } else if matches!(u16le(el, off), Some(0 | 0x8000)) {
            off += 8;
        } else {
            return None;
        }
    }
    None
}

/// Rotation about Z of a quaternion stored as four PDP integers at `o`.
fn quat_angle(el: &[u8], o: usize) -> Option<f64> {
    let mut q = [0.0; 4];
    for (i, v) in q.iter_mut().enumerate() {
        *v = f64::from(i32p(el, o + 4 * i)?) / 2_147_483_648.0;
    }
    let [q1, q2, q3, q4] = q;
    let a = (2.0 * (q3 * q4 + q1 * q2)).atan2(q1 * q1 - q2 * q2 - q3 * q3 + q4 * q4);
    Some(if a.is_finite() { a } else { 0.0 })
}

/// An ellipse (type 15) or arc (type 16) sampled as a polyline.
fn conic(el: &[u8], ty: u8, meta: &Meta) -> Option<Vec<(f64, f64)>> {
    let base = if ty == 16 { 8 } else { 0 };
    let (a, b) = (vax(el, 36 + base)? * meta.scale, vax(el, 44 + base)? * meta.scale);
    let ro = 52 + base;
    let (rot, ox, oy) = if meta.three_d {
        (quat_angle(el, ro)?, vax(el, 68 + base)?, vax(el, 76 + base)?)
    } else {
        (f64::from(i32p(el, ro)?) / 360_000.0 * PI / 180.0, vax(el, 56 + base)?, vax(el, 64 + base)?)
    };
    let (ox, oy) = (ox * meta.scale, oy * meta.scale);
    if ![a, b, rot, ox, oy].into_iter().all(good) {
        return None;
    }
    let (start, sweep) = if ty == 16 {
        let (s, w) = (i32p(el, 36)?, i32p(el, 40)? as u32);
        let mag = f64::from(w & 0x7FFF_FFFF) / 360_000.0;
        let mag = if mag == 0.0 { 360.0 } else { mag.min(360.0) };
        (f64::from(s) / 360_000.0 * PI / 180.0, if w >> 31 == 1 { -mag } else { mag } * PI / 180.0)
    } else {
        (0.0, 2.0 * PI)
    };
    let n = (sweep.abs() * 180.0 / PI / 5.0).ceil().clamp(2.0, 72.0) as usize;
    let (cr, sr) = (rot.cos(), rot.sin());
    Some(
        (0..=n)
            .map(|i| {
                let t = start + sweep * i as f64 / n as f64;
                let (x, y) = (a.abs() * t.cos(), b.abs() * t.sin());
                (ox + cr * x - sr * y, oy + sr * x + cr * y)
            })
            .collect(),
    )
}

/// The vertices of a curve (type 11) interpolated with the documented cubic segments; the
/// first two and last two points only give end tangents. Degenerate input stays a polyline.
fn curve(p: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let n = p.len();
    let seg = |k: usize| -> Option<(f64, f64, f64)> {
        let (a, b) = (p.get(k)?, p.get(k + 1)?);
        let d = (b.0 - a.0).hypot(b.1 - a.1);
        (d > 1e-12).then(|| ((b.0 - a.0) / d, (b.1 - a.1) / d, d))
    };
    if n < 5 {
        return p.to_vec();
    }
    // Slopes m[k] and lengths d[k] for k in 0..n-1.
    let mut m = Vec::with_capacity(n);
    let mut d = Vec::with_capacity(n);
    for k in 0..n - 1 {
        let Some((mx, my, dk)) = seg(k) else { return p.to_vec() };
        m.push((mx, my));
        d.push(dk);
    }
    let tang = |k: usize, c: usize| -> Option<f64> {
        let g = |i: usize| -> Option<f64> {
            let v = m.get(i)?;
            Some(if c == 0 { v.0 } else { v.1 })
        };
        let (m0, m1, m2, m3) = (g(k - 2)?, g(k - 1)?, g(k)?, g(k + 1)?);
        let (w1, w2) = ((m3 - m2).abs(), (m1 - m0).abs());
        Some(if w1 + w2 != 0.0 { (m1 * w1 + m2 * w2) / (w1 + w2) } else { (m2 + m1) / 2.0 })
    };
    let mut out = Vec::new();
    if let Some(first) = p.get(2) {
        out.push(*first);
    }
    for k in 2..n - 3 {
        let (Some(a), Some(b), Some(dk)) = (p.get(k), p.get(k + 1), d.get(k)) else { break };
        let (Some(tx0), Some(tx1), Some(ty0), Some(ty1)) = (tang(k, 0), tang(k + 1, 0), tang(k, 1), tang(k + 1, 1)) else {
            return p.to_vec();
        };
        let f = |x0: f64, x1: f64, t0: f64, t1: f64, s: f64| {
            let c = t0;
            let bb = (3.0 * (x1 - x0) / dk - 2.0 * t0 - t1) / dk;
            let aa = (t0 + t1 - 2.0 * (x1 - x0) / dk) / (dk * dk);
            ((aa * s + bb) * s + c) * s + x0
        };
        for j in 1..=8 {
            let s = dk * f64::from(j) / 8.0;
            out.push((f(a.0, b.0, tx0, tx1, s), f(a.1, b.1, ty0, ty1, s)));
        }
    }
    out
}

/// The outline of a line, line string, shape, curve, arc or ellipse element.
fn points(el: &[u8], ty: u8, meta: &Meta) -> Option<Vec<(f64, f64)>> {
    let pts = match ty {
        3 => {
            let second = if meta.three_d { 48 } else { 44 };
            vec![meta.pt(el, 36)?, meta.pt(el, second)?]
        }
        4 | 6 | 11 => {
            let n = usize::from(u16le(el, 36)?);
            let stride = if meta.three_d { 12 } else { 8 };
            let mut v = Vec::new();
            for i in 0..n {
                let Some(p) = meta.pt(el, 38 + i * stride) else { break };
                v.push(p);
            }
            if ty == 11 { curve(&v) } else { v }
        }
        15 | 16 => conic(el, ty, meta)?,
        _ => return None,
    };
    (pts.len() >= 2 && pts.iter().all(|p| good(p.0) && good(p.1))).then_some(pts)
}

fn text(el: &[u8], meta: &Meta) -> Option<Geo> {
    let (lm, hm) = (i32p(el, 38)?, i32p(el, 42)?);
    let (rot, ox, oy, nco) = if meta.three_d {
        (quat_angle(el, 46)?, i32p(el, 62)?, i32p(el, 66)?, 74)
    } else {
        (f64::from(i32p(el, 46)?) / 360_000.0 * PI / 180.0, i32p(el, 50)?, i32p(el, 54)?, 58)
    };
    let n = usize::from(*el.get(nco)?);
    let s: String = el.get(nco + 2..nco + 2 + n)?.iter().map(|&b| if b >= 32 && b != 127 { b as char } else { ' ' }).collect();
    if hm == 0 {
        return None;
    }
    // The documented character cell is 6 UORs per 1/1000 of the multiplier.
    let h = f64::from(hm).abs() / 1000.0 * 6.0 * meta.scale;
    let w = f64::from(lm).abs() / 1000.0 * 6.0 * meta.scale * n as f64;
    let at = (f64::from(ox) * meta.scale, f64::from(oy) * meta.scale);
    // ponytail: cap height taken as 0.7 of the cell height; justification and mirroring ignored.
    let g = Geo::Text { at, cap: 0.7 * h, wf: f64::from(lm).abs() / f64::from(hm).abs(), rot, size: (w, h), s };
    [at.0, at.1, h, w, rot].into_iter().all(good).then_some(g)
}

struct Chain {
    left: usize,
    pts: Vec<(f64, f64)>,
    closed: bool,
    fill: Option<u8>,
    sym: (u8, u8, u8),
}

impl Chain {
    fn push(&mut self, p: Vec<(f64, f64)>) {
        let skip = match (self.pts.last(), p.first()) {
            (Some(a), Some(b)) => (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9,
            _ => false,
        };
        let room = MAX_CHAIN_POINTS.saturating_sub(self.pts.len());
        self.pts.extend(p.into_iter().skip(usize::from(skip)).take(room));
    }

    fn prim(self) -> Prim {
        Prim { geo: Geo::Poly { pts: self.pts, closed: self.closed }, colour: self.sym.2, weight: self.sym.1, style: self.sym.0, fill: self.fill }
    }
}

/// Walk the elements in order, handing every drawable primitive to `emit`.
fn walk(bytes: &[u8], meta: &mut Meta, emit: &mut dyn FnMut(Prim)) {
    let mut pos = 0usize;
    let mut chain: Option<Chain> = None;
    let mut skip_complex = false;
    for _ in 0..MAX_ELEMENTS {
        let (Some(w0), Some(wtf)) = (u16le(bytes, pos), u16le(bytes, pos + 2)) else { break };
        if w0 == 0xFFFF {
            break;
        }
        let Some(el) = bytes.get(pos..pos + 4 + 2 * usize::from(wtf)) else { break };
        pos += el.len();
        let (ty, level, complex, deleted) = (((w0 >> 8) & 0x7F) as u8, (w0 & 0x3F) as u8, w0 & 0x80 != 0, w0 & 0x8000 != 0);

        if let Some(c) = chain.as_mut() {
            if !deleted {
                match points(el, ty, meta) {
                    Some(p) => c.push(p),
                    None => meta.skipped += 1,
                }
            }
            c.left = c.left.saturating_sub(1);
            if c.left == 0
                && let Some(c) = chain.take()
            {
                emit_chain(c, meta, emit);
            }
            continue;
        }
        if skip_complex && complex {
            continue;
        }
        skip_complex = false;
        if deleted {
            continue;
        }
        match ty {
            9 => meta.tcb(el),
            5 if level == 1 => meta.colour_table(el),
            1 | 2 | 5 | 7 | 8 | 10 => {}
            3 | 4 | 6 | 11 | 12 | 14 | 15 | 16 | 17 if !meta.shown(level) => {}
            3 | 4 | 6 | 11 | 15 | 16 => {
                let Some((props, attindx, (style, weight, colour))) = head(el) else {
                    meta.skipped += 1;
                    continue;
                };
                let closed = matches!(ty, 6 | 15);
                match points(el, ty, meta) {
                    Some(pts) => {
                        let fill = if closed { fill_of(el, props, attindx) } else { None };
                        emit(Prim { geo: Geo::Poly { pts, closed }, colour, weight, style, fill });
                    }
                    None => meta.skipped += 1,
                }
            }
            12 | 14 => {
                let Some((props, attindx, sym)) = head(el) else {
                    meta.skipped += 1;
                    continue;
                };
                let left = usize::from(u16le(el, 38).unwrap_or(0));
                if left > 0 {
                    let closed = ty == 14;
                    let fill = if closed { fill_of(el, props, attindx) } else { None };
                    chain = Some(Chain { left, pts: Vec::new(), closed, fill, sym });
                }
            }
            17 => match (head(el), text(el, meta)) {
                (Some((_, _, (style, weight, colour))), Some(geo)) => emit(Prim { geo, colour, weight, style, fill: None }),
                _ => meta.skipped += 1,
            },
            // Definitions of surfaces, solids, b-spline surfaces/curves and shared cells draw
            // through their components, which we don't interpret: skip the whole group.
            18 | 19 | 24 | 27 | 34 => {
                meta.skipped += 1;
                skip_complex = true;
            }
            _ => meta.skipped += 1,
        }
    }
    if let Some(c) = chain.take() {
        emit_chain(c, meta, emit);
    }
}

fn emit_chain(c: Chain, meta: &mut Meta, emit: &mut dyn FnMut(Prim)) {
    if c.pts.len() >= 2 {
        emit(c.prim());
    } else {
        meta.skipped += 1;
    }
}

// ---- scene -----------------------------------------------------------------------------

struct Bounds {
    min: (f64, f64),
    max: (f64, f64),
    any: bool,
}

impl Bounds {
    fn add(&mut self, p: (f64, f64)) {
        if !(good(p.0) && good(p.1)) {
            return;
        }
        if self.any {
            self.min = (self.min.0.min(p.0), self.min.1.min(p.1));
            self.max = (self.max.0.max(p.0), self.max.1.max(p.1));
        } else {
            (self.min, self.max, self.any) = (p, p, true);
        }
    }
}

/// On/off lengths in units of `u` for line styles 1..=7 (0 and others: solid).
fn dash(style: u8, u: f32) -> Vec<f32> {
    let pattern: &[f32] = match style {
        1 => &[0.3, 1.0],
        2 => &[3.0, 2.0],
        3 => &[6.0, 2.0],
        4 => &[4.0, 1.5, 0.5, 1.5],
        5 => &[1.5, 1.0],
        6 => &[4.0, 1.5, 0.5, 1.5, 0.5, 1.5],
        7 => &[6.0, 1.5, 2.0, 1.5],
        _ => &[],
    };
    pattern.iter().map(|p| p * u).collect()
}

pub(crate) fn scene(bytes: &[u8]) -> Result<Scene, String> {
    if bytes.starts_with(&OLE) {
        return Err(crate::underlay::V8_ERROR.to_string());
    }
    if !is_v7(bytes) {
        return Err("not a DGN V7 file".into());
    }
    let nothing = || "the DGN file draws nothing".to_string();

    // Pass 1: the colour table and the bounds of everything drawn.
    let mut meta = Meta::new();
    let mut b = Bounds { min: (0.0, 0.0), max: (0.0, 0.0), any: false };
    walk(bytes, &mut meta, &mut |p| match &p.geo {
        Geo::Poly { pts, .. } => pts.iter().for_each(|q| b.add(*q)),
        Geo::Text { at, rot, size, .. } => {
            let (c, s) = (rot.cos(), rot.sin());
            for (x, y) in [(0.0, 0.0), (size.0, 0.0), (size.0, size.1), (0.0, size.1)] {
                b.add((at.0 + c * x - s * y, at.1 + s * x + c * y));
            }
        }
    });
    if !b.any {
        return Err(nothing());
    }
    let size = (b.max.0 - b.min.0).max(b.max.1 - b.min.1);
    if !(size.is_finite() && size > 0.0) {
        return Err(nothing());
    }
    let k = size.clamp(1e-3, 1e7) / size;
    let margin = size * 0.01;
    let (w, h) = ((b.max.0 - b.min.0 + 2.0 * margin) * k, (b.max.1 - b.min.1 + 2.0 * margin) * k);
    let long = w.max(h);
    let mut out = Scene::new(w, h);

    // Pass 2: the primitives, y flipped, in points.
    let mut meta2 = Meta::new();
    meta2.table = meta.table.take();
    let font = cadcraft_fonts::TextFont::resolve("");
    let to_scene = |p: (f64, f64)| (((p.0 - b.min.0 + margin) * k) as f32, ((b.max.1 - p.1 + margin) * k) as f32);
    let unit = (long * 0.006) as f32;
    let colours: Vec<[u8; 4]> = (0..=255u8)
        .map(|c| {
            let [r, g, b] = meta2.rgb(c);
            [r, g, b, 255]
        })
        .collect();
    let rgba = |c: u8| colours.get(usize::from(c)).copied().unwrap_or([0, 0, 0, 255]);
    walk(bytes, &mut meta2, &mut |p| {
        let width = ((0.0004 + 0.00015 * f64::from(p.weight)) * long) as f32;
        match p.geo {
            Geo::Poly { pts, closed } => {
                let sp: Vec<(f32, f32)> = pts.iter().map(|q| to_scene(*q)).collect();
                let Some(path) = polyline(&sp, closed) else { return };
                let fill = p.fill.map(|f| (Brush::Solid(rgba(f)), FillRule::Winding));
                let mut st = StrokeStyle::solid(rgba(p.colour), width);
                st.dash = dash(p.style, unit);
                st.join = LineJoin::Round;
                st.cap = LineCap::Butt;
                out.push(Op { path, fill, stroke: Some(st), clip: 0 });
            }
            Geo::Text { at, cap, wf, rot, s, .. } => {
                let (ox, oy) = to_scene(at);
                let (c, sn) = ((rot.cos() * k) as f32, (rot.sin() * k) as f32);
                // Text space is y up; the scene is y down.
                let local = Transform::from_row(c, -sn, -sn, -c, ox, oy);
                out.text(&font, &s, cap, wf, 0.0, local, &Brush::Solid(rgba(p.colour)), 0);
            }
        }
    });
    out.skipped += meta2.skipped;
    if out.ops.is_empty() {
        return Err(nothing());
    }
    Ok(out)
}

#[cfg(test)]
#[path = "dgn_tests.rs"]
mod tests;
