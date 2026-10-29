//! `agentgoodnight-hook [agent-id]` — invoked by an agent's lifecycle hook.
//!
//! Reads the hook payload from stdin, stores a scrubbed copy in the
//! AgentGoodNight spool and exits 0. It never prints anything that could
//! change the agent's behaviour (Cursor gets the mandatory `{}`).

// No console window may flash up when an agent spawns the hook on Windows;
// inherited stdin/stdout pipes keep working in the GUI subsystem.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::Write;

fn main() {
    let agent = std::env::args().nth(1);
    let data_dir = agn_core::paths::data_dir();
    let outcome = agn_adapters::hook::run(agent.as_deref(), &mut std::io::stdin().lock(), &data_dir);
    if let Some(out) = outcome.stdout {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
    std::process::exit(0);
}
