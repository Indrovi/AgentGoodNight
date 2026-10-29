//! Opt-in, reversible hook integration.
//!
//! Nothing here runs automatically: the user triggers install/uninstall per
//! tool in the UI after seeing the file that will change. Installing
//! * backs up the existing file into AgentGoodNight's `backups/` directory,
//! * merges our entries into the existing configuration (all other keys,
//!   hooks and their order are preserved),
//! * is idempotent.
//!
//! Uninstalling removes exactly the entries that point at our hook binary.
//! Our hook entries never print anything that could steer the agent.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use agn_core::config::write_atomic;
use agn_core::model::AgentKind;
use agn_core::time::{Millis, now_millis};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::opencode;

pub const HOOK_BIN_STEM: &str = "agentgoodnight-hook";

pub fn hook_bin_name() -> String {
    format!("{HOOK_BIN_STEM}{}", std::env::consts::EXE_SUFFIX)
}

#[derive(Debug, Error)]
pub enum IntegrationError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{path} is not valid JSON ({msg}); nothing was changed")]
    InvalidConfig { path: String, msg: String },
    #[error("integration for {0} is not supported")]
    Unsupported(AgentKind),
    #[error("hook binary not found at {0}")]
    MissingHook(String),
    #[error("the installed hook {0} is in use and could not be updated; try again in a moment")]
    HookInUse(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationMode {
    /// Lifecycle hooks written into the tool's config.
    Hooks,
    /// A plugin file we own.
    Plugin,
    /// Nothing to install: AgentGoodNight reads the tool's own logs.
    Passive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationStatus {
    pub agent: AgentKind,
    pub mode: IntegrationMode,
    pub installed: bool,
    /// File that install/uninstall touches.
    pub config_path: Option<String>,
    /// When the integration was (last) installed, from our marker file.
    pub installed_at: Option<Millis>,
}

/// Locations used by the installer. Tests point these at temp dirs.
#[derive(Clone, Debug)]
pub struct IntegrationEnv {
    pub claude_root: Option<PathBuf>,
    pub gemini_root: Option<PathBuf>,
    pub cursor_root: Option<PathBuf>,
    pub opencode_root: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub spool_dir: PathBuf,
    /// Where the hook binary ships (next to the app executable).
    pub hook_source: Option<PathBuf>,
}

impl IntegrationEnv {
    pub fn system(data_dir: PathBuf, spool_dir: PathBuf, hook_source: Option<PathBuf>) -> Self {
        Self {
            claude_root: crate::claude::default_root(),
            gemini_root: crate::gemini::default_root(),
            cursor_root: crate::cursor::default_root(),
            opencode_root: opencode::config_root(),
            data_dir,
            spool_dir,
            hook_source,
        }
    }

    /// Stable hook location independent of where the app is installed.
    pub fn installed_hook(&self) -> PathBuf {
        self.data_dir.join("bin").join(hook_bin_name())
    }

    fn marker(&self, agent: AgentKind) -> PathBuf {
        self.data_dir.join("integrations").join(format!("{}.json", agent.id()))
    }

    pub fn config_path(&self, agent: AgentKind) -> Option<PathBuf> {
        match agent {
            AgentKind::ClaudeCode => self.claude_root.as_ref().map(|r| r.join("settings.json")),
            AgentKind::GeminiCli => self.gemini_root.as_ref().map(|r| r.join("settings.json")),
            AgentKind::Cursor => self.cursor_root.as_ref().map(|r| r.join("hooks.json")),
            AgentKind::OpenCode => self.opencode_root.as_ref().map(|r| r.join("plugins").join(opencode::PLUGIN_FILE)),
            AgentKind::Codex => None,
        }
    }
}

pub fn mode(agent: AgentKind) -> IntegrationMode {
    match agent {
        AgentKind::Codex => IntegrationMode::Passive,
        AgentKind::OpenCode => IntegrationMode::Plugin,
        _ => IntegrationMode::Hooks,
    }
}

const CLAUDE_EVENTS: &[(&str, bool)] = &[
    ("SessionStart", false),
    ("UserPromptSubmit", false),
    ("PostToolUse", true),
    ("SubagentStart", true),
    ("PermissionRequest", false),
    ("Notification", false),
    ("Stop", false),
    ("StopFailure", false),
    ("SessionEnd", false),
];

const GEMINI_EVENTS: &[&str] =
    &["SessionStart", "SessionEnd", "BeforeAgent", "AfterAgent", "BeforeTool", "AfterTool", "Notification"];

const CURSOR_EVENTS: &[&str] = &[
    "workspaceOpen",
    "sessionStart",
    "sessionEnd",
    "beforeSubmitPrompt",
    "postToolUse",
    "afterAgentResponse",
    "subagentStart",
    "stop",
];

pub fn status(agent: AgentKind, env: &IntegrationEnv) -> IntegrationStatus {
    let path = env.config_path(agent);
    let installed = match (agent, &path) {
        (AgentKind::Codex, _) => true,
        (AgentKind::OpenCode, Some(p)) => {
            fs::read_to_string(p).is_ok_and(|s| s.contains("AgentGoodNight lifecycle forwarder"))
        }
        (_, Some(p)) => read_json(p).ok().flatten().is_some_and(|v| contains_our_hook(&v)),
        (_, None) => false,
    };
    let installed_at = fs::read(env.marker(agent))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("installed_at").and_then(Value::as_i64));
    IntegrationStatus {
        agent,
        mode: mode(agent),
        installed,
        config_path: path.map(|p| p.to_string_lossy().into_owned()),
        installed_at: if installed { installed_at } else { None },
    }
}

/// Installs (or refreshes) the integration. Returns the backup file, if any.
pub fn install(agent: AgentKind, env: &IntegrationEnv) -> Result<Option<PathBuf>, IntegrationError> {
    let path = env.config_path(agent).ok_or(IntegrationError::Unsupported(agent))?;
    if agent == AgentKind::Codex {
        return Err(IntegrationError::Unsupported(agent));
    }
    let hook = if agent == AgentKind::OpenCode { None } else { Some(deploy_hook(env)?) };
    // Parse before backing up so an unreadable config aborts untouched.
    let existing = if hook.is_some() { read_json(&path)? } else { None };
    let backup = backup(&path, env)?;
    match hook {
        None => {
            write_atomic(&path, opencode::plugin_source(&env.spool_dir).as_bytes())?;
        }
        Some(hook) => {
            let mut root = existing.unwrap_or_else(|| json!({}));
            if !root.is_object() {
                return Err(IntegrationError::InvalidConfig {
                    path: path.display().to_string(),
                    msg: "top level is not an object".into(),
                });
            }
            remove_our_hooks(&mut root);
            add_hooks(agent, &mut root, &hook, &env.data_dir);
            write_json(&path, &root)?;
        }
    }
    write_atomic(&env.marker(agent), json!({"installed_at": now_millis()}).to_string().as_bytes())?;
    Ok(backup)
}

pub fn uninstall(agent: AgentKind, env: &IntegrationEnv) -> Result<Option<PathBuf>, IntegrationError> {
    let path = env.config_path(agent).ok_or(IntegrationError::Unsupported(agent))?;
    let _ = fs::remove_file(env.marker(agent));
    if !path.exists() {
        return Ok(None);
    }
    let backup = backup(&path, env)?;
    match agent {
        AgentKind::OpenCode => {
            if fs::read_to_string(&path).is_ok_and(|s| s.contains("AgentGoodNight lifecycle forwarder")) {
                fs::remove_file(&path)?;
            }
        }
        AgentKind::Codex => return Err(IntegrationError::Unsupported(agent)),
        _ => {
            if let Some(mut root) = read_json(&path)? {
                if remove_our_hooks(&mut root) {
                    write_json(&path, &root)?;
                }
            }
        }
    }
    Ok(backup)
}

/// Copies the shipped hook binary to the stable location in the data dir.
pub fn deploy_hook(env: &IntegrationEnv) -> Result<PathBuf, IntegrationError> {
    let dst = env.installed_hook();
    let src = env.hook_source.clone().ok_or_else(|| IntegrationError::MissingHook("<unknown>".into()))?;
    if !src.exists() {
        return Err(IntegrationError::MissingHook(src.display().to_string()));
    }
    if src != dst {
        if let Some(dir) = dst.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = dst.with_extension("new");
        fs::copy(&src, &tmp)?;
        // Replacing a binary that a hook is executing right now can fail on
        // Windows. An identical copy is fine; a different (older) one would
        // be configured with arguments it may not understand: fail instead.
        if fs::rename(&tmp, &dst).is_err() {
            let same = fs::read(&tmp).ok().zip(fs::read(&dst).ok()).is_some_and(|(a, b)| a == b);
            let _ = fs::remove_file(&tmp);
            if !same {
                return Err(IntegrationError::HookInUse(dst.display().to_string()));
            }
        }
    }
    Ok(dst)
}

fn backup(path: &Path, env: &IntegrationEnv) -> io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let dir = env.data_dir.join("backups");
    fs::create_dir_all(&dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("config");
    let parent =
        path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("x").trim_start_matches('.');
    let dst = dir.join(format!("{}-{parent}-{name}", now_millis()));
    fs::copy(path, &dst)?;
    Ok(Some(dst))
}

fn read_json(path: &Path) -> Result<Option<Value>, IntegrationError> {
    match fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let text = text.trim_start_matches('\u{feff}');
            if text.trim().is_empty() {
                return Ok(Some(json!({})));
            }
            serde_json::from_str(text)
                .map(Some)
                .map_err(|e| IntegrationError::InvalidConfig { path: path.display().to_string(), msg: e.to_string() })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_json(path: &Path, v: &Value) -> io::Result<()> {
    let mut s = serde_json::to_string_pretty(v)?;
    s.push('\n');
    write_atomic(path, s.as_bytes())
}

fn is_ours(handler: &Value) -> bool {
    handler.get("command").and_then(Value::as_str).is_some_and(|c| c.contains(HOOK_BIN_STEM))
}

fn contains_our_hook(root: &Value) -> bool {
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else { return false };
    hooks.values().filter_map(Value::as_array).flatten().any(|entry| {
        is_ours(entry) || entry.get("hooks").and_then(Value::as_array).is_some_and(|hs| hs.iter().any(is_ours))
    })
}

/// Removes our handlers everywhere; drops groups/events that become empty.
/// Returns whether anything changed.
fn remove_our_hooks(root: &mut Value) -> bool {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else { return false };
    let mut changed = false;
    let mut empty_events = Vec::new();
    for (event, entries) in hooks.iter_mut() {
        let Some(list) = entries.as_array_mut() else { continue };
        let before = list.len();
        // Flat form (Cursor): handlers directly in the event array.
        list.retain(|e| !is_ours(e));
        // Grouped form (Claude, Gemini): { matcher, hooks: [...] }.
        for group in list.iter_mut() {
            if let Some(hs) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let n = hs.len();
                hs.retain(|h| !is_ours(h));
                if hs.len() != n {
                    changed = true;
                }
            }
        }
        list.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|hs| !hs.is_empty()));
        if list.len() != before {
            changed = true;
        }
        if list.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for e in empty_events {
        hooks.remove(&e);
        changed = true;
    }
    if hooks.is_empty() {
        if let Some(obj) = root.as_object_mut() {
            obj.remove("hooks");
        }
    }
    changed
}

fn add_hooks(agent: AgentKind, root: &mut Value, hook: &Path, data_dir: &Path) {
    let obj = root.as_object_mut().expect("checked object");
    let hooks = obj.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let hooks = hooks.as_object_mut().expect("object");
    let hook_s = hook.to_string_lossy().to_string();
    // The data dir is passed explicitly: the agent's environment may differ
    // from the app's (AGN_DATA_DIR).
    let data_s = data_dir.to_string_lossy().to_string();
    match agent {
        AgentKind::ClaudeCode => {
            for (event, run_async) in CLAUDE_EVENTS {
                let mut handler = json!({
                    "type": "command",
                    "command": hook_s,
                    "args": ["claude_code", "--data-dir", data_s],
                    "timeout": 10,
                });
                if *run_async {
                    handler["async"] = json!(true);
                }
                push_event(hooks, event, json!({ "hooks": [handler] }));
            }
        }
        AgentKind::GeminiCli => {
            for event in GEMINI_EVENTS {
                let handler = json!({
                    "type": "command",
                    "name": "agentgoodnight",
                    "command": format!("\"{hook_s}\" gemini_cli --data-dir \"{data_s}\""),
                    "timeout": 10000,
                    "description": "AgentGoodNight lifecycle monitor (no effect on the agent)",
                });
                push_event(hooks, event, json!({ "hooks": [handler] }));
            }
        }
        AgentKind::Cursor => {
            // Cursor runs hook commands through PowerShell on Windows.
            let command = if cfg!(windows) {
                let q = |s: &str| s.replace('\'', "''");
                format!("& '{}' cursor --data-dir '{}'", q(&hook_s), q(&data_s))
            } else {
                let q = |s: &str| s.replace('\'', "'\\''");
                format!("'{}' cursor --data-dir '{}'", q(&hook_s), q(&data_s))
            };
            for event in CURSOR_EVENTS {
                push_event(hooks, event, json!({ "command": command, "timeout": 10 }));
            }
        }
        _ => {}
    }
    if agent == AgentKind::Cursor {
        obj.entry("version").or_insert(json!(1));
    }
}

fn push_event(hooks: &mut Map<String, Value>, event: &str, entry: Value) {
    let list = hooks.entry(event.to_string()).or_insert_with(|| Value::Array(Vec::new()));
    if !list.is_array() {
        *list = Value::Array(Vec::new());
    }
    list.as_array_mut().expect("array").push(entry);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(dir: &Path) -> IntegrationEnv {
        let hook_src = dir.join("app").join(hook_bin_name());
        fs::create_dir_all(hook_src.parent().unwrap()).unwrap();
        fs::write(&hook_src, b"binary").unwrap();
        IntegrationEnv {
            claude_root: Some(dir.join(".claude")),
            gemini_root: Some(dir.join(".gemini")),
            cursor_root: Some(dir.join(".cursor")),
            opencode_root: Some(dir.join(".config/opencode")),
            data_dir: dir.join("data"),
            spool_dir: dir.join("data/spool"),
            hook_source: Some(hook_src),
        }
    }

    #[test]
    fn claude_install_preserves_existing_and_uninstall_restores() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let settings = dir.path().join(".claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let original = json!({
            "skipWorkflowUsageWarning": true,
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "notify-send done"}]}],
                      "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}]},
            "agentPushNotifEnabled": false
        });
        fs::write(&settings, serde_json::to_string_pretty(&original).unwrap()).unwrap();

        assert!(!status(AgentKind::ClaudeCode, &env).installed);
        let backup = install(AgentKind::ClaudeCode, &env).unwrap().expect("backup made");
        assert_eq!(serde_json::from_slice::<Value>(&fs::read(&backup).unwrap()).unwrap(), original);
        let st = status(AgentKind::ClaudeCode, &env);
        assert!(st.installed);
        assert!(st.installed_at.is_some());
        assert!(env.installed_hook().exists(), "hook deployed to data dir");

        let v: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(v["skipWorkflowUsageWarning"], true);
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "notify-send done");
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 2);
        let ours = &v["hooks"]["Stop"][1]["hooks"][0];
        assert_eq!(ours["args"][0], json!("claude_code"));
        assert_eq!(ours["args"][1], json!("--data-dir"));
        assert!(ours.get("async").is_none());
        assert_eq!(v["hooks"]["PostToolUse"][0]["hooks"][0]["async"], true);
        // Key order preserved (preserve_order feature).
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["skipWorkflowUsageWarning", "hooks", "agentPushNotifEnabled"]);

        // Idempotent.
        install(AgentKind::ClaudeCode, &env).unwrap();
        let v2: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(v2["hooks"]["Stop"].as_array().unwrap().len(), 2);

        uninstall(AgentKind::ClaudeCode, &env).unwrap();
        let restored: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(restored, original);
        assert!(!status(AgentKind::ClaudeCode, &env).installed);
    }

    #[test]
    fn install_into_missing_file_and_remove_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        assert_eq!(install(AgentKind::GeminiCli, &env).unwrap(), None);
        let p = dir.path().join(".gemini/settings.json");
        let v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert!(
            v["hooks"]["AfterAgent"][0]["hooks"][0]["command"].as_str().unwrap().contains("\" gemini_cli --data-dir")
        );
        uninstall(AgentKind::GeminiCli, &env).unwrap();
        let v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v, json!({}));
    }

    #[test]
    fn cursor_flat_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let p = dir.path().join(".cursor/hooks.json");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, r#"{"version":1,"hooks":{"stop":[{"command":"./audit.sh"}]}}"#).unwrap();
        install(AgentKind::Cursor, &env).unwrap();
        let v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["hooks"]["stop"].as_array().unwrap().len(), 2);
        assert!(status(AgentKind::Cursor, &env).installed);
        uninstall(AgentKind::Cursor, &env).unwrap();
        let v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v, json!({"version":1,"hooks":{"stop":[{"command":"./audit.sh"}]}}));
    }

    #[test]
    fn opencode_plugin_file() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        install(AgentKind::OpenCode, &env).unwrap();
        assert!(status(AgentKind::OpenCode, &env).installed);
        uninstall(AgentKind::OpenCode, &env).unwrap();
        assert!(!env.config_path(AgentKind::OpenCode).unwrap().exists());
    }

    #[test]
    fn invalid_json_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let p = dir.path().join(".claude/settings.json");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"{ // comment\n}").unwrap();
        assert!(matches!(install(AgentKind::ClaudeCode, &env), Err(IntegrationError::InvalidConfig { .. })));
        assert_eq!(fs::read(&p).unwrap(), b"{ // comment\n}");
    }

    #[test]
    fn missing_hook_binary_aborts_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env(dir.path());
        env.hook_source = Some(dir.path().join("nope.exe"));
        assert!(matches!(install(AgentKind::ClaudeCode, &env), Err(IntegrationError::MissingHook(_))));
        assert!(!dir.path().join(".claude/settings.json").exists());
    }

    #[test]
    fn codex_is_passive() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        assert!(status(AgentKind::Codex, &env).installed);
        assert_eq!(status(AgentKind::Codex, &env).mode, IntegrationMode::Passive);
        assert!(install(AgentKind::Codex, &env).is_err());
    }
}
