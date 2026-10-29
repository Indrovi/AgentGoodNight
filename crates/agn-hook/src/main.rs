//! `agentgoodnight-hook [--data-dir <dir>] [agent-id]` — invoked by an
//! agent's lifecycle hook.
//!
//! Reads the hook payload from stdin, stores a scrubbed copy in the
//! AgentGoodNight spool and exits 0. It never prints anything that could
//! change the agent's behaviour (Cursor gets the mandatory `{}`).

// No console window may flash up when an agent spawns the hook on Windows;
// inherited stdin/stdout pipes keep working in the GUI subsystem.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::Write;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut data_dir: Option<PathBuf> = None;
    let mut agent: Option<String> = None;
    while let Some(a) = args.next() {
        if a == "--data-dir" {
            data_dir = args.next().map(PathBuf::from);
        } else if agent.is_none() {
            agent = Some(a);
        }
    }
    let data_dir = data_dir.or_else(installed_data_dir).unwrap_or_else(agn_core::paths::data_dir);
    let ancestors = agn_adapters::process::ancestors_of_self(8);
    let outcome = agn_adapters::hook::run(agent.as_deref(), &mut std::io::stdin().lock(), &data_dir, ancestors);
    if let Some(out) = outcome.stdout {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
    std::process::exit(0);
}

/// The installer copies the hook to `<data_dir>/bin/`; use that data dir
/// even if the agent's environment differs from the app's.
fn installed_data_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bin = exe.parent()?;
    let dir = bin.parent()?;
    (bin.file_name()? == "bin" && dir.join("spool").is_dir()).then(|| dir.to_path_buf())
}
