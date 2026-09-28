//! Agent Client Protocol transport: one agent process spoken to over stdio,
//! newline-delimited JSON-RPC 2.0.
//!
//! [`AcpConnection::start`] spawns the agent and returns the connection plus
//! a receiver of everything the agent sends on its own (`session/update`
//! notifications and requests such as `session/request_permission` or
//! `fs/read_text_file`); requests are answered with [`AcpConnection::respond`].
//! IO runs on the shared tokio runtime ([`crate::db::runtime`]); only tokio
//! channels cross into GPUI's executor.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::{mpsc, oneshot};

/// The protocol version this client speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// JSON-RPC error codes the protocol names.
pub const AUTH_REQUIRED: i64 = -32000;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INTERNAL_ERROR: i64 = -32603;

/// How to start an agent: program, arguments and extra environment.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// An error answer from the agent.
#[derive(Clone, Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Agents often put the useful detail in `data`.
        match self.data.as_ref().and_then(detail) {
            Some(d) if !self.message.contains(&d) => write!(f, "{}: {d}", self.message),
            _ => f.write_str(&self.message),
        }
    }
}

fn detail(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o
            .get("details")
            .or_else(|| o.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// Something the agent sent without being asked.
#[derive(Debug)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The process ended; the last lines it wrote to stderr.
    Exited {
        stderr: String,
    },
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

pub struct AcpConnection {
    out: mpsc::UnboundedSender<Value>,
    pending: Pending,
    next_id: AtomicI64,
    stderr: Arc<Mutex<VecDeque<String>>>,
    // Dropping the connection kills the agent (`kill_on_drop`).
    _child: Mutex<tokio::process::Child>,
}

/// Lines of stderr kept for error messages.
const STDERR_LINES: usize = 40;

impl AcpConnection {
    pub fn start(
        cmd: &AgentCommand,
        cwd: &std::path::Path,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Incoming>)> {
        let _rt = crate::db::runtime().enter();
        let mut command = tokio::process::Command::new(&cmd.program);
        command
            .args(&cmd.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A Finder-launched app has a bare PATH: agents look up node, git, …
        command.env("PATH", crate::acp_registry::search_path());
        for (k, v) in &cmd.env {
            command.env(k, v);
        }
        let mut child = command
            .spawn()
            .map_err(|e| anyhow!("{}: {e}", cmd.program.display()))?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr_pipe = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Incoming>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let stderr = Arc::new(Mutex::new(VecDeque::new()));

        {
            let stderr = stderr.clone();
            crate::db::runtime().spawn(async move {
                let mut lines = BufReader::new(stderr_pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut buf = stderr.lock().unwrap();
                    if buf.len() == STDERR_LINES {
                        buf.pop_front();
                    }
                    buf.push_back(line);
                }
            });
        }

        // Reader: responses → pending; the rest → the UI.
        {
            let pending = pending.clone();
            let stderr = stderr.clone();
            crate::db::runtime().spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    route(msg, &pending, &in_tx);
                }
                // Let stderr drain before reporting.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                pending.lock().unwrap().clear();
                let text = stderr
                    .lock()
                    .unwrap()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = in_tx.send(Incoming::Exited { stderr: text });
            });
        }

        crate::db::runtime().spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let mut line = msg.to_string();
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
                let _ = stdin.flush().await;
            }
        });

        Ok((
            Arc::new(Self {
                out: out_tx,
                pending,
                next_id: AtomicI64::new(1),
                stderr,
                _child: Mutex::new(child),
            }),
            in_rx,
        ))
    }

    /// Send a request; resolves on the agent's answer (or fails if it exits).
    pub fn request(
        &self,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value, RpcError>> + Send + 'static {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let _ = self
            .out
            .send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        async move {
            rx.await.unwrap_or_else(|_| {
                Err(RpcError {
                    code: INTERNAL_ERROR,
                    message: "The agent exited".into(),
                    data: None,
                })
            })
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self
            .out
            .send(json!({"jsonrpc":"2.0","method":method,"params":params}));
    }

    /// Answer a request the agent sent.
    pub fn respond(&self, id: Value, result: Result<Value, (i64, String)>) {
        let msg = match result {
            Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
            Err((code, message)) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
            }
        };
        let _ = self.out.send(msg);
    }

    /// The last lines the agent wrote to stderr.
    pub fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn route(msg: Value, pending: &Pending, in_tx: &mpsc::UnboundedSender<Incoming>) {
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    match (method, msg.get("id").cloned()) {
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else { return };
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let res = match msg.get("error") {
                    Some(e) => Err(RpcError {
                        code: e
                            .get("code")
                            .and_then(Value::as_i64)
                            .unwrap_or(INTERNAL_ERROR),
                        message: e
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("error")
                            .to_string(),
                        data: e.get("data").cloned(),
                    }),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(res);
            }
        }
        (Some(method), Some(id)) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let _ = in_tx.send(Incoming::Request { id, method, params });
        }
        (Some(method), None) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let _ = in_tx.send(Incoming::Notification { method, params });
        }
        (None, None) => {}
    }
}

/// `initialize` parameters: what this client can do for the agent.
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {
            "fs": { "readTextFile": true, "writeTextFile": true },
            "terminal": true,
            "auth": { "terminal": true }
        },
        "clientInfo": {
            "name": "tusk",
            "title": "Tusk",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_responses_requests_and_notifications() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (otx, mut orx) = oneshot::channel();
        pending.lock().unwrap().insert(7, otx);
        route(
            json!({"jsonrpc":"2.0","id":7,"result":{"ok":true}}),
            &pending,
            &tx,
        );
        assert_eq!(orx.try_recv().unwrap().unwrap()["ok"], true);

        route(
            json!({"jsonrpc":"2.0","id":"a","method":"fs/read_text_file","params":{"path":"/x"}}),
            &pending,
            &tx,
        );
        match rx.try_recv().unwrap() {
            Incoming::Request { id, method, params } => {
                assert_eq!(id, json!("a"));
                assert_eq!(method, "fs/read_text_file");
                assert_eq!(params["path"], "/x");
            }
            other => panic!("{other:?}"),
        }
        route(
            json!({"jsonrpc":"2.0","method":"session/update","params":{}}),
            &pending,
            &tx,
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            Incoming::Notification { .. }
        ));

        let (otx, mut orx) = oneshot::channel();
        pending.lock().unwrap().insert(8, otx);
        route(
            json!({"jsonrpc":"2.0","id":8,"error":{"code":-32000,"message":"Authentication required"}}),
            &pending,
            &tx,
        );
        assert_eq!(orx.try_recv().unwrap().unwrap_err().code, AUTH_REQUIRED);
    }

    #[test]
    fn error_detail_is_appended() {
        let e = RpcError {
            code: -32603,
            message: "Internal error".into(),
            data: Some(json!({"details":"model not found"})),
        };
        assert_eq!(e.to_string(), "Internal error: model not found");
    }
}
