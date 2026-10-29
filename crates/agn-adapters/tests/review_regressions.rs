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
            integration: env,
        };
        let monitor = Monitor::new(settings, paths, ProcessSource::Fixed(Ok(vec![]), false), PowerBackend::DryRun);
        Self { _dir: dir, root, t0: now_millis(), monitor, procs: vec![] }
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
        let out = hook::run(Some(agent), &mut input.as_slice(), &self.root.join("data"));
        assert!(out.stored);
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
    w.hook_at(130, "claude_code", json!({"session_id":"c1","hook_event_name":"UserPromptSubmit"}));
    w.hook_at(
        140,
        "claude_code",
        json!({"session_id":"c1","hook_event_name":"Stop","background_tasks":[],"session_crons":[]}),
    );
    assert!(w.run_until(121, 129).is_empty());
    assert_eq!(w.run_until(130, 200).len(), 1);
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
    w.claude(3, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert!(w.run_until(0, 60).is_empty());
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::AwaitingContinuation);
    w.claude(61, json!({"type":"user","message":{"content":"stop the loop"}}));
    w.claude(62, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"d","name":"CronDelete","input":{"id":"x"}}]}}));
    w.claude(63, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert_eq!(w.run_until(61, 100).len(), 1);

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
    w.run_until(31, 40);
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
