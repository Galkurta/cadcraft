//! Placement suggestion: how existing references of a block sit against the nearest wall
//! (line or polyline segment) is learned, and a new reference follows the most common way.
//!
//! A relation is the signed perpendicular offset from the segment and the rotation relative to
//! it. Segments are read in a canonical direction (towards +X, or +Y when vertical), so a wall
//! drawn from either end gives the same relation.

use cadcraft_geom::{Vec2, norm_angle};

/// Angles within this many radians count as the same relation.
const ANGLE_TOL: f64 = 1e-3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Relation {
    /// Distance from the segment, positive on the left of its canonical direction.
    pub offset: f64,
    /// Block rotation minus the segment's canonical direction, radians in 0..2π.
    pub angle: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Suggestion {
    pub at: Vec2,
    pub rotation: f64,
    /// Index of the segment the insertion point follows.
    pub segment: usize,
    pub relation: Relation,
    /// How many existing references share the relation.
    pub support: usize,
}

/// The segment nearest a point: its index, unit canonical direction, the foot point on it and
/// the distance.
struct Near {
    index: usize,
    dir: Vec2,
    foot: Vec2,
    dist: f64,
}

fn nearest(segs: &[(Vec2, Vec2)], p: Vec2) -> Option<Near> {
    let mut best: Option<Near> = None;
    for (index, (a, b)) in segs.iter().enumerate() {
        let d = *b - *a;
        let len = d.len();
        if len <= 1e-9 || !len.is_finite() {
            continue;
        }
        let mut dir = d / len;
        if dir.x < -1e-12 || (dir.x.abs() <= 1e-12 && dir.y < 0.0) {
            dir = -dir;
        }
        let t = ((p - *a).dot(d) / (len * len)).clamp(0.0, 1.0);
        let foot = *a + d * t;
        let dist = p.dist(foot);
        if dist.is_finite() && best.as_ref().is_none_or(|n| dist < n.dist) {
            best = Some(Near { index, dir, foot, dist });
        }
    }
    best
}

/// The relation of each reference `(insertion point, rotation)` to its nearest segment.
pub fn learn(refs: &[(Vec2, f64)], segs: &[(Vec2, Vec2)]) -> Vec<Relation> {
    refs.iter()
        .filter_map(|(p, rot)| {
            let n = nearest(segs, *p)?;
            let offset = (*p - n.foot).dot(n.dir.perp());
            let angle = norm_angle(rot - n.dir.angle());
            (offset.is_finite() && angle.is_finite()).then_some(Relation { offset, angle })
        })
        .collect()
}

/// The most common of `relations` (offsets within `tol`; the earliest among equals), applied
/// to the segment nearest `cursor`: the insertion point is the cursor's foot on it moved by the
/// offset, the rotation follows the segment.
pub fn suggest(cursor: Vec2, segs: &[(Vec2, Vec2)], relations: &[Relation], tol: f64) -> Option<Suggestion> {
    let same = |a: &Relation, b: &Relation| {
        (a.offset - b.offset).abs() <= tol && norm_angle(a.angle - b.angle).min(norm_angle(b.angle - a.angle)) <= ANGLE_TOL
    };
    let (relation, support) = relations.iter().fold(None::<(Relation, usize)>, |best, r| {
        let n = relations.iter().filter(|o| same(r, o)).count();
        if best.is_none_or(|b| n > b.1) { Some((*r, n)) } else { best }
    })?;
    let n = nearest(segs, cursor)?;
    let at = n.foot + n.dir.perp() * relation.offset;
    let rotation = norm_angle(n.dir.angle() + relation.angle);
    (at.is_finite() && rotation.is_finite()).then_some(Suggestion { at, rotation, segment: n.index, relation, support })
}
