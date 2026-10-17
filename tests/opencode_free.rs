//! End-to-end checks for OpenCode Free: the real `zilong-router` binary, with the
//! `opencode` provider pointed at a mock opencode.ai through a `providers.json`
//! override in a throwaway data directory.

use std::net::TcpListener as StdListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

const PROVIDERS: &str = include_str!("../data/providers.json");
const CHAT_PATH: &str = "/inference/v1/chat/completions";

/// What the mock opencode.ai serves and what it was sent.
#[derive(Clone, Default)]
struct Mock {
    /// `/zen/v1/models` ids; `None` answers 500.
    live: Option<Vec<&'static str>>,
    model_requests: Arc<Mutex<Vec<HeaderMap>>>,
    chats: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

async fn models(State(m): State<Mock>, headers: HeaderMap) -> Response {
    m.model_requests.lock().unwrap().push(headers);
    match &m.live {
        Some(ids) => {
            let data: Vec<Value> = ids.iter().map(|id| json!({"id": id, "object": "model"})).collect();
            Json(json!({"object": "list", "data": data})).into_response()
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "down").into_response(),
    }
}

async fn chat(State(m): State<Mock>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).expect("JSON request body");
    let model = body["model"].as_str().unwrap_or_default().to_string();
    m.chats.lock().unwrap().push((headers, body));
    let chunk = |choices: Value, usage: Value| {
        let mut c = json!({"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": 0, "model": model, "choices": choices});
        if !usage.is_null() {
            c["usage"] = usage;
        }
        format!("data: {c}\n\n")
    };
    let sse = [
        chunk(
            json!([{"index": 0, "delta": {"role": "assistant", "content": "pong"}, "finish_reason": null}]),
            Value::Null,
        ),
        chunk(json!([{"index": 0, "delta": {}, "finish_reason": "stop"}]), Value::Null),
        chunk(json!([]), json!({"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
}

/// Serves `mock` on a free local port and returns its base URL.
async fn start_mock(mock: Mock) -> String {
    let app = Router::new().route("/zen/v1/models", get(models)).route(CHAT_PATH, post(chat)).with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

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

async fn start_router(name: &str, opencode_base: &str) -> Instance {
    let dir = std::env::temp_dir().join(format!("zilong-router-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut file: Value = serde_json::from_str(PROVIDERS).unwrap();
    let opencode = file["providers"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["id"] == "opencode")
        .expect("opencode is in data/providers.json");
    opencode["transport"]["baseUrl"] = json!(opencode_base);
    std::fs::write(dir.join("providers.json"), file.to_string()).unwrap();

    let port = StdListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zilong-router"));
    cmd.args(["--host", "127.0.0.1", "--port", &port.to_string(), "--data-dir"])
        .arg(&dir)
        .env("LOG_LEVEL", "warn")
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

async fn model_ids(base: &str) -> Vec<String> {
    let v: Value = reqwest::get(format!("{base}/v1/models")).await.unwrap().json().await.unwrap();
    v["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect()
}

/// Every free chat model on OpenCode's pricing page, as `/v1/models` lists them.
const DOCUMENTED: [&str; 13] = [
    "oc/big-pickle",
    "oc/glm-5.3-flash-free",
    "oc/deepseek-v4.1-flash-free",
    "oc/space-bunny-free",
    "oc/beam-free",
    "oc/ll-alpha-free",
    "oc/longcat-2.5-preview-free",
    "oc/exo-free",
    "oc/mimo-v2.6-flash-free",
    "oc/mimo-v2.5-free",
    "oc/nemotron-3-ultra-free",
    "oc/nemotron-3.5-lightning-free",
    "oc/nemotron-3.5-super-vl-free",
];

#[tokio::test]
async fn lists_live_and_documented_free_models() {
    let mock = Mock {
        live: Some(vec![
            "big-pickle",
            "gpt-5",
            "claude-opus-5",
            "beam-free",
            "brand-new-free",
            "deepseek-v4-flash-free",
            "jev-1.13-free",
            "muse-spark-1.3-contributor-free",
        ]),
        ..Default::default()
    };
    let router = start_router("list", &start_mock(mock.clone()).await).await;
    let ids = model_ids(&router.base).await;

    // A free model only the live list knows, every documented one, and the catalogue's
    // Anthropic-endpoint model.
    for id in DOCUMENTED.iter().copied().chain(["oc/brand-new-free", "oc/union-alpha"]) {
        assert!(ids.iter().any(|m| m == id), "{id} missing from /v1/models: {ids:?}");
    }
    // Paid, dead, systemone-only and Responses-only models stay out.
    for id in [
        "oc/gpt-5",
        "oc/claude-opus-5",
        "oc/deepseek-v4-flash-free",
        "oc/jev-1.13-free",
        "oc/muse-spark-1.3-contributor-free",
    ] {
        assert!(!ids.iter().any(|m| m == id), "{id} should not be listed");
    }
    // Models in both lists are listed once.
    assert_eq!(ids.iter().filter(|m| *m == "oc/beam-free").count(), 1);

    // The live list is fetched the way the OpenCode CLI does it, and cached.
    let _ = model_ids(&router.base).await;
    let requests = mock.model_requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "the live list should be cached");
    assert_eq!(requests[0]["authorization"], "Bearer public");
    assert!(requests[0]["user-agent"].to_str().unwrap().starts_with("opencode/"));
}

#[tokio::test]
async fn documented_models_when_live_list_fails() {
    let mock = Mock { live: None, ..Default::default() };
    let router = start_router("fallback", &start_mock(mock.clone()).await).await;
    let ids = model_ids(&router.base).await;
    let free: Vec<&String> = ids.iter().filter(|m| m.starts_with("oc/") && *m != "oc/union-alpha").collect();
    assert_eq!(free, DOCUMENTED, "only the documented free models without a live list");
    assert_eq!(mock.model_requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn documented_model_is_routed_to_the_free_chat_endpoint() {
    let mock = Mock { live: Some(vec!["big-pickle"]), ..Default::default() };
    let router = start_router("chat", &start_mock(mock.clone()).await).await;
    let http = reqwest::Client::new();

    // A JSON client: the router still streams upstream (the free tier requires it) and
    // collapses the stream into one completion.
    let resp = http
        .post(format!("{}/v1/chat/completions", router.base))
        .json(&json!({"model": "oc/exo-free", "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "pong", "{v}");

    // A streaming client gets the stream through.
    let text = http
        .post(format!("{}/v1/chat/completions", router.base))
        .json(&json!({"model": "oc/nemotron-3-ultra-free", "stream": true, "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("pong") && text.contains("[DONE]"), "{text}");

    let chats = mock.chats.lock().unwrap();
    assert_eq!(chats.len(), 2);
    for ((headers, body), model) in chats.iter().zip(["exo-free", "nemotron-3-ultra-free"]) {
        assert_eq!(body["model"], model);
        assert_eq!(body["stream"], true);
        assert_eq!(headers["authorization"], "Bearer public");
        assert!(headers["user-agent"].to_str().unwrap().starts_with("opencode/"));
        assert!(headers.contains_key("x-opencode-session"));
        // The free tier only answers requests that declare OpenCode's own tools.
        let tools: Vec<&str> =
            body["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        assert!(tools.contains(&"read") && tools.contains(&"shell"), "{tools:?}");
    }
}
