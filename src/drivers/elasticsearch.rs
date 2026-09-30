//! Elasticsearch Query DSL and optimistic single-document mutations.
use super::{Db, Driver, Fut, WindowReq, http};
use crate::{
    db::{DbResult, GridColumnMeta, ObjectTree, SavedConnection, SslMode, Stmt, WhereClause},
    engine::Engine,
    objects::{ObjKind, Script},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Elasticsearch {
    endpoint: reqwest::Url,
    user: String,
    secret: String,
    auth: String,
    index: String,
    browse: Arc<Mutex<Option<Browse>>>,
}
type BrowsePage = (i64, usize, Option<Vec<Value>>, Option<Value>);
struct Browse {
    index: String,
    pit: String,
    // Keep cursor tokens for backwards navigation, but only eight result pages.
    pages: Vec<BrowsePage>,
}
impl Drop for Elasticsearch {
    fn drop(&mut self) {
        if Arc::strong_count(&self.browse) == 1
            && let Ok(mut state) = self.browse.try_lock()
            && let Some(b) = state.take()
        {
            let request = self
                .request(reqwest::Method::DELETE, &["_pit"])
                .map(|r| r.json(&json!({"id":b.pit})));
            crate::db::runtime().spawn(async move {
                if let Ok(r) = request {
                    let _ = r.send().await;
                }
            });
        }
    }
}
pub async fn connect(c: &SavedConnection, host: String, port: u16, secret: String) -> DbResult<Db> {
    let scheme = if c.ssl == SslMode::Require {
        "https"
    } else {
        "http"
    };
    let auth = if c.opt("auth_mode").is_empty() {
        if c.user.is_empty() { "none" } else { "basic" }
    } else {
        c.opt("auth_mode")
    }
    .to_string();
    if !matches!(auth.as_str(), "none" | "basic" | "api_key") {
        return Err("Authentication must be none, basic, or api_key.".into());
    }
    let e = Elasticsearch {
        endpoint: reqwest::Url::parse(&format!("{scheme}://{host}:{port}/"))
            .map_err(|e| e.to_string())?,
        user: c.user.clone(),
        secret,
        auth,
        index: if c.database.is_empty() {
            "*".into()
        } else {
            c.database.clone()
        },
        browse: Default::default(),
    };
    e.send(reqwest::Method::GET, &[], None).await?;
    Ok(Db::new(e))
}
impl Elasticsearch {
    fn request(
        &self,
        method: reqwest::Method,
        parts: &[&str],
    ) -> DbResult<reqwest::RequestBuilder> {
        let mut url = self.endpoint.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| "Invalid Elasticsearch endpoint")?;
            segments.pop_if_empty();
            for p in parts {
                segments.push(p);
            }
        }
        let mut r = http::client().request(method, url);
        match self.auth.as_str() {
            "basic" => r = r.basic_auth(&self.user, Some(&self.secret)),
            "api_key" => r = r.header("Authorization", format!("ApiKey {}", self.secret)),
            _ => {}
        }
        Ok(r)
    }
    async fn send(
        &self,
        method: reqwest::Method,
        parts: &[&str],
        body: Option<Value>,
    ) -> DbResult<Value> {
        let mut r = self.request(method, parts)?;
        if let Some(body) = body {
            r = r.json(&body);
        }
        http::send_json(r).await
    }
    async fn close(&self, pit: String) {
        let _ = self
            .send(reqwest::Method::DELETE, &["_pit"], Some(json!({"id":pit})))
            .await;
    }
    async fn search(&self, index: &str, mut body: Value, limit: usize) -> DbResult<Vec<Value>> {
        if !body.is_object() {
            return Err("Query DSL must be a JSON object.".into());
        }
        if body
            .get("from")
            .and_then(Value::as_u64)
            .is_some_and(|offset| offset > 0)
            || body.get("pit").is_some()
            || body.get("search_after").is_some()
        {
            return Err("Tusk manages PIT and search_after pagination. Remove from, pit and search_after from the query.".into());
        }
        let limit = body
            .get("size")
            .and_then(Value::as_u64)
            .map_or(limit, |size| limit.min(size as usize));
        // Preserve the complete response for aggregation and size-zero requests.
        if body.get("aggs").is_some() || body.get("aggregations").is_some() || limit == 0 {
            body["size"] = json!(0);
            return Ok(vec![
                self.send(reqwest::Method::POST, &[index, "_search"], Some(body))
                    .await?,
            ]);
        }
        let pit = http::send_json(
            self.request(reqwest::Method::POST, &[index, "_pit"])?
                .query(&[("keep_alive", "1m")]),
        )
        .await?;
        let mut id = pit["id"]
            .as_str()
            .ok_or("Elasticsearch did not return a search context")?
            .to_string();
        let result = async {
            let mut rows = Vec::new();
            body["sort"] = body.get("sort").cloned().unwrap_or(json!(["_shard_doc"]));
            body["seq_no_primary_term"] = json!(true);
            while rows.len() < limit {
                body["pit"] = json!({"id":id,"keep_alive":"1m"});
                body["size"] = json!((limit - rows.len()).min(1000));
                let response = self
                    .send(reqwest::Method::POST, &["_search"], Some(body.clone()))
                    .await?;
                if let Some(new) = response["pit_id"].as_str() {
                    id = new.into();
                }
                let hits = response["hits"]["hits"]
                    .as_array()
                    .ok_or("Invalid Elasticsearch search response")?;
                if hits.is_empty() {
                    break;
                }
                for hit in hits {
                    rows.push(hit.clone());
                }
                let Some(sort) = hits.last().and_then(|h| h.get("sort")) else {
                    break;
                };
                body["search_after"] = sort.clone();
            }
            Ok(rows)
        }
        .await;
        self.close(id).await;
        result
    }
    async fn writable(&self, index: &str) -> DbResult<()> {
        if index.contains(['*', ',', '/']) || index.is_empty() {
            return Err("Choose one concrete index for document edits.".into());
        }
        let v = self.send(reqwest::Method::GET, &[index], None).await?;
        if v.as_object()
            .is_none_or(|o| o.len() != 1 || !o.contains_key(index))
            || v[index].get("data_stream").is_some()
        {
            return Err("Editing requires a concrete index, not an alias or data stream.".into());
        }
        Ok(())
    }
}
impl Driver for Elasticsearch {
    fn reset_browse(&self) -> Fut<()> {
        let e = self.clone();
        Box::pin(async move {
            let mut state = e.browse.lock().await;
            if let Some(b) = state.take() {
                e.close(b.pit).await;
            }
            Ok(())
        })
    }
    fn engine(&self) -> Engine {
        Engine::Elasticsearch
    }
    fn version(&self) -> Fut<String> {
        let e = self.clone();
        Box::pin(async move {
            Ok(
                e.send(reqwest::Method::GET, &[], None).await?["version"]["number"]
                    .as_str()
                    .unwrap_or("")
                    .into(),
            )
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        self.schemas()
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        Box::pin(async { Ok(vec!["indices".into()]) })
    }
    fn default_schema(&self) -> Option<String> {
        Some("indices".into())
    }
    fn objects(&self, _: String) -> Fut<ObjectTree> {
        let e = self.clone();
        Box::pin(async move {
            let r = e
                .request(reqwest::Method::GET, &["_cat", "indices"])?
                .query(&[("format", "json"), ("h", "index")]);
            let v = http::send_json(r).await?;
            let mut tables = v
                .as_array()
                .ok_or("Invalid index list")?
                .iter()
                .filter_map(|v| v["index"].as_str().map(str::to_string))
                .filter(|s| !s.starts_with('.'))
                .collect::<Vec<_>>();
            tables.sort();
            Ok(ObjectTree {
                tables,
                ..Default::default()
            })
        })
    }
    fn columns(&self, _: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let e = self.clone();
        Box::pin(async move {
            let mapping = e
                .send(reqwest::Method::GET, &[&table, "_mapping"], None)
                .await?;
            let mut cols = vec![
                ("_index".to_string(), "text".to_string()),
                ("_id".into(), "text".into()),
                ("_seq_no".into(), "int8".into()),
                ("_primary_term".into(), "int8".into()),
                ("_source".into(), "jsonb".into()),
            ];
            if let Some(p) = mapping[&table]["mappings"]["properties"].as_object() {
                for (k, v) in p {
                    cols.push((
                        format!("_source.{k}"),
                        v["type"].as_str().unwrap_or("object").into(),
                    ));
                }
            }
            Ok(cols
                .into_iter()
                .map(|(name, ty)| GridColumnMeta {
                    name,
                    pg_type: ty.clone(),
                    sql_type: ty,
                    nullable: true,
                    default: None,
                    comment: None,
                    is_pk: false,
                    foreign_key: None,
                    enum_values: vec![],
                })
                .collect())
        })
    }
    fn count(&self, _: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let e = self.clone();
        Box::pin(async move {
            if filter.is_some() {
                return Err("Use Query DSL for Elasticsearch filtering.".into());
            }
            Ok(e.send(reqwest::Method::GET, &[&table, "_count"], None)
                .await?["count"]
                .as_i64()
                .unwrap_or(0))
        })
    }
    fn window(&self, r: WindowReq) -> Fut<Vec<Value>> {
        let e = self.clone();
        Box::pin(async move {
            if r.filter.is_some() || r.order_by.is_some() {
                return Err("Use Query DSL for Elasticsearch filters and sorting.".into());
            }
            let mut state = e.browse.lock().await;
            if state.as_ref().is_some_and(|b| b.index != r.table)
                && let Some(b) = state.take()
            {
                e.close(b.pit).await;
            }
            if state.is_none() {
                let v = http::send_json(
                    e.request(reqwest::Method::POST, &[&r.table, "_pit"])?
                        .query(&[("keep_alive", "1m")]),
                )
                .await?;
                *state = Some(Browse {
                    index: r.table.clone(),
                    pit: v["id"].as_str().ok_or("Missing search context")?.into(),
                    pages: vec![],
                });
            }
            let b = state.as_mut().unwrap();
            let requested = b
                .pages
                .iter()
                .position(|(offset, _, _, _)| *offset == r.offset);
            if let Some(page) = requested
                && let Some(rows) = &b.pages[page].2
            {
                return Ok(rows.clone());
            }
            loop {
                let page = requested.unwrap_or(b.pages.len());
                let offset = if page < b.pages.len() {
                    b.pages[page].0
                } else {
                    b.pages
                        .last()
                        .map_or(0, |(offset, count, _, _)| offset + *count as i64)
                };
                if offset > r.offset {
                    return Err("Choose the next or previous Elasticsearch page.".into());
                }
                let size = if page < b.pages.len() {
                    b.pages[page].1 as i64
                } else {
                    r.limit
                };
                let mut body = json!({"pit":{"id":b.pit,"keep_alive":"1m"},"size":size.clamp(1,1000),"sort":["_shard_doc"],"seq_no_primary_term":true});
                if page > 0
                    && let Some(after) = &b.pages[page - 1].3
                {
                    body["search_after"] = after.clone();
                }
                let response = match e
                    .send(reqwest::Method::POST, &["_search"], Some(body))
                    .await
                {
                    Ok(v) => v,
                    Err(err) => {
                        let pit = b.pit.clone();
                        *state = None;
                        e.close(pit).await;
                        return Err(format!(
                            "Search context expired or failed; refresh the table. {err}"
                        ));
                    }
                };
                if let Some(id) = response["pit_id"].as_str() {
                    b.pit = id.into();
                }
                let hits = response["hits"]["hits"]
                    .as_array()
                    .ok_or("Invalid search response")?
                    .clone();
                let after = hits.last().and_then(|h| h.get("sort")).cloned();
                let rows = hits
                    .into_iter()
                    .map(|mut h| {
                        if let Some(source) = h["_source"].as_object().cloned() {
                            for (k, v) in source {
                                h[format!("_source.{k}")] = v;
                            }
                        }
                        h
                    })
                    .collect::<Vec<_>>();
                let result = (offset, rows.len(), Some(rows.clone()), after);
                if page < b.pages.len() {
                    b.pages[page] = result;
                } else {
                    b.pages.push(result);
                }
                let mut retained = 0;
                for (i, (_, _, cached, _)) in b.pages.iter_mut().enumerate().rev() {
                    if cached.is_some() && i != page {
                        retained += 1;
                        if retained >= 8 {
                            *cached = None;
                        }
                    }
                }
                if offset == r.offset || rows.is_empty() {
                    return Ok(rows);
                }
            }
        })
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let e = self.clone();
        Box::pin(async move {
            let body = serde_json::from_str(&sql)
                .map_err(|err| format!("Invalid Query DSL JSON: {err}"))?;
            e.search(&e.index, body, limit.max(0) as usize).await
        })
    }
    fn query_columns(&self, _: String) -> Fut<Vec<String>> {
        Box::pin(async { Ok(vec!["_index".into(), "_id".into(), "_source".into()]) })
    }
    fn exec(&self, _: String) -> Fut<u64> {
        Box::pin(async { Err("Use the document editor for Elasticsearch writes.".into()) })
    }
    fn batch(&self, _: Vec<Stmt>) -> Fut<u64> {
        Box::pin(async { Err("Elasticsearch document changes are saved individually.".into()) })
    }
    fn script(&self, _: ObjKind, _: String, _: String, _: Script) -> Fut<String> {
        Box::pin(async { Err("Index administration is not available.".into()) })
    }
    fn document_get(&self, index: String, id: String) -> Fut<Value> {
        let e = self.clone();
        Box::pin(async move {
            e.writable(&index).await?;
            e.send(reqwest::Method::GET, &[&index, "_doc", &id], None)
                .await
        })
    }
    fn document_write(
        &self,
        index: String,
        id: String,
        source: Option<Value>,
        guard: Option<(u64, u64)>,
    ) -> Fut<Value> {
        let e = self.clone();
        Box::pin(async move {
            e.writable(&index).await?;
            if id.trim().is_empty() {
                return Err("A document ID is required.".into());
            }
            if let Some(ref s) = source
                && !s.is_object()
            {
                return Err("The document must be a JSON object.".into());
            }
            let method = if source.is_some() {
                reqwest::Method::PUT
            } else {
                reqwest::Method::DELETE
            };
            let mut request = match guard {
                Some((seq, term)) => e.request(method, &[&index, "_doc", &id])?.query(&[
                    ("if_seq_no", seq.to_string()),
                    ("if_primary_term", term.to_string()),
                ]),
                None if source.is_some() => e.request(method, &[&index, "_create", &id])?,
                None => {
                    return Err(
                        "Reload this document to obtain conflict protection before deleting."
                            .into(),
                    );
                }
            };
            request = request.query(&[("refresh", "wait_for")]);
            if let Some(source) = source {
                request = request.json(&source);
            }
            http::send_json(request).await
        })
    }
}
