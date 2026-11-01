//! `/v1/chat/completions` and `/v1/messages`: authenticate, resolve the model, pick an
//! account, translate, call upstream (falling back across accounts), translate the
//! answer back and record usage.

use std::collections::HashSet;
use std::net::SocketAddr;
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
use crate::oauth::hooks::Restore;
use crate::providers::Format;
use crate::router::{self, Pick, Verdict};
use crate::sse;
use crate::state::AppState;
use crate::translator::collapse::{collapse_sse, synthesize_events, terminal_error};
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

pub(super) async fn handle(
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
    let req = Req {
        caller: &caller,
        headers: &headers,
        source,
        body: &body,
        client_stream,
        endpoint: &endpoint,
        started,
        body_len,
    };

    if !auto {
        let resolved = crate::model::resolve(&state.registry, &aliases, &connections, &raw_model);
        return attempt(&state, &req, &resolved, &raw_model).await;
    }

    // `auto`: walk every candidate until one answers. Upstream errors only surface
    // before the first byte is sent, so moving on to the next model is always safe.
    let kind = crate::model::auto_kind(&raw_model).unwrap_or(crate::model::AutoKind::Any);
    let explicit = if requested.is_empty() { crate::discovery::saved_auto_models(&state, kind) } else { requested };
    let mut candidates = crate::discovery::auto_candidates(&state, &aliases, &connections, &explicit).await;
    // Drop unusable models before rotating so round-robin only cycles real options.
    candidates.retain(|c| {
        state.registry.get(&c.provider).is_some_and(|p| {
            caller.may_use(&p.id, &c.model)
                && p.transport_for(source, &c.model).map(|t| t.format.supported()).unwrap_or(false)
        })
    });
    match kind.score() {
        Some(score) if explicit.is_empty() => {
            // Best tier first; round-robin spreads load across that tier only.
            let top = crate::model::best_first(&mut candidates, score);
            crate::router::rotate_auto(&state, &mut candidates[..top], kind.rr_key());
        }
        _ => crate::router::rotate_auto(&state, &mut candidates, kind.rr_key()),
    }

    let mut last: Option<ApiError> = None;
    let mut failures: Vec<String> = Vec::new();
    for c in candidates {
        let Some(p) = state.registry.get(&c.provider) else { continue };
        let label = format!("{}/{}", p.id, c.model);
        match attempt(&state, &req, &c, &label).await {
            Ok(mut resp) => {
                if let Ok(v) = header::HeaderValue::from_str(&label) {
                    resp.headers_mut().insert("x-zilong-router-model", v);
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
            }
        }
    }
    Err(match last {
        Some(e) => {
            // Every candidate failed; keep the message readable when there were many.
            let skipped = failures.len().saturating_sub(10);
            let more = if skipped > 0 { format!("{skipped} earlier failures, then ") } else { String::new() };
            let message =
                format!("auto: all {} models failed. {more}{}", failures.len(), failures[skipped..].join(" | "));
            ApiError { message, ..e }
        }
        None if kind == crate::model::AutoKind::Images => ApiError::new(
            404,
            source,
            "auto-images: no image-capable model is available. Add an account for a provider with vision \
             models (Claude, GPT-5/4o, Gemini, Qwen VL, Llama 4, Grok 4…), \
             or list models in settings.autoImageModels",
        ),
        None if kind == crate::model::AutoKind::Strong => ApiError::new(
            404,
            source,
            "auto-strong: no strong model is available. Add an account for a provider with strong models \
             (Claude, GPT-5, Gemini Pro, DeepSeek, Qwen Max, Kimi, GLM, MiniMax…), \
             or list models in settings.autoStrongModels",
        ),
        None => ApiError::new(
            404,
            source,
            "auto: no usable model. Add a provider account, or list models in settings.autoModels",
        ),
    })
}

/// Dashboard model test (upstream `api/models/test/ping.js`): a tiny chat request
/// through the normal routing path. Returns `{ok, latencyMs, status, error, note}`.
pub async fn ping(state: &Arc<AppState>, model: &str) -> Value {
    const TIMEOUT: Duration = Duration::from_secs(30);
    let started = Instant::now();
    // 1024 tokens: reasoning models spend a small budget on thinking and answer nothing.
    let body =
        json!({"model": model, "max_tokens": 1024, "stream": false, "messages": [{"role": "user", "content": "hi"}]});
    // Pings skip the token saver: nothing to compress, and they'd skew its counters.
    let mut headers = HeaderMap::new();
    headers.insert(crate::token_saver::OPT_OUT_HEADER, header::HeaderValue::from_static("off"));
    let caller = Caller::Local;
    let req = Req {
        caller: &caller,
        headers: &headers,
        source: Format::Openai,
        body: &body,
        client_stream: false,
        endpoint: "/api/admin/test-models",
        started,
        body_len: body.to_string().len(),
    };
    let run = async {
        let aliases =
            state.db.kv_scope("modelAliases").map_err(|e| ApiError::new(500, Format::Openai, e.to_string()))?;
        let conns = state.db.all_connections().map_err(|e| ApiError::new(500, Format::Openai, e.to_string()))?;
        let resolved = crate::model::resolve(&state.registry, &aliases, &conns, model);
        let resp = attempt(state, &req, &resolved, model).await?;
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 4 << 20)
            .await
            .map_err(|e| ApiError::new(502, Format::Openai, format!("reading the answer: {e}")))?;
        Ok::<_, ApiError>((status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null)))
    };
    let result = tokio::time::timeout(TIMEOUT, run).await;
    let latency = started.elapsed().as_millis() as u64;
    let fail =
        |status: u16, error: String| json!({"ok": false, "latencyMs": latency, "status": status, "error": error});
    let (status, v) = match result {
        Err(_) => return fail(504, format!("no answer within {}s", TIMEOUT.as_secs())),
        Ok(Err(e)) => return fail(e.status.as_u16(), e.message.chars().take(500).collect()),
        Ok(Ok(r)) => r,
    };
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        let msg = err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string());
        return fail(status, msg.chars().take(240).collect());
    }
    let Some(choice) = v.pointer("/choices/0") else {
        return fail(status, "Provider returned no completion choices for this model".into());
    };
    // A reasoning model may spend the whole budget thinking: still a working model.
    let msg = &choice["message"];
    let empty = msg.get("content").and_then(Value::as_str).map_or(true, |c| c.trim().is_empty());
    let reasoning =
        ["reasoning", "reasoning_content", "thinking"].iter().any(|k| msg.get(k).is_some_and(|r| !r.is_null()));
    let note = (empty && reasoning).then_some("reasoning-only response");
    json!({"ok": true, "latencyMs": latency, "status": status, "error": null, "note": note})
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
    // Some upstreams only answer streamed requests; the stream is collapsed for JSON clients.
    let upstream_stream = client_stream || transport.force_stream;
    upstream_body["stream"] = json!(upstream_stream);
    if upstream_stream && target == Format::Openai && upstream_body.get("stream_options").is_none() {
        upstream_body["stream_options"] = json!({"include_usage": true});
    }
    let saver = crate::token_saver::Options::from_settings(&state.db.settings(), headers);
    crate::token_saver::apply(&mut upstream_body, target, &saver);

    let mut exclude: HashSet<String> = HashSet::new();
    let mut last_err: Option<(u16, String)> = None;
    let mut tried = 0usize;
    // An OAuth account that got a 401/403 is refreshed once and tried again.
    let mut retry_with: Option<ProviderConnection> = None;
    let mut refreshed: HashSet<String> = HashSet::new();
    loop {
        let picked = match retry_with.take() {
            Some(c) => c,
            None => match router::pick(
                state,
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
                            "No active credentials for provider '{}'. Add one with: zilong-router connection add {} <api-key>",
                            provider.id, provider.id
                        )),
                        None => ApiError::new(503, source, "all accounts failed"),
                    });
                }
            },
        };
        tried += 1;
        // Refresh tokens that are about to expire, mint per-request tokens (Copilot, MiMo).
        let picked_id = picked.id.clone();
        let conn = match crate::oauth::prepare(state, picked).await {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("credentials could not be renewed ({e}); sign in again");
                tracing::warn!("{}/{}: {msg}", provider.id, resolved.model);
                exclude.insert(picked_id);
                last_err = Some((401, msg));
                continue;
            }
        };
        let level = conn.data.get("backoffLevel").and_then(Value::as_i64).unwrap_or(0);
        let patched = crate::oauth::hooks::body(&provider.id, &conn, target, &upstream_model, &upstream_body);
        let (send_body, decloak) = match &patched {
            Some((b, r)) => (b, r.clone()),
            None => (&upstream_body, Restore::None),
        };

        match executor::send(state, provider, transport, &conn, &upstream_model, send_body, headers, upstream_stream)
            .await
        {
            Ok(resp) => {
                router::mark_success(state, &conn, &resolved.model);
                tracing::info!(
                    "{endpoint} {}/{} via {} -> {} in {}ms{}",
                    provider.id,
                    resolved.model,
                    conn.label(),
                    resp.status().as_u16(),
                    started.elapsed().as_millis(),
                    if client_stream { " (stream)" } else { "" }
                );
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
                    decloak,
                };
                return if client_stream && is_sse(&resp) {
                    Ok(stream_response(ctx, resp))
                } else {
                    json_response(ctx, resp, client_stream).await
                };
            }
            Err(e) => {
                let (status, msg) = (e.status(), e.message());
                if matches!(status, 401 | 403) && refreshed.insert(conn.id.clone()) {
                    if let Some(fresh) = crate::oauth::force_refresh(state, &conn).await {
                        tracing::info!(
                            "{}/{}: {status} via {}, retrying with renewed credentials",
                            provider.id,
                            resolved.model,
                            conn.label()
                        );
                        retry_with = Some(fresh);
                        tried -= 1;
                        continue;
                    }
                }
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
                        router::mark_unavailable(state, &conn, &resolved.model, status, &msg, verdict);
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
    /// Tool names a provider hook changed; restored in the response.
    decloak: Restore,
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
        collapse_sse(&text, ctx.target, ctx.source, &ctx.model, &ctx.decloak)
    } else {
        let mut v: Value = serde_json::from_str(&text).map_err(|_| {
            ApiError::new(
                502,
                ctx.source,
                format!("upstream returned non-JSON: {}", text.chars().take(200).collect::<String>()),
            )
        })?;
        crate::oauth::hooks::fix_response(&ctx.provider, &mut v);
        crate::oauth::hooks::decloak_json(&mut v, &ctx.decloak);
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
                    if let Some(mut ev) = parser.finish() {
                        crate::oauth::hooks::decloak_event(&mut ev, &ctx.decloak);
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
            for mut ev in parser.push(&chunk) {
                crate::oauth::hooks::decloak_event(&mut ev, &ctx.decloak);
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
