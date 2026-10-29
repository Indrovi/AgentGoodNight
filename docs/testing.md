# Testing and verification

What has been verified, how, and what still needs a human. A build alone is not proof that the
app works on an OS.

## Automated, on every push (GitHub Actions)

| Check | Windows | macOS | Linux |
|-------|:-------:|:-----:|:-----:|
| `cargo fmt`, `clippy -D warnings` | ✓ | ✓ | ✓ |
| Unit tests, overnight simulations, regression tests for every review finding (`cargo test --workspace`) | ✓ | ✓ | ✓ |
| Real power backend compiles (`--release --features real-power`, never executed) | ✓ | ✓ | ✓ |
| DryRun release bundle | ✓ | ✓ | ✓ |
| Start-up smoke test of the built app: window, tray and monitor thread come up and keep running (`scripts/smoke-app.sh`, Linux under Xvfb) | ✓ | ✓ | ✓ |
| End-to-end UI test of the real window (`scripts/ui-e2e.mjs`) | ✓ | – | – |

The UI test starts the **debug** build (it contains only the DryRun power manager) against a
throw-away sandbox and drives the WebView2 window over the DevTools protocol with real mouse
input: three concurrent sessions, rate limit, background task, interrupt and exclusion, arming,
countdown, Cancel, new activity cancelling the countdown, the countdown switching from the tools
panel to the main view, the simulated action, Connect/Disconnect of every integration into the
sandbox, the tracking switches, closing the window to the tray (the webview is released) and the
window reopening for a countdown. WebKitGTK and WKWebView offer no such protocol, so on macOS and
Linux only the smoke test covers the GUI.

## Manual verification so far

Windows 11 (AMD, S3 standby and hibernation available according to `powercfg /a`):

- **Claude Code 2.1.309 (Claude Desktop "Code" tab) and the Codex desktop app** — the read-only
  probe (`cargo run -p agn-adapters --example probe`) attributes the running Claude Desktop
  sessions through the registry and replays real history correctly (Codex: completions, goals,
  rate limits).
- **Claude Code CLI 2.1.309 with the installed hooks**, run in an isolated config directory
  without a login: the exec-form hook (`command` + `args`) runs and records `SessionStart`,
  `UserPromptSubmit`, `StopFailure` (`authentication_failed` → error) and `SessionEnd`.
  A full successful turn with hooks needs a login and was not run in isolation.
- **GUI** — the end-to-end flow above, run against the real window; idle cost of the app process
  in a debug build: ~0.2 % of one core, ~7 MB private memory with the window closed to the tray.
- **Gemini CLI 0.66.0** (npm, sandboxed home, no account): config accepted, hooks run through
  PowerShell with the call-operator form, full offline turn via the CLI's fake-response mode,
  states and process proof checked. This run found that the earlier command form never ran on
  Windows and that hooks need a trusted folder.
- **OpenCode 1.19.5** (npm, sandboxed XDG dirs, local mock model): plugin loads in TUI, `run` and
  `serve`; all mapped states observed; process proof checked.
- **Cursor** — not installed; hooks checked against the current docs and by emulating Cursor's
  documented PowerShell wrapper (answers, BOM, process chain).
- Details and remaining gaps per tool: [adapters.md](adapters.md).

Not verified yet: a real sleep or shutdown on any OS, macOS and Linux on real hardware.

## Tools for sandboxed testing

- `AGN_DATA_DIR=<dir>` — the app's own data directory (every build).
- `AGN_TEST_HOME=<dir>` and `AGN_TEST_NO_PROCESSES=1` — **debug builds only** (compiled out of
  release builds): every agent directory is read from the sandbox and the process scan is replaced
  by an empty list, so simulated agent logs can drive the UI.
- `cargo run -p agn-adapters --example sandbox -- install|uninstall|watch <agent> <home> <data>` —
  installs an integration exactly like the Connect button, but into a sandbox home that a real
  tool installation is pointed at (`USERPROFILE`/`HOME`/`XDG_*`/`CLAUDE_CONFIG_DIR`), then shows
  the monitor's view of that tool live.

## Real power test (requires the owner's explicit approval)

Not performed. The test really puts the computer to sleep or shuts it down, so:

1. Save all work and finish or close every other agent session on that computer.
2. Build with the real backend; the window must **not** show the *DRY RUN* badge:
   ```bash
   scripts/build-hook.sh
   npx @tauri-apps/cli@2 build --config src-tauri/tauri.release.conf.json --features real-power
   ```
3. Sleep: choose *Sleep*, switch monitoring on, give one agent a tiny task ("Reply with OK"),
   wait for the 60 s countdown and let it run out. After waking up the window shows the action
   with its time and monitoring is off.
4. Cancel: repeat and press *Cancel* (also from the tray menu) — nothing may happen.
5. Shutdown: choose *Shut down* and repeat 3. Applications with unsaved work may veto the
   shutdown; that is expected, nothing is force-closed.
6. Keep the stderr log and `sessions.json` from the data directory if anything differs.
