//! Power management.
//!
//! The real OS backend only exists in binaries built with
//! `--release --features real-power`. Every other build — `cargo run`,
//! `cargo test`, `tauri dev`, CI test jobs — physically lacks that code and
//! gets [`DryRunPowerManager`], which only records what it would have done.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::time::{Millis, now_millis};

#[cfg(all(feature = "real-power", not(debug_assertions)))]
mod real;

/// `true` only in binaries that contain the real OS power backend.
pub const REAL_POWER_COMPILED: bool = cfg!(all(feature = "real-power", not(debug_assertions)));

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    #[default]
    Sleep,
    Shutdown,
}

impl PowerAction {
    pub fn id(self) -> &'static str {
        match self {
            PowerAction::Sleep => "sleep",
            PowerAction::Shutdown => "shutdown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerBackend {
    DryRun,
    Real,
}

#[derive(Debug, Error)]
pub enum PowerError {
    #[error("power action failed: {0}")]
    Failed(String),
    #[error("power action not supported on this platform")]
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerRecord {
    pub at: Millis,
    pub action: PowerAction,
    pub backend: PowerBackend,
}

pub trait PowerManager: Send + Sync {
    fn backend(&self) -> PowerBackend;
    /// Requests the action from the OS. Must never force-close applications.
    fn execute(&self, action: PowerAction) -> Result<PowerRecord, PowerError>;
}

/// Records requested actions instead of performing them.
#[derive(Debug, Default)]
pub struct DryRunPowerManager {
    history: Mutex<Vec<PowerRecord>>,
    fail_next: Mutex<Option<String>>,
}

impl DryRunPowerManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn history(&self) -> Vec<PowerRecord> {
        self.history.lock().map(|h| h.clone()).unwrap_or_default()
    }

    /// Test hook: make the next `execute` fail with this message.
    pub fn fail_next(&self, msg: impl Into<String>) {
        if let Ok(mut f) = self.fail_next.lock() {
            *f = Some(msg.into());
        }
    }
}

impl PowerManager for DryRunPowerManager {
    fn backend(&self) -> PowerBackend {
        PowerBackend::DryRun
    }

    fn execute(&self, action: PowerAction) -> Result<PowerRecord, PowerError> {
        if let Some(msg) = self.fail_next.lock().ok().and_then(|mut f| f.take()) {
            return Err(PowerError::Failed(msg));
        }
        let rec = PowerRecord { at: now_millis(), action, backend: PowerBackend::DryRun };
        tracing::warn!(action = action.id(), "DRY RUN: power action recorded, nothing executed");
        if let Ok(mut h) = self.history.lock() {
            h.push(rec.clone());
        }
        Ok(rec)
    }
}

/// The power manager for this build: real only in `release + real-power`.
pub fn system_power_manager() -> Box<dyn PowerManager> {
    #[cfg(all(feature = "real-power", not(debug_assertions)))]
    {
        Box::new(real::OsPowerManager)
    }
    #[cfg(not(all(feature = "real-power", not(debug_assertions))))]
    {
        Box::new(DryRunPowerManager::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builds_never_contain_real_power() {
        assert!(!REAL_POWER_COMPILED);
        assert_eq!(system_power_manager().backend(), PowerBackend::DryRun);
    }

    #[test]
    fn dry_run_records_and_can_fail() {
        let pm = DryRunPowerManager::new();
        pm.execute(PowerAction::Sleep).unwrap();
        pm.fail_next("boom");
        assert!(pm.execute(PowerAction::Shutdown).is_err());
        pm.execute(PowerAction::Shutdown).unwrap();
        let h = pm.history();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].action, PowerAction::Sleep);
        assert_eq!(h[1].action, PowerAction::Shutdown);
        assert!(h.iter().all(|r| r.backend == PowerBackend::DryRun));
    }
}
