//! ATTACH, IMAGEATTACH and PDFATTACH: raster images (PNG, JPEG) and PDF pages placed in the
//! drawing as IMAGE objects (a PDF page is an underlay: [`cadcraft_doc::Image::page`] is 1 or
//! more). Nothing is copied into the drawing: it keeps the file's path (made absolute) and the
//! pictures are read, size-capped and cached by [`cadcraft_render::images`] when drawn.
//!
//! Without a file (the web build, generated pictures) the bytes come as base64 `data`: they are
//! registered under `path` as a name for the rest of the session ([`cadcraft_render::images::put_bytes`]),
//! and a saved drawing then shows that name's frame until the file is attached again.

use cadcraft_doc::{Common, EntityKind, Image};
use cadcraft_geom::{Vec2, Vec3};
use cadcraft_render::images;
use serde_json::{Value, json};

use super::helpers::v3;
use super::{CommandSpec, bad, f64_or, size_param, str_param};
use crate::{Accept, EngineError, Input, Interactive, Prompt, Result, Session, Step};

/// Most the origin or the picture's size may reach, in drawing units (hostile parameters).
const LIMIT: f64 = 1e9;

/// A PDF page is measured in points: 72 to the inch, and an underlay's `scale` counts inches.
const POINTS_PER_INCH: f64 = 72.0;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("attach", "Attach", run_attach)
            .menu(&["Insert", "Attach..."])
            .params(
                "{path (a .png/.jpg/.jpeg raster image, a .pdf or a .dwf/.dwfx or .dgn file; the extension picks IMAGEATTACH, PDFATTACH, DWFATTACH or DGNATTACH), …that command's other parameters} → as that command",
            )
            .interactive(|_| Ok(Box::new(AttachM { kind: None, ..AttachM::default() }))),
        CommandSpec::new("imageattach", "Attach Image", run_image)
            .menu(&["Insert", "Raster Image Reference..."])
            .alias(&["iat"])
            .params(
                "{path (PNG or JPEG file; relative paths are made absolute from the working directory), data? (base64 bytes of the picture, for the web build or generated pictures: used instead of reading the file, \
                 `path` then only names it for this session), at? ([x, y] of the lower-left corner, default [0, 0]), width? (drawing width of the picture) | scale? (drawing units per pixel, default 1), \
                 rotation? (degrees counter-clockwise about `at`, default 0), layer? (default the current layer)} \
                 → {handle, path, width, height (drawing units), pixels: [w, h]}. Pictures over 50 megapixels are refused.",
            )
            .interactive(|_| Ok(Box::new(AttachM { kind: Some(false), ..AttachM::default() }))),
        CommandSpec::new("pdfattach", "Attach PDF Underlay", run_pdf)
            .menu(&["Insert", "PDF Underlay..."])
            .alias(&["pat"])
            .params(
                "{path (PDF file), data? (base64 bytes, as for IMAGEATTACH), page? (1-based, default 1), at? ([x, y] of the page's lower-left corner, default [0, 0]), width? (drawing width of the page) | \
                 scale? (drawing units per inch of the page, as the DXF underlay scale counts it: 1 shows a US Letter page 8.5 wide, default 1), rotation? (degrees, default 0), layer? (default the current layer)} \
                 → {handle, path, page, pages, width, height (drawing units)}. The page is drawn on white paper from its paths, text, images, shadings, patterns and forms.",
            )
            .interactive(|_| Ok(Box::new(AttachM { kind: Some(true), ..AttachM::default() }))),
    ]
}

/// The path to store and, for `data`, the registered name; the picture must be readable by it.
pub(super) fn make_available(cmd: &str, p: &Value, path: &str) -> Result<String> {
    if let Some(data) = p.get("data").filter(|v| !v.is_null()) {
        let text = data.as_str().ok_or_else(|| bad(cmd, "`data` must be base64 text"))?;
        let bytes = super::file::base64_decode(text).filter(|b| !b.is_empty()).ok_or_else(|| bad(cmd, "`data` is not valid base64"))?;
        images::put_bytes(path, bytes).map_err(|e| bad(cmd, e))?;
        return Ok(path.to_string());
    }
    #[cfg(target_arch = "wasm32")]
    return Err(bad(cmd, "files can't be opened on the web: pass the picture's bytes as base64 `data`"));
    #[cfg(not(target_arch = "wasm32"))]
    Ok(absolute(path))
}

/// `path` made absolute from the working directory (as given when that fails).
pub(super) fn absolute(path: &str) -> String {
    std::path::absolute(path).map(|a| a.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string())
}

pub(super) fn path_param<'a>(cmd: &str, p: &'a Value) -> Result<&'a str> {
    str_param(p, "path").map(str::trim).filter(|x| !x.is_empty()).ok_or_else(|| bad(cmd, "`path` is required"))
}

fn run_image(s: &mut Session, p: &Value) -> Result<Value> {
    attach(s, "imageattach", p, false)
}

fn run_pdf(s: &mut Session, p: &Value) -> Result<Value> {
    attach(s, "pdfattach", p, true)
}

fn run_attach(s: &mut Session, p: &Value) -> Result<Value> {
    let path = path_param("attach", p)?;
    let ext = std::path::Path::new(path).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" => attach(s, "attach", p, false),
        "pdf" => attach(s, "attach", p, true),
        "dwf" | "dwfx" => super::attach_cad::run_dwf(s, p),
        "dgn" => super::attach_cad::run_dgn(s, p),
        _ => Err(bad("attach", "attaches PNG, JPEG, PDF, DWF, DWFx and DGN files; use XATTACH for drawings")),
    }
}

/// The drawing units per unit of the picture (pixel or point) from `width` (the drawing width of
/// the picture) or `scale` (drawing units per `per` picture units: 1 for a pixel, 72 for a PDF
/// page, whose scale counts inches).
pub(super) fn unit_scale(cmd: &str, p: &Value, natural: f64, per: f64) -> Result<f64> {
    let width = size_param(cmd, p, "width", false)?;
    let scale = size_param(cmd, p, "scale", false)?;
    let unit = match (width, scale) {
        (Some(_), Some(_)) => return Err(bad(cmd, "give `width` or `scale`, not both")),
        (Some(w), None) => w / natural,
        (None, Some(sc)) => sc / per,
        (None, None) => 1.0 / per,
    };
    if unit.is_finite() && unit > 0.0 && unit * natural <= LIMIT && unit * natural >= 1e-9 {
        Ok(unit)
    } else {
        Err(bad(cmd, "the picture would be too large or too small"))
    }
}

/// Where a picture goes: the lower-left corner, the rotation about it (degrees) and the layer.
pub(super) struct Placement {
    pub at: Vec2,
    pub rotation: f64,
    pub layer: Option<String>,
}

/// The `at`, `rotation` and `layer` parameters.
pub(super) fn placement(cmd: &str, p: &Value) -> Result<Placement> {
    let at = match p.get("at") {
        None | Some(Value::Null) => Vec2::ZERO,
        Some(v) => super::point_value(v).filter(|v| v.x.abs() <= LIMIT && v.y.abs() <= LIMIT).ok_or_else(|| bad(cmd, "`at` must be [x, y]"))?,
    };
    let rotation = match p.get("rotation") {
        None | Some(Value::Null) => 0.0,
        Some(v) => v.as_f64().filter(|r| r.is_finite()).ok_or_else(|| bad(cmd, "`rotation` must be a number of degrees"))?,
    };
    let layer = match p.get("layer").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            Some(v.as_str().filter(|n| super::layer::valid_name(n)).ok_or_else(|| bad(cmd, "`layer` must be a valid layer name"))?.trim().to_string())
        }
    };
    Ok(Placement { at, rotation, layer })
}

fn attach(s: &mut Session, cmd: &str, p: &Value, pdf: bool) -> Result<Value> {
    let path = path_param(cmd, p)?;
    s.doc()?;
    let page = if pdf {
        match p.get("page") {
            None | Some(Value::Null) => 1,
            Some(v) => v.as_u64().filter(|n| *n >= 1).and_then(|n| u32::try_from(n).ok()).ok_or_else(|| bad(cmd, "`page` must be 1 or more"))?,
        }
    } else {
        0
    };
    let stored = make_available(cmd, p, path)?;
    let natural = images::natural_size(&stored, page).map_err(|e| bad(cmd, e))?;
    let unit = unit_scale(cmd, p, natural.0, if pdf { POINTS_PER_INCH } else { 1.0 })?;
    let Placement { at, rotation, layer } = placement(cmd, p)?;
    let handle = place(s, &stored, page, natural, at, unit, rotation, layer.as_deref())?;
    let (w, h) = (natural.0 * unit, natural.1 * unit);
    let mut out = json!({ "handle": handle, "path": stored, "width": w, "height": h });
    if pdf {
        out["page"] = json!(page);
        out["pages"] = json!(page_count(&stored).unwrap_or(0));
    } else {
        out["pixels"] = json!([natural.0, natural.1]);
    }
    Ok(out)
}

/// How many pages the PDF has (`None` when it can't be told).
fn page_count(path: &str) -> Option<usize> {
    let bytes = images::read_bytes(path).ok()?;
    cadcraft_io::pdf_markup::pdf_page_content(&bytes, 1, "").ok().map(|c| c.page_count)
}

/// Add the IMAGE object of a picture, drawn at `unit` drawing units per pixel (point), turned by
/// `rotation` degrees about `at`, and return its handle.
pub(super) fn place(
    s: &mut Session,
    stored: &str,
    page: u32,
    natural: (f64, f64),
    at: Vec2,
    unit: f64,
    rotation: f64,
    layer: Option<&str>,
) -> Result<String> {
    let (sin, cos) = rotation.to_radians().sin_cos();
    let image = Image {
        insert: v3(at),
        u: Vec3::new(cos * unit, sin * unit, 0.0),
        v: Vec3::new(-sin * unit, cos * unit, 0.0),
        size: Vec2::new(natural.0, natural.1),
        path: stored.to_string(),
        page,
        ..Image::default()
    };
    let space = s.space();
    let mut common: Common = s.current_common();
    let d = s.doc_mut()?;
    if let Some(l) = layer {
        d.ensure_layer(l);
        common.layer = l.into();
    }
    d.add(&space, common, EntityKind::Image(image)).map(|h| h.hex()).map_err(|e| EngineError::Other(e.to_string()))
}

/// The prompts: file, (page,) insertion point, scale factor, rotation.
#[derive(Default)]
struct AttachM {
    /// `None` for ATTACH (the extension decides), else `Some(is_pdf)`.
    kind: Option<bool>,
    path: Option<String>,
    page: Option<u32>,
    at: Option<Vec2>,
    scale: Option<f64>,
}

impl AttachM {
    fn pdf(&self) -> bool {
        self.kind.unwrap_or_else(|| self.path.as_deref().is_some_and(|p| p.to_ascii_lowercase().ends_with(".pdf")))
    }
    fn name(&self) -> &'static str {
        match self.kind {
            None => "ATTACH",
            Some(false) => "IMAGEATTACH",
            Some(true) => "PDFATTACH",
        }
    }
}

impl Interactive for AttachM {
    fn name(&self) -> &'static str {
        AttachM::name(self)
    }
    fn prompt(&self, _s: &Session) -> Prompt {
        match (&self.path, self.page, self.at, self.scale) {
            (None, ..) => Prompt::new("Enter the file to attach (PNG, JPG or PDF)", Accept::TEXT),
            (Some(_), None, ..) if self.pdf() => Prompt::new("Enter the page number", Accept::NUMBER).default("1"),
            (Some(_), _, None, _) => Prompt::new("Specify the insertion point (lower-left corner)", Accept::POINT).default("0,0"),
            (Some(_), _, Some(at), None) => {
                let per = if self.pdf() { "inch of the page" } else { "pixel" };
                Prompt::new(format!("Specify the scale factor (drawing units per {per})"), Accept::NUMBER).default("1").base(at)
            }
            _ => Prompt::new("Specify the rotation angle", Accept::NUMBER).default("0"),
        }
    }
    fn input(&mut self, s: &mut Session, i: Input) -> Result<Step> {
        let cmd = self.name().to_ascii_lowercase();
        match (&self.path, self.page, self.at, self.scale, i) {
            (.., Input::Cancel) => Ok(Step::Cancel),
            (None, _, _, _, Input::Text(t) | Input::Keyword(t)) if !t.trim().is_empty() => {
                self.path = Some(t.trim().trim_matches('"').to_string());
                Ok(Step::Continue)
            }
            (Some(_), None, _, _, i @ (Input::Enter | Input::Text(_))) if self.pdf() => {
                self.page = Some(match i {
                    Input::Text(t) => t.trim().parse::<u32>().ok().filter(|n| *n >= 1).ok_or_else(|| bad(&cmd, "enter a page number from 1"))?,
                    _ => 1,
                });
                Ok(Step::Continue)
            }
            (Some(_), _, None, _, Input::Point(p)) => {
                self.at = Some(p);
                Ok(Step::Continue)
            }
            (Some(_), _, None, _, Input::Enter) => {
                self.at = Some(Vec2::ZERO);
                Ok(Step::Continue)
            }
            (Some(_), _, Some(_), None, i @ (Input::Enter | Input::Text(_))) => {
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
            (Some(path), page, Some(at), Some(scale), i @ (Input::Enter | Input::Text(_))) => {
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
                if let Some(pg) = page {
                    p["page"] = json!(pg);
                }
                let r = match super::attach_cad::kind_of(path) {
                    Some(cadcraft_doc::UnderlayKind::Dwf) => super::attach_cad::run_dwf(s, &p),
                    Some(cadcraft_doc::UnderlayKind::Dgn) => super::attach_cad::run_dgn(s, &p),
                    _ if self.pdf() => run_pdf(s, &p),
                    _ => run_image(s, &p),
                }?;
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
#[path = "attach_tests.rs"]
mod tests;
