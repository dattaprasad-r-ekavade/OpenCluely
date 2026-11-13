//! AI providers. Each one takes the screen prompt plus a screenshot and
//! streams the answer text through a `DeltaFn`.

pub mod codex;
pub mod gemini;
pub mod grok;
mod rpc;

use std::sync::Arc;

/// Receives answer text as it arrives.
pub type DeltaFn = Arc<dyn Fn(&str) + Send + Sync>;

/// The instruction sent with every screenshot. `custom` replaces the
/// built-in prompt when it is not empty.
pub fn screen_prompt(language: &str, custom: &str, extra: &str) -> String {
    if !custom.trim().is_empty() {
        let mut p = custom.trim().to_string();
        if !extra.trim().is_empty() {
            p.push_str("\n\n");
            p.push_str(extra.trim());
        }
        return p;
    }
    default_prompt(language, extra)
}

fn default_prompt(language: &str, extra: &str) -> String {
    let lang = if language.trim().is_empty() || language == "auto" {
        "the programming language shown on the screen (use Python if none is shown)".to_string()
    } else {
        language.to_string()
    };
    let mut p = format!(
        "You are a real-time screen assistant. The attached image is a screenshot of my screen right now.\n\
         1. Find the question, problem, or task that is visible on the screen (coding problem, \
         multiple-choice or quiz question, error message, math problem, text to explain, and so on).\n\
         2. Answer it directly. Start with the answer. Do not describe the screen.\n\
         3. For a coding problem: explain the approach in 2-4 sentences, then give complete, working code in {lang}, \
         then the time and space complexity.\n\
         4. For a multiple-choice question: give the correct option first, then a one-line reason.\n\
         5. For an error message: give the cause, then the fix.\n\
         6. If nothing on the screen looks like a question, summarize the most important content in 3 bullet points.\n\
         Be concise and use Markdown. Do not use tools, do not read or write files, and do not run commands: \
         everything you need is in the image."
    );
    if !extra.trim().is_empty() {
        p.push_str("\n\nAdditional instructions:\n");
        p.push_str(extra.trim());
    }
    p
}

/// Find a CLI executable: explicit setting, then PATH, then the usual
/// per-user install folders.
pub fn resolve_exe(setting: &str, name: &str, fallbacks: &[&str]) -> String {
    if !setting.trim().is_empty() {
        return setting.trim().to_string();
    }
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(&exe);
            if p.is_file() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        for rel in fallbacks {
            let p = std::path::Path::new(&home).join(rel).join(&exe);
            if p.is_file() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    name.to_string()
}

/// Hide the console window that Windows would open for a child process.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
