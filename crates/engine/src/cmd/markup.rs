//! MARKUPIMPORT and MARKUPASSIST: a reviewer's markups (PDF annotations, or markups read off an
//! image by the Assistant) become tagged objects on a MARKUP layer, and are then turned into real
//! drawing annotation.
//!
//! Markups arrive as JSON in normalized page coordinates (`u`, `v` in 0..1, origin top left, v
//! down; see [`cadcraft_io::pdf_markup`]): `text`, `cloud`, `polyline`, `rect`, `ellipse` and
//! `leader`. Each becomes one object (text: MTEXT; cloud, polyline, leader: polyline; rect:
//! closed polyline; ellipse: ELLIPSE), placed so the page's lower-left corner is `at` and its
//! width is `width`. Each object carries xdata of the application `CADCRAFT_MARKUP` (the kind and
//! the text) so MARKUPASSIST finds it. MARKUPASSIST lists them, or converts the given ones: text to
//! MTEXT, TEXT or an MLEADER, a leader to an MLEADER with its text, a cloud to a revision-cloud
//! polyline, other shapes to plain objects of the target layer; the markup object is removed
//! unless `keep`.

use cadcraft_color::{Color, Rgb};
use cadcraft_doc::{Entity, EntityKind, Handle, MText, RawTag, Text, mtext_literal};
use cadcraft_geom::{PolyVertex, Vec2};
use serde_json::{Value, json};

use super::draw2::{cloud_entity, cloud_vertices, default_arc_len};
use super::helpers::{lwpoly, rect_vertices, v3};
use super::{CommandSpec, bad, bool_or, size_param, str_param};
use crate::{Accept, EngineError, Input, Interactive, Prompt, Result, Session, Step};

/// The xdata application name that tags a markup object.
pub const MARKUP_APP: &str = "CADCRAFT_MARKUP";
const DEFAULT_LAYER: &str = "MARKUP";
const MAX_MARKUPS: usize = 10_000;
const MAX_POINTS: usize = 20_000;
const MAX_TEXT: usize = 10_000;
/// Longest string in one xdata group (DXF group 1000 holds 255 bytes).
const CHUNK: usize = 240;
#[cfg(not(target_arch = "wasm32"))]
const MAX_FILE: u64 = 512 << 20;
/// A text markup without a usable height: this share of the page height.
const DEFAULT_TEXT_HEIGHT: f64 = 0.02;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("markupimport", "Markup Import", run_import)
            .menu(&["Insert", "Markup Import"])
            .params(
                "{path (PDF, or PNG/JPG with markups), page? (default 1), at? ([x, y] of the page's lower-left corner, default [0, 0]), \
                 width? (drawing width of the page; default the PDF page width in points, the image width in pixels), layer? (default \"MARKUP\"), \
                 markups? ([{kind: text|cloud|polyline|rect|ellipse|leader, …}] in normalized page coordinates, origin top left; required for images)} \
                 → {handles, count, skipped}",
            )
            .interactive(|_| Ok(Box::new(ImportM::default()))),
        CommandSpec::new("markupassist", "Markup Assist", run_assist)
            .menu(&["Insert", "Markup Assist"])
            .params(
                "{handles? | all? (convert these markups; without either the markups are listed), as? (\"mtext\" default | \"mleader\" | \"text\": what a text markup becomes), \
                 layer? (default the current layer), keep? (default false: the markup is removed), arcLength? (revision cloud arc)} \
                 → {markups: [{handle, kind, text?, layer}]} listed, or {handles, converted, skipped} converted",
            )
            .interactive(|_| Ok(Box::new(AssistM))),
    ]
}

// ---------------------------------------------------------------- tagging

/// The xdata that tags an object as a markup of `kind` with `text`.
fn tags(kind: &str, text: &str) -> Vec<RawTag> {
    let tag = |code: i32, value: &str| RawTag { code, value: value.into() };
    let mut v = vec![tag(1001, MARKUP_APP), tag(1000, kind)];
    let mut rest = text;
    while !rest.is_empty() {
        let mut cut = rest.len().min(CHUNK);
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (a, b) = rest.split_at(cut);
        v.push(tag(1000, a));
        rest = b;
    }
    v
}

/// A markup object's kind and text, when `e` is tagged.
fn tagged(e: &Entity) -> Option<(String, String)> {
    let at = e.common.xdata.iter().position(|t| t.code == 1001 && t.value == MARKUP_APP)?;
    let mut groups = e.common.xdata.iter().skip(at + 1).take_while(|t| t.code != 1001).filter(|t| t.code == 1000);
    let kind = groups.next()?.value.clone();
    Some((kind, groups.map(|t| t.value.as_str()).collect()))
}

struct Found {
    handle: Handle,
    kind: String,
    text: String,
    entity: std::sync::Arc<Entity>,
}

/// Every markup object of the drawing, model space first.
fn find_markups(s: &Session) -> Result<Vec<Found>> {
    let d = s.doc()?;
    let spaces = std::iter::once(&d.model).chain(d.layouts.iter().map(|l| &l.entities));
    Ok(spaces
        .flat_map(|st| st.iter())
        .filter_map(|e| tagged(e).map(|(kind, text)| Found { handle: e.handle, kind, text, entity: e.clone() }))
        .collect())
}

// ---------------------------------------------------------------- import

fn read_file(_path: &str) -> Result<Vec<u8>> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let big = std::fs::metadata(_path).is_ok_and(|m| m.len() > MAX_FILE);
        if big {
            return Err(bad("markupimport", format!("{_path}: file too large")));
        }
        std::fs::read(_path).map_err(|e| bad("markupimport", format!("{_path}: {e}")))
    }
    #[cfg(target_arch = "wasm32")]
    Err(bad("markupimport", "paths are not available on the web"))
}

fn layer_name(cmd: &str, p: &Value, default: Option<&str>) -> Result<Option<String>> {
    let Some(v) = p.get("layer").filter(|v| !v.is_null()) else { return Ok(default.map(str::to_string)) };
    match v.as_str() {
        Some(n) if super::layer::valid_name(n) => Ok(Some(n.trim().to_string())),
        _ => Err(bad(cmd, "`layer` must be a valid layer name")),
    }
}

fn parse_points(m: &Value) -> Option<Vec<Vec2>> {
    let a = m.get("points")?.as_array()?;
    if a.len() > MAX_POINTS {
        return None;
    }
    a.iter().map(|p| super::point_value(p).filter(|v| v.x.abs() <= 1e6 && v.y.abs() <= 1e6)).collect()
}

fn parse_color(m: &Value) -> Option<Color> {
    let h = m.get("color")?.as_str()?.strip_prefix('#')?;
    (h.len() == 6).then(|| u32::from_str_radix(h, 16).ok()).flatten().map(|v| Color::True(Rgb::from_u32(v)))
}

/// One markup as an object: its geometry, its text and its colour. `map` takes normalized page
/// coordinates to the drawing.
fn markup_object(s: &Session, m: &Value, map: &dyn Fn(Vec2) -> Vec2, dh: f64) -> std::result::Result<(EntityKind, String), String> {
    let kind = m.get("kind").and_then(Value::as_str).ok_or("it has no `kind`")?;
    let text: String = m.get("text").and_then(Value::as_str).unwrap_or("").chars().take(MAX_TEXT).collect();
    let pts = || parse_points(m).map(|p| p.into_iter().map(map).collect::<Vec<_>>()).ok_or_else(|| "`points` must be a list of [u, v]".to_string());
    let poly = |p: &[Vec2], min: usize, closed: bool| {
        if p.len() < min {
            return Err(format!("a {kind} needs at least {min} points"));
        }
        Ok(lwpoly(p.iter().map(|q| PolyVertex::new(*q)).collect(), closed))
    };
    let k = match kind {
        "text" => {
            let at = m.get("at").and_then(super::point_value).ok_or("a text needs `at`")?;
            let nh = m.get("height").and_then(Value::as_f64).filter(|h| h.is_finite() && *h > 0.0 && *h < 10.0).unwrap_or(DEFAULT_TEXT_HEIGHT);
            if text.is_empty() {
                return Err("a text needs `text`".into());
            }
            let style = s.doc().map(|d| d.header.str("TEXTSTYLE", "Standard")).unwrap_or_else(|_| "Standard".into());
            EntityKind::MText(MText {
                insert: v3(map(at)),
                height: nh * dh,
                width: 0.0,
                attach: 1,
                rotation: 0.0,
                style,
                contents: mtext_literal(&text),
                line_spacing: 1.0,
                line_spacing_exact: false,
                columns: None,
            })
        }
        "cloud" => poly(&pts()?, 3, true)?,
        "polyline" => poly(&pts()?, 2, false)?,
        "leader" => poly(&pts()?, 2, false)?,
        "rect" | "ellipse" => {
            let p = pts()?;
            let [a, b] = p.as_slice() else { return Err(format!("a {kind} needs two opposite corners")) };
            if kind == "rect" {
                lwpoly(rect_vertices(*a, *b), true)
            } else {
                let (c, r) = ((*a + *b) * 0.5, Vec2::new(((b.x - a.x) * 0.5).abs(), ((b.y - a.y) * 0.5).abs()));
                let (major, ratio) = if r.x >= r.y { (Vec2::new(r.x, 0.0), r.y / r.x) } else { (Vec2::new(0.0, r.y), r.x / r.y) };
                if !(major.len() > 1e-12 && ratio > 1e-9) {
                    return Err("the ellipse has no area".into());
                }
                EntityKind::Ellipse(cadcraft_doc::Ellipse { center: v3(c), major: v3(major), ratio, start: 0.0, end: cadcraft_geom::TAU })
            }
        }
        other => return Err(format!("unknown markup kind `{other}`")),
    };
    Ok((k, text))
}

/// The page's size: a PDF page in points, or an image in pixels.
fn page_markups(p: &Value, path: &str) -> Result<(f64, f64, Vec<Value>, Vec<String>)> {
    let ext = std::path::Path::new(path).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "pdf" => {
            let page = match p.get("page") {
                None | Some(Value::Null) => 1,
                Some(v) => v
                    .as_u64()
                    .filter(|n| *n >= 1)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| bad("markupimport", "`page` must be 1 or more"))?,
            };
            let m = cadcraft_io::pdf_markup::pdf_markups(&read_file(path)?, page).map_err(|e| bad("markupimport", e.to_string()))?;
            Ok((m.width, m.height, m.markups, m.notes))
        }
        "png" | "jpg" | "jpeg" => {
            let Some(given) = p.get("markups").and_then(Value::as_array) else {
                return Err(EngineError::Other("image markups need recognition: run it from the Assistant or MCP `markup_import`".into()));
            };
            let (w, h) = cadcraft_io::pdf_markup::image_size(&read_file(path)?)
                .ok_or_else(|| bad("markupimport", format!("{path}: not a PNG or JPEG image")))?;
            Ok((f64::from(w), f64::from(h), given.clone(), Vec::new()))
        }
        _ => Err(bad("markupimport", "markups are read from PDF, PNG and JPG files")),
    }
}

fn run_import(s: &mut Session, p: &Value) -> Result<Value> {
    let path = str_param(p, "path").filter(|x| !x.is_empty()).ok_or_else(|| bad("markupimport", "`path` (a PDF, PNG or JPG file) is required"))?;
    let at = match p.get("at") {
        None | Some(Value::Null) => Vec2::ZERO,
        Some(v) => {
            super::point_value(v).filter(|v| v.x.abs() <= 1e9 && v.y.abs() <= 1e9).ok_or_else(|| bad("markupimport", "`at` must be [x, y]"))?
        }
    };
    let width = size_param("markupimport", p, "width", false)?.filter(|w| *w <= 1e9);
    if p.get("width").is_some_and(|w| !w.is_null()) && width.is_none() {
        return Err(bad("markupimport", "`width` is too large"));
    }
    let layer = layer_name("markupimport", p, Some(DEFAULT_LAYER))?.unwrap_or_else(|| DEFAULT_LAYER.into());
    s.doc()?;
    let (pw, ph, markups, notes) = page_markups(p, path)?;
    if markups.len() > MAX_MARKUPS {
        return Err(bad("markupimport", format!("too many markups (at most {MAX_MARKUPS})")));
    }
    let (dw, dh) = (width.unwrap_or(pw), width.map_or(ph, |w| ph * w / pw));
    let map = |q: Vec2| Vec2::new(at.x + q.x * dw, at.y + (1.0 - q.y) * dh);
    let mut skipped = notes;
    let mut objects = Vec::new();
    for (i, m) in markups.iter().enumerate() {
        match markup_object(s, m, &map, dh) {
            Ok((k, text)) => objects.push((k, text, m.get("kind").and_then(Value::as_str).unwrap_or("").to_string(), parse_color(m))),
            Err(why) => skipped.push(format!("markup {}: {why}", i + 1)),
        }
    }
    let space = s.space();
    let base = s.current_common();
    let d = s.doc_mut()?;
    if d.ensure_layer(&layer)
        && let Some(l) = d.layer_mut(&layer)
    {
        l.color = Color::Index(1);
    }
    let mut handles = Vec::new();
    for (k, text, kind, color) in objects {
        let mut common = base.clone();
        common.layer = layer.clone();
        common.color = color.unwrap_or(Color::ByLayer);
        common.xdata = tags(&kind, &text);
        handles.push(d.add(&space, common, k).map_err(|e| EngineError::Other(e.to_string()))?.hex());
    }
    Ok(json!({ "handles": handles, "count": handles.len(), "skipped": skipped }))
}

#[derive(Default)]
struct ImportM {
    path: Option<String>,
    at: Option<Vec2>,
}

impl Interactive for ImportM {
    fn name(&self) -> &'static str {
        "MARKUPIMPORT"
    }
    fn prompt(&self, _s: &Session) -> Prompt {
        match (&self.path, self.at) {
            (None, _) => Prompt::new("Enter the markup file (PDF, PNG or JPG)", Accept::TEXT),
            (Some(_), None) => Prompt::new("Specify the insertion point of the page's lower-left corner", Accept::POINT).default("0,0"),
            (Some(_), Some(_)) => Prompt::new("Specify the width of the page in drawing units", Accept::NUMBER).default("page size"),
        }
    }
    fn input(&mut self, s: &mut Session, i: Input) -> Result<Step> {
        match (&self.path, self.at, i) {
            (_, _, Input::Cancel) => Ok(Step::Cancel),
            (None, _, Input::Text(t) | Input::Keyword(t)) if !t.trim().is_empty() => {
                self.path = Some(t.trim().trim_matches('"').to_string());
                Ok(Step::Continue)
            }
            (Some(_), None, Input::Point(p)) => {
                self.at = Some(p);
                Ok(Step::Continue)
            }
            (Some(_), None, Input::Enter) => {
                self.at = Some(Vec2::ZERO);
                Ok(Step::Continue)
            }
            (Some(path), Some(at), i @ (Input::Enter | Input::Text(_))) => {
                let mut p = json!({ "path": path, "at": [at.x, at.y] });
                if let Input::Text(t) = i {
                    p["width"] = json!(t.trim().parse::<f64>().map_err(|_| bad("markupimport", "enter a width as a number"))?);
                }
                let r = run_import(s, &p)?;
                s.echo(format!("{} markup(s) imported on layer {DEFAULT_LAYER}.", r["count"]));
                for n in r["skipped"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    s.echo(format!("Skipped {n}"));
                }
                Ok(Step::Done)
            }
            _ => Ok(Step::Continue),
        }
    }
}

// ---------------------------------------------------------------- assist

fn points_of(e: &Entity) -> Vec<Vec2> {
    match &e.kind {
        EntityKind::LwPolyline(p) => p.vertices.iter().map(|v| v.p).collect(),
        _ => Vec::new(),
    }
}

/// The drawing annotation a markup becomes (one or more objects).
fn annotation(s: &Session, f: &Found, how: &str, arc: Option<f64>) -> std::result::Result<Vec<EntityKind>, String> {
    let pts = points_of(&f.entity);
    match (f.kind.as_str(), &f.entity.kind) {
        ("text", EntityKind::MText(m)) => match how {
            "mtext" => Ok(vec![EntityKind::MText(m.clone())]),
            "text" => {
                let lines: Vec<&str> = f.text.lines().collect();
                Ok(lines
                    .iter()
                    .enumerate()
                    .map(|(i, l)| {
                        let y = m.insert.y - m.height * (1.0 + 1.5 * i as f64);
                        EntityKind::Text(Text {
                            insert: cadcraft_geom::Vec3::new(m.insert.x, y, m.insert.z),
                            align_pt: None,
                            height: m.height,
                            value: (*l).to_string(),
                            rotation: 0.0,
                            width_factor: 1.0,
                            oblique: 0.0,
                            style: m.style.clone(),
                            halign: Default::default(),
                            valign: Default::default(),
                        })
                    })
                    .collect())
            }
            _ => {
                let p = m.insert.xy();
                let mut k = super::annotate::mleader_kind(s, &[p, p], &f.text, false).ok_or("no multileader")?;
                if let EntityKind::MLeader(ml) = &mut k {
                    // Text only: leader type None.
                    ml.leaders.clear();
                }
                Ok(vec![k])
            }
        },
        ("leader", _) => {
            super::annotate::mleader_kind(s, &pts, &f.text, true).map(|k| vec![k]).ok_or_else(|| "a leader needs two points".to_string())
        }
        ("cloud", _) => {
            let perim: f64 = pts.iter().zip(pts.iter().cycle().skip(1)).map(|(a, b)| a.dist(*b)).sum();
            let arc = arc.unwrap_or_else(|| default_arc_len(s).max(perim / 200.0));
            let vs = cloud_vertices(&pts, arc, true, false).ok_or("the cloud needs an area")?;
            Ok(vec![cloud_entity(vs, false, arc)])
        }
        ("polyline" | "rect" | "ellipse", k) => Ok(vec![k.clone()]),
        (k, _) => Err(format!("unknown markup kind `{k}`")),
    }
}

fn convert(s: &mut Session, found: Vec<Found>, how: &str, layer: Option<&str>, keep: bool, arc: Option<f64>) -> Result<(Vec<String>, Vec<String>)> {
    let (mut made, mut skipped) = (Vec::new(), Vec::new());
    for f in found {
        let locked = !keep && s.doc()?.layer(&f.entity.common.layer).is_some_and(|l| l.locked);
        if locked {
            skipped.push(format!("{}: on a locked layer", f.handle.hex()));
            continue;
        }
        match annotation(s, &f, how, arc) {
            Ok(kinds) => {
                for k in kinds {
                    let h = s.add_entity(k)?;
                    if let Some(l) = layer {
                        let l = l.to_string();
                        s.doc_mut()?.ensure_layer(&l);
                        s.doc_mut()?.modify_entity(h, |e| e.common.layer = l).map_err(|e| EngineError::Other(e.to_string()))?;
                    }
                    made.push(h.hex());
                }
                if !keep {
                    s.doc_mut()?.remove_entity(f.handle);
                }
            }
            Err(why) => skipped.push(format!("{}: {why}", f.handle.hex())),
        }
    }
    Ok((made, skipped))
}

fn run_assist(s: &mut Session, p: &Value) -> Result<Value> {
    let how = match str_param(p, "as") {
        None => "mtext",
        Some(v) if matches!(v.to_ascii_lowercase().as_str(), "mtext" | "mleader" | "text") => v,
        Some(v) => return Err(bad("markupassist", format!("`as` must be \"mtext\", \"mleader\" or \"text\", got \"{v}\""))),
    }
    .to_ascii_lowercase();
    let layer = layer_name("markupassist", p, None)?;
    let arc = size_param("markupassist", p, "arcLength", false)?;
    let mut found = find_markups(s)?;
    let wanted: Option<Vec<Handle>> = match (p.get("handles"), p.get("handle")) {
        (Some(Value::Array(a)), _) => Some(
            a.iter()
                .map(|v| v.as_str().and_then(Handle::parse_hex).or_else(|| v.as_u64().map(Handle)))
                .collect::<Option<_>>()
                .ok_or_else(|| bad("markupassist", "`handles` must be object handles"))?,
        ),
        (Some(v), _) if !v.is_null() => return Err(bad("markupassist", "`handles` must be an array of object handles")),
        (_, Some(v)) if !v.is_null() => {
            Some(vec![v.as_str().and_then(Handle::parse_hex).ok_or_else(|| bad("markupassist", "`handle` must be an object handle"))?])
        }
        _ => bool_or(p, "all", false).then(|| found.iter().map(|f| f.handle).collect()),
    };
    let Some(wanted) = wanted else {
        let list: Vec<Value> =
            found.iter().map(|f| json!({ "handle": f.handle.hex(), "kind": f.kind, "text": f.text, "layer": f.entity.common.layer })).collect();
        return Ok(json!({ "count": list.len(), "markups": list }));
    };
    let mut skipped: Vec<String> =
        wanted.iter().filter(|h| !found.iter().any(|f| f.handle == **h)).map(|h| format!("{}: not a markup", h.hex())).collect();
    found.retain(|f| wanted.contains(&f.handle));
    let (made, more) = convert(s, found, &how, layer.as_deref(), bool_or(p, "keep", false), arc)?;
    skipped.extend(more);
    s.set_selection(made.iter().filter_map(|h| Handle::parse_hex(h)).collect());
    Ok(json!({ "handles": made, "converted": made.len(), "skipped": skipped }))
}

struct AssistM;

impl Interactive for AssistM {
    fn name(&self) -> &'static str {
        "MARKUPASSIST"
    }
    fn begin(&mut self, s: &mut Session) -> Result<Step> {
        if find_markups(s)?.is_empty() {
            s.echo("There are no markups in the drawing: import them with MARKUPIMPORT.");
            return Ok(Step::Done);
        }
        Ok(Step::Continue)
    }
    fn prompt(&self, _s: &Session) -> Prompt {
        Prompt::new("Convert all markups to drawing annotation, text as", Accept::TEXT).kw(&["Mtext", "MLeader", "Text", "List"]).default("Mtext")
    }
    fn input(&mut self, s: &mut Session, i: Input) -> Result<Step> {
        let how = match i {
            Input::Cancel => return Ok(Step::Cancel),
            Input::Enter => "mtext".to_string(),
            Input::Keyword(k) | Input::Text(k) => k.to_ascii_lowercase(),
            _ => return Ok(Step::Continue),
        };
        if how == "list" || how == "l" {
            for f in find_markups(s)? {
                s.echo(format!("{} {} {}", f.handle.hex(), f.kind, f.text.replace('\n', " ")));
            }
            return Ok(Step::Continue);
        }
        let how = ["mtext", "mleader", "text"].into_iter().find(|k| *k == how || (how.len() > 1 && k.starts_with(&how))).unwrap_or("mtext");
        let r = run_assist(s, &json!({ "all": true, "as": how }))?;
        s.echo(format!("{} markup object(s) converted.", r["converted"]));
        Ok(Step::Done)
    }
}

#[cfg(test)]
#[path = "markup_tests.rs"]
mod tests;
