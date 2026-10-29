//! Agent process detection. Processes are *only* used to find work we cannot
//! see (an agent running without a working integration) and to notice that a
//! tracked session's process vanished. A process exit is never treated as a
//! successful completion.

use std::path::Path;

use agn_core::model::AgentKind;
use agn_core::time::Millis;
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRole {
    /// A process that runs one agent session (CLI, desktop-spawned agent).
    Session,
    /// A long-lived host serving many threads (Codex app-server).
    Host,
    /// An editor that may or may not run an agent (Cursor).
    Editor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProcess {
    pub agent: AgentKind,
    pub pid: u32,
    pub start_time: Millis,
    pub name: String,
    pub role: ProcessRole,
    pub origin: Option<String>,
    /// Working directory, when the OS lets us read it.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Default)]
pub struct ProcessScanner {
    sys: Option<Box<System>>,
    wsl_vm: bool,
}

impl ProcessScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the last scan saw a running WSL 2 virtual machine.
    pub fn wsl_vm(&self) -> bool {
        self.wsl_vm
    }

    pub fn scan(&mut self) -> Result<Vec<AgentProcess>, String> {
        if self.sys.is_none() {
            self.sys = Some(Box::new(System::new().map_err(|e| e.to_string())?));
        }
        let sys = self.sys.as_mut().ok_or("process scanner unavailable")?;
        let kind = ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_cwd(UpdateKind::OnlyIfNotSet);
        let n = sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        if n == 0 && sys.processes().is_empty() {
            return Err("process list is empty".into());
        }
        let mut out = Vec::new();
        let mut wsl_vm = false;
        for (pid, p) in sys.processes() {
            let name = p.name().to_string_lossy().to_string();
            wsl_vm |= is_wsl_vm(&name);
            let cmd: Vec<String> = p.cmd().iter().map(|s| s.to_string_lossy().to_string()).collect();
            if let Some((agent, role, origin)) = classify(&name, p.exe(), &cmd) {
                out.push(AgentProcess {
                    agent,
                    pid: pid.as_u32(),
                    start_time: p.start_time() as Millis * 1000,
                    name,
                    role,
                    origin,
                    cwd: p.cwd().map(|c| c.to_string_lossy().into_owned()).filter(|c| !c.is_empty()),
                });
            }
        }
        self.wsl_vm = wsl_vm;
        out.sort_by_key(|p| (p.agent, p.pid));
        Ok(out)
    }
}

/// The utility VM that hosts every WSL 2 distribution (Windows 11: VmmemWSL).
pub fn is_wsl_vm(name: &str) -> bool {
    let n = name.to_lowercase();
    n == "vmmemwsl" || n == "vmmemwsl.exe"
}

/// Compares directories the way the OS does (case-insensitive on Windows).
pub fn same_dir(a: &str, b: &str) -> bool {
    let norm = |p: &str| {
        let p = p.replace('\\', "/");
        let p = p.trim_end_matches('/').to_string();
        if cfg!(windows) { p.to_lowercase() } else { p }
    };
    norm(a) == norm(b)
}

/// Maps a process to an agent. Pure, so it is unit-tested on all platforms.
pub fn classify(name: &str, exe: Option<&Path>, cmd: &[String]) -> Option<(AgentKind, ProcessRole, Option<String>)> {
    let base = name.to_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base).to_string();
    let exe_s = exe.map(|e| e.to_string_lossy().to_lowercase().replace('\\', "/")).unwrap_or_default();
    let cmd_s = cmd.join(" ").to_lowercase().replace('\\', "/");
    // Electron/Chromium helper processes (renderer, gpu, utility, crashpad).
    if cmd_s.contains("--type=") {
        return None;
    }
    match base.as_str() {
        "claude" => {
            if exe_s.contains("claude-code") || cmd_s.contains("claude-code/") {
                Some((AgentKind::ClaudeCode, ProcessRole::Session, Some("desktop".into())))
            } else if exe_s.contains("windowsapps/claude_")
                || exe_s.contains("/anthropicclaude/")
                || exe_s.contains("/claude.app/contents/macos/claude")
            {
                None // the Claude Desktop app itself
            } else {
                Some((AgentKind::ClaudeCode, ProcessRole::Session, Some("cli".into())))
            }
        }
        "codex" => {
            let role = if ["app-server", "exec-server", "mcp-server", "proto"].iter().any(|s| cmd_has_word(cmd, s)) {
                ProcessRole::Host
            } else {
                ProcessRole::Session
            };
            Some((AgentKind::Codex, role, None))
        }
        "gemini" => Some((AgentKind::GeminiCli, ProcessRole::Session, None)),
        "opencode" => Some((AgentKind::OpenCode, ProcessRole::Session, None)),
        "cursor-agent" | "agent" if base == "cursor-agent" || exe_s.contains("cursor-agent") => {
            Some((AgentKind::Cursor, ProcessRole::Session, Some("cli".into())))
        }
        "cursor" => Some((AgentKind::Cursor, ProcessRole::Editor, Some("editor".into()))),
        "node" | "bun" | "deno" => {
            if cmd_s.contains("@anthropic-ai/claude-code") || cmd_s.contains("claude-code/cli") {
                Some((AgentKind::ClaudeCode, ProcessRole::Session, Some("cli".into())))
            } else if cmd_s.contains("@google/gemini-cli")
                || cmd_s.contains("gemini-cli/")
                || cmd_ends_with_bin(cmd, "gemini")
            {
                Some((AgentKind::GeminiCli, ProcessRole::Session, None))
            } else if cmd_s.contains("cursor-agent") {
                Some((AgentKind::Cursor, ProcessRole::Session, Some("cli".into())))
            } else if cmd_s.contains("opencode-ai") || cmd_ends_with_bin(cmd, "opencode") {
                Some((AgentKind::OpenCode, ProcessRole::Session, None))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn cmd_has_word(cmd: &[String], word: &str) -> bool {
    cmd.iter().skip(1).any(|a| a == word)
}

fn cmd_ends_with_bin(cmd: &[String], bin: &str) -> bool {
    cmd.iter().skip(1).take(2).any(|a| {
        let a = a.to_lowercase().replace('\\', "/");
        a.ends_with(&format!("/bin/{bin}")) || a.ends_with(&format!("/{bin}.js")) || a == bin
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn c(name: &str, exe: &str, cmd: &[&str]) -> Option<(AgentKind, ProcessRole)> {
        let cmd: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
        let exe = PathBuf::from(exe);
        classify(name, Some(&exe), &cmd).map(|(a, r, _)| (a, r))
    }

    #[test]
    fn claude_desktop_vs_claude_code() {
        let app = r"C:\Program Files\WindowsApps\Claude_4.29.0.0_x64__pzs8sxrjxfjjc\app\Claude.exe";
        assert_eq!(c("claude.exe", app, &[app]), None, "desktop main process");
        assert_eq!(c("claude.exe", app, &[app, "--type=renderer"]), None, "electron helper");
        let cc = r"C:\Users\u\AppData\Roaming\Claude\claude-code\2.1.309\9078\claude.exe";
        assert_eq!(
            c("claude.exe", cc, &[cc, "--output-format", "stream-json"]),
            Some((AgentKind::ClaudeCode, ProcessRole::Session))
        );
        assert_eq!(
            c("claude", "/home/u/.local/bin/claude", &["claude"]),
            Some((AgentKind::ClaudeCode, ProcessRole::Session))
        );
        assert_eq!(c("Claude", "/Applications/Claude.app/Contents/MacOS/Claude", &[]), None);
        assert_eq!(
            c("node", "/usr/bin/node", &["node", "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"]),
            Some((AgentKind::ClaudeCode, ProcessRole::Session))
        );
    }

    #[test]
    fn codex_roles() {
        let exe = r"C:\Users\u\AppData\Local\OpenAI\Codex\bin\x\codex.exe";
        assert_eq!(c("codex.exe", exe, &[exe, "-c", "x=1", "app-server"]), Some((AgentKind::Codex, ProcessRole::Host)));
        assert_eq!(c("codex.exe", exe, &[exe, "exec-server", "--remote"]), Some((AgentKind::Codex, ProcessRole::Host)));
        assert_eq!(c("codex", "/usr/local/bin/codex", &["codex"]), Some((AgentKind::Codex, ProcessRole::Session)));
        assert_eq!(c("codex-windows-sandbox-service.exe", "", &[]), None);
    }

    #[test]
    fn others() {
        assert_eq!(
            c("node", "/usr/bin/node", &["node", "/usr/local/lib/node_modules/@google/gemini-cli/dist/index.js"]),
            Some((AgentKind::GeminiCli, ProcessRole::Session))
        );
        assert_eq!(c("node", "/usr/bin/node", &["node", "server.js"]), None);
        assert_eq!(c("Cursor.exe", r"C:\Cursor\Cursor.exe", &[]), Some((AgentKind::Cursor, ProcessRole::Editor)));
        assert_eq!(c("cursor-agent", "/usr/bin/cursor-agent", &[]), Some((AgentKind::Cursor, ProcessRole::Session)));
        assert_eq!(c("opencode", "/usr/bin/opencode", &[]), Some((AgentKind::OpenCode, ProcessRole::Session)));
        assert_eq!(c("explorer.exe", r"C:\Windows\explorer.exe", &[]), None);
    }

    #[test]
    fn wsl_and_dirs() {
        assert!(is_wsl_vm("VmmemWSL"));
        assert!(!is_wsl_vm("vmmem"));
        assert!(same_dir("/work/api/", "/work/api"));
        if cfg!(windows) {
            assert!(same_dir(r"C:\Work\API", "c:/work/api"));
        }
    }

    #[test]
    fn live_scan_does_not_fail() {
        // Read-only enumeration of the current machine's processes.
        let procs = ProcessScanner::new().scan();
        assert!(procs.is_ok(), "{procs:?}");
    }
}
