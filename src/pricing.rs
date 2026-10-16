//! Model pricing (USD per 1M tokens), generated from upstream `pricing.js` into
//! `data/pricing.json`. Lookup order mirrors upstream `getPricingForModel`:
//! user override -> provider-specific table -> free namespace -> exact model id
//! (vendor prefix stripped, then raw) -> glob pattern -> $0.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub const EMBEDDED_PRICING: &str = include_str!("../data/pricing.json");

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Pricing {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    pub cached: Option<f64>,
    pub reasoning: Option<f64>,
    pub cache_creation: Option<f64>,
}

impl Pricing {
    pub const ZERO: Pricing =
        Pricing { input: 0.0, output: 0.0, cached: Some(0.0), reasoning: Some(0.0), cache_creation: Some(0.0) };

    /// Cost in USD. Same formula as upstream `calculateCost`.
    pub fn cost(&self, t: &crate::usage::TokenCounts) -> f64 {
        let fresh = (t.prompt - t.cached - t.cache_creation).max(0) as f64;
        (fresh * self.input
            + t.cached as f64 * self.cached.unwrap_or(self.input)
            + t.completion as f64 * self.output
            + t.reasoning as f64 * self.reasoning.unwrap_or(self.output)
            + t.cache_creation as f64 * self.cache_creation.unwrap_or(self.input))
            / 1_000_000.0
    }
}

#[derive(Debug, Deserialize)]
struct ModelRow {
    model: String,
    #[serde(flatten)]
    price: Pricing,
}

#[derive(Debug, Deserialize)]
struct PatternRow {
    pattern: String,
    #[serde(flatten)]
    price: Pricing,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    #[serde(default)]
    free_namespaces: Vec<String>,
    #[serde(default)]
    models: Vec<ModelRow>,
    #[serde(default)]
    providers: HashMap<String, Vec<ModelRow>>,
    #[serde(default)]
    patterns: Vec<PatternRow>,
}

pub struct Table {
    free_namespaces: Vec<String>,
    models: HashMap<String, Pricing>,
    providers: HashMap<String, HashMap<String, Pricing>>,
    patterns: Vec<(String, Pricing)>,
}

impl Table {
    pub fn load(override_path: Option<&std::path::Path>) -> anyhow::Result<Table> {
        let mut text = EMBEDDED_PRICING.to_string();
        if let Some(p) = override_path {
            if let Ok(t) = std::fs::read_to_string(p) {
                if serde_json::from_str::<File>(&t).is_ok() {
                    text = t;
                }
            }
        }
        let f: File = serde_json::from_str(&text)?;
        let lower = |rows: Vec<ModelRow>| {
            rows.into_iter().map(|r| (r.model.to_ascii_lowercase(), r.price)).collect::<HashMap<_, _>>()
        };
        Ok(Table {
            free_namespaces: f.free_namespaces.iter().map(|s| s.to_ascii_lowercase()).collect(),
            models: lower(f.models),
            providers: f.providers.into_iter().map(|(k, v)| (k, lower(v))).collect(),
            patterns: f.patterns.into_iter().map(|r| (r.pattern, r.price)).collect(),
        })
    }

    /// `overrides` is `settings.pricing`, keyed by provider then model.
    pub fn lookup(&self, provider: &str, model: &str, overrides: Option<&Value>) -> Pricing {
        if let Some(p) = overrides
            .and_then(|o| o.get(provider))
            .and_then(|p| p.get(model))
            .and_then(|v| serde_json::from_value::<Pricing>(v.clone()).ok())
        {
            return p;
        }
        let lower = model.to_ascii_lowercase();
        if let Some(p) = self.providers.get(provider).and_then(|m| m.get(&lower)) {
            return *p;
        }
        if self.free_namespaces.iter().any(|ns| lower.starts_with(ns)) || lower.ends_with(":free") {
            return Pricing::ZERO;
        }
        let stripped = lower.rsplit('/').next().unwrap_or(&lower).to_string();
        for cand in [&stripped, &lower] {
            if let Some(p) = self.models.get(cand) {
                return *p;
            }
        }
        for cand in [&stripped, &lower] {
            if let Some((_, p)) = self.patterns.iter().find(|(pat, _)| glob_match(pat, cand)) {
                return *p;
            }
        }
        Pricing::ZERO
    }
}

/// Case-insensitive glob where `*` matches any run of characters.
pub fn glob_match(pattern: &str, model: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let m: Vec<char> = model.to_ascii_lowercase().chars().collect();
    fn rec(p: &[char], m: &[char]) -> bool {
        match p.first() {
            None => m.is_empty(),
            Some('*') => (0..=m.len()).any(|i| rec(&p[1..], &m[i..])),
            Some(c) => m.first() == Some(c) && rec(&p[1..], &m[1..]),
        }
    }
    rec(&p, &m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::TokenCounts;

    #[test]
    fn glob_basics() {
        assert!(glob_match("claude-sonnet-*", "claude-sonnet-4.5"));
        assert!(glob_match("gpt-4o", "GPT-4o"));
        assert!(!glob_match("gpt-4o", "gpt-4o-mini"));
    }

    #[test]
    fn lookup_and_cost() {
        let t = Table::load(None).unwrap();
        let p = t.lookup("openrouter", "anthropic/claude-sonnet-4.5", None);
        assert!(p.input > 0.0);
        assert_eq!(t.lookup("x", "totally-unknown-model", None), Pricing::ZERO);
        let c = Pricing { input: 1.0, output: 2.0, cached: None, reasoning: None, cache_creation: None }
            .cost(&TokenCounts { prompt: 1_000_000, completion: 1_000_000, ..Default::default() });
        assert!((c - 3.0).abs() < 1e-9);
    }
}
