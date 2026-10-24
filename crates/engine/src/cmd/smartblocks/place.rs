//! Placement suggestion: how existing references of a block sit against the nearest wall
//! (line or polyline segment) is learned, and a new reference follows the way that fits the
//! cursor best.
//!
//! A relation is the signed perpendicular offset from the segment, the rotation relative to it
//! and the distance along it from the nearest anchor (an end of the segment, or a point where
//! another segment crosses or touches it: a wall corner). Segments are read in a canonical
//! direction (towards +X, or +Y when vertical), so a wall drawn from either end gives the same
//! relation. References that agree on offset and rotation make one group; when they also agree
//! on the distance from an anchor, the suggestion lands that far from the anchor nearest the
//! cursor, otherwise it follows the cursor along the wall.

use cadcraft_geom::{Line, Vec2, line_line, norm_angle};

/// Angles within this many radians count as the same relation.
const ANGLE_TOL: f64 = 1e-3;
/// Segment comparisons spent on finding anchors while learning; past it only the segment ends
/// count as anchors.
const ANCHOR_WORK: usize = 4_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Relation {
    /// Distance from the segment, positive on the left of its canonical direction.
    pub offset: f64,
    /// Block rotation minus the segment's canonical direction, radians in 0..2π.
    pub angle: f64,
    /// Distance along the segment from the nearest anchor.
    pub along: f64,
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
    /// The point sits at the learned distance from an anchor (not just where the cursor is).
    pub anchored: bool,
}

/// The segment nearest a point, read in its canonical direction.
struct Near {
    index: usize,
    /// The canonical start, the unit canonical direction and the length.
    start: Vec2,
    dir: Vec2,
    len: f64,
    /// The foot point's distance from `start`.
    s: f64,
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
        let (mut dir, mut start) = (d / len, *a);
        if dir.x < -1e-12 || (dir.x.abs() <= 1e-12 && dir.y < 0.0) {
            dir = -dir;
            start = *b;
        }
        let s = (p - start).dot(dir).clamp(0.0, len);
        let dist = p.dist(start + dir * s);
        if dist.is_finite() && best.as_ref().is_none_or(|n| dist < n.dist) {
            best = Some(Near { index, start, dir, len, s, dist });
        }
    }
    best
}

/// Distances along `n` of its ends and of every point where another segment crosses or
/// touches it, ascending.
fn anchors(segs: &[(Vec2, Vec2)], n: &Near) -> Vec<f64> {
    let wall = Line::new(n.start, n.start + n.dir * n.len);
    let mut v = vec![0.0, n.len];
    for (i, (a, b)) in segs.iter().enumerate() {
        if i != n.index
            && let Some(x) = line_line(&wall, &Line::new(*a, *b))
        {
            let s = (x - n.start).dot(n.dir);
            if s.is_finite() && s > 0.0 && s < n.len {
                v.push(s);
            }
        }
    }
    v.sort_by(f64::total_cmp);
    v
}

/// The relation of each reference `(insertion point, rotation)` to its nearest segment.
pub fn learn(refs: &[(Vec2, f64)], segs: &[(Vec2, Vec2)]) -> Vec<Relation> {
    let mut work = ANCHOR_WORK;
    refs.iter()
        .filter_map(|(p, rot)| {
            let n = nearest(segs, *p)?;
            let offset = (*p - (n.start + n.dir * n.s)).dot(n.dir.perp());
            let angle = norm_angle(rot - n.dir.angle());
            let anchors = if work >= segs.len() {
                work -= segs.len();
                anchors(segs, &n)
            } else {
                vec![0.0, n.len]
            };
            let along = anchors.iter().map(|a| (a - n.s).abs()).fold(f64::INFINITY, f64::min);
            (offset.is_finite() && angle.is_finite() && along.is_finite()).then_some(Relation { offset, angle, along })
        })
        .collect()
}

/// References sharing offset and rotation.
struct Group {
    relation: Relation,
    support: usize,
    /// The smallest and largest distance from an anchor among them.
    along: (f64, f64),
}

/// The placement of `g` on `n`: the learned distance from the anchor nearest the cursor when
/// the group agrees on one, else the cursor's own foot. Returns the distance from `n.start`.
fn position(g: &Group, n: &Near, segs: &[(Vec2, Vec2)], tol: f64) -> (f64, bool) {
    if g.along.1 - g.along.0 > tol {
        return (n.s, false);
    }
    let d = g.along.0;
    // An end anchor leads into the wall only; a crossing anchor either way.
    let mut cands: Vec<f64> = Vec::new();
    for a in anchors(segs, n) {
        cands.extend([a + d, a - d].into_iter().filter(|s| *s >= -tol && *s <= n.len + tol));
    }
    match cands.into_iter().map(|s| s.clamp(0.0, n.len)).min_by(|a, b| (a - n.s).abs().total_cmp(&(b - n.s).abs())) {
        Some(s) => (s, true),
        None => (n.s, false),
    }
}

/// The learned relation that fits `cursor` best, applied to the segment nearest it: among the
/// relations at least half as common as the most common one, the one whose placement lies
/// closest to the cursor (the most common first among equals). The insertion point follows the
/// segment's foot of the cursor, moved by the offset, unless the group agrees on the distance
/// from an anchor; the rotation follows the segment.
pub fn suggest(cursor: Vec2, segs: &[(Vec2, Vec2)], relations: &[Relation], tol: f64) -> Option<Suggestion> {
    let same = |a: &Relation, b: &Relation| {
        (a.offset - b.offset).abs() <= tol && norm_angle(a.angle - b.angle).min(norm_angle(b.angle - a.angle)) <= ANGLE_TOL
    };
    let mut groups: Vec<Group> = Vec::new();
    for r in relations {
        match groups.iter_mut().find(|g| same(&g.relation, r)) {
            Some(g) => {
                g.support += 1;
                g.along = (g.along.0.min(r.along), g.along.1.max(r.along));
            }
            None => groups.push(Group { relation: *r, support: 1, along: (r.along, r.along) }),
        }
    }
    let top = groups.iter().map(|g| g.support).max()?;
    let n = nearest(segs, cursor)?;
    let mut best: Option<(f64, Suggestion)> = None;
    for g in groups.iter().filter(|g| g.support * 2 >= top) {
        let (s, anchored) = position(g, &n, segs, tol);
        let at = n.start + n.dir * s + n.dir.perp() * g.relation.offset;
        let rotation = norm_angle(n.dir.angle() + g.relation.angle);
        let dist = at.dist(cursor);
        if !(at.is_finite() && rotation.is_finite() && dist.is_finite()) {
            continue;
        }
        // Strictly closer wins; equal distances keep the more supported (then the earlier) group.
        let better = best.as_ref().is_none_or(|(d, b)| dist < *d - 1e-12 || (dist <= *d + 1e-12 && g.support > b.support));
        if better {
            best = Some((dist, Suggestion { at, rotation, segment: n.index, relation: g.relation, support: g.support, anchored }));
        }
    }
    best.map(|(_, s)| s)
}
