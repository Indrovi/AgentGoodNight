//! Core of AgentGoodNight: the session model, the tracker that folds agent
//! lifecycle events into session states, the safety policy that decides when
//! a power action is allowed, the countdown controller and the power backends.
//!
//! This crate has no UI and no OS integration besides the (feature-gated)
//! power backend, which keeps it fully testable with simulated events.

pub mod config;
pub mod countdown;
pub mod event;
pub mod model;
pub mod paths;
pub mod policy;
pub mod power;
pub mod spool;
pub mod time;
pub mod tracker;

pub use event::{AgentEvent, EventKind, FailureKind, PendingWork};
pub use model::{AgentKind, EvidenceSource, Session, SessionKey, SessionState};
pub use policy::{AdapterHealth, BlockReason, PolicyConfig, UntrackedProcess, Verdict};
pub use power::{PowerAction, PowerBackend, PowerManager};
pub use time::Millis;
pub use tracker::{Tracker, TrackerConfig};
