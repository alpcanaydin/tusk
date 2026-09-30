//! SQL intelligence from a bundled language server: Supabase's
//! `postgres-language-server lsp-proxy` for Postgres, `sqls` for the
//! other SQL engines it knows (MySQL / MariaDB, Postgres-protocol engines,
//! SQLite, SQL Server, Vertica, ClickHouse).
//!
//! One server per connection, spoken to over stdio JSON-RPC (LSP framing).
//! The database settings travel in `initializationOptions` (+ the
//! configuration messages the server reads), so the password never touches
//! disk — no generated config file.
//!
//! Each SQL tab is one document (`file://<cache>/tusk-lsp/sql-N.sql`); the
//! editor's [`CompletionProvider`] syncs the full text and asks for
//! completions, and `publishDiagnostics` flows back to the editor as
//! squiggles through the receiver returned by [`LspClient::start`].
//!
//! IO runs on the shared tokio runtime ([`crate::db::runtime`]); only
//! runtime-agnostic tokio channels cross into GPUI's executor.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use gpui_kit::component::input::{CompletionProvider, Rope, RopeExt as _};
use gpui_kit::{App, AppContext as _, Task, Window};
use lsp_types::{CompletionContext, CompletionResponse};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Which server to run and what it's told about the database.
#[derive(Clone)]
pub struct ServerSpec {
    pub binary: &'static str,
    pub args: &'static [&'static str],
    pub init_options: Value,
    /// Answer to `workspace/configuration`.
    pub configuration: Value,
    /// Sent as `workspace/didChangeConfiguration` after `initialized`.
    pub settings: Option<Value>,
}

impl ServerSpec {
    pub fn pgls(db: &DbSettings) -> Self {
        Self {
            binary: "postgres-language-server",
            args: &["lsp-proxy"],
            init_options: json!({ "db": db.to_json() }),
            configuration: json!({ "db": db.to_json() }),
            settings: Some(json!({ "db": db.to_json() })),
        }
    }

    /// `connection` is sqls' connection config (`driver`, `host`, … or
    /// `dataSourceName`).
    pub fn sqls(connection: Value) -> Self {
        Self {
            binary: "sqls",
            args: &[],
            init_options: json!({ "connectionConfig": connection }),
            configuration: Value::Null,
            settings: None,
        }
    }
}

/// sqls' connection config for `conn` (reached at `host:port`), or `None`
/// for engines it doesn't speak (their editors use schema completion).
pub fn sqls_connection(
    conn: &crate::db::SavedConnection,
    host: &str,
    port: u16,
    password: &str,
) -> Option<Value> {
    use crate::engine::Engine as E;
    let ssl = match conn.ssl {
        crate::db::SslMode::Disable => "disable",
        crate::db::SslMode::Prefer => "prefer",
        crate::db::SslMode::Require => "require",
    };
    let server = |driver: &str| {
        json!({
            "driver": driver, "proto": "tcp", "host": host, "port": port,
            "user": conn.user, "passwd": password, "dbName": conn.database,
        })
    };
    Some(match conn.engine {
        E::MySql | E::MariaDb => server("mysql"),
        E::Cockroach | E::Redshift => {
            let mut c = server("postgresql");
            c["params"] = json!({ "sslmode": ssl });
            c
        }
        E::Vertica => server("vertica"),
        E::MsSql => {
            let mut c = server("mssql");
            c["params"] = match conn.ssl {
                crate::db::SslMode::Disable => json!({ "encrypt": "disable" }),
                crate::db::SslMode::Prefer => {
                    json!({ "encrypt": "true", "TrustServerCertificate": "true" })
                }
                crate::db::SslMode::Require => json!({ "encrypt": "true" }),
            };
            c
        }
        E::Sqlite => json!({
            "driver": "sqlite3",
            "dataSourceName": crate::drivers::sqlite::shellexpand(conn.path.as_deref().unwrap_or("")),
        }),
        // Over HTTP like the driver (sqls needs a DSN for it).
        E::ClickHouse => {
            let scheme = if conn.ssl == crate::db::SslMode::Require || port == 8443 {
                "https"
            } else {
                "http"
            };
            let enc = |s: &str| {
                s.bytes()
                    .map(|b| match b {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                            (b as char).to_string()
                        }
                        _ => format!("%{b:02X}"),
                    })
                    .collect::<String>()
            };
            let user = if conn.user.is_empty() {
                "default"
            } else {
                conn.user.as_str()
            };
            let db = if conn.database.is_empty() {
                "default"
            } else {
                conn.database.as_str()
            };
            json!({
                "driver": "clickhouse",
                "dataSourceName": format!("{scheme}://{}:{}@{host}:{port}/{}", enc(user), enc(password), enc(db)),
            })
        }
        _ => return None,
    })
}

/// Connection the language server introspects for completions.
#[derive(Clone)]
pub struct DbSettings {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: String,
}

impl DbSettings {
    fn to_json(&self) -> Value {
        json!({
            "host": self.host,
            "port": self.port,
            "username": self.username,
            "password": self.password,
            "database": self.database,
        })
    }
}

/// `(document uri, diagnostics)` as published by the server.
pub type DiagnosticsEvent = (String, Vec<lsp_types::Diagnostic>);

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

pub struct LspClient {
    out: mpsc::UnboundedSender<Value>,
    pending: Pending,
    next_id: AtomicI64,
    next_doc: AtomicI32,
    root: PathBuf,
    // Dropping the client kills the server (`kill_on_drop`).
    _child: Mutex<tokio::process::Child>,
}

/// A server binary from PATH or the usual install locations — a
/// Finder-launched app doesn't inherit the shell PATH (pnpm/npm/brew).
fn find_binary(binary: &str) -> Option<PathBuf> {
    let env = format!("TUSK_{}", if binary == "sqls" { "SQLS" } else { "PGLS" });
    if let Ok(p) = std::env::var(env) {
        return Some(PathBuf::from(p));
    }
    // Shipped inside Tusk.app first (scripts/bundle-*.sh), then the
    // dev cache next to the binary, then a user install.
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(macos) = exe.parent()
    {
        if let Some(contents) = macos.parent() {
            dirs.push(contents.join("Resources/bin"));
        }
        dirs.push(macos.join("pgls"));
    }
    dirs.extend(
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    if let Some(home) = dirs::home_dir() {
        for d in [
            "Library/pnpm/bin",
            ".local/bin",
            ".cargo/bin",
            ".npm-global/bin",
        ] {
            dirs.push(home.join(d));
        }
    }
    dirs.push("/opt/homebrew/bin".into());
    dirs.push("/usr/local/bin".into());
    dirs.into_iter()
        .map(|d| d.join(format!("{binary}{}", std::env::consts::EXE_SUFFIX)))
        .find(|p| p.is_file())
}

fn frame(msg: &Value) -> Vec<u8> {
    let body = msg.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

impl LspClient {
    /// Spawn and initialise the server. Returns immediately; requests queue
    /// until the `initialize` handshake completes.
    pub fn start(
        spec: ServerSpec,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<DiagnosticsEvent>)> {
        let name = spec.binary;
        let bin = find_binary(name).ok_or_else(|| {
            let env = if name == "sqls" {
                "TUSK_SQLS"
            } else {
                "TUSK_PGLS"
            };
            anyhow!("{name} not found; install it on PATH or set {env}")
        })?;
        let root = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("tusk-lsp");
        std::fs::create_dir_all(&root)?;

        let _rt = crate::db::runtime().enter();
        let mut child = tokio::process::Command::new(bin)
            .args(spec.args)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
        let (diag_tx, diag_rx) = mpsc::unbounded_channel::<DiagnosticsEvent>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        // Reader: responses → pending, server requests → answered,
        // publishDiagnostics → UI channel.
        {
            let pending = pending.clone();
            let out_tx = out_tx.clone();
            let configuration = spec.configuration.clone();
            crate::db::runtime().spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut len = None;
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => {
                                pending.lock().unwrap().clear();
                                return;
                            }
                            Ok(_) => {}
                        }
                        let line = line.trim_end();
                        if line.is_empty() {
                            break;
                        }
                        if let Some(v) = line.strip_prefix("Content-Length:") {
                            len = v.trim().parse::<usize>().ok();
                        }
                    }
                    let Some(len) = len else { continue };
                    let mut body = vec![0; len];
                    if reader.read_exact(&mut body).await.is_err() {
                        pending.lock().unwrap().clear();
                        return;
                    }
                    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    let method = msg.get("method").and_then(Value::as_str);
                    match (method, msg.get("id")) {
                        (None, Some(id)) => {
                            let Some(id) = id.as_i64() else { continue };
                            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                                let res = match msg.get("error") {
                                    Some(e) => Err(e.to_string()),
                                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                                };
                                let _ = tx.send(res);
                            }
                        }
                        (Some(method), Some(id)) => {
                            // Server → client request: answer so it never stalls.
                            let result = if method == "workspace/configuration" {
                                let n = msg["params"]["items"].as_array().map_or(1, Vec::len);
                                Value::Array(vec![configuration.clone(); n])
                            } else {
                                Value::Null
                            };
                            let _ = out_tx.send(json!({"jsonrpc":"2.0","id":id,"result":result}));
                        }
                        (Some("textDocument/publishDiagnostics"), None) => {
                            let uri = msg["params"]["uri"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string();
                            let diags =
                                serde_json::from_value(msg["params"]["diagnostics"].clone())
                                    .unwrap_or_default();
                            let _ = diag_tx.send((uri, diags));
                        }
                        _ => {}
                    }
                }
            });
        }

        // Writer: handshake first, then drain the queue.
        {
            let pending = pending.clone();
            let root_uri = format!("file://{}", root.display());
            crate::db::runtime().spawn(async move {
                let (tx, rx) = oneshot::channel();
                pending.lock().unwrap().insert(0, tx);
                let init = json!({
                    "jsonrpc": "2.0", "id": 0, "method": "initialize",
                    "params": {
                        "processId": std::process::id(),
                        "rootUri": root_uri,
                        "capabilities": {
                            "workspace": { "configuration": true },
                            "textDocument": {
                                "completion": { "completionItem": { "snippetSupport": false } },
                                "publishDiagnostics": {}
                            }
                        },
                        "initializationOptions": spec.init_options
                    }
                });
                if stdin.write_all(&frame(&init)).await.is_err() || rx.await.is_err() {
                    return;
                }
                let mut first = vec![json!({"jsonrpc":"2.0","method":"initialized","params":{}})];
                if let Some(settings) = spec.settings {
                    first.push(
                        json!({"jsonrpc":"2.0","method":"workspace/didChangeConfiguration",
                                      "params":{"settings": settings}}),
                    );
                }
                for msg in first {
                    if stdin.write_all(&frame(&msg)).await.is_err() {
                        return;
                    }
                }
                let _ = stdin.flush().await;
                while let Some(msg) = out_rx.recv().await {
                    if stdin.write_all(&frame(&msg)).await.is_err() {
                        return;
                    }
                    let _ = stdin.flush().await;
                }
            });
        }

        Ok((
            Arc::new(Self {
                out: out_tx,
                pending,
                next_id: AtomicI64::new(1),
                next_doc: AtomicI32::new(1),
                root,
                _child: Mutex::new(child),
            }),
            diag_rx,
        ))
    }

    fn notify(&self, method: &str, params: Value) {
        let _ = self
            .out
            .send(json!({"jsonrpc":"2.0","method":method,"params":params}));
    }

    /// Send a request; the future resolves on the server's answer (or fails if
    /// the server goes away).
    pub fn request(
        &self,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let _ = self
            .out
            .send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        async move {
            rx.await
                .map_err(|_| anyhow!("language server exited"))?
                .map_err(|e| anyhow!(e))
        }
    }

    /// Open a new SQL document; returns its URI.
    pub fn open_document(&self, text: &str) -> String {
        let n = self.next_doc.fetch_add(1, Ordering::Relaxed);
        let uri = format!("file://{}/sql-{n}.sql", self.root.display());
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri,"languageId":"sql","version":1,"text":text}}),
        );
        uri
    }

    /// Full-text sync (a range-less change is valid in incremental mode).
    pub fn change_document(&self, uri: &str, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":text}]}),
        );
    }

    pub fn close_document(&self, uri: &str) {
        self.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}));
    }
}

/// One SQL tab's link to the server: its document URI + version counter.
#[derive(Clone)]
pub struct SqlDocument {
    pub client: Arc<LspClient>,
    pub uri: String,
    version: Arc<AtomicI32>,
    /// Hash of the text sent last: the editor's change sync and the
    /// completion request often carry the same text — send it once.
    sent: Arc<AtomicI64>,
}

impl SqlDocument {
    pub fn open(client: Arc<LspClient>, text: &str) -> Self {
        let uri = client.open_document(text);
        Self {
            client,
            uri,
            version: Arc::new(AtomicI32::new(1)),
            sent: Arc::new(AtomicI64::new(text_hash(text))),
        }
    }

    pub fn sync(&self, text: &str) {
        let h = text_hash(text);
        if self.sent.swap(h, Ordering::Relaxed) == h {
            return;
        }
        let v = self.version.fetch_add(1, Ordering::Relaxed) + 1;
        self.client.change_document(&self.uri, v, text);
    }
}

fn text_hash(text: &str) -> i64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish() as i64
}

impl Drop for SqlDocument {
    fn drop(&mut self) {
        // Only the last clone (the tab itself, not the provider) closes.
        if Arc::strong_count(&self.version) == 1 {
            self.client.close_document(&self.uri);
        }
    }
}

/// The identifier being typed right before `offset` (what the menu matched).
pub fn word_prefix(text: &str, offset: usize) -> String {
    let before = &text[..offset.min(text.len())];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
        .map_or(0, |(i, c)| i + c.len_utf8());
    before[start..].to_string()
}

/// Kit menu tweaks: highlight exactly the typed prefix (it highlights
/// `filter_text.len()` chars) and show `Table · public` from `labelDetails`.
fn decorate(item: &mut lsp_types::CompletionItem, prefix: &str) {
    // Settings ▸ SQL Editor ▸ Uppercase Keywords.
    if item.kind == Some(lsp_types::CompletionItemKind::KEYWORD)
        && crate::settings::get().editor_uppercase_keywords
    {
        item.label = item.label.to_uppercase();
        if let Some(t) = &mut item.insert_text {
            *t = t.to_uppercase();
        }
        if let Some(lsp_types::CompletionTextEdit::Edit(e)) = &mut item.text_edit {
            e.new_text = e.new_text.to_uppercase();
        }
    }
    // sqls documents an item with its label as a markdown heading: the
    // menu row says as much already.
    if let Some(lsp_types::Documentation::MarkupContent(m)) = &item.documentation
        && m.value.trim_start().starts_with('#')
        && m.value.lines().filter(|l| !l.trim().is_empty()).count() <= 1
    {
        item.documentation = None;
    }
    let matches = item
        .label
        .to_lowercase()
        .starts_with(&prefix.to_lowercase());
    item.filter_text = Some(if matches {
        prefix.to_string()
    } else {
        String::new()
    });
    // The server reports tables, views and matviews all as `Class`; tell
    // views apart (menu glyph "V") from the label detail.
    if item.kind == Some(lsp_types::CompletionItemKind::CLASS)
        && item
            .label_details
            .as_ref()
            .and_then(|d| d.detail.as_deref())
            .is_some_and(|d| d.contains("View"))
    {
        item.kind = Some(lsp_types::CompletionItemKind::INTERFACE);
    }
    if item.detail.is_none()
        && let Some(d) = &item.label_details
    {
        let kind = d.detail.as_deref().unwrap_or("").trim();
        // An empty description ("Keyword" with no schema) mustn't leave a
        // dangling "Keyword ·".
        let detail = match d
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(desc) if !kind.is_empty() => format!("{kind} · {desc}"),
            Some(desc) => desc.to_string(),
            None => kind.to_string(),
        };
        if !detail.is_empty() {
            item.detail = Some(detail);
        }
    }
}

/// Word characters that keep (re)querying completions while typing, plus
/// `.` for `schema.` / `alias.` member completion.
fn is_trigger_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '"'
}

impl CompletionProvider for SqlDocument {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let full = text.to_string();
        self.sync(&full);
        let pos = text.offset_to_position(offset);
        let prefix = word_prefix(&full, offset);
        let fut = self.client.request(
            "textDocument/completion",
            json!({
                "textDocument": {"uri": self.uri},
                "position": {"line": pos.line, "character": pos.character}
            }),
        );
        cx.background_spawn(async move {
            let v = fut.await?;
            if v.is_null() {
                return Ok(CompletionResponse::Array(Vec::new()));
            }
            let mut items = match serde_json::from_value(v)? {
                CompletionResponse::Array(items) => items,
                CompletionResponse::List(list) => list.items,
            };
            for item in &mut items {
                decorate(item, &prefix);
            }
            Ok(CompletionResponse::Array(items))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text.chars().last().is_some_and(is_trigger_char)
    }
}

#[cfg(test)]
mod tests {
    use super::{decorate, frame, is_trigger_char, word_prefix};

    /// An accepted completion replaces only the word being typed (the kit
    /// used to reuse the first completion's start, so Enter replaced the
    /// line from there), and that word is the prefix the menu matched.
    /// Snippet completions insert plain text with the caret on the first
    /// tab stop (raw `${1:}` used to land in the editor).
    #[test]
    fn snippets_expand_to_plain_text() {
        use gpui_kit::base::input::expand_snippet;
        let (t, r) = expand_snippet("pg_catalog.network_supeq(${1:}, ${2:})");
        assert_eq!(t, "pg_catalog.network_supeq(, )");
        assert_eq!(r, Some(25..25));
        let (t, r) = expand_snippet("coalesce(${1:value}, ${2:default})$0");
        assert_eq!(t, "coalesce(value, default)");
        assert_eq!(r, Some(9..14));
        let (t, r) = expand_snippet("now()$0");
        assert_eq!((t.as_str(), r), ("now()", Some(5..5)));
        assert_eq!(expand_snippet("a \\$1 ${1|x,y|}").0, "a $1 x");
        assert_eq!(expand_snippet("plain").1, None);
    }

    #[test]
    fn completion_replaces_only_the_typed_word() {
        use gpui_kit::base::input::completion_word_start;
        for (text, word) in [
            ("select * from pro|", "pro"),
            ("select na| from products", "na"),
            ("sel|", "sel"),
            ("select * from public.|", ""),
            ("select * from public.us|", "us"),
            ("select |", ""),
            ("where çağrı_id|", "çağrı_id"),
        ] {
            let offset = text.find('|').unwrap();
            let text = text.replace('|', "");
            let rope = gpui_kit::base::input::Rope::from_str(&text);
            let start = completion_word_start(&rope, offset);
            assert_eq!(&text[start..offset], word, "{text:?}");
            assert_eq!(word_prefix(&text, offset), word, "{text:?}");
        }
    }

    /// sqls against the local containers: table names complete from each
    /// live catalog (dev builds bundle it with scripts/bundle-sqls.sh
    /// target/debug/pgls).
    #[test]
    fn live_sqls_engines() {
        use crate::engine::Engine as E;
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/target/debug/pgls/sqls");
        if !std::path::Path::new(bin).is_file() {
            return;
        }
        // SAFETY: tests touching this variable don't run concurrently.
        unsafe { std::env::set_var("TUSK_SQLS", bin) };
        let rt = crate::db::runtime();
        for (engine, port, user, db, pass, text, want) in [
            (
                E::MySql,
                33306,
                "root",
                "shop",
                "tusk",
                "SELECT * FROM cu",
                "customers",
            ),
            (
                E::MariaDb,
                33307,
                "root",
                "shop",
                "tusk",
                "SELECT * FROM cu",
                "customers",
            ),
            (
                E::MsSql,
                31433,
                "sa",
                "master",
                "Tusk_pass123",
                "SELECT * FROM tusk_ls",
                "tusk_lsp_probe",
            ),
            (
                E::ClickHouse,
                38123,
                "default",
                "shop",
                "tusk",
                "SELECT * FROM cu",
                "customers",
            ),
            (
                E::Cockroach,
                26257,
                "root",
                "shop",
                "",
                "SELECT * FROM cu",
                "customers",
            ),
            (
                E::Vertica,
                35433,
                "dbadmin",
                "docker",
                "",
                "SELECT * FROM cu",
                "customers",
            ),
        ] {
            if !crate::drivers::live::reachable(port) {
                continue;
            }
            let c = crate::drivers::live::conn(engine, port, user, db);
            if engine == E::MsSql {
                // A table of this test's own (others recreate dbo.people).
                let d = rt
                    .block_on(crate::drivers::connect(
                        &c,
                        c.host.clone(),
                        port,
                        pass.into(),
                    ))
                    .unwrap();
                rt.block_on(d.driver().exec(
                    "IF OBJECT_ID('dbo.tusk_lsp_probe') IS NULL CREATE TABLE dbo.tusk_lsp_probe (id INT PRIMARY KEY)".into(),
                ))
                .unwrap();
            }
            let cfg = super::sqls_connection(&c, "127.0.0.1", port, pass).unwrap();
            let (client, _diags) = super::LspClient::start(super::ServerSpec::sqls(cfg)).unwrap();
            let uri = client.open_document(text);
            // The server loads the catalog in the background: retry briefly.
            let mut labels = Vec::new();
            for _ in 0..40 {
                let v = rt
                    .block_on(client.request(
                        "textDocument/completion",
                        serde_json::json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": text.len()}}),
                    ))
                    .unwrap();
                let items = match serde_json::from_value(v)
                    .unwrap_or(lsp_types::CompletionResponse::Array(vec![]))
                {
                    lsp_types::CompletionResponse::Array(a) => a,
                    lsp_types::CompletionResponse::List(l) => l.items,
                };
                labels = items.into_iter().map(|i| i.label).collect();
                if labels.iter().any(|l| l == want) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            assert!(
                labels.iter().any(|l| l == want),
                "{}: {labels:?}",
                engine.label()
            );
        }
    }

    #[test]
    fn framing_and_triggers() {
        let f = frame(&serde_json::json!({"a": 1}));
        assert_eq!(f, b"Content-Length: 7\r\n\r\n{\"a\":1}");
        assert!(is_trigger_char('c') && is_trigger_char('.') && is_trigger_char('_'));
        assert!(!is_trigger_char(' ') && !is_trigger_char(';'));
    }

    #[test]
    fn prefix_and_decoration() {
        assert_eq!(word_prefix("SELECT * FROM cu", 16), "cu");
        assert_eq!(word_prefix("select a.na", 11), "na");
        assert_eq!(word_prefix("x ", 2), "");
        let mut item: lsp_types::CompletionItem = serde_json::from_value(serde_json::json!({
            "label": "customers",
            "labelDetails": {"detail": " Table", "description": "public"}
        }))
        .unwrap();
        decorate(&mut item, "Cu");
        assert_eq!(item.filter_text.as_deref(), Some("Cu"));
        assert_eq!(item.detail.as_deref(), Some("Table · public"));
        let mut other = item.clone();
        decorate(&mut other, "zz");
        assert_eq!(other.filter_text.as_deref(), Some(""));
    }
}
