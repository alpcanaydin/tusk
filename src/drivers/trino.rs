//! Trino coordinator HTTP protocol. Grid rows are intentionally read-only.
use super::{Db, Driver, Fut, WindowReq, http};
use crate::{
    db::{DbResult, GridColumnMeta, ObjectTree, SavedConnection, SslMode, Stmt},
    engine::{Dialect, Engine},
    objects::{ObjKind, Script},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct Trino {
    url: String,
    user: String,
    password: String,
    catalog: String,
    schema: String,
    session: Arc<Mutex<BTreeMap<String, String>>>,
}
struct Pending {
    url: Option<String>,
    driver: Trino,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(url) = self.url.take() {
            let driver = self.driver.clone();
            crate::db::runtime().spawn(async move {
                let _ = driver.request(reqwest::Method::DELETE, &url).send().await;
            });
        }
    }
}
// Abandoning a UI task must cancel the coordinator statement too.
async fn dispatch<T: Send + 'static>(
    future: impl std::future::Future<Output = DbResult<T>> + Send + 'static,
) -> DbResult<T> {
    struct Abort(tokio::task::AbortHandle);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let task = crate::db::runtime().spawn(future);
    let _guard = Abort(task.abort_handle());
    task.await
        .map_err(|error| format!("Trino task failed: {error}"))?
}
fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
pub async fn connect(
    c: &SavedConnection,
    host: String,
    port: u16,
    password: String,
) -> DbResult<Db> {
    let scheme = if c.ssl == SslMode::Require {
        "https"
    } else {
        "http"
    };
    let t = Trino {
        url: format!("{scheme}://{host}:{port}"),
        user: c.user.clone(),
        password,
        catalog: c.database.clone(),
        schema: c.opt("schema").to_string(),
        session: Default::default(),
    };
    if !t.password.is_empty() && scheme != "https" {
        return Err("Trino password authentication requires HTTPS. Select Require SSL.".into());
    }
    t.run("SELECT 1".into(), 1).await?;
    Ok(Db::new(t))
}
impl Trino {
    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut r = http::client()
            .request(method, url)
            .header("X-Trino-User", &self.user)
            .header("X-Trino-Source", "Tusk");
        if !self.password.is_empty() {
            r = r.basic_auth(&self.user, Some(&self.password));
        }
        if !self.catalog.is_empty() {
            r = r.header("X-Trino-Catalog", &self.catalog);
        }
        if !self.schema.is_empty() {
            r = r.header("X-Trino-Schema", &self.schema);
        }
        for (k, v) in self.session.lock().unwrap().iter() {
            r = r.header(k, v);
        }
        r
    }
    async fn run(&self, sql: String, limit: usize) -> DbResult<(Vec<String>, Vec<Value>, u64)> {
        let mut pending = Pending {
            url: None,
            driver: self.clone(),
        };
        let mut request = self
            .request(reqwest::Method::POST, &format!("{}/v1/statement", self.url))
            .body(sql);
        let mut cols = Vec::new();
        let mut rows = Vec::new();
        let mut affected = 0;
        loop {
            let response = request
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?;
            {
                let mut session = self.session.lock().unwrap();
                for (response_key, request_key) in [
                    ("x-trino-set-catalog", "X-Trino-Catalog"),
                    ("x-trino-set-schema", "X-Trino-Schema"),
                    ("x-trino-started-transaction-id", "X-Trino-Transaction-Id"),
                ] {
                    if let Some(v) = response
                        .headers()
                        .get(response_key)
                        .and_then(|v| v.to_str().ok())
                    {
                        session.insert(request_key.into(), v.into());
                    }
                }
                if response
                    .headers()
                    .contains_key("x-trino-clear-transaction-id")
                {
                    session.remove("X-Trino-Transaction-Id");
                }
                // Session properties must be replayed on subsequent requests.
                let mut properties: BTreeMap<String, String> = session
                    .get("X-Trino-Session")
                    .into_iter()
                    .flat_map(|s| s.split(','))
                    .filter_map(|s| s.split_once('='))
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect();
                for v in response.headers().get_all("x-trino-set-session") {
                    if let Ok(v) = v.to_str() {
                        for s in v.split(',') {
                            if let Some((k, v)) = s.trim().split_once('=') {
                                properties.insert(k.into(), v.into());
                            }
                        }
                    }
                }
                for v in response.headers().get_all("x-trino-clear-session") {
                    if let Ok(v) = v.to_str() {
                        for k in v.split(',') {
                            properties.remove(k.trim());
                        }
                    }
                }
                if !properties.is_empty() {
                    session.insert(
                        "X-Trino-Session".into(),
                        properties
                            .into_iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                } else {
                    session.remove("X-Trino-Session");
                }
            }
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            if let Some(error) = body.get("error") {
                return Err(format!(
                    "Trino: {}",
                    error["message"].as_str().unwrap_or("Query failed")
                ));
            }
            if let Some(c) = body["columns"].as_array() {
                cols = c
                    .iter()
                    .map(|v| v["name"].as_str().unwrap_or("").into())
                    .collect();
            }
            if let Some(data) = body["data"].as_array() {
                for row in data {
                    if rows.len() < limit {
                        rows.push(Value::Object(
                            cols.iter()
                                .cloned()
                                .zip(row.as_array().cloned().unwrap_or_default())
                                .collect(),
                        ));
                    }
                }
            }
            if let Some(n) = body["updateCount"].as_u64() {
                affected = n;
            }
            pending.url = body["nextUri"].as_str().map(str::to_string);
            let Some(next) = pending.url.as_ref() else {
                break;
            };
            let base = reqwest::Url::parse(&self.url).map_err(|e| e.to_string())?;
            let target = reqwest::Url::parse(next).map_err(|e| e.to_string())?;
            if base.origin() != target.origin() {
                pending.url = None;
                return Err("Trino returned a result URL outside its coordinator.".into());
            }
            if !cols.is_empty() && rows.len() >= limit {
                break;
            }
            request = self.request(reqwest::Method::GET, next);
        }
        Ok((cols, rows, affected))
    }
    fn rows(&self, sql: String, limit: usize) -> Fut<Vec<Value>> {
        let t = self.clone();
        Box::pin(async move {
            dispatch(async move {
                let label = sql.clone();
                crate::console::logged(&label, crate::console::Source::Data, async move {
                    Ok(t.run(sql, limit).await?.1)
                })
                .await
            })
            .await
        })
    }
    fn names(&self, sql: String) -> Fut<Vec<String>> {
        let t = self.clone();
        Box::pin(async move {
            dispatch(async move {
                Ok(t.run(sql, 100_000)
                    .await?
                    .1
                    .into_iter()
                    .filter_map(|v| v.as_object()?.values().next()?.as_str().map(str::to_string))
                    .collect())
            })
            .await
        })
    }
    fn table(&self, schema: &str, table: &str) -> String {
        [self.catalog.as_str(), schema, table]
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(q)
            .collect::<Vec<_>>()
            .join(".")
    }
}
impl Driver for Trino {
    fn engine(&self) -> Engine {
        Engine::Trino
    }
    fn version(&self) -> Fut<String> {
        let t = self.clone();
        Box::pin(async move {
            Ok(t.names("SELECT version()".into())
                .await?
                .first()
                .cloned()
                .unwrap_or_default())
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        self.names("SHOW CATALOGS".into())
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        self.names(if self.catalog.is_empty() {
            "SHOW SCHEMAS".into()
        } else {
            format!("SHOW SCHEMAS FROM {}", q(&self.catalog))
        })
    }
    fn default_schema(&self) -> Option<String> {
        (!self.schema.is_empty()).then(|| self.schema.clone())
    }
    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let t = self.clone();
        Box::pin(async move {
            let prefix = if t.catalog.is_empty() {
                "information_schema".into()
            } else {
                format!("{}.information_schema", q(&t.catalog))
            };
            let rows=t.rows(format!("SELECT table_name, table_type FROM {prefix}.tables WHERE table_schema = {} ORDER BY table_name",literal(&schema)),100_000).await?;
            let mut tree = ObjectTree::default();
            for r in rows {
                if let Some(n) = r["table_name"].as_str() {
                    if r["table_type"] == "VIEW" {
                        tree.views.push(n.into());
                    } else {
                        tree.tables.push(n.into());
                    }
                }
            }
            Ok(tree)
        })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let t = self.clone();
        Box::pin(async move {
            let prefix = if t.catalog.is_empty() {
                "information_schema".into()
            } else {
                format!("{}.information_schema", q(&t.catalog))
            };
            let rows=t.rows(format!("SELECT column_name, data_type, is_nullable FROM {prefix}.columns WHERE table_schema={} AND table_name={} ORDER BY ordinal_position",literal(&schema),literal(&table)),100_000).await?;
            Ok(rows
                .into_iter()
                .map(|r| GridColumnMeta {
                    name: r["column_name"].as_str().unwrap_or("").into(),
                    pg_type: r["data_type"].as_str().unwrap_or("varchar").into(),
                    sql_type: r["data_type"].as_str().unwrap_or("varchar").into(),
                    nullable: r["is_nullable"] == "YES",
                    default: None,
                    comment: None,
                    is_pk: false,
                    foreign_key: None,
                    enum_values: Vec::new(),
                })
                .collect())
        })
    }
    fn count(
        &self,
        schema: String,
        table: String,
        filter: Option<crate::db::WhereClause>,
    ) -> Fut<i64> {
        let t = self.clone();
        Box::pin(async move {
            if filter.is_some() {
                return Err("Trino filtered grid counts are not available.".into());
            }
            Ok(t.rows(
                format!("SELECT count(*) AS n FROM {}", t.table(&schema, &table)),
                1,
            )
            .await?
            .first()
            .and_then(|r| r["n"].as_i64())
            .unwrap_or(0))
        })
    }
    fn window(&self, r: WindowReq) -> Fut<Vec<Value>> {
        if r.filter.is_some() {
            return Box::pin(async {
                Err("Use the SQL editor for filtered Trino queries.".into())
            });
        }
        self.rows(
            format!(
                "SELECT * FROM {}{} OFFSET {} LIMIT {}",
                self.table(&r.schema, &r.table),
                r.order_by
                    .map(|o| format!(" ORDER BY {o}"))
                    .unwrap_or_default(),
                r.offset.max(0),
                r.limit.max(0)
            ),
            r.limit.max(0) as usize,
        )
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        self.rows(sql, limit.max(0) as usize)
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let t = self.clone();
        Box::pin(async move {
            dispatch(async move {
                Ok(t.run(
                    format!(
                        "SELECT * FROM ({}) AS tusk_columns LIMIT 0",
                        super::trim_sql(&sql)
                    ),
                    0,
                )
                .await?
                .0)
            })
            .await
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let t = self.clone();
        Box::pin(async move {
            dispatch(async move {
                let label = sql.clone();
                crate::console::logged(&label, crate::console::Source::Data, async move {
                    Ok(t.run(sql, usize::MAX).await?.2)
                })
                .await
            })
            .await
        })
    }
    fn batch(&self, _: Vec<Stmt>) -> Fut<u64> {
        Box::pin(async {
            Err(
                "Trino grid saves are unavailable; use SQL writes supported by your catalog."
                    .into(),
            )
        })
    }
    fn script(&self, _: ObjKind, _: String, _: String, _: Script) -> Fut<String> {
        Box::pin(async { Err("Trino schema editing is unavailable.".into()) })
    }
    fn dialect(&self) -> Dialect {
        Dialect::Vertica
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abandoning_statement_sends_cancel() {
        use std::io::{Read as _, Write as _};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/statement/cancel",
            server.local_addr().unwrap()
        );
        let thread = std::thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0; 4096];
            let n = socket.read(&mut buffer).unwrap();
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            String::from_utf8_lossy(&buffer[..n]).to_string()
        });
        let driver = Trino {
            url: url.clone(),
            user: "fixture".into(),
            password: String::new(),
            catalog: String::new(),
            schema: String::new(),
            session: Default::default(),
        };
        drop(Pending {
            url: Some(url),
            driver,
        });
        assert!(
            thread
                .join()
                .unwrap()
                .starts_with("DELETE /v1/statement/cancel ")
        );
    }
    #[test]
    fn identifiers_and_literals_are_escaped() {
        assert_eq!(q("a\"b"), "\"a\"\"b\"");
        assert_eq!(literal("a'b"), "'a''b'");
    }
}
