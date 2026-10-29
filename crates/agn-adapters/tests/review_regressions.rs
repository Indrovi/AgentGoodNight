//! Regression tests for the findings of the independent safety review
//! (2026-10-29). Each test describes a sequence that used to end in a power
//! action while work was ongoing or unverifiable; all must now stay safe.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use agn_adapters::hook;
use agn_adapters::integration::{self, IntegrationEnv, hook_bin_name};
use agn_adapters::monitor::{Monitor, MonitorPaths, ProcessSource};
use agn_adapters::process::{AgentProcess, ProcessRole};
use agn_core::config::Settings;
use agn_core::event::{AgentEvent, EventKind};
use agn_core::model::{AgentKind, EvidenceSource, SessionKey, SessionState};
use agn_core::policy::{BlockReason, Verdict};
use agn_core::power::{PowerAction, PowerBackend};
use agn_core::spool::{SpoolRecord, write_record};
use agn_core::time::{MINUTE, Millis, SECOND, now_millis};
use agn_core::tracker::{Tracker, TrackerConfig};
use serde_json::{Value, json};

const CODEX_ID: &str = "019a1b2c-3d4e-7f00-8a9b-0c1d2e3f4a5b";

fn iso(ms: Millis) -> String {
    let secs = ms.div_euclid(1000);
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        ms.rem_euclid(1000)
    )
}

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    t0: Millis,
    monitor: Monitor,
    procs: Vec<AgentProcess>,
    /// Parent chain the simulated hook reports (the agent process).
    hook_ancestors: Vec<u32>,
}

impl World {
    fn new_with(settings: Settings) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in [".claude/projects/C--work-web", ".claude/sessions", ".codex/sessions/2026/10/29", "data/spool", "app"]
        {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("app").join(hook_bin_name()), b"hook").unwrap();
        let env = IntegrationEnv {
            claude_root: Some(root.join(".claude")),
            gemini_root: Some(root.join(".gemini")),
            cursor_root: Some(root.join(".cursor")),
            opencode_root: Some(root.join(".config/opencode")),
            data_dir: root.join("data"),
            spool_dir: root.join("data/spool"),
            hook_source: Some(root.join("app").join(hook_bin_name())),
        };
        let paths = MonitorPaths {
            data_dir: root.join("data"),
            claude_root: Some(root.join(".claude")),
            codex_root: Some(root.join(".codex")),
            default_claude_root: None,
            default_codex_root: None,
            integration: env,
        };
        let monitor = Monitor::new(settings, paths, ProcessSource::Fixed(Ok(vec![]), false), PowerBackend::DryRun);
        Self { _dir: dir, root, t0: now_millis(), monitor, procs: vec![], hook_ancestors: vec![] }
    }

    fn new() -> Self {
        Self::new_with(Settings { settle_secs: 15, countdown_secs: 10, ..Settings::default() })
    }

    fn at(&self, sec: i64) -> Millis {
        self.t0 + sec * SECOND
    }

    fn append(&self, path: &Path, lines: &[Value]) {
        let mut f = OpenOptions::new().create(true).append(true).open(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn claude(&self, sec: i64, line: Value) {
        let mut line = line;
        line["timestamp"] = json!(iso(self.at(sec)));
        line["cwd"] = json!("C:\\work\\web");
        line["entrypoint"] = json!("cli");
        self.append(&self.root.join(".claude/projects/C--work-web/c1.jsonl"), &[line]);
    }

    fn codex(&self, sec: i64, ty: &str, payload: Value) {
        let line = json!({"timestamp": iso(self.at(sec)), "type": ty, "payload": payload});
        let name = format!("rollout-2026-10-29T10-00-00-{CODEX_ID}.jsonl");
        self.append(&self.root.join(".codex/sessions/2026/10/29").join(name), &[line]);
    }

    fn codex_done(&self, a: i64) {
        self.codex(a, "session_meta", json!({"id": CODEX_ID, "cwd": "/w"}));
        self.codex(a + 1, "event_msg", json!({"type":"task_started"}));
        self.codex(a + 2, "event_msg", json!({"type":"task_complete"}));
    }

    /// Hook record stamped with simulated time.
    fn hook_at(&self, sec: i64, agent: &str, payload: Value) {
        let mut rec = SpoolRecord::new(agent, payload);
        rec.received_at = self.at(sec);
        write_record(&self.root.join("data/spool"), &rec).unwrap();
    }

    fn hook(&self, agent: &str, payload: Value) {
        let input = payload.to_string().into_bytes();
        let out = hook::run(Some(agent), &mut input.as_slice(), &self.root.join("data"), self.hook_ancestors.clone());
        assert!(out.stored);
    }

    /// Hook record stamped with simulated time, produced by process `pid`.
    fn hook_from(&self, sec: i64, pid: u32, agent: &str, payload: Value) {
        let mut rec = SpoolRecord::new(agent, payload);
        rec.received_at = self.at(sec);
        rec.ancestors = vec![1, pid];
        write_record(&self.root.join("data/spool"), &rec).unwrap();
    }

    fn register_claude(&mut self, pid: u32, session: &str, status: &str) {
        let started = self.t0 - 60 * SECOND;
        fs::write(
            self.root.join(format!(".claude/sessions/{pid}.json")),
            json!({"pid": pid, "sessionId": session, "cwd": "C:\\work\\web", "startedAt": started, "entrypoint": "cli", "status": status})
                .to_string(),
        )
        .unwrap();
        self.add_proc(AgentKind::ClaudeCode, pid, ProcessRole::Session, started, None);
    }

    fn add_proc(&mut self, agent: AgentKind, pid: u32, role: ProcessRole, start: Millis, cwd: Option<&str>) {
        self.procs.push(AgentProcess {
            agent,
            pid,
            start_time: start,
            name: format!("{agent:?}"),
            role,
            origin: None,
            cwd: cwd.map(str::to_string),
            config_home: None,
            parent: None,
        });
        self.monitor.set_fixed_processes(Ok(self.procs.clone()));
        self.monitor.invalidate();
    }

    fn run_until(&mut self, from: i64, to: i64) -> Vec<(i64, PowerAction)> {
        let mut out = Vec::new();
        for s in from..=to {
            let action = self.monitor.cycle(self.at(s));
            self.monitor.commit_spool();
            if let Some(a) = action {
                out.push((s, a));
            }
        }
        out
    }

    fn state(&self, agent: AgentKind, id: &str) -> SessionState {
        self.monitor.tracker().get(&SessionKey::new(agent, id)).map(|s| s.state).unwrap_or(SessionState::Unknown)
    }

    fn reasons(&self) -> Vec<BlockReason> {
        match self.monitor.verdict() {
            Verdict::Blocked { reasons } => reasons.clone(),
            _ => vec![],
        }
    }
}

// Late evidence timestamped after the turn end must revive the session even
// when it is delivered after the settle window expired.
#[test]
fn late_turn_start_after_tick_still_revives() {
    let mut t = Tracker::new(TrackerConfig { settle: 90 * SECOND, ..Default::default() });
    let ev = |at, kind| AgentEvent::new(AgentKind::ClaudeCode, "s", at, kind, EvidenceSource::Transcript);
    t.apply(&ev(0, EventKind::TurnStarted), 0);
    t.apply(&ev(10_000, EventKind::TurnCompleted { continuing: false, pending: None }), 10_000);
    t.tick(100_000);
    let s = t.get(&SessionKey::new(AgentKind::ClaudeCode, "s")).unwrap();
    assert_eq!(s.state, SessionState::Completed);
    assert_eq!(s.completed_at, Some(100_000), "completion time comes from the evidence (settle end)");
    t.apply(&ev(99_500, EventKind::TurnStarted), 101_000);
    t.apply(&ev(99_900, EventKind::Activity { detail: None }), 101_000);
    assert_eq!(t.get(&SessionKey::new(AgentKind::ClaudeCode, "s")).unwrap().state, SessionState::Running);
}

#[test]
fn old_completion_first_seen_after_arming_does_not_count() {
    let mut settings = Settings { settle_secs: 15, countdown_secs: 10, ..Settings::default() };
    settings.agents.insert(AgentKind::ClaudeCode, false);
    let mut w = World::new_with(settings);
    w.register_claude(9001, "c1", "idle");
    w.claude(-7200, json!({"type":"user","message":{"content":"old task"}}));
    w.claude(-7190, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert!(w.run_until(0, 5).is_empty());
    w.monitor.arm(w.at(6));
    assert!(w.run_until(6, 10).is_empty());
    w.monitor.set_agent_enabled(AgentKind::ClaudeCode, true);
    assert!(w.run_until(11, 120).is_empty());
    assert!(w.reasons().contains(&BlockReason::NoCompletionRecorded));
}

#[test]
fn finishing_across_sleep_does_not_count_after_wake() {
    let mut w = World::new();
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    assert!(w.run_until(0, 5).is_empty());
    let wake = 3 * 3600;
    assert!(w.run_until(wake, wake + 120).is_empty());
    assert!(w.reasons().contains(&BlockReason::NoCompletionRecorded));
}

#[test]
fn spool_backlog_keeps_later_turn_start() {
    let mut w = World::new();
    let spool = w.root.join("data/spool");
    let base = w.at(-3600);
    let rec = |i: i64, name: &str| {
        let mut r = SpoolRecord::new("gemini_cli", json!({"session_id":"g1","hook_event_name":name}));
        r.received_at = base + i * 100;
        r
    };
    write_record(&spool, &rec(0, "BeforeAgent")).unwrap();
    for i in 1..4999 {
        write_record(&spool, &rec(i, "BeforeTool")).unwrap();
    }
    write_record(&spool, &rec(4999, "AfterAgent")).unwrap();
    write_record(&spool, &rec(5000, "BeforeAgent")).unwrap();
    write_record(&spool, &rec(5001, "BeforeTool")).unwrap();
    w.run_until(0, 3);
    assert_eq!(w.state(AgentKind::GeminiCli, "g1"), SessionState::Running);
    w.monitor.arm(w.at(4));
    w.codex_done(5);
    assert!(w.run_until(4, 120).is_empty());
    assert!(fs::read_dir(&spool).unwrap().filter_map(|e| e.ok()).all(|e| e.path().is_dir()), "spool drained");
}

#[test]
fn codex_process_without_rollout_blocks() {
    let mut w = World::new();
    w.add_proc(AgentKind::Codex, 4242, ProcessRole::Session, w.t0, Some("/elsewhere"));
    w.register_claude(9001, "c1", "busy");
    w.monitor.arm(w.at(0));
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.claude(5, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert!(w.run_until(0, 120).is_empty());
    assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 4242, .. })));
}

#[test]
fn codex_process_with_active_rollout_in_its_dir_is_attributed() {
    let mut w = World::new();
    w.add_proc(AgentKind::Codex, 4242, ProcessRole::Session, w.t0 - 30 * SECOND, Some("/w"));
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    assert_eq!(w.run_until(0, 40).len(), 1);
}

#[test]
fn silent_hook_only_process_blocks() {
    let mut w = World::new();
    integration::install(AgentKind::GeminiCli, w.monitor.integration_env()).unwrap();
    w.add_proc(AgentKind::GeminiCli, 7000, ProcessRole::Session, now_millis() + 60 * SECOND, None);
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    assert!(w.run_until(0, 120).is_empty());
    assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 7000, .. })));
}

#[test]
fn ignored_session_is_tracked_again_when_it_runs() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.claude(2, json!({"type":"user","message":{"content":"[Request interrupted by user]"}}));
    w.run_until(0, 5);
    assert!(w.monitor.set_session_ignored(&SessionKey::new(AgentKind::ClaudeCode, "c1"), true));
    w.monitor.arm(w.at(6));
    w.claude(7, json!({"type":"user","message":{"content":"new long task"}}));
    w.codex_done(8);
    assert!(w.run_until(6, 120).is_empty());
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::Running);
}

#[test]
fn many_background_tasks_are_not_truncated() {
    for total in [65, 600] {
        let mut w = World::new();
        w.register_claude(9001, "c1", "busy");
        w.monitor.arm(w.at(0));
        w.hook("claude_code", json!({"session_id":"c1","hook_event_name":"UserPromptSubmit"}));
        let mut tasks: Vec<Value> =
            (0..total - 1).map(|i| json!({"id": format!("done{i}"), "status": "completed"})).collect();
        tasks.push(json!({"id": "still-running", "status": "running"}));
        w.hook(
            "claude_code",
            json!({"session_id":"c1","hook_event_name":"Stop","background_tasks":tasks,"session_crons":[]}),
        );
        assert!(w.run_until(0, 120).is_empty(), "{total} tasks");
        assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::AwaitingContinuation);
    }
}

fn background_before_filler(w: &World, filler_lines: i64) {
    w.claude(-900, json!({"type":"user","message":{"content":"go"}}));
    w.claude(-899, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"run_in_background":true}}]}}));
    w.claude(-898, json!({"type":"user","toolUseResult":{"backgroundTaskId":"bg1"},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"Command running in background with ID: bg1"}]}}));
    let big = "x".repeat(200_000);
    for i in 0..filler_lines {
        let s = -890 + i * 2 / 5;
        w.claude(s, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":format!("r{i}"),"name":"Read","input":{}}]}}));
        w.claude(s, json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":format!("r{i}"),"content":big}]}}));
    }
}

#[test]
fn background_launch_inside_large_window_blocks() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    background_before_filler(&w, 25); // ~5 MB: launch is inside the window
    assert!(w.run_until(0, 2).is_empty());
    w.monitor.arm(w.at(3));
    w.claude(5, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert!(w.run_until(3, 120).is_empty());
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::AwaitingContinuation);
}

#[test]
fn background_launch_before_initial_window_blocks_until_hook_snapshot() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    background_before_filler(&w, 175); // ~35 MB: launch is outside the window
    assert!(w.run_until(0, 2).is_empty());
    w.monitor.arm(w.at(3));
    w.claude(5, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert!(w.run_until(3, 120).is_empty());
    let s = w.monitor.tracker().get(&SessionKey::new(AgentKind::ClaudeCode, "c1")).unwrap().clone();
    assert_eq!(s.state, SessionState::AwaitingContinuation);
    assert_eq!(s.reason.as_deref(), Some("history_unknown"));
    // With hooks, Claude Code reports the authoritative pending work.
    assert!(w.run_until(121, 129).is_empty());
    w.hook_at(130, "claude_code", json!({"session_id":"c1","hook_event_name":"UserPromptSubmit"}));
    w.hook_at(
        140,
        "claude_code",
        json!({"session_id":"c1","hook_event_name":"Stop","background_tasks":[],"session_crons":[]}),
    );
    assert!(w.run_until(130, 140).is_empty());
    w.claude(141, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert_eq!(w.run_until(141, 200).len(), 1);
}

#[test]
fn backward_clock_jump_disarms_and_keeps_new_work_visible() {
    let mut w = World::new();
    w.codex_done(-1800);
    assert!(w.run_until(-1800, -1700).is_empty());
    w.monitor.arm(w.at(0));
    assert!(w.run_until(0, 30).is_empty());
    w.codex(-3600, "event_msg", json!({"type":"task_started"}));
    assert!(w.run_until(-3599, -3540).is_empty());
    assert!(!w.monitor.controller().is_armed(), "a backward jump disarms");
    assert_eq!(w.state(AgentKind::Codex, CODEX_ID), SessionState::Running, "new work is not stale");
    // Re-arming after the jump needs a new completion.
    w.monitor.arm(w.at(-3539));
    assert!(w.run_until(-3539, -3400).is_empty());
}

fn subagent_file(w: &World) -> PathBuf {
    let sub = w.root.join(".claude/projects/C--work-web/c1/subagents");
    fs::create_dir_all(&sub).unwrap();
    let f = sub.join("agent-x.jsonl");
    fs::write(&f, b"{}\n").unwrap();
    f
}

fn set_mtime(f: &Path, ms: Millis) {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64);
    fs::OpenOptions::new().write(true).open(f).unwrap().set_modified(t).unwrap();
}

#[test]
fn subagent_write_after_stop_is_never_discarded() {
    for write_at in [14, 16] {
        let mut w = World::new();
        w.register_claude(9001, "c1", "busy");
        let f = subagent_file(&w);
        set_mtime(&f, w.at(-600));
        w.monitor.arm(w.at(0));
        w.claude(1, json!({"type":"user","message":{"content":"go"}}));
        w.claude(2, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
        assert!(w.run_until(0, write_at - 1).is_empty());
        set_mtime(&f, w.at(write_at));
        assert!(w.run_until(write_at, 120).is_empty(), "write at {write_at}s");
        assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::Running);
    }
}

#[test]
fn inferred_end_turn_waits_out_stop_hook_timeout() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.monitor.arm(w.at(0));
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    // CLI without stop hooks: only the final end_turn message, no summary.
    w.claude(
        2,
        json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}),
    );
    assert!(w.run_until(0, 600).is_empty(), "a stop hook may still be running");
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::Finishing);
    let actions = w.run_until(601, 700);
    assert_eq!(actions.len(), 1, "11 min inferred settle + countdown");
}

#[test]
fn confirmed_summary_shortens_inferred_wait() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.monitor.arm(w.at(0));
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.claude(
        2,
        json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}),
    );
    w.claude(3, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert_eq!(w.run_until(0, 40).len(), 1);
}

#[test]
fn cron_delete_releases_and_dead_process_ends_continuation() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.monitor.arm(w.at(0));
    w.claude(1, json!({"type":"user","message":{"content":"loop"}}));
    w.claude(2, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"c","name":"CronCreate","input":{"cron":"*/5 * * * *"}}]}}));
    w.claude(2, json!({"type":"user","toolUseResult":{"id":"job1"},"message":{"content":[{"type":"tool_result","tool_use_id":"c","content":"Scheduled"}]}}));
    w.claude(3, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert!(w.run_until(0, 60).is_empty());
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::AwaitingContinuation);
    // A failed delete and a delete of an unknown job change nothing.
    w.claude(61, json!({"type":"user","message":{"content":"stop the loop"}}));
    w.claude(62, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"d1","name":"CronDelete","input":{"id":"job1"}}]}}));
    w.claude(62, json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"d1","is_error":true,"content":"failed"}]}}));
    w.claude(63, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"d2","name":"CronDelete","input":{"id":"other"}}]}}));
    w.claude(
        63,
        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"d2","content":"ok"}]}}),
    );
    w.claude(64, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert!(w.run_until(61, 120).is_empty());
    // The matching, successful delete releases the session.
    w.claude(121, json!({"type":"user","message":{"content":"really stop"}}));
    w.claude(122, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"d3","name":"CronDelete","input":{"id":"job1"}}]}}));
    w.claude(
        122,
        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"d3","content":"Deleted"}]}}),
    );
    w.claude(123, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert_eq!(w.run_until(121, 160).len(), 1);

    // A session waiting for background work whose process exits ends (non-blocking, no completion).
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.claude(2, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"run_in_background":true}}]}}));
    w.claude(3, json!({"type":"user","toolUseResult":{"backgroundTaskId":"bg1"},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"Command running in background with ID: bg1"}]}}));
    w.claude(4, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    w.run_until(0, 30);
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::AwaitingContinuation);
    w.procs.clear();
    w.monitor.set_fixed_processes(Ok(vec![]));
    fs::remove_file(w.root.join(".claude/sessions/9001.json")).unwrap();
    w.monitor.invalidate();
    w.run_until(31, 70); // two full scans without the process
    let s = w.monitor.tracker().get(&SessionKey::new(AgentKind::ClaudeCode, "c1")).unwrap();
    assert_eq!(s.state, SessionState::Ended);
    assert!(s.ended_unfinished);
    assert!(s.completed_at.is_none());
}

#[test]
fn rejected_spool_record_blocks_hook_tools() {
    let mut w = World::new();
    integration::install(AgentKind::GeminiCli, w.monitor.integration_env()).unwrap();
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    fs::write(w.root.join("data/spool/0000000000001-1-0.json"), b"{broken").unwrap();
    assert!(w.run_until(0, 120).is_empty());
    assert!(
        w.reasons().iter().any(|r| matches!(r, BlockReason::AdapterUnobservable { agent: AgentKind::GeminiCli, .. }))
    );
    assert!(w.root.join("data/spool/rejected/0000000000001-1-0.json").exists());
    // After the hold period the tool is observable again.
    assert_eq!(w.run_until(121, 121 + 5 * 60 + 30).len(), 1);
}

#[test]
fn running_wsl_vm_blocks_unless_disabled() {
    let mut w = World::new();
    w.monitor.set_fixed_wsl(true);
    w.monitor.invalidate();
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    assert!(w.run_until(0, 120).is_empty());
    assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::EnvironmentUnobservable { .. })));
    w.monitor.set_watch_wsl(false);
    assert_eq!(w.run_until(121, 150).len(), 1);
}

#[test]
fn registry_entry_without_tracked_session_vetoes_when_busy() {
    let mut w = World::new();
    w.register_claude(9001, "unknown-session", "busy");
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    assert!(w.run_until(0, 120).is_empty());
    assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 9001, .. })));

    let mut w = World::new();
    w.register_claude(9001, "unknown-session", "idle");
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    assert_eq!(w.run_until(0, 60).len(), 1);
}

#[test]
fn inferred_settle_is_longer_than_claude_stop_hook_timeout() {
    assert!(TrackerConfig::default().inferred_settle > 10 * MINUTE);
}

// ---------------------------------------------------------------------------
// Second review round (multi-agent adversarial verification, 2026-10-29).
// ---------------------------------------------------------------------------

fn claude_bg_session(w: &World) {
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.claude(2, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"run_in_background":true}}]}}));
    w.claude(3, json!({"type":"user","toolUseResult":{"backgroundTaskId":"bg1"},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"Command running in background with ID: bg1"}]}}));
    w.claude(4, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
}

#[test]
fn r2_toggling_claude_or_a_registry_glitch_never_ends_live_background_work() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    claude_bg_session(&w);
    w.run_until(0, 20);
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::AwaitingContinuation);
    w.monitor.set_agent_enabled(AgentKind::ClaudeCode, false);
    w.run_until(21, 60);
    w.monitor.set_agent_enabled(AgentKind::ClaudeCode, true);
    // A half-written registry file during a scan.
    fs::write(w.root.join(".claude/sessions/9001.json"), b"{\"pid\": 90").unwrap();
    w.run_until(61, 100);
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::AwaitingContinuation, "process 9001 is alive");
    w.monitor.arm(w.at(101));
    w.codex_done(102);
    assert!(w.run_until(101, 200).is_empty());
}

#[test]
fn r2_own_stop_hook_record_does_not_cut_the_inferred_window() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.monitor.arm(w.at(0));
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.claude(
        2,
        json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}),
    );
    // Our Stop hook ran; another, slower Stop hook still runs, so no summary yet.
    w.hook_at(
        3,
        "claude_code",
        json!({"session_id":"c1","hook_event_name":"Stop","background_tasks":[],"session_crons":[]}),
    );
    assert!(w.run_until(0, 600).is_empty());
}

#[test]
fn r2_hook_liveness_is_per_process() {
    let mut w = World::new();
    integration::install(AgentKind::GeminiCli, w.monitor.integration_env()).unwrap();
    let start = now_millis() + 10 * SECOND;
    w.add_proc(AgentKind::GeminiCli, 7001, ProcessRole::Session, start, None);
    w.add_proc(AgentKind::GeminiCli, 7002, ProcessRole::Session, start, None);
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    // Only process 7001 runs its hooks.
    w.hook_from(30, 7001, "gemini_cli", json!({"session_id":"g1","hook_event_name":"SessionStart","source":"startup"}));
    assert!(w.run_until(0, 120).is_empty());
    assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 7002, .. })));
    assert!(!w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 7001, .. })));
}

#[test]
fn r2_one_rollout_covers_one_codex_process() {
    let mut w = World::new();
    w.add_proc(AgentKind::Codex, 4001, ProcessRole::Session, w.t0 - 30 * SECOND, Some("/w"));
    w.add_proc(AgentKind::Codex, 4002, ProcessRole::Session, w.t0 - 20 * SECOND, Some("/w"));
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    assert!(w.run_until(0, 120).is_empty());
    let untracked: Vec<u32> = w
        .reasons()
        .iter()
        .filter_map(|r| match r {
            BlockReason::UntrackedProcess { pid, .. } => Some(*pid),
            _ => None,
        })
        .collect();
    assert_eq!(untracked.len(), 1, "{untracked:?}");
}

#[test]
fn r2_running_codex_session_of_a_live_process_never_goes_stale() {
    let settings = Settings { settle_secs: 15, countdown_secs: 10, relevance_hours: 1, ..Settings::default() };
    let mut w = World::new_with(settings);
    w.add_proc(AgentKind::Codex, 4001, ProcessRole::Session, w.t0 - 30 * SECOND, Some("/w"));
    w.codex(1, "session_meta", json!({"id": CODEX_ID, "cwd": "/w"}));
    w.codex(2, "event_msg", json!({"type":"task_started"}));
    w.run_until(0, 30);
    // A long silent tool call: the rollout stays quiet for over an hour.
    w.register_claude(9001, "c1", "busy");
    w.claude(4_000, json!({"type":"user","message":{"content":"go"}}));
    w.claude(4_001, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    w.monitor.arm(w.at(3_990));
    assert!(w.run_until(3_990, 4_100).is_empty());
    assert_eq!(w.state(AgentKind::Codex, CODEX_ID), SessionState::Running);
}

#[test]
fn r2_restored_future_state_after_clock_change_is_clamped() {
    let mut w = World::new();
    w.codex_done(1);
    w.run_until(0, 60);
    let state = w.root.join("data/sessions.json");
    w.monitor.save_state(&state).unwrap();
    // New app run with the clock one hour behind.
    let mut w2 = World::new();
    w2.t0 = w.t0 - 3_600 * SECOND;
    w2.monitor.restore(Monitor::load_state(&state).unwrap());
    w2.monitor.arm(w2.at(0));
    assert!(w2.run_until(0, 60).is_empty(), "the old (future) completion must not count");
    let s = w2.monitor.tracker().get(&SessionKey::new(AgentKind::Codex, CODEX_ID)).unwrap();
    assert!(s.state_since <= w2.at(0));
}

#[test]
fn r2_process_with_foreign_config_home_blocks() {
    let mut w = World::new();
    let mut p = AgentProcess {
        agent: AgentKind::Codex,
        pid: 4100,
        start_time: w.t0 - 30 * SECOND,
        name: "codex".into(),
        role: ProcessRole::Host,
        origin: None,
        cwd: None,
        config_home: Some("/somewhere/else/.codex".into()),
        parent: None,
    };
    w.procs.push(p.clone());
    p.pid = 4101;
    p.agent = AgentKind::ClaudeCode;
    p.role = ProcessRole::Session;
    p.config_home = Some("/other/.claude".into());
    w.procs.push(p);
    w.monitor.set_fixed_processes(Ok(w.procs.clone()));
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    assert!(w.run_until(0, 60).is_empty());
    for pid in [4100u32, 4101] {
        assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: p, .. } if *p == pid)));
    }
}

#[test]
fn r2_live_session_with_old_transcript_is_still_tracked() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "idle");
    claude_bg_session(&w);
    let f = w.root.join(".claude/projects/C--work-web/c1.jsonl");
    set_mtime(&f, w.t0 - 3 * 24 * 3_600 * SECOND);
    w.monitor.arm(w.at(0));
    w.codex_done(5);
    assert!(w.run_until(0, 60).is_empty());
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::AwaitingContinuation);
}

#[test]
fn r2_hook_liveness_survives_restart() {
    let mut w = World::new();
    integration::install(AgentKind::GeminiCli, w.monitor.integration_env()).unwrap();
    w.add_proc(AgentKind::GeminiCli, 7001, ProcessRole::Session, now_millis() - 10 * SECOND, None);
    w.hook_from(1, 7001, "gemini_cli", json!({"session_id":"g1","hook_event_name":"SessionStart","source":"startup"}));
    w.run_until(0, 30);
    let state = w.root.join("data/sessions.json");
    w.monitor.save_state(&state).unwrap();
    let restored = Monitor::load_state(&state).unwrap();
    assert!(restored.proven.iter().any(|(pid, _, agent)| *pid == 7001 && *agent == AgentKind::GeminiCli));
}

#[test]
fn r2_short_sleep_during_countdown_resets_it() {
    let mut w = World::new();
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    assert!(w.run_until(0, 20).is_empty());
    assert!(matches!(w.monitor.controller().phase(), agn_core::countdown::Phase::Countdown { .. }));
    // A 40 s gap — shorter than the old 2 min threshold.
    assert!(w.run_until(60, 120).is_empty());
}

#[test]
fn r2_failed_process_scan_blocks_even_with_codex_only() {
    let mut settings = Settings { settle_secs: 15, countdown_secs: 10, ..Settings::default() };
    for a in [AgentKind::ClaudeCode, AgentKind::GeminiCli, AgentKind::Cursor, AgentKind::OpenCode] {
        settings.agents.insert(a, false);
    }
    let mut w = World::new_with(settings);
    w.monitor.set_fixed_processes(Err("denied".into()));
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    assert!(w.run_until(0, 60).is_empty());
    assert!(
        w.reasons()
            .iter()
            .any(|r| matches!(r, BlockReason::EnvironmentUnobservable { code } if code == "process_scan_failed"))
    );
}

#[test]
fn r2_cancel_after_hand_off_voids_the_action() {
    let mut w = World::new();
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    let mut handed = None;
    for s in 0..60 {
        if let Some(a) = w.monitor.cycle(w.at(s)) {
            handed = Some((s, a));
            break;
        }
    }
    let (s, _) = handed.expect("action handed off");
    // The user clicks Cancel before the host takes the hand-off.
    assert!(w.monitor.cancel_countdown(w.at(s)));
    assert_eq!(w.monitor.take_pending_action(w.at(s)), None);
    assert!(w.monitor.controller().is_armed(), "monitoring stays on");
}

#[test]
fn r2_prune_keeps_sessions_of_live_processes() {
    let mut w = World::new();
    w.add_proc(AgentKind::Codex, 4001, ProcessRole::Session, w.t0 - 30 * SECOND, Some("/w"));
    w.codex_done(1);
    w.run_until(0, 30);
    // Three days later the idle Codex CLI is still open.
    w.run_until(3 * 24 * 3_600, 3 * 24 * 3_600 + 30);
    assert!(w.monitor.tracker().get(&SessionKey::new(AgentKind::Codex, CODEX_ID)).is_some());
    assert!(!w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 4001, .. })));
}

// ---------------------------------------------------------------------------
// Third review round (second adversarial verification, 2026-10-29).
// ---------------------------------------------------------------------------

#[test]
fn r3_claude_hook_running_inside_cursor_does_not_prove_cursor() {
    let mut w = World::new();
    integration::install(AgentKind::Cursor, w.monitor.integration_env()).unwrap();
    w.add_proc(AgentKind::Cursor, 8000, ProcessRole::Editor, now_millis() + 10 * SECOND, None);
    w.register_claude(9001, "c1", "busy");
    w.monitor.arm(w.at(0));
    // Claude Code runs in Cursor's terminal: its hook chain contains the editor.
    let mut rec = SpoolRecord::new("claude_code", json!({"session_id":"c1","hook_event_name":"UserPromptSubmit"}));
    rec.received_at = w.at(20);
    rec.ancestors = vec![9001, 7777, 8000];
    write_record(&w.root.join("data/spool"), &rec).unwrap();
    w.codex_done(1);
    assert!(w.run_until(0, 120).is_empty());
    assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 8000, .. })));
}

#[test]
fn r3_parallel_cron_replace_keeps_the_new_recurring_job() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.monitor.arm(w.at(0));
    w.claude(1, json!({"type":"user","message":{"content":"loop"}}));
    w.claude(2, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"c1","name":"CronCreate","input":{}}]}}));
    w.claude(2, json!({"type":"user","toolUseResult":{"id":"job1"},"message":{"content":[{"type":"tool_result","tool_use_id":"c1","content":"ok"}]}}));
    // One assistant message: delete job1 and create job2 in parallel.
    w.claude(
        10,
        json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"d","name":"CronDelete","input":{"id":"job1"}},
        {"type":"tool_use","id":"c2","name":"CronCreate","input":{}}]}}),
    );
    w.claude(
        11,
        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"d","content":"deleted"}]}}),
    );
    w.claude(12, json!({"type":"user","toolUseResult":{"id":"job2"},"message":{"content":[{"type":"tool_result","tool_use_id":"c2","content":"ok"}]}}));
    w.claude(13, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    w.codex_done(14);
    assert!(w.run_until(17, 200).is_empty());
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::AwaitingContinuation);
}

#[test]
fn r3_hosted_codex_thread_stays_relevant_while_the_host_lives() {
    let settings = Settings { settle_secs: 15, countdown_secs: 10, relevance_hours: 1, ..Settings::default() };
    let mut w = World::new_with(settings);
    w.add_proc(AgentKind::Codex, 4500, ProcessRole::Host, w.t0 - 60 * SECOND, None);
    w.codex(1, "session_meta", json!({"id": CODEX_ID, "cwd": "/w", "originator": "Codex Desktop"}));
    w.codex(2, "event_msg", json!({"type":"task_started"}));
    // Continuous cycles over more than the relevance window, rollout silent.
    let mut s = 0;
    while s < 4_000 {
        w.run_until(s, s + 99);
        s += 100;
    }
    w.register_claude(9001, "c1", "busy");
    w.claude(4_001, json!({"type":"user","message":{"content":"go"}}));
    w.claude(4_002, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    w.monitor.arm(w.at(4_000));
    assert!(w.run_until(4_000, 4_100).is_empty());
    assert!(
        w.reasons()
            .iter()
            .any(|r| matches!(r, BlockReason::SessionActive { key, .. } if key.agent == AgentKind::Codex))
    );
}

#[test]
fn r3_restart_after_clock_change_does_not_replay_old_completions() {
    let mut w = World::new();
    w.codex_done(1);
    w.run_until(0, 60);
    let state = w.root.join("data/sessions.json");
    w.monitor.save_state(&state).unwrap();
    // New run: same files, clock one hour behind; the old lines look "future".
    let mut w2 = World::new();
    std::fs::remove_dir_all(&w2.root).unwrap();
    copy_dir(&w.root, &w2.root);
    w2.t0 = w.t0 - 3_600 * SECOND;
    w2.monitor.restore(Monitor::load_state(&state).unwrap());
    // The first cycle notices the clock went back (monitoring would be
    // switched off); the user arms afterwards.
    w2.run_until(0, 1);
    w2.monitor.arm(w2.at(2));
    assert!(w2.run_until(2, 120).is_empty(), "an old completion is not a new one");
    assert!(w2.reasons().contains(&BlockReason::NoCompletionRecorded), "{:?}", w2.reasons());
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            fs::copy(e.path(), dst).unwrap();
        }
    }
}

#[test]
fn r3_restart_keeps_the_user_ignore() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.claude(2, json!({"type":"user","message":{"content":"[Request interrupted by user]"}}));
    w.run_until(0, 20);
    w.monitor.set_session_ignored(&SessionKey::new(AgentKind::ClaudeCode, "c1"), true);
    let state = w.root.join("data/sessions.json");
    w.monitor.save_state(&state).unwrap();
    // A plain restart: a fresh monitor re-reads the same transcript.
    let paths = MonitorPaths {
        data_dir: w.root.join("data"),
        claude_root: Some(w.root.join(".claude")),
        codex_root: Some(w.root.join(".codex")),
        default_claude_root: None,
        default_codex_root: None,
        integration: w.monitor.integration_env().clone(),
    };
    let settings = Settings { settle_secs: 15, countdown_secs: 10, ..Settings::default() };
    w.monitor = Monitor::new(settings, paths, ProcessSource::Fixed(Ok(w.procs.clone()), false), PowerBackend::DryRun);
    w.monitor.restore(Monitor::load_state(&state).unwrap());
    w.run_until(21, 40);
    assert!(w.monitor.tracker().get(&SessionKey::new(AgentKind::ClaudeCode, "c1")).unwrap().ignored);
}

#[test]
fn r3_host_without_codex_home_writes_to_the_default_not_our_custom_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::create_dir_all(root.join("custom-codex/sessions")).unwrap();
    let env = IntegrationEnv {
        claude_root: None,
        gemini_root: None,
        cursor_root: None,
        opencode_root: None,
        data_dir: root.join("data"),
        spool_dir: root.join("data/spool"),
        hook_source: None,
    };
    let paths = MonitorPaths {
        data_dir: root.join("data"),
        claude_root: None,
        codex_root: Some(root.join("custom-codex")),
        default_claude_root: None,
        default_codex_root: Some(root.join(".codex")),
        integration: env,
    };
    let host = AgentProcess {
        agent: AgentKind::Codex,
        pid: 4600,
        start_time: now_millis() - 60 * SECOND,
        name: "codex".into(),
        role: ProcessRole::Host,
        origin: None,
        cwd: None,
        config_home: None,
        parent: None,
    };
    let mut m =
        Monitor::new(Settings::default(), paths, ProcessSource::Fixed(Ok(vec![host]), false), PowerBackend::DryRun);
    m.arm(now_millis());
    m.cycle(now_millis());
    let Verdict::Blocked { reasons } = m.verdict() else { panic!("must block") };
    assert!(reasons.iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 4600, .. })));
}

#[test]
fn r3_opencode_launcher_child_shares_the_proof() {
    let mut w = World::new();
    integration::install(AgentKind::OpenCode, w.monitor.integration_env()).unwrap();
    let start = now_millis() + 10 * SECOND;
    w.add_proc(AgentKind::OpenCode, 6000, ProcessRole::Session, start, None); // npm launcher
    w.procs.push(AgentProcess {
        agent: AgentKind::OpenCode,
        pid: 6001,
        start_time: start,
        name: "opencode".into(),
        role: ProcessRole::Session,
        origin: None,
        cwd: None,
        config_home: None,
        parent: Some(6000),
    });
    w.monitor.set_fixed_processes(Ok(w.procs.clone()));
    w.monitor.arm(w.at(0));
    w.codex_done(1);
    w.hook_from(30, 6001, "opencode", json!({"event":"plugin.loaded","sessionID":null}));
    assert_eq!(w.run_until(0, 120).len(), 1, "launcher and plugin process are one OpenCode");
}

#[test]
fn r3_pid_reused_after_reboot_is_not_the_session_process() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.run_until(0, 30);
    // Same pid, different process (started later), no registry entry for c1.
    fs::remove_file(w.root.join(".claude/sessions/9001.json")).unwrap();
    w.procs.clear();
    w.add_proc(AgentKind::ClaudeCode, 9001, ProcessRole::Session, w.t0 + 25 * SECOND, None);
    w.run_until(31, 80);
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::Interrupted, "the old process is gone");
}

#[test]
fn r3_hand_off_is_rechecked_after_a_late_command() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.claude(2, json!({"type":"user","message":{"content":"[Request interrupted by user]"}}));
    w.run_until(0, 10);
    w.monitor.set_session_ignored(&SessionKey::new(AgentKind::ClaudeCode, "c1"), true);
    w.monitor.arm(w.at(11));
    w.codex_done(12);
    let mut handed = None;
    for s in 11..100 {
        if w.monitor.cycle(w.at(s)).is_some() {
            handed = Some(s);
            break;
        }
    }
    let s = handed.expect("hand-off");
    // The user un-ignores the interrupted session before the host executes.
    w.monitor.set_session_ignored(&SessionKey::new(AgentKind::ClaudeCode, "c1"), false);
    assert_eq!(w.monitor.take_pending_action(w.at(s)), None);
    assert!(w.monitor.controller().is_armed());
}

#[test]
fn r3_bash_mode_and_clear_do_not_start_turns() {
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.monitor.arm(w.at(0));
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.claude(2, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    w.claude(5, json!({"type":"user","message":{"content":"<bash-input>git status</bash-input>"}}));
    w.claude(
        6,
        json!({"type":"user","message":{"content":"<bash-stdout>clean</bash-stdout><bash-stderr></bash-stderr>"}}),
    );
    assert_eq!(w.run_until(0, 60).len(), 1, "bash mode is not a model turn");
    let mut w = World::new();
    w.register_claude(9001, "c1", "busy");
    w.claude(
        1,
        json!({"type":"user","message":{"content":"<command-name>/clear</command-name><command-args></command-args>"}}),
    );
    w.run_until(0, 5);
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::Idle);
}
