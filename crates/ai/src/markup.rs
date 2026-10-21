//! Reading a markup image (a scan or photo of a marked-up sheet) with the vision model.

use serde_json::{Value, json};

use crate::{AiError, Config, Image, Message, chat};

const PROMPT: &str = "You read marked-up drawing sheets. The image is a sheet with a reviewer's handwritten or drawn \
markups (red pen, clouds, arrows, notes). List every markup as JSON and answer with ONLY a JSON array, no prose. \
Coordinates are fractions of the image: u from the left edge, v from the TOP edge, both between 0 and 1. Items: \
{\"kind\":\"text\",\"text\":\"...\",\"at\":[u,v],\"height\":h} (at = top-left of the note, h = its height as a fraction of the image height); \
{\"kind\":\"cloud\"|\"polyline\"|\"rect\"|\"ellipse\",\"points\":[[u,v],...]} (rect and ellipse: two opposite corners); \
{\"kind\":\"leader\",\"points\":[[u,v],...],\"text\":\"...\"} (first point = arrow head). Optional \"color\":\"#rrggbb\". \
If there are no markups answer [].";

/// The most markups kept from one answer.
const MAX_MARKUPS: usize = 500;
/// The most points kept on one markup.
const MAX_POINTS: usize = 1000;

/// The JSON in the model's answer: bare, or in a ``` fence, or the outermost `[...]`.
fn extract_json(reply: &str) -> Option<Value> {
    let t = reply.trim();
    if let Ok(v) = serde_json::from_str(t) {
        return Some(v);
    }
    let fenced = t.split("```").nth(1).map(|b| b.strip_prefix("json").or_else(|| b.strip_prefix("JSON")).unwrap_or(b));
    if let Some(v) = fenced.and_then(|b| serde_json::from_str(b.trim()).ok()) {
        return Some(v);
    }
    let (a, z) = (t.find('[')?, t.rfind(']')?);
    serde_json::from_str(t.get(a..=z)?).ok()
}

fn unit(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| (0.0..=1.0).contains(f))
}

fn point(v: &Value) -> Option<Value> {
    let a = v.as_array().filter(|a| a.len() == 2)?;
    Some(json!([unit(&a[0])?, unit(&a[1])?]))
}

/// A markup in the contract's shape, or `None` when it isn't one.
fn clean(m: &Value) -> Option<Value> {
    let kind = m.get("kind")?.as_str()?;
    let mut out = json!({"kind": kind});
    let points = || -> Option<Vec<Value>> { m.get("points")?.as_array()?.iter().take(MAX_POINTS).map(point).collect() };
    match kind {
        "text" => {
            let text = m.get("text")?.as_str().filter(|t| !t.trim().is_empty())?;
            out["text"] = json!(text);
            out["at"] = point(m.get("at")?)?;
            out["height"] = json!(m.get("height").and_then(Value::as_f64).filter(|h| *h > 0.0 && *h <= 1.0).unwrap_or(0.02));
        }
        "cloud" | "polyline" | "rect" | "ellipse" | "leader" => {
            let pts = points()?;
            if pts.len() < 2 || (matches!(kind, "rect" | "ellipse") && pts.len() != 2) {
                return None;
            }
            out["points"] = Value::Array(pts);
            if let Some(t) = m.get("text").and_then(Value::as_str).filter(|_| kind == "leader") {
                out["text"] = json!(t);
            }
        }
        _ => return None,
    }
    if let Some(c) =
        m.get("color").and_then(Value::as_str).filter(|c| c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit()))
    {
        out["color"] = json!(c);
    }
    Some(out)
}

/// The markups in the model's answer (the contract's markup JSON); items that aren't valid are dropped.
fn parse_markups(reply: &str) -> Result<Vec<Value>, AiError> {
    let v = extract_json(reply).ok_or_else(|| AiError::Reply("the model's answer has no JSON markups".into()))?;
    let list = match &v {
        Value::Array(a) => a.as_slice(),
        _ => v
            .get("markups")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .ok_or_else(|| AiError::Reply("the model's answer is not a list of markups".into()))?,
    };
    Ok(list.iter().filter_map(clean).take(MAX_MARKUPS).collect())
}

/// Ask the vision model for the markups in a PNG or JPEG (`mime`: `image/png` or `image/jpeg`).
/// Blocks: call it from a worker thread.
pub fn recognize_image_markups(cfg: &Config, bytes: &[u8], mime: &str) -> Result<Vec<Value>, AiError> {
    let image = Image { mime: mime.to_string(), base64: cadcraft_engine::cmd::file::base64_encode(bytes) };
    let messages = [Message::System(PROMPT.into()), Message::User { text: "Read the markups in this image.".into(), images: vec![image] }];
    let reply = chat(cfg, &messages, &Value::Null)?;
    parse_markups(&reply.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markups_come_out_of_bare_fenced_and_wrapped_answers_and_invalid_items_are_dropped() {
        let items = r##"[{"kind":"text","text":"move wall","at":[0.1,0.2],"height":0.03,"color":"#ff0000"},
            {"kind":"cloud","points":[[0.1,0.1],[0.3,0.1],[0.3,0.3]]},
            {"kind":"rect","points":[[0.1,0.1]]},
            {"kind":"text","text":"","at":[0.1,0.1]},
            {"kind":"text","text":"x","at":[1.5,0.1]},
            {"kind":"squiggle","points":[[0,0],[1,1]]},
            {"kind":"leader","points":[[0.5,0.5],[0.6,0.6]],"text":"here","color":"red"},
            7]"##;
        for reply in
            [items.to_string(), format!("Here you go:\n```json\n{items}\n```"), format!("{{\"markups\": {items}}}"), format!("Sure! {items} Done.")]
        {
            let m = parse_markups(&reply).unwrap();
            assert_eq!(m.len(), 3, "{reply}");
            assert_eq!(m[0]["color"], "#ff0000");
            assert_eq!(m[1]["kind"], "cloud");
            assert_eq!((m[2]["kind"].as_str(), m[2]["text"].as_str(), m[2].get("color")), (Some("leader"), Some("here"), None));
        }
        assert_eq!(parse_markups("[]").unwrap().len(), 0);
    }

    #[test]
    fn prose_and_garbage_are_errors() {
        for reply in ["", "I cannot see markups.", "{\"a\":1}", "[[[", "```\n```"] {
            assert!(parse_markups(reply).is_err(), "{reply}");
        }
    }
}
