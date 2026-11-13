// Floating answer window. Talks to the Rust side through window.__TAURI__.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const answerEl = $('answer');
const statusEl = $('status');
const dotEl = $('dot');
const metaEl = $('meta');
const providerEl = $('provider');
const form = $('settings');

let settings = null;
let currentId = 0;
let text = '';
let renderQueued = false;

// The model output is untrusted: render raw HTML in it as text.
const escapeHtml = (s) =>
  s.replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
marked.use({ gfm: true, breaks: false, renderer: { html: (token) => escapeHtml(token.text || '') } });

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

listen('answer', ({ payload: ev }) => {
  if (ev.id < currentId) return; // a stale, cancelled request
  switch (ev.kind) {
    case 'start':
      currentId = ev.id;
      text = '';
      answerEl.innerHTML = '';
      metaEl.textContent = '';
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

// ------------------------------------------------------------ controls
$('ask').onclick = () => invoke('ask_now');
$('hide').onclick = () => invoke('hide_window');
$('quit').onclick = () => invoke('quit_app');
$('reset').onclick = async () => {
  await invoke('new_session');
  text = '';
  answerEl.innerHTML = '<div class="hint"><p>New conversation. Press <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>0</kbd>.</p></div>';
  metaEl.textContent = '';
  setStatus('Ready', 'idle');
};
$('gear').onclick = () => {
  form.hidden = !form.hidden;
};
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') invoke('hide_window');
});

providerEl.onchange = async () => {
  settings.provider = providerEl.value;
  await invoke('save_config', { settings });
  setStatus(`Provider: ${providerEl.options[providerEl.selectedIndex].text}`, 'idle');
};

function fillForm() {
  for (const el of form.elements) {
    if (el.name && el.name in settings) el.value = settings[el.name] ?? '';
  }
  providerEl.value = settings.provider;
}

form.onsubmit = async (e) => {
  e.preventDefault();
  for (const el of form.elements) {
    if (el.name && el.name in settings) settings[el.name] = el.value;
  }
  settings.provider = providerEl.value;
  await invoke('save_config', { settings });
  form.hidden = true;
  setStatus('Settings saved', 'ok');
};

(async () => {
  const cfg = await invoke('get_config');
  settings = cfg.settings;
  $('path').textContent = cfg.settings_path;
  $('path').title = cfg.settings_path;
  fillForm();
  if (cfg.shortcut_errors.length) {
    setStatus(cfg.shortcut_errors.join(' | '), 'bad');
  }
})();
