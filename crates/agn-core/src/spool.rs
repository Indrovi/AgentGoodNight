//! The hook spool: a directory of small JSON files, one per hook invocation.
//!
//! Hooks run as short-lived processes spawned by the agents. Each writes one
//! record to a temporary name and renames it into place, so the reader never
//! sees partial files and concurrent hooks never contend for a lock. Records
//! survive while AgentGoodNight is not running, so monitoring can start
//! before or after the agents.
//!
//! Payloads are scrubbed with an allow-list before they touch the disk:
//! prompts, assistant messages and tool outputs are never stored.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::time::{Millis, now_millis};

pub const SPOOL_VERSION: u32 = 1;
const MAX_STRING: usize = 512;
const MAX_ARRAY: usize = 512;
const MAX_BATCH: usize = 5_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpoolRecord {
    pub v: u32,
    /// Adapter id the hook was installed for (`claude_code`, `cursor`, ...).
    pub agent: String,
    pub received_at: Millis,
    #[serde(default)]
    pub hook_pid: Option<u32>,
    /// Parent process chain of the hook (nearest first), so the monitor can
    /// tell which agent process ran it.
    #[serde(default)]
    pub ancestors: Vec<u32>,
    pub payload: Value,
}

impl SpoolRecord {
    pub fn new(agent: &str, payload: Value) -> Self {
        Self {
            v: SPOOL_VERSION,
            agent: agent.to_string(),
            received_at: now_millis(),
            hook_pid: Some(std::process::id()),
            ancestors: Vec::new(),
            payload: scrub(payload),
        }
    }
}

#[derive(Debug)]
pub struct SpoolEntry {
    pub path: PathBuf,
    pub record: Result<SpoolRecord, String>,
}

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Atomically writes one record into `dir`.
pub fn write_record(dir: &Path, record: &SpoolRecord) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!("{:013}-{}-{:010}.json", record.received_at, record.hook_pid.unwrap_or(0), seq);
    let tmp = dir.join(format!(".{name}.tmp"));
    let dst = dir.join(&name);
    {
        let mut f = fs::File::create(&tmp)?;
        serde_json::to_writer(&mut f, record)?;
        f.write_all(b"\n")?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, &dst)?;
    Ok(dst)
}

/// Reads up to a batch of complete records, oldest first. Does not delete.
pub fn read_batch(dir: &Path) -> io::Result<Vec<SpoolEntry>> {
    read_batch_filtered(dir, |_| true)
}

/// Like [`read_batch`], skipping file names for which `keep` is false
/// (records already applied but not yet deleted).
pub fn read_batch_filtered(dir: &Path, keep: impl Fn(&str) -> bool) -> io::Result<Vec<SpoolEntry>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut paths: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('.') && keep(n))
        })
        .collect();
    paths.sort();
    paths.truncate(MAX_BATCH);
    Ok(paths
        .into_iter()
        .map(|path| {
            let record = fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| serde_json::from_slice::<SpoolRecord>(&bytes).map_err(|e| e.to_string()));
            SpoolEntry { path, record }
        })
        .collect())
}

/// Removes stale temp files left by hooks that crashed mid-write.
pub fn sweep_temp(dir: &Path, older_than: Millis) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_millis(older_than.max(0) as u64);
    for e in rd.flatten() {
        let p = e.path();
        let is_tmp = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.') && n.ends_with(".tmp"));
        if is_tmp && e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < cutoff) {
            let _ = fs::remove_file(p);
        }
    }
}

const ALLOWED_TOP: &[&str] = &[
    "session_id",
    "conversation_id",
    "generation_id",
    "sessionID",
    "transcript_path",
    "cwd",
    "directory",
    "workspace_roots",
    "hook_event_name",
    "event",
    "type",
    "permission_mode",
    "stop_hook_active",
    "notification_type",
    "title",
    "message",
    "reason",
    "source",
    "trigger",
    "agent_id",
    "agent_type",
    "parent_id",
    "parentID",
    "tool_name",
    "tool_use_id",
    "tool_input",
    "tool_response",
    "error",
    "error_details",
    "error_message",
    "is_interrupt",
    "status",
    "final_status",
    "loop_count",
    "duration_ms",
    "is_background_agent",
    "cursor_version",
    "timestamp",
    "background_tasks",
    "session_crons",
    "attempt",
    "next",
    "retry",
];

const ALLOWED_NESTED: &[&str] = &[
    "truncated",
    "id",
    "type",
    "status",
    "name",
    "recurring",
    "run_in_background",
    "background",
    "delaySeconds",
    "stop",
    "subagent_type",
    "agent_type",
    "backgroundTaskId",
    "bash_id",
    "shellId",
    "shell_id",
    "agentId",
    "task_id",
    "taskId",
    "message",
    "attempt",
    "next",
];

/// Keeps only lifecycle-relevant fields; truncates strings and arrays.
pub fn scrub(v: Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(scrub_object(map, ALLOWED_TOP, 0)),
        other => scrub_value(other, 0),
    }
}

fn scrub_object(map: Map<String, Value>, allowed: &[&str], depth: usize) -> Map<String, Value> {
    map.into_iter()
        .filter(|(k, _)| allowed.contains(&k.as_str()))
        .map(|(k, v)| (k, scrub_value(v, depth + 1)))
        .collect()
}

fn scrub_value(v: Value, depth: usize) -> Value {
    match v {
        Value::String(s) => Value::String(truncate(s)),
        Value::Array(items) if depth < 4 => {
            let truncated = items.len() > MAX_ARRAY;
            let mut out: Vec<Value> = items.into_iter().take(MAX_ARRAY).map(|i| scrub_value(i, depth + 1)).collect();
            if truncated {
                // Readers must not treat a cut list as complete.
                out.push(serde_json::json!({ "truncated": true }));
            }
            Value::Array(out)
        }
        Value::Object(map) if depth < 4 => Value::Object(scrub_object(map, ALLOWED_NESTED, depth)),
        Value::Array(_) | Value::Object(_) => Value::Null,
        other => other,
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_STRING {
        let mut cut = MAX_STRING;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let a = SpoolRecord::new("claude_code", json!({"session_id": "s1", "hook_event_name": "Stop"}));
        let b = SpoolRecord::new("codex", json!({"session_id": "s2"}));
        write_record(dir.path(), &a).unwrap();
        write_record(dir.path(), &b).unwrap();
        fs::write(dir.path().join(".partial.json.tmp"), b"{").unwrap();
        fs::write(dir.path().join("broken.json"), b"{nope").unwrap();
        let batch = read_batch(dir.path()).unwrap();
        assert_eq!(batch.len(), 3, "temp files are skipped");
        let ok: Vec<_> = batch.iter().filter_map(|e| e.record.as_ref().ok()).collect();
        assert_eq!(ok.len(), 2);
        assert!(batch.iter().any(|e| e.record.is_err()));
    }

    #[test]
    fn missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_batch(&dir.path().join("nope")).unwrap().is_empty());
    }

    #[test]
    fn scrub_drops_content_and_keeps_lifecycle() {
        let raw = json!({
            "session_id": "abc",
            "hook_event_name": "Stop",
            "prompt": "secret prompt",
            "last_assistant_message": "secret answer",
            "tool_input": {"command": "rm -rf /", "run_in_background": true},
            "tool_response": {"stdout": "lots", "backgroundTaskId": "b1"},
            "background_tasks": [{"id": "t1", "type": "shell", "status": "running", "command": "npm test"}],
            "message": "x".repeat(2000),
        });
        let s = scrub(raw);
        assert_eq!(s["session_id"], "abc");
        assert!(s.get("prompt").is_none());
        assert!(s.get("last_assistant_message").is_none());
        assert_eq!(s["tool_input"], json!({"run_in_background": true}));
        assert_eq!(s["tool_response"], json!({"backgroundTaskId": "b1"}));
        assert_eq!(s["background_tasks"][0], json!({"id": "t1", "type": "shell", "status": "running"}));
        assert_eq!(s["message"].as_str().unwrap().len(), MAX_STRING);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "č".repeat(400);
        let t = truncate(s);
        assert!(t.len() <= MAX_STRING);
        assert!(t.chars().all(|c| c == 'č'));
    }
}
