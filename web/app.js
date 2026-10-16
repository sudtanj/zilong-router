/* mini-router dashboard. A dependency-free port of 9router's Next.js dashboard:
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
  get: () => localStorage.getItem('mr-theme') || 'system',
  isDark: () => document.documentElement.classList.contains('dark'),
  set(t) {
    localStorage.setItem('mr-theme', t);
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
function showApp() { $('#login').classList.add('hidden'); $('#app').classList.remove('hidden'); }
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
  const rows = a.candidates.slice(0, 12).map((c, i) => `<div class="row between" style="padding:4px 0">
      <span class="sm"><span class="muted">${i + 1}.</span> <code>${esc(c.model)}</code> <span class="xs muted">${esc(c.providerName)}</span></span>
      ${!c.supported ? '<span class="badge sm warning">format not supported</span>'
        : c.ready ? '<span class="badge sm success"><span class="dot"></span>ready</span>'
        : '<span class="badge sm error"><span class="dot"></span>cooling down</span>'}</div>`).join('');
  return `<div class="card" id="auto-model">
    <div class="row between mb4">
      <h2 class="ct">${ms('auto_awesome')}Auto model</h2>
      <button class="btn ghost" data-act="editAuto">${ms('tune')}Configure</button>
    </div>
    <p class="sm muted mb4">Use model <code>auto</code> (like OpenRouter's <code>openrouter/auto</code>) and mini-router sends the request to the first model that answers, skipping accounts that are cooling down. The model that answered is returned in the <code>x-mini-router-model</code> response header.</p>
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
    <p class="sm medium mt4 mb2">Order right now <span class="xs muted">(${a.source === 'autoModels' ? 'your list' : 'every model of providers with an active account'}, at most ${a.maxAttempts} tried per request)</span></p>
    ${a.candidates.length ? `<div class="col">${rows}</div>${a.candidates.length > 12 ? `<p class="xs muted mt1">…and ${a.candidates.length - 12} more</p>` : ''}`
      : `<div class="warn-box">${ms('warning', 16)}<p class="grow">No candidates: add a provider account first, or list models under Configure.</p><a class="linkbtn" href="#/providers">Providers</a></div>`}
  </div>`;
};
actions.editAuto = async () => {
  const a = await get('/auto');
  openModal({
    title: 'Auto model',
    body: field('models', 'Models to try, in order (one per line)', {type: 'textarea', rows: 6, value: a.autoModels.join('\n'),
        placeholder: 'groq/llama-3.3-70b-versatile\nds/deepseek-chat', hint: 'Leave empty to use every model of every provider with an active account.'})
      + field('max', 'Max models tried per request', {type: 'number', min: 1, value: a.maxAttempts})
      + twoButtons('Save'),
    onSubmit: async (f) => {
      const models = f.models.value.split('\n').map((s) => s.trim()).filter(Boolean);
      const max = Math.max(1, parseInt(f.max.value, 10) || 5);
      await patch('/settings', {autoModels: models, autoMaxAttempts: max});
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
      <div class="grow" style="min-width:0">
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
      <p class="xs muted mt2">RTK only; counters reset when the router restarts. Caveman and Ponytail savings show up as shorter answers on the Usage page.</p>
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
  const provs = await providerList();
  const models = provs.flatMap((p) => p.models.map((m) => p.alias + '/' + m.id));
  openModal({
    title: `Allowed for "${k.name || ''}"`,
    body: `<div class="field"><label>Model</label><input class="input mono" name="model" list="model-list" placeholder="provider/model" required autocomplete="off"><datalist id="model-list">${models.map((m) => `<option value="${esc(m)}">`).join('')}</datalist><p class="hint">Pick from the list or type any <code>provider/model</code> or alias.</p></div>` + twoButtons('Add'),
    onSubmit: async (f) => {
      const v = f.model.value.trim(); if (!v) return false;
      if (!k.accessAllow.some((m) => m.toLowerCase() === v.toLowerCase())) await patch('/keys/' + id, {accessRestricted: true, accessAllow: [...k.accessAllow, v]});
      reload();
    },
  });
};

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
    const free = regular.filter((x) => x.p.noAuth || /free/.test(x.p.category || ''));
    const apikey = regular.filter((x) => !free.includes(x)).sort((a, b) => (a.s.total > 0 ? 0 : 1) - (b.s.total > 0 ? 0 : 1) || a.p.name.localeCompare(b.p.name));
    const searching = !!provSearch || provFilter !== 'all';
    const visible = searching || showAllApikey ? apikey : apikey.slice(0, 20);
    const section = (title, items, right) => `<div class="col gap4"><div class="row between wrap gap3"><h2 class="sect-title">${title}</h2>${right || ''}</div>${items}</div>`;
    return `<div class="row end">
        <select class="small" id="prov-filter" aria-label="Filter providers by connection status">${[['all', 'All'], ['active', 'Active'], ['inactive', 'Inactive'], ['none', 'No connection']].map(([v, l]) => `<option value="${v}" ${provFilter === v ? 'selected' : ''}>${l}</option>`).join('')}</select></div>
      ${!custom.length && !regular.length ? `<div class="dashed tall">${ms('search_off', 32)}<p class="sm muted">No providers match your search or filters</p></div>` : ''}
      ${section('Custom Providers (OpenAI/Anthropic Compatible)',
        custom.length ? `<div class="pgrid">${custom.map((x) => x.card).join('')}</div>` : `<div class="dashed">${ms('extension', 18)}<span>No custom providers — use buttons above to add OpenAI/Anthropic compatible endpoints</span></div>`,
        `<div class="row gap2 wrap"><button class="btn sm primary" data-act="addCompatible" data-variant="anthropic-compatible">${ms('add')}Add Anthropic Compatible</button><button class="btn sm white" data-act="addCompatible" data-variant="openai-compatible">${ms('add')}Add OpenAI Compatible</button></div>`)}
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
  const customModels = await get('/custom-models/' + encodeURIComponent(prefix));
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
          <p class="name truncate">${esc(c.name || c.email || 'API Key')}</p>
          ${c.keyMasked ? `<p class="xs muted truncate mono">${esc(c.keyMasked)}${custom ? ' · ' + esc(c.baseUrl || '') : ''}</p>` : ''}
          <div class="tags">
            ${badge(!c.isActive ? 'disabled' : st, variant, true)}${badge('API Key')}
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
      <div class="ops"><button class="tiny" data-act="copy" data-text="${esc(prefix)}/${esc(id)}" title="Copy">${ms('content_copy', 14)}</button>
      ${isCustom ? `<button class="tiny danger" data-act="delCustomModel" data-prefix="${esc(prefix)}" data-model="${esc(id)}" title="Remove custom model">${ms('close', 14)}</button>` : ''}</div>
    </div></div>`;
  const addBtn = `<button class="btn sm primary" data-act="addConn" data-provider="${esc(prov.id)}" data-node="${custom ? node.id : ''}">${ms(custom ? 'key' : 'add')}${custom ? 'Add API Key' : 'Add Connection'}</button>`;

  return `<div class="page">
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
      <div class="row between wrap gap2 mb4"><h2 class="ct">Available Models</h2>${prov.passthroughModels || custom ? `<span class="xs muted">${custom ? 'Any model id the endpoint serves works as ' : 'Any model id is passed through as '}<code>${esc(prefix)}/&lt;model&gt;</code></span>` : ''}</div>
      <div class="models">
        ${customModels.map((m) => modelRow(m, null, true)).join('')}
        ${custom ? '' : prov.models.map((m) => modelRow(m.id, m.name, false)).join('')}
        <button class="addmodel" data-act="addCustomModel" data-prefix="${esc(prefix)}">${ms('add', 14)}Add Model</button>
      </div>
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

// ----- Settings (upstream profile/page.js) ------------------------------------------
pages.profile = async () => {
  const s = await get('/settings'); settingsCache = s;
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
        <div class="inset row between wrap gap2"><div><p class="medium">Database Location</p><p class="sm muted mono break">${esc(session.dbPath || '')}</p></div></div>
        <div class="inset row between wrap gap2"><div><p class="medium">Provider registry</p><p class="sm muted">9router ${esc(session.upstream?.upstreamVersion || '?')} · ${esc((session.upstream?.upstreamCommit || '').slice(0, 8))} · synced ${esc((session.upstream?.generatedAt || '').slice(0, 10))}</p></div></div>
      </div>
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
        ${row('Round Robin', 'Cycle through accounts to distribute load', toggle(rr, 'data-act="setStrategy"'))}
        ${rr ? row('Sticky Limit', 'Calls per account before switching', `<input id="sticky" class="input center w20 shrink0" type="number" min="1" max="10" value="${esc(s.stickyRoundRobinLimit || 3)}">`, 'pt4 bt-soft') : ''}
        <p class="xs muted pt4 bt-soft" style="font-style:italic">${rr ? `Currently distributing requests across all available accounts with ${s.stickyRoundRobinLimit || 3} calls per account.` : 'Currently using accounts in priority order (Fill First).'} Per-provider overrides live on each provider page.</p>
      </div>
    </div>

    <div class="row gap2 wrap">
      <button class="btn outline full" data-act="logout" style="flex:1">${ms('logout')}Logout</button>
    </div>
    <div class="center xs muted" style="padding:16px 0"><p>mini-router v${esc(session.version || '')}</p><p class="mt1">Local Mode - All data stored on your machine</p></div>
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
