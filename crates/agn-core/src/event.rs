//! Normalized lifecycle events. Every adapter translates its tool-specific
//! signals (hook payloads, rollout lines, plugin events) into these.

use serde::{Deserialize, Serialize};

use crate::model::{AgentKind, EvidenceSource, SessionKey};
use crate::time::Millis;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    pub agent: AgentKind,
    pub session_id: String,
    /// When the event happened according to its source.
    pub at: Millis,
    pub kind: EventKind,
    pub source: EvidenceSource,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
}

impl AgentEvent {
    pub fn new(
        agent: AgentKind,
        session_id: impl Into<String>,
        at: Millis,
        kind: EventKind,
        source: EvidenceSource,
    ) -> Self {
        Self {
            agent,
            session_id: session_id.into(),
            at,
            kind,
            source,
            cwd: None,
            pid: None,
            origin: None,
            parent: None,
        }
    }

    pub fn key(&self) -> SessionKey {
        SessionKey::new(self.agent, self.session_id.clone())
    }

    pub fn with_cwd(mut self, cwd: Option<String>) -> Self {
        self.cwd = cwd.filter(|c| !c.is_empty());
        self
    }

    pub fn with_pid(mut self, pid: Option<u32>) -> Self {
        self.pid = pid;
        self
    }

    pub fn with_origin(mut self, origin: Option<String>) -> Self {
        self.origin = origin;
        self
    }

    pub fn with_parent(mut self, parent: Option<String>) -> Self {
        self.parent = parent;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWork {
    /// Ids of background tasks that are still running.
    pub background: Vec<String>,
    /// A one-shot wake-up is scheduled.
    pub wakeup: bool,
    /// A recurring job (cron, loop) is scheduled.
    pub recurring: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Error,
    RateLimited,
    Reconnecting,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// Session process started/resumed. `source` is tool specific
    /// (`startup`, `resume`, `clear`, `compact`).
    SessionStarted {
        source: Option<String>,
    },
    /// A user prompt or task start — strong evidence that work begins.
    TurnStarted,
    /// Work is happening (tool call, model output, approval granted).
    Activity {
        detail: Option<String>,
    },
    /// Weak evidence (a log file grew, an unclassified entry). Extends the
    /// settle window but never revives a settled session on its own.
    Heartbeat,
    /// The agent waits for the user (permission, question, plan approval).
    WaitingForInput {
        reason: Option<String>,
    },
    SubagentStarted {
        id: String,
    },
    SubagentFinished {
        id: String,
    },
    BackgroundStarted {
        id: String,
    },
    BackgroundFinished {
        id: String,
    },
    /// A wake-up / follow-up was scheduled. `recurring` keeps the session
    /// alive until it ends.
    ContinuationScheduled {
        recurring: bool,
    },
    /// Explicit end-of-turn signal from the tool (Stop hook, task_complete,
    /// AfterAgent, session.idle, ...).
    TurnCompleted {
        /// The tool reported that the stop was already forced to continue
        /// once (e.g. Claude Code's `stop_hook_active`).
        continuing: bool,
        /// Authoritative snapshot of outstanding work reported together with
        /// the stop (Claude Code's `background_tasks` / `session_crons`).
        /// Replaces whatever was inferred from other sources.
        pending: Option<PendingWork>,
    },
    /// A goal / objective that auto-continues the session was set or
    /// cleared (`/goal` in Claude Code, thread goals in Codex).
    GoalStatus {
        active: bool,
    },
    /// The turn ended without success, or the agent is retrying.
    TurnFailed {
        kind: FailureKind,
        reason: Option<String>,
        resets_at: Option<Millis>,
    },
    SessionEnded {
        reason: Option<String>,
    },
    /// Evidence we could not classify. Puts the session in `Unknown`.
    Unrecognized {
        note: String,
    },
}

impl EventKind {
    /// Events that prove work is (again) in progress.
    pub fn is_strong_activity(&self) -> bool {
        matches!(
            self,
            EventKind::TurnStarted
                | EventKind::Activity { .. }
                | EventKind::SubagentStarted { .. }
                | EventKind::BackgroundStarted { .. }
        )
    }
}
