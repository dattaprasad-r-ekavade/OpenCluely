use std::path::Path;

use serde::{Deserialize, Serialize};

/// User settings, stored as JSON in the app config folder.
/// Every field has a default, so old or partial files still load.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // ------------------------------------------------------------- AI
    /// "grok" | "codex" | "gemini"
    pub provider: String,
    /// Coding language for answers. "auto" = the language on the screen.
    pub language: String,
    /// Replaces the built-in screen prompt when not empty.
    pub custom_prompt: String,
    /// Extra instructions appended to the prompt.
    pub extra_instructions: String,
    /// Start a new conversation for every capture (no follow-up context).
    pub new_chat_each_capture: bool,

    pub grok_path: String,
    pub grok_model: String,
    /// grok `reasoning_effort`: low | medium | high | xhigh
    pub grok_effort: String,

    pub codex_path: String,
    pub codex_model: String,
    /// codex reasoning effort: low | medium | high | xhigh | max
    pub codex_effort: String,

    pub gemini_api_key: String,
    pub gemini_model: String,

    // -------------------------------------------------------- hotkeys
    // Accelerator strings such as "Ctrl+Shift+0". Empty = no hotkey.
    pub hotkey_ask: String,
    pub hotkey_toggle: String,
    pub hotkey_stealth: String,
    pub hotkey_click_through: String,
    pub hotkey_new_chat: String,

    // --------------------------------------------------------- window
    /// Background opacity of the answer window, 20..=100 (%).
    pub opacity: u8,
    /// Answer text size in px, 10..=22.
    pub font_size: u8,
    /// "dark" | "light"
    pub theme: String,
    /// Window size in logical px.
    pub width: u32,
    pub height: u32,
    /// "top-right" | "top-left" | "bottom-right" | "bottom-left" | "remember"
    pub position: String,
    /// Last position (physical px), used when position = "remember".
    pub saved_x: Option<i32>,
    pub saved_y: Option<i32>,
    pub always_on_top: bool,
    /// Hide the window this many seconds after an answer (0 = never).
    pub auto_hide_secs: u32,
    /// Show the window when a capture starts.
    pub show_on_capture: bool,

    // -------------------------------------------------------- capture
    /// "cursor" (monitor under the mouse) | "primary"
    pub capture_monitor: String,
    /// Longest image edge sent to the AI in px (0 = full resolution).
    pub capture_max_edge: u32,
    /// JPEG quality 40..=100.
    pub capture_quality: u8,

    // -------------------------------------------------------- general
    /// Start with content protection on. Off = visible to screen share.
    pub start_stealth: bool,
    pub launch_at_login: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: "grok".into(),
            language: "auto".into(),
            custom_prompt: String::new(),
            extra_instructions: String::new(),
            new_chat_each_capture: false,
            grok_path: String::new(),
            grok_model: String::new(),
            grok_effort: "low".into(),
            codex_path: String::new(),
            codex_model: "gpt-6-luna".into(),
            codex_effort: "low".into(),
            gemini_api_key: String::new(),
            // Same default model as the Electron app.
            gemini_model: "gemini-3.1-flash-lite".into(),

            hotkey_ask: "Ctrl+Shift+0".into(),
            hotkey_toggle: "Ctrl+Shift+9".into(),
            hotkey_stealth: "Ctrl+Shift+8".into(),
            hotkey_click_through: "Ctrl+Shift+7".into(),
            hotkey_new_chat: String::new(),

            opacity: 90,
            font_size: 13,
            theme: "dark".into(),
            width: 460,
            height: 380,
            position: "top-right".into(),
            saved_x: None,
            saved_y: None,
            always_on_top: true,
            auto_hide_secs: 0,
            show_on_capture: true,

            capture_monitor: "cursor".into(),
            capture_max_edge: 2048,
            capture_quality: 85,

            start_stealth: true,
            launch_at_login: false,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        let mut s: Settings = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if s.gemini_api_key.is_empty() {
            if let Ok(k) = std::env::var("GEMINI_API_KEY") {
                s.gemini_api_key = k;
            }
        }
        s.clamp();
        s
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default())
    }

    /// Keep numbers in safe ranges (a hand-edited file must not break the app).
    pub fn clamp(&mut self) {
        self.opacity = self.opacity.clamp(20, 100);
        self.font_size = self.font_size.clamp(10, 22);
        self.width = self.width.clamp(280, 1600);
        self.height = self.height.clamp(140, 1400);
        self.capture_quality = self.capture_quality.clamp(40, 100);
        if self.capture_max_edge != 0 {
            self.capture_max_edge = self.capture_max_edge.clamp(640, 8192);
        }
        self.auto_hide_secs = self.auto_hide_secs.min(3600);
    }
}
