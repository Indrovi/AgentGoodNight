//! Commands invoked by the web UI.

use std::collections::HashMap;
use std::sync::Arc;

use agn_adapters::integration;
use agn_adapters::monitor::Snapshot;
use agn_core::model::{AgentKind, SessionKey};
use agn_core::power::PowerAction;
use agn_core::time::now_millis;
use tauri::{AppHandle, State};

use crate::engine::Engine;
use crate::i18n;

type E<'a> = State<'a, Arc<Engine>>;

fn agent(id: &str) -> Result<AgentKind, String> {
    AgentKind::from_id(id).ok_or_else(|| format!("unknown agent {id}"))
}

fn refresh(app: &AppHandle, engine: &Engine) -> Snapshot {
    engine.publish(app);
    engine.snapshot()
}

#[tauri::command]
pub fn get_snapshot(engine: E<'_>) -> Snapshot {
    engine.snapshot()
}

#[tauri::command]
pub fn get_strings(lang: String) -> HashMap<String, String> {
    let lang = i18n::normalize(&lang);
    let raw = match lang {
        "en" => include_str!("../../ui/i18n/en.json"),
        _ => include_str!("../../ui/i18n/cs.json"),
    };
    serde_json::from_str(raw).unwrap_or_default()
}

#[tauri::command]
pub fn set_armed(app: AppHandle, engine: E<'_>, armed: bool) -> Snapshot {
    {
        let mut m = engine.monitor();
        if armed {
            m.arm(now_millis());
        } else {
            m.disarm();
        }
    }
    refresh(&app, &engine)
}

#[tauri::command]
pub fn cancel_countdown(app: AppHandle, engine: E<'_>) -> Snapshot {
    engine.monitor().cancel_countdown(now_millis());
    refresh(&app, &engine)
}

#[tauri::command]
pub fn set_action(app: AppHandle, engine: E<'_>, action: String) -> Result<Snapshot, String> {
    let action = match action.as_str() {
        "sleep" => PowerAction::Sleep,
        "shutdown" => PowerAction::Shutdown,
        other => return Err(format!("unknown action {other}")),
    };
    engine.monitor().set_action(action, now_millis());
    engine.save_settings();
    Ok(refresh(&app, &engine))
}

#[tauri::command]
pub fn set_language(app: AppHandle, engine: E<'_>, lang: String) -> Snapshot {
    engine.monitor().set_language(i18n::normalize(&lang));
    engine.save_settings();
    refresh(&app, &engine)
}

#[tauri::command]
pub fn set_session_ignored(
    app: AppHandle,
    engine: E<'_>,
    agent_id: String,
    session_id: String,
    ignored: bool,
) -> Result<Snapshot, String> {
    let key = SessionKey::new(agent(&agent_id)?, session_id);
    engine.monitor().set_session_ignored(&key, ignored);
    engine.persist(true);
    Ok(refresh(&app, &engine))
}

#[tauri::command]
pub fn ignore_process(app: AppHandle, engine: E<'_>, pid: u32) -> Snapshot {
    engine.monitor().ignore_process(pid);
    engine.monitor().invalidate();
    refresh(&app, &engine)
}

#[tauri::command]
pub fn set_agent_enabled(app: AppHandle, engine: E<'_>, agent_id: String, enabled: bool) -> Result<Snapshot, String> {
    engine.monitor().set_agent_enabled(agent(&agent_id)?, enabled);
    engine.save_settings();
    Ok(refresh(&app, &engine))
}

/// Only ever called after the user confirmed the dialog showing the file.
#[tauri::command]
pub fn install_integration(app: AppHandle, engine: E<'_>, agent_id: String) -> Result<Option<String>, String> {
    let agent = agent(&agent_id)?;
    let env = engine.monitor().integration_env().clone();
    let backup = integration::install(agent, &env).map_err(|e| e.to_string())?;
    tracing::info!(agent = agent.id(), "integration installed");
    engine.monitor().invalidate();
    refresh(&app, &engine);
    Ok(backup.map(|p| p.display().to_string()))
}

#[tauri::command]
pub fn uninstall_integration(app: AppHandle, engine: E<'_>, agent_id: String) -> Result<Option<String>, String> {
    let agent = agent(&agent_id)?;
    let env = engine.monitor().integration_env().clone();
    let backup = integration::uninstall(agent, &env).map_err(|e| e.to_string())?;
    tracing::info!(agent = agent.id(), "integration removed");
    engine.monitor().invalidate();
    refresh(&app, &engine);
    Ok(backup.map(|p| p.display().to_string()))
}
