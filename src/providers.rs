//! Provider registry. The data comes from upstream 9router via
//! `tools/sync-upstream/convert.mjs` -> `data/providers.json` (embedded at build time).
//! A newer `providers.json` dropped into the data directory overrides the embedded copy
//! at startup, so provider updates don't require a rebuild.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EMBEDDED_PROVIDERS: &str = include_str!("../data/providers.json");

/// Wire format spoken by an upstream endpoint (or by a client).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    Openai,
    Claude,
    Gemini,
    #[serde(other)]
    Other,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Openai => "openai",
            Format::Claude => "claude",
            Format::Gemini => "gemini",
            Format::Other => "other",
        }
    }
    pub fn supported(self) -> bool {
        matches!(self, Format::Openai | Format::Claude)
    }
}

impl Default for Format {
    fn default() -> Self {
        Format::Openai
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSpec {
    /// Header name, e.g. "Authorization" or "x-api-key".
    pub header: Option<String>,
    /// "bearer" (prefix `Bearer `) or "raw".
    pub scheme: Option<String>,
    /// Split form used by a few providers: separate descriptors per credential type.
    pub api_key: Option<Box<AuthSpec>>,
    pub oauth: Option<Box<AuthSpec>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transport {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub auth: Option<AuthSpec>,
    #[serde(default)]
    pub force_stream: bool,
    #[serde(default)]
    pub url_suffix: String,
    #[serde(default)]
    pub quirks: Value,
    /// Per-status retry overrides: {"429": {"attempts": n, "delayMs": ms}} or a bare number.
    #[serde(default)]
    pub retry: Value,
    pub timeout_ms: Option<u64>,
    pub stall_timeout_ms: Option<u64>,
    pub validate_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDef {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub upstream_model_id: Option<String>,
    pub target_format: Option<Format>,
    #[serde(default)]
    pub capabilities: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Display {
    pub name: String,
    pub color: Option<String>,
    pub text_icon: Option<String>,
    pub icon: Option<String>,
    pub website: Option<String>,
    pub api_key_url: Option<String>,
    #[serde(default)]
    pub deprecated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub auth_type: String,
    #[serde(default)]
    pub no_auth: bool,
    #[serde(default = "default_priority")]
    pub priority: i64,
    pub display: Option<Display>,
    pub transport: Option<Transport>,
    /// Multi-endpoint providers (e.g. DeepSeek): one transport per wire format so the
    /// router can pick the one matching the client and skip translation.
    pub transports: Option<Vec<Transport>>,
    #[serde(default)]
    pub models: Vec<ModelDef>,
    #[serde(default)]
    pub passthrough_models: bool,
}

fn default_priority() -> i64 {
    999
}

impl Provider {
    pub fn name(&self) -> &str {
        self.display.as_ref().map(|d| d.name.as_str()).unwrap_or(&self.id)
    }

    /// Transport to use for a request whose client speaks `source`. Prefers a
    /// same-format endpoint from `transports`, then the model's declared format,
    /// then the default transport.
    pub fn transport_for(&self, source: Format, model: &str) -> Option<&Transport> {
        if let Some(list) = &self.transports {
            if let Some(t) = list.iter().find(|t| t.format == source && t.format.supported()) {
                return Some(t);
            }
        }
        let default = self.transport.as_ref()?;
        if let Some(target) = self.model(model).and_then(|m| m.target_format) {
            if let Some(list) = &self.transports {
                if let Some(t) = list.iter().find(|t| t.format == target) {
                    return Some(t);
                }
            }
        }
        Some(default)
    }

    pub fn model(&self, id: &str) -> Option<&ModelDef> {
        let base = id.trim_end_matches(|c: char| c == ' ').to_string();
        self.models.iter().find(|m| m.id == id || m.id == base)
    }

    /// Upstream model id (handles `upstreamModelId` renames such as
    /// deepseek-v4-pro-max -> deepseek-v4-pro).
    pub fn upstream_model_id<'a>(&'a self, id: &'a str) -> &'a str {
        self.model(id).and_then(|m| m.upstream_model_id.as_deref()).unwrap_or(id)
    }

    pub fn is_valid_model(&self, id: &str) -> bool {
        self.passthrough_models || self.models.is_empty() || self.model(id).is_some()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub upstream_version: Option<String>,
    pub upstream_commit: Option<String>,
    pub generated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    meta: Meta,
    providers: Vec<Provider>,
}

pub struct Registry {
    pub meta: Meta,
    providers: Vec<Provider>,
    by_id: HashMap<String, usize>,
    alias_to_id: HashMap<String, String>,
}

/// Ids of the synthetic "bring your own endpoint" providers. A connection for one of
/// these must carry `providerSpecificData.baseUrl` (and optionally `prefix`).
pub const OPENAI_COMPATIBLE: &str = "openai-compatible";
pub const ANTHROPIC_COMPATIBLE: &str = "anthropic-compatible";

impl Registry {
    pub fn load(override_path: Option<&std::path::Path>) -> anyhow::Result<Registry> {
        let mut text = EMBEDDED_PROVIDERS.to_string();
        if let Some(p) = override_path {
            if p.exists() {
                match std::fs::read_to_string(p) {
                    Ok(t) if serde_json::from_str::<File>(&t).is_ok() => {
                        tracing::info!("using provider registry override {}", p.display());
                        text = t;
                    }
                    Ok(_) => tracing::warn!("ignoring malformed {}", p.display()),
                    Err(e) => tracing::warn!("cannot read {}: {e}", p.display()),
                }
            }
        }
        let file: File = serde_json::from_str(&text)?;
        let mut providers = file.providers;
        providers.push(synthetic(OPENAI_COMPATIBLE, "OpenAI Compatible", Format::Openai));
        providers.push(synthetic(ANTHROPIC_COMPATIBLE, "Anthropic Compatible", Format::Claude));

        let mut by_id = HashMap::new();
        let mut alias_to_id = HashMap::new();
        for (i, p) in providers.iter().enumerate() {
            by_id.insert(p.id.clone(), i);
            alias_to_id.insert(p.id.clone(), p.id.clone());
            if !p.alias.is_empty() {
                alias_to_id.insert(p.alias.clone(), p.id.clone());
            }
            for a in &p.aliases {
                alias_to_id.insert(a.clone(), p.id.clone());
            }
        }
        Ok(Registry { meta: file.meta, providers, by_id, alias_to_id })
    }

    pub fn get(&self, id: &str) -> Option<&Provider> {
        self.by_id.get(id).map(|&i| &self.providers[i])
    }

    /// Resolve an alias ("ds") or id ("deepseek") to a provider id, if known.
    pub fn resolve_alias(&self, alias_or_id: &str) -> Option<&str> {
        self.alias_to_id.get(alias_or_id).map(String::as_str)
    }

    pub fn all(&self) -> &[Provider] {
        &self.providers
    }
}

fn synthetic(id: &str, name: &str, format: Format) -> Provider {
    Provider {
        id: id.to_string(),
        alias: id.to_string(),
        aliases: vec![],
        category: "custom".into(),
        auth_type: "apikey".into(),
        no_auth: false,
        priority: 500,
        display: Some(Display { name: name.into(), ..Default::default() }),
        transport: Some(Transport {
            format,
            headers: if format == Format::Claude {
                HashMap::from([("anthropic-version".to_string(), "2023-06-01".to_string())])
            } else {
                HashMap::new()
            },
            ..Default::default()
        }),
        transports: None,
        models: vec![],
        passthrough_models: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_registry_parses() {
        let r = Registry::load(None).unwrap();
        assert!(r.get("openai").is_some());
        assert_eq!(r.resolve_alias("ds"), Some("deepseek"));
        let ds = r.get("deepseek").unwrap();
        assert_eq!(ds.transport_for(Format::Claude, "deepseek-chat").unwrap().format, Format::Claude);
        assert_eq!(ds.upstream_model_id("deepseek-v4-pro-max"), "deepseek-v4-pro");
        assert!(r.get(OPENAI_COMPATIBLE).unwrap().passthrough_models);
    }
}
