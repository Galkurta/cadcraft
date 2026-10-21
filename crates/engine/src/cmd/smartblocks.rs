//! Smart Blocks: BCONVERT, BSEARCH, BDETECT and BREPLACE find identical geometry by shape, not by
//! name, and INSERT can suggest where a block goes from how its references sit on walls.
//!
//! Shapes match by rotation, shift and (when `scaled`) uniform scale, never by mirroring; see
//! [`shape`]. No model, no network: the matching is plain geometry within a tolerance.

mod place;
mod shape;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use cadcraft_doc::{Drawing, Entity, EntityKind, Handle};
use cadcraft_geom::Vec2;
use serde_json::{Map, Value, json};

use self::shape::{Feat, Instance, MAX_GROUP, Opts, Pool, PoolEnt, Sim, Style};
use super::blocks::{Keep, insert_with, make_block, valid_block_name};
use super::machines::{SelOutcome, SelectPhase, SelectRun};
use super::{CommandSpec, bad, bool_or, point_param, size_param, str_param, targets};
use crate::{Accept, EngineError, Input, Interactive, Prompt, Result, Session, Step};

/// Comparisons one command may make before it reports what it found so far.
const SEARCH_BUDGET: u64 = 20_000_000;
/// Segments and references the placement suggestion looks at.
const MAX_SEGS: usize = 20_000;
const MAX_REFS: usize = 2_000;
/// Most ranked blocks BREPLACE reports.
const MAX_RANKED: usize = 50;
/// Objects whose shape BREPLACE `shapes` ranks by.
const MAX_SHAPES: usize = 2_000;
/// Offsets within this count as the same placement.
const PLACE_TOL: f64 = 1e-6;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("bconvert", "Convert to Block", run_bconvert)
            .menu(&["Insert", "Smart Blocks", "Convert to Block"])
            .params(
                "{handles? (default the selection), name? (default a new SMARTBLOCKn), base?: [x,y] (default the lower left of the objects), \
                 includeExisting? (also block references of identical geometry; default BCONVERTINCLUDEEXISTINGBLOCKS), layer0? (the block's objects on layer 0; default BCONVERTLAYER), \
                 scaled? (default false: also copies at another size), tolerance? (drawing units), dryRun? (default false: only count; the drawing is not changed) } → {block, converted, handles, notes?} \
                 (dryRun: {dryRun: true, block (the default name), instances (the objects and the identical ones found), truncated}): \
                 the objects and every identical instance in the drawing become references to one new block. In the app, BCONVERT typed or from a menu opens the Convert to Block dialog, which runs this with its choices",
            )
            .interactive(|_| Ok(Box::new(BConvertM::new(false)))),
        CommandSpec::new("-bconvert", "Convert to Block (command line)", run_bconvert)
            .params("as BCONVERT; the prompt sequence asks for the block name")
            .interactive(|_| Ok(Box::new(BConvertM::new(true)))),
        CommandSpec::new("bsearch", "Search for Similar Objects", run_bsearch)
            .menu(&["Insert", "Smart Blocks", "Search for Similar Objects"])
            .params(
                "{handles? (the sample; default the selection), scaled? (default true), includeExisting? (default false), tolerance?} → {count, handles, instances: [{handles, at, rotation (degrees), scale}]}: \
                 selects every instance of the sample's shape; the drawing is not changed",
            )
            .interactive(|_| Ok(Box::new(SelectRun::new("bsearch", "BSEARCH")))),
        CommandSpec::new("bdetect", "Detect Repeated Groups", run_bdetect)
            .menu(&["Insert", "Smart Blocks", "Detect Repeated Groups"])
            .params(
                "{scaled? (default false), gap? (touching distance), maxSize? (objects per group, at most 50, default 50), minSize? (default 2), tolerance?, \
                 convert? (default false), index? (the group to convert, default 0), name?, base?, layer0?} → {groups: [{count, size, score, instances: [{handles, at, rotation, scale}]}], converted?}: \
                 groups of touching objects that occur at least twice, best (count × size) first",
            ),
        CommandSpec::new("breplace", "Replace Block", run_breplace)
            .menu(&["Insert", "Smart Blocks", "Replace Block"])
            .params(
                "{handles? (block references; default the selection), with? (a block name), shapes? (default false: rank by the shape of any objects, not only block references)} → {ranking: [{name, distance, similarity}]} without `with`; \
                 with it the references take that block, keeping insertion point, rotation, scale, layer and the attribute values whose tags exist",
            )
            .interactive(|_| Ok(Box::new(BReplaceM::default()))),
    ]
}

// ---------- the sample ----------

struct Sample {
    handles: Vec<Handle>,
    feats: Vec<Feat>,
    styles: Vec<Option<Style>>,
    tol: f64,
}

fn unsupported(cmd: &str) -> EngineError {
    bad(
        cmd,
        format!(
            "{cmd} matches at most {MAX_GROUP} lines, polylines, arcs, circles, ellipses, points, text, hatches, splines, dimensions and block references"
        ),
    )
}

/// The distinct existing objects among `hs`.
fn sample_ents(s: &Session, hs: &[Handle], cmd: &str) -> Result<Vec<Arc<Entity>>> {
    let d = s.doc()?;
    let mut seen = HashSet::new();
    let ents: Vec<Arc<Entity>> = hs.iter().filter(|h| seen.insert(**h)).filter_map(|h| d.entity(*h).cloned()).collect();
    if ents.is_empty() { Err(bad(cmd, "no objects selected")) } else { Ok(ents) }
}

/// The tolerance when none is given: relative to the size and distance from the origin, so
/// copies made by rotating and moving (which lose a few bits) still match.
fn default_tol<'a>(feats: impl Iterator<Item = &'a Feat>) -> f64 {
    let (mut lo, mut hi) = (Vec2::new(f64::MAX, f64::MAX), Vec2::new(f64::MIN, f64::MIN));
    let mut far = 0.0f64;
    for p in feats.flat_map(|f| f.pts.iter()) {
        lo = lo.min(*p);
        hi = hi.max(*p);
        far = far.max(p.x.abs()).max(p.y.abs());
    }
    let diag = if lo.is_finite() && hi.is_finite() { (hi - lo).len() } else { 0.0 };
    1e-6 * diag + 1e-11 * far + 1e-9
}

/// The matcher's view of `ents`, `None` when any is unsupported or there are too many.
fn sample_of(ents: &[Arc<Entity>], tol: Option<f64>) -> Option<Sample> {
    if ents.len() > MAX_GROUP {
        return None;
    }
    let feats: Option<Vec<Feat>> = ents.iter().map(|e| shape::feat_of(&e.kind)).collect();
    let feats = feats?;
    let styles = ents.iter().map(|e| Some(shape::style_of(&e.common))).collect();
    let tol = tol.unwrap_or_else(|| default_tol(feats.iter()));
    Some(Sample { handles: ents.iter().map(|e| e.handle).collect(), feats, styles, tol })
}

fn tolerance(cmd: &str, p: &Value) -> Result<Option<f64>> {
    size_param(cmd, p, "tolerance", false)
}

fn include_existing(s: &Session, p: &Value) -> bool {
    bool_or(p, "includeExisting", s.doc().is_ok_and(|d| d.header.i64("BCONVERTINCLUDEEXISTINGBLOCKS", 0) != 0))
}

fn layer0(s: &Session, p: &Value) -> bool {
    bool_or(p, "layer0", s.doc().is_ok_and(|d| d.header.i64("BCONVERTLAYER", 0) != 0))
}

/// Every place the sample occurs in the active space (itself included), the sample's own first.
fn find(s: &Session, sm: &Sample, scaled: bool, existing: bool) -> Result<(Pool, shape::Found)> {
    let pool = Pool::of_space(s.doc()?, &s.space(), existing);
    let mut found = shape::search(&sm.feats, &sm.styles, &pool, &Opts { tol: sm.tol, scaled, budget: SEARCH_BUDGET });
    let mut own = sm.handles.clone();
    own.sort_by_key(|h| h.0);
    found.instances.sort_by_key(|i| {
        let mut o = owners(&pool, i);
        o.sort_by_key(|h| h.0);
        o != own
    });
    Ok((pool, found))
}

/// The objects an instance is made of: its own, or the one block reference it was flattened from.
fn owners(pool: &Pool, i: &Instance) -> Vec<Handle> {
    let mut seen = HashSet::new();
    i.members.iter().filter_map(|m| pool.ent(*m)).map(|e| e.owner).filter(|h| seen.insert(*h)).collect()
}

fn hex(hs: &[Handle]) -> Vec<String> {
    hs.iter().map(|h| h.hex()).collect()
}

fn degrees(r: f64) -> f64 {
    r.to_degrees()
}

/// Where a reference to a block based at `base` lands to reproduce the instance.
fn transform_json(sim: &Sim, base: Vec2) -> Value {
    let at = sim.apply(base);
    json!({ "at": [at.x, at.y], "rotation": degrees(sim.rot), "scale": sim.k })
}

/// The lower left corner of the objects: the default block base point.
fn default_base(d: &Drawing, ents: &[Arc<Entity>]) -> Vec2 {
    let b = ents.iter().fold(cadcraft_geom::Bounds2::EMPTY, |b, e| b.union(&cadcraft_doc::entity_bounds(d, e, 0)));
    if b.min.is_finite() { b.min } else { Vec2::ZERO }
}

fn fresh_name(d: &Drawing) -> String {
    (1..=1_000_000u32).map(|i| format!("SMARTBLOCK{i}")).find(|n| d.block(n).is_none()).unwrap_or_else(|| "SMARTBLOCK".into())
}

// ---------- BSEARCH ----------

fn run_bsearch(s: &mut Session, p: &Value) -> Result<Value> {
    let ents = sample_ents(s, &targets(s, p)?, "bsearch")?;
    let sm = sample_of(&ents, tolerance("bsearch", p)?).ok_or_else(|| unsupported("bsearch"))?;
    let (pool, found) = find(s, &sm, bool_or(p, "scaled", true), include_existing(s, p))?;
    let base = default_base(s.doc()?, &ents);
    let mut all: Vec<Handle> = Vec::new();
    let mut instances = Vec::new();
    for i in &found.instances {
        let hs = owners(&pool, i);
        let mut t = transform_json(&i.sim, base);
        t["handles"] = json!(hex(&hs));
        instances.push(t);
        all.extend(hs);
    }
    s.set_selection(all.clone());
    let mut seen = HashSet::new();
    all.retain(|h| seen.insert(*h));
    Ok(json!({
        "count": found.instances.len(),
        "handles": hex(&all),
        "instances": instances,
        "truncated": found.truncated,
        "message": format!("{} similar instance(s) selected.", found.instances.len()),
    }))
}

// ---------- BCONVERT ----------

/// Objects that become a reference to the new block.
struct Replace {
    remove: Vec<Handle>,
    sim: Sim,
    layer: String,
}

fn layer_of(d: &Drawing, hs: &[Handle]) -> String {
    hs.first().and_then(|h| d.entity(*h)).map_or_else(|| "0".into(), |e| e.common.layer.clone())
}

fn set_layer(s: &mut Session, h: Handle, layer: &str) -> Result<()> {
    s.doc_mut()?.modify_entity(h, |e| e.common.layer = layer.to_string())?;
    Ok(())
}

/// Define `name` from `sample` (a reference replaces them) and replace each of `repl` by a
/// reference placed by its transform.
fn convert(s: &mut Session, sample: &[Handle], name: &str, base: Vec2, to_layer0: bool, repl: &[Replace]) -> Result<Vec<Handle>> {
    let layer = layer_of(s.doc()?, sample);
    let first = make_block(s, name, base, sample, Keep::Convert, "")?.ok_or_else(|| EngineError::Other("the block was not made".into()))?;
    set_layer(s, first, &layer)?;
    if to_layer0 {
        let d = s.doc_mut()?;
        if let Some(b) = d.blocks.get_mut(name) {
            let b = Arc::make_mut(b);
            for h in b.entities.handles() {
                b.entities.modify(h, |e| e.common.layer = "0".into());
            }
        }
    }
    let mut made = vec![first];
    for r in repl {
        for h in &r.remove {
            s.doc_mut()?.remove_entity(*h);
        }
        let h = insert_with(s, name, r.sim.apply(base), Vec2::new(r.sim.k, r.sim.k), r.sim.rot, None, &Map::new())?;
        set_layer(s, h, &r.layer)?;
        made.push(h);
    }
    s.set_selection(made.clone());
    Ok(made)
}

fn new_block_name(s: &Session, p: &Value, cmd: &str) -> Result<String> {
    let d = s.doc()?;
    let Some(n) = str_param(p, "name") else { return Ok(fresh_name(d)) };
    let n = n.trim();
    if !valid_block_name(n) {
        return Err(bad(cmd, "invalid block name"));
    }
    if d.block(n).is_some() {
        return Err(bad(cmd, format!("Block \"{n}\" already exists.")));
    }
    Ok(n.to_string())
}

fn run_bconvert(s: &mut Session, p: &Value) -> Result<Value> {
    let ents = sample_ents(s, &targets(s, p)?, "bconvert")?;
    let dry = bool_or(p, "dryRun", false);
    // A count only: the name given does not matter yet; the one that would be used by default is reported.
    let name = if dry { fresh_name(s.doc()?) } else { new_block_name(s, p, "bconvert")? };
    let base = point_param(p, "base").unwrap_or_else(|| s.doc().map_or(Vec2::ZERO, |d| default_base(d, &ents)));
    let to_layer0 = layer0(s, p);
    let handles: Vec<Handle> = ents.iter().map(|e| e.handle).collect();
    let mut notes = Vec::new();
    let mut repl = Vec::new();
    match sample_of(&ents, tolerance("bconvert", p)?) {
        None => notes.push(format!("Only the selection was converted: identical instances are found among at most {MAX_GROUP} lines, polylines, arcs, circles, ellipses, points, text, hatches, splines, dimensions and block references.")),
        Some(sm) => {
            let (pool, found) = find(s, &sm, bool_or(p, "scaled", false), include_existing(s, p))?;
            let mut taken: HashSet<Handle> = handles.iter().copied().collect();
            for i in &found.instances {
                let hs = owners(&pool, i);
                if hs.iter().any(|h| taken.contains(h)) {
                    continue;
                }
                taken.extend(hs.iter().copied());
                repl.push(Replace { remove: hs.clone(), sim: i.sim, layer: layer_of(s.doc()?, &hs) });
            }
            if found.truncated {
                notes.push("The search was cut short; further identical instances may remain.".into());
            }
        }
    }
    if dry {
        let truncated = notes.iter().any(|n| n.starts_with("The search"));
        return Ok(json!({ "dryRun": true, "block": name, "instances": 1 + repl.len(), "truncated": truncated, "notes": notes }));
    }
    let made = convert(s, &handles, &name, base, to_layer0, &repl)?;
    let mut r = json!({ "block": name, "converted": made.len(), "handles": hex(&made) });
    if !notes.is_empty() {
        r["notes"] = json!(notes);
    }
    r["message"] = json!(format!("Block \"{name}\" made; {} object group(s) converted.", made.len()));
    Ok(r)
}

enum ConvertAsk {
    Select,
    Name,
}

struct BConvertM {
    dash: bool,
    ask: ConvertAsk,
    sel: SelectPhase,
    picked: Vec<Handle>,
}

impl BConvertM {
    fn new(dash: bool) -> Self {
        BConvertM { dash, ask: ConvertAsk::Select, sel: SelectPhase::default(), picked: Vec::new() }
    }

    fn run(&self, s: &mut Session, name: Option<String>) -> Result<Step> {
        let mut p = json!({ "handles": hex(&self.picked) });
        if let Some(n) = name {
            p["name"] = json!(n);
        }
        let r = run_bconvert(s, &p)?;
        if let Some(m) = r.get("message").and_then(Value::as_str) {
            s.echo(m.to_string());
        }
        Ok(Step::Done)
    }

    fn selected(&mut self, s: &mut Session, hs: Vec<Handle>) -> Result<Step> {
        self.picked = hs;
        if self.dash {
            self.ask = ConvertAsk::Name;
            return Ok(Step::Continue);
        }
        self.run(s, None)
    }
}

impl Interactive for BConvertM {
    fn name(&self) -> &'static str {
        if self.dash { "-BCONVERT" } else { "BCONVERT" }
    }
    fn begin(&mut self, s: &mut Session) -> Result<Step> {
        self.sel = SelectPhase::begin(s);
        if self.sel.done {
            let hs = self.sel.picked.clone();
            return self.selected(s, hs);
        }
        Ok(Step::Continue)
    }
    fn prompt(&self, s: &Session) -> Prompt {
        match self.ask {
            ConvertAsk::Select => self.sel.prompt(),
            ConvertAsk::Name => Prompt::new("Enter block name", Accept::TEXT).default(s.doc().map(fresh_name).unwrap_or_default()),
        }
    }
    fn input(&mut self, s: &mut Session, i: Input) -> Result<Step> {
        match self.ask {
            ConvertAsk::Select => match self.sel.feed(s, &i)? {
                SelOutcome::More => Ok(Step::Continue),
                SelOutcome::Empty => Ok(Step::Done),
                SelOutcome::Done(hs) => self.selected(s, hs),
            },
            ConvertAsk::Name => match i {
                Input::Text(t) if !t.trim().is_empty() => self.run(s, Some(t.trim().to_string())),
                Input::Enter => self.run(s, None),
                _ => Ok(Step::Continue),
            },
        }
    }
}

// ---------- BDETECT ----------

/// Touching objects that occur at least twice, with each occurrence's transform from the first.
struct Group {
    count: usize,
    size: usize,
    /// Per occurrence: its objects (in the first occurrence's order) and the transform.
    instances: Vec<(Vec<Handle>, Sim)>,
}

fn run_bdetect(s: &mut Session, p: &Value) -> Result<Value> {
    let scaled = bool_or(p, "scaled", false);
    let max_size = match p.get("maxSize").and_then(Value::as_u64) {
        Some(n) => usize::try_from(n).unwrap_or(MAX_GROUP).clamp(2, MAX_GROUP),
        None => MAX_GROUP,
    };
    let min_size = p.get("minSize").and_then(Value::as_u64).map_or(2, |n| usize::try_from(n).unwrap_or(2).max(2));
    let pool = Pool::of_space(s.doc()?, &s.space(), false);
    let tol = tolerance("bdetect", p)?.unwrap_or_else(|| default_tol(pool.ents.iter().map(|e| &e.feat)));
    let gap = size_param("bdetect", p, "gap", false)?.unwrap_or(tol);
    let mut left = SEARCH_BUDGET;
    let clusters = shape::clusters(&pool, gap, max_size, &mut left);
    let mut buckets: BTreeMap<Vec<(u8, u32)>, Vec<usize>> = BTreeMap::new();
    for (ci, c) in clusters.iter().enumerate() {
        if c.len() >= min_size {
            buckets.entry(shape::histogram(&pool, c)).or_default().push(ci);
        }
    }
    let ents_of = |ci: usize| -> Vec<PoolEnt> { clusters.get(ci).into_iter().flatten().filter_map(|i| pool.ent(*i).cloned()).collect() };
    let mut groups: Vec<Group> = Vec::new();
    let mut truncated = pool.truncated || left == 0;
    'buckets: for mut todo in buckets.into_values() {
        while !todo.is_empty() {
            let first = todo.remove(0);
            let sample_ents = ents_of(first);
            let feats: Vec<Feat> = sample_ents.iter().map(|e| e.feat.clone()).collect();
            let styles: Vec<Option<Style>> = sample_ents.iter().map(|e| e.style.clone()).collect();
            let mut inst = vec![(sample_ents.iter().map(|e| e.owner).collect::<Vec<_>>(), Sim::IDENTITY)];
            let mut rest = Vec::new();
            for cj in todo {
                let cand = ents_of(cj);
                let cpool = Pool::new(cand.clone(), false);
                let found = shape::search(&feats, &styles, &cpool, &Opts { tol, scaled, budget: left });
                left = found.left;
                // Instance members follow the sample's order; the owners keep the order of the sample's objects.
                match found.instances.first() {
                    Some(i) => inst.push((owners(&cpool, i), i.sim)),
                    None => rest.push(cj),
                }
                if left == 0 {
                    truncated = true;
                    if inst.len() > 1 {
                        groups.push(Group { count: inst.len(), size: feats.len(), instances: inst });
                    }
                    break 'buckets;
                }
            }
            if inst.len() > 1 {
                groups.push(Group { count: inst.len(), size: feats.len(), instances: inst });
            }
            todo = rest;
        }
    }
    let first_handle = |g: &Group| g.instances.first().and_then(|i| i.0.first()).map(|h| h.0);
    groups
        .sort_by(|a, b| (b.count * b.size).cmp(&(a.count * a.size)).then(b.count.cmp(&a.count)).then_with(|| first_handle(a).cmp(&first_handle(b))));
    let to_base = |g: &Group| -> Result<Vec2> {
        let d = s.doc()?;
        let ents: Vec<Arc<Entity>> = g.instances.first().into_iter().flat_map(|i| i.0.iter()).filter_map(|h| d.entity(*h).cloned()).collect();
        Ok(point_param(p, "base").unwrap_or_else(|| default_base(d, &ents)))
    };
    let mut out = Vec::new();
    for g in &groups {
        let base = to_base(g)?;
        let inst: Vec<Value> = g
            .instances
            .iter()
            .map(|(hs, sim)| {
                let mut t = transform_json(sim, base);
                t["handles"] = json!(hex(hs));
                t
            })
            .collect();
        out.push(json!({ "count": g.count, "size": g.size, "score": g.count * g.size, "instances": inst }));
    }
    let mut r = json!({ "groups": out, "truncated": truncated });
    if bool_or(p, "convert", false) {
        let idx = p.get("index").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
        let g = groups.get(idx).ok_or_else(|| bad("bdetect", format!("no repeated group number {idx}")))?;
        let name = new_block_name(s, p, "bdetect")?;
        let base = to_base(g)?;
        let to_layer0 = layer0(s, p);
        let (sample, others) = g.instances.split_first().ok_or_else(|| bad("bdetect", "empty group"))?;
        let repl: Vec<Replace> =
            others.iter().filter_map(|(hs, sim)| Some(Replace { remove: hs.clone(), sim: *sim, layer: layer_of(s.doc().ok()?, hs) })).collect();
        let made = convert(s, &sample.0, &name, base, to_layer0, &repl)?;
        r["converted"] = json!({ "block": name, "converted": made.len(), "handles": hex(&made) });
    }
    r["message"] = json!(format!("{} repeated group(s) found.", groups.len()));
    Ok(r)
}

// ---------- BREPLACE ----------

/// The block references among `hs`.
fn references(d: &Drawing, hs: &[Handle]) -> Vec<Handle> {
    let mut seen = HashSet::new();
    hs.iter().filter(|h| seen.insert(**h)).filter(|h| d.entity(**h).is_some_and(|e| matches!(e.kind, EntityKind::Insert(_)))).copied().collect()
}

/// Every other block by how closely its shape resembles the blocks the references among `hs`
/// use; with `shapes`, also the other objects of `hs`, as one more shape.
fn rank(d: &Drawing, hs: &[Handle], shapes: bool) -> Vec<(String, f64)> {
    let mut used: Vec<String> = Vec::new();
    let mut others: Vec<&EntityKind> = Vec::new();
    let mut seen = HashSet::new();
    for e in hs.iter().filter(|h| seen.insert(**h)).filter_map(|h| d.entity(*h)) {
        match &e.kind {
            EntityKind::Insert(i) => {
                if !used.iter().any(|u| u.eq_ignore_ascii_case(&i.block)) {
                    used.push(i.block.clone());
                }
            }
            k if shapes && others.len() < MAX_SHAPES => others.push(k),
            _ => {}
        }
    }
    let mut sources: Vec<shape::Sig> = used.iter().filter_map(|n| d.block(n)).map(|b| shape::block_sig(b)).collect();
    if !others.is_empty() {
        sources.push(shape::sig_of(others.into_iter()));
    }
    if sources.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<(String, f64)> = d
        .blocks
        .values()
        .filter(|b| !b.anonymous && !used.iter().any(|u| u.eq_ignore_ascii_case(&b.name)))
        .map(|b| {
            let sig = shape::block_sig(b);
            let dist = sources.iter().map(|s| shape::distance(s, &sig)).sum::<f64>() / sources.len() as f64;
            (b.name.clone(), dist)
        })
        .collect();
    out.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(MAX_RANKED);
    out
}

fn ranking_json(r: &[(String, f64)]) -> Vec<Value> {
    r.iter().map(|(n, d)| json!({ "name": n, "distance": d, "similarity": 1.0 / (1.0 + d) })).collect()
}

/// Point each reference at block `with`, in place.
fn replace_refs(s: &mut Session, refs: &[Handle], with: &str) -> Result<(String, usize, usize)> {
    let blk = s.doc()?.block(with).cloned().ok_or_else(|| EngineError::Other(format!("Block \"{with}\" not found.")))?;
    let name = blk.name.clone();
    let (mut done, mut locked) = (0, 0);
    for h in refs {
        let Some(e) = s.doc()?.entity(*h).cloned() else { continue };
        let EntityKind::Insert(old) = &e.kind else { continue };
        if s.doc()?.layer(&e.common.layer).is_some_and(|l| l.locked) {
            locked += 1;
            continue;
        }
        let values: Map<String, Value> = old.attribs.iter().map(|a| (a.tag.clone(), Value::String(a.text.value.clone()))).collect();
        // The attributes the new block defines, placed for this reference, as a fresh INSERT would make them.
        let tmp = insert_with(s, &name, old.insert.xy(), Vec2::new(old.scale.x, old.scale.y), old.rotation, None, &values)?;
        let attribs = match s.doc()?.entity(tmp).map(|t| t.kind.clone()) {
            Some(EntityKind::Insert(i)) => i.attribs,
            _ => Vec::new(),
        };
        s.doc_mut()?.remove_entity(tmp);
        s.doc_mut()?.modify_entity(*h, |e| {
            if let EntityKind::Insert(i) = &mut e.kind {
                i.block = name.clone();
                i.attribs = attribs;
            }
        })?;
        done += 1;
    }
    Ok((name, done, locked))
}

fn run_breplace(s: &mut Session, p: &Value) -> Result<Value> {
    let hs = targets(s, p)?;
    let refs = references(s.doc()?, &hs);
    let shapes = bool_or(p, "shapes", false);
    if refs.is_empty() && !(shapes && !hs.is_empty()) {
        return Err(bad("breplace", "select block references"));
    }
    let Some(with) = str_param(p, "with") else {
        let ranking = rank(s.doc()?, &hs, shapes);
        return Ok(json!({ "ranking": ranking_json(&ranking) }));
    };
    if refs.is_empty() {
        return Err(bad("breplace", "select block references"));
    }
    let (name, done, locked) = replace_refs(s, &refs, with)?;
    s.set_selection(refs.clone());
    let mut r = json!({ "block": name, "replaced": done, "handles": hex(&refs) });
    if locked > 0 {
        r["message"] = json!(format!("{locked} reference(s) on a locked layer were left alone."));
    }
    Ok(r)
}

#[derive(Default)]
struct BReplaceM {
    sel: SelectPhase,
    refs: Vec<Handle>,
    ranked: Vec<String>,
    picking: bool,
}

impl BReplaceM {
    fn selected(&mut self, s: &mut Session, hs: &[Handle]) -> Result<Step> {
        self.refs = references(s.doc()?, hs);
        if self.refs.is_empty() {
            s.echo("No block references selected.");
            return Ok(Step::Done);
        }
        self.ranked = rank(s.doc()?, &self.refs, false).into_iter().map(|(n, _)| n).collect();
        if self.ranked.is_empty() {
            s.echo("There is no other block to replace with.");
            return Ok(Step::Done);
        }
        s.echo(format!("Most similar blocks: {}", self.ranked.iter().take(10).cloned().collect::<Vec<_>>().join(", ")));
        self.picking = true;
        Ok(Step::Continue)
    }
}

impl Interactive for BReplaceM {
    fn name(&self) -> &'static str {
        "BREPLACE"
    }
    fn begin(&mut self, s: &mut Session) -> Result<Step> {
        self.sel = SelectPhase::begin(s);
        if self.sel.done {
            let hs = self.sel.picked.clone();
            return self.selected(s, &hs);
        }
        Ok(Step::Continue)
    }
    fn prompt(&self, _s: &Session) -> Prompt {
        if !self.picking {
            return self.sel.prompt();
        }
        let kws: Vec<&str> = self.ranked.iter().take(10).map(String::as_str).collect();
        let p = Prompt::new("Enter the replacement block", Accept::TEXT).kw(&kws);
        match self.ranked.first() {
            Some(top) => p.default(top.clone()),
            None => p,
        }
    }
    fn input(&mut self, s: &mut Session, i: Input) -> Result<Step> {
        if !self.picking {
            return match self.sel.feed(s, &i)? {
                SelOutcome::More => Ok(Step::Continue),
                SelOutcome::Empty => Ok(Step::Done),
                SelOutcome::Done(hs) => self.selected(s, &hs),
            };
        }
        let choice = match &i {
            Input::Keyword(k) | Input::Text(k) => k.trim().to_string(),
            Input::Enter => self.ranked.first().cloned().unwrap_or_default(),
            _ => return Ok(Step::Continue),
        };
        let Some(name) = self.ranked.iter().find(|n| n.eq_ignore_ascii_case(&choice)).cloned() else {
            s.echo(format!("Block \"{choice}\" is not in the list."));
            return Ok(Step::Continue);
        };
        let (name, done, _) = replace_refs(s, &self.refs, &name)?;
        s.echo(format!("{done} reference(s) now use block \"{name}\"."));
        Ok(Step::Done)
    }
}

// ---------- INSERT placement ----------

/// Where a new reference to `name` goes when its insertion point is near `cursor`: the way the
/// block's existing references sit against the nearest line or polyline segment of the active
/// space, most common first. The position along the segment follows the cursor.
pub(crate) fn suggest_placement(s: &Session, name: &str, cursor: Vec2) -> Option<place::Suggestion> {
    let d = s.doc().ok()?;
    let store = d.space(&s.space())?;
    let mut segs: Vec<(Vec2, Vec2)> = Vec::new();
    let mut refs: Vec<(Vec2, f64)> = Vec::new();
    for e in store.iter() {
        if !d.is_visible(e) {
            continue;
        }
        match &e.kind {
            EntityKind::Line(l) if segs.len() < MAX_SEGS => segs.push((l.a.xy(), l.b.xy())),
            EntityKind::LwPolyline(pl) => {
                let n = pl.vertices.len();
                let count = if pl.closed { n } else { n.saturating_sub(1) };
                for i in 0..count {
                    if let (Some(a), Some(b)) = (pl.vertices.get(i), pl.vertices.get((i + 1) % n))
                        && a.bulge.abs() < 1e-9
                        && segs.len() < MAX_SEGS
                    {
                        segs.push((a.p, b.p));
                    }
                }
            }
            EntityKind::Insert(i) if i.block.eq_ignore_ascii_case(name) && refs.len() < MAX_REFS => refs.push((i.insert.xy(), i.rotation)),
            _ => {}
        }
    }
    place::suggest(cursor, &segs, &place::learn(&refs, &segs), PLACE_TOL)
}

#[cfg(test)]
#[path = "smartblocks_tests.rs"]
mod tests;
