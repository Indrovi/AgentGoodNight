//! Gemini CLI adapter — hooks only (Gemini CLI ≥ 0.26, `hooksConfig.enabled`
//! defaults to true).
//!
//! `BeforeAgent`/`AfterAgent` bracket one agent turn; tool and model events
//! are activity; `Notification` with `ToolPermission` means the CLI waits for
//! approval. Without the hooks, Gemini sessions cannot be verified and a
//! running Gemini process blocks the power action.
//!
//! Known limitation: `AfterAgent` may also fire when the stream ended with
//! an error; such a turn is treated as finished.

use agn_core::event::{AgentEvent, EventKind};
use agn_core::model::{AgentKind, EvidenceSource};
use agn_core::spool::SpoolRecord;
use agn_core::time::parse_rfc3339_millis;
use serde_json::Value;

use crate::str_field;

const AGENT: AgentKind = AgentKind::GeminiCli;

/// `$GEMINI_CLI_HOME/.gemini` or `~/.gemini`, as Gemini CLI resolves it.
pub fn default_root() -> Option<std::path::PathBuf> {
    if let Some(h) = std::env::var_os("GEMINI_CLI_HOME").filter(|v| !v.is_empty()) {
        return Some(std::path::PathBuf::from(h).join(".gemini"));
    }
    dirs::home_dir().map(|h| h.join(".gemini"))
}

pub fn hook_events(rec: &SpoolRecord) -> Vec<AgentEvent> {
    let p = &rec.payload;
    let Some(session_id) = str_field(p, "session_id") else { return Vec::new() };
    let at = str_field(p, "timestamp").and_then(parse_rfc3339_millis).unwrap_or(rec.received_at);
    let kind = match str_field(p, "hook_event_name").unwrap_or("") {
        "SessionStart" => EventKind::SessionStarted { source: str_field(p, "source").map(str::to_string) },
        "SessionEnd" => EventKind::SessionEnded { reason: str_field(p, "reason").map(str::to_string) },
        "BeforeAgent" => EventKind::TurnStarted,
        "AfterAgent" => EventKind::TurnCompleted {
            continuing: p.get("stop_hook_active").and_then(Value::as_bool).unwrap_or(false),
            pending: None,
        },
        "BeforeModel" | "AfterModel" | "BeforeToolSelection" | "BeforeTool" | "AfterTool" => {
            EventKind::Activity { detail: str_field(p, "hook_event_name").map(str::to_string) }
        }
        "Notification" => match str_field(p, "notification_type").unwrap_or("") {
            "ToolPermission" => EventKind::WaitingForInput { reason: Some("tool_permission".into()) },
            _ => EventKind::Heartbeat,
        },
        "PreCompress" => EventKind::Heartbeat,
        "" => return Vec::new(),
        _ => EventKind::Heartbeat,
    };
    vec![
        AgentEvent::new(AGENT, session_id, at, kind, EvidenceSource::Hook)
            .with_cwd(str_field(p, "cwd").map(str::to_string)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn k(v: Value) -> Vec<EventKind> {
        hook_events(&SpoolRecord::new("gemini_cli", v)).into_iter().map(|e| e.kind).collect()
    }

    #[test]
    fn maps_lifecycle() {
        assert_eq!(k(json!({"session_id":"g","hook_event_name":"BeforeAgent"})), vec![EventKind::TurnStarted]);
        assert_eq!(
            k(json!({"session_id":"g","hook_event_name":"AfterAgent","stop_hook_active":true})),
            vec![EventKind::TurnCompleted { continuing: true, pending: None }]
        );
        assert!(matches!(
            k(json!({"session_id":"g","hook_event_name":"Notification","notification_type":"ToolPermission"}))[0],
            EventKind::WaitingForInput { .. }
        ));
        assert!(k(json!({"hook_event_name":"AfterAgent"})).is_empty());
    }

    #[test]
    fn uses_payload_timestamp() {
        let evs = hook_events(&SpoolRecord::new(
            "gemini_cli",
            json!({"session_id":"g","hook_event_name":"BeforeTool","timestamp":"2026-10-29T12:00:00.000Z"}),
        ));
        assert_eq!(evs[0].at, 1_793_275_200_000);
    }
}
