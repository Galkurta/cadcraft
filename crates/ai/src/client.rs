//! A client for an OpenAI-compatible Chat Completions endpoint (Ollama, LM Studio, OpenAI,
//! OpenRouter, vLLM…): request bodies, the reply parser (one JSON body or an SSE stream) and a
//! blocking [`chat`] for worker threads. The Assistant itself runs [`crate::Driver`].

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::transport::{self, Flight, Wake};

/// Whether pictures are sent to the model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vision {
    /// Send them; if the endpoint refuses them, retry the turn without and stop sending.
    #[default]
    Auto,
    On,
    Off,
}

impl Vision {
    /// `auto`, `on`/`true`/`1`/`yes` or `off`/`false`/`0`/`no`, any case.
    pub fn parse(s: &str) -> Option<Vision> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Vision::Auto),
            "on" | "true" | "1" | "yes" => Some(Vision::On),
            "off" | "false" | "0" | "no" => Some(Vision::Off),
            _ => None,
        }
    }
}

/// The endpoint, model and key. Empty fields mean "not set".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub vision: Vision,
}

/// Where a local Ollama listens.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

impl Config {
    /// `CADCRAFT_AI_BASE_URL` (default [`DEFAULT_BASE_URL`]), `CADCRAFT_AI_MODEL`,
    /// `CADCRAFT_AI_KEY` falling back to `OPENAI_API_KEY`, and `CADCRAFT_AI_VISION`
    /// (`auto`, `on`, `off`).
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Config {
            base_url: var("CADCRAFT_AI_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into()),
            model: var("CADCRAFT_AI_MODEL").unwrap_or_default(),
            api_key: var("CADCRAFT_AI_KEY").or_else(|| var("OPENAI_API_KEY")).unwrap_or_default(),
            vision: var("CADCRAFT_AI_VISION").and_then(|v| Vision::parse(&v)).unwrap_or_default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("blocking chat is not available in the web build")]
    Unavailable,
    #[error("{0}")]
    Config(String),
    #[error("can't reach the AI endpoint: {0}")]
    Transport(String),
    #[error("the AI endpoint answered {status}: {body}")]
    Status { status: u16, body: String },
    #[error("unusable reply from the AI endpoint: {0}")]
    Reply(String),
    #[error("stopped")]
    Stopped,
}

/// A picture sent to the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub mime: String,
    /// Standard base64.
    pub base64: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The arguments as the model wrote them: a JSON object in text.
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    System(String),
    User { text: String, images: Vec<Image> },
    Assistant { text: String, tool_calls: Vec<ToolCall> },
    Tool { call_id: String, text: String },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message::User { text: text.into(), images: Vec::new() }
    }

    /// `images`: false drops the pictures and tells the model they were left out.
    fn to_json(&self, images_ok: bool) -> Value {
        match self {
            Message::System(t) => json!({"role": "system", "content": t}),
            Message::User { text, images } if images.is_empty() => json!({"role": "user", "content": text}),
            Message::User { text, images } if !images_ok => {
                json!({"role": "user", "content": format!("{text}\n[{} image(s) left out: this model can't see images]", images.len())})
            }
            Message::User { text, images } => {
                let mut parts = vec![json!({"type": "text", "text": text})];
                parts.extend(
                    images.iter().map(|i| json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{}", i.mime, i.base64)}})),
                );
                json!({"role": "user", "content": parts})
            }
            Message::Assistant { text, tool_calls } => {
                let mut m = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
                if !tool_calls.is_empty() {
                    m["tool_calls"] = tool_calls
                        .iter()
                        .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                        .collect();
                }
                m
            }
            Message::Tool { call_id, text } => json!({"role": "tool", "tool_call_id": call_id, "content": text}),
        }
    }
}

/// The model's answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

/// The request body: `tools` is [`crate::tool_definitions`]' `{"tools": [...]}` (or `Null` for none).
fn request_body(cfg: &Config, messages: &[Message], tools: &Value, opts: Options) -> Value {
    let mut body = json!({"model": cfg.model, "messages": messages.iter().map(|m| m.to_json(opts.images)).collect::<Vec<_>>()});
    if opts.stream {
        body["stream"] = Value::Bool(true);
    }
    let functions: Vec<Value> = tools["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["inputSchema"]}}))
        .collect();
    if !functions.is_empty() {
        body["tools"] = Value::Array(functions);
    }
    body
}

/// `choices[0].message` of a Chat Completions reply.
fn parse_reply(body: &str) -> Result<Reply, AiError> {
    let bad = |m: &str| AiError::Reply(m.to_string());
    let v: Value = serde_json::from_str(body).map_err(|e| AiError::Reply(format!("not JSON ({e})")))?;
    if let Some(e) = v.get("error") {
        return Err(AiError::Reply(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()));
    }
    let m = v.pointer("/choices/0/message").filter(|m| m.is_object()).ok_or_else(|| bad("no `choices[0].message`"))?;
    // `content` is text, null (tool calls only), or on some servers a list of `{text}` parts.
    let text = match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""),
        _ => String::new(),
    };
    let mut tool_calls = Vec::new();
    for (i, c) in m["tool_calls"].as_array().into_iter().flatten().enumerate() {
        let name = c.pointer("/function/name").and_then(Value::as_str).ok_or_else(|| bad("tool call without a function name"))?;
        let arguments = match c.pointer("/function/arguments") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => "{}".to_string(),
            Some(other) => other.to_string(),
        };
        let id = c["id"].as_str().map_or_else(|| format!("call_{i}"), str::to_string);
        tool_calls.push(ToolCall { id, name: name.to_string(), arguments });
    }
    if text.is_empty() && tool_calls.is_empty() {
        return Err(bad("empty message"));
    }
    Ok(Reply { text, tool_calls })
}

/// A line of an event stream is at most this long.
const MAX_LINE_BYTES: usize = 1 << 20;
/// Parallel tool calls kept from one streamed answer.
const MAX_CALLS: usize = 64;

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

#[derive(PartialEq, Eq)]
enum Mode {
    Undecided,
    /// One JSON body (a non-streaming reply, or a server that ignores `stream`).
    Json,
    /// Server-sent events: `data: {chunk}` lines up to `data: [DONE]`.
    Sse,
}

/// Assembles the model's answer from the bytes of the reply as they arrive: either one JSON body
/// or an SSE stream of `choices[0].delta` chunks (text pieces, tool calls by index).
pub(crate) struct Parser {
    mode: Mode,
    buf: Vec<u8>,
    text: String,
    calls: std::collections::BTreeMap<usize, PartialCall>,
    done: bool,
    /// Text was handed to `emit` while streaming.
    pub streamed: bool,
}

impl Parser {
    pub fn new() -> Self {
        Parser { mode: Mode::Undecided, buf: Vec::new(), text: String::new(), calls: Default::default(), done: false, streamed: false }
    }

    /// Feed received bytes; `emit` gets each piece of answer text as soon as it is complete.
    pub fn feed(&mut self, bytes: &[u8], emit: &mut dyn FnMut(&str)) -> Result<(), AiError> {
        if self.done {
            return Ok(());
        }
        self.buf.extend_from_slice(bytes);
        if self.mode == Mode::Undecided {
            match self.buf.iter().find(|b| !b.is_ascii_whitespace()) {
                None => return Ok(()),
                Some(b'{' | b'[') => self.mode = Mode::Json,
                Some(_) => self.mode = Mode::Sse,
            }
        }
        if self.mode == Mode::Json {
            return Ok(());
        }
        let mut start = 0;
        while let Some(len) = self.buf.get(start..).and_then(|rest| rest.iter().position(|&b| b == b'\n')) {
            let line = self.buf.get(start..start + len).map(|l| String::from_utf8_lossy(l).into_owned()).unwrap_or_default();
            start += len + 1;
            self.line(&line, emit)?;
            if self.done {
                self.buf.clear();
                return Ok(());
            }
        }
        self.buf.drain(..start);
        if self.buf.len() > MAX_LINE_BYTES {
            return Err(AiError::Reply("a line of the stream is too long".into()));
        }
        Ok(())
    }

    fn line(&mut self, line: &str, emit: &mut dyn FnMut(&str)) -> Result<(), AiError> {
        // Other fields (`event:`, `id:`, `retry:`) and `:` comments carry nothing we use.
        let Some(data) = line.trim_end_matches('\r').strip_prefix("data:").map(str::trim) else { return Ok(()) };
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        if data.is_empty() {
            return Ok(());
        }
        let v: Value = serde_json::from_str(data).map_err(|e| AiError::Reply(format!("bad stream chunk ({e})")))?;
        if let Some(e) = v.get("error") {
            return Err(AiError::Reply(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()));
        }
        let Some(delta) = v.pointer("/choices/0/delta") else { return Ok(()) };
        if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(t);
            self.streamed = true;
            emit(t);
        }
        for (i, c) in delta["tool_calls"].as_array().into_iter().flatten().enumerate() {
            let index = usize::try_from(c["index"].as_u64().unwrap_or(i as u64)).unwrap_or(usize::MAX);
            if index >= MAX_CALLS {
                return Err(AiError::Reply("too many tool calls in one answer".into()));
            }
            let call = self.calls.entry(index).or_default();
            if let Some(id) = c["id"].as_str().filter(|s| !s.is_empty()) {
                call.id = id.to_string();
            }
            if call.name.is_empty() {
                call.name = c.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_string();
            }
            match c.pointer("/function/arguments") {
                Some(Value::String(s)) => call.args.push_str(s),
                Some(Value::Null) | None => {}
                Some(other) => call.args = other.to_string(),
            }
        }
        Ok(())
    }

    /// The reply is complete (the connection closed).
    pub fn finish(mut self, emit: &mut dyn FnMut(&str)) -> Result<Reply, AiError> {
        match self.mode {
            Mode::Undecided => Err(AiError::Reply("empty reply".into())),
            Mode::Json => parse_reply(&String::from_utf8_lossy(&self.buf)),
            Mode::Sse => {
                // A last line without its newline.
                if !self.done && !self.buf.is_empty() {
                    self.buf.push(b'\n');
                    let rest = std::mem::take(&mut self.buf);
                    self.feed(&rest, emit)?;
                }
                let mut tool_calls = Vec::new();
                for (i, c) in self.calls.into_values().enumerate() {
                    if c.name.is_empty() {
                        return Err(AiError::Reply("tool call without a function name".into()));
                    }
                    let id = if c.id.is_empty() { format!("call_{i}") } else { c.id };
                    let arguments = if c.args.trim().is_empty() { "{}".to_string() } else { c.args };
                    tool_calls.push(ToolCall { id, name: c.name, arguments });
                }
                if self.text.is_empty() && tool_calls.is_empty() {
                    return Err(AiError::Reply("empty message".into()));
                }
                Ok(Reply { text: self.text, tool_calls })
            }
        }
    }
}

/// What one request is asked to include.
#[derive(Clone, Copy)]
pub(crate) struct Options {
    /// Send the pictures in the conversation (otherwise they become a short text note).
    pub images: bool,
    /// Ask for a streamed (SSE) reply.
    pub stream: bool,
}

/// A request in flight and the answer being assembled from it.
pub(crate) struct Exchange {
    flight: Flight,
    parser: Option<Parser>,
    status: Option<u16>,
    error_body: Vec<u8>,
    /// Answer text was handed out while it arrived.
    pub streamed: bool,
}

/// An error status's body is kept this far (the first 500 characters are shown).
const MAX_ERROR_BYTES: usize = 64 << 10;

impl Exchange {
    pub fn start(cfg: &Config, messages: &[Message], tools: &Value, opts: Options, wake: Wake) -> Result<Self, AiError> {
        if cfg.model.trim().is_empty() {
            return Err(AiError::Config("no AI model set: set CADCRAFT_AI_MODEL or fill in the Assistant settings".into()));
        }
        let flight = transport::start(cfg, &request_body(cfg, messages, tools, opts), wake)?;
        Ok(Exchange { flight, parser: Some(Parser::new()), status: None, error_body: Vec::new(), streamed: false })
    }

    pub fn cancel(&self) {
        self.flight.cancel();
    }

    /// Take what arrived. `None` while the reply is still coming; the answer (or why there is
    /// none) once it is over. Text pieces go to `emit`.
    pub fn poll(&mut self, emit: &mut dyn FnMut(&str)) -> Option<Result<Reply, AiError>> {
        let d = self.flight.drain();
        self.status = d.status;
        let ok = self.status.map(|s| (200..300).contains(&s));
        if !d.data.is_empty() {
            match (ok, self.parser.as_mut()) {
                (Some(true), Some(p)) => {
                    if let Err(e) = p.feed(&d.data, emit) {
                        self.flight.cancel();
                        return Some(Err(e));
                    }
                    self.streamed = p.streamed;
                }
                (Some(false), _) => {
                    let room = MAX_ERROR_BYTES.saturating_sub(self.error_body.len());
                    self.error_body.extend(d.data.iter().take(room));
                }
                _ => {}
            }
        }
        let finished = d.finished?;
        Some(match (finished, self.status) {
            (Err(e), _) => Err(AiError::Transport(e)),
            (Ok(()), Some(s)) if !(200..300).contains(&s) => {
                Err(AiError::Status { status: s, body: String::from_utf8_lossy(&self.error_body).chars().take(500).collect() })
            }
            (Ok(()), Some(_)) => match self.parser.take() {
                Some(p) => p.finish(emit),
                None => Err(AiError::Reply("empty reply".into())),
            },
            (Ok(()), None) => Err(AiError::Transport("no answer from the AI endpoint".into())),
        })
    }
}

/// Send the conversation and wait for the model's answer (not streamed, pictures included per
/// `cfg.vision`). Blocks: call it from a worker thread; the web build can't block, it reports
/// [`AiError::Unavailable`] (the Assistant drives [`crate::Driver`] instead).
#[cfg(not(target_arch = "wasm32"))]
pub fn chat(cfg: &Config, messages: &[Message], tools: &Value) -> Result<Reply, AiError> {
    let opts = Options { images: cfg.vision != Vision::Off, stream: false };
    let mut ex = Exchange::start(cfg, messages, tools, opts, std::sync::Arc::new(|| {}))?;
    loop {
        if let Some(r) = ex.poll(&mut |_| {}) {
            return r;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(target_arch = "wasm32")]
pub fn chat(_: &Config, _: &[Message], _: &Value) -> Result<Reply, AiError> {
    Err(AiError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_parse_in_the_shapes_servers_send() {
        let r = parse_reply(r#"{"choices":[{"message":{"content":"hi"}}]}"#).unwrap();
        assert_eq!((r.text.as_str(), r.tool_calls.len()), ("hi", 0));
        let r = parse_reply(
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"render","arguments":"{\"fit\":true}"}},{"function":{"name":"cancel","arguments":{}}}]}}]}"#,
        )
        .unwrap();
        assert_eq!(r.tool_calls[0], ToolCall { id: "c1".into(), name: "render".into(), arguments: "{\"fit\":true}".into() });
        assert_eq!((r.tool_calls[1].id.as_str(), r.tool_calls[1].arguments.as_str()), ("call_1", "{}"));
        let r = parse_reply(r#"{"choices":[{"message":{"content":[{"type":"text","text":"a"},{"text":"b"}]}}]}"#).unwrap();
        assert_eq!(r.text, "ab");
    }

    #[test]
    fn garbage_replies_are_errors() {
        for body in [
            "",
            "x",
            "[]",
            "{}",
            r#"{"choices":[]}"#,
            r#"{"choices":[{"message":3}]}"#,
            r#"{"choices":[{"message":{}}]}"#,
            r#"{"error":{"message":"quota"}}"#,
            r#"{"choices":[{"message":{"tool_calls":[{}]}}]}"#,
        ] {
            assert!(matches!(parse_reply(body), Err(AiError::Reply(_))), "{body}");
        }
    }

    #[test]
    fn request_carries_tools_images_and_tool_results() {
        let cfg = Config { model: "m".into(), ..Config::default() };
        let msgs = [
            Message::System("s".into()),
            Message::User { text: "look".into(), images: vec![Image { mime: "image/png".into(), base64: "QUJD".into() }] },
            Message::Assistant { text: String::new(), tool_calls: vec![ToolCall { id: "1".into(), name: "render".into(), arguments: "{}".into() }] },
            Message::Tool { call_id: "1".into(), text: "ok".into() },
        ];
        let with = Options { images: true, stream: false };
        let b = request_body(&cfg, &msgs, &crate::tool_definitions(false), with);
        assert_eq!(b["model"], "m");
        assert!(b.get("stream").is_none());
        assert_eq!(b["messages"][1]["content"][1]["image_url"]["url"], "data:image/png;base64,QUJD");
        assert_eq!(b["messages"][2]["tool_calls"][0]["function"]["name"], "render");
        assert_eq!(b["messages"][3]["tool_call_id"], "1");
        let f = b["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "command_line").unwrap();
        assert_eq!(f["function"]["parameters"]["required"][0], "text");
        assert!(request_body(&cfg, &msgs, &Value::Null, with).get("tools").is_none());
        // Without pictures the image part is gone and the model is told.
        let b = request_body(&cfg, &msgs, &Value::Null, Options { images: false, stream: true });
        assert_eq!(b["stream"], true);
        let c = b["messages"][1]["content"].as_str().unwrap();
        assert!(c.starts_with("look") && c.contains("1 image(s) left out"), "{c}");
        assert!(!b.to_string().contains("image_url"));
    }

    #[test]
    fn a_missing_model_or_a_bad_url_is_a_config_error() {
        let mut cfg = Config { base_url: "http://localhost:1/v1".into(), ..Config::default() };
        assert!(matches!(chat(&cfg, &[], &Value::Null), Err(AiError::Config(_))));
        cfg.model = "m".into();
        cfg.base_url = "ftp://x".into();
        assert!(matches!(chat(&cfg, &[], &Value::Null), Err(AiError::Config(_))));
    }

    /// Feed `chunks` (any split of the bytes) and finish; returns the answer and the emitted text pieces.
    fn stream(chunks: &[&str]) -> (Result<Reply, AiError>, Vec<String>) {
        let mut pieces = Vec::new();
        let mut p = Parser::new();
        for c in chunks {
            if let Err(e) = p.feed(c.as_bytes(), &mut |t| pieces.push(t.to_string())) {
                return (Err(e), pieces);
            }
        }
        let r = p.finish(&mut |t| pieces.push(t.to_string()));
        (r, pieces)
    }

    #[test]
    fn a_stream_reassembles_text_and_split_tool_calls_wherever_the_bytes_break() {
        let body = concat!(
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data:{\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\r\n\r\n",
            "event: x\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"command_line\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"text\\\":\\\"cir\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"c2\",\"function\":{\"name\":\"render\"}},{\"index\":0,\"function\":{\"arguments\":\"cle 0,0 5\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
            "data: {\"ignored\":after done}\n",
        );
        let whole = stream(&[body]);
        // One byte at a time, including in the middle of the multi-byte `…`-free JSON and the CRLF.
        let bytes: Vec<String> = body.chars().map(String::from).collect();
        let single = stream(&bytes.iter().map(String::as_str).collect::<Vec<_>>());
        for (r, pieces) in [whole, single] {
            let r = r.unwrap();
            assert_eq!(r.text, "Hello");
            assert_eq!(pieces, ["Hel", "lo"]);
            assert_eq!(r.tool_calls.len(), 2);
            assert_eq!(r.tool_calls[0], ToolCall { id: "c1".into(), name: "command_line".into(), arguments: "{\"text\":\"circle 0,0 5\"}".into() });
            assert_eq!((r.tool_calls[1].id.as_str(), r.tool_calls[1].name.as_str(), r.tool_calls[1].arguments.as_str()), ("c2", "render", "{}"));
        }
    }

    #[test]
    fn a_plain_json_body_and_a_missing_done_are_fine() {
        let (r, pieces) = stream(&["  \n", r#"{"choices":[{"message":{"content":"hi"}}]}"#]);
        assert_eq!((r.unwrap().text, pieces.len()), ("hi".to_string(), 0));
        // The stream just ends, the last line without its newline.
        let (r, _) = stream(&["data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n", "data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}"]);
        assert_eq!(r.unwrap().text, "ab");
        // Ollama sends tool arguments as an object, and no id.
        let (r, _) =
            stream(&["data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"name\":\"render\",\"arguments\":{\"fit\":true}}}]}}]}\n"]);
        let r = r.unwrap();
        assert_eq!((r.tool_calls[0].id.as_str(), r.tool_calls[0].arguments.as_str()), ("call_0", "{\"fit\":true}"));
    }

    #[test]
    fn garbage_streams_are_errors_not_panics() {
        let long = "x".repeat(MAX_LINE_BYTES + 1);
        let calls: String = (0..=MAX_CALLS).map(|i| format!("{{\"index\":{i},\"function\":{{\"name\":\"a\"}}}}")).collect::<Vec<_>>().join(",");
        let many = format!("data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{calls}]}}}}]}}\n");
        let cases: Vec<(&str, &str)> = vec![
            ("", "empty"),
            ("data: [DONE]\n", "no content"),
            ("data: not json\n", "bad chunk"),
            ("data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\ndata: {\n", "bad second chunk"),
            ("data: {\"error\":{\"message\":\"overloaded\"}}\n", "error chunk"),
            ("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":4000000000}]}}]}\n", "huge index"),
            ("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":-1,\"function\":{\"name\":\"\"}}]}}]}\n", "nameless call"),
            (&long, "endless line"),
            (&many, "too many calls"),
            ("data: {\"choices\":[{\"delta\":{\"tool_calls\":\"x\",\"content\":5}}]}\n", "wrong types"),
        ];
        for (body, why) in cases {
            assert!(matches!(stream(&[body]).0, Err(AiError::Reply(_))), "{why}");
        }
        // Invalid UTF-8 inside a line is an error too, never a panic.
        let mut p = Parser::new();
        let r = p.feed(b"data: \xff\xfe\n", &mut |_| {}).and_then(|()| p.finish(&mut |_| {}).map(drop));
        assert!(matches!(r, Err(AiError::Reply(_))));
    }
}
