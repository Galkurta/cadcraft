//! Underlays that are not PDFs (DWFx, DWF, DGN V7): each reader turns its file into a
//! [`Scene`] (vector paths, gradients and pictures in points, y down, origin top left) and the
//! scene is drawn with tiny-skia at the resolution the picture cache asks for. A PDF page goes
//! through the same two entry points ([`page_size`], [`render`]), which the picture cache calls
//! with the file's bytes and a 1-based sheet number, so every renderer (screen, PNG, SVG, PDF
//! plot) shows these underlays exactly as it shows a PDF page.
//!
//! The files are hostile input: scenes are capped in size, readers return errors and never
//! panic, and a primitive that can't be drawn is counted in [`Scene::skipped`].

use std::sync::Arc;

use cadcraft_fonts::TextFont;
use cadcraft_render::images::{MAX_PAGE_PX, MIN_PAGE_PX, Raster};
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Mask, Paint, Path, PathBuilder, PathSegment, Pattern, Pixmap, Point,
    RadialGradient, SpreadMode, Stroke, StrokeDash, Transform,
};

/// Most primitives kept in one scene.
pub(crate) const MAX_OPS: usize = 2_000_000;
/// Most clip regions kept in one scene.
pub(crate) const MAX_CLIPS: usize = 8_192;

pub(crate) const V8_ERROR: &str = "DGN V8 is not supported: no public specification";

/// How a path is painted.
#[derive(Clone)]
pub(crate) enum Brush {
    /// Straight RGBA.
    Solid([u8; 4]),
    /// Colour stops (offset, RGBA) along `from`→`to`; `ts` maps the brush's space to the scene.
    Linear { from: (f32, f32), to: (f32, f32), stops: Vec<(f32, [u8; 4])>, ts: Transform },
    /// Colour stops out from `center` to `radius`.
    Radial { center: (f32, f32), radius: f32, stops: Vec<(f32, [u8; 4])>, ts: Transform },
    /// A picture; `ts` maps its pixel space to the scene; `opacity` 0..=1.
    Image { pixmap: Arc<Pixmap>, ts: Transform, opacity: f32 },
}

/// A stroke's look. `width` is in scene units (points).
#[derive(Clone)]
pub(crate) struct StrokeStyle {
    pub brush: Brush,
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter: f32,
    pub dash: Vec<f32>,
    pub phase: f32,
}

impl StrokeStyle {
    pub(crate) fn solid(rgba: [u8; 4], width: f32) -> Self {
        StrokeStyle { brush: Brush::Solid(rgba), width, cap: LineCap::Butt, join: LineJoin::Miter, miter: 10.0, dash: Vec::new(), phase: 0.0 }
    }
}

/// One drawn primitive: a path filled and/or stroked, inside clip region `clip` (0: none,
/// else 1 + the index in [`Scene::clips`]).
pub(crate) struct Op {
    pub path: Path,
    pub fill: Option<(Brush, FillRule)>,
    pub stroke: Option<StrokeStyle>,
    pub clip: u32,
}

pub(crate) struct Scene {
    /// Size in points.
    pub width: f64,
    pub height: f64,
    pub background: [u8; 3],
    pub ops: Vec<Op>,
    /// Clip regions: each is the intersection of its paths (winding fill).
    pub clips: Vec<Vec<Path>>,
    /// Primitives and opcodes the reader did not understand (or had no room for).
    pub skipped: usize,
}

impl Scene {
    pub(crate) fn new(width: f64, height: f64) -> Self {
        Scene { width, height, background: [255, 255, 255], ops: Vec::new(), clips: Vec::new(), skipped: 0 }
    }

    /// Add a primitive; one past [`MAX_OPS`] is counted as skipped.
    pub(crate) fn push(&mut self, op: Op) {
        if self.ops.len() >= MAX_OPS {
            self.skipped += 1;
        } else {
            self.ops.push(op);
        }
    }

    /// Register a clip region and return its id for [`Op::clip`] (0 when there is no room).
    pub(crate) fn add_clip(&mut self, paths: Vec<Path>) -> u32 {
        if self.clips.len() >= MAX_CLIPS {
            self.skipped += 1;
            return 0;
        }
        self.clips.push(paths);
        u32::try_from(self.clips.len()).unwrap_or(0)
    }

    /// Draw `s` in `font`: `height` is the cap height in the text's own units, `local` maps
    /// text space (baseline at y = 0, x along the text, y up) to the scene.
    pub(crate) fn text(
        &mut self,
        font: &TextFont,
        s: &str,
        height: f64,
        width_factor: f64,
        oblique: f64,
        local: Transform,
        brush: &Brush,
        clip: u32,
    ) {
        if !(height.is_finite() && height > 0.0) || s.trim().is_empty() || s.len() > 100_000 {
            return;
        }
        let shaped =
            cadcraft_fonts::shape_line(font, s, height, if width_factor.is_finite() && width_factor > 0.0 { width_factor } else { 1.0 }, oblique);
        let det = (local.sx * local.sy - local.kx * local.ky).abs();
        let stroke_w = (height * 0.09) as f32 * det.sqrt();
        if !shaped.strokes.is_empty() {
            let mut pb = PathBuilder::new();
            for line in &shaped.strokes {
                let mut it = line.iter();
                if let Some(p) = it.next() {
                    pb.move_to(p.x as f32, p.y as f32);
                    for q in it {
                        pb.line_to(q.x as f32, q.y as f32);
                    }
                }
            }
            if let Some(path) = pb.finish().and_then(|p| p.transform(local)) {
                let mut st = StrokeStyle::solid([0, 0, 0, 255], stroke_w);
                st.brush = brush.clone();
                st.cap = LineCap::Round;
                st.join = LineJoin::Round;
                self.push(Op { path, fill: None, stroke: Some(st), clip });
            }
        }
        for glyph in &shaped.glyphs {
            let mut pb = PathBuilder::new();
            for contour in glyph {
                let mut it = contour.iter();
                if let Some(p) = it.next() {
                    pb.move_to(p.x as f32, p.y as f32);
                    for q in it {
                        pb.line_to(q.x as f32, q.y as f32);
                    }
                    pb.close();
                }
            }
            if let Some(path) = pb.finish().and_then(|p| p.transform(local)) {
                self.push(Op { path, fill: Some((brush.clone(), FillRule::EvenOdd)), stroke: None, clip });
            }
        }
    }

    /// The scene drawn with its long side `max_dim` pixels (clamped to the picture limits).
    pub(crate) fn render(&self, max_dim: u32) -> Result<Raster, String> {
        let (dw, dh) = (self.width, self.height);
        if !(dw.is_finite() && dh.is_finite() && dw > 0.0 && dh > 0.0) {
            return Err("the sheet has no size".into());
        }
        let long = f64::from(max_dim.clamp(MIN_PAGE_PX, MAX_PAGE_PX));
        let s = long / dw.max(dh);
        let (pw, ph) = (((dw * s).ceil() as u32).clamp(1, MAX_PAGE_PX), ((dh * s).ceil() as u32).clamp(1, MAX_PAGE_PX));
        let mut pm = Pixmap::new(pw, ph).ok_or("the sheet is too large")?;
        let [r, g, b] = self.background;
        pm.fill(Color::from_rgba8(r, g, b, 255));
        let base = Transform::from_scale(s as f32, s as f32);
        // Farthest a coordinate is allowed from the origin: 20 000 pixels.
        let lim = (20_000.0 / s) as f32;
        let mut mask: Option<(u32, Mask)> = None;
        for op in &self.ops {
            let clip = if op.clip == 0 {
                None
            } else {
                if mask.as_ref().is_none_or(|(id, _)| *id != op.clip) {
                    mask = self.mask(op.clip, pw, ph, base, lim).map(|m| (op.clip, m));
                    if mask.is_none() {
                        // An empty or unbuildable clip hides what it clips.
                        continue;
                    }
                }
                mask.as_ref().map(|(_, m)| m)
            };
            // Coordinates far from the sheet are pulled in (see `fit`).
            let Some(path) = fit(&op.path, lim) else { continue };
            if let Some((brush, rule)) = &op.fill
                && let Some(paint) = paint(brush, None)
            {
                pm.fill_path(&path, &paint, *rule, base, clip);
            }
            if let Some(st) = &op.stroke {
                let contrast = Some(self.background);
                if let Some(paint) = paint(&st.brush, contrast) {
                    // At least three quarters of a pixel wide, so thin lines stay visible.
                    let stroke = Stroke {
                        width: st.width.max((0.75 / s) as f32).min(lim),
                        miter_limit: st.miter.max(1.0),
                        line_cap: st.cap,
                        line_join: st.join,
                        dash: if st.dash.len() >= 2 && st.dash.len() % 2 == 0 && st.dash.iter().all(|d| d.is_finite() && *d > 0.0) {
                            StrokeDash::new(st.dash.clone(), st.phase)
                        } else {
                            None
                        },
                    };
                    pm.stroke_path(&path, &paint, &stroke, base, clip);
                }
            }
        }
        Ok(Raster { width: pw, height: ph, rgba: unpremultiplied(pm) })
    }

    fn mask(&self, id: u32, w: u32, h: u32, base: Transform, lim: f32) -> Option<Mask> {
        let paths = self.clips.get(usize::try_from(id).ok()?.checked_sub(1)?)?;
        let mut mask = Mask::new(w, h)?;
        let mut first = true;
        for p in paths {
            let p = fit(p, lim)?;
            if first {
                mask.fill_path(&p, FillRule::Winding, true, base);
                first = false;
            } else {
                mask.intersect_path(&p, FillRule::Winding, true, base);
            }
        }
        if first { None } else { Some(mask) }
    }
}

/// `p` with every point pulled to within `lim` scene units of the origin: tiny-skia's
/// rasteriser can't take coordinates far outside the pixmap, and what lies that far away is
/// never visible anyway (a path covering the sheet still covers it).
fn fit(p: &Path, lim: f32) -> Option<std::borrow::Cow<'_, Path>> {
    let b = p.bounds();
    if b.left() >= -lim && b.right() <= lim && b.top() >= -lim && b.bottom() <= lim {
        return Some(std::borrow::Cow::Borrowed(p));
    }
    let c = |q: Point| (q.x.clamp(-lim, lim), q.y.clamp(-lim, lim));
    let mut pb = PathBuilder::new();
    for seg in p.segments() {
        match seg {
            PathSegment::MoveTo(a) => pb.move_to(c(a).0, c(a).1),
            PathSegment::LineTo(a) => pb.line_to(c(a).0, c(a).1),
            PathSegment::QuadTo(a, b) => pb.quad_to(c(a).0, c(a).1, c(b).0, c(b).1),
            PathSegment::CubicTo(a, b, d) => pb.cubic_to(c(a).0, c(a).1, c(b).0, c(b).1, c(d).0, c(d).1),
            PathSegment::Close => pb.close(),
        }
    }
    pb.finish().map(std::borrow::Cow::Owned)
}

fn unpremultiplied(pm: Pixmap) -> Vec<u8> {
    // The page is painted onto an opaque background, so premultiplied equals straight.
    pm.take()
}

fn color(c: [u8; 4]) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], c[3])
}

/// A stroke colour too close to the paper is drawn black or white instead, so it stays visible.
fn visible(c: [u8; 4], bg: [u8; 3]) -> [u8; 4] {
    let d: i32 = (0..3).map(|i| (i32::from(c[i]) - i32::from(bg[i])).abs()).sum();
    if d >= 90 || c[3] < 128 {
        return c;
    }
    let bg_light = (u32::from(bg[0]) + u32::from(bg[1]) + u32::from(bg[2])) > 384;
    if bg_light { [0, 0, 0, c[3]] } else { [255, 255, 255, c[3]] }
}

fn stops(list: &[(f32, [u8; 4])]) -> Vec<GradientStop> {
    list.iter().map(|(o, c)| GradientStop::new(o.clamp(0.0, 1.0), color(*c))).collect()
}

fn paint<'a>(b: &'a Brush, contrast: Option<[u8; 3]>) -> Option<Paint<'a>> {
    let mut p = Paint { anti_alias: true, ..Paint::default() };
    match b {
        Brush::Solid(c) => {
            let c = contrast.map_or(*c, |bg| visible(*c, bg));
            if c[3] == 0 {
                return None;
            }
            p.set_color(color(c));
        }
        Brush::Linear { from, to, stops: st, ts } => {
            p.shader = LinearGradient::new(Point::from_xy(from.0, from.1), Point::from_xy(to.0, to.1), stops(st), SpreadMode::Pad, *ts)?;
        }
        Brush::Radial { center, radius, stops: st, ts } => {
            let c = Point::from_xy(center.0, center.1);
            p.shader = RadialGradient::new(c, c, *radius, stops(st), SpreadMode::Pad, *ts)?;
        }
        Brush::Image { pixmap, ts, opacity } => {
            p.shader = Pattern::new(pixmap.as_ref().as_ref(), SpreadMode::Pad, tiny_skia::FilterQuality::Bilinear, opacity.clamp(0.0, 1.0), *ts);
        }
    }
    Some(p)
}

/// A decoded picture as a premultiplied pixmap (`None` when it is empty or inconsistent).
pub(crate) fn pixmap(r: &Raster) -> Option<Arc<Pixmap>> {
    let size = tiny_skia::IntSize::from_wh(r.width, r.height)?;
    let mut data = r.rgba.clone();
    for px in data.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        for c in px.iter_mut().take(3) {
            *c = ((u32::from(*c) * a + 127) / 255) as u8;
        }
    }
    Pixmap::from_vec(data, size).map(Arc::new)
}

/// A path through `points` (closed when `close`); `None` for fewer than two distinct points.
pub(crate) fn polyline(points: &[(f32, f32)], close: bool) -> Option<Path> {
    let mut it = points.iter().filter(|p| p.0.is_finite() && p.1.is_finite());
    let mut pb = PathBuilder::new();
    let first = it.next()?;
    pb.move_to(first.0, first.1);
    let mut n = 0;
    for p in it {
        pb.line_to(p.0, p.1);
        n += 1;
    }
    if n == 0 {
        return None;
    }
    if close {
        pb.close();
    }
    pb.finish()
}

/// What kind of underlay file `bytes` is; a DGN V8 file is the [`V8_ERROR`].
pub fn kind(bytes: &[u8]) -> Result<cadcraft_doc::UnderlayKind, String> {
    use cadcraft_doc::UnderlayKind;
    if is_pdf(bytes) {
        Ok(UnderlayKind::Pdf)
    } else if crate::zip::is_zip(bytes) || crate::w2d::is_w2d(bytes) {
        Ok(UnderlayKind::Dwf)
    } else if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        Err(V8_ERROR.into())
    } else if crate::dgn::is_v7(bytes) {
        Ok(UnderlayKind::Dgn)
    } else {
        Err("unrecognised underlay file".into())
    }
}

/// The sheets in a file: names in order. A PDF has numbered pages and no names here.
pub fn sheets(bytes: &[u8]) -> Result<Vec<String>, String> {
    if crate::zip::is_zip(bytes) {
        return crate::dwf::sheets(bytes);
    }
    if crate::w2d::is_w2d(bytes) {
        return Ok(vec!["Sheet 1".to_string()]);
    }
    if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        return Err(V8_ERROR.into());
    }
    if crate::dgn::is_v7(bytes) {
        return Ok(vec![crate::dgn::MODEL_NAME.to_string()]);
    }
    Err("unrecognised underlay file".into())
}

/// The scene of sheet `page` (1-based) of a DWFx, DWF or DGN file.
fn scene(bytes: &[u8], page: u32) -> Result<Scene, String> {
    let index = usize::try_from(page).map_err(|_| "sheet number too large")?.checked_sub(1).ok_or("sheets are numbered from 1")?;
    if crate::zip::is_zip(bytes) {
        return crate::dwf::scene(bytes, index);
    }
    let one = |s: Result<Scene, String>| if index == 0 { s } else { Err(format!("the file has no sheet {page}")) };
    if crate::w2d::is_w2d(bytes) {
        return one(crate::w2d::scene(bytes));
    }
    if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        return Err(V8_ERROR.into());
    }
    if crate::dgn::is_v7(bytes) {
        return one(crate::dgn::scene(bytes));
    }
    Err("unrecognised underlay file".into())
}

fn is_pdf(bytes: &[u8]) -> bool {
    bytes.get(..1024.min(bytes.len())).is_some_and(|h| h.windows(5).any(|w| w == b"%PDF-"))
}

/// The displayed size of sheet `page` in points (the picture cache's size query).
pub(crate) fn page_size(bytes: &[u8], page: u32) -> Result<(f64, f64), String> {
    if is_pdf(bytes) {
        return crate::pdf_markup::page::page_size(bytes, page);
    }
    let s = scene(bytes, page)?;
    Ok((s.width, s.height))
}

/// Sheet `page` drawn with its long side `max_dim` pixels (the picture cache's decoder).
pub(crate) fn render(bytes: &[u8], page: u32, max_dim: u32) -> Result<Raster, String> {
    if is_pdf(bytes) {
        return crate::pdf_markup::page::render(bytes, page, max_dim);
    }
    scene(bytes, page)?.render(max_dim)
}
