//! DWFATTACH and DGNATTACH: a DWF/DWFx sheet or a DGN V7 design placed in the drawing as an
//! underlay, like PDFATTACH ([`super::attach`]): an IMAGE object whose `page` is the sheet
//! number, `kind` the file's kind and `sheet` its name. Nothing is copied into the drawing; the
//! picture is drawn from the file by [`cadcraft_render::images`] (the readers live in
//! `cadcraft-io`). DGN V8 files have no public specification and are refused with an error.

use cadcraft_doc::{EntityKind, Handle, UnderlayKind};
use cadcraft_geom::Vec2;
use cadcraft_render::images;
use serde_json::{Value, json};

use super::attach::{Placement, make_available, path_param, place, placement, unit_scale};
use super::{CommandSpec, bad, f64_or};
use crate::{Accept, EngineError, Input, Interactive, Prompt, Result, Session, Step};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("dwfattach", "Attach DWF Underlay", run_dwf)
            .menu(&["Insert", "DWF Underlay..."])
            .params(
                "{path (DWF or DWFx file), data? (base64 bytes, as for IMAGEATTACH), sheet? (name or 1-based number, default the first), at? ([x, y] of the sheet's lower-left corner, default [0, 0]), \
                 width? (drawing width of the sheet) | scale? (drawing units per point, default 1), rotation? (degrees, default 0), layer? (default the current layer)} \
                 → {handle, path, sheet, page, sheets, width, height}. The sheet is drawn from its vector content (paths, fills, text, pictures) on white paper.",
            )
            .interactive(|_| Ok(Box::new(CadM::new(UnderlayKind::Dwf)))),
        CommandSpec::new("dgnattach", "Attach DGN Underlay", run_dgn)
            .menu(&["Insert", "DGN Underlay..."])
            .params(
                "{path (DGN V7 file; V8 is refused: it has no public specification), data? (base64 bytes), model? (the model name, \"Model\"), at? ([x, y] of the design's lower-left corner, default [0, 0]), \
                 width? (drawing width) | scale? (drawing units per master unit, default 1), rotation? (degrees, default 0), layer? (default the current layer)} \
                 → {handle, path, sheet, page, sheets, width, height}. Lines, shapes, arcs, ellipses, curves, text and cells are drawn with the file's colour table; levels switched off in the first view are hidden.",
            )
            .interactive(|_| Ok(Box::new(CadM::new(UnderlayKind::Dgn)))),
    ]
}

pub(super) fn run_dwf(s: &mut Session, p: &Value) -> Result<Value> {
    attach_cad(s, "dwfattach", p, UnderlayKind::Dwf)
}

pub(super) fn run_dgn(s: &mut Session, p: &Value) -> Result<Value> {
    attach_cad(s, "dgnattach", p, UnderlayKind::Dgn)
}

/// The kind of underlay a file name's extension says (`attach` picks the command by it).
pub(super) fn kind_of(path: &str) -> Option<UnderlayKind> {
    let ext = std::path::Path::new(path.trim()).extension()?.to_string_lossy().to_ascii_lowercase();
    match ext.as_str() {
        "dwf" | "dwfx" => Some(UnderlayKind::Dwf),
        "dgn" => Some(UnderlayKind::Dgn),
        _ => None,
    }
}

/// The 0-based index of the sheet the `sheet` / `model` parameter names (the first when absent).
fn pick(cmd: &str, want: Option<&Value>, sheets: &[String], kind: UnderlayKind) -> Result<usize> {
    let Some(v) = want.filter(|v| !v.is_null()) else { return Ok(0) };
    let list = || sheets.join(", ");
    if let Some(n) = v.as_u64() {
        return usize::try_from(n)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .filter(|i| *i < sheets.len())
            .ok_or_else(|| bad(cmd, format!("the file has no sheet {n} (it has {})", sheets.len())));
    }
    let text = v.as_str().map(str::trim).ok_or_else(|| bad(cmd, "`sheet` must be a name or a number"))?;
    if let Some(i) = sheets.iter().position(|n| n.trim().eq_ignore_ascii_case(text)) {
        return Ok(i);
    }
    if kind == UnderlayKind::Dgn && text.eq_ignore_ascii_case("default") {
        return Ok(0);
    }
    text.parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .filter(|i| *i < sheets.len())
        .ok_or_else(|| bad(cmd, format!("the file has no sheet or model `{text}` (it has: {})", list())))
}

fn attach_cad(s: &mut Session, cmd: &str, p: &Value, kind: UnderlayKind) -> Result<Value> {
    let path = path_param(cmd, p)?;
    s.doc()?;
    let stored = make_available(cmd, p, path)?;
    let bytes = images::read_bytes(&stored).map_err(|e| bad(cmd, e))?;
    let found = cadcraft_io::underlay_kind(&bytes).map_err(|e| bad(cmd, e))?;
    if found != kind {
        let (want, other) = if kind == UnderlayKind::Dwf { ("DWF", "DGN") } else { ("DGN", "DWF") };
        return Err(bad(
            cmd,
            match found {
                UnderlayKind::Pdf => format!("{path} is a PDF: attach it with PDFATTACH"),
                UnderlayKind::Dwf | UnderlayKind::Dgn => format!("{path} is not a {want} file (it is a {other} file: attach it with {other}ATTACH)"),
            },
        ));
    }
    let sheets = cadcraft_io::underlay_sheets(&bytes).map_err(|e| bad(cmd, e))?;
    let key = if kind == UnderlayKind::Dgn { "model" } else { "sheet" };
    let index = pick(cmd, p.get(key).or_else(|| p.get("sheet")), &sheets, kind)?;
    let page = u32::try_from(index + 1).map_err(|_| bad(cmd, "the sheet number is too large"))?;
    let name = sheets.get(index).cloned().unwrap_or_default();
    let natural = images::natural_size(&stored, page).map_err(|e| bad(cmd, e))?;
    let unit = unit_scale(cmd, p, natural.0)?;
    let Placement { at, rotation, layer } = placement(cmd, p)?;
    let handle = place(s, &stored, page, natural, at, unit, rotation, layer.as_deref(), false)?;
    let h = Handle::parse_hex(&handle).ok_or_else(|| EngineError::Other("the attached underlay has no handle".into()))?;
    let sheet = name.clone();
    s.doc_mut()?
        .modify_entity(h, |e| {
            if let EntityKind::Image(i) = &mut e.kind {
                i.kind = kind;
                i.sheet = sheet;
            }
        })
        .map_err(|e| EngineError::Other(e.to_string()))?;
    Ok(json!({
        "handle": handle, "path": stored, "sheet": name, "page": page, "sheets": sheets,
        "width": natural.0 * unit, "height": natural.1 * unit,
    }))
}

/// The prompts: file, (DWF: sheet,) insertion point, scale factor, rotation.
struct CadM {
    kind: UnderlayKind,
    path: Option<String>,
    /// The sheet name or number text (DGN: empty, no prompt).
    sheet: Option<String>,
    at: Option<Vec2>,
    scale: Option<f64>,
}

impl CadM {
    fn new(kind: UnderlayKind) -> Self {
        CadM { kind, path: None, sheet: None, at: None, scale: None }
    }
    fn dwf(&self) -> bool {
        self.kind == UnderlayKind::Dwf
    }
}

impl Interactive for CadM {
    fn name(&self) -> &'static str {
        if self.dwf() { "DWFATTACH" } else { "DGNATTACH" }
    }
    fn prompt(&self, _s: &Session) -> Prompt {
        match (&self.path, &self.sheet, self.at, self.scale) {
            (None, ..) if self.dwf() => Prompt::new("Enter the DWF or DWFx file to attach", Accept::TEXT),
            (None, ..) => Prompt::new("Enter the DGN file to attach (V7)", Accept::TEXT),
            (Some(_), None, ..) => Prompt::new("Enter the sheet name or number", Accept::TEXT).default("1"),
            (Some(_), Some(_), None, _) => Prompt::new("Specify the insertion point (lower-left corner)", Accept::POINT).default("0,0"),
            (Some(_), Some(_), Some(at), None) => {
                Prompt::new("Specify the scale factor (drawing units per point or master unit)", Accept::NUMBER).default("1").base(at)
            }
            _ => Prompt::new("Specify the rotation angle", Accept::NUMBER).default("0"),
        }
    }
    fn input(&mut self, s: &mut Session, i: Input) -> Result<Step> {
        let cmd = self.name().to_ascii_lowercase();
        match (&self.path, &self.sheet, self.at, self.scale, i) {
            (.., Input::Cancel) => Ok(Step::Cancel),
            (None, _, _, _, Input::Text(t) | Input::Keyword(t)) if !t.trim().is_empty() => {
                self.path = Some(t.trim().trim_matches('"').to_string());
                if !self.dwf() {
                    self.sheet = Some(String::new());
                }
                Ok(Step::Continue)
            }
            (Some(_), None, _, _, i @ (Input::Enter | Input::Text(_))) => {
                self.sheet = Some(match i {
                    Input::Text(t) if !t.trim().is_empty() => t.trim().to_string(),
                    _ => "1".to_string(),
                });
                Ok(Step::Continue)
            }
            (Some(_), Some(_), None, _, Input::Point(p)) => {
                self.at = Some(p);
                Ok(Step::Continue)
            }
            (Some(_), Some(_), None, _, Input::Enter) => {
                self.at = Some(Vec2::ZERO);
                Ok(Step::Continue)
            }
            (Some(_), Some(_), Some(_), None, i @ (Input::Enter | Input::Text(_))) => {
                self.scale = Some(match i {
                    Input::Text(t) => t
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite() && *v > 0.0)
                        .ok_or_else(|| bad(&cmd, "enter a positive scale factor"))?,
                    _ => 1.0,
                });
                Ok(Step::Continue)
            }
            (Some(path), Some(sheet), Some(at), Some(scale), i @ (Input::Enter | Input::Text(_))) => {
                let rotation = match i {
                    Input::Text(t) => t
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| bad(&cmd, "enter the rotation as a number of degrees"))?,
                    _ => 0.0,
                };
                let mut p = json!({ "path": path, "at": [at.x, at.y], "scale": scale, "rotation": rotation });
                if !sheet.is_empty() {
                    p["sheet"] = json!(sheet);
                }
                let r = if self.dwf() { run_dwf(s, &p) } else { run_dgn(s, &p) }?;
                s.echo(format!(
                    "Attached {} ({} x {} drawing units).",
                    r["path"].as_str().unwrap_or(""),
                    f64_or(&r, "width", 0.0),
                    f64_or(&r, "height", 0.0)
                ));
                Ok(Step::Done)
            }
            _ => Ok(Step::Continue),
        }
    }
}

#[cfg(test)]
#[path = "attach_cad_tests.rs"]
mod tests;
