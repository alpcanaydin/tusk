//! Apache Cassandra (and ScyllaDB) over CQL. Keyspaces are schemas; the
//! catalog is `system_schema`. CQL has no OFFSET: a window reads up to
//! `offset + limit` rows and skips. Edits are typed literals, since CQL
//! checks a bound value against the column type.

use std::sync::Arc;

use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::value::{CqlValue, Row};
use serde_json::{Value, json};

use super::{Db, Driver, Fut, GridOp, WindowReq};
use crate::db::{
    self, DbResult, FilterTerm, GridColumnMeta, IndexDef, ObjectTree, SavedConnection, Stmt,
    WhereClause,
};
use crate::engine::{Dialect, Engine};
use crate::filter::FilterOp;
use crate::objects::{ObjKind, Script};

const D: Dialect = Dialect::Cql;
/// Rows a window may read past (CQL pages from the start).
const MAX_SCAN: i64 = 100_000;

#[derive(Clone)]
pub struct Cassandra {
    session: Arc<Session>,
    keyspace: String,
    /// (name, cql type) per `ks.table`, for typed literals.
    types: super::Shapes,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Every node through the address that was dialled: peers report their
/// cluster-internal addresses, unreachable from behind NAT, a container or
/// an SSH tunnel. The contacted node coordinates all requests.
struct ViaContact(std::net::SocketAddr);

#[async_trait::async_trait]
impl scylla::policies::address_translator::AddressTranslator for ViaContact {
    async fn translate_address(
        &self,
        _peer: &scylla::policies::address_translator::UntranslatedPeer,
    ) -> Result<std::net::SocketAddr, scylla::errors::TranslationError> {
        Ok(self.0)
    }
}

pub async fn connect(
    conn: &SavedConnection,
    host: String,
    port: u16,
    password: String,
) -> DbResult<Db> {
    let user = conn.user.clone();
    let keyspace = conn.database.trim().to_string();
    let session = db::run_db(async move {
        let contact = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(err)?
            .next()
            .ok_or_else(|| format!("Can't resolve {host}"))?;
        let mut b = SessionBuilder::new()
            .known_node_addr(contact)
            .address_translator(Arc::new(ViaContact(contact)))
            .connection_timeout(std::time::Duration::from_secs(10));
        if !user.is_empty() {
            b = b.user(user, password);
        }
        let session = b.build().await.map_err(err)?;
        if !keyspace.is_empty() {
            // Unqualified names in the editor resolve here (it may not exist yet).
            let _ = session.use_keyspace(keyspace.clone(), true).await;
        }
        Ok(session)
    })
    .await?;
    Ok(Db::new(Cassandra {
        session: Arc::new(session),
        keyspace: conn.database.trim().to_string(),
        types: Default::default(),
    }))
}

/// A CQL value as the grid's JSON.
fn json_of(v: Option<CqlValue>) -> Value {
    let Some(v) = v else {
        return Value::Null;
    };
    match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => Value::String(s),
        CqlValue::Boolean(b) => Value::Bool(b),
        CqlValue::Blob(b) => super::hex(&b),
        CqlValue::Counter(c) => c.0.into(),
        CqlValue::Double(f) => Value::from(f),
        CqlValue::Float(f) => Value::from(f as f64),
        CqlValue::Int(n) => n.into(),
        CqlValue::BigInt(n) => n.into(),
        CqlValue::SmallInt(n) => n.into(),
        CqlValue::TinyInt(n) => n.into(),
        CqlValue::Timestamp(t) => chrono::DateTime::from_timestamp_millis(t.0)
            .map(|d| Value::String(d.format("%Y-%m-%d %H:%M:%S%.3f+00").to_string()))
            .unwrap_or(Value::Null),
        CqlValue::Date(d) => {
            // Days since 1970-01-01, offset by 2^31.
            let days = d.0 as i64 - (1i64 << 31);
            chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
                .and_then(|e| e.checked_add_signed(chrono::Duration::days(days)))
                .map(|d| Value::String(d.to_string()))
                .unwrap_or(Value::Null)
        }
        CqlValue::Uuid(u) => Value::String(u.to_string()),
        CqlValue::Timeuuid(u) => Value::String(u.to_string()),
        CqlValue::Inet(ip) => Value::String(ip.to_string()),
        CqlValue::List(a) | CqlValue::Set(a) | CqlValue::Vector(a) => {
            Value::Array(a.into_iter().map(|x| json_of(Some(x))).collect())
        }
        CqlValue::Map(m) => {
            let simple = m
                .iter()
                .all(|(k, _)| matches!(k, CqlValue::Text(_) | CqlValue::Ascii(_)));
            if simple {
                Value::Object(
                    m.into_iter()
                        .map(|(k, v)| {
                            (
                                json_of(Some(k)).as_str().unwrap_or_default().to_string(),
                                json_of(Some(v)),
                            )
                        })
                        .collect(),
                )
            } else {
                Value::Array(
                    m.into_iter()
                        .map(|(k, v)| json!([json_of(Some(k)), json_of(Some(v))]))
                        .collect(),
                )
            }
        }
        CqlValue::UserDefinedType { fields, .. } => {
            Value::Object(fields.into_iter().map(|(k, v)| (k, json_of(v))).collect())
        }
        CqlValue::Tuple(t) => Value::Array(t.into_iter().map(json_of).collect()),
        CqlValue::Empty => Value::Null,
        other => Value::String(other.to_string()),
    }
}

/// Grid text as a CQL literal of type `ty`.
pub fn literal(text: Option<&str>, ty: &str) -> DbResult<String> {
    let Some(t) = text else {
        return Ok("NULL".into());
    };
    let base = ty.split('<').next().unwrap_or("").trim().to_lowercase();
    let t = t.trim_end_matches('\n');
    let plain = |ok: &dyn Fn(char) -> bool| -> DbResult<String> {
        let v = t.trim();
        if !v.is_empty() && v.chars().all(ok) {
            Ok(v.to_string())
        } else {
            Err(format!("'{t}' isn't a valid {ty}"))
        }
    };
    let numeric = |c: char| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E');
    match base.as_str() {
        "int" | "bigint" | "smallint" | "tinyint" | "varint" | "counter" | "float" | "double"
        | "decimal" => match t.trim() {
            "NaN" | "Infinity" | "-Infinity" => Ok(t.trim().into()),
            _ => plain(&numeric),
        },
        "boolean" => match t.trim().to_lowercase().as_str() {
            "true" | "t" | "1" => Ok("true".into()),
            "false" | "f" | "0" => Ok("false".into()),
            _ => Err(format!("'{t}' isn't a boolean")),
        },
        "uuid" | "timeuuid" => plain(&|c: char| c.is_ascii_hexdigit() || c == '-'),
        "blob" => {
            let h = t.trim().trim_start_matches("\\x").trim_start_matches("0x");
            if h.chars().all(|c| c.is_ascii_hexdigit()) {
                Ok(format!("0x{h}"))
            } else {
                Err("A blob is hex: 0x…".into())
            }
        }
        // Collections / UDTs / tuples: JSON in the grid → CQL literal.
        "list" | "set" | "map" | "frozen" | "tuple" | "vector" => {
            let v: Value =
                serde_json::from_str(t).map_err(|_| format!("Enter the {ty} as JSON"))?;
            Ok(cql_of_json(&v, &base))
        }
        _ => Ok(D.literal(t)),
    }
}

fn cql_of_json(v: &Value, base: &str) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => D.literal(s),
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(|x| cql_of_json(x, "")).collect();
            match base {
                "set" => format!("{{{}}}", items.join(", ")),
                "tuple" => format!("({})", items.join(", ")),
                _ => format!("[{}]", items.join(", ")),
            }
        }
        Value::Object(m) => {
            let items: Vec<String> = m
                .iter()
                .map(|(k, x)| format!("{}: {}", D.literal(k), cql_of_json(x, "")))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

impl Cassandra {
    /// Rows of a statement as JSON objects (column order kept).
    async fn rows(&self, cql: String, limit: i64) -> DbResult<Vec<Value>> {
        let cql = super::trim_sql(&cql);
        let s = self.session.clone();
        let label = cql.clone();
        db::run_logged(label, crate::console::Source::Data, async move {
            let r = s.query_unpaged(cql, ()).await.map_err(err)?;
            let Ok(rows) = r.into_rows_result() else {
                return Ok(Vec::new());
            };
            let cols: Vec<String> = rows
                .column_specs()
                .iter()
                .map(|c| c.name().to_string())
                .collect();
            let mut out = Vec::new();
            for row in rows.rows::<Row>().map_err(err)? {
                if out.len() as i64 >= limit {
                    break;
                }
                let row = row.map_err(err)?;
                out.push(Value::Object(
                    cols.iter()
                        .cloned()
                        .zip(row.columns.into_iter().map(json_of))
                        .collect(),
                ));
            }
            Ok(out)
        })
        .await
    }

    fn fut(&self, cql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move { this.rows(cql, limit).await })
    }

    async fn types_of(&self, ks: &str, table: &str) -> DbResult<Vec<(String, String)>> {
        let key = format!("{ks}.{table}");
        if let Some(t) = self.types.lock().unwrap().get(&key) {
            return Ok(t.clone());
        }
        let cols = self.columns(ks.into(), table.into()).await?;
        let t: Vec<(String, String)> = cols.into_iter().map(|c| (c.name, c.sql_type)).collect();
        self.types.lock().unwrap().insert(key, t.clone());
        Ok(t)
    }

    /// Filter terms as a CQL WHERE (+ ALLOW FILTERING).
    async fn where_cql(
        &self,
        ks: &str,
        table: &str,
        filter: &Option<WhereClause>,
    ) -> DbResult<String> {
        let Some(w) = filter else {
            return Ok(String::new());
        };
        if w.terms.is_empty() {
            return Ok(if w.sql.trim().is_empty() {
                String::new()
            } else {
                format!("WHERE {} ALLOW FILTERING", w.sql)
            });
        }
        let types = self.types_of(ks, table).await?;
        let ty = |c: &str| {
            types
                .iter()
                .find(|(n, _)| n == c)
                .map(|(_, t)| t.clone())
                .unwrap_or_else(|| "text".into())
        };
        let mut parts = Vec::new();
        for FilterTerm { column, op, value } in &w.terms {
            let t = ty(column);
            let id = D.quote(column);
            let lit = |v: &str| literal(Some(v), &t);
            parts.push(match op {
                FilterOp::Eq => format!("{id} = {}", lit(value)?),
                FilterOp::Ne => format!("{id} != {}", lit(value)?),
                FilterOp::Lt => format!("{id} < {}", lit(value)?),
                FilterOp::Gt => format!("{id} > {}", lit(value)?),
                FilterOp::Le => format!("{id} <= {}", lit(value)?),
                FilterOp::Ge => format!("{id} >= {}", lit(value)?),
                FilterOp::In => {
                    let items: DbResult<Vec<String>> = crate::filter::split_list(value)
                        .iter()
                        .map(|v| lit(v))
                        .collect();
                    format!("{id} IN ({})", items?.join(", "))
                }
                FilterOp::Between => {
                    let items = crate::filter::split_list(value);
                    let [lo, hi] = items.as_slice() else { continue };
                    format!("{id} >= {} AND {id} <= {}", lit(lo)?, lit(hi)?)
                }
                FilterOp::Like | FilterOp::ILike => format!("{id} LIKE {}", D.literal(value)),
                FilterOp::HasPrefix => format!("{id} LIKE {}", D.literal(&format!("{value}%"))),
                FilterOp::HasSuffix => format!("{id} LIKE {}", D.literal(&format!("%{value}"))),
                FilterOp::Contains => format!("{id} LIKE {}", D.literal(&format!("%{value}%"))),
                FilterOp::IsNull
                | FilterOp::IsNotNull
                | FilterOp::NotIn
                | FilterOp::NotContains => {
                    return Err(format!("CQL can't filter with {}", op.label()));
                }
            });
        }
        Ok(format!("WHERE {} ALLOW FILTERING", parts.join(" AND ")))
    }

    async fn grid_cql(&self, op: GridOp) -> DbResult<String> {
        let (schema, table) = match &op {
            GridOp::Insert { schema, table, .. }
            | GridOp::Update { schema, table, .. }
            | GridOp::Delete { schema, table, .. } => (
                schema.clone().unwrap_or_else(|| self.keyspace.clone()),
                table.clone(),
            ),
        };
        let types = self.types_of(&schema, &table).await?;
        let ty = |c: &str| {
            types
                .iter()
                .find(|(n, _)| n == c)
                .map(|(_, t)| t.clone())
                .unwrap_or_else(|| "text".into())
        };
        let target = D.qualified(&schema, &table, true);
        let pairs = |p: &[(String, Option<String>)], sep: &str| -> DbResult<String> {
            let parts: DbResult<Vec<String>> = p
                .iter()
                .map(|(c, v)| {
                    Ok(format!(
                        "{} = {}",
                        D.quote(c),
                        literal(v.as_deref(), &ty(c))?
                    ))
                })
                .collect();
            Ok(parts?.join(sep))
        };
        Ok(match op {
            GridOp::Insert { values, .. } => {
                let values: Vec<_> = values.into_iter().filter(|(_, v)| v.is_some()).collect();
                if values.is_empty() {
                    return Err("A new row needs its primary key columns".into());
                }
                let cols: Vec<String> = values.iter().map(|(c, _)| D.quote(c)).collect();
                let vals: DbResult<Vec<String>> = values
                    .iter()
                    .map(|(c, v)| literal(v.as_deref(), &ty(c)))
                    .collect();
                format!(
                    "INSERT INTO {target} ({}) VALUES ({})",
                    cols.join(", "),
                    vals?.join(", ")
                )
            }
            GridOp::Update { set, key, .. } => {
                format!(
                    "UPDATE {target} SET {} WHERE {}",
                    pairs(&set, ", ")?,
                    pairs(&key, " AND ")?
                )
            }
            GridOp::Delete { key, .. } => {
                format!("DELETE FROM {target} WHERE {}", pairs(&key, " AND ")?)
            }
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

const SYSTEM: &[&str] = &[
    "system",
    "system_auth",
    "system_distributed",
    "system_schema",
    "system_traces",
    "system_views",
    "system_virtual_schema",
];

impl Driver for Cassandra {
    fn engine(&self) -> Engine {
        Engine::Cassandra
    }
    fn default_schema(&self) -> Option<String> {
        Some(self.keyspace.clone()).filter(|k| !k.is_empty())
    }
    fn row_key(&self) -> bool {
        false
    }
    fn version(&self) -> Fut<String> {
        let f = self.fut("SELECT release_version FROM system.local".into(), 1);
        Box::pin(async move {
            Ok(format!(
                "Cassandra {}",
                f.await?
                    .first()
                    .map(|r| s(&r["release_version"]))
                    .unwrap_or_default()
            ))
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        self.schemas()
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let f = self.fut(
            "SELECT keyspace_name FROM system_schema.keyspaces".into(),
            10_000,
        );
        Box::pin(async move {
            let mut out: Vec<String> = f
                .await?
                .iter()
                .map(|r| s(&r["keyspace_name"]))
                .filter(|k| !SYSTEM.contains(&k.as_str()))
                .collect();
            out.sort();
            Ok(out)
        })
    }
    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        // The sidebar's keyspace is where the editor's unqualified names go.
        let session = self.session.clone();
        let ks = schema.clone();
        let use_ks = db::run_db(async move {
            let _ = session.use_keyspace(ks, true).await;
            Ok(())
        });
        let lit = D.literal(&schema);
        let tables = self.fut(
            format!("SELECT table_name FROM system_schema.tables WHERE keyspace_name = {lit}"),
            100_000,
        );
        let views = self.fut(
            format!("SELECT view_name FROM system_schema.views WHERE keyspace_name = {lit}"),
            100_000,
        );
        let funcs = self.fut(
            format!(
                "SELECT function_name FROM system_schema.functions WHERE keyspace_name = {lit}"
            ),
            100_000,
        );
        Box::pin(async move {
            use_ks.await?;
            let mut tree = ObjectTree {
                tables: tables.await?.iter().map(|r| s(&r["table_name"])).collect(),
                matviews: views
                    .await
                    .unwrap_or_default()
                    .iter()
                    .map(|r| s(&r["view_name"]))
                    .collect(),
                functions: funcs
                    .await
                    .unwrap_or_default()
                    .iter()
                    .map(|r| s(&r["function_name"]))
                    .collect(),
                ..Default::default()
            };
            tree.tables.sort();
            tree.matviews.sort();
            Ok(tree)
        })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let f = self.fut(
            format!(
                "SELECT column_name, type, kind, position, clustering_order FROM system_schema.columns
                  WHERE keyspace_name = {} AND table_name = {}",
                D.literal(&schema),
                D.literal(&table)
            ),
            10_000,
        );
        Box::pin(async move {
            let mut rows = f.await?;
            // Partition key, clustering columns (by position), then the rest by name.
            let rank = |r: &Value| match s(&r["kind"]).as_str() {
                "partition_key" => 0,
                "clustering" => 1,
                "static" => 2,
                _ => 3,
            };
            rows.sort_by(|a, b| {
                (
                    rank(a),
                    a["position"].as_i64().unwrap_or(0),
                    s(&a["column_name"]),
                )
                    .cmp(&(
                        rank(b),
                        b["position"].as_i64().unwrap_or(0),
                        s(&b["column_name"]),
                    ))
            });
            Ok(rows
                .iter()
                .map(|r| {
                    let ty = s(&r["type"]);
                    let pk = rank(r) < 2;
                    GridColumnMeta {
                        name: s(&r["column_name"]),
                        pg_type: cql_grid_type(&ty),
                        nullable: !pk,
                        default: None,
                        comment: match s(&r["kind"]).as_str() {
                            "partition_key" => Some("partition key".into()),
                            "clustering" => {
                                Some(format!("clustering ({})", s(&r["clustering_order"])))
                            }
                            _ => None,
                        },
                        is_pk: pk,
                        foreign_key: None,
                        enum_values: Vec::new(),
                        sql_type: ty,
                    }
                })
                .collect())
        })
    }
    fn count(&self, schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let this = self.clone();
        Box::pin(async move {
            let w = this.where_cql(&schema, &table, &filter).await?;
            let r = this
                .rows(
                    format!(
                        "SELECT COUNT(*) AS n FROM {} {w}",
                        D.qualified(&schema, &table, true)
                    ),
                    1,
                )
                .await?;
            Ok(r.first().and_then(|r| r["n"].as_i64()).unwrap_or(0))
        })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move {
            let w = this.where_cql(&req.schema, &req.table, &req.filter).await?;
            let end = (req.offset + req.limit).min(MAX_SCAN.max(req.limit));
            let lim = match w.strip_suffix(" ALLOW FILTERING") {
                Some(pred) => format!("{pred} LIMIT {end} ALLOW FILTERING"),
                None => format!("{w} LIMIT {end}"),
            };
            let rows = this
                .rows(
                    format!(
                        "SELECT * FROM {} {lim}",
                        D.qualified(&req.schema, &req.table, true)
                    ),
                    end,
                )
                .await?;
            // SELECT * returns the key columns first, the rest by name: the
            // same order `columns` reports.
            Ok(rows.into_iter().skip(req.offset.max(0) as usize).collect())
        })
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        self.fut(sql, limit)
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let f = self.fut(sql, 1);
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
                this.rows(st, 0).await?;
                n += 1;
            }
            this.types.lock().unwrap().clear();
            Ok(n)
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let this = self.clone();
        Box::pin(async move {
            // CQL batches are atomic but can't mix counter / non-counter
            // tables; statements run one after another.
            let mut n = 0;
            for st in stmts {
                let cql = match super::parse_grid_stmt(&st) {
                    Some(op) => this.grid_cql(op).await?,
                    None if st.params.is_empty() => st.sql.clone(),
                    None => super::clickhouse::inline(&st.sql, &st.params, D),
                };
                this.rows(cql, 0).await?;
                n += 1;
            }
            // Structure edits change the column types literals follow.
            this.types.lock().unwrap().clear();
            Ok(n)
        })
    }
    fn script(&self, kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String> {
        let target = D.qualified(&schema, &name, true);
        let this = self.clone();
        Box::pin(async move {
            let cols = this.columns(schema.clone(), name.clone()).await?;
            Ok(match which {
                Script::Create => {
                    let mut lines: Vec<String> = cols
                        .iter()
                        .map(|c| format!("    {} {}", D.quote(&c.name), c.sql_type))
                        .collect();
                    let part: Vec<String> = cols
                        .iter()
                        .filter(|c| c.comment.as_deref() == Some("partition key"))
                        .map(|c| D.quote(&c.name))
                        .collect();
                    let clus: Vec<String> = cols
                        .iter()
                        .filter(|c| {
                            c.comment
                                .as_deref()
                                .is_some_and(|k| k.starts_with("clustering"))
                        })
                        .map(|c| D.quote(&c.name))
                        .collect();
                    let pk = if clus.is_empty() {
                        format!("({})", part.join(", "))
                    } else {
                        format!("(({}), {})", part.join(", "), clus.join(", "))
                    };
                    lines.push(format!("    PRIMARY KEY {pk}"));
                    format!("CREATE TABLE {target} (\n{}\n);", lines.join(",\n"))
                }
                Script::Drop if kind == ObjKind::MatView => {
                    format!("DROP MATERIALIZED VIEW {target};")
                }
                Script::Select => format!("SELECT * FROM {target} LIMIT 100;"),
                w => super::dml_script(D, w, &target, &cols),
            })
        })
    }
    fn indexes(&self, schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let f = self.fut(
            format!(
                "SELECT index_name, kind, options FROM system_schema.indexes WHERE keyspace_name = {} AND table_name = {}",
                D.literal(&schema),
                D.literal(&table)
            ),
            10_000,
        );
        Box::pin(async move {
            Ok(f.await?
                .iter()
                .map(|r| IndexDef {
                    name: s(&r["index_name"]),
                    algorithm: s(&r["kind"]).to_lowercase(),
                    unique: false,
                    primary: false,
                    columns: s(&r["options"]["target"]),
                    include: String::new(),
                    condition: None,
                    comment: None,
                    constraint: None,
                })
                .collect())
        })
    }
}

fn cql_grid_type(ty: &str) -> String {
    let base = ty.split('<').next().unwrap_or("").trim();
    match base {
        "list" | "set" | "map" | "frozen" | "tuple" | "vector" => "json".into(),
        "text" | "ascii" => "text".into(),
        "timestamp" => "timestamptz".into(),
        "counter" | "varint" => "int8".into(),
        other => super::short_type(other),
    }
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn literals() {
        assert_eq!(super::literal(Some("it's"), "text").unwrap(), "'it''s'");
        assert_eq!(super::literal(Some("42"), "int").unwrap(), "42");
        assert!(super::literal(Some("1; DROP"), "int").is_err());
        assert_eq!(
            super::literal(Some(r#"["a","b"]"#), "set<text>").unwrap(),
            "{'a', 'b'}"
        );
        assert_eq!(
            super::literal(Some(r#"{"k":1}"#), "map<text, int>").unwrap(),
            "{'k': 1}"
        );
        assert_eq!(super::literal(None, "int").unwrap(), "NULL");
    }

    #[test]
    fn live_cassandra() {
        if !live::reachable(39042) {
            return;
        }
        let c = live::conn(Engine::Cassandra, 39042, "", "tusk_scratch");
        let rt = crate::db::runtime();
        let db = rt
            .block_on(crate::drivers::connect(
                &c,
                c.host.clone(),
                c.port,
                String::new(),
            ))
            .unwrap();
        let d = db.driver();
        rt.block_on(d.exec(
            "CREATE KEYSPACE IF NOT EXISTS tusk_scratch WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1};
             DROP TABLE IF EXISTS tusk_scratch.people;
             CREATE TABLE tusk_scratch.people (team text, id int, email text, tags set<text>, seen timestamp, PRIMARY KEY (team, id));
             INSERT INTO tusk_scratch.people (team, id, email, tags) VALUES ('a', 1, 'a@x', {'x'});
             INSERT INTO tusk_scratch.people (team, id, email) VALUES ('a', 2, 'b@x');
             INSERT INTO tusk_scratch.people (team, id, email, seen) VALUES ('b', 1, 'c@y', '2024-01-02 03:04:05+0000');"
                .into(),
        ))
        .unwrap();
        assert!(rt.block_on(d.version()).unwrap().starts_with("Cassandra "));
        assert!(
            rt.block_on(d.schemas())
                .unwrap()
                .contains(&"tusk_scratch".to_string())
        );
        assert_eq!(
            rt.block_on(d.objects("tusk_scratch".into()))
                .unwrap()
                .tables,
            ["people"]
        );
        let cols = rt
            .block_on(d.columns("tusk_scratch".into(), "people".into()))
            .unwrap();
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["team", "id", "email", "seen", "tags"]);
        assert!(cols[0].is_pk && cols[1].is_pk && !cols[2].is_pk);
        assert_eq!(
            rt.block_on(d.count("tusk_scratch".into(), "people".into(), None))
                .unwrap(),
            3
        );
        let req = |offset, filter| crate::drivers::WindowReq {
            schema: "tusk_scratch".into(),
            table: "people".into(),
            filter,
            order_by: None,
            with_key: false,
            limit: 2,
            offset,
        };
        let first = rt.block_on(d.window(req(0, None))).unwrap();
        let rest = rt.block_on(d.window(req(2, None))).unwrap();
        assert_eq!((first.len(), rest.len()), (2, 1));
        let keys: Vec<&str> = first[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, names);
        let all: Vec<serde_json::Value> = first.into_iter().chain(rest).collect();
        let a1 = all
            .iter()
            .find(|r| r["team"] == "a" && r["id"] == 1)
            .unwrap();
        assert_eq!(a1["tags"], serde_json::json!(["x"]));
        let b1 = all.iter().find(|r| r["team"] == "b").unwrap();
        assert_eq!(b1["seen"], "2024-01-02 03:04:05.000+00");
        let f = crate::db::WhereClause {
            terms: vec![crate::db::FilterTerm {
                column: "email".into(),
                op: crate::filter::FilterOp::Eq,
                value: "b@x".into(),
            }],
            ..Default::default()
        };
        assert_eq!(
            rt.block_on(d.count("tusk_scratch".into(), "people".into(), Some(f)))
                .unwrap(),
            1
        );
        let st = |sql: &str, p: &[Option<&str>]| crate::db::Stmt {
            sql: sql.into(),
            params: p.iter().map(|v| v.map(str::to_string)).collect(),
        };
        rt.block_on(d.batch(vec![
            st(
                r#"UPDATE "tusk_scratch"."people" SET "email" = ?, "tags" = ? WHERE "team" = ? AND "id" = ?"#,
                &[Some("it's@x"), Some(r#"["y","z"]"#), Some("a"), Some("2")],
            ),
            st(r#"DELETE FROM "tusk_scratch"."people" WHERE "team" = ? AND "id" = ?"#, &[Some("b"), Some("1")]),
            st(r#"INSERT INTO "tusk_scratch"."people" ("team", "id", "email") VALUES (?, ?, ?)"#, &[Some("c"), Some("9"), Some("d@x")]),
        ]))
        .unwrap();
        let r = rt
            .block_on(d.query_rows(
                "SELECT email, tags FROM tusk_scratch.people WHERE team = 'a' AND id = 2".into(),
                5,
            ))
            .unwrap();
        assert_eq!(r[0]["email"], "it's@x");
        assert_eq!(r[0]["tags"], serde_json::json!(["y", "z"]));
        assert_eq!(
            rt.block_on(d.count("tusk_scratch".into(), "people".into(), None))
                .unwrap(),
            3
        );
        assert!(
            rt.block_on(d.script(
                crate::objects::ObjKind::Table,
                "tusk_scratch".into(),
                "people".into(),
                crate::objects::Script::Create
            ))
            .unwrap()
            .contains(r#"PRIMARY KEY (("team"), "id")"#)
        );
        rt.block_on(d.exec("DROP KEYSPACE tusk_scratch".into()))
            .unwrap();
    }
}
