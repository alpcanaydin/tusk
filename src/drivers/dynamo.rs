//! Amazon DynamoDB over its JSON API, signed with AWS Signature V4. Tables
//! are read with Scan (key attributes first, then the attributes seen in
//! a sample); the editor runs PartiQL; a save is one ExecuteTransaction.

use std::collections::HashMap;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{Db, Driver, Fut, GridOp, WindowReq, http};
use crate::db::{
    self, DbResult, FilterTerm, GridColumnMeta, IndexDef, ObjectTree, SavedConnection, Stmt,
    WhereClause,
};
use crate::engine::Engine;
use crate::filter::FilterOp;
use crate::objects::{ObjKind, Script};

/// Items read to infer a table's non-key attributes.
const SAMPLE: usize = 200;
/// Items a window may scan past (Scan has no offset).
const MAX_SCAN: usize = 200_000;

#[derive(Clone)]
pub struct Dynamo {
    endpoint: String,
    region: String,
    access_key: String,
    secret: String,
    token: Option<String>,
    /// (name, attribute type) per table, key attributes first.
    shapes: super::Shapes,
}

pub async fn connect(conn: &SavedConnection, secret: String) -> DbResult<Db> {
    let region = match conn.opt("region") {
        "" => "us-east-1".to_string(),
        r => r.trim().to_string(),
    };
    let endpoint = match conn.opt("endpoint").trim() {
        "" => format!("https://dynamodb.{region}.amazonaws.com"),
        e if e.starts_with("http") => e.trim_end_matches('/').to_string(),
        e => format!("https://{}", e.trim_end_matches('/')),
    };
    let d = Dynamo {
        endpoint,
        region,
        access_key: conn.opt("access_key").trim().to_string(),
        secret,
        token: Some(conn.opt("session_token").to_string()).filter(|t| !t.is_empty()),
        shapes: Default::default(),
    };
    d.call("ListTables", json!({"Limit": 1})).await?;
    Ok(Db::new(d))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

fn sha_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// AWS credentials for signing.
pub struct Creds<'a> {
    pub access_key: &'a str,
    pub secret: &'a str,
    pub token: Option<&'a str>,
    pub region: &'a str,
}

/// SigV4 `Authorization` for a POST to `/` of `host`.
pub fn sign(c: &Creds, host: &str, amz_date: &str, target: &str, body: &[u8]) -> String {
    let Creds {
        access_key,
        secret,
        token,
        region,
    } = *c;
    let date = &amz_date[..8];
    let mut headers = vec![
        ("content-type", "application/x-amz-json-1.0".to_string()),
        ("host", host.to_string()),
        ("x-amz-date", amz_date.to_string()),
    ];
    if let Some(t) = token {
        headers.push(("x-amz-security-token", t.to_string()));
    }
    headers.push(("x-amz-target", target.to_string()));
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
    let signed = signed.join(";");
    let canonical = format!(
        "POST\n/\n\n{canonical_headers}\n{signed}\n{}",
        sha_hex(body)
    );
    let scope = format!("{date}/{region}/dynamodb/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha_hex(canonical.as_bytes())
    );
    let mut k = hmac(format!("AWS4{secret}").as_bytes(), date);
    for part in [region, "dynamodb", "aws4_request"] {
        k = hmac(&k, part);
    }
    let signature = hex::encode(hmac(&k, &to_sign));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={signature}"
    )
}

// ---- AttributeValue <-> JSON --------------------------------------------

/// An AttributeValue as the grid's JSON.
pub fn to_json(av: &Value) -> Value {
    let Some((ty, v)) = av.as_object().and_then(|o| o.iter().next()) else {
        return Value::Null;
    };
    match ty.as_str() {
        "S" => v.clone(),
        "N" => v
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .filter(Value::is_number)
            .unwrap_or_else(|| v.clone()),
        "BOOL" => v.clone(),
        "NULL" => Value::Null,
        "B" => base64::engine::general_purpose::STANDARD
            .decode(v.as_str().unwrap_or(""))
            .map(|b| super::hex(&b))
            .unwrap_or(Value::Null),
        "M" => Value::Object(
            v.as_object()
                .into_iter()
                .flatten()
                .map(|(k, x)| (k.clone(), to_json(x)))
                .collect(),
        ),
        "L" => Value::Array(v.as_array().into_iter().flatten().map(to_json).collect()),
        "SS" => v.clone(),
        "NS" => Value::Array(
            v.as_array()
                .into_iter()
                .flatten()
                .map(|n| to_json(&json!({"N": n})))
                .collect(),
        ),
        "BS" => Value::Array(
            v.as_array()
                .into_iter()
                .flatten()
                .map(|b| to_json(&json!({"B": b})))
                .collect(),
        ),
        _ => v.clone(),
    }
}

/// Plain JSON as an AttributeValue.
fn from_json(v: &Value) -> Value {
    match v {
        Value::Null => json!({"NULL": true}),
        Value::Bool(b) => json!({"BOOL": b}),
        Value::Number(n) => json!({"N": n.to_string()}),
        Value::String(s) => json!({"S": s}),
        Value::Array(a) => json!({"L": a.iter().map(from_json).collect::<Vec<_>>()}),
        Value::Object(m) => {
            json!({"M": m.iter().map(|(k, x)| (k.clone(), from_json(x))).collect::<Map<_, _>>()})
        }
    }
}

/// Grid text as an AttributeValue of type `ty` (S, N, BOOL, M, L, SS, …).
pub fn typed(text: Option<&str>, ty: &str) -> DbResult<Value> {
    let Some(t) = text else {
        return Ok(json!({"NULL": true}));
    };
    let parsed =
        || serde_json::from_str::<Value>(t).map_err(|_| format!("Enter the {ty} value as JSON"));
    Ok(match ty {
        "N" => {
            let n = t.trim();
            if n.parse::<f64>().is_err() {
                return Err(format!("'{t}' isn't a number"));
            }
            json!({"N": n})
        }
        "BOOL" => match t.trim().to_lowercase().as_str() {
            "true" | "t" | "1" => json!({"BOOL": true}),
            "false" | "f" | "0" => json!({"BOOL": false}),
            _ => return Err(format!("'{t}' isn't a boolean")),
        },
        "B" => {
            let h = t.trim().trim_start_matches("\\x").trim_start_matches("0x");
            let bytes = hex::decode(h).map_err(|_| "Binary values are hex".to_string())?;
            json!({"B": base64::engine::general_purpose::STANDARD.encode(bytes)})
        }
        "M" | "L" | "mixed" => match parsed() {
            Ok(v) => from_json(&v),
            Err(_) if ty == "mixed" => json!({"S": t}),
            Err(e) => return Err(e),
        },
        "SS" | "NS" => {
            let items: Vec<String> = parsed()?
                .as_array()
                .ok_or("A set is a JSON array")?
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    o => o.to_string(),
                })
                .collect();
            json!({ ty: items })
        }
        _ => json!({"S": t}),
    })
}

fn grid_type(ty: &str) -> String {
    match ty {
        "S" => "text",
        "N" => "numeric",
        "BOOL" => "bool",
        "B" => "bytea",
        _ => "json",
    }
    .into()
}

/// Terms from the filter bar as a Scan FilterExpression (+ names / values).
fn filter_expr(
    terms: &[FilterTerm],
    shape: &[(String, String)],
) -> DbResult<Option<(String, Value, Value)>> {
    if terms.is_empty() {
        return Ok(None);
    }
    let ty = |c: &str| {
        shape
            .iter()
            .find(|(n, _)| n == c)
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| "S".into())
    };
    let mut names = Map::new();
    let mut values = Map::new();
    let mut parts = Vec::new();
    for (i, FilterTerm { column, op, value }) in terms.iter().enumerate() {
        let n = format!("#n{i}");
        names.insert(n.clone(), json!(column));
        let t = ty(column);
        let scalar = if matches!(t.as_str(), "SS" | "NS" | "L" | "M") {
            "S".to_string()
        } else {
            t.clone()
        };
        let mut val = |j: usize, v: &str, as_type: &str| -> DbResult<String> {
            let k = format!(":v{i}_{j}");
            values.insert(k.clone(), typed(Some(v), as_type)?);
            Ok(k)
        };
        parts.push(match op {
            FilterOp::Eq => format!("{n} = {}", val(0, value, &t)?),
            FilterOp::Ne => format!("{n} <> {}", val(0, value, &t)?),
            FilterOp::Lt => format!("{n} < {}", val(0, value, &t)?),
            FilterOp::Gt => format!("{n} > {}", val(0, value, &t)?),
            FilterOp::Le => format!("{n} <= {}", val(0, value, &t)?),
            FilterOp::Ge => format!("{n} >= {}", val(0, value, &t)?),
            FilterOp::In | FilterOp::NotIn => {
                let items = crate::filter::split_list(value);
                let keys: DbResult<Vec<String>> = items
                    .iter()
                    .enumerate()
                    .map(|(j, v)| val(j, v, &t))
                    .collect();
                let e = format!("{n} IN ({})", keys?.join(", "));
                if *op == FilterOp::NotIn {
                    format!("NOT ({e})")
                } else {
                    e
                }
            }
            FilterOp::Between => {
                let items = crate::filter::split_list(value);
                let [lo, hi] = items.as_slice() else { continue };
                format!("{n} BETWEEN {} AND {}", val(0, lo, &t)?, val(1, hi, &t)?)
            }
            FilterOp::IsNull => {
                format!("(attribute_not_exists({n}) OR attribute_type({n}, :null{i}))")
            }
            FilterOp::IsNotNull => {
                format!("(attribute_exists({n}) AND NOT attribute_type({n}, :null{i}))")
            }
            FilterOp::Contains | FilterOp::Like | FilterOp::ILike => {
                format!("contains({n}, {})", val(0, value, &scalar)?)
            }
            FilterOp::NotContains => format!("NOT contains({n}, {})", val(0, value, &scalar)?),
            FilterOp::HasPrefix => format!("begins_with({n}, {})", val(0, value, &scalar)?),
            FilterOp::HasSuffix => return Err("DynamoDB can't filter by suffix".into()),
        });
        if matches!(op, FilterOp::IsNull | FilterOp::IsNotNull) {
            values.insert(format!(":null{i}"), json!({"S": "NULL"}));
        }
    }
    Ok(Some((
        parts.join(" AND "),
        Value::Object(names),
        Value::Object(values),
    )))
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Dynamo {
    async fn call(&self, op: &str, body: Value) -> DbResult<Value> {
        let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
        let url = reqwest::Url::parse(&self.endpoint).map_err(|e| e.to_string())?;
        let host = match url.port() {
            Some(p) => format!("{}:{p}", url.host_str().unwrap_or("")),
            None => url.host_str().unwrap_or("").to_string(),
        };
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let target = format!("DynamoDB_20120810.{op}");
        let creds = Creds {
            access_key: &self.access_key,
            secret: &self.secret,
            token: self.token.as_deref(),
            region: &self.region,
        };
        let auth = sign(&creds, &host, &amz_date, &target, &body);
        let mut req = http::client()
            .post(url)
            .header("content-type", "application/x-amz-json-1.0")
            .header("x-amz-date", &amz_date)
            .header("x-amz-target", &target)
            .header("authorization", auth)
            .body(body);
        if let Some(t) = &self.token {
            req = req.header("x-amz-security-token", t);
        }
        http::send_json(req).await.map_err(|e| {
            // `HTTP 400: com.amazonaws…#ResourceNotFoundException` → the message part.
            e.split_once('#').map(|(_, m)| m.to_string()).unwrap_or(e)
        })
    }

    async fn describe(&self, table: &str) -> DbResult<Value> {
        Ok(self
            .call("DescribeTable", json!({"TableName": table}))
            .await?["Table"]
            .clone())
    }

    /// Key attributes (hash, range) then sampled ones, with their types.
    async fn shape(&self, table: &str) -> DbResult<Vec<(String, String)>> {
        if let Some(s) = self.shapes.lock().unwrap().get(table) {
            return Ok(s.clone());
        }
        let t = self.describe(table).await?;
        let defs: HashMap<String, String> = t["AttributeDefinitions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| {
                (
                    a["AttributeName"].as_str().unwrap_or("").to_string(),
                    a["AttributeType"].as_str().unwrap_or("S").to_string(),
                )
            })
            .collect();
        let mut keys: Vec<(String, String)> = t["KeySchema"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|k| {
                (
                    k["KeyType"].as_str().unwrap_or("").to_string(),
                    k["AttributeName"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        keys.sort_by_key(|(kind, _)| kind != "HASH");
        let mut shape: Vec<(String, String)> = keys
            .into_iter()
            .map(|(_, n)| {
                (
                    n.clone(),
                    defs.get(&n).cloned().unwrap_or_else(|| "S".into()),
                )
            })
            .collect();
        let key_count = shape.len();
        let sample = self
            .call("Scan", json!({"TableName": table, "Limit": SAMPLE}))
            .await?;
        let mut seen: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for item in sample["Items"].as_array().into_iter().flatten() {
            for (k, av) in item.as_object().into_iter().flatten() {
                if shape[..key_count].iter().any(|(n, _)| n == k) {
                    continue;
                }
                if !order.contains(k) {
                    order.push(k.clone());
                }
                if let Some(ty) = av.as_object().and_then(|o| o.keys().next())
                    && ty != "NULL"
                {
                    seen.entry(k.clone()).or_default().insert(ty.clone());
                }
            }
        }
        order.sort();
        for k in order {
            let ty = match seen.get(&k) {
                Some(s) if s.len() == 1 => s.iter().next().cloned().unwrap_or_default(),
                Some(s) if !s.is_empty() => "mixed".into(),
                _ => "S".into(),
            };
            shape.push((k, ty));
        }
        self.shapes
            .lock()
            .unwrap()
            .insert(table.to_string(), shape.clone());
        Ok(shape)
    }

    /// Scan pages until `want` items (or the end); `count_only` counts.
    async fn scan(
        &self,
        table: &str,
        filter: &Option<WhereClause>,
        want: usize,
        count_only: bool,
    ) -> DbResult<(Vec<Value>, usize)> {
        let shape = self.shape(table).await?;
        let terms = filter.as_ref().map(|f| f.terms.clone()).unwrap_or_default();
        let expr = filter_expr(&terms, &shape)?;
        let mut items = Vec::new();
        let mut count = 0usize;
        let mut start: Option<Value> = None;
        loop {
            let mut body = json!({"TableName": table});
            if count_only {
                body["Select"] = json!("COUNT");
            } else {
                body["Limit"] = json!((want - items.len()).clamp(1, 1000));
            }
            if let Some((e, n, v)) = &expr {
                body["FilterExpression"] = json!(e);
                body["ExpressionAttributeNames"] = n.clone();
                body["ExpressionAttributeValues"] = v.clone();
            }
            if let Some(s) = start.take() {
                body["ExclusiveStartKey"] = s;
            }
            let r = self.call("Scan", body).await?;
            count += r["Count"].as_u64().unwrap_or(0) as usize;
            items.extend(r["Items"].as_array().cloned().unwrap_or_default());
            match r.get("LastEvaluatedKey") {
                Some(k)
                    if !k.is_null()
                        && (count_only || items.len() < want)
                        && count < MAX_SCAN * 10 =>
                {
                    start = Some(k.clone())
                }
                _ => break,
            }
        }
        items.truncate(want);
        Ok((items, count))
    }

    /// A PartiQL statement's items (NextToken pages up to `limit`).
    async fn partiql(&self, sql: String, limit: usize) -> DbResult<Vec<Value>> {
        let mut items = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let mut body = json!({"Statement": sql});
            if let Some(t) = next.take() {
                body["NextToken"] = json!(t);
            }
            let r = self.call("ExecuteStatement", body).await?;
            items.extend(r["Items"].as_array().cloned().unwrap_or_default());
            match r["NextToken"].as_str() {
                Some(t) if items.len() < limit => next = Some(t.to_string()),
                _ => break,
            }
        }
        items.truncate(limit);
        Ok(items)
    }

    async fn grid_statement(&self, op: GridOp) -> DbResult<Value> {
        let table = match &op {
            GridOp::Insert { table, .. }
            | GridOp::Update { table, .. }
            | GridOp::Delete { table, .. } => table.clone(),
        };
        let shape = self.shape(&table).await?;
        let ty = |c: &str| {
            shape
                .iter()
                .find(|(n, _)| n == c)
                .map(|(_, t)| t.clone())
                .unwrap_or_else(|| "mixed".into())
        };
        let mut params = Vec::new();
        let mut pairs = |p: &[(String, Option<String>)], sep: &str| -> DbResult<String> {
            let mut out = Vec::new();
            for (c, v) in p {
                params.push(typed(v.as_deref(), &ty(c))?);
                out.push(format!("{} = ?", quote(c)));
            }
            Ok(out.join(sep))
        };
        let t = quote(&table);
        let sql = match &op {
            GridOp::Insert { values, .. } => {
                let mut fields = Vec::new();
                for (c, v) in values.iter().filter(|(_, v)| v.is_some()) {
                    params.push(typed(v.as_deref(), &ty(c))?);
                    fields.push(format!("'{}': ?", c.replace('\'', "''")));
                }
                if fields.is_empty() {
                    return Err("A new item needs its key attributes".into());
                }
                format!("INSERT INTO {t} VALUE {{{}}}", fields.join(", "))
            }
            GridOp::Update { set, key, .. } => {
                let s = pairs(set, ", ")?;
                format!("UPDATE {t} SET {s} WHERE {}", pairs(key, " AND ")?)
            }
            GridOp::Delete { key, .. } => format!("DELETE FROM {t} WHERE {}", pairs(key, " AND ")?),
        };
        Ok(json!({"Statement": sql, "Parameters": params}))
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

impl Driver for Dynamo {
    fn engine(&self) -> Engine {
        Engine::DynamoDb
    }
    fn default_schema(&self) -> Option<String> {
        Some(self.region.clone())
    }
    fn row_key(&self) -> bool {
        false
    }
    fn version(&self) -> Fut<String> {
        let r = self.region.clone();
        Box::pin(async move { Ok(format!("DynamoDB ({r})")) })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        self.schemas()
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let r = self.region.clone();
        Box::pin(async move { Ok(vec![r]) })
    }
    fn objects(&self, _schema: String) -> Fut<ObjectTree> {
        let this = self.clone();
        Box::pin(async move {
            db::run_logged(
                "ListTables".into(),
                crate::console::Source::Meta,
                async move {
                    let mut tables = Vec::new();
                    let mut start: Option<String> = None;
                    loop {
                        let mut body = json!({"Limit": 100});
                        if let Some(s) = start.take() {
                            body["ExclusiveStartTableName"] = json!(s);
                        }
                        let r = this.call("ListTables", body).await?;
                        tables.extend(r["TableNames"].as_array().into_iter().flatten().map(s));
                        match r["LastEvaluatedTableName"].as_str() {
                            Some(n) => start = Some(n.to_string()),
                            None => break,
                        }
                    }
                    Ok(ObjectTree {
                        tables,
                        ..Default::default()
                    })
                },
            )
            .await
        })
    }
    fn columns(&self, _schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let this = self.clone();
        Box::pin(async move {
            this.shapes.lock().unwrap().remove(&table);
            let label = format!("DescribeTable {table}");
            let (shape, t) = db::run_logged(label, crate::console::Source::Meta, async move {
                let t = this.describe(&table).await?;
                Ok((this.shape(&table).await?, t))
            })
            .await?;
            let key_kind = |n: &str| {
                t["KeySchema"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|k| k["AttributeName"] == n)
                    .map(|k| {
                        if k["KeyType"] == "HASH" {
                            "partition key"
                        } else {
                            "sort key"
                        }
                    })
            };
            Ok(shape
                .into_iter()
                .map(|(name, ty)| {
                    let kind = key_kind(&name);
                    GridColumnMeta {
                        is_pk: kind.is_some(),
                        nullable: kind.is_none(),
                        comment: kind.map(str::to_string),
                        pg_type: grid_type(&ty),
                        sql_type: ty,
                        default: None,
                        foreign_key: None,
                        enum_values: Vec::new(),
                        name,
                    }
                })
                .collect())
        })
    }
    fn count(&self, _schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let this = self.clone();
        Box::pin(async move {
            let label = format!("Scan {table} (COUNT)");
            db::run_logged(label, crate::console::Source::Data, async move {
                Ok(this.scan(&table, &filter, usize::MAX, true).await?.1 as i64)
            })
            .await
        })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move {
            let label = format!("Scan {}", req.table);
            db::run_logged(label, crate::console::Source::Data, async move {
                let shape = this.shape(&req.table).await?;
                let offset = req.offset.max(0) as usize;
                let want =
                    (offset + req.limit.max(0) as usize).min(MAX_SCAN.max(req.limit as usize));
                let (items, _) = this.scan(&req.table, &req.filter, want, false).await?;
                Ok(items
                    .iter()
                    .skip(offset)
                    .map(|item| {
                        Value::Object(
                            shape
                                .iter()
                                .map(|(k, _)| (k.clone(), to_json(&item[k.as_str()])))
                                .collect(),
                        )
                    })
                    .collect())
            })
            .await
        })
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move {
            let sql = super::trim_sql(&sql);
            let label = sql.clone();
            db::run_logged(label, crate::console::Source::Data, async move {
                let items = this.partiql(sql, limit.max(0) as usize).await?;
                // Attributes differ per item: the union, in first-seen order.
                let mut cols: Vec<String> = Vec::new();
                for i in &items {
                    for k in i.as_object().into_iter().flatten().map(|(k, _)| k) {
                        if !cols.contains(k) {
                            cols.push(k.clone());
                        }
                    }
                }
                Ok(items
                    .iter()
                    .map(|i| {
                        Value::Object(
                            cols.iter()
                                .map(|c| (c.clone(), to_json(&i[c.as_str()])))
                                .collect(),
                        )
                    })
                    .collect())
            })
            .await
        })
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let f = self.query_rows(sql, 50);
        Box::pin(async move {
            Ok(f.await?
                .first()
                .and_then(|r| r.as_object())
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default())
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let this = self.clone();
        Box::pin(async move {
            let mut n = 0;
            for st in db::split_statements(&sql) {
                let label = st.clone();
                let this = this.clone();
                db::run_logged(label, crate::console::Source::Data, async move {
                    this.partiql(st, usize::MAX).await
                })
                .await?;
                n += 1;
            }
            this.shapes.lock().unwrap().clear();
            Ok(n)
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let this = self.clone();
        Box::pin(async move {
            let started = std::time::Instant::now();
            let label = stmts
                .iter()
                .map(|s| s.sql.as_str())
                .collect::<Vec<_>>()
                .join(";\n");
            let r = async {
                let mut tx = Vec::new();
                for st in &stmts {
                    let op = super::parse_grid_stmt(st)
                        .ok_or_else(|| format!("DynamoDB can't run: {}", st.sql))?;
                    tx.push(this.grid_statement(op).await?);
                }
                // Transactions take up to 100 statements.
                for chunk in tx.chunks(100) {
                    this.call("ExecuteTransaction", json!({"TransactStatements": chunk}))
                        .await?;
                }
                Ok(stmts.len() as u64)
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
    fn script(&self, _kind: ObjKind, _schema: String, name: String, which: Script) -> Fut<String> {
        let this = self.clone();
        Box::pin(async move {
            let t = quote(&name);
            let shape = this.shape(&name).await.unwrap_or_default();
            let placeholder = |ty: &str| match ty {
                "N" => "0",
                "BOOL" => "false",
                "M" => "{}",
                "L" => "[]",
                _ => "''",
            };
            Ok(match which {
                Script::Select => format!("SELECT * FROM {t}"),
                Script::Insert => {
                    let fields: Vec<String> = shape
                        .iter()
                        .map(|(k, ty)| format!("'{}': {}", k.replace('\'', "''"), placeholder(ty)))
                        .collect();
                    format!("INSERT INTO {t} VALUE {{{}}}", fields.join(", "))
                }
                Script::Update => format!(
                    "UPDATE {t} SET \"attr\" = '' WHERE {}",
                    first_key_pred(&shape)
                ),
                Script::Delete => format!("DELETE FROM {t} WHERE {}", first_key_pred(&shape)),
                _ => serde_json::to_string_pretty(&this.describe(&name).await?).unwrap_or_default(),
            })
        })
    }
    fn indexes(&self, _schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let this = self.clone();
        Box::pin(async move {
            let t = this.describe(&table).await?;
            let keys = |k: &Value| {
                k.as_array()
                    .into_iter()
                    .flatten()
                    .map(|k| s(&k["AttributeName"]))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut out = vec![IndexDef {
                name: "PRIMARY".into(),
                algorithm: "hash".into(),
                unique: true,
                primary: true,
                columns: keys(&t["KeySchema"]),
                include: String::new(),
                condition: None,
                comment: None,
                constraint: None,
            }];
            for (field, kind) in [
                ("GlobalSecondaryIndexes", "global"),
                ("LocalSecondaryIndexes", "local"),
            ] {
                for ix in t[field].as_array().into_iter().flatten() {
                    out.push(IndexDef {
                        name: s(&ix["IndexName"]),
                        algorithm: kind.into(),
                        unique: false,
                        primary: false,
                        columns: keys(&ix["KeySchema"]),
                        include: ix["Projection"]["NonKeyAttributes"]
                            .as_array()
                            .map(|a| a.iter().map(s).collect::<Vec<_>>().join(", "))
                            .unwrap_or_default(),
                        condition: None,
                        comment: Some(s(&ix["Projection"]["ProjectionType"]))
                            .filter(|p| !p.is_empty()),
                        constraint: None,
                    });
                }
            }
            Ok(out)
        })
    }
}

/// `"pk" = ''` for the key attributes of `shape` (templates).
fn first_key_pred(shape: &[(String, String)]) -> String {
    shape
        .iter()
        .take(1)
        .map(|(k, ty)| format!("{} = {}", quote(k), if ty == "N" { "0" } else { "''" }))
        .collect::<Vec<_>>()
        .join(" AND ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::drivers::live;
    use crate::engine::Engine;

    /// The AWS SigV4 test-suite vector for a POST with a JSON body.
    #[test]
    fn sigv4_signs() {
        let creds = super::Creds {
            access_key: "AKIDEXAMPLE",
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            token: None,
            region: "us-east-1",
        };
        let a = super::sign(
            &creds,
            "dynamodb.us-east-1.amazonaws.com",
            "20150830T123600Z",
            "DynamoDB_20120810.ListTables",
            b"{}",
        );
        assert!(a.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/dynamodb/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-target, Signature="
        ));
        assert_eq!(a.rsplit('=').next().unwrap().len(), 64);
    }

    #[test]
    fn attribute_values() {
        assert_eq!(
            super::to_json(&json!({"M": {"a": {"N": "1.5"}, "b": {"SS": ["x"]}}})),
            json!({"a": 1.5, "b": ["x"]})
        );
        assert_eq!(super::typed(Some("42"), "N").unwrap(), json!({"N": "42"}));
        assert!(super::typed(Some("x"), "N").is_err());
        assert_eq!(
            super::typed(Some(r#"{"k":[1]}"#), "M").unwrap(),
            json!({"M": {"k": {"L": [{"N": "1"}]}}})
        );
    }

    #[test]
    fn live_dynamodb_local() {
        if !live::reachable(38000) {
            return;
        }
        let mut c = live::conn(Engine::DynamoDb, 0, "", "");
        c.options
            .insert("endpoint".into(), "http://127.0.0.1:38000".into());
        c.options.insert("region".into(), "us-east-1".into());
        c.options.insert("access_key".into(), "tusk".into());
        let rt = crate::db::runtime();
        let db = rt.block_on(super::connect(&c, "tusk".into())).unwrap();
        let d = db.driver();
        let dy = super::Dynamo {
            endpoint: "http://127.0.0.1:38000".into(),
            region: "us-east-1".into(),
            access_key: "tusk".into(),
            secret: "tusk".into(),
            token: None,
            shapes: Default::default(),
        };
        rt.block_on(dy.call("DeleteTable", json!({"TableName": "tusk_scratch_people"})))
            .ok();
        rt.block_on(dy.call(
            "CreateTable",
            json!({
                "TableName": "tusk_scratch_people",
                "AttributeDefinitions": [{"AttributeName": "team", "AttributeType": "S"}, {"AttributeName": "id", "AttributeType": "N"}],
                "KeySchema": [{"AttributeName": "team", "KeyType": "HASH"}, {"AttributeName": "id", "KeyType": "RANGE"}],
                "BillingMode": "PAY_PER_REQUEST"
            }),
        ))
        .unwrap();
        rt.block_on(d.exec(
            "INSERT INTO \"tusk_scratch_people\" VALUE {'team': 'a', 'id': 1, 'email': 'a@x', 'tags': ['x']};
             INSERT INTO \"tusk_scratch_people\" VALUE {'team': 'a', 'id': 2, 'email': 'b@x', 'age': 30};
             INSERT INTO \"tusk_scratch_people\" VALUE {'team': 'b', 'id': 1, 'email': 'c@y'}"
                .into(),
        ))
        .unwrap();
        assert!(
            rt.block_on(d.objects("us-east-1".into()))
                .unwrap()
                .tables
                .contains(&"tusk_scratch_people".to_string())
        );
        let cols = rt
            .block_on(d.columns("us-east-1".into(), "tusk_scratch_people".into()))
            .unwrap();
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["team", "id", "age", "email", "tags"]);
        assert!(cols[0].is_pk && cols[1].is_pk);
        assert_eq!(cols[2].pg_type, "numeric");
        let table = || "tusk_scratch_people".to_string();
        assert_eq!(
            rt.block_on(d.count("us-east-1".into(), table(), None))
                .unwrap(),
            3
        );
        let req = |offset, filter| crate::drivers::WindowReq {
            schema: "us-east-1".into(),
            table: table(),
            filter,
            order_by: None,
            with_key: false,
            limit: 2,
            offset,
        };
        let a = rt.block_on(d.window(req(0, None))).unwrap();
        let b = rt.block_on(d.window(req(2, None))).unwrap();
        assert_eq!((a.len(), b.len()), (2, 1));
        assert_eq!(
            a[0].as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            names
        );
        let term = |col: &str, op, v: &str| crate::db::FilterTerm {
            column: col.into(),
            op,
            value: v.into(),
        };
        let f = crate::db::WhereClause {
            terms: vec![
                term("email", crate::filter::FilterOp::HasPrefix, "b"),
                term("age", crate::filter::FilterOp::Ge, "30"),
            ],
            ..Default::default()
        };
        assert_eq!(
            rt.block_on(d.count("us-east-1".into(), table(), Some(f)))
                .unwrap(),
            1
        );
        let st = |sql: &str, p: &[Option<&str>]| crate::db::Stmt {
            sql: sql.into(),
            params: p.iter().map(|v| v.map(str::to_string)).collect(),
        };
        rt.block_on(d.batch(vec![
            st(r#"UPDATE "tusk_scratch_people" SET "age" = ?, "tags" = ? WHERE "team" = ? AND "id" = ?"#, &[Some("31"), Some(r#"["y"]"#), Some("a"), Some("2")]),
            st(r#"DELETE FROM "tusk_scratch_people" WHERE "team" = ? AND "id" = ?"#, &[Some("b"), Some("1")]),
            st(r#"INSERT INTO "tusk_scratch_people" ("team", "id", "email") VALUES (?, ?, ?)"#, &[Some("c"), Some("7"), Some("d@x")]),
        ]))
        .unwrap();
        let r = rt
            .block_on(d.query_rows(
                "SELECT * FROM \"tusk_scratch_people\" WHERE team = 'a' AND id = 2".into(),
                5,
            ))
            .unwrap();
        assert_eq!(
            (r[0]["age"].clone(), r[0]["tags"].clone()),
            (json!(31), json!(["y"]))
        );
        assert_eq!(
            rt.block_on(d.count("us-east-1".into(), table(), None))
                .unwrap(),
            3
        );
        let ix = rt.block_on(d.indexes("us-east-1".into(), table())).unwrap();
        assert_eq!(ix[0].columns, "team, id");
        // A PartiQL result edits in place.
        let q = r#"SELECT * FROM "tusk_scratch_people" WHERE team = 'a' AND id = 2"#;
        let rows = rt.block_on(d.query_rows(q.into(), 10)).unwrap();
        let cols = crate::sql::infer_columns(&rows);
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let src = rt
            .block_on(crate::db::result_edit_source(
                &db,
                q,
                "us-east-1",
                names.clone(),
            ))
            .unwrap()
            .unwrap();
        let ix = names.iter().position(|n| n == "email").unwrap();
        let mut g = crate::sql::QueryDelegate::empty();
        g.set_result(cols, crate::sql::rows_to_vec(rows), Some(src));
        g.edits.insert(
            0,
            std::collections::BTreeMap::from([(ix, Some("edited@x".to_string()))]),
        );
        rt.block_on(crate::db::execute_batch(&db, g.save_statements()))
            .unwrap();
        let r = rt.block_on(d.query_rows(q.into(), 10)).unwrap();
        assert_eq!(r[0]["email"], "edited@x");
        rt.block_on(dy.call("DeleteTable", json!({"TableName": "tusk_scratch_people"})))
            .unwrap();
    }
}
