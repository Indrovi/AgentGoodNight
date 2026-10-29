//! Claude Code adapter (CLI, IDE extensions and the Claude Desktop "Code" tab,
//! which runs its own copy of Claude Code with the same `~/.claude` files).
//!
//! Evidence, strongest first:
//! * lifecycle hooks (`Stop` with `background_tasks` / `session_crons`,
//!   `StopFailure`, `SessionStart`/`End`, ...) — opt-in, installed by the user;
//! * session transcripts `~/.claude/projects/<slug>/<session>.jsonl`
//!   (end-of-turn `stop_hook_summary`, `goal_status`, interrupts, API errors,
//!   task notifications) plus writes of subagents/workflows below
//!   `<slug>/<session>/`;
//! * the per-process registry `~/.claude/sessions/<pid>.json` (pid → session),
//!   used only to attribute processes, never to decide completion.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use agn_core::event::{AgentEvent, EventKind, FailureKind, PendingWork};
use agn_core::model::{AgentKind, EvidenceSource};
use agn_core::spool::SpoolRecord;
use agn_core::time::{HOUR, Millis, parse_rfc3339_millis};
use serde_json::Value;

use crate::tail::{FileTail, list_files, mtime_millis, newest_mtime};
use crate::{SourceStatus, str_field};

const AGENT: AgentKind = AgentKind::ClaudeCode;
const INITIAL_WINDOW: u64 = 4 * 1024 * 1024;
const GOAL_SCAN_WINDOW: u64 = 32 * 1024 * 1024;

/// `CLAUDE_CONFIG_DIR` or `~/.claude`.
pub fn default_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|h| h.join(".claude"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryEntry {
    pub pid: u32,
    pub session_id: String,
    pub started_at: Option<Millis>,
    pub entrypoint: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug)]
struct Transcript {
    tail: FileTail,
    session_id: String,
    /// `<slug>/<session-id>/subagents` — background agents and workflows.
    sub_dir: PathBuf,
    sub_mtime: Option<Millis>,
    parser: LineParser,
}

pub struct ClaudeAdapter {
    root: PathBuf,
    window: Millis,
    transcripts: BTreeMap<PathBuf, Transcript>,
    registry: Vec<RegistryEntry>,
}

impl ClaudeAdapter {
    pub fn new(root: PathBuf, window: Millis) -> Self {
        Self { root, window, transcripts: BTreeMap::new(), registry: Vec::new() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn registry(&self) -> &[RegistryEntry] {
        &self.registry
    }

    /// Polls transcripts. `full` also rediscovers files, rescans subagent
    /// trees and re-reads the process registry.
    pub fn poll(&mut self, now: Millis, full: bool) -> (Vec<AgentEvent>, SourceStatus) {
        let projects = self.root.join("projects");
        if !self.root.exists() {
            return (Vec::new(), SourceStatus::Missing);
        }
        let mut status = SourceStatus::Ok;
        if full {
            if let Err(e) = self.discover(&projects, now) {
                if projects.exists() {
                    status = SourceStatus::ReadError(e.to_string());
                }
            }
            self.registry = read_registry(&self.root.join("sessions"));
        }
        let mut events = Vec::new();
        for (path, t) in self.transcripts.iter_mut() {
            let first = !t.parser.started;
            match t.tail.read_new(INITIAL_WINDOW) {
                Ok(read) => {
                    if first {
                        t.parser.started = true;
                        if read.from_middle {
                            if let Some(ev) = scan_last_goal(path, &t.session_id) {
                                events.push(ev);
                            }
                        }
                    }
                    for line in &read.lines {
                        t.parser.parse(&t.session_id, line, &mut events);
                    }
                    if read.grew && read.lines.is_empty() {
                        events.push(t.parser.event(&t.session_id, read.mtime, EventKind::Heartbeat));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => status = SourceStatus::ReadError(e.to_string()),
            }
            if full {
                let mut budget = 5_000;
                if let Some(m) = newest_mtime(&t.sub_dir, 4, &mut budget) {
                    if t.sub_mtime.is_none_or(|old| m > old) {
                        let first_seen = t.sub_mtime.is_none();
                        t.sub_mtime = Some(m);
                        // Background agents/workflows wrote after we last
                        // looked: the parent session is not done.
                        if !first_seen || now - m < 10 * 60 * 1000 {
                            events.push(t.parser.event(
                                &t.session_id,
                                m,
                                EventKind::Activity { detail: Some("subagent_activity".into()) },
                            ));
                        }
                    }
                }
            }
        }
        // Forget transcripts that have been quiet for long.
        let horizon = now - self.window * 2;
        self.transcripts.retain(|_, t| t.tail.mtime() == 0 || t.tail.mtime() >= horizon);
        (events, status)
    }

    fn discover(&mut self, projects: &Path, now: Millis) -> std::io::Result<()> {
        for dir in fs::read_dir(projects)?.flatten() {
            if !dir.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let files = match list_files(&dir.path(), |n| n.ends_with(".jsonl")) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for (path, meta) in files {
                if self.transcripts.contains_key(&path) || now - mtime_millis(&meta) > self.window {
                    continue;
                }
                let Some(session_id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                    continue;
                };
                let sub_dir = dir.path().join(&session_id).join("subagents");
                self.transcripts.insert(
                    path.clone(),
                    Transcript {
                        tail: FileTail::new(path),
                        session_id,
                        sub_dir,
                        sub_mtime: None,
                        parser: LineParser::default(),
                    },
                );
            }
        }
        Ok(())
    }
}

/// Finds the last `goal_status` attachment in the final part of a large
/// transcript whose initial window started mid-file.
fn scan_last_goal(path: &Path, session_id: &str) -> Option<AgentEvent> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(GOAL_SCAN_WINDOW);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(GOAL_SCAN_WINDOW).read_to_end(&mut buf).ok()?;
    let needle = b"\"goal_status\"";
    let pos = buf.windows(needle.len()).rposition(|w| w == needle)?;
    let line_start = buf[..pos].iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let line_end = buf[pos..].iter().position(|b| *b == b'\n').map_or(buf.len(), |p| pos + p);
    let v: Value = serde_json::from_slice(&buf[line_start..line_end]).ok()?;
    let att = v.get("attachment")?;
    if str_field(att, "type") != Some("goal_status") {
        return None;
    }
    let at = v.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_millis)?;
    let met = att.get("met").and_then(Value::as_bool).unwrap_or(false);
    Some(AgentEvent::new(AGENT, session_id, at, EventKind::GoalStatus { active: !met }, EvidenceSource::Transcript))
}

pub fn read_registry(dir: &Path) -> Vec<RegistryEntry> {
    let Ok(files) = list_files(dir, |n| n.ends_with(".json")) else { return Vec::new() };
    let mut out = Vec::new();
    for (path, _) in files {
        let Ok(bytes) = fs::read(&path) else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { continue };
        let (Some(pid), Some(session_id)) =
            (v.get("pid").and_then(Value::as_u64), v.get("sessionId").and_then(Value::as_str))
        else {
            continue;
        };
        out.push(RegistryEntry {
            pid: pid as u32,
            session_id: session_id.to_string(),
            started_at: v.get("startedAt").and_then(Value::as_i64),
            entrypoint: str_field(&v, "entrypoint").map(str::to_string),
            cwd: str_field(&v, "cwd").map(str::to_string),
        });
    }
    out
}

pub fn origin_from_entrypoint(ep: &str) -> String {
    match ep {
        "claude-desktop" => "desktop".into(),
        "cli" => "cli".into(),
        other => other.to_string(),
    }
}

/// Stateful transcript line parser (remembers background tool launches
/// until their results arrive).
#[derive(Debug, Default)]
pub struct LineParser {
    started: bool,
    cwd: Option<String>,
    origin: Option<String>,
    /// tool_use id -> tool name, for launches that may run in background.
    bg_candidates: BTreeMap<String, String>,
    seen_task_ids: BTreeSet<String>,
}

const TERMINAL_TASK_STATUS: &[&str] = &[
    "completed",
    "complete",
    "done",
    "failed",
    "error",
    "killed",
    "stopped",
    "cancelled",
    "canceled",
    "timeout",
    "timed_out",
    "aborted",
];

impl LineParser {
    fn event(&self, session_id: &str, at: Millis, kind: EventKind) -> AgentEvent {
        AgentEvent::new(AGENT, session_id, at, kind, EvidenceSource::Transcript)
            .with_cwd(self.cwd.clone())
            .with_origin(self.origin.clone())
    }

    pub fn parse(&mut self, session_id: &str, line: &str, out: &mut Vec<AgentEvent>) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        if let Some(cwd) = str_field(&v, "cwd") {
            self.cwd = Some(cwd.to_string());
        }
        if let Some(ep) = str_field(&v, "entrypoint") {
            self.origin = Some(origin_from_entrypoint(ep));
        }
        let Some(at) = v.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_millis) else {
            return; // metadata lines (titles, cost state, ...) carry no timestamp
        };
        let mut kinds: Vec<EventKind> = Vec::new();
        let mut push = |kind: EventKind| kinds.push(kind);
        match str_field(&v, "type").unwrap_or("") {
            "user" => kinds = self.user_entry(&v),
            "assistant" => kinds = self.assistant_entry(&v),
            "system" => match str_field(&v, "subtype").unwrap_or("") {
                "stop_hook_summary" => {
                    if v.get("preventedContinuation").and_then(Value::as_bool).unwrap_or(false) {
                        push(EventKind::Activity { detail: Some("stop_hook_blocked".into()) });
                    } else {
                        push(EventKind::TurnCompleted { continuing: false, pending: None });
                    }
                }
                "api_error" => {
                    if v.get("retryAttempt").is_some() || v.get("retryInMs").is_some() {
                        push(EventKind::TurnFailed {
                            kind: FailureKind::Reconnecting,
                            reason: Some("api_retry".into()),
                            resets_at: None,
                        });
                    } else {
                        push(EventKind::Heartbeat);
                    }
                }
                _ => push(EventKind::Heartbeat),
            },
            "attachment" => {
                let att = v.get("attachment").cloned().unwrap_or(Value::Null);
                match str_field(&att, "type").unwrap_or("") {
                    "goal_status" => {
                        let met = att.get("met").and_then(Value::as_bool).unwrap_or(false);
                        push(EventKind::GoalStatus { active: !met });
                    }
                    "queued_command" => {
                        // Task notifications delivered mid-turn arrive this way.
                        let prompt = str_field(&att, "prompt").unwrap_or("");
                        for (task, status) in task_notifications(prompt) {
                            self.seen_task_ids.insert(task.clone());
                            if TERMINAL_TASK_STATUS.contains(&status.as_str()) {
                                push(EventKind::BackgroundFinished { id: task });
                            }
                        }
                        push(EventKind::Activity { detail: Some("queued_command".into()) });
                    }
                    _ => push(EventKind::Heartbeat),
                }
            }
            "queue-operation" => {
                if str_field(&v, "operation") == Some("enqueue") {
                    let content = str_field(&v, "content").unwrap_or("");
                    for (task, status) in task_notifications(content) {
                        self.seen_task_ids.insert(task.clone());
                        if TERMINAL_TASK_STATUS.contains(&status.as_str()) {
                            push(EventKind::BackgroundFinished { id: task });
                        }
                    }
                    push(EventKind::Activity { detail: Some("queued".into()) });
                } else {
                    push(EventKind::Heartbeat);
                }
            }
            _ => {}
        }
        for kind in kinds {
            out.push(self.event(session_id, at, kind));
        }
    }

    fn user_entry(&mut self, v: &Value) -> Vec<EventKind> {
        let mut kinds = Vec::new();
        let content = v.get("message").and_then(|m| m.get("content"));
        let texts = content_texts(content);
        let has_tool_result = content
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|i| str_field(i, "type") == Some("tool_result")));

        if has_tool_result {
            kinds.push(EventKind::Activity { detail: Some("tool_result".into()) });
            for item in content.and_then(Value::as_array).into_iter().flatten() {
                if str_field(item, "type") != Some("tool_result") {
                    continue;
                }
                let Some(id) = str_field(item, "tool_use_id") else { continue };
                if let Some(tool) = self.bg_candidates.remove(id) {
                    let text = content_texts(item.get("content")).join("\n");
                    let structured = v.get("toolUseResult");
                    if let Some(task) = background_id(&tool, &text, structured) {
                        if !self.seen_task_ids.contains(&task) {
                            kinds.push(EventKind::BackgroundStarted { id: task });
                        }
                    } else if launched_in_background(&tool, &text) {
                        // Launched, but no id we can match later: sticky.
                        kinds.push(EventKind::BackgroundStarted { id: format!("anon:{id}") });
                    }
                }
            }
            return kinds;
        }

        let joined = texts.join("\n");
        let origin = str_field(v, "turnOrigin").unwrap_or("");
        let is_notification = origin == "task_notification"
            || v.get("origin").and_then(|o| str_field(o, "kind")) == Some("task-notification")
            || joined.contains("<task-notification>");
        if is_notification {
            for (task, status) in task_notifications(&joined) {
                self.seen_task_ids.insert(task.clone());
                if TERMINAL_TASK_STATUS.contains(&status.as_str()) {
                    kinds.push(EventKind::BackgroundFinished { id: task });
                }
            }
            kinds.push(EventKind::TurnStarted);
            return kinds;
        }
        if joined.trim_start().starts_with("[Request interrupted by user") {
            kinds.push(EventKind::TurnFailed {
                kind: FailureKind::Interrupted,
                reason: Some("user".into()),
                resets_at: None,
            });
            return kinds;
        }
        if joined.contains("<command-name>/goal</command-name>") {
            let args = between(&joined, "<command-args>", "</command-args>").unwrap_or("").trim().to_string();
            if args.eq_ignore_ascii_case("clear") {
                kinds.push(EventKind::GoalStatus { active: false });
            }
            kinds.push(EventKind::Heartbeat);
            return kinds;
        }
        let is_meta = v.get("isMeta").and_then(Value::as_bool).unwrap_or(false);
        let local = joined.contains("<local-command-stdout>")
            || joined.contains("<local-command-stderr>")
            || joined.contains("<command-name>")
            || joined.contains("<local-command-caveat>");
        if is_meta || local || texts.is_empty() {
            kinds.push(EventKind::Heartbeat);
        } else {
            kinds.push(EventKind::TurnStarted);
        }
        kinds
    }

    fn assistant_entry(&mut self, v: &Value) -> Vec<EventKind> {
        let mut kinds = Vec::new();
        if v.get("isApiErrorMessage").and_then(Value::as_bool).unwrap_or(false) {
            let err = str_field(v, "error").unwrap_or("");
            let api = str_field(v, "apiError").unwrap_or("");
            let text = content_texts(v.get("message").and_then(|m| m.get("content"))).join(" ").to_lowercase();
            let limited = err == "rate_limit"
                || api.contains("limit")
                || v.get("apiErrorStatus").and_then(Value::as_u64) == Some(429)
                || text.contains("limit");
            let resets_at = v
                .get("quotaLimits")
                .and_then(|q| q.get("resetsAt"))
                .and_then(Value::as_i64)
                .map(|s| if s < 100_000_000_000 { s * 1000 } else { s });
            let kind = if limited { FailureKind::RateLimited } else { FailureKind::Error };
            let reason = v
                .get("quotaLimits")
                .and_then(|q| str_field(q, "rateLimitType"))
                .or(Some(err).filter(|e| !e.is_empty()))
                .map(str::to_string);
            kinds.push(EventKind::TurnFailed { kind, reason, resets_at });
            return kinds;
        }
        let items = v.get("message").and_then(|m| m.get("content")).and_then(Value::as_array);
        let mut any = false;
        for item in items.into_iter().flatten() {
            any = true;
            if str_field(item, "type") != Some("tool_use") {
                continue;
            }
            let name = str_field(item, "name").unwrap_or("").to_string();
            let input = item.get("input").cloned().unwrap_or(Value::Null);
            let id = str_field(item, "id").unwrap_or("").to_string();
            let bg_flag = input.get("run_in_background").and_then(Value::as_bool).unwrap_or(false);
            match name.as_str() {
                "ScheduleWakeup" => {
                    if !input.get("stop").and_then(Value::as_bool).unwrap_or(false) {
                        kinds.push(EventKind::ContinuationScheduled { recurring: false });
                    }
                }
                "CronCreate" => {
                    let recurring = input.get("recurring").and_then(Value::as_bool).unwrap_or(true);
                    kinds.push(EventKind::ContinuationScheduled { recurring });
                }
                _ => {}
            }
            let maybe_bg = bg_flag || matches!(name.as_str(), "Monitor" | "Agent" | "Task" | "Workflow");
            if maybe_bg && !id.is_empty() {
                self.bg_candidates.insert(id, name.clone());
            }
            kinds.push(EventKind::Activity { detail: Some(format!("tool:{name}")) });
        }
        if any && kinds.is_empty() {
            kinds.push(EventKind::Activity { detail: Some("assistant".into()) });
        }
        kinds
    }
}

fn content_texts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(s.clone()),
                Value::Object(_) => match str_field(i, "type") {
                    Some("text") => str_field(i, "text").map(str::to_string),
                    Some("tool_result") => Some(content_texts(i.get("content")).join("\n")),
                    _ => None,
                },
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let a = s.find(start)? + start.len();
    let b = s[a..].find(end)? + a;
    Some(&s[a..b])
}

/// `(task-id, status)` pairs of every `<task-notification>` in the text.
pub fn task_notifications(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<task-notification>") {
        let block_end = rest[start..].find("</task-notification>").map_or(rest.len(), |e| start + e);
        let block = &rest[start..block_end];
        if let Some(id) = between(block, "<task-id>", "</task-id>") {
            let status = between(block, "<status>", "</status>").unwrap_or("").trim().to_lowercase();
            out.push((id.trim().to_string(), status));
        }
        rest = &rest[block_end..];
        if rest.len() == text.len() {
            break;
        }
        if let Some(stripped) = rest.strip_prefix("</task-notification>") {
            rest = stripped;
        }
    }
    out
}

fn ident_after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let i = text.find(marker)? + marker.len();
    let rest = text[i..].trim_start();
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(rest.len());
    let id = &rest[..end];
    (!id.is_empty()).then_some(id)
}

/// Task id of a background launch, from structured results or result text.
fn background_id(tool: &str, text: &str, structured: Option<&Value>) -> Option<String> {
    if let Some(s) = structured.filter(|s| s.is_object()) {
        let is_async = s.get("isAsync").and_then(Value::as_bool);
        if is_async == Some(false) {
            return None; // foreground subagent: finished when the result arrived
        }
        for k in ["backgroundTaskId", "taskId", "task_id", "shellId", "bash_id"] {
            if let Some(id) = s.get(k).and_then(Value::as_str) {
                return Some(id.to_string());
            }
        }
        if is_async == Some(true) {
            if let Some(id) = s.get("agentId").or_else(|| s.get("agent_id")).and_then(Value::as_str) {
                return Some(id.to_string());
            }
        }
    }
    let launched = launched_in_background(tool, text);
    if !launched {
        return None;
    }
    for marker in ["with ID:", "agentId:", "task ID:", "Task ID:", "task_id:", "taskId:", "ID:"] {
        if let Some(id) = ident_after(text, marker) {
            return Some(id.to_string());
        }
    }
    None
}

fn launched_in_background(tool: &str, text: &str) -> bool {
    let t = text.to_lowercase();
    match tool {
        "Agent" | "Task" => t.contains("async agent launched") || t.contains("running in the background"),
        "Workflow" => t.contains("background") || t.contains("task id") || t.contains("returns immediately"),
        "Monitor" => !t.contains("error"),
        _ => t.contains("background") || t.contains("with id"),
    }
}

/// Normalizes one Claude Code hook invocation.
pub fn hook_events(rec: &SpoolRecord) -> Vec<AgentEvent> {
    let p = &rec.payload;
    let Some(session_id) = str_field(p, "session_id") else { return Vec::new() };
    let at = rec.received_at;
    let name = str_field(p, "hook_event_name").unwrap_or("");
    let mut kinds = Vec::new();
    match name {
        "SessionStart" => kinds.push(EventKind::SessionStarted { source: str_field(p, "source").map(str::to_string) }),
        "UserPromptSubmit" | "UserPromptExpansion" => kinds.push(EventKind::TurnStarted),
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PostToolBatch" | "PermissionDenied"
        | "ElicitationResult" | "SubagentStart" | "TaskCreated" | "TaskCompleted" => {
            let tool = str_field(p, "tool_name").unwrap_or(name);
            if name == "PreToolUse" {
                let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
                match tool {
                    "ScheduleWakeup" if !input.get("stop").and_then(Value::as_bool).unwrap_or(false) => {
                        kinds.push(EventKind::ContinuationScheduled { recurring: false })
                    }
                    "CronCreate" => kinds.push(EventKind::ContinuationScheduled {
                        recurring: input.get("recurring").and_then(Value::as_bool).unwrap_or(true),
                    }),
                    _ => {}
                }
            }
            kinds.push(EventKind::Activity { detail: Some(format!("{name}:{tool}")) });
        }
        "PermissionRequest" | "Elicitation" => {
            kinds.push(EventKind::WaitingForInput { reason: Some(snake(name)) });
        }
        "Notification" => {
            let t = str_field(p, "notification_type").unwrap_or("");
            match t {
                "permission_prompt"
                | "agent_needs_input"
                | "elicitation_dialog"
                | "elicitation_url_dialog"
                | "quota_auto_resume_stale" => kinds.push(EventKind::WaitingForInput { reason: Some(t.to_string()) }),
                "quota_auto_resume_fired" | "elicitation_complete" | "elicitation_response" => {
                    kinds.push(EventKind::Activity { detail: Some(t.to_string()) })
                }
                "quota_auto_resume_disabled" => kinds.push(EventKind::TurnFailed {
                    kind: FailureKind::RateLimited,
                    reason: Some(t.to_string()),
                    resets_at: None,
                }),
                _ => kinds.push(EventKind::Heartbeat),
            }
        }
        "Stop" => {
            let continuing = p.get("stop_hook_active").and_then(Value::as_bool).unwrap_or(false);
            kinds.push(EventKind::TurnCompleted { continuing, pending: pending_from_stop(p) });
        }
        "StopFailure" => {
            let err = str_field(p, "error").unwrap_or("unknown");
            let kind = if err == "rate_limit" { FailureKind::RateLimited } else { FailureKind::Error };
            kinds.push(EventKind::TurnFailed { kind, reason: Some(err.to_string()), resets_at: None });
        }
        "SessionEnd" => kinds.push(EventKind::SessionEnded { reason: str_field(p, "reason").map(str::to_string) }),
        "" => {}
        _ => kinds.push(EventKind::Heartbeat),
    }
    kinds
        .into_iter()
        .map(|k| {
            AgentEvent::new(AGENT, session_id, at, k, EvidenceSource::Hook)
                .with_cwd(str_field(p, "cwd").map(str::to_string))
        })
        .collect()
}

/// `background_tasks` + `session_crons` of a Stop payload (Claude Code
/// ≥ 2.1.145). `None` when the fields are absent (older versions).
pub fn pending_from_stop(p: &Value) -> Option<PendingWork> {
    let tasks = p.get("background_tasks");
    let crons = p.get("session_crons");
    if tasks.is_none() && crons.is_none() {
        return None;
    }
    let mut pending = PendingWork::default();
    for (i, t) in tasks.and_then(Value::as_array).into_iter().flatten().enumerate() {
        let status = str_field(t, "status").unwrap_or("").to_lowercase();
        if TERMINAL_TASK_STATUS.contains(&status.as_str()) {
            continue;
        }
        let id = str_field(t, "id").map(str::to_string).unwrap_or_else(|| format!("task#{i}"));
        pending.background.push(id);
    }
    for c in crons.and_then(Value::as_array).into_iter().flatten() {
        if c.get("recurring").and_then(Value::as_bool).unwrap_or(true) {
            pending.recurring = true;
        } else {
            pending.wakeup = true;
        }
    }
    Some(pending)
}

fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// How long a quiet Desktop/CLI session process may stay unattributed
/// before the registry is considered stale (not used for completion).
pub const REGISTRY_GRACE: Millis = HOUR;

#[cfg(test)]
mod tests {
    use super::*;
    use agn_core::SessionState;
    use agn_core::tracker::{Tracker, TrackerConfig};
    use serde_json::json;

    fn parse_all(lines: &[Value]) -> Vec<AgentEvent> {
        let mut p = LineParser::default();
        let mut out = Vec::new();
        for l in lines {
            p.parse("s1", &l.to_string(), &mut out);
        }
        out
    }

    fn kinds(evs: &[AgentEvent]) -> Vec<EventKind> {
        evs.iter().map(|e| e.kind.clone()).collect()
    }

    fn run(lines: &[Value], settle_until: Millis) -> SessionState {
        let mut t = Tracker::new(TrackerConfig { settle: 1_000 });
        let now = 4_000_000_000_000;
        t.apply_all(parse_all(lines), now);
        t.tick(settle_until);
        t.get(&agn_core::SessionKey::new(AGENT, "s1")).unwrap().state
    }

    fn ts(sec: u32) -> String {
        format!("2026-10-29T10:00:{sec:02}.000Z")
    }

    #[test]
    fn normal_turn_completes() {
        let lines = vec![
            json!({"type":"user","timestamp":ts(1),"turnOrigin":"human","cwd":"C:\\p","entrypoint":"claude-desktop","message":{"role":"user","content":"do it"}}),
            json!({"type":"assistant","timestamp":ts(2),"message":{"content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}}]}}),
            json!({"type":"user","timestamp":ts(3),"message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"ok"}]}}),
            json!({"type":"assistant","timestamp":ts(4),"message":{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}),
            json!({"type":"system","subtype":"stop_hook_summary","timestamp":ts(5),"preventedContinuation":false}),
            json!({"type":"last-prompt"}),
        ];
        let evs = parse_all(&lines);
        assert_eq!(evs[0].origin.as_deref(), Some("desktop"));
        assert_eq!(evs[0].cwd.as_deref(), Some("C:\\p"));
        assert!(matches!(evs.last().unwrap().kind, EventKind::TurnCompleted { .. }));
        assert_eq!(run(&lines, 4_000_000_000_000), SessionState::Completed);
    }

    #[test]
    fn blocked_stop_hook_keeps_running() {
        let lines = vec![
            json!({"type":"user","timestamp":ts(1),"message":{"content":"go"}}),
            json!({"type":"system","subtype":"stop_hook_summary","timestamp":ts(5),"preventedContinuation":true}),
        ];
        assert_eq!(run(&lines, 4_000_000_000_000), SessionState::Running);
    }

    #[test]
    fn goal_status_blocks_until_met() {
        let mut lines = vec![
            json!({"type":"attachment","timestamp":ts(1),"attachment":{"type":"goal_status","met":false}}),
            json!({"type":"user","timestamp":ts(1),"message":{"content":"<command-name>/goal</command-name><command-args>ship it</command-args>"}}),
            json!({"type":"user","timestamp":ts(2),"message":{"content":"ship it"}}),
            json!({"type":"system","subtype":"stop_hook_summary","timestamp":ts(5),"preventedContinuation":false}),
        ];
        assert_eq!(run(&lines, 4_000_000_000_000), SessionState::AwaitingContinuation);
        lines.push(json!({"type":"attachment","timestamp":ts(6),"attachment":{"type":"goal_status","met":true}}));
        assert_eq!(run(&lines, 4_000_000_000_000), SessionState::Completed);
    }

    #[test]
    fn background_bash_launch_and_notification() {
        let lines = vec![
            json!({"type":"user","timestamp":ts(1),"message":{"content":"go"}}),
            json!({"type":"assistant","timestamp":ts(2),"message":{"content":[{"type":"tool_use","id":"tu9","name":"Bash","input":{"command":"npm test","run_in_background":true}}]}}),
            json!({"type":"user","timestamp":ts(3),"message":{"content":[{"type":"tool_result","tool_use_id":"tu9","content":"Command running in background with ID: be4w3nxaq. Output is being written to: x"}]}}),
            json!({"type":"system","subtype":"stop_hook_summary","timestamp":ts(5),"preventedContinuation":false}),
        ];
        assert!(kinds(&parse_all(&lines)).contains(&EventKind::BackgroundStarted { id: "be4w3nxaq".into() }));
        assert_eq!(run(&lines, 4_000_000_000_000), SessionState::AwaitingContinuation);

        let mut done = lines.clone();
        done.push(json!({"type":"user","timestamp":ts(30),"turnOrigin":"task_notification","origin":{"kind":"task-notification"},
            "message":{"content":"<task-notification>\n<task-id>be4w3nxaq</task-id>\n<status>completed</status>\n<summary>ok</summary>\n</task-notification>"}}));
        done.push(
            json!({"type":"system","subtype":"stop_hook_summary","timestamp":ts(40),"preventedContinuation":false}),
        );
        assert_eq!(run(&done, 4_000_000_000_000), SessionState::Completed);
    }

    #[test]
    fn async_agent_launch_uses_agent_id() {
        let lines = vec![
            json!({"type":"assistant","timestamp":ts(2),"message":{"content":[{"type":"tool_use","id":"tuA","name":"Agent","input":{"prompt":"x"}}]}}),
            json!({"type":"user","timestamp":ts(3),"message":{"content":[{"type":"tool_result","tool_use_id":"tuA","content":[{"type":"text","text":"Async agent launched successfully.\nagentId: a5031b0a29c4f689b (internal ID)"}]}]}}),
        ];
        assert!(kinds(&parse_all(&lines)).contains(&EventKind::BackgroundStarted { id: "a5031b0a29c4f689b".into() }));
    }

    #[test]
    fn foreground_agent_is_not_background() {
        let lines = vec![
            json!({"type":"assistant","timestamp":ts(2),"message":{"content":[{"type":"tool_use","id":"tuB","name":"Agent","input":{"prompt":"x"}}]}}),
            json!({"type":"user","timestamp":ts(3),"message":{"content":[{"type":"tool_result","tool_use_id":"tuB","content":"Here is the final report of the subagent."}]}}),
        ];
        assert!(!kinds(&parse_all(&lines)).iter().any(|k| matches!(k, EventKind::BackgroundStarted { .. })));
    }

    #[test]
    fn background_without_id_is_sticky() {
        let lines = vec![
            json!({"type":"assistant","timestamp":ts(2),"message":{"content":[{"type":"tool_use","id":"tuC","name":"Bash","input":{"run_in_background":true}}]}}),
            json!({"type":"user","timestamp":ts(3),"message":{"content":[{"type":"tool_result","tool_use_id":"tuC","content":"started in background"}]}}),
        ];
        assert!(kinds(&parse_all(&lines)).contains(&EventKind::BackgroundStarted { id: "anon:tuC".into() }));
    }

    #[test]
    fn usage_limit_and_interrupt() {
        let limit = json!({"type":"assistant","timestamp":ts(2),"isApiErrorMessage":true,"error":"rate_limit","apiErrorStatus":429,
            "quotaLimits":{"status":"rejected","resetsAt":1793300000,"rateLimitType":"five_hour"},
            "message":{"model":"<synthetic>","content":[{"type":"text","text":"You've hit your session limit · resets 12:30pm"}]}});
        let evs = parse_all(&[limit]);
        assert_eq!(
            evs[0].kind,
            EventKind::TurnFailed {
                kind: FailureKind::RateLimited,
                reason: Some("five_hour".into()),
                resets_at: Some(1_793_300_000_000)
            }
        );
        let int = json!({"type":"user","timestamp":ts(3),"message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}});
        assert!(matches!(parse_all(&[int])[0].kind, EventKind::TurnFailed { kind: FailureKind::Interrupted, .. }));
        let retry = json!({"type":"system","subtype":"api_error","timestamp":ts(4),"retryAttempt":2,"maxRetries":10});
        assert!(matches!(parse_all(&[retry])[0].kind, EventKind::TurnFailed { kind: FailureKind::Reconnecting, .. }));
    }

    #[test]
    fn schedule_wakeup_and_cron() {
        let lines = vec![
            json!({"type":"assistant","timestamp":ts(2),"message":{"content":[{"type":"tool_use","id":"w","name":"ScheduleWakeup","input":{"delaySeconds":1200}}]}}),
            json!({"type":"assistant","timestamp":ts(3),"message":{"content":[{"type":"tool_use","id":"c","name":"CronCreate","input":{"cron":"*/5 * * * *"}}]}}),
        ];
        let k = kinds(&parse_all(&lines));
        assert!(k.contains(&EventKind::ContinuationScheduled { recurring: false }));
        assert!(k.contains(&EventKind::ContinuationScheduled { recurring: true }));
    }

    #[test]
    fn queue_operations_and_local_commands() {
        let lines = vec![
            json!({"type":"queue-operation","operation":"enqueue","timestamp":ts(1),"content":"<task-notification>"}),
            json!({"type":"user","timestamp":ts(2),"isMeta":true,"message":{"content":"caveat"}}),
            json!({"type":"user","timestamp":ts(3),"message":{"content":"<local-command-stdout>ok</local-command-stdout>"}}),
            json!({"type":"custom-title","customTitle":"x"}),
            json!("not an object"),
        ];
        let k = kinds(&parse_all(&lines));
        assert_eq!(k.len(), 3);
        assert!(matches!(k[0], EventKind::Activity { .. }));
        assert_eq!(k[1], EventKind::Heartbeat);
        assert_eq!(k[2], EventKind::Heartbeat);
    }

    #[test]
    fn task_notification_parser() {
        let t = "<task-notification><task-id>a</task-id><status>completed</status></task-notification>x<task-notification><task-id>b</task-id><status>running</status></task-notification>";
        assert_eq!(task_notifications(t), vec![("a".into(), "completed".into()), ("b".into(), "running".into())]);
        assert!(task_notifications("nothing").is_empty());
        assert_eq!(task_notifications("<task-notification><task-id>c</task-id>"), vec![("c".into(), "".into())]);
    }

    #[test]
    fn hook_stop_with_pending_work() {
        let rec = SpoolRecord::new(
            "claude_code",
            json!({"session_id":"s","hook_event_name":"Stop","stop_hook_active":false,
                "background_tasks":[{"id":"t1","type":"shell","status":"running"},{"id":"t2","type":"subagent","status":"completed"}],
                "session_crons":[{"id":"c1","recurring":false}]}),
        );
        let evs = hook_events(&rec);
        assert_eq!(
            evs[0].kind,
            EventKind::TurnCompleted {
                continuing: false,
                pending: Some(PendingWork { background: vec!["t1".into()], wakeup: true, recurring: false })
            }
        );
        let old = SpoolRecord::new("claude_code", json!({"session_id":"s","hook_event_name":"Stop"}));
        assert_eq!(hook_events(&old)[0].kind, EventKind::TurnCompleted { continuing: false, pending: None });
    }

    #[test]
    fn hook_mapping() {
        let mk =
            |v: Value| hook_events(&SpoolRecord::new("claude_code", v)).into_iter().map(|e| e.kind).collect::<Vec<_>>();
        assert_eq!(
            mk(json!({"session_id":"s","hook_event_name":"UserPromptSubmit","prompt":"x"})),
            vec![EventKind::TurnStarted]
        );
        assert!(matches!(
            mk(json!({"session_id":"s","hook_event_name":"StopFailure","error":"rate_limit"}))[0],
            EventKind::TurnFailed { kind: FailureKind::RateLimited, .. }
        ));
        assert!(matches!(
            mk(json!({"session_id":"s","hook_event_name":"StopFailure","error":"overloaded"}))[0],
            EventKind::TurnFailed { kind: FailureKind::Error, .. }
        ));
        assert!(matches!(
            mk(json!({"session_id":"s","hook_event_name":"Notification","notification_type":"permission_prompt"}))[0],
            EventKind::WaitingForInput { .. }
        ));
        assert_eq!(
            mk(json!({"session_id":"s","hook_event_name":"Notification","notification_type":"idle_prompt"})),
            vec![EventKind::Heartbeat]
        );
        assert_eq!(mk(json!({"session_id":"s","hook_event_name":"SubagentStop"})), vec![EventKind::Heartbeat]);
        assert!(matches!(
            mk(json!({"session_id":"s","hook_event_name":"PermissionRequest"}))[0],
            EventKind::WaitingForInput { .. }
        ));
        assert_eq!(
            mk(json!({"session_id":"s","hook_event_name":"SessionStart","source":"compact"})),
            vec![EventKind::SessionStarted { source: Some("compact".into()) }]
        );
        assert!(mk(json!({"hook_event_name":"Stop"})).is_empty(), "no session id, no event");
        let k = mk(
            json!({"session_id":"s","hook_event_name":"PreToolUse","tool_name":"ScheduleWakeup","tool_input":{"delaySeconds":60}}),
        );
        assert_eq!(k[0], EventKind::ContinuationScheduled { recurring: false });
    }

    #[test]
    fn adapter_polls_temp_tree_and_registry() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("projects").join("C--work-app");
        fs::create_dir_all(proj.join("sid1").join("subagents")).unwrap();
        fs::create_dir_all(dir.path().join("sessions")).unwrap();
        fs::write(
            dir.path().join("sessions").join("123.json"),
            json!({"pid":123,"sessionId":"sid1","cwd":"C:\\work\\app","startedAt":1,"entrypoint":"cli","status":"busy"}).to_string(),
        )
        .unwrap();
        let lines = [
            json!({"type":"user","timestamp":ts(1),"cwd":"C:\\work\\app","entrypoint":"cli","message":{"content":"go"}}),
            json!({"type":"system","subtype":"stop_hook_summary","timestamp":ts(5),"preventedContinuation":false}),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(proj.join("sid1.jsonl"), body).unwrap();
        let now = agn_core::time::now_millis();
        let mut a = ClaudeAdapter::new(dir.path().to_path_buf(), 24 * HOUR);
        let (evs, st) = a.poll(now, true);
        assert_eq!(st, SourceStatus::Ok);
        assert_eq!(a.registry().len(), 1);
        assert_eq!(a.registry()[0].session_id, "sid1");
        assert!(evs.iter().any(|e| matches!(e.kind, EventKind::TurnCompleted { .. })));
        let (evs, _) = a.poll(now, false);
        assert!(evs.is_empty(), "no new lines");
        let missing = ClaudeAdapter::new(dir.path().join("nope"), HOUR).poll(now, true).1;
        assert_eq!(missing, SourceStatus::Missing);
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    use agn_core::SessionState;
    use agn_core::tracker::{Tracker, TrackerConfig};
    use serde_json::json;

    fn ts(sec: u32) -> String {
        format!("2026-10-29T11:00:{sec:02}.000Z")
    }

    /// Shapes observed in real Claude Code 2.1.309 (Desktop) transcripts.
    #[test]
    fn mid_turn_notifications_and_structured_results() {
        let lines = vec![
            json!({"type":"user","timestamp":ts(1),"turnOrigin":"human","message":{"content":"go"}}),
            json!({"type":"assistant","timestamp":ts(2),"message":{"content":[{"type":"tool_use","id":"t1","name":"Agent","input":{"prompt":"x","run_in_background":true}}]}}),
            json!({"type":"user","timestamp":ts(3),"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a5031b0a29c4f689b"},
                "message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"Async agent launched successfully."}]}]}}),
            json!({"type":"assistant","timestamp":ts(4),"message":{"content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"x","run_in_background":true}}]}}),
            json!({"type":"user","timestamp":ts(5),"toolUseResult":{"stdout":"","backgroundTaskId":"be4w3nxaq"},
                "message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"Command running in background with ID: be4w3nxaq."}]}}),
            json!({"type":"assistant","timestamp":ts(6),"message":{"content":[{"type":"tool_use","id":"t3","name":"Agent","input":{"prompt":"y"}}]}}),
            json!({"type":"user","timestamp":ts(7),"toolUseResult":{"isAsync":false,"agentId":"fg1"},
                "message":{"content":[{"type":"tool_result","tool_use_id":"t3","content":"final report"}]}}),
            json!({"type":"queue-operation","operation":"enqueue","timestamp":ts(8),
                "content":"<task-notification>\n<task-id>be4w3nxaq</task-id>\n<status>completed</status>\n</task-notification>"}),
            json!({"type":"attachment","timestamp":ts(9),"attachment":{"type":"queued_command","commandMode":"task-notification",
                "prompt":"<task-notification>\n<task-id>a5031b0a29c4f689b</task-id>\n<status>completed</status>\n</task-notification>"}}),
            json!({"type":"system","subtype":"stop_hook_summary","timestamp":ts(10),"preventedContinuation":false}),
        ];
        let mut p = LineParser::default();
        let mut evs = Vec::new();
        for l in &lines {
            p.parse("s", &l.to_string(), &mut evs);
        }
        let started: Vec<_> = evs
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::BackgroundStarted { id } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(started, vec!["a5031b0a29c4f689b", "be4w3nxaq"], "foreground agent not counted");
        let mut t = Tracker::new(TrackerConfig { settle: 1_000 });
        t.apply_all(evs, 4_000_000_000_000);
        t.tick(4_000_000_000_000);
        let s = t.get(&agn_core::SessionKey::new(AGENT, "s")).unwrap();
        assert!(s.background.is_empty(), "{:?}", s.background);
        assert_eq!(s.state, SessionState::Completed);
    }
}
