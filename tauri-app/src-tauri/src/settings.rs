use std::path::Path;

use serde::{Deserialize, Serialize};

/// User settings, stored as JSON in the app config folder.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "grok" | "codex" | "gemini"
    pub provider: String,
    /// Coding language for answers. "auto" = the language on the screen.
    pub language: String,
    /// Extra instructions appended to the screen prompt.
    pub extra_instructions: String,

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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: "grok".into(),
            language: "auto".into(),
            extra_instructions: String::new(),
            grok_path: String::new(),
            grok_model: String::new(),
            grok_effort: "low".into(),
            codex_path: String::new(),
            codex_model: "gpt-6-luna".into(),
            codex_effort: "low".into(),
            gemini_api_key: String::new(),
            // Same default model as the Electron app.
            gemini_model: "gemini-3.1-flash-lite".into(),
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
        s
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default())
    }
}
