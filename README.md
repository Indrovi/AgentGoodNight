# AgentGoodNight

**Sleep or shut down your computer once your AI coding agents have _provably_ finished — never on a timer.**

You start Claude Code, Codex or Gemini on a long task and go to bed. AgentGoodNight watches the
agents' own lifecycle signals and only acts after every tracked run has recorded its completion.
If anything is still running, waiting for you, rate-limited, scheduled to continue or simply
unknown, the computer stays on.

- Rust + Tauri 2, tiny dark window, tray icon, background monitoring (closing the window releases
  the webview; the monitor itself needs a few MB of RAM and almost no CPU)
- Windows, macOS, Linux; fully local — no cloud, no telemetry, no AI APIs
- UI in Czech and English (i18n-ready)
- MIT licensed

## How it decides

AgentGoodNight never infers completion from a PID, a process exit, CPU usage or idleness.
Completion comes only from an explicit end-of-work event emitted by the agent
(e.g. Claude Code's `stop_hook_summary`, Codex's `task_complete`), followed by a quiet settle
window that absorbs stop-hook continuations such as `/goal`:

- **Confirmed end** (the tool says all its stop hooks have run): 90 s settle.
- **Unconfirmed end** (Claude Code's final `end_turn` message, or its `Stop` hook, which runs
  alongside other Stop hooks that may still block): 11 min settle, or the longest Stop-hook
  timeout configured in the user, project, plugin or managed settings plus one minute. A settle
  window is never shortened except by a confirmed end.

The action runs only when **all** of these hold, and they are checked twice — the second time on a
fresh full rescan at the end of the 60 s countdown:

1. Monitoring is switched on (it can be switched on before the first run; it is never persisted
   across restarts and switches itself off after acting).
2. At least one task completion was recorded after monitoring was switched on.
3. No relevant session is *running, finishing, waiting for input, awaiting continuation*
   (background tasks, subagents, scheduled wake-ups, cron loops, active goals),
   *reconnecting, rate-limited, errored, interrupted* or *unknown*.
4. Every enabled tool can actually be observed. A tool that runs without a working integration,
   an unreadable data directory, a rejected hook record or a failed process scan **blocks** the
   action. On Windows a running WSL 2 VM blocks too (agents inside it are invisible), unless
   turned off in the tools panel.
5. No agent process is running that cannot be attributed to a tracked session:
   - a Claude Code process needs its registry entry and a tracked session;
   - a Codex CLI process needs its own rollout, active since it started, in its directory;
   - a Gemini, Cursor or OpenCode process needs a hook record produced by that very process
     (or its launcher/helper) since it started;
   - a process whose `CLAUDE_CONFIG_DIR` / `CODEX_HOME` points somewhere else always blocks.

A completion counts only as of the time its evidence says, never as of the moment it was
discovered, and evidence dated in the future (a clock turned back) never completes anything.
After a restart the log readers continue from their saved positions, so history is never
replayed. New activity cancels the countdown, and so does a time gap of more than 15 s (e.g. the
machine slept): after wake-up only completions recorded from then on count. A clock that jumps
backwards switches monitoring off. A one-shot wake-up or cron job counts as fired only when a new
turn starts after it was due. Right before the action is handed to the OS, the verdict is
evaluated once more, so a Cancel or any change made during the last rescan still wins. Shutdown
never force-closes applications (apps may veto it).

Individual sessions or processes can be excluded explicitly in the UI. An excluded session is
tracked again as soon as it starts new work; the exclusion survives restarts.

## Supported tools

| Tool | Variants | Evidence | Setup | Status |
|------|----------|----------|-------|--------|
| Claude Code | CLI, IDE, Claude Desktop "Code" tab | transcripts in `~/.claude/projects` (end of turn, `/goal`, background tasks, crons, interrupts, usage limits) + subagent/workflow writes + process registry; optional lifecycle hooks add the authoritative `background_tasks`/`session_crons` list and `StopFailure` | none (hooks optional, one click) | verified on Windows with Claude Code 2.1.309 |
| Codex | CLI, IDE extension, Codex desktop app | rollout files in `~/.codex/sessions` (`task_started`, `task_complete`, `turn_aborted`, goals, rate limits) | none | parser verified against real rollouts; approval waits are not persisted and simply count as running |
| Gemini CLI | CLI (verified with 0.66) | lifecycle hooks (`BeforeAgent`/`AfterAgent`, tools, `ToolPermission`) | hooks (one click); hooks only run in trusted folders | experimental; hooks and a full turn verified on Windows, permission prompts not yet |
| Cursor | editor; `cursor-agent` only blocks | hooks (`stop` with status, `sessionEnd`) | hooks (one click) | experimental — checked against the docs, not on a real installation; a running `cursor-agent` always blocks |
| OpenCode | 1.x (verified with 1.19) | plugin forwarding `session.status` / errors / permissions | plugin (one click) | experimental; verified on Windows with a local mock model |

Without the integration a running Gemini, Cursor or OpenCode process blocks the action, unless you
turn that tool's tracking off. The exact event-to-state mapping per tool is in
[docs/adapters.md](docs/adapters.md).

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

- `cargo test --workspace` runs unit tests, overnight simulations (concurrent sessions, `/goal`,
  background tasks, rate limits, late log lines, sleep/wake, clock jumps, restarts, countdown
  cancellation, ...) and a regression test for every finding of three independent adversarial
  safety reviews (`crates/agn-adapters/tests/review_regressions.rs`).
- The real OS power backend is compiled **only** in release builds with the `real-power` feature.
  Every debug or test build and any build without that feature physically lacks that code and uses
  a DryRun manager that only records what it would have done (the UI shows a *DRY RUN* badge).
- `cargo run -p agn-adapters --example probe` prints what the monitor sees on your machine,
  read-only.
- `node scripts/ui-e2e.mjs` (Windows; Linux through `tauri-driver` and WebKitWebDriver) drives
  the real window of the debug build through the whole overnight flow against a sandbox;
  `scripts/smoke-app.sh` checks that a built app opens its window. What is verified where, and the
  checklist for a first real power test, are in [docs/testing.md](docs/testing.md).

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
- A Claude Code CLI session without any Stop hooks writes no end-of-turn summary; its last
  `end_turn` message then counts only after the long (11 min) settle. Installing the hooks adds a
  Stop hook, so Claude Code writes the summary and the short settle applies.
- In transcript-only mode, cron and one-shot jobs are matched by the job id Claude Code returns;
  when no id can be read, the job is assumed to stay scheduled (the session keeps waiting) until a
  Stop hook reports the authoritative list.
- Transcripts are read from their last 32 MB. Work announced before that point is unknown, so
  such sessions wait (*older history unknown*) until a Stop hook reports the authoritative list
  of background tasks — connect the Claude Code hooks for very long sessions.
- Codex does not persist approval prompts, stream errors or reconnects; such turns stay *running*.
- Only local agents are tracked: Claude Desktop chats and Cowork tasks, Codex cloud tasks,
  Cursor background (cloud) agents and agents inside Docker containers are out of scope.
- Cursor's editor counts as observed only after it produced a hook record (e.g. `workspaceOpen`)
  since it started; until then it blocks.
- Gemini's `AfterAgent` may also fire after a stream error. Gemini runs hooks only in trusted
  folders; elsewhere a running Gemini blocks.
- An idle `opencode serve` loads its plugin only with the first client request and blocks until
  then; an OpenCode process that dies mid-turn leaves its session blocking until it is excluded or
  the relevance window ends.
- Tools that are not listed (e.g. Google Antigravity, Windsurf, Copilot agents) are not detected at
  all — they neither block nor complete anything.
- A build is not proof of correct behaviour on every OS. No real sleep or shutdown has been
  performed yet on any OS (see [docs/testing.md](docs/testing.md)). On Windows PCs with Modern
  Standby (no S3 state, see `powercfg /a`) the sleep request may hibernate or fail instead; on
  macOS, shutting down asks for the Automation permission for System Events the first time.

Future ideas: estimated energy savings, more tools.

## License

[MIT](LICENSE)
