//! PDF pages as pictures, for PDF underlays (PDFATTACH) and markup paper: one page drawn with
//! tiny-skia at the resolution it is shown at.
//!
//! Our own interpreter (clean-room, from the public PDF content stream syntax and the page
//! resources of ISO 32000): paths (`m l c v y h re`) filled, stroked and clipped, the graphics
//! state (`q Q cm w J j M d gs`, with `CA` / `ca` alpha), colour spaces (device, CIE-based by
//! their device equivalent, Indexed, Separation / DeviceN, Pattern), text (`BT … ET`, every
//! text-state operator, simple, Type 3 and Type 0 fonts, embedded TrueType / OpenType / CFF /
//! Type 1 outlines, else our own fonts with the widths of the font dictionary), images (XObjects
//! and inline: bit depths 1–16, Decode, ImageMask, SMask, Mask, Flate/LZW/RunLength/ASCII
//! filters and DCT), shadings (`sh` and shading patterns: types 1–3 with function types 0, 2, 3
//! and 4), tiling patterns and form XObjects. A white page is the background.
//!
//! Not drawn: JPX, JBIG2 and CCITT images, mesh shadings (types 4–7), clipping text render
//! modes, soft-mask and blend-mode graphics states, overprint. What is skipped is reported in
//! the notes of [`render_notes`]. The content is hostile input: operand counts, graphics-state
//! depth, form/pattern/Type 3 nesting, path size, clip count, operator count, decoded pixels and
//! glyph count are capped, and every malformed operator is skipped.

mod cff;
mod cmap;
mod color;
mod enc;
mod font;
mod func;
mod image;
mod shade;
mod type1;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use cadcraft_render::images::{MAX_PAGE_PX, MIN_PAGE_PX, Raster};
use tiny_skia::{
    FillRule, FilterQuality, LineCap, LineJoin, Mask, Paint, Path, PathBuilder, Pattern, Pixmap, Rect, SpreadMode, Stroke, StrokeDash, Transform,
};

use self::color::{Cs, cmyk, gray, rgb};
use self::font::{Font, Shape};
use self::image::Img;
use self::shade::Shading;
use crate::pdf_markup::{Dict, Lex, Obj, Pdf, locate, page_streams};

const MAX_OPS: usize = 5_000_000;
const MAX_OPERANDS: usize = 64;
/// Most nested `q`.
const MAX_DEPTH: usize = 128;
/// Most nested forms, patterns and Type 3 glyphs.
const MAX_NEST: usize = 10;
const MAX_PATH_SEGMENTS: usize = 2_000_000;
const MAX_CLIPS: usize = 2_000;
const MAX_FORMS: usize = 50_000;
const MAX_GLYPHS: usize = 3_000_000;
const MAX_FONT_LOADS: usize = 2_000;
/// Pixels decoded from images and painted by shadings in one page.
const MAX_PIXELS: u64 = 400 << 20;
/// Longest side of a tiling pattern's cell, in pixels.
const MAX_CELL: f64 = 2048.0;

/// The displayed size of page `page` in points.
pub fn page_size(bytes: &[u8], page: u32) -> Result<(f64, f64), String> {
    let (mut pdf, pdict, _) = locate(bytes, page as usize, "").map_err(|e| e.to_string())?;
    let g = pdf.page_geometry(&pdict).ok_or("the page has no usable size")?;
    Ok((g.dw, g.dh))
}

/// Page `page` drawn with its long side `max_dim` pixels (clamped to the picture limits).
pub fn render(bytes: &[u8], page: u32, max_dim: u32) -> Result<Raster, String> {
    render_notes(bytes, page, max_dim).map(|(r, _)| r)
}

/// [`render`] with what could not be drawn.
pub fn render_notes(bytes: &[u8], page: u32, max_dim: u32) -> Result<(Raster, Vec<String>), String> {
    let (mut pdf, pdict, _) = locate(bytes, page as usize, "").map_err(|e| e.to_string())?;
    let g = pdf.page_geometry(&pdict).ok_or("the page has no usable size")?;
    let (dw, dh) = (g.dw, g.dh);
    if !(dw.is_finite() && dh.is_finite() && dw > 0.0 && dh > 0.0) {
        return Err("the page has no size".into());
    }
    let long = f64::from(max_dim.clamp(MIN_PAGE_PX, MAX_PAGE_PX));
    let s = long / dw.max(dh);
    let (pw, ph) = (((dw * s).ceil() as u32).clamp(1, MAX_PAGE_PX), ((dh * s).ceil() as u32).clamp(1, MAX_PAGE_PX));
    let mut pm = Pixmap::new(pw, ph).ok_or("the page is too large")?;
    pm.fill(tiny_skia::Color::WHITE);
    // User space → displayed points (origin lower left, y up) → pixels (y down).
    let (w, h) = (g.w, g.h);
    let (a, b, cc, d, e, f) = match g.rot {
        90 => (0.0, -1.0, 1.0, 0.0, -g.y0, w + g.x0),
        180 => (-1.0, 0.0, 0.0, -1.0, w + g.x0, h + g.y0),
        270 => (0.0, 1.0, -1.0, 0.0, h + g.y0, -g.x0),
        _ => (1.0, 0.0, 0.0, 1.0, -g.x0, -g.y0),
    };
    let to_page = Transform::from_row(a as f32, b as f32, cc as f32, d as f32, e as f32, f as f32);
    let to_px = Transform::from_row(s as f32, 0.0, 0.0, -s as f32, 0.0, (dh * s) as f32);
    let base = to_page.post_concat(to_px);
    let content = page_streams(&mut pdf, &pdict);
    let res = match pdf.val(&pdict, "Resources") {
        Obj::Dict(d) => d,
        _ => Dict::new(),
    };
    let mut cx = Ctx::new(&mut pdf);
    cx.run(&mut pm, &content, &res, State::new(base), base, 0);
    let notes = cx.notes;
    Ok((Raster { width: pw, height: ph, rgba: pm.take() }, notes))
}

// ---- matrices ----

type M = [f64; 6];

const IDENT: M = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `a` then `b`.
fn mmul(a: M, b: M) -> M {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

fn ts_of(m: M) -> Transform {
    Transform::from_row(m[0] as f32, m[1] as f32, m[2] as f32, m[3] as f32, m[4] as f32, m[5] as f32)
}

fn matrix(v: Option<Vec<f64>>) -> Transform {
    match v.as_deref() {
        Some([a, b, c, d, e, f]) => {
            let t = Transform::from_row(*a as f32, *b as f32, *c as f32, *d as f32, *e as f32, *f as f32);
            if t.is_finite() { t } else { Transform::identity() }
        }
        _ => Transform::identity(),
    }
}

// ---- graphics state ----

enum Pat {
    Shade {
        sh: Shading,
        matrix: Transform,
    },
    Tile {
        content: Vec<u8>,
        bbox: [f64; 4],
        xstep: f64,
        ystep: f64,
        matrix: Transform,
        uncolored: bool,
        res: Dict,
        cell: RefCell<Option<(Transform, [u8; 3], Rc<Pixmap>)>>,
    },
}

#[derive(Clone)]
enum Paint3 {
    Solid([u8; 3]),
    /// A pattern, with the colour of an uncoloured tiling pattern.
    Pat(Rc<Pat>, [u8; 3]),
}

#[derive(Clone)]
struct State {
    ctm: Transform,
    fill: Paint3,
    stroke: Paint3,
    fill_cs: Rc<Cs>,
    stroke_cs: Rc<Cs>,
    fa: f32,
    sa: f32,
    width: f32,
    cap: LineCap,
    join: LineJoin,
    miter: f32,
    dash: Option<(Vec<f32>, f32)>,
    clip: Option<Rc<Mask>>,
    font: Option<Rc<Font>>,
    fsize: f64,
    tc: f64,
    tw: f64,
    th: f64,
    tl: f64,
    rise: f64,
    mode: i64,
}

impl State {
    fn new(ctm: Transform) -> State {
        State {
            ctm,
            fill: Paint3::Solid([0; 3]),
            stroke: Paint3::Solid([0; 3]),
            fill_cs: Rc::new(Cs::Gray),
            stroke_cs: Rc::new(Cs::Gray),
            fa: 1.0,
            sa: 1.0,
            width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter: 10.0,
            dash: None,
            clip: None,
            font: None,
            fsize: 0.0,
            tc: 0.0,
            tw: 0.0,
            th: 1.0,
            tl: 0.0,
            rise: 0.0,
            mode: 0,
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Fill(FillRule),
    Stroke,
}

struct Ctx<'a, 'p> {
    pdf: &'p mut Pdf<'a>,
    notes: Vec<String>,
    ops: usize,
    px: u64,
    glyphs: usize,
    forms: usize,
    clips: usize,
    font_loads: usize,
    next_font: usize,
    fonts: HashMap<u32, Option<Rc<Font>>>,
    imgs: HashMap<u32, Option<Rc<Img>>>,
    shapes: HashMap<(usize, u32), Option<Rc<Shape>>>,
    pats: HashMap<u32, Option<Rc<Pat>>>,
    procs: HashMap<u32, Option<Rc<Vec<u8>>>>,
    /// Objects being drawn (forms, Type 3 glyph procedures): none may contain itself.
    active: Vec<u32>,
}

/// The last `N` operands as numbers.
fn last<const N: usize>(st: &[Obj]) -> Option<[f64; N]> {
    let from = st.len().checked_sub(N)?;
    let mut out = [0.0; N];
    for (o, v) in st.get(from..)?.iter().zip(out.iter_mut()) {
        let Obj::Num(n) = o else { return None };
        *v = *n;
    }
    Some(out)
}

fn numbers(st: &[Obj]) -> Vec<f64> {
    st.iter().filter_map(|o| if let Obj::Num(v) = o { Some(*v) } else { None }).collect()
}

/// The scale from a transform's unit vectors to pixels, for hairlines.
fn px_scale(t: &Transform) -> f32 {
    ((t.sx * t.sy - t.kx * t.ky).abs() as f64).sqrt().max(1e-9) as f32
}

impl<'a, 'p> Ctx<'a, 'p> {
    fn new(pdf: &'p mut Pdf<'a>) -> Self {
        Ctx {
            pdf,
            notes: Vec::new(),
            ops: 0,
            px: MAX_PIXELS,
            glyphs: 0,
            forms: 0,
            clips: 0,
            font_loads: 0,
            next_font: 0,
            fonts: HashMap::new(),
            imgs: HashMap::new(),
            shapes: HashMap::new(),
            pats: HashMap::new(),
            procs: HashMap::new(),
            active: Vec::new(),
        }
    }

    /// The entry `name` of the resource category `cat`.
    fn resource(&mut self, res: &Dict, cat: &str, name: &str) -> Option<Obj> {
        let Obj::Dict(d) = self.pdf.val(res, cat) else { return None };
        d.get(name).cloned()
    }

    // ---- painting ----

    /// Fills or strokes `path` (in the space `ts` maps to device pixels) with the state's paint.
    fn draw(&mut self, pm: &mut Pixmap, gs: &State, path: &Path, ts: Transform, kind: Kind, base: Transform, depth: usize) {
        let (src, alpha) = match kind {
            Kind::Fill(_) => (&gs.fill, gs.fa),
            Kind::Stroke => (&gs.stroke, gs.sa),
        };
        let stroke = Stroke {
            width: gs.width.max(1.0 / px_scale(&ts)),
            miter_limit: gs.miter.max(1.0),
            line_cap: gs.cap,
            line_join: gs.join,
            dash: gs.dash.as_ref().and_then(|(a, ph)| StrokeDash::new(a.clone(), *ph)),
        };
        let go = |pm: &mut Pixmap, paint: &Paint| match kind {
            Kind::Fill(rule) => pm.fill_path(path, paint, rule, ts, gs.clip.as_deref()),
            Kind::Stroke => pm.stroke_path(path, paint, &stroke, ts, gs.clip.as_deref()),
        };
        match src {
            Paint3::Solid(c) => {
                let mut p = Paint::default();
                p.set_color_rgba8(c[0], c[1], c[2], (alpha.clamp(0.0, 1.0) * 255.0).round() as u8);
                p.anti_alias = true;
                go(pm, &p);
            }
            Paint3::Pat(pat, col) => {
                let Some(inv) = ts.invert() else { return };
                // The painted area in device pixels.
                let Some(dev) = path.clone().transform(ts) else { return };
                let b = dev.bounds();
                let grow = if matches!(kind, Kind::Stroke) { stroke.width * px_scale(&ts) * stroke.miter_limit.max(1.0) } else { 0.0 };
                let (x0, y0) = ((b.left() - grow).floor().max(0.0) as i32, (b.top() - grow).floor().max(0.0) as i32);
                let (x1, y1) = ((b.right() + grow).ceil().min(pm.width() as f32) as i32, (b.bottom() + grow).ceil().min(pm.height() as f32) as i32);
                if x1 <= x0 || y1 <= y0 {
                    return;
                }
                match &**pat {
                    Pat::Shade { sh, matrix } => {
                        let to_dev = matrix.post_concat(base);
                        let Some(layer) = sh.paint((x1 - x0) as u32, (y1 - y0) as u32, (x0, y0), to_dev, true, &mut self.px) else { return };
                        let shader = Pattern::new(
                            layer.as_ref(),
                            SpreadMode::Pad,
                            FilterQuality::Nearest,
                            alpha,
                            Transform::from_translate(x0 as f32, y0 as f32).post_concat(inv),
                        );
                        let p = Paint { shader, anti_alias: true, ..Paint::default() };
                        go(pm, &p);
                    }
                    Pat::Tile { content, bbox, xstep, ystep, matrix, uncolored, res, cell } => {
                        if depth >= MAX_NEST {
                            return;
                        }
                        let to_dev = matrix.post_concat(base);
                        let cached = cell.borrow().as_ref().filter(|(b, c, _)| *b == base && (!uncolored || c == col)).map(|(_, _, p)| p.clone());
                        let cellpm = match cached {
                            Some(c) => c,
                            None => {
                                let (sx, sy) = (f64::from(to_dev.sx).hypot(f64::from(to_dev.ky)), f64::from(to_dev.kx).hypot(f64::from(to_dev.sy)));
                                let (xs, ys) = (xstep.abs(), ystep.abs());
                                let (cw, ch) = ((xs * sx).ceil().clamp(1.0, MAX_CELL), (ys * sy).ceil().clamp(1.0, MAX_CELL));
                                let Some(mut cp) = Pixmap::new(cw as u32, ch as u32) else { return };
                                let ctm = Transform::from_translate(-bbox[0] as f32, -bbox[1] as f32)
                                    .post_concat(Transform::from_scale((cw / xs) as f32, (ch / ys) as f32));
                                let mut st = State::new(ctm);
                                if *uncolored {
                                    st.fill = Paint3::Solid(*col);
                                    st.stroke = Paint3::Solid(*col);
                                }
                                self.run_locked(&mut cp, content, res, st, ctm, depth + 1, *uncolored);
                                let rc = Rc::new(cp);
                                *cell.borrow_mut() = Some((base, *col, rc.clone()));
                                rc
                            }
                        };
                        let (cw, ch) = (cellpm.width() as f64, cellpm.height() as f64);
                        let to_pat =
                            Transform::from_row((xstep.abs() / cw) as f32, 0.0, 0.0, (ystep.abs() / ch) as f32, bbox[0] as f32, bbox[1] as f32);
                        let shader = Pattern::new(
                            (*cellpm).as_ref(),
                            SpreadMode::Repeat,
                            FilterQuality::Bilinear,
                            alpha,
                            to_pat.post_concat(to_dev).post_concat(inv),
                        );
                        let p = Paint { shader, anti_alias: true, ..Paint::default() };
                        go(pm, &p);
                    }
                }
            }
        }
    }

    /// Intersects the clip with `path` (under `ts`).
    fn clip(&mut self, pm: &Pixmap, gs: &mut State, path: &Path, ts: Transform, rule: FillRule) {
        if self.clips >= MAX_CLIPS {
            return;
        }
        self.clips += 1;
        let mut m = match gs.clip.as_deref() {
            Some(m) => m.clone(),
            None => {
                let Some(mut m) = Mask::new(pm.width(), pm.height()) else { return };
                m.data_mut().fill(255);
                m
            }
        };
        m.intersect_path(path, rule, true, ts);
        gs.clip = Some(Rc::new(m));
    }

    fn draw_image(&mut self, pm: &mut Pixmap, gs: &State, img: &Img) {
        let ctm = gs.ctm;
        let (lx, ly) = (f64::from(ctm.sx).hypot(f64::from(ctm.ky)), f64::from(ctm.kx).hypot(f64::from(ctm.sy)));
        if !(lx.is_finite() && ly.is_finite()) || lx < 1e-6 || ly < 1e-6 {
            return;
        }
        let (kx, ky) = (((f64::from(img.w) / lx).floor() as usize).clamp(1, 32), ((f64::from(img.h) / ly).floor() as usize).clamp(1, 32));
        let (ow, oh) = (img.w as usize / kx, img.h as usize / ky);
        if ow == 0 || oh == 0 {
            return;
        }
        let colour = match &gs.fill {
            Paint3::Solid(c) | Paint3::Pat(_, c) => *c,
        };
        // Premultiplied, box-reduced pixels.
        let mut data = Vec::with_capacity(ow * oh * 4);
        for y in 0..oh {
            for x in 0..ow {
                let mut acc = [0u32; 4];
                for dy in 0..ky {
                    for dx in 0..kx {
                        let at = ((y * ky + dy) * img.w as usize + x * kx + dx) * 4;
                        let Some(p) = img.rgba.get(at..at + 4) else { continue };
                        let a = u32::from(p[3]);
                        let c = if img.stencil { colour } else { [p[0], p[1], p[2]] };
                        acc[0] += u32::from(c[0]) * a;
                        acc[1] += u32::from(c[1]) * a;
                        acc[2] += u32::from(c[2]) * a;
                        acc[3] += a;
                    }
                }
                let n = (kx * ky) as u32;
                let div = |v: u32| (v / (255 * n)) as u8;
                data.extend([div(acc[0]), div(acc[1]), div(acc[2]), (acc[3] / n) as u8]);
            }
        }
        let Some(size) = tiny_skia::IntSize::from_wh(ow as u32, oh as u32) else { return };
        let Some(src) = Pixmap::from_vec(data, size) else { return };
        let shader = Pattern::new(
            src.as_ref(),
            SpreadMode::Pad,
            FilterQuality::Bilinear,
            gs.fa,
            Transform::from_row(1.0 / ow as f32, 0.0, 0.0, -1.0 / oh as f32, 0.0, 1.0),
        );
        let paint = Paint { shader, anti_alias: true, ..Paint::default() };
        if let Some(r) = Rect::from_xywh(0.0, 0.0, 1.0, 1.0) {
            pm.fill_rect(r, &paint, ctm, gs.clip.as_deref());
        }
    }

    // ---- resources ----

    fn font(&mut self, res: &Dict, name: &str) -> Option<Rc<Font>> {
        let o = self.resource(res, "Font", name)?;
        let key = if let Obj::Ref(n) = &o { Some(*n) } else { None };
        if let Some(k) = key
            && let Some(f) = self.fonts.get(&k)
        {
            return f.clone();
        }
        if self.font_loads >= MAX_FONT_LOADS {
            return None;
        }
        self.font_loads += 1;
        self.next_font += 1;
        let id = self.next_font;
        let loaded = match self.pdf.resolve(&o) {
            Obj::Dict(d) => Font::load(self.pdf, &d, id).map(Rc::new),
            _ => None,
        };
        if let Some(k) = key {
            self.fonts.insert(k, loaded.clone());
        }
        loaded
    }

    fn pattern(&mut self, res: &Dict, name: &str) -> Option<Rc<Pat>> {
        let o = self.resource(res, "Pattern", name)?;
        let key = if let Obj::Ref(n) = &o { Some(*n) } else { None };
        if let Some(k) = key
            && let Some(p) = self.pats.get(&k)
        {
            return p.clone();
        }
        let loaded = self.load_pattern(&o, res).map(Rc::new);
        if let Some(k) = key {
            self.pats.insert(k, loaded.clone());
        }
        loaded
    }

    fn load_pattern(&mut self, o: &Obj, res: &Dict) -> Option<Pat> {
        match self.pdf.resolve(o) {
            Obj::Dict(d) if self.pdf.num_of(&d, "PatternType") == Some(2.0) => {
                let sh = shade::load(self.pdf, d.get("Shading")?, res, &mut self.notes)?;
                let matrix = matrix(self.pdf.nums_of(&d, "Matrix"));
                Some(Pat::Shade { sh, matrix })
            }
            Obj::Stream(d, a, b, id) if self.pdf.num_of(&d, "PatternType") == Some(1.0) => {
                let content = self.pdf.decode(&d, a, b, id)?;
                let bb = self.pdf.nums_of(&d, "BBox").filter(|v| v.len() == 4)?;
                let (xstep, ystep) = (self.pdf.num_of(&d, "XStep")?, self.pdf.num_of(&d, "YStep")?);
                if !(xstep.is_finite() && ystep.is_finite()) || xstep.abs() < 1e-6 || ystep.abs() < 1e-6 {
                    return None;
                }
                let res = match self.pdf.val(&d, "Resources") {
                    Obj::Dict(r) => r,
                    _ => res.clone(),
                };
                let uncolored = self.pdf.num_of(&d, "PaintType") == Some(2.0);
                let matrix = matrix(self.pdf.nums_of(&d, "Matrix"));
                Some(Pat::Tile {
                    content,
                    bbox: [bb[0].min(bb[2]), bb[1].min(bb[3]), bb[0].max(bb[2]), bb[1].max(bb[3])],
                    xstep,
                    ystep,
                    matrix,
                    uncolored,
                    res,
                    cell: RefCell::new(None),
                })
            }
            _ => None,
        }
    }

    /// A glyph procedure or other content stream, decoded once.
    fn proc_stream(&mut self, o: &Obj) -> Option<Rc<Vec<u8>>> {
        let key = if let Obj::Ref(n) = o { Some(*n) } else { None };
        if let Some(k) = key
            && let Some(p) = self.procs.get(&k)
        {
            return p.clone();
        }
        let data = match self.pdf.resolve(o) {
            Obj::Stream(d, a, b, id) => self.pdf.decode(&d, a, b, id).map(Rc::new),
            _ => None,
        };
        if let Some(k) = key {
            self.procs.insert(k, data.clone());
        }
        data
    }

    // ---- XObjects ----

    fn do_xobject(&mut self, pm: &mut Pixmap, gs: &State, res: &Dict, name: &str, depth: usize) {
        let Some(o) = self.resource(res, "XObject", name) else { return };
        let key = if let Obj::Ref(n) = &o { Some(*n) } else { None };
        let Obj::Stream(d, a, b, id) = self.pdf.resolve(&o) else { return };
        match self.pdf.val(&d, "Subtype") {
            Obj::Name(n) if n == "Form" => {
                if depth >= MAX_NEST || self.forms >= MAX_FORMS || key.is_some_and(|k| self.active.contains(&k)) {
                    return;
                }
                self.forms += 1;
                let Some(data) = self.proc_stream(&o) else { return };
                let mut st = gs.clone();
                st.ctm = matrix(self.pdf.nums_of(&d, "Matrix")).post_concat(gs.ctm);
                if let Some(bb) = self.pdf.nums_of(&d, "BBox").filter(|v| v.len() == 4)
                    && let Some(r) =
                        Rect::from_ltrb(bb[0].min(bb[2]) as f32, bb[1].min(bb[3]) as f32, bb[0].max(bb[2]) as f32, bb[1].max(bb[3]) as f32)
                {
                    let ctm = st.ctm;
                    self.clip(pm, &mut st, &PathBuilder::from_rect(r), ctm, FillRule::Winding);
                }
                let fres = match self.pdf.val(&d, "Resources") {
                    Obj::Dict(r) => r,
                    _ => res.clone(),
                };
                let base = st.ctm;
                self.active.extend(key);
                self.run(pm, &data, &fres, st, base, depth + 1);
                if key.is_some() {
                    self.active.pop();
                }
            }
            Obj::Name(n) if n == "Image" => {
                let cached = key.and_then(|k| self.imgs.get(&k).cloned());
                let img = match cached {
                    Some(i) => i,
                    None => {
                        let raw = self.pdf.data.get(a..b).map(<[u8]>::to_vec);
                        let img = raw.and_then(|raw| self.pdf.decode_raw(&d, raw, Some(id))).and_then(|(data, pending)| {
                            let mut env = image::Env { pdf: self.pdf, budget: &mut self.px, notes: &mut self.notes };
                            image::load(&mut env, &d, data, pending, res, 0)
                        });
                        let img = img.map(Rc::new);
                        if let Some(k) = key {
                            self.imgs.insert(k, img.clone());
                        }
                        img
                    }
                };
                if let Some(img) = img {
                    self.draw_image(pm, gs, &img);
                }
            }
            _ => {}
        }
    }

    /// `BI … ID … EI`: the image at the lexer's position, drawn.
    fn inline_image(&mut self, pm: &mut Pixmap, gs: &State, lex: &mut Lex, res: &Dict) {
        let mut d = Dict::new();
        loop {
            lex.skip_ws();
            if lex.starts(b"ID") {
                break;
            }
            let before = lex.p;
            let Some(Obj::Name(k)) = lex.obj(0) else {
                if lex.p == before {
                    lex.p += 1;
                }
                if lex.p >= lex.d.len() {
                    return;
                }
                continue;
            };
            let Some(v) = lex.obj(0) else { return };
            let k = match k.as_str() {
                "BPC" => "BitsPerComponent",
                "CS" => "ColorSpace",
                "D" => "Decode",
                "DP" => "DecodeParms",
                "F" => "Filter",
                "H" => "Height",
                "IM" => "ImageMask",
                "W" => "Width",
                other => other,
            };
            if d.len() < 32 {
                d.insert(k.to_string(), v);
            }
        }
        lex.p += 2;
        if lex.peek().is_some_and(|b| matches!(b, 9 | 10 | 12 | 13 | 32)) {
            lex.p += 1;
        }
        let start = lex.p;
        // The data ends at an `EI` between whitespace that is followed by plausible content.
        let mut end = None;
        let mut i = start;
        while let Some(pos) = lex.d.get(i..).and_then(|r| r.windows(2).position(|w| w == b"EI")).map(|p| p + i) {
            let before_ok = pos == start || lex.d.get(pos - 1).is_some_and(|b| matches!(b, 0 | 9 | 10 | 12 | 13 | 32));
            let after = lex.d.get(pos + 2..).unwrap_or_default();
            let after_ok = after.first().is_none_or(|b| matches!(b, 0 | 9 | 10 | 12 | 13 | 32))
                && after.iter().take(12).all(|b| b.is_ascii_graphic() || matches!(b, 9 | 10 | 13 | 32));
            if before_ok && after_ok {
                end = Some(pos);
                break;
            }
            i = pos + 2;
        }
        let Some(end) = end else {
            lex.p = lex.d.len();
            return;
        };
        lex.p = end + 2;
        let data = lex.d.get(start..end.saturating_sub(usize::from(end > start))).unwrap_or_default().to_vec();
        let Some((raw, pending)) = self.pdf.decode_raw(&d, data, None) else { return };
        let mut env = image::Env { pdf: self.pdf, budget: &mut self.px, notes: &mut self.notes };
        if let Some(img) = image::load(&mut env, &d, raw, pending, res, 0) {
            self.draw_image(pm, gs, &img);
        }
    }

    // ---- text ----

    fn show(&mut self, pm: &mut Pixmap, gs: &State, tm: &mut M, s: &[u8], res: &Dict, base: Transform, depth: usize) {
        let Some(font) = gs.font.clone() else { return };
        for ch in font.chars(s) {
            self.glyphs += 1;
            if self.glyphs > MAX_GLYPHS {
                return;
            }
            let fs = gs.fsize;
            // Em space → text space → user space.
            let mut glyph_m = mmul([fs * gs.th, 0.0, 0.0, fs, 0.0, gs.rise], *tm);
            let shape = if font.t3.is_none() {
                let key = (font.id, ch.key);
                let cached = self.shapes.get(&key).cloned();
                match cached {
                    Some(s) => s,
                    None => {
                        let s = font.shape(&ch).map(Rc::new);
                        self.shapes.insert(key, s.clone());
                        s
                    }
                }
            } else {
                None
            };
            let w = ch.w.or_else(|| shape.as_ref().map(|s| s.adv)).unwrap_or(0.0);
            if font.vertical {
                // The glyph's origin is shifted by the position vector (w0 / 2, DW2 y).
                glyph_m = mmul([1.0, 0.0, 0.0, 1.0, -w / 2.0, -font.dw2.0 / 1000.0], glyph_m);
            } else if let (font::Src::Std(_), Some(cw), Some(sh)) = (&font.src, ch.w, &shape)
                && sh.adv > 0.0
            {
                // A substitute font is squeezed or stretched to the declared width.
                let sx = (cw / sh.adv).clamp(0.6, 1.6);
                glyph_m = mmul([sx, 0.0, 0.0, 1.0, 0.0, 0.0], glyph_m);
            }
            if gs.mode & 3 != 3 {
                if let Some(t3) = &font.t3 {
                    self.type3_glyph(pm, gs, &font, t3, &ch, glyph_m, res, base, depth);
                } else if let Some(shape) = &shape {
                    self.draw_shape(pm, gs, shape, glyph_m, base, depth);
                }
            }
            let (mut tx, mut ty) = (0.0, 0.0);
            let extra = gs.tc + if ch.n == 1 && ch.code == 32 { gs.tw } else { 0.0 };
            if font.vertical {
                ty = font.dw2.1 / 1000.0 * fs + extra;
            } else {
                tx = (w * fs + extra) * gs.th;
            }
            *tm = mmul([1.0, 0.0, 0.0, 1.0, tx, ty], *tm);
        }
    }

    fn draw_shape(&mut self, pm: &mut Pixmap, gs: &State, shape: &Shape, glyph_m: M, base: Transform, depth: usize) {
        let to_user = ts_of(glyph_m);
        let full = to_user.post_concat(gs.ctm);
        let mode = gs.mode & 3;
        if let Some(p) = &shape.fill {
            if mode == 0 || mode == 2 {
                self.draw(pm, gs, p, full, Kind::Fill(shape.rule), base, depth);
            }
            if (mode == 1 || mode == 2)
                && let Some(up) = p.clone().transform(to_user)
            {
                let ctm = gs.ctm;
                self.draw(pm, gs, &up, ctm, Kind::Stroke, base, depth);
            }
        }
        if let Some(l) = &shape.line {
            // Stroke-font glyphs are lines: drawn in the fill colour, 6 % of an em wide.
            let mut sg = gs.clone();
            sg.stroke = gs.fill.clone();
            sg.sa = gs.fa;
            sg.width = 0.06;
            sg.cap = LineCap::Round;
            sg.join = LineJoin::Round;
            sg.dash = None;
            self.draw(pm, &sg, l, full, Kind::Stroke, base, depth);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn type3_glyph(
        &mut self,
        pm: &mut Pixmap,
        gs: &State,
        font: &Font,
        t3: &font::Type3,
        ch: &font::Ch,
        glyph_m: M,
        res: &Dict,
        base: Transform,
        depth: usize,
    ) {
        if depth >= MAX_NEST || self.forms >= MAX_FORMS {
            return;
        }
        let Some(name) = font.enc.names.get(ch.code as usize).and_then(|n| n.as_deref()) else { return };
        let Some(po) = t3.procs.get(name).cloned() else { return };
        let key = if let Obj::Ref(n) = &po { Some(*n) } else { None };
        if key.is_some_and(|k| self.active.contains(&k)) {
            return;
        }
        let Some(content) = self.proc_stream(&po) else { return };
        self.forms += 1;
        let mut st = gs.clone();
        st.ctm = ts_of(mmul(t3.matrix, glyph_m)).post_concat(gs.ctm);
        st.font = None;
        let gres = t3.resources.clone().unwrap_or_else(|| res.clone());
        self.active.extend(key);
        let b = st.ctm;
        let _ = base;
        self.run(pm, &content, &gres, st, b, depth + 1);
        if key.is_some() {
            self.active.pop();
        }
    }

    // ---- the interpreter ----

    fn run(&mut self, pm: &mut Pixmap, content: &[u8], res: &Dict, gs: State, base: Transform, depth: usize) {
        self.run_locked(pm, content, res, gs, base, depth, false);
    }

    /// Runs a content stream. `lock` ignores colour operators (uncoloured patterns).
    #[allow(clippy::too_many_arguments)]
    fn run_locked(&mut self, pm: &mut Pixmap, content: &[u8], res: &Dict, mut gs: State, base: Transform, depth: usize, mut lock: bool) {
        let mut lex = Lex { d: content, p: 0 };
        let mut st: Vec<Obj> = Vec::new();
        let mut saved: Vec<State> = Vec::new();
        let mut pb = PathBuilder::new();
        let mut segments = 0usize;
        let mut cur = (0.0f32, 0.0f32);
        let mut pending_clip: Option<FillRule> = None;
        let (mut tm, mut tlm) = (IDENT, IDENT);
        loop {
            lex.skip_ws();
            let Some(b) = lex.peek() else { break };
            let before = lex.p;
            let op: Vec<u8> = if b.is_ascii_alphabetic() || matches!(b, b'\'' | b'"') {
                let w = lex.word();
                match w {
                    b"true" => {
                        st.push(Obj::Bool(true));
                        continue;
                    }
                    b"false" => {
                        st.push(Obj::Bool(false));
                        continue;
                    }
                    b"null" => {
                        st.push(Obj::Null);
                        continue;
                    }
                    _ => w.to_vec(),
                }
            } else {
                match lex.obj(0) {
                    Some(o) => {
                        if st.len() >= MAX_OPERANDS {
                            st.clear();
                        }
                        st.push(o);
                    }
                    None if lex.p == before => lex.p += 1,
                    None => {}
                }
                continue;
            };
            self.ops += 1;
            if self.ops > MAX_OPS {
                return;
            }
            let f = |i: usize, n: &[f64]| n.get(i).copied().unwrap_or(0.0) as f32;
            match op.as_slice() {
                b"q" => {
                    if saved.len() < MAX_DEPTH {
                        saved.push(gs.clone());
                    }
                }
                b"Q" => {
                    if let Some(s) = saved.pop() {
                        gs = s;
                    }
                }
                b"cm" => {
                    if let Some(n) = last::<6>(&st) {
                        let m = Transform::from_row(f(0, &n), f(1, &n), f(2, &n), f(3, &n), f(4, &n), f(5, &n));
                        if m.is_finite() {
                            gs.ctm = m.post_concat(gs.ctm);
                        }
                    }
                }
                b"w" => {
                    if let Some([w]) = last::<1>(&st) {
                        gs.width = w.abs() as f32;
                    }
                }
                b"J" => {
                    if let Some([v]) = last::<1>(&st) {
                        gs.cap = match v as i64 {
                            1 => LineCap::Round,
                            2 => LineCap::Square,
                            _ => LineCap::Butt,
                        };
                    }
                }
                b"j" => {
                    if let Some([v]) = last::<1>(&st) {
                        gs.join = match v as i64 {
                            1 => LineJoin::Round,
                            2 => LineJoin::Bevel,
                            _ => LineJoin::Miter,
                        };
                    }
                }
                b"M" => {
                    if let Some([v]) = last::<1>(&st) {
                        gs.miter = v as f32;
                    }
                }
                b"d" => {
                    if let [.., Obj::Arr(a), Obj::Num(phase)] = st.as_slice() {
                        gs.dash = dash_of(a, *phase);
                    }
                }
                b"gs" => {
                    if let Some(Obj::Name(n)) = st.last()
                        && let Some(o) = self.resource(res, "ExtGState", n)
                        && let Obj::Dict(d) = self.pdf.resolve(&o)
                    {
                        self.ext_gstate(&d, &mut gs);
                    }
                }
                b"g" | b"G" | b"rg" | b"RG" | b"k" | b"K" if !lock => {
                    let fill = op[0].is_ascii_lowercase();
                    let (cs, col) = match op.as_slice() {
                        b"g" | b"G" => (Cs::Gray, last::<1>(&st).map(|[v]| gray(v))),
                        b"rg" | b"RG" => (Cs::Rgb, last::<3>(&st).map(|[r, g, b]| rgb(r, g, b))),
                        _ => (Cs::Cmyk, last::<4>(&st).map(|[c, m, y, k]| cmyk(c, m, y, k))),
                    };
                    if let Some(c) = col {
                        if fill {
                            gs.fill_cs = Rc::new(cs);
                            gs.fill = Paint3::Solid(c);
                        } else {
                            gs.stroke_cs = Rc::new(cs);
                            gs.stroke = Paint3::Solid(c);
                        }
                    }
                }
                b"cs" | b"CS" if !lock => {
                    if let Some(o) = st.last()
                        && let Some(cs) = Cs::load(self.pdf, o, res, 0)
                    {
                        let c = Paint3::Solid(cs.rgb(&cs.initial()));
                        if op.as_slice() == b"cs" {
                            gs.fill = c;
                            gs.fill_cs = cs;
                        } else {
                            gs.stroke = c;
                            gs.stroke_cs = cs;
                        }
                    }
                }
                b"sc" | b"scn" | b"SC" | b"SCN" if !lock => {
                    let fill = op[0].is_ascii_lowercase();
                    let cs = if fill { gs.fill_cs.clone() } else { gs.stroke_cs.clone() };
                    let nums = numbers(&st);
                    let paint = match (&*cs, st.last()) {
                        (Cs::Pattern(under), Some(Obj::Name(n))) => {
                            let col = under.as_ref().map_or([0; 3], |u| u.rgb(&nums));
                            self.pattern(res, n).map(|p| Paint3::Pat(p, col))
                        }
                        (Cs::Pattern(_), _) => None,
                        (cs, _) => Some(Paint3::Solid(cs.rgb(&nums))),
                    };
                    if let Some(p) = paint {
                        if fill {
                            gs.fill = p;
                        } else {
                            gs.stroke = p;
                        }
                    }
                }
                b"m" | b"l" | b"c" | b"v" | b"y" | b"re" if segments < MAX_PATH_SEGMENTS => {
                    segments += 1;
                    let n: Vec<f64> = match op.as_slice() {
                        b"m" | b"l" => last::<2>(&st).map(|a| a.to_vec()),
                        b"v" | b"y" | b"re" => last::<4>(&st).map(|a| a.to_vec()),
                        _ => last::<6>(&st).map(|a| a.to_vec()),
                    }
                    .unwrap_or_default();
                    if !n.is_empty() {
                        match op.as_slice() {
                            b"m" => {
                                pb.move_to(f(0, &n), f(1, &n));
                                cur = (f(0, &n), f(1, &n));
                            }
                            b"l" => {
                                pb.line_to(f(0, &n), f(1, &n));
                                cur = (f(0, &n), f(1, &n));
                            }
                            b"c" => {
                                pb.cubic_to(f(0, &n), f(1, &n), f(2, &n), f(3, &n), f(4, &n), f(5, &n));
                                cur = (f(4, &n), f(5, &n));
                            }
                            b"v" => {
                                pb.cubic_to(cur.0, cur.1, f(0, &n), f(1, &n), f(2, &n), f(3, &n));
                                cur = (f(2, &n), f(3, &n));
                            }
                            b"y" => {
                                pb.cubic_to(f(0, &n), f(1, &n), f(2, &n), f(3, &n), f(2, &n), f(3, &n));
                                cur = (f(2, &n), f(3, &n));
                            }
                            _ => {
                                let (x, y, w, h) = (f(0, &n), f(1, &n), f(2, &n), f(3, &n));
                                pb.move_to(x, y);
                                pb.line_to(x + w, y);
                                pb.line_to(x + w, y + h);
                                pb.line_to(x, y + h);
                                pb.close();
                                cur = (x, y);
                            }
                        }
                    }
                }
                b"h" => pb.close(),
                b"W" => pending_clip = Some(FillRule::Winding),
                b"W*" => pending_clip = Some(FillRule::EvenOdd),
                b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n" => {
                    if matches!(op.as_slice(), b"s" | b"b" | b"b*") {
                        pb.close();
                    }
                    let fill = match op.as_slice() {
                        b"f" | b"F" | b"B" | b"b" => Some(FillRule::Winding),
                        b"f*" | b"B*" | b"b*" => Some(FillRule::EvenOdd),
                        _ => None,
                    };
                    let stroke = matches!(op.as_slice(), b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*");
                    if let Some(path) = std::mem::take(&mut pb).finish() {
                        if let Some(rule) = pending_clip.take() {
                            let ctm = gs.ctm;
                            self.clip(pm, &mut gs, &path, ctm, rule);
                        }
                        let ctm = gs.ctm;
                        if let Some(rule) = fill {
                            self.draw(pm, &gs, &path, ctm, Kind::Fill(rule), base, depth);
                        }
                        if stroke {
                            self.draw(pm, &gs, &path, ctm, Kind::Stroke, base, depth);
                        }
                    }
                    pending_clip = None;
                    segments = 0;
                }
                b"sh" => {
                    if let Some(Obj::Name(n)) = st.last()
                        && let Some(o) = self.resource(res, "Shading", n)
                        && let Some(sh) = shade::load(self.pdf, &o, res, &mut self.notes)
                        && let Some(layer) = sh.paint(pm.width(), pm.height(), (0, 0), gs.ctm, false, &mut self.px)
                    {
                        let paint = tiny_skia::PixmapPaint { opacity: gs.fa, ..tiny_skia::PixmapPaint::default() };
                        pm.draw_pixmap(0, 0, layer.as_ref(), &paint, Transform::identity(), gs.clip.as_deref());
                    }
                }
                b"Do" => {
                    if let Some(Obj::Name(n)) = st.last() {
                        let n = n.clone();
                        self.do_xobject(pm, &gs, res, &n, depth);
                    }
                }
                b"BI" => self.inline_image(pm, &gs, &mut lex, res),
                b"d0" => {}
                b"d1" => lock = true,
                // ---- text ----
                b"BT" => {
                    tm = IDENT;
                    tlm = IDENT;
                }
                b"Tc" => {
                    if let Some([v]) = last::<1>(&st) {
                        gs.tc = v;
                    }
                }
                b"Tw" => {
                    if let Some([v]) = last::<1>(&st) {
                        gs.tw = v;
                    }
                }
                b"Tz" => {
                    if let Some([v]) = last::<1>(&st) {
                        gs.th = v / 100.0;
                    }
                }
                b"TL" => {
                    if let Some([v]) = last::<1>(&st) {
                        gs.tl = v;
                    }
                }
                b"Ts" => {
                    if let Some([v]) = last::<1>(&st) {
                        gs.rise = v;
                    }
                }
                b"Tr" => {
                    if let Some([v]) = last::<1>(&st) {
                        gs.mode = v as i64;
                    }
                }
                b"Tf" => {
                    if let [.., Obj::Name(n), Obj::Num(size)] = st.as_slice() {
                        let (n, size) = (n.clone(), *size);
                        gs.font = self.font(res, &n);
                        gs.fsize = size;
                    }
                }
                b"Td" | b"TD" => {
                    if let Some([x, y]) = last::<2>(&st) {
                        if op.as_slice() == b"TD" {
                            gs.tl = -y;
                        }
                        tlm = mmul([1.0, 0.0, 0.0, 1.0, x, y], tlm);
                        tm = tlm;
                    }
                }
                b"Tm" => {
                    if let Some(m) = last::<6>(&st) {
                        tlm = m;
                        tm = m;
                    }
                }
                b"T*" => {
                    tlm = mmul([1.0, 0.0, 0.0, 1.0, 0.0, -gs.tl], tlm);
                    tm = tlm;
                }
                b"Tj" | b"'" | b"\"" => {
                    if op.as_slice() != b"Tj" {
                        if op.as_slice() == b"\""
                            && let Some([aw, ac]) = aw_ac(&st)
                        {
                            gs.tw = aw;
                            gs.tc = ac;
                        }
                        tlm = mmul([1.0, 0.0, 0.0, 1.0, 0.0, -gs.tl], tlm);
                        tm = tlm;
                    }
                    if let Some(Obj::Str(s)) = st.last() {
                        let s = s.clone();
                        self.show(pm, &gs, &mut tm, &s, res, base, depth);
                    }
                }
                b"TJ" => {
                    if let Some(Obj::Arr(a)) = st.last() {
                        let a = a.clone();
                        for e in &a {
                            match e {
                                Obj::Str(s) => self.show(pm, &gs, &mut tm, s, res, base, depth),
                                Obj::Num(n) => {
                                    let v = -n / 1000.0 * gs.fsize;
                                    let vertical = gs.font.as_ref().is_some_and(|f| f.vertical);
                                    tm = if vertical {
                                        mmul([1.0, 0.0, 0.0, 1.0, 0.0, v], tm)
                                    } else {
                                        mmul([1.0, 0.0, 0.0, 1.0, v * gs.th, 0.0], tm)
                                    };
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
            st.clear();
        }
    }

    fn ext_gstate(&mut self, d: &Dict, gs: &mut State) {
        if let Some(v) = self.pdf.num_of(d, "LW") {
            gs.width = v.abs() as f32;
        }
        if let Some(v) = self.pdf.num_of(d, "LC") {
            gs.cap = match v as i64 {
                1 => LineCap::Round,
                2 => LineCap::Square,
                _ => LineCap::Butt,
            };
        }
        if let Some(v) = self.pdf.num_of(d, "LJ") {
            gs.join = match v as i64 {
                1 => LineJoin::Round,
                2 => LineJoin::Bevel,
                _ => LineJoin::Miter,
            };
        }
        if let Some(v) = self.pdf.num_of(d, "ML") {
            gs.miter = v as f32;
        }
        if let Obj::Arr(a) = self.pdf.val(d, "D")
            && let (Some(Obj::Arr(arr)), Some(Obj::Num(phase))) = (a.first().map(|o| self.pdf.resolve(o)), a.get(1).map(|o| self.pdf.resolve(o)))
        {
            let nums: Vec<Obj> = arr.iter().map(|o| self.pdf.resolve(o)).collect();
            gs.dash = dash_of(&nums, phase);
        }
        if let Some(v) = self.pdf.num_of(d, "CA") {
            gs.sa = v.clamp(0.0, 1.0) as f32;
        }
        if let Some(v) = self.pdf.num_of(d, "ca") {
            gs.fa = v.clamp(0.0, 1.0) as f32;
        }
    }
}

fn dash_of(a: &[Obj], phase: f64) -> Option<(Vec<f32>, f32)> {
    let v: Vec<f32> = a.iter().filter_map(|o| if let Obj::Num(n) = o { Some(*n as f32) } else { None }).collect();
    (!v.is_empty() && v.iter().all(|x| *x >= 0.0 && x.is_finite()) && v.iter().sum::<f32>() > 0.0).then_some((v, phase as f32))
}

/// `aw ac string "`: the two numbers before the string.
fn aw_ac(st: &[Obj]) -> Option<[f64; 2]> {
    last::<2>(st.get(..st.len().checked_sub(1)?)?)
}
