//! Connection form state: InputState entities + draft SSL/tag/SSH + saved list.
//!
//! Secrets (database password, SSH password / key passphrase) live only in
//! masked InputStates and the Keychain — never in `SavedConnection` (which is
//! what gets serialized to JSON).

use gpui_kit::component::input::InputState;
use gpui_kit::*;

use crate::db::{self, ConnTag, SavedConnection, SshConfig, SslMode, load_password};

/// Engine-specific form fields: (key in `SavedConnection::options`, label, placeholder).
pub const OPTION_FIELDS: &[(&str, &str, &str)] = &[
    ("account_id", "Account ID", "Cloudflare account id"),
    ("account", "Account", "xy12345.eu-central-1"),
    ("warehouse", "Warehouse", "COMPUTE_WH"),
    ("role", "Role", "SYSADMIN"),
    ("schema", "Schema", "PUBLIC"),
    ("project", "Project", "my-project-id"),
    ("dataset", "Dataset", "my_dataset (optional)"),
    ("key_file", "Key File", "~/service-account.json"),
    ("region", "Region", "us-east-1"),
    ("access_key", "Access Key", "AKIA…"),
    ("endpoint", "Endpoint", "http://localhost:8000 (optional)"),
];

pub struct ConnectionForm {
    /// The engine picked for this connection.
    pub engine: crate::engine::Engine,
    /// Database file (SQLite / DuckDB) or server URL (LibSQL).
    pub path: Entity<InputState>,
    /// Engine-specific fields, keyed like [`OPTION_FIELDS`].
    pub options: Vec<(&'static str, Entity<InputState>)>,
    pub name: Entity<InputState>,
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    pub database: Entity<InputState>,
    pub user: Entity<InputState>,
    pub password: Entity<InputState>,
    pub folder: Entity<InputState>,
    pub ssl: SslMode,
    pub tag: Option<ConnTag>,
    pub status_color: Option<usize>,
    // ---- SSH tunnel ----
    pub ssh_enabled: bool,
    pub ssh_use_key: bool,
    pub ssh_host: Entity<InputState>,
    pub ssh_port: Entity<InputState>,
    pub ssh_user: Entity<InputState>,
    pub ssh_key: Entity<InputState>,
    /// SSH password, or the key's passphrase when `ssh_use_key`.
    pub ssh_secret: Entity<InputState>,
    /// Name of the saved connection being edited (None = new).
    pub editing: Option<String>,
    pub saved: Vec<SavedConnection>,
    pub selected: Option<usize>,
    pub busy: bool,
    /// Store the password in the Keychain on save (toggle in the dialog).
    pub save_password: bool,
    /// (success, message)
    pub notice: Option<(bool, String)>,
    /// Last connection's timestamp is kept when an existing profile is saved.
    last_used: Option<i64>,
}

fn input(
    window: &mut Window,
    cx: &mut App,
    placeholder: &str,
    value: String,
) -> Entity<InputState> {
    let placeholder = placeholder.to_string();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

fn secret(window: &mut Window, cx: &mut App, placeholder: &str) -> Entity<InputState> {
    let placeholder = placeholder.to_string();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .masked(true)
    })
}

impl ConnectionForm {
    /// The password is NOT read here: a Keychain read can raise the macOS
    /// access prompt, and at startup that blocked the first frame. It is
    /// fetched when connecting ([`Self::password_or_keychain`]).
    pub fn new(window: &mut Window, cx: &mut App, initial: &SavedConnection) -> Self {
        let ssh = initial.ssh.clone();
        Self {
            engine: initial.engine,
            path: input(
                window,
                cx,
                "~/data/app.db",
                initial.path.clone().unwrap_or_default(),
            ),
            options: OPTION_FIELDS
                .iter()
                .map(|(k, _, ph)| (*k, input(window, cx, ph, initial.opt(k).to_string())))
                .collect(),
            name: input(window, cx, "My database", initial.name.clone()),
            host: input(window, cx, "127.0.0.1", initial.host.clone()),
            port: input(window, cx, "5432", initial.port.to_string()),
            database: input(window, cx, "mydb", initial.database.clone()),
            user: input(window, cx, "postgres", initial.user.clone()),
            password: secret(
                window,
                cx,
                &format!("Password (stored in {})", db::CREDENTIAL_STORE),
            ),
            folder: input(
                window,
                cx,
                "No folder",
                initial.folder.clone().unwrap_or_default(),
            ),
            ssl: initial.ssl,
            tag: initial.tag,
            status_color: initial.status_color,
            ssh_enabled: ssh.is_some(),
            ssh_use_key: ssh.as_ref().is_some_and(|s| s.key_path.is_some()),
            ssh_host: input(
                window,
                cx,
                "bastion.example.com",
                ssh.as_ref().map(|s| s.host.clone()).unwrap_or_default(),
            ),
            ssh_port: input(
                window,
                cx,
                "22",
                ssh.as_ref()
                    .map_or("22".to_string(), |s| s.port.to_string()),
            ),
            ssh_user: input(
                window,
                cx,
                "ubuntu",
                ssh.as_ref().map(|s| s.user.clone()).unwrap_or_default(),
            ),
            ssh_key: input(
                window,
                cx,
                "~/.ssh/id_ed25519",
                ssh.as_ref()
                    .and_then(|s| s.key_path.clone())
                    .unwrap_or_default(),
            ),
            ssh_secret: secret(window, cx, &format!("Stored in {}", db::CREDENTIAL_STORE)),
            editing: None,
            saved: db::load_connections(),
            selected: None,
            busy: false,
            save_password: true,
            notice: None,
            last_used: initial.last_used,
        }
    }

    pub fn text(entity: &Entity<InputState>, cx: &App) -> String {
        entity.read(cx).value().to_string()
    }

    /// Current form contents as (SavedConnection, password).
    pub fn read_form(&self, cx: &App) -> (SavedConnection, String) {
        let port = Self::text(&self.port, cx)
            .trim()
            .parse::<u16>()
            .unwrap_or(0);
        let folder = Self::text(&self.folder, cx).trim().to_string();
        let ssh = self.ssh_enabled.then(|| {
            let key = Self::text(&self.ssh_key, cx).trim().to_string();
            SshConfig {
                host: Self::text(&self.ssh_host, cx).trim().to_string(),
                port: Self::text(&self.ssh_port, cx).trim().parse().unwrap_or(22),
                user: Self::text(&self.ssh_user, cx).trim().to_string(),
                // Empty key file = try the default keys (see ssh.rs).
                key_path: self.ssh_use_key.then_some(key),
            }
        });
        (
            SavedConnection {
                engine: self.engine,
                path: Some(Self::text(&self.path, cx).trim().to_string()).filter(|p| !p.is_empty()),
                options: self
                    .options
                    .iter()
                    .map(|(k, e)| (k.to_string(), Self::text(e, cx).trim().to_string()))
                    .filter(|(_, v)| !v.is_empty())
                    .collect(),
                name: Self::text(&self.name, cx).trim().to_string(),
                host: Self::text(&self.host, cx).trim().to_string(),
                port,
                database: Self::text(&self.database, cx).trim().to_string(),
                user: Self::text(&self.user, cx).trim().to_string(),
                ssl: self.ssl,
                folder: (!folder.is_empty()).then_some(folder),
                tag: self.tag,
                last_used: self.last_used,
                ssh,
                status_color: self.status_color,
            },
            Self::text(&self.password, cx),
        )
    }

    /// Typed SSH password / passphrase (empty = use the Keychain's).
    pub fn ssh_secret_text(&self, cx: &App) -> String {
        Self::text(&self.ssh_secret, cx)
    }

    /// The typed password, or the saved one from the Keychain.
    pub fn password_or_keychain(conn_name: &str, typed: String) -> String {
        if typed.is_empty() {
            load_password(conn_name).unwrap_or_default()
        } else {
            typed
        }
    }

    /// Load a saved connection into the form (single click / Edit).
    pub fn load(&mut self, conn: &SavedConnection, window: &mut Window, cx: &mut App) {
        let set = |e: &Entity<InputState>, v: String, window: &mut Window, cx: &mut App| {
            e.update(cx, |s, cx| s.set_value(v, window, cx));
        };
        set(&self.name, conn.name.clone(), window, cx);
        set(
            &self.path,
            conn.path.clone().unwrap_or_default(),
            window,
            cx,
        );
        for (k, e) in &self.options {
            set(e, conn.opt(k).to_string(), window, cx);
        }
        self.engine = conn.engine;
        set(&self.host, conn.host.clone(), window, cx);
        set(&self.port, conn.port.to_string(), window, cx);
        set(&self.database, conn.database.clone(), window, cx);
        set(&self.user, conn.user.clone(), window, cx);
        set(&self.password, String::new(), window, cx);
        set(
            &self.folder,
            conn.folder.clone().unwrap_or_default(),
            window,
            cx,
        );
        let ssh = conn.ssh.clone();
        set(
            &self.ssh_host,
            ssh.as_ref().map(|s| s.host.clone()).unwrap_or_default(),
            window,
            cx,
        );
        set(
            &self.ssh_port,
            ssh.as_ref()
                .map_or("22".to_string(), |s| s.port.to_string()),
            window,
            cx,
        );
        set(
            &self.ssh_user,
            ssh.as_ref().map(|s| s.user.clone()).unwrap_or_default(),
            window,
            cx,
        );
        set(
            &self.ssh_key,
            ssh.as_ref()
                .and_then(|s| s.key_path.clone())
                .unwrap_or_default(),
            window,
            cx,
        );
        set(&self.ssh_secret, String::new(), window, cx);
        self.ssh_enabled = ssh.is_some();
        self.ssh_use_key = ssh.as_ref().is_some_and(|s| s.key_path.is_some());
        self.ssl = conn.ssl;
        self.tag = conn.tag;
        self.status_color = conn.status_color;
        self.last_used = conn.last_used;
        self.editing = Some(conn.name.clone());
        self.notice = None;
    }
}
