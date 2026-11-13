//! The answer window (placement, stealth, click-through, appearance),
//! the settings window and the tray menu.
//!
//! IMPORTANT: shortcut, tray and window-event handlers run on the
//! event-loop thread. Window queries such as `cursor_position` wait for
//! that same thread, so calling them there deadlocks. Every public entry
//! point here that is called from a handler does its work in a task.

use std::sync::atomic::Ordering;

use tauri::menu::MenuItem;
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, Monitor, PhysicalPosition, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, Wry,
};

use crate::settings::Settings;
use crate::AppState;

pub const ANSWER: &str = "answer";
pub const SETTINGS: &str = "settings";
const MARGIN: i32 = 24;

/// Tray menu items whose text shows a hotkey or a state.
pub struct TrayItems {
    pub ask: MenuItem<Wry>,
    pub toggle: MenuItem<Wry>,
    pub stealth: MenuItem<Wry>,
    pub click: MenuItem<Wry>,
    pub new_chat: MenuItem<Wry>,
}

pub fn answer_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(ANSWER)
}

fn hk(accel: &str) -> String {
    if accel.trim().is_empty() {
        String::new()
    } else {
        format!("  ({})", accel.trim())
    }
}

pub fn update_tray(app: &AppHandle) {
    let (Some(items), Some(state)) = (app.try_state::<TrayItems>(), app.try_state::<AppState>()) else {
        return;
    };
    let s = state.settings.lock().unwrap().clone();
    let stealth = state.stealth.load(Ordering::SeqCst);
    let click = state.click_through.load(Ordering::SeqCst);
    let _ = items.ask.set_text(format!("Analyze screen{}", hk(&s.hotkey_ask)));
    let _ = items.toggle.set_text(format!("Show / hide window{}", hk(&s.hotkey_toggle)));
    let _ = items.stealth.set_text(format!(
        "Stealth: {}{}",
        if stealth { "ON (hidden from capture)" } else { "OFF (VISIBLE to capture)" },
        hk(&s.hotkey_stealth)
    ));
    let _ = items.click.set_text(format!(
        "Click-through: {}{}",
        if click { "ON" } else { "OFF" },
        hk(&s.hotkey_click_through)
    ));
    let _ = items.new_chat.set_text(format!("New conversation{}", hk(&s.hotkey_new_chat)));
}

// ------------------------------------------------------------ placement

/// Corner position for the window on a monitor, inside its work area
/// (the screen without the taskbar).
fn corner(win: &WebviewWindow, m: &Monitor, position: &str) -> Option<PhysicalPosition<i32>> {
    let size = win.outer_size().ok()?;
    let wa = m.work_area();
    let left = wa.position.x + MARGIN;
    let right = wa.position.x + wa.size.width as i32 - size.width as i32 - MARGIN;
    let top = wa.position.y + MARGIN;
    let bottom = wa.position.y + wa.size.height as i32 - size.height as i32 - MARGIN;
    let (x, y) = match position {
        "top-left" => (left, top),
        "bottom-left" => (left, bottom),
        "bottom-right" => (right, bottom),
        _ => (right, top),
    };
    Some(PhysicalPosition::new(x, y))
}

fn place(app: &AppHandle, win: &WebviewWindow, target: Option<Monitor>, s: &Settings) {
    if s.position == "remember" {
        if let (Some(x), Some(y)) = (s.saved_x, s.saved_y) {
            // Only if that point is still on a connected monitor.
            if app.monitor_from_point(x as f64 + 20.0, y as f64 + 20.0).ok().flatten().is_some() {
                let _ = win.set_position(PhysicalPosition::new(x, y));
                return;
            }
        }
    }
    let m = target.or_else(|| win.current_monitor().ok().flatten()).or_else(|| app.primary_monitor().ok().flatten());
    if let Some(p) = m.as_ref().and_then(|m| corner(win, m, &s.position)) {
        let _ = win.set_position(p);
    }
}

/// Show the answer window without focus. Corner positions follow the
/// monitor under the mouse; "remember" keeps the last place.
pub fn show_on_monitor(app: &AppHandle, cursor: Option<PhysicalPosition<f64>>) {
    let Some(win) = answer_window(app) else { return };
    let state = app.state::<AppState>();
    let s = state.settings.lock().unwrap().clone();

    let target = cursor.and_then(|c| app.monitor_from_point(c.x, c.y).ok().flatten());
    let current = win.current_monitor().ok().flatten();
    let other_monitor = match (&target, &current) {
        (Some(t), Some(c)) => t.position() != c.position(),
        _ => false,
    };
    let placed = state.placed.load(Ordering::SeqCst);
    if !placed || (other_monitor && s.position != "remember") {
        place(app, &win, target, &s);
        state.placed.store(true, Ordering::SeqCst);
    }
    if !win.is_visible().unwrap_or(false) {
        let _ = win.show();
    }
}

pub fn hide_answer(app: &AppHandle) {
    if let Some(w) = answer_window(app) {
        let _ = w.hide();
    }
    crate::persist_position(app);
}

pub fn toggle_window(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(win) = answer_window(&app) else { return };
        if win.is_visible().unwrap_or(false) {
            hide_answer(&app);
        } else {
            let cursor = app.cursor_position().ok();
            show_on_monitor(&app, cursor);
        }
    });
}

/// Apply size, always-on-top and position settings to the answer window.
/// `old` is None at startup.
pub fn apply_window_settings(app: &AppHandle, old: Option<&Settings>, new: &Settings) {
    let Some(win) = answer_window(app) else { return };
    let size_changed = old.is_none_or(|o| o.width != new.width || o.height != new.height);
    if size_changed {
        let _ = win.set_size(LogicalSize::new(new.width as f64, new.height as f64));
    }
    if old.is_none_or(|o| o.always_on_top != new.always_on_top) {
        let _ = win.set_always_on_top(new.always_on_top);
    }
    if old.is_some_and(|o| o.position != new.position || size_changed) {
        // Re-place now if visible, else on the next show.
        app.state::<AppState>().placed.store(false, Ordering::SeqCst);
        if win.is_visible().unwrap_or(false) {
            let cursor = app.cursor_position().ok();
            show_on_monitor(app, cursor);
        }
    }
    let _ = app.emit("settings", new);
}

// ------------------------------------------------------- stealth & click

/// Content protection on or off for all app windows. Off makes them
/// visible to screen share, recording AND remote-desktop tools.
pub fn set_stealth(app: &AppHandle, on: bool) {
    app.state::<AppState>().stealth.store(on, Ordering::SeqCst);
    for label in [ANSWER, SETTINGS] {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.set_content_protected(on);
        }
    }
    update_tray(app);
    let _ = app.emit("stealth", on);
}

pub fn toggle_stealth(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let on = !app.state::<AppState>().stealth.load(Ordering::SeqCst);
        set_stealth(&app, on);
        if !on {
            // Show the window, so a remote-desktop user can see it at once.
            let cursor = app.cursor_position().ok();
            show_on_monitor(&app, cursor);
        }
    });
}

/// Click-through: mouse clicks go to the app under the window. The window
/// can then not be clicked, so only the hotkey or the tray turns it off.
pub fn set_click_through(app: &AppHandle, on: bool) {
    app.state::<AppState>().click_through.store(on, Ordering::SeqCst);
    if let Some(w) = answer_window(app) {
        let _ = w.set_ignore_cursor_events(on);
    }
    update_tray(app);
    let _ = app.emit("click_through", on);
}

pub fn toggle_click_through(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let on = !app.state::<AppState>().click_through.load(Ordering::SeqCst);
        set_click_through(&app, on);
        if on {
            let cursor = app.cursor_position().ok();
            show_on_monitor(&app, cursor);
        }
    });
}

// -------------------------------------------------------- settings window

pub fn open_settings(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(w) = app.get_webview_window(SETTINGS) {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
            return;
        }
        let stealth = app.state::<AppState>().stealth.load(Ordering::SeqCst);
        // Fit the work area (screen without taskbar) of small or scaled screens.
        let (mut w, mut h) = (640.0, 600.0);
        if let Ok(Some(m)) = app.primary_monitor() {
            let wa = m.work_area();
            let scale = m.scale_factor();
            w = f64::min(w, wa.size.width as f64 / scale - 40.0);
            h = f64::min(h, wa.size.height as f64 / scale - 60.0);
        }
        let built = WebviewWindowBuilder::new(&app, SETTINGS, WebviewUrl::App("settings.html".into()))
            .title("OpenCluely Settings")
            .inner_size(w, h)
            .min_inner_size(480.0, 360.0)
            .center()
            // Above the always-on-top answer window, which would cover it.
            .always_on_top(true)
            .content_protected(stealth)
            .focused(true)
            .build();
        if let Err(e) = built {
            eprintln!("[settings] cannot open window: {e}");
        }
    });
}
