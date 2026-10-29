# Adapters: evidence and state mapping

Verified 2026-10-29 against official docs/source and, where available, real local data
(Windows 11, Claude Desktop 4.29 with Claude Code 2.1.309, Codex desktop app). States are defined
in `crates/agn-core/src/model.rs`; only `completed`, `idle` and `ended` are non-blocking.

## Claude Code — CLI, IDE, Claude Desktop "Code" tab

Docs: <https://code.claude.com/docs/en/hooks>. The desktop app runs its own copy of Claude Code
(`%APPDATA%\Claude\claude-code\<ver>\claude.exe`) and reads the same `~/.claude` settings.

| Evidence | Mapping |
|----------|---------|
| transcript `user` (human / `task_notification`) | turn started |
| transcript `user` with `turnOrigin: scheduled` | the earliest due wake-up fired + turn started |
| transcript `user` local command output, bash mode (`<bash-input>`, `<bash-stdout>`), `isMeta` | activity only (no model turn) |
| transcript `user` `/clear` | idle (new conversation) |
| transcript `assistant` tool use, tool results, `queue-operation` enqueue, `queued_command` | activity |
| `system/stop_hook_summary`, `preventedContinuation: false` | confirmed end of turn (short settle) |
| `system/stop_hook_summary`, `preventedContinuation: true` (stop hook such as `/goal` blocked) | activity |
| final `assistant` message with `stop_reason: end_turn` (no Stop hooks configured → no summary) | inferred end of turn (long settle) |
| `attachment/goal_status` `met: false` / `true`, `/goal clear` | goal active / cleared |
| tool use with `run_in_background`, async `Agent`, `Monitor`, `Workflow` + id from `toolUseResult` / result text | background task (without a readable id: stays pending until a Stop hook snapshot) |
| `<task-notification>` with terminal `<status>` (user entry, queued command or queue operation) | background task finished |
| `ScheduleWakeup` (due time from `delaySeconds`) | one wake-up, pending until a turn starts after it was due |
| `CronCreate` `recurring: false` / `true`, job id from the tool result | one-shot job / recurring continuation |
| `CronDelete` of a job id we saw created (successful result) | that job cancelled; the session settles when no recurring job is left |
| first read starts inside the file (> 32 MB) | *history unknown* until a Stop hook snapshot |
| writes below `<session>/subagents/` (background agents, workflows) | activity of the parent |
| `isApiErrorMessage` with `rate_limit` / 429 / `quotaLimits` | rate limited (with reset time) |
| `system/api_error` with `retryAttempt` | reconnecting |
| `[Request interrupted by user` | interrupted |
| hook `UserPromptSubmit` / `UserPromptExpansion` | turn started |
| hook `PreToolUse` for `ScheduleWakeup` / `CronCreate` | wake-up / continuation scheduled |
| hook `PostToolUse` / `SubagentStart` (installed as async hooks) | heartbeat only, never revives a finished turn |
| hook `Stop` + `background_tasks` + `session_crons` (≥ 2.1.145) | inferred end of turn with the authoritative pending-work snapshot; other Stop hooks may still block, so it never counts as a confirmed end |
| hook `StopFailure` (`rate_limit` → rate limited, others → error) | failure |
| hook `PermissionRequest`, `Elicitation`, `Notification` `permission_prompt` / `agent_needs_input` / `elicitation_*` | waiting for input |
| hook `SessionStart` / `SessionEnd` | idle / ended |
| `~/.claude/sessions/<pid>.json` (`sessionId`, `startedAt`, `entrypoint`, `status`) | pid + process start ↔ session; `status` is only a veto (a busy process without a tracked session blocks) |
| `hooks.Stop[].hooks[].timeout` in user, project (every tracked cwd and its parents), local, plugin `hooks/hooks.json` and managed settings | length of the inferred settle window (max + 1 min, at least 11 min) |

Notes: the transcript is an internal format. No hook fires on a user interrupt. `SubagentStop` fires
repeatedly on 2.1.309 and is ignored. Usage-limit auto-continue means a rate-limited session is
paused, not finished. Installed hooks use exec form (`command` = hook path,
`args` = `["claude_code", "--data-dir", <dir>]`) and print nothing. Parser state and file offsets
are saved, so a restart continues reading instead of replaying the transcript.

## Codex — CLI, IDE extension, Codex desktop app

Source: `codex-rs` protocol and rollout policy; docs at <https://learn.chatgpt.com/docs>.

| Rollout line (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl[.zst]`) | Mapping |
|-----------------------------|---------|
| `session_meta` (`id`, `cwd`, `originator`, `source`, subagent parent) | metadata |
| `event_msg/task_started` | turn started |
| `event_msg/task_complete`, `error: null` | end of turn |
| `event_msg/task_complete`, `error.codex_error_info: usage_limit_exceeded` | rate limited |
| `event_msg/task_complete` with other error | error |
| `event_msg/turn_aborted` `interrupted` / `replaced` / `budget_limited` | interrupted / activity / error |
| `event_msg/thread_goal_updated` `active`, `usageLimited` / `blocked` / other | goal active / waiting for input / cleared |
| `event_msg/token_count.rate_limits` (`used_percent`, `resets_at`) | reset time |
| subagent rollouts | separate sessions with `parent` |

Not persisted by Codex: approval requests, stream errors, reconnects. Such turns stay `running`.
Codex hooks (`~/.codex/hooks.json`) exist but require manual trust in `/hooks`, so they are not
needed or installed. `notify` is often used by the desktop app itself and is never touched.

## Gemini CLI (experimental)

Docs: <https://geminicli.com/docs/hooks/>. `BeforeAgent` → turn started, `AfterAgent` → end of
turn, `BeforeTool`/`AfterTool`/`BeforeModel`/`AfterModel` → activity, `Notification`
`ToolPermission` → waiting for input, `SessionStart`/`SessionEnd`. Payload `timestamp` is used for
ordering. Not verified on a real installation; `AfterAgent` may fire after a stream error.

## Cursor (experimental)

Docs: <https://cursor.com/docs/agent/hooks>. `beforeSubmitPrompt` → turn started, tool/agent
events → activity, `stop` `completed` / `aborted` / `error` → end / interrupted / error,
`sessionEnd` `completed` → end of turn (the CLI in `-p` mode emits no `stop`). Hooks run through
PowerShell on Windows and stdin starts with a BOM; the hook always prints `{}`. A `followup_message`
from another hook can continue the conversation; the settle window absorbs it.

## OpenCode (experimental, 1.x only)

Docs: <https://opencode.ai/docs/plugins/>. The plugin forwards `session.status` (`busy` → turn
started, `idle` → end of turn, `retry` → reconnecting / rate limited), `session.error`
(`MessageAbortedError` → interrupted), `permission.asked` / `question.asked` → waiting for input.
Child sessions (`parentID`) are tracked separately, so a background child blocks while its parent is
idle. OpenCode 2.x uses an incompatible plugin API and is not supported yet.

## Processes

Process scans never complete a session. A process is identified by (pid, start time), so a pid
reused after a reboot is a different process. Scans detect agents we cannot see and block:

- Gemini, Cursor or OpenCode running without the integration, started before it was installed,
  or silent. A hook record proves only the process found in the hook's own ancestor chain
  (pid, start time, tool), shared with its launcher or helper processes of the same tool; a
  Claude Code hook running in Cursor's terminal does not prove Cursor;
- a Claude Code process missing from the registry, or registered as busy without a tracked session;
- a Codex CLI/exec process without its own rollout (one rollout per process) active in its working
  directory (`cwd`, `-C`/`--cd`) since it started. Threads of the Codex desktop app or IDE host stay
  relevant as long as the host process runs;
- a Claude Code or Codex process whose `CLAUDE_CONFIG_DIR` / `CODEX_HOME` differs from the
  directory AgentGoodNight reads;
- a failed process scan (environment unobservable);
- a running WSL 2 VM (Windows, optional).

Tracked Claude Code sessions whose process vanished mid-turn (absent from two consecutive
successful scans with a readable registry) become `interrupted`; a session that only waited for
background work becomes `ended` (nothing can continue it, nothing completed).
