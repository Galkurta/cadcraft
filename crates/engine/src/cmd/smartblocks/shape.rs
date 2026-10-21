//! Smart Blocks geometry: every supported object reduces to a [`Feat`] (key points, radii, a few
//! invariants), and a group of them matches another when one rotation + uniform scale + shift
//! (a [`Sim`], never a mirror) carries each object onto an object of the other group.
//!
//! The search anchors on one sample object: its key-point pair fixes the transform for every
//! same-kind candidate in the drawing, and the rest of the sample is then looked up at the
//! transformed positions. Every loop is bounded by a comparison budget.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cadcraft_color::Color;
use cadcraft_doc::{Block, Common, Drawing, EntityKind, Handle, Space};
use cadcraft_geom::{TAU, Vec2, norm_angle};

/// Most vertices of a polyline the matcher looks at (a longer one is not a smart-block shape).
const MAX_VERTS: usize = 256;
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
}

/// A supported object as the matcher sees it.
#[derive(Clone, Debug)]
pub struct Feat {
    pub tag: Tag,
    /// Key points; they follow the object through the transform.
    pub pts: Vec<Vec2>,
    /// Polyline bulges, one per vertex (the last counts only when closed).
    pub bulge: Vec<f64>,
    /// Lengths: they scale with the transform.
    pub rad: Vec<f64>,
    /// Invariants under the transform.
    pub scal: Vec<f64>,
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
    fn finite(&self) -> bool {
        self.pts.iter().all(|p| p.is_finite() && p.x.abs() < MAX_COORD && p.y.abs() < MAX_COORD)
            && self.rad.iter().chain(&self.bulge).chain(&self.scal).all(|v| v.is_finite() && v.abs() < MAX_COORD)
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
            rad: self.rad.iter().map(|r| r * s.k).collect(),
            scal: self.scal.clone(),
            closed: self.closed,
            text: self.text.clone(),
        }
    }

    /// A length measuring the object, for block similarity.
    fn measure(&self) -> f64 {
        let at = |i: usize| self.pts.get(i).copied().unwrap_or(Vec2::ZERO);
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
            Tag::Poly => {
                let n = self.pts.len();
                let open: f64 = self.pts.windows(2).map(|w| w.first().zip(w.get(1)).map_or(0.0, |(a, b)| a.dist(*b))).sum();
                if self.closed && n > 2 { open + at(0).dist(at(n - 1)) } else { open }
            }
            Tag::Point | Tag::Text | Tag::MText | Tag::Insert => 0.0,
        }
    }
}

/// The matcher's view of an object; `None` for kinds smart blocks don't match (hatches, splines,
/// dimensions…), arrayed references and over-long polylines.
pub fn feat_of(k: &EntityKind) -> Option<Feat> {
    let mut f = Feat { tag: Tag::Point, pts: Vec::new(), bulge: Vec::new(), rad: Vec::new(), scal: Vec::new(), closed: false, text: String::new() };
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
            if p.vertices.is_empty() || p.vertices.len() > MAX_VERTS {
                return None;
            }
            f.tag = Tag::Poly;
            f.pts = p.vertices.iter().map(|v| v.p).collect();
            f.bulge = p.vertices.iter().map(|v| v.bulge).collect();
            f.rad = vec![p.const_width];
            f.closed = p.closed;
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
/// polyline from either end, a closed one from any vertex in either direction.
fn variants(f: &Feat) -> usize {
    let n = f.pts.len();
    match f.tag {
        Tag::Line => 2,
        Tag::Poly if n > 1 => {
            if f.closed {
                2 * n
            } else {
                2
            }
        }
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
        Tag::Poly if n > 1 => {
            if f.closed {
                if v < n { (v + i) % n } else { (v - n + n - i % n) % n }
            } else if v == 0 {
                i
            } else {
                n - 1 - i.min(n - 1)
            }
        }
        _ => i,
    }
}

/// Whether reading `v` runs along `f`'s own direction.
fn forward(f: &Feat, v: usize) -> bool {
    match f.tag {
        Tag::Line => v == 0,
        Tag::Poly if f.pts.len() > 1 => {
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

/// Whether `s` (already transformed) is `d` read in way `v`.
fn eq_under(s: &Feat, d: &Feat, tol: f64, v: usize) -> bool {
    if s.tag != d.tag || s.closed != d.closed || s.pts.len() != d.pts.len() || s.bulge.len() != d.bulge.len() || s.text != d.text {
        return false;
    }
    if !near_all(&s.rad, &d.rad, tol) || !near_all(&s.scal, &d.scal, INV_TOL) {
        return false;
    }
    let n = s.pts.len();
    for (i, a) in s.pts.iter().enumerate() {
        match d.pts.get(map_idx(d, v, i)) {
            Some(b) if a.near(*b, tol) => {}
            _ => return false,
        }
    }
    if s.tag == Tag::Poly {
        let segs = if s.closed { n } else { n.saturating_sub(1) };
        let fwd = forward(d, v);
        for i in 0..segs {
            let (a, b) = (map_idx(d, v, i), map_idx(d, v, (i + 1) % n));
            let (idx, sign) = if fwd { (a, 1.0) } else { (b, -1.0) };
            match (s.bulge.get(i), d.bulge.get(idx)) {
                (Some(x), Some(y)) if (x - sign * y).abs() <= INV_TOL => {}
                _ => return false,
            }
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

// ---------- detection of repeated groups ----------

/// Groups of touching objects (key points within `gap` of each other) of 2..=`max_size`
/// objects, as pool indices; flattened objects are left out.
// ponytail: links need coincident key points (end points, centres); a T-junction mid-segment does not link.
pub fn clusters(pool: &Pool, gap: f64, max_size: usize, budget: &mut u64) -> Vec<Vec<usize>> {
    let cell = gap.max(1e-9);
    let key = |p: Vec2| ((p.x / cell).floor() as i64, (p.y / cell).floor() as i64);
    let mut parent: Vec<usize> = (0..pool.ents.len()).collect();
    fn find(p: &mut [usize], mut i: usize) -> usize {
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
    let mut grid: HashMap<(i64, i64), Vec<(usize, Vec2)>> = HashMap::new();
    'all: for (i, e) in pool.ents.iter().enumerate() {
        if e.flat {
            continue;
        }
        let mut pts = e.feat.pts.clone();
        if e.feat.tag == Tag::Circle
            && let (Some(c), Some(r)) = (e.feat.pts.first(), e.feat.rad.first())
        {
            pts.extend([*c + Vec2::new(*r, 0.0), *c + Vec2::new(-*r, 0.0), *c + Vec2::new(0.0, *r), *c + Vec2::new(0.0, -*r)]);
        }
        for p in pts {
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
                            let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                            if let Some(slot) = parent.get_mut(a) {
                                *slot = b;
                            }
                        }
                    }
                }
            }
            grid.entry((cx, cy)).or_default().push((i, p));
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, e) in pool.ents.iter().enumerate() {
        if !e.flat {
            let r = find(&mut parent, i);
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
    /// Short side over long side of the extents.
    aspect: f64,
    /// Object lengths over the extents' diagonal, longest first.
    lens: Vec<f64>,
}

pub fn block_sig(b: &Block) -> Sig {
    let mut kinds: BTreeMap<&'static str, u32> = BTreeMap::new();
    let (mut lo, mut hi) = (Vec2::new(f64::MAX, f64::MAX), Vec2::new(f64::MIN, f64::MIN));
    let mut lens = Vec::new();
    for e in b.entities.iter() {
        *kinds.entry(e.kind.type_name()).or_default() += 1;
        if let Some(f) = feat_of(&e.kind) {
            let r = f.rad.first().copied().filter(|_| matches!(f.tag, Tag::Circle | Tag::Arc)).unwrap_or(0.0);
            for p in &f.pts {
                lo = lo.min(*p - Vec2::new(r, r));
                hi = hi.max(*p + Vec2::new(r, r));
            }
            lens.push(f.measure());
        }
    }
    let (w, h) = if lo.is_finite() && hi.is_finite() { (hi.x - lo.x, hi.y - lo.y) } else { (0.0, 0.0) };
    let (long, short) = (w.max(h), w.min(h));
    let diag = w.hypot(h);
    let aspect = if long > 0.0 { short / long } else { 1.0 };
    let mut lens: Vec<f64> = lens.into_iter().map(|l| if diag > 0.0 { l / diag } else { 0.0 }).collect();
    lens.sort_by(|a, b| b.total_cmp(a));
    lens.truncate(64);
    Sig { kinds, aspect, lens }
}

/// 0 for blocks of the same shape; grows with differences in object kinds, extents' aspect
/// and the distribution of object lengths.
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
