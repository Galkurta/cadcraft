//! Placement relations kept across drawings: how references of a block sit against walls
//! (offset, rotation, distance from a wall end), per block name and per shape signature hash so a
//! renamed copy of a block finds them. The engine only holds the data; the host saves and loads it
//! as JSON ([`Learned::to_json`], [`Learned::from_json`]) and decides where it lives.
//!
//! Capped at [`MAX_BLOCKS`] blocks of at most [`MAX_RELATIONS`] relations each; the block used
//! longest ago is dropped first.

use serde_json::{Value, json};

use super::place::Relation;

/// Blocks kept.
pub const MAX_BLOCKS: usize = 200;
/// Relations kept per block.
pub const MAX_RELATIONS: usize = 32;
/// Longest block name kept, in characters.
const MAX_NAME: usize = 255;

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    name: String,
    /// The block's shape signature hash; 0 when it has no shape to compare.
    sig: u64,
    relations: Vec<Relation>,
}

/// The learned relations, least recently used block first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Learned {
    entries: Vec<Entry>,
}

fn f64_at(v: &Value, i: usize) -> Option<f64> {
    v.get(i)?.as_f64().filter(|f| f.is_finite())
}

/// `[offset, angle, along]`, all finite.
fn relation_of(v: &Value) -> Option<Relation> {
    Some(Relation { offset: f64_at(v, 0)?, angle: f64_at(v, 1)?, along: f64_at(v, 2)? })
}

impl Learned {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Block names, most recently used first, at most `n`.
    pub fn recent(&self, n: usize) -> Vec<String> {
        self.entries.iter().rev().take(n).map(|e| e.name.clone()).collect()
    }

    fn take(&mut self, name: &str) -> Option<Entry> {
        let i = self.entries.iter().position(|e| e.name.eq_ignore_ascii_case(name))?;
        Some(self.entries.remove(i))
    }

    fn put(&mut self, e: Entry) {
        self.take(&e.name);
        self.entries.push(e);
        let extra = self.entries.len().saturating_sub(MAX_BLOCKS);
        self.entries.drain(..extra);
    }

    /// The relations to place `name` by: its own, else those of the most recently used block of
    /// the same shape (`sig`, when not 0). Empty when nothing is known.
    pub fn lookup(&self, name: &str, sig: u64) -> &[Relation] {
        let named = self.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name) && !e.relations.is_empty());
        let same_shape = || self.entries.iter().rev().find(|e| sig != 0 && e.sig == sig && !e.relations.is_empty());
        named.or_else(same_shape).map_or(&[], |e| e.relations.as_slice())
    }

    /// Note `name` as used just now, with `found`, the relations of its latest references in the
    /// drawing (oldest first). ponytail: a drawing showing at least as many relations as are
    /// kept replaces them, otherwise the newest one is appended; no merging by agreement.
    pub fn learn(&mut self, name: &str, sig: u64, found: &[Relation]) {
        if name.is_empty() || name.chars().count() > MAX_NAME {
            return;
        }
        let mut relations = self.take(name).map(|e| e.relations).unwrap_or_default();
        if let Some(last) = found.last() {
            if found.len() >= relations.len() {
                relations = found.to_vec();
            } else {
                relations.push(*last);
            }
        }
        let extra = relations.len().saturating_sub(MAX_RELATIONS);
        relations.drain(..extra);
        self.put(Entry { name: name.to_string(), sig, relations });
    }

    /// `[{name, sig (hex string), relations: [[offset, angle, along]]}]`, oldest first.
    pub fn to_json(&self) -> Value {
        Value::Array(
            self.entries
                .iter()
                .map(|e| json!({ "name": e.name, "sig": format!("{:x}", e.sig), "relations": e.relations.iter().map(|r| [r.offset, r.angle, r.along]).collect::<Vec<_>>() }))
                .collect(),
        )
    }

    /// Read what [`Self::to_json`] wrote. Anything else (not an array, entries without a name,
    /// non-finite numbers, too many of them) is dropped or cut to the caps; never an error.
    pub fn from_json(v: &Value) -> Learned {
        let mut out = Learned::default();
        let Some(a) = v.as_array() else { return out };
        for e in a.iter().skip(a.len().saturating_sub(MAX_BLOCKS)) {
            let Some(name) = e.get("name").and_then(Value::as_str).filter(|n| !n.is_empty() && n.chars().count() <= MAX_NAME) else { continue };
            let sig = match e.get("sig") {
                Some(Value::String(h)) => u64::from_str_radix(h, 16).unwrap_or(0),
                Some(n) => n.as_u64().unwrap_or(0),
                None => 0,
            };
            let rels: &[Value] = e.get("relations").and_then(Value::as_array).map_or(&[], Vec::as_slice);
            let relations: Vec<Relation> = rels.iter().skip(rels.len().saturating_sub(MAX_RELATIONS)).filter_map(relation_of).collect();
            out.put(Entry { name: name.to_string(), sig, relations });
        }
        out
    }
}
