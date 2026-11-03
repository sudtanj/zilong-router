//! The built-in `zilong` MCP server: lets an agent look at the router's status and manage
//! skills through the same `/mcp` gateway as the upstream servers (`zilong__<tool>`).
//!
//! * Opt-in per API key (`Allow zilong control` in the dashboard, off by default); OAuth
//!   client tokens inherit the flag of the key they stand for, and a loopback caller
//!   without a key never gets it. A key without the flag neither sees nor can call these
//!   tools (see [`allowed`]).
//! * Secrets never reach the agent: every answer is built from explicit allow-lists of
//!   fields (never a whole row or JSON blob) and then passed through [`scrub`], which
//!   drops secret-looking fields and masks secret-looking strings. There is no tool that
//!   reads API keys, provider keys, OAuth tokens, MCP headers or client secrets, the
//!   dashboard password or the environment.
//! * Skills are written through `crate::skills` (the dashboard's validation and limits).

use std::collections::{BTreeMap, HashSet};

use serde_json::{json, Map, Value};
use time::OffsetDateTime;

use crate::auth::Caller;
use crate::db::{now_iso, Db, ProviderConnection};
use crate::skills::{self, Skill};
use crate::state::AppState;

/// The server's id; its tools are named `zilong__<tool>`.
pub const SERVER: &str = "zilong";
pub const DESCRIPTION: &str = "the router itself: status of providers, models, usage and MCP servers, and skills";
const SCOPE: &str = "keyControl";

pub const TOOLS: [&str; 9] = [
    "list_providers",
    "list_models",
    "usage_summary",
    "list_mcp_servers",
    "list_skills",
    "get_skill",
    "save_skill",
    "set_skill_enabled",
    "delete_skill",
];

// ----- access -------------------------------------------------------------------------

/// Whether the key `key_id` may use the control tools.
pub fn key_allowed(db: &Db, key_id: &str) -> bool {
    db.kv_get(SCOPE, key_id).ok().flatten().and_then(|v| v.as_bool()).unwrap_or(false)
}

pub fn set_key_allowed(db: &Db, key_id: &str, on: bool) -> anyhow::Result<()> {
    db.kv_set(SCOPE, key_id, on.then(|| json!(true)).as_ref())
}

/// Whether `caller` may see and call the control tools: only a key that has the flag.
pub fn allowed(db: &Db, caller: &Caller) -> bool {
    match caller {
        Caller::Key(k) => key_allowed(db, &k.id),
        Caller::Local => false,
    }
}

// ----- keeping secrets out ---------------------------------------------------------------

fn secret_key(name: &str) -> bool {
    const PARTS: [&str; 9] =
        ["apikey", "api_key", "secret", "password", "authorization", "cookie", "accesstoken", "refreshtoken", "header"];
    let k = name.to_ascii_lowercase();
    PARTS.iter().any(|p| k.contains(p)) || k.ends_with("token") || k == "bearer"
}

fn is_uuid(t: &str) -> bool {
    let parts: Vec<&str> = t.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12].iter().zip(&parts).all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn looks_secret(t: &str) -> bool {
    const PREFIXES: [&str; 11] =
        ["sk-", "sk_", "ghp_", "gho_", "ghu_", "ghs_", "github_pat_", "xox", "AIza", "eyJ", "glpat-"];
    if t.len() < 16 || is_uuid(t) {
        return false;
    }
    if PREFIXES.iter().any(|p| t.starts_with(p)) || t.starts_with("ya29.") || t.starts_with("AKIA") {
        return true;
    }
    t.len() >= 32 && t.chars().any(|c| c.is_ascii_digit()) && t.chars().any(|c| c.is_ascii_alphabetic())
}

/// `text` with every secret-looking word (API key shapes, JWTs, long random strings)
/// replaced by `[redacted]`.
pub fn redact(text: &str) -> String {
    fn flush(word: &mut String, out: &mut String) {
        if looks_secret(word) {
            out.push_str("[redacted]");
        } else {
            out.push_str(word);
        }
        word.clear();
    }
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '=' | '+' | '/') {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// Defense in depth for what the allow-listed views already leave out: drops fields named
/// like secrets and redacts the strings that look like one.
pub fn scrub(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(redact(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(scrub).collect()),
        Value::Object(o) => {
            Value::Object(o.into_iter().filter(|(k, _)| !secret_key(k)).map(|(k, v)| (k, scrub(v))).collect())
        }
        v => v,
    }
}

// ----- results -----------------------------------------------------------------------------

fn text_result(text: String, is_error: bool) -> Value {
    let mut r = json!({"content": [{"type": "text", "text": text}]});
    if is_error {
        r["isError"] = json!(true);
    }
    r
}

/// A status answer (scrubbed).
fn status(v: Value) -> Value {
    text_result(scrub(v).to_string(), false)
}

fn fail(msg: impl Into<String>) -> Value {
    text_result(redact(&msg.into()), true)
}

fn pick(row: &Value, fields: &[&str]) -> Value {
    let mut o = Map::new();
    for f in fields {
        if let Some(v) = row.get(*f) {
            o.insert((*f).to_string(), v.clone());
        }
    }
    Value::Object(o)
}

fn short(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ----- views (explicit field lists) ---------------------------------------------------------------

/// A provider account without its credentials.
fn connection_view(c: &ProviderConnection, provider_name: &str, now: OffsetDateTime) -> Value {
    let locks = crate::dashboard::api::locks_of(c, now);
    let last_error = c.data.get("lastError").and_then(Value::as_str).map(|e| short(e, 300));
    json!({
        "id": c.id,
        "provider": c.provider,
        "providerName": provider_name,
        "name": c.name,
        "enabled": c.is_active,
        "priority": c.priority,
        "authType": c.auth_type,
        "testStatus": c.data.get("testStatus").and_then(Value::as_str),
        "lastError": last_error,
        "errorCode": c.data.get("errorCode"),
        "backoffLevel": c.data.get("backoffLevel"),
        "lastUsedAt": c.data.get("lastUsedAt").and_then(Value::as_str),
        "cooldown": !locks.is_empty(),
        "cooldowns": locks,
    })
}

fn skill_view(s: &Skill) -> Value {
    json!({
        "id": s.id,
        "name": s.name,
        "description": s.description,
        "enabled": s.enabled,
        "source": s.source.split('@').next().unwrap_or_default(),
        "files": s.files.iter().map(|f| json!({"path": f.path, "bytes": f.bytes})).collect::<Vec<_>>(),
        "bytes": s.bytes,
        "updatedAt": s.updated_at,
    })
}

fn server_view(state: &AppState, s: &crate::mcp::ServerConfig) -> Value {
    let cached = crate::mcp::cached_tools(&state.db, &s.id);
    let host = reqwest::Url::parse(&s.url).ok().and_then(|u| u.host_str().map(str::to_string));
    let signed_out = crate::mcp::auth::load(&state.db, &s.id).is_some_and(|a| a.needs_sign_in);
    let stats = state.mcp.stats(&s.id);
    let nested = cached.as_ref().and_then(|c| crate::mcp::nested::detect(s, &c.tools));
    json!({
        "id": s.id,
        "name": s.name,
        "description": s.description,
        "urlHost": host,
        "transport": s.transport,
        "enabled": s.enabled,
        "status": state.mcp.status(&s.id),
        "connectFailed": stats.connect_failed,
        "needsSignIn": signed_out,
        "toolCount": cached.as_ref().map(|c| c.tools.len()),
        "nestedGateway": nested.map(crate::mcp::nested::Adapter::label),
        "calls": stats.calls,
        "errors": stats.errors,
        "lastCallAt": stats.last_call_at,
    })
}

// ----- tool definitions ------------------------------------------------------------------------

/// The tools, named without the `zilong__` prefix.
pub fn tool_defs() -> Vec<Value> {
    let name = json!({"type": "string", "description": "Skill name (see list_skills)."});
    vec![
        json!({
            "name": "list_providers",
            "description": "The provider accounts (connections) the router uses: id, provider, name, enabled, last \
                error and cooldown state. Never includes keys or tokens.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"title": "List providers", "readOnlyHint": true}
        }),
        json!({
            "name": "list_models",
            "description": "Models the router can serve, per provider (only providers with an account or without \
                login, unless all is true), and the model aliases.",
            "inputSchema": {"type": "object", "properties": {
                "provider": {"type": "string", "description": "Only this provider (id or alias)."},
                "all": {"type": "boolean", "description": "Include providers without an account."}
            }},
            "annotations": {"title": "List models", "readOnlyHint": true}
        }),
        json!({
            "name": "usage_summary",
            "description": "Request, token and cost totals and the busiest providers and models of a period.",
            "inputSchema": {"type": "object", "properties": {
                "period": {"type": "string", "enum": ["today", "24h", "7d", "30d", "60d", "all"],
                    "description": "Default 24h."}
            }},
            "annotations": {"title": "Usage summary", "readOnlyHint": true}
        }),
        json!({
            "name": "list_mcp_servers",
            "description": "The MCP servers behind the gateway: name, host, status and tool counts. No headers, tokens \
                or client secrets.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"title": "List MCP servers", "readOnlyHint": true}
        }),
        json!({
            "name": "list_skills",
            "description": "Every skill, enabled or not, with its files and size, and the storage limits.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"title": "List skills", "readOnlyHint": true}
        }),
        json!({
            "name": "get_skill",
            "description": "A skill's SKILL.md and other text files, as stored.",
            "inputSchema": {"type": "object", "properties": {"name": name}, "required": ["name"]},
            "annotations": {"title": "Get skill", "readOnlyHint": true}
        }),
        json!({
            "name": "save_skill",
            "description": "Create a skill, or replace the one with the same name. skillMd is the SKILL.md text with \
                `name` and `description` in its frontmatter; files adds other text files (path -> text). The \
                dashboard's limits apply (1 MB per skill, 10 MB in total).",
            "inputSchema": {"type": "object", "properties": {
                "skillMd": {"type": "string", "description": "SKILL.md, frontmatter included."},
                "files": {"type": "object", "additionalProperties": {"type": "string"},
                    "description": "Other files: path relative to the skill folder -> text."},
                "name": {"type": "string", "description": "Overrides the frontmatter name."},
                "description": {"type": "string", "description": "Overrides the frontmatter description."}
            }, "required": ["skillMd"]},
            "annotations": {"title": "Save skill", "destructiveHint": true}
        }),
        json!({
            "name": "set_skill_enabled",
            "description": "Switch a skill on or off (only enabled skills are offered to clients).",
            "inputSchema": {"type": "object", "properties": {
                "name": name, "enabled": {"type": "boolean"}
            }, "required": ["name", "enabled"]},
            "annotations": {"title": "Enable or disable skill", "idempotentHint": true}
        }),
        json!({
            "name": "delete_skill",
            "description": "Delete a skill and its files.",
            "inputSchema": {"type": "object", "properties": {"name": name}, "required": ["name"]},
            "annotations": {"title": "Delete skill", "destructiveHint": true}
        }),
    ]
}

// ----- calls ------------------------------------------------------------------------------------------

/// Runs control tool `name` (the caller's access was checked by the gateway).
pub fn call(state: &AppState, name: &str, args: &Value) -> Value {
    match name {
        "list_providers" => list_providers(state),
        "list_models" => list_models(state, args),
        "usage_summary" => usage_summary(state, args),
        "list_mcp_servers" => {
            let list: Vec<Value> = crate::mcp::servers(&state.db).iter().map(|s| server_view(state, s)).collect();
            status(json!({"servers": list}))
        }
        "list_skills" => {
            let all = skills::list(&state.db);
            let used: usize = all.iter().map(|s| s.bytes).sum();
            let list: Vec<Value> = all.iter().map(skill_view).collect();
            status(json!({
                "skills": list,
                "usedBytes": used,
                "maxSkillBytes": skills::MAX_SKILL_BYTES,
                "maxTotalBytes": skills::MAX_TOTAL_BYTES,
            }))
        }
        "get_skill" => get_skill(state, args),
        "save_skill" => save_skill(state, args),
        "set_skill_enabled" => set_skill_enabled(state, args),
        "delete_skill" => delete_skill(state, args),
        _ => fail(format!("unknown tool: {SERVER}__{name}")),
    }
}

fn list_providers(state: &AppState) -> Value {
    let conns = match state.db.all_connections() {
        Ok(c) => c,
        Err(e) => return fail(format!("{e:#}")),
    };
    let now = OffsetDateTime::now_utc();
    let list: Vec<Value> = conns
        .iter()
        .map(|c| {
            let name = state.registry.get(&c.provider).map(|p| p.name().to_string());
            connection_view(c, &name.unwrap_or_else(|| c.provider.clone()), now)
        })
        .collect();
    status(json!({"connections": list}))
}

fn list_models(state: &AppState, args: &Value) -> Value {
    let only = args.get("provider").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
    let connected: HashSet<String> =
        state.db.all_connections().unwrap_or_default().into_iter().map(|c| c.provider).collect();
    let hidden = state.db.kv_scope("hiddenModels").unwrap_or_default();
    let mut out = Vec::new();
    for p in state.registry.all() {
        let wanted = match only {
            Some(o) => p.id.eq_ignore_ascii_case(o) || p.alias.eq_ignore_ascii_case(o),
            None => all || p.no_auth || connected.contains(&p.id),
        };
        if !wanted {
            continue;
        }
        let models: Vec<&str> = p
            .models
            .iter()
            .map(|m| m.id.as_str())
            .filter(|id| !crate::model::is_hidden(&hidden, &[p.id.as_str(), p.alias.as_str()], id))
            .collect();
        out.push(json!({
            "provider": p.id,
            "alias": p.alias,
            "name": p.name(),
            "accounts": state.db.connections_for(&p.id, false).map(|c| c.len()).unwrap_or(0),
            "models": models,
        }));
    }
    let aliases = state.db.kv_scope("modelAliases").unwrap_or_default();
    status(json!({"providers": out, "aliases": aliases}))
}

fn usage_summary(state: &AppState, args: &Value) -> Value {
    let period = args.get("period").and_then(Value::as_str).unwrap_or("24h");
    if !["today", "24h", "7d", "30d", "60d", "all"].contains(&period) {
        return fail("period must be one of today, 24h, 7d, 30d, 60d, all");
    }
    let (since, hourly) = crate::dashboard::api::period_since(period);
    let stats = match state.db.usage_stats(since.as_deref(), hourly) {
        Ok(s) => s,
        Err(e) => return fail(format!("{e:#}")),
    };
    let top = |group: &str, fields: &[&str]| -> Vec<Value> {
        stats
            .get(group)
            .and_then(Value::as_array)
            .map_or(Vec::new(), |rows| rows.iter().take(10).map(|r| pick(r, fields)).collect())
    };
    let numbers = ["requests", "promptTokens", "completionTokens", "cachedTokens", "cost", "lastUsed"];
    let by_provider: Vec<&str> = ["provider"].into_iter().chain(numbers).collect();
    let by_model: Vec<&str> = ["provider", "model"].into_iter().chain(numbers).collect();
    status(json!({
        "period": period,
        "totals": stats.get("totals").map(|t| pick(t, &[
            "totalRequests", "totalPromptTokens", "totalCompletionTokens", "totalCachedTokens", "totalCost", "totalErrors",
        ])),
        "byProvider": top("byProvider", &by_provider),
        "byModel": top("byModel", &by_model),
    }))
}

fn arg_str<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).map(str::trim).unwrap_or_default()
}

/// The skill called `name` (its name in any case, or its id).
fn find_skill(db: &Db, name: &str) -> Result<Skill, String> {
    if name.is_empty() {
        return Err("name is required (see list_skills)".into());
    }
    let id = skills::slug(name);
    skills::list(db)
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(name) || s.id == id)
        .ok_or_else(|| format!("unknown skill `{name}`: see list_skills"))
}

fn get_skill(state: &AppState, args: &Value) -> Value {
    let skill = match find_skill(&state.db, arg_str(args, "name")) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    // Skill content is the user's own text and is returned as stored.
    let content = skills::content(&state.db, &skill.id).unwrap_or_default();
    let mut v = skill_view(&skill);
    v["skillMd"] = json!(content.skill_md);
    v["fileContents"] = json!(content.files);
    text_result(v.to_string(), false)
}

fn save_skill(state: &AppState, args: &Value) -> Value {
    let skill_md = args.get("skillMd").and_then(Value::as_str).unwrap_or_default();
    if skill_md.trim().is_empty() {
        return fail("skillMd (the SKILL.md text) is required");
    }
    let mut files = BTreeMap::new();
    match args.get("files") {
        None | Some(Value::Null) => {}
        Some(Value::Object(o)) => {
            for (path, text) in o {
                let Some(text) = text.as_str() else { return fail(format!("file `{path}` must be text")) };
                files.insert(path.clone(), text.to_string());
            }
        }
        Some(_) => return fail("files must be an object of path -> text"),
    }
    let opt = |k: &str| Some(arg_str(args, k)).filter(|s| !s.is_empty());
    let (skill, content) = match skills::build(skill_md, files, "paste".into(), opt("name"), opt("description")) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    let old = skills::get(&state.db, &skill.id);
    match skills::store(&state.db, skill, &content, old.as_ref()) {
        Ok(saved) => status(json!({"saved": true, "created": old.is_none(), "skill": skill_view(&saved)})),
        Err(e) => fail(e),
    }
}

fn set_skill_enabled(state: &AppState, args: &Value) -> Value {
    let Some(on) = args.get("enabled").and_then(Value::as_bool) else {
        return fail("enabled (true or false) is required");
    };
    let mut skill = match find_skill(&state.db, arg_str(args, "name")) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    skill.enabled = on;
    skill.updated_at = now_iso();
    match skills::save_meta(&state.db, &skill) {
        Ok(()) => status(json!({"skill": skill_view(&skill)})),
        Err(e) => fail(format!("{e:#}")),
    }
}

fn delete_skill(state: &AppState, args: &Value) -> Value {
    let skill = match find_skill(&state.db, arg_str(args, "name")) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    match skills::delete(&state.db, &skill.id) {
        Ok(()) => status(json!({"deleted": skill.name})),
        Err(e) => fail(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ApiKey;

    #[test]
    fn redacts_secret_shapes_but_keeps_ids() {
        let text =
            "failed with sk-abcdefghijklmnop1234 and Bearer eyJhbGciOiJIUzI1NiJ9.abc.def at 2026-11-02T10:00:00Z";
        let out = redact(text);
        assert!(!out.contains("sk-abcdef") && !out.contains("eyJhbG"), "{out}");
        assert!(out.contains("2026-11-02T10:00:00Z"), "{out}");
        let long = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6";
        assert_eq!(redact(long), "[redacted]");
        let id = "123e4567-e89b-12d3-a456-426614174000";
        assert_eq!(redact(id), id);
        assert_eq!(redact("claude-3-5-sonnet-20241022"), "claude-3-5-sonnet-20241022");
    }

    #[test]
    fn scrub_drops_secret_fields() {
        let v = scrub(json!({"id": "x", "apiKey": "k", "providerSpecificData": {"accessToken": "t"},
            "headers": {"Authorization": "Bearer abc"}, "promptTokens": 5, "note": "sk-abcdefghijklmnopqrstuvwx"}));
        assert_eq!(v, json!({"id": "x", "providerSpecificData": {}, "promptTokens": 5, "note": "[redacted]"}));
    }

    #[test]
    fn connection_view_has_no_credentials() {
        let secret = "sk-live-0123456789abcdef0123456789abcdef";
        let c = ProviderConnection {
            id: "123e4567-e89b-12d3-a456-426614174000".into(),
            provider: "openai".into(),
            auth_type: "apikey".into(),
            name: Some("main".into()),
            email: Some("me@example.com".into()),
            priority: Some(1),
            is_active: true,
            data: json!({"apiKey": secret, "accessToken": secret, "refreshToken": secret, "testStatus": "active",
                "lastError": format!("bad key {secret}"), "providerSpecificData": {"baseUrl": "https://x.test"}}),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let out = scrub(connection_view(&c, "OpenAI", OffsetDateTime::now_utc())).to_string();
        assert!(!out.contains(secret) && !out.contains("0123456789abcdef"), "{out}");
        assert!(!out.contains("me@example.com") && !out.contains("x.test"), "{out}");
        assert!(out.contains("\"enabled\":true") && out.contains("main"), "{out}");
    }

    #[test]
    fn access_is_per_key() {
        let path = std::env::temp_dir().join(format!("zilong-control-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        let key = ApiKey {
            id: "k1".into(),
            key: "sk-test".into(),
            name: None,
            is_active: true,
            created_at: String::new(),
            access_restricted: false,
            access_allow: vec![],
        };
        let caller = Caller::Key(key);
        assert!(!allowed(&db, &caller), "off by default");
        assert!(!allowed(&db, &Caller::Local));
        set_key_allowed(&db, "k1", true).unwrap();
        assert!(allowed(&db, &caller));
        set_key_allowed(&db, "k1", false).unwrap();
        assert!(!allowed(&db, &caller));
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
