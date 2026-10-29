//! Filesystem locations used by the app and its hook mode.

use std::path::PathBuf;

pub const APP_DIR_NAME: &str = "AgentGoodNight";

/// Overrides the data directory (tests, portable setups).
pub const DATA_DIR_ENV: &str = "AGN_DATA_DIR";

/// `%LOCALAPPDATA%\AgentGoodNight`, `~/Library/Application Support/AgentGoodNight`
/// or `~/.local/share/AgentGoodNight`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR_NAME)
}

pub fn spool_dir() -> PathBuf {
    data_dir().join("spool")
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

pub fn state_path() -> PathBuf {
    data_dir().join("sessions.json")
}

pub fn backups_dir() -> PathBuf {
    data_dir().join("backups")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}
