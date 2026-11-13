//! JSON-RPC 2.0 over a child process's stdin/stdout, one JSON message per
//! line. Used by both persistent CLI providers:
//! - `grok agent stdio`  (ACP, Agent Client Protocol)
//! - `codex app-server`  (Codex app-server protocol)

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Notify};

type Pending = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// Called for every message from the server that is not a response.
/// `is_request` is true when the server expects an answer. Return
/// `Some(result)` to answer it; `None` answers "method not supported".
pub type Handler = Arc<dyn Fn(&str, &Value, bool) -> Option<Value> + Send + Sync>;

struct Inner {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    /// Woken when the process exits.
    exited: Arc<Notify>,
    // Dropping the last `Rpc` handle kills the process (kill_on_drop).
    _child: StdMutex<Child>,
}

#[derive(Clone)]
pub struct Rpc {
    inner: Arc<Inner>,
}

impl Rpc {
    pub fn spawn(mut cmd: Command, what: &str, handler: Handler) -> Result<Self> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(super::CREATE_NO_WINDOW);
        let mut child = cmd.spawn().with_context(|| format!("cannot start {what}"))?;
        let mut stdin = child.stdin.take().context("child stdin")?;
        let stdout = child.stdout.take().context("child stdout")?;

        // Writer task: the only owner of stdin.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let pending: Pending = Default::default();
        let alive = Arc::new(AtomicBool::new(true));
        let exited = Arc::new(Notify::new());
        tokio::spawn(read_loop(
            stdout,
            tx.clone(),
            pending.clone(),
            handler,
            alive.clone(),
            exited.clone(),
        ));

        Ok(Rpc {
            inner: Arc::new(Inner {
                tx,
                pending,
                next_id: AtomicU64::new(0),
                alive,
                exited,
                _child: StdMutex::new(child),
            }),
        })
    }

    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::SeqCst)
    }

    pub fn exited(&self) -> Arc<Notify> {
        self.inner.exited.clone()
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.inner
            .tx
            .send(msg.to_string())
            .map_err(|_| anyhow!("the CLI process is not running"))?;
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => bail!("{method}: {e}"),
            Err(_) => bail!("the CLI process closed the connection"),
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = self.inner.tx.send(msg.to_string());
    }
}

async fn read_loop(
    stdout: tokio::process::ChildStdout,
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    handler: Handler,
    alive: Arc<AtomicBool>,
    exited: Arc<Notify>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let method = msg.get("method").and_then(|m| m.as_str());
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match (method, id) {
            // Response to one of our requests.
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { continue };
                if let Some(waiter) = pending.lock().unwrap().remove(&id) {
                    let r = match msg.get("error") {
                        Some(e) => Err(e
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown error")
                            .to_string()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = waiter.send(r);
                }
            }
            // Request from the server: it waits for our answer.
            (Some(m), Some(id)) => {
                let reply = match handler(m, &params, true) {
                    Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    None => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32601, "message": "not supported by this client" }
                    }),
                };
                let _ = tx.send(reply.to_string());
            }
            // Notification.
            (Some(m), None) => {
                handler(m, &params, false);
            }
            _ => {}
        }
    }
    alive.store(false, Ordering::SeqCst);
    for (_, waiter) in pending.lock().unwrap().drain() {
        let _ = waiter.send(Err("the CLI process exited".into()));
    }
    exited.notify_waiters();
}
