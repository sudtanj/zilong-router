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

/// `auto` (also `zilong-router/auto`, and the `auto-strong` variants):
/// let the router pick any available model.
pub fn is_auto(raw: &str) -> bool {
    is_auto_strong(raw)
        || matches!(strip_context_marker(raw).to_ascii_lowercase().as_str(), "auto" | "zilong-router/auto")
}

/// `auto-strong` (also `zilong-router/auto-strong`, and `auto-coding` for coding
/// agents): like `auto`, but only models that cope with agent work (long system
/// prompts, many tools, multi-step tool use).
pub fn is_auto_strong(raw: &str) -> bool {
    matches!(
        strip_context_marker(raw).to_ascii_lowercase().as_str(),
        "auto-strong" | "zilong-router/auto-strong" | "auto-coding" | "zilong-router/auto-coding"
    )
}

/// How well a model handles agent work, from its id: 3 frontier, 2 strong, 1 capable,
/// 0 unknown or too small. Providers publish no capability data, so this is a list of
/// families known to call tools reliably; anything not on it is left out of
/// `auto-strong` rather than guessed.
pub fn strength(model: &str) -> u8 {
    let last = model.rsplit('/').next().unwrap_or(model);
    let id = last.split(':').next().unwrap_or(last).to_ascii_lowercase();
    let id = id.as_str();
    let has = |w: &str| id.contains(w);
    let any = |ws: &[&str]| ws.iter().any(|&w| has(w));
    if any(&["nano", "lite", "tiny", "-small", "-air", "distill"]) {
        return 0;
    }
    if params_b(id).is_some_and(|b| b < 100) {
        return 0;
    }
    let claude = has("claude");
    let claude3 = has("claude-3");
    let gpt5 = id.starts_with("gpt-5") || id.starts_with("gpt-6");
    let o_series = id.len() >= 2 && id.starts_with('o') && id.as_bytes()[1].is_ascii_digit();
    let gemini = has("gemini") && !has("gemini-1") && !has("gemini-2.0");
    let deepseek = has("deepseek") && !has("r1");
    let deepseek_next = deepseek && (has("v4") || has("v5"));
    let qwen = has("qwen") && !has("qwq") && !has("qwen2");
    // First match wins.
    let rules: &[(bool, u8)] = &[
        (claude && (has("opus") || has("fable")), 3),
        (claude && has("sonnet") && !claude3, 3),
        (claude && has("sonnet") && any(&["3-5", "3.5", "3-7", "3.7"]), 2),
        (claude && has("haiku") && !claude3, 1),
        (gpt5 && has("mini"), 1),
        (gpt5, 3),
        (id.starts_with("gpt-4.1") && !has("mini"), 2),
        ((id.starts_with("gpt-4o") || id.starts_with("gpt-4-turbo")) && !has("mini"), 1),
        (id.starts_with("gpt-oss"), 1),
        (o_series && id.starts_with("o4") && has("mini"), 1),
        (o_series && id.starts_with("o1") && !has("mini"), 2),
        (o_series && !has("mini"), 3),
        (gemini && has("pro"), 3),
        (gemini && has("flash"), 2),
        (has("grok-4") || has("grok-5"), 3),
        (id.starts_with("grok-") && has("code"), 2),
        (has("grok-3"), 1),
        (deepseek_next && has("pro"), 3),
        (deepseek_next, 2),
        (deepseek && any(&["v3.1", "v3.2", "v3p1", "v3p2", "chat", "reasoner"]), 2),
        (deepseek && has("v3"), 1),
        (qwen && (has("max") || has("397b")), 3),
        (qwen && has("coder") && any(&["480b", "plus", "next"]), 2),
        (qwen && !has("coder") && (has("235b") || has("plus")), 2),
        (has("kimi"), 3),
        (has("glm") && has("flash"), 1),
        (has("glm-5"), 3),
        (any(&["glm-4.5", "glm-4.6", "glm-4.7", "glm-4-7"]), 2),
        (has("minimax") && has("m3"), 3),
        (has("minimax") && has("m2"), 2),
        (any(&["mistral-large", "mistral-medium", "magistral-medium", "devstral"]), 2),
        (has("mimo-v") && has("pro"), 2),
        (has("mimo-v"), 1),
        (has("seed") && (has("pro") || has("code")), 2),
        (has("nemotron") && has("ultra"), 2),
        (has("nemotron") && has("super"), 1),
        (id.starts_with("command-a"), 1),
    ];
    rules.iter().find(|(hit, _)| *hit).map_or(0, |(_, tier)| *tier)
}

/// Largest "<n>b" parameter count in a model id ("qwen3-235b-a22b" -> 235).
fn params_b(id: &str) -> Option<u64> {
    let bytes = id.as_bytes();
    let mut best = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'a') {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let at_b = i < bytes.len() && bytes[i] == b'b';
            let ends = i + 1 >= bytes.len() || !bytes[i + 1].is_ascii_alphanumeric();
            if at_b && ends {
                if let Ok(n) = id[start..i].parse::<u64>() {
                    best = Some(best.map_or(n, |b: u64| b.max(n)));
                }
            }
        } else {
            i += 1;
        }
    }
    best
}

/// `auto-strong`: keep only models with a known strength, strongest first (the order
/// within a tier is kept). Returns how many share the top tier, the part round-robin
/// may rotate.
pub fn strongest_first(candidates: &mut Vec<Resolved>) -> usize {
    candidates.retain(|c| strength(&c.model) > 0);
    candidates.sort_by_key(|c| std::cmp::Reverse(strength(&c.model)));
    let top = candidates.first().map(|c| strength(&c.model));
    candidates.iter().take_while(|c| Some(strength(&c.model)) == top).count()
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

/// Whether a model was removed on a provider page (kv scope `hiddenModels`, keyed
/// like `customModels`): such models are left out of `/v1/models`, the dashboard and
/// `auto`, but still answer when asked for by name.
pub fn is_hidden(hidden: &serde_json::Map<String, Value>, keys: &[&str], model: &str) -> bool {
    keys.iter().any(|k| custom_list(hidden, k).any(|m| m == model))
}

/// Keys a candidate's hidden models are stored under: the prefix of a pinned custom
/// endpoint, else the provider's alias and id.
pub fn hidden_keys<'a>(registry: &'a Registry, connections: &'a [ProviderConnection], r: &'a Resolved) -> Vec<&'a str> {
    if let Some(id) = r.pinned_connection.as_deref() {
        let c = connections.iter().find(|c| c.id == id);
        return vec![c.and_then(connection_prefix).unwrap_or(&r.provider)];
    }
    let mut keys = vec![r.provider.as_str()];
    keys.extend(registry.get(&r.provider).map(|p| p.alias.as_str()));
    keys
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
        assert!(is_auto("auto") && is_auto("zilong-router/auto") && !is_auto("openrouter/auto"));

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

    #[test]
    fn strength_keeps_agent_capable_families_and_drops_small_models() {
        for (id, want) in [
            ("claude-opus-4-6", 3),
            ("anthropic/claude-sonnet-4.6", 3),
            ("claude-3-5-sonnet-20241022", 2),
            ("claude-haiku-4-5", 1),
            ("gpt-5.4", 3),
            ("gpt-5-mini", 1),
            ("gpt-5-nano", 0),
            ("gpt-4.1", 2),
            ("gpt-4o-mini", 0),
            ("o3", 3),
            ("o3-mini", 0),
            ("o4-mini", 1),
            ("gemini-3.1-pro-preview", 3),
            ("google/gemini-2.5-flash", 2),
            ("gemini-3.1-flash-lite-preview", 0),
            ("gemini-2.0-flash", 0),
            ("deepseek-chat", 2),
            ("deepseek/deepseek-v4-pro", 3),
            ("@cf/deepseek-ai/deepseek-r1-distill-qwen-32b", 0),
            ("qwen3-coder-480b-a35b-instruct-turbo", 2),
            ("qwen/qwen3.7-max", 3),
            ("qwen-3-32b", 0),
            ("Qwen/Qwen2.5-72B-Instruct", 0),
            ("moonshotai/kimi-k2.6", 3),
            ("glm-4.6", 2),
            ("glm-4.5-air", 0),
            ("MiniMax-M2.5", 2),
            ("grok-4", 3),
            ("gpt-oss-120b", 1),
            ("gpt-oss-20b", 0),
            ("llama-3.3-70b-versatile", 0),
            ("meta-llama/llama-4-maverick-17b-128e-instruct", 0),
            ("nvidia/nemotron-3-super-120b-a12b:free", 1),
            ("kilo-auto/free", 0),
            ("big-pickle", 0),
        ] {
            assert_eq!(strength(id), want, "{id}");
        }
        assert_eq!(params_b("qwen3-235b-a22b"), Some(235));
        assert_eq!(params_b("claude-opus-4-20250514"), None);
    }

    #[test]
    fn auto_strong_keeps_strong_models_strongest_first() {
        assert!(is_auto_strong("auto-strong") && is_auto_strong("zilong-router/auto-strong"));
        assert!(is_auto("auto-strong") && !is_auto_strong("auto"));
        assert!(is_auto_strong("auto-coding") && is_auto("zilong-router/auto-coding"));
        let r = |p: &str, m: &str| Resolved { provider: p.into(), model: m.into(), pinned_connection: None };
        let mut c = vec![
            r("groq", "llama-3.3-70b-versatile"),
            r("deepseek", "deepseek-chat"),
            r("anthropic", "claude-sonnet-4-6"),
            r("openai", "gpt-4o"),
            r("gemini", "gemini-2.5-pro"),
        ];
        let top = strongest_first(&mut c);
        let order: Vec<&str> = c.iter().map(|x| x.model.as_str()).collect();
        assert_eq!(order, ["claude-sonnet-4-6", "gemini-2.5-pro", "deepseek-chat", "gpt-4o"]);
        assert_eq!(top, 2);
    }

    #[test]
    fn hidden_models_are_keyed_by_prefix_alias_or_id() {
        let r = Registry::load(None).unwrap();
        let mut hidden = serde_json::Map::new();
        hidden.insert("oc".into(), json!(["big-pickle"]));
        hidden.insert("myproxy".into(), json!(["llama3"]));
        let oc = Resolved { provider: "opencode".into(), model: "big-pickle".into(), pinned_connection: None };
        assert_eq!(hidden_keys(&r, &[], &oc), ["opencode", "oc"]);
        assert!(is_hidden(&hidden, &hidden_keys(&r, &[], &oc), &oc.model));
        assert!(!is_hidden(&hidden, &hidden_keys(&r, &[], &oc), "exo-free"));

        let conn = ProviderConnection {
            id: "node1".into(),
            provider: OPENAI_COMPATIBLE.into(),
            auth_type: "apikey".into(),
            name: None,
            email: None,
            priority: Some(1),
            is_active: true,
            data: json!({"apiKey": "k", "providerSpecificData": {"prefix": "myproxy"}}),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let conns = [conn];
        let pinned = Resolved {
            provider: OPENAI_COMPATIBLE.into(),
            model: "llama3".into(),
            pinned_connection: Some("node1".into()),
        };
        assert_eq!(hidden_keys(&r, &conns, &pinned), ["myproxy"]);
        assert!(is_hidden(&hidden, &hidden_keys(&r, &conns, &pinned), "llama3"));
    }
}
