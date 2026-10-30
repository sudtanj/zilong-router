/* zilong-router dashboard. A dependency-free port of 9router's Next.js dashboard:
   same layout, pages, components and copy, backed by /api/admin. */
(() => {
'use strict';

// ----- helpers -------------------------------------------------------------------
const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const fmt = (n) => new Intl.NumberFormat().format(n || 0);
const fmtCost = (n) => '$' + (n || 0).toFixed(2);
const fmtCost4 = (n) => '$' + (n || 0).toFixed(4);
const fmtTokens = (n) => n >= 1e6 ? (n / 1e6).toFixed(1) + 'M' : n >= 1e3 ? (n / 1e3).toFixed(1) + 'K' : String(n || 0);
const relTime = (iso) => {
  if (!iso) return 'Never';
  const m = Math.floor((Date.now() - new Date(iso)) / 60000);
  if (m < 1) return 'Just now'; if (m < 60) return m + 'm ago'; if (m < 1440) return Math.floor(m / 60) + 'h ago';
  return new Date(iso).toLocaleDateString();
};
const ms = (name, size) => `<span class="ms${size ? ' i' + size : ''}">${name}</span>`;
const maskKey = (k) => k && k.length > 12 ? k.slice(0, 8) + '••••••••' + k.slice(-4) : '••••••••';
const isCooling = (c) => (c.locks || []).some((l) => new Date(l.until).getTime() > Date.now());
const lockUntil = (c) => (c.locks || []).map((l) => l.until).filter(Boolean).sort()[0] || null;
const cooldownText = (until) => {
  const d = new Date(until).getTime() - Date.now(); if (d <= 0) return '';
  const s = Math.floor(d / 1000);
  if (s < 60) return s + 's'; if (s < 3600) return Math.floor(s / 60) + 'm ' + (s % 60) + 's';
  return Math.floor(s / 3600) + 'h ' + Math.floor(s % 3600 / 60) + 'm';
};

// ----- api -----------------------------------------------------------------------
async function api(method, path, body) {
  const res = await fetch('/api/admin' + path, {
    method, headers: body !== undefined ? {'content-type': 'application/json'} : {},
    body: body !== undefined ? JSON.stringify(body) : undefined, credentials: 'same-origin',
  });
  if (res.status === 401 && path !== '/login') { showLogin(); throw new Error('login required'); }
  let data = null; try { data = await res.json(); } catch (_) {}
  if (!res.ok) throw new Error((data && data.error) || res.statusText);
  return data;
}
const get = (p) => api('GET', p), post = (p, b) => api('POST', p, b || {}), patch = (p, b) => api('PATCH', p, b), del = (p) => api('DELETE', p);

// ----- notifications (upstream notificationStore look) ----------------------------
const ICONS = {success: 'check_circle', error: 'error', warning: 'warning', info: 'info'};
function notify(type, message, title) {
  const el = document.createElement('div');
  el.className = 'toast ' + type;
  el.innerHTML = `${ms(ICONS[type], 18)}<div class="grow">${title ? `<p class="semibold" style="margin-bottom:2px">${esc(title)}</p>` : ''}<p>${esc(message)}</p></div>`;
  $('#toasts').appendChild(el);
  setTimeout(() => el.remove(), type === 'error' ? 6000 : 3000);
}
const fail = (e) => { if (e && e.message !== 'login required') notify('error', e.message || String(e)); };

// ----- modal (upstream Modal with traffic lights) --------------------------------
function openModal({title, body, footer, size = '', onSubmit, onClose}) {
  const root = $('#modal-root');
  root.innerHTML = `<div class="overlay"><div class="bg fade-in"></div>
    <form class="modal fade-in ${size}" id="modal">
      <div class="mhead"><div class="row" style="gap:0">
        <div class="lights"><button type="button" data-close title="Close"><span>✕</span></button><i></i><i></i></div>
        <h2>${esc(title)}</h2></div></div>
      <div class="mbody">${body}<p class="err" id="modal-err"></p></div>
      ${footer ? `<div class="mfoot">${footer}</div>` : ''}
    </form></div>`;
  const form = $('#modal');
  const close = () => { root.innerHTML = ''; document.removeEventListener('keydown', onKey); if (onClose) onClose(); };
  const onKey = (e) => { if (e.key === 'Escape') close(); };
  document.addEventListener('keydown', onKey);
  $('.overlay .bg', root).onclick = close;
  $$('[data-close]', root).forEach((b) => (b.onclick = close));
  form.onsubmit = async (e) => {
    e.preventDefault();
    if (!onSubmit) return;
    const btn = $('[type=submit]', form); if (btn) btn.disabled = true;
    try { if ((await onSubmit(form)) !== false) close(); }
    catch (err) { $('#modal-err').textContent = err.message; if (btn) btn.disabled = false; }
  };
  const first = $('.mbody input:not([type=hidden]), .mbody select, .mbody textarea', form); if (first) first.focus();
  return {close, form};
}
const field = (name, label, opts = {}) => `<div class="field"><label>${esc(label)}${opts.required ? '<span class="req">*</span>' : ''}</label>
  ${opts.type === 'select' ? `<div class="selectwrap"><select class="input" name="${name}">${opts.options.map((o) => `<option value="${esc(o.value)}" ${o.value === opts.value ? 'selected' : ''}>${esc(o.label)}</option>`).join('')}</select>${ms('expand_more', 20)}</div>`
  : opts.type === 'textarea' ? `<textarea class="input mono" rows="${opts.rows || 4}" name="${name}" placeholder="${esc(opts.placeholder || '')}">${esc(opts.value ?? '')}</textarea>`
  : `<input class="input${opts.mono ? ' mono' : ''}" name="${name}" type="${opts.type || 'text'}" value="${esc(opts.value ?? '')}" placeholder="${esc(opts.placeholder || '')}" ${opts.required ? 'required' : ''} ${opts.min != null ? `min="${opts.min}"` : ''} autocomplete="off">`}
  ${opts.hint ? `<p class="hint">${opts.hint}</p>` : ''}</div>`;
const twoButtons = (ok, okClass = 'primary') => `<div class="row gap2"><button type="submit" class="btn ${okClass} full">${esc(ok)}</button><button type="button" class="btn ghost full" data-close>Cancel</button></div>`;

function confirmModal({title = 'Confirm', message, confirmText = 'Confirm', variant = 'danger'}) {
  return new Promise((resolve) => {
    let done = false;
    openModal({
      title, size: 'sm',
      body: `<p class="msg">${esc(message)}</p>`,
      footer: `<button type="button" class="btn ghost" data-close>Cancel</button><button type="submit" class="btn ${variant}">${esc(confirmText)}</button>`,
      onSubmit: () => { done = true; resolve(true); },
      onClose: () => { if (!done) resolve(false); },
    });
  });
}
const toggle = (checked, attrs = '', size = '') => `<button type="button" role="switch" aria-checked="${!!checked}" class="toggle ${size}" ${attrs}><i></i></button>`;
const badge = (txt, variant = '', dot = false, size = 'sm') => `<span class="badge ${variant} ${size}">${dot ? '<span class="dot"></span>' : ''}${txt}</span>`;

// ----- theme -----------------------------------------------------------------------
const theme = {
  get: () => localStorage.getItem('zr-theme') || 'system',
  isDark: () => document.documentElement.classList.contains('dark'),
  set(t) {
    localStorage.setItem('zr-theme', t);
    const dark = t === 'dark' || (t === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches);
    document.documentElement.classList.toggle('dark', dark);
    $('#theme-toggle .ms').textContent = dark ? 'light_mode' : 'dark_mode';
    $('#menu [data-act=toggleTheme] .ms').textContent = dark ? 'light_mode' : 'dark_mode';
  },
  flip() { theme.set(theme.isDark() ? 'light' : 'dark'); },
};
$('#theme-toggle').onclick = theme.flip;
window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => { if (theme.get() === 'system') theme.set('system'); });

// ----- header menu / mobile nav ---------------------------------------------------
$('#menu-toggle').onclick = (e) => { e.stopPropagation(); $('#menu').classList.toggle('hidden'); };
document.addEventListener('click', (e) => { if (!e.target.closest('#menu')) $('#menu').classList.add('hidden'); });
$('#menubtn').onclick = () => document.body.classList.add('nav-open');
$('#scrim').onclick = () => document.body.classList.remove('nav-open');
$$('#nav a').forEach((a) => (a.onclick = () => document.body.classList.remove('nav-open')));

// ----- auth -------------------------------------------------------------------------
let session = {};
function showLogin() { $('#app').classList.add('hidden'); $('#login').classList.remove('hidden'); $('#login-password').focus(); }
// ----- database indicator (sidebar foot) -------------------------------------------
function dbHealth(d) {
  const t = d.turso;
  if (!t) return {cls: 'success', label: 'SQLite · local file', badge: 'Local', hint: d.path || ''};
  const where = d.mode === 'turso' ? 'Turso' : 'Turso mirror';
  if (t.online === true) return {cls: 'success', label: `${where} · online`, badge: 'Online', hint: `${t.host} · last sync ${relTime(t.lastOkAt).toLowerCase()}`};
  if (t.online === false) return {cls: 'error', label: `${where} · offline`, badge: 'Offline', hint: t.lastError || t.host};
  return {cls: 'warning', label: `${where} · connecting`, badge: 'Connecting', hint: t.host};
}
function paintDbStat(d) {
  const el = $('#dbstat'), h = dbHealth(d);
  el.className = 'dbstat ' + h.cls; el.title = h.hint;
  el.innerHTML = `<i></i><span class="truncate">${esc(h.label)}</span>${ms('database', 16)}`;
}
async function refreshDbStat() {
  try { paintDbStat(await get('/database')); }
  catch (e) {
    if (e.message === 'login required') return;
    const el = $('#dbstat'); el.className = 'dbstat error'; el.title = e.message;
    el.innerHTML = `<i></i><span class="truncate">Router unreachable</span>${ms('database', 16)}`;
  }
}
let dbTimer = null;
function showApp() {
  $('#login').classList.add('hidden'); $('#app').classList.remove('hidden');
  refreshDbStat(); if (!dbTimer) dbTimer = setInterval(refreshDbStat, 30000);
}
$('#login-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const btn = $('#login-btn'); btn.disabled = true; $('#login-error').textContent = '';
  try { await post('/login', {password: $('#login-password').value}); $('#login-password').value = ''; session = await get('/session'); showApp(); route(); }
  catch (e) { $('#login-error').textContent = e.message === 'wrong password' ? 'Invalid password' : e.message; }
  finally { btn.disabled = false; }
};
async function logout() { try { await post('/logout'); } catch (_) {} showLogin(); }

// ----- router + header (upstream Header.getPageInfo) ------------------------------
const PAGE_INFO = {
  endpoint: {title: 'Endpoint', description: 'API endpoint configuration', icon: 'api'},
  providers: {title: 'Providers', description: 'Manage your AI provider connections', icon: 'dns'},
  usage: {title: 'Usage & Analytics', description: 'Monitor your API usage, token consumption, and request logs', icon: 'bar_chart'},
  'token-saver': {title: 'Token Saver', description: 'Compress prompts and outputs to save tokens', icon: 'savings'},
  mcp: {title: 'MCP Gateway', description: 'One MCP endpoint in front of all your MCP servers', icon: 'hub'},
  'console-log': {title: 'Console Log', description: 'Live server console output', icon: 'monitor'},
  profile: {title: 'Settings', description: 'Manage your preferences', icon: 'settings'},
};
const pages = {};
const actions = {};
let searchHandler = null;
let timers = [];
const every = (ms_, fn) => timers.push(setInterval(fn, ms_));
function setHeader(info) {
  const t = $('#page-title');
  if (info.breadcrumbs) {
    t.innerHTML = `<div class="crumbs">${info.breadcrumbs.map((c, i) => `${i ? ms('chevron_right', 18) : ''}${c.href ? `<a href="${c.href}">${esc(c.label)}</a>` : `<div class="row">${c.image ? `<img src="${c.image}" alt="" onerror="this.remove()">` : ''}<h1 class="xl semibold truncate" style="font-size:24px">${esc(c.label)}</h1></div>`}`).join('')}</div>`;
  } else {
    t.innerHTML = `<h1>${info.icon ? ms(info.icon) : ''}${esc(info.title)}</h1>${info.description ? `<p class="truncate">${esc(info.description)}</p>` : ''}`;
  }
}
function setSearch(placeholder, handler) {
  searchHandler = handler;
  const s = $('#search'); s.classList.toggle('hidden', !handler);
  $('#search-input').placeholder = placeholder || ''; $('#search-input').value = '';
}
$('#search-input').oninput = (e) => { if (searchHandler) searchHandler(e.target.value); };

async function route() {
  timers.forEach(clearInterval); timers = [];
  const parts = (location.hash.replace(/^#\/?/, '') || 'endpoint').split('/');
  const name = parts[0] === 'dashboard' ? (parts[1] || 'endpoint') : parts[0];
  const page = pages[name] || pages.endpoint;
  $$('#nav a[data-page]').forEach((a) => a.classList.toggle('active', a.dataset.page === name));
  setSearch(null, null);
  setHeader(PAGE_INFO[name] || PAGE_INFO.endpoint);
  const main = $('#main');
  main.innerHTML = '<div class="loading"><span class="ms i32 spin">progress_activity</span></div>';
  try { main.innerHTML = await page(parts.slice(1)); }
  catch (e) { if (e.message !== 'login required') main.innerHTML = `<p class="danger">${esc(e.message)}</p>`; }
}
window.addEventListener('hashchange', route);
const reload = () => route();

// Delegated click actions: <button data-act="name" data-...>
document.addEventListener('click', async (ev) => {
  const b = ev.target.closest('[data-act]'); if (!b) return;
  const act = actions[b.dataset.act]; if (!act) return;
  ev.preventDefault(); ev.stopPropagation();
  b.disabled = true;
  try { await act(b.dataset, b, ev); } catch (e) { fail(e); } finally { b.disabled = false; }
});
actions.toggleTheme = () => { theme.flip(); $('#menu').classList.add('hidden'); };
actions.logout = () => { $('#menu').classList.add('hidden'); return logout(); };
// navigator.clipboard only exists in secure contexts (https or localhost); over plain
// http on the LAN fall back to a hidden textarea + execCommand('copy').
const copyText = async (text) => {
  if (navigator.clipboard && window.isSecureContext) {
    try { await navigator.clipboard.writeText(text); return; } catch { /* fall through */ }
  }
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.setAttribute('readonly', '');
  ta.style.cssText = 'position:fixed;top:0;left:0;opacity:0;pointer-events:none';
  document.body.appendChild(ta);
  ta.select();
  ta.setSelectionRange(0, text.length);
  const ok = document.execCommand('copy');
  ta.remove();
  if (!ok) { window.prompt('Copy this value:', text); }
};
actions.copy = async ({text}, b) => {
  await copyText(text);
  const icon = $('.ms', b); if (icon) { const old = icon.textContent; icon.textContent = 'check'; setTimeout(() => (icon.textContent = old), 2000); }
  const lbl = $('.lbl', b); if (lbl) { const old = lbl.textContent; lbl.textContent = 'Copied!'; setTimeout(() => (lbl.textContent = old), 2000); }
};

// ----- Donate (upstream DonateModal; channels are the maintainer's own) -----------
const DONATE = [
  {label: 'GitHub Sponsors', icon: 'favorite', color: '#DB61A2', description: 'Monthly or one-time sponsorship', url: 'https://github.com/sponsors/sudtanj'},
  {label: 'PayPal', icon: 'payments', color: '#0070BA', description: 'One-time donation via PayPal', url: 'https://www.paypal.me/sudtanj'},
  {label: 'Bitcoin', icon: 'currency_bitcoin', color: '#F7931A', description: 'Send BTC to this address', address: '32Ja5bcaAAYXFjKCRmcYe3SM2RsSUCmXtQ'},
  {label: 'Commercial license', icon: 'business_center', color: '#E56A4A', description: 'Using it in or for a business? Get a commercial license', url: 'https://www.linkedin.com/in/sudtanj/', cta: 'Contact'},
];
const donateCard = (c) => `<div class="donate-card">
    <div class="donate-ico" style="background:${c.color}20;color:${c.color}">${ms(c.icon, 26)}</div>
    <p class="semibold">${esc(c.label)}</p>
    <p class="xs muted center">${esc(c.description)}</p>
    ${c.address ? `<button type="button" class="donate-addr mono" data-act="copy" data-text="${esc(c.address)}" title="Copy address"><span class="lbl">${esc(c.address)}</span>${ms('content_copy', 14)}</button>
      <a class="btn sm donate-open" style="background:${c.color}" href="bitcoin:${esc(c.address)}">Open wallet${ms('open_in_new', 16)}</a>`
    : `<a class="btn sm donate-open" style="background:${c.color}" href="${esc(c.url)}" target="_blank" rel="noopener noreferrer">${esc(c.cta || 'Open')}${ms('open_in_new', 16)}</a>`}
  </div>`;
actions.donate = () => openModal({
  title: 'Support zilong-router', size: 'wide',
  body: `<p class="sm muted center" style="margin-bottom:24px">zilong-router is free for personal use. If it saves you time or money, consider supporting its development.</p>
    <div class="donate-grid">${DONATE.map(donateCard).join('')}</div>`,
});

// ----- Endpoint & Key (upstream EndpointPageClient) -------------------------------
let keyCache = [], settingsCache = {}, visibleKeys = new Set();
pages.endpoint = async () => {
  const [keys, settings, auto] = await Promise.all([get('/keys'), get('/settings'), get('/auto').catch(() => null)]);
  keyCache = keys; settingsCache = settings;
  const origin = location.origin;
  const isRemote = !['localhost', '127.0.0.1', '[::1]'].includes(location.hostname);
  const endpointRow = (label, url, on) => `<div class="row">
      <span class="codepill ${on ? 'on' : ''}">${esc(label)}</span>
      <input class="input mono grow" value="${esc(url)}" readonly>
      <button class="tiny" data-act="copy" data-text="${esc(url)}" title="Copy">${ms('content_copy', 18)}</button></div>`;
  return `<div class="page gap8">
    <div class="card">
      <h2 class="ct mb4">${ms('api')}API Endpoint</h2>
      <div class="col gap2">
        ${endpointRow('Local', origin + '/v1', false)}
        ${endpointRow('Anthropic', origin, false)}
        <p class="xs muted mt1">OpenAI-style tools use the <code>/v1</code> URL. Anthropic-style tools (Claude Code) use the origin as <code>ANTHROPIC_BASE_URL</code>. Models are <code>provider/model</code>, or <code>auto</code>.</p>
      </div>
      ${!settingsCache.hasPassword && isRemote ? `<div class="mt4">${securityWarning('Dashboard uses the default password — change it in Settings.', '#/profile', 'Change password')}</div>` : ''}
    </div>

    ${auto ? autoCard(auto, origin) : ''}

    <div class="card" id="require-api-key">
      <div class="row between mb4">
        <h2 class="ct">${ms('vpn_key')}API Keys</h2>
        <button class="btn primary" data-act="addKey">${ms('add')}Create Key</button>
      </div>
      <div class="row between pb4 mb4 bb">
        <div><p class="medium">Require API key</p><p class="sm muted">Requests without a valid key will be rejected</p></div>
        ${toggle(settingsCache.requireApiKey, 'data-act="setSetting" data-key="requireApiKey"')}
      </div>
      ${isRemote && !settingsCache.requireApiKey ? `<div class="mb4" style="margin-top:-8px">${securityWarning('Endpoint is exposed without an API key.')}</div>` : ''}
      ${keyCache.length === 0 ? `<div class="empty">
          <div class="ring">${ms('vpn_key', 32)}</div>
          <p class="medium" style="margin-bottom:4px">No API keys yet</p>
          <p class="sm muted mb4">Create your first API key to get started</p>
          <button class="btn primary" data-act="addKey">${ms('add')}Create Key</button></div>`
        : `<div class="col">${keyCache.map(keyRow).join('')}</div>`}
    </div>
  </div>`;
};
const autoCard = (a, origin) => {
  const example = `curl ${origin}/v1/chat/completions \\
  -H "Authorization: Bearer <api-key>" -H "Content-Type: application/json" \\
  -d '{"model":"auto","messages":[{"role":"user","content":"Hello"}]}'`;
  const candRows = (list) => list.slice(0, 12).map((c, i) => `<div class="row between" style="padding:4px 0">
      <span class="sm"><span class="muted">${i + 1}.</span> <code>${esc(c.model)}</code> <span class="xs muted">${esc(c.providerName)}</span></span>
      ${!c.supported ? '<span class="badge sm warning">format not supported</span>'
        : c.ready ? '<span class="badge sm success"><span class="dot"></span>ready</span>'
        : '<span class="badge sm error"><span class="dot"></span>cooling down</span>'}</div>`).join('');
  const rows = candRows(a.candidates);
  const strong = a.strongCandidates || [];
  const images = a.imageCandidates || [];
  return `<div class="card" id="auto-model">
    <div class="row between mb4">
      <h2 class="ct">${ms('auto_awesome')}Auto model</h2>
      <button class="btn ghost" data-act="editAuto">${ms('tune')}Configure</button>
    </div>
    <p class="sm muted mb4">Use model <code>auto</code> (like OpenRouter's <code>openrouter/auto</code>) and zilong-router sends the request to the first model that answers, skipping accounts that are cooling down. The model that answered is returned in the <code>x-zilong-router-model</code> response header.</p>
    <div class="col gap2">
      <div class="row"><span class="codepill">Model</span><input class="input mono grow" value="auto" readonly>
        <button class="tiny" data-act="copy" data-text="auto" title="Copy">${ms('content_copy', 18)}</button></div>
      <div class="row"><span class="codepill">Base URL</span><input class="input mono grow" value="${esc(origin + '/v1')}" readonly>
        <button class="tiny" data-act="copy" data-text="${esc(origin + '/v1')}" title="Copy">${ms('content_copy', 18)}</button></div>
    </div>
    <p class="sm medium mt4 mb2">How to connect</p>
    <ol class="sm muted" style="margin:0 0 8px 18px;line-height:1.7">
      <li>In your app pick an <b>OpenAI-compatible</b> provider and set the base URL above.</li>
      <li>Paste an API key from <b>API Keys</b> below (needed from other machines).</li>
      <li>Set the model to <code>auto</code>. Claude Code: <code>ANTHROPIC_BASE_URL=${esc(origin)}</code> and <code>ANTHROPIC_MODEL=auto</code>.</li>
      <li>Optional: send <code>"models": ["groq/…", "ds/…"]</code> in the request to choose the order for that call.</li>
    </ol>
    <div class="row"><textarea class="input mono grow" rows="3" readonly>${esc(example)}</textarea>
      <button class="tiny" data-act="copy" data-text="${esc(example)}" title="Copy">${ms('content_copy', 18)}</button></div>
    <p class="sm medium mt4 mb2">Order right now <span class="xs muted">(${a.source === 'autoModels' ? 'your list' : 'every model of every provider with an active account; endpoints without a catalogue are asked for their model list'}; tried in this order until one answers)</span></p>
    ${a.candidates.length ? `<div class="col">${rows}</div>${a.candidates.length > 12 ? `<p class="xs muted mt1">…and ${a.candidates.length - 12} more</p>` : ''}`
      : `<div class="warn-box">${ms('warning', 16)}<p class="grow">No candidates. Add a provider account, or check that your endpoint answers <code>GET /models</code> (otherwise add its models on the provider page).</p><a class="linkbtn" href="#/providers">Providers</a></div>`}
    <p class="sm medium mt4 mb2">Strong models only: <code>auto-strong</code> <button class="tiny" data-act="copy" data-text="auto-strong" title="Copy">${ms('content_copy', 14)}</button> <span class="xs muted">or</span> <code>auto-coding</code> <button class="tiny" data-act="copy" data-text="auto-coding" title="Copy">${ms('content_copy', 14)}</button></p>
    <p class="xs muted mb2">For agents with many tools (kongming, Claude Code, coding agents): only model families known to call tools reliably (Claude, GPT-5 and o3, Gemini Pro and Flash, DeepSeek V3.1+, Qwen Max/Plus/Coder, Kimi, GLM 4.5+, MiniMax M2+, Grok 4…), strongest first. Small models (nano, lite, mini, under 100B) are left out. ${a.strongSource === 'autoStrongModels' ? 'Using your list.' : ''}</p>
    ${strong.length ? `<div class="col">${candRows(strong)}</div>${strong.length > 12 ? `<p class="xs muted mt1">…and ${strong.length - 12} more</p>` : ''}`
      : `<div class="warn-box">${ms('warning', 16)}<p class="grow">No strong model among your accounts. Add a provider with a strong model, or list models for auto-strong under Configure.</p><a class="linkbtn" href="#/providers">Providers</a></div>`}
    <p class="sm medium mt4 mb2">Image input: <code>auto-images</code> <button class="tiny" data-act="copy" data-text="auto-images" title="Copy">${ms('content_copy', 14)}</button></p>
    <p class="xs muted mb2">For requests with screenshots or photos: only model families that read images (Claude, GPT-5/4.1/4o and o3, Gemini, Qwen VL, Llama 4, Grok 4, GLM V, Kimi K2.5+…), best first. Text-only models and image generators are left out. ${a.imageSource === 'autoImageModels' ? 'Using your list.' : ''}</p>
    ${images.length ? `<div class="col">${candRows(images)}</div>${images.length > 12 ? `<p class="xs muted mt1">…and ${images.length - 12} more</p>` : ''}`
      : `<div class="warn-box">${ms('warning', 16)}<p class="grow">No image-capable model among your accounts. Add a provider with a vision model, or list models for auto-images under Configure.</p><a class="linkbtn" href="#/providers">Providers</a></div>`}
  </div>`;
};
actions.editAuto = async () => {
  const a = await get('/auto');
  openModal({
    title: 'Auto model',
    body: field('models', 'Models to try, in order (one per line)', {type: 'textarea', rows: 6, value: a.autoModels.join('\n'),
        placeholder: 'groq/llama-3.3-70b-versatile\nds/deepseek-chat', hint: 'Leave empty to use every model of every provider with an active account (compatible endpoints are asked for their model list).'})
      + field('strongModels', 'Models for auto-strong, in order (one per line)', {type: 'textarea', rows: 4, value: (a.autoStrongModels || []).join('\n'),
        placeholder: 'anthropic/claude-sonnet-4-6\nds/deepseek-chat', hint: 'Leave empty to use every strong model of every provider with an active account, strongest first.'})
      + field('imageModels', 'Models for auto-images, in order (one per line)', {type: 'textarea', rows: 4, value: (a.autoImageModels || []).join('\n'),
        placeholder: 'gemini/gemini-2.5-flash\nopenai/gpt-4o', hint: 'Leave empty to use every image-capable model of every provider with an active account, best first.'})
      + twoButtons('Save'),
    onSubmit: async (f) => {
      const lines = (el) => el.value.split('\n').map((s) => s.trim()).filter(Boolean);
      await patch('/settings', {autoModels: lines(f.models), autoStrongModels: lines(f.strongModels), autoImageModels: lines(f.imageModels)});
      notify('success', 'Auto model saved');
      reload();
    },
  });
};
const securityWarning = (msg, href, label) => `<div class="warn-box">${ms('warning', 16)}<p class="grow">${esc(msg)}</p>${href ? `<a class="linkbtn" href="${href}">${esc(label)}</a>` : ''}</div>`;

function keyRow(k) {
  const shown = visibleKeys.has(k.id);
  return `<div class="keyrow ${k.isActive ? '' : 'off'}">
    <div class="grow">
      <p class="sm medium">${esc(k.name || 'API Key')}</p>
      <div class="row wrap mt1" style="gap:8px">
        <code>${esc(shown ? k.key : maskKey(k.key))}</code>
        <div class="row" style="gap:4px">
          <button class="tiny" data-act="toggleKeyVis" data-id="${k.id}" title="${shown ? 'Hide key' : 'Show key'}">${ms(shown ? 'visibility_off' : 'visibility', 14)}</button>
          <button class="tiny" data-act="copy" data-text="${esc(k.key)}">${ms('content_copy', 14)}</button>
        </div>
      </div>
      <p class="xs muted mt1">Created ${new Date(k.createdAt).toLocaleDateString()}</p>
      ${k.isActive ? '' : '<p class="xs mt1" style="color:#f97316">Paused</p>'}
      <div class="mt2">
        <div class="row gap3">${toggle(k.accessRestricted, `data-act="restrictKey" data-id="${k.id}"`, 'sm')}<span class="sm medium">Restrict access</span></div>
        ${k.accessRestricted ? `<div class="row wrap mt2" style="gap:6px">
          ${k.accessAllow.length === 0 ? '<span class="xs" style="color:#f97316">No models allowed — this key can call nothing.</span>' : ''}
          ${k.accessAllow.map((m) => `<span class="chip">${esc(m)}<button data-act="keyAllowRemove" data-id="${k.id}" data-model="${esc(m)}" title="Remove ${esc(m)}">${ms('close', 12)}</button></span>`).join('')}
          <button class="chip add" data-act="keyAllowAdd" data-id="${k.id}">${ms('add', 14)}Add model</button>
        </div>` : ''}
      </div>
    </div>
    <div class="row gap2 shrink0" style="padding-top:2px">
      ${toggle(k.isActive, `data-act="toggleKey" data-id="${k.id}" title="${k.isActive ? 'Pause key' : 'Resume key'}"`, 'sm')}
      <button class="tiny red del" data-act="delKey" data-id="${k.id}" style="padding:8px">${ms('delete', 18)}</button>
    </div>
  </div>`;
}


// ----- Token Saver (upstream TokenSaverClient, minus Headroom/PXPIPE) ---------------
const CAVEMAN_LEVELS = [
  {id: 'lite', label: 'Lite', desc: 'Drop filler, keep grammar'},
  {id: 'full', label: 'Full', desc: 'Drop articles, fragments OK'},
  {id: 'ultra', label: 'Ultra', desc: 'Telegraphic, max compression'},
  {id: 'wenyan-lite', label: '文 Lite', desc: 'Classical Chinese, light compression', wenyan: true},
  {id: 'wenyan', label: '文 Full', desc: 'Maximum 文言文, 80-90% reduction', wenyan: true},
  {id: 'wenyan-ultra', label: '文 Ultra', desc: 'Extreme classical compression', wenyan: true},
];
const PONYTAIL_LEVELS = [
  {id: 'lite', label: 'Lite', desc: 'Build asked, name lazier option'},
  {id: 'full', label: 'Full', desc: 'Ladder enforced: stdlib/native first'},
  {id: 'ultra', label: 'Ultra', desc: 'YAGNI extremist, deletion first'},
];
const fmtBytes = (n) => n >= 1048576 ? (n / 1048576).toFixed(1) + ' MB' : n >= 1024 ? (n / 1024).toFixed(1) + ' KB' : n + ' B';
pages['token-saver'] = async () => {
  const [s, ts] = await Promise.all([get('/settings'), get('/token-saver')]);
  settingsCache = s;
  const rtkOn = s.rtkEnabled !== false;
  const cavemanLevel = s.cavemanLevel || 'full', ponytailLevel = s.ponytailLevel || 'full';
  // Wenyan levels only make sense for Chinese readers, as upstream hides them by locale.
  const showWenyan = /^zh/i.test(navigator.language || '') || CAVEMAN_LEVELS.find((l) => l.id === cavemanLevel)?.wenyan;
  const levels = (list, cur, key) => `<div class="col" style="align-items:flex-end;gap:4px">
      <div class="row wrap" style="gap:6px;justify-content:flex-end">${list.map((l) => `<button class="lvl ${l.id === cur ? 'on' : ''}" data-act="setLevel" data-key="${key}" data-level="${l.id}" title="${esc(l.desc)}">${esc(l.label)}</button>`).join('')}</div>
      <p class="xs" style="color:var(--primary)">${esc(list.find((l) => l.id === cur)?.desc || '')}</p></div>`;
  const row = (title, link, linkText, desc, key, on, extra = '', first = false) => `<div class="row between wrap ${first ? 'pb4' : 'pt4 mt4'}" style="gap:16px;${first ? '' : 'border-top:1px solid var(--border)'}">
      <div class="grow" style="flex:1 1 220px;min-width:0">
        <p class="medium">${esc(title)} <a class="xs" href="${link}" target="_blank" rel="noreferrer" style="color:var(--primary);text-decoration:underline">(${esc(linkText)})</a></p>
        <p class="sm muted">${desc}</p>
      </div>
      <div class="row gap3 shrink0">${on ? extra : ''}${toggle(on, `data-act="setSetting" data-key="${key}"`)}</div>
    </div>`;
  const st = ts.stats;
  const pct = st.bytesBefore ? ((st.bytesSaved / st.bytesBefore) * 100).toFixed(1) : '0';
  return `<div class="page gap8">
    <div class="card" id="rtk">
      <h2 class="ct mb4">${ms('bolt')}Token Saver</h2>
      ${row('Compress tool output', 'https://github.com/rtk-ai/rtk', 'RTK', 'git/grep/ls/tree/logs → 60-90% fewer input tokens', 'rtkEnabled', rtkOn, '', true)}
      ${row('Compress LLM output', 'https://github.com/JuliusBrussee/caveman', 'Caveman', 'Terse-style system prompt → ~65% fewer output tokens (up to 87%)', 'cavemanEnabled', !!s.cavemanEnabled,
        levels(showWenyan ? CAVEMAN_LEVELS : CAVEMAN_LEVELS.filter((l) => !l.wenyan), cavemanLevel, 'cavemanLevel'))}
      ${row('Lazy senior dev', 'https://github.com/DietrichGebert/ponytail', 'Ponytail', 'Bias the model toward minimal code: YAGNI, reuse stdlib, deletion over addition', 'ponytailEnabled', !!s.ponytailEnabled,
        levels(PONYTAIL_LEVELS, ponytailLevel, 'ponytailLevel'))}
      <p class="xs muted mt4">Context compression (Headroom) is not included: it needs a Python proxy and ML models, too heavy for this device. Send <code>${esc(ts.optOutHeader)}: off</code> on a request to skip every saver for it.</p>
    </div>
    <div class="card">
      <h2 class="ct mb4">${ms('savings')}Savings since start</h2>
      <div class="row wrap" style="gap:24px">
        <div><p class="xs muted">Requests compressed</p><p class="xl semibold">${st.requests}</p></div>
        <div><p class="xs muted">Tool outputs compressed</p><p class="xl semibold">${st.hits}</p></div>
        <div><p class="xs muted">Input saved</p><p class="xl semibold">${fmtBytes(st.bytesSaved)} <span class="sm muted">(${pct}%)</span></p></div>
        <div><p class="xs muted">≈ Tokens saved</p><p class="xl semibold">${st.tokensSaved.toLocaleString()}</p></div>
      </div>
      <div class="row wrap mt4" style="gap:24px">
        <div><p class="xs muted">Requests with tool output</p><p class="xl semibold">${st.toolRequests}</p></div>
        <div><p class="xs muted">Tool outputs checked</p><p class="xl semibold">${st.toolOutputs}</p></div>
        <div><p class="xs muted">Caveman applied</p><p class="xl semibold">${st.caveman}</p></div>
        <div><p class="xs muted">Ponytail applied</p><p class="xl semibold">${st.ponytail}</p></div>
      </div>
      <p class="xs muted mt2">Counters reset when the router restarts; dashboard model tests are not counted. RTK only compresses tool outputs over 500 B that match a filter (git, grep, find, ls, tree, build logs, long logs); plain chat has none. Caveman and Ponytail savings show up as shorter answers on the Usage page.</p>
    </div>
  </div>`;
};
actions.setLevel = async ({key, level}) => {
  settingsCache = await patch('/settings', {[key]: level});
  reload();
};
actions.setSetting = async ({key}, b) => {
  const next = b.getAttribute('aria-checked') !== 'true';
  if (key === 'requireLogin' && !next && !session.local) {
    if (!(await confirmModal({title: 'Disable login', message: 'Anyone who can reach this page will be able to use the dashboard without a password. Continue?', confirmText: 'Disable'}))) return;
  }
  const s = await patch('/settings', {[key]: next});
  settingsCache = s; b.setAttribute('aria-checked', String(next));
  if (['requireLogin', 'fallbackStrategy', 'cavemanEnabled', 'ponytailEnabled'].includes(key)) reload();
};
actions.addKey = () => openModal({
  title: 'Create API Key',
  body: field('name', 'Key Name', {placeholder: 'Production Key', required: true}) + twoButtons('Create'),
  onSubmit: async (f) => {
    const k = await post('/keys', {name: f.name.value.trim()});
    setTimeout(() => openModal({
      title: 'API Key Created',
      body: `<div class="note-yellow"><b>Save this key now!</b>This is the only time you will see this key. Store it securely.</div>
        <div class="row gap2"><input class="input mono grow" value="${esc(k.key)}" readonly><button type="button" class="btn secondary" data-act="copy" data-text="${esc(k.key)}">${ms('content_copy')}<span class="lbl">Copy</span></button></div>
        <button type="submit" class="btn primary full">Done</button>`,
      onSubmit: () => {},
      onClose: reload,
    }), 50);
  },
});
actions.toggleKeyVis = ({id}) => { visibleKeys.has(id) ? visibleKeys.delete(id) : visibleKeys.add(id); reload(); };
actions.toggleKey = async ({id}, b) => {
  const k = keyCache.find((x) => x.id === id); const next = !k.isActive;
  if (!next && !(await confirmModal({title: 'Pause API Key', message: `Pause API key "${k.name || ''}"?\n\nThis key will stop working immediately but can be resumed later.`, confirmText: 'Pause'}))) return;
  await patch('/keys/' + id, {isActive: next}); reload();
};
actions.delKey = async ({id}) => {
  const k = keyCache.find((x) => x.id === id);
  if (await confirmModal({title: 'Delete API Key', message: `Delete API key "${k.name || ''}"? This cannot be undone.`, confirmText: 'Delete'})) { await del('/keys/' + id); reload(); }
};
actions.restrictKey = async ({id}, b) => {
  const k = keyCache.find((x) => x.id === id); const next = !k.accessRestricted;
  if (next && !(await confirmModal({title: 'Restrict API Key', message: `Restrict API key "${k.name || ''}"?\n\nIt will only be able to call the models you add. Until you add one, it can call nothing.`, confirmText: 'Restrict', variant: 'primary'}))) return;
  await patch('/keys/' + id, {accessRestricted: next}); reload();
};
actions.keyAllowRemove = async ({id, model}) => {
  const k = keyCache.find((x) => x.id === id);
  await patch('/keys/' + id, {accessAllow: k.accessAllow.filter((m) => m !== model)}); reload();
};
actions.keyAllowAdd = async ({id}) => {
  const k = keyCache.find((x) => x.id === id);
  const [provs, mcpList] = await Promise.all([providerList(), get('/mcp/servers').catch(() => ({servers: []}))]);
  const models = [...provs.flatMap((p) => p.models.map((m) => p.alias + '/' + m.id)), 'mcp/*', ...mcpList.servers.map((s) => 'mcp/' + s.id)];
  openModal({
    title: `Allowed for "${k.name || ''}"`,
    body: `<div class="field"><label>Model or MCP server</label><input class="input mono" name="model" list="model-list" placeholder="provider/model" required autocomplete="off"><datalist id="model-list">${models.map((m) => `<option value="${esc(m)}">`).join('')}</datalist><p class="hint">Pick from the list or type any <code>provider/model</code> or alias; <code>mcp/&lt;server&gt;</code> (or <code>mcp/*</code>) lets the key use that MCP server through <code>/mcp</code>.</p></div>` + twoButtons('Add'),
    onSubmit: async (f) => {
      const v = f.model.value.trim(); if (!v) return false;
      if (!k.accessAllow.some((m) => m.toLowerCase() === v.toLowerCase())) await patch('/keys/' + id, {accessRestricted: true, accessAllow: [...k.accessAllow, v]});
      reload();
    },
  });
};

// ----- MCP Gateway (zilong-router's own; no upstream page) --------------------------
let mcpCache = [];
const mcpPath = (id, rest = '') => '/mcp/servers/' + encodeURIComponent(id) + rest;
const slugify = (s) => s.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 32).replace(/-+$/, '');
const mcpStatus = (s) => !s.enabled ? badge('Disabled')
  : s.status === 'running' ? badge('Running', 'success', true)
  : s.status === 'busy' ? badge('Starting', 'warning', true)
  : s.stats.lastError && s.toolCount == null ? badge('Error', 'error', true)
  : badge('Stopped', '', true);
pages.mcp = async () => {
  const d = await get('/mcp/servers');
  mcpCache = d.servers;
  const url = location.origin + '/mcp';
  const claude = `claude mcp add --transport http zilong-router ${url} --header "Authorization: Bearer <api-key>"`;
  const jsonCfg = JSON.stringify({mcpServers: {'zilong-router': {type: 'http', url, headers: {Authorization: 'Bearer <api-key>'}}}}, null, 2);
  const copyRow = (label, value) => `<div class="row"><span class="codepill">${esc(label)}</span><input class="input mono grow" value="${esc(value)}" readonly>
      <button class="tiny" data-act="copy" data-text="${esc(value)}" title="Copy">${ms('content_copy', 18)}</button></div>`;
  const buttons = `<button class="btn ghost" data-act="mcpImport">${ms('upload')}Import JSON</button><button class="btn primary" data-act="mcpAdd">${ms('add')}Add server</button>`;
  const idle = d.idleSecs ? `stop after ${d.idleSecs >= 60 ? Math.round(d.idleSecs / 6) / 10 + ' min' : d.idleSecs + ' s'} without calls` : 'keep running once started (MCP_IDLE_SECS=0)';
  return `<div class="page gap8">
    <div class="card">
      <h2 class="ct mb4">${ms('hub')}Connect your agent</h2>
      <p class="sm muted mb4">One MCP endpoint for every server below. Instead of hundreds of tool definitions the agent gets four meta tools (<code>search_tools</code>, <code>get_tool_schemas</code>, <code>call_tool</code>, <code>list_servers</code>), so its context stays small however many servers you add.</p>
      <div class="col gap2">${copyRow('Meta tools', url)}${copyRow('All tools', url + '/all')}</div>
      <p class="xs muted mt1"><code>/mcp/all</code> lists every tool directly as <code>server__tool</code> instead. From other machines send an API key from <a href="#/endpoint">Endpoint &amp; Key</a>; a restricted key only reaches the servers on its list as <code>mcp/&lt;server&gt;</code> (or <code>mcp/*</code> for all).</p>
      <p class="sm medium mt4 mb2">Claude Code</p>
      <div class="row"><input class="input mono grow" value="${esc(claude)}" readonly><button class="tiny" data-act="copy" data-text="${esc(claude)}" title="Copy">${ms('content_copy', 18)}</button></div>
      <p class="sm medium mt4 mb2">Cursor, Windsurf, VS Code and other JSON configs</p>
      <div class="row"><textarea class="input mono grow" rows="9" readonly>${esc(jsonCfg)}</textarea><button class="tiny" data-act="copy" data-text="${esc(jsonCfg)}" title="Copy">${ms('content_copy', 18)}</button></div>
    </div>
    <div class="card">
      <div class="row between wrap mb4" style="gap:8px"><h2 class="ct">${ms('dns')}Servers</h2><div class="row gap2">${buttons}</div></div>
      ${mcpCache.length === 0 ? `<div class="empty">
          <div class="ring">${ms('hub', 32)}</div>
          <p class="medium" style="margin-bottom:4px">No MCP servers yet</p>
          <p class="sm muted mb4">Add one, or paste the <code>mcpServers</code> JSON from Claude Desktop, Cursor or VS Code.</p>
          <div class="row gap2" style="justify-content:center">${buttons}</div></div>`
        : `<div class="col">${mcpCache.map(mcpRow).join('')}</div>`}
      <p class="xs muted mt4">Remote servers only (Streamable HTTP or SSE): the router never runs server processes itself. Connections open on first use and ${idle}. Tool lists are cached, so searching does not connect; use ${ms('refresh', 14)} after a server gains tools. Calls and errors also go to the <a href="#/console-log">Console Log</a>.</p>
    </div>
  </div>`;
};
function mcpAuthLine(s) {
  const a = s.auth;
  if (!a) return '';
  if (a.status === 'needsSignIn') {
    return `<p class="xs mt1 row wrap" style="gap:8px">${badge('Sign-in required', 'warning', true)}<a href="#" data-act="mcpSignIn" data-id="${esc(s.id)}" style="color:var(--primary)">Sign in with OAuth</a>${a.error ? `<span class="muted break">${esc(a.error)}</span>` : ''}</p>`;
  }
  const mins = a.expiresAt ? Math.round((new Date(a.expiresAt) - Date.now()) / 60000) : null;
  const left = mins == null ? '' : mins <= 0 ? 'token expired' : mins < 120 ? `token expires in ${mins} min` : `token expires in ${Math.round(mins / 60)} h`;
  const until = left ? ` · ${left}${a.refreshable ? ', refreshed automatically' : ''}` : '';
  return `<p class="xs muted mt1">${badge('OAuth', 'success', true)} signed in${a.scope ? ` · <span class="mono">${esc(a.scope)}</span>` : ''}${until}</p>`;
}
function mcpRow(s) {
  const target = s.url;
  const st = s.stats || {};
  const calls = st.calls ? ` · ${fmt(st.calls)} calls, ${fmt(st.errors)} errors, ${Math.round(st.totalMs / st.calls)} ms avg` : '';
  const tools = s.toolCount != null ? `<a href="#" data-act="mcpTools" data-id="${esc(s.id)}" style="color:var(--primary)">${s.toolCount} tools</a> · listed ${relTime(s.toolsFetchedAt).toLowerCase()}` : 'Tools not listed yet';
  return `<div class="keyrow ${s.enabled ? '' : 'off'}">
    <div class="grow" style="min-width:0">
      <div class="row wrap" style="gap:8px"><p class="sm medium">${esc(s.name)}</p><code>${esc(s.id)}</code>${badge(s.transport === 'sse' ? 'SSE' : 'HTTP', 'primary')}${mcpStatus(s)}</div>
      <p class="xs muted mono mt1 truncate" title="${esc(target)}">${esc(target)}</p>
      ${s.description ? `<p class="xs muted mt1">${esc(s.description)}</p>` : ''}
      <p class="xs muted mt1">${tools}${calls}</p>
      ${st.lastError ? `<p class="xs danger mt1 break">${esc(st.lastError)}</p>` : ''}
      ${mcpAuthLine(s)}
    </div>
    <div class="row gap2 shrink0" style="padding-top:2px">
      ${s.auth && s.auth.status === 'signedIn'
        ? `<button class="tiny" data-act="mcpSignOut" data-id="${esc(s.id)}" title="Sign out (forget the OAuth tokens)">${ms('logout', 18)}</button>`
        : `<button class="tiny" data-act="mcpSignIn" data-id="${esc(s.id)}" title="Sign in with OAuth">${ms('login', 18)}</button>`}
      <button class="tiny" data-act="mcpRefresh" data-id="${esc(s.id)}" title="Reconnect and list tools">${ms('refresh', 18)}</button>
      ${s.status === 'running' ? `<button class="tiny" data-act="mcpStop" data-id="${esc(s.id)}" title="Disconnect">${ms('stop_circle', 18)}</button>` : ''}
      <button class="tiny" data-act="mcpEdit" data-id="${esc(s.id)}" title="Edit">${ms('edit', 18)}</button>
      ${toggle(s.enabled, `data-act="mcpToggle" data-id="${esc(s.id)}" title="${s.enabled ? 'Disable' : 'Enable'}"`, 'sm')}
      <button class="tiny red del" data-act="mcpDelete" data-id="${esc(s.id)}" style="padding:8px">${ms('delete', 18)}</button>
    </div>
  </div>`;
}
const linesToMap = (text, sep) => Object.fromEntries(text.split('\n').map((l) => l.trim()).filter(Boolean)
  .map((l) => { const i = l.indexOf(sep); return i < 0 ? [l, ''] : [l.slice(0, i).trim(), l.slice(i + 1).trim()]; }));
const mapToLines = (o, sep) => Object.entries(o || {}).map(([k, v]) => k + sep + v).join('\n');
function mcpForm(s) {
  const edit = !!s;
  s = s || {transport: 'http', headers: {}};
  const {form} = openModal({
    title: edit ? `Edit ${s.name}` : 'Add MCP server',
    body: `<div class="row gap2" style="align-items:flex-start"><div class="grow">${field('name', 'Name', {value: s.name, placeholder: 'GitHub', required: true})}</div>
        <div class="grow">${field('sid', 'Id', {value: s.id, placeholder: 'github', mono: true, hint: edit ? 'Cannot be changed.' : 'a-z, 0-9 and -. Tools are named <code>id__tool</code>.'})}</div></div>
      ${field('description', 'Description', {value: s.description, placeholder: 'Issues, pull requests and code search', hint: 'Shown to agents by <code>list_servers</code>, so they know what the server is for.'})}
      ${field('transport', 'Transport', {type: 'select', value: s.transport, options: [{value: 'http', label: 'Streamable HTTP (one URL, usually …/mcp)'}, {value: 'sse', label: 'SSE (older servers, usually …/sse)'}]})}
      ${field('url', 'URL', {value: s.url, placeholder: 'https://example.com/mcp', mono: true, required: true})}
      ${field('headers', 'Headers (Name: value, one per line)', {type: 'textarea', rows: 3, value: mapToLines(s.headers, ': '), placeholder: 'Authorization: Bearer …'})}
      ${field('timeoutSecs', 'Call timeout in seconds', {type: 'number', min: 0, value: s.timeoutSecs || '', placeholder: '120'})}
      <details class="mb4" ${s.oauthClientId || s.oauthScope ? 'open' : ''}><summary class="sm medium" style="cursor:pointer">OAuth (optional)</summary>
        <p class="hint mt1 mb2">Servers that need OAuth get a Sign in button after a failed call; the router registers itself with their authorization server. Only fill these in if that server does not allow registration.</p>
        <div class="row gap2" style="align-items:flex-start"><div class="grow">${field('oauthClientId', 'Client id', {value: s.oauthClientId, mono: true})}</div>
          <div class="grow">${field('oauthClientSecret', 'Client secret', {type: 'password', value: s.oauthClientSecret, placeholder: 'optional'})}</div></div>
        ${field('oauthScope', 'Scopes', {value: s.oauthScope, placeholder: 'as the server advertises', mono: true})}
      </details>
      ${edit ? '<p class="hint mb4">Values of secret-looking headers show as ••••••••; leave them as they are to keep them.</p>' : ''}
      ${twoButtons(edit ? 'Save' : 'Add server')}`,
    onSubmit: async (f) => {
      const body = {
        name: f.name.value.trim(), description: f.description.value.trim(), transport: f.transport.value,
        url: f.url.value.trim(), headers: linesToMap(f.headers.value, ':'), timeoutSecs: Number(f.timeoutSecs.value) || 0,
        oauthClientId: f.oauthClientId.value.trim(), oauthClientSecret: f.oauthClientSecret.value, oauthScope: f.oauthScope.value.trim(),
      };
      if (edit) await patch(mcpPath(s.id), body);
      else await post('/mcp/servers', {...body, id: f.sid.value.trim() || slugify(body.name), enabled: true});
      notify('success', edit ? 'Server saved' : 'Server added. Its tools are listed on first use, or now with the refresh button.');
      reload();
    },
  });
  if (edit) form.sid.readOnly = true;
  else {
    form.sid.oninput = () => (form.sid.dataset.touched = '1');
    form.name.oninput = () => { if (!form.sid.dataset.touched) form.sid.value = slugify(form.name.value); };
  }
}
const mcpToolsModal = (title, tools) => openModal({
  title: `${title} · ${tools.length} tools`, size: 'wide',
  body: tools.length ? `<div class="col">${tools.map((t) => `<div style="padding:8px 0;border-bottom:1px solid var(--divider)">
      <p class="sm medium mono">${esc(t.name)}</p>
      ${t.description ? `<p class="xs muted mt1">${esc(t.description.length > 300 ? t.description.slice(0, 300) + '…' : t.description)}</p>` : ''}
      ${t.parameters ? `<p class="xs mono mt1">${esc(t.parameters)}</p>` : ''}</div>`).join('')}</div>`
    : '<p class="sm muted">This server has no tools.</p>',
});
actions.mcpAdd = () => mcpForm(null);
actions.mcpEdit = ({id}) => mcpForm(mcpCache.find((s) => s.id === id));
actions.mcpToggle = async ({id}, b) => { await patch(mcpPath(id), {enabled: b.getAttribute('aria-checked') !== 'true'}); reload(); };
actions.mcpStop = async ({id}) => { await post(mcpPath(id, '/stop')); reload(); };
actions.mcpDelete = async ({id}) => {
  const s = mcpCache.find((x) => x.id === id);
  if (await confirmModal({title: 'Delete MCP server', message: `Delete "${s.name}"? Agents lose its tools.`, confirmText: 'Delete'})) { await del(mcpPath(id)); reload(); }
};
actions.mcpTools = async ({id}) => {
  const s = mcpCache.find((x) => x.id === id);
  mcpToolsModal(s.name, (await get(mcpPath(id, '/tools'))).tools);
};
// OAuth sign-in to an MCP server: the authorization server sends the browser back to
// /mcp/oauth/callback on this origin, which finishes by itself and tells this tab; if
// that address does not reach the router, the user pastes it here instead.
actions.mcpSignIn = async ({id}) => {
  const s = mcpCache.find((x) => x.id === id);
  const r = await post(mcpPath(id, '/oauth/start'), {origin: location.origin});
  let channel = null;
  const m = openModal({
    title: 'Sign in to ' + s.name,
    body: (r.redirectUri.startsWith(location.origin + '/') ? `<ol class="sm muted" style="margin:0 0 12px 18px;line-height:1.7">
        <li>Open the sign-in page, log in and allow access.</li>
        <li>You come back to <code>${esc(r.redirectUri)}</code>, which finishes the sign-in; this window closes by itself.</li>
        <li>If that page does not load, copy its whole address from the address bar and paste it below.</li></ol>`
      : `<ol class="sm muted" style="margin:0 0 12px 18px;line-height:1.7">
        <li>Open the sign-in page, log in and allow access.</li>
        <li>This server only accepts https or <code>localhost</code> addresses, so it sends you to <code>${esc(r.redirectUri)}</code>. That page will not load (unless the router runs on this computer); that is expected.</li>
        <li>Copy its whole address from the address bar and paste it below.</li></ol>
        <p class="xs muted mb4">Open this dashboard over https (for example Tailscale Serve) to skip the copy and paste.</p>`)
      + `
      <a class="btn primary full mb4" href="${esc(r.authUrl)}" target="_blank" rel="noopener">${ms('open_in_new')}Open sign-in page</a>`
      + field('callback', 'Callback URL', {type: 'textarea', rows: 3, placeholder: r.redirectUri + '?code=…&state=…', required: true})
      + twoButtons('Connect'),
    onSubmit: async (f) => {
      await post(mcpPath(id, '/oauth/exchange'), {session: r.session, callback: f.callback.value.trim()});
      notify('success', `${s.name}: signed in`); reload();
    },
    onClose: () => { if (channel) channel.close(); },
  });
  try {
    channel = new BroadcastChannel('zilong-router-mcp-oauth');
    channel.onmessage = (e) => {
      if (!e.data || e.data.server !== id) { if (e.data && !e.data.ok) notify('error', e.data.error); return; }
      m.close(); notify('success', `${s.name}: signed in`); reload();
    };
  } catch (_) {}
};
actions.mcpSignOut = async ({id}) => {
  const s = mcpCache.find((x) => x.id === id);
  if (await confirmModal({title: 'Sign out', message: `Forget the OAuth tokens for "${s.name}"? Agents lose its tools until you sign in again.`, confirmText: 'Sign out'})) {
    await del(mcpPath(id, '/oauth')); reload();
  }
};
actions.mcpRefresh = async ({id}, b) => {
  const icon = $('.ms', b); if (icon) icon.classList.add('spin');
  try {
    const r = await post(mcpPath(id, '/refresh'));
    mcpToolsModal(r.server.name, r.tools);
  } finally { reload(); }
};
actions.mcpImport = () => openModal({
  title: 'Import MCP servers', size: 'wide',
  body: field('config', 'mcpServers JSON', {type: 'textarea', rows: 12,
      placeholder: '{\n  "mcpServers": {\n    "github": {\n      "type": "http",\n      "url": "https://api.githubcopilot.com/mcp/",\n      "headers": {"Authorization": "Bearer ghp_…"}\n    },\n    "legacy": {"type": "sse", "url": "https://example.com/sse"}\n  }\n}',
      hint: 'As written by Claude Desktop (<code>claude_desktop_config.json</code>), Cursor (<code>~/.cursor/mcp.json</code>), Windsurf or VS Code (<code>servers</code>). Only servers with a URL are imported; local <code>command</code> servers are not supported. Servers whose id already exists are skipped.'})
    + twoButtons('Import'),
  onSubmit: async (f) => {
    const r = await post('/mcp/import', {config: f.config.value});
    const rejected = r.rejected || [];
    notify(rejected.length && !r.added.length ? 'error' : 'success', `Added ${r.added.length} server(s)${r.skipped.length ? `; skipped existing: ${r.skipped.join(', ')}` : ''}${rejected.length ? `; not imported: ${rejected.join('; ')}` : ''}`);
    reload();
  },
});

// ----- Providers (upstream providers/page.js) --------------------------------------
let provCache = null, connCache = null;
async function providerList() { return provCache || (provCache = await get('/providers')); }
async function connectionList() { return connCache || (connCache = await get('/connections')); }
const invalidate = () => { provCache = null; connCache = null; };
const isCustomId = (id) => id === 'openai-compatible' || id === 'anthropic-compatible';
const CUSTOM_META = {
  'openai-compatible': {name: 'OpenAI Compatible', color: '#10A37F', textIcon: 'OC', icon: '/providers/oai-cc.png', badge: 'Chat', api: 'Chat Completions', path: '/chat/completions', defaultUrl: 'https://api.openai.com/v1', prefixPh: 'oc-prod', namePh: 'OpenAI Compatible (Prod)', hint: 'Use the base URL (ending in /v1) for your OpenAI-compatible API.'},
  'anthropic-compatible': {name: 'Anthropic Compatible', color: '#D97757', textIcon: 'AC', icon: '/providers/anthropic-m.png', badge: 'Messages', api: 'Messages API', path: '/messages', defaultUrl: 'https://api.anthropic.com/v1', prefixPh: 'ac-prod', namePh: 'Anthropic Compatible (Prod)', hint: 'Use the base URL (ending in /v1) for your Anthropic-compatible API. The system will append /messages.'},
};
const errorTag = (c) => {
  const n = Number(c.errorCode); if (Number.isFinite(n) && n >= 400) return String(n);
  const m = String(c.lastError || '').toLowerCase();
  if (m.includes('invalid api key') || m.includes('unauthorized') || m.includes('revoked')) return 'AUTH';
  if (m.includes('rate limit')) return '429';
  return c.lastError ? 'ERR' : null;
};
const effStatus = (c) => {
  if (c.testStatus === 'unavailable' && !isCooling(c)) return 'active';
  return c.testStatus || (c.lastError ? 'error' : 'active');
};
function statsFor(conns) {
  const connected = conns.filter((c) => c.isActive && ['active', 'success'].includes(effStatus(c))).length;
  const errs = conns.filter((c) => ['error', 'expired', 'unavailable'].includes(effStatus(c)));
  const latest = errs[0];
  return {connected, error: errs.length, total: conns.length, allDisabled: conns.length > 0 && conns.every((c) => !c.isActive), errorCode: latest ? errorTag(latest) : null};
}
const statusDisplay = (s) => {
  const parts = [];
  if (s.connected > 0) parts.push(badge(`${s.connected} Connected`, 'success', true));
  if (s.error > 0) parts.push(badge(`${s.error} Error${s.errorCode ? ` (${s.errorCode})` : ''}`, 'error', true));
  return parts.length ? parts.join('') : '<span class="muted">No connections</span>';
};
const providerIcon = (p, size = 30) => `<div class="picon" style="background:${esc(p.color ? (p.color.length > 7 ? p.color : p.color + '15') : 'var(--surface-2)')}">
  <img src="${esc(p.icon || '/providers/' + p.id + '.png')}" alt="" width="${size}" height="${size}" loading="lazy" onerror="this.outerHTML='<span style=\\'color:${esc(p.color || '')}\\'>${esc(p.textIcon || p.id.slice(0, 2).toUpperCase())}</span>'"></div>`;

function providerCard(p, stats, extraBadge) {
  return `<a class="pcard" href="#/providers/${esc(p.href || p.id)}"><div class="card xs hover ${stats.allDisabled ? 'op50' : ''}">
    <div class="row between gap3">
      <div class="row gap3 grow">${providerIcon(p)}
        <div class="grow"><h3 class="truncate">${esc(p.name)}</h3>
          <div class="meta">${stats.allDisabled ? badge(ms('pause_circle', 12) + 'Disabled', '', false) : p.noAuth ? badge('Ready', 'success', true) : statusDisplay(stats) + (extraBadge || '')}</div></div></div>
      ${stats.total > 0 ? `<div class="tgl shrink0">${toggle(!stats.allDisabled, `data-act="toggleProvider" data-id="${esc(p.key)}" title="${stats.allDisabled ? 'Enable provider' : 'Disable provider'}"`, 'sm')}</div>` : ''}
    </div></div></a>`;
}

let provFilter = 'all', provSearch = '', showAllApikey = false;
pages.providers = async (parts) => {
  if (parts[0]) return providerDetail(parts[0]);
  invalidate();
  const [provs, conns] = await Promise.all([providerList(), connectionList()]);
  setSearch('Search providers...', (q) => { provSearch = q.trim().toLowerCase(); $('#prov-body').innerHTML = render(); });
  const render = () => {
    const match = (name) => !provSearch || (name || '').toLowerCase().includes(provSearch);
    const filt = (s, noAuth) => provFilter === 'all' || (noAuth ? 'active' : s.total === 0 ? 'none' : s.allDisabled ? 'inactive' : 'active') === provFilter;
    const custom = conns.filter((c) => isCustomId(c.provider)).map((c) => {
      const m = CUSTOM_META[c.provider], s = statsFor([c]);
      return {card: providerCard({id: c.provider, key: 'custom:' + c.id, href: 'custom:' + c.id, name: c.name || m.name, color: m.color, textIcon: m.textIcon, icon: m.icon}, s, badge(m.badge)), name: c.name || m.name, s};
    }).filter((x) => match(x.name) && filt(x.s));
    const regular = provs.filter((p) => !isCustomId(p.id)).map((p) => ({p, s: statsFor(conns.filter((c) => c.provider === p.id))}))
      .filter((x) => match(x.p.name) && filt(x.s, x.p.noAuth));
    const oauth = regular.filter((x) => x.p.oauth);
    const free = regular.filter((x) => !oauth.includes(x) && (x.p.noAuth || /free/.test(x.p.category || '')));
    const apikey = regular.filter((x) => !free.includes(x) && !oauth.includes(x)).sort((a, b) => (a.s.total > 0 ? 0 : 1) - (b.s.total > 0 ? 0 : 1) || a.p.name.localeCompare(b.p.name));
    const searching = !!provSearch || provFilter !== 'all';
    const visible = searching || showAllApikey ? apikey : apikey.slice(0, 20);
    const section = (title, items, right) => `<div class="col gap4"><div class="row between wrap gap3"><h2 class="sect-title">${title}</h2>${right || ''}</div>${items}</div>`;
    return `<div class="row end">
        <select class="small" id="prov-filter" aria-label="Filter providers by connection status">${[['all', 'All'], ['active', 'Active'], ['inactive', 'Inactive'], ['none', 'No connection']].map(([v, l]) => `<option value="${v}" ${provFilter === v ? 'selected' : ''}>${l}</option>`).join('')}</select></div>
      ${!custom.length && !regular.length ? `<div class="dashed tall">${ms('search_off', 32)}<p class="sm muted">No providers match your search or filters</p></div>` : ''}
      ${section('Custom Providers (OpenAI/Anthropic Compatible)',
        custom.length ? `<div class="pgrid">${custom.map((x) => x.card).join('')}</div>` : `<div class="dashed">${ms('extension', 18)}<span>No custom providers — use buttons above to add OpenAI/Anthropic compatible endpoints</span></div>`,
        `<div class="row gap2 wrap"><button class="btn sm primary" data-act="addCompatible" data-variant="anthropic-compatible">${ms('add')}Add Anthropic Compatible</button><button class="btn sm white" data-act="addCompatible" data-variant="openai-compatible">${ms('add')}Add OpenAI Compatible</button></div>`)}
      ${oauth.length ? section('OAuth Providers', `<div class="pgrid">${oauth.map((x) => providerCard({...x.p, key: x.p.id}, x.s)).join('')}</div>`) : ''}
      ${free.length ? section('Free Tier Providers', `<div class="pgrid">${free.map((x) => providerCard({...x.p, key: x.p.id}, x.s)).join('')}</div>`) : ''}
      ${apikey.length ? section('API Key Providers', `<div class="pgrid">${visible.map((x) => providerCard({...x.p, key: x.p.id}, x.s)).join('')}</div>` +
        (!searching && !showAllApikey && apikey.length > 20 ? `<button class="showall" data-act="showAllApikey">${ms('expand_more', 16)}Show all ${apikey.length} providers</button>` : '')) : ''}`;
  };
  setTimeout(() => { const f = $('#prov-filter'); if (f) f.onchange = () => { provFilter = f.value; $('#prov-body').innerHTML = render(); }; });
  return `<div class="page" id="prov-body">${render()}</div>`;
};
actions.showAllApikey = () => { showAllApikey = true; reload(); };
actions.toggleProvider = async ({id}, b) => {
  const conns = await connectionList();
  const list = id.startsWith('custom:') ? conns.filter((c) => c.id === id.slice(7)) : conns.filter((c) => c.provider === id);
  const next = list.every((c) => !c.isActive);
  await Promise.allSettled(list.map((c) => patch('/connections/' + c.id, {isActive: next})));
  invalidate(); reload();
};

// Compatible endpoint modal (upstream AddCompatibleModal + the API key it needs)
actions.addCompatible = ({variant}) => {
  const m = CUSTOM_META[variant];
  openModal({
    title: 'Add ' + m.name,
    body: field('name', 'Name', {placeholder: m.namePh, required: true}) + field('prefix', 'Prefix', {placeholder: m.prefixPh, required: true, hint: 'Models are addressed as <code>prefix/model</code>.'})
      + (variant === 'openai-compatible' ? field('apiType', 'API Type', {type: 'select', value: 'chat', options: [{value: 'chat', label: 'Chat Completions'}]}) : '')
      + field('baseUrl', 'Base URL', {value: m.defaultUrl, required: true, hint: m.hint}) + field('apiKey', 'API Key', {type: 'password', placeholder: 'sk-...', required: true})
      + twoButtons('Add'),
    onSubmit: async (f) => {
      await post('/connections', {provider: variant, apiKey: f.apiKey.value, name: f.name.value.trim(), baseUrl: f.baseUrl.value.trim(), prefix: f.prefix.value.trim()});
      invalidate(); notify('success', m.name + ' added'); reload();
    },
  });
};

// ----- Provider detail (upstream providers/[id]/page.js) ---------------------------
async function providerDetail(rawId) {
  invalidate();
  const [provs, conns, settings] = await Promise.all([providerList(), connectionList(), get('/settings')]);
  settingsCache = settings;
  const custom = rawId.startsWith('custom:');
  const node = custom ? conns.find((c) => c.id === rawId.slice(7)) : null;
  const providerId = custom ? (node ? node.provider : 'openai-compatible') : rawId;
  const prov = provs.find((p) => p.id === providerId || p.alias === providerId);
  if (!prov || (custom && !node)) { setHeader(PAGE_INFO.providers); return `<div class="dashed tall">${ms('search_off', 32)}<p class="sm muted">Provider not found</p><a class="btn sm secondary" href="#/providers">Back to providers</a></div>`; }
  const meta = custom ? CUSTOM_META[providerId] : null;
  const name = custom ? (node.name || meta.name) : prov.name;
  const icon = custom ? meta.icon : '/providers/' + prov.id + '.png';
  setHeader({breadcrumbs: [{label: 'Providers', href: '#/providers'}, {label: name, image: icon}]});
  const list = custom ? [node] : conns.filter((c) => c.provider === prov.id).sort((a, b) => (a.priority ?? 999) - (b.priority ?? 999));
  const prefix = custom ? (node.prefix || providerId) : prov.alias;
  const [customModels, hiddenModels] = await Promise.all([get('/custom-models/' + encodeURIComponent(prefix)), get('/models/' + encodeURIComponent(prefix) + '/hidden')]);
  // A live model list needs an account to ask with (OpenCode Free needs none).
  const canRefresh = custom || prov.id === 'opencode' || list.some((c) => c.isActive);
  const strat = (settings.providerStrategies || {})[prov.id] || {};
  const rr = (strat.fallbackStrategy || settings.fallbackStrategy) === 'round-robin';
  every(1000, () => $$('[data-cool]').forEach((el) => { el.textContent = '⏱ ' + cooldownText(el.dataset.cool); if (!cooldownText(el.dataset.cool)) el.remove(); }));

  const connRow = (c, i) => {
    const cooling = c.isActive && isCooling(c), until = lockUntil(c), st = effStatus(c);
    const variant = !c.isActive ? '' : ['active', 'success'].includes(st) ? 'success' : ['error', 'expired', 'unavailable'].includes(st) ? 'error' : '';
    return `<div><div class="conn ${c.isActive ? '' : 'off'}">
      <div class="row gap3 grow start">
        <div class="arrows"><button data-act="moveConn" data-id="${c.id}" data-dir="-1" ${i === 0 ? 'disabled' : ''}>${ms('keyboard_arrow_up', 14)}</button><button data-act="moveConn" data-id="${c.id}" data-dir="1" ${i === list.length - 1 ? 'disabled' : ''}>${ms('keyboard_arrow_down', 14)}</button></div>
        ${ms('key', 16)}
        <div class="grow">
          <p class="name truncate">${esc(c.name || c.email || (c.authType === 'oauth' ? 'Signed-in account' : 'API Key'))}</p>
          ${c.keyMasked ? `<p class="xs muted truncate mono">${esc(c.keyMasked)}${custom ? ' · ' + esc(c.baseUrl || '') : ''}</p>` : ''}
          <div class="tags">
            ${badge(!c.isActive ? 'disabled' : st, variant, true)}${badge(c.authType === 'oauth' ? 'OAuth' : 'API Key')}${c.authType === 'oauth' && c.expiresAt ? `<span class="xs muted" title="Refreshed automatically">token until ${esc(new Date(c.expiresAt).toLocaleString())}</span>` : ''}
            ${cooling && until ? `<span class="cool" data-cool="${esc(until)}">⏱ ${cooldownText(until)}</span>` : ''}
            ${c.lastError && c.isActive ? `<span class="lasterr truncate" title="${esc(c.lastError)}">${esc(c.lastError)}</span>` : ''}
            <span class="xs muted">#${c.priority ?? i + 1}</span>
          </div>
        </div>
      </div>
      <div class="row gap2 shrink0">
        <div class="acts">
          ${cooling || c.lastError ? `<button class="act" data-act="unlockConn" data-id="${c.id}">${ms('lock_open', 18)}<span>Unlock</span></button>` : ''}
          <button class="act" data-act="editConn" data-id="${c.id}">${ms('edit', 18)}<span>Edit</span></button>
          <button class="act danger" data-act="delConn" data-id="${c.id}">${ms('delete', 18)}<span>Delete</span></button>
        </div>
        ${toggle(c.isActive, `data-act="toggleConn" data-id="${c.id}" title="${c.isActive ? 'Disable connection' : 'Enable connection'}"`, 'sm')}
      </div>
    </div></div>`;
  };
  const modelRow = (id, nm, isCustom) => `<div class="mrow"><div class="in">
      ${ms('smart_toy', 16)}
      <div class="col gap1 grow"><code class="truncate">${esc(prefix)}/${esc(id)}</code>${nm && nm !== id ? `<span class="nm truncate">${esc(nm)}</span>` : ''}</div>
      <span class="mtest" data-model="${esc(prefix)}/${esc(id)}"></span>
      <div class="ops"><button class="tiny" data-act="copy" data-text="${esc(prefix)}/${esc(id)}" title="Copy">${ms('content_copy', 14)}</button>
      ${isCustom ? `<button class="tiny danger" data-act="delCustomModel" data-prefix="${esc(prefix)}" data-model="${esc(id)}" title="Remove custom model">${ms('close', 14)}</button>` : ''}</div>
    </div></div>`;
  const keyBtn = `<button class="btn sm ${prov.oauth && !custom ? 'secondary' : 'primary'}" data-act="addConn" data-provider="${esc(prov.id)}" data-node="${custom ? node.id : ''}">${ms(custom ? 'key' : 'add')}${custom ? 'Add API Key' : prov.oauth ? 'Add API Key' : 'Add Connection'}</button>`;
  const signInBtn = !custom && prov.oauth ? `<button class="btn sm primary" data-act="oauthLogin" data-provider="${esc(prov.id)}">${ms('login')}Sign in with ${esc(prov.name)}</button>` : '';
  const addBtn = `<div class="row gap2 wrap">${signInBtn}${custom || !prov.oauth || prov.acceptsApiKey ? keyBtn : ''}</div>`;

  return `<div class="page">
    ${!custom && prov.riskNotice ? `<div class="warn-box">${ms('warning', 16)}<p class="grow">${esc(prov.riskNotice)}</p></div>` : ''}
    ${custom ? `<div class="card"><div class="row between start wrap gap3">
        <div class="grow"><h2 class="ct">${esc(meta.name)} Details</h2><p class="sm muted break">${esc(meta.api)} · ${esc((node.baseUrl || '').replace(/\/$/, ''))}${esc(meta.path)} · prefix <code>${esc(prefix)}</code></p></div>
        <div class="row gap2 wrap"><button class="btn sm secondary" data-act="editConn" data-id="${node.id}">${ms('edit')}Edit</button><button class="btn sm secondary" data-act="delConn" data-id="${node.id}" data-back="1">${ms('delete')}Delete</button></div>
      </div></div>` : ''}
    <div class="card">
      <div class="row between wrap gap3 mb4">
        <h2 class="ct">Connections</h2>
        <div class="row gap4 wrap">
          <div class="row gap2 wrap"><span class="xs muted medium">Round Robin</span>${toggle(rr, `data-act="providerRR" data-id="${esc(prov.id)}"`)}
            ${rr ? `<div class="row" style="gap:6px"><span class="xs muted">Sticky:</span><input type="number" min="1" class="input" style="width:56px;padding:4px 8px;font-size:12px" value="${esc(strat.stickyRoundRobinLimit ?? settings.stickyRoundRobinLimit ?? 3)}" data-sticky="${esc(prov.id)}"></div>` : ''}
          </div>
        </div>
      </div>
      ${list.length === 0 ? `<div class="row between wrap gap3">
          <div class="row gap3"><div class="icon-tile tint-primary" style="border-radius:50%;width:36px;height:36px">${ms('key', 18)}</div><p class="sm muted">No connections yet</p></div>
          ${addBtn}</div>`
        : `<div class="connlist">${list.map(connRow).join('')}</div>${custom ? '' : `<div class="mt4 row gap2">${addBtn}</div>`}`}
    </div>
    <div class="card">
      <div class="row between wrap gap2 mb4"><div class="row gap3 wrap"><h2 class="ct">Available Models</h2>${prov.passthroughModels || custom ? `<span class="xs muted">${custom ? 'Any model id the endpoint serves works as ' : 'Any model id is passed through as '}<code>${esc(prefix)}/&lt;model&gt;</code></span>` : ''}</div>
        <div class="row gap2 wrap">
          ${canRefresh ? `<button class="btn sm secondary" data-act="refreshModels" data-provider="${esc(prov.id)}" data-node="${custom ? node.id : ''}" data-prefix="${esc(prefix)}" title="Pull the provider's latest model list and add the new models that answer">${ms('sync')}Refresh models</button>` : ''}
          ${customModels.length || (!custom && prov.models.length) ? `<button class="btn sm secondary" data-act="cleanUpModels" data-prefix="${esc(prefix)}" title="Test every model below and remove the ones that fail">${ms('cleaning_services')}Clean up</button>
          <button class="btn sm secondary" data-act="testModels" data-prefix="${esc(prefix)}" title="Send a short test request to every model below">${ms('science')}Test all</button>` : ''}
        </div></div>
      <div class="models">
        ${customModels.map((m) => modelRow(m, null, true)).join('')}
        ${custom ? '' : prov.models.map((m) => modelRow(m.id, m.name, false)).join('')}
        <button class="addmodel" data-act="addCustomModel" data-prefix="${esc(prefix)}">${ms('add', 14)}Add Model</button>
      </div>
      ${hiddenModels.length ? `<div class="row gap2 wrap mt4"><span class="xs muted" title="${esc(hiddenModels.join(', '))}">${hiddenModels.length} model${hiddenModels.length === 1 ? '' : 's'} removed by Clean up: left out of /v1/models and auto</span>
        <button class="btn sm ghost" data-act="restoreModels" data-prefix="${esc(prefix)}">${ms('undo')}Restore</button></div>` : ''}
    </div>
  </div>`;
}
document.addEventListener('change', async (e) => {
  const inp = e.target.closest('[data-sticky]'); if (!inp) return;
  const s = await get('/settings'); const ps = s.providerStrategies || {};
  ps[inp.dataset.sticky] = {...(ps[inp.dataset.sticky] || {}), stickyRoundRobinLimit: Math.max(1, Number(inp.value) || 1)};
  await patch('/settings', {providerStrategies: ps}); notify('success', 'Sticky limit saved');
});
actions.providerRR = async ({id}, b) => {
  const next = b.getAttribute('aria-checked') !== 'true';
  const s = await get('/settings'); const ps = s.providerStrategies || {};
  ps[id] = {...(ps[id] || {}), fallbackStrategy: next ? 'round-robin' : 'fill-first'};
  await patch('/settings', {providerStrategies: ps}); reload();
};
actions.moveConn = async ({id, dir}) => {
  const conns = await connectionList(); const c = conns.find((x) => x.id === id);
  const list = conns.filter((x) => x.provider === c.provider).sort((a, b) => (a.priority ?? 999) - (b.priority ?? 999));
  const i = list.indexOf(c), j = i + Number(dir); if (j < 0 || j >= list.length) return;
  [list[i], list[j]] = [list[j], list[i]];
  await Promise.all(list.map((x, k) => patch('/connections/' + x.id, {priority: k + 1})));
  invalidate(); reload();
};
// "Test all" (upstream provider test-models): one short request per model, a few at
// a time, with progress in a modal and next to each listed model. Resolves to the
// results and the modal, or null when the test itself could not run.
const spinner = (size) => ms('progress_activity', size).replace('class="ms', 'class="ms spin');
const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;
async function runModelTests(b, title, models, intro = '') {
  const slots = $$('.models .mtest').filter((s) => models.includes(s.dataset.model));
  const label = b.innerHTML;
  b.innerHTML = `${spinner(16)}Testing ${models.length}…`;
  slots.forEach((s) => (s.innerHTML = badge('testing…', 'info')));
  const m = openModal({title, size: 'lg', body: `<div id="tm-body" class="row gap2">${spinner(18)}<p class="sm muted">${intro}Testing ${plural(models.length, 'model')}, a few at a time. Slow models take up to 30s each.</p></div>`});
  let res;
  try { res = await post('/test-models', {models}); }
  catch (e) { slots.forEach((s) => (s.innerHTML = '')); const el = $('#tm-body'); if (el) el.innerHTML = `<p class="sm" style="color:#dc2626">${esc(e.message)}</p>`; return null; }
  finally { b.innerHTML = label; }
  for (const r of res.results) {
    const s = slots.find((x) => x.dataset.model === r.model); if (!s) continue;
    s.innerHTML = r.ok ? badge(`${r.latencyMs}ms`, 'success', true) : `<span title="${esc(r.error || '')}">${badge('failed', 'error', true)}</span>`;
  }
  return {res, m};
}
// Fills the test modal (if still open): summary badges, `extra`, one row per model.
function showTestReport({res, m}, extra = '') {
  const el = $('#tm-body'); if (!el || !m.form.isConnected) return;
  const failed = res.total - res.ok;
  const rows = [...res.results].sort((x, y) => Number(x.ok) - Number(y.ok)).map((r) => `<div class="row gap3 start" style="padding:8px 0;border-top:1px solid var(--border)">
      <span style="color:${r.ok ? '#16a34a' : '#dc2626'}">${ms(r.ok ? 'check_circle' : 'cancel', 18)}</span>
      <div class="grow" style="min-width:0"><code class="sm break">${esc(r.model)}</code>
        ${r.ok ? (r.note ? `<p class="xs muted">${esc(r.note)}</p>` : '') : `<p class="xs break" style="color:#dc2626">${r.status ? 'HTTP ' + esc(r.status) + ': ' : ''}${esc(r.error || 'failed')}</p>`}</div>
      <span class="xs muted shrink0">${esc(r.latencyMs)}ms</span></div>`).join('');
  el.outerHTML = `<div>
    <div class="row gap2 wrap mb4">${badge(`${res.ok} working`, 'success', true, 'md')}${failed ? badge(`${failed} failed`, 'error', true, 'md') : ''}<span class="xs muted">of ${res.total} models</span></div>
    ${extra}<div style="max-height:60vh;overflow:auto">${rows}</div></div>`;
}
// Models a clean up removes: every one that did not answer, except rate limits (the
// model works, the account is only busy right now).
const failedModels = (res) => res.results.filter((r) => !r.ok && r.status !== 429).map((r) => r.model);
const modelIds = (prefix, models) => models.map((m) => m.slice(prefix.length + 1));
const listedModels = () => $$('.models .mtest').map((s) => s.dataset.model);
const removeModels = (prefix, models) => post('/models/' + encodeURIComponent(prefix) + '/remove', {models: modelIds(prefix, models)});

actions.testModels = async ({prefix}, b) => {
  const models = listedModels();
  if (!models.length) return notify('warning', 'No models to test');
  const t = await runModelTests(b, 'Test all models', models); if (!t) return;
  const failed = failedModels(t.res);
  showTestReport(t, failed.length ? `<div class="mb4"><button type="button" class="btn sm danger" data-act="removeFailed" data-prefix="${esc(prefix)}" data-models="${esc(JSON.stringify(failed))}">${ms('delete', 16)}Remove ${plural(failed.length, 'failed model')}</button></div>` : '');
};
actions.removeFailed = async ({prefix, models}) => {
  const list = JSON.parse(models);
  await removeModels(prefix, list);
  $('#modal [data-close]')?.click();
  notify('success', `Removed ${plural(list.length, 'model')}`); invalidate(); reload();
};
// "Clean up": test every listed model and remove the ones that fail.
actions.cleanUpModels = async ({prefix}, b) => {
  const models = listedModels();
  if (!models.length) return notify('warning', 'No models to clean up');
  if (!(await confirmModal({title: 'Clean up models', message: `Test all ${plural(models.length, 'model')} and remove the ones that fail? Rate-limited models are kept. Removed catalogue models can be restored; failing models you added yourself are deleted.`, confirmText: 'Clean up'}))) return;
  await new Promise((r) => setTimeout(r)); // let the confirm dialog close before the next one opens
  const t = await runModelTests(b, 'Clean up models', models); if (!t) return;
  const failed = failedModels(t.res);
  if (failed.length) await removeModels(prefix, failed);
  showTestReport(t, `<p class="sm mb4">${failed.length ? `Removed ${plural(failed.length, 'model')} that failed.` : 'Every model answered: nothing to remove.'}</p>`);
  if (failed.length) { invalidate(); reload(); }
};
// "Refresh models": pull the provider's live model list, test the models this page
// doesn't list yet and add the ones that answer.
actions.refreshModels = async ({provider, node, prefix}, b) => {
  const MAX = 300; // /test-models limit
  const label = b.innerHTML;
  b.innerHTML = `${spinner(16)}Fetching…`;
  let live;
  try { live = (await post('/refresh-models', {provider, node: node || null})).models; }
  finally { b.innerHTML = label; }
  const listed = new Set(listedModels());
  const fresh = live.map((id) => `${prefix}/${id}`).filter((m) => !listed.has(m));
  if (!fresh.length) return notify('success', `Up to date: every model the provider lists (${live.length}) is already here`);
  const t = await runModelTests(b, 'Refresh models', fresh.slice(0, MAX), `The provider lists ${plural(fresh.length, 'model')} not on this page${fresh.length > MAX ? `; testing the first ${MAX}` : ''}. `);
  if (!t) return;
  const working = t.res.results.filter((r) => r.ok).map((r) => r.model);
  if (working.length) await post('/models/' + encodeURIComponent(prefix) + '/add', {models: modelIds(prefix, working)});
  showTestReport(t, `<p class="sm mb4">${working.length ? `Added ${plural(working.length, 'working model')} to the list.` : 'None of the new models answered: nothing added.'}</p>`);
  if (working.length) { invalidate(); reload(); }
};
actions.restoreModels = async ({prefix}) => {
  await del('/models/' + encodeURIComponent(prefix) + '/hidden');
  notify('success', 'Removed models restored'); invalidate(); reload();
};
actions.toggleConn = async ({id}, b) => { await patch('/connections/' + id, {isActive: b.getAttribute('aria-checked') !== 'true'}); invalidate(); reload(); };
actions.unlockConn = async ({id}) => { await post('/connections/' + id + '/unlock'); invalidate(); notify('success', 'Connection unlocked'); reload(); };
actions.delConn = async ({id, back}) => {
  const c = (await connectionList()).find((x) => x.id === id);
  if (!(await confirmModal({title: 'Delete Connection', message: `Delete connection "${c.name || c.keyMasked || id}"?`, confirmText: 'Delete'}))) return;
  await del('/connections/' + id); invalidate();
  if (back) location.hash = '#/providers'; else reload();
};
actions.addConn = ({provider, node}) => {
  const custom = isCustomId(provider), m = CUSTOM_META[provider];
  openModal({
    title: `Add ${custom ? m.name : (provCache || []).find((p) => p.id === provider)?.name || provider} API Key`,
    body: field('name', 'Name', {placeholder: 'Production Key'}) + field('apiKey', 'API Key', {type: 'password', placeholder: 'sk-...', required: true})
      + (custom ? field('baseUrl', 'Base URL', {value: m.defaultUrl, required: true, hint: m.hint}) + field('prefix', 'Prefix', {placeholder: m.prefixPh, required: true}) : '')
      + twoButtons('Add'),
    onSubmit: async (f) => {
      await post('/connections', {provider, apiKey: f.apiKey.value, name: f.name.value.trim() || null, baseUrl: custom ? f.baseUrl.value.trim() : null, prefix: custom ? f.prefix.value.trim() : null});
      invalidate(); notify('success', 'Connection added'); reload();
    },
  });
};
// OAuth sign-in (upstream OAuthModal): redirect flows paste the callback URL back,
// device flows show a code and poll until the account approves it.
actions.oauthLogin = async ({provider}) => {
  const name = (provCache || []).find((p) => p.id === provider)?.name || provider;
  const port = location.port || (location.protocol === 'https:' ? '443' : '80');
  const s = await post('/oauth/' + encodeURIComponent(provider) + '/start', {redirectUri: `http://localhost:${port}/callback`});
  const done = (c) => { invalidate(); notify('success', `${name}: signed in${c && (c.email || c.name) ? ' as ' + (c.email || c.name) : ''}`); reload(); };
  if (s.flow === 'device') return oauthDevice(provider, name, s, done);
  let channel = null;
  const finish = async (callback) => {
    const r = await post('/oauth/' + encodeURIComponent(provider) + '/exchange', {session: s.session, callback});
    done(r);
  };
  const m = openModal({
    title: 'Sign in with ' + name,
    body: `<ol class="sm muted" style="margin:0 0 12px 18px;line-height:1.7">
        <li>Open the sign-in page and log in.</li>
        <li>You end up on <code>${esc(s.redirectUri.replace(/\?.*/, ''))}</code>. That page may not load (it points at your own computer, not this router): that's fine.</li>
        <li>Copy the whole address from the browser's address bar and paste it below.</li></ol>
      <a class="btn primary full mb4" href="${esc(s.authUrl)}" target="_blank" rel="noopener">${ms('open_in_new')}Open sign-in page</a>`
      + field('callback', 'Callback URL or code', {type: 'textarea', rows: 3, placeholder: s.redirectUri + '?code=…&state=…', required: true})
      + twoButtons('Connect'),
    onSubmit: async (f) => { await finish(f.callback.value.trim()); },
    onClose: () => { if (channel) channel.close(); },
  });
  // If the redirect does reach this router (browser on the same machine), finish by itself.
  try {
    channel = new BroadcastChannel('zilong-router-oauth');
    channel.onmessage = (e) => { const t = $('textarea[name=callback]', m.form); if (t && !t.value) { t.value = e.data; m.form.requestSubmit(); } };
  } catch (_) {}
};
function oauthDevice(provider, name, s, done) {
  let stop = false, wait = Math.max(1, s.interval || 5) * 1000;
  const deadline = Date.now() + (s.expiresIn || 900) * 1000;
  const link = s.verificationUriComplete || s.verificationUri;
  const m = openModal({
    title: 'Sign in with ' + name,
    body: `<p class="sm muted mb4">Open the page below, sign in and enter this code to approve zilong-router.</p>
      <div class="row gap2 mb4"><input class="input mono grow" style="font-size:20px;text-align:center;letter-spacing:2px" value="${esc(s.userCode || '')}" readonly>
        <button type="button" class="tiny" data-act="copy" data-text="${esc(s.userCode || '')}" title="Copy">${ms('content_copy', 18)}</button></div>
      <a class="btn primary full mb4" href="${esc(link)}" target="_blank" rel="noopener">${ms('open_in_new')}Open ${esc(link.replace(/^https?:\/\//, '').replace(/[?#].*/, ''))}</a>
      <p class="sm muted row gap2" id="oauth-status">${ms('hourglass_top', 16)}Waiting for approval…</p>`,
    footer: '<button type="button" class="btn ghost full" data-close>Cancel</button>',
    onClose: () => { stop = true; },
  });
  const status = (t) => { const el = $('#oauth-status'); if (el) el.textContent = t; };
  const tick = async () => {
    if (stop) return;
    if (Date.now() > deadline) { status('The code expired. Close this and start again.'); return; }
    try {
      const r = await post('/oauth/' + encodeURIComponent(provider) + '/poll', {session: s.session});
      if (r.status === 'done') { stop = true; m.close(); done(r.connection); return; }
      if (r.status === 'slow_down') wait += 5000;
    } catch (err) { status(err.message); return; }
    setTimeout(tick, wait);
  };
  setTimeout(tick, wait);
}
actions.editConn = async ({id}) => {
  const c = (await connectionList()).find((x) => x.id === id); const custom = isCustomId(c.provider);
  openModal({
    title: 'Edit Connection',
    body: field('name', 'Name', {value: c.name || '', placeholder: 'Production Key'}) + field('priority', 'Priority', {type: 'number', value: c.priority ?? '', min: 1, hint: 'Lower runs first under fill-first.'})
      + field('apiKey', 'API Key', {type: 'password', placeholder: 'Enter new API key', hint: 'Leave blank to keep the current key.'})
      + (custom ? field('baseUrl', 'Base URL', {value: c.baseUrl || '', required: true}) + field('prefix', 'Prefix', {value: c.prefix || '', required: true}) : '')
      + twoButtons('Save'),
    onSubmit: async (f) => {
      const body = {name: f.name.value.trim(), priority: f.priority.value === '' ? null : Number(f.priority.value)};
      if (f.apiKey.value) body.apiKey = f.apiKey.value;
      if (custom) { body.baseUrl = f.baseUrl.value.trim(); body.prefix = f.prefix.value.trim(); }
      await patch('/connections/' + id, body); invalidate(); reload();
    },
  });
};
actions.addCustomModel = ({prefix}) => openModal({
  title: 'Add Model',
  body: field('modelId', 'Model ID', {placeholder: 'e.g. gpt-4, claude-3-opus', required: true, mono: true, hint: `Saved as <code>${esc(prefix)}/&lt;model&gt;</code> for copying; routing passes the id through unchanged.`}) + twoButtons('Add'),
  onSubmit: async (f) => { await post('/custom-models/' + encodeURIComponent(prefix), {model_id: f.modelId.value.trim()}); reload(); },
});
actions.delCustomModel = async ({prefix, model}) => { await del('/custom-models/' + encodeURIComponent(prefix) + '/' + encodeURIComponent(model)); reload(); };

// ----- Usage & Analytics (upstream UsageStats) -------------------------------------
const PERIODS = [['today', 'Today'], ['24h', '24h'], ['7d', '7D'], ['30d', '30D'], ['60d', '60D'], ['all', 'All']];
let usagePeriod = 'today', usageTab = 'overview', chartMode = 'tokens', tableView = 'model', valueMode = 'costs', sortBy = 'requests', sortOrder = 'desc';
const expanded = new Set(JSON.parse(localStorage.getItem('usage-stats:expanded') || '[]'));
const segmented = (opts, cur, act, size = '') => `<div class="seg ${size}">${opts.map(([v, l]) => `<button type="button" class="${v === cur ? 'on' : ''}" data-act="${act}" data-v="${v}">${l}</button>`).join('')}</div>`;
const brandSeg = (opts, cur, act) => `<div class="seg brand">${opts.map(([v, l]) => `<button type="button" class="${v === cur ? 'on' : ''}" data-act="${act}" data-v="${v}">${l}</button>`).join('')}</div>`;

pages.usage = async () => {
  const [stats, conns] = await Promise.all([get('/usage?period=' + usagePeriod), connectionList(), providerList()]);
  const t = stats.totals;
  const statCard = (l, v, cls, note) => `<div class="card stat"><span class="l">${l}</span><span class="v ${cls || ''}" title="${esc(v)}">${v}</span>${note ? `<span class="n">${note}</span>` : ''}</div>`;
  const active = [...new Set(conns.filter((c) => c.isActive).map((c) => c.provider))];
  const byProv = Object.fromEntries(stats.byProvider.map((r) => [r.provider, r]));
  const lastProv = stats.recentRequests[0]?.provider;

  const recent = `<div class="card sm recentwrap"><div class="hd">Recent Requests</div>
    ${!stats.recentRequests.length ? '<div class="grow row center muted sm" style="justify-content:center">No requests yet.</div>' : `<div class="scroll"><table class="recent"><thead><tr><th style="width:8px"></th><th>Model</th><th class="right">In / Out</th><th class="right">When</th></tr></thead><tbody>
      ${stats.recentRequests.map((r) => { const ok = !r.status || r.status === 'ok' || r.status === 'success'; return `<tr><td><span class="dot" style="background:${ok ? 'var(--success)' : 'var(--danger)'}"></span></td><td class="mono truncate" style="max-width:140px" title="${esc(r.model)}">${esc(r.model)}</td><td class="right" style="white-space:nowrap"><span class="primary">${fmt(r.promptTokens)}↑</span> <span class="success">${fmt(r.completionTokens)}↓</span></td><td class="right muted" style="white-space:nowrap">${relTime(r.timestamp)}</td></tr>`; }).join('')}
    </tbody></table></div>`}</div>`;
  const topology = `<div class="card sm"><div class="hd xs semibold muted" style="text-transform:uppercase;letter-spacing:.05em;padding:8px 4px;border-bottom:1px solid var(--border)">Providers</div>
    ${!active.length ? '<div class="loading sm">No active providers. Add one under Providers.</div>' : `<div class="col gap2 mt3">${active.map((p) => { const r = byProv[p]; const prov = (provCache || []).find((x) => x.id === p); return `<div class="row between" style="padding:8px 12px;border-radius:8px;background:var(--bg);border:1px solid ${p === lastProv ? 'rgba(229,106,74,.5)' : 'var(--border)'}">
        <div class="row gap3">${providerIcon({id: p, color: prov?.color, textIcon: prov?.alias?.slice(0, 2).toUpperCase(), icon: isCustomId(p) ? CUSTOM_META[p].icon : null}, 24)}<span class="sm medium">${esc(prov?.name || p)}</span>${p === lastProv ? badge('last used', 'primary') : ''}</div>
        <div class="xs muted">${r ? `${fmt(r.requests)} req · ${fmtTokens(r.promptTokens + r.completionTokens)} tok · ${fmtCost4(r.cost)}` : 'no traffic'}</div></div>`; }).join('')}</div>`}</div>`;

  // Time series bar chart
  const series = [...stats.chart].sort((a, b) => a.bucket < b.bucket ? -1 : 1);
  const cfg = {tokens: {k: (r) => r.promptTokens + r.completionTokens, color: '#6366f1', f: fmtTokens}, requests: {k: (r) => r.requests, color: '#14b8a6', f: fmt}, cost: {k: (r) => r.cost, color: '#f59e0b', f: fmtCost4}}[chartMode];
  const max = Math.max(1e-9, ...series.map(cfg.k));
  const chart = `<div class="card sm col gap3">
    ${brandSeg([['tokens', 'Tokens'], ['requests', 'Requests'], ['cost', 'Cost']], chartMode, 'chartMode')}
    ${!series.length ? '<div class="loading sm">No data for this period.</div>' : `<div class="chart" style="--chart:${cfg.color}">${series.map((r) => `<div class="bar" title="${esc(r.bucket)}: ${cfg.f(cfg.k(r))}"><i style="height:${Math.max(1, cfg.k(r) / max * 100)}%"></i></div>`).join('')}</div>
    <div class="axis"><span>${esc(series[0].bucket.replace('T', ' ') + (series[0].bucket.length === 13 ? ':00' : ''))}</span><span>${esc(series[series.length - 1].bucket.replace('T', ' ') + (series[series.length - 1].bucket.length === 13 ? ':00' : ''))}</span></div>`}</div>`;

  const hbars = (title, rows, key) => { const m = Math.max(1e-9, ...rows.map((r) => r.requests)); return `<div class="card sm col gap3"><h3 class="sm semibold">${title}</h3>
    ${!rows.length ? '<p class="muted sm">No data.</p>' : `<div class="hbars">${rows.slice(0, 8).map((r) => `<div class="hbar"><span class="truncate" title="${esc(r[key])}">${esc(r[key] || '—')}</span><div class="tr"><i style="width:${r.requests / m * 100}%;background:${key === 'model' ? '#6366f1' : 'var(--brand-500)'}"></i></div><span class="right muted">${fmt(r.requests)}</span></div>`).join('')}</div>`}</div>`; };

  return `<div class="page">
    <div class="row between wrap gap2">
      ${segmented([['overview', 'Overview'], ['details', 'Details']], usageTab, 'usageTab')}
      ${usageTab === 'overview' ? segmented(PERIODS, usagePeriod, 'usagePeriod', 'sm') : ''}
    </div>
    ${usageTab === 'details' ? detailsTab(stats) : `
    <div class="stats">
      ${statCard('Total Requests', fmt(t.totalRequests))}
      ${statCard('Total Input Tokens', fmt(t.totalPromptTokens), 'primary')}
      ${statCard('Cached Tokens', fmt(t.totalCachedTokens), 'info')}
      ${statCard('Output Tokens', fmt(t.totalCompletionTokens), 'success')}
      ${statCard('Est. Cost', '~' + fmtCost(t.totalCost), 'warning', 'Estimated, not actual billing')}
    </div>
    <div class="two wide">${topology}${recent}</div>
    ${chart}
    <div class="two">${hbars('Requests by Provider', [...stats.byProvider].sort((a, b) => b.requests - a.requests), 'provider')}${hbars('Top Models', [...stats.byModel].sort((a, b) => b.requests - a.requests), 'model')}</div>
    ${usageTable(stats)}`}
  </div>`;
};
function detailsTab(stats) {
  return `<div class="card p0"><table class="ut"><thead><tr><th>Time</th><th>Provider</th><th>Model</th><th>Endpoint</th><th>Status</th><th class="r">Input</th><th class="r">Output</th><th class="r">Latency</th><th class="r">Cost</th></tr></thead><tbody>
    ${!stats.recentRequests.length ? '<tr><td colspan="9" class="center muted">No requests recorded yet.</td></tr>' : stats.recentRequests.map((r) => `<tr><td class="muted" style="white-space:nowrap" title="${esc(r.timestamp)}">${relTime(r.timestamp)}</td><td>${badge(esc(r.provider))}</td><td class="mono xs">${esc(r.model)}</td><td class="mono xs">${esc(r.endpoint || '')}</td><td>${badge(esc(r.status || 'success'), !r.status || r.status === 'success' ? 'success' : 'error', true)}</td><td class="r">${fmt(r.promptTokens)}</td><td class="r">${fmt(r.completionTokens)}</td><td class="r muted">${r.meta && r.meta.latencyMs != null ? fmt(r.meta.latencyMs) + ' ms' : '—'}</td><td class="r warning">${fmtCost4(r.cost)}</td></tr>`).join('')}
  </tbody></table></div>`;
}
function usageTable(stats) {
  const VIEWS = {
    model: {rows: stats.byModel, group: (r) => r.model || 'Unknown Model', cols: [['model', 'Model'], ['provider', 'Provider']], cells: (r) => [esc(r.model), badge(esc(r.provider))]},
    account: {rows: stats.byAccount, group: (r) => r.accountName || `Account ${(r.connectionId || '').slice(0, 8)}...`, cols: [['accountName', 'Account'], ['model', 'Model'], ['provider', 'Provider']], cells: (r) => [esc(r.accountName || (r.connectionId || '').slice(0, 8)), esc(r.model), badge(esc(r.provider))]},
    apiKey: {rows: stats.byApiKey, group: (r) => r.keyName ? `${r.keyName} (${r.apiKey})` : r.apiKey || 'Local (no key)', cols: [['keyName', 'API Key Name'], ['model', 'Model'], ['provider', 'Provider']], cells: (r) => [`<span class="mono xs">${esc(r.apiKey || 'local')}</span>`, esc(r.model), badge(esc(r.provider))]},
    endpoint: {rows: stats.byEndpoint, group: (r) => r.endpoint || 'Unknown Endpoint', cols: [['endpoint', 'Endpoint'], ['model', 'Model'], ['provider', 'Provider']], cells: (r) => [`<span class="mono">${esc(r.endpoint)}</span>`, esc(r.model), badge(esc(r.provider))]},
  };
  const v = VIEWS[tableView];
  const valCols = valueMode === 'tokens' ? [['promptTokens', 'Input Tokens'], ['cachedTokens', 'Cached'], ['completionTokens', 'Output Tokens'], ['totalTokens', 'Total Tokens']] : [['inputCost', 'Input Cost'], ['cachedCost', 'Cached Cost'], ['outputCost', 'Output Cost'], ['cost', 'Total Cost']];
  const enrich = (r) => { const tot = r.promptTokens + r.completionTokens, per = tot ? r.cost / tot : 0; return {...r, totalTokens: tot, inputCost: Math.max(0, r.promptTokens - r.cachedTokens) * per, cachedCost: r.cachedTokens * per, outputCost: r.completionTokens * per}; };
  const groups = {};
  for (const r of v.rows.map(enrich)) { const k = v.group(r); (groups[k] = groups[k] || {key: k, items: [], requests: 0, promptTokens: 0, completionTokens: 0, cachedTokens: 0, totalTokens: 0, cost: 0, inputCost: 0, cachedCost: 0, outputCost: 0, lastUsed: null}); const g = groups[k]; g.items.push(r); for (const f of ['requests', 'promptTokens', 'completionTokens', 'cachedTokens', 'totalTokens', 'cost', 'inputCost', 'cachedCost', 'outputCost']) g[f] += r[f] || 0; if (r.lastUsed > (g.lastUsed || '')) g.lastUsed = r.lastUsed; }
  const cmp = (a, b) => { let x = a[sortBy], y = b[sortBy]; if (typeof x === 'string') x = x.toLowerCase(); if (typeof y === 'string') y = y.toLowerCase(); return (x < y ? -1 : x > y ? 1 : 0) * (sortOrder === 'asc' ? 1 : -1); };
  const sorted = Object.values(groups).sort((a, b) => cmp(a.items[0], b.items[0]) || cmp(a, b));
  const sortIcon = (f) => `<span class="sort ${sortBy === f ? 'on' : ''}">${sortBy === f ? (sortOrder === 'asc' ? '↑' : '↓') : '↕'}</span>`;
  const vals = (r) => (valueMode === 'tokens' ? [fmt(r.promptTokens), r.cachedTokens ? fmt(r.cachedTokens) : '—', fmt(r.completionTokens), `<b>${fmt(r.totalTokens)}</b>`] : [fmtCost(r.inputCost), r.cachedCost ? fmtCost(r.cachedCost) : '—', fmtCost(r.outputCost), `<b class="warning">${fmtCost(r.cost)}</b>`]).map((x) => `<td class="r">${x}</td>`).join('');
  const colspan = v.cols.length + 2 + valCols.length;
  return `<div class="col gap3">
    <div class="row between wrap gap2">
      <div class="selectwrap" style="width:auto"><select class="table" id="table-view">${[['model', 'Usage by Model'], ['account', 'Usage by Account'], ['apiKey', 'Usage by API Key'], ['endpoint', 'Usage by Endpoint']].map(([k, l]) => `<option value="${k}" ${tableView === k ? 'selected' : ''}>${l}</option>`).join('')}</select></div>
      ${brandSeg([['costs', 'Costs'], ['tokens', 'Tokens']], valueMode, 'valueMode')}
    </div>
    <div class="card p0"><div style="overflow-x:auto"><table class="ut"><thead><tr>
      ${v.cols.map(([f, l]) => `<th data-act="sort" data-f="${f}">${l} ${sortIcon(f)}</th>`).join('')}<th class="r" data-act="sort" data-f="requests">Requests ${sortIcon('requests')}</th><th class="r" data-act="sort" data-f="lastUsed">Last Used ${sortIcon('lastUsed')}</th>
      ${valCols.map(([f, l]) => `<th class="r" data-act="sort" data-f="${f}">${l} ${sortIcon(f)}</th>`).join('')}</tr></thead><tbody>
      ${!sorted.length ? `<tr><td colspan="${colspan}" class="center muted" style="padding:32px">No usage recorded yet.</td></tr>` : sorted.map((g) => `
        <tr class="grp" data-act="toggleGroup" data-k="${esc(g.key)}"><td><div class="row gap2">${ms('chevron_right', 18)}<span class="medium" style="${expanded.has(g.key) ? '' : ''}">${esc(g.key)}</span><span class="xs muted">(${g.items.length})</span></div></td>${v.cols.slice(1).map(() => '<td class="muted">—</td>').join('')}<td class="r">${fmt(g.requests)}</td><td class="r muted" style="white-space:nowrap">${relTime(g.lastUsed)}</td>${vals(g)}</tr>
        ${expanded.has(g.key) ? g.items.map((r) => `<tr class="det">${v.cells(r).map((c) => `<td>${c}</td>`).join('')}<td class="r">${fmt(r.requests)}</td><td class="r muted" style="white-space:nowrap">${relTime(r.lastUsed)}</td>${vals(r)}</tr>`).join('') : ''}`).join('')}
    </tbody></table></div></div>
  </div>`;
}
document.addEventListener('change', (e) => { if (e.target.id === 'table-view') { tableView = e.target.value; reload(); } });
actions.usageTab = ({v}) => { usageTab = v; reload(); };
actions.usagePeriod = ({v}) => { usagePeriod = v; reload(); };
actions.chartMode = ({v}) => { chartMode = v; reload(); };
actions.valueMode = ({v}) => { valueMode = v; reload(); };
actions.sort = ({f}) => { if (sortBy === f) sortOrder = sortOrder === 'asc' ? 'desc' : 'asc'; else { sortBy = f; sortOrder = 'asc'; } reload(); };
actions.toggleGroup = ({k}) => { expanded.has(k) ? expanded.delete(k) : expanded.add(k); localStorage.setItem('usage-stats:expanded', JSON.stringify([...expanded])); reload(); };

// ----- Console Log (upstream console-log/ConsoleLogClient.js) -----------------------
// Polled every 1.5s with the sequence number of the newest line seen, instead of
// upstream's SSE stream, so the board holds no connection open.
const CONSOLE_MAX = 200;
let consoleLines = [], consoleLast = 0;
const logLine = (l) => `<div class="l-${(l.match(/\b(ERROR|WARN|INFO|DEBUG|TRACE)\b/) || [])[1] || 'LOG'}">${esc(l)}</div>`;
const consoleHtml = () => consoleLines.length ? consoleLines.map(logLine).join('') : '<span class="muted">No console logs yet.</span>';
async function pollConsole() {
  const d = await get('/console-logs?after=' + consoleLast);
  const box = $('#console'); if (!box) return;
  if (!d.lines.length && !d.reset) { consoleLast = d.last; return; }
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
  consoleLines = (d.reset ? d.lines : consoleLines.concat(d.lines)).slice(-CONSOLE_MAX);
  consoleLast = d.last;
  box.innerHTML = consoleHtml();
  if (atBottom) box.scrollTop = box.scrollHeight;
}
pages['console-log'] = async () => {
  const d = await get('/console-logs');
  consoleLines = d.lines.slice(-CONSOLE_MAX); consoleLast = d.last;
  setTimeout(() => { const box = $('#console'); if (box) box.scrollTop = box.scrollHeight; });
  every(1500, () => pollConsole().catch(() => {}));
  return `<div class="card flush">
    <div class="row between gap2" style="padding:12px 16px 8px"><p class="xs muted">Last ${CONSOLE_MAX} lines of the router log</p>
      <button class="btn sm outline" data-act="clearConsole">${ms('delete')}Clear</button></div>
    <div class="console" id="console">${consoleHtml()}</div>
  </div>`;
};
actions.clearConsole = async () => {
  await del('/console-logs');
  consoleLines = []; const box = $('#console'); if (box) box.innerHTML = consoleHtml();
};

// ----- Settings (upstream profile/page.js) ------------------------------------------
const DB_MODE = {local: 'Local SQLite file', mirror: 'Local SQLite file, mirrored to Turso', turso: 'Turso (working copy kept in memory on this device)'};
function dbCard(d) {
  const t = d.turso, h = dbHealth(d);
  const inset = (title, body) => `<div class="inset"><p class="medium">${title}</p>${body}</div>`;
  const parts = [
    `<div class="inset row between wrap gap2"><div><p class="medium">Storage</p><p class="sm muted">${esc(DB_MODE[d.mode] || d.mode)}</p></div><span class="badge ${h.cls}">${esc(h.badge)}</span></div>`,
    inset('Database Location', `<p class="sm muted mono break">${esc(d.path || 'In memory: nothing is written to the SD card, Turso holds the durable copy')}</p>`),
    inset('Size', `<p class="sm muted">${d.sizeBytes != null ? fmtBytes(d.sizeBytes) : '?'} · usage history kept ${d.usageKeepDays} days${t && d.mode === 'mirror' ? ' here (Turso keeps all of it)' : ''}</p>`),
  ];
  if (t) {
    parts.push(inset('Turso', `<p class="sm muted mono break">${esc(t.host)}</p>
      <p class="sm muted mt1">${t.lastOkAt ? `Last successful sync ${relTime(t.lastOkAt).toLowerCase()}${t.lastRoundMs != null && t.online ? ` (took ${t.lastRoundMs}ms)` : ''}` : 'Not synced yet'} · pushes every ${t.syncSecs}s</p>
      ${t.online === false && !t.ready ? '<p class="sm warning mt1">Not connected since start-up: changes are held back until Turso answers.</p>' : ''}
      ${t.online === false && t.lastError ? `<p class="sm danger mt1 break">${esc(t.lastError)}</p>` : ''}
      ${t.ping ? `<p class="sm mt1 ${t.ping.ok ? 'success' : 'danger'}">${t.ping.ok ? `Reachable now · ${t.ping.latencyMs}ms round trip` : `Not reachable now: ${esc(t.ping.error)}`}</p>` : ''}`));
  }
  return parts.join('');
}
actions.pingDb = async () => {
  const d = await get('/database?ping=1');
  const card = $('#db-card'); if (card) card.innerHTML = dbCard(d);
  paintDbStat(d);
};
// Import from 9router: pick a file, preview what it holds (dry run), then import.
async function importUpload(file, q) {
  const res = await fetch('/api/admin/import?' + new URLSearchParams(q), {method: 'POST', body: file, credentials: 'same-origin'});
  if (res.status === 401) { showLogin(); throw new Error('login required'); }
  let data = null; try { data = await res.json(); } catch (_) {}
  if (!res.ok) throw new Error((data && data.error) || res.statusText);
  return data;
}
actions.import9router = () => {
  $('#menu').classList.add('hidden');
  const inp = document.createElement('input');
  inp.type = 'file'; inp.accept = '.json,.sqlite,.db,application/json';
  inp.onchange = () => { if (inp.files[0]) previewImport(inp.files[0]).catch(fail); };
  inp.click();
};
async function previewImport(file) {
  const m = openModal({title: 'Import from 9router', body: `<div class="loading" style="padding:24px"><span class="ms i32 spin">progress_activity</span></div><p class="center sm muted">Reading ${esc(file.name)}…</p>`});
  let p;
  try { p = await importUpload(file, {dryRun: true, providers: true, keys: true, settings: true}); }
  catch (e) { m.close(); throw e; }
  m.close();
  const provs = Object.entries(p.byProvider || {}), skipped = Object.entries(p.skipped || {});
  const opt = (name, label, desc, on, n) => `<label class="inset row gap3 start" style="cursor:pointer${n ? '' : ';opacity:.5'}"><input type="checkbox" name="${name}" ${on && n ? 'checked' : ''} ${n ? '' : 'disabled'} style="margin-top:3px"><div class="grow"><p class="medium">${label}</p><p class="sm muted">${desc}</p></div></label>`;
  const nothing = !p.connections && !p.keys && !p.aliases && !p.customModels && !p.settings;
  openModal({
    title: 'Import from 9router',
    body: `<p class="sm muted mb4">${esc(file.name)} (${esc(p.format)}). Entries are added or updated by id; nothing already here is deleted.</p>
      <div class="col gap3">
        ${opt('providers', `Providers: ${p.connections} account${p.connections === 1 ? '' : 's'}`, `${provs.length ? provs.map(([k, n]) => `${esc(k)} ×${n}`).join(', ') : 'none'}${p.customModels || p.aliases ? ` · ${p.customModels} custom model${p.customModels === 1 ? '' : 's'}, ${p.aliases} alias${p.aliases === 1 ? '' : 'es'}` : ''}`, true, p.connections + p.customModels + p.aliases)}
        ${opt('keys', `API keys: ${p.keys}`, 'Clients keep working with their existing keys.', true, p.keys)}
        ${opt('settings', `Settings: ${p.settings} value${p.settings === 1 ? '' : 's'}`, 'Routing strategy, sticky limit, per-provider overrides and the like. The dashboard password and Require login stay as they are here.', false, p.settings)}
        ${skipped.length ? `<div class="warn-box">${ms('info', 16)}<p class="grow sm">Not imported, zilong-router does not support these providers: ${skipped.map(([k, n]) => `${esc(k)} ×${n}`).join(', ')}.</p></div>` : ''}
      </div>`,
    footer: nothing ? '<button type="button" class="btn ghost full" data-close>Nothing to import</button>' : twoButtons('Import'),
    onSubmit: async (f) => {
      const q = {providers: f.providers.checked, keys: f.keys.checked, settings: f.settings.checked};
      if (!q.providers && !q.keys && !q.settings) throw new Error('Pick at least one thing to import');
      const r = await importUpload(file, q);
      const done = [q.providers && `${r.connections} accounts`, q.keys && `${r.keys} API keys`, q.settings && r.settings && 'settings'].filter(Boolean);
      notify('success', `Imported ${done.join(', ')}`, 'Import from 9router');
      reload();
    },
  });
}

pages.profile = async () => {
  const [s, db] = await Promise.all([get('/settings'), get('/database')]); settingsCache = s;
  paintDbStat(db);
  const rr = s.fallbackStrategy === 'round-robin';
  setTimeout(() => {
    const f = $('#pw-form'); if (!f) return;
    f.onsubmit = async (ev) => {
      ev.preventDefault();
      const st = $('#pw-status'); st.textContent = ''; st.className = 'sm';
      if (f.new.value !== f.confirm.value) { st.textContent = 'New passwords do not match'; st.classList.add('danger'); return; }
      try { await post('/password', {current: f.current.value, new: f.new.value}); f.reset(); st.textContent = 'Password updated'; st.classList.add('success'); settingsCache.hasPassword = true; }
      catch (e) { st.textContent = e.message; st.classList.add('danger'); }
    };
    const sticky = $('#sticky'); if (sticky) sticky.onchange = async () => { await patch('/settings', {stickyRoundRobinLimit: Math.min(10, Math.max(1, Number(sticky.value) || 3))}); notify('success', 'Sticky limit saved'); };
  });
  const row = (title, desc, control, cls = '') => `<div class="row between start gap4 ${cls}"><div class="grow"><p class="medium">${title}</p><p class="sm muted">${desc}</p></div>${control}</div>`;
  return `<div class="narrow"><div class="page">
    <div class="card">
      <div class="row between wrap gap4 mb4">
        <div class="row gap4"><div class="icon-tile tint-green" style="width:48px;height:48px">${ms('computer', 24)}</div><div><h2 class="xl semibold">Local Mode</h2><p class="sm muted">Running on your machine</p></div></div>
        <div class="theme-seg">${['light', 'dark', 'system'].map((o) => `<button type="button" class="${theme.get() === o ? 'on' : ''}" data-act="setTheme" data-v="${o}">${ms(o === 'light' ? 'light_mode' : o === 'dark' ? 'dark_mode' : 'contrast', 18)}<span>${o}</span></button>`).join('')}</div>
      </div>
      <div class="col gap3 pt4 bt">
        <div class="inset row between wrap gap2"><div><p class="medium">Provider registry</p><p class="sm muted">9router ${esc(session.upstream?.upstreamVersion || '?')} · ${esc((session.upstream?.upstreamCommit || '').slice(0, 8))} · synced ${esc((session.upstream?.generatedAt || '').slice(0, 10))}</p></div></div>
      </div>
    </div>

    <div class="card">
      <div class="row between wrap gap3 mb4"><div class="row gap3"><div class="icon-tile p tint-green">${ms('database', 20)}</div><h3 class="ct">Database</h3></div>${db.turso ? `<button class="btn sm outline" data-act="pingDb" title="Run a query on Turso now">${ms('network_check')}Check connection</button>` : ''}</div>
      <div class="col gap3" id="db-card">${dbCard(db)}</div>
    </div>

    <div class="card">
      <div class="row between wrap gap3 mb4"><div class="row gap3"><div class="icon-tile p tint-orange">${ms('move_to_inbox', 20)}</div><h3 class="ct">Import from 9router</h3></div><button class="btn sm secondary" data-act="import9router">${ms('upload_file')}Choose file</button></div>
      <p class="sm muted">Bring provider accounts, custom endpoints and models, model aliases, API keys and optionally settings over from an old 9router install. You see what the file holds before anything is imported. Any of these works:</p>
      <ul class="sm muted mt2" style="padding-left:20px;list-style:disc">
        <li>the backup from 9router's <b>Settings → Download Backup</b> (<code>9router-backup-….json</code>)</li>
        <li><code>~/.9router/db/data.sqlite</code> (stop 9router first so its last changes are in the file)</li>
        <li><code>~/.9router/db.json</code> from older versions</li>
      </ul>
    </div>

    <div class="card">
      <div class="row gap3 mb4"><div class="icon-tile p tint-primary">${ms('shield', 20)}</div><h3 class="ct">Security</h3></div>
      <div class="col gap4">
        ${row('Require login', 'When ON, dashboard requires password. When OFF, access without login.', toggle(s.requireLogin, 'data-act="setSetting" data-key="requireLogin"'))}
        ${row('Require API key', 'When ON, even local requests to /v1 must send a key.', toggle(s.requireApiKey, 'data-act="setSetting" data-key="requireApiKey"'))}
        <form id="pw-form" class="col gap4 pt4 bt-soft">
          ${s.hasPassword ? '' : `<div class="warn-box">${ms('warning', 16)}<p class="grow">The default password <code>123456</code> is in use. Set your own before exposing the port.</p></div>`}
          <div class="field"><label class="sm medium">Current Password</label><input class="input" type="password" name="current" placeholder="Enter current password" autocomplete="current-password" required></div>
          <div class="two" style="gap:16px">
            <div class="field"><label class="sm medium">New Password</label><input class="input" type="password" name="new" placeholder="Enter new password" autocomplete="new-password" minlength="4" required></div>
            <div class="field"><label class="sm medium">Confirm New Password</label><input class="input" type="password" name="confirm" placeholder="Confirm new password" autocomplete="new-password" required></div>
          </div>
          <p id="pw-status" class="sm"></p>
          <div><button type="submit" class="btn primary">${s.hasPassword ? 'Update Password' : 'Set Password'}</button></div>
        </form>
      </div>
    </div>

    <div class="card">
      <div class="row gap3 mb4"><div class="icon-tile p tint-blue">${ms('route', 20)}</div><h3 class="ct">Routing Strategy</h3></div>
      <div class="col gap4">
        ${row('Round Robin', 'Cycle through accounts, and the models of auto, to distribute load', toggle(rr, 'data-act="setStrategy"'))}
        ${rr ? row('Sticky Limit', 'Calls per account (or auto model) before switching', `<input id="sticky" class="input center w20 shrink0" type="number" min="1" max="10" value="${esc(s.stickyRoundRobinLimit || 3)}">`, 'pt4 bt-soft') : ''}
        <p class="xs muted pt4 bt-soft" style="font-style:italic">${rr ? `Currently distributing requests across all available accounts with ${s.stickyRoundRobinLimit || 3} calls per account; auto rotates its models the same way.` : 'Currently using accounts in priority order (Fill First); auto tries its models in order and falls back on failure.'} Per-provider overrides live on each provider page.</p>
      </div>
    </div>

    <div class="row gap2 wrap">
      <button class="btn outline full" data-act="logout" style="flex:1">${ms('logout')}Logout</button>
    </div>
    <div class="center xs muted" style="padding:16px 0"><p>zilong-router v${esc(session.version || '')}</p><p class="mt1">${db.mode === 'local' ? 'Local Mode - All data stored on your machine' : 'Data stored on your machine and on Turso'}</p></div>
  </div></div>`;
};
actions.setTheme = ({v}) => { theme.set(v); $$('.theme-seg button').forEach((b) => b.classList.toggle('on', b.dataset.v === v)); };
actions.setStrategy = async (_, b) => {
  const next = b.getAttribute('aria-checked') !== 'true';
  await patch('/settings', {fallbackStrategy: next ? 'round-robin' : 'fill-first'}); reload();
};

// ----- boot --------------------------------------------------------------------------
(async () => {
  theme.set(theme.get());
  try {
    session = await get('/session');
    $('#version').textContent = 'v' + session.version;
    if (session.authenticated) { showApp(); route(); } else showLogin();
  } catch (e) { showLogin(); }
})();
})();
