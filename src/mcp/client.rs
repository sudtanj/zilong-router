//! Connections to upstream MCP servers, remote only: Streamable HTTP (JSON-RPC over
//! POST, answers as JSON or SSE, with the `Mcp-Session-Id` the server hands out) and
//! the older HTTP+SSE transport (a GET event stream that names a POST endpoint and
//! carries the answers).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::{ServerConfig, Transport};

/// What we ask for; servers answer with the version they speak.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Remote servers may be slow to wake up (serverless cold starts).
const INIT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long an SSE server may take to name its POST endpoint.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(30);

fn init_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "zilong-router", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// The `result` of a JSON-RPC response, or its `error` as an `Err`.
fn into_result(msg: Value) -> Result<Value> {
    if let Some(e) = msg.get("error") {
        let text = e.get("message").and_then(Value::as_str).unwrap_or("error");
        match e.get("code").and_then(Value::as_i64) {
            Some(code) => bail!("{text} ({code})"),
            None => bail!("{text}"),
        }
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

/// A message or a batch, as a list.
fn messages(v: Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items,
        v => vec![v],
    }
}

/// Our answer to a request the server sends us. We declare no client capabilities, so
/// only `ping` (and an empty `roots/list`, which some servers ask regardless) succeed.
fn answer_server_request(method: &str, id: &Value) -> Value {
    match method {
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "roots/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"roots": []}}),
        _ => {
            let error = json!({"code": -32601, "message": "not supported by zilong-router"});
            json!({"jsonrpc": "2.0", "id": id, "error": error})
        }
    }
}

/// An HTTP error with the start of its body, for the dashboard.
async fn http_error(resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let body: String = body.chars().take(300).collect();
    anyhow!("HTTP {status}: {}", body.trim())
}

fn is_event_stream(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"))
}

pub enum Client {
    Http(Http),
    Sse(Sse),
}

impl Client {
    /// Opens a session with the server and runs the `initialize` handshake.
    pub async fn connect(cfg: &ServerConfig, http: &reqwest::Client) -> Result<Client> {
        match cfg.transport {
            Transport::Http => {
                let c = Http::new(cfg, http.clone());
                c.handshake(INIT_TIMEOUT).await.context("initialize")?;
                Ok(Client::Http(c))
            }
            Transport::Sse => {
                let c = Sse::open(cfg, http.clone()).await?;
                c.request("initialize", init_params(), INIT_TIMEOUT).await.context("initialize")?;
                c.notify("notifications/initialized").await?;
                Ok(Client::Sse(c))
            }
        }
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        match self {
            Client::Http(c) => c.request(method, params, timeout).await,
            Client::Sse(c) => c.request(method, params, timeout).await,
        }
    }

    /// False once an SSE server closed its event stream (it is reopened on next use).
    pub fn alive(&self) -> bool {
        match self {
            Client::Http(_) => true,
            Client::Sse(c) => c.alive.load(Ordering::Relaxed),
        }
    }

    /// True once after the server sent `notifications/tools/list_changed`.
    pub fn take_tools_changed(&self) -> bool {
        let flag = match self {
            Client::Http(c) => &c.tools_changed,
            Client::Sse(c) => &*c.tools_changed,
        };
        flag.swap(false, Ordering::Relaxed)
    }
}

// ----- HTTP+SSE (protocol 2024-11-05) ---------------------------------------------

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// Where to POST and with which headers; shared with the stream reader, which answers
/// the server's own requests (pings) there.
struct Poster {
    http: reqwest::Client,
    endpoint: String,
    headers: Vec<(String, String)>,
}

impl Poster {
    /// Sends one message. Answers arrive on the event stream; a server that puts one in
    /// the POST body instead gets it returned here.
    async fn post(&self, msg: &Value) -> Result<Option<Value>> {
        let mut rb = self.http.post(&self.endpoint).header(reqwest::header::CONTENT_TYPE, "application/json");
        for (k, v) in &self.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let resp = rb.body(msg.to_string()).send().await?;
        if !resp.status().is_success() {
            return Err(http_error(resp).await);
        }
        let body = resp.text().await.unwrap_or_default();
        let Some(id) = msg.get("id") else { return Ok(None) };
        let Ok(v) = serde_json::from_str::<Value>(&body) else { return Ok(None) };
        Ok(messages(v).into_iter().find(|m| m.get("id") == Some(id) && m.get("method").is_none()))
    }
}

pub struct Sse {
    poster: Arc<Poster>,
    pending: Pending,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    tools_changed: Arc<AtomicBool>,
    /// Reads the event stream; aborting it closes the connection.
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Sse {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Sse {
    /// Opens the event stream and waits for the `endpoint` event.
    async fn open(cfg: &ServerConfig, http: reqwest::Client) -> Result<Sse> {
        let url = reqwest::Url::parse(cfg.url.trim()).context("invalid URL")?;
        let headers: Vec<(String, String)> = cfg.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut rb = http.get(url.clone()).header(reqwest::header::ACCEPT, "text/event-stream");
        for (k, v) in &headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let resp = tokio::time::timeout(INIT_TIMEOUT, rb.send())
            .await
            .map_err(|_| anyhow!("no answer within {} s", INIT_TIMEOUT.as_secs()))??;
        if !resp.status().is_success() {
            return Err(http_error(resp).await);
        }
        if !is_event_stream(&resp) {
            bail!("not an SSE endpoint (no text/event-stream); try the Streamable HTTP transport");
        }

        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let tools_changed = Arc::new(AtomicBool::new(false));
        let (endpoint_tx, endpoint_rx) = oneshot::channel::<Arc<Poster>>();
        let reader = {
            let (pending, alive, changed) = (pending.clone(), alive.clone(), tools_changed.clone());
            let id = cfg.id.clone();
            tokio::spawn(async move {
                let mut endpoint_tx = Some(endpoint_tx);
                let mut poster: Option<Arc<Poster>> = None;
                let mut parser = crate::sse::Parser::default();
                let mut stream = resp.bytes_stream();
                while let Some(Ok(chunk)) = stream.next().await {
                    for ev in parser.push(&chunk) {
                        if ev.event.as_deref() == Some("endpoint") {
                            let Ok(endpoint) = url.join(ev.data.trim()) else {
                                tracing::warn!("mcp {id}: bad endpoint {:?}", ev.data);
                                continue;
                            };
                            let p = Arc::new(Poster {
                                http: http.clone(),
                                endpoint: endpoint.to_string(),
                                headers: headers.clone(),
                            });
                            poster = Some(p.clone());
                            if let Some(tx) = endpoint_tx.take() {
                                let _ = tx.send(p);
                            }
                            continue;
                        }
                        if !matches!(ev.event.as_deref(), None | Some("message")) {
                            continue;
                        }
                        let Ok(v) = serde_json::from_str::<Value>(ev.data.trim()) else { continue };
                        for msg in messages(v) {
                            let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
                            match (method.as_deref(), msg.get("id").cloned()) {
                                (None, Some(id)) => {
                                    if let Some(tx) = id.as_u64().and_then(|n| pending.lock().remove(&n)) {
                                        let _ = tx.send(msg);
                                    }
                                }
                                (Some(m), Some(id)) => {
                                    if let Some(p) = poster.clone() {
                                        let answer = answer_server_request(m, &id);
                                        tokio::spawn(async move {
                                            let _ = p.post(&answer).await;
                                        });
                                    }
                                }
                                (Some("notifications/tools/list_changed"), None) => {
                                    changed.store(true, Ordering::Relaxed);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                alive.store(false, Ordering::Relaxed);
                // Dropping the senders wakes every waiting request with "closed".
                pending.lock().clear();
            })
        };
        let poster = match tokio::time::timeout(ENDPOINT_TIMEOUT, endpoint_rx).await {
            Ok(Ok(p)) => p,
            Ok(Err(_)) => {
                reader.abort();
                bail!("the event stream ended before naming its endpoint");
            }
            Err(_) => {
                reader.abort();
                bail!("no endpoint event within {} s", ENDPOINT_TIMEOUT.as_secs());
            }
        };
        Ok(Sse { poster, pending, next_id: AtomicU64::new(1), alive, tools_changed, reader })
    }

    async fn notify(&self, method: &str) -> Result<()> {
        self.poster.post(&json!({"jsonrpc": "2.0", "method": method})).await.map(|_| ())
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if !self.alive.load(Ordering::Relaxed) {
            bail!("the server closed the event stream");
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let answer = async {
            if let Some(resp) = self.poster.post(&msg).await? {
                return Ok(resp);
            }
            rx.await.map_err(|_| anyhow!("the server closed the event stream"))
        };
        match tokio::time::timeout(timeout, answer).await {
            Ok(Ok(resp)) => {
                self.pending.lock().remove(&id);
                into_result(resp)
            }
            Ok(Err(e)) => {
                self.pending.lock().remove(&id);
                Err(e)
            }
            Err(_) => {
                self.pending.lock().remove(&id);
                let cancel = json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": {"requestId": id, "reason": "timeout"}});
                let _ = self.poster.post(&cancel).await;
                bail!("no answer within {} s", timeout.as_secs())
            }
        }
    }
}

// ----- Streamable HTTP ------------------------------------------------------------

/// The server forgot our session (HTTP 404 with a session id): start a new one.
#[derive(Debug)]
struct SessionExpired;

impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MCP session expired")
    }
}

impl std::error::Error for SessionExpired {}

pub struct Http {
    url: String,
    headers: Vec<(String, String)>,
    http: reqwest::Client,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    next_id: AtomicU64,
    tools_changed: AtomicBool,
}

impl Drop for Http {
    /// Best effort: tell the server it can drop our session.
    fn drop(&mut self) {
        let Some(session) = self.session.lock().take() else { return };
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let mut rb = self.http.delete(&self.url).header("mcp-session-id", session);
        for (k, v) in &self.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        rt.spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(5), rb.send()).await;
        });
    }
}

impl Http {
    fn new(cfg: &ServerConfig, http: reqwest::Client) -> Http {
        Http {
            url: cfg.url.trim().to_string(),
            headers: cfg.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            http,
            session: Mutex::new(None),
            protocol: Mutex::new(None),
            next_id: AtomicU64::new(1),
            tools_changed: AtomicBool::new(false),
        }
    }

    async fn handshake(&self, timeout: Duration) -> Result<()> {
        *self.session.lock() = None;
        *self.protocol.lock() = None;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let init = json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": init_params()});
        let resp = self.post(&init, timeout).await?.ok_or_else(|| anyhow!("no answer to initialize"))?;
        let result = into_result(resp)?;
        *self.protocol.lock() = result.get("protocolVersion").and_then(Value::as_str).map(str::to_string);
        self.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), timeout).await?;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = match self.post(&msg, timeout).await {
            Err(e) if e.is::<SessionExpired>() => {
                self.handshake(timeout).await?;
                self.post(&msg, timeout).await?
            }
            other => other?,
        };
        into_result(resp.ok_or_else(|| anyhow!("no answer from the server"))?)
    }

    async fn post(&self, msg: &Value, timeout: Duration) -> Result<Option<Value>> {
        match tokio::time::timeout(timeout, self.post_inner(msg)).await {
            Ok(r) => r,
            Err(_) => bail!("no answer within {} s", timeout.as_secs()),
        }
    }

    /// Sends one message and returns the response carrying its id (`None` for a
    /// notification, which the server acknowledges with 202).
    async fn post_inner(&self, msg: &Value) -> Result<Option<Value>> {
        let session = self.session.lock().clone();
        let mut rb = self
            .http
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream");
        for (k, v) in &self.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if let Some(s) = &session {
            rb = rb.header("mcp-session-id", s.as_str());
        }
        if let Some(p) = self.protocol.lock().clone() {
            rb = rb.header("mcp-protocol-version", p);
        }
        let resp = rb.body(msg.to_string()).send().await?;
        let status = resp.status();
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock() = Some(s.to_string());
        }
        if status == reqwest::StatusCode::NOT_FOUND && session.is_some() {
            return Err(SessionExpired.into());
        }
        if !status.is_success() {
            return Err(http_error(resp).await);
        }
        let Some(id) = msg.get("id") else { return Ok(None) };
        let is_answer = |m: &Value| m.get("id") == Some(id) && m.get("method").is_none();

        if !is_event_stream(&resp) {
            let v: Value = resp.json().await.context("invalid JSON from the server")?;
            return messages(v).into_iter().find(is_answer).map(Some).ok_or_else(|| anyhow!("no answer in the reply"));
        }
        let mut parser = crate::sse::Parser::default();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            for ev in parser.push(&chunk?) {
                let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { continue };
                for m in messages(v) {
                    if is_answer(&m) {
                        return Ok(Some(m));
                    }
                    if m.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed") {
                        self.tools_changed.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        bail!("the event stream ended without an answer")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_errors_become_errs() {
        assert_eq!(into_result(json!({"id": 1, "result": {"a": 1}})).unwrap(), json!({"a": 1}));
        let e = into_result(json!({"id": 1, "error": {"code": -32601, "message": "nope"}})).unwrap_err();
        assert_eq!(e.to_string(), "nope (-32601)");
    }

    #[test]
    fn answers_pings_only() {
        assert_eq!(answer_server_request("ping", &json!(7))["result"], json!({}));
        assert_eq!(answer_server_request("sampling/createMessage", &json!(8))["error"]["code"], -32601);
    }
}
