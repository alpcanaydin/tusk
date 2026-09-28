//! Oracle Database over TNS in pure Rust (no Instant Client). Schemas are
//! users; the catalog is `ALL_*`. Rows are keyed by ROWID; parameters are
//! `:n` bound as text (the session's NLS formats read dates back in).

use std::sync::Arc;

use oracle_rs::constants::OracleType;
use oracle_rs::{Config, Connection, Value as OValue};
use serde_json::Value;

use super::{Db, Driver, Fut, WindowReq};
use crate::db::{
    self, DbResult, GridColumnMeta, IndexDef, ObjectTree, SavedConnection, SslMode, Stmt,
    WhereClause,
};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

const D: Dialect = Dialect::Oracle;
/// Rows in one reply (the driver's prefetch size).
const BATCH: usize = 100;

pub struct Oracle {
    session: Arc<Session>,
    user: String,
}

/// The connection, reopened when the driver library leaves it unusable
/// (after some server errors it stays out of the ready state).
struct Session {
    cfg: Config,
    conn: tokio::sync::Mutex<Arc<Connection>>,
}

impl Session {
    async fn open(cfg: &Config) -> DbResult<Connection> {
        let c = Connection::connect_with_config(cfg.clone())
            .await
            .map_err(err)?;
        // Dates / timestamps as ISO text both ways.
        for (k, v) in [
            ("NLS_DATE_FORMAT", "YYYY-MM-DD HH24:MI:SS"),
            ("NLS_TIMESTAMP_FORMAT", "YYYY-MM-DD HH24:MI:SS.FF6"),
            (
                "NLS_TIMESTAMP_TZ_FORMAT",
                "YYYY-MM-DD HH24:MI:SS.FF6 TZH:TZM",
            ),
            ("NLS_NUMERIC_CHARACTERS", ".,"),
        ] {
            // The setting applies even when the reply trips the decoder.
            let _ = c
                .execute(&format!("ALTER SESSION SET {k} = '{v}'"), &[])
                .await;
        }
        Ok(c)
    }

    /// A ready connection (on the tokio runtime).
    async fn get(&self) -> DbResult<Arc<Connection>> {
        let mut c = self.conn.lock().await;
        if c.is_closed() || c.state().await != oracle_rs::ConnectionState::Ready {
            *c = Arc::new(Self::open(&self.cfg).await?);
        }
        Ok(c.clone())
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub async fn connect(
    conn: &SavedConnection,
    host: String,
    port: u16,
    password: String,
) -> DbResult<Db> {
    let service = match conn.database.trim() {
        "" => "FREEPDB1".to_string(),
        s => s.to_string(),
    };
    let user = conn.user.clone();
    let ssl = conn.ssl;
    let (c, user) = db::run_db(async move {
        let mut cfg = match service.strip_prefix("SID:") {
            Some(sid) => Config::with_sid(host, port, sid.trim(), user.clone(), password),
            None => Config::new(host, port, service, user.clone(), password),
        };
        if ssl == SslMode::Require {
            cfg = cfg.with_tls().map_err(err)?;
        }
        let c = Session::open(&cfg).await?;
        Ok((
            Session {
                cfg,
                conn: tokio::sync::Mutex::new(Arc::new(c)),
            },
            user,
        ))
    })
    .await?;
    Ok(Db::new(Oracle {
        session: Arc::new(c),
        user: user.to_uppercase(),
    }))
}

fn two(n: u8) -> String {
    format!("{n:02}")
}

/// An Oracle value as the grid's JSON.
fn json_of(v: OValue) -> Value {
    match v {
        OValue::Null => Value::Null,
        OValue::String(s) => Value::String(s),
        OValue::Bytes(b) => super::hex(&b),
        OValue::Integer(n) => n.into(),
        OValue::Float(f) => Value::from(f),
        OValue::Number(n) => {
            let t = n.as_str().to_string();
            match t.parse::<i64>() {
                Ok(i) if n.is_integer => i.into(),
                _ => Value::String(t),
            }
        }
        OValue::Date(d) => Value::String(format!(
            "{}-{}-{} {}:{}:{}",
            d.year,
            two(d.month),
            two(d.day),
            two(d.hour),
            two(d.minute),
            two(d.second)
        )),
        OValue::Timestamp(t) => {
            let mut s = format!(
                "{}-{}-{} {}:{}:{}",
                t.year,
                two(t.month),
                two(t.day),
                two(t.hour),
                two(t.minute),
                two(t.second)
            );
            if t.microsecond > 0 {
                s.push_str(&format!(".{:06}", t.microsecond));
            }
            if t.has_timezone() {
                let sign = if t.tz_hour_offset < 0 || t.tz_minute_offset < 0 {
                    '-'
                } else {
                    '+'
                };
                s.push_str(&format!(
                    "{sign}{:02}:{:02}",
                    t.tz_hour_offset.unsigned_abs(),
                    t.tz_minute_offset.unsigned_abs()
                ));
            }
            Value::String(s)
        }
        OValue::RowId(r) => r.to_string().map(Value::String).unwrap_or(Value::Null),
        OValue::Boolean(b) => Value::Bool(b),
        OValue::Lob(l) => match l {
            oracle_rs::types::LobValue::Inline(b) => match String::from_utf8(b.to_vec()) {
                Ok(s) => Value::String(s),
                Err(e) => super::hex(e.as_bytes()),
            },
            oracle_rs::types::LobValue::Null => Value::Null,
            oracle_rs::types::LobValue::Empty => Value::String(String::new()),
            oracle_rs::types::LobValue::Locator(_) => Value::String("(LOB)".into()),
        },
        OValue::Json(j) => j,
        other => Value::String(format!("{other:?}")),
    }
}

/// Numbers arrive as text: integers become JSON numbers, binary floats
/// too; other NUMBERs stay text (full precision).
fn typed(v: Value, kind: OracleType) -> Value {
    let Value::String(t) = &v else {
        return v;
    };
    match kind {
        OracleType::Number | OracleType::BinaryInteger => {
            t.parse::<i64>().map(Value::from).unwrap_or(v)
        }
        OracleType::BinaryFloat | OracleType::BinaryDouble => {
            t.parse::<f64>().map(Value::from).unwrap_or(v)
        }
        _ => v,
    }
}

fn binds(params: &[Option<String>]) -> Vec<OValue> {
    params
        .iter()
        .map(|p| p.clone().map(OValue::String).unwrap_or(OValue::Null))
        .collect()
}

/// Is `sql` a query (SELECT / WITH)?
fn is_query(sql: &str) -> bool {
    let head = sql
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_uppercase();
    matches!(head.as_str(), "SELECT" | "WITH")
}

impl Oracle {
    /// Rows of a query (objects in column order), fetching past the first
    /// round trip up to `limit`.
    async fn rows_with(
        &self,
        sql: String,
        params: Vec<Option<String>>,
        limit: i64,
    ) -> DbResult<Vec<Value>> {
        let session = self.session.clone();
        let sql = super::trim_sql(&sql);
        let label = sql.clone();
        db::run_logged(label, crate::console::Source::Data, async move {
            let conn = session.get().await?;
            let limit = limit.max(0) as usize;
            let p = binds(&params);
            let r = conn.query(&sql, &p).await.map_err(err)?;
            let names: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
            let kinds: Vec<OracleType> = r.columns.iter().map(|c| c.oracle_type).collect();
            let mut out = Vec::new();
            let mut batch = r.rows;
            loop {
                let got = batch.len();
                for row in batch {
                    if out.len() >= limit {
                        break;
                    }
                    let mut vals = Vec::with_capacity(kinds.len());
                    for (v, k) in row.into_values().into_iter().zip(&kinds) {
                        // LOBs past the inline size come as locators: read them.
                        let v = match v {
                            OValue::Lob(oracle_rs::types::LobValue::Locator(loc)) => match k {
                                OracleType::Blob => {
                                    super::hex(&conn.read_blob(&loc).await.map_err(err)?)
                                }
                                _ => Value::String(conn.read_clob(&loc).await.map_err(err)?),
                            },
                            v => typed(json_of(v), *k),
                        };
                        vals.push(v);
                    }
                    out.extend(super::objects_from(&names, vec![vals]));
                }
                // A reply holds one prefetch batch; a full one means there may
                // be more, read as the next page of the same query.
                if out.len() >= limit || got < BATCH || !is_query(&sql) {
                    break;
                }
                let page = format!(
                    "SELECT * FROM ({sql}) OFFSET {} ROWS FETCH NEXT {BATCH} ROWS ONLY",
                    out.len()
                );
                match conn.query(&page, &p).await {
                    Ok(next) => batch = next.rows,
                    // Not wrappable (e.g. duplicate column names): the first rows.
                    Err(_) => break,
                }
            }
            Ok(out)
        })
        .await
    }

    fn rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone_handle();
        Box::pin(async move { this.rows_with(sql, Vec::new(), limit).await })
    }

    fn clone_handle(&self) -> Oracle {
        Oracle {
            session: self.session.clone(),
            user: self.user.clone(),
        }
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

/// Oracle-maintained schemas hidden from the picker.
const SYSTEM_USERS: &str =
    "SELECT username FROM all_users WHERE oracle_maintained = 'N' ORDER BY username";

impl Driver for Oracle {
    fn engine(&self) -> Engine {
        Engine::Oracle
    }
    fn default_schema(&self) -> Option<String> {
        Some(self.user.clone())
    }
    fn version(&self) -> Fut<String> {
        let f = self.rows("SELECT banner_full AS v FROM v$version".into(), 1);
        let g = self.rows("SELECT banner AS v FROM v$version".into(), 1);
        Box::pin(async move {
            let r = match f.await {
                Ok(r) => r,
                Err(_) => g.await?,
            };
            Ok(r.first()
                .map(|r| s(&r["V"]))
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or("Oracle")
                .to_string())
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        let f = self.rows(
            "SELECT SYS_CONTEXT('USERENV', 'CON_NAME') AS n FROM dual".into(),
            1,
        );
        Box::pin(async move { Ok(f.await?.iter().map(|r| s(&r["N"])).collect()) })
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let f = self.rows(SYSTEM_USERS.into(), 100_000);
        let user = self.user.clone();
        Box::pin(async move {
            let mut out: Vec<String> = f.await?.iter().map(|r| s(&r["USERNAME"])).collect();
            if !out.contains(&user) {
                out.insert(0, user);
            }
            Ok(out)
        })
    }
    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let lit = D.literal(&schema);
        let f = self.rows(
            format!(
                "SELECT object_name AS n, object_type AS k FROM all_objects
                  WHERE owner = {lit} AND object_type IN ('TABLE', 'VIEW', 'MATERIALIZED VIEW', 'FUNCTION', 'PROCEDURE', 'PACKAGE')
                    AND object_name NOT LIKE 'BIN$%' AND generated = 'N'
                  ORDER BY object_name"
            ),
            100_000,
        );
        Box::pin(async move {
            let mut tree = ObjectTree::default();
            let rows = f.await?;
            let matviews: Vec<String> = rows
                .iter()
                .filter(|r| s(&r["K"]) == "MATERIALIZED VIEW")
                .map(|r| s(&r["N"]))
                .collect();
            for r in &rows {
                let n = s(&r["N"]);
                match s(&r["K"]).as_str() {
                    // A materialized view also lists its container table.
                    "TABLE" if !matviews.contains(&n) => tree.tables.push(n),
                    "VIEW" => tree.views.push(n),
                    "MATERIALIZED VIEW" => tree.matviews.push(n),
                    "FUNCTION" | "PROCEDURE" | "PACKAGE" => tree.functions.push(n),
                    _ => {}
                }
            }
            Ok(tree)
        })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let (o, t) = (D.literal(&schema), D.literal(&table));
        let f = self.rows(
            format!(
                "SELECT c.column_name AS name, c.data_type AS dt, c.char_length AS len, c.data_precision AS prec,
                        c.data_scale AS scale, c.nullable AS nullable, c.data_default_vc AS dflt, cm.comments AS cmt,
                        (SELECT COUNT(*) FROM all_constraints k JOIN all_cons_columns kc
                             ON kc.owner = k.owner AND kc.constraint_name = k.constraint_name
                          WHERE k.owner = c.owner AND k.table_name = c.table_name AND k.constraint_type = 'P'
                            AND kc.column_name = c.column_name) AS pk,
                        (SELECT MIN(r.table_name || '(' || rc.column_name || ')')
                           FROM all_constraints k
                           JOIN all_cons_columns kc ON kc.owner = k.owner AND kc.constraint_name = k.constraint_name
                           JOIN all_constraints r ON r.owner = k.r_owner AND r.constraint_name = k.r_constraint_name
                           JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name
                                AND rc.position = kc.position
                          WHERE k.owner = c.owner AND k.table_name = c.table_name AND k.constraint_type = 'R'
                            AND kc.column_name = c.column_name) AS fk
                   FROM all_tab_columns c
                   LEFT JOIN all_col_comments cm
                     ON cm.owner = c.owner AND cm.table_name = c.table_name AND cm.column_name = c.column_name
                  WHERE c.owner = {o} AND c.table_name = {t}
                  ORDER BY c.column_id"
            ),
            10_000,
        );
        Box::pin(async move {
            Ok(f.await?
                .iter()
                .map(|r| {
                    let dt = s(&r["DT"]);
                    let sql_type = match dt.as_str() {
                        "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" | "RAW" => {
                            format!("{dt}({})", s(&r["LEN"]))
                        }
                        "NUMBER" if !r["PREC"].is_null() => match r["SCALE"].as_i64() {
                            Some(0) | None => format!("NUMBER({})", s(&r["PREC"])),
                            Some(sc) => format!("NUMBER({},{sc})", s(&r["PREC"])),
                        },
                        _ => dt.clone(),
                    };
                    let short = match dt.as_str() {
                        "NUMBER" if r["SCALE"].as_i64() == Some(0) => "int8".to_string(),
                        "NUMBER" => "numeric".to_string(),
                        "DATE" => "timestamp".to_string(),
                        d if d.starts_with("TIMESTAMP") && d.contains("TIME ZONE") => {
                            "timestamptz".to_string()
                        }
                        d if d.starts_with("TIMESTAMP") => "timestamp".to_string(),
                        d => super::short_type(d),
                    };
                    GridColumnMeta {
                        name: s(&r["NAME"]),
                        pg_type: short,
                        sql_type,
                        nullable: s(&r["NULLABLE"]) == "Y",
                        default: Some(s(&r["DFLT"]).trim().to_string()).filter(|d| !d.is_empty()),
                        comment: Some(s(&r["CMT"])).filter(|c| !c.is_empty()),
                        is_pk: r["PK"].as_i64().unwrap_or(0) > 0,
                        foreign_key: Some(s(&r["FK"])).filter(|f| !f.is_empty()),
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
        Box::pin(async move { Ok(f.await?.first().and_then(|r| r["N"].as_i64()).unwrap_or(0)) })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let key = if req.with_key {
            format!("ROWIDTOCHAR(t.ROWID) AS {}, ", D.quote(db::CTID_COL))
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT {key}t.* FROM {} t {} {}",
            D.qualified(&req.schema, &req.table, true),
            where_sql(&req.filter),
            D.page(
                req.order_by.as_deref().unwrap_or(""),
                &req.limit.min(i64::from(i32::MAX)).to_string(),
                &req.offset.to_string()
            )
        );
        self.rows(sql, req.limit)
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone_handle();
        Box::pin(async move {
            let sql = super::trim_sql(&sql);
            if is_query(&sql) {
                return this.rows_with(sql, Vec::new(), limit).await;
            }
            let n = this.exec_one(&sql, &[]).await?;
            Ok(vec![serde_json::json!({ "rows affected": n })])
        })
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let session = self.session.clone();
        let sql = super::trim_sql(&sql);
        Box::pin(async move {
            db::run_db(async move {
                let r = session
                    .get()
                    .await?
                    .query(&format!("SELECT * FROM ({sql}) WHERE 1 = 0"), &[])
                    .await
                    .map_err(err)?;
                Ok(r.columns.iter().map(|c| c.name.clone()).collect())
            })
            .await
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let this = self.clone_handle();
        Box::pin(async move {
            let mut n = 0;
            for st in split_oracle(&sql) {
                n += this.exec_one(&st, &[]).await?;
            }
            db::run_db({
                let s = this.session.clone();
                async move { s.get().await?.commit().await.map_err(err) }
            })
            .await?;
            Ok(n)
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let session = self.session.clone();
        Box::pin(async move {
            db::run_db(async move {
                let conn = session.get().await?;
                let mut affected = 0;
                for st in &stmts {
                    let started = std::time::Instant::now();
                    match conn
                        .execute(&super::trim_sql(&st.sql), &binds(&st.params))
                        .await
                    {
                        Ok(r) => {
                            crate::console::record(
                                &st.sql,
                                started,
                                crate::console::Source::Data,
                                None,
                            );
                            affected += r.rows_affected;
                        }
                        Err(e) => {
                            let msg = err(e);
                            crate::console::record(
                                &st.sql,
                                started,
                                crate::console::Source::Data,
                                Some(&msg),
                            );
                            let _ = conn.rollback().await;
                            return Err(format!("{msg}\n  in: {}", st.sql));
                        }
                    }
                }
                conn.commit().await.map_err(err)?;
                Ok(affected)
            })
            .await
        })
    }
    fn script(&self, kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String> {
        let target = D.qualified(&schema, &name, true);
        let ddl_kind = match kind {
            ObjKind::View => "VIEW",
            ObjKind::MatView => "MATERIALIZED_VIEW",
            ObjKind::Function => "FUNCTION",
            _ => "TABLE",
        };
        let ddl = self.rows(
            format!(
                "SELECT DBMS_METADATA.GET_DDL({}, {}, {}) AS d FROM dual",
                D.literal(ddl_kind),
                D.literal(&name),
                D.literal(&schema)
            ),
            1,
        );
        let cols = self.columns(schema, name);
        Box::pin(async move {
            match (kind, which) {
                (_, Script::Create) => match ddl.await {
                    Ok(r) if !r.is_empty() => Ok(s(&r[0]["D"]).trim().to_string()),
                    _ => Ok(super::create_table_from(D, &target, &cols.await?)),
                },
                (ObjKind::View, Script::Drop) => Ok(format!("DROP VIEW {target};")),
                (ObjKind::MatView, Script::Drop) => Ok(format!("DROP MATERIALIZED VIEW {target};")),
                (ObjKind::Function, Script::Drop) => Ok(format!("DROP FUNCTION {target};")),
                (ObjKind::Function, _) => Ok(format!("SELECT {target}() FROM dual;")),
                (_, w) => Ok(super::dml_script(D, w, &target, &cols.await?)),
            }
        })
    }
    fn indexes(&self, schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let f = self.rows(
            format!(
                "SELECT i.index_name AS n, i.index_type AS algo, i.uniqueness AS u,
                        (SELECT COUNT(*) FROM all_constraints k WHERE k.owner = i.table_owner AND k.index_name = i.index_name
                            AND k.constraint_type = 'P') AS p,
                        (SELECT LISTAGG(c.column_name || DECODE(c.descend, 'DESC', ' DESC'), ', ')
                                WITHIN GROUP (ORDER BY c.column_position)
                           FROM all_ind_columns c WHERE c.index_owner = i.owner AND c.index_name = i.index_name) AS cols
                   FROM all_indexes i
                  WHERE i.table_owner = {} AND i.table_name = {}
                  ORDER BY p DESC, i.index_name",
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
                    name: s(&r["N"]),
                    algorithm: s(&r["ALGO"]).to_lowercase(),
                    unique: s(&r["U"]) == "UNIQUE",
                    primary: r["P"].as_i64().unwrap_or(0) > 0,
                    columns: s(&r["COLS"]),
                    include: String::new(),
                    condition: None,
                    comment: None,
                    constraint: None,
                })
                .collect())
        })
    }
    fn triggers(&self, schema: String, table: String) -> Fut<Vec<Value>> {
        self.rows(
            format!(
                "SELECT trigger_name AS \"name\", trigger_type || ' ' || triggering_event AS \"timing\", status AS \"status\"
                   FROM all_triggers WHERE table_owner = {} AND table_name = {} ORDER BY trigger_name",
                D.literal(&schema),
                D.literal(&table)
            ),
            1000,
        )
    }
}

impl Oracle {
    async fn exec_one(&self, sql: &str, params: &[Option<String>]) -> DbResult<u64> {
        let session = self.session.clone();
        let sql = sql.trim().to_string();
        // PL/SQL blocks keep their final `;`; plain statements can't have one.
        let sql = if is_plsql(&sql) {
            sql
        } else {
            super::trim_sql(&sql)
        };
        let label = sql.clone();
        let p = binds(params);
        db::run_logged(label, crate::console::Source::Data, async move {
            Ok(session
                .get()
                .await?
                .execute(&sql, &p)
                .await
                .map_err(err)?
                .rows_affected)
        })
        .await
    }
}

fn is_plsql(sql: &str) -> bool {
    let u = sql.trim_start().to_uppercase();
    u.starts_with("BEGIN")
        || u.starts_with("DECLARE")
        || (u.starts_with("CREATE")
            && ["PROCEDURE", "FUNCTION", "PACKAGE", "TRIGGER", "TYPE BODY"]
                .iter()
                .any(|k| u.contains(k)))
}

/// Statements of a script: `;`-separated, but a PL/SQL block runs to a
/// line holding only `/`.
pub fn split_oracle(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut block: Option<String> = None;
    for line in sql.lines() {
        if let Some(b) = block.as_mut() {
            if line.trim() == "/" {
                out.push(b.trim().to_string());
                block = None;
            } else {
                b.push_str(line);
                b.push('\n');
            }
            continue;
        }
        // A block starts on a fresh statement (nothing pending before it).
        let pending = plain.trim();
        if is_plsql(line) && (pending.is_empty() || pending.ends_with(';')) {
            out.extend(db::split_statements(&std::mem::take(&mut plain)));
            block = Some(format!("{line}\n"));
            continue;
        }
        plain.push_str(line);
        plain.push('\n');
    }
    out.extend(db::split_statements(&plain));
    if let Some(b) = block {
        out.push(b.trim().to_string());
    }
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn scripts_split() {
        let parts = super::split_oracle("CREATE TABLE a (x INT);\nBEGIN\n  NULL;\nEND;\n/\n");
        assert_eq!(parts, ["CREATE TABLE a (x INT)", "BEGIN\n  NULL;\nEND;"]);
    }

    #[test]
    fn live_oracle() {
        if !live::reachable(31521) {
            return;
        }
        let c = live::conn(Engine::Oracle, 31521, "tusk", "FREEPDB1");
        let rt = crate::db::runtime();
        let db = rt
            .block_on(crate::drivers::connect(
                &c,
                c.host.clone(),
                c.port,
                "tusk".into(),
            ))
            .unwrap();
        let d = db.driver();
        rt.block_on(d.exec("BEGIN EXECUTE IMMEDIATE 'DROP TABLE people'; EXCEPTION WHEN OTHERS THEN NULL; END;".into())).unwrap();
        rt.block_on(d.exec(
            "CREATE TABLE people (id NUMBER(10) PRIMARY KEY, email VARCHAR2(200), born DATE, amount NUMBER(10,2), note CLOB);
             INSERT INTO people VALUES (1, 'a@x', DATE '1990-01-02', 12.5, 'hello');
             INSERT INTO people VALUES (2, 'b@x', NULL, NULL, NULL)"
                .into(),
        ))
        .unwrap();
        live::exercise(c, "tusk", "PEOPLE", "EMAIL");
        let rows = rt
            .block_on(d.query_rows("SELECT * FROM people ORDER BY id".into(), 5))
            .unwrap();
        assert_eq!(rows[0]["ID"], 1);
        assert_eq!(rows[0]["BORN"], "1990-01-02 00:00:00");
        assert_eq!(rows[0]["AMOUNT"], "12.5");
        assert_eq!(rows[0]["NOTE"], "hello");
        // Long results page past the first fetch.
        let many = rt
            .block_on(d.query_rows(
                "SELECT level AS n FROM dual CONNECT BY level <= 2500".into(),
                3000,
            ))
            .unwrap();
        assert_eq!(many.len(), 2500);
        rt.block_on(d.exec("DROP TABLE people".into())).unwrap();
    }
}
