//! The monitor loop: collects evidence from every adapter, feeds the tracker,
//! evaluates the safety policy and drives the countdown controller.
//!
//! The host (the Tauri app, or a test) calls [`Monitor::cycle`] about once a
//! second. When the countdown expired and a fresh full rescan still allows
//! the action, the monitor stores it as a pending hand-off; the host takes it
//! with [`Monitor::take_pending_action`] right before calling its
//! `PowerManager` (a cancel in between voids it) and reports the result with
//! [`Monitor::record_power`].

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
use agn_core::tracker::{CLAMP_SLACK, Tracker, TrackerConfig};
use serde::{Deserialize, Serialize};

use crate::claude::{self, ClaudeAdapter, RegistryEntry, origin_from_entrypoint};
use crate::codex::CodexAdapter;
use crate::integration::{self, IntegrationEnv, IntegrationStatus};
use crate::process::{AgentProcess, ProcessRole, ProcessScanner, same_dir};
use crate::{SourceStatus, cursor, gemini, opencode};

/// Full rescans (directory discovery, processes, integration status).
pub const FULL_INTERVAL: Millis = 15 * SECOND;
/// Sessions shown in the UI even when settled, if active this recently.
const SHOW_RECENT: Millis = 12 * HOUR;
const MAX_SHOWN: usize = 60;
/// A pause between two cycles longer than this means the system slept or
/// the process was suspended (the loop runs every 1–2 s; well below the
/// shortest countdown).
pub const RESUME_GAP: Millis = 15 * SECOND;
/// The wall clock going back by more than this disarms monitoring.
pub const CLOCK_BACK_TOLERANCE: Millis = 5 * SECOND;
/// Settled sessions quiet for longer than this are forgotten.
const PRUNE_SETTLED: Millis = 48 * HOUR;
/// Blocking sessions without a live process are forgotten after this.
const PRUNE_BLOCKING: Millis = 14 * 24 * HOUR;
/// A rejected spool record keeps hook-based tools unobservable this long.
const SPOOL_REJECT_HOLD: Millis = 5 * MINUTE;
/// Spool records applied per cycle at most (the rest follows next cycle,
/// while the verdict stays blocked by the backlog).
const SPOOL_CYCLE_CAP: usize = 50_000;
/// Read attempts before an unreadable spool record is set aside.
const SPOOL_MAX_RETRIES: u32 = 3;
/// Full scans a tracked process must be missing before its session counts
/// as gone.
const GONE_STRIKES: u8 = 2;
/// Hook liveness entries kept at most.
const MAX_PROC_SEEN: usize = 10_000;
/// Gap credit added to the relevance window at most.
const MAX_GAP_CREDIT: Millis = 30 * 24 * HOUR;

pub enum ProcessSource {
    System(ProcessScanner),
    /// Fixed list (tests, simulations); the flag simulates a running WSL VM.
    Fixed(Result<Vec<AgentProcess>, String>, bool),
}

pub struct MonitorPaths {
    pub data_dir: PathBuf,
    pub claude_root: Option<PathBuf>,
    pub codex_root: Option<PathBuf>,
    /// Where Claude Code / Codex keep data when no env var says otherwise
    /// (`~/.claude`, `~/.codex`). A process without `CLAUDE_CONFIG_DIR` /
    /// `CODEX_HOME` writes there — which matters if we watch elsewhere.
    pub default_claude_root: Option<PathBuf>,
    pub default_codex_root: Option<PathBuf>,
    pub integration: IntegrationEnv,
}

impl MonitorPaths {
    pub fn spool_dir(&self) -> PathBuf {
        self.data_dir.join("spool")
    }
}

/// What survives an app restart.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub tracker: Tracker,
    #[serde(default)]
    pub last_cycle: Option<Millis>,
    /// Processes (pid, start time, tool) whose integration was proven.
    #[serde(default)]
    pub proven: Vec<(u32, Millis, AgentKind)>,
    /// Where each log reader stopped, so a restart does not replay history.
    #[serde(default)]
    pub claude_tails: Vec<crate::claude::TranscriptSave>,
    #[serde(default)]
    pub codex_tails: Vec<crate::codex::RolloutSave>,
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
    spool_rejected_at: Option<Millis>,
    /// Records applied to the tracker, deleted by `commit_spool` once the
    /// host persisted the state.
    spool_done: Vec<PathBuf>,
    spool_seen: BTreeSet<String>,
    spool_retries: BTreeMap<String, u32>,
    /// More records were waiting than one cycle may apply.
    spool_backlog: bool,
    /// A record could not be read this cycle (later ones were held back).
    spool_pending: bool,
    /// (pid, tool) -> latest hook record of that tool that this process (or
    /// a child) produced.
    proc_seen: BTreeMap<(u32, AgentKind), Millis>,
    /// Processes whose integration demonstrably runs, by identity.
    proven: BTreeSet<(u32, Millis, AgentKind)>,
    /// Latest hook process chain per hook-only session, to link it to its
    /// process once a scan knows that process.
    session_chain: BTreeMap<SessionKey, Vec<u32>>,
    /// `armed_at` of the run that handed off `pending_action`.
    pending_armed_at: Option<Millis>,
    gone_strikes: BTreeMap<SessionKey, u8>,
    wsl_vm: bool,
    environment: Vec<String>,
    integrations: BTreeMap<AgentKind, IntegrationStatus>,
    registry: Vec<RegistryEntry>,
    ignored_procs: BTreeSet<(u32, Millis)>,
    health: Vec<AdapterHealth>,
    untracked: Vec<UntrackedProcess>,
    verdict: Verdict,
    last_full: Option<Millis>,
    last_cycle: Option<Millis>,
    /// Last cycle of the previous run (restored), for clock checks.
    restored_cycle: Option<Millis>,
    /// Time spent asleep/suspended; extends the relevance window so a long
    /// sleep cannot make a paused session look stale.
    gap_credit: Millis,
    pending_action: Option<PowerAction>,
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
    /// Environment codes that block (`wsl_running`, `process_scan_failed`).
    pub environment: Vec<String>,
    pub watch_wsl: bool,
    pub last_power: Option<PowerRecord>,
    pub last_error: Option<String>,
    pub language: String,
}

/// Spool records read in one cycle; committed only after they were applied.
#[derive(Default)]
struct StagedSpool {
    events: Vec<AgentEvent>,
    done: Vec<PathBuf>,
    names: Vec<String>,
    retried: Vec<String>,
    proc_seen: Vec<((u32, AgentKind), Millis)>,
    chains: Vec<(SessionKey, Vec<u32>)>,
}

impl Monitor {
    pub fn new(settings: Settings, paths: MonitorPaths, procs: ProcessSource, backend: PowerBackend) -> Self {
        let window = settings.relevance_window();
        let claude = paths.claude_root.clone().map(|r| ClaudeAdapter::new(r, window));
        let codex = paths.codex_root.clone().map(|r| CodexAdapter::new(r, window));
        let controller = Controller::new(settings.action, settings.countdown());
        let tracker = Tracker::new(TrackerConfig { settle: settings.settle(), ..Default::default() });
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
            spool_rejected_at: None,
            spool_done: Vec::new(),
            spool_seen: BTreeSet::new(),
            spool_retries: BTreeMap::new(),
            spool_backlog: false,
            spool_pending: false,
            proc_seen: BTreeMap::new(),
            proven: BTreeSet::new(),
            session_chain: BTreeMap::new(),
            pending_armed_at: None,
            gone_strikes: BTreeMap::new(),
            wsl_vm: false,
            environment: Vec::new(),
            integrations: BTreeMap::new(),
            registry: Vec::new(),
            ignored_procs: BTreeSet::new(),
            health: Vec::new(),
            untracked: Vec::new(),
            verdict: Verdict::Disarmed,
            last_full: None,
            last_cycle: None,
            restored_cycle: None,
            gap_credit: 0,
            pending_action: None,
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

    /// Restores what a previous run persisted (sessions, ignore flags, hook
    /// liveness proofs). Clock checks against the previous run's last cycle
    /// happen on the first cycle.
    pub fn restore(&mut self, state: PersistedState) {
        let mut tracker = state.tracker;
        tracker.set_config(self.tracker.config());
        // Logs are re-read from their tails: what was already applied must
        // not be applied again.
        tracker.mark_replay_horizon();
        self.tracker = tracker;
        self.proven = state.proven.into_iter().collect();
        self.restored_cycle = state.last_cycle;
        if let Some(c) = self.claude.as_mut() {
            c.import(state.claude_tails);
        }
        if let Some(c) = self.codex.as_mut() {
            c.import(state.codex_tails);
        }
    }

    pub fn save_state(&self, path: &Path) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct Out<'a> {
            tracker: &'a Tracker,
            last_cycle: Option<Millis>,
            proven: Vec<(u32, Millis, AgentKind)>,
            claude_tails: Vec<crate::claude::TranscriptSave>,
            codex_tails: Vec<crate::codex::RolloutSave>,
        }
        let out = Out {
            tracker: &self.tracker,
            last_cycle: self.last_cycle,
            proven: self.proven.iter().copied().collect(),
            claude_tails: self.claude.as_ref().map(|c| c.export()).unwrap_or_default(),
            codex_tails: self.codex.as_ref().map(|c| c.export()).unwrap_or_default(),
        };
        write_atomic(path, &serde_json::to_vec(&out)?)
    }

    /// Reads persisted state (also the older format: a bare tracker).
    pub fn load_state(path: &Path) -> Option<PersistedState> {
        let bytes = fs::read(path).ok()?;
        serde_json::from_slice::<PersistedState>(&bytes).ok().or_else(|| {
            serde_json::from_slice::<Tracker>(&bytes)
                .ok()
                .map(|tracker| PersistedState { tracker, ..Default::default() })
        })
    }

    // ---- user commands -------------------------------------------------

    pub fn arm(&mut self, now: Millis) {
        self.controller.arm(now);
        self.pending_action = None;
        self.last_error = None;
    }

    pub fn disarm(&mut self) {
        self.controller.disarm();
        self.pending_action = None;
    }

    /// Cancels a running countdown, or an action that was already handed
    /// off but not yet taken by the host.
    pub fn cancel_countdown(&mut self, now: Millis) -> bool {
        let handed_off = self.pending_action.take().is_some();
        if handed_off {
            // Monitoring stays on; a new completion is required.
            self.controller.arm(now);
        }
        self.controller.cancel(now) || handed_off
    }

    pub fn set_action(&mut self, action: PowerAction, now: Millis) {
        self.settings.action = action;
        self.controller.set_action(action, now);
        if self.pending_action.is_some() {
            // The user changed what should happen: start over.
            self.pending_action = None;
            self.controller.arm(now);
        }
    }

    pub fn set_language(&mut self, lang: &str) {
        self.settings.language = lang.to_string();
    }

    pub fn set_agent_enabled(&mut self, agent: AgentKind, enabled: bool) {
        self.settings.agents.insert(agent, enabled);
        self.last_full = None;
    }

    pub fn set_watch_wsl(&mut self, watch: bool) {
        self.settings.watch_wsl = watch;
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

    /// The action handed off by the last cycle, if nothing cancelled it.
    /// The host calls this under the lock immediately before executing.
    pub fn take_pending_action(&mut self, now: Millis) -> Option<PowerAction> {
        let action = self.pending_action.take()?;
        let armed_at = self.pending_armed_at.take();
        // Anything the user changed since the final rescan (ignore flags,
        // tools re-enabled, WSL watch) must still be respected.
        self.compute_health(now);
        let verdict = self.evaluate_at(now, armed_at);
        if verdict.is_ready() {
            Some(action)
        } else {
            self.verdict = verdict;
            self.controller.arm(now);
            None
        }
    }

    /// Forces the next cycle to do a full rescan (after integration changes).
    pub fn invalidate(&mut self) {
        self.last_full = None;
    }

    pub fn set_fixed_processes(&mut self, procs: Result<Vec<AgentProcess>, String>) {
        let wsl = matches!(self.procs, ProcessSource::Fixed(_, true));
        self.procs = ProcessSource::Fixed(procs, wsl);
    }

    pub fn set_fixed_wsl(&mut self, running: bool) {
        if let ProcessSource::Fixed(p, _) = &self.procs {
            self.procs = ProcessSource::Fixed(p.clone(), running);
        }
    }

    /// Spool records applied since the last commit. The host persists the
    /// tracker first, then calls [`Monitor::commit_spool`].
    pub fn has_uncommitted_spool(&self) -> bool {
        !self.spool_done.is_empty()
    }

    /// Deletes spool records whose effect is now persisted.
    pub fn commit_spool(&mut self) {
        for path in std::mem::take(&mut self.spool_done) {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            match fs::remove_file(&path) {
                Ok(()) => {
                    self.spool_seen.remove(&name);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.spool_seen.remove(&name);
                }
                // Stays in spool_seen, so it is never applied twice.
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "could not remove spool record"),
            }
        }
    }

    // ---- the loop -------------------------------------------------------

    /// One monitoring step. Returns (and stores as pending hand-off) an
    /// action only after the countdown expired *and* a fresh full rescan
    /// still allows it.
    pub fn cycle(&mut self, now: Millis) -> Option<PowerAction> {
        let live_prev = self.last_cycle.replace(now);
        let prev = live_prev.or(self.restored_cycle.take());
        if let Some(prev) = prev {
            if now + CLOCK_BACK_TOLERANCE < prev {
                self.on_clock_back(now, prev);
            } else if live_prev.is_some() && now - prev > RESUME_GAP {
                self.gap_credit = (self.gap_credit + (now - prev)).min(MAX_GAP_CREDIT);
                // Woke up from sleep: never act on a countdown that "expired"
                // while the machine was off; only new completions count.
                if self.controller.rearm_after_gap(now) {
                    tracing::warn!(gap_ms = now - prev, "time gap detected, countdown reset");
                }
                self.pending_action = None;
                self.last_full = None;
            }
        }
        self.restored_cycle = None;
        let full = self.last_full.is_none_or(|t| now - t >= FULL_INTERVAL || now < t);
        self.collect(now, full);
        if self.controller.on_verdict(&self.verdict, now) == Some(Command::Verify) {
            self.collect(now, true);
            let armed_at = self.controller.armed_at();
            if let Some(Command::Execute(action)) = self.controller.on_verified(&self.verdict) {
                self.pending_action = Some(action);
                self.pending_armed_at = armed_at;
                return Some(action);
            }
        }
        None
    }

    fn on_clock_back(&mut self, now: Millis, prev: Millis) {
        // Every comparison with older timestamps is meaningless now.
        if self.controller.disarm().is_some() {
            self.last_error = Some("clock_changed".into());
        }
        self.pending_action = None;
        self.tracker.clamp_future(now, prev);
        let base = now - CLAMP_SLACK;
        for t in self.proc_seen.values_mut() {
            *t = (*t).min(base);
        }
        // `proven` is by process identity and survives the jump.
        if let Some(t) = self.spool_rejected_at.as_mut() {
            *t = (*t).min(now);
        }
        self.last_full = None;
        tracing::warn!(jump_ms = prev - now, "clock moved backwards, monitoring disarmed");
    }

    fn collect(&mut self, now: Millis, full: bool) {
        let staged = self.drain_spool(now);
        let mut events = staged.events;
        if let Some(c) = self.claude.as_mut().filter(|_| self.settings.agent_enabled(AgentKind::ClaudeCode)) {
            let (evs, st) = c.poll(now, full);
            events.extend(evs);
            self.sources.insert(AgentKind::ClaudeCode, st);
            if full {
                self.registry = c.registry().to_vec();
                let inferred = c.max_stop_hook_timeout().map_or(0, |t| t + MINUTE);
                let cfg = self.tracker.config();
                let wanted = TrackerConfig::default().inferred_settle.max(inferred);
                if cfg.inferred_settle != wanted {
                    self.tracker.set_config(TrackerConfig { inferred_settle: wanted, ..cfg });
                }
            }
        }
        if let Some(c) = self.codex.as_mut().filter(|_| self.settings.agent_enabled(AgentKind::Codex)) {
            let (evs, st) = c.poll(now, full);
            events.extend(evs);
            self.sources.insert(AgentKind::Codex, st);
        }
        // Events of disabled tools are still tracked (only the decision
        // ignores them), so re-enabling a tool does not lose its state.
        self.tracker.apply_all(events, now);
        // Applied: now the records may be considered consumed.
        self.spool_done.extend(staged.done);
        self.spool_seen.extend(staged.names);
        for name in staged.retried {
            self.spool_retries.remove(&name);
        }
        for (key, at) in staged.proc_seen {
            let e = self.proc_seen.entry(key).or_insert(at);
            *e = (*e).max(at);
        }
        for (key, chain) in staged.chains {
            self.session_chain.insert(key, chain);
        }
        if self.proc_seen.len() > MAX_PROC_SEEN {
            let mut v: Vec<((u32, AgentKind), Millis)> = self.proc_seen.iter().map(|(k, v)| (*k, *v)).collect();
            v.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
            v.truncate(MAX_PROC_SEEN / 2);
            self.proc_seen = v.into_iter().collect();
        }
        self.tracker.tick(now);
        if full {
            self.last_full = Some(now);
            for agent in AgentKind::ALL {
                self.integrations.insert(agent, integration::status(agent, &self.paths.integration));
            }
            self.scan_processes();
            self.attribute(now);
            let alive = self.alive_set();
            self.tracker.prune(now, PRUNE_SETTLED, PRUNE_BLOCKING, |s| is_alive(s, &alive));
            spool::sweep_temp(&self.paths.spool_dir(), 10 * MINUTE);
        }
        self.compute_health(now);
        self.verdict = self.evaluate(now);
    }

    /// Reads every waiting hook record (bounded per cycle), oldest first.
    /// Nothing is marked consumed here — see [`Monitor::collect`]. A record
    /// that cannot be read holds back all later ones for this cycle (so an
    /// old end-of-turn can never be applied after a newer turn start), is
    /// retried, and is finally moved to `spool/rejected`.
    fn drain_spool(&mut self, now: Millis) -> StagedSpool {
        let dir = self.paths.spool_dir();
        let mut staged = StagedSpool::default();
        self.spool_backlog = false;
        self.spool_pending = false;
        let mut batch_seen: BTreeSet<String> = BTreeSet::new();
        'outer: loop {
            let seen = &self.spool_seen;
            let local = &batch_seen;
            let batch = match spool::read_batch_filtered(&dir, |n| !seen.contains(n) && !local.contains(n)) {
                Ok(b) => {
                    self.spool_error = None;
                    b
                }
                Err(e) => {
                    self.spool_error = Some(e.to_string());
                    break;
                }
            };
            if batch.is_empty() {
                break;
            }
            for entry in batch {
                let name = entry.path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
                match entry.record {
                    Ok(rec) => {
                        let agent = if cursor::looks_like_cursor(&rec.payload) {
                            Some(AgentKind::Cursor)
                        } else {
                            AgentKind::from_id(&rec.agent)
                        };
                        let chain: Vec<u32> = rec.hook_pid.into_iter().chain(rec.ancestors.iter().copied()).collect();
                        // A record proves only its own tool's integration.
                        if let Some(a) = agent {
                            for pid in &chain {
                                staged.proc_seen.push(((*pid, a), rec.received_at));
                            }
                        }
                        let evs = normalize(&rec);
                        if agent.is_some_and(|a| a != AgentKind::ClaudeCode) {
                            if let Some(e) = evs.first() {
                                staged.chains.push((e.key(), chain.clone()));
                            }
                        }
                        staged.events.extend(evs);
                        staged.retried.push(name.clone());
                        batch_seen.insert(name.clone());
                        staged.names.push(name);
                        staged.done.push(entry.path);
                        if staged.done.len() >= SPOOL_CYCLE_CAP {
                            self.spool_backlog = true;
                            break 'outer;
                        }
                    }
                    Err(e) => {
                        let tries = self.spool_retries.entry(name.clone()).or_insert(0);
                        *tries += 1;
                        if *tries >= SPOOL_MAX_RETRIES {
                            self.spool_retries.remove(&name);
                            self.reject_spool_file(&entry.path, now);
                            tracing::warn!(path = %entry.path.display(), error = %e, "spool record rejected");
                            batch_seen.insert(name);
                            continue;
                        }
                        // Later records wait until this one could be read.
                        self.spool_pending = true;
                        break 'outer;
                    }
                }
            }
        }
        staged
    }

    fn reject_spool_file(&mut self, path: &Path, now: Millis) {
        self.spool_rejected += 1;
        self.spool_rejected_at = Some(now);
        let dir = self.paths.spool_dir().join("rejected");
        let moved =
            fs::create_dir_all(&dir).is_ok() && path.file_name().is_some_and(|n| fs::rename(path, dir.join(n)).is_ok());
        if !moved {
            let _ = fs::remove_file(path);
        }
    }

    fn scan_processes(&mut self) {
        let result = match &mut self.procs {
            ProcessSource::System(s) => {
                let r = s.scan();
                if r.is_ok() {
                    self.wsl_vm = s.wsl_vm();
                }
                r
            }
            ProcessSource::Fixed(r, wsl) => {
                self.wsl_vm = *wsl;
                r.clone()
            }
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

    /// Links processes to sessions, marks sessions whose process vanished
    /// and collects processes we cannot attribute.
    fn attribute(&mut self, now: Millis) {
        self.untracked.clear();
        let scan_ok = self.process_error.is_none();
        let enabled = |a: AgentKind| self.settings.agent_enabled(a);

        // ---- Claude Code: registry pid -> session.
        let claude_root = self.claude.as_ref().map(|c| c.root().to_path_buf());
        if enabled(AgentKind::ClaudeCode) {
            let mut attributed = BTreeSet::new();
            for entry in &self.registry {
                let Some(proc_) =
                    self.processes.iter().find(|p| p.pid == entry.pid && p.agent == AgentKind::ClaudeCode)
                else {
                    continue;
                };
                if !start_matches(proc_.start_time, entry.started_at) {
                    continue;
                }
                let key = SessionKey::new(AgentKind::ClaudeCode, entry.session_id.clone());
                // A registered process is attributed if we track its session,
                // or it is idle and never wrote a transcript.
                let tracked = self.tracker.get(&key).is_some();
                let fresh_idle = entry.status.as_deref() == Some("idle")
                    && !self.claude.as_ref().is_some_and(|c| c.has_transcript(&entry.session_id));
                if tracked || fresh_idle {
                    attributed.insert(entry.pid);
                }
                let origin = entry.entrypoint.as_deref().map(origin_from_entrypoint);
                self.tracker.annotate(
                    &key,
                    Some((entry.pid, proc_.start_time)),
                    origin.as_deref(),
                    entry.cwd.as_deref(),
                );
            }
            for p in
                self.processes.iter().filter(|p| p.agent == AgentKind::ClaudeCode && p.role == ProcessRole::Session)
            {
                let elsewhere = foreign_home(p, claude_root.as_deref(), self.paths.default_claude_root.as_deref());
                if !attributed.contains(&p.pid) || elsewhere {
                    self.untracked.push(untracked(p, &self.ignored_procs));
                }
            }
            // Sessions whose process is gone (two full scans in a row, with
            // a complete registry and process list).
            let registry_ok = self.claude.as_ref().is_some_and(|c| c.registry_ok());
            if scan_ok && registry_ok {
                let claude_alive: Vec<(u32, Millis)> = self
                    .processes
                    .iter()
                    .filter(|p| p.agent == AgentKind::ClaudeCode)
                    .map(|p| (p.pid, p.start_time))
                    .collect();
                let with_pid: Vec<(SessionKey, u32, Option<Millis>)> = self
                    .tracker
                    .sessions()
                    .filter(|s| s.key.agent == AgentKind::ClaudeCode)
                    .filter_map(|s| s.pid.map(|pid| (s.key.clone(), pid, s.pid_start)))
                    .collect();
                for (key, pid, start) in with_pid {
                    let alive = claude_alive.iter().any(|(p, st)| *p == pid && start.is_none_or(|s| s == *st));
                    if alive {
                        self.gone_strikes.remove(&key);
                        continue;
                    }
                    let strikes = self.gone_strikes.entry(key.clone()).or_insert(0);
                    *strikes += 1;
                    if *strikes >= GONE_STRIKES {
                        self.gone_strikes.remove(&key);
                        self.tracker.process_gone(&key, now);
                    }
                }
            }
        }

        // ---- Hook/plugin-only tools: a process counts as observed only if
        // it started after the integration was installed *and* a hook record
        // produced by that very process (or a child) arrived since it started.
        for agent in [AgentKind::GeminiCli, AgentKind::Cursor, AgentKind::OpenCode] {
            if !enabled(agent) {
                continue;
            }
            let Some(st) = self.integrations.get(&agent).filter(|s| s.installed) else { continue };
            let since = st.installed_at.unwrap_or(0);
            let procs: Vec<&AgentProcess> = self.processes.iter().filter(|p| p.agent == agent).collect();
            for p in &procs {
                if self.proc_seen.get(&(p.pid, agent)).is_some_and(|t| *t + 5 * SECOND >= p.start_time) {
                    self.proven.insert((p.pid, p.start_time, agent));
                }
            }
            let is_proven = |p: &AgentProcess| self.proven.contains(&(p.pid, p.start_time, agent));
            for p in &procs {
                let predates_install = p.start_time + 5 * SECOND < since;
                // Launchers and helper children of a proven process of the
                // same tool share its integration.
                let family = procs.iter().any(|q| is_proven(q) && (Some(q.pid) == p.parent || q.parent == Some(p.pid)));
                if predates_install || !(is_proven(p) || family) {
                    self.untracked.push(untracked(p, &self.ignored_procs));
                }
            }
        }
        // Link hook-only sessions to their (single-session CLI) process, so a
        // long silent turn stays relevant while that process lives. Editors
        // and multi-session servers are not linked: a leftover conversation
        // would otherwise block for as long as the editor runs.
        let links: Vec<(SessionKey, (u32, Millis))> = self
            .session_chain
            .iter()
            .filter_map(|(key, chain)| {
                chain.iter().find_map(|pid| {
                    self.processes
                        .iter()
                        .find(|p| {
                            p.pid == *pid
                                && p.agent == key.agent
                                && p.role == ProcessRole::Session
                                && matches!(p.agent, AgentKind::GeminiCli | AgentKind::Cursor)
                        })
                        .map(|p| (key.clone(), (p.pid, p.start_time)))
                })
            })
            .collect();
        for (key, pid) in links {
            self.tracker.annotate(&key, Some(pid), None, None);
        }

        // ---- Codex: every CLI/exec process needs its own tracked rollout
        // (one rollout covers one process), active since the process started,
        // in the same directory when both are known.
        if enabled(AgentKind::Codex) {
            let codex_root = self.codex.as_ref().map(|c| c.root().to_path_buf());
            let mut procs: Vec<&AgentProcess> = self.processes.iter().filter(|p| p.agent == AgentKind::Codex).collect();
            procs.sort_by_key(|p| p.start_time);
            let mut used: BTreeSet<SessionKey> = BTreeSet::new();
            let mut matches: Vec<(SessionKey, (u32, Millis))> = Vec::new();
            for p in procs {
                if foreign_home(p, codex_root.as_deref(), self.paths.default_codex_root.as_deref()) {
                    // Hosts and CLIs alike: data written somewhere we do not watch.
                    self.untracked.push(untracked(p, &self.ignored_procs));
                    continue;
                }
                if p.role != ProcessRole::Session {
                    continue;
                }
                let candidates: Vec<&Session> = self
                    .tracker
                    .sessions()
                    .filter(|s| s.key.agent == AgentKind::Codex && !used.contains(&s.key))
                    .filter(|s| s.last_activity + 5 * SECOND >= p.start_time)
                    .collect();
                let chosen = match &p.cwd {
                    Some(pc) => candidates
                        .iter()
                        .filter(|s| s.cwd.as_deref().is_some_and(|sc| same_dir(pc, sc)))
                        .min_by_key(|s| (s.first_seen - p.start_time).abs())
                        .copied(),
                    // Unknown directory: only an unambiguous match counts.
                    None if candidates.len() == 1 => candidates.first().copied(),
                    None => None,
                };
                match chosen {
                    Some(s) => {
                        used.insert(s.key.clone());
                        matches.push((s.key.clone(), (p.pid, p.start_time)));
                    }
                    None => self.untracked.push(untracked(p, &self.ignored_procs)),
                }
            }
            // Liveness: a thread active since a live Codex process started
            // (its CLI in the same directory, or any app-server/host) stays
            // relevant while that process runs.
            let matched: BTreeSet<SessionKey> = matches.iter().map(|(k, _)| k.clone()).collect();
            let hosted: Vec<(SessionKey, (u32, Millis))> = self
                .tracker
                .sessions()
                .filter(|s| s.key.agent == AgentKind::Codex && !matched.contains(&s.key))
                .filter_map(|s| {
                    self.processes
                        .iter()
                        .filter(|p| p.agent == AgentKind::Codex && p.start_time <= s.last_activity + 5 * SECOND)
                        .filter(|p| match (p.role, &p.cwd, &s.cwd) {
                            (ProcessRole::Session, Some(pc), Some(sc)) => same_dir(pc, sc),
                            (ProcessRole::Session, _, _) => false,
                            _ => true,
                        })
                        .max_by_key(|p| p.start_time)
                        .map(|p| (s.key.clone(), (p.pid, p.start_time)))
                })
                .collect();
            for (key, pid) in matches.into_iter().chain(hosted) {
                self.tracker.annotate(&key, Some(pid), None, None);
            }
        }

        if scan_ok {
            // Proofs live as long as their process.
            let live: BTreeSet<(u32, Millis)> = self.processes.iter().map(|p| (p.pid, p.start_time)).collect();
            self.proven.retain(|(pid, start, _)| live.contains(&(*pid, *start)));
            let pids: BTreeSet<u32> = live.iter().map(|(p, _)| *p).collect();
            let recent = now - 2 * FULL_INTERVAL;
            self.proc_seen.retain(|(pid, _), at| pids.contains(pid) || *at >= recent);
            let keys: BTreeSet<SessionKey> = self.tracker.sessions().map(|s| s.key.clone()).collect();
            self.session_chain.retain(|k, _| keys.contains(k));
        }

        self.environment.clear();
        if self.settings.watch_wsl && self.wsl_vm {
            self.environment.push("wsl_running".into());
        }
        if self.process_error.is_some() {
            // Attribution, WSL detection and hook liveness all depend on it.
            self.environment.push("process_scan_failed".into());
        }
    }

    fn compute_health(&mut self, now: Millis) {
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
                        AdapterHealth::ok(agent, if agent == AgentKind::OpenCode { "plugin" } else { "hooks" })
                    } else if procs > 0 {
                        AdapterHealth::unobservable(agent, "no_integration", None)
                    } else {
                        AdapterHealth::ok(agent, "not_detected")
                    }
                }
            };
            let uses_spool = installed && agent != AgentKind::Codex;
            let recently_rejected = self.spool_rejected_at.is_some_and(|t| now - t < SPOOL_REJECT_HOLD);
            if uses_spool && h.observable {
                if let Some(e) = &self.spool_error {
                    h = AdapterHealth::unobservable(agent, "spool_error", Some(e.clone()));
                } else if recently_rejected {
                    h = AdapterHealth::unobservable(agent, "spool_error", Some("rejected record".into()));
                } else if self.spool_backlog || self.spool_pending {
                    h = AdapterHealth::unobservable(agent, "spool_backlog", None);
                }
            }
            if h.observable {
                if let Some(e) = &self.process_error {
                    h = AdapterHealth::unobservable(agent, "process_scan_failed", Some(e.clone()));
                }
            }
            self.health.push(h);
        }
    }

    /// Live agent processes as (pid, agent) -> start time.
    fn alive_set(&self) -> BTreeMap<(u32, AgentKind), Millis> {
        self.processes.iter().map(|p| ((p.pid, p.agent), p.start_time)).collect()
    }

    fn policy_config(&self) -> PolicyConfig {
        PolicyConfig { relevance_window: self.settings.relevance_window() + self.gap_credit }
    }

    fn evaluate(&self, now: Millis) -> Verdict {
        self.evaluate_at(now, self.controller.armed_at())
    }

    fn evaluate_at(&self, now: Millis, armed_at: Option<Millis>) -> Verdict {
        let sessions: Vec<&Session> =
            self.tracker.sessions().filter(|s| self.settings.agent_enabled(s.key.agent)).collect();
        let alive_set = self.alive_set();
        let alive = |s: &Session| is_alive(s, &alive_set);
        policy::evaluate(
            &PolicyInput {
                now,
                armed_at,
                sessions: &sessions,
                health: &self.health,
                untracked: &self.untracked,
                environment: &self.environment,
                alive: &alive,
            },
            &self.policy_config(),
        )
    }

    pub fn snapshot(&self, now: Millis) -> Snapshot {
        let cfg = self.policy_config();
        let alive_set = self.alive_set();
        let mut sessions: Vec<SessionView> = self
            .tracker
            .sessions()
            .filter(|s| self.settings.agent_enabled(s.key.agent))
            .filter_map(|s| {
                let alive = is_alive(s, &alive_set);
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
            environment: self.environment.clone(),
            watch_wsl: self.settings.watch_wsl,
            last_power: self.last_power.clone(),
            last_error: self.last_error.clone(),
            language: self.settings.language.clone(),
        }
    }
}

/// A session's pid is alive only if a process of the same tool — started
/// at the recorded time — has it.
fn is_alive(s: &Session, alive: &BTreeMap<(u32, AgentKind), Millis>) -> bool {
    let Some(pid) = s.pid else { return false };
    alive.get(&(pid, s.key.agent)).is_some_and(|start| s.pid_start.is_none_or(|st| st == *start))
}

/// The process keeps its data somewhere other than the directory we watch
/// (its env var, or the tool's default when it has none).
fn foreign_home(p: &AgentProcess, watched: Option<&Path>, default: Option<&Path>) -> bool {
    let home = p.config_home.clone().or_else(|| default.map(|d| d.to_string_lossy().into_owned()));
    match (home, watched) {
        (Some(home), Some(root)) => !same_dir(&home, &root.to_string_lossy()),
        (Some(_), None) => true,
        (None, _) => false,
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
