//! "Migrate from TablePlus": import its PostgreSQL connections (with their
//! groups, tags, status colors, SSH settings and Keychain passwords) from
//! `~/Library/Application Support/com.tinyapp.TablePlus/Data`.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::db::{self, ConnTag, STATUS_COLORS, SavedConnection, SshConfig, SslMode};

fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/Application Support/com.tinyapp.TablePlus/Data")
}

/// Is there a TablePlus install to import from?
pub fn available() -> bool {
    data_dir().join("Connections.plist").is_file()
}

/// A plist as JSON (plutil ships with macOS).
fn read_plist(name: &str) -> Result<Value, String> {
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(data_dir().join(name))
        .output()
        .map_err(|e| format!("plutil: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

/// Nearest status swatch to a `#RRGGBB` color.
fn nearest_status(hex: &str) -> Option<usize> {
    let v = u32::from_str_radix(hex.trim_start_matches('#').get(..6)?, 16).ok()?;
    let ch = |c: u32, s: u32| ((c >> s) & 0xFF) as i32;
    STATUS_COLORS
        .iter()
        .enumerate()
        .min_by_key(|(_, c)| {
            (0..3)
                .map(|i| {
                    let s = 16 - i * 8;
                    (ch(v, s) - ch(**c, s)).pow(2)
                })
                .sum::<i32>()
        })
        .map(|(i, _)| i)
}

/// Where a plan's profiles come from.
#[derive(Clone, Default, PartialEq)]
pub enum Source {
    #[default]
    TablePlus,
    /// A Docker Compose project (its name = the folder).
    Compose(String),
}

/// What an import would bring in / brought in.
#[derive(Default)]
pub struct Plan {
    pub source: Source,
    /// Each profile with its TablePlus id (Compose: its password).
    pub connections: Vec<(SavedConnection, String)>,
    pub groups: Vec<String>,
    /// Unsupported / already existing profiles, by name (with the reason).
    pub skipped: Vec<String>,
}

/// The engine of a TablePlus `Driver` value.
fn engine_of(driver: &str) -> Option<crate::engine::Engine> {
    use crate::engine::Engine as E;
    let d = driver.to_lowercase().replace([' ', '_', '-'], "");
    Some(match d.as_str() {
        "postgresql" | "postgres" => E::Postgres,
        "redshift" => E::Redshift,
        "cockroachdb" | "cockroach" => E::Cockroach,
        "greenplum" => E::Greenplum,
        "vertica" => E::Vertica,
        "mysql" | "mysql8" => E::MySql,
        "mariadb" => E::MariaDb,
        "sqlite" => E::Sqlite,
        "duckdb" => E::DuckDb,
        "libsql" => E::LibSql,
        "cloudflared1" | "d1" => E::CloudflareD1,
        "sqlserver" | "microsoftsqlserver" | "mssql" => E::MsSql,
        "oracle" => E::Oracle,
        "clickhouse" | "httpclickhouse" => E::ClickHouse,
        "snowflake" => E::Snowflake,
        "bigquery" => E::BigQuery,
        "redis" => E::Redis,
        "mongodb" | "mongo" => E::MongoDb,
        "cassandra" => E::Cassandra,
        "dynamodb" => E::DynamoDb,
        _ => return None,
    })
}

/// Read TablePlus' connections; `existing` names are skipped.
pub fn plan(existing: &[String]) -> Result<Plan, String> {
    let conns = read_plist("Connections.plist")?;
    let groups = read_plist("ConnectionGroups.plist").unwrap_or(Value::Array(Vec::new()));
    let group_names: HashMap<String, String> = groups
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| {
            Some((
                g.get("ID")?.as_str()?.to_string(),
                g.get("Name")?.as_str()?.to_string(),
            ))
        })
        .collect();
    let s = |c: &Value, k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let b = |c: &Value, k: &str| c.get(k).and_then(Value::as_bool).unwrap_or(false);
    let mut plan = Plan::default();
    for c in conns.as_array().into_iter().flatten() {
        let name = s(c, "ConnectionName");
        let engine = engine_of(&s(c, "Driver"));
        if engine.is_none() || existing.contains(&name) {
            plan.skipped.push(name);
            continue;
        }
        let folder = group_names.get(&s(c, "GroupID")).cloned();
        if let Some(f) = &folder
            && !plan.groups.contains(f)
        {
            plan.groups.push(f.clone());
        }
        let ssh = b(c, "isOverSSH").then(|| SshConfig {
            host: s(c, "ServerAddress"),
            port: s(c, "ServerPort").parse().unwrap_or(22),
            user: s(c, "ServerUser"),
            key_path: b(c, "isUsePrivateKey").then(|| s(c, "ServerPrivateKeyName")),
        });
        let engine = engine.unwrap_or_default();
        let path = s(c, "DatabasePath");
        let conn = SavedConnection {
            engine,
            path: (engine.form() == crate::engine::Form::File && !path.is_empty()).then_some(path),
            options: Default::default(),
            name: name.clone(),
            host: s(c, "DatabaseHost"),
            port: s(c, "DatabasePort").parse().unwrap_or(5432),
            database: s(c, "DatabaseName"),
            user: s(c, "DatabaseUser"),
            ssl: match c.get("tLSMode").and_then(Value::as_i64) {
                Some(1) => SslMode::Disable,
                Some(2..) => SslMode::Require,
                _ => SslMode::Prefer,
            },
            folder,
            tag: ConnTag::from_label(&s(c, "Enviroment")),
            last_used: None,
            ssh,
            status_color: nearest_status(&s(c, "statusColor")),
        };
        plan.connections.push((conn, s(c, "ID")));
    }
    Ok(plan)
}

/// Import one planned profile: saved to connections.json (+ its group),
/// Keychain password copied. Returns whether a password came along.
pub fn import_one(conn: &SavedConnection, id: &str) -> Result<bool, String> {
    let read = |acct: String| {
        keyring::Entry::new("com.tableplus.TablePlus", &acct)
            .ok()
            .and_then(|e| e.get_password().ok())
            .filter(|p| !p.is_empty())
    };
    let mut copied = false;
    if let Some(pw) = read(format!("{id}_database"))
        && db::save_password(&conn.name, &pw).is_ok()
    {
        copied = true;
    }
    if conn.ssh.is_some()
        && let Some(pw) = read(format!("{id}_server"))
    {
        let _ = db::save_ssh_secret(&conn.name, &pw);
    }
    save_profile(conn)?;
    Ok(copied)
}

/// Add a profile to connections.json (unless one has its name) and its
/// folder to the groups.
pub fn save_profile(conn: &SavedConnection) -> Result<(), String> {
    let mut saved = db::load_connections();
    if !saved.iter().any(|c| c.name == conn.name) {
        saved.push(conn.clone());
        db::save_connections(&saved).map_err(|e| e.to_string())?;
    }
    if let Some(f) = &conn.folder {
        let mut groups = db::load_groups();
        if !groups.contains(f) {
            groups.push(f.clone());
            db::save_groups(&groups).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::nearest_status;

    #[test]
    fn status_colors_snap_to_swatches() {
        assert_eq!(nearest_status("#007F3D"), Some(0)); // green
        assert_eq!(nearest_status("#686B6F"), Some(1)); // grey
        assert_eq!(nearest_status("nope"), None);
    }
}
