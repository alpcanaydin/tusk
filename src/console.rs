//! Query log: every statement Tusk sends (the Console panel, live) and the
//! ones the user ran from a SQL tab (History, kept on disk).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Who sent a statement: the user (SQL editor, grid saves) or Tusk itself
/// (sidebar lists, row counts, metadata).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Source {
    Data,
    Meta,
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub at: chrono::DateTime<chrono::Local>,
    pub sql: String,
    pub ms: u128,
    pub source: Source,
    /// None on success, else the error text.
    pub error: Option<String>,
}

const LOG_CAP: usize = 2000;

static LOG: Mutex<Vec<LogLine>> = Mutex::new(Vec::new());
/// Bumped on every append: the panel repaints when it changes.
static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn seq() -> u64 {
    SEQ.load(Ordering::Relaxed)
}

pub fn record(sql: &str, started: Instant, source: Source, error: Option<&str>) {
    let line = LogLine {
        at: chrono::Local::now(),
        sql: sql.trim().to_string(),
        ms: started.elapsed().as_millis(),
        source,
        error: error.map(str::to_string),
    };
    if let Ok(mut log) = LOG.lock() {
        log.push(line);
        let n = log.len();
        if n > LOG_CAP {
            log.drain(..n - LOG_CAP);
        }
    }
    SEQ.fetch_add(1, Ordering::Relaxed);
}

/// Time `fut` and log `sql` with its outcome.
pub async fn logged<T>(
    sql: &str,
    source: Source,
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    let started = Instant::now();
    let r = fut.await;
    record(sql, started, source, r.as_ref().err().map(String::as_str));
    r
}

pub fn lines() -> Vec<LogLine> {
    LOG.lock().map(|l| l.clone()).unwrap_or_default()
}

pub fn clear() {
    if let Ok(mut log) = LOG.lock() {
        log.clear();
    }
    SEQ.fetch_add(1, Ordering::Relaxed);
}

// ---- History (user-run statements, persisted) ----

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryItem {
    pub at: chrono::DateTime<chrono::Local>,
    pub sql: String,
    pub connection: String,
    pub database: String,
    pub ms: u128,
    pub ok: bool,
}

const HISTORY_CAP: usize = 1000;

fn history_path() -> std::path::PathBuf {
    crate::db::app_dir().join("history.json")
}

static HISTORY: Mutex<Option<Vec<HistoryItem>>> = Mutex::new(None);

fn with_history<R>(f: impl FnOnce(&mut Vec<HistoryItem>) -> R) -> R {
    let mut guard = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    let items = guard.get_or_insert_with(|| {
        std::fs::read(history_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    });
    f(items)
}

fn save_history(items: &[HistoryItem]) {
    let path = history_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(b) = serde_json::to_vec(items) {
        let _ = std::fs::write(path, b);
    }
}

pub fn add_history(item: HistoryItem) {
    with_history(|items| {
        // Re-running the same text moves it to the top instead of piling up.
        items.retain(|i| !(i.sql == item.sql && i.connection == item.connection));
        items.push(item);
        let n = items.len();
        if n > HISTORY_CAP {
            items.drain(..n - HISTORY_CAP);
        }
        save_history(items);
    });
    SEQ.fetch_add(1, Ordering::Relaxed);
}

/// Newest first, for one connection.
pub fn history(connection: &str) -> Vec<HistoryItem> {
    with_history(|items| {
        items
            .iter()
            .rev()
            .filter(|i| i.connection == connection)
            .cloned()
            .collect()
    })
}

pub fn remove_history(at: chrono::DateTime<chrono::Local>, sql: &str) {
    with_history(|items| {
        items.retain(|i| !(i.at == at && i.sql == sql));
        save_history(items);
    });
    SEQ.fetch_add(1, Ordering::Relaxed);
}

pub fn clear_history(connection: &str) {
    with_history(|items| {
        items.retain(|i| i.connection != connection);
        save_history(items);
    });
    SEQ.fetch_add(1, Ordering::Relaxed);
}
