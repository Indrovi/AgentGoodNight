//! Cursor adapter — hooks only (`~/.cursor/hooks.json`, Cursor ≥ 1.7).
//!
//! The `stop` hook reports `status` = `completed` / `aborted` / `error`.
//! Another hook's `followup_message` can auto-continue the conversation;
//! the settle window absorbs that. The Cursor CLI does not emit `stop` in
//! `-p` mode, so `sessionEnd` with reason `completed` also counts as the end
//! of work. Cursor's editor process alone proves nothing, so it is only used
//! to detect that Cursor runs without the integration.

use agn_core::event::{AgentEvent, EventKind, FailureKind};
use agn_core::model::{AgentKind, EvidenceSource};
use agn_core::spool::SpoolRecord;
use serde_json::Value;

use crate::str_field;

const AGENT: AgentKind = AgentKind::Cursor;

pub fn default_root() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".cursor"))
}

/// Cursor payloads carry `conversation_id`/`cursor_version`; Cursor may also
/// run imported Claude Code hooks, which then deliver Cursor payloads.
pub fn looks_like_cursor(p: &Value) -> bool {
    p.get("cursor_version").is_some() || (p.get("conversation_id").is_some() && p.get("generation_id").is_some())
}

pub fn hook_events(rec: &SpoolRecord) -> Vec<AgentEvent> {
    let p = &rec.payload;
    let Some(session_id) = str_field(p, "conversation_id").or_else(|| str_field(p, "session_id")) else {
        return Vec::new();
    };
    let cwd = p
        .get("workspace_roots")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_string);
    let name = str_field(p, "hook_event_name").unwrap_or("");
    let status = str_field(p, "status").unwrap_or("");
    let mut kinds = Vec::new();
    match name {
        "sessionStart" => kinds.push(EventKind::SessionStarted { source: None }),
        "beforeSubmitPrompt" => kinds.push(EventKind::TurnStarted),
        "preToolUse"
        | "postToolUse"
        | "postToolUseFailure"
        | "beforeShellExecution"
        | "afterShellExecution"
        | "beforeMCPExecution"
        | "afterMCPExecution"
        | "beforeReadFile"
        | "afterFileEdit"
        | "subagentStart" => kinds.push(EventKind::Activity { detail: Some(name.to_string()) }),
        // Output-less observers that may arrive after `stop`: never revive a
        // finished turn.
        "afterAgentResponse" | "afterAgentThought" | "subagentStop" | "preCompact" | "beforeTabFileRead"
        | "afterTabFileEdit" => kinds.push(EventKind::Heartbeat),
        "stop" => kinds.push(match status {
            "completed" => EventKind::TurnCompleted {
                continuing: p.get("loop_count").and_then(Value::as_u64).unwrap_or(0) > 0,
                pending: None,
            },
            "aborted" => EventKind::TurnFailed {
                kind: FailureKind::Interrupted,
                reason: Some("aborted".into()),
                resets_at: None,
            },
            "error" => {
                EventKind::TurnFailed { kind: FailureKind::Error, reason: Some("error".into()), resets_at: None }
            }
            other => EventKind::Unrecognized { note: format!("cursor stop status {other:?}") },
        }),
        "sessionEnd" => {
            let reason = str_field(p, "reason").unwrap_or("");
            if reason == "completed" {
                kinds.push(EventKind::TurnCompleted { continuing: false, pending: None });
            }
            kinds.push(EventKind::SessionEnded { reason: Some(reason.to_string()).filter(|r| !r.is_empty()) });
        }
        "workspaceOpen" | "" => {}
        _ => kinds.push(EventKind::Heartbeat),
    }
    kinds
        .into_iter()
        .map(|k| AgentEvent::new(AGENT, session_id, rec.received_at, k, EvidenceSource::Hook).with_cwd(cwd.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn k(v: Value) -> Vec<EventKind> {
        hook_events(&SpoolRecord::new("cursor", v)).into_iter().map(|e| e.kind).collect()
    }

    #[test]
    fn maps_stop_statuses() {
        let base = |s: &str| json!({"conversation_id":"c","generation_id":"g","hook_event_name":"stop","status":s,"loop_count":0});
        assert_eq!(k(base("completed")), vec![EventKind::TurnCompleted { continuing: false, pending: None }]);
        assert!(matches!(k(base("aborted"))[0], EventKind::TurnFailed { kind: FailureKind::Interrupted, .. }));
        assert!(matches!(k(base("error"))[0], EventKind::TurnFailed { kind: FailureKind::Error, .. }));
        assert!(matches!(k(base("weird"))[0], EventKind::Unrecognized { .. }));
    }

    #[test]
    fn cli_session_end_completed() {
        let kinds = k(json!({"conversation_id":"c","hook_event_name":"sessionEnd","reason":"completed"}));
        assert_eq!(kinds.len(), 2);
        assert!(matches!(kinds[1], EventKind::SessionEnded { .. }));
    }

    #[test]
    fn detects_cursor_payloads() {
        assert!(looks_like_cursor(&json!({"cursor_version":"2.1"})));
        assert!(!looks_like_cursor(&json!({"session_id":"x"})));
    }

    #[test]
    fn workspace_root_becomes_cwd() {
        let evs = hook_events(&SpoolRecord::new(
            "cursor",
            json!({"conversation_id":"c","hook_event_name":"beforeSubmitPrompt","workspace_roots":["/w/site"]}),
        ));
        assert_eq!(evs[0].cwd.as_deref(), Some("/w/site"));
    }
}
