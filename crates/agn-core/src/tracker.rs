//! Session tracker: folds normalized events into per-session state.
//!
//! Design rules (see README "How it decides"):
//! * Completion is only ever derived from an explicit end-of-turn event,
//!   followed by a settle window without new activity. Process exits, CPU or
//!   idleness never complete a session.
//! * Events older than the current state's start never change the state, so
//!   slow sources (transcript tailing) cannot revive a finished turn with
//!   lines written before the turn ended. End-of-turn evidence older than the
//!   latest progress belongs to a previous turn and is ignored too.
//! * Event times are capped at "now"; a completion is as old as its evidence.
//! * Anything announcing future work (subagents, background tasks, wake-ups,
//!   cron loops, goals) keeps the session in `AwaitingContinuation`.
//! * A settle window is never shortened, except by confirmed evidence.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::event::{AgentEvent, EventKind, FailureKind, PendingWork};
use crate::model::{Session, SessionKey, SessionState, Wakeup};
use crate::time::{MINUTE, Millis, SECOND};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackerConfig {
    /// Quiet period after a confirmed end-of-turn event before the session
    /// counts as completed. Absorbs stop-hook continuations (`/goal`),
    /// queued prompts and late log writes. A debounce, not an estimate.
    pub settle: Millis,
    /// Settle window after an *inferred* end of turn (no confirmation that
    /// the tool's stop hooks finished). Longer than Claude Code's default
    /// stop-hook timeout of 600 s; raised when longer hooks are configured.
    #[serde(default = "default_inferred_settle")]
    pub inferred_settle: Millis,
}

fn default_inferred_settle() -> Millis {
    11 * MINUTE
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self { settle: 90 * SECOND, inferred_settle: default_inferred_settle() }
    }
}

/// Pseudo background id: the session's log was first read from the middle,
/// so work announced before that point is unknown. Only an authoritative
/// pending-work snapshot (Claude Code Stop hook) clears it.
pub const HISTORY_UNKNOWN: &str = "history:unknown";
/// Pseudo background id for a pending-work snapshot that was truncated.
pub const SNAPSHOT_TRUNCATED: &str = "snapshot:truncated";
/// How far before "now" timestamps are clamped after a backward clock jump.
pub const CLAMP_SLACK: Millis = 2 * MINUTE;
/// A turn that starts this close before a wake-up's due time counts as the
/// wake-up firing.
pub const WAKEUP_TOLERANCE: Millis = MINUTE;
/// Evidence dated further in the future than this cannot complete anything
/// (it comes from a clock that has since been turned back).
pub const FUTURE_TOLERANCE: Millis = 5 * SECOND;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Tracker {
    cfg: TrackerConfig,
    #[serde(with = "as_pairs")]
    sessions: BTreeMap<SessionKey, Session>,
    /// Finished subagent/background ids, so out-of-order delivery of a
    /// "finished" before its "started" cannot leave a phantom entry behind.
    #[serde(default, with = "as_pairs")]
    tombstones: BTreeMap<SessionKey, BTreeSet<String>>,
    #[serde(skip)]
    revision: u64,
}

impl Tracker {
    pub fn new(cfg: TrackerConfig) -> Self {
        Self { cfg, ..Default::default() }
    }

    pub fn config(&self) -> TrackerConfig {
        self.cfg
    }

    pub fn set_config(&mut self, cfg: TrackerConfig) {
        self.cfg = cfg;
    }

    /// Increments whenever any session changed; cheap change detection for
    /// the UI and persistence.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    pub fn get(&self, key: &SessionKey) -> Option<&Session> {
        self.sessions.get(key)
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn set_ignored(&mut self, key: &SessionKey, ignored: bool) -> bool {
        match self.sessions.get_mut(key) {
            Some(s) if s.ignored != ignored => {
                s.ignored = ignored;
                self.revision += 1;
                true
            }
            _ => false,
        }
    }

    /// Metadata from sources that prove nothing about activity (process
    /// registry, process matching): attaches pid/origin/cwd without touching
    /// timestamps. A registry cwd is the session's project directory.
    pub fn annotate(
        &mut self,
        key: &SessionKey,
        pid: Option<(u32, Millis)>,
        origin: Option<&str>,
        cwd: Option<&str>,
    ) -> bool {
        let Some(s) = self.sessions.get_mut(key) else { return false };
        let before = (s.pid, s.pid_start, s.origin.clone(), s.cwd.clone());
        if let Some((pid, start)) = pid {
            s.pid = Some(pid);
            s.pid_start = Some(start);
        }
        if s.origin.is_none() {
            s.origin = origin.map(str::to_string);
        }
        if cwd.is_some() {
            s.cwd = cwd.map(str::to_string);
        }
        let changed = before != (s.pid, s.pid_start, s.origin.clone(), s.cwd.clone());
        if changed {
            self.revision += 1;
        }
        changed
    }

    /// After restoring persisted state: everything up to each session's
    /// last activity was already applied, so re-reading those log lines
    /// must not change anything (no replayed completions, no un-ignoring).
    pub fn mark_replay_horizon(&mut self) {
        for s in self.sessions.values_mut() {
            s.replay_until = s.replay_until.max(s.last_activity);
        }
    }

    /// Applies a batch of events in chronological order.
    pub fn apply_all(&mut self, mut events: Vec<AgentEvent>, now: Millis) -> bool {
        events.sort_by_key(|e| e.at);
        let mut changed = false;
        for ev in &events {
            changed |= self.apply(ev, now);
        }
        changed
    }

    pub fn apply(&mut self, ev: &AgentEvent, now: Millis) -> bool {
        let key = ev.key();
        let settle = self.cfg.settle;
        let inferred_settle = self.cfg.inferred_settle.max(settle);
        // Evidence from "the future" (clock skew, a clock that jumped back)
        // is treated as happening now.
        let at = ev.at.min(now);
        let tombs = self.tombstones.entry(key.clone()).or_default();
        let s = self.sessions.entry(key.clone()).or_insert_with(|| Session::new(key, at));
        let before = s.clone();
        let is_new = before.sources.is_empty();

        s.sources.insert(ev.source);
        // The first directory seen is the project; later ones are often the
        // agent shell's current directory.
        if s.cwd.is_none() {
            s.cwd = ev.cwd.clone();
        }
        if ev.pid.is_some() {
            s.pid = ev.pid;
            s.pid_start = None;
        }
        if ev.origin.is_some() {
            s.origin = ev.origin.clone();
        }
        if ev.parent.is_some() {
            s.parent = ev.parent.clone();
        }
        s.last_activity = s.last_activity.max(at);
        // Already applied in a previous run (logs re-read after a restart).
        if s.replay_until > 0 && ev.at <= s.replay_until {
            let changed = *s != before;
            if changed {
                self.revision += 1;
            }
            return changed;
        }
        // A brand-new session has no state to protect yet.
        let fresh = at >= s.state_since || (before.state == SessionState::Unknown && is_new);
        // An end of turn older than the latest progress belongs to an
        // earlier turn (e.g. a delayed record); one from the future (a clock
        // turned back) cannot complete anything.
        let end_fresh = fresh && at >= s.last_progress && ev.at <= now + FUTURE_TOLERANCE;

        match &ev.kind {
            EventKind::SessionStarted { source } => {
                let src = source.as_deref().unwrap_or("");
                if src == "compact" {
                    extend_settle(s, at, settle);
                } else if fresh {
                    if src != "clear" {
                        s.open_subagents.clear();
                        s.background.clear();
                        s.wakeups.clear();
                        s.recurring = false;
                    }
                    transition(s, SessionState::Idle, at, None);
                }
            }
            EventKind::TurnStarted => {
                if fresh {
                    progress(s, at);
                    retire_due_wakeups(s, at);
                    s.ended_unfinished = false;
                    // "Ignore" covers the episode the user looked at, not
                    // future work — but a re-delivered start of the same
                    // turn is not new work.
                    if before.state != SessionState::Running || at > before.state_since {
                        s.ignored = false;
                    }
                    transition(s, SessionState::Running, at, None);
                }
            }
            EventKind::Activity { .. } => {
                if fresh {
                    progress(s, at);
                    resume(s, at, at > before.state_since);
                }
            }
            EventKind::Heartbeat => {
                extend_settle(s, at, settle);
            }
            EventKind::WaitingForInput { reason } => {
                if fresh {
                    transition(s, SessionState::WaitingForInput, at, reason.clone());
                }
            }
            EventKind::SubagentStarted { id } => {
                if fresh {
                    progress(s, at);
                    resume(s, at, at > before.state_since);
                }
                if !tombs.contains(id) {
                    s.open_subagents.insert(id.clone());
                }
                if !fresh {
                    hold_for_continuation(s, at);
                }
            }
            EventKind::SubagentFinished { id } => {
                s.open_subagents.remove(id);
                tombs.insert(id.clone());
                release_continuation_with(s, at.max(s.state_since), settle, inferred_settle);
            }
            EventKind::BackgroundStarted { id } => {
                if fresh {
                    progress(s, at);
                    resume(s, at, at > before.state_since);
                }
                let covered = s.pending_snapshot_at.is_some_and(|t| at < t);
                if !tombs.contains(id) && !covered {
                    s.background.insert(id.clone());
                }
                if !fresh {
                    hold_for_continuation(s, at);
                }
            }
            EventKind::BackgroundFinished { id } => {
                s.background.remove(id);
                tombs.insert(id.clone());
                release_continuation_with(s, at.max(s.state_since), settle, inferred_settle);
            }
            EventKind::HistoryUnknown => {
                // Only unknown if we have not already observed the session up
                // to this point (e.g. state restored from a previous run).
                let observed = !is_new && before.last_activity >= at;
                let covered = s.pending_snapshot_at.is_some_and(|t| at < t);
                if !observed && !covered {
                    s.background.insert(HISTORY_UNKNOWN.into());
                    hold_for_continuation(s, at);
                }
            }
            EventKind::ContinuationScheduled { recurring, due } => {
                if *recurring {
                    s.recurring = true;
                    hold_for_continuation(s, at);
                } else if !(due.is_some_and(|d| d + WAKEUP_TOLERANCE <= s.last_progress) && !fresh) {
                    // (a replayed wake-up that demonstrably fired already is skipped)
                    s.wakeups.push(Wakeup { due: *due, scheduled_at: at, id: None });
                    hold_for_continuation(s, at);
                }
            }
            EventKind::OneShotScheduled { id } => {
                if !s.wakeups.iter().any(|w| w.id.as_deref() == Some(id.as_str())) {
                    s.wakeups.push(Wakeup { due: None, scheduled_at: at, id: Some(id.clone()) });
                }
                hold_for_continuation(s, at);
            }
            EventKind::OneShotCancelled { id } => {
                s.wakeups.retain(|w| w.id.as_deref() != Some(id.as_str()));
                release_continuation_with(s, at.max(s.state_since), settle, inferred_settle);
            }
            EventKind::ContinuationFired => {
                fire_one_wakeup(s, at);
            }
            EventKind::ContinuationCancelled => {
                s.recurring = false;
                release_continuation_with(s, at.max(s.state_since), settle, inferred_settle);
            }
            EventKind::TurnEndInferred { pending } => {
                if let Some(p) = pending {
                    apply_snapshot(s, tombs, p, at, fresh, settle, inferred_settle);
                }
                if end_fresh && !keeps_failure(s) {
                    s.last_turn_end = Some(at);
                    s.last_end_inferred = true;
                    if s.has_pending_continuation() {
                        let reason = continuation_reason(s);
                        transition(s, SessionState::AwaitingContinuation, at, Some(reason));
                    } else {
                        let until = at + inferred_settle;
                        let keep = (s.state == SessionState::Finishing).then_some(s.settle_until).flatten();
                        transition(s, SessionState::Finishing, at, None);
                        // Never shortens a running window.
                        s.settle_until = Some(keep.map_or(until, |k| k.max(until)));
                    }
                }
            }
            EventKind::GoalStatus { active } => {
                s.goal_active = *active;
                if *active {
                    hold_for_continuation(s, at);
                } else {
                    release_continuation_with(s, at.max(s.state_since), settle, inferred_settle);
                }
            }
            EventKind::TurnCompleted { continuing, pending } => {
                if let Some(p) = pending {
                    apply_snapshot(s, tombs, p, at, fresh, settle, inferred_settle);
                }
                if end_fresh && !keeps_failure(s) {
                    s.last_turn_end = Some(at);
                    s.last_end_inferred = false;
                    if s.has_pending_continuation() {
                        let reason = continuation_reason(s);
                        transition(s, SessionState::AwaitingContinuation, at, Some(reason));
                    } else {
                        // Confirmed end: may replace a longer inferred window.
                        let window = if *continuing { settle * 2 } else { settle };
                        transition(s, SessionState::Finishing, at, None);
                        s.settle_until = Some(at + window);
                    }
                }
            }
            EventKind::TurnFailed { kind, reason, resets_at } => {
                if end_fresh {
                    let state = match kind {
                        FailureKind::Error => SessionState::Error,
                        FailureKind::RateLimited => SessionState::RateLimited,
                        FailureKind::Reconnecting => SessionState::Reconnecting,
                        FailureKind::Interrupted => SessionState::Interrupted,
                    };
                    transition(s, state, at, reason.clone());
                    s.resets_at = *resets_at;
                }
            }
            EventKind::SessionEnded { reason } => {
                if fresh {
                    match s.state {
                        SessionState::Finishing => {
                            s.completed_at = Some(at);
                        }
                        SessionState::Completed | SessionState::Idle | SessionState::Ended => {}
                        _ => s.ended_unfinished = true,
                    }
                    s.open_subagents.clear();
                    s.background.clear();
                    s.wakeups.clear();
                    s.recurring = false;
                    s.goal_active = false;
                    transition(s, SessionState::Ended, at, reason.clone());
                }
            }
            EventKind::Unrecognized { note } => {
                if is_new || s.state == SessionState::Unknown {
                    transition(s, SessionState::Unknown, at, Some(note.clone()));
                } else {
                    extend_settle(s, at, settle);
                }
            }
        }

        let changed = *s != before;
        if changed {
            self.revision += 1;
        }
        changed
    }

    /// Advances time-based transitions (settle window expiry).
    pub fn tick(&mut self, now: Millis) -> bool {
        let mut changed = false;
        for s in self.sessions.values_mut() {
            let Some(until) = s.settle_until.filter(|t| *t <= now) else { continue };
            if s.state != SessionState::Finishing {
                continue;
            }
            // state_since stays at the end-of-turn evidence: anything
            // timestamped after it — even if delivered late — still revives
            // the session.
            let since = s.state_since;
            if s.has_pending_continuation() {
                let reason = continuation_reason(s);
                transition(s, SessionState::AwaitingContinuation, since, Some(reason));
            } else {
                transition(s, SessionState::Completed, since, None);
                // A completion is as old as its evidence, never "now": an old
                // turn first seen later must not satisfy "completed after
                // arming".
                s.completed_at = Some(until);
            }
            changed = true;
        }
        if changed {
            self.revision += 1;
        }
        changed
    }

    /// Called when the process that owned a session disappeared without a
    /// session-end event. A finished turn becomes `Ended`; anything still
    /// active becomes `Interrupted` (blocking) — a vanished process is never
    /// treated as successful completion.
    pub fn process_gone(&mut self, key: &SessionKey, now: Millis) -> bool {
        let Some(s) = self.sessions.get_mut(key) else { return false };
        let changed = match s.state {
            SessionState::Finishing => {
                s.completed_at = Some(s.last_turn_end.unwrap_or(now).min(now));
                transition(s, SessionState::Ended, now, Some("process_exited".into()));
                true
            }
            SessionState::Completed | SessionState::Idle | SessionState::Ended => false,
            SessionState::Interrupted if s.reason.as_deref() == Some("process_exited") => false,
            // The turn had ended and only background work was pending; it died
            // with the process. Nothing can continue, but nothing completed.
            SessionState::AwaitingContinuation => {
                s.ended_unfinished = true;
                s.background.clear();
                s.open_subagents.clear();
                s.wakeups.clear();
                s.recurring = false;
                s.goal_active = false;
                transition(s, SessionState::Ended, now, Some("process_exited".into()));
                true
            }
            _ => {
                s.ended_unfinished = true;
                transition(s, SessionState::Interrupted, now, Some("process_exited".into()));
                true
            }
        };
        // The pid may be reused by an unrelated process.
        s.pid = None;
        s.pid_start = None;
        if changed {
            self.revision += 1;
        }
        changed
    }

    /// After the wall clock jumped backwards from `prev` to `now`: pulls
    /// every timestamp that now lies in the future back to shortly before
    /// `now`, keeps the remaining part of settle windows and due times, so
    /// new events are not treated as stale, no window completes early and
    /// old completions cannot count for a later arming.
    pub fn clamp_future(&mut self, now: Millis, prev: Millis) -> bool {
        let settle = self.cfg.settle;
        let base = now - CLAMP_SLACK;
        let shift = |t: Millis| if t > base { now + (t - prev).max(0) } else { t };
        let mut changed = false;
        for s in self.sessions.values_mut() {
            let before = s.clone();
            s.state_since = s.state_since.min(base);
            s.last_activity = s.last_activity.min(base);
            s.last_progress = s.last_progress.min(base);
            s.first_seen = s.first_seen.min(base);
            s.last_turn_end = s.last_turn_end.map(|t| t.min(base));
            s.pending_snapshot_at = s.pending_snapshot_at.map(|t| t.min(base));
            s.settle_until = s.settle_until.map(|t| if t > base { now + (t - prev).max(settle) } else { t });
            for w in &mut s.wakeups {
                w.due = w.due.map(shift);
                w.scheduled_at = w.scheduled_at.min(base);
            }
            // Timestamps no longer tell old from new evidence.
            s.replay_until = 0;
            s.resets_at = s.resets_at.map(shift);
            if s.completed_at.is_some_and(|t| t >= base) {
                s.completed_at = Some(base - 1);
            }
            changed |= *s != before;
        }
        if changed {
            self.revision += 1;
        }
        changed
    }

    /// Forgets sessions nobody needs: settled ones quiet for `settled_age`,
    /// blocking ones quiet for `blocking_age` — except those `keep` vetoes
    /// (e.g. sessions whose process is alive).
    pub fn prune(
        &mut self,
        now: Millis,
        settled_age: Millis,
        blocking_age: Millis,
        keep: impl Fn(&Session) -> bool,
    ) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|_, s| {
            let age = now - s.last_activity;
            let limit = if s.state.is_settled() || s.ignored { settled_age } else { blocking_age };
            age < limit || keep(s)
        });
        let keys: BTreeSet<SessionKey> = self.sessions.keys().cloned().collect();
        self.tombstones.retain(|k, _| keys.contains(k));
        let removed = before - self.sessions.len();
        if removed > 0 {
            self.revision += 1;
        }
        removed
    }
}

/// JSON object keys must be strings; struct-keyed maps are stored as pairs.
mod as_pairs {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer, K: Serialize, V: Serialize>(m: &BTreeMap<K, V>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(m.iter())
    }

    pub fn deserialize<'de, D, K, V>(d: D) -> Result<BTreeMap<K, V>, D::Error>
    where
        D: Deserializer<'de>,
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
    {
        Ok(Vec::<(K, V)>::deserialize(d)?.into_iter().collect())
    }
}

fn transition(s: &mut Session, state: SessionState, at: Millis, reason: Option<String>) {
    if s.state != state {
        s.state = state;
        s.state_since = at.max(s.state_since);
    }
    s.reason = reason;
    if state != SessionState::Finishing {
        s.settle_until = None;
    }
    if state != SessionState::RateLimited {
        s.resets_at = None;
    }
}

fn progress(s: &mut Session, at: Millis) {
    s.last_progress = s.last_progress.max(at);
}

/// A new turn starting after a wake-up's due time — and after the turn that
/// scheduled it ended — is that wake-up firing. Activity never retires one.
fn retire_due_wakeups(s: &mut Session, at: Millis) {
    let ended_since = |w: &Wakeup| s.last_turn_end.is_some_and(|e| e >= w.scheduled_at);
    let fired: Vec<usize> = s
        .wakeups
        .iter()
        .enumerate()
        .filter(|(_, w)| at > w.scheduled_at && ended_since(w) && w.due.is_some_and(|d| at + WAKEUP_TOLERANCE >= d))
        .map(|(i, _)| i)
        .collect();
    // One turn fires one wake-up (the earliest due).
    if let Some(i) = fired.into_iter().min_by_key(|i| s.wakeups[*i].due) {
        s.wakeups.remove(i);
    }
}

/// A scheduler-started turn retires exactly one pending wake-up: the
/// earliest one already due, else one with an unknown due time.
fn fire_one_wakeup(s: &mut Session, at: Millis) {
    let due_now = s
        .wakeups
        .iter()
        .enumerate()
        .filter(|(_, w)| w.due.is_some_and(|d| d <= at + WAKEUP_TOLERANCE))
        .min_by_key(|(_, w)| w.due)
        .map(|(i, _)| i);
    let pick = due_now.or_else(|| s.wakeups.iter().position(|w| w.due.is_none()));
    if let Some(i) = pick {
        s.wakeups.remove(i);
    }
}

/// An end-of-turn without any progress since a failure is the end of the
/// failed turn, not a success.
fn keeps_failure(s: &Session) -> bool {
    matches!(s.state, SessionState::Error | SessionState::Interrupted) && s.last_progress <= s.state_since
}

/// Applies an authoritative pending-work snapshot (newest wins).
fn apply_snapshot(
    s: &mut Session,
    tombs: &BTreeSet<String>,
    p: &PendingWork,
    at: Millis,
    fresh: bool,
    settle: Millis,
    inferred_settle: Millis,
) {
    if s.pending_snapshot_at.is_some_and(|t| at < t) {
        return;
    }
    s.background = p.background.iter().filter(|id| !tombs.contains(*id)).cloned().collect();
    if !p.wakeup {
        s.wakeups.clear();
    } else if s.wakeups.is_empty() {
        s.wakeups.push(Wakeup { due: None, scheduled_at: at, id: None });
    }
    s.recurring = p.recurring;
    s.pending_snapshot_at = Some(at);
    if !fresh {
        // An older but authoritative snapshot can still resolve what we
        // were waiting for — or reveal work we did not know about.
        hold_for_continuation(s, at);
        release_continuation_with(s, at.max(s.state_since), settle, inferred_settle);
    }
}

/// Strong activity: the session is working again.
fn resume(s: &mut Session, at: Millis, newer: bool) {
    if s.state == SessionState::Running {
        return;
    }
    // A session that comes back to life is tracked again even if the user
    // ignored its previous state (not for evidence tied with that state).
    if newer {
        s.ignored = false;
    }
    transition(s, SessionState::Running, at, None);
}

fn extend_settle(s: &mut Session, at: Millis, settle: Millis) {
    if s.state == SessionState::Finishing {
        let until = at + settle;
        if s.settle_until.is_none_or(|t| t < until) {
            s.settle_until = Some(until);
        }
    }
}

/// Work announced before the turn ended keeps a finishing/finished session
/// from being treated as done.
fn hold_for_continuation(s: &mut Session, at: Millis) {
    if matches!(s.state, SessionState::Finishing | SessionState::Completed) && s.has_pending_continuation() {
        let reason = continuation_reason(s);
        let since = s.state_since.max(at);
        transition(s, SessionState::AwaitingContinuation, since, Some(reason));
        s.completed_at = None;
    } else if s.state == SessionState::AwaitingContinuation {
        s.reason = Some(continuation_reason(s));
    }
}

/// When the last pending item finishes, the session gets a fresh settle
/// window: the tool usually re-invokes the model to report the result. If
/// the turn's end was only inferred, the long window applies.
fn release_continuation_with(s: &mut Session, at: Millis, settle: Millis, inferred_settle: Millis) {
    if s.state == SessionState::AwaitingContinuation {
        if s.has_pending_continuation() {
            s.reason = Some(continuation_reason(s));
        } else {
            let window = if s.last_end_inferred { inferred_settle } else { settle };
            transition(s, SessionState::Finishing, at, None);
            s.settle_until = Some(at + window);
        }
    }
}

fn continuation_reason(s: &Session) -> String {
    if s.recurring {
        "recurring_task".into()
    } else if s.goal_active {
        "goal".into()
    } else if !s.open_subagents.is_empty() {
        "subagents".into()
    } else if s.background.contains(HISTORY_UNKNOWN) {
        "history_unknown".into()
    } else if !s.background.is_empty() {
        "background_task".into()
    } else {
        "scheduled_wakeup".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, EvidenceSource};

    fn ev(at: Millis, kind: EventKind) -> AgentEvent {
        AgentEvent::new(AgentKind::ClaudeCode, "s1", at, kind, EvidenceSource::Simulation)
    }

    fn key() -> SessionKey {
        SessionKey::new(AgentKind::ClaudeCode, "s1")
    }

    #[test]
    fn json_roundtrip_keeps_sessions_and_tombstones() {
        let mut t = Tracker::new(TrackerConfig::default());
        t.apply(&ev(10, EventKind::TurnStarted), 10);
        t.apply(&ev(10, EventKind::BackgroundFinished { id: "b".into() }), 10);
        t.set_ignored(&key(), true);
        let json = serde_json::to_string(&t).unwrap();
        let back: Tracker = serde_json::from_str(&json).unwrap();
        let s = back.get(&key()).unwrap();
        assert!(s.ignored);
        assert_eq!(s.state, SessionState::Running);
        assert_eq!(back.tombstones.len(), 1);
    }

    #[test]
    fn inferred_window_is_never_shortened_by_an_unconfirmed_end() {
        let mut t = Tracker::new(TrackerConfig { settle: 90 * SECOND, ..Default::default() });
        t.apply(&ev(0, EventKind::TurnStarted), 0);
        t.apply(&ev(1_000, EventKind::TurnEndInferred { pending: None }), 1_000);
        let long = t.get(&key()).unwrap().settle_until.unwrap();
        t.apply(&ev(2_000, EventKind::TurnEndInferred { pending: Some(PendingWork::default()) }), 2_000);
        assert!(t.get(&key()).unwrap().settle_until.unwrap() >= long);
        t.apply(&ev(3_000, EventKind::TurnCompleted { continuing: false, pending: None }), 3_000);
        assert_eq!(t.get(&key()).unwrap().settle_until, Some(93_000), "confirmation shortens");
    }

    #[test]
    fn stale_end_of_turn_does_not_end_a_newer_turn() {
        let mut t = Tracker::new(TrackerConfig::default());
        t.apply(&ev(0, EventKind::TurnStarted), 0);
        t.apply(&ev(5_000, EventKind::Activity { detail: None }), 5_000);
        // A delayed Stop record from before the latest progress.
        t.apply(&ev(4_000, EventKind::TurnCompleted { continuing: false, pending: None }), 6_000);
        assert_eq!(t.get(&key()).unwrap().state, SessionState::Running);
    }

    #[test]
    fn wakeup_stays_pending_until_due() {
        let mut t = Tracker::new(TrackerConfig { settle: 1_000, ..Default::default() });
        t.apply(&ev(0, EventKind::TurnStarted), 0);
        t.apply(&ev(1_000, EventKind::ContinuationScheduled { recurring: false, due: Some(600_000) }), 1_000);
        t.apply(&ev(1_500, EventKind::Activity { detail: None }), 1_500);
        t.apply(&ev(2_000, EventKind::TurnCompleted { continuing: false, pending: None }), 2_000);
        t.apply(&ev(10_000, EventKind::TurnStarted), 10_000);
        t.apply(&ev(11_000, EventKind::TurnCompleted { continuing: false, pending: None }), 11_000);
        t.tick(20_000);
        assert_eq!(t.get(&key()).unwrap().state, SessionState::AwaitingContinuation, "not fired yet");
        t.apply(&ev(600_500, EventKind::TurnStarted), 600_500);
        t.apply(&ev(601_000, EventKind::TurnCompleted { continuing: false, pending: None }), 601_000);
        t.tick(700_000);
        assert_eq!(t.get(&key()).unwrap().state, SessionState::Completed);
    }

    #[test]
    fn unknown_due_needs_a_fired_signal() {
        let mut t = Tracker::new(TrackerConfig { settle: 1_000, ..Default::default() });
        t.apply(&ev(0, EventKind::ContinuationScheduled { recurring: false, due: None }), 0);
        t.apply(&ev(5_000, EventKind::TurnStarted), 5_000);
        t.apply(&ev(6_000, EventKind::TurnCompleted { continuing: false, pending: None }), 6_000);
        assert_eq!(t.get(&key()).unwrap().state, SessionState::AwaitingContinuation);
        t.apply(&ev(7_000, EventKind::ContinuationFired), 7_000);
        t.apply(&ev(7_000, EventKind::TurnStarted), 7_000);
        t.apply(&ev(8_000, EventKind::TurnCompleted { continuing: false, pending: None }), 8_000);
        t.tick(20_000);
        assert_eq!(t.get(&key()).unwrap().state, SessionState::Completed);
    }

    #[test]
    fn idle_after_an_error_is_not_success() {
        let mut t = Tracker::new(TrackerConfig { settle: 1_000, ..Default::default() });
        t.apply(&ev(0, EventKind::TurnStarted), 0);
        t.apply(&ev(1_000, EventKind::TurnFailed { kind: FailureKind::Error, reason: None, resets_at: None }), 1_000);
        t.apply(&ev(1_100, EventKind::TurnCompleted { continuing: false, pending: None }), 1_100);
        t.tick(10_000);
        let s = t.get(&key()).unwrap();
        assert_eq!(s.state, SessionState::Error);
        assert!(s.completed_at.is_none());
    }

    #[test]
    fn redelivered_turn_start_keeps_ignore() {
        let mut t = Tracker::new(TrackerConfig::default());
        t.apply(&ev(100, EventKind::TurnStarted), 100);
        t.set_ignored(&key(), true);
        t.apply(&ev(100, EventKind::TurnStarted), 200);
        assert!(t.get(&key()).unwrap().ignored);
        t.apply(&ev(300, EventKind::TurnStarted), 300);
        assert!(!t.get(&key()).unwrap().ignored, "a newer turn is new work");
    }

    #[test]
    fn history_marker_skipped_for_observed_sessions() {
        let mut t = Tracker::new(TrackerConfig::default());
        t.apply(&ev(1_000, EventKind::HistoryUnknown), 1_000);
        assert!(t.get(&key()).unwrap().background.contains(HISTORY_UNKNOWN), "new session: unknown");
        let mut t = Tracker::new(TrackerConfig::default());
        t.apply(&ev(5_000, EventKind::TurnStarted), 5_000);
        t.apply(&ev(4_000, EventKind::HistoryUnknown), 6_000);
        assert!(!t.get(&key()).unwrap().background.contains(HISTORY_UNKNOWN), "already observed past that point");
    }

    #[test]
    fn clamp_keeps_remaining_settle() {
        let mut t = Tracker::new(TrackerConfig { settle: 90 * SECOND, ..Default::default() });
        t.apply(&ev(1_000_000, EventKind::TurnStarted), 1_000_000);
        t.apply(&ev(1_000_000, EventKind::TurnEndInferred { pending: None }), 1_000_000);
        // Clock steps back by an hour right after.
        t.clamp_future(1_000_000 - 3_600_000, 1_000_000);
        t.tick(1_000_000 - 3_600_000 + 5 * SECOND);
        assert_eq!(t.get(&key()).unwrap().state, SessionState::Finishing, "window not cut short");
    }

    #[test]
    fn one_fired_turn_retires_one_wakeup() {
        let mut t = Tracker::new(TrackerConfig { settle: 1_000, ..Default::default() });
        t.apply(&ev(0, EventKind::TurnStarted), 0);
        t.apply(&ev(1, EventKind::OneShotScheduled { id: "cron5am".into() }), 1);
        t.apply(&ev(2, EventKind::ContinuationScheduled { recurring: false, due: Some(600_000) }), 2);
        t.apply(&ev(3, EventKind::TurnCompleted { continuing: false, pending: None }), 3);
        t.apply(&ev(603_000, EventKind::ContinuationFired), 603_000);
        t.apply(&ev(603_000, EventKind::TurnStarted), 603_000);
        t.apply(&ev(604_000, EventKind::TurnCompleted { continuing: false, pending: None }), 604_000);
        t.tick(700_000);
        let s = t.get(&key()).unwrap();
        assert_eq!(s.state, SessionState::AwaitingContinuation, "the 5 am job is still pending");
        assert_eq!(s.wakeups.len(), 1);
        assert_eq!(s.wakeups[0].id.as_deref(), Some("cron5am"));
    }

    #[test]
    fn release_after_an_inferred_end_uses_the_long_window() {
        let mut t = Tracker::new(TrackerConfig { settle: 90 * SECOND, ..Default::default() });
        t.apply(&ev(0, EventKind::TurnStarted), 0);
        t.apply(&ev(1_000, EventKind::GoalStatus { active: true }), 1_000);
        t.apply(&ev(2_000, EventKind::TurnEndInferred { pending: None }), 2_000);
        t.apply(&ev(3_000, EventKind::GoalStatus { active: false }), 3_000);
        let s = t.get(&key()).unwrap();
        assert_eq!(s.state, SessionState::Finishing);
        assert!(s.settle_until.unwrap() >= 3_000 + 10 * MINUTE);
    }

    #[test]
    fn late_snapshot_with_running_work_re_holds_a_completed_session() {
        let mut t = Tracker::new(TrackerConfig { settle: 1_000, ..Default::default() });
        t.apply(&ev(0, EventKind::TurnStarted), 0);
        t.apply(&ev(5_000, EventKind::TurnCompleted { continuing: false, pending: None }), 5_000);
        t.tick(10_000);
        assert_eq!(t.get(&key()).unwrap().state, SessionState::Completed);
        let pending = PendingWork { background: vec!["bg".into()], ..Default::default() };
        t.apply(&ev(4_000, EventKind::TurnCompleted { continuing: false, pending: Some(pending) }), 11_000);
        let s = t.get(&key()).unwrap();
        assert_eq!(s.state, SessionState::AwaitingContinuation);
        assert!(s.completed_at.is_none());
    }

    #[test]
    fn future_events_are_capped_at_now() {
        let mut t = Tracker::new(TrackerConfig::default());
        t.apply(&ev(9_000_000, EventKind::TurnStarted), 1_000);
        assert_eq!(t.get(&key()).unwrap().state_since, 1_000);
    }
}
