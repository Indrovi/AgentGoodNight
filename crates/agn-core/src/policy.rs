//! The safety arbiter. Decides whether a power action is allowed *right now*.
//!
//! The verdict is `Ready` only when every one of these holds:
//! 1. monitoring is armed,
//! 2. every enabled adapter reports it can observe its tool,
//! 3. no agent process is running that we cannot attribute to a tracked session,
//! 4. no relevant, non-ignored session is in a blocking state,
//! 5. at least one completion was recorded after monitoring was armed.
//!
//! Any doubt produces a `Blocked` verdict with machine-readable reasons.

use serde::{Deserialize, Serialize};

use crate::model::{AgentKind, Session, SessionKey, SessionState};
use crate::time::{HOUR, Millis};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// Sessions quiet for longer than this are considered stale and do not
    /// block — unless their process is known to be alive.
    pub relevance_window: Millis,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self { relevance_window: 24 * HOUR }
    }
}

/// Health of one adapter as reported in each monitoring cycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterHealth {
    pub agent: AgentKind,
    pub enabled: bool,
    /// `false` means the adapter cannot currently observe its tool well enough
    /// to rule out unseen work. Blocks the power action while enabled.
    pub observable: bool,
    /// Machine-readable status code (`ok`, `not_installed`, `no_integration`,
    /// `read_error`, ...), translated by the UI.
    pub code: String,
    pub detail: Option<String>,
}

impl AdapterHealth {
    pub fn ok(agent: AgentKind, code: impl Into<String>) -> Self {
        Self { agent, enabled: true, observable: true, code: code.into(), detail: None }
    }

    pub fn unobservable(agent: AgentKind, code: impl Into<String>, detail: Option<String>) -> Self {
        Self { agent, enabled: true, observable: false, code: code.into(), detail }
    }
}

/// A live agent process that no tracked session accounts for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UntrackedProcess {
    pub agent: AgentKind,
    pub pid: u32,
    pub name: String,
    pub ignored: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockReason {
    SessionActive {
        key: SessionKey,
        state: SessionState,
    },
    AdapterUnobservable {
        agent: AgentKind,
        code: String,
    },
    UntrackedProcess {
        agent: AgentKind,
        pid: u32,
    },
    /// Agents could be running somewhere we cannot look into (e.g. a WSL VM).
    EnvironmentUnobservable {
        code: String,
    },
    NoCompletionRecorded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Verdict {
    Disarmed,
    Blocked { reasons: Vec<BlockReason> },
    Ready { completed: Vec<SessionKey> },
}

impl Verdict {
    pub fn is_ready(&self) -> bool {
        matches!(self, Verdict::Ready { .. })
    }
}

pub struct PolicyInput<'a> {
    pub now: Millis,
    pub armed_at: Option<Millis>,
    pub sessions: &'a [&'a Session],
    pub health: &'a [AdapterHealth],
    pub untracked: &'a [UntrackedProcess],
    /// Codes of environments that may run unobservable agents (wsl_running).
    pub environment: &'a [String],
    /// Sessions whose owning process is currently known to be alive.
    pub alive: &'a dyn Fn(&Session) -> bool,
}

/// Is this session part of tonight's decision? Quiet sessions stay relevant
/// while their process lives — unless the only reason they block is history
/// we never saw (a long-idle desktop session would otherwise block forever).
pub fn is_relevant(s: &Session, now: Millis, cfg: &PolicyConfig, alive: bool) -> bool {
    if s.ignored {
        return false;
    }
    let recent = now - s.last_activity <= cfg.relevance_window;
    let only_history = s.state == SessionState::AwaitingContinuation
        && s.background.len() == 1
        && s.background.contains(crate::tracker::HISTORY_UNKNOWN)
        && s.open_subagents.is_empty()
        && s.wakeups.is_empty()
        && !s.recurring
        && !s.goal_active;
    recent || (alive && s.state.is_blocking() && !only_history)
}

pub fn evaluate(input: &PolicyInput<'_>, cfg: &PolicyConfig) -> Verdict {
    let Some(armed_at) = input.armed_at else {
        return Verdict::Disarmed;
    };
    let mut reasons = Vec::new();

    for h in input.health {
        if h.enabled && !h.observable {
            reasons.push(BlockReason::AdapterUnobservable { agent: h.agent, code: h.code.clone() });
        }
    }
    for p in input.untracked {
        if !p.ignored {
            reasons.push(BlockReason::UntrackedProcess { agent: p.agent, pid: p.pid });
        }
    }

    for code in input.environment {
        reasons.push(BlockReason::EnvironmentUnobservable { code: code.clone() });
    }

    let mut completed = Vec::new();
    for s in input.sessions {
        let alive = (input.alive)(s);
        if !is_relevant(s, input.now, cfg, alive) {
            continue;
        }
        if s.state.is_blocking() {
            reasons.push(BlockReason::SessionActive { key: s.key.clone(), state: s.state });
        } else if s.completed_at.is_some_and(|t| t >= armed_at && t <= input.now) {
            completed.push(s.key.clone());
        }
    }

    if completed.is_empty() {
        reasons.push(BlockReason::NoCompletionRecorded);
    }

    if reasons.is_empty() { Verdict::Ready { completed } } else { Verdict::Blocked { reasons } }
}
