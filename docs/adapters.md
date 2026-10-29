# Adapters: evidence and state mapping

Verified 2026-10-29 against official docs/source and, where available, real local data
(Windows 11, Claude Desktop 4.29 with Claude Code 2.1.309, Codex desktop app). States are defined
in `crates/agn-core/src/model.rs`; only `completed`, `idle` and `ended` are non-blocking.

## Claude Code — CLI, IDE, Claude Desktop "Code" tab

Docs: <https://code.claude.com/docs/en/hooks>. The desktop app runs its own copy of Claude Code
(`%APPDATA%\Claude\claude-code\<ver>\claude.exe`) and reads the same `~/.claude` settings.

| Evidence | Mapping |
|----------|---------|
| transcript `user` (human / `scheduled` / `task_notification`) | turn started |
| transcript `assistant` tool use, tool results, `queue-operation` enqueue, `queued_command` | activity |
| `system/stop_hook_summary`, `preventedContinuation: false` | end of turn |
| `system/stop_hook_summary`, `preventedContinuation: true` (stop hook such as `/goal` blocked) | activity |
| final `assistant` message with `stop_reason: end_turn` (no Stop hooks configured → no summary) | inferred end of turn (11 min settle) |
| `attachment/goal_status` `met: false` / `true`, `/goal clear` | goal active / cleared |
| tool use with `run_in_background`, async `Agent`, `Monitor`, `Workflow` + id from `toolUseResult` / result text | background task |
| `<task-notification>` with terminal `<status>` (user entry, queued command or queue operation) | background task finished |
| `ScheduleWakeup` / `CronCreate` / `CronDelete` tool use | wake-up / recurring continuation / cancelled |
| first read starts inside the file (> 32 MB) | *history unknown* until a Stop hook snapshot |
| writes below `<session>/subagents/` (background agents, workflows) | activity of the parent |
| `isApiErrorMessage` with `rate_limit` / 429 / `quotaLimits` | rate limited (with reset time) |
| `system/api_error` with `retryAttempt` | reconnecting |
| `[Request interrupted by user` | interrupted |
| hook `UserPromptSubmit` | turn started |
| hook `Stop` + `background_tasks` + `session_crons` (≥ 2.1.145) | end of turn with authoritative pending work |
| hook `StopFailure` (`rate_limit` → rate limited, others → error) | failure |
| hook `PermissionRequest`, `Notification` `permission_prompt` / `agent_needs_input` / `elicitation_*` | waiting for input |
| hook `SessionStart` / `SessionEnd` | idle / ended |
| `~/.claude/sessions/<pid>.json` (`sessionId`, `startedAt`, `entrypoint`, `status`) | pid ↔ session; `status` is only a veto (a busy process without a tracked session blocks) |

Notes: the transcript is an internal format. No hook fires on a user interrupt. `SubagentStop` fires
repeatedly on 2.1.309 and is ignored. Usage-limit auto-continue means a rate-limited session is
paused, not finished. Installed hooks use exec form (`command` + `args`) and print nothing.

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

Process scans never complete a session. They detect agents we cannot see and block:

- Gemini, Cursor or OpenCode running without the integration, started before it was installed,
  or silent (no hook record since the process started);
- a Claude Code process missing from the registry, or registered as busy without a tracked session;
- a Codex CLI/exec process without an active rollout in its working directory since it started;
- a running WSL 2 VM (Windows, optional).

Tracked Claude Code sessions whose process vanished mid-turn become `interrupted`; a session that
only waited for background work becomes `ended` (nothing can continue it, nothing completed).
