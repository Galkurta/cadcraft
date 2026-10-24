//! The Assistant's conversation: send, run the tool calls the model asks for, send the results,
//! until it answers in text.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

use crate::client::{Exchange, Options};
use crate::transport::Wake;
use crate::{AiError, Backend, Config, Image, Message, Reply, ToolCall, Vision, call_tool_with, tool_definitions};

/// What the model is told about CADCraft. Ours.
pub const SYSTEM_PROMPT: &str = "You are the CADCraft Assistant, built into a 2D/3D CAD drafting app. You change the open drawing \
only through the tools. Fastest path: `command_line` with command-line syntax, e.g. `circle 0,0 5`, `line 0,0 10,0 10,5 c`, \
`rectang 0,0 4,3`, `zoom e`; or `execute` with JSON parameters (use `list_commands` to find ids and parameter docs). \
Coordinates are drawing units, y up. Keep steps small, then verify with `inspect_drawing` or `query_entities`, and look at the \
result with `render`. Say briefly what you did, and say so when something failed.";

/// One thing that happened while the Assistant worked, for the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A piece of what the model wrote: the whole answer, or while it streams the next few words
    /// (consecutive pieces are one text).
    Text(String),
    /// The model called a tool (`args`: its JSON text).
    ToolCall { name: String, args: String },
    /// The tool's answer, shortened (`error`: the tool failed or the call was refused).
    ToolResult { name: String, summary: String, error: bool },
    /// Something worth a line that is not an answer or a failure (a retry without pictures).
    Note(String),
    /// The turn ended without an answer: the endpoint failed, the step limit was hit or the user stopped it.
    Error(String),
}

/// Tool answers sent to the model are cut to this many characters (a command list is huge).
const MAX_TOOL_CHARS: usize = 20_000;
/// The transcript keeps this many characters of a tool's arguments and answer.
const SUMMARY_CHARS: usize = 2000;

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

/// The conversation: what was said, the endpoint, and what the endpoint turned out not to take.
pub struct Agent {
    pub config: Config,
    /// The conversation so far, starting with the system prompt.
    pub messages: Vec<Message>,
    /// Requests to the model per user message.
    pub max_steps: usize,
    /// The endpoint refused pictures (with `Vision::Auto`): none are sent for the rest of the session.
    pub no_vision: bool,
    /// The endpoint refused `stream: true`: replies are asked for whole for the rest of the session.
    pub no_stream: bool,
}

impl Agent {
    pub fn new(config: Config) -> Self {
        Agent { config, messages: vec![Message::System(SYSTEM_PROMPT.into())], max_steps: 20, no_vision: false, no_stream: false }
    }

    /// Forget the conversation.
    pub fn clear(&mut self) {
        self.messages.truncate(1);
    }

    /// Are pictures sent right now?
    fn images_ok(&self) -> bool {
        match self.config.vision {
            Vision::On => true,
            Vision::Off => false,
            Vision::Auto => !self.no_vision,
        }
    }

    fn has_images(&self) -> bool {
        self.messages.iter().any(|m| matches!(m, Message::User { images, .. } if !images.is_empty()))
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

    /// Answer `user` on this thread: [`Driver`] polled until it is done. Every event goes to
    /// `emit` as it happens and to the returned transcript; `cancel` is checked between polls.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn send(&mut self, b: &mut dyn Backend, user: &str, cancel: &AtomicBool, emit: &mut dyn FnMut(&Event)) -> Vec<Event> {
        let stop = Arc::new(AtomicBool::new(false));
        let placeholder = Agent::new(self.config.clone());
        let mut driver = Driver::start(std::mem::replace(self, placeholder), user, b.has_ui(), stop.clone(), Arc::new(|| {}));
        let mut log = Vec::new();
        loop {
            stop.store(cancel.load(Ordering::Relaxed), Ordering::Relaxed);
            let running = driver.poll(b);
            for e in driver.take_events() {
                emit(&e);
                log.push(e);
            }
            if !running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        *self = driver.into_agent();
        log
    }
}

/// One user message being answered: a state machine the UI polls every frame (native and web
/// alike). Request in flight → [`Driver::poll`] collects the streamed reply → the tool calls run
/// (on the caller's thread, through the [`Backend`]) → the next request, until the model answers
/// in text, the step limit is hit, an error ends it or the user cancels.
pub struct Driver {
    agent: Agent,
    cancel: Arc<AtomicBool>,
    wake: Wake,
    tools: Value,
    steps: usize,
    exchange: Option<Exchange>,
    events: Vec<Event>,
    finished: bool,
}

impl Driver {
    /// Add `user` to the conversation and get going; the first request goes out on the first poll.
    /// `wake` is called (from any thread) when there is something new to poll for.
    pub fn start(mut agent: Agent, user: &str, has_ui: bool, cancel: Arc<AtomicBool>, wake: Wake) -> Driver {
        agent.messages.push(Message::user(user));
        Driver { agent, cancel, wake, tools: tool_definitions(has_ui), steps: 0, exchange: None, events: Vec::new(), finished: false }
    }

    /// The conversation, for the next turn.
    pub fn into_agent(self) -> Agent {
        self.agent
    }

    /// What happened since the last call.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Stop now, without an error event: the request in flight is cancelled and the turn is over.
    pub fn abort(&mut self) {
        if let Some(ex) = self.exchange.take() {
            ex.cancel();
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.finished = true;
    }

    fn fail(&mut self, message: String) {
        self.exchange = None;
        self.finished = true;
        self.events.push(Event::Error(message));
    }

    /// Do what can be done without waiting. True while the turn is still running.
    pub fn poll(&mut self, b: &mut dyn Backend) -> bool {
        while !self.finished {
            if self.cancel.load(Ordering::Relaxed) {
                if let Some(ex) = &self.exchange {
                    ex.cancel();
                }
                self.fail(AiError::Stopped.to_string());
                break;
            }
            let Some(ex) = self.exchange.as_mut() else {
                self.begin_request();
                continue;
            };
            let events = &mut self.events;
            let Some(result) = ex.poll(&mut |t| events.push(Event::Text(t.to_string()))) else { return true };
            let streamed = ex.streamed;
            self.exchange = None;
            match result {
                Ok(reply) => self.handle_reply(b, reply, streamed),
                Err(e) => {
                    if !self.retry_without(&e) {
                        self.fail(e.to_string());
                    }
                }
            }
        }
        !self.finished
    }

    fn begin_request(&mut self) {
        if self.steps >= self.agent.max_steps {
            let n = self.agent.max_steps;
            return self.fail(format!("stopped after {n} steps without a final answer"));
        }
        self.steps += 1;
        let opts = Options { images: self.agent.images_ok(), stream: !self.agent.no_stream };
        match Exchange::start(&self.agent.config, &self.agent.messages, &self.tools, opts, self.wake.clone()) {
            Ok(ex) => self.exchange = Some(ex),
            Err(e) => self.fail(e.to_string()),
        }
    }

    /// An endpoint that refuses pictures or streaming gets the same request without them, once
    /// per refusal (the flags make a second identical refusal an error).
    fn retry_without(&mut self, e: &AiError) -> bool {
        let AiError::Status { status: 400..=499, body } = e else { return false };
        let body = body.to_ascii_lowercase();
        let a = &mut self.agent;
        if a.config.vision == Vision::Auto
            && !a.no_vision
            && a.has_images()
            && ["image", "vision", "multimodal", "content type", "content-type"].iter().any(|w| body.contains(w))
        {
            a.no_vision = true;
            self.events.push(Event::Note("The model doesn't take images; retrying without them.".into()));
        } else if !a.no_stream && body.contains("stream") {
            a.no_stream = true;
            self.events.push(Event::Note("The endpoint doesn't stream; retrying without.".into()));
        } else {
            return false;
        }
        self.steps = self.steps.saturating_sub(1);
        true
    }

    fn handle_reply(&mut self, b: &mut dyn Backend, reply: Reply, streamed: bool) {
        if !streamed && !reply.text.trim().is_empty() {
            self.events.push(Event::Text(reply.text.clone()));
        }
        self.agent.messages.push(Message::Assistant { text: reply.text, tool_calls: reply.tool_calls.clone() });
        if reply.tool_calls.is_empty() {
            self.finished = true;
            return;
        }
        let mut pictures = Vec::new();
        for call in &reply.tool_calls {
            self.events.push(Event::ToolCall { name: call.name.clone(), args: shorten(&call.arguments, SUMMARY_CHARS) });
            let (text, images, error) = self.agent.run_call(b, call);
            let mut seen = shorten(&text, SUMMARY_CHARS);
            if !images.is_empty() {
                seen.push_str(" [image]");
            }
            self.events.push(Event::ToolResult { name: call.name.clone(), summary: seen, error });
            let told = if images.is_empty() { text } else { format!("{text}\n(the image follows in the next message)") };
            self.agent.messages.push(Message::Tool { call_id: call.id.clone(), text: shorten(&told, MAX_TOOL_CHARS) });
            pictures.extend(images);
        }
        if !pictures.is_empty() {
            // ponytail: only the newest pictures stay in the conversation; older ones would
            // be resent with every request.
            for m in &mut self.agent.messages {
                if let Message::User { images, .. } = m {
                    images.clear();
                }
            }
            self.agent.messages.push(Message::User { text: "Image(s) returned by the tool call(s) above.".into(), images: pictures });
        }
    }
}
