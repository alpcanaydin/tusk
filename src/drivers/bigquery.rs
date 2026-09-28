//! Google BigQuery over its REST API. Datasets are schemas. Credentials: a
//! service-account key file (signed JWT → OAuth token), the gcloud
//! application-default login, or none against an emulator endpoint. Saves
//! inline typed literals, since BigQuery won't coerce a STRING parameter.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Db, Driver, Fut, GridOp, WindowReq, http};
use crate::db::{self, DbResult, GridColumnMeta, ObjectTree, SavedConnection, Stmt, WhereClause};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

const D: Dialect = Dialect::BigQuery;
const API: &str = "https://bigquery.googleapis.com";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/bigquery";

#[derive(Clone)]
enum Auth {
    None,
    Token(String),
    ServiceAccount {
        email: String,
        key: String,
        token_uri: String,
    },
    User {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
}

#[derive(Clone)]
pub struct BigQuery {
    base: String,
    project: String,
    dataset: String,
    location: Option<String>,
    emulator: bool,
    auth: Auth,
    token: Arc<Mutex<Option<(String, Instant)>>>,
    /// (name, type) per `dataset.table`, for typed literals.
    types: super::Shapes,
}

fn load_auth(conn: &SavedConnection, secret: &str) -> DbResult<Auth> {
    let key_json = match conn.opt("key_file").trim() {
        "" if secret.trim_start().starts_with('{') => Some(secret.to_string()),
        "" => None,
        path => {
            let p = super::sqlite::shellexpand(path);
            Some(
                std::fs::read_to_string(&p)
                    .map_err(|e| format!("Can't read the key file {p}: {e}"))?,
            )
        }
    };
    let from_json = |text: &str| -> DbResult<Auth> {
        let v: Value =
            serde_json::from_str(text).map_err(|e| format!("The key file isn't JSON: {e}"))?;
        match v["type"].as_str() {
            Some("service_account") => Ok(Auth::ServiceAccount {
                email: v["client_email"].as_str().unwrap_or_default().to_string(),
                key: v["private_key"].as_str().unwrap_or_default().to_string(),
                token_uri: v["token_uri"].as_str().unwrap_or(TOKEN_URL).to_string(),
            }),
            Some("authorized_user") => Ok(Auth::User {
                client_id: v["client_id"].as_str().unwrap_or_default().to_string(),
                client_secret: v["client_secret"].as_str().unwrap_or_default().to_string(),
                refresh_token: v["refresh_token"].as_str().unwrap_or_default().to_string(),
            }),
            other => Err(format!("Unsupported credentials type {other:?}")),
        }
    };
    if let Some(text) = key_json {
        return from_json(&text);
    }
    if !secret.trim().is_empty() {
        return Ok(Auth::Token(secret.trim().to_string()));
    }
    if !conn.opt("endpoint").trim().is_empty() {
        return Ok(Auth::None);
    }
    // gcloud auth application-default login
    let adc = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .ok()
        .unwrap_or_else(|| {
            super::sqlite::shellexpand("~/.config/gcloud/application_default_credentials.json")
        });
    match std::fs::read_to_string(&adc) {
        Ok(text) => from_json(&text),
        Err(_) => Err(
            "Choose a service-account key file (or run gcloud auth application-default login)."
                .into(),
        ),
    }
}

pub async fn connect(conn: &SavedConnection, secret: String) -> DbResult<Db> {
    let endpoint = conn
        .opt("endpoint")
        .trim()
        .trim_end_matches('/')
        .to_string();
    let project = conn.opt("project").trim().to_string();
    if project.is_empty() {
        return Err("Fill in the project".into());
    }
    let bq = BigQuery {
        base: format!(
            "{}/bigquery/v2/projects/{project}",
            if endpoint.is_empty() { API } else { &endpoint }
        ),
        dataset: conn.opt("dataset").trim().to_string(),
        location: Some(conn.opt("region").trim().to_string()).filter(|l| !l.is_empty()),
        emulator: !endpoint.is_empty(),
        auth: load_auth(conn, &secret)?,
        project,
        token: Default::default(),
        types: Default::default(),
    };
    bq.get("datasets?maxResults=1").await?;
    Ok(Db::new(bq))
}

impl BigQuery {
    async fn bearer(&self) -> DbResult<Option<String>> {
        if let Some((t, at)) = self.token.lock().unwrap().clone()
            && at.elapsed() < Duration::from_secs(50 * 60)
        {
            return Ok(Some(t));
        }
        let form: Vec<(&str, String)> = match &self.auth {
            Auth::None => return Ok(None),
            Auth::Token(t) => return Ok(Some(t.clone())),
            Auth::ServiceAccount {
                email,
                key,
                token_uri,
            } => {
                let now = chrono::Utc::now().timestamp();
                let claims = json!({"iss": email, "scope": SCOPE, "aud": token_uri, "iat": now, "exp": now + 3600});
                let key = jsonwebtoken::EncodingKey::from_rsa_pem(key.as_bytes())
                    .map_err(|e| format!("Bad private key: {e}"))?;
                let jwt = jsonwebtoken::encode(
                    &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
                    &claims,
                    &key,
                )
                .map_err(|e| e.to_string())?;
                vec![
                    (
                        "grant_type",
                        "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
                    ),
                    ("assertion", jwt),
                ]
            }
            Auth::User {
                client_id,
                client_secret,
                refresh_token,
            } => vec![
                ("grant_type", "refresh_token".into()),
                ("client_id", client_id.clone()),
                ("client_secret", client_secret.clone()),
                ("refresh_token", refresh_token.clone()),
            ],
        };
        let uri = match &self.auth {
            Auth::ServiceAccount { token_uri, .. } => token_uri.clone(),
            _ => TOKEN_URL.to_string(),
        };
        let v = http::send_json(http::client().post(uri).form(&form)).await?;
        let t = v["access_token"]
            .as_str()
            .ok_or("No access token in the OAuth reply")?
            .to_string();
        *self.token.lock().unwrap() = Some((t.clone(), Instant::now()));
        Ok(Some(t))
    }

    async fn authed(&self, req: reqwest::RequestBuilder) -> DbResult<Value> {
        let req = match self.bearer().await? {
            Some(t) => req.bearer_auth(t),
            None => req,
        };
        http::send_json(req).await
    }

    async fn get(&self, path: &str) -> DbResult<Value> {
        self.authed(http::client().get(format!("{}/{path}", self.base)))
            .await
    }

    /// Run a statement to completion: (field schema, rows as JSON).
    async fn query(&self, sql: String, limit: usize) -> DbResult<(Vec<Value>, Vec<Value>, u64)> {
        let mut body = json!({"query": sql, "useLegacySql": false, "maxResults": limit.clamp(1, 10_000), "timeoutMs": 60_000});
        if !self.dataset.is_empty() {
            body["defaultDataset"] = json!({"projectId": self.project, "datasetId": self.dataset});
        }
        if let Some(l) = &self.location {
            body["location"] = json!(l);
        }
        let mut r = self
            .authed(
                http::client()
                    .post(format!("{}/queries", self.base))
                    .json(&body),
            )
            .await?;
        let job = r["jobReference"]["jobId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let loc = r["jobReference"]["location"]
            .as_str()
            .map(str::to_string)
            .or_else(|| self.location.clone());
        let results = |page: Option<&str>| {
            let mut q = vec![
                ("maxResults".to_string(), limit.clamp(1, 10_000).to_string()),
                ("timeoutMs".into(), "60000".into()),
            ];
            if let Some(p) = page {
                q.push(("pageToken".into(), p.to_string()));
            }
            if let Some(l) = &loc {
                q.push(("location".into(), l.clone()));
            }
            http::client()
                .get(format!("{}/queries/{job}", self.base))
                .query(&q)
        };
        while r["jobComplete"] == Value::Bool(false) {
            r = self.authed(results(None)).await?;
        }
        if let Some(e) = r["errors"].as_array().and_then(|a| a.first()) {
            return Err(e["message"].as_str().unwrap_or("query failed").to_string());
        }
        let fields = r["schema"]["fields"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let affected = r["numDmlAffectedRows"]
            .as_str()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let mut rows: Vec<Value> = r["rows"].as_array().cloned().unwrap_or_default();
        let mut page = r["pageToken"].as_str().map(str::to_string);
        while rows.len() < limit {
            let Some(p) = page.take() else { break };
            let next = self.authed(results(Some(&p))).await?;
            rows.extend(next["rows"].as_array().cloned().unwrap_or_default());
            page = next["pageToken"].as_str().map(str::to_string);
        }
        rows.truncate(limit);
        let out = rows.iter().map(|row| record(&fields, row)).collect();
        Ok((fields, out, affected))
    }

    async fn rows(&self, sql: String, limit: i64) -> DbResult<Vec<Value>> {
        let sql = super::trim_sql(&sql);
        let this = self.clone();
        let label = sql.clone();
        db::run_logged(label, crate::console::Source::Data, async move {
            Ok(this.query(sql, limit.max(0) as usize).await?.1)
        })
        .await
    }

    fn fut(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move { this.rows(sql, limit).await })
    }

    async fn table(&self, dataset: &str, table: &str) -> DbResult<Value> {
        self.get(&format!("datasets/{}/tables/{}", enc(dataset), enc(table)))
            .await
    }

    async fn types_of(&self, dataset: &str, table: &str) -> DbResult<Vec<(String, String)>> {
        let key = format!("{dataset}.{table}");
        if let Some(t) = self.types.lock().unwrap().get(&key) {
            return Ok(t.clone());
        }
        let t = self.table(dataset, table).await?;
        let types: Vec<(String, String)> = t["schema"]["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| (s(&f["name"]), field_type(f)))
            .collect();
        self.types.lock().unwrap().insert(key, types.clone());
        Ok(types)
    }

    async fn grid_sql(&self, op: GridOp) -> DbResult<String> {
        let (schema, table) = match &op {
            GridOp::Insert { schema, table, .. }
            | GridOp::Update { schema, table, .. }
            | GridOp::Delete { schema, table, .. } => (
                schema.clone().unwrap_or_else(|| self.dataset.clone()),
                table.clone(),
            ),
        };
        let types = self.types_of(&schema, &table).await?;
        let ty = |c: &str| {
            types
                .iter()
                .find(|(n, _)| n == c)
                .map(|(_, t)| t.clone())
                .unwrap_or_else(|| "STRING".into())
        };
        let target = D.qualified(&schema, &table, true);
        let lit = |c: &str, v: &Option<String>| literal(v.as_deref(), &ty(c));
        let eq = |c: &str, v: &Option<String>| match v {
            None => format!("{} IS NULL", D.quote(c)),
            Some(_) => format!("{} = {}", D.quote(c), lit(c, v)),
        };
        Ok(match op {
            GridOp::Insert { values, .. } => {
                let values: Vec<_> = values.into_iter().filter(|(_, v)| v.is_some()).collect();
                let cols: Vec<String> = values.iter().map(|(c, _)| D.quote(c)).collect();
                let vals: Vec<String> = values.iter().map(|(c, v)| lit(c, v)).collect();
                format!(
                    "INSERT INTO {target} ({}) VALUES ({})",
                    cols.join(", "),
                    vals.join(", ")
                )
            }
            GridOp::Update { set, key, .. } => {
                let sets: Vec<String> = set
                    .iter()
                    .map(|(c, v)| format!("{} = {}", D.quote(c), lit(c, v)))
                    .collect();
                let pred: Vec<String> = key.iter().map(|(c, v)| eq(c, v)).collect();
                format!(
                    "UPDATE {target} SET {} WHERE {}",
                    sets.join(", "),
                    pred.join(" AND ")
                )
            }
            GridOp::Delete { key, .. } => {
                let pred: Vec<String> = key.iter().map(|(c, v)| eq(c, v)).collect();
                format!("DELETE FROM {target} WHERE {}", pred.join(" AND "))
            }
        })
    }
}

fn enc(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

/// `INT64`, `ARRAY<STRING>`, `STRUCT<…>` from a schema field.
fn field_type(f: &Value) -> String {
    let base = match s(&f["type"]).as_str() {
        "INTEGER" => "INT64".to_string(),
        "FLOAT" => "FLOAT64".to_string(),
        "BOOLEAN" => "BOOL".to_string(),
        "RECORD" | "STRUCT" => {
            let inner: Vec<String> = f["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| format!("{} {}", s(&x["name"]), field_type(x)))
                .collect();
            format!("STRUCT<{}>", inner.join(", "))
        }
        t => t.to_string(),
    };
    if f["mode"] == "REPEATED" {
        format!("ARRAY<{base}>")
    } else {
        base
    }
}

/// One `{f: [{v}]}` row as a JSON object, typed by `fields`.
fn record(fields: &[Value], row: &Value) -> Value {
    let cells = row["f"].as_array().cloned().unwrap_or_default();
    Value::Object(
        fields
            .iter()
            .zip(cells.iter())
            .map(|(f, c)| (s(&f["name"]), cell(f, &c["v"])))
            .collect(),
    )
}

fn cell(f: &Value, v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    if f["mode"] == "REPEATED" {
        let mut single = f.clone();
        single["mode"] = json!("NULLABLE");
        return Value::Array(
            v.as_array()
                .into_iter()
                .flatten()
                .map(|x| cell(&single, &x["v"]))
                .collect(),
        );
    }
    let t = v.as_str().unwrap_or_default();
    match s(&f["type"]).as_str() {
        "INTEGER" | "INT64" => t
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| v.clone()),
        "FLOAT" | "FLOAT64" => t
            .parse::<f64>()
            .map(Value::from)
            .unwrap_or_else(|_| v.clone()),
        "BOOLEAN" | "BOOL" => Value::Bool(t.eq_ignore_ascii_case("true")),
        "TIMESTAMP" => t
            .parse::<f64>()
            .ok()
            .and_then(|secs| chrono::DateTime::from_timestamp_micros((secs * 1e6).round() as i64))
            .map(|d| Value::String(d.format("%Y-%m-%d %H:%M:%S%.f UTC").to_string()))
            .unwrap_or_else(|| v.clone()),
        "RECORD" | "STRUCT" => record(f["fields"].as_array().map(Vec::as_slice).unwrap_or(&[]), v),
        "JSON" => serde_json::from_str(t).unwrap_or_else(|_| v.clone()),
        "BYTES" => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(t)
                .map(|b| super::hex(&b))
                .unwrap_or_else(|_| v.clone())
        }
        _ => v.clone(),
    }
}

/// Grid text as a literal of BigQuery type `ty`.
pub fn literal(text: Option<&str>, ty: &str) -> String {
    let Some(t) = text else {
        return "NULL".into();
    };
    let q = D.literal(t);
    match ty.split('<').next().unwrap_or("").trim() {
        "STRING" => q,
        "BYTES" => format!(
            "FROM_HEX({})",
            D.literal(t.trim().trim_start_matches("\\x").trim_start_matches("0x"))
        ),
        "JSON" => format!("PARSE_JSON({q})"),
        "ARRAY" | "STRUCT" => format!("JSON_QUERY_ARRAY({q})"),
        "INT64" | "FLOAT64" | "NUMERIC" | "BIGNUMERIC" | "BOOL" | "DATE" | "DATETIME" | "TIME"
        | "TIMESTAMP" | "GEOGRAPHY" => {
            format!("CAST({q} AS {ty})")
        }
        _ => q,
    }
}

fn where_sql(filter: &Option<WhereClause>) -> String {
    match filter {
        Some(w) if !w.sql.is_empty() => format!("WHERE {}", w.sql),
        _ => String::new(),
    }
}

impl Driver for BigQuery {
    fn engine(&self) -> Engine {
        Engine::BigQuery
    }
    fn default_schema(&self) -> Option<String> {
        Some(self.dataset.clone()).filter(|d| !d.is_empty())
    }
    fn row_key(&self) -> bool {
        false
    }
    fn version(&self) -> Fut<String> {
        let p = self.project.clone();
        let emulator = self.emulator;
        Box::pin(async move {
            Ok(format!(
                "BigQuery ({p}{})",
                if emulator { ", emulator" } else { "" }
            ))
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        let p = self.project.clone();
        Box::pin(async move { Ok(vec![p]) })
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let this = self.clone();
        Box::pin(async move {
            db::run_logged(
                "datasets.list".into(),
                crate::console::Source::Meta,
                async move {
                    let mut out = Vec::new();
                    let mut page: Option<String> = None;
                    loop {
                        let path = match &page {
                            Some(p) => format!("datasets?maxResults=1000&pageToken={}", enc(p)),
                            None => "datasets?maxResults=1000".into(),
                        };
                        let v = this.get(&path).await?;
                        out.extend(
                            v["datasets"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|d| s(&d["datasetReference"]["datasetId"])),
                        );
                        match v["nextPageToken"].as_str() {
                            Some(p) => page = Some(p.to_string()),
                            None => break,
                        }
                    }
                    out.sort();
                    Ok(out)
                },
            )
            .await
        })
    }
    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let this = self.clone();
        Box::pin(async move {
            let label = format!("tables.list ({schema})");
            db::run_logged(label, crate::console::Source::Meta, async move {
                let mut tree = ObjectTree::default();
                let mut page: Option<String> = None;
                loop {
                    let mut path = format!("datasets/{}/tables?maxResults=1000", enc(&schema));
                    if let Some(p) = &page {
                        path.push_str(&format!("&pageToken={}", enc(p)));
                    }
                    let v = this.get(&path).await?;
                    for t in v["tables"].as_array().into_iter().flatten() {
                        let name = s(&t["tableReference"]["tableId"]);
                        match t["type"].as_str().unwrap_or("TABLE") {
                            "VIEW" => tree.views.push(name),
                            "MATERIALIZED_VIEW" => tree.matviews.push(name),
                            _ => tree.tables.push(name),
                        }
                    }
                    match v["nextPageToken"].as_str() {
                        Some(p) => page = Some(p.to_string()),
                        None => break,
                    }
                }
                tree.tables.sort();
                tree.views.sort();
                Ok(tree)
            })
            .await
        })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let this = self.clone();
        Box::pin(async move {
            this.types
                .lock()
                .unwrap()
                .remove(&format!("{schema}.{table}"));
            let t = this.table(&schema, &table).await?;
            let pk: Vec<String> = t["tableConstraints"]["primaryKey"]["columns"]
                .as_array()
                .into_iter()
                .flatten()
                .map(s)
                .collect();
            Ok(t["schema"]["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    let ty = field_type(f);
                    let name = s(&f["name"]);
                    GridColumnMeta {
                        pg_type: match ty.split('<').next().unwrap_or("") {
                            "ARRAY" | "STRUCT" | "JSON" => "json".into(),
                            "INT64" => "int8".into(),
                            "FLOAT64" => "float8".into(),
                            "TIMESTAMP" => "timestamptz".into(),
                            "STRING" => "text".into(),
                            other => super::short_type(other),
                        },
                        nullable: f["mode"] != "REQUIRED",
                        default: f["defaultValueExpression"].as_str().map(str::to_string),
                        comment: f["description"].as_str().map(str::to_string),
                        is_pk: pk.contains(&name),
                        foreign_key: None,
                        enum_values: Vec::new(),
                        sql_type: ty,
                        name,
                    }
                })
                .collect())
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
        Box::pin(async move { Ok(f.await?.first().and_then(|r| r["n"].as_i64()).unwrap_or(0)) })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
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
        self.fut(sql, req.limit)
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move {
            let sql = super::trim_sql(&sql);
            let label = sql.clone();
            db::run_logged(label, crate::console::Source::Data, async move {
                let (fields, rows, affected) = this.query(sql, limit.max(0) as usize).await?;
                if fields.is_empty() {
                    return Ok(vec![json!({ "rows affected": affected })]);
                }
                Ok(rows)
            })
            .await
        })
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let this = self.clone();
        Box::pin(async move {
            let sql = format!("SELECT * FROM ({}) LIMIT 0", super::trim_sql(&sql));
            Ok(this
                .query(sql, 1)
                .await?
                .0
                .iter()
                .map(|f| s(&f["name"]))
                .collect())
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let this = self.clone();
        let types = self.types.clone();
        Box::pin(async move {
            let sql = super::trim_sql(&sql);
            let label = sql.clone();
            let n = db::run_logged(label, crate::console::Source::Data, async move {
                // A script runs as one job; the emulator takes one statement at a time.
                if this.emulator {
                    let mut n = 0;
                    for st in db::split_statements(&sql) {
                        n += this.query(st, 1).await?.2;
                    }
                    Ok(n)
                } else {
                    Ok(this.query(sql, 1).await?.2)
                }
            })
            .await?;
            types.lock().unwrap().clear();
            Ok(n)
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let this = self.clone();
        Box::pin(async move {
            let mut sqls = Vec::new();
            for st in &stmts {
                sqls.push(match super::parse_grid_stmt(st) {
                    Some(op) => this.grid_sql(op).await?,
                    None => super::clickhouse::inline(&st.sql, &st.params, D),
                });
            }
            let started = std::time::Instant::now();
            let label = sqls.join(";\n");
            let r = async {
                // DDL can't run inside a transaction: one job at a time.
                let ddl = sqls.iter().any(|q| {
                    let head = q.split_whitespace().next().unwrap_or("").to_uppercase();
                    matches!(head.as_str(), "CREATE" | "ALTER" | "DROP" | "TRUNCATE")
                });
                if ddl {
                    this.types.lock().unwrap().clear();
                }
                if this.emulator || sqls.len() == 1 || ddl {
                    let mut n = 0;
                    for q in &sqls {
                        n += this.query(q.clone(), 1).await?.2;
                    }
                    Ok(n)
                } else {
                    // One multi-statement transaction.
                    let script = format!(
                        "BEGIN TRANSACTION;\n{};\nCOMMIT TRANSACTION;",
                        sqls.join(";\n")
                    );
                    this.query(script, 1).await.map(|_| stmts.len() as u64)
                }
            }
            .await;
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
        let this = self.clone();
        let target = D.qualified(&schema, &name, true);
        Box::pin(async move {
            let t = this.table(&schema, &name).await?;
            let cols = this.columns(schema.clone(), name.clone()).await?;
            Ok(match (kind, which) {
                (ObjKind::View | ObjKind::MatView, Script::Create) => {
                    let q = t["view"]["query"]
                        .as_str()
                        .or(t["materializedView"]["query"].as_str())
                        .unwrap_or_default();
                    format!("CREATE VIEW {target} AS\n{q};")
                }
                (_, Script::Create) => {
                    let lines: Vec<String> = cols
                        .iter()
                        .map(|c| {
                            format!(
                                "    {} {}{}",
                                D.quote(&c.name),
                                c.sql_type,
                                if c.nullable { "" } else { " NOT NULL" }
                            )
                        })
                        .collect();
                    format!("CREATE TABLE {target} (\n{}\n);", lines.join(",\n"))
                }
                (ObjKind::View, Script::Drop) => format!("DROP VIEW {target};"),
                (_, Script::Select) => format!("SELECT * FROM {target} LIMIT 100;"),
                (_, w) => super::dml_script(D, w, &target, &cols),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn literals() {
        assert_eq!(super::literal(Some("it's"), "STRING"), r"'it\'s'");
        assert_eq!(super::literal(Some("5"), "INT64"), "CAST('5' AS INT64)");
        assert_eq!(super::literal(None, "INT64"), "NULL");
    }

    #[test]
    fn live_bigquery_emulator() {
        if !live::reachable(39050) {
            return;
        }
        let mut c = live::conn(Engine::BigQuery, 0, "", "");
        c.options.insert("project".into(), "tusk-test".into());
        c.options.insert("dataset".into(), "shop".into());
        c.options
            .insert("endpoint".into(), "http://127.0.0.1:39050".into());
        let rt = crate::db::runtime();
        let db = rt.block_on(super::connect(&c, String::new())).unwrap();
        let d = db.driver();
        rt.block_on(d.exec("DROP TABLE IF EXISTS shop.people".into()))
            .ok();
        rt.block_on(d.exec(
            "CREATE TABLE shop.people (id INT64 NOT NULL, email STRING, score FLOAT64, active BOOL, tags ARRAY<STRING>);
             INSERT INTO shop.people (id, email, score, active, tags) VALUES (1, 'a@x', 1.5, true, ['x']), (2, 'b@x', NULL, false, [])"
                .into(),
        ))
        .unwrap();
        assert!(
            rt.block_on(d.schemas())
                .unwrap()
                .contains(&"shop".to_string())
        );
        assert!(
            rt.block_on(d.objects("shop".into()))
                .unwrap()
                .tables
                .contains(&"people".to_string())
        );
        let cols = rt
            .block_on(d.columns("shop".into(), "people".into()))
            .unwrap();
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "email", "score", "active", "tags"]);
        assert_eq!(
            (cols[0].pg_type.as_str(), cols[4].sql_type.as_str()),
            ("int8", "ARRAY<STRING>")
        );
        assert_eq!(
            rt.block_on(d.count("shop".into(), "people".into(), None))
                .unwrap(),
            2
        );
        let rows = rt
            .block_on(d.window(crate::drivers::WindowReq {
                schema: "shop".into(),
                table: "people".into(),
                filter: None,
                order_by: Some("ORDER BY `id`".into()),
                with_key: false,
                limit: 10,
                offset: 0,
            }))
            .unwrap();
        assert_eq!(rows[0]["id"], 1);
        assert_eq!(rows[0]["score"], 1.5);
        assert_eq!(rows[0]["active"], true);
        assert_eq!(rows[0]["tags"], serde_json::json!(["x"]));
        let st = |sql: &str, p: &[Option<&str>]| crate::db::Stmt {
            sql: sql.into(),
            params: p.iter().map(|v| v.map(str::to_string)).collect(),
        };
        rt.block_on(d.batch(vec![
            st(
                "UPDATE `shop`.`people` SET `email` = ?, `score` = ? WHERE `id` = ?",
                &[Some("it's@x"), Some("2.5"), Some("2")],
            ),
            st(
                "INSERT INTO `shop`.`people` (`id`, `email`) VALUES (?, ?)",
                &[Some("3"), Some("c@x")],
            ),
            st("DELETE FROM `shop`.`people` WHERE `id` = ?", &[Some("1")]),
        ]))
        .unwrap();
        let r = rt
            .block_on(d.query_rows(
                "SELECT id, email, score FROM shop.people ORDER BY id".into(),
                10,
            ))
            .unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(
            (r[0]["email"].clone(), r[0]["score"].clone()),
            (serde_json::json!("it's@x"), serde_json::json!(2.5))
        );
        rt.block_on(d.exec("DROP TABLE shop.people".into()))
            .unwrap();
    }
}
