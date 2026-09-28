//! LibSQL / Turso over the Hrana HTTP protocol (`/v2/pipeline`). SQLite
//! underneath: the catalog queries are shared with the SQLite driver.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::{Value, json};

use super::sqlite::{self as lite, RowsFn};
use super::{Db, Driver, Fut, WindowReq, http};
use crate::db::{
    self, DbResult, GridColumnMeta, IndexDef, ObjectTree, SavedConnection, Stmt, WhereClause,
};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

const D: Dialect = Dialect::Sqlite;

#[derive(Clone)]
pub struct Hrana {
    url: String,
    token: String,
}

pub struct LibSql {
    h: Hrana,
    rows: RowsFn,
}

/// `libsql://db.turso.io` → `https://db.turso.io` (the HTTP endpoint).
pub fn http_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    for (from, to) in [
        ("libsql://", "https://"),
        ("wss://", "https://"),
        ("ws://", "http://"),
    ] {
        if let Some(rest) = u.strip_prefix(from) {
            return format!("{to}{rest}");
        }
    }
    if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("https://{u}")
    }
}

pub async fn connect(conn: &SavedConnection, token: String) -> DbResult<Db> {
    let h = Hrana {
        url: http_url(conn.path.as_deref().unwrap_or("")),
        token,
    };
    h.execute("SELECT 1".into(), Vec::new()).await?;
    let hh = h.clone();
    let rows: RowsFn = Arc::new(move |sql: String| {
        let h = hh.clone();
        Box::pin(async move { h.rows(sql).await })
    });
    Ok(Db::new(LibSql { h, rows }))
}

fn arg(v: &Option<String>) -> Value {
    match v {
        Some(s) => json!({"type": "text", "value": s}),
        None => json!({"type": "null"}),
    }
}

/// A Hrana value as JSON.
fn value(v: &Value) -> Value {
    match v["type"].as_str() {
        Some("integer") => v["value"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .map(Value::from)
            .unwrap_or(Value::Null),
        Some("float") => v["value"].clone(),
        Some("text") => v["value"].clone(),
        Some("blob") => base64::engine::general_purpose::STANDARD
            .decode(v["base64"].as_str().unwrap_or(""))
            .map(|b| super::hex(&b))
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// The error of a failed pipeline / batch step.
fn step_error(r: &Value) -> Option<String> {
    (r["type"] == "error").then(|| {
        r["error"]["message"]
            .as_str()
            .unwrap_or("error")
            .to_string()
    })
}

impl Hrana {
    async fn pipeline(&self, requests: Vec<Value>) -> DbResult<Value> {
        let mut req = http::client()
            .post(format!("{}/v2/pipeline", self.url))
            .json(&json!({ "requests": requests }));
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        http::send_json(req).await
    }

    /// One statement: (columns, rows, affected).
    async fn execute(
        &self,
        sql: String,
        args: Vec<Option<String>>,
    ) -> DbResult<(Vec<String>, Vec<Vec<Value>>, u64)> {
        let stmt = json!({ "sql": sql, "args": args.iter().map(arg).collect::<Vec<_>>() });
        let v = self
            .pipeline(vec![
                json!({"type": "execute", "stmt": stmt}),
                json!({"type": "close"}),
            ])
            .await?;
        let r = &v["results"][0];
        if let Some(e) = step_error(r) {
            return Err(e);
        }
        let res = &r["response"]["result"];
        let cols = res["cols"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| c["name"].as_str().unwrap_or("").to_string())
            .collect();
        let rows = res["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| r.as_array().into_iter().flatten().map(value).collect())
            .collect();
        Ok((cols, rows, res["affected_row_count"].as_u64().unwrap_or(0)))
    }

    async fn rows(&self, sql: String) -> DbResult<Vec<Value>> {
        let sql = super::trim_sql(&sql);
        let this = self.clone();
        let label = sql.clone();
        db::run_logged(label, crate::console::Source::Data, async move {
            let (cols, rows, _) = this.execute(sql, Vec::new()).await?;
            Ok(super::objects_from(&cols, rows))
        })
        .await
    }

    /// Statements in one transaction: BEGIN, each step only after the last
    /// succeeded, COMMIT — or ROLLBACK when one failed.
    async fn transaction(&self, stmts: Vec<Stmt>) -> DbResult<u64> {
        let n = stmts.len();
        let mut steps = vec![json!({"stmt": {"sql": "BEGIN"}})];
        for (i, st) in stmts.iter().enumerate() {
            steps.push(json!({
                "stmt": {"sql": st.sql, "args": st.params.iter().map(arg).collect::<Vec<_>>()},
                "condition": {"type": "ok", "step": i}
            }));
        }
        steps.push(json!({"stmt": {"sql": "COMMIT"}, "condition": {"type": "ok", "step": n}}));
        steps.push(json!({"stmt": {"sql": "ROLLBACK"}, "condition": {"type": "not", "cond": {"type": "ok", "step": n + 1}}}));
        let v = self
            .pipeline(vec![
                json!({"type": "batch", "batch": {"steps": steps}}),
                json!({"type": "close"}),
            ])
            .await?;
        let r = &v["results"][0];
        if let Some(e) = step_error(r) {
            return Err(e);
        }
        let results = &r["response"]["result"]["step_results"];
        let errors = &r["response"]["result"]["step_errors"];
        for i in 1..=n {
            if let Some(msg) = errors[i]["message"].as_str() {
                return Err(format!("{msg}\n  in: {}", stmts[i - 1].sql));
            }
        }
        Ok((1..=n)
            .map(|i| results[i]["affected_row_count"].as_u64().unwrap_or(0))
            .sum())
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

impl Driver for LibSql {
    fn engine(&self) -> Engine {
        Engine::LibSql
    }
    fn default_schema(&self) -> Option<String> {
        Some("main".into())
    }
    fn version(&self) -> Fut<String> {
        let rows = self.rows.clone();
        Box::pin(async move {
            let r = rows("SELECT sqlite_version() AS v".into()).await?;
            Ok(format!(
                "LibSQL (SQLite {})",
                r.first().map(|r| s(&r["v"])).unwrap_or_default()
            ))
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        Box::pin(async { Ok(vec!["main".to_string()]) })
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        Box::pin(async { Ok(vec!["main".to_string()]) })
    }
    fn objects(&self, _schema: String) -> Fut<ObjectTree> {
        let rows = self.rows.clone();
        Box::pin(async move { lite::objects(&rows).await })
    }
    fn columns(&self, _schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let rows = self.rows.clone();
        Box::pin(async move { lite::columns(&rows, &table).await })
    }
    fn count(&self, _schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let rows = self.rows.clone();
        Box::pin(async move {
            let r = rows(format!(
                "SELECT COUNT(*) AS n FROM {} {}",
                D.quote(&table),
                lite::where_sql(&filter)
            ))
            .await?;
            Ok(r.first().and_then(|r| r["n"].as_i64()).unwrap_or(0))
        })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        (self.rows)(lite::window_sql(&req))
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let f = (self.rows)(sql);
        Box::pin(async move {
            let mut r = f.await?;
            r.truncate(limit.max(0) as usize);
            Ok(r)
        })
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let h = self.h.clone();
        Box::pin(async move {
            let (cols, _, _) = h
                .execute(
                    format!("SELECT * FROM ({}) LIMIT 0", super::trim_sql(&sql)),
                    Vec::new(),
                )
                .await?;
            Ok(cols)
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let h = self.h.clone();
        Box::pin(async move {
            let label = sql.clone();
            db::run_logged(label, crate::console::Source::Data, async move {
                let stmts: Vec<Stmt> = db::split_statements(&sql)
                    .into_iter()
                    .map(Stmt::plain)
                    .collect();
                if stmts.len() > 1 {
                    return h.transaction(stmts).await;
                }
                Ok(h.execute(super::trim_sql(&sql), Vec::new()).await?.2)
            })
            .await
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let h = self.h.clone();
        Box::pin(async move {
            let started = std::time::Instant::now();
            let label = stmts
                .iter()
                .map(|s| s.sql.as_str())
                .collect::<Vec<_>>()
                .join(";\n");
            let r = h.transaction(stmts).await;
            crate::console::record(
                &label,
                started,
                crate::console::Source::Data,
                r.as_ref().err().map(String::as_str),
            );
            r
        })
    }
    fn script(&self, kind: ObjKind, _schema: String, name: String, which: Script) -> Fut<String> {
        let rows = self.rows.clone();
        Box::pin(async move { lite::script(&rows, kind, &name, which).await })
    }
    fn triggers(&self, _schema: String, table: String) -> Fut<Vec<Value>> {
        let rows = self.rows.clone();
        Box::pin(async move { lite::triggers(&rows, &table).await })
    }
    fn indexes(&self, _schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let rows = self.rows.clone();
        Box::pin(async move { lite::indexes(&rows, &table).await })
    }
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn urls() {
        assert_eq!(
            super::http_url("libsql://db-me.turso.io"),
            "https://db-me.turso.io"
        );
        assert_eq!(
            super::http_url("http://127.0.0.1:8080/"),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn live_libsql_server() {
        if !live::reachable(38080) {
            return;
        }
        let mut c = live::conn(Engine::LibSql, 0, "", "");
        c.path = Some("http://127.0.0.1:38080".into());
        let rt = crate::db::runtime();
        let db = rt.block_on(super::connect(&c, String::new())).unwrap();
        rt.block_on(db.driver().exec(
            "DROP TABLE IF EXISTS people; CREATE TABLE people (id INTEGER PRIMARY KEY, email TEXT, n REAL);
             INSERT INTO people (email, n) VALUES ('a@x', 1.5), ('b@x', NULL);"
                .into(),
        ))
        .unwrap();
        live::exercise(c, "", "people", "email");
    }
}
