//! OpenCluely (Tauri): screen assistant.
//!
//! A hotkey (default Ctrl+Shift+0) captures the monitor, sends it to the
//! selected AI provider, and streams the answer into a small floating
//! window. The window is content-protected (hidden from screen share,
//! recording and our own screenshots), is not in the taskbar, and appears
//! without taking keyboard focus. Everything is configurable in the
//! settings window (tray menu or the gear button).

mod capture;
mod hotkeys;
mod providers;
mod settings;
mod windows;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Instant;

use base64::Engine;
use serde::Serialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tokio::sync::Mutex;

use providers::codex::CodexApp;
use providers::grok::GrokAcp;
use providers::DeltaFn;
use settings::Settings;
use windows::{TrayItems, ANSWER};

#[derive(Clone, Serialize)]
struct AnswerEvent {
    /// start | status | delta | done | error
    kind: &'static str,
    id: u64,
    text: String,
}

pub struct AppState {
    /// std Mutex: short critical sections, never held across `.await`.
    pub settings: StdMutex<Settings>,
    settings_path: PathBuf,
    workdir: PathBuf,
    seq: AtomicU64,
    running: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    grok: Mutex<Option<GrokAcp>>,
    codex: Mutex<Option<CodexApp>>,
    /// Registered shortcut id -> action.
    pub hotkeys: StdMutex<HashMap<u32, hotkeys::Action>>,
    pub hotkey_errors: StdMutex<Vec<String>>,
    pub hotkeys_paused: AtomicBool,
    /// The answer window has been placed at least once.
    pub placed: AtomicBool,
    /// Content protection on (hidden from capture).
    pub stealth: AtomicBool,
    pub click_through: AtomicBool,
    /// Last answer-window position, saved when position = "remember".
    last_pos: StdMutex<Option<(i32, i32)>>,
}

fn emit(app: &AppHandle, kind: &'static str, id: u64, text: impl Into<String>) {
    let _ = app.emit_to(ANSWER, "answer", AnswerEvent { kind, id, text: text.into() });
}

fn settings_of(app: &AppHandle) -> Settings {
    app.state::<AppState>().settings.lock().unwrap().clone()
}

/// Save the dragged window position when the position setting is "remember".
pub fn persist_position(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some((x, y)) = *state.last_pos.lock().unwrap() else { return };
    let mut s = state.settings.lock().unwrap();
    if s.position == "remember" && (s.saved_x, s.saved_y) != (Some(x), Some(y)) {
        s.saved_x = Some(x);
        s.saved_y = Some(y);
        let _ = s.save(&state.settings_path);
    }
}

// ---------------------------------------------------------------- requests

/// Hotkey "Analyze screen". Cancels a running answer and starts a new one.
pub fn ask_screen(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let id = state.seq.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(prev) = state.running.lock().await.take() {
            prev.abort();
        }
        cancel_providers(&state).await;
        if settings_of(&app).new_chat_each_capture {
            reset_sessions(&state).await;
        }
        let task_app = app.clone();
        let handle = tauri::async_runtime::spawn(async move { run_request(task_app, id).await });
        *state.running.lock().await = Some(handle);
    });
}

/// Hotkey / button "New conversation".
pub fn new_chat(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        if let Some(prev) = state.running.lock().await.take() {
            prev.abort();
        }
        reset_sessions(&state).await;
        let _ = app.emit_to(ANSWER, "new_chat", ());
    });
}

async fn run_request(app: AppHandle, id: u64) {
    let started = Instant::now();
    let settings = settings_of(&app);
    let cursor = app.cursor_position().ok();

    emit(&app, "start", id, settings.provider.clone());
    if settings.show_on_capture {
        windows::show_on_monitor(&app, cursor);
    }
    emit(&app, "status", id, "Capturing screen…");

    // 1. Screenshot.
    let opts = capture::CaptureOptions {
        point: match settings.capture_monitor.as_str() {
            "primary" => None,
            _ => cursor.map(|c| (c.x as i32, c.y as i32)),
        },
        max_edge: settings.capture_max_edge,
        jpeg_quality: settings.capture_quality,
    };
    let shot = match tauri::async_runtime::spawn_blocking(move || capture::capture(&opts)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return emit(&app, "error", id, format!("{e:#}")),
        Err(e) => return emit(&app, "error", id, e.to_string()),
    };
    let capture_ms = started.elapsed().as_millis();
    let image_b64 = base64::engine::general_purpose::STANDARD.encode(&shot.jpeg);

    // 2. Ask the provider and stream the answer.
    let prompt = providers::screen_prompt(
        &settings.language,
        &settings.custom_prompt,
        &settings.extra_instructions,
    );
    let sink: DeltaFn = {
        let app = app.clone();
        std::sync::Arc::new(move |d: &str| emit(&app, "delta", id, d))
    };
    let provider = settings.provider.clone();
    emit(
        &app,
        "status",
        id,
        format!("Asking {} ({}×{})…", provider_label(&provider), shot.width, shot.height),
    );

    let result = match provider.as_str() {
        "codex" => match codex_client(&app, &settings).await {
            Ok(c) => c.ask(&prompt, &image_b64, capture::Screenshot::MIME, sink).await,
            Err(e) => Err(e),
        },
        "gemini" => {
            providers::gemini::ask(
                &settings.gemini_api_key,
                &settings.gemini_model,
                &prompt,
                &image_b64,
                capture::Screenshot::MIME,
                sink,
            )
            .await
        }
        _ => match grok_client(&app, &settings).await {
            Ok(g) => g.ask(&prompt, &image_b64, capture::Screenshot::MIME, sink).await,
            Err(e) => Err(e),
        },
    };

    #[cfg(debug_assertions)]
    match &result {
        Ok(t) => eprintln!(
            "[answer #{id}] {provider} {:.1}s, {} chars: {}",
            started.elapsed().as_secs_f32(),
            t.len(),
            t.chars().take(300).collect::<String>().replace('\n', " ⏎ ")
        ),
        Err(e) => eprintln!("[answer #{id}] {provider} error: {e:#}"),
    }

    match result {
        Ok(_) => emit(
            &app,
            "done",
            id,
            format!(
                "{} · {:.1}s (capture {}ms)",
                provider_label(&provider),
                started.elapsed().as_secs_f32(),
                capture_ms
            ),
        ),
        Err(e) if e.to_string() == "cancelled" => {}
        Err(e) => emit(&app, "error", id, format!("{e:#}")),
    }
}

/// Return the warm Grok ACP process, or start one.
async fn grok_client(app: &AppHandle, settings: &Settings) -> anyhow::Result<GrokAcp> {
    let state = app.state::<AppState>();
    let mut slot = state.grok.lock().await;
    if let Some(g) = slot.as_ref() {
        if g.is_alive() {
            return Ok(g.clone());
        }
    }
    let exe = providers::resolve_exe(&settings.grok_path, "grok", &[".grok/bin", ".local/bin"]);
    let g = GrokAcp::start(&exe, &settings.grok_model, &settings.grok_effort, &state.workdir).await?;
    *slot = Some(g.clone());
    Ok(g)
}

/// Return the warm Codex app-server process, or start one.
async fn codex_client(app: &AppHandle, settings: &Settings) -> anyhow::Result<CodexApp> {
    let state = app.state::<AppState>();
    let mut slot = state.codex.lock().await;
    if let Some(c) = slot.as_ref() {
        if c.is_alive() {
            return Ok(c.clone());
        }
    }
    let exe = providers::resolve_exe(
        &settings.codex_path,
        "codex",
        &["AppData/Local/Programs/OpenAI/Codex/bin", "AppData/Local/OpenAI/Codex/bin", ".local/bin"],
    );
    let c = CodexApp::start(&exe, &settings.codex_model, &settings.codex_effort, &state.workdir).await?;
    *slot = Some(c.clone());
    Ok(c)
}

/// Start the selected CLI provider in the background, so the first answer is fast.
fn warm_up(app: &AppHandle, settings: Settings) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let r = match settings.provider.as_str() {
            "grok" => grok_client(&app, &settings).await.map(|_| ()),
            "codex" => codex_client(&app, &settings).await.map(|_| ()),
            _ => Ok(()),
        };
        if let Err(e) = r {
            eprintln!("[{}] warm-up failed: {e:#}", settings.provider);
        }
    });
}

async fn cancel_providers(state: &AppState) {
    if let Some(g) = state.grok.lock().await.as_ref() {
        g.cancel().await;
    }
    if let Some(c) = state.codex.lock().await.as_ref() {
        c.cancel().await;
    }
}

async fn reset_sessions(state: &AppState) {
    if let Some(g) = state.grok.lock().await.as_ref() {
        g.new_session().await;
    }
    if let Some(c) = state.codex.lock().await.as_ref() {
        c.new_session().await;
    }
}

fn provider_label(p: &str) -> &'static str {
    match p {
        "codex" => "Codex CLI",
        "gemini" => "Gemini",
        _ => "Grok CLI",
    }
}

// ---------------------------------------------------------------- commands

#[derive(Serialize)]
struct ConfigView {
    settings: Settings,
    defaults: Settings,
    settings_path: String,
    hotkey_errors: Vec<String>,
    stealth: bool,
    click_through: bool,
    version: &'static str,
}

#[tauri::command]
fn get_config(state: tauri::State<'_, AppState>) -> ConfigView {
    ConfigView {
        settings: state.settings.lock().unwrap().clone(),
        defaults: Settings::default(),
        settings_path: state.settings_path.to_string_lossy().into_owned(),
        hotkey_errors: state.hotkey_errors.lock().unwrap().clone(),
        stealth: state.stealth.load(Ordering::SeqCst),
        click_through: state.click_through.load(Ordering::SeqCst),
        version: env!("CARGO_PKG_VERSION"),
    }
}

/// Save and apply settings. Returns the hotkey problems (empty = all fine).
#[tauri::command]
async fn save_config(app: AppHandle, mut settings: Settings) -> Result<Vec<String>, String> {
    settings.clamp();
    let state = app.state::<AppState>();
    let old = {
        let mut cur = state.settings.lock().unwrap();
        // The saved position is owned by the app, not by the form.
        settings.saved_x = cur.saved_x;
        settings.saved_y = cur.saved_y;
        let old = cur.clone();
        *cur = settings.clone();
        cur.save(&state.settings_path).map_err(|e| e.to_string())?;
        old
    };

    // Providers: restart a CLI with its new options on the next question.
    let grok_changed = old.grok_path != settings.grok_path
        || old.grok_model != settings.grok_model
        || old.grok_effort != settings.grok_effort;
    let codex_changed = old.codex_path != settings.codex_path
        || old.codex_model != settings.codex_model
        || old.codex_effort != settings.codex_effort;
    if grok_changed {
        state.grok.lock().await.take();
    }
    if codex_changed {
        state.codex.lock().await.take();
    }
    if old.provider != settings.provider || grok_changed || codex_changed {
        warm_up(&app, settings.clone());
    }

    windows::apply_window_settings(&app, Some(&old), &settings);

    if old.launch_at_login != settings.launch_at_login {
        let al = app.autolaunch();
        let r = if settings.launch_at_login { al.enable() } else { al.disable() };
        if let Err(e) = r {
            return Err(format!("Launch at login: {e}"));
        }
    }

    let hotkeys_changed = hotkeys::list(&old)
        .iter()
        .zip(hotkeys::list(&settings).iter())
        .any(|(a, b)| a.1 != b.1);
    let errors = if hotkeys_changed || state.hotkeys_paused.load(Ordering::SeqCst) {
        hotkeys::apply(&app)
    } else {
        state.hotkey_errors.lock().unwrap().clone()
    };
    windows::update_tray(&app);
    Ok(errors)
}

/// Quick provider switch from the answer window header.
#[tauri::command]
fn set_provider(app: AppHandle, provider: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let s = {
        let mut s = state.settings.lock().unwrap();
        s.provider = provider;
        s.save(&state.settings_path).map_err(|e| e.to_string())?;
        s.clone()
    };
    let _ = app.emit("settings", &s);
    warm_up(&app, s);
    Ok(())
}

#[tauri::command]
fn pause_hotkeys(app: AppHandle) {
    hotkeys::pause(&app);
}

#[tauri::command]
fn resume_hotkeys(app: AppHandle) -> Vec<String> {
    hotkeys::apply(&app)
}

#[tauri::command]
fn toggle_stealth_cmd(app: AppHandle) {
    windows::toggle_stealth(&app);
}

#[tauri::command]
fn open_settings(app: AppHandle) {
    windows::open_settings(&app);
}

#[tauri::command]
fn ask_now(app: AppHandle) {
    ask_screen(&app);
}

#[tauri::command]
fn new_session(app: AppHandle) {
    new_chat(&app);
}

#[tauri::command]
fn hide_window(app: AppHandle) {
    windows::hide_answer(&app);
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

// --------------------------------------------------------------------- run

pub fn run() {
    let app = tauri::Builder::default()
        // Must be the first plugin. A second copy of the app exits at once
        // (two copies would fight over the global shortcuts); the running
        // copy shows its window instead.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let cursor = app.cursor_position().ok();
                windows::show_on_monitor(&app, cursor);
            });
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, sc, ev| hotkeys::on_shortcut(app, sc, ev.state()))
                .build(),
        )
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .on_window_event(|window, event| {
            if window.label() != ANSWER {
                return;
            }
            if let WindowEvent::Moved(p) = event {
                if let Some(state) = window.app_handle().try_state::<AppState>() {
                    *state.last_pos.lock().unwrap() = Some((p.x, p.y));
                }
            }
        })
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_data_dir()?;
            // Empty folder used as the working directory of the CLI agents.
            let workdir = data_dir.join("workspace");
            std::fs::create_dir_all(&workdir)?;
            let settings_path = config_dir.join("settings.json");
            let settings = Settings::load(&settings_path);

            // Debug aid: OPENCLUELY_CAPTURE_VISIBLE=1 starts with stealth off.
            let force_visible = std::env::var("OPENCLUELY_CAPTURE_VISIBLE").is_ok_and(|v| v == "1");
            let stealth = settings.start_stealth && !force_visible;

            app.manage(AppState {
                settings: StdMutex::new(settings.clone()),
                settings_path,
                workdir,
                seq: AtomicU64::new(0),
                running: Mutex::new(None),
                grok: Mutex::new(None),
                codex: Mutex::new(None),
                hotkeys: Default::default(),
                hotkey_errors: Default::default(),
                hotkeys_paused: AtomicBool::new(false),
                placed: AtomicBool::new(false),
                stealth: AtomicBool::new(stealth),
                click_through: AtomicBool::new(false),
                last_pos: StdMutex::new(None),
            });

            // Tray icon: the window is not in the taskbar, so this is the way
            // to reach settings and to quit.
            let item = |id: &str| MenuItem::with_id(app, id, id, true, None::<&str>);
            let items = TrayItems {
                ask: item("ask")?,
                toggle: item("toggle")?,
                stealth: item("stealth")?,
                click: item("click")?,
                new_chat: item("new_chat")?,
            };
            let settings_item = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit OpenCluely", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let sep2 = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(
                app,
                &[
                    &items.ask,
                    &items.toggle,
                    &items.new_chat,
                    &sep,
                    &items.stealth,
                    &items.click,
                    &settings_item,
                    &sep2,
                    &quit_item,
                ],
            )?;
            app.manage(items);
            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("OpenCluely")
                .menu(&menu)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "ask" => ask_screen(app),
                    "toggle" => windows::toggle_window(app),
                    "new_chat" => new_chat(app),
                    "stealth" => windows::toggle_stealth(app),
                    "click" => windows::toggle_click_through(app),
                    "settings" => windows::open_settings(app),
                    "quit" => app.exit(0),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            let handle = app.handle().clone();
            hotkeys::apply(&handle);
            windows::set_stealth(&handle, stealth);
            windows::apply_window_settings(&handle, None, &settings);

            // Keep the OS launch-at-login entry in line with the setting.
            let al = handle.autolaunch();
            if al.is_enabled().unwrap_or(false) != settings.launch_at_login {
                let _ = if settings.launch_at_login { al.enable() } else { al.disable() };
            }

            warm_up(&handle, settings);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            set_provider,
            pause_hotkeys,
            resume_hotkeys,
            toggle_stealth_cmd,
            open_settings,
            ask_now,
            new_session,
            hide_window,
            quit_app
        ])
        .build(tauri::generate_context!())
        .expect("error while building OpenCluely");

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            persist_position(app);
            // Stop the CLI child processes before we exit.
            let state = app.state::<AppState>();
            let grok = state.grok.try_lock().ok().and_then(|mut g| g.take());
            let codex = state.codex.try_lock().ok().and_then(|mut c| c.take());
            drop((grok, codex));
        }
    });
}
