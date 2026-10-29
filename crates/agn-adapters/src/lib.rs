//! Tool adapters and the monitor loop.
//!
//! Each AI tool has its own module that turns that tool's real lifecycle
//! signals into [`agn_core::AgentEvent`]s:
//!
//! | tool        | primary evidence                         | install needed |
//! |-------------|------------------------------------------|----------------|
//! | Claude Code | hooks + transcripts + process registry   | hooks optional |
//! | Codex       | rollout files (`task_started/complete`)  | no             |
//! | Gemini CLI  | hooks                                    | yes            |
//! | Cursor      | hooks                                    | yes            |
//! | OpenCode    | plugin (1.x API, experimental)           | yes            |
//!
//! [`monitor::Monitor`] combines them with process scanning, the tracker,
//! the safety policy and the countdown controller.

pub mod claude;
pub mod codex;
pub mod cursor;
pub mod gemini;
pub mod hook;
pub mod integration;
pub mod monitor;
pub mod opencode;
pub mod process;
pub mod tail;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// State of a passive log source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "detail", rename_all = "snake_case")]
pub enum SourceStatus {
    Ok,
    /// The tool's data directory does not exist (not installed / never used).
    Missing,
    ReadError(String),
}

pub(crate) fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}
