//! End-to-end simulations of overnight scenarios against the tracker, policy
//! and countdown controller. Every power action goes to a DryRun manager.

use agn_core::countdown::{Command, Controller, Phase};
use agn_core::event::{AgentEvent, EventKind, FailureKind, PendingWork};
use agn_core::model::{AgentKind, EvidenceSource, Session, SessionKey, SessionState};
use agn_core::policy::{self, AdapterHealth, BlockReason, PolicyConfig, PolicyInput, UntrackedProcess, Verdict};
use agn_core::power::{DryRunPowerManager, PowerAction, PowerManager};
use agn_core::time::{MINUTE, Millis, SECOND};
use agn_core::tracker::{Tracker, TrackerConfig};

const SETTLE: Millis = 90 * SECOND;

struct Sim {
    now: Millis,
    tracker: Tracker,
    controller: Controller,
    health: Vec<AdapterHealth>,
    untracked: Vec<UntrackedProcess>,
    power: DryRunPowerManager,
    alive_pids: Vec<u32>,
    environment: Vec<String>,
}

impl Sim {
    fn new() -> Self {
        Self {
            now: 1_000_000,
            tracker: Tracker::new(TrackerConfig { settle: SETTLE, ..Default::default() }),
            controller: Controller::new(PowerAction::Sleep, 60 * SECOND),
            health: AgentKind::ALL.into_iter().map(|a| AdapterHealth::ok(a, "ok")).collect(),
            untracked: vec![],
            power: DryRunPowerManager::new(),
            alive_pids: vec![],
            environment: vec![],
        }
    }

    fn ev(&mut self, agent: AgentKind, id: &str, kind: EventKind) {
        let e = AgentEvent::new(agent, id, self.now, kind, EvidenceSource::Simulation)
            .with_cwd(Some(format!("/work/{id}")));
        self.tracker.apply(&e, self.now);
    }

    /// Event that happened `ago` ms in the past but is delivered now.
    fn late(&mut self, agent: AgentKind, id: &str, ago: Millis, kind: EventKind) {
        let e = AgentEvent::new(agent, id, self.now - ago, kind, EvidenceSource::Transcript);
        self.tracker.apply(&e, self.now);
    }

    fn state(&self, agent: AgentKind, id: &str) -> SessionState {
        self.tracker.get(&SessionKey::new(agent, id)).expect("session").state
    }

    fn verdict(&self) -> Verdict {
        let sessions: Vec<&Session> = self.tracker.sessions().collect();
        let alive = |s: &Session| s.pid.is_some_and(|p| self.alive_pids.contains(&p));
        policy::evaluate(
            &PolicyInput {
                now: self.now,
                armed_at: self.controller.armed_at(),
                sessions: &sessions,
                health: &self.health,
                untracked: &self.untracked,
                environment: &self.environment,
                alive: &alive,
            },
            &PolicyConfig::default(),
        )
    }

    /// Advances time in 1 s steps, running the same loop as the app.
    /// Returns the number of power actions executed.
    fn run_for(&mut self, dur: Millis) -> usize {
        let end = self.now + dur;
        let before = self.power.history().len();
        while self.now < end {
            self.now += SECOND;
            self.tracker.tick(self.now);
            let v = self.verdict();
            if let Some(Command::Verify) = self.controller.on_verdict(&v, self.now) {
                // The app rescans every source here; in the simulation the
                // state is already current, so re-evaluate.
                self.tracker.tick(self.now);
                let fresh = self.verdict();
                if let Some(Command::Execute(action)) = self.controller.on_verified(&fresh) {
                    self.power.execute(action).unwrap();
                }
            }
        }
        self.power.history().len() - before
    }

    fn arm(&mut self) {
        self.controller.arm(self.now);
    }

    fn has_reason(&self, pred: impl Fn(&BlockReason) -> bool) -> bool {
        match self.verdict() {
            Verdict::Blocked { reasons } => reasons.iter().any(pred),
            _ => false,
        }
    }
}

use AgentKind::*;

fn stop() -> EventKind {
    EventKind::TurnCompleted { continuing: false, pending: None }
}

#[test]
fn disarmed_never_acts() {
    let mut sim = Sim::new();
    sim.ev(ClaudeCode, "a", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "a", stop());
    assert_eq!(sim.run_for(10 * MINUTE), 0);
    assert_eq!(sim.verdict(), Verdict::Disarmed);
}

#[test]
fn single_session_completes_then_sleeps_once() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "a", EventKind::TurnStarted);
    assert_eq!(sim.run_for(5 * MINUTE), 0);
    sim.ev(ClaudeCode, "a", stop());
    assert_eq!(sim.state(ClaudeCode, "a"), SessionState::Finishing);
    // settle (90 s) + countdown (60 s)
    assert_eq!(sim.run_for(80 * SECOND), 0, "still settling");
    assert_eq!(sim.run_for(30 * SECOND), 0, "countdown running");
    assert!(matches!(sim.controller.phase(), Phase::Countdown { .. }));
    assert_eq!(sim.run_for(60 * SECOND), 1, "exactly one dry-run action");
    assert_eq!(sim.power.history()[0].action, PowerAction::Sleep);
    assert!(!sim.controller.is_armed(), "disarmed after the action");
    assert_eq!(sim.run_for(30 * MINUTE), 0, "never repeats");
}

#[test]
fn monitoring_armed_before_first_run_waits_for_completion() {
    let mut sim = Sim::new();
    sim.arm();
    assert_eq!(sim.run_for(30 * MINUTE), 0);
    assert!(sim.has_reason(|r| *r == BlockReason::NoCompletionRecorded));
    sim.ev(Codex, "t1", EventKind::TurnStarted);
    sim.run_for(MINUTE);
    sim.ev(Codex, "t1", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn completion_before_arming_does_not_count() {
    let mut sim = Sim::new();
    sim.ev(ClaudeCode, "old", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "old", stop());
    sim.run_for(5 * MINUTE);
    assert_eq!(sim.state(ClaudeCode, "old"), SessionState::Completed);
    sim.arm();
    assert_eq!(sim.run_for(30 * MINUTE), 0);
    assert!(sim.has_reason(|r| *r == BlockReason::NoCompletionRecorded));
}

#[test]
fn concurrent_sessions_across_tools_and_projects() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "web", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "api", EventKind::TurnStarted);
    sim.ev(Codex, "infra", EventKind::TurnStarted);
    sim.ev(GeminiCli, "docs", EventKind::TurnStarted);
    sim.run_for(10 * MINUTE);

    sim.ev(ClaudeCode, "web", stop());
    sim.ev(GeminiCli, "docs", stop());
    assert_eq!(sim.run_for(20 * MINUTE), 0, "api and infra still running");

    sim.ev(Codex, "infra", stop());
    assert_eq!(sim.run_for(20 * MINUTE), 0, "api still running");
    assert!(sim.has_reason(|r| matches!(r, BlockReason::SessionActive { key, .. } if key.id == "api")));

    sim.ev(ClaudeCode, "api", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn goal_continuation_within_settle_window_is_not_completion() {
    // /goal: the Stop is blocked by the goal hook and Claude keeps working.
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "g", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "g", EventKind::GoalStatus { active: true });
    sim.ev(ClaudeCode, "g", stop());
    assert_eq!(sim.state(ClaudeCode, "g"), SessionState::AwaitingContinuation);
    sim.run_for(40 * SECOND);
    sim.ev(ClaudeCode, "g", EventKind::Activity { detail: Some("PostToolUse".into()) });
    assert_eq!(sim.state(ClaudeCode, "g"), SessionState::Running);
    sim.ev(ClaudeCode, "g", EventKind::TurnCompleted { continuing: true, pending: None });
    assert_eq!(sim.run_for(30 * MINUTE), 0, "goal still active");
    sim.ev(ClaudeCode, "g", EventKind::GoalStatus { active: false });
    assert_eq!(sim.state(ClaudeCode, "g"), SessionState::Finishing);
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn stop_hook_continuation_without_goal_marker_is_caught_by_settle() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "s", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "s", stop());
    sim.run_for(60 * SECOND);
    // A stop hook blocked; the model resumes with a new tool call.
    sim.ev(ClaudeCode, "s", EventKind::Activity { detail: None });
    assert_eq!(sim.run_for(10 * MINUTE), 0);
    assert_eq!(sim.state(ClaudeCode, "s"), SessionState::Running);
}

#[test]
fn background_tasks_reported_at_stop_keep_session_alive() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "b", EventKind::TurnStarted);
    let pending = PendingWork { background: vec!["shell-1".into(), "agent-2".into()], ..Default::default() };
    sim.ev(ClaudeCode, "b", EventKind::TurnCompleted { continuing: false, pending: Some(pending) });
    assert_eq!(sim.state(ClaudeCode, "b"), SessionState::AwaitingContinuation);
    assert_eq!(sim.run_for(2 * 60 * MINUTE), 0);

    // One finishes and re-invokes the model; the next stop reports the rest.
    sim.ev(ClaudeCode, "b", EventKind::TurnStarted);
    let pending = PendingWork { background: vec!["agent-2".into()], ..Default::default() };
    sim.ev(ClaudeCode, "b", EventKind::TurnCompleted { continuing: false, pending: Some(pending) });
    assert_eq!(sim.run_for(30 * MINUTE), 0);

    sim.ev(ClaudeCode, "b", EventKind::TurnStarted);
    let none = PendingWork::default();
    sim.ev(ClaudeCode, "b", EventKind::TurnCompleted { continuing: false, pending: Some(none) });
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn background_finish_without_reinvocation_still_completes() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "b", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "b", EventKind::BackgroundStarted { id: "x1".into() });
    sim.ev(ClaudeCode, "b", stop());
    assert_eq!(sim.state(ClaudeCode, "b"), SessionState::AwaitingContinuation);
    sim.run_for(10 * MINUTE);
    sim.ev(ClaudeCode, "b", EventKind::BackgroundFinished { id: "x1".into() });
    assert_eq!(sim.state(ClaudeCode, "b"), SessionState::Finishing);
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn late_transcript_lines_cannot_revive_a_finished_turn() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "t", EventKind::TurnStarted);
    sim.run_for(MINUTE);
    sim.ev(ClaudeCode, "t", stop());
    sim.run_for(5 * SECOND);
    // Transcript tail delivers an assistant entry written before the stop.
    sim.late(ClaudeCode, "t", 10 * SECOND, EventKind::Activity { detail: Some("assistant".into()) });
    assert_eq!(sim.state(ClaudeCode, "t"), SessionState::Finishing);
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn late_background_launch_blocks_even_after_completion() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "t", EventKind::TurnStarted);
    sim.run_for(MINUTE);
    sim.ev(ClaudeCode, "t", stop());
    sim.run_for(100 * SECOND);
    assert_eq!(sim.state(ClaudeCode, "t"), SessionState::Completed);
    // Countdown running; a slow source reports a background launch from
    // before the stop. The countdown must be cancelled.
    sim.late(ClaudeCode, "t", 2 * MINUTE, EventKind::BackgroundStarted { id: "late".into() });
    assert_eq!(sim.state(ClaudeCode, "t"), SessionState::AwaitingContinuation);
    assert_eq!(sim.run_for(10 * MINUTE), 0);
    assert_eq!(sim.controller.phase(), Phase::Watching);
}

#[test]
fn snapshot_supersedes_older_inferred_background() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "t", EventKind::TurnStarted);
    sim.run_for(MINUTE);
    sim.ev(ClaudeCode, "t", EventKind::TurnCompleted { continuing: false, pending: Some(PendingWork::default()) });
    // Transcript later reports a launch that the authoritative stop payload
    // already accounted for (it had finished).
    sim.late(ClaudeCode, "t", 30 * SECOND, EventKind::BackgroundStarted { id: "done-already".into() });
    assert_eq!(sim.state(ClaudeCode, "t"), SessionState::Finishing);
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn rate_limit_blocks_until_resumed_and_completed() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "r", EventKind::TurnStarted);
    sim.ev(Codex, "c", EventKind::TurnStarted);
    sim.ev(Codex, "c", stop());
    sim.ev(
        ClaudeCode,
        "r",
        EventKind::TurnFailed {
            kind: FailureKind::RateLimited,
            reason: Some("five_hour".into()),
            resets_at: Some(sim.now + 3 * 60 * MINUTE),
        },
    );
    assert_eq!(sim.run_for(3 * 60 * MINUTE), 0, "rate-limited session will auto-continue");
    sim.ev(ClaudeCode, "r", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "r", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn waiting_for_input_error_reconnect_interrupt_and_unknown_block() {
    for kind in [
        EventKind::WaitingForInput { reason: Some("permission_prompt".into()) },
        EventKind::TurnFailed { kind: FailureKind::Error, reason: None, resets_at: None },
        EventKind::TurnFailed { kind: FailureKind::Reconnecting, reason: None, resets_at: None },
        EventKind::TurnFailed { kind: FailureKind::Interrupted, reason: None, resets_at: None },
        EventKind::Unrecognized { note: "mystery".into() },
    ] {
        let mut sim = Sim::new();
        sim.arm();
        sim.ev(Codex, "done", EventKind::TurnStarted);
        sim.ev(Codex, "done", stop());
        sim.ev(ClaudeCode, "x", kind.clone());
        assert_eq!(sim.run_for(60 * MINUTE), 0, "{kind:?} must block");
        assert!(sim.state(ClaudeCode, "x").is_blocking());
    }
}

#[test]
fn reconnect_then_recovery() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(Codex, "c", EventKind::TurnStarted);
    sim.ev(Codex, "c", EventKind::TurnFailed { kind: FailureKind::Reconnecting, reason: None, resets_at: None });
    assert_eq!(sim.run_for(5 * MINUTE), 0);
    sim.ev(Codex, "c", EventKind::Activity { detail: None });
    sim.ev(Codex, "c", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn untracked_process_and_unobservable_adapter_block() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(Codex, "c", EventKind::TurnStarted);
    sim.ev(Codex, "c", stop());
    sim.untracked.push(UntrackedProcess { agent: Cursor, pid: 4242, name: "cursor-agent".into(), ignored: false });
    assert_eq!(sim.run_for(10 * MINUTE), 0);
    assert!(sim.has_reason(|r| matches!(r, BlockReason::UntrackedProcess { pid: 4242, .. })));

    sim.untracked[0].ignored = true;
    sim.health[2] = AdapterHealth::unobservable(GeminiCli, "no_integration", None);
    assert_eq!(sim.run_for(10 * MINUTE), 0);
    assert!(sim.has_reason(|r| matches!(r, BlockReason::AdapterUnobservable { agent: GeminiCli, .. })));

    sim.health[2].enabled = false; // user disabled that adapter explicitly
    // The completion is old but recorded after arming, so it still counts.
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn ignored_and_stale_sessions_do_not_block() {
    let mut sim = Sim::new();
    sim.ev(ClaudeCode, "stale", EventKind::TurnStarted);
    sim.now += 25 * 60 * MINUTE; // 25 h later
    sim.arm();
    sim.ev(ClaudeCode, "stuck", EventKind::TurnStarted);
    sim.tracker.set_ignored(&SessionKey::new(ClaudeCode, "stuck"), true);
    sim.ev(Codex, "c", EventKind::TurnStarted);
    sim.ev(Codex, "c", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn stale_session_with_live_process_still_blocks() {
    let mut sim = Sim::new();
    let e =
        AgentEvent::new(ClaudeCode, "long", sim.now, EventKind::TurnStarted, EvidenceSource::Hook).with_pid(Some(777));
    sim.tracker.apply(&e, sim.now);
    sim.alive_pids.push(777);
    sim.now += 30 * 60 * MINUTE;
    sim.arm();
    sim.ev(Codex, "c", EventKind::TurnStarted);
    sim.ev(Codex, "c", stop());
    assert_eq!(sim.run_for(10 * MINUTE), 0);
    sim.alive_pids.clear();
    assert_eq!(sim.run_for(3 * MINUTE), 1, "dead stale session no longer relevant");
}

#[test]
fn new_activity_cancels_countdown_and_reverification_catches_last_second_work() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "a", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "a", stop());
    sim.run_for(100 * SECOND);
    assert!(matches!(sim.controller.phase(), Phase::Countdown { .. }));
    sim.ev(Codex, "new", EventKind::TurnStarted);
    sim.run_for(SECOND);
    assert_eq!(sim.controller.phase(), Phase::Watching, "new activity cancels countdown");
    sim.ev(Codex, "new", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn user_cancel_requires_a_new_completion() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "a", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "a", stop());
    sim.run_for(100 * SECOND);
    assert!(sim.controller.cancel(sim.now));
    assert_eq!(sim.run_for(60 * MINUTE), 0);
    sim.ev(ClaudeCode, "a", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "a", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn session_end_after_stop_counts_as_completion_but_not_mid_run() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "headless", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "headless", stop());
    sim.ev(ClaudeCode, "headless", EventKind::SessionEnded { reason: Some("other".into()) });
    assert_eq!(sim.state(ClaudeCode, "headless"), SessionState::Ended);
    assert_eq!(sim.run_for(2 * MINUTE), 1);

    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "killed", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "killed", EventKind::SessionEnded { reason: Some("other".into()) });
    let s = sim.tracker.get(&SessionKey::new(ClaudeCode, "killed")).unwrap();
    assert!(s.ended_unfinished);
    assert_eq!(sim.run_for(30 * MINUTE), 0, "no completion was ever recorded");
}

#[test]
fn vanished_process_is_never_completion() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "crash", EventKind::TurnStarted);
    sim.tracker.process_gone(&SessionKey::new(ClaudeCode, "crash"), sim.now);
    assert_eq!(sim.state(ClaudeCode, "crash"), SessionState::Interrupted);
    sim.ev(Codex, "ok", EventKind::TurnStarted);
    sim.ev(Codex, "ok", stop());
    assert_eq!(sim.run_for(30 * MINUTE), 0);
}

#[test]
fn scheduled_wakeup_and_recurring_cron() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "loop", EventKind::TurnStarted);
    sim.ev(ClaudeCode, "loop", EventKind::ContinuationScheduled { recurring: false });
    sim.ev(ClaudeCode, "loop", stop());
    assert_eq!(sim.state(ClaudeCode, "loop"), SessionState::AwaitingContinuation);
    assert_eq!(sim.run_for(20 * MINUTE), 0);
    // Wake-up fires: a new turn that does not reschedule.
    sim.ev(ClaudeCode, "loop", EventKind::Activity { detail: None });
    sim.ev(ClaudeCode, "loop", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);

    let mut sim = Sim::new();
    sim.arm();
    sim.ev(ClaudeCode, "cron", EventKind::TurnStarted);
    let pending = PendingWork { recurring: true, ..Default::default() };
    sim.ev(ClaudeCode, "cron", EventKind::TurnCompleted { continuing: false, pending: Some(pending) });
    assert_eq!(sim.run_for(6 * 60 * MINUTE), 0, "recurring jobs keep the PC on");
}

#[test]
fn subagents_of_codex_are_separate_sessions() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(Codex, "parent", EventKind::TurnStarted);
    let child = AgentEvent::new(Codex, "child", sim.now, EventKind::TurnStarted, EvidenceSource::Rollout)
        .with_parent(Some("parent".into()));
    sim.tracker.apply(&child, sim.now);
    sim.ev(Codex, "parent", stop());
    assert_eq!(sim.run_for(30 * MINUTE), 0, "child thread still running");
    sim.ev(Codex, "child", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn out_of_order_subagent_finish_before_start() {
    let mut sim = Sim::new();
    sim.arm();
    sim.ev(Cursor, "c", EventKind::TurnStarted);
    sim.ev(Cursor, "c", EventKind::SubagentFinished { id: "s1".into() });
    sim.late(Cursor, "c", SECOND, EventKind::SubagentStarted { id: "s1".into() });
    sim.ev(Cursor, "c", stop());
    assert_eq!(sim.state(Cursor, "c"), SessionState::Finishing);
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}

#[test]
fn shutdown_action_and_failure_handling() {
    let mut sim = Sim::new();
    sim.controller.set_action(PowerAction::Shutdown, sim.now);
    sim.arm();
    sim.ev(GeminiCli, "g", EventKind::TurnStarted);
    sim.ev(GeminiCli, "g", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);
    assert_eq!(sim.power.history()[0].action, PowerAction::Shutdown);
}

#[test]
fn many_concurrent_sessions_stress() {
    let mut sim = Sim::new();
    sim.arm();
    let ids: Vec<String> = (0..200).map(|i| format!("s{i}")).collect();
    for (i, id) in ids.iter().enumerate() {
        let agent = AgentKind::ALL[i % 5];
        sim.ev(agent, id, EventKind::TurnStarted);
    }
    for (i, id) in ids.iter().enumerate() {
        let agent = AgentKind::ALL[i % 5];
        sim.ev(agent, id, stop());
        if i < ids.len() - 1 {
            assert_eq!(sim.run_for(SECOND), 0);
        }
    }
    sim.ev(ClaudeCode, "s0", EventKind::Activity { detail: None });
    assert_eq!(sim.run_for(10 * MINUTE), 0, "one revived session blocks all");
    sim.ev(ClaudeCode, "s0", stop());
    assert_eq!(sim.run_for(3 * MINUTE), 1);
}
