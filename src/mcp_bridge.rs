//! Tusk's tools for AI agents, as an MCP server.
//!
//! Agents start MCP servers themselves, so the server is this binary run as
//! `tusk --mcp-bridge <socket>` (listed in `session/new`). It speaks MCP on
//! stdio and relays each tool call over a Unix socket to the running app
//! ([`Host`]), which answers from the live connection and the open tabs.
//! Nothing here runs the user's SQL: queries go into a tab for the user to
//! review and run.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot};

pub const FLAG: &str = "--mcp-bridge";
pub const SERVER_NAME: &str = "tusk";
/// The bridge's secret travels in its environment (not argv, which `ps` shows).
const TOKEN_ENV: &str = "TUSK_MCP_TOKEN";

/// The tools, as `tools/list` returns them.
pub fn tools() -> Value {
    json!([
        {
            "name": "open_sql_tab",
            "description": "Write a SQL query into a new query tab in Tusk for the user to review. \
                The query is NOT executed; the user runs it. Use this whenever you give the user SQL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "The SQL text." },
                    "title": { "type": "string", "description": "Short tab title (optional)." }
                },
                "required": ["sql"]
            }
        },
        {
            "name": "replace_active_query",
            "description": "Replace the text of the query tab the user has open (or open a new one) \
                with this SQL. Not executed.",
            "inputSchema": {
                "type": "object",
                "properties": { "sql": { "type": "string" } },
                "required": ["sql"]
            }
        },
        {
            "name": "get_active_query",
            "description": "The SQL text of the query tab the user has open, and its selection.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_context",
            "description": "The connection, database, current schema, the schemas, and what the user has open.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_tables",
            "description": "Tables, views and materialized views of a schema (default: the current one).",
            "inputSchema": {
                "type": "object",
                "properties": { "schema": { "type": "string" } }
            }
        },
        {
            "name": "describe_table",
            "description": "The CREATE statement of a table or view: columns, types, defaults, keys, \
                indexes, constraints.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "table": { "type": "string" },
                    "schema": { "type": "string" }
                },
                "required": ["table"]
            }
        }
    ])
}

/// A tool call from an agent, for the app to answer.
pub struct ToolRequest {
    pub name: String,
    pub args: Value,
    pub reply: oneshot::Sender<Result<String, String>>,
}

/// The app side: a Unix socket the bridges connect to.
pub struct Host {
    pub socket: PathBuf,
    /// Per-launch secret the bridge must send with every call.
    token: String,
}

/// 128 random bits as hex.
fn random_token() -> String {
    use std::io::Read as _;
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        // Fall back to time + pid (still unguessable enough for a 0600 socket).
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            ^ u128::from(std::process::id());
        b = n.to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Host {
    pub fn start() -> Result<(Self, mpsc::UnboundedReceiver<ToolRequest>)> {
        // A private (0700) directory, the socket itself 0600, and a secret
        // per launch: only this user's bridge started by us gets in.
        let dir = crate::acp_registry::data_dir().join("run");
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let socket = dir.join(format!("mcp-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let _rt = crate::db::runtime().enter();
        let listener = tokio::net::UnixListener::bind(&socket)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        }
        let token = random_token();
        let expected = token.clone();
        let (tx, rx) = mpsc::unbounded_channel::<ToolRequest>();
        crate::db::runtime().spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let tx = tx.clone();
                let expected = expected.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        if msg["token"].as_str() != Some(expected.as_str()) {
                            return; // not our bridge: drop the connection
                        }
                        let (reply, rx) = oneshot::channel();
                        let _ = tx.send(ToolRequest {
                            name: msg["tool"].as_str().unwrap_or_default().to_string(),
                            args: msg.get("args").cloned().unwrap_or(json!({})),
                            reply,
                        });
                        let res = rx.await.unwrap_or_else(|_| Err("Tusk closed".into()));
                        let out = match res {
                            Ok(text) => json!({"id": msg["id"], "ok": true, "text": text}),
                            Err(text) => json!({"id": msg["id"], "ok": false, "text": text}),
                        };
                        let mut s = out.to_string();
                        s.push('\n');
                        if write.write_all(s.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Ok((Self { socket, token }, rx))
    }

    /// The MCP server entry for `session/new`.
    pub fn server_spec(&self) -> Value {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tusk"));
        json!({
            "name": SERVER_NAME,
            "command": exe.display().to_string(),
            "args": [FLAG, self.socket.display().to_string()],
            "env": [{ "name": TOKEN_ENV, "value": self.token }]
        })
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// `tusk --mcp-bridge <socket>`: an MCP stdio server relaying to the app.
pub fn run(socket: &Path) {
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut conn: Option<(
        std::os::unix::net::UnixStream,
        BufReader<std::os::unix::net::UnixStream>,
    )> = None;
    let mut next = 0u64;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // notifications (initialized, cancelled)
        };
        let method = msg["method"].as_str().unwrap_or_default();
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "title": "Tusk", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Tools of Tusk, the PostgreSQL client the user is working in. \
                    Inspect the database with get_context / list_tables / describe_table. \
                    Put SQL for the user into a tab with open_sql_tab; never run it yourself."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                next += 1;
                let name = msg["params"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
                let (ok, text) = call(&mut conn, socket, &token, next, &name, args);
                Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": !ok }))
            }
            _ => Err((-32601, format!("unknown method {method}"))),
        };
        let out = match result {
            Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
            Err((code, m)) => json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":m}}),
        };
        if writeln!(stdout, "{out}").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

fn call(
    conn: &mut Option<(
        std::os::unix::net::UnixStream,
        BufReader<std::os::unix::net::UnixStream>,
    )>,
    socket: &Path,
    token: &str,
    id: u64,
    name: &str,
    args: Value,
) -> (bool, String) {
    // One reconnect: the app may have restarted its host.
    for _ in 0..2 {
        if conn.is_none() {
            match std::os::unix::net::UnixStream::connect(socket) {
                Ok(s) => match s.try_clone() {
                    Ok(r) => *conn = Some((s, BufReader::new(r))),
                    Err(e) => return (false, e.to_string()),
                },
                Err(_) => return (false, "Tusk is not running.".into()),
            }
        }
        let Some((w, r)) = conn.as_mut() else {
            continue;
        };
        let req = json!({"id": id, "token": token, "tool": name, "args": args});
        if writeln!(w, "{req}").is_err() {
            *conn = None;
            continue;
        }
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(n) if n > 0 => {
                let v: Value = serde_json::from_str(&line).unwrap_or(json!({}));
                return (
                    v["ok"].as_bool().unwrap_or(false),
                    v["text"].as_str().unwrap_or_default().to_string(),
                );
            }
            _ => *conn = None,
        }
    }
    (false, "Lost the connection to Tusk.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_is_well_formed() {
        let t = tools();
        let names: Vec<&str> = t
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"open_sql_tab"));
        for tool in t.as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }
}
