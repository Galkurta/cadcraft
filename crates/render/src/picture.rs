//! IMAGE entities (raster images and PDF underlays) in the display list: one [`Kind::Image`]
//! primitive carries the placement, clip and fade (the pixels are fetched from [`crate::images`]
//! when a renderer draws it), and the frame is ordinary polylines. A picture whose file can't be
//! found draws its frame, both diagonals and its file name instead.

use cadcraft_doc::{Image, Text};
use cadcraft_geom::{Vec2, Vec3};

use crate::{Builder, Ctx, DPrim, Ink, Kind};

/// A PDF page whose size isn't known (file missing or unreadable), in points: ISO A4.
const A4_PT: (f64, f64) = (595.276, 841.89);

/// One picture to draw, in world coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageDraw {
    /// The raster file or PDF, and the PDF page (0 for a raster image).
    pub path: String,
    pub page: u32,
    /// The lower-left corner, and the full width and height vectors.
    pub origin: Vec2,
    pub u: Vec2,
    pub v: Vec2,
    /// Polygons the picture is drawn inside (all of them): its clip boundary, a viewport's edge.
    pub clips: Vec<Vec<Vec2>>,
    /// Fade towards the background, percent.
    pub fade: u8,
    /// Shown in shades of gray (an underlay's monochrome flag).
    pub monochrome: bool,
    /// Polygons the picture is *not* drawn inside: an inverted clip boundary.
    pub holes: Vec<Vec<Vec2>>,
    /// The file couldn't be found; only the frame is drawn.
    pub missing: bool,
}

impl ImageDraw {
    /// Where the picture's pixel `(x, y)` (fractional, origin top left, y down) falls.
    pub fn world(&self, x: f64, y: f64, w: f64, h: f64) -> Vec2 {
        self.origin + self.u * (x / w) + self.v * (1.0 - y / h)
    }
    /// The longest side in drawing units, for choosing a PDF page's resolution.
    pub fn longest(&self) -> f64 {
        self.u.len().max(self.v.len())
    }
    /// Whether the decoded pixels are drawn as they are (no fade, gray or hole).
    pub fn is_plain(&self) -> bool {
        self.fade == 0 && !self.monochrome && self.holes.is_empty()
    }
    /// A number that differs between styles of the same decoded picture (fade, gray, holes), for
    /// caches of the styled pixels.
    pub fn style_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        (self.fade, self.monochrome).hash(&mut h);
        // Holes are cut at the picture's placement.
        if !self.holes.is_empty() {
            for p in self.holes.iter().flatten().chain([&self.origin, &self.u, &self.v]) {
                (p.x.to_bits(), p.y.to_bits()).hash(&mut h);
            }
        }
        h.finish()
    }
}

/// The picture's size in its own units: pixels, or points for a PDF page.
fn natural(i: &Image) -> (f64, f64) {
    let ok = |s: Vec2| s.x.is_finite() && s.y.is_finite() && s.x > 0.0 && s.y > 0.0;
    if i.page == 0 {
        return if ok(i.size) { (i.size.x, i.size.y) } else { (1.0, 1.0) };
    }
    match crate::images::natural_size(&i.path, i.page) {
        Ok(s) if s.0 > 0.0 && s.1 > 0.0 => s,
        _ if ok(i.size) => (i.size.x, i.size.y),
        _ => A4_PT,
    }
}

/// The clip boundary in world coordinates (`None`: the whole picture). Boundary coordinates are
/// pixels from the top-left corner, y down, a pixel's centre at its integer position; for a PDF
/// page they are points from the top-left corner.
fn clip_polygon(i: &Image, size: (f64, f64)) -> Option<Vec<Vec2>> {
    if !i.clipping || i.clip.len() < 2 || i.clip.iter().any(|q| !(q.x.is_finite() && q.y.is_finite())) {
        return None;
    }
    let half = if i.page == 0 { 0.5 } else { 0.0 };
    let at = |q: Vec2| i.insert.xy() + i.u.xy() * (q.x + half) + i.v.xy() * (size.1 - half - q.y);
    let pts: Vec<Vec2> = match i.clip.as_slice() {
        [a, b] => vec![at(*a), at(Vec2::new(b.x, a.y)), at(*b), at(Vec2::new(a.x, b.y))],
        many => many.iter().map(|q| at(*q)).collect(),
    };
    Some(pts)
}

pub(crate) fn draw(b: &mut Builder, ctx: &Ctx, i: &Image, rgb: Ink, lw: f32) {
    let h = &ctx.d.header;
    let size = natural(i);
    let o = i.insert.xy();
    let (u, v) = (i.u.xy() * size.0, i.v.xy() * size.1);
    if ![o.x, o.y, u.x, u.y, v.x, v.y].iter().all(|n| n.is_finite()) {
        return;
    }
    let frame = [o, o + u, o + u + v, o + v, o];
    let missing = !crate::images::available(&i.path);
    let show_frame = missing
        || match h.i64("IMAGEFRAME", 1) {
            0 => false,
            2 => !b.plotting,
            _ => true,
        };
    let boundary: Vec<Vec<Vec2>> = clip_polygon(i, size).into_iter().map(|c| c.into_iter().map(|q| ctx.xf.apply(q)).collect()).collect();
    // An inverted boundary cuts a hole instead of limiting the picture.
    let (clips, holes) = if i.clip_inverted { (Vec::new(), boundary) } else { (boundary, Vec::new()) };
    let draw = ImageDraw {
        path: i.path.clone(),
        page: i.page,
        origin: ctx.xf.apply(o),
        u: ctx.xf.apply_vec(u),
        v: ctx.xf.apply_vec(v),
        clips,
        fade: i.fade.min(100),
        monochrome: i.monochrome,
        holes,
        missing,
    };
    let transparency = if b.opts.transparency { b.transparency } else { 0 };
    if !missing {
        b.image(ctx, rgb, transparency, &frame, draw);
    }
    if show_frame {
        b.polyline(ctx, rgb, lw, &frame);
    }
    if missing {
        b.polyline(ctx, rgb, lw, &[o, o + u + v]);
        b.polyline(ctx, rgb, lw, &[o + u, o + v]);
        label(b, ctx, rgb, lw, i, o, u, v);
    }
}

/// The file name inside the frame, along its bottom edge.
fn label(b: &mut Builder, ctx: &Ctx, rgb: Ink, lw: f32, i: &Image, o: Vec2, u: Vec2, v: Vec2) {
    let name = i.path.rsplit(['/', '\\']).next().unwrap_or("");
    let height = u.len().min(v.len()) * 0.08;
    if !b.opts.text || name.is_empty() || !(height.is_finite() && height > 0.0) {
        return;
    }
    let t = Text {
        insert: Vec3::new(o.x + u.x * 0.03 + v.x * 0.03, o.y + u.y * 0.03 + v.y * 0.03, 0.0),
        align_pt: None,
        height,
        value: name.to_string(),
        rotation: u.y.atan2(u.x),
        width_factor: 1.0,
        oblique: 0.0,
        style: "Standard".into(),
        halign: Default::default(),
        valign: Default::default(),
    };
    let (sh, _) = crate::place_text_entity(ctx.d, &t, name);
    b.shaped(ctx, rgb, lw, &sh);
}

impl Builder<'_> {
    /// Push a picture primitive; `frame` (closed outline, local coordinates) gives its vertices.
    fn image(&mut self, ctx: &Ctx, color: Ink, transparency: u8, frame: &[Vec2], draw: ImageDraw) {
        let (color, _) = self.styled(color, 0.0);
        let start = self.list.verts.len() as u32;
        for p in frame {
            let q = ctx.xf.apply(*p);
            self.list.bounds.add(q);
            self.list.verts.push(q);
        }
        let index = self.list.images.len() as u32;
        self.list.images.push(draw);
        self.list.prims.push(DPrim {
            handle: ctx.top,
            color: color.rgb,
            aci7: color.aci7,
            lw: 0.0,
            transparency,
            kind: Kind::Image(index),
            start,
            len: frame.len() as u32,
        });
    }
}
