//! Smart Blocks geometry: every supported object reduces to a [`Feat`] (key points, radii, a few
//! invariants), and a group of them matches another when one rotation + uniform scale + shift
//! (a [`Sim`], never a mirror) carries each object onto an object of the other group.
//!
//! The search anchors on one sample object: its key-point pair fixes the transform for every
//! same-kind candidate in the drawing, and the rest of the sample is then looked up at the
//! transformed positions. Every loop is bounded by a comparison budget.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cadcraft_color::Color;
use cadcraft_doc::{Block, Common, DimKind, Drawing, EntityKind, Handle, Space};
use cadcraft_geom::{Arc as GArc, Circle as GCircle, Line as GLine, PolyVertex, Polyline, Segment, TAU, Vec2, norm_angle};

/// Most vertices of a polyline the matcher looks at (a longer one is not a smart-block shape).
const MAX_VERTS: usize = 256;
/// Most boundary loops of a hatch the matcher looks at.
const MAX_LOOPS: usize = 16;
/// Most objects in a sample or a detected group.
pub const MAX_GROUP: usize = 50;
/// Most drawing objects (and flattened block-reference objects) a search looks at.
pub const MAX_POOL: usize = 20_000;
/// Most instances one search reports.
const MAX_FOUND: usize = 10_000;
/// Coordinates beyond this are treated as hostile.
const MAX_COORD: f64 = 1e15;
/// Tolerance on the dimensionless invariants (bulges, ellipse ratio and parameters).
const INV_TOL: f64 = 1e-7;
/// Tolerance on directions (hatch angle, dimension rotation), radians.
const ANG_TOL: f64 = 1e-4;
/// Points sampled round a circle or an arc for the block outline.
const OUTLINE_SAMPLES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tag {
    Line,
    Circle,
    Arc,
    Ellipse,
    Poly,
    Point,
    Text,
    MText,
    Insert,
    Hatch,
    Spline,
    Dim,
}

/// A supported object as the matcher sees it.
#[derive(Clone, Debug)]
pub struct Feat {
    pub tag: Tag,
    /// Key points; they follow the object through the transform. A hatch: its first boundary
    /// loop. A spline: its control points, then its fit points.
    pub pts: Vec<Vec2>,
    /// Polyline (and hatch loop) bulges, one per vertex (the last counts only when closed).
    pub bulge: Vec<f64>,
    /// Polyline start and end widths per vertex: lengths, so they scale with the transform.
    pub widths: Vec<(f64, f64)>,
    /// Lengths: they scale with the transform.
    pub rad: Vec<f64>,
    /// Invariants under the transform.
    pub scal: Vec<f64>,
    /// Directions that turn with the transform (hatch angle, linear dimension rotation).
    pub dirs: Vec<f64>,
    /// A hatch's boundary loops after the first, as closed polylines.
    pub loops: Vec<Feat>,
    /// A spline's control point count.
    pub split: usize,
    pub closed: bool,
    pub text: String,
}

/// Rotation by `rot`, scale by `k`, then shift by `t`.
#[derive(Clone, Copy, Debug)]
pub struct Sim {
    pub k: f64,
    pub rot: f64,
    pub t: Vec2,
}

impl Sim {
    pub const IDENTITY: Sim = Sim { k: 1.0, rot: 0.0, t: Vec2::ZERO };

    pub fn apply(&self, p: Vec2) -> Vec2 {
        p.rotate(self.rot) * self.k + self.t
    }

    /// The transform taking `a0` to `b0` and `a1` to `b1`.
    pub fn from_pairs(a0: Vec2, a1: Vec2, b0: Vec2, b1: Vec2) -> Option<Sim> {
        let (da, db) = (a1 - a0, b1 - b0);
        let (la, lb) = (da.len(), db.len());
        if la <= 1e-12 || lb <= 1e-12 {
            return None;
        }
        let k = lb / la;
        let rot = da.cross(db).atan2(da.dot(db));
        let t = b0 - a0.rotate(rot) * k;
        (k.is_finite() && (1e-6..=1e6).contains(&k) && rot.is_finite() && t.is_finite()).then_some(Sim { k, rot: norm_angle(rot), t })
    }
}

impl Feat {
    fn new(tag: Tag) -> Feat {
        Feat {
            tag,
            pts: Vec::new(),
            bulge: Vec::new(),
            widths: Vec::new(),
            rad: Vec::new(),
            scal: Vec::new(),
            dirs: Vec::new(),
            loops: Vec::new(),
            split: 0,
            closed: false,
            text: String::new(),
        }
    }

    fn finite(&self) -> bool {
        let ok = |v: &f64| v.is_finite() && v.abs() < MAX_COORD;
        self.pts.iter().all(|p| p.is_finite() && p.x.abs() < MAX_COORD && p.y.abs() < MAX_COORD)
            && self.rad.iter().chain(&self.bulge).chain(&self.scal).chain(&self.dirs).all(ok)
            && self.widths.iter().all(|(a, b)| ok(a) && ok(b))
            && self.loops.iter().all(Feat::finite)
            && !self.pts.is_empty()
    }

    pub fn center(&self) -> Vec2 {
        let n = self.pts.len().max(1) as f64;
        let s = self.pts.iter().fold(Vec2::ZERO, |a, p| a + *p);
        Vec2::new(s.x / n, s.y / n)
    }

    pub fn mapped(&self, s: &Sim) -> Feat {
        Feat {
            tag: self.tag,
            pts: self.pts.iter().map(|p| s.apply(*p)).collect(),
            bulge: self.bulge.clone(),
            widths: self.widths.iter().map(|(a, b)| (a * s.k, b * s.k)).collect(),
            rad: self.rad.iter().map(|r| r * s.k).collect(),
            scal: self.scal.clone(),
            dirs: self.dirs.iter().map(|a| a + s.rot).collect(),
            loops: self.loops.iter().map(|l| l.mapped(s)).collect(),
            split: self.split,
            closed: self.closed,
            text: self.text.clone(),
        }
    }

    /// A length measuring the object, for block similarity.
    fn measure(&self) -> f64 {
        let at = |i: usize| self.pts.get(i).copied().unwrap_or(Vec2::ZERO);
        let path = |pts: &[Vec2], closed: bool| {
            let open: f64 = pts.windows(2).map(|w| w.first().zip(w.get(1)).map_or(0.0, |(a, b)| a.dist(*b))).sum();
            match (closed, pts.first(), pts.last()) {
                (true, Some(a), Some(b)) if pts.len() > 2 => open + a.dist(*b),
                _ => open,
            }
        };
        match self.tag {
            Tag::Line => at(0).dist(at(1)),
            Tag::Circle => TAU * self.rad.first().copied().unwrap_or(0.0),
            Tag::Arc => {
                let (u, v) = (at(1) - at(0), at(2) - at(0));
                let sweep = norm_angle(u.cross(v).atan2(u.dot(v)));
                sweep * self.rad.first().copied().unwrap_or(0.0)
            }
            Tag::Ellipse => {
                let a = at(0).dist(at(1));
                std::f64::consts::PI * a * (1.0 + self.scal.first().copied().unwrap_or(1.0))
            }
            Tag::Poly | Tag::Hatch => path(&self.pts, self.closed),
            Tag::Spline => path(self.pts.get(..self.split).unwrap_or(&[]), false),
            Tag::Point | Tag::Text | Tag::MText | Tag::Insert | Tag::Dim => 0.0,
        }
    }
}

fn poly_feat(vertices: &[PolyVertex], closed: bool) -> Option<Feat> {
    if vertices.is_empty() || vertices.len() > MAX_VERTS {
        return None;
    }
    let mut f = Feat::new(Tag::Poly);
    f.pts = vertices.iter().map(|v| v.p).collect();
    f.bulge = vertices.iter().map(|v| v.bulge).collect();
    f.widths = vertices.iter().map(|v| (v.start_width, v.end_width)).collect();
    f.closed = closed;
    Some(f)
}

/// The matcher's view of an object; `None` for kinds smart blocks don't match (3D objects,
/// regions, leaders, tables, images…), arrayed references and over-long polylines or splines.
pub fn feat_of(k: &EntityKind) -> Option<Feat> {
    let mut f = Feat::new(Tag::Point);
    match k {
        EntityKind::Line(l) => {
            f.tag = Tag::Line;
            f.pts = vec![l.a.xy(), l.b.xy()];
        }
        EntityKind::Circle(c) => {
            f.tag = Tag::Circle;
            f.pts = vec![c.center.xy()];
            f.rad = vec![c.radius.abs()];
        }
        EntityKind::Arc(a) => {
            f.tag = Tag::Arc;
            let c = a.center.xy();
            let r = a.radius.abs();
            f.pts = vec![c, Vec2::polar(c, r, a.start), Vec2::polar(c, r, a.end)];
            f.rad = vec![r];
        }
        EntityKind::Ellipse(e) => {
            f.tag = Tag::Ellipse;
            f.pts = vec![e.center.xy(), e.center.xy() + e.major.xy()];
            f.scal = vec![e.ratio, e.start, e.end];
        }
        EntityKind::LwPolyline(p) => {
            f = poly_feat(&p.vertices, p.closed)?;
            f.rad = vec![p.const_width];
        }
        EntityKind::Hatch(h) => {
            if h.loops.is_empty() || h.loops.len() > MAX_LOOPS {
                return None;
            }
            let mut loops = h.loops.iter().map(|l| poly_feat(&l.vertices, true));
            f = loops.next().flatten()?;
            f.tag = Tag::Hatch;
            f.loops = loops.collect::<Option<Vec<_>>>()?;
            f.rad = vec![h.scale];
            f.dirs = vec![h.angle];
            let fill = h.gradient.as_ref().map(|g| (&g.name, g.color1, g.color2, g.centered));
            f.text = format!("{}|{}|{}|{fill:?}", h.pattern.to_ascii_uppercase(), h.solid, h.style);
            // ponytail: the pattern origin is not compared, loops are paired in any order but only the first one anchors the match.
        }
        EntityKind::Spline(sp) => {
            if sp.control.is_empty() || sp.control.len() + sp.fit.len() > MAX_VERTS || sp.knots.len() > 2 * MAX_VERTS {
                return None;
            }
            f.tag = Tag::Spline;
            f.pts = sp.control.iter().chain(&sp.fit).copied().collect();
            f.split = sp.control.len();
            f.closed = sp.closed;
            let rational = sp.weights.iter().any(|w| (w - 1.0).abs() > 1e-12);
            let weights: &[f64] = if rational { &sp.weights } else { &[] };
            // Knots as fractions of their span: a scaled copy keeps them.
            let (lo, hi) = sp.knots.iter().fold((f64::MAX, f64::MIN), |(lo, hi), k| (lo.min(*k), hi.max(*k)));
            let span = if hi > lo { hi - lo } else { 1.0 };
            f.scal = [sp.degree as f64, weights.len() as f64]
                .into_iter()
                .chain(weights.iter().copied())
                .chain(sp.knots.iter().map(|k| (k - lo) / span))
                .collect();
        }
        EntityKind::Dimension(d) => {
            f.tag = Tag::Dim;
            // The definition points each kind uses: the others hold no data.
            let (p13, p15, p16, name) = match d.kind {
                DimKind::Linear { rotation } => {
                    f.dirs = vec![rotation];
                    (true, false, false, "linear")
                }
                DimKind::Aligned => (true, false, false, "aligned"),
                DimKind::Angular => (true, true, true, "angular"),
                DimKind::Angular3P => (true, true, false, "angular3p"),
                DimKind::Diameter => (false, true, false, "diameter"),
                DimKind::Radius => (false, true, false, "radius"),
                DimKind::Ordinate { x_type } => (true, false, false, if x_type { "ordinate-x" } else { "ordinate-y" }),
                DimKind::ArcLength => (true, true, false, "arclength"),
            };
            f.pts.push(d.defpt.xy());
            if p13 {
                f.pts.extend([d.p13.xy(), d.p14.xy()]);
            }
            if p15 {
                f.pts.push(d.p15.xy());
            }
            if p16 {
                f.pts.push(d.p16.xy());
            }
            if d.user_text_pos {
                f.pts.push(d.text_mid.xy());
            }
            f.text = format!(
                "{name}|{}|{}|{}|{}",
                d.style.to_ascii_uppercase(),
                d.text,
                !d.marks.is_empty(),
                serde_json::Value::Object(d.overrides.clone())
            );
        }
        EntityKind::Point(p) => f.pts = vec![p.p.xy()],
        EntityKind::Text(t) => {
            f.tag = Tag::Text;
            f.pts = vec![t.insert.xy(), t.insert.xy() + Vec2::from_angle(t.rotation) * t.height];
            f.text = t.value.clone();
        }
        EntityKind::MText(t) => {
            f.tag = Tag::MText;
            f.pts = vec![t.insert.xy(), t.insert.xy() + Vec2::from_angle(t.rotation) * t.height];
            f.text = t.contents.clone();
        }
        EntityKind::Insert(i) => {
            if i.rows != 1 || i.cols != 1 {
                return None;
            }
            f.tag = Tag::Insert;
            let o = i.insert.xy();
            f.pts = vec![o, o + Vec2::new(i.scale.x, 0.0).rotate(i.rotation), o + Vec2::new(0.0, i.scale.y).rotate(i.rotation)];
            f.text = i.block.to_ascii_uppercase();
        }
        _ => return None,
    }
    f.finite().then_some(f)
}

// ---------- correspondences between equal objects ----------

/// How many ways `f`'s key points can be read in order: a line either way round, an open
/// polyline or spline from either end, a closed polyline (or a hatch's first loop) from any
/// vertex in either direction.
fn variants(f: &Feat) -> usize {
    let n = f.pts.len();
    match f.tag {
        Tag::Line => 2,
        Tag::Poly | Tag::Hatch if n > 1 => {
            if f.closed {
                2 * n
            } else {
                2
            }
        }
        Tag::Spline if !f.closed => 2,
        _ => 1,
    }
}

/// The key point of `f` that stands for point `i` of the other object, in reading `v`.
fn map_idx(f: &Feat, v: usize, i: usize) -> usize {
    let n = f.pts.len().max(1);
    match f.tag {
        Tag::Line => {
            if v == 0 {
                i
            } else {
                1usize.saturating_sub(i)
            }
        }
        Tag::Poly | Tag::Hatch if n > 1 => {
            if f.closed {
                if v < n { (v + i) % n } else { (v - n + n - i % n) % n }
            } else if v == 0 {
                i
            } else {
                n - 1 - i.min(n - 1)
            }
        }
        // Control points, then fit points: each run read backwards.
        Tag::Spline if v != 0 => {
            let c = f.split.min(n);
            if i < c { c - 1 - i } else { n.saturating_sub(1 + (i - c).min(n.saturating_sub(c + 1))) }
        }
        _ => i,
    }
}

/// Whether reading `v` runs along `f`'s own direction.
fn forward(f: &Feat, v: usize) -> bool {
    match f.tag {
        Tag::Line | Tag::Spline => v == 0,
        Tag::Poly | Tag::Hatch if f.pts.len() > 1 => {
            if f.closed {
                v < f.pts.len()
            } else {
                v == 0
            }
        }
        _ => true,
    }
}

fn near_all(a: &[f64], b: &[f64], tol: f64) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol)
}

/// A spline's invariants read from its other end: weights and knots reversed.
fn spline_reversed(scal: &[f64]) -> Vec<f64> {
    let nw = scal.get(1).map_or(0, |n| *n as usize);
    let (head, rest) = scal.split_at(2.min(scal.len()));
    let (w, knots) = rest.split_at(nw.min(rest.len()));
    head.iter().copied().chain(w.iter().rev().copied()).chain(knots.iter().rev().map(|k| 1.0 - k)).collect()
}

/// Whether two directions are the same turn (`period`: 2π, or π for lines without a sense).
fn same_dir(a: f64, b: f64, period: f64) -> bool {
    let d = (a - b).rem_euclid(period);
    d <= ANG_TOL || period - d <= ANG_TOL
}

/// Whether `s` (already transformed) is `d` read in way `v`.
fn eq_under(s: &Feat, d: &Feat, tol: f64, v: usize) -> bool {
    if s.tag != d.tag
        || s.closed != d.closed
        || s.pts.len() != d.pts.len()
        || s.bulge.len() != d.bulge.len()
        || s.widths.len() != d.widths.len()
        || s.dirs.len() != d.dirs.len()
        || s.loops.len() != d.loops.len()
        || s.split != d.split
        || s.text != d.text
    {
        return false;
    }
    let scal_ok = if s.tag == Tag::Spline && !forward(d, v) {
        near_all(&s.scal, &spline_reversed(&d.scal), INV_TOL)
    } else {
        near_all(&s.scal, &d.scal, INV_TOL)
    };
    if !near_all(&s.rad, &d.rad, tol) || !scal_ok {
        return false;
    }
    let period = if s.tag == Tag::Dim { std::f64::consts::PI } else { TAU };
    if !s.dirs.iter().zip(&d.dirs).all(|(a, b)| same_dir(*a, *b, period)) {
        return false;
    }
    let n = s.pts.len();
    for (i, a) in s.pts.iter().enumerate() {
        match d.pts.get(map_idx(d, v, i)) {
            Some(b) if a.near(*b, tol) => {}
            _ => return false,
        }
    }
    if matches!(s.tag, Tag::Poly | Tag::Hatch) {
        let segs = if s.closed { n } else { n.saturating_sub(1) };
        let fwd = forward(d, v);
        for i in 0..segs {
            let (a, b) = (map_idx(d, v, i), map_idx(d, v, (i + 1) % n));
            let (idx, sign) = if fwd { (a, 1.0) } else { (b, -1.0) };
            match (s.bulge.get(i), d.bulge.get(idx)) {
                (Some(x), Some(y)) if (x - sign * y).abs() <= INV_TOL => {}
                _ => return false,
            }
            // A segment read backwards starts where it ends.
            let dw = d.widths.get(idx).map(|(w0, w1)| if fwd { (*w0, *w1) } else { (*w1, *w0) });
            match (s.widths.get(i), dw) {
                (Some((x0, x1)), Some((y0, y1))) if (x0 - y0).abs() <= tol && (x1 - y1).abs() <= tol => {}
                _ => return false,
            }
        }
    }
    // The other loops of a hatch pair up with the other's, in any order.
    let mut free: Vec<&Feat> = d.loops.iter().collect();
    for l in &s.loops {
        match free.iter().position(|o| (0..variants(o)).any(|lv| eq_under(l, o, tol, lv))) {
            Some(i) => {
                free.swap_remove(i);
            }
            None => return false,
        }
    }
    true
}

// ---------- the drawing's objects ----------

/// What must agree between two objects of one group besides their shape.
pub type Style = (String, Color, String);

pub fn style_of(c: &Common) -> Style {
    (c.layer.clone(), c.color, c.linetype.clone())
}

#[derive(Clone)]
pub struct PoolEnt {
    pub feat: Feat,
    /// The object that owns it: itself, or the block reference it was flattened from.
    pub owner: Handle,
    pub flat: bool,
    pub style: Option<Style>,
    pub center: Vec2,
}

/// The searchable objects of one space.
pub struct Pool {
    pub ents: Vec<PoolEnt>,
    by_tag: HashMap<Tag, Vec<usize>>,
    flat_count: HashMap<Handle, usize>,
    /// Objects past [`MAX_POOL`] were left out.
    pub truncated: bool,
}

impl Pool {
    pub fn new(ents: Vec<PoolEnt>, truncated: bool) -> Pool {
        let mut by_tag: HashMap<Tag, Vec<usize>> = HashMap::new();
        let mut flat_count: HashMap<Handle, usize> = HashMap::new();
        for (i, e) in ents.iter().enumerate() {
            by_tag.entry(e.feat.tag).or_default().push(i);
            if e.flat {
                *flat_count.entry(e.owner).or_default() += 1;
            }
        }
        for l in by_tag.values_mut() {
            l.sort_by(|a, b| {
                let x = |i: &usize| ents.get(*i).map_or(0.0, |e| e.center.x);
                x(a).total_cmp(&x(b))
            });
        }
        Pool { ents, by_tag, flat_count, truncated }
    }

    fn list(&self, t: Tag) -> &[usize] {
        self.by_tag.get(&t).map_or(&[], Vec::as_slice)
    }

    pub fn ent(&self, i: usize) -> Option<&PoolEnt> {
        self.ents.get(i)
    }

    /// Visible objects of `space` not on a locked layer. With `include_existing`, a reference to
    /// a block made of supported objects also contributes its objects, flattened to the drawing.
    pub fn of_space(d: &Drawing, space: &Space, include_existing: bool) -> Pool {
        let mut ents = Vec::new();
        let mut truncated = false;
        let Some(store) = d.space(space) else { return Pool::new(ents, false) };
        for e in store.iter() {
            if ents.len() >= MAX_POOL {
                truncated = true;
                break;
            }
            if !d.is_visible(e) || d.layer(&e.common.layer).is_some_and(|l| l.locked) {
                continue;
            }
            if let Some(feat) = feat_of(&e.kind) {
                let center = feat.center();
                ents.push(PoolEnt { feat, owner: e.handle, flat: false, style: Some(style_of(&e.common)), center });
            }
            if include_existing
                && let EntityKind::Insert(ins) = &e.kind
                && let Some(b) = d.block(&ins.block)
            {
                for feat in flatten(b, ins) {
                    let center = feat.center();
                    ents.push(PoolEnt { feat, owner: e.handle, flat: true, style: None, center });
                }
            }
        }
        Pool::new(ents, truncated)
    }
}

/// A reference's block objects in the drawing; empty when the block holds an unsupported
/// object, is too big, or the reference is mirrored, stretched or arrayed.
fn flatten(b: &Block, ins: &cadcraft_doc::Insert) -> Vec<Feat> {
    let (sx, sy) = (ins.scale.x, ins.scale.y);
    if ins.rows != 1 || ins.cols != 1 || sx <= 0.0 || (sx - sy).abs() > 1e-12 * sx.max(1.0) || b.entities.len() > MAX_GROUP || b.entities.is_empty() {
        return Vec::new();
    }
    let k = sx;
    let sim = Sim { k, rot: ins.rotation, t: ins.insert.xy() - b.base.xy().rotate(ins.rotation) * k };
    let feats: Option<Vec<Feat>> = b.entities.iter().map(|e| feat_of(&e.kind).map(|f| f.mapped(&sim))).collect();
    feats.filter(|v| v.iter().all(Feat::finite)).unwrap_or_default()
}

// ---------- the search ----------

pub struct Opts {
    /// Absolute tolerance in drawing units.
    pub tol: f64,
    /// Accept copies at another size.
    pub scaled: bool,
    /// Most comparisons the search may make.
    pub budget: u64,
}

/// One place the sample occurs.
pub struct Instance {
    /// Pool index of the object matching each sample object, in sample order.
    pub members: Vec<usize>,
    /// Carries the sample onto this instance.
    pub sim: Sim,
}

pub struct Found {
    pub instances: Vec<Instance>,
    /// The budget ran out before every candidate was tried.
    pub truncated: bool,
    /// Budget not spent.
    pub left: u64,
}

/// Where in the sample a rotation is readable: the farthest pair of one object's key points.
fn far_pair(pts: &[Vec2]) -> Option<(usize, usize, f64)> {
    let far = |from: Vec2| {
        pts.iter().enumerate().fold((0usize, 0.0f64), |best, (i, p)| {
            let d = from.dist(*p);
            if d > best.1 { (i, d) } else { best }
        })
    };
    let (j, _) = far(*pts.first()?);
    let (i, d) = far(*pts.get(j)?);
    Some((i, j, d))
}

struct Searcher<'a> {
    sample: &'a [Feat],
    styles: &'a [Option<Style>],
    pool: &'a Pool,
    o: &'a Opts,
    left: u64,
    truncated: bool,
    seen: HashSet<Vec<usize>>,
    out: Vec<Instance>,
}

pub fn search(sample: &[Feat], styles: &[Option<Style>], pool: &Pool, o: &Opts) -> Found {
    let mut s = Searcher { sample, styles, pool, o, left: o.budget, truncated: false, seen: HashSet::new(), out: Vec::new() };
    if !sample.is_empty() && sample.len() <= MAX_GROUP && o.tol.is_finite() && o.tol >= 0.0 {
        s.run();
    }
    Found { instances: s.out, truncated: s.truncated || pool.truncated, left: s.left }
}

impl Searcher<'_> {
    fn spend(&mut self) -> bool {
        if self.left == 0 || self.out.len() >= MAX_FOUND {
            self.truncated = true;
            return false;
        }
        self.left -= 1;
        true
    }

    fn min_len(&self) -> f64 {
        (self.o.tol * 100.0).max(1e-9)
    }

    fn scale_ok(&self, k: f64, len: f64) -> bool {
        self.o.scaled || (k - 1.0).abs() * len <= self.o.tol
    }

    fn run(&mut self) {
        let min = self.min_len();
        // The sample object that tells the rotation and is rarest in the drawing.
        let mut best: Option<(usize, usize, usize, usize)> = None;
        for (si, f) in self.sample.iter().enumerate() {
            if let Some((i, j, d)) = far_pair(&f.pts)
                && d > min
            {
                let n = self.pool.list(f.tag).len();
                if best.is_none_or(|b| n < b.3) {
                    best = Some((si, i, j, n));
                }
            }
        }
        match best {
            Some((e0, i, j, _)) => self.anchored(e0, i, j),
            None => self.rotation_free(),
        }
    }

    fn anchored(&mut self, e0: usize, i: usize, j: usize) {
        let Some(s) = self.sample.get(e0) else { return };
        let (Some(&a0), Some(&a1)) = (s.pts.get(i), s.pts.get(j)) else { return };
        let len = a0.dist(a1);
        for &di in self.pool.list(s.tag) {
            let Some(d) = self.pool.ent(di).map(|e| &e.feat) else { continue };
            if d.pts.len() != s.pts.len() || d.text != s.text {
                continue;
            }
            for v in 0..variants(d) {
                if !self.spend() {
                    return;
                }
                let (Some(&b0), Some(&b1)) = (d.pts.get(map_idx(d, v, i)), d.pts.get(map_idx(d, v, j))) else { continue };
                if let Some(sim) = Sim::from_pairs(a0, a1, b0, b1)
                    && self.scale_ok(sim.k, len)
                    && !self.verify(sim)
                {
                    return;
                }
            }
        }
    }

    /// A sample of only circles and points: no rotation to read from one object.
    fn rotation_free(&mut self) {
        let min = self.min_len();
        let Some(e0) = (0..self.sample.len()).min_by_key(|i| self.sample.get(*i).map_or(0, |f| self.pool.list(f.tag).len())) else { return };
        let Some(f0) = self.sample.get(e0) else { return };
        let c0 = f0.center();
        let mut far: Option<(usize, f64)> = None;
        for (i, f) in self.sample.iter().enumerate() {
            let d = f.center().dist(c0);
            if i != e0 && far.is_none_or(|b| d > b.1) {
                far = Some((i, d));
            }
        }
        match far {
            Some((e1, d)) if d > min => {
                let Some(f1) = self.sample.get(e1) else { return };
                let c1 = f1.center();
                for &d0 in self.pool.list(f0.tag) {
                    for &d1 in self.pool.list(f1.tag) {
                        if !self.spend() {
                            return;
                        }
                        let (Some(p0), Some(p1)) = (self.pool.ent(d0), self.pool.ent(d1)) else { continue };
                        if d0 != d1
                            && let Some(sim) = Sim::from_pairs(c0, c1, p0.center, p1.center)
                            && self.scale_ok(sim.k, d)
                            && !self.verify(sim)
                        {
                            return;
                        }
                    }
                }
            }
            // One object, or concentric ones: a shift (and the size) carries the sample.
            _ => {
                let r0 = f0.rad.first().copied().unwrap_or(0.0);
                for &di in self.pool.list(f0.tag) {
                    if !self.spend() {
                        return;
                    }
                    let Some(p) = self.pool.ent(di) else { continue };
                    let k = if r0 > 0.0 { p.feat.rad.first().copied().unwrap_or(0.0) / r0 } else { 1.0 };
                    if k.is_finite()
                        && (1e-6..=1e6).contains(&k)
                        && self.scale_ok(k, r0.max(1.0))
                        && !self.verify(Sim { k, rot: 0.0, t: p.center - c0 * k })
                    {
                        return;
                    }
                }
            }
        }
    }

    /// Tries `sim`: records the instance when it carries every sample object onto its own
    /// drawing object (and nothing like it was recorded). `false` only when out of budget.
    fn verify(&mut self, sim: Sim) -> bool {
        let n = self.sample.len();
        let tol = self.o.tol * sim.k.max(1.0);
        let reach = tol * 2.0 + 1e-12;
        let mut members: Vec<usize> = Vec::with_capacity(n);
        for (si, s) in self.sample.iter().enumerate() {
            let m = s.mapped(&sim);
            let c = m.center();
            let list = self.pool.list(s.tag);
            let lo = list.partition_point(|&i| self.pool.ent(i).is_some_and(|e| e.center.x < c.x - reach));
            let mut hit = None;
            for &pi in list.get(lo..).unwrap_or(&[]) {
                let Some(e) = self.pool.ent(pi) else { continue };
                if e.center.x > c.x + reach {
                    break;
                }
                if self.left == 0 {
                    self.truncated = true;
                    return false;
                }
                self.left -= 1;
                if (e.center.y - c.y).abs() > reach || members.contains(&pi) {
                    continue;
                }
                if let (Some(Some(a)), Some(b)) = (self.styles.get(si), &e.style)
                    && a != b
                {
                    continue;
                }
                if (0..variants(&e.feat)).any(|v| eq_under(&m, &e.feat, tol, v)) {
                    hit = Some(pi);
                    break;
                }
            }
            match hit {
                Some(pi) => members.push(pi),
                None => return true,
            }
        }
        if !self.whole_owner(&members) {
            return true;
        }
        let mut key = members.clone();
        key.sort_unstable();
        if self.seen.insert(key) {
            self.out.push(Instance { members, sim });
        }
        true
    }

    /// Objects flattened from a block reference count only as that whole reference.
    fn whole_owner(&self, members: &[usize]) -> bool {
        let ents: Vec<&PoolEnt> = members.iter().filter_map(|i| self.pool.ent(*i)).collect();
        if !ents.iter().any(|e| e.flat) {
            return true;
        }
        let Some(first) = ents.first() else { return false };
        ents.iter().all(|e| e.flat && e.owner == first.owner) && self.pool.flat_count.get(&first.owner) == Some(&members.len())
    }
}

// ---------- curves ----------

/// What a straight or curved object is made of, for touching tests and outlines.
enum Curve {
    Seg(Segment),
    Circle(GCircle),
}

impl Curve {
    fn closest(&self, p: Vec2) -> Vec2 {
        match self {
            Curve::Seg(s) => s.closest(p),
            Curve::Circle(c) => c.closest(p),
        }
    }

    fn len(&self) -> f64 {
        match self {
            Curve::Seg(s) => s.len(),
            Curve::Circle(c) => TAU * c.radius,
        }
    }

    /// The point `t` (0..=1) of the way along.
    fn at(&self, t: f64) -> Vec2 {
        match self {
            Curve::Seg(s) => s.at(t),
            Curve::Circle(c) => Vec2::polar(c.center, c.radius, t * TAU),
        }
    }
}

/// The lines, arcs and circles of `f` (a closed hatch loop counts as a polyline); none for the
/// other kinds.
fn curves(f: &Feat) -> Vec<Curve> {
    let (Some(&a), Some(&b)) = (f.pts.first(), f.pts.get(1)) else {
        return match (f.tag, f.pts.first(), f.rad.first()) {
            (Tag::Circle, Some(c), Some(r)) => vec![Curve::Circle(GCircle::new(*c, *r))],
            _ => Vec::new(),
        };
    };
    match f.tag {
        Tag::Line => vec![Curve::Seg(Segment::Line(GLine::new(a, b)))],
        Tag::Arc => match (f.pts.get(2), f.rad.first()) {
            (Some(e), Some(r)) => vec![Curve::Seg(Segment::Arc { arc: GArc::new(a, *r, a.angle_to(b), a.angle_to(*e)), ccw: true })],
            _ => Vec::new(),
        },
        Tag::Poly | Tag::Hatch => {
            let vertices = f.pts.iter().zip(&f.bulge).map(|(p, bulge)| PolyVertex::with_bulge(*p, *bulge)).collect();
            Polyline { vertices, closed: f.closed }.segments().into_iter().map(Curve::Seg).collect()
        }
        _ => Vec::new(),
    }
}

// ---------- detection of repeated groups ----------

fn uf_find(p: &mut [usize], mut i: usize) -> usize {
    while let Some(&up) = p.get(i) {
        if up == i {
            break;
        }
        let grand = p.get(up).copied().unwrap_or(up);
        if let Some(slot) = p.get_mut(i) {
            *slot = grand;
        }
        i = grand;
    }
    i
}

fn uf_union(p: &mut [usize], i: usize, j: usize) {
    let (a, b) = (uf_find(p, i), uf_find(p, j));
    if let Some(slot) = p.get_mut(a) {
        *slot = b;
    }
}

/// The points of an object that link it to the objects it touches: its key points, and the
/// four quadrant points of a circle.
fn link_points(f: &Feat) -> Vec<Vec2> {
    let mut pts = f.pts.clone();
    if f.tag == Tag::Circle
        && let (Some(c), Some(r)) = (f.pts.first(), f.rad.first())
    {
        pts.extend([*c + Vec2::new(*r, 0.0), *c + Vec2::new(-*r, 0.0), *c + Vec2::new(0.0, *r), *c + Vec2::new(0.0, -*r)]);
    }
    pts
}

/// Most grid cells one curve registers in; a longer one is sampled more coarsely.
const MAX_CELLS_PER_CURVE: usize = 8192;

/// Links an object to another when one of its link points lies on the other's line, arc or
/// circle (a T-junction: a wall ending on another wall's side). Curves go into a grid of cells
/// at least twice `gap` wide, one registration per cell they pass through.
fn link_t_junctions(pool: &Pool, gap: f64, parent: &mut [usize], budget: &mut u64) {
    let (mut lo, mut hi) = (Vec2::new(f64::MAX, f64::MAX), Vec2::new(f64::MIN, f64::MIN));
    for e in pool.ents.iter().filter(|e| !e.flat) {
        for p in &e.feat.pts {
            lo = lo.min(*p);
            hi = hi.max(*p);
        }
    }
    if !(lo.is_finite() && hi.is_finite()) {
        return;
    }
    let cell = (gap * 2.0).max((hi.x - lo.x).max(hi.y - lo.y) / 1024.0).max(1e-9);
    if !cell.is_finite() {
        return;
    }
    let key = |p: Vec2| ((p.x / cell).floor() as i64, (p.y / cell).floor() as i64);
    let mut all: Vec<(usize, Curve)> = Vec::new();
    let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for (i, e) in pool.ents.iter().enumerate().filter(|(_, e)| !e.flat) {
        for c in curves(&e.feat) {
            let steps = ((c.len() / (cell / 2.0)).ceil().max(1.0) as usize).min(MAX_CELLS_PER_CURVE);
            let ci = all.len();
            let mut last = None;
            for s in 0..=steps {
                let k = key(c.at(s as f64 / steps as f64));
                if last != Some(k) {
                    if *budget == 0 {
                        return;
                    }
                    *budget -= 1;
                    grid.entry(k).or_default().push(ci);
                    last = Some(k);
                }
            }
            all.push((i, c));
        }
    }
    for (i, e) in pool.ents.iter().enumerate().filter(|(_, e)| !e.flat) {
        for p in link_points(&e.feat) {
            let (cx, cy) = key(p);
            for dx in -1..=1i64 {
                for dy in -1..=1i64 {
                    let Some(near) = grid.get(&(cx.saturating_add(dx), cy.saturating_add(dy))) else { continue };
                    for &ci in near {
                        let Some((j, c)) = all.get(ci) else { continue };
                        if *j == i || uf_find(parent, i) == uf_find(parent, *j) {
                            continue;
                        }
                        if *budget == 0 {
                            return;
                        }
                        *budget -= 1;
                        if c.closest(p).near(p, gap) {
                            uf_union(parent, i, *j);
                        }
                    }
                }
            }
        }
    }
}

/// Groups of touching objects (key points within `gap` of each other, or a key point on
/// another object's line, arc or circle) of 2..=`max_size` objects, as pool indices; flattened
/// objects are left out.
pub fn clusters(pool: &Pool, gap: f64, max_size: usize, budget: &mut u64) -> Vec<Vec<usize>> {
    let cell = gap.max(1e-9);
    let key = |p: Vec2| ((p.x / cell).floor() as i64, (p.y / cell).floor() as i64);
    let mut parent: Vec<usize> = (0..pool.ents.len()).collect();
    let mut grid: HashMap<(i64, i64), Vec<(usize, Vec2)>> = HashMap::new();
    'all: for (i, e) in pool.ents.iter().enumerate() {
        if e.flat {
            continue;
        }
        for p in link_points(&e.feat) {
            let (cx, cy) = key(p);
            for dx in -1..=1i64 {
                for dy in -1..=1i64 {
                    let Some(near) = grid.get(&(cx.saturating_add(dx), cy.saturating_add(dy))) else { continue };
                    for &(j, q) in near {
                        if *budget == 0 {
                            break 'all;
                        }
                        *budget -= 1;
                        if j != i && q.near(p, gap) {
                            uf_union(&mut parent, i, j);
                        }
                    }
                }
            }
            grid.entry((cx, cy)).or_default().push((i, p));
        }
    }
    link_t_junctions(pool, gap, &mut parent, budget);
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, e) in pool.ents.iter().enumerate() {
        if !e.flat {
            let r = uf_find(&mut parent, i);
            groups.entry(r).or_default().push(i);
        }
    }
    let mut out: Vec<Vec<usize>> = groups.into_values().filter(|g| g.len() >= 2 && g.len() <= max_size).collect();
    out.sort_by_key(|g| g.first().copied());
    out
}

/// What groups must share to be worth a full comparison: the count of each kind of object.
pub fn histogram(pool: &Pool, group: &[usize]) -> Vec<(u8, u32)> {
    let mut h: BTreeMap<u8, u32> = BTreeMap::new();
    for e in group.iter().filter_map(|i| pool.ent(*i)) {
        *h.entry(e.feat.tag as u8).or_default() += 1;
    }
    h.into_iter().collect()
}

// ---------- block similarity ----------

/// A block definition's shape summary, independent of position, rotation and size.
pub struct Sig {
    kinds: BTreeMap<&'static str, u32>,
    /// Short side over long side of the smallest rectangle (at any rotation) holding the shape.
    aspect: f64,
    /// Object lengths over that rectangle's diagonal, longest first.
    lens: Vec<f64>,
}

/// Most outline points the signature's rectangle is fitted to.
const MAX_CLOUD: usize = 8192;

/// Points along the outline of `f`, enough to bound its shape at any rotation.
fn outline(f: &Feat, out: &mut Vec<Vec2>) {
    let sweep_of = |a: f64, b: f64| match (b - a).rem_euclid(TAU) {
        s if s < 1e-12 => TAU,
        s => s,
    };
    match (f.tag, f.pts.first(), f.pts.get(1)) {
        (Tag::Circle | Tag::Arc | Tag::Ellipse | Tag::Poly | Tag::Hatch, ..) => {}
        _ => {
            out.extend(f.pts.iter().copied());
            return;
        }
    }
    if f.tag == Tag::Ellipse {
        if let (Some(&c), Some(&m), [ratio, start, end, ..]) = (f.pts.first(), f.pts.get(1), f.scal.as_slice()) {
            let (major, sweep) = (m - c, sweep_of(*start, *end));
            let minor = major.perp() * *ratio;
            out.extend((0..=OUTLINE_SAMPLES).map(|i| {
                let t = start + sweep * i as f64 / OUTLINE_SAMPLES as f64;
                c + major * t.cos() + minor * t.sin()
            }));
        }
        return;
    }
    if matches!(f.tag, Tag::Poly | Tag::Hatch) {
        out.extend(f.pts.iter().copied());
    }
    for c in curves(f) {
        let n = match &c {
            Curve::Seg(Segment::Line(_)) => 1,
            _ => OUTLINE_SAMPLES,
        };
        out.extend((0..=n).map(|i| c.at(i as f64 / n as f64)));
    }
    if f.tag == Tag::Hatch {
        out.extend(f.loops.iter().flat_map(|l| l.pts.iter().copied()));
    }
}

/// The convex hull of `pts` (counter-clockwise, no repeated points).
fn convex_hull(mut pts: Vec<Vec2>) -> Vec<Vec2> {
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let mut hull: Vec<Vec2> = Vec::with_capacity(pts.len() * 2);
    for pass in 0..2 {
        let floor = hull.len() + 1;
        let mut walk = |p: Vec2| {
            while hull.len() >= floor.max(2) && hull.get(hull.len() - 2).zip(hull.last()).is_some_and(|(a, b)| (*b - *a).cross(p - *a) <= 0.0) {
                hull.pop();
            }
            hull.push(p);
        };
        if pass == 0 {
            pts.iter().for_each(|p| walk(*p));
        } else {
            pts.iter().rev().skip(1).for_each(|p| walk(*p));
        }
    }
    hull.pop();
    hull
}

/// Most hull points the rectangle fit looks at.
const MAX_HULL: usize = 1024;

/// Sides of the smallest rectangle, at any rotation, holding `pts`. Among rectangles of equal
/// area the thinnest wins, so the result does not depend on how the points are turned.
fn min_area_rect(pts: Vec<Vec2>) -> (f64, f64) {
    let mut hull = convex_hull(pts);
    // ponytail: a hull of more than MAX_HULL points (a huge, round shape) is thinned, so the rectangle is approximate there; the fit is quadratic in it.
    if hull.len() > MAX_HULL {
        let stride = hull.len().div_ceil(MAX_HULL);
        hull = hull.into_iter().step_by(stride).collect();
    }
    let (Some(&first), Some(&second)) = (hull.first(), hull.get(1)) else { return (0.0, 0.0) };
    if hull.len() < 3 {
        return (first.dist(second), 0.0);
    }
    let mut best: Option<(f64, f64, f64)> = None;
    for (i, a) in hull.iter().enumerate() {
        let Some(b) = hull.get((i + 1) % hull.len()) else { continue };
        let d = (*b - *a).normalized();
        let (mut lo, mut hi) = (Vec2::new(f64::MAX, f64::MAX), Vec2::new(f64::MIN, f64::MIN));
        for p in &hull {
            let q = Vec2::new((*p - *a).dot(d), (*p - *a).dot(d.perp()));
            lo = lo.min(q);
            hi = hi.max(q);
        }
        let (long, short) = ((hi.x - lo.x).max(hi.y - lo.y), (hi.x - lo.x).min(hi.y - lo.y));
        let area = long * short;
        if best.is_none_or(|(ba, _, bs)| area < ba * (1.0 - 1e-9) || (area <= ba * (1.0 + 1e-9) && short < bs)) {
            best = Some((area, long, short));
        }
    }
    best.map_or((0.0, 0.0), |(_, long, short)| (long, short))
}

pub fn block_sig(b: &Block) -> Sig {
    sig_of(b.entities.iter().map(|e| &e.kind))
}

/// The signature of a group of objects, as if they were a block.
pub fn sig_of<'a>(kinds_in: impl Iterator<Item = &'a EntityKind>) -> Sig {
    let mut kinds: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut cloud: Vec<Vec2> = Vec::new();
    let mut lens = Vec::new();
    for k in kinds_in {
        *kinds.entry(k.type_name()).or_default() += 1;
        if let Some(f) = feat_of(k) {
            if cloud.len() < MAX_CLOUD {
                outline(&f, &mut cloud);
            }
            lens.push(f.measure());
        }
    }
    let (long, short) = min_area_rect(cloud);
    let diag = long.hypot(short);
    let aspect = if long > 0.0 { short / long } else { 1.0 };
    let mut lens: Vec<f64> = lens.into_iter().map(|l| if diag > 0.0 { l / diag } else { 0.0 }).collect();
    lens.sort_by(|a, b| b.total_cmp(a));
    lens.truncate(64);
    Sig { kinds, aspect, lens }
}

/// 0 for blocks of the same shape; grows with differences in object kinds, the rectangle's
/// aspect and the distribution of object lengths.
pub fn distance(a: &Sig, b: &Sig) -> f64 {
    let all: BTreeSet<&str> = a.kinds.keys().chain(b.kinds.keys()).copied().collect();
    let total: u32 = a.kinds.values().chain(b.kinds.values()).sum();
    let diff: u32 = all.iter().map(|k| a.kinds.get(k).copied().unwrap_or(0).abs_diff(b.kinds.get(k).copied().unwrap_or(0))).sum();
    let kinds = if total > 0 { f64::from(diff) / f64::from(total) } else { 0.0 };
    let n = a.lens.len().max(b.lens.len());
    let lens = if n > 0 {
        (0..n).map(|i| (a.lens.get(i).copied().unwrap_or(0.0) - b.lens.get(i).copied().unwrap_or(0.0)).abs()).sum::<f64>() / n as f64
    } else {
        0.0
    };
    kinds * 2.0 + (a.aspect - b.aspect).abs() + lens * 2.0
}
