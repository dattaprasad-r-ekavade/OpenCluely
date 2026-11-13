// Settings window.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow } = window.__TAURI__.window;

const $ = (id) => document.getElementById(id);
const form = $('form');
let defaults = null;
let saved = null;

// ------------------------------------------------------------ tabs
for (const btn of document.querySelectorAll('#tabs button')) {
  btn.onclick = () => {
    document.querySelectorAll('#tabs button').forEach((b) => b.classList.toggle('active', b === btn));
    document.querySelectorAll('section[data-panel]').forEach((p) =>
      p.classList.toggle('active', p.dataset.panel === btn.dataset.tab),
    );
  };
}

// ------------------------------------------------------------ form <-> settings
const NUMBER_FIELDS = new Set([
  'opacity', 'font_size', 'width', 'height', 'auto_hide_secs', 'capture_max_edge', 'capture_quality',
]);

function fill(s) {
  for (const el of form.elements) {
    if (!el.name || !(el.name in s)) continue;
    if (el.type === 'checkbox') el.checked = !!s[el.name];
    else el.value = s[el.name] ?? '';
  }
  form.querySelectorAll('input[type=range]').forEach(showRange);
  checkHotkeyDuplicates();
}

function read() {
  const s = { ...saved };
  for (const el of form.elements) {
    if (!el.name || !(el.name in s)) continue;
    if (el.type === 'checkbox') s[el.name] = el.checked;
    else if (NUMBER_FIELDS.has(el.name)) s[el.name] = Number(el.value) || 0;
    else s[el.name] = el.value;
  }
  return s;
}

function showRange(el) {
  const out = el.parentElement.querySelector('output');
  if (out) out.textContent = el.value;
}
form.querySelectorAll('input[type=range]').forEach((el) => el.addEventListener('input', () => showRange(el)));

function message(text, kind = '') {
  const m = $('message');
  m.textContent = text;
  m.className = kind;
}

// ------------------------------------------------------------ hotkey recorder
const MODIFIER_CODES = new Set([
  'ControlLeft', 'ControlRight', 'ShiftLeft', 'ShiftRight', 'AltLeft', 'AltRight', 'MetaLeft', 'MetaRight',
]);

const MODIFIER_KEYS = new Set(['Control', 'Shift', 'Alt', 'Meta', 'OS']);

// Some input (remote desktop, injected keys) has no scan code, so e.code is
// empty. Fall back to keyCode, which still names the physical key.
function codeOf(e) {
  if (e.code) return e.code;
  const k = e.keyCode;
  if (k >= 48 && k <= 57) return 'Digit' + (k - 48);
  if (k >= 65 && k <= 90) return 'Key' + String.fromCharCode(k);
  if (k >= 96 && k <= 105) return 'Numpad' + (k - 96);
  if (k >= 112 && k <= 135) return 'F' + (k - 111);
  return e.key || '';
}

function keyName(code) {
  if (code.startsWith('Digit')) return code.slice(5);
  if (code.startsWith('Key')) return code.slice(3);
  return code;
}

let recording = null; // { el, before }

for (const el of form.querySelectorAll('input.hk')) {
  el.placeholder = 'none';
  el.addEventListener('focus', async () => {
    recording = { el, before: el.value };
    el.classList.add('recording');
    el.value = '';
    el.placeholder = 'Press keys…';
    // Stop the global hotkeys, so pressing an old combination does not trigger it.
    await invoke('pause_hotkeys');
  });
  el.addEventListener('blur', () => stopRecording(false));
  el.addEventListener('keydown', (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (!recording) return;
    const mods = [];
    if (e.ctrlKey) mods.push('Ctrl');
    if (e.altKey) mods.push('Alt');
    if (e.shiftKey) mods.push('Shift');
    if (e.metaKey) mods.push('Super');
    const code = codeOf(e);
    if (MODIFIER_CODES.has(code) || MODIFIER_KEYS.has(e.key) || !code) {
      el.value = mods.length ? mods.join('+') + '+…' : '';
      return;
    }
    if (!mods.length && code === 'Escape') return stopRecording(true);
    if (!mods.length && (code === 'Backspace' || code === 'Delete')) {
      el.value = '';
      return el.blur();
    }
    const key = keyName(code);
    if (!mods.length && !/^F\d{1,2}$/.test(key)) {
      el.value = '';
      message('Use at least one of Ctrl, Alt, Shift or Win, or an F-key.', 'bad');
      return;
    }
    el.value = [...mods, key].join('+');
    message('');
    el.blur();
  });
}

async function stopRecording(cancel) {
  if (!recording) return;
  const { el, before } = recording;
  recording = null;
  el.classList.remove('recording');
  el.placeholder = 'none';
  if (cancel || el.value.endsWith('…')) el.value = before;
  checkHotkeyDuplicates();
  if (cancel) el.blur();
  // Re-register the saved hotkeys until the user saves.
  const errors = await invoke('resume_hotkeys');
  if (errors.length) message(errors.join('\n'), 'bad');
}

function checkHotkeyDuplicates() {
  const seen = new Map();
  for (const el of form.querySelectorAll('input.hk')) {
    el.classList.remove('dup');
    const v = el.value.trim().toLowerCase();
    if (!v) continue;
    if (seen.has(v)) {
      el.classList.add('dup');
      seen.get(v).classList.add('dup');
    } else seen.set(v, el);
  }
}

// ------------------------------------------------------------ stealth
function showStealth(on) {
  const s = $('stealthState');
  s.textContent = on ? 'Stealth is ON: windows are hidden from capture.' : 'Stealth is OFF: windows are VISIBLE to capture.';
  s.className = on ? 'ok' : 'bad';
}
listen('stealth', ({ payload }) => showStealth(payload));
$('stealthToggle').onclick = () => invoke('toggle_stealth_cmd');

// ------------------------------------------------------------ buttons
$('save').onclick = async () => {
  if (form.querySelector('input.hk.dup')) {
    message('Two actions use the same hotkey. Change one of them.', 'bad');
    return;
  }
  try {
    const s = read();
    const errors = await invoke('save_config', { settings: s });
    saved = s;
    if (errors.length) message('Saved, but:\n' + errors.join('\n'), 'bad');
    else message('Saved.', 'ok');
  } catch (e) {
    message(String(e), 'bad');
  }
};
$('cancel').onclick = () => getCurrentWindow().close();
$('reset').onclick = () => {
  // Keep the secrets and paths: a reset should not lose an API key.
  const keep = ['gemini_api_key', 'grok_path', 'codex_path'];
  const s = { ...defaults };
  for (const k of keep) s[k] = saved[k];
  fill(s);
  message('Defaults loaded. Click Save to apply them.', '');
};
$('quit').onclick = () => invoke('quit_app');
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !recording) getCurrentWindow().close();
  if (e.key === 's' && e.ctrlKey) {
    e.preventDefault();
    $('save').click();
  }
});
// If the window closes while recording, turn the hotkeys back on.
window.addEventListener('beforeunload', () => {
  if (recording) invoke('resume_hotkeys');
});

// Keep the AI provider in sync with the quick switch in the answer window.
listen('settings', ({ payload }) => {
  if (saved && payload.provider !== saved.provider) {
    saved.provider = payload.provider;
    form.elements.provider.value = payload.provider;
  }
});

(async () => {
  const cfg = await invoke('get_config');
  defaults = cfg.defaults;
  saved = cfg.settings;
  $('version').textContent = 'v' + cfg.version;
  $('path').textContent = cfg.settings_path;
  fill(saved);
  showStealth(cfg.stealth);
  if (cfg.hotkey_errors.length) message(cfg.hotkey_errors.join('\n'), 'bad');
})();
