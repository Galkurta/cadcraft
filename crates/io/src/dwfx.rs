//! DWFx (OPC package of XPS FixedPages, ECMA-388) sheets.
//!
//! The package is walked as OPC does: `_rels/.rels` names the FixedDocumentSequence, which lists
//! FixedDocuments, which list FixedPages. Each page's XML is read into a small capped arena DOM
//! and drawn into a [`Scene`]: Canvas (transform, clip, opacity), Path (mini-language and
//! PathGeometry), Glyphs (drawn with an installed font, else the stroke font, because the
//! embedded fonts are obfuscated) and solid, gradient and image brushes. What is not understood
//! (visual brushes, opacity masks' effect, unknown elements) is counted in `Scene::skipped`.
//!
//! The package is hostile input: parts, nodes, depth, segments, text and picture pixels are all
//! capped, there is no recursion over the document (an explicit stack), and nothing panics.

use std::collections::HashMap;
use std::sync::Arc;

use cadcraft_fonts::TextFont;
use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use tiny_skia::{FillRule, LineCap, LineJoin, Path, PathBuilder, Pixmap, Transform};

use crate::underlay::{Brush, Op, Scene, StrokeStyle};
use crate::zip::Zip;

/// Largest XML part read.
const MAX_XML: usize = 64 << 20;
/// Largest picture part read.
const MAX_IMAGE: usize = 32 << 20;
const MAX_NODES: usize = 2_000_000;
const MAX_DEPTH: usize = 128;
/// Path segments drawn on one page, across all paths (a shared geometry can be reused).
const MAX_SEGMENTS: usize = 20_000_000;
/// Decoded picture pixels on one page.
const MAX_PIXELS: u64 = 32 << 20;
/// Characters of text on one page.
const MAX_TEXT: usize = 5_000_000;
const MAX_DOCS: usize = 10_000;
const MAX_PAGES: usize = 100_000;
/// XPS units are 1/96 inch; the scene is in points.
const PT: f32 = 0.75;
/// Cap height of a font as a share of its em.
const CAP_EM: f64 = 0.72;

// ---------------------------------------------------------------- package

pub(crate) fn is_dwfx(zip: &Zip) -> bool {
    zip.contains("[Content_Types].xml") && (sequence_part(zip).is_some() || zip.names().any(|n| ends_with_ci(n, ".fpage")))
}

pub(crate) fn sheets(zip: &Zip) -> Result<Vec<String>, String> {
    let parts = page_parts(zip)?;
    Ok(parts.iter().enumerate().map(|(i, p)| page_name(zip, p).unwrap_or_else(|| format!("Sheet {}", i + 1))).collect())
}

pub(crate) fn scene(zip: &Zip, index: usize) -> Result<Scene, String> {
    let parts = page_parts(zip)?;
    let part = parts.get(index).ok_or_else(|| format!("the file has no sheet {}", index.saturating_add(1)))?;
    let dom = parse_xml(&read_text(zip, part)?)?;
    if dom.name(0) != "FixedPage" {
        return Err(format!("part `{part}` is not a FixedPage"));
    }
    let size = |k: &str| dom.attr(0, k).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0).map(|v| v.min(1e7));
    let (w, h) = size("Width").zip(size("Height")).ok_or("the FixedPage has no size")?;
    let mut scene = Scene::new(w * f64::from(PT), h * f64::from(PT));
    scene.skipped += dom.dropped;
    let mut res = HashMap::new();
    for n in 0..dom.nodes.len() {
        if dom.name(n) == "ResourceDictionary" {
            for &k in dom.kids(n) {
                if let Some(key) = dom.attr(k, "Key") {
                    res.entry(key.to_string()).or_insert(k);
                }
            }
        }
    }
    let mut cx = Ctx {
        zip,
        dom: &dom,
        part,
        scene,
        res,
        fonts: HashMap::new(),
        images: HashMap::new(),
        pixels: MAX_PIXELS,
        segments: MAX_SEGMENTS,
        text: MAX_TEXT,
    };
    cx.run(St { ts: Transform::from_scale(PT, PT), clip: 0, opacity: 1.0 });
    Ok(cx.scene)
}

fn ends_with_ci(name: &str, suffix: &str) -> bool {
    name.to_ascii_lowercase().ends_with(suffix)
}

/// `target` resolved against the part `base` (OPC): no leading slash, `.` and `..` folded.
fn resolve(base: &str, target: &str) -> String {
    let t = target.split(['#', '?']).next().unwrap_or("");
    let mut parts: Vec<&str> = Vec::new();
    if !t.starts_with('/') {
        let base = base.trim_start_matches('/');
        if let Some((dir, _)) = base.rsplit_once('/') {
            parts.extend(dir.split('/').filter(|s| !s.is_empty()));
        }
    }
    for seg in t.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// The FixedDocumentSequence part: the package relationship, else the first `.fdseq`.
fn sequence_part(zip: &Zip) -> Option<String> {
    if let Ok(text) = read_text(zip, "_rels/.rels")
        && let Ok(dom) = parse_xml(&text)
    {
        for n in 0..dom.nodes.len() {
            if dom.name(n) == "Relationship"
                && dom.attr(n, "Type").is_some_and(|t| t.to_ascii_lowercase().ends_with("/fixedrepresentation"))
                && let Some(t) = dom.attr(n, "Target")
            {
                let part = resolve("", t);
                if zip.contains(&part) {
                    return Some(part);
                }
            }
        }
    }
    zip.names().find(|n| ends_with_ci(n, ".fdseq")).map(str::to_string)
}

/// The FixedPage parts in document order.
fn page_parts(zip: &Zip) -> Result<Vec<String>, String> {
    let mut pages = Vec::new();
    if let Some(seq) = sequence_part(zip)
        && let Ok(dom) = parse_xml(&read_text(zip, &seq)?)
    {
        let mut docs = 0;
        for n in 0..dom.nodes.len() {
            if dom.name(n) != "DocumentReference" {
                continue;
            }
            docs += 1;
            let (Some(src), true) = (dom.attr(n, "Source"), docs <= MAX_DOCS) else { continue };
            let doc = resolve(&seq, src);
            let Ok(text) = read_text(zip, &doc) else { continue };
            let Ok(d) = parse_xml(&text) else { continue };
            for p in 0..d.nodes.len() {
                if d.name(p) == "PageContent"
                    && let Some(s) = d.attr(p, "Source")
                    && pages.len() < MAX_PAGES
                {
                    pages.push(resolve(&doc, s));
                }
            }
        }
    }
    if pages.is_empty() {
        pages = zip.names().filter(|n| ends_with_ci(n, ".fpage")).take(MAX_PAGES).map(str::to_string).collect();
    }
    if pages.is_empty() { Err("the package has no pages".into()) } else { Ok(pages) }
}

/// The `Name` of the page's root element, if it has one.
fn page_name(zip: &Zip, part: &str) -> Option<String> {
    let text = read_text(zip, part).ok()?;
    let mut r = Reader::from_str(&text);
    loop {
        match r.read_event().ok()? {
            Event::Start(e) | Event::Empty(e) => {
                return attrs(&e).into_iter().find(|(k, v)| k == "Name" && !v.trim().is_empty()).map(|(_, v)| v);
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// A part as text: UTF-8, or UTF-16 with a byte order mark.
fn read_text(zip: &Zip, part: &str) -> Result<String, String> {
    let b = zip.read(part, MAX_XML)?;
    let utf16 = |le: bool, b: &[u8]| {
        let units = b.as_chunks::<2>().0.iter().map(|&p| if le { u16::from_le_bytes(p) } else { u16::from_be_bytes(p) });
        char::decode_utf16(units).map(|c| c.unwrap_or('\u{fffd}')).collect::<String>()
    };
    Ok(match b.as_slice() {
        [0xFF, 0xFE, rest @ ..] => utf16(true, rest),
        [0xFE, 0xFF, rest @ ..] => utf16(false, rest),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        all => String::from_utf8_lossy(all).into_owned(),
    })
}

// -------------------------------------------------------------------- DOM

struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    kids: Vec<usize>,
}

/// Elements and attributes with their namespace prefixes stripped; no text.
struct Dom {
    nodes: Vec<Node>,
    /// Elements left out for depth or count.
    dropped: usize,
}

impl Dom {
    fn name(&self, n: usize) -> &str {
        self.nodes.get(n).map_or("", |x| x.name.as_str())
    }
    fn attr(&self, n: usize, key: &str) -> Option<&str> {
        self.nodes.get(n)?.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    fn kids(&self, n: usize) -> &[usize] {
        self.nodes.get(n).map_or(&[][..], |x| x.kids.as_slice())
    }
}

fn local(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    s.rsplit(':').next().unwrap_or("").to_string()
}

fn attrs(e: &BytesStart) -> Vec<(String, String)> {
    e.attributes().flatten().filter_map(|a| Some((local(a.key.as_ref()), a.normalized_value(XmlVersion::Explicit1_0).ok()?.into_owned()))).collect()
}

fn parse_xml(text: &str) -> Result<Dom, String> {
    let mut r = Reader::from_str(text);
    let mut dom = Dom { nodes: Vec::new(), dropped: 0 };
    let mut stack: Vec<usize> = Vec::new();
    // Start tags inside a dropped element.
    let mut ignore = 0usize;
    let add = |dom: &mut Dom, stack: &[usize], e: &BytesStart| -> Option<usize> {
        if dom.nodes.len() >= MAX_NODES || (stack.is_empty() && !dom.nodes.is_empty()) {
            dom.dropped += 1;
            return None;
        }
        let i = dom.nodes.len();
        dom.nodes.push(Node { name: local(e.name().as_ref()), attrs: attrs(e), kids: Vec::new() });
        if let Some(p) = stack.last().and_then(|p| dom.nodes.get_mut(*p)) {
            p.kids.push(i);
        }
        Some(i)
    };
    loop {
        match r.read_event().map_err(|e| format!("the XML is damaged: {e}"))? {
            Event::Start(e) => {
                if ignore > 0 || stack.len() >= MAX_DEPTH {
                    ignore += 1;
                    dom.dropped += 1;
                } else if let Some(i) = add(&mut dom, &stack, &e) {
                    stack.push(i);
                } else {
                    ignore += 1;
                }
            }
            Event::Empty(e) => {
                if ignore == 0 && stack.len() < MAX_DEPTH {
                    add(&mut dom, &stack, &e);
                } else {
                    dom.dropped += 1;
                }
            }
            Event::End(_) => {
                if ignore > 0 {
                    ignore -= 1;
                } else {
                    stack.pop();
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if dom.nodes.is_empty() {
        return Err("the XML part is empty".into());
    }
    Ok(dom)
}

// ----------------------------------------------------------------- values

fn f(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

/// Numbers separated by commas or white space, up to the first bad one.
fn nums(s: &str) -> impl Iterator<Item = f32> + '_ {
    s.split(|c: char| c == ',' || c.is_whitespace()).filter(|t| !t.is_empty()).map_while(f)
}

fn quad(s: &str) -> Option<[f32; 4]> {
    let mut it = nums(s);
    Some([it.next()?, it.next()?, it.next()?, it.next()?])
}

fn pair(s: &str) -> Option<(f32, f32)> {
    let mut it = nums(s);
    Some((it.next()?, it.next()?))
}

fn is_true(s: Option<&str>) -> bool {
    s.is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

/// `m11,m12,m21,m22,dx,dy`.
fn matrix(s: &str) -> Option<Transform> {
    let v: Vec<f32> = nums(s).take(7).collect();
    let [a, b, c, d, e, g] = v.as_slice() else { return None };
    Some(Transform::from_row(*a, *b, *c, *d, *e, *g))
}

fn srgb(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let e = if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (e * 255.0).round().clamp(0.0, 255.0) as u8
}

/// `#AARRGGBB`, `#RRGGBB`, `sc#a,r,g,b`, `sc#r,g,b` or a few names, as straight RGBA.
fn color(s: &str) -> Option<[u8; 4]> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("sc#") {
        let v: Vec<f32> = nums(h).take(5).collect();
        return match v.as_slice() {
            [a, r, g, b] => Some([srgb(*r), srgb(*g), srgb(*b), (a.clamp(0.0, 1.0) * 255.0).round() as u8]),
            [r, g, b] => Some([srgb(*r), srgb(*g), srgb(*b), 255]),
            _ => None,
        };
    }
    if let Some(h) = s.strip_prefix('#') {
        if !h.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        let [a, r, g, b] = v.to_be_bytes();
        return match h.len() {
            8 => Some([r, g, b, a]),
            6 => Some([r, g, b, 255]),
            _ => None,
        };
    }
    match s.to_ascii_lowercase().as_str() {
        "transparent" => Some([0, 0, 0, 0]),
        "black" => Some([0, 0, 0, 255]),
        "white" => Some([255, 255, 255, 255]),
        _ => None,
    }
}

fn fade(c: [u8; 4], op: f32) -> [u8; 4] {
    [c[0], c[1], c[2], (f32::from(c[3]) * op.clamp(0.0, 1.0)).round() as u8]
}

/// The key of a `{StaticResource key}` reference.
fn static_res(v: &str) -> Option<&str> {
    let v = v.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
    Some(v.strip_prefix("StaticResource")?.trim())
}

// ------------------------------------------------------------ path builder

/// A path builder with a segment budget.
struct Pb {
    b: PathBuilder,
    cur: (f32, f32),
    start: (f32, f32),
    /// The last cubic's second control point (for `S`).
    c2: Option<(f32, f32)>,
    left: usize,
    over: bool,
}

impl Pb {
    fn new(left: usize) -> Self {
        Pb { b: PathBuilder::new(), cur: (0.0, 0.0), start: (0.0, 0.0), c2: None, left, over: false }
    }
    fn ok(&mut self) -> bool {
        if self.left == 0 {
            self.over = true;
            return false;
        }
        self.left -= 1;
        true
    }
    fn move_to(&mut self, x: f32, y: f32) {
        if self.ok() {
            self.b.move_to(x, y);
            self.cur = (x, y);
            self.start = (x, y);
            self.c2 = None;
        }
    }
    fn line_to(&mut self, x: f32, y: f32) {
        if self.ok() {
            self.b.line_to(x, y);
            self.cur = (x, y);
            self.c2 = None;
        }
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        if self.ok() {
            self.b.quad_to(x1, y1, x, y);
            self.cur = (x, y);
            self.c2 = None;
        }
    }
    fn cubic_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        if self.ok() {
            self.b.cubic_to(x1, y1, x2, y2, x, y);
            self.cur = (x, y);
            self.c2 = Some((x2, y2));
        }
    }
    fn close(&mut self) {
        if self.ok() {
            self.b.close();
            self.cur = self.start;
            self.c2 = None;
        }
    }

    /// An SVG-style elliptical arc from the current point (as cubic Béziers).
    fn arc(&mut self, rx: f32, ry: f32, rot: f32, large: bool, sweep: bool, x: f32, y: f32) {
        let (x0, y0) = self.cur;
        if x0 == x && y0 == y {
            return;
        }
        let (mut rx, mut ry) = (f64::from(rx.abs()), f64::from(ry.abs()));
        if rx == 0.0 || ry == 0.0 {
            return self.line_to(x, y);
        }
        let (s, c) = f64::from(rot).to_radians().sin_cos();
        let (x0, y0, x1, y1) = (f64::from(x0), f64::from(y0), f64::from(x), f64::from(y));
        let (dx, dy) = ((x0 - x1) / 2.0, (y0 - y1) / 2.0);
        let (xp, yp) = (c * dx + s * dy, -s * dx + c * dy);
        let lam = xp * xp / (rx * rx) + yp * yp / (ry * ry);
        if lam > 1.0 {
            let k = lam.sqrt();
            rx *= k;
            ry *= k;
        }
        let num = rx * rx * ry * ry - rx * rx * yp * yp - ry * ry * xp * xp;
        let den = rx * rx * yp * yp + ry * ry * xp * xp;
        let mut co = if den == 0.0 { 0.0 } else { (num / den).max(0.0).sqrt() };
        if large == sweep {
            co = -co;
        }
        let (cxp, cyp) = (co * rx * yp / ry, -co * ry * xp / rx);
        let (cx, cy) = (c * cxp - s * cyp + (x0 + x1) / 2.0, s * cxp + c * cyp + (y0 + y1) / 2.0);
        let ang = |ux: f64, uy: f64, vx: f64, vy: f64| (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
        let (ux, uy) = ((xp - cxp) / rx, (yp - cyp) / ry);
        let th = ang(1.0, 0.0, ux, uy);
        let mut dth = ang(ux, uy, (-xp - cxp) / rx, (-yp - cyp) / ry);
        if !sweep && dth > 0.0 {
            dth -= std::f64::consts::TAU;
        } else if sweep && dth < 0.0 {
            dth += std::f64::consts::TAU;
        }
        let n = (dth.abs() / std::f64::consts::FRAC_PI_2).ceil().clamp(1.0, 4.0);
        let d = dth / n;
        let t = 4.0 / 3.0 * (d / 4.0).tan();
        let map = |ux: f64, uy: f64| ((cx + c * rx * ux - s * ry * uy) as f32, (cy + s * rx * ux + c * ry * uy) as f32);
        for k in 0..n as u32 {
            let a1 = th + f64::from(k) * d;
            let a2 = a1 + d;
            let p1 = map(a1.cos() - t * a1.sin(), a1.sin() + t * a1.cos());
            let p2 = map(a2.cos() + t * a2.sin(), a2.sin() - t * a2.cos());
            let p3 = if k + 1 == n as u32 { (x, y) } else { map(a2.cos(), a2.sin()) };
            self.cubic_to(p1.0, p1.1, p2.0, p2.1, p3.0, p3.1);
        }
    }
}

fn sep(b: &[u8], i: &mut usize) {
    while matches!(b.get(*i), Some(b' ' | b',' | b'\t' | b'\r' | b'\n')) {
        *i += 1;
    }
}

fn digits(b: &[u8], i: &mut usize) {
    while b.get(*i).is_some_and(u8::is_ascii_digit) {
        *i += 1;
    }
}

fn num(b: &[u8], i: &mut usize) -> Option<f32> {
    sep(b, i);
    let s = *i;
    if matches!(b.get(*i), Some(b'+' | b'-')) {
        *i += 1;
    }
    digits(b, i);
    if b.get(*i) == Some(&b'.') {
        *i += 1;
        digits(b, i);
    }
    if matches!(b.get(*i), Some(b'e' | b'E')) {
        let save = *i;
        *i += 1;
        if matches!(b.get(*i), Some(b'+' | b'-')) {
            *i += 1;
        }
        let d = *i;
        digits(b, i);
        if *i == d {
            *i = save;
        }
    }
    f(std::str::from_utf8(b.get(s..*i)?).ok()?)
}

fn args<const N: usize>(b: &[u8], i: &mut usize) -> Option<[f32; N]> {
    let mut a = [0.0; N];
    for v in a.iter_mut() {
        *v = num(b, i)?;
    }
    Some(a)
}

/// One command's arguments. `cmd` becomes the implicit repeat command.
fn step(b: &[u8], i: &mut usize, cmd: &mut u8, pb: &mut Pb) -> Option<()> {
    let rel = cmd.is_ascii_lowercase();
    let (ox, oy) = if rel { pb.cur } else { (0.0, 0.0) };
    match cmd.to_ascii_uppercase() {
        b'M' => {
            let [x, y] = args(b, i)?;
            pb.move_to(ox + x, oy + y);
            *cmd = if rel { b'l' } else { b'L' };
        }
        b'L' => {
            let [x, y] = args(b, i)?;
            pb.line_to(ox + x, oy + y);
        }
        b'H' => {
            let [x] = args(b, i)?;
            pb.line_to(ox + x, pb.cur.1);
        }
        b'V' => {
            let [y] = args(b, i)?;
            pb.line_to(pb.cur.0, oy + y);
        }
        b'C' => {
            let [x1, y1, x2, y2, x, y] = args(b, i)?;
            pb.cubic_to(ox + x1, oy + y1, ox + x2, oy + y2, ox + x, oy + y);
        }
        b'S' => {
            let [x2, y2, x, y] = args(b, i)?;
            let (cx, cy) = pb.cur;
            let (x1, y1) = pb.c2.map_or((cx, cy), |(px, py)| (2.0 * cx - px, 2.0 * cy - py));
            pb.cubic_to(x1, y1, ox + x2, oy + y2, ox + x, oy + y);
        }
        b'Q' => {
            let [x1, y1, x, y] = args(b, i)?;
            pb.quad_to(ox + x1, oy + y1, ox + x, oy + y);
        }
        b'A' => {
            let [rx, ry, rot, large, sweep, x, y] = args(b, i)?;
            pb.arc(rx, ry, rot, large != 0.0, sweep != 0.0, ox + x, oy + y);
        }
        _ => return None,
    }
    Some(())
}

/// The path mini-language into `pb`; the fill rule of an `F0`/`F1` prefix.
fn parse_data(s: &str, pb: &mut Pb) -> Option<FillRule> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut rule = None;
    sep(b, &mut i);
    if matches!(b.get(i), Some(b'F' | b'f')) {
        i += 1;
        rule = Some(if b.get(i) == Some(&b'1') { FillRule::Winding } else { FillRule::EvenOdd });
        i += 1;
    }
    let mut cmd = 0u8;
    loop {
        sep(b, &mut i);
        let Some(&c) = b.get(i) else { break };
        if c.is_ascii_alphabetic() {
            cmd = c;
            i += 1;
            if matches!(c, b'Z' | b'z') {
                pb.close();
                continue;
            }
        } else if cmd == 0 || matches!(cmd, b'Z' | b'z') {
            break;
        }
        if step(b, &mut i, &mut cmd, pb).is_none() || pb.left == 0 {
            break;
        }
    }
    rule
}

// --------------------------------------------------------------- the page

#[derive(Clone, Copy)]
struct St {
    ts: Transform,
    clip: u32,
    opacity: f32,
}

/// An attribute value, or the element that stands for it (a property element or a resource).
#[derive(Clone, Copy)]
enum Prop<'a> {
    Str(&'a str),
    Node(usize),
}

struct Ctx<'a> {
    zip: &'a Zip<'a>,
    dom: &'a Dom,
    part: &'a str,
    scene: Scene,
    res: HashMap<String, usize>,
    fonts: HashMap<String, TextFont>,
    images: HashMap<String, Option<Arc<Pixmap>>>,
    pixels: u64,
    segments: usize,
    text: usize,
}

impl<'a> Ctx<'a> {
    fn run(&mut self, root: St) {
        let dom = self.dom;
        let mut stack: Vec<(usize, St)> = dom.kids(0).iter().rev().map(|&k| (k, root)).collect();
        while let Some((n, st)) = stack.pop() {
            match dom.name(n) {
                "Canvas" => {
                    if let Some(st) = self.frame(n, st) {
                        stack.extend(dom.kids(n).iter().rev().map(|&k| (k, st)));
                    }
                }
                "Path" => self.path(n, st),
                "Glyphs" => self.glyphs(n, st),
                name if name.contains('.') => {}
                _ => self.scene.skipped += 1,
            }
        }
    }

    /// Attribute `key` of `n`, or its property element's content, resolving `{StaticResource}`.
    fn prop(&self, n: usize, key: &str) -> Option<Prop<'a>> {
        let dom: &'a Dom = self.dom;
        if let Some(v) = dom.attr(n, key) {
            return match static_res(v) {
                Some(k) => self.res.get(k).map(|&i| Prop::Node(i)),
                None => Some(Prop::Str(v)),
            };
        }
        let want = format!("{}.{}", dom.name(n), key);
        let holder = dom.kids(n).iter().copied().find(|&k| dom.name(k) == want)?;
        dom.kids(holder).first().map(|&c| Prop::Node(c))
    }

    fn transform_of(&self, n: usize, key: &str) -> Option<Transform> {
        match self.prop(n, key)? {
            Prop::Str(s) => matrix(s),
            Prop::Node(i) if self.dom.name(i) == "MatrixTransform" => matrix(self.dom.attr(i, "Matrix")?),
            Prop::Node(_) => None,
        }
    }

    fn clip_paths(&self, id: u32) -> Vec<Path> {
        usize::try_from(id).ok().and_then(|i| i.checked_sub(1)).and_then(|i| self.scene.clips.get(i)).cloned().unwrap_or_default()
    }

    /// The state inside element `n`: its transform, clip and opacity applied; `None` when its
    /// clip is empty (nothing shows).
    fn frame(&mut self, n: usize, mut st: St) -> Option<St> {
        if let Some(m) = self.transform_of(n, "RenderTransform") {
            st.ts = st.ts.pre_concat(m);
        }
        if let Some(o) = self.dom.attr(n, "Opacity").and_then(f) {
            st.opacity *= o.clamp(0.0, 1.0);
        }
        if let Some(p) = self.prop(n, "Clip") {
            let clip = self.geometry(p, false).0.and_then(|c| c.transform(st.ts))?;
            let mut paths = self.clip_paths(st.clip);
            paths.push(clip);
            let id = self.scene.add_clip(paths);
            if id != 0 {
                st.clip = id;
            }
        }
        Some(st)
    }

    /// A geometry in its own space: (outline, fill rule, whether some figures are not filled).
    /// With `only_filled`, unfilled figures are left out.
    fn geometry(&mut self, p: Prop<'a>, only_filled: bool) -> (Option<Path>, FillRule, bool) {
        let dom: &'a Dom = self.dom;
        let mut pb = Pb::new(self.segments);
        let mut rule = FillRule::EvenOdd;
        let mut unfilled = false;
        let mut tr = None;
        match p {
            Prop::Str(s) => rule = parse_data(s, &mut pb).unwrap_or(rule),
            Prop::Node(i) if dom.name(i) == "PathGeometry" => {
                if dom.attr(i, "FillRule").is_some_and(|r| r.trim().eq_ignore_ascii_case("NonZero")) {
                    rule = FillRule::Winding;
                }
                if let Some(s) = dom.attr(i, "Figures") {
                    rule = parse_data(s, &mut pb).unwrap_or(rule);
                }
                let mut figs: Vec<usize> = Vec::new();
                for &k in dom.kids(i) {
                    match dom.name(k) {
                        "PathFigure" => figs.push(k),
                        "PathGeometry.Figures" => figs.extend(dom.kids(k).iter().copied().filter(|&c| dom.name(c) == "PathFigure")),
                        _ => {}
                    }
                }
                for fig in figs {
                    let filled = dom.attr(fig, "IsFilled").is_none_or(|v| is_true(Some(v)));
                    unfilled |= !filled;
                    if filled || !only_filled {
                        figure(dom, fig, &mut pb);
                    }
                }
                tr = self.transform_of(i, "Transform");
            }
            Prop::Node(_) => self.scene.skipped += 1,
        }
        self.segments = pb.left;
        if pb.over {
            self.scene.skipped += 1;
        }
        let mut path = pb.b.finish();
        if let Some(t) = tr {
            path = path.and_then(|p| p.transform(t));
        }
        (path, rule, unfilled)
    }

    fn path(&mut self, n: usize, st: St) {
        let Some(st) = self.frame(n, st) else { return };
        let Some(data) = self.prop(n, "Data") else { return };
        let (Some(all), rule, unfilled) = self.geometry(data, false) else { return };
        let b = all.bounds();
        let bb = [b.left(), b.top(), b.width(), b.height()];
        let fill = self.prop(n, "Fill").and_then(|p| self.brush(p, bb, st.ts, st.opacity)).map(|b| (b, rule));
        let stroke = self.stroke(n, st, bb);
        if fill.is_none() && stroke.is_none() {
            return;
        }
        let Some(scene_all) = all.transform(st.ts) else { return };
        if unfilled {
            if let Some(filled) = self.geometry(data, true).0.and_then(|p| p.transform(st.ts)) {
                self.scene.push(Op { path: filled, fill, stroke: None, clip: st.clip });
            }
            self.scene.push(Op { path: scene_all, fill: None, stroke, clip: st.clip });
        } else {
            self.scene.push(Op { path: scene_all, fill, stroke, clip: st.clip });
        }
    }

    fn stroke(&mut self, n: usize, st: St, bb: [f32; 4]) -> Option<StrokeStyle> {
        let dom = self.dom;
        let p = self.prop(n, "Stroke")?;
        let brush = self.brush(p, bb, st.ts, st.opacity)?;
        let t = dom.attr(n, "StrokeThickness").and_then(f).unwrap_or(1.0).max(0.0);
        let k = (st.ts.sx * st.ts.sy - st.ts.kx * st.ts.ky).abs().sqrt();
        let width = t * k;
        let word = |key: &str| dom.attr(n, key).map(str::trim);
        let cap = |key: &str| match word(key) {
            Some("Round") => LineCap::Round,
            Some("Square") => LineCap::Square,
            _ => LineCap::Butt,
        };
        let mut dash: Vec<f32> =
            word("StrokeDashArray").map(|s| nums(s).take(64).map(|d| (d * width).max(width * 1e-3).max(1e-4)).collect()).unwrap_or_default();
        if dash.len() % 2 == 1 {
            dash.extend_from_within(..);
        }
        let cap = if !dash.is_empty() && word("StrokeDashCap").is_some() { cap("StrokeDashCap") } else { cap("StrokeStartLineCap") };
        let join = match word("StrokeLineJoin") {
            Some("Round") => LineJoin::Round,
            Some("Bevel") => LineJoin::Bevel,
            _ => LineJoin::Miter,
        };
        Some(StrokeStyle {
            brush,
            width,
            cap,
            join,
            miter: dom.attr(n, "StrokeMiterLimit").and_then(f).unwrap_or(10.0).max(1.0),
            dash,
            phase: dom.attr(n, "StrokeDashOffset").and_then(f).unwrap_or(0.0) * width,
        })
    }

    fn glyphs(&mut self, n: usize, st: St) {
        let Some(st) = self.frame(n, st) else { return };
        let dom = self.dom;
        let v = |k: &str| dom.attr(n, k).and_then(f);
        let (Some(ox), Some(oy), Some(em)) = (v("OriginX"), v("OriginY"), v("FontRenderingEmSize")) else {
            self.scene.skipped += 1;
            return;
        };
        let text = dom.attr(n, "UnicodeString").unwrap_or("");
        let text = text.strip_prefix("{}").unwrap_or(text);
        if text.trim().is_empty() || em <= 0.0 {
            if dom.attr(n, "Indices").is_some() {
                self.scene.skipped += 1;
            }
            return;
        }
        if text.len() > self.text {
            self.scene.skipped += 1;
            return;
        }
        self.text -= text.len();
        let w = em * 0.5 * text.chars().count().min(100_000) as f32;
        let Some(brush) = self.prop(n, "Fill").and_then(|p| self.brush(p, [ox, oy - em, w, em], st.ts, st.opacity)) else { return };
        let uri = dom.attr(n, "FontUri").unwrap_or("");
        let base = uri.rsplit('/').next().unwrap_or("");
        let family = base.rsplit_once('.').map_or(base, |(a, _)| a).to_string();
        let font = self.fonts.entry(family).or_insert_with_key(|k| TextFont::resolve(k));
        let local = st.ts.pre_concat(Transform::from_row(1.0, 0.0, 0.0, -1.0, ox, oy));
        self.scene.text(font, text, f64::from(em) * CAP_EM, 1.0, 0.0, local, &brush, st.clip);
    }

    /// The brush of `p` for an element whose geometry has box `bb` (x, y, w, h) and transform `ts`.
    fn brush(&mut self, p: Prop<'a>, bb: [f32; 4], ts: Transform, op: f32) -> Option<Brush> {
        let dom: &'a Dom = self.dom;
        let i = match p {
            Prop::Str(s) => return color(s).map(|c| Brush::Solid(fade(c, op))),
            Prop::Node(i) => i,
        };
        let op = op * dom.attr(i, "Opacity").and_then(f).map_or(1.0, |v| v.clamp(0.0, 1.0));
        let space = |this: &Self| {
            let absolute = dom.attr(i, "MappingMode").is_some_and(|m| m.trim().eq_ignore_ascii_case("Absolute"));
            let map = if absolute {
                Transform::identity()
            } else {
                let (w, h) = (if bb[2] > 0.0 { bb[2] } else { 1.0 }, if bb[3] > 0.0 { bb[3] } else { 1.0 });
                Transform::from_row(w, 0.0, 0.0, h, bb[0], bb[1])
            };
            ts.pre_concat(this.transform_of(i, "Transform").unwrap_or_else(Transform::identity)).pre_concat(map)
        };
        match dom.name(i) {
            "SolidColorBrush" => color(dom.attr(i, "Color")?).map(|c| Brush::Solid(fade(c, op))),
            "LinearGradientBrush" => {
                let stops = self.stops(i, op);
                let from = dom.attr(i, "StartPoint").and_then(pair).unwrap_or((0.0, 0.0));
                let to = dom.attr(i, "EndPoint").and_then(pair).unwrap_or((1.0, 1.0));
                match stops.as_slice() {
                    [] => None,
                    [(_, c)] => Some(Brush::Solid(*c)),
                    _ => Some(Brush::Linear { from, to, stops, ts: space(self) }),
                }
            }
            "RadialGradientBrush" => {
                let stops = self.stops(i, op);
                let c = dom.attr(i, "Center").and_then(pair).unwrap_or((0.5, 0.5));
                let rx = dom.attr(i, "RadiusX").and_then(f).unwrap_or(0.5);
                let ry = dom.attr(i, "RadiusY").and_then(f).unwrap_or(0.5);
                if rx <= 0.0 || ry <= 0.0 {
                    return None;
                }
                // The circle of radius `rx` becomes the ellipse `rx` × `ry` around the centre.
                let ellipse = Transform::from_translate(c.0, c.1)
                    .pre_concat(Transform::from_scale(1.0, ry / rx))
                    .pre_concat(Transform::from_translate(-c.0, -c.1));
                match stops.as_slice() {
                    [] => None,
                    [(_, c)] => Some(Brush::Solid(*c)),
                    _ => Some(Brush::Radial { center: c, radius: rx, stops, ts: space(self).pre_concat(ellipse) }),
                }
            }
            "ImageBrush" => {
                let pixmap = self.image(dom.attr(i, "ImageSource")?)?;
                let (pw, ph) = (pixmap.width() as f32, pixmap.height() as f32);
                let vb = dom.attr(i, "ViewBox").and_then(quad).unwrap_or([0.0, 0.0, pw, ph]);
                let vp = dom.attr(i, "Viewport").and_then(quad).unwrap_or(vb);
                if vb[2] <= 0.0 || vb[3] <= 0.0 {
                    return None;
                }
                let (sx, sy) = (vp[2] / vb[2], vp[3] / vb[3]);
                // ponytail: ViewBox taken in pixels (96 dpi); honour the image's own dpi if needed.
                let into = Transform::from_row(sx, 0.0, 0.0, sy, vp[0] - vb[0] * sx, vp[1] - vb[1] * sy);
                let ts = ts.pre_concat(self.transform_of(i, "Transform").unwrap_or_else(Transform::identity)).pre_concat(into);
                Some(Brush::Image { pixmap, ts, opacity: op })
            }
            _ => {
                self.scene.skipped += 1;
                None
            }
        }
    }

    /// Gradient stops of brush `i`, by offset, with opacity `op` applied.
    fn stops(&self, i: usize, op: f32) -> Vec<(f32, [u8; 4])> {
        let dom = self.dom;
        let mut nodes: Vec<usize> = Vec::new();
        for &k in dom.kids(i) {
            if dom.name(k) == "GradientStop" {
                nodes.push(k);
            } else if dom.name(k).ends_with(".GradientStops") {
                nodes.extend(dom.kids(k).iter().copied().filter(|&c| dom.name(c) == "GradientStop"));
            }
        }
        let mut out: Vec<(f32, [u8; 4])> = nodes
            .into_iter()
            .take(1024)
            .filter_map(|s| Some((dom.attr(s, "Offset").and_then(f)?.clamp(0.0, 1.0), fade(color(dom.attr(s, "Color")?)?, op))))
            .collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }

    /// The picture at part `src`, decoded once; `None` (counted) when unreadable or over budget.
    fn image(&mut self, src: &str) -> Option<Arc<Pixmap>> {
        let key = resolve(self.part, src);
        if let Some(hit) = self.images.get(&key) {
            return hit.clone();
        }
        let got = self.load_image(&key);
        if got.is_none() {
            self.scene.skipped += 1;
        }
        self.images.insert(key, got.clone());
        got
    }

    fn load_image(&mut self, key: &str) -> Option<Arc<Pixmap>> {
        let bytes = self.zip.read(key, MAX_IMAGE).ok()?;
        let (w, h) = cadcraft_render::images::raster_dimensions(&bytes).ok()?;
        self.pixels = self.pixels.checked_sub(u64::from(w) * u64::from(h))?;
        crate::underlay::pixmap(&cadcraft_render::images::decode_raster(&bytes).ok()?)
    }
}

/// One PathFigure's segments into `pb`.
fn figure(dom: &Dom, fig: usize, pb: &mut Pb) {
    if let Some((x, y)) = dom.attr(fig, "StartPoint").and_then(pair) {
        pb.move_to(x, y);
    }
    for &s in dom.kids(fig) {
        let pts = dom.attr(s, "Points").unwrap_or("");
        match dom.name(s) {
            "PolyLineSegment" => {
                let mut it = nums(pts);
                while let (Some(x), Some(y)) = (it.next(), it.next()) {
                    pb.line_to(x, y);
                    if pb.left == 0 {
                        break;
                    }
                }
            }
            "PolyBezierSegment" => {
                let mut it = nums(pts);
                while let (Some(a), Some(b), Some(c), Some(d), Some(e), Some(g)) = (it.next(), it.next(), it.next(), it.next(), it.next(), it.next())
                {
                    pb.cubic_to(a, b, c, d, e, g);
                    if pb.left == 0 {
                        break;
                    }
                }
            }
            "PolyQuadraticBezierSegment" => {
                let mut it = nums(pts);
                while let (Some(a), Some(b), Some(c), Some(d)) = (it.next(), it.next(), it.next(), it.next()) {
                    pb.quad_to(a, b, c, d);
                    if pb.left == 0 {
                        break;
                    }
                }
            }
            "ArcSegment" => {
                if let (Some((x, y)), Some((rx, ry))) = (dom.attr(s, "Point").and_then(pair), dom.attr(s, "Size").and_then(pair)) {
                    let rot = dom.attr(s, "RotationAngle").and_then(f).unwrap_or(0.0);
                    let clockwise = dom.attr(s, "SweepDirection").is_some_and(|v| v.trim().eq_ignore_ascii_case("Clockwise"));
                    pb.arc(rx, ry, rot, is_true(dom.attr(s, "IsLargeArc")), clockwise, x, y);
                }
            }
            _ => {}
        }
    }
    if is_true(dom.attr(fig, "IsClosed")) {
        pb.close();
    }
}

#[cfg(test)]
#[path = "dwfx_tests.rs"]
mod tests;
