//! Codex CLI over the app-server protocol: one persistent
//! `codex app-server` process (JSON-RPC over stdio).
//!
//! Compared with one `codex exec` per question (about 23 s, no streaming),
//! the warm process streams the answer (`item/agentMessage/delta`), and
//! follow-up questions reuse the same thread, so they keep their context.
//!
//! Safety: the thread runs with `sandbox: read-only` and
//! `approvalPolicy: never`, in an empty working folder, with the user's MCP
//! servers turned off. Any approval request that still arrives is declined.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{bail, Context, Result};
use serde_json::json;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};

use super::rpc::{Handler, Rpc};
use super::DeltaFn;

const DEVELOPER_INSTRUCTIONS: &str = "You are a real-time screen assistant inside a desktop overlay. \
The user sends screenshots of their screen. Answer from the image only. \
Never run commands, never read or write files, and never call tools.";

/// Turn results: turn id -> Ok(()) or an error message.
type Completed = Arc<StdMutex<HashMap<String, Result<(), String>>>>;

#[derive(Default)]
struct Routing {
    sink: Option<DeltaFn>,
    /// When known, only deltas of this turn reach the sink.
    turn: Option<String>,
    /// Deltas of these turns are dropped (cancelled answers).
    ignored: HashSet<String>,
    last_item: Option<String>,
}

struct Inner {
    rpc: Rpc,
    routing: Arc<StdMutex<Routing>>,
    completed: Completed,
    turn_done: Arc<Notify>,
    thread: Mutex<Option<String>>,
    active_turn: StdMutex<Option<String>>,
    cwd: PathBuf,
    model: String,
    effort: String,
}

#[derive(Clone)]
pub struct CodexApp {
    inner: Arc<Inner>,
}

impl CodexApp {
    pub async fn start(exe: &str, model: &str, effort: &str, cwd: &Path) -> Result<Self> {
        let routing: Arc<StdMutex<Routing>> = Default::default();
        let completed: Completed = Default::default();
        let turn_done = Arc::new(Notify::new());

        let handler: Handler = {
            let routing = routing.clone();
            let completed = completed.clone();
            let turn_done = turn_done.clone();
            Arc::new(move |method, params, is_request| {
                match method {
                    "item/agentMessage/delta" => {
                        let mut r = routing.lock().unwrap();
                        let turn = params["turnId"].as_str().unwrap_or_default();
                        if r.ignored.contains(turn) || r.turn.as_deref().is_some_and(|t| t != turn) {
                            return None;
                        }
                        let item = params["itemId"].as_str().map(String::from);
                        let delta = params["delta"].as_str().unwrap_or_default();
                        // A turn can hold several agent messages: keep them apart.
                        let sep = r.last_item.is_some() && r.last_item != item;
                        r.last_item = item;
                        if let Some(s) = r.sink.as_ref() {
                            if sep {
                                s("\n\n");
                            }
                            s(delta);
                        }
                    }
                    "turn/completed" => {
                        let turn = &params["turn"];
                        let id = turn["id"].as_str().unwrap_or_default().to_string();
                        let result = match turn["status"].as_str() {
                            Some("completed") => Ok(()),
                            Some("interrupted") => Err("cancelled".to_string()),
                            _ => Err(turn
                                .pointer("/error/message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("the turn failed")
                                .to_string()),
                        };
                        completed.lock().unwrap().insert(id, result);
                        turn_done.notify_waiters();
                    }
                    _ => {}
                }
                if !is_request {
                    return None;
                }
                // Decline everything the agent asks to do.
                match method {
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                        Some(json!({ "decision": "decline" }))
                    }
                    "execCommandApproval" | "applyPatchApproval" => Some(json!({ "decision": "abort" })),
                    "mcpServer/elicitation/request" => {
                        Some(json!({ "action": "decline", "content": null, "_meta": null }))
                    }
                    _ => None,
                }
            })
        };

        let mut cmd = Command::new(exe);
        cmd.arg("app-server").current_dir(cwd);
        for name in user_mcp_servers() {
            cmd.arg("-c").arg(format!("mcp_servers.{name}.enabled=false"));
        }
        let rpc = Rpc::spawn(cmd, &format!("Codex CLI ({exe} app-server)"), handler)?;

        let app = CodexApp {
            inner: Arc::new(Inner {
                rpc,
                routing,
                completed,
                turn_done,
                thread: Mutex::new(None),
                active_turn: StdMutex::new(None),
                cwd: cwd.to_path_buf(),
                model: model.trim().to_string(),
                effort: effort.trim().to_string(),
            }),
        };
        app.inner
            .rpc
            .request(
                "initialize",
                json!({
                    "clientInfo": { "name": "opencluely", "title": "OpenCluely", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": null
                }),
            )
            .await?;
        app.inner.rpc.notify("initialized", json!({}));
        // Open the thread now, so the first question does not wait for it.
        app.thread_id().await?;
        Ok(app)
    }

    pub fn is_alive(&self) -> bool {
        self.inner.rpc.is_alive()
    }

    async fn thread_id(&self) -> Result<String> {
        let mut guard = self.inner.thread.lock().await;
        if let Some(id) = guard.as_ref() {
            return Ok(id.clone());
        }
        let mut params = json!({
            "cwd": self.inner.cwd,
            "approvalPolicy": "never",
            "sandbox": "read-only",
            "ephemeral": true,
            "developerInstructions": DEVELOPER_INSTRUCTIONS,
        });
        if !self.inner.model.is_empty() {
            params["model"] = json!(self.inner.model);
        }
        let r = self.inner.rpc.request("thread/start", params).await?;
        let id = r.pointer("/thread/id").and_then(|v| v.as_str()).context("Codex: no thread id")?;
        *guard = Some(id.to_string());
        Ok(id.to_string())
    }

    /// Forget the conversation. The next question opens a new thread.
    pub async fn new_session(&self) {
        self.cancel().await;
        *self.inner.thread.lock().await = None;
    }

    /// Stop the answer that is running now, if any.
    pub async fn cancel(&self) {
        let turn = self.inner.active_turn.lock().unwrap().take();
        {
            let mut r = self.inner.routing.lock().unwrap();
            r.sink = None;
            if let Some(t) = &turn {
                r.ignored.insert(t.clone());
            }
        }
        if let (Some(turn), Some(thread)) = (turn, self.inner.thread.lock().await.clone()) {
            let rpc = self.inner.rpc.clone();
            tokio::spawn(async move {
                let _ = rpc
                    .request("turn/interrupt", json!({ "threadId": thread, "turnId": turn }))
                    .await;
            });
        }
    }

    pub async fn ask(&self, prompt: &str, image_b64: &str, mime: &str, sink: DeltaFn) -> Result<String> {
        let thread = self.thread_id().await?;

        let full = Arc::new(StdMutex::new(String::new()));
        {
            let full = full.clone();
            let wrapped: DeltaFn = Arc::new(move |d: &str| {
                full.lock().unwrap().push_str(d);
                sink(d);
            });
            let mut r = self.inner.routing.lock().unwrap();
            r.sink = Some(wrapped);
            r.turn = None;
            r.last_item = None;
        }

        let mut params = json!({
            "threadId": thread,
            "input": [
                { "type": "text", "text": prompt, "text_elements": [] },
                { "type": "image", "url": format!("data:{mime};base64,{image_b64}") }
            ]
        });
        if !self.inner.effort.is_empty() {
            params["effort"] = json!(self.inner.effort);
        }
        let r = self.inner.rpc.request("turn/start", params).await.map_err(|e| e.context("Codex"))?;
        let turn = r.pointer("/turn/id").and_then(|v| v.as_str()).context("Codex: no turn id")?.to_string();
        *self.inner.active_turn.lock().unwrap() = Some(turn.clone());
        self.inner.routing.lock().unwrap().turn = Some(turn.clone());

        // Wait for `turn/completed` of this turn (or for the process to exit).
        let exited = self.inner.rpc.exited();
        let outcome = loop {
            let done = self.inner.turn_done.notified();
            let gone = exited.notified();
            tokio::pin!(done, gone);
            done.as_mut().enable();
            gone.as_mut().enable();
            if let Some(r) = self.inner.completed.lock().unwrap().remove(&turn) {
                break r;
            }
            if !self.is_alive() {
                break Err("Codex CLI exited".to_string());
            }
            tokio::select! { _ = done => {}, _ = gone => {} }
        };

        {
            let mut a = self.inner.active_turn.lock().unwrap();
            if a.as_deref() == Some(turn.as_str()) {
                *a = None;
            }
        }
        self.inner.routing.lock().unwrap().sink = None;

        if let Err(e) = outcome {
            bail!(if e == "cancelled" { e } else { format!("Codex: {e}") });
        }
        let text = full.lock().unwrap().clone();
        if text.trim().is_empty() {
            bail!("Codex returned an empty answer");
        }
        Ok(text)
    }
}

/// Names of the MCP servers in ~/.codex/config.toml (`[mcp_servers.NAME]`).
/// They are turned off for this app: they add start-up time and tools that
/// a screenshot answer does not need.
fn user_mcp_servers() -> Vec<String> {
    let home = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(|h| PathBuf::from(h).join(".codex"))
    });
    let Some(text) = home.and_then(|h| std::fs::read_to_string(h.join("config.toml")).ok()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("[mcp_servers.")?.strip_suffix(']').map(str::to_string))
        // Only plain names; skip sub-tables such as [mcp_servers.x.env].
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .collect();
    names.dedup();
    names
}
