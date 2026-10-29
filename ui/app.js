'use strict';

// Outside Tauri (plain browser preview) a static demo snapshot is shown.
const TAURI = window.__TAURI__;
const ICONS = window.AGN_ICONS;

const $ = (id) => document.getElementById(id);
let strings = {};
let snap = null;
let lang = 'cs';
let lastOk = Date.now();

// ---------------------------------------------------------------- backend

async function call(cmd, args = {}) {
  if (!TAURI) return demo.handle(cmd, args);
  return TAURI.core.invoke(cmd, args);
}

async function loadStrings(l) {
  if (TAURI) {
    strings = await call('get_strings', { lang: l });
  } else {
    strings = await (await fetch(`i18n/${l}.json`)).json();
  }
  lang = l;
  document.documentElement.lang = l;
  document.querySelectorAll('[data-i18n]').forEach((el) => { el.textContent = t(el.dataset.i18n); });
  $('dry-badge').title = t('app.dryRunHint');
  $('lang').value = l;
}

function t(key, params = {}) {
  let s = strings[key] ?? key;
  for (const [k, v] of Object.entries(params)) s = s.split(`{${k}}`).join(String(v));
  return s;
}

// ---------------------------------------------------------------- helpers

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function icon(name, cls = 'tool-icon') {
  const span = el('span', cls);
  span.innerHTML = ICONS[name] || ICONS.warn; // trusted constants only
  return span;
}

function ago(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 5) return t('time.now');
  if (s < 60) return t('time.sec', { n: s });
  if (s < 3600) return t('time.min', { n: Math.round(s / 60) });
  if (s < 86400) return t('time.hour', { n: Math.round(s / 3600) });
  return t('time.day', { n: Math.round(s / 86400) });
}

function clock(ms) {
  return new Date(ms).toLocaleTimeString(lang, { hour: '2-digit', minute: '2-digit' });
}

function mmss(ms) {
  const s = Math.ceil(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

const toolName = (id) => ({ claude_code: 'Claude Code', codex: 'Codex', gemini_cli: 'Gemini CLI', opencode: 'OpenCode', cursor: 'Cursor' }[id] || id);

function reasonText(r) {
  if (!r) return '';
  const k = `reason.${r}`;
  return strings[k] ? t(k) : r.replace(/_/g, ' ');
}

// ---------------------------------------------------------------- render

function render(s) {
  snap = s;
  $('dry-badge').hidden = s.backend !== 'dry_run';
  const arm = $('arm');
  arm.setAttribute('aria-checked', String(s.armed));
  arm.title = s.armed ? t('monitor.on') : t('monitor.off');
  document.querySelectorAll('.segmented button').forEach((b) => {
    b.setAttribute('aria-checked', String(b.dataset.action === s.action));
  });
  renderStatus(s);
  renderSessions(s);
  if (!$('settings-view').hidden) renderAdapters(s);
}

function activeSessions(s) {
  return s.sessions.filter((x) => x.blocking && x.relevant && !x.ignored);
}

function renderStatus(s) {
  const box = $('status');
  const dot = $('status-dot');
  const title = $('status-title');
  const sub = $('status-sub');
  const cancel = $('cancel');
  const progress = $('progress');
  box.classList.remove('countdown');
  cancel.hidden = true;
  progress.hidden = true;
  sub.textContent = '';
  dot.className = 'dot';

  if (Date.now() - lastOk > 12000) {
    title.textContent = t('status.stale');
    dot.classList.add('bad');
    return;
  }
  if (s.phase.phase === 'countdown' || s.phase.phase === 'verifying') {
    box.classList.add('countdown');
    dot.classList.add('warn');
    const rem = s.remaining_ms ?? 0;
    title.textContent = s.phase.phase === 'verifying'
      ? t('status.verifying')
      : t(`status.countdown.${s.action}`, { time: mmss(rem) });
    sub.textContent = t('status.countdownHint');
    cancel.hidden = false;
    progress.hidden = false;
    $('progress-bar').style.width = `${Math.max(0, Math.min(100, (rem / s.countdown_ms) * 100))}%`;
    return;
  }
  if (s.last_error) {
    title.textContent = t('status.error', { error: s.last_error });
    dot.classList.add('bad');
  }
  if (!s.armed) {
    if (s.last_power && !s.last_error) {
      const key = s.last_power.backend === 'dry_run' ? 'status.simulated' : 'status.done';
      title.textContent = t(`${key}.${s.last_power.action}`, { time: clock(s.last_power.at) });
      dot.classList.add('ok');
      sub.textContent = t('status.offHint');
    } else if (!s.last_error) {
      title.textContent = t('status.off');
      sub.textContent = t('status.offHint');
    }
    return;
  }
  const reasons = s.verdict.type === 'blocked' ? s.verdict.reasons : [];
  const unobs = reasons.find((r) => r.type === 'adapter_unobservable');
  const untracked = reasons.find((r) => r.type === 'untracked_process');
  const active = reasons.filter((r) => r.type === 'session_active');
  if (unobs) {
    title.textContent = t('status.unobservable', { tool: toolName(unobs.agent) });
    sub.textContent = t(`adapter.${unobs.code}`);
    dot.classList.add('bad');
  } else if (untracked) {
    title.textContent = t('status.untracked', { tool: toolName(untracked.agent) });
    dot.classList.add('bad');
  } else if (active.length) {
    title.textContent = t('status.waitingSessions', { count: active.length });
    dot.classList.add('run');
  } else if (s.sessions.length === 0) {
    title.textContent = t('status.noSessions');
    dot.classList.add('run');
  } else {
    title.textContent = t('status.waitingFirst');
    dot.classList.add('run');
  }
}

function renderSessions(s) {
  const list = $('session-list');
  const ulist = $('untracked-list');
  list.replaceChildren();
  ulist.replaceChildren();

  for (const p of s.untracked) {
    const li = el('li', `item alert${p.ignored ? ' dim' : ''}`);
    li.append(icon(p.agent));
    const main = el('div', 'item-main');
    main.append(el('div', 'item-title', `${t('untracked.title')}: ${toolName(p.agent)}`));
    main.append(el('div', 'item-sub', `${p.name} · PID ${p.pid}`));
    li.append(main);
    const side = el('div', 'item-side');
    if (p.ignored) {
      side.append(el('span', 'state', t('untracked.ignored')));
    } else {
      const b = el('button', 'btn tiny', t('untracked.ignore'));
      b.addEventListener('click', () => act('ignore_process', { pid: p.pid }));
      side.append(b);
    }
    li.append(side);
    ulist.append(li);
  }

  for (const x of s.sessions) {
    const li = el('li', `item${(!x.relevant || x.ignored) ? ' dim' : ''}`);
    li.append(icon(x.agent));
    const main = el('div', 'item-main');
    const name = x.project || x.id.slice(0, 8);
    main.append(el('div', 'item-title', name));
    const bits = [toolName(x.agent)];
    if (x.origin && x.origin !== 'unknown') bits.push(x.origin);
    if (x.parent) bits.push(t('sessions.subagent'));
    const reason = reasonText(x.reason);
    if (reason && x.state !== 'completed') bits.push(reason);
    if (x.resets_at) bits.push(t('sessions.resets', { time: clock(x.resets_at) }));
    if (x.ignored) bits.push(t('sessions.ignored'));
    else if (!x.relevant) bits.push(t('sessions.stale'));
    bits.push(ago(x.last_activity));
    const subEl = el('div', 'item-sub', bits.join(' · '));
    subEl.title = x.cwd || '';
    main.append(subEl);
    li.append(main);
    const side = el('div', 'item-side');
    const toggle = el('button', `icon-btn${x.ignored ? ' on' : ''}`);
    toggle.innerHTML = x.ignored ? ICONS.eye : ICONS.eyeOff;
    toggle.title = x.ignored ? t('sessions.unignore') : t('sessions.ignore');
    toggle.addEventListener('click', () => act('set_session_ignored', { agentId: x.agent, sessionId: x.id, ignored: !x.ignored }));
    side.append(toggle);
    side.append(el('span', `state ${x.state}`, t(`state.${x.state}`)));
    li.append(side);
    list.append(li);
  }
  const active = activeSessions(s).length;
  $('session-count').textContent = s.sessions.length ? `${active} / ${s.sessions.length}` : '';
  $('sessions-empty').hidden = s.sessions.length > 0 || s.untracked.length > 0;
}

function renderAdapters(s) {
  const list = $('adapter-list');
  list.replaceChildren();
  for (const a of s.adapters) {
    const li = el('li', 'adapter');
    li.append(icon(a.agent));
    const main = el('div', 'item-main');
    main.append(el('div', 'item-title', a.name));
    const code = a.enabled ? a.code : 'disabled';
    const line = el('div', `status-line ${a.observable ? 'ok' : 'bad'}`, t(`adapter.${code}`));
    main.append(line);
    const hint = el('div', 'item-sub', t(`adapterHint.${a.agent}`));
    main.append(hint);
    if (a.processes) main.append(el('div', 'item-sub', t('settings.processes', { n: a.processes })));
    li.append(main);

    const actions = el('div', 'adapter-actions');
    const sw = el('button', 'switch small-switch');
    sw.setAttribute('role', 'switch');
    sw.setAttribute('aria-checked', String(a.enabled));
    sw.title = t('settings.track');
    sw.append(el('span', 'knob'));
    sw.addEventListener('click', () => act('set_agent_enabled', { agentId: a.agent, enabled: !a.enabled }));
    actions.append(sw);
    if (a.integration.mode !== 'passive') {
      const installed = a.integration.installed;
      const b = el('button', 'btn tiny', installed ? t('settings.disconnect') : t('settings.connect'));
      b.addEventListener('click', () => toggleIntegration(a, installed));
      actions.append(b);
    }
    li.append(actions);
    list.append(li);
  }
}

// ---------------------------------------------------------------- actions

async function act(cmd, args) {
  try {
    const next = await call(cmd, args);
    if (next && next.phase) render(next);
  } catch (e) {
    $('settings-msg').textContent = t('settings.failed', { error: String(e) });
  }
}

function confirmDialog(text, okLabel) {
  return new Promise((resolve) => {
    $('modal-text').textContent = text;
    $('modal-yes').textContent = okLabel;
    $('modal').hidden = false;
    const done = (v) => {
      $('modal').hidden = true;
      $('modal-yes').onclick = null;
      $('modal-no').onclick = null;
      resolve(v);
    };
    $('modal-yes').onclick = () => done(true);
    $('modal-no').onclick = () => done(false);
  });
}

async function toggleIntegration(a, installed) {
  const path = a.integration.config_path || '?';
  const text = installed ? t('settings.confirmDisconnect', { path }) : t('settings.confirmConnect', { path });
  const ok = await confirmDialog(text, installed ? t('settings.disconnect') : t('settings.connect'));
  if (!ok) return;
  try {
    const backup = await call(installed ? 'uninstall_integration' : 'install_integration', { agentId: a.agent });
    $('settings-msg').textContent = backup ? t('settings.done', { backup }) : t('settings.doneNoBackup');
  } catch (e) {
    $('settings-msg').textContent = t('settings.failed', { error: String(e) });
  }
  poll();
}

async function poll() {
  try {
    const s = await call('get_snapshot');
    lastOk = Date.now();
    render(s);
  } catch (_) {
    if (snap) render(snap);
  }
}

function wire() {
  $('logo').innerHTML = ICONS.logo;
  document.querySelector('.gear').innerHTML = ICONS.gear;
  $('arm').addEventListener('click', () => act('set_armed', { armed: !(snap && snap.armed) }));
  document.querySelectorAll('.segmented button').forEach((b) => {
    b.addEventListener('click', () => act('set_action', { action: b.dataset.action }));
  });
  $('cancel').addEventListener('click', () => act('cancel_countdown'));
  $('open-settings').addEventListener('click', () => {
    $('main-view').hidden = true;
    $('settings-view').hidden = false;
    $('settings-msg').textContent = '';
    if (snap) renderAdapters(snap);
  });
  $('close-settings').addEventListener('click', () => {
    $('settings-view').hidden = true;
    $('main-view').hidden = false;
  });
  $('lang').addEventListener('change', async (e) => {
    await loadStrings(e.target.value);
    await act('set_language', { lang: e.target.value });
  });
  document.addEventListener('contextmenu', (e) => e.preventDefault());
}

async function start() {
  wire();
  let initial = null;
  try { initial = await call('get_snapshot'); } catch (_) { /* rendered on next poll */ }
  await loadStrings(initial?.language || 'cs');
  if (initial) { lastOk = Date.now(); render(initial); }
  if (TAURI) {
    await TAURI.event.listen('agn://snapshot', (e) => { lastOk = Date.now(); render(e.payload); });
  }
  setInterval(poll, 4000);
}

// ---------------------------------------------------------------- demo

const demo = {
  state: null,
  init() {
    const now = Date.now();
    const s = (agent, id, project, state, extra = {}) => ({
      key: `${agent}:${id}`, agent, id, project, cwd: `C:\\Users\\me\\${project}`, state, reason: null,
      origin: 'desktop', parent: null, last_activity: now - 40000, state_since: now - 60000, resets_at: null,
      blocking: !['completed', 'idle', 'ended'].includes(state), relevant: true, ignored: false,
      ended_unfinished: false, pending: [], ...extra,
    });
    const adapter = (agent, name, code, observable, mode, installed, processes = 0) => ({
      agent, name, enabled: true, observable, code, detail: null,
      integration: { agent, mode, installed, config_path: `C:\\Users\\me\\.${agent}\\settings.json`, installed_at: null }, processes,
    });
    this.state = {
      armed: true, phase: { phase: 'watching' }, remaining_ms: null, countdown_ms: 60000, action: 'sleep', backend: 'dry_run',
      verdict: { type: 'blocked', reasons: [{ type: 'session_active', key: { agent: 'claude_code', id: 'a' }, state: 'running' }] },
      sessions: [
        s('claude_code', 'a1b2c3d4', 'web-shop', 'running', { last_activity: now - 3000 }),
        s('codex', 'e5f6a7b8', 'api-server', 'awaiting_continuation', { reason: 'background_task', origin: 'cli' }),
        s('claude_code', 'c9d0e1f2', 'infra', 'rate_limited', { reason: 'five_hour', resets_at: now + 3600000 }),
        s('gemini_cli', 'a3b4c5d6', 'docs', 'completed', { origin: null, last_activity: now - 900000 }),
      ],
      adapters: [
        adapter('claude_code', 'Claude Code', 'transcripts', true, 'hooks', false, 2),
        adapter('codex', 'Codex', 'rollouts', true, 'passive', true, 1),
        adapter('gemini_cli', 'Gemini CLI', 'hooks', true, 'hooks', true),
        adapter('opencode', 'OpenCode', 'not_detected', true, 'plugin', false),
        adapter('cursor', 'Cursor', 'no_integration', false, 'hooks', false, 1),
      ],
      untracked: [], last_power: null, last_error: null, language: 'cs',
    };
  },
  handle(cmd, args) {
    if (!this.state) this.init();
    const st = this.state;
    if (cmd === 'set_armed') { st.armed = args.armed; st.phase = { phase: args.armed ? 'watching' : 'off' }; }
    if (cmd === 'set_action') st.action = args.action;
    if (cmd === 'set_language') st.language = args.lang;
    if (cmd === 'set_session_ignored') st.sessions.forEach((x) => { if (x.id === args.sessionId) x.ignored = args.ignored; });
    if (cmd === 'set_agent_enabled') st.adapters.forEach((a) => { if (a.agent === args.agentId) a.enabled = args.enabled; });
    if (cmd === 'install_integration' || cmd === 'uninstall_integration') {
      st.adapters.forEach((a) => { if (a.agent === args.agentId) a.integration.installed = cmd === 'install_integration'; });
      return Promise.resolve(null);
    }
    return Promise.resolve(JSON.parse(JSON.stringify(st)));
  },
};

start();
