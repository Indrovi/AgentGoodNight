//! Read-only probe: runs the monitor against this machine's real agent data
//! and prints what it sees. Uses a throw-away data directory, never arms
//! monitoring and never touches agent configuration.
//!
//! cargo run -p agn-adapters --example probe

use agn_adapters::integration::IntegrationEnv;
use agn_adapters::monitor::{Monitor, MonitorPaths, ProcessSource};
use agn_adapters::process::ProcessScanner;
use agn_adapters::{claude, codex};
use agn_core::config::Settings;
use agn_core::power::PowerBackend;
use agn_core::time::now_millis;

fn main() {
    let data = std::env::temp_dir().join(format!("agn-probe-{}", std::process::id()));
    let paths = MonitorPaths {
        data_dir: data.clone(),
        claude_root: claude::default_root(),
        codex_root: codex::default_root(),
        integration: IntegrationEnv::system(data.clone(), data.join("spool"), None),
    };
    let mut m =
        Monitor::new(Settings::default(), paths, ProcessSource::System(ProcessScanner::new()), PowerBackend::DryRun);
    let t0 = std::time::Instant::now();
    m.cycle(now_millis());
    let first = t0.elapsed();
    let t1 = std::time::Instant::now();
    m.cycle(now_millis());
    let incremental = t1.elapsed();
    let snap = m.snapshot(now_millis());
    println!("first cycle {first:?}, incremental {incremental:?}");
    println!("verdict (disarmed): {:?}", snap.verdict);
    for a in &snap.adapters {
        println!(
            "adapter {:<12} enabled={} observable={} code={} integration={:?} installed={} procs={}",
            a.name, a.enabled, a.observable, a.code, a.integration.mode, a.integration.installed, a.processes
        );
    }
    for u in &snap.untracked {
        println!("untracked {:?} pid={} {}", u.agent, u.pid, u.name);
    }
    let now = now_millis();
    for s in &snap.sessions {
        println!(
            "{:<11} {:<22} {:<10} {:<24} reason={:<18} origin={:<8} idle={:>5}s pending={}",
            s.agent.display_name(),
            s.project.clone().unwrap_or_default(),
            &s.id[..s.id.len().min(8)],
            s.state.id(),
            s.reason.clone().unwrap_or_default(),
            s.origin.clone().unwrap_or_default(),
            (now - s.last_activity) / 1000,
            s.pending.len()
        );
    }
    let _ = std::fs::remove_dir_all(data);
}
