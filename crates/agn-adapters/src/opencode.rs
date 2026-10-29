//! OpenCode adapter (experimental) — a small plugin for the OpenCode 1.x
//! plugin API forwards bus events into the AgentGoodNight spool.
//!
//! OpenCode 2.x uses an incompatible plugin API and is not supported yet;
//! a running OpenCode process without a working plugin blocks the action.
//! Child sessions (task tool, possibly `background: true`) are tracked as
//! separate sessions, so a busy child blocks even while its parent is idle.

use agn_core::event::{AgentEvent, EventKind, FailureKind};
use agn_core::model::{AgentKind, EvidenceSource};
use agn_core::spool::SpoolRecord;
use serde_json::Value;

use crate::str_field;

const AGENT: AgentKind = AgentKind::OpenCode;

/// `$XDG_CONFIG_HOME/opencode` or `~/.config/opencode` (also on Windows).
pub fn config_root() -> Option<std::path::PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(std::path::PathBuf::from(x).join("opencode"));
    }
    dirs::home_dir().map(|h| h.join(".config").join("opencode"))
}

pub const PLUGIN_FILE: &str = "agentgoodnight.js";

/// Source of the plugin. `__SPOOL_DIR__` is replaced at install time.
pub const PLUGIN_SOURCE: &str = r#"// AgentGoodNight lifecycle forwarder for OpenCode 1.x.
// Installed by AgentGoodNight; delete this file to remove the integration.
// It only writes small JSON files into the AgentGoodNight spool directory and
// never changes OpenCode's behaviour.
import { mkdirSync, writeFileSync, renameSync } from "node:fs";
import { join } from "node:path";

const SPOOL = __SPOOL_DIR__;
const WANTED = new Set([
  "session.status", "session.idle", "session.error", "session.created", "session.deleted",
  "permission.asked", "permission.replied", "question.asked", "question.replied", "question.rejected",
  "session.compacted",
]);
let seq = 0;

function write(payload) {
  try {
    mkdirSync(SPOOL, { recursive: true });
    const now = Date.now();
    const name = `${String(now).padStart(13, "0")}-${process.pid}-oc${seq++}.json`;
    const tmp = join(SPOOL, `.${name}.tmp`);
    writeFileSync(tmp, JSON.stringify({ v: 1, agent: "opencode", received_at: now, hook_pid: process.pid, payload }) + "\n");
    renameSync(tmp, join(SPOOL, name));
  } catch (_) { /* never disturb OpenCode */ }
}

export const AgentGoodNight = async ({ directory }) => {
  // Proves to AgentGoodNight that the plugin runs in this OpenCode process.
  write({ event: "plugin.loaded", sessionID: null, directory: directory || null });
  return {
  event: async ({ event }) => {
    if (!event || !WANTED.has(event.type)) return;
    const p = event.properties || {};
    const info = p.info || {};
    write({
      event: event.type,
      sessionID: p.sessionID || info.id || null,
      parentID: info.parentID || null,
      directory: info.directory || directory || null,
      status: p.status ? { type: p.status.type, attempt: p.status.attempt, message: p.status.message, next: p.status.next } : null,
      error: p.error ? { name: p.error.name } : null,
    });
  },
  };
};
"#;

pub fn plugin_source(spool_dir: &std::path::Path) -> String {
    let literal = serde_json::to_string(&spool_dir.to_string_lossy()).unwrap_or_else(|_| "\"\"".into());
    PLUGIN_SOURCE.replace("__SPOOL_DIR__", &literal)
}

pub fn hook_events(rec: &SpoolRecord) -> Vec<AgentEvent> {
    let p = &rec.payload;
    let Some(session_id) = str_field(p, "sessionID") else { return Vec::new() };
    let status = p.get("status").cloned().unwrap_or(Value::Null);
    let kind = match str_field(p, "event").unwrap_or("") {
        "session.status" => match str_field(&status, "type").unwrap_or("") {
            "busy" => EventKind::TurnStarted,
            "idle" => EventKind::TurnCompleted { continuing: false, pending: None },
            "retry" => {
                let msg = str_field(&status, "message").unwrap_or("").to_lowercase();
                let limited =
                    msg.contains("rate") || msg.contains("429") || msg.contains("limit") || msg.contains("quota");
                EventKind::TurnFailed {
                    kind: if limited { FailureKind::RateLimited } else { FailureKind::Reconnecting },
                    reason: Some("retry".into()),
                    resets_at: status.get("next").and_then(Value::as_i64),
                }
            }
            other => EventKind::Unrecognized { note: format!("opencode status {other:?}") },
        },
        "session.idle" => EventKind::TurnCompleted { continuing: false, pending: None },
        "session.error" => {
            let name = p.get("error").and_then(|e| str_field(e, "name")).unwrap_or("");
            if name == "MessageAbortedError" {
                EventKind::TurnFailed {
                    kind: FailureKind::Interrupted,
                    reason: Some("aborted".into()),
                    resets_at: None,
                }
            } else {
                EventKind::TurnFailed { kind: FailureKind::Error, reason: Some(name.to_string()), resets_at: None }
            }
        }
        "session.created" => EventKind::SessionStarted { source: Some("startup".into()) },
        "session.deleted" => EventKind::SessionEnded { reason: Some("deleted".into()) },
        "permission.asked" | "question.asked" => {
            EventKind::WaitingForInput { reason: str_field(p, "event").map(str::to_string) }
        }
        "permission.replied" | "question.replied" | "question.rejected" => EventKind::Activity { detail: None },
        _ => EventKind::Heartbeat,
    };
    vec![
        AgentEvent::new(AGENT, session_id, rec.received_at, kind, EvidenceSource::Plugin)
            .with_cwd(str_field(p, "directory").map(str::to_string))
            .with_parent(str_field(p, "parentID").map(str::to_string)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn k(v: Value) -> EventKind {
        hook_events(&SpoolRecord::new("opencode", v)).remove(0).kind
    }

    #[test]
    fn maps_status_and_errors() {
        assert_eq!(
            k(json!({"event":"session.status","sessionID":"o","status":{"type":"busy"}})),
            EventKind::TurnStarted
        );
        assert!(matches!(
            k(json!({"event":"session.status","sessionID":"o","status":{"type":"idle"}})),
            EventKind::TurnCompleted { .. }
        ));
        assert!(matches!(
            k(
                json!({"event":"session.status","sessionID":"o","status":{"type":"retry","message":"429 rate limited","next":5}})
            ),
            EventKind::TurnFailed { kind: FailureKind::RateLimited, resets_at: Some(5), .. }
        ));
        assert!(matches!(
            k(json!({"event":"session.error","sessionID":"o","error":{"name":"MessageAbortedError"}})),
            EventKind::TurnFailed { kind: FailureKind::Interrupted, .. }
        ));
        assert!(matches!(k(json!({"event":"permission.asked","sessionID":"o"})), EventKind::WaitingForInput { .. }));
    }

    #[test]
    fn plugin_source_embeds_spool_path() {
        let src = plugin_source(std::path::Path::new(r"C:\Users\x\AppData\Local\AgentGoodNight\spool"));
        assert!(src.contains(r#"const SPOOL = "C:\\Users\\x\\AppData\\Local\\AgentGoodNight\\spool";"#));
        assert!(!src.contains("__SPOOL_DIR__"));
    }
}
