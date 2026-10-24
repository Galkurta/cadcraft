//! CPU rasteriser: draws a display list with tiny-skia (PNG export, plot preview, tests).

use cadcraft_color::Rgb;
use cadcraft_geom::{Bounds2, Vec2};
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::{DisplayList, ImageDraw, Kind};

/// A 2D view: world point at the image centre and pixels per drawing unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub center: Vec2,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
}

impl View {
    pub fn to_screen(&self, p: Vec2) -> (f32, f32) {
        let x = (p.x - self.center.x) * self.scale + f64::from(self.width) / 2.0;
        let y = f64::from(self.height) / 2.0 - (p.y - self.center.y) * self.scale;
        (x as f32, y as f32)
    }
    /// Fit `b` with a margin fraction.
    pub fn fit(b: &Bounds2, width: u32, height: u32, margin: f64) -> View {
        if b.is_empty() {
            return View { center: Vec2::ZERO, scale: 1.0, width, height };
        }
        let w = b.width().max(1e-9);
        let h = b.height().max(1e-9);
        let s = (f64::from(width) / w).min(f64::from(height) / h) * (1.0 - margin);
        View { center: b.center(), scale: if s.is_finite() && s > 0.0 { s } else { 1.0 }, width, height }
    }
    /// The view of `window` (drawing units), filling the image: `width` × `height` pixels when both
    /// are given; otherwise the side given (default 1600 wide) with the other matching the window's
    /// shape, each side 16..=8192.
    pub fn of_window(window: &Bounds2, width: Option<u32>, height: Option<u32>) -> View {
        let aspect = if window.width() > 1e-12 && window.height() > 1e-12 { window.width() / window.height() } else { 1.0 };
        let side = |v: f64| v.round().clamp(16.0, 8192.0) as u32;
        let (w, h) = match (width, height) {
            (Some(w), Some(h)) => (side(f64::from(w)), side(f64::from(h))),
            (None, Some(h)) => (side(f64::from(h) * aspect), side(f64::from(h))),
            (w, None) => {
                let w = f64::from(w.unwrap_or(1600));
                (side(w), side(w / aspect))
            }
        };
        View::fit(window, w, h, 0.0)
    }
    pub fn world_bounds(&self) -> Bounds2 {
        let hw = f64::from(self.width) / 2.0 / self.scale;
        let hh = f64::from(self.height) / 2.0 / self.scale;
        Bounds2::new(self.center - Vec2::new(hw, hh), self.center + Vec2::new(hw, hh))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RasterOptions {
    pub background: Rgb,
    /// Line width in pixels for zero-lineweight lines.
    pub hairline: f32,
    /// Pixels per mm for lineweights (0 = ignore lineweights).
    pub px_per_mm: f32,
    pub antialias: bool,
}

impl Default for RasterOptions {
    fn default() -> Self {
        RasterOptions { background: Rgb(33, 40, 48), hairline: 1.0, px_per_mm: 0.0, antialias: true }
    }
}

/// Render to a pixmap. Returns `None` for zero or absurd sizes.
pub fn render(list: &DisplayList, view: &View, o: &RasterOptions) -> Option<Pixmap> {
    if view.width == 0 || view.height == 0 || view.width > 16384 || view.height > 16384 {
        return None;
    }
    let mut pm = Pixmap::new(view.width, view.height)?;
    pm.fill(tiny_skia::Color::from_rgba8(o.background.0, o.background.1, o.background.2, 255));
    let vis = view.world_bounds();
    for p in &list.prims {
        let c = p.display_rgb(o.background);
        let mut paint = Paint::default();
        // A transparent object blends with what is drawn under it.
        paint.set_color_rgba8(c.0, c.1, c.2, p.alpha());
        paint.anti_alias = o.antialias;
        let pts = list.points(p);
        match p.kind {
            Kind::Polyline => {
                let bb = Bounds2::from_points(pts.iter().copied());
                if !bb.intersects(&vis) {
                    continue;
                }
                let mut pb = PathBuilder::new();
                for (i, q) in pts.iter().enumerate() {
                    let (x, y) = view.to_screen(*q);
                    if i == 0 {
                        pb.move_to(x, y);
                    } else {
                        pb.line_to(x, y);
                    }
                }
                if let Some(path) = pb.finish() {
                    let w = if o.px_per_mm > 0.0 && p.lw > 0.0 { (p.lw * o.px_per_mm).max(o.hairline) } else { o.hairline };
                    let stroke = Stroke { width: w, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() };
                    pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                }
            }
            Kind::Tris | Kind::Mask => {
                if p.kind == Kind::Mask {
                    paint.set_color_rgba8(o.background.0, o.background.1, o.background.2, 255);
                }
                let mut pb = PathBuilder::new();
                for t in pts.chunks(3) {
                    if let [a, b, c] = t {
                        let (ax, ay) = view.to_screen(*a);
                        let (bx, by) = view.to_screen(*b);
                        let (cx, cy) = view.to_screen(*c);
                        pb.move_to(ax, ay);
                        pb.line_to(bx, by);
                        pb.line_to(cx, cy);
                        pb.close();
                    }
                }
                if let Some(path) = pb.finish() {
                    pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
                }
            }
            Kind::Point => {
                if let Some(q) = pts.first() {
                    let (x, y) = view.to_screen(*q);
                    // Filled as a path: tiny-skia's anti-aliased `fill_rect` trips a debug
                    // assertion on rects under two pixels wide whose sides both fall mid-pixel.
                    if let Some(r) = tiny_skia::Rect::from_xywh(x - 0.5, y - 0.5, 1.5, 1.5) {
                        pm.fill_path(&PathBuilder::from_rect(r), &paint, FillRule::Winding, Transform::identity(), None);
                    }
                }
            }
            Kind::Image(ix) => {
                if let Some(img) = list.images.get(ix as usize) {
                    draw_image(&mut pm, view, img, p.alpha(), o.background);
                }
            }
            Kind::Infinite { ray } => {
                let (Some(base), Some(dir)) = (pts.first(), pts.get(1)) else { continue };
                let far = (vis.width() + vis.height()) * 2.0 + base.dist(vis.center());
                let a = if ray { *base } else { *base - *dir * far };
                let b = *base + *dir * far;
                let mut pb = PathBuilder::new();
                let (ax, ay) = view.to_screen(a);
                let (bx, by) = view.to_screen(b);
                pb.move_to(ax, ay);
                pb.line_to(bx, by);
                if let Some(path) = pb.finish() {
                    pm.stroke_path(&path, &paint, &Stroke { width: o.hairline, ..Default::default() }, Transform::identity(), None);
                }
            }
        }
    }
    Some(pm)
}

/// Render and encode as PNG.
pub fn render_png(list: &DisplayList, view: &View, o: &RasterOptions) -> Option<Vec<u8>> {
    render(list, view, o)?.encode_png().ok()
}

/// Draw a picture: its pixels (a PDF page at about the resolution it shows at), faded towards
/// the background, `alpha` opaque, inside its clip polygons. A picture that can't be read is
/// left out (its frame is drawn as polylines).
fn draw_image(pm: &mut Pixmap, view: &View, img: &ImageDraw, alpha: u8, bg: Rgb) {
    use tiny_skia::{FilterQuality, Mask, PixmapPaint};
    if img.missing || alpha == 0 {
        return;
    }
    let want = (img.longest() * view.scale).clamp(1.0, 1e5) as u32;
    let Ok(raster) = crate::images::load(&img.path, img.page, want) else { return };
    let (w, h) = (f64::from(raster.width), f64::from(raster.height));
    let screen = |x: f64, y: f64| view.to_screen(img.world(x, y, w, h));
    let ((x0, y0), (x1, y1), (x2, y2)) = (screen(0.0, 0.0), screen(w, 0.0), screen(0.0, h));
    let corners = [(x0, y0), (x1, y1), (x1 + x2 - x0, y1 + y2 - y0), (x2, y2)];
    let fits = |f: fn(&(f32, f32)) -> f32, lo: f32, hi: f32| {
        let (min, max) = corners.iter().fold((f32::MAX, f32::MIN), |(a, b), c| (a.min(f(c)), b.max(f(c))));
        max >= lo && min <= hi
    };
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite())
        || !fits(|c| c.0, 0.0, view.width as f32)
        || !fits(|c| c.1, 0.0, view.height as f32)
    {
        return;
    }
    let styled;
    let raster = if img.is_plain() {
        &*raster
    } else {
        styled = crate::images::styled(&raster, img, bg);
        &styled
    };
    // tiny-skia pixmaps are premultiplied.
    let mut data = raster.rgba.clone();
    for [r, g, b, a] in data.as_chunks_mut::<4>().0 {
        let a = u32::from(*a);
        *r = ((u32::from(*r) * a + 127) / 255) as u8;
        *g = ((u32::from(*g) * a + 127) / 255) as u8;
        *b = ((u32::from(*b) * a + 127) / 255) as u8;
    }
    let Some(size) = tiny_skia::IntSize::from_wh(raster.width, raster.height) else { return };
    let Some(src) = Pixmap::from_vec(data, size) else { return };
    let mut mask: Option<Mask> = None;
    for poly in &img.clips {
        let mut pb = PathBuilder::new();
        for (i, q) in poly.iter().enumerate() {
            let (x, y) = view.to_screen(*q);
            if i == 0 { pb.move_to(x, y) } else { pb.line_to(x, y) }
        }
        pb.close();
        let Some(path) = pb.finish() else { return };
        match &mut mask {
            None => {
                let Some(mut m) = Mask::new(view.width, view.height) else { return };
                m.fill_path(&path, FillRule::Winding, true, Transform::identity());
                mask = Some(m);
            }
            Some(m) => m.intersect_path(&path, FillRule::Winding, true, Transform::identity()),
        }
    }
    let t = Transform::from_row((x1 - x0) / w as f32, (y1 - y0) / w as f32, (x2 - x0) / h as f32, (y2 - y0) / h as f32, x0, y0);
    // Magnified pictures keep their pixels; reduced ones are smoothed.
    let quality = if (x1 - x0).hypot(y1 - y0) > 2.0 * w as f32 { FilterQuality::Nearest } else { FilterQuality::Bilinear };
    let paint = PixmapPaint { opacity: f32::from(alpha) / 255.0, quality, ..PixmapPaint::default() };
    pm.draw_pixmap(0, 0, src.as_ref(), &paint, t, mask.as_ref());
}
