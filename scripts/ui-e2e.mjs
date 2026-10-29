// End-to-end UI test of the real app window.
//
// Starts the *debug* build — which contains only the DryRun power manager —
// against a throw-away sandbox (AGN_TEST_HOME, no process scan), drives the
// real window and checks the overnight flow: sessions and states, arming,
// countdown, Cancel, new activity cancelling the countdown, the simulated
// action, integrations and, on Windows, closing to the tray and reopening on
// a countdown.
//
// Windows: WebView2 over the Chrome DevTools Protocol (real mouse input).
// Linux:   WebKitGTK over WebDriver via tauri-driver (+ WebKitWebDriver);
//          run it under a display, e.g. xvfb-run.
//
//   cargo build -p agentgoodnight -p agn-hook && node scripts/ui-e2e.mjs [path-to-binary]

import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const WIN = process.platform === 'win32';
const EXT = WIN ? '.exe' : '';
const PORT = 9333 + Math.floor(Math.random() * 500);
const exe = path.resolve(process.argv[2] || `target/debug/agentgoodnight${EXT}`);
// Connect copies the hook binary that sits next to the app.
const hookBin = path.join(path.dirname(exe), `agentgoodnight-hook${EXT}`);
if (!fs.existsSync(exe) || !fs.existsSync(hookBin)) {
  console.error(`missing ${exe} or ${hookBin} — run: cargo build -p agentgoodnight -p agn-hook`);
  process.exit(2);
}
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'agn-ui-e2e-'));
const home = path.join(root, 'home');
const data = path.join(root, 'data');
for (const d of ['.claude/projects', '.claude/sessions', '.codex/sessions']) fs.mkdirSync(path.join(home, d), { recursive: true });
fs.mkdirSync(data, { recursive: true });
fs.writeFileSync(path.join(data, 'settings.json'), JSON.stringify({ settle_secs: 15, countdown_secs: 10, language: 'en' }));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const CI = !!process.env.GITHUB_ACTIONS;
// CI machines are slower; timeouts scale.
const T = (ms) => (CI ? ms * 3 : ms);
const annotate = (text) => CI && console.log(`::error::${text.replace(/%/g, '%25').replace(/\r?\n/g, '%0A')}`);
let failures = 0;
function check(ok, what, detail = '') {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${detail && !ok ? ` — ${detail}` : ''}`);
  if (!ok) {
    failures++;
    annotate(`UI e2e: ${what}${detail ? ` — ${detail}` : ''}`);
  }
}

// ------------------------------------------------------------ simulated agents

const append = (file, obj) => {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, JSON.stringify(obj) + '\n');
};
function claude(id, cwd, what) {
  const file = path.join(home, '.claude', 'projects', cwd.replace(/[^A-Za-z0-9]/g, '-'), `${id}.jsonl`);
  const base = { sessionId: id, cwd, timestamp: new Date().toISOString(), entrypoint: 'cli' };
  const lines = {
    user: [{ ...base, type: 'user', message: { role: 'user', content: 'Refactor the checkout' } }],
    stop: [
      { ...base, type: 'assistant', message: { role: 'assistant', stop_reason: 'end_turn', content: [{ type: 'text', text: 'Done.' }] } },
      { ...base, type: 'system', subtype: 'stop_hook_summary', preventedContinuation: false },
    ],
    bg: [
      { ...base, type: 'assistant', message: { role: 'assistant', content: [{ type: 'tool_use', id: 'toolu_bg', name: 'Bash', input: { command: 'npm test', run_in_background: true } }] } },
      { ...base, type: 'user', toolUseResult: { backgroundTaskId: 'bash_1' }, message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'toolu_bg', content: 'Command running in background with ID: bash_1' }] } },
    ],
    bgdone: [{ ...base, type: 'user', message: { role: 'user', content: '<task-notification><task-id>bash_1</task-id><status>completed</status></task-notification>' } }],
    ratelimit: [{ ...base, type: 'assistant', isApiErrorMessage: true, error: 'rate_limit', quotaLimits: { rateLimitType: 'five_hour', resetsAt: Math.floor(Date.now() / 1000) + 3600 }, message: { role: 'assistant', content: [{ type: 'text', text: 'limit' }] } }],
    interrupt: [{ ...base, type: 'user', message: { role: 'user', content: '[Request interrupted by user]' } }],
  }[what];
  lines.forEach((l) => append(file, l));
}
function codex(id, cwd, what) {
  const file = path.join(home, '.codex', 'sessions', '2026', '01', '01', `rollout-2026-01-01T00-00-00-${id}.jsonl`);
  const ts = new Date().toISOString();
  if (!fs.existsSync(file)) append(file, { timestamp: ts, type: 'session_meta', payload: { id, cwd, originator: 'codex_cli_rs' } });
  append(file, { timestamp: ts, type: 'event_msg', payload: { type: what === 'start' ? 'task_started' : 'task_complete' } });
}

// ------------------------------------------------------------------ CDP driver

async function connect() {
  for (let i = 0; i < T(120); i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json();
      const page = list.find((t) => t.type === 'page');
      if (page) {
        const ws = new WebSocket(page.webSocketDebuggerUrl);
        await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
        let id = 0;
        const pending = new Map();
        ws.onmessage = (m) => {
          const msg = JSON.parse(m.data);
          const p = pending.get(msg.id);
          if (p) { pending.delete(msg.id); msg.error ? p.rej(new Error(JSON.stringify(msg.error))) : p.res(msg.result); }
        };
        const send = (method, params = {}) => new Promise((res, rej) => {
          const n = ++id;
          pending.set(n, { res, rej });
          ws.send(JSON.stringify({ id: n, method, params }));
        });
        return { ws, send };
      }
    } catch { /* not up yet */ }
    await sleep(500);
  }
  throw new Error(`WebView DevTools endpoint did not come up\n${diagnose()}`);
}

// What runs, with which flags and ports, and which windows exist.
function diagnose() {
  const ps = `
$w = Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" | Select-Object ProcessId, ParentProcessId, @{n='Cmd';e={ if ($_.CommandLine -match '--type=') { $_.CommandLine.Substring(0, [Math]::Min(200, $_.CommandLine.Length)) } else { $_.CommandLine } }}
"webview2 processes: $(@($w).Count)"; $w | ForEach-Object { "  $($_.ProcessId) <- $($_.ParentProcessId): $($_.Cmd)" }
"listening on ${PORT}: " + (@(Get-NetTCPConnection -State Listen -LocalPort ${PORT} -ErrorAction SilentlyContinue).Count)
$p = Get-Process -Id ${app.pid} -ErrorAction SilentlyContinue
"app alive: $([bool]$p) window: '$($p.MainWindowTitle)' handle: $($p.MainWindowHandle) session: $($p.SessionId)"
"requested: AGN_TEST_DEVTOOLS_PORT=${PORT}"`;
  try {
    return execFileSync('powershell', ['-NoProfile', '-Command', ps]).toString();
  } catch (e) {
    return `diagnostics failed: ${e}`;
  }
}

// ------------------------------------------------------------ WebDriver (Linux)

const WD = 'http://127.0.0.1:4444';
let wdSession = null;
async function wd(method, url, body) {
  const r = await fetch(`${WD}${url}`, {
    method,
    headers: { 'content-type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(T(40000)),
  });
  const j = await r.json();
  if (j.value && j.value.error) throw new Error(`${j.value.error}: ${j.value.message}`);
  return j.value;
}
async function wdConnect() {
  for (let i = 0; i < T(60); i++) {
    try { if ((await (await fetch(`${WD}/status`)).json()).value) break; } catch { /* starting */ }
    await sleep(500);
  }
  const s = await wd('POST', '/session', { capabilities: { alwaysMatch: { 'tauri:options': { application: exe } } } });
  wdSession = s.sessionId;
}

let cdp;
async function js(expr) {
  if (!WIN) return wd('POST', `/session/${wdSession}/execute/sync`, { script: `return (${expr});`, args: [] });
  const r = await cdp.send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
  return r.result.value;
}
async function until(expr, timeout, what) {
  const ms = T(timeout);
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    try { if (await js(expr)) { check(true, what); return true; } } catch { /* page reloading */ }
    await sleep(250);
  }
  check(false, what, `timeout after ${ms} ms`);
  return false;
}
async function click(selector) {
  if (!WIN) {
    // WebDriver element click: scrolls into view and uses real input.
    const el = await wd('POST', `/session/${wdSession}/element`, { using: 'css selector', value: selector });
    await wd('POST', `/session/${wdSession}/element/${Object.values(el)[0]}/click`, {});
    return;
  }
  const box = await js(`(() => { const e = document.querySelector(${JSON.stringify(selector)}); if (!e) return null; e.scrollIntoView({ block: 'center' }); const r = e.getBoundingClientRect(); return r.width ? { x: r.x + r.width / 2, y: r.y + r.height / 2 } : null; })()`);
  if (!box) throw new Error(`not clickable: ${selector}`);
  for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) {
    await cdp.send('Input.dispatchMouseEvent', { type, x: box.x, y: box.y, button: 'left', clickCount: 1 });
  }
}
// Same as clicking the window's close button: WM_CLOSE to the app's window.
function closeWindow(pid) {
  const ps = `
Add-Type @'
using System; using System.Text; using System.Runtime.InteropServices;
public static class W {
  public delegate bool P(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(P f, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint p);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
}
'@
$n = 0
[W]::EnumWindows({ param($h, $l)
  $p = 0; [void][W]::GetWindowThreadProcessId($h, [ref]$p)
  $sb = New-Object System.Text.StringBuilder 64; [void][W]::GetWindowText($h, $sb, 64)
  if ($p -eq ${pid} -and $sb.ToString() -eq 'AgentGoodNight') { [void][W]::PostMessage($h, 0x10, [IntPtr]::Zero, [IntPtr]::Zero); $script:n++ }
  return $true }, [IntPtr]::Zero) | Out-Null
"closed $n"`;
  const out = execFileSync('powershell', ['-NoProfile', '-Command', ps]).toString().trim();
  check(out === 'closed 1', 'window found and closed', out);
}

const states = () => js(`[...document.querySelectorAll('#session-list li')].map(li => li.querySelector('.item-title').textContent + '=' + li.querySelector('.state').className.replace('state ', ''))`);
const stateOf = (name) => `[...document.querySelectorAll('#session-list li')].some(li => li.querySelector('.item-title').textContent === ${JSON.stringify(name)}`;
const phase = (p) => `typeof snap !== 'undefined' && snap && snap.phase.phase === ${JSON.stringify(p)}`;

// ------------------------------------------------------------------- scenario

const env = {
  ...process.env,
  AGN_DATA_DIR: data,
  AGN_TEST_HOME: home,
  AGN_TEST_NO_PROCESSES: '1',
  // Debug builds on Windows only: DevTools port and a WebView2 profile of
  // its own.
  ...(WIN ? { AGN_TEST_DEVTOOLS_PORT: String(PORT) } : {}),
  // Linux: Tauri lets WebKitWebDriver automate the window only with this.
  ...(WIN ? {} : { TAURI_WEBVIEW_AUTOMATION: 'true' }),
};
// Linux: tauri-driver starts the app (with this environment) for the session.
const app = WIN
  ? spawn(exe, [], { env, stdio: ['ignore', 'pipe', 'pipe'] })
  : spawn('tauri-driver', [], { env, stdio: ['ignore', 'pipe', 'pipe'] });
let log = '';
app.stdout.on('data', (d) => { log += d; });
app.stderr.on('data', (d) => { log += d; });
let exited = false;
app.on('exit', () => { exited = true; });

const A = 'aaaaaaaa-0000-4000-8000-000000000001';
const B = 'bbbbbbbb-0000-4000-8000-000000000002';
const C = 'cccccccc-0000-7000-8000-000000000003';

try {
  if (WIN) cdp = await connect();
  else await wdConnect();
  await until(`document.querySelector('#dry-badge') && !document.querySelector('#dry-badge').hidden`, 20000, 'DRY RUN badge shown');
  check((await js(`document.querySelector('#arm').getAttribute('aria-checked')`)) === 'false', 'monitoring starts switched off');

  claude(A, 'C:\\work\\web-shop', 'user');
  claude(B, 'C:\\work\\infra', 'user');
  codex(C, 'C:\\work\\api-server', 'start');
  await until(`document.querySelectorAll('#session-list li').length === 3`, 30000, 'three concurrent sessions listed');
  await until(`${stateOf('web-shop')} && li.querySelector('.state.running'))`, 5000, 'Claude session running');

  await click('#arm');
  await until(`document.querySelector('#arm').getAttribute('aria-checked') === 'true'`, 5000, 'switch arms monitoring');

  claude(B, 'C:\\work\\infra', 'ratelimit');
  claude(A, 'C:\\work\\web-shop', 'bg');
  claude(A, 'C:\\work\\web-shop', 'stop');
  codex(C, 'C:\\work\\api-server', 'done');
  await until(`${stateOf('infra')} && li.querySelector('.state.rate_limited'))`, 15000, 'rate limit shown');
  await until(`${stateOf('web-shop')} && li.querySelector('.state.awaiting_continuation'))`, 15000, 'background task keeps the session waiting');
  await until(`${stateOf('api-server')} && li.querySelector('.state.completed'))`, 40000, 'Codex completes after the settle window');
  check(await js(phase('watching')), 'no countdown while others are not finished');

  claude(B, 'C:\\work\\infra', 'interrupt');
  await until(`${stateOf('infra')} && li.querySelector('.state.interrupted'))`, 15000, 'interrupt shown');
  await js(`[...document.querySelectorAll('#session-list li')].find(li => li.querySelector('.state.interrupted')).querySelector('.icon-btn').dataset.e2e = 'target'`);
  await click('[data-e2e="target"]');
  await until(`${stateOf('infra')} && li.classList.contains('dim'))`, 5000, 'interrupted session excluded by the user');

  claude(A, 'C:\\work\\web-shop', 'bgdone');
  claude(A, 'C:\\work\\web-shop', 'stop');
  await until(`${phase('countdown')} && !document.querySelector('#cancel').hidden`, 60000, 'countdown with Cancel after all tasks finished');
  await click('#cancel');
  await until(`${phase('watching')} && document.querySelector('#cancel').hidden`, 5000, 'Cancel stops the countdown, monitoring stays on');

  claude(A, 'C:\\work\\web-shop', 'user');
  claude(A, 'C:\\work\\web-shop', 'stop');
  await until(phase('countdown'), 60000, 'new completion starts a countdown again');
  codex(C, 'C:\\work\\api-server', 'start');
  await until(phase('watching'), 15000, 'new agent activity cancels the countdown');

  // Tools panel open when the countdown starts: Cancel must still be visible.
  await click('#open-settings');
  codex(C, 'C:\\work\\api-server', 'done');
  await until(`${phase('countdown')} && document.querySelector('#settings-view').hidden && !document.querySelector('#cancel').hidden`, 60000, 'countdown switches to the main view');
  await until(`typeof snap !== 'undefined' && snap && !snap.armed && snap.last_power && snap.last_power.backend === 'dry_run'`, 30000, 'countdown ends in a DryRun action and switches monitoring off');
  if (log.includes('starting AgentGoodNight')) {
    check(/DRY RUN: power action recorded/.test(log), 'log confirms nothing was executed');
  }

  // Integrations write only into the sandbox and survive a round trip.
  await click('#open-settings');
  await until(`document.querySelectorAll('#adapter-list li').length === 5`, 5000, 'five adapters listed');
  for (const [n, agent, file] of [[1, 'claude_code', '.claude/settings.json'], [3, 'gemini_cli', '.gemini/settings.json'], [4, 'opencode', '.config/opencode/plugins/agentgoodnight.js']]) {
    await click(`#adapter-list li:nth-child(${n}) .btn`);
    await until(`!document.querySelector('#modal').hidden`, 3000, `${agent}: confirmation shows the file`);
    await click('#modal-yes');
    await until(`snap.adapters.find(a => a.agent === '${agent}').integration.installed`, 8000, `${agent}: connected`);
    check(fs.existsSync(path.join(home, file)), `${agent}: config written into the sandbox`);
    await click(`#adapter-list li:nth-child(${n}) .btn`);
    await until(`!document.querySelector('#modal').hidden`, 3000, `${agent}: disconnect confirmation`);
    await click('#modal-yes');
    await until(`!snap.adapters.find(a => a.agent === '${agent}').integration.installed`, 8000, `${agent}: disconnected`);
  }
  await click(`#adapter-list li:nth-child(4) .switch`);
  await until(`!snap.adapters.find(a => a.agent === 'opencode').enabled`, 5000, 'tracking switch works for every tool');
  await click(`#adapter-list li:nth-child(4) .switch`);
  await click('#close-settings');

  // Closing the window keeps monitoring in the tray; a countdown reopens it.
  // (Windows only: a WebDriver session ends with its window.)
  if (WIN) {
    await click('#arm');
    await until(`snap.armed`, 5000, 're-armed');
    cdp.ws.close();
    closeWindow(app.pid);
    await sleep(T(3000));
    check(!exited, 'closing the window keeps the app running');
    const pages = await (await fetch(`http://127.0.0.1:${PORT}/json`).catch(() => null))?.json().catch(() => []) ?? [];
    check(!pages.some((t) => t.type === 'page'), 'closed window releases the webview');
    claude(A, 'C:\\work\\web-shop', 'user');
    claude(A, 'C:\\work\\web-shop', 'stop');
    cdp = await connect();
    await until(`${phase('countdown')} && !document.querySelector('#cancel').hidden`, 60000, 'countdown reopens the window with Cancel');
    await click('#cancel');
    await until(phase('watching'), 5000, 'Cancel works in the reopened window');
  }
  console.log('sessions at the end:', await states());
} catch (e) {
  check(false, 'scenario', `${e.stack || e}${e.cause ? `\ncause: ${e.cause.stack || e.cause}` : ''}`);
} finally {
  if (wdSession) await wd('DELETE', `/session/${wdSession}`).catch(() => {});
  if (!exited) app.kill();
  await sleep(1000);
  if (failures) {
    console.log('\n--- app log ---\n' + log);
    annotate(`app log (tail):\n${log.split('\n').slice(-25).join('\n')}`);
  }
  try { fs.rmSync(root, { recursive: true, force: true }); } catch { /* webview may still hold files */ }
}
console.log(failures ? `\n${failures} check(s) failed` : '\nall UI checks passed');
process.exit(failures ? 1 : 0);
