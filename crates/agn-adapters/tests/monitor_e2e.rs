//! End-to-end: real files in temp dirs (transcripts, rollouts, spool,
//! registry), a fixed process list and simulated time. No real agent
//! configuration is read or written and no power action is executed.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use agn_adapters::hook;
use agn_adapters::integration::{self, IntegrationEnv, hook_bin_name};
use agn_adapters::monitor::{Monitor, MonitorPaths, ProcessSource};
use agn_adapters::process::{AgentProcess, ProcessRole};
use agn_core::config::Settings;
use agn_core::model::{AgentKind, SessionKey, SessionState};
use agn_core::policy::{BlockReason, Verdict};
use agn_core::power::{PowerAction, PowerBackend};
use agn_core::time::{Millis, SECOND, now_millis};
use serde_json::{Value, json};

const CODEX_ID: &str = "019a1b2c-3d4e-7f00-8a9b-0c1d2e3f4a5b";

fn iso(ms: Millis) -> String {
    let secs = ms.div_euclid(1000);
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant)
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
    fn new() -> Self {
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
        let settings = Settings { settle_secs: 15, countdown_secs: 10, ..Settings::default() };
        let monitor = Monitor::new(settings, paths, ProcessSource::Fixed(Ok(vec![]), false), PowerBackend::DryRun);
        Self { _dir: dir, root, t0: now_millis(), monitor, procs: vec![], hook_ancestors: vec![] }
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

    fn hook(&self, agent: &str, payload: Value) {
        let mut input = payload.to_string().into_bytes();
        let out = hook::run(Some(agent), &mut input.as_slice(), &self.root.join("data"), self.hook_ancestors.clone());
        assert!(out.stored);
        input.clear();
    }

    fn register_claude(&mut self, pid: u32, session: &str) {
        let started = self.t0 - 60 * SECOND;
        fs::write(
            self.root.join(format!(".claude/sessions/{pid}.json")),
            json!({"pid": pid, "sessionId": session, "cwd": "C:\\work\\web", "startedAt": started, "entrypoint": "cli", "status": "busy"})
                .to_string(),
        )
        .unwrap();
        self.add_proc(AgentKind::ClaudeCode, pid, ProcessRole::Session, started);
    }

    fn add_proc(&mut self, agent: AgentKind, pid: u32, role: ProcessRole, start: Millis) {
        self.procs.push(AgentProcess {
            agent,
            pid,
            start_time: start,
            name: format!("{agent:?}"),
            role,
            origin: None,
            cwd: None,
            config_home: None,
            parent: None,
        });
        self.monitor.set_fixed_processes(Ok(self.procs.clone()));
        self.monitor.invalidate();
    }

    /// Runs cycles every second up to `sec`; returns executed actions.
    fn run_until(&mut self, from: i64, to: i64) -> Vec<PowerAction> {
        let mut out = Vec::new();
        for s in from..=to {
            let action = self.monitor.cycle(self.at(s));
            self.monitor.commit_spool();
            if let Some(a) = action {
                self.monitor.record_power(Ok(agn_core::power::PowerRecord {
                    at: self.at(s),
                    action: a,
                    backend: PowerBackend::DryRun,
                }));
                out.push(a);
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

#[test]
fn claude_and_codex_overnight_then_exactly_one_action() {
    let mut w = World::new();
    w.register_claude(9001, "c1");
    w.monitor.arm(w.at(0));

    w.claude(1, json!({"type":"user","turnOrigin":"human","message":{"content":"refactor"}}));
    w.codex(
        1,
        "session_meta",
        json!({"id": CODEX_ID, "cwd": "/work/api", "originator": "codex_cli_rs", "source": "cli"}),
    );
    w.codex(2, "event_msg", json!({"type":"task_started"}));
    assert!(w.run_until(0, 5).is_empty());
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::Running);
    assert_eq!(w.state(AgentKind::Codex, CODEX_ID), SessionState::Running);
    let pid = w.monitor.tracker().get(&SessionKey::new(AgentKind::ClaudeCode, "c1")).unwrap().pid;
    assert_eq!(pid, Some(9001), "registry links the process");

    assert!(w.run_until(6, 59).is_empty());
    w.codex(60, "event_msg", json!({"type":"task_complete","error":null}));
    assert!(w.run_until(60, 100).is_empty(), "claude still running");
    assert_eq!(w.state(AgentKind::Codex, CODEX_ID), SessionState::Completed);

    // Background bash launched, then the turn ends: must keep waiting.
    w.claude(101, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"run_in_background":true}}]}}));
    w.claude(102, json!({"type":"user","toolUseResult":{"backgroundTaskId":"bg1"},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"Command running in background with ID: bg1"}]}}));
    w.claude(103, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert!(w.run_until(101, 400).is_empty());
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::AwaitingContinuation);

    // Notification arrives, Claude reports and stops.
    w.claude(401, json!({"type":"user","turnOrigin":"task_notification","message":{"content":"<task-notification><task-id>bg1</task-id><status>completed</status></task-notification>"}}));
    assert!(w.run_until(401, 409).is_empty());
    w.claude(410, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    let actions = w.run_until(410, 470);
    assert_eq!(actions, vec![PowerAction::Sleep], "settle 15 s + countdown 10 s");
    assert!(!w.monitor.controller().is_armed());
    assert!(w.run_until(471, 700).is_empty(), "never repeats");
}

#[test]
fn untracked_claude_process_blocks_until_ignored() {
    let mut w = World::new();
    w.register_claude(9001, "c1");
    w.add_proc(AgentKind::ClaudeCode, 9002, ProcessRole::Session, w.t0); // no registry entry
    w.monitor.arm(w.at(0));
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.claude(5, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert!(w.run_until(0, 120).is_empty());
    assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 9002, .. })));
    assert!(w.monitor.ignore_process(9002));
    assert_eq!(w.run_until(121, 150), vec![PowerAction::Sleep]);
}

#[test]
fn gemini_without_integration_blocks_until_disabled() {
    let mut w = World::new();
    w.add_proc(AgentKind::GeminiCli, 7000, ProcessRole::Session, w.t0);
    w.monitor.arm(w.at(0));
    w.codex(1, "session_meta", json!({"id": CODEX_ID, "cwd": "/w"}));
    w.codex(2, "event_msg", json!({"type":"task_started"}));
    w.codex(3, "event_msg", json!({"type":"task_complete"}));
    assert!(w.run_until(0, 120).is_empty());
    assert!(
        w.reasons().iter().any(|r| matches!(r, BlockReason::AdapterUnobservable { agent: AgentKind::GeminiCli, .. }))
    );
    w.monitor.set_agent_enabled(AgentKind::GeminiCli, false);
    assert_eq!(w.run_until(121, 150), vec![PowerAction::Sleep]);
}

#[test]
fn cursor_with_hooks_and_a_pre_install_process() {
    let mut w = World::new();
    // Cursor editor started before the integration existed.
    w.add_proc(AgentKind::Cursor, 5000, ProcessRole::Editor, w.t0 - 3_600 * SECOND);
    integration::install(AgentKind::Cursor, w.monitor.integration_env()).unwrap();
    w.monitor.invalidate();
    w.monitor.arm(w.at(0));
    w.hook("cursor", json!({"conversation_id":"k1","generation_id":"g","hook_event_name":"beforeSubmitPrompt","cursor_version":"2.1","workspace_roots":["/w/site"]}));
    w.hook("cursor", json!({"conversation_id":"k1","generation_id":"g","hook_event_name":"stop","status":"completed","loop_count":0,"cursor_version":"2.1"}));
    assert!(w.run_until(0, 120).is_empty());
    assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 5000, .. })), "restart needed");
    // User restarts Cursor: the old process is gone, a new one started after install.
    w.procs.clear();
    w.add_proc(AgentKind::Cursor, 5001, ProcessRole::Editor, now_millis() + 10 * SECOND);
    assert!(w.run_until(121, 140).is_empty(), "no hook record since the restart: integration unproven");
    assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::UntrackedProcess { pid: 5001, .. })));
    // The restarted editor runs its hooks (workspaceOpen) — now it is observed.
    std::thread::sleep(std::time::Duration::from_millis(20));
    w.procs.clear();
    w.add_proc(AgentKind::Cursor, 5001, ProcessRole::Editor, now_millis());
    std::thread::sleep(std::time::Duration::from_millis(10));
    w.hook_ancestors = vec![9999, 5001]; // shell, then the Cursor process
    w.hook(
        "cursor",
        json!({"conversation_id":"","generation_id":"","hook_event_name":"workspaceOpen","cursor_version":"2.1"}),
    );
    let actions = w.run_until(141, 180);
    assert_eq!(actions, vec![PowerAction::Sleep]);
    let snap = w.monitor.snapshot(w.at(180));
    let k1 = snap.sessions.iter().find(|s| s.id == "k1").unwrap();
    assert_eq!(k1.project.as_deref(), Some("site"));
}

#[test]
fn claude_hooks_report_rate_limit_then_resume() {
    let mut w = World::new();
    w.register_claude(9001, "c1");
    w.monitor.arm(w.at(0));
    w.hook("claude_code", json!({"session_id":"c1","hook_event_name":"UserPromptSubmit","prompt":"secret"}));
    w.hook("claude_code", json!({"session_id":"c1","hook_event_name":"StopFailure","error":"rate_limit"}));
    assert!(w.run_until(0, 300).is_empty());
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::RateLimited);
    // Auto-continue after the reset, then a clean stop with no pending work.
    std::thread::sleep(std::time::Duration::from_millis(5));
    w.hook("claude_code", json!({"session_id":"c1","hook_event_name":"UserPromptSubmit"}));
    w.hook("claude_code", json!({"session_id":"c1","hook_event_name":"Stop","background_tasks":[],"session_crons":[]}));
    // The hook Stop alone is unconfirmed (other Stop hooks may still run)…
    assert!(w.run_until(301, 340).is_empty());
    // …until Claude Code writes stop_hook_summary after all Stop hooks.
    w.claude(341, json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":false}));
    assert_eq!(w.run_until(341, 380), vec![PowerAction::Sleep]);
    assert!(fs::read_dir(w.root.join("data/spool")).unwrap().next().is_none(), "spool drained");
}

#[test]
fn vanished_claude_process_mid_run_never_completes() {
    let mut w = World::new();
    w.register_claude(9001, "c1");
    w.monitor.arm(w.at(0));
    w.claude(1, json!({"type":"user","message":{"content":"go"}}));
    w.codex(1, "session_meta", json!({"id": CODEX_ID, "cwd": "/w"}));
    w.codex(2, "event_msg", json!({"type":"task_started"}));
    w.codex(3, "event_msg", json!({"type":"task_complete"}));
    assert!(w.run_until(0, 30).is_empty());
    // Crash: process and registry gone, no stop recorded.
    w.procs.clear();
    w.monitor.set_fixed_processes(Ok(vec![]));
    fs::remove_file(w.root.join(".claude/sessions/9001.json")).unwrap();
    w.monitor.invalidate();
    assert!(w.run_until(31, 400).is_empty());
    assert_eq!(w.state(AgentKind::ClaudeCode, "c1"), SessionState::Interrupted);
}

#[test]
fn process_scan_failure_blocks() {
    let mut w = World::new();
    w.monitor.arm(w.at(0));
    w.codex(1, "session_meta", json!({"id": CODEX_ID, "cwd": "/w"}));
    w.codex(2, "event_msg", json!({"type":"task_started"}));
    w.codex(3, "event_msg", json!({"type":"task_complete"}));
    w.monitor.set_fixed_processes(Err("access denied".into()));
    w.monitor.invalidate();
    assert!(w.run_until(0, 200).is_empty());
    assert!(w.reasons().iter().any(|r| matches!(r, BlockReason::AdapterUnobservable { .. })));
}

#[test]
fn wake_from_sleep_during_countdown_never_acts_immediately() {
    let mut w = World::new();
    w.monitor.arm(w.at(0));
    w.codex(1, "session_meta", json!({"id": CODEX_ID, "cwd": "/w"}));
    w.codex(2, "event_msg", json!({"type":"task_started"}));
    w.codex(3, "event_msg", json!({"type":"task_complete"}));
    assert!(w.run_until(0, 20).is_empty());
    assert!(matches!(w.monitor.controller().phase(), agn_core::countdown::Phase::Countdown { .. }));
    // The machine slept for three hours (e.g. the user put it to sleep).
    assert!(w.run_until(3 * 3600, 3 * 3600 + 120).is_empty(), "old completion must not count after resume");
    assert!(w.monitor.controller().is_armed());
    assert!(w.reasons().contains(&BlockReason::NoCompletionRecorded));
}
