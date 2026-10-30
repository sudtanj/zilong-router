//! Connections to upstream MCP servers: a child process speaking newline-delimited
//! JSON-RPC on stdin/stdout, or a Streamable HTTP endpoint (JSON or SSE answers, with
//! the `Mcp-Session-Id` the server hands out).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

use super::{ServerConfig, Transport};

/// What we ask for; servers answer with the version they speak.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// `initialize` may have to wait for `npx`/`uvx` to download the server first.
const INIT_TIMEOUT: Duration = Duration::from_secs(120);
const STDERR_LINES: usize = 20;

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

pub enum Client {
    Stdio(Stdio),
    Http(Http),
}

impl Client {
    /// Starts (or opens a session with) the server and runs the `initialize` handshake.
    pub async fn connect(cfg: &ServerConfig, http: &reqwest::Client) -> Result<Client> {
        match cfg.transport {
            Transport::Stdio => {
                let c = Stdio::spawn(cfg)?;
                c.request("initialize", init_params(), INIT_TIMEOUT).await.context("initialize")?;
                c.notify("notifications/initialized").await?;
                Ok(Client::Stdio(c))
            }
            Transport::Http => {
                let c = Http::new(cfg, http.clone());
                c.handshake(INIT_TIMEOUT).await.context("initialize")?;
                Ok(Client::Http(c))
            }
        }
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        match self {
            Client::Stdio(c) => c.request(method, params, timeout).await,
            Client::Http(c) => c.request(method, params, timeout).await,
        }
    }

    pub fn alive(&self) -> bool {
        match self {
            Client::Stdio(c) => c.alive.load(Ordering::Relaxed),
            Client::Http(_) => true,
        }
    }

    /// True once after the server sent `notifications/tools/list_changed`.
    pub fn take_tools_changed(&self) -> bool {
        let flag = match self {
            Client::Stdio(c) => &c.tools_changed,
            Client::Http(c) => &c.tools_changed,
        };
        flag.swap(false, Ordering::Relaxed)
    }

    /// The last lines a stdio server wrote to stderr (empty for HTTP).
    pub fn stderr_tail(&self) -> Vec<String> {
        match self {
            Client::Stdio(c) => c.stderr.lock().iter().cloned().collect(),
            Client::Http(_) => Vec::new(),
        }
    }
}

// ----- stdio ----------------------------------------------------------------------

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct Stdio {
    /// Killed when dropped (`kill_on_drop`).
    _child: Child,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    tools_changed: Arc<AtomicBool>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Stdio {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn write_line(stdin: &tokio::sync::Mutex<ChildStdin>, msg: &Value) -> std::io::Result<()> {
    let mut line = msg.to_string();
    line.push('\n');
    let mut w = stdin.lock().await;
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

impl Stdio {
    fn spawn(cfg: &ServerConfig) -> Result<Stdio> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if !cfg.cwd.trim().is_empty() {
            cmd.current_dir(cfg.cwd.trim());
        }
        let mut child = cmd.spawn().with_context(|| format!("cannot start `{}`", cfg.command))?;
        let stdin = Arc::new(tokio::sync::Mutex::new(child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?));
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr_pipe = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let tools_changed = Arc::new(AtomicBool::new(false));
        let stderr: Arc<Mutex<VecDeque<String>>> = Arc::default();

        let reader = {
            let (pending, alive, changed) = (pending.clone(), alive.clone(), tools_changed.clone());
            let stdin = stdin.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    // Servers that log to stdout break the protocol; skip what isn't JSON.
                    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
                    for msg in messages(v) {
                        let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
                        match (method.as_deref(), msg.get("id").cloned()) {
                            (None, Some(id)) => {
                                if let Some(tx) = id.as_u64().and_then(|n| pending.lock().remove(&n)) {
                                    let _ = tx.send(msg);
                                }
                            }
                            (Some(m), Some(id)) => {
                                let _ = write_line(&stdin, &answer_server_request(m, &id)).await;
                            }
                            (Some("notifications/tools/list_changed"), None) => changed.store(true, Ordering::Relaxed),
                            _ => {}
                        }
                    }
                }
                alive.store(false, Ordering::Relaxed);
                // Dropping the senders wakes every waiting request with "exited".
                pending.lock().clear();
            })
        };
        let stderr_task = {
            let (buf, name) = (stderr.clone(), cfg.id.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr_pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!("mcp {name}: {line}");
                    let mut b = buf.lock();
                    b.push_back(line);
                    if b.len() > STDERR_LINES {
                        b.pop_front();
                    }
                }
            })
        };

        Ok(Stdio {
            _child: child,
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            alive,
            tools_changed,
            stderr,
            tasks: vec![reader, stderr_task],
        })
    }

    fn exited(&self) -> anyhow::Error {
        let tail: Vec<String> = self.stderr.lock().iter().rev().take(5).rev().cloned().collect();
        if tail.is_empty() {
            anyhow!("server process exited")
        } else {
            anyhow!("server process exited: {}", tail.join(" | "))
        }
    }

    async fn notify(&self, method: &str) -> Result<()> {
        write_line(&self.stdin, &json!({"jsonrpc": "2.0", "method": method})).await.map_err(|_| self.exited())
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(self.exited());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if write_line(&self.stdin, &msg).await.is_err() {
            self.pending.lock().remove(&id);
            return Err(self.exited());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => into_result(resp),
            Ok(Err(_)) => Err(self.exited()),
            Err(_) => {
                self.pending.lock().remove(&id);
                let cancel = json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": {"requestId": id, "reason": "timeout"}});
                let _ = write_line(&self.stdin, &cancel).await;
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
            let body = resp.text().await.unwrap_or_default();
            let body: String = body.chars().take(300).collect();
            bail!("HTTP {status}: {}", body.trim());
        }
        let Some(id) = msg.get("id") else { return Ok(None) };
        let is_answer = |m: &Value| m.get("id") == Some(id) && m.get("method").is_none();

        let sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream"));
        if !sse {
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
