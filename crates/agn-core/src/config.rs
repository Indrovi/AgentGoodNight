//! Persistent user settings.
//!
//! Monitoring itself is deliberately *not* persisted: every app start begins
//! disarmed, so a forgotten switch can never power off the computer the next
//! day.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::AgentKind;
use crate::power::PowerAction;
use crate::time::{HOUR, Millis, SECOND};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub action: PowerAction,
    /// UI language (`cs`, `en`, ...).
    pub language: String,
    /// Adapters the user wants to take into account. A disabled adapter is
    /// ignored entirely — including its running processes.
    pub agents: BTreeMap<AgentKind, bool>,
    /// Debounce after an end-of-turn event, seconds.
    pub settle_secs: u64,
    /// Countdown before the action, seconds (minimum 10).
    pub countdown_secs: u64,
    /// Sessions quiet for longer than this do not block, hours.
    pub relevance_hours: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            action: PowerAction::Sleep,
            language: "cs".into(),
            agents: AgentKind::ALL.into_iter().map(|k| (k, true)).collect(),
            settle_secs: 90,
            countdown_secs: 60,
            relevance_hours: 24,
        }
    }
}

impl Settings {
    pub fn agent_enabled(&self, agent: AgentKind) -> bool {
        self.agents.get(&agent).copied().unwrap_or(true)
    }

    pub fn settle(&self) -> Millis {
        (self.settle_secs.clamp(15, 3_600) as Millis) * SECOND
    }

    pub fn countdown(&self) -> Millis {
        (self.countdown_secs.clamp(10, 600) as Millis) * SECOND
    }

    pub fn relevance_window(&self) -> Millis {
        (self.relevance_hours.clamp(1, 24 * 14) as Millis) * HOUR
    }

    /// Missing or unreadable files yield defaults; a corrupt file is kept
    /// aside so the user can inspect it.
    pub fn load(path: &Path) -> Self {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "settings unreadable, using defaults");
                let _ = fs::rename(path, path.with_extension("json.corrupt"));
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }
}

/// Writes via a temp file + rename so readers never see half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp-agn");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        assert_eq!(Settings::load(&p), Settings::default());
        let mut s = Settings { action: PowerAction::Shutdown, ..Default::default() };
        s.agents.insert(AgentKind::Cursor, false);
        s.save(&p).unwrap();
        let back = Settings::load(&p);
        assert_eq!(back, s);
        assert!(!back.agent_enabled(AgentKind::Cursor));
        assert!(back.agent_enabled(AgentKind::Codex));
    }

    #[test]
    fn corrupt_file_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        fs::write(&p, b"{not json").unwrap();
        assert_eq!(Settings::load(&p), Settings::default());
        assert!(p.with_extension("json.corrupt").exists());
    }

    #[test]
    fn partial_file_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        fs::write(&p, br#"{"action":"shutdown"}"#).unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.action, PowerAction::Shutdown);
        assert_eq!(s.countdown_secs, 60);
    }

    #[test]
    fn clamps_unsafe_values() {
        let s = Settings { countdown_secs: 0, settle_secs: 0, ..Default::default() };
        assert_eq!(s.countdown(), 10 * SECOND);
        assert_eq!(s.settle(), 15 * SECOND);
    }
}
