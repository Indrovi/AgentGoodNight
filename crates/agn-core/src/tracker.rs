//! Session tracker: folds normalized events into per-session state.
//!
//! Design rules (see README "Safety model"):
//! * Completion is only ever derived from an explicit end-of-turn event,
//!   followed by a settle window without new activity. Process exits, CPU or
//!   idleness never complete a session.
//! * Events older than the current state's start never change the state, so
//!   slow sources (transcript tailing) cannot revive a finished turn with
//!   lines written before the turn ended.
//! * Anything announcing future work (subagents, background tasks, wake-ups,
//!   cron loops) keeps the session in `AwaitingContinuation`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::event::{AgentEvent, EventKind, FailureKind};
use crate::model::{Session, SessionKey, SessionState};
use crate::time::{MINUTE, Millis, SECOND};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackerConfig {
    /// Quiet period after an end-of-turn event before the session counts as
    /// completed. Absorbs stop-hook continuations (`/goal`), queued prompts
    /// and late log writes. This is a debounce, not a completion estimate.
    pub settle: Millis,
    /// Settle window after an *inferred* end of turn (no confirmation that
    /// the tool's stop hooks finished). Longer than Claude Code's default
    /// stop-hook timeout of 600 s.
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
    /// registry): attaches pid/origin/cwd without touching timestamps. A
    /// registry cwd is the session's project directory and wins.
    pub fn annotate(&mut self, key: &SessionKey, pid: Option<u32>, origin: Option<&str>, cwd: Option<&str>) -> bool {
        let Some(s) = self.sessions.get_mut(key) else { return false };
        let before = (s.pid, s.origin.clone(), s.cwd.clone());
        if pid.is_some() {
            s.pid = pid;
        }
        if s.origin.is_none() {
            s.origin = origin.map(str::to_string);
        }
        if cwd.is_some() {
            s.cwd = cwd.map(str::to_string);
        }
        let changed = before != (s.pid, s.origin.clone(), s.cwd.clone());
        if changed {
            self.revision += 1;
        }
        changed
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
        let tombs = self.tombstones.entry(key.clone()).or_default();
        let s = self.sessions.entry(key.clone()).or_insert_with(|| Session::new(key, ev.at.min(now)));
        let before = s.clone();

        s.sources.insert(ev.source);
        // The first directory seen is the project; later ones are often the
        // agent shell's current directory.
        if s.cwd.is_none() {
            s.cwd = ev.cwd.clone();
        }
        if ev.pid.is_some() {
            s.pid = ev.pid;
        }
        if ev.origin.is_some() {
            s.origin = ev.origin.clone();
        }
        if ev.parent.is_some() {
            s.parent = ev.parent.clone();
        }
        let at = ev.at;
        s.last_activity = s.last_activity.max(at.min(now));
        // A brand-new session has no state to protect yet.
        let fresh = at >= s.state_since || before.state == SessionState::Unknown && before.sources.is_empty();

        match &ev.kind {
            EventKind::SessionStarted { source } => {
                let src = source.as_deref().unwrap_or("");
                if src == "compact" {
                    extend_settle(s, at, settle);
                } else if fresh {
                    if src != "clear" {
                        s.open_subagents.clear();
                        s.background.clear();
                        s.wakeup_scheduled = false;
                        s.recurring = false;
                    }
                    transition(s, SessionState::Idle, at, None);
                }
            }
            EventKind::TurnStarted => {
                if fresh {
                    s.wakeup_scheduled = false;
                    s.ended_unfinished = false;
                    // "Ignore" covers the episode the user looked at, not
                    // future work in the same session.
                    s.ignored = false;
                    transition(s, SessionState::Running, at, None);
                }
            }
            EventKind::Activity { .. } => {
                if fresh {
                    resume(s, at);
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
                    resume(s, at);
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
                release_continuation(s, at.max(s.state_since), settle);
            }
            EventKind::BackgroundStarted { id } => {
                if fresh {
                    resume(s, at);
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
                release_continuation(s, at.max(s.state_since), settle);
            }
            EventKind::ContinuationScheduled { recurring } => {
                if *recurring {
                    s.recurring = true;
                } else {
                    s.wakeup_scheduled = true;
                }
                hold_for_continuation(s, at);
            }
            EventKind::ContinuationCancelled => {
                s.recurring = false;
                release_continuation(s, at.max(s.state_since), settle);
            }
            EventKind::TurnEndInferred => {
                if fresh {
                    s.last_turn_end = Some(at);
                    if s.has_pending_continuation() {
                        let reason = continuation_reason(s);
                        transition(s, SessionState::AwaitingContinuation, at, Some(reason));
                    } else {
                        transition(s, SessionState::Finishing, at, None);
                        s.settle_until = Some(at + inferred_settle);
                    }
                }
            }
            EventKind::GoalStatus { active } => {
                s.goal_active = *active;
                if *active {
                    hold_for_continuation(s, at);
                } else {
                    release_continuation(s, at.max(s.state_since), settle);
                }
            }
            EventKind::TurnCompleted { continuing, pending } => {
                if let Some(p) = pending {
                    if s.pending_snapshot_at.is_none_or(|t| at >= t) {
                        s.background = p.background.iter().filter(|id| !tombs.contains(*id)).cloned().collect();
                        s.wakeup_scheduled = p.wakeup;
                        s.recurring = p.recurring;
                        s.pending_snapshot_at = Some(at);
                        if !fresh {
                            // An older but authoritative snapshot can still
                            // resolve what we were waiting for.
                            release_continuation(s, at.max(s.state_since), settle);
                        }
                    }
                }
                if fresh {
                    s.last_turn_end = Some(at);
                    if s.has_pending_continuation() {
                        let reason = continuation_reason(s);
                        transition(s, SessionState::AwaitingContinuation, at, Some(reason));
                    } else {
                        let window = if *continuing { settle * 2 } else { settle };
                        transition(s, SessionState::Finishing, at, None);
                        s.settle_until = Some(at + window);
                    }
                }
            }
            EventKind::TurnFailed { kind, reason, resets_at } => {
                if fresh {
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
                            s.completed_at = Some(at.min(now));
                        }
                        SessionState::Completed | SessionState::Idle | SessionState::Ended => {}
                        _ => s.ended_unfinished = true,
                    }
                    s.open_subagents.clear();
                    s.background.clear();
                    s.wakeup_scheduled = false;
                    s.recurring = false;
                    s.goal_active = false;
                    transition(s, SessionState::Ended, at, reason.clone());
                }
            }
            EventKind::Unrecognized { note } => {
                if before.sources.is_empty() || s.state == SessionState::Unknown {
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
                s.wakeup_scheduled = false;
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
        if changed {
            self.revision += 1;
        }
        changed
    }

    /// After the wall clock jumped backwards: pulls every timestamp that now
    /// lies in the future back to shortly before `now` (the jump happened
    /// somewhere since the last cycle), so new events are not treated as
    /// stale and old completions cannot count for a later arming.
    pub fn clamp_future(&mut self, now: Millis) -> bool {
        let settle = self.cfg.settle;
        let now = now - CLAMP_SLACK;
        let mut changed = false;
        for s in self.sessions.values_mut() {
            let before = s.clone();
            s.state_since = s.state_since.min(now);
            s.last_activity = s.last_activity.min(now);
            s.first_seen = s.first_seen.min(now);
            s.last_turn_end = s.last_turn_end.map(|t| t.min(now));
            s.pending_snapshot_at = s.pending_snapshot_at.map(|t| t.min(now));
            s.settle_until = s.settle_until.map(|t| t.min(now + settle));
            if s.completed_at.is_some_and(|t| t >= now) {
                s.completed_at = Some(now - 1);
            }
            changed |= *s != before;
        }
        if changed {
            self.revision += 1;
        }
        changed
    }

    /// Drops settled sessions that have been quiet for longer than `max_age`.
    pub fn prune(&mut self, now: Millis, max_age: Millis) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|_, s| !(s.state.is_settled() || s.ignored) || now - s.last_activity < max_age);
        let keep: BTreeSet<SessionKey> = self.sessions.keys().cloned().collect();
        self.tombstones.retain(|k, _| keep.contains(k));
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

/// Strong activity: the session is working again. If it had ended a turn,
/// whatever one-shot wake-up was pending has evidently fired.
fn resume(s: &mut Session, at: Millis) {
    if s.state == SessionState::Running {
        return;
    }
    if matches!(s.state, SessionState::AwaitingContinuation | SessionState::Finishing | SessionState::Completed) {
        s.wakeup_scheduled = false;
    }
    // A session that comes back to life is tracked again even if the user
    // ignored its previous state.
    s.ignored = false;
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
/// window: the tool usually re-invokes the model to report the result.
fn release_continuation(s: &mut Session, at: Millis, settle: Millis) {
    if s.state == SessionState::AwaitingContinuation {
        if s.has_pending_continuation() {
            s.reason = Some(continuation_reason(s));
        } else {
            transition(s, SessionState::Finishing, at, None);
            s.settle_until = Some(at + settle);
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

    #[test]
    fn json_roundtrip_keeps_sessions_and_tombstones() {
        let mut t = Tracker::new(TrackerConfig::default());
        let ev = |kind| AgentEvent::new(AgentKind::ClaudeCode, "s1", 10, kind, EvidenceSource::Simulation);
        t.apply(&ev(EventKind::TurnStarted), 10);
        t.apply(&ev(EventKind::BackgroundFinished { id: "b".into() }), 10);
        t.set_ignored(&SessionKey::new(AgentKind::ClaudeCode, "s1"), true);
        let json = serde_json::to_string(&t).unwrap();
        let back: Tracker = serde_json::from_str(&json).unwrap();
        let s = back.get(&SessionKey::new(AgentKind::ClaudeCode, "s1")).unwrap();
        assert!(s.ignored);
        assert_eq!(s.state, SessionState::Running);
        assert_eq!(back.tombstones.len(), 1);
    }
}
