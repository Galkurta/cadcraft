//! The Assistant's conversation: send, run the tool calls the model asks for, send the results,
//! until it answers in text.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

use crate::{AiError, Backend, Config, Image, Message, ToolCall, call_tool_with, chat, tool_definitions};

/// What the model is told about CADCraft. Ours.
pub const SYSTEM_PROMPT: &str = "You are the CADCraft Assistant, built into a 2D/3D CAD drafting app. You change the open drawing \
only through the tools. Fastest path: `command_line` with command-line syntax, e.g. `circle 0,0 5`, `line 0,0 10,0 10,5 c`, \
`rectang 0,0 4,3`, `zoom e`; or `execute` with JSON parameters (use `list_commands` to find ids and parameter docs). \
Coordinates are drawing units, y up. Keep steps small, then verify with `inspect_drawing` or `query_entities`, and look at the \
result with `render`. Say briefly what you did, and say so when something failed.";

/// One thing that happened while the Assistant worked, for the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The model wrote this.
    Text(String),
    /// The model called a tool (`args`: its JSON text).
    ToolCall { name: String, args: String },
    /// The tool's answer, shortened (`error`: the tool failed or the call was refused).
    ToolResult { name: String, summary: String, error: bool },
    /// The turn ended without an answer: the endpoint failed, the step limit was hit or the user stopped it.
    Error(String),
}

/// Tool answers sent to the model are cut to this many characters (a command list is huge).
const MAX_TOOL_CHARS: usize = 20_000;
/// The transcript shows this many characters of a tool's answer.
const SUMMARY_CHARS: usize = 300;

fn shorten(s: &str, n: usize) -> String {
    if s.chars().nth(n).is_none() { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// A tool's reply as the model sees it: its text, its images, and whether it failed.
fn flatten(r: &Value) -> (String, Vec<Image>, bool) {
    let items = r["content"].as_array().map_or(&[][..], Vec::as_slice);
    let text = items.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n");
    let images = items
        .iter()
        .filter(|c| c["type"] == "image")
        .filter_map(|c| Some(Image { mime: c["mimeType"].as_str()?.to_string(), base64: c["data"].as_str()?.to_string() }))
        .collect();
    (text, images, r["isError"] == true)
}

pub struct Agent {
    pub config: Config,
    /// The conversation so far, starting with the system prompt.
    pub messages: Vec<Message>,
    /// Requests to the model per user message.
    pub max_steps: usize,
}

impl Agent {
    pub fn new(config: Config) -> Self {
        Agent { config, messages: vec![Message::System(SYSTEM_PROMPT.into())], max_steps: 20 }
    }

    /// Forget the conversation.
    pub fn clear(&mut self) {
        self.messages.truncate(1);
    }

    fn run_call(&self, b: &mut dyn Backend, call: &ToolCall) -> (String, Vec<Image>, bool) {
        let args = match call.arguments.trim() {
            "" => Ok(Value::Null),
            a => serde_json::from_str::<Value>(a).map_err(|e| format!("the arguments are not valid JSON: {e}")),
        };
        match args.and_then(|a| call_tool_with(b, &self.config, &call.name, &a)) {
            Ok(r) => flatten(&r),
            Err(e) => (e, Vec::new(), true),
        }
    }

    /// Answer `user`: loop model → tools → model until the model answers in text, `cancel` is set
    /// (checked before each request, so the one running finishes) or `max_steps` requests were made.
    /// Every event goes to `emit` as it happens and to the returned transcript.
    pub fn send(&mut self, b: &mut dyn Backend, user: &str, cancel: &AtomicBool, emit: &mut dyn FnMut(&Event)) -> Vec<Event> {
        let mut log = Vec::new();
        let mut note = |e: Event| {
            emit(&e);
            log.push(e);
        };
        self.messages.push(Message::user(user));
        let tools = tool_definitions(b.has_ui());
        for _ in 0..self.max_steps {
            if cancel.load(Ordering::Relaxed) {
                note(Event::Error(AiError::Stopped.to_string()));
                return log;
            }
            let reply = match chat(&self.config, &self.messages, &tools) {
                Ok(r) => r,
                Err(e) => {
                    note(Event::Error(e.to_string()));
                    return log;
                }
            };
            if !reply.text.trim().is_empty() {
                note(Event::Text(reply.text.clone()));
            }
            self.messages.push(Message::Assistant { text: reply.text, tool_calls: reply.tool_calls.clone() });
            if reply.tool_calls.is_empty() {
                return log;
            }
            let mut pictures = Vec::new();
            for call in &reply.tool_calls {
                note(Event::ToolCall { name: call.name.clone(), args: shorten(&call.arguments, SUMMARY_CHARS) });
                let (text, images, error) = self.run_call(b, call);
                let mut seen = shorten(&text, SUMMARY_CHARS);
                if !images.is_empty() {
                    seen.push_str(" [image]");
                }
                note(Event::ToolResult { name: call.name.clone(), summary: seen, error });
                let told = if images.is_empty() { text } else { format!("{text}\n(the image follows in the next message)") };
                self.messages.push(Message::Tool { call_id: call.id.clone(), text: shorten(&told, MAX_TOOL_CHARS) });
                pictures.extend(images);
            }
            if !pictures.is_empty() {
                // ponytail: only the newest pictures stay in the conversation; older ones would
                // be resent with every request.
                for m in &mut self.messages {
                    if let Message::User { images, .. } = m {
                        images.clear();
                    }
                }
                self.messages.push(Message::User { text: "Image(s) returned by the tool call(s) above.".into(), images: pictures });
            }
        }
        note(Event::Error(format!("stopped after {} steps without a final answer", self.max_steps)));
        log
    }
}
