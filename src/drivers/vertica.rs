//! Vertica over its Postgres-style wire protocol, with the simple query
//! protocol only: Vertica's type ids aren't Postgres ones and it has no
//! `pg_catalog` to resolve them from, so results come back as text and are
//! typed from `v_catalog`. Parameters are inlined as literals.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio_postgres::SimpleQueryMessage;

use super::{Db, Driver, Fut, WindowReq};
use crate::db::{
    self, DbResult, GridColumnMeta, ObjectTree, SavedConnection, SslMode, Stmt, WhereClause,
};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

const D: Dialect = Dialect::Vertica;

#[derive(Clone)]
pub struct Vertica {
    client: Arc<tokio_postgres::Client>,
    /// Column types per `schema.table` (types window rows).
    types: super::Shapes,
}

fn err(e: tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(d) => d.message().to_string(),
        None => e.to_string(),
    }
}

pub async fn connect(
    conn: &SavedConnection,
    host: String,
    port: u16,
    password: String,
) -> DbResult<Db> {
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(&host)
        .port(port)
        .user(&conn.user)
        .password(password)
        .dbname(&conn.database)
        .application_name("Tusk")
        .connect_timeout(std::time::Duration::from_secs(10));
    let ssl = conn.ssl;
    let client = db::run_db(async move {
        let client = if ssl == SslMode::Disable {
            let (client, connection) = cfg.connect(tokio_postgres::NoTls).await.map_err(err)?;
            tokio::spawn(connection);
            client
        } else {
            if ssl == SslMode::Prefer {
                cfg.ssl_mode(tokio_postgres::config::SslMode::Prefer);
            } else {
                cfg.ssl_mode(tokio_postgres::config::SslMode::Require);
            }
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
            let (client, connection) = cfg
                .connect(tokio_postgres_rustls::MakeRustlsConnect::new(tls))
                .await
                .map_err(err)?;
            tokio::spawn(connection);
            client
        };
        // Each statement commits unless a save wraps several in BEGIN … COMMIT.
        client
            .simple_query("SET SESSION AUTOCOMMIT TO ON")
            .await
            .map_err(err)?;
        Ok(client)
    })
    .await?;
    Ok(Db::new(Vertica {
        client: Arc::new(client),
        types: Default::default(),
    }))
}

/// `$n` placeholders replaced by literals (outside quotes).
pub fn inline_dollars(sql: &str, params: &[Option<String>]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if c == '\'' {
            quoted = !quoted;
            out.push(c);
            continue;
        }
        if c == '$' && !quoted && chars.peek().is_some_and(char::is_ascii_digit) {
            let mut n = String::new();
            while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                n.push(*d);
                chars.next();
            }
            match n
                .parse::<usize>()
                .ok()
                .and_then(|i| params.get(i.wrapping_sub(1)))
            {
                Some(Some(v)) => out.push_str(&D.literal(v)),
                _ => out.push_str("NULL"),
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// A text cell typed by its Vertica type name.
fn typed(v: Option<&str>, ty: &str) -> Value {
    let Some(t) = v else {
        return Value::Null;
    };
    let ty = ty.to_lowercase();
    if ty.starts_with("int") {
        return t
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(t.into()));
    }
    if ty.starts_with("float") {
        return t
            .parse::<f64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(t.into()));
    }
    if ty == "boolean" {
        return Value::Bool(t == "t" || t == "true");
    }
    Value::String(t.to_string())
}

impl Vertica {
    /// Rows of the last result set of `sql`: (column names, text rows).
    async fn raw(&self, sql: String) -> DbResult<(Vec<String>, Vec<Vec<Option<String>>>, u64)> {
        let client = self.client.clone();
        db::run_db(async move {
            let msgs = client.simple_query(&sql).await.map_err(err)?;
            let mut cols = Vec::new();
            let mut rows = Vec::new();
            let mut affected = 0;
            for m in msgs {
                match m {
                    SimpleQueryMessage::RowDescription(d) => {
                        cols = d.iter().map(|c| c.name().to_string()).collect();
                        rows.clear();
                    }
                    SimpleQueryMessage::Row(r) => {
                        if cols.is_empty() {
                            cols = r.columns().iter().map(|c| c.name().to_string()).collect();
                        }
                        rows.push((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect());
                    }
                    SimpleQueryMessage::CommandComplete(n) => affected = n,
                    _ => {}
                }
            }
            Ok((cols, rows, affected))
        })
        .await
    }

    /// Rows as JSON objects; `types` (column → type) types the cells.
    async fn rows(
        &self,
        sql: String,
        limit: i64,
        types: Option<Vec<(String, String)>>,
    ) -> DbResult<Vec<Value>> {
        let sql = super::trim_sql(&sql);
        let this = self.clone();
        let label = sql.clone();
        db::run_logged(label, crate::console::Source::Data, async move {
            let (cols, rows, _) = this.raw(sql).await?;
            let ty = |c: &str| {
                types
                    .as_ref()
                    .and_then(|t| t.iter().find(|(n, _)| n == c))
                    .map(|(_, t)| t.as_str())
                    .unwrap_or("varchar")
            };
            Ok(rows
                .into_iter()
                .take(limit.max(0) as usize)
                .map(|r| {
                    let vals: Vec<Value> = r
                        .iter()
                        .zip(&cols)
                        .map(|(v, c)| typed(v.as_deref(), ty(c)))
                        .collect();
                    super::objects_from(&cols, vec![vals])
                        .pop()
                        .unwrap_or(Value::Null)
                })
                .collect())
        })
        .await
    }

    fn fut(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move { this.rows(sql, limit, None).await })
    }

    async fn types_of(&self, schema: &str, table: &str) -> Vec<(String, String)> {
        let key = format!("{schema}.{table}");
        if let Some(t) = self.types.lock().unwrap().get(&key) {
            return t.clone();
        }
        let t: Vec<(String, String)> = self
            .columns(schema.to_string(), table.to_string())
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|c| (c.name, c.sql_type))
            .collect();
        self.types.lock().unwrap().insert(key, t.clone());
        t
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
        Some(w) if !w.sql.is_empty() => format!(
            "WHERE {}",
            inline_dollars(
                &w.sql,
                &w.params.iter().cloned().map(Some).collect::<Vec<_>>()
            )
        ),
        _ => String::new(),
    }
}

impl Driver for Vertica {
    fn engine(&self) -> Engine {
        Engine::Vertica
    }
    fn default_schema(&self) -> Option<String> {
        Some("public".into())
    }
    fn row_key(&self) -> bool {
        false
    }
    fn version(&self) -> Fut<String> {
        let f = self.fut("SELECT version() AS v".into(), 1);
        Box::pin(async move { Ok(f.await?.first().map(|r| s(&r["v"])).unwrap_or_default()) })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        let f = self.fut(
            "SELECT database_name AS n FROM v_catalog.databases".into(),
            100,
        );
        Box::pin(async move { Ok(f.await?.iter().map(|r| s(&r["n"])).collect()) })
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let f = self.fut(
            "SELECT schema_name AS n FROM v_catalog.schemata WHERE NOT is_system_schema ORDER BY 1"
                .into(),
            10_000,
        );
        Box::pin(async move { Ok(f.await?.iter().map(|r| s(&r["n"])).collect()) })
    }
    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let lit = D.literal(&schema);
        let t = self.fut(
            format!(
                "SELECT table_name AS n FROM v_catalog.tables WHERE table_schema = {lit} ORDER BY 1"
            ),
            100_000,
        );
        let v = self.fut(
            format!(
                "SELECT table_name AS n FROM v_catalog.views WHERE table_schema = {lit} ORDER BY 1"
            ),
            100_000,
        );
        Box::pin(async move {
            Ok(ObjectTree {
                tables: t.await?.iter().map(|r| s(&r["n"])).collect(),
                views: v
                    .await
                    .unwrap_or_default()
                    .iter()
                    .map(|r| s(&r["n"]))
                    .collect(),
                ..Default::default()
            })
        })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let (sc, tb) = (D.literal(&schema), D.literal(&table));
        let cols = self.fut(
            format!(
                "SELECT column_name AS name, data_type AS ty, is_nullable AS nullable, column_default AS dflt
                   FROM v_catalog.columns WHERE table_schema = {sc} AND table_name = {tb}
                 UNION ALL
                 SELECT column_name, data_type, TRUE, NULL
                   FROM v_catalog.view_columns WHERE table_schema = {sc} AND table_name = {tb}"
            ),
            10_000,
        );
        let order = self.fut(
            format!(
                "SELECT column_name AS name, ordinal_position AS pos FROM v_catalog.columns WHERE table_schema = {sc} AND table_name = {tb}
                 UNION ALL
                 SELECT column_name, ordinal_position FROM v_catalog.view_columns WHERE table_schema = {sc} AND table_name = {tb}"
            ),
            10_000,
        );
        let pk = self.fut(
            format!("SELECT column_name AS name FROM v_catalog.primary_keys WHERE table_schema = {sc} AND table_name = {tb}"),
            1_000,
        );
        Box::pin(async move {
            let pos: HashMap<String, i64> = order
                .await?
                .iter()
                .map(|r| (s(&r["name"]), s(&r["pos"]).parse().unwrap_or(0)))
                .collect();
            let pk: Vec<String> = pk
                .await
                .unwrap_or_default()
                .iter()
                .map(|r| s(&r["name"]))
                .collect();
            let mut out: Vec<GridColumnMeta> = cols
                .await?
                .iter()
                .map(|r| {
                    let ty = s(&r["ty"]);
                    let name = s(&r["name"]);
                    GridColumnMeta {
                        pg_type: match ty.to_lowercase().split('(').next().unwrap_or("") {
                            "int" => "int8".into(),
                            "float" => "float8".into(),
                            "long varchar" => "text".into(),
                            "timestamptz" => "timestamptz".into(),
                            t => super::short_type(t),
                        },
                        nullable: s(&r["nullable"]) != "f",
                        default: Some(s(&r["dflt"])).filter(|d| !d.is_empty()),
                        comment: None,
                        is_pk: pk.contains(&name),
                        foreign_key: None,
                        enum_values: Vec::new(),
                        sql_type: ty,
                        name,
                    }
                })
                .collect();
            out.sort_by_key(|c| pos.get(&c.name).copied().unwrap_or(0));
            Ok(out)
        })
    }
    fn count(&self, schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let f = self.fut(
            format!(
                "SELECT COUNT(*) AS n FROM {} {}",
                D.qualified(&schema, &table, true),
                where_sql(&filter)
            ),
            1,
        );
        Box::pin(async move {
            Ok(f.await?
                .first()
                .map(|r| s(&r["n"]))
                .and_then(|n| n.parse().ok())
                .unwrap_or(0))
        })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move {
            let types = this.types_of(&req.schema, &req.table).await;
            let sql = format!(
                "SELECT * FROM {} {} {}",
                D.qualified(&req.schema, &req.table, true),
                where_sql(&req.filter),
                D.page(
                    req.order_by.as_deref().unwrap_or(""),
                    &req.limit.to_string(),
                    &req.offset.to_string()
                )
            );
            this.rows(sql, req.limit, Some(types)).await
        })
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        self.fut(sql, limit)
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let this = self.clone();
        Box::pin(async move {
            Ok(this
                .raw(format!(
                    "SELECT * FROM ({}) q LIMIT 0",
                    super::trim_sql(&sql)
                ))
                .await?
                .0)
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let this = self.clone();
        Box::pin(async move {
            let label = sql.clone();
            let n = db::run_logged(label, crate::console::Source::Data, async move {
                Ok(this.raw(sql).await?.2)
            })
            .await?;
            Ok(n)
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let this = self.clone();
        Box::pin(async move {
            let started = std::time::Instant::now();
            // Grid saves use `?`; anything numbered `$n`.
            let sqls: Vec<String> = stmts
                .iter()
                .map(|st| {
                    if st.sql.contains('$') {
                        inline_dollars(&st.sql, &st.params)
                    } else {
                        super::clickhouse::inline(&st.sql, &st.params, D)
                    }
                })
                .collect();
            let label = sqls.join(";\n");
            let r = async {
                this.raw("BEGIN".into()).await?;
                let mut n = 0;
                for sql in &sqls {
                    match this.raw(sql.clone()).await {
                        Ok((_, _, a)) => n += a,
                        Err(e) => {
                            let _ = this.raw("ROLLBACK".into()).await;
                            return Err(format!("{e}\n  in: {sql}"));
                        }
                    }
                }
                this.raw("COMMIT".into()).await?;
                Ok(n)
            }
            .await;
            this.types.lock().unwrap().clear();
            crate::console::record(
                &label,
                started,
                crate::console::Source::Data,
                r.as_ref().err().map(String::as_str),
            );
            r
        })
    }
    fn script(&self, kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String> {
        let target = D.qualified(&schema, &name, true);
        let ddl = self.fut(
            format!(
                "SELECT EXPORT_OBJECTS('', {}, false) AS d",
                D.literal(&format!("{schema}.{name}"))
            ),
            1,
        );
        let cols = self.columns(schema, name);
        Box::pin(async move {
            match (kind, which) {
                (_, Script::Create) => Ok(ddl
                    .await?
                    .first()
                    .map(|r| s(&r["d"]))
                    .unwrap_or_default()
                    .trim()
                    .to_string()),
                (ObjKind::View, Script::Drop) => Ok(format!("DROP VIEW {target};")),
                (_, w) => Ok(super::dml_script(D, w, &target, &cols.await?)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn dollars_inline() {
        assert_eq!(
            super::inline_dollars(
                "a = CAST($1 AS int) AND b = '$2' AND c = $2",
                &[Some("it's".into()), None]
            ),
            "a = CAST('it''s' AS int) AND b = '$2' AND c = NULL"
        );
    }

    /// The community 9.2 image: database `docker`, `public.customers`
    /// seeded by hand.
    #[test]
    fn live_vertica() {
        if live::reachable(35433) {
            live::exercise(
                live::conn(Engine::Vertica, 35433, "dbadmin", "docker"),
                "",
                "customers",
                "email",
            );
        }
    }
}
