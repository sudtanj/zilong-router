//! `/v1/responses` (OpenAI Responses API, what Codex speaks): translated to a chat
//! request and served by the `/v1/chat/completions` path, so routing, `auto`, account
//! fallback and usage tracking are the same. The answer is translated back, as one
//! JSON document or as a Responses event stream.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::{chat, ApiError};
use crate::providers::Format;
use crate::sse;
use crate::state::AppState;
use crate::translator::responses::{self, Ctx, StreamConverter};

pub async fn responses(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return ApiError::new(400, Format::Openai, format!("invalid JSON body: {e}")).into_response(),
    };
    if v.get("previous_response_id").is_some_and(|p| !p.is_null()) {
        return ApiError::new(
            400,
            Format::Openai,
            "previous_response_id is not supported: responses are not stored, send the full input",
        )
        .into_response();
    }
    let (chat_body, ctx) = match responses::request_to_chat(&v) {
        Ok(r) => r,
        Err(e) => return ApiError::new(400, Format::Openai, e).into_response(),
    };
    let resp = chat::handle(state, peer, headers, uri, Format::Openai, Bytes::from(chat_body.to_string())).await;
    if resp.status() != StatusCode::OK {
        // Errors are already `{"error": {...}}`, the Responses error shape too.
        return resp;
    }
    let sse = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));
    if sse {
        stream(resp, ctx)
    } else {
        json(resp, ctx).await
    }
}

/// Response with the inner one's headers (e.g. `x-zilong-router-model`) minus the body ones.
fn rebuild(parts: &axum::http::response::Parts, content_type: &'static str, body: Body) -> Response {
    let mut out = Response::new(body);
    for (k, v) in parts.headers.iter() {
        if *k != header::CONTENT_LENGTH && *k != header::CONTENT_TYPE {
            out.headers_mut().append(k.clone(), v.clone());
        }
    }
    out.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static(content_type));
    out
}

async fn json(resp: Response, ctx: Ctx) -> Response {
    let (parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => return ApiError::new(502, Format::Openai, format!("reading the answer: {e}")).into_response(),
    };
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let doc = responses::chat_to_response(&v, &ctx);
    rebuild(&parts, "application/json", Body::from(doc.to_string()))
}

fn stream(resp: Response, ctx: Ctx) -> Response {
    let (parts, body) = resp.into_parts();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    tokio::spawn(async move {
        let mut inner = body.into_data_stream();
        let mut parser = sse::Parser::default();
        let mut conv = StreamConverter::new(ctx);
        let send = |evs: Vec<sse::Event>| {
            let tx = tx.clone();
            async move {
                for e in evs {
                    if tx.send(Ok(e.to_bytes())).await.is_err() {
                        return false;
                    }
                }
                true
            }
        };
        while let Some(chunk) = inner.next().await {
            let evs: Vec<sse::Event> = match chunk {
                Ok(c) => parser.push(&c).iter().flat_map(|ev| conv.feed(ev)).collect(),
                Err(e) => conv.fail(&format!("upstream stream error: {e}")),
            };
            // Client gone: dropping `inner` lets the chat task see it too.
            if !send(evs).await || conv.finished {
                return;
            }
        }
        let mut evs: Vec<sse::Event> = parser.finish().iter().flat_map(|ev| conv.feed(ev)).collect();
        evs.extend(conv.finish());
        let _ = send(evs).await;
    });
    let mut out = rebuild(&parts, "text/event-stream", Body::from_stream(ReceiverStream::new(rx)));
    out.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-cache"));
    out.headers_mut().insert("x-accel-buffering", header::HeaderValue::from_static("no"));
    out
}
