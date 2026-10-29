//! Domain model shared by adapters, the tracker, the policy and the UI.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::time::Millis;

/// AI coding tools AgentGoodNight knows about. Each one has its own adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    GeminiCli,
    OpenCode,
    Cursor,
}

impl AgentKind {
    pub const ALL: [AgentKind; 5] =
        [AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::GeminiCli, AgentKind::OpenCode, AgentKind::Cursor];

    /// Stable identifier used on the command line (`--hook <id>`), in spool
    /// files and in the UI.
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude_code",
            AgentKind::Codex => "codex",
            AgentKind::GeminiCli => "gemini_cli",
            AgentKind::OpenCode => "opencode",
            AgentKind::Cursor => "cursor",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "Claude Code",
            AgentKind::Codex => "Codex",
            AgentKind::GeminiCli => "Gemini CLI",
            AgentKind::OpenCode => "OpenCode",
            AgentKind::Cursor => "Cursor",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        AgentKind::ALL.into_iter().find(|k| k.id() == id)
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Lifecycle state of one agent session as far as AgentGoodNight can prove it.
///
/// Only [`SessionState::Completed`], [`SessionState::Idle`] and
/// [`SessionState::Ended`] are non-blocking. Everything else — including
/// `Unknown` — keeps the computer awake.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// A turn/task is in progress.
    Running,
    /// The agent reported the end of a turn; waiting out the settle window in
    /// case a stop hook, `/goal` or a queued message continues the work.
    Finishing,
    /// End of work recorded and the settle window passed without new activity.
    Completed,
    /// The session is open but no task was started yet (or after `/clear`).
    Idle,
    /// The agent asks the user something (permission, question, plan approval).
    WaitingForInput,
    /// The turn ended but background work, subagents or a scheduled wake-up
    /// will continue it.
    AwaitingContinuation,
    /// The agent reported an error that ended the turn.
    Error,
    /// The agent is retrying a dropped connection / stream.
    Reconnecting,
    /// A usage or rate limit stopped the agent until the window resets.
    RateLimited,
    /// The user or the system interrupted the turn.
    Interrupted,
    /// The session itself was closed.
    Ended,
    /// We have evidence the session exists but cannot classify it.
    Unknown,
}

impl SessionState {
    pub fn is_blocking(self) -> bool {
        !matches!(self, SessionState::Completed | SessionState::Idle | SessionState::Ended)
    }

    /// States after which only a new explicit turn start revives the session.
    pub fn is_settled(self) -> bool {
        matches!(self, SessionState::Completed | SessionState::Idle | SessionState::Ended)
    }

    pub fn id(self) -> &'static str {
        match self {
            SessionState::Running => "running",
            SessionState::Finishing => "finishing",
            SessionState::Completed => "completed",
            SessionState::Idle => "idle",
            SessionState::WaitingForInput => "waiting_for_input",
            SessionState::AwaitingContinuation => "awaiting_continuation",
            SessionState::Error => "error",
            SessionState::Reconnecting => "reconnecting",
            SessionState::RateLimited => "rate_limited",
            SessionState::Interrupted => "interrupted",
            SessionState::Ended => "ended",
            SessionState::Unknown => "unknown",
        }
    }
}

/// Where a piece of evidence came from. Hooks and rollout lifecycle events are
/// first-class; transcript parsing and process scans are supporting evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Hook,
    Plugin,
    Rollout,
    Transcript,
    SessionRegistry,
    Process,
    Simulation,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionKey {
    pub agent: AgentKind,
    pub id: String,
}

impl SessionKey {
    pub fn new(agent: AgentKind, id: impl Into<String>) -> Self {
        Self { agent, id: id.into() }
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.agent.id(), self.id)
    }
}

/// One tracked agent session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub key: SessionKey,
    pub state: SessionState,
    /// Machine-readable reason for the current state (`permission_prompt`,
    /// `background_task`, `usage_limit`, ...). Translated by the UI.
    pub reason: Option<String>,
    pub cwd: Option<String>,
    /// `cli`, `desktop`, `vscode`, `exec`, ... when the adapter can tell.
    pub origin: Option<String>,
    /// Parent session for subagents / child threads.
    pub parent: Option<String>,
    pub pid: Option<u32>,
    pub first_seen: Millis,
    pub last_activity: Millis,
    pub state_since: Millis,
    /// Time of the last explicit end-of-turn signal.
    pub last_turn_end: Option<Millis>,
    /// Time the session last reached `Completed` (settle window passed).
    pub completed_at: Option<Millis>,
    /// End of the settle window while `Finishing`.
    pub settle_until: Option<Millis>,
    /// Unix epoch millis when a rate limit resets, if reported.
    pub resets_at: Option<Millis>,
    pub ignored: bool,
    pub sources: BTreeSet<EvidenceSource>,
    /// Subagents started and not yet reported finished.
    pub open_subagents: BTreeSet<String>,
    /// Background tasks (shells, monitors, async agents) not yet finished.
    pub background: BTreeSet<String>,
    /// A one-shot wake-up was scheduled (cleared by the next turn start).
    pub wakeup_scheduled: bool,
    /// Recurring continuation (cron/loop) — sticky until the session ends.
    pub recurring: bool,
    /// An auto-continuing goal is active.
    pub goal_active: bool,
    /// Time of the last authoritative pending-work snapshot; older inferred
    /// background starts are already accounted for by it.
    pub pending_snapshot_at: Option<Millis>,
    /// The session ended while it was still doing work.
    pub ended_unfinished: bool,
}

impl Session {
    pub fn new(key: SessionKey, now: Millis) -> Self {
        Self {
            key,
            state: SessionState::Unknown,
            reason: None,
            cwd: None,
            origin: None,
            parent: None,
            pid: None,
            first_seen: now,
            last_activity: now,
            state_since: now,
            last_turn_end: None,
            completed_at: None,
            settle_until: None,
            resets_at: None,
            ignored: false,
            sources: BTreeSet::new(),
            open_subagents: BTreeSet::new(),
            background: BTreeSet::new(),
            wakeup_scheduled: false,
            recurring: false,
            goal_active: false,
            pending_snapshot_at: None,
            ended_unfinished: false,
        }
    }

    /// True when something the agent itself announced will resume this
    /// session later (background work, subagents, wake-ups, cron loops,
    /// active goals).
    pub fn has_pending_continuation(&self) -> bool {
        !self.open_subagents.is_empty()
            || !self.background.is_empty()
            || self.wakeup_scheduled
            || self.recurring
            || self.goal_active
    }

    /// Short project label (last path component of `cwd`).
    pub fn project_label(&self) -> Option<String> {
        let cwd = self.cwd.as_deref()?;
        let trimmed = cwd.trim_end_matches(['/', '\\']);
        let name = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
        if name.is_empty() { None } else { Some(name.to_string()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_states() {
        use SessionState::*;
        for s in [
            Running,
            Finishing,
            WaitingForInput,
            AwaitingContinuation,
            Error,
            Reconnecting,
            RateLimited,
            Interrupted,
            Unknown,
        ] {
            assert!(s.is_blocking(), "{s:?} must block");
        }
        for s in [Completed, Idle, Ended] {
            assert!(!s.is_blocking(), "{s:?} must not block");
        }
    }

    #[test]
    fn agent_ids_roundtrip() {
        for k in AgentKind::ALL {
            assert_eq!(AgentKind::from_id(k.id()), Some(k));
        }
        assert_eq!(AgentKind::from_id("nope"), None);
    }

    #[test]
    fn project_label() {
        let mut s = Session::new(SessionKey::new(AgentKind::Codex, "x"), 0);
        s.cwd = Some(r"C:\Users\me\Documents\Proj\".into());
        assert_eq!(s.project_label().as_deref(), Some("Proj"));
        s.cwd = Some("/home/me/work/api".into());
        assert_eq!(s.project_label().as_deref(), Some("api"));
        s.cwd = Some("/".into());
        assert_eq!(s.project_label(), None);
    }
}
