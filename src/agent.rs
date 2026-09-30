//! One AI chat: an agent process (over ACP) and the conversation shown in
//! the AI panel.
//!
//! [`AgentThread`] starts the agent (installing it first when needed), runs
//! `initialize` → `session/new`, sends prompts and folds the agent's
//! `session/update`s into [`Entry`]s: messages and thoughts (Markdown), tool
//! calls with their permission prompts, diffs and terminals, the plan, the
//! slash commands, modes / session options and context usage. It also
//! answers the agent's `fs/*` and `terminal/*` requests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui_kit::component::text::TextViewState;
use gpui_kit::*;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::acp::{self, AcpConnection, Incoming, RpcError};
use crate::acp_registry::AgentSpec;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Installing / starting the agent, or opening the session.
    Starting(String),
    /// The agent wants a login first.
    AuthRequired,
    Ready,
    /// A prompt is running.
    Busy,
    /// The agent could not start or went away.
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    pub description: String,
    /// `terminal` methods: run the agent with these args in a terminal.
    pub terminal: Option<TerminalLogin>,
}

/// Args and environment for a terminal login.
pub type TerminalLogin = (Vec<String>, Vec<(String, String)>);

/// How a terminal ended: exit code, or the signal that stopped it.
pub type ExitStatus = Option<(Option<i64>, Option<String>)>;

#[derive(Clone, Debug)]
pub struct SelectOption {
    pub value: String,
    pub name: String,
    pub group: Option<String>,
}

#[derive(Clone, Debug)]
pub enum ConfigKind {
    Select {
        current: String,
        options: Vec<SelectOption>,
    },
    Boolean(bool),
}

#[derive(Clone, Debug)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub category: Option<String>,
    pub kind: ConfigKind,
}

#[derive(Clone, Debug)]
pub struct Command {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
    pub priority: String,
}

#[derive(Clone, Debug)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: String,
}

pub struct Permission {
    pub request: Value,
    pub options: Vec<PermissionOption>,
}

pub enum ToolContent {
    Text(Entity<TextViewState>),
    Diff {
        path: String,
        old: Option<String>,
        new: String,
    },
    Terminal(String),
}

pub struct ToolCall {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub content: Vec<ToolContent>,
    pub locations: Vec<String>,
    pub raw_input: Value,
    pub expanded: bool,
    pub permission: Option<Permission>,
}

#[allow(clippy::large_enum_variant)] // one per chat line; boxing buys nothing
pub enum Entry {
    User {
        text: String,
    },
    Agent {
        md: Entity<TextViewState>,
        id: Option<String>,
    },
    Thought {
        md: Entity<TextViewState>,
        expanded: bool,
    },
    Tool(ToolCall),
    Notice {
        text: String,
        error: bool,
    },
}

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated: Option<String>,
}

/// A terminal the agent asked for (`terminal/create`).
pub struct Terminal {
    pub command: String,
    pub output: Arc<Mutex<String>>,
    pub truncated: Arc<Mutex<bool>>,
    pub exit: Arc<Mutex<ExitStatus>>,
    kill: Option<tokio::sync::oneshot::Sender<()>>,
    exited: Arc<tokio::sync::Notify>,
}

pub struct AgentThread {
    pub spec: AgentSpec,
    pub status: Status,
    conn: Option<Arc<AcpConnection>>,
    pub session_id: Option<String>,
    pub agent_name: String,
    pub caps: Value,
    pub auth_methods: Vec<AuthMethod>,
    pub entries: Vec<Entry>,
    /// Legacy session modes (when the agent has no `mode` config option).
    pub modes: Option<(String, Vec<SelectOption>)>,
    pub config: Vec<ConfigOption>,
    pub commands: Vec<Command>,
    pub plan: Vec<PlanEntry>,
    /// Context window: used / size tokens, and the cost so far.
    pub usage: Option<(u64, u64, Option<String>)>,
    pub title: Option<String>,
    pub terminals: HashMap<String, Terminal>,
    next_terminal: u64,
    /// The agent program's path (terminal logins run it).
    program: Option<acp::AgentCommand>,
    mcp: Value,
    cwd: PathBuf,
    dirty_tx: mpsc::UnboundedSender<()>,
    _tasks: Vec<Task<()>>,
}

pub enum ThreadEvent {
    /// Entries changed (the panel remeasures / follows the tail).
    Changed,
}

impl EventEmitter<ThreadEvent> for AgentThread {}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// Text of a content block (text, or the name / text of a resource).
/// URI of the database context Tusk sends with every prompt.
pub(crate) const CONTEXT_URI: &str = "tusk://context";

/// A prompt block carrying Tusk's injected context (not something the user typed).
fn is_context_block(b: &Value) -> bool {
    matches!(b["type"].as_str(), Some("resource" | "resource_link"))
        && (b["resource"]["uri"] == CONTEXT_URI || b["uri"] == CONTEXT_URI)
}

/// The user's own words from a replayed prompt: agents replay the injected
/// context as text (`tusk://context` plus `<context ref="tusk://context">…
/// </context>`), which must not show in the user's bubble.
pub(crate) fn strip_injected_context(text: &str) -> String {
    let open = format!("<context ref=\"{CONTEXT_URI}\">");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(&open) {
        out.push_str(&rest[..i]);
        rest = &rest[i + open.len()..];
        match rest.find("</context>") {
            Some(j) => rest = &rest[j + "</context>".len()..],
            None => rest = "",
        }
    }
    out.push_str(rest);
    out.lines()
        .filter(|l| {
            let t = l.trim();
            t != CONTEXT_URI
                && t != format!("@{CONTEXT_URI}")
                && !(t.starts_with('[') && t.ends_with(&format!("]({CONTEXT_URI})")))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn block_text(b: &Value) -> String {
    match b["type"].as_str() {
        Some("text") => s(&b["text"]),
        Some("resource_link") => format!("[{}]({})", s(&b["name"]), s(&b["uri"])),
        Some("resource") => {
            let r = &b["resource"];
            if r.get("text").is_some() {
                format!("```\n{}\n```", s(&r["text"]))
            } else {
                s(&r["uri"])
            }
        }
        Some("image") => "*(image)*".into(),
        Some("audio") => "*(audio)*".into(),
        _ => String::new(),
    }
}

fn parse_select_options(v: &Value) -> Vec<SelectOption> {
    let mut out = Vec::new();
    for o in v.as_array().into_iter().flatten() {
        if let Some(group) = o.get("group") {
            for g in o["options"].as_array().into_iter().flatten() {
                out.push(SelectOption {
                    value: s(&g["value"]),
                    name: s(&g["name"]),
                    group: Some(
                        o["name"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| s(group)),
                    ),
                });
            }
        } else {
            out.push(SelectOption {
                value: s(&o["value"]),
                name: s(&o["name"]),
                group: None,
            });
        }
    }
    out
}

pub fn parse_config(v: &Value) -> Vec<ConfigOption> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let kind = match o["type"].as_str()? {
                "select" => ConfigKind::Select {
                    current: s(&o["currentValue"]),
                    options: parse_select_options(&o["options"]),
                },
                "boolean" => ConfigKind::Boolean(o["currentValue"].as_bool().unwrap_or(false)),
                _ => return None,
            };
            Some(ConfigOption {
                id: s(&o["id"]),
                name: s(&o["name"]),
                category: o["category"].as_str().map(str::to_string),
                kind,
            })
        })
        .collect()
}

fn parse_modes(v: &Value) -> Option<(String, Vec<SelectOption>)> {
    let current = v.get("currentModeId")?.as_str()?.to_string();
    let modes = v["availableModes"]
        .as_array()?
        .iter()
        .map(|m| SelectOption {
            value: s(&m["id"]),
            name: s(&m["name"]),
            group: None,
        })
        .collect();
    Some((current, modes))
}

fn parse_plan(v: &Value) -> Vec<PlanEntry> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|e| PlanEntry {
            content: s(&e["content"]),
            status: s(&e["status"]),
            priority: s(&e["priority"]),
        })
        .collect()
}

impl AgentThread {
    pub fn new(spec: AgentSpec, mcp: Value, cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let (dirty_tx, mut dirty_rx) = mpsc::unbounded_channel::<()>();
        // Terminal output arrives off the UI thread: redraw on each chunk.
        let redraw = cx.spawn(async move |this, cx| {
            while dirty_rx.recv().await.is_some() {
                while dirty_rx.try_recv().is_ok() {}
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(80))
                    .await;
            }
        });
        let mut t = Self {
            agent_name: spec.name.clone(),
            spec,
            status: Status::Starting("Starting…".into()),
            conn: None,
            session_id: None,
            caps: Value::Null,
            auth_methods: Vec::new(),
            entries: Vec::new(),
            modes: None,
            config: Vec::new(),
            commands: Vec::new(),
            plan: Vec::new(),
            usage: None,
            title: None,
            terminals: HashMap::new(),
            next_terminal: 1,
            program: None,
            mcp,
            cwd,
            dirty_tx,
            _tasks: vec![redraw],
        };
        t.start(cx);
        t
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(ThreadEvent::Changed);
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let spec = self.spec.clone();
        let cwd = self.cwd.clone();
        self.status = Status::Starting(if crate::acp_registry::installed(&spec) {
            format!("Starting {}…", spec.name)
        } else {
            format!("Installing {}…", spec.name)
        });
        let task = cx.spawn(async move |this, cx| {
            let resolved = cx
                .background_executor()
                .spawn(async move { crate::acp_registry::resolve(&spec) })
                .await;
            let cmd = match resolved {
                Ok(c) => c,
                Err(e) => {
                    let _ = this.update(cx, |t, cx| t.fail(format!("{e:#}"), cx));
                    return;
                }
            };
            let started = AcpConnection::start(&cmd, &cwd);
            let (conn, mut rx) = match started {
                Ok(x) => x,
                Err(e) => {
                    let _ = this.update(cx, |t, cx| t.fail(format!("{e:#}"), cx));
                    return;
                }
            };
            let ok = this.update(cx, |t, cx| {
                t.conn = Some(conn.clone());
                t.program = Some(cmd.clone());
                t.status = Status::Starting("Connecting…".into());
                cx.notify();
            });
            if ok.is_err() {
                return;
            }
            // Everything the agent sends on its own.
            let listen = cx.spawn({
                let this = this.clone();
                async move |cx| {
                    while let Some(msg) = rx.recv().await {
                        if this.update(cx, |t, cx| t.incoming(msg, cx)).is_err() {
                            return;
                        }
                    }
                }
            });
            let _ = this.update(cx, |t, _| t._tasks.push(listen));
            let init = conn.request("initialize", acp::initialize_params()).await;
            match init {
                Ok(r) => {
                    let _ = this.update(cx, |t, cx| {
                        t.caps = r["agentCapabilities"].clone();
                        if let Some(n) = r["agentInfo"]["title"]
                            .as_str()
                            .or(r["agentInfo"]["name"].as_str())
                            && !n.is_empty()
                        {
                            t.agent_name = n.to_string();
                        }
                        t.auth_methods = r["authMethods"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|m| AuthMethod {
                                id: s(&m["id"]),
                                name: s(&m["name"]),
                                description: s(&m["description"]),
                                terminal: (m["type"] == "terminal").then(|| {
                                    (
                                        m["args"]
                                            .as_array()
                                            .into_iter()
                                            .flatten()
                                            .filter_map(|a| a.as_str().map(str::to_string))
                                            .collect(),
                                        m["env"]
                                            .as_object()
                                            .into_iter()
                                            .flatten()
                                            .map(|(k, v)| (k.clone(), s(v)))
                                            .collect(),
                                    )
                                }),
                            })
                            .collect();
                        t.new_session(cx);
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |t, cx| t.fail(t.explain(&e), cx));
                }
            }
        });
        self._tasks.push(task);
    }

    fn explain(&self, e: &RpcError) -> String {
        let tail = self
            .conn
            .as_ref()
            .map(|c| c.stderr_tail())
            .unwrap_or_default();
        if tail.trim().is_empty() {
            e.to_string()
        } else {
            format!("{e}\n\n{}", tail.trim())
        }
    }

    fn fail(&mut self, msg: String, cx: &mut Context<Self>) {
        self.status = Status::Failed(msg);
        self.conn = None;
        self.reject_permissions();
        self.changed(cx);
    }

    pub fn supports(&self, path: &[&str]) -> bool {
        let mut v = &self.caps;
        for p in path {
            v = &v[*p];
        }
        !v.is_null() && v != &Value::Bool(false)
    }

    /// `session/new` (again after a login).
    pub fn new_session(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.conn.clone() else {
            return;
        };
        self.status = Status::Starting("Opening a chat…".into());
        cx.notify();
        let params =
            json!({ "cwd": self.cwd.display().to_string(), "mcpServers": [self.mcp.clone()] });
        let task = cx.spawn(async move |this, cx| {
            let r = conn.request("session/new", params).await;
            let _ = this.update(cx, |t, cx| match r {
                Ok(r) => {
                    t.session_ready(&r, cx);
                    t.session_id = r["sessionId"].as_str().map(str::to_string);
                    t.apply_saved_config(cx);
                }
                Err(e) if e.code == acp::AUTH_REQUIRED => {
                    t.status = Status::AuthRequired;
                    t.changed(cx);
                }
                Err(e) => t.fail(t.explain(&e), cx),
            });
        });
        self._tasks.push(task);
    }

    fn session_ready(&mut self, r: &Value, cx: &mut Context<Self>) {
        self.modes = parse_modes(&r["modes"]);
        if r.get("configOptions").is_some_and(Value::is_array) {
            self.config = parse_config(&r["configOptions"]);
        }
        self.status = Status::Ready;
        self.changed(cx);
    }

    /// Re-apply the options picked last time for this agent.
    fn apply_saved_config(&mut self, cx: &mut Context<Self>) {
        let saved = crate::settings::get()
            .agent_config
            .get(&self.spec.id)
            .cloned()
            .unwrap_or_default();
        for (id, value) in saved {
            let differs =
                self.config
                    .iter()
                    .find(|o| o.id == id)
                    .is_some_and(|o| match (&o.kind, &value) {
                        (ConfigKind::Select { current, options }, Value::String(v)) => {
                            current != v && options.iter().any(|x| &x.value == v)
                        }
                        (ConfigKind::Boolean(b), Value::Bool(v)) => b != v,
                        _ => false,
                    });
            if differs {
                self.set_config(&id, value, false, cx);
            } else if id == "__mode"
                && let (Some((cur, modes)), Value::String(v)) = (&self.modes, &value)
                && cur != v
                && modes.iter().any(|m| &m.value == v)
            {
                self.set_mode(v.clone(), false, cx);
            }
        }
    }

    /// `authenticate` with one of the agent's login methods.
    pub fn authenticate(&mut self, method: &AuthMethod, cx: &mut Context<Self>) {
        #[cfg(windows)]
        if let (Some((args, env)), Some(prog)) = (&method.terminal, &self.program) {
            // Terminal login: run the agent's own login flow in a new console.
            use std::os::windows::process::CommandExt as _;
            const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
            let _ = std::process::Command::new(&prog.program)
                .args(prog.args.iter().chain(args))
                .envs(env.iter().map(|(k, v)| (k, v)))
                .creation_flags(CREATE_NEW_CONSOLE)
                .spawn();
            self.entries.push(Entry::Notice {
                text: format!(
                    "Finish signing in to {} in the console window, then press Retry.",
                    self.agent_name
                ),
                error: false,
            });
            self.changed(cx);
            return;
        }
        #[cfg(not(windows))]
        if let (Some((args, env)), Some(prog)) = (&method.terminal, &self.program) {
            // Terminal login: run the agent's own login flow in Terminal.
            let mut line = shell_quote(&prog.program.display().to_string());
            for a in prog.args.iter().chain(args) {
                line.push(' ');
                line.push_str(&shell_quote(a));
            }
            let exports: String = env
                .iter()
                .map(|(k, v)| format!("export {k}={}; ", shell_quote(v)))
                .collect();
            let script = format!(
                "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
                format!("{exports}{line}")
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
            );
            let _ = std::process::Command::new("osascript")
                .arg("-e")
                .arg(script)
                .spawn();
            self.entries.push(Entry::Notice {
                text: format!(
                    "Finish signing in to {} in Terminal, then press Retry.",
                    self.agent_name
                ),
                error: false,
            });
            self.changed(cx);
            return;
        }
        let Some(conn) = self.conn.clone() else {
            return;
        };
        self.status = Status::Starting(format!("Signing in with {}…", method.name));
        cx.notify();
        let id = method.id.clone();
        let task = cx.spawn(async move |this, cx| {
            let r = conn
                .request("authenticate", json!({ "methodId": id }))
                .await;
            let _ = this.update(cx, |t, cx| match r {
                Ok(_) => t.new_session(cx),
                Err(e) => {
                    t.status = Status::AuthRequired;
                    t.entries.push(Entry::Notice {
                        text: e.to_string(),
                        error: true,
                    });
                    t.changed(cx);
                }
            });
        });
        self._tasks.push(task);
    }

    /// Send a prompt: the user's text plus context blocks.
    /// Send `text`, showing `shown` in the transcript (with attachment names).
    pub fn send_shown(
        &mut self,
        text: String,
        shown: String,
        context: Vec<Value>,
        cx: &mut Context<Self>,
    ) {
        let (Some(conn), Some(sid)) = (self.conn.clone(), self.session_id.clone()) else {
            return;
        };
        if self.status != Status::Ready {
            return;
        }
        self.entries.push(Entry::User { text: shown });
        self.status = Status::Busy;
        self.changed(cx);
        let mut prompt = vec![json!({ "type": "text", "text": text })];
        let embedded = self.supports(&["promptCapabilities", "embeddedContext"]);
        for c in context {
            if embedded || c["type"] == "text" {
                prompt.push(c);
            } else if c["type"] == "resource" {
                // Tagged, so a replay of this prompt can hide it again.
                let body = s(&c["resource"]["text"]);
                let uri = s(&c["resource"]["uri"]);
                prompt.push(json!({
                    "type": "text",
                    "text": format!("<context ref=\"{uri}\">\n{body}\n</context>"),
                }));
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let r = conn
                .request(
                    "session/prompt",
                    json!({ "sessionId": sid, "prompt": prompt }),
                )
                .await;
            let _ = this.update(cx, |t, cx| {
                if matches!(t.status, Status::Failed(_)) {
                    return;
                }
                t.status = Status::Ready;
                t.reject_permissions();
                match r {
                    Ok(r) => {
                        let note = match r["stopReason"].as_str() {
                            Some("max_tokens") => Some("The reply hit the token limit."),
                            Some("max_turn_requests") => Some("The agent hit its turn limit."),
                            Some("refusal") => Some("The agent declined to continue."),
                            _ => None,
                        };
                        if let Some(n) = note {
                            t.entries.push(Entry::Notice {
                                text: n.into(),
                                error: false,
                            });
                        }
                    }
                    Err(e) if e.code == acp::AUTH_REQUIRED => t.status = Status::AuthRequired,
                    Err(e) => t.entries.push(Entry::Notice {
                        text: t.explain(&e),
                        error: true,
                    }),
                }
                t.changed(cx);
            });
        });
        self._tasks.push(task);
    }

    /// Stop the running prompt.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if let (Some(conn), Some(sid)) = (&self.conn, &self.session_id) {
            conn.notify("session/cancel", json!({ "sessionId": sid }));
        }
        self.reject_permissions();
        cx.notify();
    }

    /// Unanswered permission prompts answer `cancelled` (after a stop).
    fn reject_permissions(&mut self) {
        let conn = self.conn.clone();
        for e in &mut self.entries {
            if let Entry::Tool(tc) = e
                && let Some(p) = tc.permission.take()
                && let Some(conn) = &conn
            {
                conn.respond(
                    p.request,
                    Ok(json!({ "outcome": { "outcome": "cancelled" } })),
                );
            }
        }
    }

    /// The user picked a permission option on a tool call.
    pub fn answer_permission(&mut self, tool_id: &str, option: &str, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        for e in &mut self.entries {
            if let Entry::Tool(tc) = e
                && tc.id == tool_id
                && let Some(p) = tc.permission.take()
                && let Some(conn) = &conn
            {
                conn.respond(
                    p.request,
                    Ok(json!({ "outcome": { "outcome": "selected", "optionId": option } })),
                );
            }
        }
        self.changed(cx);
    }

    pub fn set_config(&mut self, id: &str, value: Value, remember: bool, cx: &mut Context<Self>) {
        let (Some(conn), Some(sid)) = (self.conn.clone(), self.session_id.clone()) else {
            return;
        };
        // Show the pick right away.
        for o in &mut self.config {
            if o.id == id {
                match (&mut o.kind, &value) {
                    (ConfigKind::Select { current, .. }, Value::String(v)) => *current = v.clone(),
                    (ConfigKind::Boolean(b), Value::Bool(v)) => *b = *v,
                    _ => {}
                }
            }
        }
        if remember {
            let (agent, key, v) = (self.spec.id.clone(), id.to_string(), value.clone());
            crate::settings::update(cx, |p| {
                p.agent_config.entry(agent).or_default().insert(key, v);
            });
        }
        cx.notify();
        let mut params = json!({ "sessionId": sid, "configId": id, "value": value });
        if value.is_boolean() {
            params["type"] = json!("boolean");
        }
        let task = cx.spawn(async move |this, cx| {
            let r = conn.request("session/set_config_option", params).await;
            let _ = this.update(cx, |t, cx| {
                match r {
                    Ok(r) if r["configOptions"].is_array() => {
                        t.config = parse_config(&r["configOptions"])
                    }
                    Ok(_) => {}
                    Err(e) => t.entries.push(Entry::Notice {
                        text: e.to_string(),
                        error: true,
                    }),
                }
                t.changed(cx);
            });
        });
        self._tasks.push(task);
    }

    pub fn set_mode(&mut self, id: String, remember: bool, cx: &mut Context<Self>) {
        let (Some(conn), Some(sid)) = (self.conn.clone(), self.session_id.clone()) else {
            return;
        };
        if let Some((cur, _)) = &mut self.modes {
            *cur = id.clone();
        }
        if remember {
            let (agent, v) = (self.spec.id.clone(), id.clone());
            crate::settings::update(cx, |p| {
                p.agent_config
                    .entry(agent)
                    .or_default()
                    .insert("__mode".into(), json!(v));
            });
        }
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let r = conn
                .request(
                    "session/set_mode",
                    json!({ "sessionId": sid, "modeId": id }),
                )
                .await;
            if let Err(e) = r {
                let _ = this.update(cx, |t, cx| {
                    t.entries.push(Entry::Notice {
                        text: e.to_string(),
                        error: true,
                    });
                    t.changed(cx);
                });
            }
        });
        self._tasks.push(task);
    }

    /// `session/list` (when the agent keeps its chats).
    pub fn list_sessions(&self, cx: &mut Context<Self>) -> Task<Vec<SessionSummary>> {
        let Some(conn) = self.conn.clone() else {
            return Task::ready(Vec::new());
        };
        if !self.supports(&["sessionCapabilities", "list"]) {
            return Task::ready(Vec::new());
        }
        let cwd = self.cwd.display().to_string();
        cx.background_spawn(async move {
            let r = conn
                .request("session/list", json!({ "cwd": cwd }))
                .await
                .unwrap_or_default();
            r["sessions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| SessionSummary {
                    id: s(&x["sessionId"]),
                    title: x["title"]
                        .as_str()
                        .filter(|t| !t.is_empty())
                        .unwrap_or("Untitled chat")
                        .to_string(),
                    updated: x["updatedAt"].as_str().map(str::to_string),
                })
                .collect()
        })
    }

    /// Reopen an earlier chat: `session/load` replays it; `session/resume`
    /// continues without the history.
    pub fn open_session(&mut self, id: String, title: String, cx: &mut Context<Self>) {
        let Some(conn) = self.conn.clone() else {
            return;
        };
        let load = self.supports(&["loadSession"]);
        let method = if load {
            "session/load"
        } else {
            "session/resume"
        };
        self.entries.clear();
        self.plan.clear();
        self.usage = None;
        self.title = Some(title);
        self.session_id = Some(id.clone());
        self.status = Status::Starting("Opening the chat…".into());
        self.changed(cx);
        let params = json!({
            "sessionId": id,
            "cwd": self.cwd.display().to_string(),
            "mcpServers": [self.mcp.clone()]
        });
        let task = cx.spawn(async move |this, cx| {
            let r = conn.request(method, params).await;
            let _ = this.update(cx, |t, cx| match r {
                Ok(r) => {
                    t.session_ready(&r, cx);
                    if !load {
                        t.entries.push(Entry::Notice {
                            text:
                                "Continuing this chat (the agent can't show its earlier messages)."
                                    .into(),
                            error: false,
                        });
                        t.changed(cx);
                    }
                }
                Err(e) => {
                    t.status = Status::Ready;
                    t.entries.push(Entry::Notice {
                        text: e.to_string(),
                        error: true,
                    });
                    t.changed(cx);
                }
            });
        });
        self._tasks.push(task);
    }

    fn incoming(&mut self, msg: Incoming, cx: &mut Context<Self>) {
        match msg {
            Incoming::Notification { method, params } => {
                if method == "session/update"
                    && params["sessionId"].as_str() == self.session_id.as_deref()
                {
                    self.update(&params["update"], cx);
                }
            }
            Incoming::Request { id, method, params } => self.request(id, &method, params, cx),
            Incoming::Exited { stderr } => {
                if !matches!(self.status, Status::Failed(_)) {
                    let msg = if stderr.trim().is_empty() {
                        format!("{} exited.", self.agent_name)
                    } else {
                        format!("{} exited.\n\n{}", self.agent_name, stderr.trim())
                    };
                    self.fail(msg, cx);
                }
            }
        }
    }

    fn markdown(&self, text: &str, cx: &mut Context<Self>) -> Entity<TextViewState> {
        let text = text.to_string();
        cx.new(|cx| TextViewState::markdown(&text, cx))
    }

    fn update(&mut self, u: &Value, cx: &mut Context<Self>) {
        match u["sessionUpdate"].as_str().unwrap_or_default() {
            "agent_message_chunk" => {
                let text = block_text(&u["content"]);
                let id = u["messageId"].as_str().map(str::to_string);
                match self.entries.last_mut() {
                    Some(Entry::Agent { md, id: last }) if id.is_none() || *last == id => {
                        md.update(cx, |m, cx| m.push_str(&text, cx));
                    }
                    _ => {
                        let md = self.markdown(&text, cx);
                        self.entries.push(Entry::Agent { md, id });
                    }
                }
            }
            "agent_thought_chunk" => {
                let text = block_text(&u["content"]);
                match self.entries.last_mut() {
                    Some(Entry::Thought { md, .. }) => md.update(cx, |m, cx| m.push_str(&text, cx)),
                    _ => {
                        let md = self.markdown(&text, cx);
                        self.entries.push(Entry::Thought {
                            md,
                            expanded: false,
                        });
                    }
                }
            }
            "user_message_chunk" => {
                // A replayed prompt also carries Tusk's context block.
                if is_context_block(&u["content"]) {
                    return;
                }
                let text = block_text(&u["content"]);
                match self.entries.last_mut() {
                    Some(Entry::User { text: t }) => t.push_str(&text),
                    _ => self.entries.push(Entry::User { text }),
                }
            }
            "tool_call" | "tool_call_update" => self.upsert_tool(u, cx),
            "plan" => self.plan = parse_plan(&u["entries"]),
            "plan_update" => {
                let p = &u["plan"];
                self.plan = match p["type"].as_str() {
                    Some("items") => parse_plan(&p["entries"]),
                    Some("markdown") => vec![PlanEntry {
                        content: s(&p["markdown"]),
                        status: "in_progress".into(),
                        priority: String::new(),
                    }],
                    _ => self.plan.clone(),
                };
            }
            "plan_removed" => self.plan.clear(),
            "available_commands_update" => {
                self.commands = u["availableCommands"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| Command {
                        name: s(&c["name"]),
                        description: s(&c["description"]),
                    })
                    .collect();
            }
            "current_mode_update" => {
                if let Some((cur, _)) = &mut self.modes {
                    *cur = s(&u["currentModeId"]);
                }
            }
            "config_option_update" => self.config = parse_config(&u["configOptions"]),
            "session_info_update" => {
                if let Some(t) = u["title"].as_str() {
                    self.title = Some(t.to_string());
                }
            }
            "usage_update" => {
                let cost = u["cost"]["amount"].as_f64().map(|a| {
                    let cur = u["cost"]["currency"].as_str().unwrap_or("USD");
                    if cur == "USD" {
                        format!("${a:.2}")
                    } else {
                        format!("{a:.2} {cur}")
                    }
                });
                self.usage = Some((
                    u["used"].as_u64().unwrap_or(0),
                    u["size"].as_u64().unwrap_or(0),
                    cost,
                ));
            }
            _ => return,
        }
        self.changed(cx);
    }

    fn tool_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        self.entries.iter_mut().rev().find_map(|e| match e {
            Entry::Tool(tc) if tc.id == id => Some(tc),
            _ => None,
        })
    }

    fn tool_content(&self, v: &Value, cx: &mut Context<Self>) -> Vec<ToolContent> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| match c["type"].as_str()? {
                "content" => Some(ToolContent::Text(
                    self.markdown(&block_text(&c["content"]), cx),
                )),
                "diff" => Some(ToolContent::Diff {
                    path: s(&c["path"]),
                    old: c["oldText"].as_str().map(str::to_string),
                    new: s(&c["newText"]),
                }),
                "terminal" => Some(ToolContent::Terminal(s(&c["terminalId"]))),
                _ => None,
            })
            .collect()
    }

    fn upsert_tool(&mut self, u: &Value, cx: &mut Context<Self>) {
        let id = s(&u["toolCallId"]);
        let content = u
            .get("content")
            .filter(|c| c.is_array())
            .map(|c| self.tool_content(c, cx));
        let locations: Option<Vec<String>> =
            u.get("locations").and_then(Value::as_array).map(|l| {
                l.iter()
                    .map(|x| match x["line"].as_u64() {
                        Some(n) => format!("{}:{n}", s(&x["path"])),
                        None => s(&x["path"]),
                    })
                    .collect()
            });
        if let Some(tc) = self.tool_mut(&id) {
            if let Some(t) = u["title"].as_str() {
                tc.title = t.to_string();
            }
            if let Some(k) = u["kind"].as_str() {
                tc.kind = k.to_string();
            }
            if let Some(st) = u["status"].as_str() {
                tc.status = st.to_string();
            }
            if let Some(c) = content {
                tc.content = c;
            }
            if let Some(l) = locations {
                tc.locations = l;
            }
            if !u["rawInput"].is_null() {
                tc.raw_input = u["rawInput"].clone();
            }
            return;
        }
        self.entries.push(Entry::Tool(ToolCall {
            title: u["title"].as_str().unwrap_or("Tool").to_string(),
            kind: u["kind"].as_str().unwrap_or("other").to_string(),
            status: u["status"].as_str().unwrap_or("pending").to_string(),
            content: content.unwrap_or_default(),
            locations: locations.unwrap_or_default(),
            raw_input: u["rawInput"].clone(),
            expanded: false,
            permission: None,
            id,
        }));
    }

    fn request(&mut self, id: Value, method: &str, params: Value, cx: &mut Context<Self>) {
        let Some(conn) = self.conn.clone() else {
            return;
        };
        match method {
            "session/request_permission" => {
                let tool = &params["toolCall"];
                self.upsert_tool(tool, cx);
                let tid = s(&tool["toolCallId"]);
                let options = params["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|o| PermissionOption {
                        id: s(&o["optionId"]),
                        name: s(&o["name"]),
                        kind: s(&o["kind"]),
                    })
                    .collect();
                if let Some(tc) = self.tool_mut(&tid) {
                    tc.permission = Some(Permission {
                        request: id,
                        options,
                    });
                    tc.expanded = true;
                }
                self.changed(cx);
            }
            "fs/read_text_file" => {
                let path = s(&params["path"]);
                let res = std::fs::read_to_string(&path)
                    .map(|text| {
                        let line = params["line"].as_u64().unwrap_or(1).max(1) as usize;
                        let limit = params["limit"].as_u64().map(|l| l as usize);
                        if line == 1 && limit.is_none() {
                            text
                        } else {
                            let lines = text.lines().skip(line - 1);
                            match limit {
                                Some(l) => lines.take(l).collect::<Vec<_>>().join("\n"),
                                None => lines.collect::<Vec<_>>().join("\n"),
                            }
                        }
                    })
                    .map(|content| json!({ "content": content }))
                    .map_err(|e| (acp::INTERNAL_ERROR, format!("{path}: {e}")));
                conn.respond(id, res);
            }
            "fs/write_text_file" => {
                let path = PathBuf::from(s(&params["path"]));
                let res = path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|_| std::fs::write(&path, s(&params["content"])))
                    .map(|_| Value::Null)
                    .map_err(|e| (acp::INTERNAL_ERROR, format!("{}: {e}", path.display())));
                conn.respond(id, res);
            }
            "terminal/create" => {
                let res = self.create_terminal(&params);
                conn.respond(
                    id,
                    res.map(|t| json!({ "terminalId": t }))
                        .map_err(|e| (acp::INTERNAL_ERROR, e)),
                );
                self.changed(cx);
            }
            "terminal/output" => {
                let res = match self.terminals.get(&s(&params["terminalId"])) {
                    Some(t) => {
                        let exit = t.exit.lock().unwrap().clone();
                        Ok(json!({
                            "output": t.output.lock().unwrap().clone(),
                            "truncated": *t.truncated.lock().unwrap(),
                            "exitStatus": exit.map(|(c, sig)| json!({ "exitCode": c, "signal": sig })),
                        }))
                    }
                    None => Err((acp::INTERNAL_ERROR, "no such terminal".to_string())),
                };
                conn.respond(id, res);
            }
            "terminal/wait_for_exit" => match self.terminals.get(&s(&params["terminalId"])) {
                Some(t) => {
                    let (exit, notify) = (t.exit.clone(), t.exited.clone());
                    crate::db::runtime().spawn(async move {
                        loop {
                            let n = notify.notified();
                            if let Some((c, sig)) = exit.lock().unwrap().clone() {
                                conn.respond(id, Ok(json!({ "exitCode": c, "signal": sig })));
                                return;
                            }
                            n.await;
                        }
                    });
                }
                None => conn.respond(
                    id,
                    Err((acp::INTERNAL_ERROR, "no such terminal".to_string())),
                ),
            },
            "terminal/kill" | "terminal/release" => {
                let tid = s(&params["terminalId"]);
                if let Some(t) = self.terminals.get_mut(&tid)
                    && let Some(k) = t.kill.take()
                {
                    let _ = k.send(());
                }
                if method == "terminal/release" {
                    // Keep the output for the transcript; only the process goes.
                }
                conn.respond(id, Ok(Value::Null));
            }
            _ => conn.respond(
                id,
                Err((acp::METHOD_NOT_FOUND, format!("{method} is not supported"))),
            ),
        }
    }

    fn create_terminal(&mut self, p: &Value) -> Result<String, String> {
        let program = s(&p["command"]);
        let args: Vec<String> = p["args"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().map(str::to_string))
            .collect();
        let limit = p["outputByteLimit"].as_u64().map(|l| l as usize);
        let cwd = p["cwd"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.clone());
        let _rt = crate::db::runtime().enter();
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&args)
            .current_dir(cwd)
            .env("PATH", crate::acp_registry::search_path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for e in p["env"].as_array().into_iter().flatten() {
            cmd.env(s(&e["name"]), s(&e["value"]));
        }
        let mut child = cmd.spawn().map_err(|e| format!("{program}: {e}"))?;
        let tid = format!("term-{}", self.next_terminal);
        self.next_terminal += 1;
        let output = Arc::new(Mutex::new(String::new()));
        let truncated = Arc::new(Mutex::new(false));
        let exit = Arc::new(Mutex::new(None));
        let exited = Arc::new(tokio::sync::Notify::new());
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
        let append = {
            let (output, truncated, dirty) =
                (output.clone(), truncated.clone(), self.dirty_tx.clone());
            move |chunk: &[u8]| {
                let mut o = output.lock().unwrap();
                o.push_str(&String::from_utf8_lossy(chunk));
                if let Some(limit) = limit
                    && o.len() > limit
                {
                    // Drop from the front, on a char boundary.
                    let mut cut = o.len() - limit;
                    while !o.is_char_boundary(cut) {
                        cut += 1;
                    }
                    o.drain(..cut);
                    *truncated.lock().unwrap() = true;
                }
                let _ = dirty.send(());
            }
        };
        for pipe in [
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let append = append.clone();
            crate::db::runtime().spawn(async move {
                use tokio::io::AsyncReadExt as _;
                let mut pipe = pipe;
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    append(&buf[..n]);
                }
            });
        }
        {
            let (exit, exited, dirty) = (exit.clone(), exited.clone(), self.dirty_tx.clone());
            crate::db::runtime().spawn(async move {
                let status = tokio::select! {
                    st = child.wait() => st.ok(),
                    _ = kill_rx => { let _ = child.kill().await; child.wait().await.ok() }
                };
                #[cfg(unix)]
                let sig = {
                    use std::os::unix::process::ExitStatusExt as _;
                    status
                        .and_then(|s| s.signal())
                        .map(|n| format!("signal {n}"))
                };
                #[cfg(not(unix))]
                let sig: Option<String> = None;
                *exit.lock().unwrap() = Some((status.and_then(|s| s.code()).map(i64::from), sig));
                exited.notify_waiters();
                let _ = dirty.send(());
            });
        }
        let mut shown = program.clone();
        for a in &args {
            shown.push(' ');
            shown.push_str(a);
        }
        self.terminals.insert(
            tid.clone(),
            Terminal {
                command: shown,
                output,
                truncated,
                exit,
                kill: Some(kill_tx),
                exited,
            },
        );
        Ok(tid)
    }

    /// Retry after a failure or a login.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        match self.status {
            Status::AuthRequired if self.conn.is_some() => self.new_session(cx),
            _ => {
                self.conn = None;
                self.session_id = None;
                self.start(cx);
                cx.notify();
            }
        }
    }

    /// "New Chat": a fresh session with the same agent.
    pub fn reset_chat(&mut self, cx: &mut Context<Self>) {
        if self.status == Status::Busy {
            self.cancel(cx);
        }
        self.entries.clear();
        self.plan.clear();
        self.usage = None;
        self.title = None;
        self.session_id = None;
        self.changed(cx);
        if self.conn.is_some() {
            self.new_session(cx);
        } else {
            self.retry(cx);
        }
    }

    pub fn toggle_entry(&mut self, ix: usize, cx: &mut Context<Self>) {
        match self.entries.get_mut(ix) {
            Some(Entry::Thought { expanded, .. }) => *expanded = !*expanded,
            Some(Entry::Tool(tc)) => tc.expanded = !tc.expanded,
            _ => return,
        }
        self.changed(cx);
    }
}

impl Drop for AgentThread {
    fn drop(&mut self) {
        for t in self.terminals.values_mut() {
            if let Some(k) = t.kill.take() {
                let _ = k.send(());
            }
        }
    }
}

#[cfg_attr(windows, allow(dead_code))] // Terminal login is POSIX-shell only
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports: a glob of the parent would bring gpui's `test`
    // attribute macro in place of the standard `#[test]`.
    use super::{
        ConfigKind, block_text, is_context_block, parse_config, shell_quote, strip_injected_context,
    };
    use serde_json::json;

    #[test]
    fn parses_grouped_and_flat_config_options() {
        let v = json!([
            {"id":"model","name":"Model","type":"select","currentValue":"b",
             "options":[{"value":"a","name":"A"},{"value":"b","name":"B"}]},
            {"id":"effort","name":"Effort","type":"select","currentValue":"x",
             "options":[{"group":"g","name":"Group","options":[{"value":"x","name":"X"}]}]},
            {"id":"fast","name":"Fast","type":"boolean","currentValue":true},
            {"id":"odd","name":"Odd","type":"slider"}
        ]);
        let c = parse_config(&v);
        assert_eq!(c.len(), 3);
        assert!(
            matches!(&c[0].kind, ConfigKind::Select { current, options } if current == "b" && options.len() == 2)
        );
        assert!(
            matches!(&c[1].kind, ConfigKind::Select { options, .. } if options[0].group.as_deref() == Some("Group"))
        );
        assert!(matches!(c[2].kind, ConfigKind::Boolean(true)));
    }

    #[test]
    fn replayed_prompt_hides_injected_context() {
        let replay = "why is it slow?\ntusk://context\n<context ref=\"tusk://context\">\nYou are the assistant inside Tusk…\nConnection: x\n</context>";
        assert_eq!(strip_injected_context(replay), "why is it slow?");
        assert_eq!(
            strip_injected_context("[@tusk://context](tusk://context)\nhi"),
            "hi"
        );
        // Cut off mid-context (still streaming): nothing of it shows.
        assert_eq!(
            strip_injected_context("hi <context ref=\"tusk://context\">You are"),
            "hi"
        );
        assert_eq!(
            strip_injected_context("plain <context> tag"),
            "plain <context> tag"
        );
        assert!(is_context_block(
            &json!({"type":"resource","resource":{"uri":"tusk://context","text":"x"}})
        ));
        assert!(!is_context_block(
            &json!({"type":"text","text":"tusk://context"})
        ));
    }

    #[test]
    fn block_texts() {
        assert_eq!(block_text(&json!({"type":"text","text":"hi"})), "hi");
        assert_eq!(
            block_text(&json!({"type":"resource_link","name":"a","uri":"file:///a"})),
            "[a](file:///a)"
        );
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("--login"), "--login");
    }
}
