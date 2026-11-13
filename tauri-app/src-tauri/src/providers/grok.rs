//! Grok CLI over ACP (Agent Client Protocol): one persistent
//! `grok agent stdio` process.
//!
//! Why ACP and not `grok -p`:
//! - the process and the session stay warm (about 4 s to the first answer
//!   instead of about 15 s), and follow-up questions keep their context;
//! - the screenshot goes over stdin, so its size is not limited by the
//!   Windows command line (32K characters).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::Mutex;

use super::rpc::{Handler, Rpc};
use super::DeltaFn;

type Sink = Arc<StdMutex<Option<DeltaFn>>>;

struct Inner {
    rpc: Rpc,
    sink: Sink,
    busy: AtomicBool,
    session: Mutex<Option<String>>,
    cwd: PathBuf,
    model: String,
    effort: String,
}

#[derive(Clone)]
pub struct GrokAcp {
    inner: Arc<Inner>,
}

impl GrokAcp {
    pub async fn start(exe: &str, model: &str, effort: &str, cwd: &Path) -> Result<Self> {
        let sink: Sink = Default::default();
        let handler: Handler = {
            let sink = sink.clone();
            Arc::new(move |method, params, is_request| {
                if method == "session/update" {
                    let u = &params["update"];
                    if u["sessionUpdate"] == "agent_message_chunk" {
                        if let Some(t) = u.pointer("/content/text").and_then(|t| t.as_str()) {
                            if let Some(s) = sink.lock().unwrap().as_ref() {
                                s(t);
                            }
                        }
                    }
                    return None;
                }
                // Deny every tool permission: this app only wants a text answer
                // about the screenshot. Grok's own `_x.ai/...` requests get the
                // same neutral reply.
                if is_request && (method == "session/request_permission" || method.starts_with('_')) {
                    return Some(json!({ "outcome": { "outcome": "cancelled" } }));
                }
                None
            })
        };

        let mut cmd = Command::new(exe);
        cmd.args(["agent", "stdio"]).current_dir(cwd);
        let rpc = Rpc::spawn(cmd, &format!("Grok CLI ({exe} agent stdio)"), handler)?;

        let acp = GrokAcp {
            inner: Arc::new(Inner {
                rpc,
                sink,
                busy: AtomicBool::new(false),
                session: Mutex::new(None),
                cwd: cwd.to_path_buf(),
                model: model.trim().to_string(),
                effort: effort.trim().to_string(),
            }),
        };
        acp.inner
            .rpc
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": { "readTextFile": false, "writeTextFile": false },
                        "terminal": false
                    }
                }),
            )
            .await?;
        // Open the session now, so the first question does not wait for it.
        acp.session_id().await?;
        Ok(acp)
    }

    pub fn is_alive(&self) -> bool {
        self.inner.rpc.is_alive()
    }

    async fn session_id(&self) -> Result<String> {
        let mut guard = self.inner.session.lock().await;
        if let Some(id) = guard.as_ref() {
            return Ok(id.clone());
        }
        let r = self
            .inner
            .rpc
            .request("session/new", json!({ "cwd": self.inner.cwd, "mcpServers": [] }))
            .await?;
        let id = r["sessionId"].as_str().context("Grok: no sessionId")?.to_string();
        // Best effort: an unknown model or effort value must not block answers.
        for (key, value) in [("model", &self.inner.model), ("reasoning_effort", &self.inner.effort)] {
            if !value.is_empty() {
                let _ = self
                    .inner
                    .rpc
                    .request(
                        "session/set_config_option",
                        json!({ "sessionId": id, "configId": key, "value": value }),
                    )
                    .await;
            }
        }
        *guard = Some(id.clone());
        Ok(id)
    }

    /// Forget the conversation. The next question opens a new session.
    pub async fn new_session(&self) {
        self.cancel().await;
        *self.inner.session.lock().await = None;
    }

    /// Stop the answer that is running now, if any.
    pub async fn cancel(&self) {
        *self.inner.sink.lock().unwrap() = None;
        if self.inner.busy.swap(false, Ordering::SeqCst) {
            if let Some(id) = self.inner.session.lock().await.clone() {
                self.inner.rpc.notify("session/cancel", json!({ "sessionId": id }));
            }
        }
    }

    pub async fn ask(&self, prompt: &str, image_b64: &str, mime: &str, sink: DeltaFn) -> Result<String> {
        let sid = self.session_id().await?;

        let full = Arc::new(StdMutex::new(String::new()));
        {
            let full = full.clone();
            let wrapped: DeltaFn = Arc::new(move |d: &str| {
                full.lock().unwrap().push_str(d);
                sink(d);
            });
            *self.inner.sink.lock().unwrap() = Some(wrapped);
        }
        self.inner.busy.store(true, Ordering::SeqCst);

        let result = self
            .inner
            .rpc
            .request(
                "session/prompt",
                json!({
                    "sessionId": sid,
                    "prompt": [
                        { "type": "text", "text": prompt },
                        { "type": "image", "mimeType": mime, "data": image_b64 }
                    ]
                }),
            )
            .await;

        self.inner.busy.store(false, Ordering::SeqCst);
        *self.inner.sink.lock().unwrap() = None;

        let r: Value = result.map_err(|e| e.context("Grok"))?;
        if r["stopReason"] == "cancelled" {
            bail!("cancelled");
        }
        let text = full.lock().unwrap().clone();
        if text.trim().is_empty() {
            bail!("Grok returned an empty answer (stop reason: {})", r["stopReason"]);
        }
        Ok(text)
    }
}
