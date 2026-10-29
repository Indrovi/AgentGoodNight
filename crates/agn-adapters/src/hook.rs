//! What the `agentgoodnight-hook` binary does when an agent invokes it.
//!
//! Contract with the agents: read the JSON payload from stdin, store a
//! scrubbed copy in the spool, exit 0, and print nothing that could influence
//! the agent (Cursor requires `{}` on stdout, everyone else gets nothing).
//! Every failure is swallowed — a broken monitor must never break an agent.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use agn_core::model::AgentKind;
use agn_core::spool::{SpoolRecord, write_record};
use serde_json::Value;

use crate::cursor::looks_like_cursor;

const MAX_STDIN: u64 = 16 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct HookOutcome {
    /// Text to print on stdout (exactly this, nothing else).
    pub stdout: Option<&'static str>,
    pub stored: bool,
}

/// `ancestors`: the hook's parent process chain, nearest first, so the
/// monitor can attribute the record to the agent process that ran it.
pub fn run(agent_arg: Option<&str>, stdin: &mut dyn Read, data_dir: &Path, ancestors: Vec<u32>) -> HookOutcome {
    let mut raw = Vec::new();
    let read_ok = stdin.take(MAX_STDIN).read_to_end(&mut raw).is_ok();
    let text = String::from_utf8_lossy(&raw);
    // Cursor on Windows prefixes stdin with a UTF-8 BOM.
    let text = text.trim_start_matches('\u{feff}').trim();
    let payload: Option<Value> = if read_ok { serde_json::from_str(text).ok() } else { None };

    let mut agent = agent_arg.and_then(AgentKind::from_id);
    if let Some(p) = &payload {
        if looks_like_cursor(p) {
            agent = Some(AgentKind::Cursor);
        }
    }
    let stdout = (agent == Some(AgentKind::Cursor)).then_some("{}");

    let (Some(agent), Some(payload)) = (agent, payload) else {
        log_error(data_dir, &format!("unusable hook input (agent={agent_arg:?}, {} bytes)", raw.len()));
        return HookOutcome { stdout, stored: false };
    };
    let mut rec = SpoolRecord::new(agent.id(), payload);
    rec.ancestors = ancestors;
    let stored = match write_record(&data_dir.join("spool"), &rec) {
        Ok(_) => true,
        Err(e) => {
            log_error(data_dir, &format!("spool write failed: {e}"));
            false
        }
    };
    HookOutcome { stdout, stored }
}

fn log_error(data_dir: &Path, msg: &str) {
    let dir = data_dir.join("logs");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("hook-errors.log");
    if fs::metadata(&path).is_ok_and(|m| m.len() > 512 * 1024) {
        let _ = fs::rename(&path, dir.join("hook-errors.old.log"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {msg}", agn_core::time::now_millis());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agn_core::spool::read_batch;

    #[test]
    fn stores_scrubbed_record_silently() {
        let dir = tempfile::tempdir().unwrap();
        let mut input = r#"{"session_id":"s","hook_event_name":"Stop","last_assistant_message":"secret"}"#.as_bytes();
        let out = run(Some("claude_code"), &mut input, dir.path(), vec![42, 7]);
        assert_eq!(out, HookOutcome { stdout: None, stored: true });
        let batch = read_batch(&dir.path().join("spool")).unwrap();
        let rec = batch[0].record.as_ref().unwrap();
        assert_eq!(rec.agent, "claude_code");
        assert!(rec.payload.get("last_assistant_message").is_none());
        assert_eq!(rec.ancestors, vec![42, 7]);
    }

    #[test]
    fn cursor_bom_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let mut input = "\u{feff}{\"conversation_id\":\"c\",\"generation_id\":\"g\",\"hook_event_name\":\"stop\",\"status\":\"completed\"}".as_bytes();
        // Installed for Claude but invoked by Cursor (imported hooks).
        let out = run(Some("claude_code"), &mut input, dir.path(), vec![]);
        assert_eq!(out.stdout, Some("{}"));
        let batch = read_batch(&dir.path().join("spool")).unwrap();
        assert_eq!(batch[0].record.as_ref().unwrap().agent, "cursor");
    }

    #[test]
    fn garbage_is_logged_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let mut input = "not json".as_bytes();
        let out = run(Some("gemini_cli"), &mut input, dir.path(), vec![]);
        assert!(!out.stored);
        assert!(dir.path().join("logs/hook-errors.log").exists());
        let mut input = "{}".as_bytes();
        assert!(!run(None, &mut input, dir.path(), vec![]).stored, "unknown agent");
    }
}
