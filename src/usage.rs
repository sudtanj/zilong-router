//! Token accounting: extract usage from upstream payloads (any format), merge
//! across stream events, cost it and persist one `usageHistory` row.

use serde_json::{json, Value};

use crate::db::UsageRow;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    /// Cache-inclusive prompt tokens (Claude: input + cache_read + cache_creation).
    pub prompt: i64,
    pub completion: i64,
    pub cached: i64,
    pub cache_creation: i64,
    pub reasoning: i64,
}

impl TokenCounts {
    pub fn is_empty(&self) -> bool {
        self.prompt == 0 && self.completion == 0
    }

    /// Field-wise max, as upstream `mergeUsage`: Claude splits usage across
    /// message_start and message_delta, OpenAI repeats it on the last chunk.
    pub fn merge(&mut self, other: TokenCounts) {
        self.prompt = self.prompt.max(other.prompt);
        self.completion = self.completion.max(other.completion);
        self.cached = self.cached.max(other.cached);
        self.cache_creation = self.cache_creation.max(other.cache_creation);
        self.reasoning = self.reasoning.max(other.reasoning);
    }

    pub fn total(&self) -> i64 {
        self.prompt + self.completion
    }

    pub fn to_json(&self) -> Value {
        json!({
            "prompt_tokens": self.prompt, "completion_tokens": self.completion,
            "total_tokens": self.total(), "cached_tokens": self.cached,
            "cache_creation_input_tokens": self.cache_creation, "reasoning_tokens": self.reasoning,
        })
    }
}

fn i(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_i64).unwrap_or(0)
}

/// Pull usage from a JSON payload or SSE event body in OpenAI, Claude or Gemini shape.
pub fn extract(v: &Value) -> Option<TokenCounts> {
    // Claude: top-level `usage` (message_delta) or `message.usage` (message_start).
    if let Some(u) = v.get("usage").or_else(|| v.pointer("/message/usage")) {
        if u.get("input_tokens").is_some() || u.get("output_tokens").is_some() {
            let cr = i(u.get("cache_read_input_tokens"));
            let cc = i(u.get("cache_creation_input_tokens"));
            return Some(TokenCounts {
                prompt: i(u.get("input_tokens")) + cr + cc,
                completion: i(u.get("output_tokens")),
                cached: cr,
                cache_creation: cc,
                reasoning: 0,
            });
        }
        if u.get("prompt_tokens").is_some() || u.get("completion_tokens").is_some() {
            let cached = u
                .pointer("/prompt_tokens_details/cached_tokens")
                .or_else(|| u.get("prompt_cache_hit_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            return Some(TokenCounts {
                prompt: i(u.get("prompt_tokens")),
                completion: i(u.get("completion_tokens")),
                cached,
                cache_creation: i(u.pointer("/prompt_tokens_details/cache_creation_tokens")),
                reasoning: i(u.pointer("/completion_tokens_details/reasoning_tokens")),
            });
        }
    }
    if let Some(u) = v.get("usageMetadata").or_else(|| v.pointer("/response/usageMetadata")) {
        return Some(TokenCounts {
            prompt: i(u.get("promptTokenCount")),
            completion: i(u.get("candidatesTokenCount")) + i(u.get("thoughtsTokenCount")),
            cached: i(u.get("cachedContentTokenCount")),
            cache_creation: 0,
            reasoning: i(u.get("thoughtsTokenCount")),
        });
    }
    None
}

/// Rough fallback when upstream sent no usage: chars / 4.
pub fn estimate(request_body_len: usize, output_chars: usize) -> TokenCounts {
    TokenCounts {
        prompt: (request_body_len / 4) as i64,
        completion: (output_chars / 4).max(1) as i64,
        ..Default::default()
    }
}

pub struct Record<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub connection_id: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub endpoint: &'a str,
    pub status: &'a str,
    pub tokens: TokenCounts,
    pub latency_ms: u128,
    pub ttft_ms: Option<u128>,
}

/// Persist a request. Skips rows with no tokens at all, like upstream.
pub fn record(state: &AppState, r: Record<'_>) {
    if r.tokens.is_empty() {
        return;
    }
    let settings = state.db.settings();
    let price = state.pricing.lookup(r.provider, r.model, settings.get("pricing"));
    let row = UsageRow {
        timestamp: crate::db::now_iso(),
        provider: r.provider.to_string(),
        model: r.model.to_string(),
        connection_id: r.connection_id.map(str::to_string),
        api_key: r.api_key.map(mask_key),
        endpoint: r.endpoint.to_string(),
        prompt_tokens: r.tokens.prompt,
        completion_tokens: r.tokens.completion,
        cost: price.cost(&r.tokens),
        status: r.status.to_string(),
        tokens: r.tokens.to_json(),
        meta: json!({"latencyMs": r.latency_ms as u64, "ttftMs": r.ttft_ms.map(|v| v as u64)}),
    };
    if let Err(e) = state.db.insert_usage(&row) {
        tracing::warn!("usage insert failed: {e}");
    }
}

/// Upstream stores raw keys; we store a masked form so the usage table isn't a
/// credential dump if the SD card walks away.
pub fn mask_key(k: &str) -> String {
    if k.len() <= 10 {
        return "***".into();
    }
    format!("{}…{}", &k[..6], &k[k.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_claude_and_openai() {
        let c = extract(&json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":5}}})).unwrap();
        assert_eq!(c.prompt, 15);
        assert_eq!(c.cached, 5);
        let o = extract(
            &json!({"usage":{"prompt_tokens":7,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":2}}}),
        )
        .unwrap();
        assert_eq!((o.prompt, o.completion, o.cached), (7, 3, 2));
        assert!(extract(&json!({"choices":[]})).is_none());
    }

    #[test]
    fn merge_is_max() {
        let mut a = TokenCounts { prompt: 10, ..Default::default() };
        a.merge(TokenCounts { completion: 4, ..Default::default() });
        assert_eq!((a.prompt, a.completion), (10, 4));
    }
}
