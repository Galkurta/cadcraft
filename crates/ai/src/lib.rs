//! CADCraft's AI features: the tool set agents drive the app with (shared by the MCP server and
//! the Assistant), a client for one OpenAI-compatible Chat Completions endpoint and the Assistant's
//! agent loop.
//!
//! Native builds run a request on a worker thread, the web build over the browser's `fetch`; in
//! both the Assistant polls a [`Driver`] every frame, so nothing here blocks the UI. The API key
//! lives in the OS credential store ([`keystore`]).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

mod agent;
mod client;
pub mod keystore;
mod tools;
mod transport;

pub use agent::{Agent, Driver, Event, SYSTEM_PROMPT};
pub use client::{AiError, Config, Image, Message, Reply, ToolCall, Vision, chat};
pub use tools::{call_tool, tool_definitions};
pub use transport::{Wake, insecure_key_host};

use serde_json::Value;

/// Something that answers control-channel methods (`engine.execute`, `cmdline.input`,
/// `drawing.inspect`, `ui.render`…). See `cadcraft_ui_egui::control` for the full list.
pub trait Backend {
    /// Invoke a control method and return its result or a descriptive error.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
    /// True when a real UI is attached (`ui.screenshot` works).
    fn has_ui(&self) -> bool;
    /// Describe the backend connection for client-facing status.
    fn describe(&self) -> String;
}

#[cfg(test)]
mod tests;
