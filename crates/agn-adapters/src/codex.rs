//! Codex adapter (CLI, IDE extension and the Codex desktop app — all of them
//! run the same core and write rollouts to `~/.codex/sessions`).
//!
//! Rollouts persist explicit lifecycle events (`task_started`,
//! `task_complete` with an optional `error`, `turn_aborted`,
//! `thread_goal_updated`, `token_count.rate_limits`). Approval waits, stream
//! errors and reconnects are *not* persisted; such turns simply stay
//! `Running`, which blocks — the safe direction.
//!
//! Subagent threads have their own rollouts and are tracked as separate
//! sessions linked to their parent.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use agn_core::event::{AgentEvent, EventKind, FailureKind};
use agn_core::model::{AgentKind, EvidenceSource};
use agn_core::time::{Millis, parse_rfc3339_millis};
use serde_json::Value;

use crate::tail::{FileTail, decode_zstd, mtime_millis};
use crate::{SourceStatus, str_field};

const AGENT: AgentKind = AgentKind::Codex;
const INITIAL_WINDOW: u64 = 8 * 1024 * 1024;

/// `CODEX_HOME` or `~/.codex`.
pub fn default_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

#[derive(Debug)]
struct Rollout {
    tail: FileTail,
    parser: RolloutParser,
}

pub struct CodexAdapter {
    root: PathBuf,
    window: Millis,
    rollouts: BTreeMap<PathBuf, Rollout>,
}

impl CodexAdapter {
    pub fn new(root: PathBuf, window: Millis) -> Self {
        Self { root, window, rollouts: BTreeMap::new() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn poll(&mut self, now: Millis, full: bool) -> (Vec<AgentEvent>, SourceStatus) {
        if !self.root.exists() {
            return (Vec::new(), SourceStatus::Missing);
        }
        let sessions = self.root.join("sessions");
        let mut status = SourceStatus::Ok;
        if full {
            let mut found = Vec::new();
            if let Err(e) = walk_rollouts(&sessions, 3, &mut found) {
                if sessions.exists() {
                    status = SourceStatus::ReadError(e.to_string());
                }
            }
            for (path, mtime) in found {
                if now - mtime > self.window || self.rollouts.contains_key(&path) {
                    continue;
                }
                // A plain file superseded by its compressed twin (or vice
                // versa) must not be tracked twice.
                let twin = twin_path(&path);
                if self.rollouts.contains_key(&twin) {
                    continue;
                }
                let parser = RolloutParser::new(session_id_from_name(&path));
                self.rollouts.insert(path.clone(), Rollout { tail: FileTail::new(path), parser });
            }
        }
        let mut events = Vec::new();
        for (path, r) in self.rollouts.iter_mut() {
            let first = !r.parser.started;
            match r.tail.read_new(INITIAL_WINDOW) {
                Ok(read) => {
                    if first {
                        r.parser.started = true;
                        if read.from_middle {
                            if let Some(meta) = read_first_line(path) {
                                r.parser.parse(&meta, &mut events);
                            }
                        }
                    }
                    for line in &read.lines {
                        r.parser.parse(line, &mut events);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => status = SourceStatus::ReadError(e.to_string()),
            }
        }
        let horizon = now - self.window * 2;
        self.rollouts.retain(|p, r| p.exists() && (r.tail.mtime() == 0 || r.tail.mtime() >= horizon));
        (events, status)
    }
}

fn is_rollout(name: &str) -> bool {
    name.starts_with("rollout-") && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
}

fn twin_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_suffix(".zst") {
        Some(plain) => PathBuf::from(plain),
        None => PathBuf::from(format!("{s}.zst")),
    }
}

fn walk_rollouts(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, Millis)>) -> std::io::Result<()> {
    for e in fs::read_dir(dir)?.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            if depth > 0 {
                let _ = walk_rollouts(&e.path(), depth - 1, out);
            }
        } else if e.file_name().to_str().is_some_and(is_rollout) {
            out.push((e.path(), mtime_millis(&meta)));
        }
    }
    Ok(())
}

/// `rollout-2026-09-23T10-11-12-<uuid>.jsonl[.zst]` → `<uuid>`.
pub fn session_id_from_name(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let stem = name.trim_end_matches(".zst").trim_end_matches(".jsonl");
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 5 {
        let uuid = parts[parts.len() - 5..].join("-");
        if uuid.len() == 36 {
            return uuid;
        }
    }
    stem.to_string()
}

fn read_first_line(path: &Path) -> Option<String> {
    let mut buf = Vec::new();
    if path.extension().is_some_and(|e| e == "zst") {
        buf = decode_zstd(&fs::read(path).ok()?).ok()?;
    } else {
        fs::File::open(path).ok()?.take(4 * 1024 * 1024).read_to_end(&mut buf).ok()?;
    }
    let end = buf.iter().position(|b| *b == b'\n')?;
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

#[derive(Debug)]
pub struct RolloutParser {
    started: bool,
    session_id: String,
    cwd: Option<String>,
    origin: Option<String>,
    parent: Option<String>,
    /// Latest reported reset of an exhausted rate-limit window.
    limit_resets_at: Option<Millis>,
}

impl RolloutParser {
    pub fn new(session_id: String) -> Self {
        Self { started: false, session_id, cwd: None, origin: None, parent: None, limit_resets_at: None }
    }

    fn event(&self, at: Millis, kind: EventKind) -> AgentEvent {
        AgentEvent::new(AGENT, self.session_id.clone(), at, kind, EvidenceSource::Rollout)
            .with_cwd(self.cwd.clone())
            .with_origin(self.origin.clone())
            .with_parent(self.parent.clone())
    }

    pub fn parse(&mut self, line: &str, out: &mut Vec<AgentEvent>) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        let Some(at) = str_field(&v, "timestamp").and_then(parse_rfc3339_millis) else { return };
        let payload = v.get("payload").cloned().unwrap_or(Value::Null);
        match str_field(&v, "type").unwrap_or("") {
            "session_meta" => {
                if let Some(id) = str_field(&payload, "id") {
                    self.session_id = id.to_string();
                }
                if let Some(cwd) = str_field(&payload, "cwd") {
                    self.cwd = Some(cwd.to_string());
                }
                self.origin = Some(origin_of(&payload));
                self.parent = str_field(&payload, "parent_thread_id").map(str::to_string).or_else(|| {
                    payload
                        .pointer("/source/subagent/thread_spawn/parent_thread_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
                out.push(self.event(at, EventKind::Heartbeat));
            }
            "event_msg" => {
                if let Some(kind) = self.event_msg(&payload) {
                    out.push(self.event(at, kind));
                }
            }
            "response_item"
            | "turn_context"
            | "compacted"
            | "world_state"
            | "token_usage_record"
            | "inter_agent_communication_metadata" => out.push(self.event(at, EventKind::Heartbeat)),
            _ => {}
        }
    }

    fn event_msg(&mut self, p: &Value) -> Option<EventKind> {
        Some(match str_field(p, "type")? {
            "task_started" | "turn_started" => EventKind::TurnStarted,
            "task_complete" | "turn_complete" => match p.get("error").filter(|e| !e.is_null()) {
                None => EventKind::TurnCompleted { continuing: false, pending: None },
                Some(err) => {
                    let info = str_field(err, "codex_error_info").unwrap_or("");
                    let msg = str_field(err, "message").unwrap_or("").to_lowercase();
                    if info.contains("usage_limit") || info.contains("rate_limit") || msg.contains("usage limit") {
                        EventKind::TurnFailed {
                            kind: FailureKind::RateLimited,
                            reason: Some(if info.is_empty() { "usage_limit".into() } else { info.to_string() }),
                            resets_at: self.limit_resets_at,
                        }
                    } else {
                        EventKind::TurnFailed {
                            kind: FailureKind::Error,
                            reason: Some(if info.is_empty() { "error".into() } else { info.to_string() }),
                            resets_at: None,
                        }
                    }
                }
            },
            "turn_aborted" => match str_field(p, "reason").unwrap_or("") {
                "replaced" => EventKind::Activity { detail: Some("turn_replaced".into()) },
                "review_ended" => EventKind::TurnCompleted { continuing: false, pending: None },
                "budget_limited" => EventKind::TurnFailed {
                    kind: FailureKind::Error,
                    reason: Some("budget_limited".into()),
                    resets_at: None,
                },
                other => EventKind::TurnFailed {
                    kind: FailureKind::Interrupted,
                    reason: Some(if other.is_empty() { "interrupted".into() } else { other.to_string() }),
                    resets_at: None,
                },
            },
            "thread_goal_updated" => {
                let goal = p.get("goal").unwrap_or(p);
                match str_field(goal, "status").unwrap_or("") {
                    "active" | "usageLimited" | "usage_limited" => EventKind::GoalStatus { active: true },
                    "blocked" => EventKind::WaitingForInput { reason: Some("goal_blocked".into()) },
                    _ => EventKind::GoalStatus { active: false },
                }
            }
            "token_count" => {
                self.note_rate_limits(p);
                EventKind::Heartbeat
            }
            "stream_error" => EventKind::TurnFailed {
                kind: FailureKind::Reconnecting,
                reason: Some("stream_error".into()),
                resets_at: None,
            },
            "error" => {
                EventKind::TurnFailed { kind: FailureKind::Error, reason: Some("error".into()), resets_at: None }
            }
            "exec_approval_request" | "apply_patch_approval_request" | "request_user_input" | "elicitation_request" => {
                EventKind::WaitingForInput { reason: Some("approval".into()) }
            }
            _ => EventKind::Heartbeat,
        })
    }

    fn note_rate_limits(&mut self, p: &Value) {
        let Some(rl) = p.get("rate_limits").or_else(|| p.pointer("/info/rate_limits")) else { return };
        let mut resets = None;
        for window in ["primary", "secondary"] {
            let Some(w) = rl.get(window) else { continue };
            let used = w.get("used_percent").and_then(Value::as_f64).unwrap_or(0.0);
            if used >= 100.0 {
                if let Some(r) = w.get("resets_at").and_then(Value::as_i64) {
                    let ms = if r < 100_000_000_000 { r * 1000 } else { r };
                    resets = Some(resets.map_or(ms, |x: Millis| x.max(ms)));
                }
            }
        }
        if resets.is_some() || rl.get("rate_limit_reached_type").is_some_and(|v| !v.is_null()) {
            self.limit_resets_at = resets.or(self.limit_resets_at);
        } else {
            self.limit_resets_at = None;
        }
    }
}

fn origin_of(meta: &Value) -> String {
    let originator = str_field(meta, "originator").unwrap_or("").to_lowercase();
    if meta.pointer("/source/subagent").is_some() || str_field(meta, "thread_source") == Some("subagent") {
        return "subagent".into();
    }
    if originator.contains("desktop") {
        return "desktop".into();
    }
    match str_field(meta, "source") {
        Some("vscode") => "ide".into(),
        Some("exec") => "exec".into(),
        Some("cli") => "cli".into(),
        Some(other) => other.to_string(),
        None if originator.contains("cli") => "cli".into(),
        None => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agn_core::SessionState;
    use agn_core::tracker::{Tracker, TrackerConfig};
    use serde_json::json;

    const ID: &str = "019a1b2c-3d4e-7f00-8a9b-0c1d2e3f4a5b";

    fn line(sec: u32, ty: &str, payload: Value) -> String {
        json!({"timestamp": format!("2026-10-29T10:00:{sec:02}.000Z"), "type": ty, "payload": payload}).to_string()
    }

    fn state(lines: &[String]) -> (SessionState, Vec<AgentEvent>) {
        let mut p = RolloutParser::new(ID.into());
        let mut evs = Vec::new();
        for l in lines {
            p.parse(l, &mut evs);
        }
        let mut t = Tracker::new(TrackerConfig { settle: 1_000 });
        t.apply_all(evs.clone(), 4_000_000_000_000);
        t.tick(4_000_000_000_000);
        (t.get(&agn_core::SessionKey::new(AGENT, ID)).unwrap().state, evs)
    }

    fn meta() -> String {
        line(0, "session_meta", json!({"id": ID, "cwd": "/w/api", "originator": "Codex Desktop", "source": "vscode"}))
    }

    #[test]
    fn task_lifecycle() {
        let base = vec![meta(), line(1, "event_msg", json!({"type":"task_started","turn_id":"t1"}))];
        let (s, evs) = state(&base);
        assert_eq!(s, SessionState::Running);
        assert_eq!(evs[0].origin.as_deref(), Some("desktop"));
        assert_eq!(evs[0].cwd.as_deref(), Some("/w/api"));
        let mut done = base.clone();
        done.push(line(2, "response_item", json!({"type":"message"})));
        done.push(line(3, "event_msg", json!({"type":"task_complete","turn_id":"t1","error":null})));
        assert_eq!(state(&done).0, SessionState::Completed);
    }

    #[test]
    fn usage_limit_error_and_abort() {
        let limited = vec![
            meta(),
            line(1, "event_msg", json!({"type":"task_started"})),
            line(
                2,
                "event_msg",
                json!({"type":"token_count","rate_limits":{"primary":{"used_percent":100.0,"window_minutes":300,"resets_at":1793300000}}}),
            ),
            line(
                3,
                "event_msg",
                json!({"type":"task_complete","error":{"message":"You've hit your usage limit","codex_error_info":"usage_limit_exceeded"}}),
            ),
        ];
        let (s, evs) = state(&limited);
        assert_eq!(s, SessionState::RateLimited);
        assert!(evs.iter().any(|e| matches!(e.kind, EventKind::TurnFailed { resets_at: Some(1_793_300_000_000), .. })));

        let err = vec![
            meta(),
            line(1, "event_msg", json!({"type":"task_started"})),
            line(2, "event_msg", json!({"type":"task_complete","error":{"message":"boom"}})),
        ];
        assert_eq!(state(&err).0, SessionState::Error);

        let aborted = vec![
            meta(),
            line(1, "event_msg", json!({"type":"task_started"})),
            line(2, "event_msg", json!({"type":"turn_aborted","reason":"interrupted"})),
        ];
        assert_eq!(state(&aborted).0, SessionState::Interrupted);

        let replaced = vec![
            meta(),
            line(1, "event_msg", json!({"type":"task_started"})),
            line(2, "event_msg", json!({"type":"turn_aborted","reason":"replaced"})),
        ];
        assert_eq!(state(&replaced).0, SessionState::Running);
    }

    #[test]
    fn goals_keep_session_alive() {
        let lines = vec![
            meta(),
            line(1, "event_msg", json!({"type":"thread_goal_updated","goal":{"status":"active","objective":"x"}})),
            line(2, "event_msg", json!({"type":"task_started"})),
            line(3, "event_msg", json!({"type":"task_complete"})),
        ];
        assert_eq!(state(&lines).0, SessionState::AwaitingContinuation);
        let mut done = lines.clone();
        done.push(line(9, "event_msg", json!({"type":"thread_goal_updated","goal":{"status":"complete"}})));
        assert_eq!(state(&done).0, SessionState::Completed);
        let mut blocked = lines.clone();
        blocked.push(line(9, "event_msg", json!({"type":"thread_goal_updated","goal":{"status":"blocked"}})));
        assert_eq!(state(&blocked).0, SessionState::WaitingForInput);
    }

    #[test]
    fn subagent_meta_links_parent() {
        let mut p = RolloutParser::new("x".into());
        let mut evs = Vec::new();
        p.parse(
            &line(0, "session_meta", json!({"id":"child","cwd":"/w","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent","depth":1}}}})),
            &mut evs,
        );
        assert_eq!(evs[0].session_id, "child");
        assert_eq!(evs[0].parent.as_deref(), Some("parent"));
        assert_eq!(evs[0].origin.as_deref(), Some("subagent"));
    }

    #[test]
    fn session_id_from_file_name() {
        let p = PathBuf::from(format!("/x/rollout-2026-09-23T10-11-12-{ID}.jsonl.zst"));
        assert_eq!(session_id_from_name(&p), ID);
        assert_eq!(session_id_from_name(Path::new("/x/rollout-weird.jsonl")), "rollout-weird");
    }

    #[test]
    fn adapter_reads_plain_and_tracks_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("sessions/2026/10/29");
        fs::create_dir_all(&day).unwrap();
        let body = [meta(), line(1, "event_msg", json!({"type":"task_started"}))].join("\n") + "\n";
        fs::write(day.join(format!("rollout-2026-10-29T10-00-00-{ID}.jsonl")), body).unwrap();
        let mut a = CodexAdapter::new(dir.path().to_path_buf(), 48 * agn_core::time::HOUR);
        let (evs, st) = a.poll(agn_core::time::now_millis(), true);
        assert_eq!(st, SourceStatus::Ok);
        assert!(evs.iter().any(|e| e.kind == EventKind::TurnStarted && e.session_id == ID));
    }
}
