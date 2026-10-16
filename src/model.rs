//! Model string resolution: `provider/model`, `alias/model`, user aliases, custom
//! endpoint prefixes and name-based inference, in the same order as upstream
//! (`open-sse/services/model.js` + `src/sse/services/model.js`).

use std::collections::HashMap;

use serde_json::Value;

use crate::db::ProviderConnection;
use crate::providers::{Registry, ANTHROPIC_COMPATIBLE, OPENAI_COMPATIBLE};

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

/// `auto` (also `mini-router/auto`): let the router pick any available model.
pub fn is_auto(raw: &str) -> bool {
    matches!(strip_context_marker(raw).to_ascii_lowercase().as_str(), "auto" | "mini-router/auto")
}

/// Ordered candidates for `auto`.
///
/// * `explicit`: the request's `models` array (OpenRouter's fallback list) or
///   `settings.autoModels`; each entry is resolved like a normal model string.
/// * Otherwise every model of every provider with an active account: catalogue
///   models, models added on the provider page (`custom`, kv scope `customModels`),
///   and for providers with neither (compatible endpoints) the live `/models` list
///   in `discovered` (connection id -> ids). The first model of each provider
///   (providers by best account priority) comes first, then the rest, so one broken
///   provider does not eat the whole attempt budget.
pub fn auto_candidates(
    registry: &Registry,
    aliases: &serde_json::Map<String, Value>,
    connections: &[ProviderConnection],
    explicit: &[String],
    custom: &serde_json::Map<String, Value>,
    discovered: &HashMap<String, Vec<String>>,
) -> Vec<Resolved> {
    if !explicit.is_empty() {
        return explicit.iter().filter(|m| !is_auto(m)).map(|m| resolve(registry, aliases, connections, m)).collect();
    }
    let mut sources = auto_sources(registry, connections, custom);
    for s in &mut sources {
        if s.models.is_empty() {
            for c in &s.conns {
                push_unique(&mut s.models, discovered.get(&c.id).into_iter().flatten().cloned());
            }
        }
    }
    sources.sort_by_key(|s| s.rank);
    let depth = sources.iter().map(|s| s.models.len()).max().unwrap_or(0);
    let mut out = Vec::new();
    for i in 0..depth {
        for s in &sources {
            if let Some(m) = s.models.get(i) {
                out.push(Resolved {
                    provider: s.provider.to_string(),
                    model: m.clone(),
                    pinned_connection: s.pinned.map(str::to_string),
                });
            }
        }
    }
    out
}

/// Active accounts whose provider has no known models, so `auto` must ask the
/// endpoint itself.
pub fn needs_live_models<'a>(
    registry: &'a Registry,
    connections: &'a [ProviderConnection],
    custom: &serde_json::Map<String, Value>,
) -> Vec<&'a ProviderConnection> {
    auto_sources(registry, connections, custom)
        .into_iter()
        .filter(|s| s.models.is_empty())
        .flat_map(|s| s.conns)
        .collect()
}

/// One provider (or one compatible-endpoint account) `auto` can draw models from.
struct Source<'a> {
    provider: &'a str,
    pinned: Option<&'a str>,
    /// (best account priority, provider priority)
    rank: (i64, i64),
    models: Vec<String>,
    conns: Vec<&'a ProviderConnection>,
}

fn auto_sources<'a>(
    registry: &'a Registry,
    connections: &'a [ProviderConnection],
    custom: &serde_json::Map<String, Value>,
) -> Vec<Source<'a>> {
    let compatible = |id: &str| id == OPENAI_COMPATIBLE || id == ANTHROPIC_COMPATIBLE;
    let mut out = Vec::new();
    for p in registry.all().iter().filter(|p| !compatible(&p.id)) {
        let conns: Vec<&ProviderConnection> =
            connections.iter().filter(|c| c.is_active && c.provider == p.id).collect();
        let best = conns.iter().map(|c| c.priority.unwrap_or(999)).min();
        let Some(rank) = best.or(p.no_auth.then_some(1000)) else { continue };
        let mut models: Vec<String> = p.models.iter().map(|m| m.id.clone()).collect();
        push_unique(&mut models, custom_list(custom, &p.alias).chain(custom_list(custom, &p.id)));
        out.push(Source { provider: &p.id, pinned: None, rank: (rank, p.priority), models, conns });
    }
    // Each compatible endpoint is its own source, pinned to its account.
    for c in connections.iter().filter(|c| c.is_active && compatible(&c.provider)) {
        let mut models = Vec::new();
        push_unique(&mut models, custom_list(custom, connection_prefix(c).unwrap_or(&c.provider)));
        out.push(Source {
            provider: &c.provider,
            pinned: Some(&c.id),
            rank: (c.priority.unwrap_or(999), 500),
            models,
            conns: vec![c],
        });
    }
    out
}

/// The `prefix` a custom endpoint account declared ("myproxy" in "myproxy/gpt-4o").
pub fn connection_prefix(c: &ProviderConnection) -> Option<&str> {
    c.data
        .pointer("/providerSpecificData/prefix")
        .or_else(|| c.data.get("prefix"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn custom_list<'a>(custom: &'a serde_json::Map<String, Value>, key: &str) -> impl Iterator<Item = String> + 'a {
    custom.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(|m| m.as_str().map(str::to_string))
}

fn push_unique(list: &mut Vec<String>, more: impl IntoIterator<Item = String>) {
    for m in more {
        if !list.contains(&m) {
            list.push(m);
        }
    }
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

    #[test]
    fn auto_candidates_follow_explicit_list_or_accounts() {
        let r = Registry::load(None).unwrap();
        let aliases = serde_json::Map::new();
        let none = serde_json::Map::new();
        assert!(is_auto("auto") && is_auto("mini-router/auto") && !is_auto("openrouter/auto"));

        let explicit = ["ds/deepseek-chat".to_string(), "auto".to_string(), "gpt-4o".to_string()];
        let c = auto_candidates(&r, &aliases, &[], &explicit, &none, &HashMap::new());
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].provider.as_str(), c[0].model.as_str()), ("deepseek", "deepseek-chat"));

        // No accounts: only no-auth providers can be offered.
        let c = auto_candidates(&r, &aliases, &[], &[], &none, &HashMap::new());
        assert!(c.iter().all(|x| r.get(&x.provider).map(|p| p.no_auth).unwrap_or(false)));

        // A compatible endpoint has no catalogue: it asks for a live list, then uses it.
        let conn = ProviderConnection {
            id: "node1".into(),
            provider: OPENAI_COMPATIBLE.into(),
            auth_type: "apikey".into(),
            name: None,
            email: None,
            priority: Some(1),
            is_active: true,
            data: json!({"apiKey": "k", "providerSpecificData": {"baseUrl": "http://x/v1"}}),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let conns = [conn];
        let wanted = needs_live_models(&r, &conns, &none);
        assert_eq!(wanted.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["node1"]);
        let live = HashMap::from([("node1".to_string(), vec!["llama3".to_string()])]);
        let c = auto_candidates(&r, &aliases, &conns, &[], &none, &live);
        assert_eq!(
            c[0],
            Resolved {
                provider: OPENAI_COMPATIBLE.into(),
                model: "llama3".into(),
                pinned_connection: Some("node1".into())
            }
        );

        // Models added on the provider page count as known models.
        let mut custom = serde_json::Map::new();
        custom.insert(OPENAI_COMPATIBLE.into(), json!(["qwen"]));
        assert!(needs_live_models(&r, &conns, &custom).is_empty());
        assert_eq!(auto_candidates(&r, &aliases, &conns, &[], &custom, &HashMap::new())[0].model, "qwen");
    }
}
