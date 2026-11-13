//! OpenCluely (Tauri). Phase 0: screen assistant.
//!
//! Ctrl+Shift+0  capture the monitor under the mouse, send it to the AI,
//!               stream the answer into a small floating window.
//! Ctrl+Shift+9  show or hide the floating window.
//!
//! The floating window is content-protected (not visible to screen share,
//! recording, or our own screenshots), is not in the taskbar, and appears
//! without taking keyboard focus from the app you are using.

mod capture;
mod providers;
mod settings;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use base64::Engine;
use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, RunEvent, WebviewWindow};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tokio::sync::Mutex;

use providers::codex::CodexApp;
use providers::grok::GrokAcp;
use providers::DeltaFn;
use settings::Settings;

const WINDOW: &str = "answer";
const MARGIN: i32 = 24;

#[derive(Clone, Serialize)]
struct AnswerEvent {
    /// start | status | delta | done | error
    kind: &'static str,
    id: u64,
    text: String,
}

struct AppState {
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    workdir: PathBuf,
    seq: AtomicU64,
    running: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    grok: Mutex<Option<GrokAcp>>,
    codex: Mutex<Option<CodexApp>>,
    shortcut_errors: std::sync::Mutex<Vec<String>>,
    placed: AtomicBool,
}

fn emit(app: &AppHandle, kind: &'static str, id: u64, text: impl Into<String>) {
    let _ = app.emit_to(WINDOW, "answer", AnswerEvent { kind, id, text: text.into() });
}

fn answer_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(WINDOW)
}

/// Show the window without focus. On first use, or when the question comes
/// from another monitor, put it in the top-right corner of that monitor.
fn show_on_monitor(app: &AppHandle, cursor: Option<PhysicalPosition<f64>>) {
    let Some(win) = answer_window(app) else { return };
    let state = app.state::<AppState>();

    let target = cursor.and_then(|c| app.monitor_from_point(c.x, c.y).ok().flatten());
    let current = win.current_monitor().ok().flatten();
    let other_monitor = match (&target, &current) {
        (Some(t), Some(c)) => t.position() != c.position(),
        _ => false,
    };
    if !state.placed.load(Ordering::SeqCst) || other_monitor {
        if let (Some(m), Ok(size)) = (target.or(current), win.outer_size()) {
            let x = m.position().x + m.size().width as i32 - size.width as i32 - MARGIN;
            let y = m.position().y + MARGIN;
            let _ = win.set_position(PhysicalPosition::new(x, y));
            state.placed.store(true, Ordering::SeqCst);
        }
    }
    if !win.is_visible().unwrap_or(false) {
        let _ = win.show();
    }
}

/// Shortcut and tray handlers run on the event-loop thread. Window queries
/// such as `cursor_position` wait for that same thread, so calling them
/// there deadlocks. Always do the work in a background task.
fn toggle_window(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(win) = answer_window(&app) else { return };
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
        } else {
            let cursor = app.cursor_position().ok();
            show_on_monitor(&app, cursor);
        }
    });
}

/// Entry point for Ctrl+Shift+0. Cancels a running answer and starts a new one.
fn ask_screen(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let id = state.seq.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(prev) = state.running.lock().await.take() {
            prev.abort();
        }
        cancel_providers(&state).await;
        let task_app = app.clone();
        let handle = tauri::async_runtime::spawn(async move { run_request(task_app, id).await });
        *state.running.lock().await = Some(handle);
    });
}

async fn run_request(app: AppHandle, id: u64) {
    let started = Instant::now();
    let state = app.state::<AppState>();
    let settings = state.settings.lock().await.clone();
    let cursor = app.cursor_position().ok();

    emit(&app, "start", id, settings.provider.clone());
    show_on_monitor(&app, cursor);
    emit(&app, "status", id, "Capturing screen…");

    // 1. Screenshot of the monitor under the mouse.
    let (cx, cy) = cursor.map(|c| (c.x as i32, c.y as i32)).unwrap_or((0, 0));
    let shot = match tauri::async_runtime::spawn_blocking(move || capture::capture_at(cx, cy)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return emit(&app, "error", id, format!("{e:#}")),
        Err(e) => return emit(&app, "error", id, e.to_string()),
    };
    let capture_ms = started.elapsed().as_millis();
    let image_b64 = base64::engine::general_purpose::STANDARD.encode(&shot.jpeg);

    // 2. Ask the provider and stream the answer.
    let prompt = providers::screen_prompt(&settings.language, &settings.extra_instructions);
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
    settings_path: String,
    shortcut_errors: Vec<String>,
}

#[tauri::command]
async fn get_config(state: tauri::State<'_, AppState>) -> Result<ConfigView, String> {
    Ok(ConfigView {
        settings: state.settings.lock().await.clone(),
        settings_path: state.settings_path.to_string_lossy().into_owned(),
        shortcut_errors: state.shortcut_errors.lock().unwrap().clone(),
    })
}

#[tauri::command]
async fn save_config(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    settings: Settings,
) -> Result<(), String> {
    let mut current = state.settings.lock().await;
    let grok_changed = current.grok_path != settings.grok_path
        || current.grok_model != settings.grok_model
        || current.grok_effort != settings.grok_effort;
    let codex_changed = current.codex_path != settings.codex_path
        || current.codex_model != settings.codex_model
        || current.codex_effort != settings.codex_effort;
    let provider_changed = current.provider != settings.provider;
    *current = settings.clone();
    current.save(&state.settings_path).map_err(|e| e.to_string())?;
    drop(current);
    // Restart a CLI with its new options on the next question.
    if grok_changed {
        state.grok.lock().await.take();
    }
    if codex_changed {
        state.codex.lock().await.take();
    }
    if provider_changed || grok_changed || codex_changed {
        warm_up(&app, settings);
    }
    Ok(())
}

#[tauri::command]
fn ask_now(app: AppHandle) {
    ask_screen(&app);
}

#[tauri::command]
async fn new_session(state: tauri::State<'_, AppState>) -> Result<(), String> {
    if let Some(prev) = state.running.lock().await.take() {
        prev.abort();
    }
    if let Some(g) = state.grok.lock().await.as_ref() {
        g.new_session().await;
    }
    if let Some(c) = state.codex.lock().await.as_ref() {
        c.new_session().await;
    }
    Ok(())
}

#[tauri::command]
fn hide_window(app: AppHandle) {
    if let Some(w) = answer_window(&app) {
        let _ = w.hide();
    }
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

// --------------------------------------------------------------------- run

pub fn run() {
    let ask = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Digit0);
    let toggle = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Digit9);

    let app = tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, sc, ev| {
                    if ev.state() != ShortcutState::Pressed {
                        return;
                    }
                    if sc == &ask {
                        ask_screen(app);
                    } else if sc == &toggle {
                        toggle_window(app);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_data_dir()?;
            // Empty folder used as the working directory of the CLI agents.
            let workdir = data_dir.join("workspace");
            std::fs::create_dir_all(&workdir)?;
            let settings_path = config_dir.join("settings.json");
            let settings = Settings::load(&settings_path);

            app.manage(AppState {
                settings: Mutex::new(settings.clone()),
                settings_path,
                workdir,
                seq: AtomicU64::new(0),
                running: Mutex::new(None),
                grok: Mutex::new(None),
                codex: Mutex::new(None),
                shortcut_errors: Default::default(),
                placed: AtomicBool::new(false),
            });

            let gs = app.global_shortcut();
            for (sc, name) in [(ask, "Ctrl+Shift+0"), (toggle, "Ctrl+Shift+9")] {
                if let Err(e) = gs.register(sc) {
                    let msg = format!("{name} is used by another app: {e}");
                    eprintln!("[shortcut] {msg}");
                    app.state::<AppState>().shortcut_errors.lock().unwrap().push(msg);
                }
            }

            // Tray icon: the window is not in the taskbar, so this is the way to quit.
            let ask_item = MenuItem::with_id(app, "ask", "Analyze screen (Ctrl+Shift+0)", true, None::<&str>)?;
            let toggle_item = MenuItem::with_id(app, "toggle", "Show / hide (Ctrl+Shift+9)", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&ask_item, &toggle_item, &quit_item])?;
            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("OpenCluely")
                .menu(&menu)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "ask" => ask_screen(app),
                    "toggle" => toggle_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            warm_up(app.handle(), settings);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            ask_now,
            new_session,
            hide_window,
            quit_app
        ])
        .build(tauri::generate_context!())
        .expect("error while building OpenCluely");

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            // Stop the CLI child processes before we exit.
            let state = app.state::<AppState>();
            let grok = state.grok.try_lock().ok().and_then(|mut g| g.take());
            let codex = state.codex.try_lock().ok().and_then(|mut c| c.take());
            drop((grok, codex));
        }
    });
}
