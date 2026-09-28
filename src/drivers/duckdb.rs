//! DuckDB files (the embedded engine, built into the app). DuckDB's API is
//! synchronous: every call runs on a blocking thread with the connection
//! behind a mutex.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::{Db, Driver, Fut, WindowReq};
use crate::db::{
    self, DbResult, GridColumnMeta, IndexDef, ObjectTree, SavedConnection, Stmt, WhereClause,
};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

const D: Dialect = Dialect::DuckDb;

pub struct Duck {
    conn: Arc<Mutex<duckdb::Connection>>,
}

pub async fn connect(conn: &SavedConnection) -> DbResult<Db> {
    let path = super::sqlite::shellexpand(&conn.path.clone().unwrap_or_default());
    let c =
        tokio_blocking(move || duckdb::Connection::open(&path).map_err(|e| format!("{path}: {e}")))
            .await?;
    Ok(Db::new(Duck {
        conn: Arc::new(Mutex::new(c)),
    }))
}

async fn tokio_blocking<T: Send + 'static>(
    f: impl FnOnce() -> DbResult<T> + Send + 'static,
) -> DbResult<T> {
    db::runtime()
        .spawn_blocking(f)
        .await
        .map_err(|e| format!("duckdb task failed: {e}"))?
}

fn micros_to(unit: duckdb::types::TimeUnit, v: i64) -> i64 {
    use duckdb::types::TimeUnit::*;
    match unit {
        Second => v * 1_000_000,
        Millisecond => v * 1_000,
        Microsecond => v,
        Nanosecond => v / 1_000,
    }
}

/// A DuckDB value as JSON (dates / times as ISO text, nested values as JSON).
fn json(v: duckdb::types::Value) -> Value {
    use duckdb::types::Value as V;
    match v {
        V::Null => Value::Null,
        V::Boolean(b) => Value::Bool(b),
        V::TinyInt(n) => n.into(),
        V::SmallInt(n) => n.into(),
        V::Int(n) => n.into(),
        V::BigInt(n) => n.into(),
        V::UTinyInt(n) => n.into(),
        V::USmallInt(n) => n.into(),
        V::UInt(n) => n.into(),
        V::UBigInt(n) => n.into(),
        V::HugeInt(n) => Value::String(n.to_string()),
        V::UHugeInt(n) => Value::String(n.to_string()),
        V::Float(f) => Value::from(f as f64),
        V::Double(f) => Value::from(f),
        V::Decimal(d) => Value::String(d.to_string()),
        V::Text(s) => Value::String(s),
        V::Blob(b) => super::hex(&b),
        V::Date32(days) => chrono::NaiveDate::from_num_days_from_ce_opt(days + 719_163)
            .map(|d| Value::String(d.to_string()))
            .unwrap_or(Value::Null),
        V::Time64(unit, t) => {
            let us = micros_to(unit, t);
            chrono::NaiveTime::from_num_seconds_from_midnight_opt(
                (us / 1_000_000) as u32,
                ((us % 1_000_000) * 1000) as u32,
            )
            .map(|t| Value::String(t.to_string()))
            .unwrap_or(Value::Null)
        }
        V::Timestamp(unit, t) => chrono::DateTime::from_timestamp_micros(micros_to(unit, t))
            .map(|d| Value::String(d.naive_utc().to_string()))
            .unwrap_or(Value::Null),
        V::List(items) | V::Array(items) => Value::Array(items.into_iter().map(json).collect()),
        V::Struct(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), json(v.clone())))
                .collect(),
        ),
        V::Enum(s) => Value::String(s),
        other => Value::String(format!("{other:?}")),
    }
}

fn rows_sync(conn: &duckdb::Connection, sql: &str, limit: i64) -> DbResult<Vec<Value>> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let names: Vec<String> = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let vals: Vec<Value> = (0..names.len())
            .map(|i| {
                row.get::<_, duckdb::types::Value>(i)
                    .map(json)
                    .unwrap_or(Value::Null)
            })
            .collect();
        out.extend(super::objects_from(&names, vec![vals]));
        if out.len() as i64 >= limit {
            break;
        }
    }
    Ok(out)
}

impl Duck {
    fn rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let conn = self.conn.clone();
        let sql = super::trim_sql(&sql);
        Box::pin(async move {
            let label = sql.clone();
            db::run_logged(label, crate::console::Source::Data, async move {
                tokio_blocking(move || rows_sync(&conn.lock().unwrap(), &sql, limit)).await
            })
            .await
        })
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

fn where_sql(filter: &Option<WhereClause>) -> String {
    match filter {
        Some(w) if !w.sql.is_empty() => format!("WHERE {}", w.sql),
        _ => String::new(),
    }
}

impl Driver for Duck {
    fn engine(&self) -> Engine {
        Engine::DuckDb
    }
    fn default_schema(&self) -> Option<String> {
        Some("main".into())
    }
    fn version(&self) -> Fut<String> {
        let f = self.rows("SELECT version() AS v".into(), 1);
        Box::pin(async move {
            Ok(format!(
                "DuckDB {}",
                f.await?.first().map(|r| s(&r["v"])).unwrap_or_default()
            ))
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        let f = self.rows(
            "SELECT database_name AS n FROM duckdb_databases() WHERE NOT internal".into(),
            1000,
        );
        Box::pin(async move { Ok(f.await?.iter().map(|r| s(&r["n"])).collect()) })
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let f = self.rows(
            "SELECT DISTINCT schema_name AS n FROM duckdb_schemas()
              WHERE database_name = current_database()
                AND schema_name NOT IN ('information_schema', 'pg_catalog')
              ORDER BY CASE WHEN schema_name = 'main' THEN 0 ELSE 1 END, n"
                .into(),
            1000,
        );
        Box::pin(async move { Ok(f.await?.iter().map(|r| s(&r["n"])).collect()) })
    }
    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let t = self.rows(
            format!(
                "SELECT table_name AS n, table_type AS k FROM information_schema.tables
                  WHERE table_schema = {} AND table_catalog = current_database() ORDER BY table_name",
                D.literal(&schema)
            ),
            100_000,
        );
        Box::pin(async move {
            let mut tree = ObjectTree::default();
            for r in t.await? {
                if s(&r["k"]) == "VIEW" {
                    tree.views.push(s(&r["n"]));
                } else {
                    tree.tables.push(s(&r["n"]));
                }
            }
            Ok(tree)
        })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let f = self.rows(
            format!(
                "SELECT c.column_name AS name, c.data_type AS ty, c.is_nullable AS nullable,
                        c.column_default AS dflt,
                        (SELECT d.comment FROM duckdb_columns() d
                          WHERE d.schema_name = c.table_schema AND d.table_name = c.table_name
                            AND d.column_name = c.column_name AND d.database_name = current_database()) AS cmt,
                        EXISTS (SELECT 1 FROM duckdb_constraints() k
                                 WHERE k.schema_name = c.table_schema AND k.table_name = c.table_name
                                   AND k.constraint_type = 'PRIMARY KEY'
                                   AND list_contains(k.constraint_column_names, c.column_name)) AS pk
                   FROM information_schema.columns c
                  WHERE c.table_schema = {} AND c.table_name = {} AND c.table_catalog = current_database()
                  ORDER BY c.ordinal_position",
                D.literal(&schema),
                D.literal(&table)
            ),
            10_000,
        );
        Box::pin(async move {
            Ok(f.await?
                .iter()
                .map(|r| {
                    let ty = s(&r["ty"]);
                    GridColumnMeta {
                        name: s(&r["name"]),
                        pg_type: super::short_type(&ty),
                        sql_type: ty,
                        nullable: s(&r["nullable"]) != "NO",
                        default: r["dflt"].as_str().map(str::to_string),
                        comment: r["cmt"]
                            .as_str()
                            .filter(|c| !c.is_empty())
                            .map(str::to_string),
                        is_pk: r["pk"] == Value::Bool(true),
                        foreign_key: None,
                        enum_values: Vec::new(),
                    }
                })
                .collect())
        })
    }
    fn count(&self, schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let f = self.rows(
            format!(
                "SELECT COUNT(*) AS n FROM {} {}",
                D.qualified(&schema, &table, true),
                where_sql(&filter)
            ),
            1,
        );
        Box::pin(async move { Ok(f.await?.first().and_then(|r| r["n"].as_i64()).unwrap_or(0)) })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let sql = format!(
            "SELECT {}* FROM {} {} {} LIMIT {} OFFSET {}",
            if req.with_key {
                format!("CAST(rowid AS VARCHAR) AS {}, ", D.quote(db::CTID_COL))
            } else {
                String::new()
            },
            D.qualified(&req.schema, &req.table, true),
            where_sql(&req.filter),
            req.order_by.as_deref().unwrap_or(""),
            req.limit.min(i64::MAX / 2),
            req.offset
        );
        self.rows(sql, req.limit)
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        self.rows(sql, limit)
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let conn = self.conn.clone();
        let sql = super::trim_sql(&sql);
        Box::pin(async move {
            tokio_blocking(move || {
                let c = conn.lock().unwrap();
                let mut stmt = c
                    .prepare(&format!("SELECT * FROM ({sql}) LIMIT 0"))
                    .map_err(|e| e.to_string())?;
                let rows = stmt.query([]).map_err(|e| e.to_string())?;
                Ok(rows.as_ref().map(|s| s.column_names()).unwrap_or_default())
            })
            .await
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let conn = self.conn.clone();
        Box::pin(async move {
            let label = sql.clone();
            db::run_logged(label, crate::console::Source::Data, async move {
                tokio_blocking(move || {
                    let c = conn.lock().unwrap();
                    // Several statements: run as a batch (no count).
                    if crate::db::split_statements(&sql).len() > 1 {
                        c.execute_batch(&sql).map_err(|e| e.to_string())?;
                        return Ok(0);
                    }
                    c.execute(&sql, [])
                        .map(|n| n as u64)
                        .map_err(|e| e.to_string())
                })
                .await
            })
            .await
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let conn = self.conn.clone();
        Box::pin(async move {
            tokio_blocking(move || {
                let mut c = conn.lock().unwrap();
                let tx = c.transaction().map_err(|e| e.to_string())?;
                let mut affected = 0u64;
                for st in &stmts {
                    let started = std::time::Instant::now();
                    let params: Vec<&dyn duckdb::ToSql> =
                        st.params.iter().map(|p| p as &dyn duckdb::ToSql).collect();
                    let n = tx.execute(&st.sql, params.as_slice()).map_err(|e| {
                        let msg = e.to_string();
                        crate::console::record(
                            &st.sql,
                            started,
                            crate::console::Source::Data,
                            Some(&msg),
                        );
                        format!("{msg}\n  in: {}", st.sql)
                    })?;
                    crate::console::record(&st.sql, started, crate::console::Source::Data, None);
                    affected += n as u64;
                }
                tx.commit().map_err(|e| e.to_string())?;
                Ok(affected)
            })
            .await
        })
    }
    fn script(&self, kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String> {
        let target = D.qualified(&schema, &name, true);
        let ddl = self.rows(
            format!(
                "SELECT sql FROM duckdb_tables() WHERE schema_name = {s} AND table_name = {n}
                 UNION ALL SELECT sql FROM duckdb_views() WHERE schema_name = {s} AND view_name = {n}",
                s = D.literal(&schema),
                n = D.literal(&name)
            ),
            1,
        );
        let cols = self.columns(schema, name);
        Box::pin(async move {
            match which {
                Script::Create => Ok(ddl.await?.first().map(|r| s(&r["sql"])).unwrap_or_default()),
                Script::Drop if kind == ObjKind::View => Ok(format!("DROP VIEW {target};")),
                w => Ok(super::dml_script(D, w, &target, &cols.await?)),
            }
        })
    }
    fn indexes(&self, schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let f = self.rows(
            format!(
                "SELECT index_name AS n, is_unique AS u, is_primary AS p, expressions AS e
                   FROM duckdb_indexes() WHERE schema_name = {} AND table_name = {}",
                D.literal(&schema),
                D.literal(&table)
            ),
            1000,
        );
        Box::pin(async move {
            Ok(f.await
                .unwrap_or_default()
                .iter()
                .map(|r| IndexDef {
                    name: s(&r["n"]),
                    algorithm: "art".into(),
                    unique: r["u"] == Value::Bool(true),
                    primary: r["p"] == Value::Bool(true),
                    columns: s(&r["e"]).trim_matches(['[', ']']).to_string(),
                    include: String::new(),
                    condition: None,
                    comment: None,
                    constraint: None,
                })
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_duckdb_file() {
        let dir = std::env::temp_dir().join(format!("tusk-duck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.duckdb");
        let mut c = crate::drivers::live::conn(crate::engine::Engine::DuckDb, 0, "", "");
        c.path = Some(path.display().to_string());
        let rt = crate::db::runtime();
        let db = rt.block_on(super::connect(&c)).unwrap();
        rt.block_on(db.driver().exec(
            "CREATE TABLE people (id INTEGER PRIMARY KEY, email VARCHAR, born DATE, seen_at TIMESTAMP, tags VARCHAR[]);
             INSERT INTO people VALUES (1, 'a@x', DATE '1990-01-02', TIMESTAMP '2026-01-02 03:04:05', ['a','b']),
                                       (2, 'b@x', NULL, NULL, NULL);"
                .into(),
        ))
        .unwrap();
        drop(db);
        crate::drivers::live::exercise(c.clone(), "", "people", "email");
        let db = rt.block_on(super::connect(&c)).unwrap();
        let rows = rt
            .block_on(
                db.driver()
                    .query_rows("SELECT * FROM people ORDER BY id".into(), 10),
            )
            .unwrap();
        assert_eq!(rows[0]["born"], "1990-01-02");
        assert_eq!(rows[0]["seen_at"], "2026-01-02 03:04:05");
        assert_eq!(rows[0]["tags"], serde_json::json!(["a", "b"]));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
