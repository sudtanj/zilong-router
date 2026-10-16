//! Live model lists for accounts whose provider has no catalogue (OpenAI / Anthropic
//! compatible endpoints, passthrough providers): `GET <api root>/models` with the
//! account's credentials, cached per account so `auto` costs no extra call per request.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use axum::http::HeaderMap;
use parking_lot::Mutex;
use serde_json::Value;

use crate::db::ProviderConnection;
use crate::model::Resolved;
use crate::state::AppState;

const TTL_OK: Duration = Duration::from_secs(30 * 60);
/// Failed lookups are retried sooner, but not on every request.
const TTL_ERR: Duration = Duration::from_secs(5 * 60);
const TIMEOUT: Duration = Duration::from_secs(8);
const MAX_MODELS: usize = 200;

/// Connection id -> (fetched at, model ids).
pub type Cache = Mutex<HashMap<String, (Instant, Vec<String>)>>;

/// `auto` candidates, fetching live model lists where the catalogue has none.
pub async fn auto_candidates(
    state: &AppState,
    aliases: &serde_json::Map<String, Value>,
    connections: &[ProviderConnection],
    explicit: &[String],
) -> Vec<Resolved> {
    let custom = state.db.kv_scope("customModels").unwrap_or_default();
    let mut discovered = HashMap::new();
    if explicit.is_empty() {
        let wanted = crate::model::needs_live_models(&state.registry, connections, &custom);
        let lists = futures_util::future::join_all(wanted.iter().map(|c| models_for(state, c))).await;
        discovered = wanted.iter().map(|c| c.id.clone()).zip(lists).collect();
    }
    crate::model::auto_candidates(&state.registry, aliases, connections, explicit, &custom, &discovered)
}

/// Model ids an account's endpoint advertises (empty when it can't be listed).
pub async fn models_for(state: &AppState, conn: &ProviderConnection) -> Vec<String> {
    let cached = state.model_cache.lock().get(&conn.id).cloned();
    if let Some((at, list)) = cached {
        if at.elapsed() < if list.is_empty() { TTL_ERR } else { TTL_OK } {
            return list;
        }
    }
    let list = match fetch(state, conn).await {
        Ok(l) => {
            tracing::info!("auto: {} ({}) lists {} models", conn.provider, conn.label(), l.len());
            l
        }
        Err(e) => {
            tracing::info!("auto: cannot list models of {} ({}): {e}", conn.provider, conn.label());
            Vec::new()
        }
    };
    state.model_cache.lock().insert(conn.id.clone(), (Instant::now(), list.clone()));
    list
}

async fn fetch(state: &AppState, conn: &ProviderConnection) -> anyhow::Result<Vec<String>> {
    let p = state.registry.get(&conn.provider).ok_or_else(|| anyhow!("unknown provider"))?;
    let t = p.transport.as_ref().ok_or_else(|| anyhow!("no endpoint"))?;
    if !t.format.supported() {
        bail!("{} endpoints are not supported", t.format.as_str());
    }
    let url = format!("{}/models", api_root(conn.base_url_override().unwrap_or(&t.base_url)));
    let mut rb = state.http.get(&url).timeout(TIMEOUT);
    for (k, v) in crate::executor::build_headers(state, p, t, conn, &HeaderMap::new(), false) {
        if !k.eq_ignore_ascii_case("content-type") {
            rb = rb.header(k, v);
        }
    }
    let resp = rb.send().await?;
    if !resp.status().is_success() {
        bail!("{url} answered {}", resp.status());
    }
    let v: Value = resp.json().await?;
    Ok(parse_ids(&v))
}

/// `{data: [{id}]}` (OpenAI, Anthropic) or `{models: [{id|name}]}` (Ollama style).
fn parse_ids(v: &Value) -> Vec<String> {
    let items = v.get("data").or_else(|| v.get("models")).and_then(Value::as_array);
    let mut out: Vec<String> = Vec::new();
    for item in items.into_iter().flatten() {
        let id = item.as_str().or_else(|| item.get("id").or_else(|| item.get("name")).and_then(Value::as_str));
        if let Some(id) = id.map(str::trim).filter(|s| !s.is_empty() && is_chat_model(s)) {
            if !out.iter().any(|m| m == id) {
                out.push(id.to_string());
            }
        }
        if out.len() >= MAX_MODELS {
            break;
        }
    }
    out
}

/// Skip ids that can never answer a chat request.
fn is_chat_model(id: &str) -> bool {
    let l = id.to_ascii_lowercase();
    !["embed", "whisper", "tts", "dall-e", "moderation", "rerank", "transcribe", "image"].iter().any(|w| l.contains(w))
}

/// API root of an endpoint URL: drop the resource path ("…/v1/chat/completions" -> "…/v1").
fn api_root(url: &str) -> &str {
    let u = url.split('?').next().unwrap_or(url).trim_end_matches('/');
    ["/chat/completions", "/messages", "/responses", "/completions"].iter().find_map(|s| u.strip_suffix(s)).unwrap_or(u)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_model_lists_and_roots() {
        let v =
            json!({"data": [{"id": "gpt-4o"}, {"id": "text-embedding-3-small"}, {"id": "gpt-4o"}, {"id": "llama3"}]});
        assert_eq!(parse_ids(&v), ["gpt-4o", "llama3"]);
        assert_eq!(parse_ids(&json!({"models": [{"name": "qwen2.5:7b"}]})), ["qwen2.5:7b"]);
        assert_eq!(api_root("https://x.ai/v1/chat/completions"), "https://x.ai/v1");
        assert_eq!(api_root("https://api.anthropic.com/v1/messages?beta=true"), "https://api.anthropic.com/v1");
        assert_eq!(api_root("http://10.0.0.2:11434/v1/"), "http://10.0.0.2:11434/v1");
    }
}
