//! PDF pages as pictures, for PDF underlays (PDFATTACH): the vector content of one page drawn
//! with tiny-skia at the resolution it is shown at.
//!
//! Our own interpreter (clean-room, from the public PDF content stream syntax): paths (`m l c v
//! y h re`) filled and stroked (`f F f* B B* b b* S s`), clipping (`W W*`), the graphics state
//! (`q Q cm w J j M d`) and device colours (`g G rg RG k K` and `sc scn SC SCN` with 1, 3 or 4
//! components). A white page is the background.
//!
//! Not drawn (ponytail: add when a real drawing needs it): text, embedded images, shadings and
//! patterns, form XObjects (`Do`), transparency (`gs`), non-device colour spaces (their
//! components are read as gray, RGB or CMYK by count). The content is hostile input: operand
//! counts, graphics-state depth, path size, clip count and operator count are capped, and every
//! malformed operator is skipped.

use cadcraft_render::images::{MAX_PAGE_PX, MIN_PAGE_PX, Raster};
use tiny_skia::{FillRule, LineCap, LineJoin, Mask, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform};

use crate::pdf_markup::{PdfPageContent, pdf_page_content};

const MAX_OPS: usize = 5_000_000;
const MAX_OPERANDS: usize = 64;
const MAX_DEPTH: usize = 128;
const MAX_PATH_SEGMENTS: usize = 2_000_000;
const MAX_CLIPS: usize = 2_000;

/// The displayed size of page `page` in points.
pub fn page_size(bytes: &[u8], page: u32) -> Result<(f64, f64), String> {
    let c = content(bytes, page)?;
    Ok(displayed(&c))
}

fn content(bytes: &[u8], page: u32) -> Result<PdfPageContent, String> {
    pdf_page_content(bytes, page as usize, "").map_err(|e| e.to_string())
}

fn displayed(c: &PdfPageContent) -> (f64, f64) {
    if matches!(c.rotate, 90 | 270) { (c.h, c.w) } else { (c.w, c.h) }
}

/// Page `page` drawn with its long side `max_dim` pixels (clamped to the picture limits).
pub fn render(bytes: &[u8], page: u32, max_dim: u32) -> Result<Raster, String> {
    let c = content(bytes, page)?;
    let (dw, dh) = displayed(&c);
    if !(dw.is_finite() && dh.is_finite() && dw > 0.0 && dh > 0.0) {
        return Err("the page has no size".into());
    }
    let long = f64::from(max_dim.clamp(MIN_PAGE_PX, MAX_PAGE_PX));
    let s = long / dw.max(dh);
    let (pw, ph) = (((dw * s).ceil() as u32).clamp(1, MAX_PAGE_PX), ((dh * s).ceil() as u32).clamp(1, MAX_PAGE_PX));
    let mut pm = Pixmap::new(pw, ph).ok_or("the page is too large")?;
    pm.fill(tiny_skia::Color::WHITE);
    // User space → displayed points (origin lower left, y up) → pixels (y down).
    let (w, h) = (c.w, c.h);
    let (a, b, cc, d, e, f) = match c.rotate {
        90 => (0.0, -1.0, 1.0, 0.0, -c.y0, w + c.x0),
        180 => (-1.0, 0.0, 0.0, -1.0, w + c.x0, h + c.y0),
        270 => (0.0, 1.0, -1.0, 0.0, h + c.y0, -c.x0),
        _ => (1.0, 0.0, 0.0, 1.0, -c.x0, -c.y0),
    };
    let to_page = Transform::from_row(a as f32, b as f32, cc as f32, d as f32, e as f32, f as f32);
    let to_px = Transform::from_row(s as f32, 0.0, 0.0, -s as f32, 0.0, (dh * s) as f32);
    run(&mut pm, &c.content, to_page.post_concat(to_px));
    Ok(Raster { width: pw, height: ph, rgba: pm.take() })
}

#[derive(Clone)]
struct State {
    ctm: Transform,
    fill: [u8; 3],
    stroke: [u8; 3],
    width: f32,
    cap: LineCap,
    join: LineJoin,
    miter: f32,
    dash: Option<(Vec<f32>, f32)>,
    clip: Option<std::rc::Rc<Mask>>,
}

enum Operand {
    Num(f64),
    Name,
    Array(Vec<f64>),
    Other,
}

struct Lex<'a> {
    d: &'a [u8],
    p: usize,
}

enum Tok<'a> {
    Operand(Operand),
    Op(&'a [u8]),
}

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delim(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

impl<'a> Lex<'a> {
    fn skip_ws(&mut self) {
        while let Some(&b) = self.d.get(self.p) {
            if is_ws(b) {
                self.p += 1;
            } else if b == b'%' {
                while self.d.get(self.p).is_some_and(|b| !matches!(b, 10 | 13)) {
                    self.p += 1;
                }
            } else {
                break;
            }
        }
    }

    /// A literal string's end, with nesting and escapes.
    fn skip_string(&mut self) {
        let mut depth = 0usize;
        while let Some(&b) = self.d.get(self.p) {
            self.p += 1;
            match b {
                b'\\' => self.p += 1,
                b'(' => depth += 1,
                b')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// An inline image: skip to its `EI` keyword.
    fn skip_inline_image(&mut self) {
        while self.p < self.d.len() {
            if self.d.get(self.p..self.p + 2) == Some(b"EI")
                && self.p.checked_sub(1).and_then(|i| self.d.get(i)).is_some_and(|b| is_ws(*b))
                && self.d.get(self.p + 2).is_none_or(|b| is_ws(*b))
            {
                self.p += 2;
                return;
            }
            self.p += 1;
        }
    }

    fn next(&mut self) -> Option<Tok<'a>> {
        self.skip_ws();
        let b = *self.d.get(self.p)?;
        match b {
            b'/' => {
                self.p += 1;
                while self.d.get(self.p).is_some_and(|b| !is_ws(*b) && !is_delim(*b)) {
                    self.p += 1;
                }
                Some(Tok::Operand(Operand::Name))
            }
            b'(' => {
                self.skip_string();
                Some(Tok::Operand(Operand::Other))
            }
            b'<' if self.d.get(self.p + 1) == Some(&b'<') => {
                // A dictionary: skip to its matching `>>`.
                let mut depth = 0usize;
                while self.p < self.d.len() {
                    match self.d.get(self.p..self.p + 2) {
                        Some(b"<<") => {
                            depth += 1;
                            self.p += 2;
                        }
                        Some(b">>") => {
                            depth = depth.saturating_sub(1);
                            self.p += 2;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => self.p += 1,
                    }
                }
                Some(Tok::Operand(Operand::Other))
            }
            b'<' => {
                while self.d.get(self.p).is_some_and(|b| *b != b'>') {
                    self.p += 1;
                }
                self.p += 1;
                Some(Tok::Operand(Operand::Other))
            }
            b'[' => {
                self.p += 1;
                let mut nums = Vec::new();
                loop {
                    self.skip_ws();
                    match self.d.get(self.p) {
                        None => break,
                        Some(b']') => {
                            self.p += 1;
                            break;
                        }
                        Some(_) => match self.next() {
                            Some(Tok::Operand(Operand::Num(n))) if nums.len() < 1024 => nums.push(n),
                            Some(_) => {}
                            None => break,
                        },
                    }
                }
                Some(Tok::Operand(Operand::Array(nums)))
            }
            b']' | b')' | b'>' | b'{' | b'}' => {
                self.p += 1;
                Some(Tok::Operand(Operand::Other))
            }
            _ => {
                let start = self.p;
                while self.d.get(self.p).is_some_and(|b| !is_ws(*b) && !is_delim(*b)) {
                    self.p += 1;
                }
                let w = self.d.get(start..self.p).unwrap_or(&[]);
                match number(w) {
                    Some(n) => Some(Tok::Operand(Operand::Num(n))),
                    None => Some(Tok::Op(w)),
                }
            }
        }
    }
}

fn number(w: &[u8]) -> Option<f64> {
    if !w.iter().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.')) || w.is_empty() {
        return None;
    }
    std::str::from_utf8(w).ok()?.parse::<f64>().ok().filter(|n| n.is_finite())
}

fn gray(v: f64) -> [u8; 3] {
    let g = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [g, g, g]
}

fn rgb(r: f64, g: f64, b: f64) -> [u8; 3] {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [c(r), c(g), c(b)]
}

fn cmyk(c: f64, m: f64, y: f64, k: f64) -> [u8; 3] {
    let k = 1.0 - k.clamp(0.0, 1.0);
    rgb((1.0 - c.clamp(0.0, 1.0)) * k, (1.0 - m.clamp(0.0, 1.0)) * k, (1.0 - y.clamp(0.0, 1.0)) * k)
}

/// A colour from `sc`-style operands: 1 component gray, 3 RGB, 4 CMYK.
fn by_count(n: &[f64]) -> Option<[u8; 3]> {
    match n {
        [g] => Some(gray(*g)),
        [r, g, b] => Some(rgb(*r, *g, *b)),
        [c, m, y, k] => Some(cmyk(*c, *m, *y, *k)),
        _ => None,
    }
}

/// The trailing numeric operands.
fn tail(stack: &[Operand], n: usize) -> Option<Vec<f64>> {
    let from = stack.len().checked_sub(n)?;
    stack
        .get(from..)?
        .iter()
        .map(|o| match o {
            Operand::Num(v) => Some(*v),
            _ => None,
        })
        .collect()
}

/// Every leading numeric operand (for `sc`/`scn`, which end in an optional pattern name).
fn numbers(stack: &[Operand]) -> Vec<f64> {
    stack.iter().filter_map(|o| if let Operand::Num(v) = o { Some(*v) } else { None }).collect()
}

fn run(pm: &mut Pixmap, content: &[u8], base: Transform) {
    let mut lex = Lex { d: content, p: 0 };
    let mut stack: Vec<Operand> = Vec::new();
    let mut gs =
        State { ctm: base, fill: [0; 3], stroke: [0; 3], width: 1.0, cap: LineCap::Butt, join: LineJoin::Miter, miter: 10.0, dash: None, clip: None };
    let mut saved: Vec<State> = Vec::new();
    let mut pb = PathBuilder::new();
    let mut segments = 0usize;
    let mut cur = (0.0f32, 0.0f32);
    let mut pending_clip: Option<FillRule> = None;
    let mut clips = 0usize;
    let mut ops = 0usize;
    let (pw, ph) = (pm.width(), pm.height());
    // The scale from user space to pixels, for hairlines.
    let px_scale = |t: &Transform| ((t.sx * t.sy - t.kx * t.ky).abs() as f64).sqrt().max(1e-9) as f32;
    while let Some(tok) = lex.next() {
        let op = match tok {
            Tok::Operand(o) => {
                if stack.len() >= MAX_OPERANDS {
                    stack.clear();
                }
                stack.push(o);
                continue;
            }
            Tok::Op(op) => op,
        };
        ops += 1;
        if ops > MAX_OPS {
            return;
        }
        let f = |i: usize, n: &[f64]| n.get(i).copied().unwrap_or(0.0) as f32;
        // Path painting: build the path in user space, draw with the CTM.
        let mut paint_path = |gs: &State, pb: &mut PathBuilder, fill: Option<FillRule>, stroke: bool, close: bool| {
            if close {
                pb.close();
            }
            let path = std::mem::take(pb).finish();
            let Some(path) = path else { return };
            if let Some(rule) = fill {
                let mut p = Paint::default();
                p.set_color_rgba8(gs.fill[0], gs.fill[1], gs.fill[2], 255);
                p.anti_alias = true;
                pm.fill_path(&path, &p, rule, gs.ctm, gs.clip.as_deref());
            }
            if stroke {
                let mut p = Paint::default();
                p.set_color_rgba8(gs.stroke[0], gs.stroke[1], gs.stroke[2], 255);
                p.anti_alias = true;
                let one_px = 1.0 / px_scale(&gs.ctm);
                let st = Stroke {
                    width: gs.width.max(one_px),
                    miter_limit: gs.miter.max(1.0),
                    line_cap: gs.cap,
                    line_join: gs.join,
                    dash: gs.dash.as_ref().and_then(|(a, ph)| StrokeDash::new(a.clone(), *ph)),
                };
                pm.stroke_path(&path, &p, &st, gs.ctm, gs.clip.as_deref());
            }
        };
        match op {
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
                if let Some(n) = tail(&stack, 6) {
                    let m = Transform::from_row(f(0, &n), f(1, &n), f(2, &n), f(3, &n), f(4, &n), f(5, &n));
                    if m.is_finite() {
                        gs.ctm = m.post_concat(gs.ctm);
                    }
                }
            }
            b"w" => {
                if let Some(n) = tail(&stack, 1) {
                    gs.width = f(0, &n).abs();
                }
            }
            b"J" => {
                if let Some(n) = tail(&stack, 1) {
                    gs.cap = match n[0] as i64 {
                        1 => LineCap::Round,
                        2 => LineCap::Square,
                        _ => LineCap::Butt,
                    };
                }
            }
            b"j" => {
                if let Some(n) = tail(&stack, 1) {
                    gs.join = match n[0] as i64 {
                        1 => LineJoin::Round,
                        2 => LineJoin::Bevel,
                        _ => LineJoin::Miter,
                    };
                }
            }
            b"M" => {
                if let Some(n) = tail(&stack, 1) {
                    gs.miter = f(0, &n);
                }
            }
            b"d" => {
                if let [Operand::Array(a), Operand::Num(ph)] = &stack[stack.len().saturating_sub(2)..] {
                    let a: Vec<f32> = a.iter().map(|v| *v as f32).collect();
                    gs.dash = (!a.is_empty() && a.iter().all(|v| *v >= 0.0) && a.iter().sum::<f32>() > 0.0).then_some((a, *ph as f32));
                }
            }
            b"g" => {
                if let Some(n) = tail(&stack, 1) {
                    gs.fill = gray(n[0]);
                }
            }
            b"G" => {
                if let Some(n) = tail(&stack, 1) {
                    gs.stroke = gray(n[0]);
                }
            }
            b"rg" => {
                if let Some(n) = tail(&stack, 3) {
                    gs.fill = rgb(n[0], n[1], n[2]);
                }
            }
            b"RG" => {
                if let Some(n) = tail(&stack, 3) {
                    gs.stroke = rgb(n[0], n[1], n[2]);
                }
            }
            b"k" => {
                if let Some(n) = tail(&stack, 4) {
                    gs.fill = cmyk(n[0], n[1], n[2], n[3]);
                }
            }
            b"K" => {
                if let Some(n) = tail(&stack, 4) {
                    gs.stroke = cmyk(n[0], n[1], n[2], n[3]);
                }
            }
            b"sc" | b"scn" => {
                if let Some(c) = by_count(&numbers(&stack)) {
                    gs.fill = c;
                }
            }
            b"SC" | b"SCN" => {
                if let Some(c) = by_count(&numbers(&stack)) {
                    gs.stroke = c;
                }
            }
            b"m" | b"l" | b"c" | b"v" | b"y" | b"re" if segments < MAX_PATH_SEGMENTS => {
                segments += 1;
                let want = match op {
                    b"m" | b"l" => 2,
                    b"v" | b"y" | b"re" => 4,
                    _ => 6,
                };
                if let Some(n) = tail(&stack, want) {
                    match op {
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
                let close = matches!(op, b"s" | b"b" | b"b*");
                let fill = match op {
                    b"f" | b"F" | b"B" | b"b" => Some(FillRule::Winding),
                    b"f*" | b"B*" | b"b*" => Some(FillRule::EvenOdd),
                    _ => None,
                };
                let stroke = matches!(op, b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*");
                if let Some(rule) = pending_clip.take()
                    && clips < MAX_CLIPS
                {
                    clips += 1;
                    let mut copy = pb.clone();
                    if let Some(path) = std::mem::take(&mut copy).finish().and_then(|p| p.transform(gs.ctm)) {
                        let mut m = match gs.clip.as_deref() {
                            Some(m) => m.clone(),
                            None => {
                                let Some(mut m) = Mask::new(pw, ph) else { return };
                                m.data_mut().fill(255);
                                m
                            }
                        };
                        m.intersect_path(&path, rule, true, Transform::identity());
                        gs.clip = Some(std::rc::Rc::new(m));
                    }
                }
                paint_path(&gs, &mut pb, fill, stroke, close);
                segments = 0;
            }
            b"BI" => lex.skip_inline_image(),
            _ => {}
        }
        stack.clear();
    }
}
