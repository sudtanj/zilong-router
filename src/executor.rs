//! Upstream HTTP call: URL, headers, auth and per-status retries
//! (`open-sse/executors/base.js` + `default.js`, generic part only).

use std::collections::HashMap;
use std::time::Duration;

use axum::http::HeaderMap;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;

use crate::db::ProviderConnection;
use crate::providers::{AuthSpec, Format, Provider, Transport};
use crate::state::AppState;

/// Default retry table: (status, attempts, delay ms). 429 is never retried on the
/// same account; the router moves to another one instead.
const DEFAULT_RETRY: &[(u16, u32, u64)] = &[(429, 0, 0), (502, 3, 3000), (503, 3, 2000), (504, 2, 3000)];

#[derive(Debug)]
pub enum ExecError {
    /// Upstream answered with a non-2xx status; body is the (possibly partial) text.
    Status { status: u16, body: String },
    /// No HTTP response at all (connect/timeout/TLS).
    Network(String),
}

impl ExecError {
    pub fn status(&self) -> u16 {
        match self {
            ExecError::Status { status, .. } => *status,
            ExecError::Network(_) => 502,
        }
    }
    pub fn message(&self) -> String {
        match self {
            ExecError::Status { body, .. } => extract_message(body),
            ExecError::Network(m) => format!("upstream unreachable: {m}"),
        }
    }
}

/// Pull `error.message` (OpenAI / Anthropic shape) out of an error body, else raw text.
pub fn extract_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        for p in ["/error/message", "/message", "/error", "/detail"] {
            if let Some(s) = v.pointer(p).and_then(Value::as_str) {
                return s.to_string();
            }
        }
        if let Some(s) = v.pointer("/error").filter(|e| e.is_object()) {
            return s.to_string();
        }
    }
    let t = body.trim();
    if t.starts_with('<') {
        // HTML error page: keep the title only.
        if let Some(start) = t.find("<title>") {
            if let Some(end) = t[start..].find("</title>") {
                return t[start + 7..start + end].trim().to_string();
            }
        }
        return "upstream returned an HTML error page".into();
    }
    t.chars().take(500).collect()
}

/// Final URL for a request.
pub fn build_url(
    provider: &Provider,
    transport: &Transport,
    conn: &ProviderConnection,
    model: &str,
    stream: bool,
) -> String {
    let mut base = transport.base_url.clone();
    if let Some(o) = conn.base_url_override() {
        let o = o.trim_end_matches('/');
        base = match transport.format {
            // Custom endpoints are given as the API root ("…/v1"); add the resource path.
            Format::Claude => format!("{o}/messages"),
            Format::Openai => format!("{o}/chat/completions"),
            _ => o.to_string(),
        };
        // Allow a full resource URL too ("…/v1/chat/completions").
        if o.ends_with("/chat/completions") || o.ends_with("/messages") {
            base = o.to_string();
        }
    }
    if transport.format == Format::Gemini {
        let action = if stream { ":streamGenerateContent?alt=sse" } else { ":generateContent" };
        base = format!("{}/{}{}", base.trim_end_matches('/'), model, action);
    }
    if let Some(acct) = conn.data.pointer("/providerSpecificData/accountId").and_then(Value::as_str) {
        base = base.replace("{accountId}", acct);
    }
    let _ = provider;
    format!("{base}{}", transport.url_suffix)
}

fn apply_auth(
    headers: &mut HashMap<String, String>,
    spec: Option<&AuthSpec>,
    format: Format,
    is_oauth: bool,
    secret: &str,
) {
    let resolved =
        spec.map(|s| if is_oauth { s.oauth.as_deref().unwrap_or(s) } else { s.api_key.as_deref().unwrap_or(s) });
    let (header, scheme) = match resolved.and_then(|s| s.header.clone()) {
        Some(h) => (h, resolved.and_then(|s| s.scheme.clone()).unwrap_or_else(|| "bearer".into())),
        None => match format {
            Format::Claude if !is_oauth => ("x-api-key".to_string(), "raw".to_string()),
            Format::Gemini if !is_oauth => ("x-goog-api-key".to_string(), "raw".to_string()),
            _ => ("Authorization".to_string(), "bearer".to_string()),
        },
    };
    let value = if scheme.eq_ignore_ascii_case("raw") { secret.to_string() } else { format!("Bearer {secret}") };
    headers.insert(header, value);
}

/// Headers for the upstream call. Static transport headers, auth, the client's
/// `anthropic-beta` merged for Claude-format upstreams,
/// provider quirks (`oauth::hooks::headers`), user overrides from
/// `settings.providerOverrides[provider].headers`, and `Accept` for streams.
#[allow(clippy::too_many_arguments)]
pub fn build_headers(
    state: &AppState,
    provider: &Provider,
    transport: &Transport,
    conn: &ProviderConnection,
    client: &HeaderMap,
    model: &str,
    body: &Value,
    stream: bool,
) -> HashMap<String, String> {
    let mut h: HashMap<String, String> = HashMap::new();
    h.insert("Content-Type".into(), "application/json".into());
    for (k, v) in &transport.headers {
        h.insert(k.clone(), v.clone());
    }
    let secret = conn.secret().unwrap_or("public");
    let is_oauth =
        conn.auth_type == "oauth" || (conn.data.get("apiKey").is_none() && conn.data.get("accessToken").is_some());
    if !provider.no_auth {
        apply_auth(&mut h, transport.auth.as_ref(), transport.format, is_oauth, secret);
    }
    if transport.format == Format::Claude {
        if let Some(beta) = client.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
            let key = h
                .keys()
                .find(|k| k.eq_ignore_ascii_case("anthropic-beta"))
                .cloned()
                .unwrap_or_else(|| "Anthropic-Beta".into());
            let mut set: Vec<String> =
                h.get(&key).map(|s| s.split(',').map(|x| x.trim().to_string()).collect()).unwrap_or_default();
            for b in beta.split(',').map(str::trim).filter(|b| !b.is_empty()) {
                if !set.iter().any(|x| x == b) {
                    set.push(b.to_string());
                }
            }
            h.insert(key, set.join(","));
        }
    }
    crate::oauth::hooks::headers(provider, conn, client, model, body, &mut h);
    if let Some(Value::Object(over)) =
        state.db.settings().pointer(&format!("/providerOverrides/{}/headers", provider.id))
    {
        for (k, v) in over {
            if let Some(s) = v.as_str() {
                h.insert(k.clone(), s.to_string());
            }
        }
    }
    if stream {
        h.insert("Accept".into(), "text/event-stream".into());
    }
    h
}

fn retry_policy(transport: &Transport, status: u16) -> (u32, u64) {
    if let Some(v) = transport.retry.get(status.to_string()) {
        return match v {
            Value::Number(n) => (n.as_u64().unwrap_or(0) as u32, 2000),
            Value::Object(o) => (
                o.get("attempts").and_then(Value::as_u64).unwrap_or(0) as u32,
                o.get("delayMs").and_then(Value::as_u64).unwrap_or(2000),
            ),
            _ => (0, 0),
        };
    }
    DEFAULT_RETRY.iter().find(|r| r.0 == status).map(|r| (r.1, r.2)).unwrap_or((0, 0))
}

/// POST `body` upstream. Returns the response once headers arrive with a 2xx status.
#[allow(clippy::too_many_arguments)]
pub async fn send(
    state: &AppState,
    provider: &Provider,
    transport: &Transport,
    conn: &ProviderConnection,
    model: &str,
    body: &Value,
    client_headers: &HeaderMap,
    stream: bool,
) -> Result<reqwest::Response, ExecError> {
    let url = build_url(provider, transport, conn, model, stream);
    let headers = build_headers(state, provider, transport, conn, client_headers, model, body, stream);
    let mut hm = reqwest::header::HeaderMap::new();
    for (k, v) in &headers {
        if let (Ok(n), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            hm.insert(n, val);
        }
    }
    let payload = serde_json::to_vec(body).map_err(|e| ExecError::Network(e.to_string()))?;
    let timeout = Duration::from_millis(transport.timeout_ms.unwrap_or(state.cfg.connect_timeout_ms));

    let mut attempt: u32 = 0;
    loop {
        let req = state.http.post(&url).headers(hm.clone()).body(payload.clone()).timeout(timeout);
        // `.timeout` here bounds the whole request for non-streaming calls; for streams
        // reqwest applies it until the response headers arrive, after which the body is
        // read without this deadline (stall detection happens in the handler).
        let result = req.send().await;
        let (status, retry_delay, body_text) = match result {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(|s| s * 1000);
                let text = resp.text().await.unwrap_or_default();
                (status, retry_after, text)
            }
            Err(e) => {
                let kind = if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect"
                } else {
                    "request"
                };
                (502, None, format!("{kind} error: {e}"))
            }
        };
        let (max_attempts, delay_ms) = retry_policy(transport, status);
        if attempt < max_attempts {
            attempt += 1;
            let wait = retry_delay.unwrap_or(delay_ms).min(30_000);
            tracing::info!("{} {} -> {}, retry {}/{} in {}ms", provider.id, model, status, attempt, max_attempts, wait);
            tokio::time::sleep(Duration::from_millis(wait)).await;
            continue;
        }
        return Err(
            if body_text.starts_with("timeout error")
                || body_text.starts_with("connect error")
                || body_text.starts_with("request error")
            {
                ExecError::Network(body_text)
            } else {
                ExecError::Status { status, body: body_text }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conn(data: Value) -> ProviderConnection {
        ProviderConnection {
            id: "c".into(),
            provider: "p".into(),
            auth_type: "apikey".into(),
            name: None,
            email: None,
            priority: None,
            is_active: true,
            data,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn url_building() {
        let reg = crate::providers::Registry::load(None).unwrap();
        let p = reg.get("anthropic").unwrap();
        let t = p.transport.as_ref().unwrap();
        assert_eq!(build_url(p, t, &conn(json!({})), "m", true), "https://api.anthropic.com/v1/messages");
        let custom = reg.get(crate::providers::OPENAI_COMPATIBLE).unwrap();
        let ct = custom.transport.as_ref().unwrap();
        let c = conn(json!({"providerSpecificData": {"baseUrl": "http://host:11434/v1/"}}));
        assert_eq!(build_url(custom, ct, &c, "m", false), "http://host:11434/v1/chat/completions");
    }

    #[test]
    fn auth_defaults_by_format() {
        let mut h = HashMap::new();
        apply_auth(&mut h, None, Format::Claude, false, "k");
        assert_eq!(h.get("x-api-key").map(String::as_str), Some("k"));
        let mut h = HashMap::new();
        apply_auth(&mut h, None, Format::Openai, false, "k");
        assert_eq!(h.get("Authorization").map(String::as_str), Some("Bearer k"));
        assert_eq!(extract_message(r#"{"error":{"message":"nope"}}"#), "nope");
    }
}
