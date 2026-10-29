//! Verifies an integration against a real tool installation without touching
//! this machine's configuration: everything lives below a sandbox home that
//! the tool under test is pointed at (e.g. `USERPROFILE`/`HOME`/`XDG_*`).
//!
//! cargo run -p agn-adapters --example sandbox -- install   <agent> <home> <data-dir>
//! cargo run -p agn-adapters --example sandbox -- uninstall <agent> <home> <data-dir>
//! cargo run -p agn-adapters --example sandbox -- watch     <agent> <home> <data-dir> [seconds]
//!
//! `watch` runs the real monitor (real process scan, DryRun power, never
//! armed) and prints every change of that tool's sessions, observability and
//! untracked processes. `<agent>` is an id such as `gemini_cli`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agn_adapters::integration::{self, IntegrationEnv, hook_bin_name};
use agn_adapters::monitor::{Monitor, MonitorPaths, ProcessSource};
use agn_adapters::process::ProcessScanner;
use agn_core::config::Settings;
use agn_core::model::AgentKind;
use agn_core::power::PowerBackend;
use agn_core::time::now_millis;

fn env_for(home: &Path, data: &Path) -> IntegrationEnv {
    // Examples live in target/<profile>/examples; the hook next to them.
    let hook = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent()?.parent().map(|d| d.join(hook_bin_name())))
        .filter(|p| p.is_file());
    IntegrationEnv {
        claude_root: Some(home.join(".claude")),
        gemini_root: Some(home.join(".gemini")),
        cursor_root: Some(home.join(".cursor")),
        opencode_root: Some(home.join(".config").join("opencode")),
        data_dir: data.to_path_buf(),
        spool_dir: data.join("spool"),
        hook_source: hook,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("usage: sandbox install|uninstall|watch <agent> <home> <data-dir> [seconds]");
        std::process::exit(2);
    }
    let Some(agent) = AgentKind::ALL.into_iter().find(|k| k.id() == args[1]) else {
        eprintln!("unknown agent {}", args[1]);
        std::process::exit(2);
    };
    let home = PathBuf::from(&args[2]);
    let data = PathBuf::from(&args[3]);
    let env = env_for(&home, &data);
    match args[0].as_str() {
        "install" => match integration::install(agent, &env) {
            Ok(p) => println!("installed: {p:?}"),
            Err(e) => {
                eprintln!("install failed: {e}");
                std::process::exit(1);
            }
        },
        "uninstall" => match integration::uninstall(agent, &env) {
            Ok(p) => println!("removed: {p:?}"),
            Err(e) => {
                eprintln!("uninstall failed: {e}");
                std::process::exit(1);
            }
        },
        "watch" => watch(agent, &home, env, args.get(4).and_then(|s| s.parse().ok()).unwrap_or(120)),
        other => {
            eprintln!("unknown command {other}");
            std::process::exit(2);
        }
    }
}

fn watch(agent: AgentKind, home: &Path, env: IntegrationEnv, secs: u64) {
    let settings = Settings {
        agents: AgentKind::ALL.into_iter().map(|k| (k, k == agent)).collect(),
        settle_secs: 15,
        ..Settings::default()
    };
    // Claude Code / Codex logs are read from the sandbox home as well.
    let claude = (agent == AgentKind::ClaudeCode).then(|| home.join(".claude"));
    let codex = (agent == AgentKind::Codex).then(|| home.join(".codex"));
    let paths = MonitorPaths {
        data_dir: env.data_dir.clone(),
        claude_root: claude.clone(),
        codex_root: codex.clone(),
        default_claude_root: claude,
        default_codex_root: codex,
        integration: env,
    };
    let mut m = Monitor::new(settings, paths, ProcessSource::System(ProcessScanner::new()), PowerBackend::DryRun);
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < Duration::from_secs(secs) {
        let now = now_millis();
        m.cycle(now);
        m.commit_spool();
        let snap = m.snapshot(now);
        let mut lines = Vec::new();
        for a in snap.adapters.iter().filter(|a| a.agent == agent) {
            lines.push(format!(
                "adapter observable={} code={} installed={} procs={}",
                a.observable, a.code, a.integration.installed, a.processes
            ));
        }
        for u in snap.untracked.iter().filter(|u| u.agent == agent) {
            lines.push(format!("untracked pid={} {}", u.pid, u.name));
        }
        for s in snap.sessions.iter().filter(|s| s.agent == agent) {
            lines.push(format!(
                "session {} project={} state={} reason={} pending={:?}",
                s.id,
                s.project.clone().unwrap_or_default(),
                s.state.id(),
                s.reason.clone().unwrap_or_default(),
                s.pending
            ));
        }
        lines.push(format!("verdict {:?}", snap.verdict));
        let text = lines.join("\n");
        if text != last {
            println!("--- t+{:.0}s\n{text}", start.elapsed().as_secs_f64());
            last = text;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
