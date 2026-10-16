//! `/v1/chat/completions` and `/v1/messages`: authenticate, resolve the model, pick an
//! account, translate, call upstream (falling back across accounts), translate the
//! answer back and record usage.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::ApiError;
use crate::auth::{authenticate, AuthError, Caller};
use crate::db::ProviderConnection;
use crate::executor::{self, ExecError};
use crate::providers::Format;
use crate::router::{self, Pick, Verdict};
use crate::sse;
use crate::state::AppState;
use crate::translator::{self, StreamTranslator};
use crate::usage::{self, TokenCounts};

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    handle(state, peer, headers, uri, Format::Openai, body).await
}

pub async fn messages(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    handle(state, peer, headers, uri, Format::Claude, body).await
}

/// Local estimate (chars / 4) like upstream; no provider call.
pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Err(e) = auth(&state, peer, &headers, uri.query(), Format::Claude) {
        return e.into_response();
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let chars = ["system", "messages", "tools"]
        .iter()
        .map(|k| v.get(k).map(|x| x.to_string().len()).unwrap_or(0))
        .sum::<usize>();
    Json(json!({"input_tokens": chars.div_ceil(4)})).into_response()
}

fn auth(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
    query: Option<&str>,
    fmt: Format,
) -> Result<Caller, ApiError> {
    let require = state.db.settings().get("requireApiKey").and_then(Value::as_bool).unwrap_or(false);
    authenticate(&state.cfg, &state.db, require, peer.ip(), headers, query).map_err(|e| match e {
        AuthError::Missing => ApiError::new(401, fmt, "API key required for remote API access"),
        AuthError::Invalid => ApiError::new(401, fmt, "Invalid API key"),
    })
}

async fn handle(
    state: Arc<AppState>,
    peer: SocketAddr,
    headers: HeaderMap,
    uri: Uri,
    source: Format,
    raw: Bytes,
) -> Response {
    match handle_inner(state, peer, headers, uri, source, raw).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn handle_inner(
    state: Arc<AppState>,
    peer: SocketAddr,
    headers: HeaderMap,
    uri: Uri,
    source: Format,
    raw: Bytes,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let caller = auth(&state, peer, &headers, uri.query(), source)?;
    let mut body: Value =
        serde_json::from_slice(&raw).map_err(|e| ApiError::new(400, source, format!("invalid JSON body: {e}")))?;
    let endpoint = uri.path().to_string();
    let raw_model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
        .ok_or_else(|| ApiError::new(400, source, "model is required"))?
        .to_string();
    let client_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let aliases = state.db.kv_scope("modelAliases").map_err(|e| ApiError::new(500, source, e.to_string()))?;
    let connections = state.db.all_connections().map_err(|e| ApiError::new(500, source, e.to_string()))?;
    let body_len = raw.len();
    let auto = crate::model::is_auto(&raw_model);
    // OpenRouter-style fallback list; consumed here, never forwarded upstream.
    let requested: Vec<String> = match body.as_object_mut().and_then(|o| o.remove("models")) {
        Some(Value::Array(a)) if auto => a.into_iter().filter_map(|m| m.as_str().map(str::to_string)).collect(),
        Some(other) if !auto => {
            body["models"] = other;
            Vec::new()
        }
        _ => Vec::new(),
    };
    let sends = AtomicUsize::new(0);
    let req = Req {
        caller: &caller,
        headers: &headers,
        source,
        body: &body,
        client_stream,
        endpoint: &endpoint,
        started,
        body_len,
        sends: &sends,
    };

    if !auto {
        let resolved = crate::model::resolve(&state.registry, &aliases, &connections, &raw_model);
        return attempt(&state, &req, &resolved, &raw_model).await;
    }

    // `auto`: walk the candidates until one answers. Upstream errors only surface
    // before the first byte is sent, so moving on to the next model is always safe.
    let settings = state.db.settings();
    let explicit: Vec<String> = if requested.is_empty() {
        settings
            .get("autoModels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect()
    } else {
        requested
    };
    let max = settings.get("autoMaxAttempts").and_then(Value::as_u64).unwrap_or(5).max(1) as usize;
    let candidates = crate::discovery::auto_candidates(&state, &aliases, &connections, &explicit).await;

    let mut last: Option<ApiError> = None;
    let mut failures: Vec<String> = Vec::new();
    let (mut sent, mut tried_models) = (0usize, 0usize);
    for c in candidates {
        let Some(p) = state.registry.get(&c.provider) else { continue };
        let usable = caller.may_use(&p.id, &c.model)
            && p.transport_for(source, &c.model).map(|t| t.format.supported()).unwrap_or(false);
        if !usable {
            continue;
        }
        let label = format!("{}/{}", p.id, c.model);
        match attempt(&state, &req, &c, &label).await {
            Ok(mut resp) => {
                if let Ok(v) = header::HeaderValue::from_str(&label) {
                    resp.headers_mut().insert("x-mini-router-model", v);
                }
                return Ok(resp);
            }
            Err(e) => {
                tracing::info!(
                    "auto: {label} unavailable ({} {}), trying the next model",
                    e.status.as_u16(),
                    e.message
                );
                failures.push(format!(
                    "{label}: {} {}",
                    e.status.as_u16(),
                    e.message.chars().take(160).collect::<String>()
                ));
                last = Some(e);
                // Models whose accounts are all missing or cooling down cost no upstream
                // call; only models that were actually tried count against the budget.
                if req.sends.load(Ordering::Relaxed) > sent {
                    sent = req.sends.load(Ordering::Relaxed);
                    tried_models += 1;
                }
                if tried_models >= max {
                    break;
                }
            }
        }
    }
    Err(match last {
        Some(e) => ApiError { message: format!("auto: no model succeeded. Tried: {}", failures.join(" | ")), ..e },
        None => ApiError::new(
            404,
            source,
            "auto: no usable model. Add a provider account, or list models in settings.autoModels",
        ),
    })
}

#[derive(Clone, Copy)]
struct Req<'a> {
    caller: &'a Caller,
    headers: &'a HeaderMap,
    source: Format,
    body: &'a Value,
    client_stream: bool,
    endpoint: &'a str,
    started: Instant,
    body_len: usize,
    /// Upstream calls made so far (across `auto` candidates).
    sends: &'a AtomicUsize,
}

async fn attempt(
    state: &Arc<AppState>,
    req: &Req<'_>,
    resolved: &crate::model::Resolved,
    raw_model: &str,
) -> Result<Response, ApiError> {
    let Req { caller, headers, source, body, client_stream, endpoint, started, body_len, .. } = *req;
    let provider = state.registry.get(&resolved.provider).ok_or_else(|| {
        ApiError::new(404, source, format!("Unknown provider '{}' in model '{raw_model}'", resolved.provider))
    })?;
    if !caller.may_use(&provider.id, &resolved.model) {
        return Err(ApiError::new(403, source, format!("This API key may not use {}/{}", provider.id, resolved.model)));
    }
    if !provider.is_valid_model(&resolved.model) {
        tracing::debug!("{} is not in the {} model list; forwarding anyway", resolved.model, provider.id);
    }
    let transport = provider
        .transport_for(source, &resolved.model)
        .ok_or_else(|| ApiError::new(406, source, format!("Provider {} has no chat endpoint", provider.id)))?;
    if !transport.format.supported() {
        return Err(ApiError::new(
            406,
            source,
            format!(
                "Provider {} uses the '{}' wire format, which this build does not implement",
                provider.id,
                transport.format.as_str()
            ),
        ));
    }
    let target = transport.format;
    let upstream_model = provider.upstream_model_id(&resolved.model).to_string();
    let mut upstream_body = translator::translate_request(source, target, body.clone(), &upstream_model)
        .map_err(|e| ApiError::new(406, source, e))?;
    upstream_body["stream"] = json!(client_stream);
    if client_stream && target == Format::Openai && upstream_body.get("stream_options").is_none() {
        upstream_body["stream_options"] = json!({"include_usage": true});
    }
    let saver = crate::token_saver::Options::from_settings(&state.db.settings(), headers);
    crate::token_saver::apply(&mut upstream_body, target, &saver);

    let mut exclude: HashSet<String> = HashSet::new();
    let mut last_err: Option<(u16, String)> = None;
    let mut tried = 0usize;
    loop {
        let conn = match router::pick(
            &state,
            &provider.id,
            &resolved.model,
            &exclude,
            resolved.pinned_connection.as_deref(),
        )
        .map_err(|e| ApiError::new(500, source, e.to_string()))?
        {
            Pick::Connection(c) => c,
            Pick::AllLocked { retry_after_secs } => {
                let msg = match &last_err {
                    Some((_, m)) => format!(
                        "[{}/{}] {m} (all accounts cooling down, retry in {retry_after_secs}s)",
                        provider.id, resolved.model
                    ),
                    None => format!(
                        "[{}/{}] all accounts are rate limited; retry in {retry_after_secs}s",
                        provider.id, resolved.model
                    ),
                };
                return Err(ApiError::new(503, source, msg).retry_after(retry_after_secs));
            }
            Pick::None => {
                return Err(match last_err {
                    Some((status, m)) => ApiError::new(if status >= 500 { 502 } else { status }, source, format!("[{}/{}] {m}", provider.id, resolved.model)),
                    None if tried == 0 => ApiError::new(404, source, format!(
                        "No active credentials for provider '{}'. Add one with: mini-router connection add {} <api-key>",
                        provider.id, provider.id
                    )),
                    None => ApiError::new(503, source, "all accounts failed"),
                });
            }
        };
        tried += 1;
        req.sends.fetch_add(1, Ordering::Relaxed);
        let level = conn.data.get("backoffLevel").and_then(Value::as_i64).unwrap_or(0);

        match executor::send(
            &state,
            provider,
            transport,
            &conn,
            &upstream_model,
            &upstream_body,
            &headers,
            client_stream,
        )
        .await
        {
            Ok(resp) => {
                router::mark_success(&state, &conn, &resolved.model);
                let ctx = ReqCtx {
                    state: state.clone(),
                    provider: provider.id.clone(),
                    model: resolved.model.clone(),
                    conn,
                    api_key: caller.key_value().map(str::to_string),
                    endpoint: endpoint.to_string(),
                    started,
                    body_len,
                    source,
                    target,
                };
                return if client_stream && is_sse(&resp) {
                    Ok(stream_response(ctx, resp))
                } else {
                    json_response(ctx, resp, client_stream).await
                };
            }
            Err(e) => {
                let (status, msg) = (e.status(), e.message());
                let verdict = router::classify(status, &msg, level);
                tracing::warn!(
                    "{}/{} via {} failed: {} {}",
                    provider.id,
                    resolved.model,
                    conn.label(),
                    status,
                    msg.chars().take(200).collect::<String>()
                );
                match verdict {
                    Verdict::NoFallback => {
                        return Err(ApiError::new(
                            status,
                            source,
                            format!("[{}/{}] {msg}", provider.id, resolved.model),
                        ));
                    }
                    Verdict::Fallback { .. } => {
                        router::mark_unavailable(&state, &conn, &resolved.model, status, &msg, verdict);
                        exclude.insert(conn.id.clone());
                        last_err = Some((status, msg));
                        if matches!(e, ExecError::Network(_)) && tried >= 3 {
                            break;
                        }
                    }
                }
            }
        }
    }
    let (status, m) = last_err.unwrap_or((502, "upstream unreachable".into()));
    Err(ApiError::new(
        if status >= 500 { 502 } else { status },
        source,
        format!("[{}/{}] {m}", provider.id, resolved.model),
    ))
}

struct ReqCtx {
    state: Arc<AppState>,
    provider: String,
    model: String,
    conn: ProviderConnection,
    api_key: Option<String>,
    endpoint: String,
    started: Instant,
    body_len: usize,
    source: Format,
    target: Format,
}

impl ReqCtx {
    fn record(&self, tokens: TokenCounts, output_chars: usize, status: &str, ttft: Option<u128>) {
        let tokens = if tokens.is_empty() { usage::estimate(self.body_len, output_chars) } else { tokens };
        usage::record(
            &self.state,
            usage::Record {
                provider: &self.provider,
                model: &self.model,
                connection_id: Some(&self.conn.id),
                api_key: self.api_key.as_deref(),
                endpoint: &self.endpoint,
                status,
                tokens,
                latency_ms: self.started.elapsed().as_millis(),
                ttft_ms: ttft,
            },
        );
    }
}

fn is_sse(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("text/event-stream"))
        .unwrap_or(false)
}

async fn json_response(ctx: ReqCtx, resp: reqwest::Response, client_stream: bool) -> Result<Response, ApiError> {
    let upstream_sse = is_sse(&resp);
    let text = resp.text().await.map_err(|e| ApiError::new(502, ctx.source, format!("reading upstream body: {e}")))?;
    let v: Value = if upstream_sse {
        // Upstream insisted on streaming; collapse it into one JSON document.
        collapse_sse(&text, ctx.target, ctx.source, &ctx.model)
    } else {
        let v: Value = serde_json::from_str(&text).map_err(|_| {
            ApiError::new(
                502,
                ctx.source,
                format!("upstream returned non-JSON: {}", text.chars().take(200).collect::<String>()),
            )
        })?;
        translator::translate_response(ctx.target, ctx.source, &v, &ctx.model)
    };
    let tokens = usage::extract(&v).unwrap_or_default();
    ctx.record(tokens, text.len(), "200 OK", None);
    if client_stream {
        // Client wanted SSE but upstream gave JSON (rare): emit the document as a stream.
        let mut t = StreamTranslator::new(ctx.source, ctx.source, &ctx.model);
        let mut bytes = Vec::new();
        for ev in synthesize_events(&v, ctx.source) {
            for e in t.feed(&ev) {
                bytes.extend_from_slice(&e.to_bytes());
            }
        }
        for e in t.finish() {
            bytes.extend_from_slice(&e.to_bytes());
        }
        return Ok(sse_headers(Response::builder().status(200)).body(Body::from(bytes)).unwrap());
    }
    Ok((StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response())
}

/// Replay a full SSE body through the stream translator and assemble the final JSON.
fn collapse_sse(text: &str, from: Format, to: Format, model: &str) -> Value {
    let mut parser = sse::Parser::default();
    let mut tr = StreamTranslator::new(from, to, model);
    let mut events = Vec::new();
    for ev in parser.push(text.as_bytes()).into_iter().chain(parser.finish()) {
        events.extend(tr.feed(&ev));
    }
    events.extend(tr.finish());
    assemble(&events, to, model, &tr.usage)
}

/// Build a complete response document from translated client-format events.
fn assemble(events: &[sse::Event], fmt: Format, model: &str, usage: &TokenCounts) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tools: Vec<(String, String, String)> = Vec::new(); // id, name, args
    let mut finish = String::new();
    let mut id = String::new();
    for ev in events {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { continue };
        match fmt {
            Format::Openai => {
                if id.is_empty() {
                    id = v.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                }
                let Some(c) = v.pointer("/choices/0") else { continue };
                if let Some(t) = c.pointer("/delta/content").and_then(Value::as_str) {
                    text.push_str(t);
                }
                if let Some(t) = c.pointer("/delta/reasoning_content").and_then(Value::as_str) {
                    reasoning.push_str(t);
                }
                for tc in c.pointer("/delta/tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    while tools.len() <= idx {
                        tools.push((String::new(), String::new(), String::new()));
                    }
                    if let Some(i) = tc.get("id").and_then(Value::as_str) {
                        tools[idx].0 = i.to_string();
                    }
                    if let Some(n) = tc.pointer("/function/name").and_then(Value::as_str) {
                        tools[idx].1 = n.to_string();
                    }
                    if let Some(a) = tc.pointer("/function/arguments").and_then(Value::as_str) {
                        tools[idx].2.push_str(a);
                    }
                }
                if let Some(f) = c.get("finish_reason").and_then(Value::as_str) {
                    finish = f.to_string();
                }
            }
            _ => match v.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    id = v.pointer("/message/id").and_then(Value::as_str).unwrap_or("").to_string()
                }
                Some("content_block_start") => {
                    if v.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use") {
                        tools.push((
                            v.pointer("/content_block/id").and_then(Value::as_str).unwrap_or("").into(),
                            v.pointer("/content_block/name").and_then(Value::as_str).unwrap_or("").into(),
                            String::new(),
                        ));
                    }
                }
                Some("content_block_delta") => match v.pointer("/delta/type").and_then(Value::as_str) {
                    Some("text_delta") => text.push_str(v.pointer("/delta/text").and_then(Value::as_str).unwrap_or("")),
                    Some("thinking_delta") => {
                        reasoning.push_str(v.pointer("/delta/thinking").and_then(Value::as_str).unwrap_or(""))
                    }
                    Some("input_json_delta") => {
                        if let Some(t) = tools.last_mut() {
                            t.2.push_str(v.pointer("/delta/partial_json").and_then(Value::as_str).unwrap_or(""));
                        }
                    }
                    _ => {}
                },
                Some("message_delta") => {
                    finish = v.pointer("/delta/stop_reason").and_then(Value::as_str).unwrap_or("").to_string()
                }
                _ => {}
            },
        }
    }
    match fmt {
        Format::Openai => {
            let mut msg = json!({"role": "assistant", "content": if text.is_empty() && !tools.is_empty() { Value::Null } else { json!(text) }});
            if !reasoning.is_empty() {
                msg["reasoning_content"] = json!(reasoning);
            }
            if !tools.is_empty() {
                msg["tool_calls"] = tools
                    .iter()
                    .map(|(i, n, a)| json!({"id": i, "type": "function", "function": {"name": n, "arguments": a}}))
                    .collect();
            }
            json!({"id": if id.is_empty() { format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()) } else { id },
                "object": "chat.completion", "created": time::OffsetDateTime::now_utc().unix_timestamp(), "model": model,
                "choices": [{"index": 0, "message": msg, "finish_reason": if finish.is_empty() { "stop" } else { &finish }}],
                "usage": translator::response::openai_usage(usage)})
        }
        _ => {
            let mut content = Vec::new();
            if !reasoning.is_empty() {
                content.push(json!({"type": "thinking", "thinking": reasoning, "signature": ""}));
            }
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
            }
            for (i, n, a) in &tools {
                content.push(json!({"type": "tool_use", "id": i, "name": n, "input": translator::safe_parse_json(a)}));
            }
            json!({"id": if id.is_empty() { format!("msg_{}", uuid::Uuid::new_v4().simple()) } else { id }, "type": "message", "role": "assistant",
                "model": model, "content": content, "stop_reason": if finish.is_empty() { "end_turn" } else { &finish },
                "stop_sequence": Value::Null, "usage": translator::response::claude_usage(usage)})
        }
    }
}

/// Turn a complete response document into the events a stream would have carried.
fn synthesize_events(v: &Value, fmt: Format) -> Vec<sse::Event> {
    match fmt {
        Format::Openai => {
            let msg = v.pointer("/choices/0/message").cloned().unwrap_or(json!({}));
            let mut delta = msg.clone();
            if let Some(o) = delta.as_object_mut() {
                o.remove("role");
            }
            let mut tc_delta = json!({});
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                tc_delta["tool_calls"] = calls
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let mut c = c.clone();
                        c["index"] = json!(i);
                        c
                    })
                    .collect();
                delta.as_object_mut().unwrap().remove("tool_calls");
            }
            let base = |d: Value, f: Option<&str>| json!({"id": v["id"], "object": "chat.completion.chunk", "created": v["created"], "model": v["model"], "choices": [{"index": 0, "delta": d, "finish_reason": f}]});
            let mut evs =
                vec![sse::Event::json(&base(json!({"role": "assistant"}), None)), sse::Event::json(&base(delta, None))];
            if tc_delta.get("tool_calls").is_some() {
                evs.push(sse::Event::json(&base(tc_delta, None)));
            }
            let mut last = base(json!({}), v.pointer("/choices/0/finish_reason").and_then(Value::as_str));
            last["usage"] = v.get("usage").cloned().unwrap_or(Value::Null);
            evs.push(sse::Event::json(&last));
            evs.push(sse::Event::data("[DONE]"));
            evs
        }
        _ => {
            let mut evs = vec![];
            let mut start = v.clone();
            start["content"] = json!([]);
            start["stop_reason"] = Value::Null;
            evs.push(sse::Event::named_json("message_start", &json!({"type": "message_start", "message": start})));
            for (i, b) in v.get("content").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let ty = b.get("type").and_then(Value::as_str).unwrap_or("text");
                let (head, delta) = match ty {
                    "tool_use" => (
                        json!({"type": "tool_use", "id": b["id"], "name": b["name"], "input": {}}),
                        json!({"type": "input_json_delta", "partial_json": b["input"].to_string()}),
                    ),
                    "thinking" => (
                        json!({"type": "thinking", "thinking": ""}),
                        json!({"type": "thinking_delta", "thinking": b["thinking"]}),
                    ),
                    _ => (json!({"type": "text", "text": ""}), json!({"type": "text_delta", "text": b["text"]})),
                };
                evs.push(sse::Event::named_json(
                    "content_block_start",
                    &json!({"type": "content_block_start", "index": i, "content_block": head}),
                ));
                evs.push(sse::Event::named_json(
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": i, "delta": delta}),
                ));
                evs.push(sse::Event::named_json(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": i}),
                ));
            }
            evs.push(sse::Event::named_json("message_delta", &json!({"type": "message_delta", "delta": {"stop_reason": v["stop_reason"], "stop_sequence": Value::Null}, "usage": v["usage"]})));
            evs.push(sse::Event::named_json("message_stop", &json!({"type": "message_stop"})));
            evs
        }
    }
}

fn sse_headers(b: axum::http::response::Builder) -> axum::http::response::Builder {
    b.header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
}

/// Pump the upstream SSE body through the translator into the client. Runs as a task
/// so slow clients don't hold the handler; usage is recorded when the stream ends.
fn stream_response(ctx: ReqCtx, resp: reqwest::Response) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let stall = Duration::from_millis(ctx.state.cfg.stall_timeout_ms);
    tokio::spawn(async move {
        let mut upstream = resp.bytes_stream();
        let mut parser = sse::Parser::default();
        let mut tr = StreamTranslator::new(ctx.target, ctx.source, &ctx.model);
        let mut ttft: Option<u128> = None;
        let mut status = "200 OK";
        let mut client_gone = false;

        loop {
            let next = tokio::time::timeout(stall, upstream.next()).await;
            let chunk = match next {
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => {
                    tracing::warn!("{}/{} stream error: {e}", ctx.provider, ctx.model);
                    status = "FAILED stream";
                    let _ = tx.send(Ok(terminal_error(ctx.source, &format!("upstream stream error: {e}")))).await;
                    break;
                }
                Ok(None) => {
                    if let Some(ev) = parser.finish() {
                        for out in tr.feed(&ev) {
                            let _ = tx.send(Ok(out.to_bytes())).await;
                        }
                    }
                    for out in tr.finish() {
                        let _ = tx.send(Ok(out.to_bytes())).await;
                    }
                    break;
                }
                Err(_) => {
                    tracing::warn!("{}/{} stream stalled for {:?}", ctx.provider, ctx.model, stall);
                    status = "FAILED stall";
                    let _ = tx.send(Ok(terminal_error(ctx.source, "upstream stream stalled"))).await;
                    break;
                }
            };
            if ttft.is_none() {
                ttft = Some(ctx.started.elapsed().as_millis());
            }
            for ev in parser.push(&chunk) {
                for out in tr.feed(&ev) {
                    if tx.send(Ok(out.to_bytes())).await.is_err() {
                        client_gone = true;
                        break;
                    }
                }
                if client_gone {
                    break;
                }
            }
            if client_gone {
                tracing::info!("{}/{} client disconnected mid-stream", ctx.provider, ctx.model);
                status = "499 client closed";
                break;
            }
            if tr.finished {
                break;
            }
        }
        ctx.record(tr.usage, tr.output_chars, status, ttft);
    });

    sse_headers(Response::builder().status(200)).body(Body::from_stream(ReceiverStream::new(rx))).unwrap()
}

/// Bytes to append when the stream breaks after 200 was already sent.
fn terminal_error(fmt: Format, message: &str) -> Bytes {
    match fmt {
        Format::Claude => sse::Event::named_json(
            "error",
            &json!({"type": "error", "error": {"type": "api_error", "message": message}}),
        )
        .to_bytes(),
        _ => {
            let mut b =
                sse::Event::json(&json!({"error": {"message": message, "type": "server_error"}})).to_bytes().to_vec();
            b.extend_from_slice(&sse::Event::data("[DONE]").to_bytes());
            Bytes::from(b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_openai_from_events() {
        let evs = vec![
            sse::Event::json(
                &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hel"}}]}),
            ),
            sse::Event::json(&json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "lo"}}]})),
            sse::Event::json(
                &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ),
        ];
        let v = assemble(&evs, Format::Openai, "m", &TokenCounts { prompt: 1, completion: 2, ..Default::default() });
        assert_eq!(v["choices"][0]["message"]["content"], "Hello");
        assert_eq!(v["usage"]["total_tokens"], 3);
        let back = synthesize_events(&v, Format::Openai);
        assert_eq!(back.last().unwrap().data, "[DONE]");
    }
}
