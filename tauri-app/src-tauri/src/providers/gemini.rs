//! Gemini over the REST streaming API (server-sent events).

use anyhow::{bail, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};

use super::DeltaFn;

pub async fn ask(
    api_key: &str,
    model: &str,
    prompt: &str,
    image_b64: &str,
    mime: &str,
    sink: DeltaFn,
) -> Result<String> {
    if api_key.trim().is_empty() {
        bail!("No Gemini API key. Open settings (gear icon) and paste a key from aistudio.google.com.");
    }
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse"
    );
    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [
                { "text": prompt },
                { "inline_data": { "mime_type": mime, "data": image_b64 } }
            ]
        }],
        "generationConfig": { "temperature": 0.4, "maxOutputTokens": 8192 }
    });

    let resp = reqwest::Client::new()
        .post(url)
        .header("x-goog-api-key", api_key.trim())
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or(text);
        bail!("Gemini HTTP {status}: {msg}");
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut full = String::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let Ok(v) = serde_json::from_str::<Value>(data.trim()) else { continue };
            if let Some(parts) = v.pointer("/candidates/0/content/parts").and_then(|p| p.as_array()) {
                for part in parts {
                    // Skip thought summaries; keep answer text only.
                    if part.get("thought").and_then(|t| t.as_bool()) == Some(true) {
                        continue;
                    }
                    if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                        full.push_str(t);
                        sink(t);
                    }
                }
            }
        }
    }
    if full.is_empty() {
        bail!("Gemini returned an empty answer");
    }
    Ok(full)
}
