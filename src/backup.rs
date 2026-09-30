//! Backup / Restore windows: pick a connection and a database, the client
//! tools' version, options as chips, then Start. `pg_dump` writes the backup
//! (optionally gzipped); `pg_restore` (archives) or `psql` (plain SQL) loads
//! one. SSH profiles go through their tunnel; the password travels in
//! `PGPASSWORD`, never on the command line.

use crate::theme::TextCaption as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{Disableable as _, Icon, Root, Sizable as _, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::db::{self, SavedConnection, SslMode};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Backup,
    Restore,
}

pub const BACKUP_OPTIONS: [&str; 6] = [
    "--data-only",
    "--clean",
    "--create",
    "--no-owner",
    "--schema-only",
    "--format=custom",
];
pub const RESTORE_OPTIONS: [&str; 7] = [
    "--data-only",
    "--clean",
    "--create",
    "--exit-on-error",
    "--no-owner",
    "--schema-only",
    "--single-transaction",
];

// ---------------------------------------------------------------------------
// client tools
// ---------------------------------------------------------------------------

/// Folders that may hold the PostgreSQL client tools: the copy bundled in
/// Tusk.app first (`Contents/Resources/pgtools/bin`, relocatable, no
/// Homebrew needed), then `$TUSK_PG_BIN`, PATH and the usual install places
/// (GUI apps don't get the shell's PATH).
fn tool_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(contents) = exe.parent().and_then(|p| p.parent())
    {
        dirs.push(contents.join("Resources/pgtools/bin"));
    }
    // Dev builds: the same bundle staged next to the binary.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        dirs.push(dir.join("pgtools/bin"));
    }
    if let Some(d) = std::env::var_os("TUSK_PG_BIN") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    for v in ["18", "17", "16", "15", "14", "13", "12"] {
        dirs.push(PathBuf::from(format!(
            "/opt/homebrew/opt/postgresql@{v}/bin"
        )));
        dirs.push(PathBuf::from(format!("/usr/local/opt/postgresql@{v}/bin")));
        dirs.push(PathBuf::from(format!(
            "/Applications/Postgres.app/Contents/Versions/{v}/bin"
        )));
    }
    // Windows: the EDB installer's `C:\Program Files\PostgreSQL\<v>\bin`.
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        for v in ["18", "17", "16", "15", "14", "13", "12"] {
            dirs.push(PathBuf::from(&pf).join("PostgreSQL").join(v).join("bin"));
        }
    }
    for d in [
        "/opt/homebrew/opt/libpq/bin",
        "/opt/homebrew/bin",
        "/usr/local/opt/libpq/bin",
        "/usr/local/bin",
        "/Applications/Postgres.app/Contents/Versions/latest/bin",
    ] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// One installed set of client tools.
#[derive(Clone, Debug, PartialEq)]
pub struct Tools {
    /// `PostgreSQL 17.4`
    pub label: String,
    pub major: u32,
    pub dir: PathBuf,
}

/// `pg_dump (PostgreSQL) 17.4 (Homebrew)` → (`PostgreSQL 17.4`, 17).
pub fn parse_version(out: &str) -> Option<(String, u32)> {
    let v = out
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let major = v.split('.').next()?.parse().ok()?;
    Some((format!("PostgreSQL {v}"), major))
}

/// Every distinct toolset found, newest first.
pub fn installed_tools() -> Vec<Tools> {
    let mut out: Vec<Tools> = Vec::new();
    for dir in tool_dirs() {
        let dump = dir.join(format!("pg_dump{}", std::env::consts::EXE_SUFFIX));
        if !dump.is_file() {
            continue;
        }
        let Ok(o) = Command::new(&dump).arg("--version").output() else {
            continue;
        };
        let Some((label, major)) = parse_version(&String::from_utf8_lossy(&o.stdout)) else {
            continue;
        };
        if !out.iter().any(|t| t.label == label) {
            out.push(Tools { label, major, dir });
        }
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.major));
    out
}

/// The toolset for a server: same major if installed, else a newer one
/// (a newer pg_dump can dump an older server), else the newest there is.
pub fn pick_tools(tools: &[Tools], server_major: Option<u32>) -> Option<usize> {
    server_major
        .and_then(|m| tools.iter().position(|t| t.major == m))
        .or_else(|| {
            tools
                .iter()
                .rposition(|t| server_major.is_none_or(|m| t.major >= m))
        })
        .or(if tools.is_empty() { None } else { Some(0) })
}

/// Where a connection is reachable + how to log in.
#[derive(Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub ssl: SslMode,
}

fn run_tool(
    dir: &Path,
    tool: &str,
    ep: &Endpoint,
    db: &str,
    args: &[String],
) -> Result<String, String> {
    let path = dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
    if !path.is_file() {
        return Err(format!("{tool} not found in {}", dir.display()));
    }
    let out = Command::new(&path)
        .arg("-h")
        .arg(&ep.host)
        .arg("-p")
        .arg(ep.port.to_string())
        .arg("-U")
        .arg(&ep.user)
        .arg("-d")
        .arg(db)
        .args(args)
        .env("PGPASSWORD", &ep.password)
        .env(
            "PGSSLMODE",
            match ep.ssl {
                SslMode::Disable => "disable",
                SslMode::Prefer => "prefer",
                SslMode::Require => "require",
            },
        )
        .output()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        Ok(log)
    } else if log.trim().is_empty() {
        Err(format!("{tool} exited with {}", out.status))
    } else {
        Err(log)
    }
}

/// Backup file: `template` with `{connection}` / `{database}` / `{date}` /
/// `{time}` filled in, `.dump` for the custom format else `.sql`, `+.gz`.
/// Import ▸ From SQL Dump: run a plain SQL file with psql (so the `COPY …
/// FROM stdin` blocks of a dump work), in one transaction, stopping at the
/// first error.
pub fn run_sql_file(ep: &Endpoint, db: &str, file: &Path) -> Result<String, String> {
    let tools = installed_tools();
    let t = pick_tools(&tools, None)
        .and_then(|i| tools.get(i))
        .ok_or("psql not found; install PostgreSQL client tools or set TUSK_PG_BIN")?;
    let args: Vec<String> = [
        "-X",
        "-q",
        "-v",
        "ON_ERROR_STOP=1",
        "--single-transaction",
        "-f",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(std::iter::once(file.display().to_string()))
    .collect();
    run_tool(&t.dir, "psql", ep, db, &args)
}

pub fn backup_file_name(
    template: &str,
    conn: &str,
    database: &str,
    options: &[String],
    gzip: bool,
) -> String {
    let now = chrono::Local::now();
    let mut name = template
        .replace("{connection}", conn)
        .replace("{database}", database)
        .replace("{date}", &now.format("%Y-%m-%d").to_string())
        .replace("{time}", &now.format("%H-%M-%S").to_string());
    if name.trim().is_empty() {
        name = database.to_string();
    }
    let custom = options.iter().any(|o| o == "--format=custom");
    name.push_str(if custom { ".dump" } else { ".sql" });
    if gzip {
        name.push_str(".gz");
    }
    name.replace('/', "-")
}

/// `gzip -f`: `path` → `path.gz`, the original removed.
fn gzip_file(path: &Path) -> Result<PathBuf, String> {
    let gz = PathBuf::from(format!("{}.gz", path.display()));
    let run = || -> std::io::Result<()> {
        let mut input = std::fs::File::open(path)?;
        let out = std::fs::File::create(&gz)?;
        let mut enc = flate2::write::GzEncoder::new(out, flate2::Compression::default());
        std::io::copy(&mut input, &mut enc)?;
        enc.finish()?;
        std::fs::remove_file(path)
    };
    run().map_err(|e| format!("gzip: {e}"))?;
    Ok(gz)
}

/// A `.gz` backup unpacked to a temp file, for restoring.
fn gunzip_to_temp(path: &Path) -> Result<PathBuf, String> {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "restore".into());
    let out_path = std::env::temp_dir().join(format!("tusk-{}-{stem}", std::process::id()));
    let run = || -> std::io::Result<()> {
        let mut dec = flate2::read::MultiGzDecoder::new(std::fs::File::open(path)?);
        let mut out = std::fs::File::create(&out_path)?;
        std::io::copy(&mut dec, &mut out).map(|_| ())
    };
    run().map_err(|e| format!("gzip: {e}"))?;
    Ok(out_path)
}

/// Plain SQL goes to psql, custom / tar archives to pg_restore.
fn is_plain_sql(path: &Path) -> bool {
    let Ok(head) = std::fs::read(path).map(|b| b.into_iter().take(512).collect::<Vec<u8>>()) else {
        return true;
    };
    let archive = head.starts_with(b"PGDMP") || head.windows(5).any(|w| w == b"ustar");
    !archive
}

/// psql understands only some of the restore options; the rest are dropped.
fn psql_args(options: &[String], file: &Path) -> Vec<String> {
    let mut a = vec!["-f".to_string(), file.display().to_string()];
    for o in options {
        match o.as_str() {
            "--single-transaction" => a.push(o.clone()),
            "--exit-on-error" => {
                a.push("-v".into());
                a.push("ON_ERROR_STOP=1".into());
            }
            _ => {}
        }
    }
    a
}

// ---------------------------------------------------------------------------
// window
// ---------------------------------------------------------------------------

#[derive(Default)]
struct OpenBackup(Option<AnyWindowHandle>);
impl Global for OpenBackup {}

pub struct BackupWindow {
    focus: FocusHandle,
    mode: Mode,
    conns: Vec<SavedConnection>,
    conn_search: Entity<InputState>,
    db_search: Entity<InputState>,
    file_name: Entity<InputState>,
    selected: Option<usize>,
    databases: Vec<String>,
    selected_db: Option<String>,
    loading: bool,
    /// Open connection to the picked profile (keeps its SSH tunnel alive).
    connected: Option<db::Connected>,
    endpoint: Option<Endpoint>,
    tools: Vec<Tools>,
    tool_ix: Option<usize>,
    options: Vec<String>,
    gzip: bool,
    busy: bool,
    /// A save / open panel or a confirmation is up: Return must answer
    /// it, not start another run from the window's own Return binding.
    modal: bool,
    notice: Option<(bool, String)>,
    _subs: Vec<Subscription>,
}

/// The folder the last backup went to (the save panel opens there next).
static LAST_BACKUP_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

impl BackupWindow {
    /// `preselect`: open with this connection (and database) picked.
    pub fn open(mode: Mode, preselect: Option<(String, String)>, cx: &mut App) {
        if let Some(handle) = cx.default_global::<OpenBackup>().0 {
            let _ = cx.update_window(handle, |_, window, _| window.remove_window());
        }
        let bounds = Bounds::centered(None, size(px(760.), px(460.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(660.), px(380.))),
                focus: !crate::background(),
                ..TitleBar::window_options()
            },
            move |window, cx| {
                let view = cx.new(|cx| Self::new(mode, preselect, window, cx));
                view.read(cx).focus.clone().focus(window, cx);
                cx.new(|cx| Root::new(view, window, cx))
            },
        );
        match result {
            Ok(h) => cx.set_global(OpenBackup(Some(h.into()))),
            Err(e) => log::warn!("backup window failed to open: {e}"),
        }
    }

    fn new(
        mode: Mode,
        preselect: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = |p: &str, window: &mut Window, cx: &mut Context<Self>| {
            let p = p.to_string();
            cx.new(|cx| InputState::new(window, cx).placeholder(p))
        };
        let conn_search = search("Search for connection…", window, cx);
        let db_search = search("Search for database…", window, cx);
        let file_name = cx.new(|cx| {
            let mut st = InputState::new(window, cx);
            st.set_value("{database}_{date}".to_string(), window, cx);
            st
        });
        let subs = [&conn_search, &db_search]
            .into_iter()
            .map(|s| {
                cx.subscribe(s, |_, _, ev: &InputEvent, cx| {
                    if matches!(ev, InputEvent::Change) {
                        cx.notify();
                    }
                })
            })
            .collect();
        let tools = installed_tools();
        let mut this = BackupWindow {
            focus: cx.focus_handle(),
            mode,
            // pg_dump / pg_restore only speak Postgres: no SQLite etc. here.
            conns: db::load_connections()
                .into_iter()
                .filter(|c| c.engine.caps().backup)
                .collect(),
            conn_search,
            db_search,
            file_name,
            selected: None,
            databases: Vec::new(),
            selected_db: None,
            loading: false,
            connected: None,
            endpoint: None,
            tool_ix: pick_tools(&tools, None),
            tools,
            options: vec![match mode {
                Mode::Backup => "--format=custom".into(),
                Mode::Restore => "--single-transaction".into(),
            }],
            gzip: false,
            busy: false,
            modal: false,
            notice: None,
            _subs: subs,
        };
        if let Some((conn, database)) = preselect
            && let Some(ix) = this.conns.iter().position(|c| c.name == conn)
        {
            this.pick_connection(ix, Some(database), cx);
        }
        this
    }

    /// Connect to the profile (Keychain password, SSH tunnel) and list its
    /// databases; the server's major version picks the matching tools.
    fn pick_connection(&mut self, ix: usize, database: Option<String>, cx: &mut Context<Self>) {
        let Some(conn) = self.conns.get(ix).cloned() else {
            return;
        };
        self.selected = Some(ix);
        self.databases.clear();
        self.selected_db = None;
        self.connected = None;
        self.endpoint = None;
        self.loading = true;
        self.notice = None;
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let password = db::load_password(&conn.name).unwrap_or_default();
            let result = async {
                let connected = db::connect(conn.clone(), password.clone(), None).await?;
                let dbs = db::fetch_databases(&connected.pool).await?;
                let version = db::run_query_rows(&connected.pool, "SHOW server_version_num", 1)
                    .await
                    .ok()
                    .and_then(|r| r.into_iter().next())
                    .and_then(|v| v.as_object().and_then(|o| o.values().next().cloned()))
                    .and_then(|v| v.as_str().and_then(|s| s.parse::<u32>().ok()))
                    .map(|n| n / 10000);
                Ok::<_, String>((connected, dbs, version))
            }
            .await;
            let _ = weak.update(cx, |this: &mut BackupWindow, cx| {
                this.loading = false;
                match result {
                    Ok((connected, dbs, version)) => {
                        this.endpoint = Some(Endpoint {
                            host: connected.host.clone(),
                            port: connected.port,
                            user: conn.user.clone(),
                            password,
                            ssl: conn.ssl,
                        });
                        this.connected = Some(connected);
                        this.selected_db = database
                            .filter(|d| dbs.contains(d))
                            .or_else(|| dbs.iter().find(|d| **d == conn.database).cloned());
                        this.databases = dbs;
                        this.tool_ix = pick_tools(&this.tools, version);
                    }
                    Err(e) => this.notice = Some((false, e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.modal {
            return;
        }
        let (Some(ix), Some(database), Some(ep)) = (
            self.selected,
            self.selected_db.clone(),
            self.endpoint.clone(),
        ) else {
            self.notice = Some((false, "Pick a connection and a database first.".into()));
            cx.notify();
            return;
        };
        let Some(tools) = self.tool_ix.and_then(|i| self.tools.get(i)).cloned() else {
            self.notice = Some((
                false,
                "PostgreSQL client tools are missing from this build.".into(),
            ));
            cx.notify();
            return;
        };
        let conn = self.conns[ix].name.clone();
        let options = self.options.clone();
        match self.mode {
            Mode::Backup => {
                let name = backup_file_name(
                    &self.file_name.read(cx).value(),
                    &conn,
                    &database,
                    &options,
                    self.gzip,
                );
                let gzip = self.gzip;
                let dir = LAST_BACKUP_DIR
                    .lock()
                    .ok()
                    .and_then(|d| d.clone())
                    .filter(|d| d.is_dir())
                    .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Downloads"));
                let rx = cx.prompt_for_new_path(&dir, Some(&name));
                self.modal = true;
                cx.spawn(async move |weak, cx: &mut AsyncApp| {
                    let picked = rx.await;
                    let _ = weak.update(cx, |this: &mut BackupWindow, _| this.modal = false);
                    let Ok(Ok(Some(path))) = picked else { return };
                    if let (Some(parent), Ok(mut last)) = (path.parent(), LAST_BACKUP_DIR.lock()) {
                        *last = Some(parent.to_path_buf());
                    }
                    let _ = weak.update(cx, |this: &mut BackupWindow, cx| {
                        this.busy = true;
                        this.notice = Some((true, format!("Backing up {database}…")));
                        cx.notify();
                    });
                    // pg_dump writes the plain file; gzip compresses it after.
                    let raw = if gzip {
                        PathBuf::from(path.to_string_lossy().trim_end_matches(".gz").to_string())
                    } else {
                        path.clone()
                    };
                    let mut args = options.clone();
                    args.push("-f".into());
                    args.push(raw.display().to_string());
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            run_tool(&tools.dir, "pg_dump", &ep, &database, &args)?;
                            if gzip { gzip_file(&raw) } else { Ok(raw) }
                        })
                        .await;
                    let _ = weak.update(cx, |this: &mut BackupWindow, cx| {
                        this.busy = false;
                        this.notice = Some(match result {
                            Ok(file) => {
                                cx.reveal_path(&file);
                                (true, format!("Backup saved to {}", file.display()))
                            }
                            Err(e) => (false, e),
                        });
                        cx.notify();
                    });
                })
                .detach();
            }
            Mode::Restore => {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: false,
                    prompt: Some("Restore".into()),
                });
                let handle = window.window_handle();
                self.modal = true;
                cx.spawn(async move |weak, cx: &mut AsyncApp| {
                    let file = match rx.await {
                        Ok(Ok(Some(paths))) => paths.into_iter().next(),
                        _ => None,
                    };
                    let confirmed = match &file {
                        // Return = Restore: the user just picked the file.
                        Some(file) => match handle.update(cx, |_, window, cx| {
                            window.prompt(
                                PromptLevel::Warning,
                                &format!("Restore into “{database}”?"),
                                Some(&format!("{}\n\n{}", file.display(), options.join(" "))),
                                &["Restore", "Cancel"],
                                cx,
                            )
                        }) {
                            Ok(answer) => answer.await == Ok(0),
                            Err(_) => false,
                        },
                        None => false,
                    };
                    let _ = weak.update(cx, |this: &mut BackupWindow, _| this.modal = false);
                    let (true, Some(file)) = (confirmed, file) else {
                        return;
                    };
                    let _ = weak.update(cx, |this: &mut BackupWindow, cx| {
                        this.busy = true;
                        this.notice = Some((true, format!("Restoring into {database}…")));
                        cx.notify();
                    });
                    let db2 = database.clone();
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            let gz = file.extension().is_some_and(|e| e == "gz");
                            let input = if gz {
                                gunzip_to_temp(&file)?
                            } else {
                                file.clone()
                            };
                            let r = if is_plain_sql(&input) {
                                run_tool(
                                    &tools.dir,
                                    "psql",
                                    &ep,
                                    &db2,
                                    &psql_args(&options, &input),
                                )
                            } else {
                                let mut a = options.clone();
                                a.push(input.display().to_string());
                                run_tool(&tools.dir, "pg_restore", &ep, &db2, &a)
                            };
                            if gz {
                                let _ = std::fs::remove_file(&input);
                            }
                            r
                        })
                        .await;
                    let _ = weak.update(cx, |this: &mut BackupWindow, cx| {
                        this.busy = false;
                        this.notice = Some(match result {
                            Ok(_) => (true, format!("Restored into {database}")),
                            Err(e) => (false, e),
                        });
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    /// Restore ▸ "New database": create one on the picked server, select it.
    fn new_database(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pool) = self.connected.as_ref().map(|c| c.pool.clone()) else {
            return;
        };
        if self.modal {
            return;
        }
        // The name typed in the database search, else restored, restored_2…
        let typed = self.db_search.read(cx).value().trim().to_string();
        let mut name = if typed.is_empty() || self.databases.contains(&typed) {
            "restored".to_string()
        } else {
            typed
        };
        let mut n = 2;
        while self.databases.contains(&name) {
            name = format!("restored_{n}");
            n += 1;
        }
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Create database “{name}”?"),
            Some("To use another name, type it in the database search first."),
            &["Create", "Cancel"],
            cx,
        );
        self.modal = true;
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let answer = answer.await;
            let _ = weak.update(cx, |this: &mut BackupWindow, _| this.modal = false);
            if answer != Ok(0) {
                return;
            }
            let sql = format!("CREATE DATABASE {}", db::quote_ident(&name));
            let result = db::run_exec(&pool, &sql).await;
            let _ = weak.update(cx, |this: &mut BackupWindow, cx| {
                match result {
                    Ok(_) => {
                        this.databases.push(name.clone());
                        this.databases.sort();
                        this.selected_db = Some(name);
                    }
                    Err(e) => this.notice = Some((false, e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn connection_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (fg, muted) = (t.foreground, t.muted_foreground);
        let sel_bg = crate::theme::selection(t);
        let q = self.conn_search.read(cx).value().to_lowercase();
        let mut folders: Vec<Option<String>> = Vec::new();
        for c in &self.conns {
            if !folders.contains(&c.folder) {
                folders.push(c.folder.clone());
            }
        }
        folders.sort_by_key(|f| f.is_none());
        let mut list = div().flex().flex_col().gap_0p5().p_1();
        for folder in folders {
            let members: Vec<usize> = (0..self.conns.len())
                .filter(|&i| {
                    let c = &self.conns[i];
                    c.folder == folder
                        && (q.is_empty()
                            || c.name.to_lowercase().contains(&q)
                            || c.database.to_lowercase().contains(&q))
                })
                .collect();
            if members.is_empty() {
                continue;
            }
            if let Some(f) = &folder {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .px_2()
                        .pt_1()
                        .h(px(24.))
                        .text_caption()
                        .text_color(muted)
                        .child(Icon::new(IconName::Folder).size(px(12.)))
                        .child(f.clone()),
                );
            }
            for ix in members {
                let c = &self.conns[ix];
                let active = self.selected == Some(ix);
                let tint = c.status_rgb().map_or(muted, |x| rgb(x).into());
                list = list.child(
                    div()
                        .id(("bk-conn", ix))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .h(px(26.))
                        .rounded(crate::theme::RADIUS_SM)
                        .when(folder.is_some(), |this| this.pl(px(22.)))
                        .when(active, |this| this.bg(sel_bg))
                        .when(!active, |this| {
                            this.hover(|this| this.bg(muted.opacity(0.08)))
                        })
                        .child(Icon::new(IconName::Database).size(px(12.)).text_color(tint))
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(fg)
                                .child(c.name.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_caption()
                                .text_color(muted.opacity(0.6))
                                .child(format!("{} · {}", c.endpoint(), c.database)),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.pick_connection(ix, None, cx)),
                        ),
                );
            }
        }
        list.into_any_element()
    }

    fn database_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (fg, muted) = (t.foreground, t.muted_foreground);
        let sel_bg = crate::theme::selection(t);
        if self.loading {
            return div()
                .p_3()
                .text_sm()
                .text_color(muted)
                .child("Connecting…")
                .into_any_element();
        }
        let q = self.db_search.read(cx).value().to_lowercase();
        let mut list = div().flex().flex_col().gap_0p5().p_1();
        for d in self
            .databases
            .iter()
            .filter(|d| q.is_empty() || d.to_lowercase().contains(&q))
        {
            let active = self.selected_db.as_deref() == Some(d.as_str());
            let name = d.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("bk-db-{d}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(26.))
                    .rounded(crate::theme::RADIUS_SM)
                    .when(active, |this| this.bg(sel_bg))
                    .when(!active, |this| {
                        this.hover(|this| this.bg(muted.opacity(0.08)))
                    })
                    .child(
                        Icon::new(IconName::Database)
                            .size(px(12.))
                            .text_color(muted),
                    )
                    .child(div().text_sm().text_color(fg).child(d.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_db = Some(name.clone());
                        cx.notify();
                    })),
            );
        }
        list.into_any_element()
    }

    /// Version, "Add option…" and the option chips.
    fn options_column(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (fg, muted, border, card, accent) = (
            t.foreground,
            t.muted_foreground,
            t.border,
            t.group_box,
            t.accent,
        );
        let enabled = self.selected.is_some();
        let label = self
            .tool_ix
            .and_then(|i| self.tools.get(i))
            .map(|t| t.label.clone())
            .unwrap_or_else(|| {
                if self.tools.is_empty() {
                    "No client tools found".into()
                } else {
                    "Choose version".into()
                }
            });
        let (tools, current, view) = (self.tools.clone(), self.tool_ix, cx.entity());
        let version = Button::new("bk-version")
            .label(label)
            .small()
            .w_full()
            .disabled(!enabled || self.tools.is_empty())
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for (i, t) in tools.iter().enumerate() {
                    let v = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(t.label.clone())
                            .checked(current == Some(i))
                            .on_click(move |_, _, cx| {
                                v.update(cx, |this, cx| {
                                    this.tool_ix = Some(i);
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            });
        let all: &'static [&'static str] = match self.mode {
            Mode::Backup => &BACKUP_OPTIONS,
            Mode::Restore => &RESTORE_OPTIONS,
        };
        let (picked, view) = (self.options.clone(), cx.entity());
        let add = Button::new("bk-add-option")
            .label("Add option…")
            .small()
            .w_full()
            .disabled(!enabled)
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for &o in all {
                    let v = view.clone();
                    menu = menu.item(
                        // Literal flags: no `--` → `—` ligature.
                        PopupMenuItem::element(move |_, _| {
                            div().font_features(crate::theme::no_ligatures()).child(o)
                        })
                        .checked(picked.iter().any(|p| p == o))
                        .on_click(move |_, _, cx| {
                            v.update(cx, |this, cx| {
                                match this.options.iter().position(|x| x == o) {
                                    Some(p) => {
                                        this.options.remove(p);
                                    }
                                    None => this.options.push(o.to_string()),
                                }
                                cx.notify();
                            });
                        }),
                    );
                }
                menu
            });
        let mut chips = div().flex().flex_wrap().gap_1().p_2();
        if self.options.is_empty() {
            chips = chips.child(
                div()
                    .text_sm()
                    .text_color(muted.opacity(0.6))
                    .child("Options…"),
            );
        }
        for (i, o) in self.options.iter().enumerate() {
            chips = chips.child(
                div()
                    .id(("bk-chip", i))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .h(px(20.))
                    .rounded(crate::theme::RADIUS_SM)
                    .bg(accent.opacity(0.22))
                    .text_caption()
                    .font_family(crate::settings::table_font())
                    .font_features(crate::theme::no_ligatures())
                    .text_color(fg)
                    .child(o.clone())
                    .child(Icon::new(IconName::X).size(px(10.)).text_color(muted))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if i < this.options.len() {
                            this.options.remove(i);
                        }
                        cx.notify();
                    })),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .w(px(220.))
            .flex_none()
            .child(version)
            .child(add)
            .child(
                div()
                    .flex_1()
                    .min_h(px(120.))
                    .rounded(crate::theme::RADIUS_MD)
                    .border_1()
                    .border_color(border)
                    .bg(card)
                    .child(chips),
            )
            .into_any_element()
    }
}

impl BackupWindow {
    fn render_inner(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let (bg, fg, border, card) = (t.background, t.foreground, t.border, t.group_box);
        let (ok_c, err_c) = (t.green, t.red);
        let backup = self.mode == Mode::Backup;
        let pane = |id: &'static str, search: &Entity<InputState>, body: AnyElement| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    Input::new(search)
                        .small()
                        .prefix(Icon::new(IconName::Search).size(px(12.))),
                )
                .child(
                    div()
                        .id(id)
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .rounded(crate::theme::RADIUS_MD)
                        .border_1()
                        .border_color(border)
                        .bg(card)
                        .child(body),
                )
        };
        let conns = pane(
            "bk-conns",
            &self.conn_search.clone(),
            self.connection_list(cx),
        );
        let dbs = pane("bk-dbs", &self.db_search.clone(), self.database_list(cx));
        let options = self.options_column(cx);
        let view = cx.entity();
        let file_row = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().text_sm().text_color(fg).child("File name:"))
            .child(
                div().flex_1().child(
                    Input::new(&self.file_name)
                        .small()
                        .font_family(crate::settings::table_font()),
                ),
            )
            .child(
                Button::new("bk-customize")
                    .label("Customize")
                    .small()
                    .dropdown_menu(move |mut menu, _, _| {
                        for (label, token) in [
                            ("Connection name", "{connection}"),
                            ("Database name", "{database}"),
                            ("Date (YYYY-MM-DD)", "{date}"),
                            ("Time (HH-MM-SS)", "{time}"),
                        ] {
                            let v = view.clone();
                            menu = menu.item(PopupMenuItem::new(label).on_click(
                                move |_, window, cx| {
                                    v.update(cx, |this, cx| {
                                        this.file_name.update(cx, |st, cx| {
                                            let cur = st.value().to_string();
                                            let sep = if cur.is_empty() || cur.ends_with('_') {
                                                ""
                                            } else {
                                                "_"
                                            };
                                            st.set_value(format!("{cur}{sep}{token}"), window, cx);
                                        });
                                    });
                                },
                            ));
                        }
                        menu
                    }),
            );
        let footer = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .children(self.notice.clone().map(|(ok, msg)| {
                        div()
                            .id("bk-notice")
                            .max_h(px(60.))
                            .overflow_y_scroll()
                            .text_caption()
                            .text_color(if ok { ok_c } else { err_c })
                            .child(msg)
                    })),
            )
            .when(backup, |this| {
                this.child(
                    Checkbox::new("bk-gzip")
                        .label("Compress file using Gzip")
                        .checked(self.gzip)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.gzip = *v;
                            cx.notify();
                        })),
                )
            })
            .when(!backup, |this| {
                this.child(
                    Button::new("bk-newdb")
                        .label("New database")
                        .small()
                        .disabled(self.connected.is_none() || self.busy)
                        .on_click(cx.listener(|this, _, window, cx| this.new_database(window, cx))),
                )
            })
            .child(
                Button::new("bk-start")
                    .label(if backup {
                        "Start backup…"
                    } else {
                        "Start restore…"
                    })
                    .small()
                    .primary()
                    .disabled(self.busy || self.selected_db.is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
            );
        let columns = if backup {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .gap_3()
                .child(conns)
                .child(dbs)
                .child(options)
        } else {
            // Restore keeps the options on the left.
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .gap_3()
                .child(options)
                .child(conns)
                .child(dbs)
        };
        div()
            .track_focus(&self.focus)
            .key_context(crate::dialog_keys::CONTEXT)
            .on_action(crate::dialog_keys::close)
            .on_action(
                cx.listener(|this, _: &crate::dialog_keys::DialogConfirm, window, cx| {
                    // Return does what the (enabled) Start button does.
                    if this.selected_db.is_some() {
                        this.start(window, cx);
                    }
                }),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(bg)
            .text_color(fg)
            .font_family(crate::settings::ui_font())
            .child(
                TitleBar::new()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .justify_center()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if backup {
                                "Backup database"
                            } else {
                                "Restore database"
                            }),
                    )
                    .child(div().w(px(60.))),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .px_4()
                    .pb_4()
                    .pt_2()
                    .when(backup, |this| this.child(file_row))
                    .child(columns)
                    .child(footer),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Tools, backup_file_name, gunzip_to_temp, gzip_file, parse_version, pick_tools, psql_args,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn gzip_round_trip() {
        let src = std::env::temp_dir().join(format!("tusk-gz-test-{}.sql", std::process::id()));
        let body = "SELECT 1;\n".repeat(1000);
        std::fs::write(&src, &body).unwrap();
        let gz = gzip_file(&src).unwrap();
        assert!(!src.exists(), "gzip -f removes the original");
        let back = gunzip_to_temp(&gz).unwrap();
        assert_eq!(std::fs::read_to_string(&back).unwrap(), body);
        let _ = std::fs::remove_file(gz);
        let _ = std::fs::remove_file(back);
    }

    #[test]
    fn versions_names_and_args() {
        assert_eq!(
            parse_version("pg_dump (PostgreSQL) 17.4 (Homebrew)"),
            Some(("PostgreSQL 17.4".into(), 17))
        );
        let t = |m: u32| Tools {
            label: format!("PostgreSQL {m}"),
            major: m,
            dir: PathBuf::new(),
        };
        let tools = vec![t(18), t(16)];
        assert_eq!(pick_tools(&tools, Some(16)), Some(1));
        assert_eq!(pick_tools(&tools, Some(17)), Some(0));
        assert_eq!(pick_tools(&[], Some(17)), None);
        let opts = vec!["--format=custom".to_string()];
        assert_eq!(
            backup_file_name("{connection}_{database}", "Local", "shop", &opts, true),
            "Local_shop.dump.gz"
        );
        assert_eq!(backup_file_name("", "c", "shop", &[], false), "shop.sql");
        let a = psql_args(
            &[
                "--single-transaction".into(),
                "--exit-on-error".into(),
                "--clean".into(),
            ],
            Path::new("/x.sql"),
        );
        assert_eq!(
            a,
            [
                "-f",
                "/x.sql",
                "--single-transaction",
                "-v",
                "ON_ERROR_STOP=1"
            ]
        );
    }
}

#[cfg(test)]
mod live_tests {
    use super::{Endpoint, installed_tools, run_tool};

    /// Needs the dev Postgres (docker compose) and the staged tools
    /// (`scripts/bundle-pgtools.sh target/debug/pgtools`); skipped otherwise.
    #[test]
    fn dump_and_restore_round_trip() {
        let tools = installed_tools();
        let Some(t) = tools.first() else { return };
        let ep = Endpoint {
            host: "127.0.0.1".into(),
            port: 55432,
            user: "tusk".into(),
            password: "tusk".into(),
            ssl: crate::db::SslMode::Disable,
        };
        let file = std::env::temp_dir().join(format!("tusk-rt-{}.dump", std::process::id()));
        let f = file.display().to_string();
        let args: Vec<String> = [
            "--format=custom",
            "--no-owner",
            "-t",
            "public.products",
            "-f",
            &f,
        ]
        .map(String::from)
        .to_vec();
        if run_tool(&t.dir, "pg_dump", &ep, "tusk_dev", &args).is_err() {
            return; // no database running
        }
        assert!(
            std::fs::metadata(&file)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        );
        let _ = run_tool(
            &t.dir,
            "psql",
            &ep,
            "tusk_dev",
            &["-c".into(), "DROP DATABASE IF EXISTS tusk_rt".into()],
        );
        run_tool(
            &t.dir,
            "psql",
            &ep,
            "tusk_dev",
            &["-c".into(), "CREATE DATABASE tusk_rt".into()],
        )
        .unwrap();
        let r = run_tool(
            &t.dir,
            "pg_restore",
            &ep,
            "tusk_rt",
            &[
                "--no-owner".into(),
                "--single-transaction".into(),
                "--exit-on-error".into(),
                f.clone(),
            ],
        );
        let _ = run_tool(
            &t.dir,
            "psql",
            &ep,
            "tusk_dev",
            &["-c".into(), "DROP DATABASE IF EXISTS tusk_rt".into()],
        );
        let _ = std::fs::remove_file(&file);
        r.unwrap();
    }
}

impl Render for BackupWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .child(self.render_inner(window, cx))
            .children(gpui_kit::component::Root::render_notification_layer(
                window, cx,
            ))
    }
}
