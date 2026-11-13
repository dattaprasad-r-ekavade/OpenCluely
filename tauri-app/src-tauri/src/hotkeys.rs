//! Global hotkeys, read from the settings and changeable at runtime.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::Ordering;

use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::settings::Settings;
use crate::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Ask,
    Toggle,
    Stealth,
    ClickThrough,
    NewChat,
}

/// (action, accelerator, human name) for every configurable hotkey.
pub fn list(s: &Settings) -> [(Action, String, &'static str); 5] {
    [
        (Action::Ask, s.hotkey_ask.clone(), "Analyze screen"),
        (Action::Toggle, s.hotkey_toggle.clone(), "Show / hide window"),
        (Action::Stealth, s.hotkey_stealth.clone(), "Toggle stealth"),
        (Action::ClickThrough, s.hotkey_click_through.clone(), "Toggle click-through"),
        (Action::NewChat, s.hotkey_new_chat.clone(), "New conversation"),
    ]
}

/// Unregister everything, then register the hotkeys from the settings.
/// Returns one message per hotkey that could not be registered.
pub fn apply(app: &AppHandle) -> Vec<String> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();

    let mut map: HashMap<u32, Action> = HashMap::new();
    let mut errors = Vec::new();
    for (action, accel, name) in list(&settings) {
        let accel = accel.trim();
        if accel.is_empty() {
            continue;
        }
        let sc = match Shortcut::from_str(accel) {
            Ok(sc) => sc,
            Err(e) => {
                errors.push(format!("{name}: \"{accel}\" is not a valid key combination ({e})."));
                continue;
            }
        };
        if map.contains_key(&sc.id()) {
            errors.push(format!("{name}: {accel} is already used by another action."));
            continue;
        }
        match gs.register(sc) {
            Ok(()) => {
                map.insert(sc.id(), action);
            }
            Err(e) => errors.push(format!("{name}: {accel} is used by another app ({e}).")),
        }
    }
    for e in &errors {
        eprintln!("[hotkeys] {e}");
    }
    *state.hotkeys.lock().unwrap() = map;
    *state.hotkey_errors.lock().unwrap() = errors.clone();
    state.hotkeys_paused.store(false, Ordering::SeqCst);
    crate::windows::update_tray(app);
    errors
}

/// Unregister all hotkeys while the settings window records a new combination,
/// so pressing the old combination does not trigger its action.
pub fn pause(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.hotkeys_paused.store(true, Ordering::SeqCst);
    let _ = app.global_shortcut().unregister_all();
}

/// Plugin handler: map the pressed shortcut to its action.
pub fn on_shortcut(app: &AppHandle, sc: &Shortcut, state: ShortcutState) {
    if state != ShortcutState::Pressed {
        return;
    }
    let Some(app_state) = app.try_state::<AppState>() else { return };
    let action = app_state.hotkeys.lock().unwrap().get(&sc.id()).copied();
    match action {
        Some(Action::Ask) => crate::ask_screen(app),
        Some(Action::Toggle) => crate::windows::toggle_window(app),
        Some(Action::Stealth) => crate::windows::toggle_stealth(app),
        Some(Action::ClickThrough) => crate::windows::toggle_click_through(app),
        Some(Action::NewChat) => crate::new_chat(app),
        None => {}
    }
}
