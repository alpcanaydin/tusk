//! SQLite files (sqlx's SQLite driver), and the catalog queries LibSQL and
//! Cloudflare D1 share with it: they all speak SQLite and describe tables
//! with `sqlite_master` / `PRAGMA table_info`.

use std::sync::Arc;

use serde_json::Value;

use super::{Db, Driver, Fut, WindowReq};
use crate::db::{
    self, DbResult, GridColumnMeta, IndexDef, ObjectTree, SavedConnection, Stmt, WhereClause,
};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

/// Runs a statement and returns its rows (sqlx, HTTP, …).
pub type RowsFn = Arc<dyn Fn(String) -> Fut<Vec<Value>> + Send + Sync>;

const D: Dialect = Dialect::Sqlite;

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Tables and views (no internal `sqlite_*` / `_cf_*` / `_litestream*` ones).
pub async fn objects(rows: &RowsFn) -> DbResult<ObjectTree> {
    let r = rows(
        "SELECT name, type FROM sqlite_master
          WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'
            AND name NOT LIKE '_cf_%' AND name NOT LIKE '_litestream%'
          ORDER BY name"
            .into(),
    )
    .await?;
    let mut tree = ObjectTree::default();
    for r in r {
        let name = s(&r["name"]);
        if r["type"].as_str() == Some("view") {
            tree.views.push(name);
        } else {
            tree.tables.push(name);
        }
    }
    Ok(tree)
}

pub async fn columns(rows: &RowsFn, table: &str) -> DbResult<Vec<GridColumnMeta>> {
    let info = rows(format!("PRAGMA table_info({})", D.quote(table))).await?;
    let fks = rows(format!("PRAGMA foreign_key_list({})", D.quote(table)))
        .await
        .unwrap_or_default();
    Ok(info
        .iter()
        .map(|r| {
            let name = s(&r["name"]);
            let ty = s(&r["type"]);
            let fk = fks
                .iter()
                .find(|f| s(&f["from"]) == name)
                .map(|f| format!("{}({})", s(&f["table"]), s(&f["to"])));
            GridColumnMeta {
                pg_type: super::short_type(if ty.is_empty() { "text" } else { &ty }),
                sql_type: if ty.is_empty() { "ANY".into() } else { ty },
                nullable: r["notnull"].as_i64().unwrap_or(0) == 0 && s(&r["notnull"]) != "1",
                default: match &r["dflt_value"] {
                    Value::Null => None,
                    v => Some(s(v)),
                },
                comment: None,
                is_pk: r["pk"].as_i64().unwrap_or(0) > 0
                    || s(&r["pk"]).parse::<i64>().unwrap_or(0) > 0,
                foreign_key: fk,
                enum_values: Vec::new(),
                name,
            }
        })
        .collect())
}

pub async fn indexes(rows: &RowsFn, table: &str) -> DbResult<Vec<IndexDef>> {
    let list = rows(format!("PRAGMA index_list({})", D.quote(table))).await?;
    let mut out = Vec::new();
    for ix in list {
        let name = s(&ix["name"]);
        let cols = rows(format!("PRAGMA index_info({})", D.quote(&name)))
            .await
            .unwrap_or_default();
        let origin = s(&ix["origin"]);
        out.push(IndexDef {
            algorithm: "btree".into(),
            unique: ix["unique"].as_i64() == Some(1) || s(&ix["unique"]) == "1",
            primary: origin == "pk",
            columns: cols
                .iter()
                .map(|c| s(&c["name"]))
                .collect::<Vec<_>>()
                .join(", "),
            include: String::new(),
            condition: None,
            comment: None,
            constraint: (origin == "pk" || origin == "u").then(|| name.clone()),
            name,
        });
    }
    Ok(out)
}

pub async fn triggers(rows: &RowsFn, table: &str) -> DbResult<Vec<Value>> {
    rows(format!(
        "SELECT name AS trigger_name, sql AS definition FROM sqlite_master
          WHERE type = 'trigger' AND tbl_name = {} ORDER BY name",
        D.literal(table)
    ))
    .await
}

pub async fn script(rows: &RowsFn, kind: ObjKind, name: &str, which: Script) -> DbResult<String> {
    let target = D.quote(name);
    match which {
        Script::Create => {
            let r = rows(format!(
                "SELECT sql FROM sqlite_master WHERE name = {}",
                D.literal(name)
            ))
            .await?;
            let mut out = r.first().map(|r| s(&r["sql"])).unwrap_or_default();
            if kind == ObjKind::Table {
                let idx = rows(format!(
                    "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = {} AND sql IS NOT NULL",
                    D.literal(name)
                ))
                .await
                .unwrap_or_default();
                for i in idx {
                    out.push_str(&format!(";\n{}", s(&i["sql"])));
                }
            }
            out.push(';');
            Ok(out)
        }
        Script::Drop => Ok(format!(
            "DROP {} {target};",
            if kind == ObjKind::View {
                "VIEW"
            } else {
                "TABLE"
            }
        )),
        Script::Truncate => Ok(format!("DELETE FROM {target};")),
        w => Ok(super::dml_script(
            D,
            w,
            &target,
            &columns(rows, name).await?,
        )),
    }
}

pub fn where_sql(filter: &Option<WhereClause>) -> String {
    match filter {
        Some(w) if !w.sql.is_empty() => format!("WHERE {}", w.sql),
        _ => String::new(),
    }
}

/// `SELECT … LIMIT … OFFSET …` for a grid window, with `rowid` first when asked.
pub fn window_sql(req: &WindowReq) -> String {
    format!(
        "SELECT {}* FROM {} {} {} LIMIT {} OFFSET {}",
        if req.with_key {
            format!("CAST(rowid AS TEXT) AS {}, ", D.quote(db::CTID_COL))
        } else {
            String::new()
        },
        D.quote(&req.table),
        where_sql(&req.filter),
        req.order_by.as_deref().unwrap_or(""),
        req.limit.min(i64::MAX / 2),
        req.offset
    )
}

/// SQLite file through sqlx.
pub struct Sqlite {
    pool: sqlx::SqlitePool,
    rows: RowsFn,
}

pub async fn connect(conn: &SavedConnection) -> DbResult<Db> {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    let raw = conn.path.clone().unwrap_or_default();
    let path = shellexpand(&raw);
    let opts = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = db::run_db(async move {
        SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .map_err(|e| format!("{path}: {e}"))
    })
    .await?;
    let p = pool.clone();
    let rows: RowsFn = Arc::new(move |sql: String| {
        let p = p.clone();
        Box::pin(async move { sqlite_rows(&p, &sql, i64::MAX).await })
    });
    Ok(Db::new(Sqlite { pool, rows }))
}

/// `~/…` → the home directory.
pub fn shellexpand(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|h| h.join(rest).display().to_string())
            .unwrap_or_else(|| p.to_string()),
        None => p.to_string(),
    }
}

fn err(e: sqlx::Error) -> String {
    match e.as_database_error() {
        Some(d) => d.message().to_string(),
        None => e.to_string(),
    }
}

fn value(row: &sqlx::sqlite::SqliteRow, i: usize) -> Value {
    use sqlx::{Row as _, TypeInfo as _, ValueRef as _};
    let Ok(raw) = row.try_get_raw(i) else {
        return Value::Null;
    };
    if raw.is_null() {
        return Value::Null;
    }
    match raw.type_info().name() {
        "INTEGER" => row
            .try_get::<i64, _>(i)
            .map(Value::from)
            .unwrap_or(Value::Null),
        "REAL" => row
            .try_get::<f64, _>(i)
            .map(Value::from)
            .unwrap_or(Value::Null),
        "BLOB" => row
            .try_get::<Vec<u8>, _>(i)
            .map(|b| super::hex(&b))
            .unwrap_or(Value::Null),
        _ => row
            .try_get_unchecked::<String, _>(i)
            .map(Value::String)
            .unwrap_or(Value::Null),
    }
}

async fn sqlite_rows(pool: &sqlx::SqlitePool, sql: &str, limit: i64) -> DbResult<Vec<Value>> {
    use futures::TryStreamExt as _;
    use sqlx::{Column as _, Row as _};
    let pool = pool.clone();
    let sql = super::trim_sql(sql);
    db::run_logged(sql.clone(), crate::console::Source::Data, async move {
        let mut out = Vec::new();
        let mut stream = sqlx::query(&sql).fetch(&pool);
        while let Some(row) = stream.try_next().await.map_err(err)? {
            let names: Vec<String> = row.columns().iter().map(|c| c.name().to_string()).collect();
            let vals = (0..names.len()).map(|i| value(&row, i)).collect();
            out.extend(super::objects_from(&names, vec![vals]));
            if out.len() as i64 >= limit {
                break;
            }
        }
        Ok(out)
    })
    .await
}

async fn run_stmt(conn: &mut sqlx::SqliteConnection, stmt: &Stmt) -> Result<u64, sqlx::Error> {
    use sqlx::Executor as _;
    let mut q = sqlx::query(&stmt.sql);
    for p in &stmt.params {
        q = q.bind(p.clone());
    }
    Ok(conn.execute(q).await?.rows_affected())
}

impl Driver for Sqlite {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }
    fn default_schema(&self) -> Option<String> {
        Some("main".into())
    }
    fn version(&self) -> Fut<String> {
        let rows = self.rows.clone();
        Box::pin(async move {
            let r = rows("SELECT sqlite_version() AS v".into()).await?;
            Ok(format!(
                "SQLite {}",
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
        Box::pin(async move { objects(&rows).await })
    }
    fn columns(&self, _schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let rows = self.rows.clone();
        Box::pin(async move { columns(&rows, &table).await })
    }
    fn count(&self, _schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let rows = self.rows.clone();
        Box::pin(async move {
            let r = rows(format!(
                "SELECT COUNT(*) AS n FROM {} {}",
                D.quote(&table),
                where_sql(&filter)
            ))
            .await?;
            Ok(r.first().and_then(|r| r["n"].as_i64()).unwrap_or(0))
        })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let pool = self.pool.clone();
        Box::pin(async move { sqlite_rows(&pool, &window_sql(&req), req.limit).await })
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let pool = self.pool.clone();
        Box::pin(async move { sqlite_rows(&pool, &sql, limit).await })
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            use sqlx::{Column as _, Executor as _};
            let sql = super::trim_sql(&sql);
            db::run_db(async move {
                let d = pool.describe(&sql).await.map_err(err)?;
                Ok(d.columns().iter().map(|c| c.name().to_string()).collect())
            })
            .await
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let pool = self.pool.clone();
        Box::pin(async move {
            db::run_logged(sql.clone(), crate::console::Source::Data, async move {
                Ok(sqlx::raw_sql(&sql)
                    .execute(&pool)
                    .await
                    .map_err(err)?
                    .rows_affected())
            })
            .await
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let pool = self.pool.clone();
        Box::pin(async move {
            db::run_db(async move {
                let mut tx = pool.begin().await.map_err(err)?;
                let mut affected = 0;
                for stmt in &stmts {
                    let started = std::time::Instant::now();
                    let n = run_stmt(&mut tx, stmt).await.map_err(|e| {
                        let msg = err(e);
                        crate::console::record(
                            &stmt.sql,
                            started,
                            crate::console::Source::Data,
                            Some(&msg),
                        );
                        format!("{msg}\n  in: {}", stmt.sql)
                    })?;
                    crate::console::record(&stmt.sql, started, crate::console::Source::Data, None);
                    affected += n;
                }
                tx.commit().await.map_err(err)?;
                Ok(affected)
            })
            .await
        })
    }
    fn script(&self, kind: ObjKind, _schema: String, name: String, which: Script) -> Fut<String> {
        let rows = self.rows.clone();
        Box::pin(async move { script(&rows, kind, &name, which).await })
    }
    fn triggers(&self, _schema: String, table: String) -> Fut<Vec<Value>> {
        let rows = self.rows.clone();
        Box::pin(async move { triggers(&rows, &table).await })
    }
    fn indexes(&self, _schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let rows = self.rows.clone();
        Box::pin(async move { indexes(&rows, &table).await })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_sqlite_file() {
        let dir = std::env::temp_dir().join(format!("tusk-sqlite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let _ = std::fs::remove_file(&path);
        let mut c = crate::drivers::live::conn(crate::engine::Engine::Sqlite, 0, "", "");
        c.path = Some(path.display().to_string());
        let rt = crate::db::runtime();
        let db = rt.block_on(super::connect(&c)).unwrap();
        rt.block_on(
            db.driver().exec(
                "CREATE TABLE people (id INTEGER PRIMARY KEY, email TEXT, n REAL); \
             INSERT INTO people (email, n) VALUES ('a@x', 1.5), ('b@x', NULL);"
                    .into(),
            ),
        )
        .unwrap();
        crate::drivers::live::exercise(c, "", "people", "email");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
