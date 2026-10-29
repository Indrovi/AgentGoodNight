//! Background monitor thread and shared state.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agn_adapters::integration::{IntegrationEnv, hook_bin_name};
use agn_adapters::monitor::{Monitor, MonitorPaths, ProcessSource, Snapshot};
use agn_adapters::process::ProcessScanner;
use agn_adapters::{claude, codex};
use agn_core::config::Settings;
use agn_core::countdown::Phase;
use agn_core::paths;
use agn_core::power::{PowerBackend, PowerManager, system_power_manager};
use agn_core::time::{Millis, now_millis};
use tauri::{AppHandle, Emitter};

use crate::tray;

pub const SNAPSHOT_EVENT: &str = "agn://snapshot";

pub struct Engine {
    monitor: Mutex<Monitor>,
    power: Box<dyn PowerManager>,
    data_dir: PathBuf,
    last_json: Mutex<String>,
    last_phase: Mutex<Phase>,
    saved_revision: AtomicU64,
    last_save: Mutex<Millis>,
}

impl Engine {
    pub fn new() -> Self {
        let data_dir = paths::data_dir();
        let settings = Settings::load(&paths::settings_path());
        let power = system_power_manager();
        let hook_source = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(hook_bin_name())));
        let monitor_paths = MonitorPaths {
            data_dir: data_dir.clone(),
            claude_root: claude::default_root(),
            codex_root: codex::default_root(),
            integration: IntegrationEnv::system(data_dir.clone(), paths::spool_dir(), hook_source),
        };
        let mut monitor =
            Monitor::new(settings, monitor_paths, ProcessSource::System(ProcessScanner::new()), power.backend());
        if let Some(saved) = Monitor::load_state(&paths::state_path()) {
            monitor.restore(saved);
        }
        Self {
            monitor: Mutex::new(monitor),
            power,
            data_dir,
            last_json: Mutex::new(String::new()),
            last_phase: Mutex::new(Phase::Off),
            saved_revision: AtomicU64::new(0),
            last_save: Mutex::new(0),
        }
    }

    pub fn backend(&self) -> PowerBackend {
        self.power.backend()
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn monitor(&self) -> MutexGuard<'_, Monitor> {
        self.monitor.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn save_settings(&self) {
        let settings = self.monitor().settings().clone();
        if let Err(e) = settings.save(&paths::settings_path()) {
            tracing::warn!(error = %e, "could not save settings");
        }
    }

    /// Persists tracked sessions when they changed (at most every 30 s
    /// unless `force`).
    pub fn persist(&self, force: bool) {
        let now = now_millis();
        let mut last = self.last_save.lock().unwrap_or_else(|p| p.into_inner());
        if !force && now - *last < 30_000 {
            return;
        }
        let m = self.monitor();
        let rev = m.tracker().revision();
        if force || rev != self.saved_revision.load(Ordering::Relaxed) {
            if let Err(e) = m.save_state(&paths::state_path()) {
                tracing::warn!(error = %e, "could not persist sessions");
            }
            self.saved_revision.store(rev, Ordering::Relaxed);
        }
        *last = now;
    }

    pub fn snapshot(&self) -> Snapshot {
        self.monitor().snapshot(now_millis())
    }

    /// Runs one monitor cycle; executes the power action if the monitor
    /// hands one over.
    fn step(&self) {
        let now = now_millis();
        let action = {
            let mut m = self.monitor();
            match catch_unwind(AssertUnwindSafe(|| m.cycle(now))) {
                Ok(a) => a,
                Err(_) => {
                    // Fail safe: an internal error disarms monitoring.
                    m.disarm();
                    m.record_power(Err("internal monitor error – monitoring disarmed".into()));
                    None
                }
            }
        };
        {
            let mut m = self.monitor();
            if m.has_uncommitted_spool() {
                // Hook records are deleted only after their effect is saved.
                match m.save_state(&paths::state_path()) {
                    Ok(()) => m.commit_spool(),
                    Err(e) => tracing::warn!(error = %e, "could not persist sessions; keeping spool records"),
                }
            }
        }
        if let Some(action) = action {
            tracing::warn!(?action, backend = ?self.power.backend(), "all checks passed twice, executing power action");
            let result = self.power.execute(action).map_err(|e| e.to_string());
            self.monitor().record_power(result);
        }
    }

    /// Emits the snapshot to the UI and updates the tray when it changed.
    pub fn publish(&self, app: &AppHandle) {
        let snap = self.snapshot();
        let json = match serde_json::to_string(&snap) {
            Ok(j) => j,
            Err(_) => return,
        };
        {
            let mut last = self.last_json.lock().unwrap_or_else(|p| p.into_inner());
            if *last == json {
                return;
            }
            *last = json;
        }
        let _ = app.emit(SNAPSHOT_EVENT, &snap);
        tray::update(app, &snap);
        let mut last_phase = self.last_phase.lock().unwrap_or_else(|p| p.into_inner());
        let entering_countdown =
            matches!(snap.phase, Phase::Countdown { .. }) && !matches!(*last_phase, Phase::Countdown { .. });
        *last_phase = snap.phase;
        if entering_countdown {
            // Make sure whoever is at the computer can cancel.
            tray::show_main(app);
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

pub fn spawn_loop(app: AppHandle, engine: Arc<Engine>) {
    std::thread::Builder::new()
        .name("agn-monitor".into())
        .spawn(move || {
            loop {
                engine.step();
                engine.publish(&app);
                engine.persist(false);
                let armed = engine.monitor().controller().is_armed();
                std::thread::sleep(Duration::from_millis(if armed { 1_000 } else { 2_000 }));
            }
        })
        .expect("failed to start monitor thread");
}
