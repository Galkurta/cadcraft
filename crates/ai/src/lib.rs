//! CADCraft's AI features: the tool set agents drive the app with (shared by the MCP server and
//! the Assistant), a client for one OpenAI-compatible Chat Completions endpoint, the Assistant's
//! agent loop and the recognition of image markups by a vision model.
//!
//! Native builds only talk to the network; in the web build [`chat`] reports "not available yet".
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

mod agent;
mod client;
mod markup;
mod tools;

pub use agent::{Agent, Event, SYSTEM_PROMPT};
pub use client::{AiError, Config, Image, Message, Reply, ToolCall, chat};
pub use markup::recognize_image_markups;
pub use tools::{call_tool, call_tool_with, tool_definitions};

use serde_json::Value;

/// Something that answers control-channel methods (`engine.execute`, `cmdline.input`,
/// `drawing.inspect`, `ui.render`…). See `cadcraft_ui_egui::control` for the full list.
pub trait Backend {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
    /// True when a real UI is attached (`ui.screenshot` works).
    fn has_ui(&self) -> bool;
    fn describe(&self) -> String;
}

#[cfg(test)]
mod tests;
