//! Shared HTTP client for the drivers that talk JSON over HTTPS
//! (ClickHouse, LibSQL, Cloudflare D1, Snowflake, BigQuery, DynamoDB).

use std::sync::LazyLock;
use std::time::Duration;

use serde_json::Value;

use crate::db::DbResult;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("Tusk/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
});

pub fn client() -> &'static reqwest::Client {
    &CLIENT
}

/// Send a request on the shared runtime; the body as text, errors with the
/// server's message.
pub async fn send(req: reqwest::RequestBuilder) -> DbResult<String> {
    crate::db::run_db(async move {
        let resp = req.send().await.map_err(|e| describe(&e))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(error_text(status.as_u16(), &text));
        }
        Ok(text)
    })
    .await
}

/// [`send`] and parse JSON.
pub async fn send_json(req: reqwest::RequestBuilder) -> DbResult<Value> {
    let text = send(req).await?;
    serde_json::from_str(&text).map_err(|e| format!("bad JSON from the server: {e}"))
}

fn describe(e: &reqwest::Error) -> String {
    if e.is_connect() {
        format!("Can't reach the server: {e}")
    } else if e.is_timeout() {
        "The server didn't answer in time.".to_string()
    } else {
        e.to_string()
    }
}

/// The most useful part of an error body (JSON `message` / `error`, else the text).
fn error_text(status: u16, body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            let pick = |v: &Value| -> Option<String> {
                for k in ["message", "error", "Message", "__type"] {
                    match &v[k] {
                        Value::String(s) => return Some(s.clone()),
                        Value::Object(_) => {
                            if let Some(s) = v[k]["message"].as_str() {
                                return Some(s.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                v["errors"][0]["message"].as_str().map(str::to_string)
            };
            pick(&v)
        })
        .unwrap_or_else(|| body.trim().chars().take(500).collect());
    format!("HTTP {status}: {msg}")
}
