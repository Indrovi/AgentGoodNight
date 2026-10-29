//! The monitor loop: collects evidence from every adapter, feeds the tracker,
//! evaluates the safety policy and drives the countdown controller.
//!
//! The host (the Tauri app, or a test) calls [`Monitor::cycle`] about once a
//! second. When it returns `Some(action)`, every check passed twice — the
//! second time on a fresh full rescan — and the host hands the action to its
//! `PowerManager`, then reports the result with [`Monitor::record_power`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use agn_core::config::{Settings, write_atomic};
use agn_core::countdown::{Command, Controller, Phase};
use agn_core::event::AgentEvent;
use agn_core::model::{AgentKind, Session, SessionKey, SessionState};
use agn_core::policy::{self, AdapterHealth, PolicyConfig, PolicyInput, UntrackedProcess, Verdict};
use agn_core::power::{PowerAction, PowerBackend, PowerRecord};
use agn_core::spool::{self, SpoolRecord};
use agn_core::time::{HOUR, MINUTE, Millis, SECOND};
use agn_core::tracker::{Tracker, TrackerConfig};
use serde::Serialize;

use crate::claude::{self, ClaudeAdapter, RegistryEntry, origin_from_entrypoint};
use crate::codex::CodexAdapter;
use crate::integration::{self, IntegrationEnv, IntegrationStatus};
use crate::process::{AgentProcess, ProcessRole, ProcessScanner};
use crate::{SourceStatus, cursor, gemini, opencode};

/// Full rescans (directory discovery, processes, integration status).
pub const FULL_INTERVAL: Millis = 15 * SECOND;
/// Sessions shown in the UI even when settled, if active this recently.
const SHOW_RECENT: Millis = 12 * HOUR;
const MAX_SHOWN: usize = 60;
/// A pause between two cycles longer than this means the system slept or
/// the process was suspended.
pub const RESUME_GAP: Millis = 2 * MINUTE;

pub enum ProcessSource {
    System(ProcessScanner),
    /// Fixed list (tests, simulations).
    Fixed(Result<Vec<AgentProcess>, String>),
}

pub struct MonitorPaths {
    pub data_dir: PathBuf,
    pub claude_root: Option<PathBuf>,
    pub codex_root: Option<PathBuf>,
    pub integration: IntegrationEnv,
}

impl MonitorPaths {
    pub fn spool_dir(&self) -> PathBuf {
        self.data_dir.join("spool")
    }
}

pub struct Monitor {
    settings: Settings,
    paths: MonitorPaths,
    tracker: Tracker,
    controller: Controller,
    claude: Option<ClaudeAdapter>,
    codex: Option<CodexAdapter>,
    procs: ProcessSource,
    processes: Vec<AgentProcess>,
    process_error: Option<String>,
    sources: BTreeMap<AgentKind, SourceStatus>,
    spool_error: Option<String>,
    spool_rejected: u64,
    integrations: BTreeMap<AgentKind, IntegrationStatus>,
    registry: Vec<RegistryEntry>,
    ignored_procs: BTreeSet<(u32, Millis)>,
    health: Vec<AdapterHealth>,
    untracked: Vec<UntrackedProcess>,
    verdict: Verdict,
    last_full: Option<Millis>,
    last_cycle: Option<Millis>,
    backend: PowerBackend,
    last_power: Option<PowerRecord>,
    last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionView {
    pub key: String,
    pub agent: AgentKind,
    pub id: String,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub state: SessionState,
    pub reason: Option<String>,
    pub origin: Option<String>,
    pub parent: Option<String>,
    pub last_activity: Millis,
    pub state_since: Millis,
    pub resets_at: Option<Millis>,
    pub blocking: bool,
    pub relevant: bool,
    pub ignored: bool,
    pub ended_unfinished: bool,
    pub pending: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AdapterView {
    pub agent: AgentKind,
    pub name: &'static str,
    pub enabled: bool,
    pub observable: bool,
    pub code: String,
    pub detail: Option<String>,
    pub integration: IntegrationStatus,
    pub processes: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Snapshot {
    pub armed: bool,
    pub phase: Phase,
    pub remaining_ms: Option<Millis>,
    pub countdown_ms: Millis,
    pub action: PowerAction,
    pub backend: PowerBackend,
    pub verdict: Verdict,
    pub sessions: Vec<SessionView>,
    pub adapters: Vec<AdapterView>,
    pub untracked: Vec<UntrackedProcess>,
    pub last_power: Option<PowerRecord>,
    pub last_error: Option<String>,
    pub language: String,
}

impl Monitor {
    pub fn new(settings: Settings, paths: MonitorPaths, procs: ProcessSource, backend: PowerBackend) -> Self {
        let window = settings.relevance_window();
        let claude = paths.claude_root.clone().map(|r| ClaudeAdapter::new(r, window));
        let codex = paths.codex_root.clone().map(|r| CodexAdapter::new(r, window));
        let controller = Controller::new(settings.action, settings.countdown());
        let tracker = Tracker::new(TrackerConfig { settle: settings.settle() });
        Self {
            settings,
            paths,
            tracker,
            controller,
            claude,
            codex,
            procs,
            processes: Vec::new(),
            process_error: None,
            sources: BTreeMap::new(),
            spool_error: None,
            spool_rejected: 0,
            integrations: BTreeMap::new(),
            registry: Vec::new(),
            ignored_procs: BTreeSet::new(),
            health: Vec::new(),
            untracked: Vec::new(),
            verdict: Verdict::Disarmed,
            last_full: None,
            last_cycle: None,
            backend,
            last_power: None,
            last_error: None,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn tracker(&self) -> &Tracker {
        &self.tracker
    }

    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    pub fn controller(&self) -> &Controller {
        &self.controller
    }

    pub fn integration_env(&self) -> &IntegrationEnv {
        &self.paths.integration
    }

    /// Restores sessions persisted by a previous run (ignored flags, states
    /// of hook-only tools whose events were already consumed).
    pub fn restore(&mut self, mut tracker: Tracker) {
        tracker.set_config(TrackerConfig { settle: self.settings.settle() });
        self.tracker = tracker;
    }

    pub fn save_state(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(path, &serde_json::to_vec(&self.tracker)?)
    }

    pub fn load_state(path: &Path) -> Option<Tracker> {
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }

    // ---- user commands -------------------------------------------------

    pub fn arm(&mut self, now: Millis) {
        self.controller.arm(now);
        self.last_error = None;
    }

    pub fn disarm(&mut self) {
        self.controller.disarm();
    }

    pub fn cancel_countdown(&mut self, now: Millis) -> bool {
        self.controller.cancel(now)
    }

    pub fn set_action(&mut self, action: PowerAction, now: Millis) {
        self.settings.action = action;
        self.controller.set_action(action, now);
    }

    pub fn set_language(&mut self, lang: &str) {
        self.settings.language = lang.to_string();
    }

    pub fn set_agent_enabled(&mut self, agent: AgentKind, enabled: bool) {
        self.settings.agents.insert(agent, enabled);
        self.last_full = None;
    }

    pub fn set_session_ignored(&mut self, key: &SessionKey, ignored: bool) -> bool {
        self.tracker.set_ignored(key, ignored)
    }

    pub fn ignore_process(&mut self, pid: u32) -> bool {
        match self.processes.iter().find(|p| p.pid == pid) {
            Some(p) => self.ignored_procs.insert((p.pid, p.start_time)),
            None => false,
        }
    }

    pub fn record_power(&mut self, result: Result<PowerRecord, String>) {
        match result {
            Ok(rec) => {
                self.last_power = Some(rec);
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(e),
        }
    }

    /// Forces the next cycle to do a full rescan (after integration changes).
    pub fn invalidate(&mut self) {
        self.last_full = None;
    }

    pub fn set_fixed_processes(&mut self, procs: Result<Vec<AgentProcess>, String>) {
        self.procs = ProcessSource::Fixed(procs);
    }

    // ---- the loop -------------------------------------------------------

    /// One monitoring step. Returns an action only after the countdown
    /// expired *and* a fresh full rescan still allows it.
    pub fn cycle(&mut self, now: Millis) -> Option<PowerAction> {
        if let Some(prev) = self.last_cycle.replace(now) {
            if now - prev > RESUME_GAP || now < prev {
                // Woke up from sleep (or the clock jumped): never act on a
                // countdown that "expired" while the machine was off.
                if self.controller.rearm_after_gap(now) {
                    tracing::warn!(gap_ms = now - prev, "time gap detected, countdown reset");
                }
                self.last_full = None;
            }
        }
        let full = self.last_full.is_none_or(|t| now - t >= FULL_INTERVAL || now < t);
        self.collect(now, full);
        if self.controller.on_verdict(&self.verdict, now) == Some(Command::Verify) {
            self.collect(now, true);
            if let Some(Command::Execute(action)) = self.controller.on_verified(&self.verdict) {
                return Some(action);
            }
        }
        None
    }

    fn collect(&mut self, now: Millis, full: bool) {
        let mut events = self.drain_spool(now);
        if let Some(c) = self.claude.as_mut().filter(|_| self.settings.agent_enabled(AgentKind::ClaudeCode)) {
            let (evs, st) = c.poll(now, full);
            events.extend(evs);
            self.sources.insert(AgentKind::ClaudeCode, st);
            if full {
                self.registry = c.registry().to_vec();
            }
        }
        if let Some(c) = self.codex.as_mut().filter(|_| self.settings.agent_enabled(AgentKind::Codex)) {
            let (evs, st) = c.poll(now, full);
            events.extend(evs);
            self.sources.insert(AgentKind::Codex, st);
        }
        events.retain(|e| self.settings.agent_enabled(e.agent));
        self.tracker.apply_all(events, now);
        self.tracker.tick(now);
        if full {
            self.last_full = Some(now);
            for agent in AgentKind::ALL {
                self.integrations.insert(agent, integration::status(agent, &self.paths.integration));
            }
            self.scan_processes();
            self.attribute(now);
            spool::sweep_temp(&self.paths.spool_dir(), 10 * MINUTE);
        }
        self.compute_health();
        self.verdict = self.evaluate(now);
    }

    fn drain_spool(&mut self, now: Millis) -> Vec<AgentEvent> {
        let dir = self.paths.spool_dir();
        let batch = match spool::read_batch(&dir) {
            Ok(b) => {
                self.spool_error = None;
                b
            }
            Err(e) => {
                self.spool_error = Some(e.to_string());
                return Vec::new();
            }
        };
        let mut events = Vec::new();
        for entry in batch {
            match entry.record {
                Ok(rec) => events.extend(normalize(&rec)),
                Err(e) => {
                    self.spool_rejected += 1;
                    tracing::warn!(path = %entry.path.display(), error = %e, "rejected spool record");
                }
            }
            if let Err(e) = fs::remove_file(&entry.path) {
                // Leave it; a duplicate replay is harmless (old events are not fresh).
                tracing::warn!(path = %entry.path.display(), error = %e, "could not remove spool record");
            }
        }
        let _ = now;
        events
    }

    fn scan_processes(&mut self) {
        let result = match &mut self.procs {
            ProcessSource::System(s) => s.scan(),
            ProcessSource::Fixed(r) => r.clone(),
        };
        match result {
            Ok(p) => {
                self.processes = p;
                self.process_error = None;
                let live: BTreeSet<(u32, Millis)> = self.processes.iter().map(|p| (p.pid, p.start_time)).collect();
                self.ignored_procs.retain(|k| live.contains(k));
            }
            Err(e) => self.process_error = Some(e),
        }
    }

    /// Links Claude Code processes to sessions via the registry, marks
    /// sessions whose process vanished and collects unattributable processes.
    fn attribute(&mut self, now: Millis) {
        self.untracked.clear();
        let scan_ok = self.process_error.is_none();
        let enabled = |a: AgentKind| self.settings.agent_enabled(a);

        // Claude Code: registry pid -> session.
        let mut claude_pids = BTreeSet::new();
        if enabled(AgentKind::ClaudeCode) {
            for entry in &self.registry {
                let Some(proc_) =
                    self.processes.iter().find(|p| p.pid == entry.pid && p.agent == AgentKind::ClaudeCode)
                else {
                    continue;
                };
                if !start_matches(proc_.start_time, entry.started_at) {
                    continue;
                }
                claude_pids.insert(entry.pid);
                let key = SessionKey::new(AgentKind::ClaudeCode, entry.session_id.clone());
                let origin = entry.entrypoint.as_deref().map(origin_from_entrypoint);
                self.tracker.annotate(&key, Some(entry.pid), origin.as_deref(), entry.cwd.as_deref());
            }
            for p in
                self.processes.iter().filter(|p| p.agent == AgentKind::ClaudeCode && p.role == ProcessRole::Session)
            {
                if !claude_pids.contains(&p.pid) {
                    self.untracked.push(untracked(p, &self.ignored_procs));
                }
            }
        }
        if scan_ok {
            let alive: BTreeSet<u32> = self.processes.iter().map(|p| p.pid).collect();
            let gone: Vec<SessionKey> = self
                .tracker
                .sessions()
                .filter(|s| s.key.agent == AgentKind::ClaudeCode)
                .filter(|s| s.pid.is_some_and(|pid| !alive.contains(&pid) || !claude_pids.contains(&pid)))
                .map(|s| s.key.clone())
                .collect();
            for key in gone {
                // Only meaningful once the registry no longer lists the pid
                // for this session (a restarted desktop session gets a new pid).
                let still_listed = self.registry.iter().any(|r| r.session_id == key.id && claude_pids.contains(&r.pid));
                if !still_listed {
                    self.tracker.process_gone(&key, now);
                }
            }
        }

        // Hook/plugin-only tools: processes that started before the
        // integration was installed cannot have loaded it.
        for agent in [AgentKind::GeminiCli, AgentKind::Cursor, AgentKind::OpenCode] {
            if !enabled(agent) {
                continue;
            }
            let Some(st) = self.integrations.get(&agent).filter(|s| s.installed) else { continue };
            let since = st.installed_at.unwrap_or(0);
            for p in self.processes.iter().filter(|p| p.agent == agent && p.start_time + 5 * SECOND < since) {
                self.untracked.push(untracked(p, &self.ignored_procs));
            }
        }
    }

    fn compute_health(&mut self) {
        self.health.clear();
        for agent in AgentKind::ALL {
            if !self.settings.agent_enabled(agent) {
                self.health.push(AdapterHealth {
                    agent,
                    enabled: false,
                    observable: true,
                    code: "disabled".into(),
                    detail: None,
                });
                continue;
            }
            let procs = self.processes.iter().filter(|p| p.agent == agent).count();
            let installed = self.integrations.get(&agent).is_some_and(|s| s.installed);
            let mut h = match agent {
                AgentKind::ClaudeCode | AgentKind::Codex => {
                    let ok_code = match agent {
                        AgentKind::ClaudeCode if installed => "hooks",
                        AgentKind::ClaudeCode => "transcripts",
                        _ => "rollouts",
                    };
                    let root_known = match agent {
                        AgentKind::ClaudeCode => self.claude.is_some(),
                        _ => self.codex.is_some(),
                    };
                    match self.sources.get(&agent) {
                        _ if !root_known && procs > 0 => AdapterHealth::unobservable(agent, "no_data_dir", None),
                        _ if !root_known => AdapterHealth::ok(agent, "not_detected"),
                        Some(SourceStatus::Ok) => AdapterHealth::ok(agent, ok_code),
                        Some(SourceStatus::Missing) | None if procs == 0 => AdapterHealth::ok(agent, "not_detected"),
                        Some(SourceStatus::Missing) | None => AdapterHealth::unobservable(agent, "no_data_dir", None),
                        Some(SourceStatus::ReadError(e)) => {
                            AdapterHealth::unobservable(agent, "read_error", Some(e.clone()))
                        }
                    }
                }
                _ => {
                    if installed {
                        match &self.spool_error {
                            Some(e) => AdapterHealth::unobservable(agent, "spool_error", Some(e.clone())),
                            None => {
                                AdapterHealth::ok(agent, if agent == AgentKind::OpenCode { "plugin" } else { "hooks" })
                            }
                        }
                    } else if procs > 0 {
                        AdapterHealth::unobservable(agent, "no_integration", None)
                    } else {
                        AdapterHealth::ok(agent, "not_detected")
                    }
                }
            };
            if agent == AgentKind::ClaudeCode && installed && h.observable {
                if let Some(e) = &self.spool_error {
                    h = AdapterHealth::unobservable(agent, "spool_error", Some(e.clone()));
                }
            }
            if agent != AgentKind::Codex && h.observable {
                if let Some(e) = &self.process_error {
                    h = AdapterHealth::unobservable(agent, "process_scan_failed", Some(e.clone()));
                }
            }
            self.health.push(h);
        }
    }

    fn alive_pids(&self) -> BTreeSet<u32> {
        self.processes.iter().map(|p| p.pid).collect()
    }

    fn evaluate(&self, now: Millis) -> Verdict {
        let sessions: Vec<&Session> =
            self.tracker.sessions().filter(|s| self.settings.agent_enabled(s.key.agent)).collect();
        let alive_set = self.alive_pids();
        let alive = |s: &Session| s.pid.is_some_and(|p| alive_set.contains(&p));
        policy::evaluate(
            &PolicyInput {
                now,
                armed_at: self.controller.armed_at(),
                sessions: &sessions,
                health: &self.health,
                untracked: &self.untracked,
                alive: &alive,
            },
            &PolicyConfig { relevance_window: self.settings.relevance_window() },
        )
    }

    pub fn snapshot(&self, now: Millis) -> Snapshot {
        let cfg = PolicyConfig { relevance_window: self.settings.relevance_window() };
        let alive_set = self.alive_pids();
        let mut sessions: Vec<SessionView> = self
            .tracker
            .sessions()
            .filter(|s| self.settings.agent_enabled(s.key.agent))
            .filter_map(|s| {
                let alive = s.pid.is_some_and(|p| alive_set.contains(&p));
                let relevant = policy::is_relevant(s, now, &cfg, alive)
                    || (s.ignored && policy::is_relevant(&Session { ignored: false, ..s.clone() }, now, &cfg, alive));
                let recent = now - s.last_activity <= SHOW_RECENT;
                ((relevant && s.state.is_blocking()) || recent).then(|| view(s, relevant))
            })
            .collect();
        sessions.sort_by(|a, b| {
            let rank = |v: &SessionView| if v.blocking && v.relevant && !v.ignored { 0 } else { 1 };
            rank(a).cmp(&rank(b)).then(b.last_activity.cmp(&a.last_activity))
        });
        sessions.truncate(MAX_SHOWN);
        let adapters = AgentKind::ALL
            .into_iter()
            .map(|agent| {
                let h = self.health.iter().find(|h| h.agent == agent);
                AdapterView {
                    agent,
                    name: agent.display_name(),
                    enabled: self.settings.agent_enabled(agent),
                    observable: h.is_none_or(|h| h.observable),
                    code: h.map_or_else(|| "pending".into(), |h| h.code.clone()),
                    detail: h.and_then(|h| h.detail.clone()),
                    integration: self
                        .integrations
                        .get(&agent)
                        .cloned()
                        .unwrap_or_else(|| integration::status(agent, &self.paths.integration)),
                    processes: self.processes.iter().filter(|p| p.agent == agent).count(),
                }
            })
            .collect();
        Snapshot {
            armed: self.controller.is_armed(),
            phase: self.controller.phase(),
            remaining_ms: self.controller.remaining(now),
            countdown_ms: self.controller.countdown_len(),
            action: self.controller.action(),
            backend: self.backend,
            verdict: self.verdict.clone(),
            sessions,
            adapters,
            untracked: self.untracked.clone(),
            last_power: self.last_power.clone(),
            last_error: self.last_error.clone(),
            language: self.settings.language.clone(),
        }
    }
}

fn view(s: &Session, relevant: bool) -> SessionView {
    let mut pending: Vec<String> = s.background.iter().cloned().collect();
    pending.extend(s.open_subagents.iter().cloned());
    SessionView {
        key: s.key.to_string(),
        agent: s.key.agent,
        id: s.key.id.clone(),
        project: s.project_label(),
        cwd: s.cwd.clone(),
        state: s.state,
        reason: s.reason.clone(),
        origin: s.origin.clone(),
        parent: s.parent.clone(),
        last_activity: s.last_activity,
        state_since: s.state_since,
        resets_at: s.resets_at,
        blocking: s.state.is_blocking(),
        relevant,
        ignored: s.ignored,
        ended_unfinished: s.ended_unfinished,
        pending,
    }
}

fn untracked(p: &AgentProcess, ignored: &BTreeSet<(u32, Millis)>) -> UntrackedProcess {
    UntrackedProcess {
        agent: p.agent,
        pid: p.pid,
        name: p.name.clone(),
        ignored: ignored.contains(&(p.pid, p.start_time)),
    }
}

/// Registry `startedAt` (ms) vs. OS process start (s resolution).
fn start_matches(proc_start: Millis, registry_start: Option<Millis>) -> bool {
    registry_start.is_none_or(|r| (proc_start - r).abs() <= 2 * MINUTE)
}

/// Routes a spool record to its tool's normalizer.
pub fn normalize(rec: &SpoolRecord) -> Vec<AgentEvent> {
    if cursor::looks_like_cursor(&rec.payload) {
        return cursor::hook_events(rec);
    }
    match AgentKind::from_id(&rec.agent) {
        Some(AgentKind::ClaudeCode) => claude::hook_events(rec),
        Some(AgentKind::GeminiCli) => gemini::hook_events(rec),
        Some(AgentKind::Cursor) => cursor::hook_events(rec),
        Some(AgentKind::OpenCode) => opencode::hook_events(rec),
        // Codex is observed through rollouts; no hook records expected.
        Some(AgentKind::Codex) | None => Vec::new(),
    }
}
