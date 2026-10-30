//! End-to-end checks for the MCP gateway: the real `zilong-router` binary in front of a
//! mock Streamable HTTP MCP server (sessions, paginated tool lists, SSE answers, an
//! upstream auth header) and a mock server on the older HTTP+SSE transport (an event
//! stream naming a POST endpoint, answers on the stream).

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::TcpListener as StdListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

const UPSTREAM_AUTH: &str = "Bearer up-secret";

fn authorized(headers: &HeaderMap) -> bool {
    headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some(UPSTREAM_AUTH)
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({"name": name, "description": description, "inputSchema": schema})
}

fn text(t: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": t.into()}]})
}

/// What both mock servers answer to a request after `initialize`: the result, or the
/// JSON-RPC error. `echo` puts `prefix` before the text, so tests can tell them apart.
fn answer(msg: &Value, prefix: &str) -> Result<Value, Value> {
    let result = match msg["method"].as_str().unwrap_or_default() {
        // Two pages, to check that the gateway follows nextCursor.
        "tools/list" if msg["params"]["cursor"] == "p2" => {
            json!({"tools": [tool("fail", "Always fails", json!({"type": "object"}))]})
        }
        "tools/list" => {
            let echo = json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]});
            let add = json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                "required": ["a", "b"]});
            json!({"tools": [tool("echo", "Echo text back", echo), tool("add", "Add two numbers", add)],
                "nextCursor": "p2"})
        }
        "tools/call" => {
            let args = &msg["params"]["arguments"];
            match msg["params"]["name"].as_str() {
                Some("echo") => text(format!("{prefix}{}", args["text"].as_str().unwrap_or_default())),
                Some("add") => text((args["a"].as_f64().unwrap() + args["b"].as_f64().unwrap()).to_string()),
                _ => json!({"content": [{"type": "text", "text": "it broke"}], "isError": true}),
            }
        }
        _ => return Err(json!({"code": -32601, "message": "method not found"})),
    };
    Ok(result)
}

fn response(id: &Value, answer: Result<Value, Value>) -> Value {
    match answer {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    }
}

// ----- mock Streamable HTTP server ---------------------------------------------------

#[derive(Clone, Default)]
struct Upstream {
    sessions: Arc<Mutex<HashSet<String>>>,
    inits: Arc<AtomicUsize>,
}

async fn upstream_mcp(State(u): State<Upstream>, headers: HeaderMap, Json(msg): Json<Value>) -> Response {
    if !authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "bad upstream credentials").into_response();
    }
    let method = msg["method"].as_str().unwrap_or_default();
    let id = msg["id"].clone();
    if method == "initialize" {
        let sid = format!("s{}", u.inits.fetch_add(1, Ordering::SeqCst) + 1);
        u.sessions.lock().unwrap().insert(sid.clone());
        let result = json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock", "version": "1"}});
        return ([("mcp-session-id", sid)], Json(response(&id, Ok(result)))).into_response();
    }
    let sid = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !u.sessions.lock().unwrap().contains(sid) {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    }
    if id.is_null() {
        return StatusCode::ACCEPTED.into_response();
    }
    let body = response(&id, answer(&msg, ""));
    if method == "tools/call" {
        // Tool results come back as an event stream, the other answers as plain JSON.
        ([(header::CONTENT_TYPE, "text/event-stream")], format!("event: message\ndata: {body}\n\n")).into_response()
    } else {
        Json(body).into_response()
    }
}

/// Forget every session, as a restarted server would.
async fn upstream_reset(State(u): State<Upstream>) -> StatusCode {
    u.sessions.lock().unwrap().clear();
    StatusCode::OK
}

async fn start_upstream(u: Upstream) -> String {
    let app = Router::new().route("/mcp", post(upstream_mcp)).route("/reset", post(upstream_reset)).with_state(u);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

// ----- mock HTTP+SSE server ----------------------------------------------------------

#[derive(Clone, Default)]
struct SseUpstream {
    /// Open event streams by session id.
    streams: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>>,
    opened: Arc<AtomicUsize>,
}

/// `GET /sse`: an event stream that first names the endpoint to POST messages to.
async fn sse_open(State(u): State<SseUpstream>, headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "bad upstream credentials").into_response();
    }
    let sid = format!("e{}", u.opened.fetch_add(1, Ordering::SeqCst) + 1);
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    tx.send(format!("event: endpoint\ndata: /messages?sessionId={sid}\n\n")).unwrap();
    u.streams.lock().unwrap().insert(sid, tx);
    let body = Body::from_stream(UnboundedReceiverStream::new(rx).map(Ok::<_, Infallible>));
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

/// `POST /messages?sessionId=…`: accepted at once, answered on the session's stream.
async fn sse_message(
    State(u): State<SseUpstream>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(msg): Json<Value>,
) -> Response {
    if !authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "bad upstream credentials").into_response();
    }
    let sid = q.get("sessionId").cloned().unwrap_or_default();
    let Some(tx) = u.streams.lock().unwrap().get(&sid).cloned() else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    let id = msg["id"].clone();
    if !id.is_null() {
        let body = match msg["method"].as_str() {
            Some("initialize") => {
                let result = json!({"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock-sse", "version": "1"}});
                response(&id, Ok(result))
            }
            _ => response(&id, answer(&msg, "sse: ")),
        };
        let _ = tx.send(format!("event: message\ndata: {body}\n\n"));
    }
    (StatusCode::ACCEPTED, "Accepted").into_response()
}

/// Close every event stream, as a restarted server would.
async fn sse_reset(State(u): State<SseUpstream>) -> StatusCode {
    u.streams.lock().unwrap().clear();
    StatusCode::OK
}

async fn start_sse_upstream(u: SseUpstream) -> String {
    let app = Router::new()
        .route("/sse", get(sse_open))
        .route("/messages", post(sse_message))
        .route("/reset", post(sse_reset))
        .with_state(u);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

// ----- the router ------------------------------------------------------------------------

/// A running router process; killed and its data directory removed on drop.
struct Instance {
    child: Child,
    base: String,
    dir: PathBuf,
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn start_router(name: &str, idle_secs: u64) -> Instance {
    let dir = std::env::temp_dir().join(format!("zilong-router-mcp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let port = StdListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zilong-router"));
    cmd.args(["--host", "127.0.0.1", "--port", &port.to_string(), "--data-dir"])
        .arg(&dir)
        .env("LOG_LEVEL", "warn")
        .env("MCP_IDLE_SECS", idle_secs.to_string())
        .stdout(Stdio::null());
    for var in [
        "ZILONG_ROUTER_CONFIG",
        "REQUIRE_API_KEY",
        "TURSO_DATABASE_URL",
        "TURSO_AUTH_TOKEN",
        "TURSO_ONLY",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env_remove(var);
    }
    let mut instance = Instance { child: cmd.spawn().unwrap(), base: format!("http://127.0.0.1:{port}"), dir };
    let http = reqwest::Client::new();
    for _ in 0..150 {
        if let Some(status) = instance.child.try_wait().unwrap() {
            panic!("zilong-router exited early: {status}");
        }
        if http.get(format!("{}/api/health", instance.base)).send().await.is_ok_and(|r| r.status().is_success()) {
            return instance;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("zilong-router did not become ready on {}", instance.base);
}

async fn admin(method: reqwest::Method, base: &str, path: &str, body: Option<Value>) -> Value {
    let mut rb = reqwest::Client::new().request(method, format!("{base}/api/admin{path}"));
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    let resp = rb.send().await.unwrap();
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    assert!(status.is_success(), "{path}: {status} {v}");
    v
}

async fn add_server(base: &str, server: Value) {
    admin(reqwest::Method::POST, base, "/mcp/servers", Some(server)).await;
}

async fn http_server(base: &str, id: &str, upstream: &str) {
    let server = json!({"id": id, "transport": "http", "url": format!("{upstream}/mcp"),
        "headers": {"Authorization": UPSTREAM_AUTH}, "description": "mock web tools"});
    add_server(base, server).await;
}

async fn sse_server(base: &str, id: &str, upstream: &str) {
    let server = json!({"id": id, "transport": "sse", "url": format!("{upstream}/sse"),
        "headers": {"Authorization": UPSTREAM_AUTH}, "description": "mock legacy tools"});
    add_server(base, server).await;
}

async fn server_status(base: &str, id: &str) -> Value {
    let list = admin(reqwest::Method::GET, base, "/mcp/servers", None).await;
    list["servers"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap().clone()
}

/// One JSON-RPC request to the gateway; returns the whole response.
async fn rpc(base: &str, path: &str, key: Option<&str>, method: &str, params: Value) -> Value {
    let mut rb = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}));
    if let Some(k) = key {
        rb = rb.bearer_auth(k);
    }
    let resp = rb.send().await.unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

async fn call(base: &str, path: &str, key: Option<&str>, tool: &str, args: Value) -> Value {
    let v = rpc(base, path, key, "tools/call", json!({"name": tool, "arguments": args})).await;
    assert!(v.get("error").is_none(), "{tool}: {v}");
    v["result"].clone()
}

/// `call_tool` through the meta endpoint.
async fn run(base: &str, key: Option<&str>, tool: &str, args: Value) -> Value {
    call(base, "/mcp", key, "call_tool", json!({"tool": tool, "arguments": args})).await
}

fn first_text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_default()
}

/// The JSON a meta tool returns as its text block.
fn parsed(result: &Value) -> Value {
    serde_json::from_str(first_text(result)).unwrap_or_else(|e| panic!("{e}: {result}"))
}

fn tool_names(search: &Value) -> Vec<String> {
    search["results"].as_array().unwrap().iter().map(|r| r["tool"].as_str().unwrap().to_string()).collect()
}

// ----- tests -----------------------------------------------------------------------------

#[tokio::test]
async fn meta_tools_reach_http_and_sse_servers() {
    let upstream = Upstream::default();
    let up = start_upstream(upstream.clone()).await;
    let sse_up = start_sse_upstream(SseUpstream::default()).await;
    let router = start_router("meta", 300).await;
    let base = router.base.as_str();
    http_server(base, "web", &up).await;
    sse_server(base, "legacy", &sse_up).await;

    let hello = json!({"protocolVersion": "2025-03-26", "capabilities": {}});
    let init = rpc(base, "/mcp", None, "initialize", hello).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert!(init["result"]["instructions"].as_str().unwrap().contains("web (mock web tools)"));
    let list = rpc(base, "/mcp", None, "tools/list", json!({})).await;
    let names: Vec<&str> =
        list["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names, ["search_tools", "get_tool_schemas", "call_tool", "list_servers"]);

    // Search: both pages of each server's tools.
    let all = parsed(&call(base, "/mcp", None, "search_tools", json!({"query": "", "limit": 50})).await);
    let found = tool_names(&all);
    for t in ["web__echo", "web__add", "web__fail", "legacy__echo", "legacy__fail"] {
        assert!(found.contains(&t.to_string()), "{t} missing: {all}");
    }
    let query = json!({"query": "add numbers", "server": "web"});
    let best = parsed(&call(base, "/mcp", None, "search_tools", query).await);
    assert_eq!(tool_names(&best)[0], "web__add");
    assert_eq!(best["results"][0]["parameters"], "a: number, b: number");

    let schemas = parsed(&call(base, "/mcp", None, "get_tool_schemas", json!({"tools": ["web__add", "nope"]})).await);
    assert_eq!(schemas["tools"][0]["inputSchema"]["required"], json!(["a", "b"]));
    assert!(schemas["tools"][1]["error"].is_string());

    // Calls: arguments as an object or as a JSON string, failures as isError results.
    let r = run(base, None, "web__add", json!({"a": 2, "b": 3})).await;
    assert_eq!(first_text(&r), "5");
    let r = run(base, None, "web/add", json!("{\"a\": 1, \"b\": 1}")).await;
    assert_eq!(first_text(&r), "2");
    let r = run(base, None, "web__fail", Value::Null).await;
    assert_eq!(r["isError"], true);
    let r = run(base, None, "nope__x", Value::Null).await;
    assert_eq!(r["isError"], true);
    assert!(first_text(&r).contains("unknown tool"), "{r}");
    let r = run(base, None, "legacy__echo", json!({"text": "yo"})).await;
    assert_eq!(first_text(&r), "sse: yo");
    let r = run(base, None, "legacy__add", json!({"a": 20, "b": 22})).await;
    assert_eq!(first_text(&r), "42");
    // `echo` alone is ambiguous with two servers having it.
    let r = run(base, None, "echo", json!({"text": "x"})).await;
    assert!(first_text(&r).contains("several servers"), "{r}");

    // The upstream forgot its session: the gateway starts a new one and retries.
    let inits = upstream.inits.load(Ordering::SeqCst);
    reqwest::Client::new().post(format!("{up}/reset")).send().await.unwrap();
    let r = run(base, None, "web__echo", json!({"text": "again"})).await;
    assert_eq!(first_text(&r), "again");
    assert_eq!(upstream.inits.load(Ordering::SeqCst), inits + 1);

    // /mcp/all: every tool under its own name.
    let flat = rpc(base, "/mcp/all", None, "tools/list", json!({})).await;
    let flat_names: Vec<&str> =
        flat["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(flat_names.contains(&"web__add") && flat_names.contains(&"legacy__add"), "{flat_names:?}");
    let r = call(base, "/mcp/all", None, "web__add", json!({"a": 4, "b": 5})).await;
    assert_eq!(first_text(&r), "9");
    let unknown = rpc(base, "/mcp/all", None, "tools/call", json!({"name": "web_add"})).await;
    assert_eq!(unknown["error"]["code"], -32602);

    // Dashboard view: secrets masked, tools counted, calls recorded.
    let web = server_status(base, "web").await;
    assert_eq!(web["headers"]["Authorization"], "••••••••");
    assert_eq!(web["toolCount"], 3);
    assert!(web["stats"]["calls"].as_u64().unwrap() >= 5, "{web}");
    let tools = admin(reqwest::Method::GET, base, "/mcp/servers/web/tools", None).await;
    assert_eq!(tools["tools"].as_array().unwrap().len(), 3);
    assert_eq!(server_status(base, "legacy").await["transport"], "sse");

    // Saving the form with the masked value keeps the stored header.
    let patch = json!({"description": "renamed", "headers": {"Authorization": "••••••••"}});
    admin(reqwest::Method::PATCH, base, "/mcp/servers/web", Some(patch)).await;
    let r = run(base, None, "web__echo", json!({"text": "still"})).await;
    assert_eq!(first_text(&r), "still", "{r}");

    // Disabled servers disappear from the gateway.
    admin(reqwest::Method::PATCH, base, "/mcp/servers/web", Some(json!({"enabled": false}))).await;
    let servers = parsed(&call(base, "/mcp", None, "list_servers", json!({})).await);
    assert!(!servers["servers"].as_array().unwrap().iter().any(|s| s["server"] == "web"), "{servers}");
}

#[tokio::test]
async fn restricted_keys_see_only_their_servers() {
    let up = start_upstream(Upstream::default()).await;
    let router = start_router("keys", 300).await;
    let base = router.base.as_str();
    http_server(base, "a", &up).await;
    http_server(base, "b", &up).await;

    let key = admin(reqwest::Method::POST, base, "/keys", Some(json!({"name": "agent"}))).await;
    let id = key["id"].as_str().unwrap();
    let secret = key["key"].as_str().unwrap();
    let restrict = json!({"accessRestricted": true, "accessAllow": ["mcp/a"]});
    admin(reqwest::Method::PATCH, base, &format!("/keys/{id}"), Some(restrict)).await;

    let found = tool_names(&parsed(&call(base, "/mcp", Some(secret), "search_tools", json!({"query": ""})).await));
    assert!(!found.is_empty() && found.iter().all(|t| t.starts_with("a__")), "{found:?}");
    let r = run(base, Some(secret), "b__echo", json!({"text": "x"})).await;
    assert_eq!(r["isError"], true, "{r}");
    let r = run(base, Some(secret), "a__echo", json!({"text": "x"})).await;
    assert_eq!(first_text(&r), "x");

    let resp = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .bearer_auth("sk-wrong")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn sse_connections_close_when_idle_and_reopen_on_use() {
    let sse = SseUpstream::default();
    let sse_up = start_sse_upstream(sse.clone()).await;
    let router = start_router("idle", 1).await;
    let base = router.base.as_str();
    sse_server(base, "legacy", &sse_up).await;

    let r = run(base, None, "legacy__echo", json!({"text": "1"})).await;
    assert_eq!(first_text(&r), "sse: 1");
    assert_eq!(server_status(base, "legacy").await["status"], "running");
    assert_eq!(sse.opened.load(Ordering::SeqCst), 1);

    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(server_status(base, "legacy").await["status"], "stopped");
    let r = run(base, None, "legacy__echo", json!({"text": "2"})).await;
    assert_eq!(first_text(&r), "sse: 2");
    assert_eq!(sse.opened.load(Ordering::SeqCst), 2);

    // The server closed its stream (a restart): the next call opens a new one.
    reqwest::Client::new().post(format!("{sse_up}/reset")).send().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = run(base, None, "legacy__echo", json!({"text": "3"})).await;
    assert_eq!(first_text(&r), "sse: 3");
    assert_eq!(sse.opened.load(Ordering::SeqCst), 3);

    // A server that cannot be reached reports why.
    add_server(base, json!({"id": "broken", "transport": "sse", "url": "http://127.0.0.1:9/sse"})).await;
    let r = run(base, None, "broken__x", Value::Null).await;
    assert_eq!(r["isError"], true, "{r}");
    assert!(server_status(base, "broken").await["stats"]["lastError"].is_string());
}

#[tokio::test]
async fn only_remote_servers_are_accepted() {
    let router = start_router("remote-only", 300).await;
    let base = router.base.as_str();
    let http = reqwest::Client::new();
    for server in [
        json!({"id": "local", "command": "npx", "args": ["-y", "some-mcp-server"]}),
        json!({"id": "local", "transport": "stdio", "command": "npx"}),
        json!({"id": "local", "url": "file:///tmp/server"}),
    ] {
        let resp = http.post(format!("{base}/api/admin/mcp/servers")).json(&server).send().await.unwrap();
        assert!(resp.status().is_client_error(), "{server}: {}", resp.status());
    }

    let config = json!({"mcpServers": {
        "files": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/"]},
        "remote": {"type": "sse", "url": "https://example.com/sse"},
    }});
    let r = admin(reqwest::Method::POST, base, "/mcp/import", Some(json!({"config": config}))).await;
    assert_eq!(r["added"], json!(["remote"]));
    assert_eq!(r["rejected"].as_array().unwrap().len(), 1, "{r}");
    let list = admin(reqwest::Method::GET, base, "/mcp/servers", None).await;
    assert_eq!(list["servers"].as_array().unwrap().len(), 1);
}
