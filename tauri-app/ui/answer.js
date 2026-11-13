// Floating answer window. Talks to the Rust side through window.__TAURI__.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const answerEl = $('answer');
const statusEl = $('status');
const dotEl = $('dot');
const metaEl = $('meta');
const providerEl = $('provider');

let settings = null;
let currentId = 0;
let text = '';
let renderQueued = false;
let hideTimer = null;
let hovered = false;

// The model output is untrusted: render raw HTML in it as text.
const escapeHtml = (s) =>
  s.replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
marked.use({ gfm: true, breaks: false, renderer: { html: (token) => escapeHtml(token.text || '') } });

const kbd = (accel) =>
  accel ? accel.split('+').map((k) => `<kbd>${escapeHtml(k)}</kbd>`).join('+') : '<em>no hotkey</em>';

function showHint(first) {
  if (!settings) return;
  answerEl.innerHTML = `<div class="hint">
    <p>${first || 'Press'} ${kbd(settings.hotkey_ask)} to analyze the screen.</p>
    <p>This window is invisible to screen sharing and recording.</p>
    <p>${kbd(settings.hotkey_toggle)} or <kbd>Esc</kbd> hides it. ${kbd(settings.hotkey_stealth)} toggles stealth.</p>
    <p>Change hotkeys, look and AI in settings (&#9881;).</p>
  </div>`;
}

function setStatus(msg, state) {
  statusEl.textContent = msg;
  dotEl.className = 'dot ' + state;
}

function nearBottom() {
  return answerEl.scrollHeight - answerEl.scrollTop - answerEl.clientHeight < 40;
}

function render() {
  renderQueued = false;
  const stick = nearBottom();
  answerEl.innerHTML = marked.parse(text);
  for (const pre of answerEl.querySelectorAll('pre')) {
    const btn = document.createElement('button');
    btn.className = 'copy';
    btn.textContent = 'Copy';
    btn.onclick = async () => {
      await navigator.clipboard.writeText(pre.querySelector('code')?.innerText ?? pre.innerText);
      btn.textContent = 'Copied';
      setTimeout(() => (btn.textContent = 'Copy'), 1200);
    };
    pre.appendChild(btn);
  }
  if (stick) answerEl.scrollTop = answerEl.scrollHeight;
}

function queueRender() {
  if (!renderQueued) {
    renderQueued = true;
    requestAnimationFrame(render);
  }
}

// ------------------------------------------------------------ auto-hide
function clearAutoHide() {
  clearTimeout(hideTimer);
  hideTimer = null;
}
function scheduleAutoHide() {
  clearAutoHide();
  const secs = settings?.auto_hide_secs || 0;
  if (secs <= 0) return;
  hideTimer = setTimeout(function tick() {
    // Wait while the mouse is over the window (the user is reading).
    if (hovered) {
      hideTimer = setTimeout(tick, 1000);
      return;
    }
    invoke('hide_window');
  }, secs * 1000);
}
document.addEventListener('mouseenter', () => (hovered = true));
document.addEventListener('mouseleave', () => (hovered = false));

// ------------------------------------------------------------ appearance
function applySettings(s) {
  settings = s;
  const root = document.documentElement;
  root.dataset.theme = s.theme === 'light' ? 'light' : 'dark';
  root.style.setProperty('--alpha', String(s.opacity / 100));
  root.style.setProperty('--font-size', `${s.font_size}px`);
  providerEl.value = s.provider;
  $('ask').title = `Analyze screen${s.hotkey_ask ? ` (${s.hotkey_ask})` : ''}`;
  $('hide').title = `Hide (${s.hotkey_toggle ? s.hotkey_toggle + ' or ' : ''}Esc)`;
  if (!text && !currentId) showHint();
}

function showStealth(on) {
  $('stealthWarn').hidden = on;
  const btn = $('stealthBtn');
  btn.classList.toggle('off', !on);
  const hk = settings?.hotkey_stealth ? ` (${settings.hotkey_stealth})` : '';
  btn.title = on
    ? `Stealth ON: hidden from capture. Click to turn off${hk}.`
    : `Stealth OFF: VISIBLE to capture. Click to turn on${hk}.`;
}

// ------------------------------------------------------------ events
listen('answer', ({ payload: ev }) => {
  if (ev.id < currentId) return; // a stale, cancelled request
  switch (ev.kind) {
    case 'start':
      currentId = ev.id;
      text = '';
      answerEl.innerHTML = '';
      metaEl.textContent = '';
      clearAutoHide();
      setStatus('Starting…', 'busy');
      break;
    case 'status':
      setStatus(ev.text, 'busy');
      break;
    case 'delta':
      if (!text) setStatus('Answering…', 'busy');
      text += ev.text;
      queueRender();
      break;
    case 'done':
      render();
      setStatus('Done', 'ok');
      metaEl.textContent = ev.text;
      scheduleAutoHide();
      break;
    case 'error': {
      setStatus('Error', 'bad');
      const div = document.createElement('div');
      div.className = 'error';
      div.textContent = ev.text;
      answerEl.appendChild(div);
      break;
    }
  }
});
listen('settings', ({ payload }) => applySettings(payload));
listen('stealth', ({ payload }) => showStealth(payload));
listen('click_through', ({ payload }) => ($('clickBadge').hidden = !payload));
listen('new_chat', () => {
  text = '';
  currentId = 0;
  metaEl.textContent = '';
  clearAutoHide();
  showHint('New conversation. Press');
  setStatus('Ready', 'idle');
});

// ------------------------------------------------------------ controls
$('ask').onclick = () => invoke('ask_now');
$('hide').onclick = () => invoke('hide_window');
$('reset').onclick = () => invoke('new_session');
$('gear').onclick = () => invoke('open_settings');
$('stealthBtn').onclick = () => invoke('toggle_stealth_cmd');
providerEl.onchange = () => invoke('set_provider', { provider: providerEl.value });
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') invoke('hide_window');
});

(async () => {
  const cfg = await invoke('get_config');
  applySettings(cfg.settings);
  showStealth(cfg.stealth);
  $('clickBadge').hidden = !cfg.click_through;
  if (cfg.hotkey_errors.length) setStatus(cfg.hotkey_errors.join(' | '), 'bad');
})();
