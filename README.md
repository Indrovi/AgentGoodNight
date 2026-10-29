# AgentGoodNight

**Sleep or shut down your computer once your AI coding agents have _provably_ finished — never on a timer.**

You start Claude Code, Codex or Gemini on a long task and go to bed. AgentGoodNight watches the
agents' own lifecycle signals and only acts after every tracked run has recorded its completion.
If anything is still running, waiting for you, rate-limited, scheduled to continue or simply
unknown, the computer stays on.

- Rust + Tauri 2, tiny dark window, tray icon, background monitoring
- Windows, macOS, Linux; fully local — no cloud, no telemetry, no AI APIs
- UI in Czech and English (i18n-ready)
- MIT licensed

## How it decides

AgentGoodNight never infers completion from a PID, a process exit, CPU usage or idleness.
Completion comes only from an explicit end-of-work event emitted by the agent
(e.g. Claude Code's `Stop` hook / `stop_hook_summary`, Codex's `task_complete`), followed by a
quiet settle window that absorbs stop-hook continuations such as `/goal`.

The action runs only when **all** of these hold, and they are checked twice — the second time on a
fresh full rescan at the end of the 60 s countdown:

1. Monitoring is switched on (it can be switched on before the first run; it is never persisted
   across restarts and switches itself off after acting).
2. At least one task completion was recorded after monitoring was switched on.
3. No relevant session is *running, finishing, waiting for input, awaiting continuation*
   (background tasks, subagents, scheduled wake-ups, cron loops, active goals),
   *reconnecting, rate-limited, errored, interrupted* or *unknown*.
4. Every enabled tool can actually be observed. A tool that runs without a working integration,
   an unreadable data directory or a failed process scan **blocks** the action.
5. No agent process is running that cannot be attributed to a tracked session.

New activity cancels the countdown, and so does a time gap such as the machine having slept: after
wake-up only completions recorded from then on count. Shutdown never force-closes applications
(apps may veto it).
Individual sessions or processes can be excluded from the decision explicitly in the UI.

## Supported tools

| Tool | Variants | Evidence | Setup | Status |
|------|----------|----------|-------|--------|
| Claude Code | CLI, IDE, Claude Desktop "Code" tab | transcripts in `~/.claude/projects` (end of turn, `/goal`, background tasks, interrupts, usage limits) + process registry; optional lifecycle hooks add `background_tasks`/`session_crons` and `StopFailure` | none (hooks optional, one click) | verified on Windows with Claude Code 2.1.309 |
| Codex | CLI, IDE extension, Codex desktop app | rollout files in `~/.codex/sessions` (`task_started`, `task_complete`, `turn_aborted`, goals, rate limits) | none | parser verified against real rollouts; approval waits are not persisted and simply count as running |
| Gemini CLI | CLI ≥ 0.26 | lifecycle hooks (`BeforeAgent`/`AfterAgent`, tools, `ToolPermission`) | hooks (one click) | experimental — not verified on a real installation |
| Cursor | editor ≥ 1.7, `cursor-agent` | hooks (`stop` with status, `sessionEnd`) | hooks (one click) | experimental; the CLI in `-p` mode emits no `stop` |
| OpenCode | 1.x | plugin forwarding `session.status` / errors / permissions | plugin (one click) | experimental; 2.x plugin API not supported yet |

Without the integration a running Gemini, Cursor or OpenCode process blocks the action, unless you
turn that tool's tracking off.

### Integrations are opt-in and reversible

Nothing is installed automatically. "Connect" shows the exact file that will change, backs it up to
the AgentGoodNight data directory, merges entries pointing at the bundled `agentgoodnight-hook`
binary and leaves every other setting untouched. "Disconnect" removes exactly those entries.
The hook stores a scrubbed copy of the event (no prompts, no model output, no tool output) and
prints nothing that could steer the agent.

## Install

Releases are published as unsigned draft builds after review. Until then, build from source.

```bash
# prerequisites: Rust (stable), Node 18+, Tauri 2 system deps (https://tauri.app/start/prerequisites/)
scripts/build-hook.sh
npx @tauri-apps/cli@2 build --config src-tauri/tauri.release.conf.json --features real-power
```

Without `--features real-power` the build contains only the simulated power manager.

## Development and safety

- `cargo test --workspace` runs unit tests and overnight simulations (concurrent sessions,
  `/goal`, background tasks, rate limits, late log lines, countdown cancellation, ...).
- The real OS power backend is compiled **only** in release builds with the `real-power` feature.
  Every debug, test and `tauri dev` build physically lacks that code and uses a DryRun manager that
  only records what it would have done (the UI shows a *DRY RUN* badge).
- `cargo run -p agn-adapters --example probe` prints what the monitor sees on your machine,
  read-only.

```
crates/agn-core      session model, tracker, safety policy, countdown, power backends, spool
crates/agn-adapters  per-tool adapters, process scanning, hook installer, monitor loop
crates/agn-hook      the tiny executable agents call from their hooks
src-tauri            Tauri shell: window, tray, commands
ui                   static HTML/CSS/JS, translations in ui/i18n
```

Data lives in `%LOCALAPPDATA%\AgentGoodNight`, `~/Library/Application Support/AgentGoodNight`
or `~/.local/share/AgentGoodNight` (`AGN_DATA_DIR` overrides it).

## Known limitations

- Claude Code transcripts are an internal format; a format change makes sessions look *unknown*
  or *running* (the safe direction) until the adapter is updated.
- A session whose app restarts mid-goal may need up to the last 32 MB of transcript to recover
  the `/goal` state.
- Codex does not persist approval prompts, stream errors or reconnects; such turns stay *running*.
- Only local agents are tracked: Claude Desktop chats and Cowork tasks, Codex cloud tasks and
  Cursor background (cloud) agents are out of scope.
- Gemini's `AfterAgent` may also fire after a stream error.
- A build is not proof of correct behaviour on every OS; Linux and macOS power actions are
  untested on real hardware so far.

Future ideas: estimated energy savings, more tools.

## License

[MIT](LICENSE)
