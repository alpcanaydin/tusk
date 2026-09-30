//! Direct local/custom model connections using compatible chat-completions APIs.
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub tools: bool,
    pub key_required: bool,
}
impl Default for Provider {
    fn default() -> Self {
        Self {
            name: "Custom endpoint".into(),
            base_url: "http://localhost:11434/v1".into(),
            model: String::new(),
            tools: true,
            key_required: false,
        }
    }
}
pub fn defaults() -> BTreeMap<String, Provider> {
    BTreeMap::from([
        (
            "ollama".into(),
            Provider {
                name: "Ollama".into(),
                ..Default::default()
            },
        ),
        (
            "lm-studio".into(),
            Provider {
                name: "LM Studio".into(),
                base_url: "http://localhost:1234/v1".into(),
                ..Default::default()
            },
        ),
    ])
}
pub fn secret_id(id: &str) -> String {
    format!("tusk-ai:{id}")
}
pub fn load_key(p: &Provider, id: &str) -> Result<String, String> {
    if p.key_required {
        crate::db::load_password(&secret_id(id))
    } else {
        Ok(String::new())
    }
}
fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())
}
fn url(p: &Provider, path: &str) -> Result<String, String> {
    let u = reqwest::Url::parse(&p.base_url)
        .map_err(|_| "Enter a valid HTTP(S) base URL.".to_string())?;
    if !matches!(u.scheme(), "http" | "https")
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err("Use an HTTP(S) base URL without credentials, query, or fragment.".into());
    }
    Ok(format!("{}/{path}", p.base_url.trim_end_matches('/')))
}
pub fn validate_endpoint(p: &Provider) -> Result<(), String> {
    url(p, "models").map(|_| ())
}
fn auth(r: reqwest::RequestBuilder, key: &str) -> reqwest::RequestBuilder {
    if key.is_empty() {
        r
    } else {
        r.bearer_auth(key)
    }
}
pub async fn models(p: &Provider, key: &str) -> Result<Vec<String>, String> {
    let response = auth(
        client()?
            .get(url(p, "models")?)
            .timeout(Duration::from_secs(10)),
        key,
    )
    .send()
    .await
    .map_err(|_| {
        "Server offline or unreachable. Start the model server and check its endpoint.".to_string()
    })?;
    if !response.status().is_success() {
        return Err(format!(
            "Model discovery failed (HTTP {}). Check authentication or enter a model ID manually.",
            response.status()
        ));
    }
    let v: Value = response.json().await.map_err(|e| e.to_string())?;
    let mut models = v["data"]
        .as_array()
        .ok_or("Model discovery unavailable; enter a model ID manually.")?
        .iter()
        .filter_map(|v| v["id"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    Ok(models)
}
pub enum Event {
    Text(String),
    Notice(String),
    Done(Vec<Value>),
    Error(String),
}
fn emit(tx: &mpsc::UnboundedSender<Event>, event: Event) -> Result<(), String> {
    tx.send(event).map_err(|_| "Conversation closed".into())
}
#[derive(Default)]
struct Tool {
    id: String,
    name: String,
    args: String,
}
fn delta(
    v: &Value,
    text: &mut String,
    tools: &mut BTreeMap<u64, Tool>,
    tx: &mpsc::UnboundedSender<Event>,
) -> Result<(), String> {
    if let Some(e) = v.get("error") {
        return Err(e.to_string());
    }
    let d = &v["choices"][0]["delta"];
    if let Some(s) = d["content"].as_str() {
        text.push_str(s);
        emit(tx, Event::Text(s.into()))?;
    }
    if let Some(calls) = d["tool_calls"].as_array() {
        for call in calls {
            let t = tools
                .entry(call["index"].as_u64().ok_or("Invalid tool-call index")?)
                .or_default();
            if let Some(id) = call["id"].as_str() {
                t.id.push_str(id);
            }
            if let Some(n) = call["function"]["name"].as_str() {
                t.name.push_str(n);
            }
            if let Some(a) = call["function"]["arguments"].as_str() {
                t.args.push_str(a);
            }
            if t.args.len() > 1_000_000 {
                return Err("Tool arguments exceed the supported limit.".into());
            }
        }
    }
    Ok(())
}
pub fn validate_tool(name: &str, args: &Value) -> Result<(), String> {
    let all = crate::mcp_bridge::tools();
    let spec = all
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .ok_or("Unknown tool; database execution is unavailable to the assistant.")?;
    let obj = args.as_object().ok_or("Tool arguments must be an object")?;
    if let Some(required) = spec["inputSchema"]["required"].as_array() {
        for key in required {
            let key = key.as_str().unwrap();
            if !obj.contains_key(key) {
                return Err(format!("Missing tool argument: {key}"));
            }
        }
    }
    for (k, v) in obj {
        if spec["inputSchema"]["properties"].get(k).is_none() || !v.is_string() {
            return Err(format!("Invalid tool argument: {k}"));
        }
    }
    Ok(())
}
pub async fn chat(
    p: Provider,
    key: String,
    mut messages: Vec<Value>,
    mcp: Value,
    tx: mpsc::UnboundedSender<Event>,
) -> Result<(), String> {
    if p.model.trim().is_empty() {
        return Err("Choose a model in AI Providers settings or the model selector.".into());
    }
    let client = client()?;
    for round in 0..=8 {
        let mut body = json!({"model":p.model,"messages":messages,"stream":true});
        if p.tools {
            body["tools"]=Value::Array(crate::mcp_bridge::tools().as_array().unwrap().iter().map(|s|json!({"type":"function","function":{"name":s["name"],"description":s["description"],"parameters":s["inputSchema"]}})).collect());
        }
        let response = auth(client.post(url(&p, "chat/completions")?).json(&body), &key)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "Chat failed (HTTP {}). Check the model, authentication, and tool support in AI Providers.",
                response.status()
            ));
        }
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut text = String::new();
        let mut tools = BTreeMap::new();
        let mut done = false;
        while let Some(chunk) = stream.next().await {
            buffer.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
            if buffer.len() > 2_000_000 {
                return Err("Streaming response exceeds the supported event size.".into());
            }
            while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                let line = String::from_utf8(buffer.drain(..=end).collect())
                    .map_err(|_| "Invalid UTF-8 stream")?;
                if let Some(data) = line.trim().strip_prefix("data:") {
                    let data = data.trim();
                    if data == "[DONE]" {
                        done = true;
                        break;
                    }
                    if !data.is_empty() {
                        let v: Value = serde_json::from_str(data)
                            .map_err(|e| format!("Malformed streaming reply: {e}"))?;
                        delta(&v, &mut text, &mut tools, &tx)?;
                    }
                }
            }
            if done {
                break;
            }
        }
        if !done {
            return Err("The model stream ended before completion. Retry explicitly.".into());
        }
        if tools.is_empty() {
            messages.push(json!({"role":"assistant","content":text}));
            emit(&tx, Event::Done(messages))?;
            return Ok(());
        }
        if !p.tools || round == 8 {
            return Err("The assistant reached the limit of eight tool rounds. Send a follow-up to continue.".into());
        }
        let calls=tools.values().map(|t|json!({"id":t.id,"type":"function","function":{"name":t.name,"arguments":t.args}})).collect::<Vec<_>>();
        messages.push(json!({"role":"assistant","content":text,"tool_calls":calls}));
        for t in tools.into_values() {
            if t.id.is_empty() {
                return Err("Tool call has no ID".into());
            }
            let args: Value = serde_json::from_str(&t.args)
                .map_err(|e| format!("Malformed tool arguments: {e}"))?;
            validate_tool(&t.name, &args)?;
            emit(&tx, Event::Notice(format!("Using {}", t.name)))?;
            let spec = mcp.clone();
            let name = t.name.clone();
            let (ok, result) =
                tokio::task::spawn_blocking(move || crate::mcp_bridge::invoke(&spec, &name, args))
                    .await
                    .map_err(|e| e.to_string())?;
            messages.push(json!({"role":"tool","tool_call_id":t.id,"content":if ok{result}else{format!("Tool failed: {result}")}}));
        }
    }
    Err("Tool round limit reached".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tools_cannot_execute() {
        assert!(validate_tool("run_sql", &json!({"sql":"DELETE FROM users"})).is_err());
        assert!(validate_tool("open_sql_tab", &json!({"sql":42})).is_err());
        assert!(validate_tool("describe_table", &json!({})).is_err());
        assert!(validate_tool("open_sql_tab", &json!({"sql":"SELECT 1"})).is_ok());
    }
    #[test]
    fn fragmented_tool_and_text() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut text = String::new();
        let mut tools = BTreeMap::new();
        delta(&json!({"choices":[{"delta":{"content":"hi","tool_calls":[{"index":0,"id":"a","function":{"name":"get_context","arguments":"{"}}]}}]}),&mut text,&mut tools,&tx).unwrap();
        delta(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]}}]}),&mut text,&mut tools,&tx).unwrap();
        assert_eq!(tools[&0].args, "{}");
        assert_eq!(text, "hi");
        assert!(matches!(rx.try_recv(), Ok(Event::Text(_))));
    }
}

#[cfg(test)]
mod transport_tests {
    use super::{Event, Provider, chat, models};
    use serde_json::json;
    use std::io::{Read as _, Write as _};
    fn server(status: &str, body: String) -> (String, std::thread::JoinHandle<String>) {
        server_replies(status, body, 1)
    }
    fn server_replies(
        status: &str,
        body: String,
        replies: usize,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let status = status.to_string();
        let thread = std::thread::spawn(move || {
            let mut captured = String::new();
            for _ in 0..replies {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let n = socket.read(&mut buffer).unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let len = headers
                            .lines()
                            .find_map(|s| {
                                s.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|s| s.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).unwrap();
                for byte in body.as_bytes() {
                    if socket.write_all(&[*byte]).is_err() {
                        break;
                    }
                }
                captured = String::from_utf8(request).unwrap();
            }
            captured
        });
        (endpoint, thread)
    }
    #[test]
    fn discovers_models_and_streams_unicode() {
        let (base, server) = server("200 OK", "{\"data\":[{\"id\":\"local-model\"}]}".into());
        let p = Provider {
            base_url: base,
            ..Default::default()
        };
        assert_eq!(
            crate::db::runtime()
                .block_on(models(&p, "fixture-key"))
                .unwrap(),
            vec!["local-model"]
        );
        assert!(server.join().unwrap().contains("Bearer fixture-key"));
        let body =
            "data: {\"choices\":[{\"delta\":{\"content\":\"café 日本語\"}}]}\n\ndata: [DONE]\n\n";
        let (base, server) = self::server("200 OK", body.into());
        let p = Provider {
            base_url: base,
            model: "local-model".into(),
            tools: false,
            ..Default::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        crate::db::runtime()
            .block_on(chat(
                p,
                String::new(),
                vec![json!({"role":"user","content":"hello"})],
                json!({}),
                tx,
            ))
            .unwrap();
        assert!(matches!(rx.try_recv(), Ok(Event::Text(s)) if s == "café 日本語"));
        assert!(matches!(rx.try_recv(), Ok(Event::Done(_))));
        let request = server.join().unwrap();
        assert!(request.contains("local-model"));
        assert!(!request.contains("\"tools\""));
    }
    #[test]
    fn bounds_tool_loops_without_execution() {
        let (host, mut tools) = crate::mcp_bridge::Host::start().unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        let handler = crate::db::runtime().spawn(async move {
            while let Some(request) = tools.recv().await {
                assert_eq!(request.name, "get_context");
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = request.reply.send(Ok("fixture metadata".into()));
            }
        });
        let reply = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"function\":{\"name\":\"get_context\",\"arguments\":\"{}\"}}]}}]}\n\ndata: [DONE]\n\n";
        let (base, server) = server_replies("200 OK", reply.into(), 9);
        let p = Provider {
            base_url: base,
            model: "fixture".into(),
            tools: true,
            ..Default::default()
        };
        let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
        let error = crate::db::runtime()
            .block_on(chat(p, String::new(), vec![], host.server_spec(), tx))
            .unwrap_err();
        assert!(error.contains("eight tool rounds"), "{error}");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 8);
        handler.abort();
        server.join().unwrap();
    }
    #[test]
    fn rejects_bad_stream_and_unavailable_auth() {
        let (base, server) = server("401 Unauthorized", "{}".into());
        let p = Provider {
            base_url: base,
            ..Default::default()
        };
        assert!(
            crate::db::runtime()
                .block_on(models(&p, ""))
                .unwrap_err()
                .contains("401")
        );
        server.join().unwrap();
        let (base, server) = self::server("200 OK", "data: not-json\n\n".into());
        let p = Provider {
            base_url: base,
            model: "local-model".into(),
            ..Default::default()
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(
            crate::db::runtime()
                .block_on(chat(p, String::new(), vec![], json!({}), tx))
                .unwrap_err()
                .contains("Malformed")
        );
        server.join().unwrap();
    }
}
