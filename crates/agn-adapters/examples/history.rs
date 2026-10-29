//! Read-only parser check against all local history: replays every Codex
//! rollout and Claude Code transcript through the adapters and the tracker
//! and prints the resulting final states.
//!
//! cargo run -p agn-adapters --example history

use std::collections::BTreeMap;
use std::path::Path;

use agn_adapters::claude::LineParser;
use agn_adapters::codex::{RolloutParser, session_id_from_name};
use agn_adapters::tail::FileTail;
use agn_adapters::{claude, codex};
use agn_core::time::now_millis;
use agn_core::tracker::{Tracker, TrackerConfig};

fn walk(dir: &Path, depth: usize, pred: &dyn Fn(&str) -> bool, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth > 0 {
                walk(&p, depth - 1, pred, out);
            }
        } else if p.file_name().and_then(|n| n.to_str()).is_some_and(pred) {
            out.push(p);
        }
    }
}

fn main() {
    let now = now_millis();
    let mut tracker = Tracker::new(TrackerConfig::default());
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    if let Some(root) = codex::default_root() {
        let mut files = Vec::new();
        for d in ["sessions", "archived_sessions"] {
            walk(&root.join(d), 4, &|n| n.starts_with("rollout-") && n.contains(".jsonl"), &mut files);
        }
        for f in &files {
            let mut tail = FileTail::new(f.clone());
            let mut parser = RolloutParser::new(session_id_from_name(f));
            let mut events = Vec::new();
            match tail.read_new(u64::MAX / 4) {
                Ok(read) => read.lines.iter().for_each(|l| parser.parse(l, &mut events)),
                Err(e) => println!("codex read error {}: {e}", f.display()),
            }
            *counts.entry("codex events".to_string()).or_default() += events.len();
            tracker.apply_all(events, now);
        }
        println!("codex rollouts: {}", files.len());
    }

    if let Some(root) = claude::default_root() {
        let mut files = Vec::new();
        walk(&root.join("projects"), 1, &|n| n.ends_with(".jsonl"), &mut files);
        for f in &files {
            let sid = f.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
            let mut tail = FileTail::new(f.clone());
            let mut parser = LineParser::default();
            let mut events = Vec::new();
            // Same initial window the live adapter uses.
            match tail.read_new(4 * 1024 * 1024) {
                Ok(read) => read.lines.iter().for_each(|l| parser.parse(&sid, l, &mut events)),
                Err(e) => println!("claude read error {}: {e}", f.display()),
            }
            *counts.entry("claude events".into()).or_default() += events.len();
            tracker.apply_all(events, now);
        }
        println!("claude transcripts: {}", files.len());
    }

    tracker.tick(now + 10 * 60 * 1000);
    for s in tracker.sessions() {
        *counts.entry(format!("{} {}", s.key.agent.id(), s.state.id())).or_default() += 1;
        if s.state.is_blocking() {
            println!(
                "  blocking: {:<11} {:<22} {:<22} reason={:?} pending={} idle={}h",
                s.key.agent.id(),
                s.project_label().unwrap_or_default(),
                s.state.id(),
                s.reason,
                s.background.len(),
                (now - s.last_activity) / 3_600_000
            );
        }
    }
    for (k, v) in counts {
        println!("{k}: {v}");
    }
}
