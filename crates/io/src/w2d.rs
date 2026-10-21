//! W2D (WHIP) streams: the 2D drawing stream of a DWF. Autodesk's published opcode tables define
//! three kinds of opcode (single-byte, extended ASCII `( ... )`, extended binary `{ len op ... }`);
//! this reader draws the geometry, colours, fills, text and images into a [`Scene`].
//!
//! The stream is hostile input: counts and sizes are checked against the bytes left, every point,
//! primitive, opcode and image is capped, nesting is bounded, and nothing indexes or panics. A
//! single-byte opcode the reader does not know cannot be skipped (its operand length is
//! unknown), so reading stops there and what was drawn stays.

use std::collections::HashMap;
use std::f64::consts::{PI, TAU};
use std::sync::Arc;

use cadcraft_fonts::TextFont;
use cadcraft_render::images::{Raster, decode_raster, raster_dimensions};
use tiny_skia::{FillRule, LineCap, LineJoin, Path, PathBuilder, Pixmap, Rect, Transform};

use crate::underlay::{self, Brush, MAX_CLIPS, Op, Scene, StrokeStyle};

/// Most opcodes executed from one stream.
const MAX_OPCODES: usize = 20_000_000;
/// Most primitives kept before the rest are counted as skipped.
const MAX_PRIMS: usize = if cfg!(test) { 20_000 } else { 1_000_000 };
/// Most points kept in total.
const MAX_POINTS: usize = 8_000_000;
/// Most points of one readable-ASCII polyline.
const MAX_ASCII_POINTS: usize = 1_000_000;
/// Deepest nesting of extended ASCII opcodes that are read (the rest are skipped).
const MAX_DEPTH: u32 = 32;
/// Most pixels of one image.
const MAX_IMAGE_PIXELS: u64 = 16_000_000;
/// Most distinct fonts resolved.
const MAX_FONTS: usize = 64;
/// Logical coordinates are kept inside this, so sums of relative moves stay exact in `f64`.
const LIM: f64 = 1_099_511_627_776.0;
/// The share of the paper the drawing's box fills (the rest is margin).
const FILL: f64 = 0.96;
/// Scene coordinates are kept within this many points of the origin: the rasteriser panics on
/// absurdly large paths, and a drawing that far off the paper is not visible anyway.
const FAR: f64 = 100_000.0;

/// Whether `bytes` start with a DWF or W2D header (`(DWF V06.00)`, `(W2D V06.00)`).
pub(crate) fn is_w2d(bytes: &[u8]) -> bool {
    bytes.starts_with(b"(DWF V") || bytes.starts_with(b"(W2D V")
}

/// The scene of a W2D stream at its natural size.
pub(crate) fn scene(bytes: &[u8]) -> Result<Scene, String> {
    scene_with_size(bytes, None)
}

/// The scene of a W2D stream; `paper_pts` (width, height in points) overrides the natural size.
pub(crate) fn scene_with_size(bytes: &[u8], paper_pts: Option<(f64, f64)>) -> Result<Scene, String> {
    if !is_w2d(bytes) || bytes.len() < 12 {
        return Err("not a W2D stream".into());
    }
    let mut rd = Reader::new();
    rd.run(bytes.get(12..).unwrap_or(&[]), 0);
    rd.build(paper_pts)
}

// ---- byte reader ---------------------------------------------------------------------------

struct Rd<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn new(d: &'a [u8]) -> Self {
        Rd { d, p: 0 }
    }
    fn peek(&self) -> Option<u8> {
        self.d.get(self.p).copied()
    }
    fn left(&self) -> usize {
        self.d.len().saturating_sub(self.p)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let e = self.p.checked_add(n)?;
        let s = self.d.get(self.p..e)?;
        self.p = e;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        let b = *self.d.get(self.p)?;
        self.p += 1;
        Some(b)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(<[u8; 2]>::try_from(self.take(2)?).ok()?))
    }
    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_le_bytes(<[u8; 2]>::try_from(self.take(2)?).ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(<[u8; 4]>::try_from(self.take(4)?).ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(<[u8; 4]>::try_from(self.take(4)?).ok()?))
    }
    /// A binary count: one byte, or 0 followed by a 16-bit extension (256 + value).
    fn count(&mut self) -> Option<usize> {
        match self.u8()? {
            0 => Some(usize::from(self.u16()?) + 256),
            b => Some(usize::from(b)),
        }
    }
    /// A readable integer; white space and commas before it are skipped. `None` (nothing consumed
    /// but the separators) when no digit follows.
    fn int(&mut self) -> Option<i64> {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n' | b',')) {
            self.p += 1;
        }
        let start = self.p;
        let neg = match self.peek() {
            Some(b'-') => {
                self.p += 1;
                true
            }
            Some(b'+') => {
                self.p += 1;
                false
            }
            _ => false,
        };
        let mut v: i64 = 0;
        let mut any = false;
        while let Some(c @ b'0'..=b'9') = self.peek() {
            v = v.saturating_mul(10).saturating_add(i64::from(c - b'0'));
            any = true;
            self.p += 1;
        }
        if !any {
            self.p = start;
            return None;
        }
        Some(if neg { -v } else { v })
    }
    /// Whether a readable integer follows (after white space and commas).
    fn num_ahead(&self) -> bool {
        let mut q = self.p;
        while matches!(self.d.get(q), Some(b' ' | b'\t' | b'\r' | b'\n' | b',')) {
            q += 1;
        }
        matches!(self.d.get(q), Some(b'-' | b'+' | b'0'..=b'9'))
    }
}

/// The index of the `)` that closes an extended ASCII opcode whose body starts at `from`
/// (parentheses nest, `'...'` strings may hold anything, `\` escapes, `{len ...}` is skipped by its
/// count). `None` when it never closes.
fn ext_end(d: &[u8], from: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut p = from;
    while let Some(&b) = d.get(p) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(p);
                }
            }
            b'\'' => {
                p += 1;
                loop {
                    match d.get(p)? {
                        b'\\' => p = p.checked_add(2)?,
                        b'\'' => break,
                        _ => p += 1,
                    }
                }
            }
            b'{' => {
                let len = u32::from_le_bytes(<[u8; 4]>::try_from(d.get(p + 1..p.checked_add(5)?)?).ok()?);
                if len == 0 {
                    return None;
                }
                p = p.checked_add(usize::try_from(len).ok()?)?.checked_add(4)?;
            }
            _ => {}
        }
        p += 1;
    }
    None
}

// ---- drawing state ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Style {
    cap: LineCap,
    join: LineJoin,
    dash: u8,
}

struct ArcShape {
    c: [f64; 2],
    rx: f64,
    ry: f64,
    tilt: f64,
    a0: f64,
    sweep: f64,
    full: bool,
    pie: bool,
}

struct TextShape {
    font: Arc<TextFont>,
    s: String,
    h: f64,
    wf: f64,
    obl: f64,
    rot: f64,
    at: [f64; 2],
}

struct ImageShape {
    pm: Arc<Pixmap>,
    lo: [f64; 2],
    hi: [f64; 2],
    w: u32,
    h: u32,
}

/// A primitive in logical coordinates (y up); points live in `Reader::pts`.
enum Shape {
    Poly { start: usize, len: usize, closed: bool },
    Bezier { start: usize, len: usize },
    Tris { start: usize, len: usize },
    Marker { start: usize, len: usize, size: f64 },
    Multi(Box<[(usize, usize)]>),
    Arc(Box<ArcShape>),
    Text(Box<TextShape>),
    Image(Box<ImageShape>),
}

struct Prim {
    shape: Shape,
    color: [u8; 4],
    fill: bool,
    weight: f32,
    style: Style,
    clip: u32,
}

struct Reader {
    pts: Vec<[f64; 2]>,
    prims: Vec<Prim>,
    clips: Vec<[f64; 4]>,
    cur: [f64; 2],
    color: [u8; 4],
    cmap: Vec<[u8; 4]>,
    fill: bool,
    visible: bool,
    weight: f64,
    style: Style,
    clip: u32,
    marker: f64,
    font: String,
    fonts: HashMap<String, Arc<TextFont>>,
    ext: Option<[f64; 4]>,
    bounds: Option<[f64; 4]>,
    /// (x, y real size of one logical unit as inches).
    scale: Option<(f64, f64)>,
    bg: [u8; 3],
    skipped: usize,
    opcodes: usize,
    done: bool,
}

fn lim(v: f64) -> f64 {
    v.clamp(-LIM, LIM)
}

fn grow(ext: &mut Option<[f64; 4]>, x: f64, y: f64) {
    match ext {
        Some(e) => {
            e[0] = e[0].min(x);
            e[1] = e[1].min(y);
            e[2] = e[2].max(x);
            e[3] = e[3].max(y);
        }
        None => *ext = Some([x, y, x, y]),
    }
}

fn units(v: u16) -> f64 {
    f64::from(v) * TAU / 65536.0
}

impl Reader {
    fn new() -> Self {
        Reader {
            pts: Vec::new(),
            prims: Vec::new(),
            clips: Vec::new(),
            cur: [0.0, 0.0],
            color: [255, 255, 255, 255],
            cmap: default_cmap(),
            fill: false,
            visible: true,
            weight: 0.0,
            style: Style { cap: LineCap::Butt, join: LineJoin::Miter, dash: 0 },
            clip: 0,
            marker: 0.0,
            font: String::new(),
            fonts: HashMap::new(),
            ext: None,
            bounds: None,
            scale: None,
            bg: [255, 255, 255],
            skipped: 0,
            opcodes: 0,
            done: false,
        }
    }

    /// Execute the opcodes of `d`. A truncated or unknown single-byte opcode ends this level.
    fn run(&mut self, d: &[u8], depth: u32) {
        let mut r = Rd::new(d);
        while !self.done {
            let Some(op) = r.u8() else { return };
            if matches!(op, b' ' | b'\t' | b'\r' | b'\n') {
                continue;
            }
            self.opcodes += 1;
            if self.opcodes > MAX_OPCODES {
                self.skipped += 1;
                self.done = true;
                return;
            }
            if self.step(&mut r, op, depth).is_none() {
                return;
            }
        }
    }

    fn step(&mut self, r: &mut Rd, op: u8, depth: u32) -> Option<()> {
        match op {
            0x03 => {
                let c = r.take(4)?;
                if let [a, b, c, d] = *c {
                    self.color = [a, b, c, d];
                }
            }
            0x06 => self.set_font(r)?,
            0x07 => self.gouraud(r, false)?,
            0x67 => self.gouraud(r, true)?,
            0x0B | 0x11 => self.contours(r, false)?,
            0x4B | 0x6B | 0x71 => self.contours(r, true)?,
            0x0C => self.seg(r, false)?,
            0x6C => self.seg(r, true)?,
            0x8C => {
                for _ in 0..r.u8()? {
                    self.seg(r, false)?;
                }
            }
            0x10 => self.poly(r, false)?,
            0x70 => self.poly(r, true)?,
            0x12 => {
                let c = self.rpt(r, false)?;
                let rad = f64::from(r.u16()?);
                self.arc(c, rad, rad, 0.0, 0, 0);
            }
            0x72 => {
                let c = self.rpt(r, true)?;
                let rad = f64::from(r.u32()?);
                self.arc(c, rad, rad, 0.0, 0, 0);
            }
            0x92 => {
                let c = self.rpt(r, true)?;
                let rad = f64::from(r.u32()?);
                let (s, e) = (i64::from(r.u16()?), i64::from(r.u16()?));
                self.arc(c, rad, rad, 0.0, s, e);
            }
            0x65 => {
                let c = self.rpt(r, true)?;
                let (rh, rv) = (f64::from(r.u32()?), f64::from(r.u32()?));
                let (s, e, t) = (i64::from(r.u16()?), i64::from(r.u16()?), r.u16()?);
                self.arc(c, rh, rv, units(t), s, e);
            }
            0x14 => self.tris(r, false)?,
            0x74 => self.tris(r, true)?,
            0x77 => {
                // Textured polytriangle: the operand is listed but not defined; consume, draw nothing.
                let n = r.count()?;
                r.take(n.checked_mul(8)?)?;
                self.skipped += 1;
            }
            0x18 => {
                let f = r.i32()?;
                let h = r.i32()?;
                let at = self.rpt(r, true)?;
                let s = text_of(r)?;
                self.text(at, f64::from(h).abs(), (f & 0xFFFF) as u16, 1.0, 0, s);
            }
            0x78 => {
                let ws = r.u16()?;
                let _ics = r.u16()?;
                let _flags = r.i32()?;
                let q = r.u16()?;
                for _ in 0..2 {
                    for _ in 1..r.count()? {
                        r.count()?;
                    }
                }
                let f = r.i32()?;
                let h = r.i32()?;
                let at = self.rpt(r, true)?;
                r.take(32)?;
                let s = text_of(r)?;
                let wf = if ws == 0 { 1.0 } else { f64::from(ws) / 1024.0 };
                self.text(at, f64::from(h).abs(), (f & 0xFFFF) as u16, wf, q, s);
            }
            0x43 => {
                let i = r.int()?;
                self.set_index(i);
            }
            0x63 => {
                let i = r.u8()?;
                self.set_index(i64::from(i));
            }
            0x45 => {
                let c = [lim(r.int()? as f64), lim(r.int()? as f64)];
                let (rh, rv) = (r.int()? as f64, r.int()? as f64);
                let (s, e) = (r.int()?, r.int()?);
                let t = r.int()?;
                self.cur = c;
                self.arc(c, rh, rv, units(t.rem_euclid(65536) as u16), s, e);
            }
            0x46 => self.fill = true,
            0x66 => self.fill = false,
            0x47 => {
                r.int()?;
            }
            0x87 => {
                r.u16()?;
            }
            0x4C => {
                let s = self.pts.len();
                for _ in 0..2 {
                    let p = self.apt(r)?;
                    self.pts.push(p);
                }
                self.add(Shape::Poly { start: s, len: 2, closed: false }, false);
            }
            0x4D => self.ascii_set(r, true)?,
            0x50 => self.ascii_set(r, false)?,
            0x4F => {
                let (x, y) = (r.u32()?, r.u32()?);
                self.cur = [lim(f64::from(x)), lim(f64::from(y))];
            }
            0x52 => {
                let c = self.apt(r)?;
                let rad = r.int()? as f64;
                let (s, e) = if r.num_ahead() { (r.int()?, r.int()?) } else { (0, 0) };
                self.arc(c, rad, rad, 0.0, s, e);
            }
            0x53 => self.marker = r.int()?.max(0) as f64,
            0x73 => self.marker = f64::from(r.u32()?),
            0x56 => self.visible = true,
            0x76 => self.visible = false,
            0x62 => self.bezier(r, true)?,
            0x6D => self.markers(r, true)?,
            0x8D => self.markers(r, false)?,
            b'(' => self.ext_ascii(r, depth)?,
            b'{' => self.ext_binary(r)?,
            _ => {
                self.skipped += 1;
                return None;
            }
        }
        Some(())
    }

    // ---- points ----------------------------------------------------------------------------

    /// The next relative point: the move from the previous point, which becomes the current point.
    fn rpt(&mut self, r: &mut Rd, wide: bool) -> Option<[f64; 2]> {
        let (dx, dy) = if wide { (f64::from(r.i32()?), f64::from(r.i32()?)) } else { (f64::from(r.i16()?), f64::from(r.i16()?)) };
        self.cur = [lim(self.cur[0] + dx), lim(self.cur[1] + dy)];
        Some(self.cur)
    }

    /// The next readable absolute point (also the current point afterwards).
    fn apt(&mut self, r: &mut Rd) -> Option<[f64; 2]> {
        let (x, y) = (r.int()?, r.int()?);
        self.cur = [lim(x as f64), lim(y as f64)];
        Some(self.cur)
    }

    // ---- state ----------------------------------------------------------------------------

    fn set_index(&mut self, i: i64) {
        if let Some(c) = usize::try_from(i).ok().and_then(|i| self.cmap.get(i)) {
            self.color = *c;
        }
    }

    fn set_font(&mut self, r: &mut Rd) -> Option<()> {
        let n = r.count()?;
        let name = r.take(n)?;
        r.u8()?;
        r.u32()?;
        r.u32()?;
        self.font = String::from_utf8_lossy(name).into_owned();
        Some(())
    }

    fn font(&mut self) -> Arc<TextFont> {
        if let Some(f) = self.fonts.get(&self.font) {
            return f.clone();
        }
        let f = if self.fonts.len() >= MAX_FONTS { Arc::new(TextFont::Stroke) } else { Arc::new(TextFont::resolve(&self.font)) };
        if self.fonts.len() < MAX_FONTS {
            self.fonts.insert(self.font.clone(), f.clone());
        }
        f
    }

    // ---- primitives -------------------------------------------------------------------------

    /// Drop the points of a primitive that is not kept.
    fn discard(&mut self, shape: &Shape) {
        let start = match shape {
            Shape::Poly { start, .. } | Shape::Bezier { start, .. } | Shape::Tris { start, .. } | Shape::Marker { start, .. } => Some(*start),
            Shape::Multi(subs) => subs.first().map(|s| s.0),
            _ => None,
        };
        if let Some(s) = start {
            self.pts.truncate(s);
        }
    }

    fn add(&mut self, shape: Shape, fill: bool) {
        if self.color[3] == 0 {
            self.discard(&shape);
            return;
        }
        self.push_prim(shape, fill, self.color);
    }

    fn push_prim(&mut self, shape: Shape, fill: bool, color: [u8; 4]) {
        if !self.visible {
            self.discard(&shape);
            return;
        }
        if self.prims.len() >= MAX_PRIMS || self.pts.len() > MAX_POINTS {
            self.skipped += 1;
            self.discard(&shape);
            return;
        }
        self.grow_shape(&shape);
        self.prims.push(Prim { shape, color, fill, weight: self.weight as f32, style: self.style, clip: self.clip });
    }

    fn grow_shape(&mut self, shape: &Shape) {
        let Reader { pts, ext, .. } = self;
        let mut pts_of = |start: usize, len: usize| {
            for p in pts.get(start..start.saturating_add(len)).unwrap_or(&[]) {
                grow(ext, p[0], p[1]);
            }
        };
        match shape {
            Shape::Poly { start, len, .. } | Shape::Bezier { start, len } | Shape::Tris { start, len } | Shape::Marker { start, len, .. } => {
                pts_of(*start, *len);
            }
            Shape::Multi(subs) => {
                for (s, l) in subs.iter() {
                    pts_of(*s, *l);
                }
            }
            Shape::Arc(a) => {
                let rr = a.rx.max(a.ry);
                grow(ext, a.c[0] - rr, a.c[1] - rr);
                grow(ext, a.c[0] + rr, a.c[1] + rr);
            }
            Shape::Text(t) => {
                let w = t.s.chars().count() as f64 * 0.6 * t.h * t.wf;
                let (sn, cs) = t.rot.sin_cos();
                for (u, v) in [(0.0, 0.0), (w, 0.0), (w, t.h), (0.0, t.h)] {
                    grow(ext, t.at[0] + cs * u - sn * v, t.at[1] + sn * u + cs * v);
                }
            }
            Shape::Image(i) => {
                grow(ext, i.lo[0], i.lo[1]);
                grow(ext, i.hi[0], i.hi[1]);
            }
        }
    }

    /// Binary points pushed as a connected chain; the points of one primitive are relative to
    /// each other.
    fn chain(&mut self, r: &mut Rd, n: usize, wide: bool) -> Option<usize> {
        let start = self.pts.len();
        for _ in 0..n {
            let p = self.rpt(r, wide)?;
            self.pts.push(p);
        }
        Some(start)
    }

    fn seg(&mut self, r: &mut Rd, wide: bool) -> Option<()> {
        let s = self.chain(r, 2, wide)?;
        self.add(Shape::Poly { start: s, len: 2, closed: false }, false);
        Some(())
    }

    fn poly(&mut self, r: &mut Rd, wide: bool) -> Option<()> {
        let n = r.count()?;
        let s = self.chain(r, n, wide)?;
        self.finish_poly(s);
        Some(())
    }

    fn finish_poly(&mut self, start: usize) {
        let len = self.pts.len().saturating_sub(start);
        let region = self.fill && len >= 3;
        match len {
            0 => {}
            1 => self.add(Shape::Marker { start, len: 1, size: 0.0 }, false),
            _ => self.add(Shape::Poly { start, len, closed: region }, region),
        }
    }

    /// Readable `P`/`M`: a count and absolute points.
    fn ascii_set(&mut self, r: &mut Rd, marker: bool) -> Option<()> {
        let n = usize::try_from(r.int()?).ok()?;
        if n > MAX_ASCII_POINTS {
            self.skipped += 1;
            return None;
        }
        let s = self.pts.len();
        for _ in 0..n {
            let p = self.apt(r)?;
            self.pts.push(p);
        }
        self.finish_set(s, marker);
        Some(())
    }

    fn finish_set(&mut self, start: usize, marker: bool) {
        if marker {
            let len = self.pts.len().saturating_sub(start);
            if len > 0 {
                let size = self.marker;
                self.add(Shape::Marker { start, len, size }, false);
            }
        } else {
            self.finish_poly(start);
        }
    }

    fn markers(&mut self, r: &mut Rd, wide: bool) -> Option<()> {
        let n = r.count()?;
        let s = self.chain(r, n, wide)?;
        self.finish_set(s, true);
        Some(())
    }

    fn tris(&mut self, r: &mut Rd, wide: bool) -> Option<()> {
        let n = r.count()?;
        let s = self.chain(r, n, wide)?;
        if n >= 3 {
            self.add(Shape::Tris { start: s, len: n }, self.fill);
        } else {
            self.discard(&Shape::Tris { start: s, len: n });
        }
        Some(())
    }

    /// A Gouraud polytriangle (binary): each triangle takes the average of its corners' colours.
    fn gouraud(&mut self, r: &mut Rd, wide: bool) -> Option<()> {
        let n = r.count()?;
        let mut v: Vec<([f64; 2], [u8; 4])> = Vec::new();
        for _ in 0..n.checked_add(2)? {
            let p = self.rpt(r, wide)?;
            let c = r.take(4)?;
            let col = if let [a, b, c, d] = *c { [a, b, c, d] } else { [0; 4] };
            v.push((p, col));
        }
        self.gouraud_tris(&v);
        Some(())
    }

    fn gouraud_tris(&mut self, v: &[([f64; 2], [u8; 4])]) {
        for w in v.windows(3) {
            let [a, b, c] = w else { continue };
            let s = self.pts.len();
            self.pts.extend([a.0, b.0, c.0]);
            let mut avg = [0u8; 4];
            for (i, o) in avg.iter_mut().enumerate() {
                let sum: u32 = [a, b, c].iter().map(|x| u32::from(x.1.get(i).copied().unwrap_or(0))).sum();
                *o = (sum / 3) as u8;
            }
            if avg[3] == 0 {
                self.pts.truncate(s);
                continue;
            }
            self.push_prim(Shape::Tris { start: s, len: 3 }, self.fill, avg);
        }
    }

    /// Contour set: a count of contours, each point count, then all the points; always filled.
    fn contours(&mut self, r: &mut Rd, wide: bool) -> Option<()> {
        let cs = r.count()?;
        let mut counts = Vec::new();
        let mut total = 0usize;
        for _ in 0..cs {
            let c = r.count()?;
            total = total.checked_add(c)?;
            counts.push(c);
        }
        if total.checked_mul(if wide { 8 } else { 4 })? > r.left() {
            return None;
        }
        let mut subs = Vec::with_capacity(counts.len());
        for c in counts {
            let s = self.chain(r, c, wide)?;
            subs.push((s, c));
        }
        self.add(Shape::Multi(subs.into_boxed_slice()), true);
        Some(())
    }

    fn bezier(&mut self, r: &mut Rd, wide: bool) -> Option<()> {
        let n = r.count()?;
        let s = self.chain(r, n.checked_mul(3)?.checked_add(1)?, wide)?;
        let len = self.pts.len().saturating_sub(s);
        self.add(Shape::Bezier { start: s, len }, self.fill);
        Some(())
    }

    /// A circle, ellipse, or a part of one (angles in 65536ths of a turn).
    fn arc(&mut self, c: [f64; 2], rx: f64, ry: f64, tilt: f64, start: i64, end: i64) {
        if !(rx > 0.0 && ry > 0.0 && rx.is_finite() && ry.is_finite()) {
            return;
        }
        let (start, end) = (start.clamp(-(1 << 40), 1 << 40), end.clamp(-(1 << 40), 1 << 40));
        let sweep = (end - start).rem_euclid(65536);
        let full = sweep == 0;
        let shape = ArcShape {
            c,
            rx,
            ry,
            tilt,
            a0: start.rem_euclid(65536) as f64 * TAU / 65536.0,
            sweep: if full { TAU } else { sweep as f64 * TAU / 65536.0 },
            full,
            pie: self.fill && !full,
        };
        self.add(Shape::Arc(Box::new(shape)), self.fill);
    }

    fn text(&mut self, at: [f64; 2], h: f64, rot: u16, wf: f64, obl: u16, s: String) {
        if h <= 0.0 || s.trim().is_empty() {
            return;
        }
        let font = self.font();
        let shape = TextShape { font, s, h, wf, obl: units(obl), rot: units(rot), at };
        self.add(Shape::Text(Box::new(shape)), true);
    }

    // ---- extended opcodes ----------------------------------------------------------------------

    fn ext_binary(&mut self, r: &mut Rd) -> Option<()> {
        let len = usize::try_from(r.u32()?).ok()?;
        if len == 0 {
            // The writer could not tell the length: the block cannot be skipped.
            self.skipped += 1;
            return None;
        }
        let blk = r.take(len)?;
        let Some((op, body)) = blk.split_first_chunk::<2>() else {
            self.skipped += 1;
            return Some(());
        };
        let data = body.strip_suffix(b"}").unwrap_or(body);
        let ok = match u16::from_le_bytes(*op) {
            1 => self.binary_colormap(data),
            op @ (2 | 4..=8) => self.image(op, data),
            _ => None,
        };
        if ok.is_none() {
            self.skipped += 1;
        }
        Some(())
    }

    fn binary_colormap(&mut self, data: &[u8]) -> Option<()> {
        let mut r = Rd::new(data);
        let n = match r.u8()? {
            0 => 256,
            n => usize::from(n),
        };
        self.cmap = rgba_list(&mut r, n)?;
        Some(())
    }

    fn image(&mut self, op: u16, data: &[u8]) -> Option<()> {
        let mut r = Rd::new(data);
        let (cols, rows) = (r.u16()?, r.u16()?);
        let p1 = self.rpt(&mut r, true)?;
        let p2 = self.rpt(&mut r, true)?;
        r.u32()?;
        let px = u64::from(cols) * u64::from(rows);
        if px == 0 || px > MAX_IMAGE_PIXELS {
            return None;
        }
        let px = usize::try_from(px).ok()?;
        let (cols_n, rows_n) = (usize::from(cols), usize::from(rows));
        let rgba: Raster = match op {
            8 => {
                let n = usize::try_from(r.u32()?).ok()?;
                let jpeg = r.take(n)?;
                let (w, h) = raster_dimensions(jpeg).ok()?;
                if u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
                    return None;
                }
                decode_raster(jpeg).ok()?
            }
            _ => {
                let local = match op {
                    2 | 5 => {
                        let n = match r.u8()? {
                            0 => 256,
                            n => usize::from(n),
                        };
                        Some(rgba_list(&mut r, n)?)
                    }
                    _ => None,
                };
                let n = usize::try_from(r.u32()?).ok()?;
                let d = r.take(n)?;
                let mut out = Vec::with_capacity(px.checked_mul(4)?);
                let black = [0, 0, 0, 255];
                match op {
                    2 => {
                        let map = local.as_deref().unwrap_or(&[]);
                        let stride = cols_n.div_ceil(8);
                        if stride.checked_mul(rows_n)? > d.len() {
                            return None;
                        }
                        for y in 0..rows_n {
                            for x in 0..cols_n {
                                let byte = d.get(y * stride + x / 8).copied().unwrap_or(0);
                                let bit = usize::from((byte >> (7 - (x % 8))) & 1);
                                out.extend(map.get(bit).copied().unwrap_or(black));
                            }
                        }
                    }
                    4 | 5 => {
                        let map: &[[u8; 4]] = local.as_deref().unwrap_or(&self.cmap);
                        for &i in d.get(..px)? {
                            out.extend(map.get(usize::from(i)).copied().unwrap_or(black));
                        }
                    }
                    6 => {
                        for c in d.get(..px.checked_mul(3)?)?.as_chunks::<3>().0 {
                            out.extend(c);
                            out.push(255);
                        }
                    }
                    _ => out.extend(d.get(..px.checked_mul(4)?)?),
                }
                Raster { width: u32::from(cols), height: u32::from(rows), rgba: out }
            }
        };
        let (w, h) = (rgba.width, rgba.height);
        let pm = underlay::pixmap(&rgba)?;
        let lo = [p1[0].min(p2[0]), p1[1].min(p2[1])];
        let hi = [p1[0].max(p2[0]), p1[1].max(p2[1])];
        let shape = Shape::Image(Box::new(ImageShape { pm, lo, hi, w, h }));
        self.push_prim(shape, true, [255, 255, 255, 255]);
        Some(())
    }

    fn set_clip(&mut self, rect: Option<[f64; 4]>) {
        let Some(c) = rect else {
            self.clip = 0;
            return;
        };
        let c = [c[0].min(c[2]), c[1].min(c[3]), c[0].max(c[2]), c[1].max(c[3])];
        if let Some(i) = self.clips.iter().rposition(|k| *k == c) {
            self.clip = u32::try_from(i + 1).unwrap_or(0);
        } else if self.clips.len() < MAX_CLIPS {
            self.clips.push(c);
            self.clip = u32::try_from(self.clips.len()).unwrap_or(0);
        } else {
            self.skipped += 1;
            self.clip = 0;
        }
    }

    fn ext_ascii(&mut self, r: &mut Rd, depth: u32) -> Option<()> {
        let start = r.p;
        let close = ext_end(r.d, start)?;
        let body = r.d.get(start..close)?;
        r.p = close + 1;
        let n = body.iter().position(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\'' | b'(')).unwrap_or(body.len());
        let (name, rest) = body.split_at(n);
        let name = String::from_utf8_lossy(name).to_ascii_lowercase();
        // A malformed operand spoils only this opcode.
        let _ = self.ext_body(&name, rest, depth);
        Some(())
    }

    fn ext_body(&mut self, name: &str, rest: &[u8], depth: u32) -> Option<()> {
        let mut a = Rd::new(rest);
        match name {
            "color" => {
                if let (Some(rr), Some(g), Some(b), Some(al)) = (a.int(), a.int(), a.int(), a.int()) {
                    self.color = [byte(rr), byte(g), byte(b), byte(al)];
                }
            }
            "background" => {
                if let (Some(rr), Some(g), Some(b)) = (a.int(), a.int(), a.int()) {
                    self.bg = [byte(rr), byte(g), byte(b)];
                }
            }
            "colormap" => {
                let n = usize::try_from(a.int()?).ok()?.min(256);
                if let Some(list) = rgba_ascii(&mut a, n)
                    && !list.is_empty()
                {
                    self.cmap = list;
                }
            }
            "lineweight" => self.weight = lim(a.int().unwrap_or(0).max(0) as f64),
            "clip" => {
                let q = (|| Some([a.int()? as f64, a.int()? as f64, a.int()? as f64, a.int()? as f64]))();
                self.set_clip(q.map(|q| q.map(lim)));
            }
            "bounds" => {
                if let (Some(x0), Some(y0), Some(x1), Some(y1)) = (a.int(), a.int(), a.int(), a.int()) {
                    let (x0, y0, x1, y1) = (lim(x0 as f64), lim(y0 as f64), lim(x1 as f64), lim(y1 as f64));
                    self.bounds = Some([x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]);
                }
            }
            "scale" => {
                let text = String::from_utf8_lossy(rest);
                let mut w = text.split_whitespace();
                let sx = w.next().and_then(|t| t.parse::<f64>().ok());
                let sy = w.next().and_then(|t| t.parse::<f64>().ok());
                let u = w.next().and_then(|t| unit_inches(t.trim_matches('\'')));
                if let (Some(sx), Some(sy), Some(u)) = (sx, sy, u)
                    && sx.is_finite()
                    && sy.is_finite()
                    && sx > 0.0
                    && sy > 0.0
                {
                    self.scale = Some((sx * u, sy * u));
                }
            }
            "linecap" => {
                self.style.cap = match word(rest).as_str() {
                    "round" => LineCap::Round,
                    "square" => LineCap::Square,
                    _ => LineCap::Butt,
                }
            }
            "linejoin" => {
                self.style.join = match word(rest).as_str() {
                    "round" => LineJoin::Round,
                    "bevel" => LineJoin::Bevel,
                    _ => LineJoin::Miter,
                }
            }
            "linepattern" => self.style.dash = dash_id(&word(rest)),
            "bezier" => {
                let n = usize::try_from(a.int()?).ok()?;
                if n > MAX_ASCII_POINTS / 3 {
                    self.skipped += 1;
                    return Some(());
                }
                let s = self.pts.len();
                for _ in 0..n.checked_mul(3)?.checked_add(1)? {
                    let p = self.apt(&mut a)?;
                    self.pts.push(p);
                }
                let len = self.pts.len() - s;
                self.add(Shape::Bezier { start: s, len }, self.fill);
            }
            "contourset" => {
                let cs = usize::try_from(a.int()?).ok()?;
                if cs > rest.len() {
                    return Some(());
                }
                let mut counts = Vec::new();
                let mut total = 0usize;
                for _ in 0..cs {
                    let c = usize::try_from(a.int()?).ok()?;
                    total = total.checked_add(c)?;
                    counts.push(c);
                }
                if total > MAX_ASCII_POINTS || total > a.left() {
                    self.skipped += 1;
                    return Some(());
                }
                let mut subs = Vec::new();
                for c in counts {
                    let s = self.pts.len();
                    for _ in 0..c {
                        let p = self.apt(&mut a)?;
                        self.pts.push(p);
                    }
                    subs.push((s, c));
                }
                self.add(Shape::Multi(subs.into_boxed_slice()), true);
            }
            "gouraud" => {
                let n = usize::try_from(a.int()?).ok()?;
                if n > MAX_ASCII_POINTS || n > a.left() {
                    self.skipped += 1;
                    return Some(());
                }
                let mut v = Vec::new();
                for _ in 0..n + 2 {
                    let p = self.apt(&mut a)?;
                    let col = [byte(a.int()?), byte(a.int()?), byte(a.int()?), byte(a.int()?)];
                    v.push((p, col));
                }
                self.gouraud_tris(&v);
            }
            "drawinginfo" => {
                if depth < MAX_DEPTH {
                    self.run(rest, depth + 1);
                } else {
                    self.skipped += 1;
                }
            }
            "endofdwf" => self.done = true,
            "view" | "layer" | "comment" | "url" | "author" | "created" | "creator" | "description" | "modified" | "projection" | "sourcecreated"
            | "sourcefilename" | "sourcemodified" | "embedfile" | "embedref" => {}
            _ => self.skipped += 1,
        }
        Some(())
    }

    // ---- scene -------------------------------------------------------------------------------

    fn build(self, paper: Option<(f64, f64)>) -> Result<Scene, String> {
        let empty = || "the sheet is empty".to_string();
        if self.prims.is_empty() {
            return Err(empty());
        }
        let [mut x0, mut y0, mut x1, mut y1] =
            self.bounds.filter(|b| b.iter().all(|v| v.is_finite()) && (b[2] > b[0] || b[3] > b[1])).or(self.ext).ok_or_else(empty)?;
        let span = (x1 - x0).max(y1 - y0).max(1.0);
        for (lo, hi) in [(&mut x0, &mut x1), (&mut y0, &mut y1)] {
            let want = (*hi - *lo).max(span * 1e-3);
            let mid = (*hi + *lo) / 2.0;
            *lo = mid - want / 2.0;
            *hi = mid + want / 2.0;
        }
        let (w, h) = (x1 - x0, y1 - y0);
        let (pw, ph) = match paper {
            Some((a, b)) if a.is_finite() && b.is_finite() && a > 0.0 && b > 0.0 => (a.clamp(1.0, 14400.0), b.clamp(1.0, 14400.0)),
            _ => natural_size(w, h, self.scale),
        };
        let s = (pw * FILL / w).min(ph * FILL / h);
        if !(s.is_finite() && s > 0.0) {
            return Err("the sheet has no size".into());
        }
        let m = Map { x0, y1, ox: (pw - w * s) / 2.0, oy: (ph - h * s) / 2.0, s };
        let mut scene = Scene::new(pw, ph);
        scene.background = self.bg;
        scene.skipped = self.skipped;
        let mut clip_ids: Vec<Option<u32>> = vec![None; self.clips.len()];
        for prim in &self.prims {
            let clip = match prim.clip.checked_sub(1).and_then(|i| usize::try_from(i).ok()) {
                Some(i) => match (self.clips.get(i), clip_ids.get_mut(i)) {
                    (Some(c), Some(slot)) => *slot.get_or_insert_with(|| scene.add_clip(clip_paths(&m, c))),
                    _ => 0,
                },
                None => 0,
            };
            self.emit(&mut scene, &m, prim, clip);
        }
        if scene.ops.is_empty() {
            return Err(empty());
        }
        Ok(scene)
    }

    fn emit(&self, scene: &mut Scene, m: &Map, prim: &Prim, clip: u32) {
        let pts = |start: usize, len: usize| self.pts.get(start..start.saturating_add(len)).unwrap_or(&[]);
        let paint = |scene: &mut Scene, path: Option<Path>, fill: bool, rule: FillRule| {
            if let Some(path) = path {
                scene.push(op(path, prim, fill, rule, m.s, clip));
            } else {
                scene.skipped += 1;
            }
        };
        match &prim.shape {
            Shape::Poly { start, len, closed } => {
                let mut pb = PathBuilder::new();
                for (i, p) in pts(*start, *len).iter().enumerate() {
                    let (x, y) = m.pt(p[0], p[1]);
                    if i == 0 {
                        pb.move_to(x, y);
                    } else {
                        pb.line_to(x, y);
                    }
                }
                if *closed {
                    pb.close();
                }
                paint(scene, pb.finish(), prim.fill, FillRule::EvenOdd);
            }
            Shape::Bezier { start, len } => {
                let p = pts(*start, *len);
                let mut pb = PathBuilder::new();
                if let Some(f) = p.first() {
                    let (x, y) = m.pt(f[0], f[1]);
                    pb.move_to(x, y);
                }
                for c in p.get(1..).unwrap_or(&[]).as_chunks::<3>().0 {
                    let [a, b, e] = c;
                    let (a, b, e) = (m.pt(a[0], a[1]), m.pt(b[0], b[1]), m.pt(e[0], e[1]));
                    pb.cubic_to(a.0, a.1, b.0, b.1, e.0, e.1);
                }
                if prim.fill {
                    pb.close();
                }
                paint(scene, pb.finish(), prim.fill, FillRule::EvenOdd);
            }
            Shape::Tris { start, len } => {
                let p = pts(*start, *len);
                let mut pb = PathBuilder::new();
                for t in p.windows(3) {
                    let [a, b, c] = t else { continue };
                    let (mut a, mut b, c) = (m.pt(a[0], a[1]), m.pt(b[0], b[1]), m.pt(c[0], c[1]));
                    // One orientation for all, so the winding rule fills overlaps once.
                    if (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0) < 0.0 {
                        std::mem::swap(&mut a, &mut b);
                    }
                    pb.move_to(a.0, a.1);
                    pb.line_to(b.0, b.1);
                    pb.line_to(c.0, c.1);
                    pb.close();
                }
                paint(scene, pb.finish(), prim.fill, FillRule::Winding);
            }
            Shape::Marker { start, len, size } => {
                let half = if *size > 0.0 { (size * m.s / 2.0).clamp(0.75, FAR) } else { 1.5 } as f32;
                let d = half * 0.7;
                let mut pb = PathBuilder::new();
                for p in pts(*start, *len) {
                    let (x, y) = m.pt(p[0], p[1]);
                    for (dx, dy) in [(half, 0.0), (0.0, half), (d, d), (d, -d)] {
                        pb.move_to(x - dx, y - dy);
                        pb.line_to(x + dx, y + dy);
                    }
                }
                paint(scene, pb.finish(), false, FillRule::Winding);
            }
            Shape::Multi(subs) => {
                let mut pb = PathBuilder::new();
                for (s, l) in subs.iter() {
                    for (i, p) in pts(*s, *l).iter().enumerate() {
                        let (x, y) = m.pt(p[0], p[1]);
                        if i == 0 {
                            pb.move_to(x, y);
                        } else {
                            pb.line_to(x, y);
                        }
                    }
                    pb.close();
                }
                paint(scene, pb.finish(), true, FillRule::EvenOdd);
            }
            Shape::Arc(a) => paint(scene, arc_path(m, a), prim.fill, FillRule::EvenOdd),
            Shape::Text(t) => {
                let (sn, cs) = t.rot.sin_cos();
                let (tx, ty) = m.pt(t.at[0], t.at[1]);
                let local = Transform::from_row((cs * m.s) as f32, (-sn * m.s) as f32, (-sn * m.s) as f32, (-cs * m.s) as f32, tx, ty);
                // The height given is the line height; capitals are about four fifths of it. Type
                // taller than the paper is nonsense (and the rasteriser fails on huge outlines).
                if t.h * 0.8 * m.s > 20_000.0 {
                    scene.skipped += 1;
                    return;
                }
                scene.text(&t.font, &t.s, t.h * 0.8, t.wf, t.obl, local, &Brush::Solid(prim.color), clip);
            }
            Shape::Image(i) => {
                let (l, b) = m.pt(i.lo[0], i.lo[1]);
                let (rt, t) = m.pt(i.hi[0], i.hi[1]);
                let rect = Rect::from_ltrb(l.min(rt), t.min(b), l.max(rt), t.max(b));
                let (Some(rect), true) = (rect, i.w > 0 && i.h > 0) else {
                    scene.skipped += 1;
                    return;
                };
                let ts = Transform::from_row(
                    (rect.width() / i.w as f32).max(f32::MIN_POSITIVE),
                    0.0,
                    0.0,
                    (rect.height() / i.h as f32).max(f32::MIN_POSITIVE),
                    rect.left(),
                    rect.top(),
                );
                scene.push(Op {
                    path: PathBuilder::from_rect(rect),
                    fill: Some((Brush::Image { pixmap: i.pm.clone(), ts, opacity: 1.0 }, FillRule::Winding)),
                    stroke: None,
                    clip,
                });
            }
        }
    }
}

/// Scene coordinates (points, y down) of logical points.
struct Map {
    x0: f64,
    y1: f64,
    ox: f64,
    oy: f64,
    s: f64,
}

impl Map {
    fn pt(&self, x: f64, y: f64) -> (f32, f32) {
        ((self.ox + (x - self.x0) * self.s).clamp(-FAR, FAR) as f32, (self.oy + (self.y1 - y) * self.s).clamp(-FAR, FAR) as f32)
    }
}

fn clip_paths(m: &Map, c: &[f64; 4]) -> Vec<Path> {
    let (l, b) = m.pt(c[0], c[1]);
    let (r, t) = m.pt(c[2], c[3]);
    // A clip with no area hides what it clips: an empty region list.
    Rect::from_ltrb(l.min(r), t.min(b), l.max(r), t.max(b)).map(|r| vec![PathBuilder::from_rect(r)]).unwrap_or_default()
}

fn arc_path(m: &Map, a: &ArcShape) -> Option<Path> {
    let (st, ct) = a.tilt.sin_cos();
    let at = |u: f64, v: f64| {
        let (x, y) = (a.rx * u, a.ry * v);
        m.pt(a.c[0] + ct * x - st * y, a.c[1] + st * x + ct * y)
    };
    let n = (a.sweep / (PI / 2.0)).ceil().clamp(1.0, 4.0) as usize;
    let d = a.sweep / n as f64;
    let k = 4.0 / 3.0 * (d / 4.0).tan();
    let mut pb = PathBuilder::new();
    let (s0, c0) = a.a0.sin_cos();
    let first = at(c0, s0);
    if a.pie {
        let o = at(0.0, 0.0);
        pb.move_to(o.0, o.1);
        pb.line_to(first.0, first.1);
    } else {
        pb.move_to(first.0, first.1);
    }
    for i in 0..n {
        let (t0, t1) = (a.a0 + d * i as f64, a.a0 + d * (i + 1) as f64);
        let ((s0, c0), (s1, c1)) = (t0.sin_cos(), t1.sin_cos());
        let p1 = at(c0 - k * s0, s0 + k * c0);
        let p2 = at(c1 + k * s1, s1 - k * c1);
        let e = at(c1, s1);
        pb.cubic_to(p1.0, p1.1, p2.0, p2.1, e.0, e.1);
    }
    if a.full || a.pie {
        pb.close();
    }
    pb.finish()
}

/// The dash lengths (points) of a line pattern id.
fn dash(id: u8) -> Vec<f32> {
    match id {
        1 => vec![4.0, 3.0],
        2 => vec![1.0, 2.5],
        3 => vec![6.0, 2.0, 1.0, 2.0],
        4 => vec![6.0, 2.0, 1.0, 2.0, 1.0, 2.0],
        5 => vec![6.0, 2.0, 1.0, 2.0, 1.0, 2.0, 1.0, 2.0],
        6 => vec![10.0, 4.0],
        7 => vec![14.0, 3.0, 4.0, 3.0],
        8 => vec![14.0, 3.0, 4.0, 3.0, 4.0, 3.0],
        _ => Vec::new(),
    }
}

fn dash_id(p: &str) -> u8 {
    match p {
        "- -" => 1,
        "...." => 2,
        "-.-." => 3,
        "-.." => 4,
        "-..." => 5,
        "-- --" => 6,
        "center" => 7,
        "phantom" => 8,
        _ => 0,
    }
}

fn op(path: Path, prim: &Prim, fill: bool, rule: FillRule, s: f64, clip: u32) -> Op {
    if fill {
        return Op { path, fill: Some((Brush::Solid(prim.color), rule)), stroke: None, clip };
    }
    let width = (f64::from(prim.weight) * s).clamp(0.5, FAR) as f32;
    let mut st = StrokeStyle::solid(prim.color, width);
    st.cap = prim.style.cap;
    st.join = prim.style.join;
    st.dash = dash(prim.style.dash);
    Op { path, fill: None, stroke: Some(st), clip }
}

/// The paper size in points: from `(Scale ..)` when given, else a letter-ish long side; the
/// drawing's box fills [`FILL`] of it.
fn natural_size(w: f64, h: f64, scale: Option<(f64, f64)>) -> (f64, f64) {
    let (mut pw, mut ph) = match scale {
        Some((sx, sy)) => (w * sx * 72.0 / FILL, h * sy * 72.0 / FILL),
        None if w >= h => (792.0, 792.0 * h / w),
        None => (792.0 * w / h, 792.0),
    };
    let long = pw.max(ph);
    if !(long.is_finite() && long > 0.0) {
        return (792.0, 792.0);
    }
    let k = long.clamp(200.0, 14400.0) / long;
    pw *= k;
    ph *= k;
    (pw.max(1.0), ph.max(1.0))
}

fn unit_inches(u: &str) -> Option<f64> {
    Some(match u.to_ascii_lowercase().as_str() {
        "inch" | "inches" | "in" => 1.0,
        "foot" | "feet" | "ft" => 12.0,
        "meter" | "meters" | "metre" | "metres" | "m" => 1000.0 / 25.4,
        "millimeter" | "millimeters" | "millimetre" | "millimetres" | "mm" => 1.0 / 25.4,
        "centimeter" | "centimeters" | "centimetre" | "centimetres" | "cm" => 10.0 / 25.4,
        _ => return None,
    })
}

/// A text operand of an extended ASCII opcode: trimmed, unquoted, lower case.
fn word(rest: &[u8]) -> String {
    String::from_utf8_lossy(rest).trim().trim_matches('\'').trim().to_ascii_lowercase()
}

fn byte(v: i64) -> u8 {
    v.clamp(0, 255) as u8
}

/// The UTF-16 text of a binary text operand (count, then characters).
fn text_of(r: &mut Rd) -> Option<String> {
    let n = r.count()?;
    let raw = r.take(n.checked_mul(2)?)?;
    let units = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c));
    Some(char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}')).collect())
}

/// `n` RGBA entries as bytes.
fn rgba_list(r: &mut Rd, n: usize) -> Option<Vec<[u8; 4]>> {
    let raw = r.take(n.checked_mul(4)?)?;
    Some(raw.as_chunks::<4>().0.to_vec())
}

/// Up to `n` readable `r,g,b,a` entries (stops at the first that is not there).
fn rgba_ascii(a: &mut Rd, n: usize) -> Option<Vec<[u8; 4]>> {
    let mut out = Vec::new();
    for _ in 0..n {
        let (Some(r), Some(g), Some(b), Some(al)) = (a.int(), a.int(), a.int(), a.int()) else { break };
        out.push([byte(r), byte(g), byte(b), byte(al)]);
    }
    Some(out)
}

/// The default colour map: the 256 colours of the published `(ColorMap 256 ...)` default.
fn default_cmap() -> Vec<[u8; 4]> {
    DEFAULT_CMAP
        .split_whitespace()
        .filter_map(|t| {
            let mut it = t.split(',').map(|v| v.parse::<u8>().unwrap_or(0));
            Some([it.next()?, it.next()?, it.next()?, 255])
        })
        .collect()
}

const DEFAULT_CMAP: &str = "\
0,0,0 255,0,0 255,255,0 0,255,0 0,255,255 0,0,255 255,0,255 0,0,0 128,128,128 220,220,220 \
255,0,0 255,127,127 221,0,0 221,110,110 184,0,0 184,92,92 149,0,0 149,74,74 114,0,0 114,57,57 \
255,63,0 255,159,127 221,55,0 221,138,110 184,46,0 184,115,92 149,37,0 149,93,74 114,28,0 114,71,57 \
255,127,0 255,191,127 221,110,0 221,165,110 184,92,0 184,138,92 149,74,0 149,112,74 114,57,0 114,86,57 \
255,191,0 255,223,127 221,165,0 221,193,110 184,138,0 184,161,92 149,112,0 149,131,74 114,86,0 114,100,57 \
255,255,0 255,255,127 221,221,0 221,221,110 184,184,0 184,184,92 149,149,0 149,149,74 114,114,0 114,114,57 \
191,255,0 223,255,127 165,221,0 193,221,110 138,184,0 161,184,92 112,149,0 131,149,74 86,114,0 100,114,57 \
127,255,0 191,255,127 110,221,0 165,221,110 92,184,0 138,184,92 74,149,0 112,149,74 57,114,0 86,114,57 \
63,255,0 159,255,127 55,221,0 138,221,110 46,184,0 115,184,92 37,149,0 93,149,74 28,114,0 71,114,57 \
0,255,0 127,255,127 0,221,0 110,221,110 0,184,0 92,184,92 0,149,0 74,149,74 0,114,0 57,114,57 \
0,255,63 127,255,159 0,221,55 110,221,138 0,184,46 92,184,115 0,149,37 74,149,93 0,114,28 57,114,71 \
0,255,127 127,255,191 0,221,110 110,221,165 0,184,92 92,184,138 0,149,74 74,149,112 0,114,57 57,114,86 \
0,255,191 127,255,223 0,221,165 110,221,193 0,184,138 92,184,161 0,149,112 74,149,131 0,114,86 57,114,100 \
0,255,255 127,255,255 0,221,221 110,221,221 0,184,184 92,184,184 0,149,149 74,149,149 0,114,114 57,114,114 \
0,191,255 127,223,255 0,165,221 110,193,221 0,138,184 92,161,184 0,112,149 74,131,149 0,86,114 57,100,114 \
0,127,255 127,191,255 0,110,221 110,165,221 0,92,184 92,138,184 0,74,149 74,112,149 0,57,114 57,86,114 \
0,63,255 127,159,255 0,55,221 110,138,221 0,46,184 92,115,184 0,37,149 74,93,149 0,28,114 57,71,114 \
0,0,255 127,127,255 0,0,221 110,110,221 0,0,184 92,92,184 0,0,149 74,74,149 0,0,114 57,57,114 \
63,0,255 159,127,255 55,0,221 138,110,221 46,0,184 115,92,184 37,0,149 93,74,149 28,0,114 71,57,114 \
127,0,255 191,127,255 110,0,221 165,110,221 92,0,184 138,92,184 74,0,149 112,74,149 57,0,114 86,57,114 \
191,0,255 223,127,255 165,0,221 193,110,221 138,0,184 161,92,184 112,0,149 131,74,149 86,0,114 100,57,114 \
255,0,255 255,127,255 221,0,221 221,110,221 184,0,184 184,92,184 149,0,149 149,74,149 114,0,114 114,57,114 \
255,0,191 255,127,223 221,0,165 221,110,193 184,0,138 184,92,161 149,0,112 149,74,131 114,0,86 114,57,100 \
255,0,127 255,127,191 221,0,110 221,110,165 184,0,92 184,92,138 149,0,74 149,74,112 114,0,57 114,57,86 \
255,0,63 255,127,159 221,0,55 221,110,138 184,0,46 184,92,115 149,0,37 149,74,93 114,0,28 114,57,71 \
84,84,84 118,118,118 152,152,152 186,186,186 220,220,220 255,255,255";

#[cfg(test)]
#[path = "w2d_tests.rs"]
mod tests;
