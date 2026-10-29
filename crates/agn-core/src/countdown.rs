//! Arming, countdown and final verification — a small pure state machine.
//!
//! ```text
//!  Off --arm--> Watching --Ready--> Countdown --deadline--> Verifying
//!                  ^  ^                 |                       |
//!                  |  +---not Ready-----+                       |
//!                  |  +---cancel (re-arms from now)             |
//!                  |  +---------------- fresh verdict not Ready-+
//!                  +-- Off after the action was handed to the PowerManager
//! ```
//!
//! The controller never performs the action itself. It emits
//! [`Command::Verify`] when the countdown expires; the host must then rescan
//! every source, evaluate the policy again and call [`Controller::on_verified`].

use serde::{Deserialize, Serialize};

use crate::policy::Verdict;
use crate::power::PowerAction;
use crate::time::{Millis, SECOND};

pub const DEFAULT_COUNTDOWN: Millis = 60 * SECOND;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    Off,
    Watching,
    Countdown { deadline: Millis },
    Verifying,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Rescan all sources now and call [`Controller::on_verified`].
    Verify,
    /// Everything checked out twice; hand this to the PowerManager.
    Execute(PowerAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopCause {
    UserDisarmed,
    ActionHandedOff,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Controller {
    phase: Phase,
    armed_at: Option<Millis>,
    action: PowerAction,
    countdown: Millis,
}

impl Default for Controller {
    fn default() -> Self {
        Self::new(PowerAction::Sleep, DEFAULT_COUNTDOWN)
    }
}

impl Controller {
    pub fn new(action: PowerAction, countdown: Millis) -> Self {
        Self { phase: Phase::Off, armed_at: None, action, countdown: countdown.max(SECOND) }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn armed_at(&self) -> Option<Millis> {
        self.armed_at
    }

    pub fn is_armed(&self) -> bool {
        self.phase != Phase::Off
    }

    pub fn action(&self) -> PowerAction {
        self.action
    }

    pub fn countdown_len(&self) -> Millis {
        self.countdown
    }

    /// Changing the action restarts a running countdown so the user always
    /// gets the full warning period for what will actually happen.
    pub fn set_action(&mut self, action: PowerAction, now: Millis) {
        if self.action != action {
            self.action = action;
            if matches!(self.phase, Phase::Countdown { .. } | Phase::Verifying) {
                self.phase = Phase::Countdown { deadline: now + self.countdown };
            }
        }
    }

    pub fn arm(&mut self, now: Millis) {
        if self.phase == Phase::Off {
            self.phase = Phase::Watching;
            self.armed_at = Some(now);
        }
    }

    pub fn disarm(&mut self) -> Option<StopCause> {
        if self.phase == Phase::Off {
            return None;
        }
        self.phase = Phase::Off;
        self.armed_at = None;
        Some(StopCause::UserDisarmed)
    }

    /// User pressed Cancel during the countdown. Monitoring stays armed, but
    /// only completions recorded from now on can trigger a new countdown.
    pub fn cancel(&mut self, now: Millis) -> bool {
        if matches!(self.phase, Phase::Countdown { .. } | Phase::Verifying) {
            self.phase = Phase::Watching;
            self.armed_at = Some(now);
            true
        } else {
            false
        }
    }

    /// Feed the latest regular verdict.
    pub fn on_verdict(&mut self, verdict: &Verdict, now: Millis) -> Option<Command> {
        match self.phase {
            Phase::Off | Phase::Verifying => None,
            Phase::Watching => {
                if verdict.is_ready() {
                    self.phase = Phase::Countdown { deadline: now + self.countdown };
                }
                None
            }
            Phase::Countdown { deadline } => {
                if !verdict.is_ready() {
                    self.phase = Phase::Watching;
                    None
                } else if now >= deadline {
                    self.phase = Phase::Verifying;
                    Some(Command::Verify)
                } else {
                    None
                }
            }
        }
    }

    /// Feed the verdict computed from a fresh full rescan right before acting.
    pub fn on_verified(&mut self, verdict: &Verdict) -> Option<Command> {
        if self.phase != Phase::Verifying {
            return None;
        }
        if verdict.is_ready() {
            // Disarm before handing off: after wake-up nothing re-triggers.
            self.phase = Phase::Off;
            self.armed_at = None;
            Some(Command::Execute(self.action))
        } else {
            self.phase = Phase::Watching;
            None
        }
    }

    pub fn remaining(&self, now: Millis) -> Option<Millis> {
        match self.phase {
            Phase::Countdown { deadline } => Some((deadline - now).max(0)),
            Phase::Verifying => Some(0),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::BlockReason;

    fn ready() -> Verdict {
        Verdict::Ready { completed: vec![] }
    }

    fn blocked() -> Verdict {
        Verdict::Blocked { reasons: vec![BlockReason::NoCompletionRecorded] }
    }

    #[test]
    fn full_cycle_requires_verification() {
        let mut c = Controller::new(PowerAction::Sleep, 60 * SECOND);
        assert_eq!(c.on_verdict(&ready(), 0), None, "off ignores verdicts");
        c.arm(0);
        assert_eq!(c.on_verdict(&ready(), 1_000), None);
        assert_eq!(c.phase(), Phase::Countdown { deadline: 61_000 });
        assert_eq!(c.on_verdict(&ready(), 30_000), None);
        assert_eq!(c.remaining(30_000), Some(31_000));
        assert_eq!(c.on_verdict(&ready(), 61_000), Some(Command::Verify));
        assert_eq!(c.on_verdict(&ready(), 62_000), None, "no double verify");
        assert_eq!(c.on_verified(&ready()), Some(Command::Execute(PowerAction::Sleep)));
        assert_eq!(c.phase(), Phase::Off);
        assert!(!c.is_armed());
    }

    #[test]
    fn activity_cancels_countdown() {
        let mut c = Controller::default();
        c.arm(0);
        c.on_verdict(&ready(), 0);
        assert!(matches!(c.phase(), Phase::Countdown { .. }));
        c.on_verdict(&blocked(), 10_000);
        assert_eq!(c.phase(), Phase::Watching);
    }

    #[test]
    fn failed_verification_returns_to_watching() {
        let mut c = Controller::new(PowerAction::Shutdown, SECOND);
        c.arm(0);
        c.on_verdict(&ready(), 0);
        assert_eq!(c.on_verdict(&ready(), 1_000), Some(Command::Verify));
        assert_eq!(c.on_verified(&blocked()), None);
        assert_eq!(c.phase(), Phase::Watching);
    }

    #[test]
    fn cancel_rearms_from_now() {
        let mut c = Controller::default();
        c.arm(0);
        c.on_verdict(&ready(), 5);
        assert!(c.cancel(100));
        assert_eq!(c.armed_at(), Some(100));
        assert_eq!(c.phase(), Phase::Watching);
        assert!(!c.cancel(200), "nothing to cancel");
    }

    #[test]
    fn changing_action_restarts_countdown() {
        let mut c = Controller::new(PowerAction::Sleep, 60 * SECOND);
        c.arm(0);
        c.on_verdict(&ready(), 0);
        c.set_action(PowerAction::Shutdown, 50_000);
        assert_eq!(c.phase(), Phase::Countdown { deadline: 110_000 });
        assert_eq!(c.action(), PowerAction::Shutdown);
    }

    #[test]
    fn verify_outside_verifying_is_ignored() {
        let mut c = Controller::default();
        assert_eq!(c.on_verified(&ready()), None);
        c.arm(0);
        assert_eq!(c.on_verified(&ready()), None);
    }
}
