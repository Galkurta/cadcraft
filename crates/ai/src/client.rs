//! A blocking client for an OpenAI-compatible Chat Completions endpoint (Ollama, LM Studio,
//! OpenAI, OpenRouter, vLLM…).

use serde_json::{Value, json};

/// The endpoint, model and key. Empty fields mean "not set".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

/// Where a local Ollama listens.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

impl Config {
    /// `CADCRAFT_AI_BASE_URL` (default [`DEFAULT_BASE_URL`]), `CADCRAFT_AI_MODEL`, and
    /// `CADCRAFT_AI_KEY` falling back to `OPENAI_API_KEY`.
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Config {
            base_url: var("CADCRAFT_AI_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into()),
            model: var("CADCRAFT_AI_MODEL").unwrap_or_default(),
            api_key: var("CADCRAFT_AI_KEY").or_else(|| var("OPENAI_API_KEY")).unwrap_or_default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("the AI assistant is not available yet in the web build")]
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

    fn to_json(&self) -> Value {
        match self {
            Message::System(t) => json!({"role": "system", "content": t}),
            Message::User { text, images } if images.is_empty() => json!({"role": "user", "content": text}),
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
fn request_body(cfg: &Config, messages: &[Message], tools: &Value) -> Value {
    let mut body = json!({"model": cfg.model, "messages": messages.iter().map(Message::to_json).collect::<Vec<_>>()});
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

/// The reply body is read up to this many bytes.
#[cfg(not(target_arch = "wasm32"))]
const MAX_REPLY_BYTES: u64 = 4 << 20;
/// Local models can take minutes for a long answer.
#[cfg(not(target_arch = "wasm32"))]
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

#[cfg(not(target_arch = "wasm32"))]
fn post(cfg: &Config, body: &Value) -> Result<String, AiError> {
    let base = cfg.base_url.trim().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(AiError::Config(format!("the AI base URL `{base}` must start with http:// or https://")));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.post(format!("{base}/chat/completions")).header("Content-Type", "application/json");
    if !cfg.api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", cfg.api_key));
    }
    let mut resp = req.send(body.to_string()).map_err(|e| AiError::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().with_config().limit(MAX_REPLY_BYTES).read_to_string().map_err(|e| AiError::Transport(e.to_string()))?;
    if (200..300).contains(&status) { Ok(text) } else { Err(AiError::Status { status, body: text.chars().take(500).collect() }) }
}

#[cfg(target_arch = "wasm32")]
fn post(_: &Config, _: &Value) -> Result<String, AiError> {
    Err(AiError::Unavailable)
}

/// Send the conversation and wait for the model's answer. Blocks: call it from a worker thread.
pub fn chat(cfg: &Config, messages: &[Message], tools: &Value) -> Result<Reply, AiError> {
    if cfg.model.trim().is_empty() {
        return Err(AiError::Config("no AI model set: set CADCRAFT_AI_MODEL or fill in the Assistant settings".into()));
    }
    parse_reply(&post(cfg, &request_body(cfg, messages, tools))?)
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
        let cfg = Config { base_url: String::new(), model: "m".into(), api_key: String::new() };
        let msgs = [
            Message::System("s".into()),
            Message::User { text: "look".into(), images: vec![Image { mime: "image/png".into(), base64: "QUJD".into() }] },
            Message::Assistant { text: String::new(), tool_calls: vec![ToolCall { id: "1".into(), name: "render".into(), arguments: "{}".into() }] },
            Message::Tool { call_id: "1".into(), text: "ok".into() },
        ];
        let b = request_body(&cfg, &msgs, &crate::tool_definitions(false));
        assert_eq!(b["model"], "m");
        assert_eq!(b["messages"][1]["content"][1]["image_url"]["url"], "data:image/png;base64,QUJD");
        assert_eq!(b["messages"][2]["tool_calls"][0]["function"]["name"], "render");
        assert_eq!(b["messages"][3]["tool_call_id"], "1");
        let f = b["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "command_line").unwrap();
        assert_eq!(f["function"]["parameters"]["required"][0], "text");
        assert!(request_body(&cfg, &msgs, &Value::Null).get("tools").is_none());
    }

    #[test]
    fn a_missing_model_or_a_bad_url_is_a_config_error() {
        let mut cfg = Config { base_url: "http://localhost:1/v1".into(), model: String::new(), api_key: String::new() };
        assert!(matches!(chat(&cfg, &[], &Value::Null), Err(AiError::Config(_))));
        cfg.model = "m".into();
        cfg.base_url = "ftp://x".into();
        assert!(matches!(chat(&cfg, &[], &Value::Null), Err(AiError::Config(_))));
    }
}
