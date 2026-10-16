//! Model string resolution: `provider/model`, `alias/model`, user aliases, custom
//! endpoint prefixes and name-based inference, in the same order as upstream
//! (`open-sse/services/model.js` + `src/sse/services/model.js`).

use serde_json::Value;

use crate::db::ProviderConnection;
use crate::providers::Registry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub provider: String,
    pub model: String,
    /// Set when the prefix matched a custom endpoint connection's `prefix`; the
    /// router then uses only that connection.
    pub pinned_connection: Option<String>,
}

/// Claude Code appends `[1m]` to request the 1M context window; strip it.
pub fn strip_context_marker(model: &str) -> &str {
    model.trim().strip_suffix("[1m]").unwrap_or(model.trim())
}

/// Name-prefix inference (first match wins, default openai), same table as upstream
/// minus the Codex-only slugs we can't serve without OAuth.
pub fn infer_provider(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude-") {
        "anthropic"
    } else if m.starts_with("gemini-") {
        "gemini"
    } else if m.starts_with("gpt-") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
        "openai"
    } else if m.starts_with("deepseek-") {
        "openrouter"
    } else if m.starts_with("grok-") {
        "xai"
    } else if m.starts_with("mistral") || m.starts_with("codestral") {
        "mistral"
    } else {
        "openai"
    }
}

/// Resolve a client model string.
///
/// * `aliases`: the `modelAliases` kv scope, values `"provider/model"` or `{provider, model}`.
/// * `connections`: all connections, used to match custom endpoint prefixes.
pub fn resolve(
    registry: &Registry,
    aliases: &serde_json::Map<String, Value>,
    connections: &[ProviderConnection],
    raw: &str,
) -> Resolved {
    let s = strip_context_marker(raw);

    if let Some((prefix, rest)) = s.split_once('/') {
        if let Some(id) = registry.resolve_alias(prefix) {
            return Resolved { provider: id.to_string(), model: rest.to_string(), pinned_connection: None };
        }
        // Custom endpoint prefix ("myproxy/gpt-4o") -> the connection that declared it.
        if let Some(c) = connections.iter().find(|c| {
            c.data.pointer("/providerSpecificData/prefix").and_then(Value::as_str) == Some(prefix)
                || c.data.get("prefix").and_then(Value::as_str) == Some(prefix)
        }) {
            return Resolved {
                provider: c.provider.clone(),
                model: rest.to_string(),
                pinned_connection: Some(c.id.clone()),
            };
        }
        // Unknown prefix: treat the whole thing as a model id (e.g. "meta-llama/llama-3"
        // on a passthrough provider) and infer the provider from the tail.
        return Resolved { provider: infer_provider(rest).to_string(), model: s.to_string(), pinned_connection: None };
    }

    if let Some(target) = aliases.get(s).and_then(|v| alias_target(registry, v)) {
        return target;
    }

    Resolved { provider: infer_provider(s).to_string(), model: s.to_string(), pinned_connection: None }
}

fn alias_target(registry: &Registry, v: &Value) -> Option<Resolved> {
    let (p, m) = match v {
        Value::String(s) => s.split_once('/').map(|(p, m)| (p.to_string(), m.to_string()))?,
        Value::Object(o) => (o.get("provider")?.as_str()?.to_string(), o.get("model")?.as_str()?.to_string()),
        _ => return None,
    };
    let provider = registry.resolve_alias(&p).unwrap_or(&p).to_string();
    Some(Resolved { provider, model: m, pinned_connection: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_prefixes_and_aliases() {
        let r = Registry::load(None).unwrap();
        let mut aliases = serde_json::Map::new();
        aliases.insert("fast".into(), json!("groq/llama-3.3-70b-versatile"));
        let res = resolve(&r, &aliases, &[], "ds/deepseek-chat");
        assert_eq!(res.provider, "deepseek");
        assert_eq!(res.model, "deepseek-chat");
        assert_eq!(resolve(&r, &aliases, &[], "fast").provider, "groq");
        assert_eq!(resolve(&r, &aliases, &[], "claude-sonnet-4-5[1m]").provider, "anthropic");
        assert_eq!(resolve(&r, &aliases, &[], "gpt-4o").model, "gpt-4o");
    }
}
