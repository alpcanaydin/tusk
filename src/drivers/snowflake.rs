//! Snowflake over the SQL API (`/api/v2/statements`). Sign-in with a
//! programmatic access token or a key pair (a JWT signed with the user's
//! RSA key). Results arrive as text in partitions, typed by `rowType`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Db, Driver, Fut, WindowReq, http};
use crate::db::{self, DbResult, GridColumnMeta, ObjectTree, SavedConnection, Stmt, WhereClause};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

const D: Dialect = Dialect::Snowflake;

#[derive(Clone)]
enum Auth {
    Token(String),
    KeyPair {
        key_pem: String,
        account: String,
        user: String,
    },
}

#[derive(Clone)]
pub struct Snowflake {
    url: String,
    database: String,
    schema: String,
    warehouse: String,
    role: String,
    auth: Auth,
    jwt: Arc<Mutex<Option<(String, Instant)>>>,
}

/// `xy12345.eu-central-1` / `org-account` / a full URL → the API host.
pub fn host_of(account: &str) -> String {
    let a = account
        .trim()
        .trim_start_matches("https://")
        .trim_end_matches('/');
    if a.contains(".snowflakecomputing.") {
        a.to_string()
    } else {
        format!("{a}.snowflakecomputing.com")
    }
}

pub async fn connect(conn: &SavedConnection, secret: String) -> DbResult<Db> {
    let account = conn.opt("account").trim().to_string();
    if account.is_empty() {
        return Err("Fill in the account".into());
    }
    let key = conn.opt("key_file").trim();
    let auth = if !key.is_empty() {
        let p = super::sqlite::shellexpand(key);
        let key_pem = std::fs::read_to_string(&p)
            .map_err(|e| format!("Can't read the private key {p}: {e}"))?;
        // The account part of the identifier: before the region / cloud.
        let locator = account.split('.').next().unwrap_or(&account).to_uppercase();
        Auth::KeyPair {
            key_pem,
            account: locator,
            user: conn.user.trim().to_uppercase(),
        }
    } else if !secret.trim().is_empty() {
        Auth::Token(secret.trim().to_string())
    } else {
        return Err("Enter an access token or choose a private key".into());
    };
    let endpoint = conn
        .opt("endpoint")
        .trim()
        .trim_end_matches('/')
        .to_string();
    let sf = Snowflake {
        url: if endpoint.is_empty() {
            format!("https://{}", host_of(&account))
        } else {
            endpoint
        },
        database: conn.database.trim().to_string(),
        schema: conn.opt("schema").trim().to_string(),
        warehouse: conn.opt("warehouse").trim().to_string(),
        role: conn.opt("role").trim().to_string(),
        auth,
        jwt: Default::default(),
    };
    sf.run("SELECT CURRENT_VERSION() AS v".into(), 1).await?;
    Ok(Db::new(sf))
}

/// `SHA256:<base64>` of the public key's DER: the fingerprint Snowflake
/// stores with `ALTER USER … SET RSA_PUBLIC_KEY`.
pub fn fingerprint(key_pem: &str) -> DbResult<String> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey};
    let key = rsa::RsaPrivateKey::from_pkcs8_pem(key_pem)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(key_pem))
        .map_err(|e| format!("Bad private key (an unencrypted PEM is needed): {e}"))?;
    let der = key
        .to_public_key()
        .to_public_key_der()
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(der.as_bytes()))
    ))
}

/// One result cell (text) typed by its `rowType` entry.
pub fn cell(col: &Value, v: &Value) -> Value {
    let Some(t) = v.as_str() else {
        return Value::Null;
    };
    let scale = col["scale"].as_i64().unwrap_or(0);
    match col["type"].as_str().unwrap_or("text") {
        "fixed" if scale == 0 => t
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| v.clone()),
        "real" => t
            .parse::<f64>()
            .map(Value::from)
            .unwrap_or_else(|_| v.clone()),
        "boolean" => Value::Bool(t.eq_ignore_ascii_case("true") || t == "1"),
        "date" => t
            .parse::<i64>()
            .ok()
            .and_then(|days| {
                chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?
                    .checked_add_signed(chrono::Duration::days(days))
            })
            .map(|d| Value::String(d.to_string()))
            .unwrap_or_else(|| v.clone()),
        "time" => t
            .parse::<f64>()
            .ok()
            .and_then(|secs| {
                chrono::NaiveTime::from_num_seconds_from_midnight_opt(
                    secs as u32,
                    ((secs.fract()) * 1e9) as u32,
                )
            })
            .map(|d| Value::String(d.to_string()))
            .unwrap_or_else(|| v.clone()),
        "timestamp_ntz" | "timestamp_ltz" | "timestamp_tz" => {
            // `secs.frac` (+ ` offset` in minutes + 1440 for _tz).
            let mut parts = t.split(' ');
            let secs: f64 = parts
                .next()
                .and_then(|p| p.parse().ok())
                .unwrap_or(f64::NAN);
            let Some(utc) = chrono::DateTime::from_timestamp_micros((secs * 1e6).round() as i64)
            else {
                return v.clone();
            };
            match parts.next().and_then(|o| o.parse::<i32>().ok()) {
                Some(off) => {
                    let off = chrono::FixedOffset::east_opt((off - 1440) * 60)
                        .unwrap_or(chrono::FixedOffset::east_opt(0).unwrap());
                    Value::String(
                        utc.with_timezone(&off)
                            .format("%Y-%m-%d %H:%M:%S%.f %:z")
                            .to_string(),
                    )
                }
                None if col["type"] == "timestamp_ntz" => {
                    Value::String(utc.naive_utc().format("%Y-%m-%d %H:%M:%S%.f").to_string())
                }
                None => Value::String(utc.format("%Y-%m-%d %H:%M:%S%.f UTC").to_string()),
            }
        }
        "variant" | "object" | "array" => serde_json::from_str(t).unwrap_or_else(|_| v.clone()),
        _ => v.clone(),
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

impl Snowflake {
    fn bearer(&self) -> DbResult<(String, &'static str)> {
        match &self.auth {
            Auth::Token(t) => Ok((t.clone(), "PROGRAMMATIC_ACCESS_TOKEN")),
            Auth::KeyPair {
                key_pem,
                account,
                user,
            } => {
                if let Some((t, at)) = self.jwt.lock().unwrap().clone()
                    && at.elapsed() < Duration::from_secs(50 * 60)
                {
                    return Ok((t, "KEYPAIR_JWT"));
                }
                let now = chrono::Utc::now().timestamp();
                let claims = json!({
                    "iss": format!("{account}.{user}.{}", fingerprint(key_pem)?),
                    "sub": format!("{account}.{user}"),
                    "iat": now,
                    "exp": now + 3600,
                });
                let key = jsonwebtoken::EncodingKey::from_rsa_pem(key_pem.as_bytes())
                    .map_err(|e| e.to_string())?;
                let t = jsonwebtoken::encode(
                    &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
                    &claims,
                    &key,
                )
                .map_err(|e| e.to_string())?;
                *self.jwt.lock().unwrap() = Some((t.clone(), Instant::now()));
                Ok((t, "KEYPAIR_JWT"))
            }
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> DbResult<reqwest::RequestBuilder> {
        let (token, kind) = self.bearer()?;
        Ok(http::client()
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(token)
            .header("X-Snowflake-Authorization-Token-Type", kind)
            .header("Accept", "application/json"))
    }

    /// Run one statement (or a `;` script): (rowType, rows, affected).
    async fn statement(
        &self,
        sql: String,
        limit: usize,
    ) -> DbResult<(Vec<Value>, Vec<Value>, u64)> {
        let statements = db::split_statements(&sql).len().max(1);
        let mut body = json!({"statement": sql, "timeout": 600});
        for (k, v) in [
            ("database", &self.database),
            ("schema", &self.schema),
            ("warehouse", &self.warehouse),
            ("role", &self.role),
        ] {
            if !v.is_empty() {
                body[k] = json!(v);
            }
        }
        if statements > 1 {
            body["parameters"] = json!({"MULTI_STATEMENT_COUNT": statements.to_string()});
        }
        let req = self
            .request(reqwest::Method::POST, "/api/v2/statements?async=false")?
            .json(&body);
        let mut r = http::send_json(req).await?;
        // 202: still running — poll the handle.
        let handle = s(&r["statementHandle"]);
        let mut wait = Duration::from_millis(200);
        while r["resultSetMetaData"].is_null() && r["code"] == "333334" {
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(2));
            r = http::send_json(self.request(
                reqwest::Method::GET,
                &format!("/api/v2/statements/{handle}"),
            )?)
            .await?;
        }
        // A script: the result of its last statement.
        if let Some(last) = r["statementHandles"]
            .as_array()
            .and_then(|a| a.last())
            .map(s)
        {
            r = http::send_json(
                self.request(reqwest::Method::GET, &format!("/api/v2/statements/{last}"))?,
            )
            .await?;
        }
        let meta = &r["resultSetMetaData"];
        let cols = meta["rowType"].as_array().cloned().unwrap_or_default();
        let mut raw: Vec<Value> = r["data"].as_array().cloned().unwrap_or_default();
        let parts = meta["partitionInfo"].as_array().map(Vec::len).unwrap_or(1);
        let handle = s(&r["statementHandle"]);
        for p in 1..parts {
            if raw.len() >= limit {
                break;
            }
            let next = http::send_json(self.request(
                reqwest::Method::GET,
                &format!("/api/v2/statements/{handle}?partition={p}"),
            )?)
            .await?;
            raw.extend(next["data"].as_array().cloned().unwrap_or_default());
        }
        raw.truncate(limit);
        let names: Vec<String> = cols.iter().map(|c| s(&c["name"])).collect();
        let rows = raw
            .iter()
            .map(|row| {
                let cells = row.as_array().cloned().unwrap_or_default();
                let vals: Vec<Value> = cols
                    .iter()
                    .zip(cells.iter())
                    .map(|(c, v)| cell(c, v))
                    .collect();
                super::objects_from(&names, vec![vals])
                    .pop()
                    .unwrap_or(Value::Null)
            })
            .collect();
        let affected = r["stats"]["numRowsInserted"].as_u64().unwrap_or(0)
            + r["stats"]["numRowsUpdated"].as_u64().unwrap_or(0)
            + r["stats"]["numRowsDeleted"].as_u64().unwrap_or(0);
        Ok((cols, rows, affected))
    }

    async fn run(&self, sql: String, limit: i64) -> DbResult<Vec<Value>> {
        let sql = super::trim_sql(&sql);
        let this = self.clone();
        let label = sql.clone();
        db::run_logged(label, crate::console::Source::Data, async move {
            Ok(this.statement(sql, limit.max(0) as usize).await?.1)
        })
        .await
    }

    fn fut(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move { this.run(sql, limit).await })
    }
}

fn where_sql(filter: &Option<WhereClause>) -> String {
    match filter {
        Some(w) if !w.sql.is_empty() => format!("WHERE {}", w.sql),
        _ => String::new(),
    }
}

/// `information_schema` of the database holding `schema`.
fn info(database: &str) -> String {
    if database.is_empty() {
        "information_schema".into()
    } else {
        format!("{}.information_schema", D.quote(database))
    }
}

impl Driver for Snowflake {
    fn engine(&self) -> Engine {
        Engine::Snowflake
    }
    fn default_schema(&self) -> Option<String> {
        Some(if self.schema.is_empty() {
            "PUBLIC".into()
        } else {
            self.schema.to_uppercase()
        })
    }
    fn row_key(&self) -> bool {
        false
    }
    fn version(&self) -> Fut<String> {
        let f = self.fut("SELECT CURRENT_VERSION() AS v".into(), 1);
        Box::pin(async move {
            Ok(format!(
                "Snowflake {}",
                f.await?.first().map(|r| s(&r["V"])).unwrap_or_default()
            ))
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        let f = self.fut("SHOW TERSE DATABASES".into(), 10_000);
        Box::pin(async move { Ok(f.await?.iter().map(|r| s(&r["name"])).collect()) })
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let f = self.fut(
            format!("SELECT schema_name AS n FROM {}.schemata WHERE schema_name <> 'INFORMATION_SCHEMA' ORDER BY 1", info(&self.database)),
            10_000,
        );
        Box::pin(async move { Ok(f.await?.iter().map(|r| s(&r["N"])).collect()) })
    }
    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let lit = D.literal(&schema);
        let t = self.fut(
            format!("SELECT table_name AS n, table_type AS k FROM {}.tables WHERE table_schema = {lit} ORDER BY 1", info(&self.database)),
            100_000,
        );
        let f = self.fut(
            format!("SELECT DISTINCT function_name AS n FROM {}.functions WHERE function_schema = {lit} ORDER BY 1", info(&self.database)),
            100_000,
        );
        Box::pin(async move {
            let mut tree = ObjectTree::default();
            for r in t.await? {
                match s(&r["K"]).as_str() {
                    "VIEW" => tree.views.push(s(&r["N"])),
                    "MATERIALIZED VIEW" => tree.matviews.push(s(&r["N"])),
                    _ => tree.tables.push(s(&r["N"])),
                }
            }
            tree.functions = f
                .await
                .unwrap_or_default()
                .iter()
                .map(|r| s(&r["N"]))
                .collect();
            Ok(tree)
        })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let cols = self.fut(
            format!(
                "SELECT column_name AS name, data_type AS dt, character_maximum_length AS len, numeric_precision AS prec,
                        numeric_scale AS scale, is_nullable AS nullable, column_default AS dflt, comment AS cmt
                   FROM {}.columns WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                info(&self.database),
                D.literal(&schema),
                D.literal(&table)
            ),
            10_000,
        );
        let pk = self.fut(
            format!(
                "SHOW PRIMARY KEYS IN TABLE {}",
                D.qualified(&schema, &table, true)
            ),
            1000,
        );
        Box::pin(async move {
            let pk: Vec<String> = pk
                .await
                .unwrap_or_default()
                .iter()
                .map(|r| s(&r["column_name"]))
                .collect();
            Ok(cols
                .await?
                .iter()
                .map(|r| {
                    let dt = s(&r["DT"]);
                    let sql_type = match dt.as_str() {
                        "TEXT" if !r["LEN"].is_null() && r["LEN"].as_i64() != Some(16_777_216) => {
                            format!("VARCHAR({})", s(&r["LEN"]))
                        }
                        "NUMBER" => format!("NUMBER({},{})", s(&r["PREC"]), s(&r["SCALE"])),
                        _ => dt.clone(),
                    };
                    let short = match dt.as_str() {
                        "NUMBER" if r["SCALE"].as_i64() == Some(0) => "int8".into(),
                        "NUMBER" => "numeric".into(),
                        "TEXT" => "text".into(),
                        "TIMESTAMP_NTZ" => "timestamp".into(),
                        "TIMESTAMP_LTZ" | "TIMESTAMP_TZ" => "timestamptz".into(),
                        other => super::short_type(other),
                    };
                    let name = s(&r["NAME"]);
                    GridColumnMeta {
                        is_pk: pk.contains(&name),
                        pg_type: short,
                        sql_type,
                        nullable: s(&r["NULLABLE"]) == "YES",
                        default: Some(s(&r["DFLT"])).filter(|d| !d.is_empty()),
                        comment: Some(s(&r["CMT"])).filter(|d| !d.is_empty()),
                        foreign_key: None,
                        enum_values: Vec::new(),
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
        Box::pin(async move { Ok(f.await?.first().and_then(|r| r["N"].as_i64()).unwrap_or(0)) })
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
        self.fut(sql, limit)
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let this = self.clone();
        Box::pin(async move {
            let sql = format!("SELECT * FROM ({}) LIMIT 0", super::trim_sql(&sql));
            let cols = db::run_db(async move { this.statement(sql, 0).await })
                .await?
                .0;
            Ok(cols.iter().map(|c| s(&c["name"])).collect())
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let this = self.clone();
        Box::pin(async move {
            let sql = super::trim_sql(&sql);
            let label = sql.clone();
            db::run_logged(label, crate::console::Source::Data, async move {
                Ok(this.statement(sql, 0).await?.2)
            })
            .await
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let this = self.clone();
        Box::pin(async move {
            // One script in a transaction; values inlined (a script can't bind).
            let body: Vec<String> = stmts
                .iter()
                .map(|st| super::clickhouse::inline(&st.sql, &st.params, D))
                .collect();
            let script = format!("BEGIN;\n{};\nCOMMIT;", body.join(";\n"));
            let started = std::time::Instant::now();
            let n = stmts.len() as u64;
            let sql = script.clone();
            let r = db::run_db(async move { this.statement(sql, 0).await.map(|_| n) }).await;
            crate::console::record(
                &script,
                started,
                crate::console::Source::Data,
                r.as_ref().err().map(String::as_str),
            );
            r
        })
    }
    fn script(&self, kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String> {
        let target = D.qualified(&schema, &name, true);
        let what = match kind {
            ObjKind::View | ObjKind::MatView => "VIEW",
            ObjKind::Function => "FUNCTION",
            _ => "TABLE",
        };
        let ddl = self.fut(
            format!(
                "SELECT GET_DDL({}, {}) AS d",
                D.literal(what),
                D.literal(&format!("{schema}.{name}"))
            ),
            1,
        );
        let cols = self.columns(schema, name);
        Box::pin(async move {
            match which {
                Script::Create => Ok(ddl.await?.first().map(|r| s(&r["D"])).unwrap_or_default()),
                Script::Drop => Ok(format!("DROP {what} {target};")),
                w => Ok(super::dml_script(D, w, &target, &cols.await?)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn accounts_and_cells() {
        assert_eq!(
            super::host_of("xy12345.eu-central-1"),
            "xy12345.eu-central-1.snowflakecomputing.com"
        );
        assert_eq!(
            super::host_of("https://org-acct.snowflakecomputing.com/"),
            "org-acct.snowflakecomputing.com"
        );
        let col = |t: &str, scale: i64| json!({"type": t, "scale": scale});
        assert_eq!(super::cell(&col("fixed", 0), &json!("42")), json!(42));
        assert_eq!(super::cell(&col("fixed", 2), &json!("4.20")), json!("4.20"));
        assert_eq!(
            super::cell(&col("date", 0), &json!("19000")),
            json!("2022-01-08")
        );
        assert_eq!(
            super::cell(&col("timestamp_ntz", 9), &json!("1700000000.500000000")),
            json!("2023-11-14 22:13:20.500")
        );
        assert_eq!(
            super::cell(&col("timestamp_tz", 9), &json!("1700000000.000000000 1500")),
            json!("2023-11-14 23:13:20 +01:00")
        );
        assert_eq!(
            super::cell(&col("variant", 0), &json!("{\"a\":1}")),
            json!({"a": 1})
        );
        assert_eq!(
            super::cell(&col("text", 0), &serde_json::Value::Null),
            serde_json::Value::Null
        );
        assert_eq!(
            crate::engine::Dialect::Snowflake.literal(r"a\b'c"),
            r"'a\\b''c'"
        );
    }

    #[test]
    fn key_pair_fingerprint() {
        use rsa::pkcs8::EncodePrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).unwrap();
        let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        let fp = super::fingerprint(&pem).unwrap();
        assert!(fp.starts_with("SHA256:") && fp.len() == 7 + 44, "{fp}");
    }
}
