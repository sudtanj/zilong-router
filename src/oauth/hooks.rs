//! Request-time quirks of the OAuth and free providers: extra headers (upstream
//! `executors/default.js` HEADER_HOOKS), GitHub Copilot routing and body fixes
//! (`executors/github.js`), the MiMo free-tier JWT (`executors/mimo-free.js`) and
//! Claude Code identity for Claude OAuth tokens (`utils/claudeCloaking.js`). OpenCode
//! Free lives in `opencode.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use anyhow::{bail, Result};
use axum::http::HeaderMap;
use parking_lot::Mutex;
use serde_json::{json, Value};

use super::{opencode, util};
use crate::db::ProviderConnection;
use crate::providers::{Format, Provider, Transport};
use crate::sse::Event;
use crate::state::AppState;

const GITHUB_MESSAGES: &str = "https://api.githubcopilot.com/v1/messages";
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

pub const CLAUDE_SYSTEM_PROMPT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const CLAUDE_CLI_VERSION: &str = "2.1.280";
const CLAUDE_TOOL_SUFFIX: &str = "_ide";
/// Claude Code's own tool names, offered as unavailable decoys next to the client's.
const CC_DECOY_TOOLS: [&str; 20] = [
    "Task",
    "TaskOutput",
    "TaskStop",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
    "Bash",
    "Glob",
    "Grep",
    "Read",
    "Edit",
    "Write",
    "NotebookEdit",
    "WebFetch",
    "WebSearch",
    "AskUserQuestion",
    "Skill",
    "EnterPlanMode",
    "ExitPlanMode",
];
const HEAVY_AGENT_BETAS: [&str; 2] = ["advanced-tool-use-2025-11-20", "effort-2025-11-24"];
const REDACT_THINKING_BETA: &str = "redact-thinking-2026-02-12";

pub fn is_mimo(provider: &str) -> bool {
    provider == "mimo-free" || provider == "mmf"
}

fn is_claude_oauth(conn: &ProviderConnection) -> bool {
    conn.secret().is_some_and(|t| t.contains("sk-ant-oat"))
}

/// Registry fix-ups for providers whose behaviour upstream keeps in code.
pub fn patch_registry(providers: &mut [Provider]) {
    for p in providers.iter_mut() {
        if is_mimo(&p.id) {
            p.no_auth = true;
        }
        if p.id == "github" {
            if let Some(t) = p.transport.clone() {
                let mut claude = t.clone();
                claude.base_url = GITHUB_MESSAGES.into();
                claude.format = Format::Claude;
                p.transports = Some(vec![Transport { format: Format::Openai, ..t }, claude]);
            }
        }
        if p.id == opencode::ID {
            opencode::patch(p);
        }
    }
}

/// Endpoint chosen by model rather than by client format. GitHub serves Claude
/// models on its Anthropic `/v1/messages` shim (the only Copilot endpoint that
/// reports prompt-cache tokens), everything else on chat completions; OpenCode
/// serves `union-alpha` on `/messages` only.
pub fn transport_for<'a>(p: &'a Provider, model: &str) -> Option<&'a Transport> {
    match p.id.as_str() {
        "github" => {
            let want = if model.to_ascii_lowercase().contains("claude") { Format::Claude } else { Format::Openai };
            p.transports.as_ref()?.iter().find(|t| t.format == want)
        }
        opencode::ID => opencode::transport_for(p, model),
        _ => None,
    }
}

/// Tool names the provider hook changed, to be put back in the response.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Restore {
    #[default]
    None,
    /// Claude OAuth cloaking: strip the `_ide` suffix.
    ClaudeSuffix,
    /// Sent name -> the caller's name (OpenCode fingerprint conflicts).
    Names(HashMap<String, String>),
}

impl Restore {
    fn original<'a>(&'a self, sent: &'a str) -> Option<&'a str> {
        match self {
            Restore::None => None,
            Restore::ClaudeSuffix => sent.strip_suffix(CLAUDE_TOOL_SUFFIX).filter(|n| !n.is_empty()),
            Restore::Names(m) => m.get(sent).map(String::as_str),
        }
    }

    /// Whether `data` may mention a changed name (cheap pre-check for stream events).
    fn mentioned_in(&self, data: &str) -> bool {
        match self {
            Restore::None => false,
            Restore::ClaudeSuffix => data.contains(CLAUDE_TOOL_SUFFIX),
            Restore::Names(m) => m.keys().any(|k| data.contains(k.as_str())),
        }
    }
}

fn set(h: &mut HashMap<String, String>, k: &str, v: impl Into<String>) {
    h.retain(|x, _| !x.eq_ignore_ascii_case(k));
    h.insert(k.to_string(), v.into());
}

fn has(h: &HashMap<String, String>, k: &str) -> bool {
    h.keys().any(|x| x.eq_ignore_ascii_case(k))
}

fn psd_str<'a>(c: &'a ProviderConnection, k: &str) -> Option<&'a str> {
    c.data.pointer(&format!("/providerSpecificData/{k}")).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Per-provider headers, applied after the registry headers and auth.
pub fn headers(
    provider: &Provider,
    conn: &ProviderConnection,
    client: &HeaderMap,
    model: &str,
    body: &Value,
    h: &mut HashMap<String, String>,
) {
    match provider.id.as_str() {
        opencode::ID => opencode::headers(client, model, h),
        "kimi" => {
            for (k, v) in util::kimi_headers(psd_str(conn, "deviceId")) {
                set(h, k, v);
            }
        }
        "minimax-code" | "minimax-code-global" => {
            // A placeholder key rides next to the Bearer token, plus session headers.
            set(h, "x-api-key", "sk-xxx");
            set(h, "X-Mavis-Timezone-Offset", "0");
            set(h, "X-Mavis-Session-Id", uuid::Uuid::new_v4().to_string());
            if !has(h, "anthropic-version") {
                set(h, "anthropic-version", ANTHROPIC_VERSION);
            }
        }
        // Subscription (minted) keys need the API version; plain API keys don't.
        "muse" if conn.auth_type == "oauth" => set(h, "x-api-version", "1.0.0"),
        "cline" | "clinepass" => {
            let v = env!("CARGO_PKG_VERSION");
            for (k, val) in [
                ("HTTP-Referer", "https://cline.bot".to_string()),
                ("X-Title", "Cline".into()),
                ("User-Agent", format!("9Router/{v}")),
                ("X-PLATFORM", "linux".into()),
                ("X-PLATFORM-VERSION", "v24.14.0".into()),
                ("X-CLIENT-TYPE", "9router".into()),
                ("X-CLIENT-VERSION", v.into()),
                ("X-CORE-VERSION", v.into()),
                ("X-IS-MULTIROOT", "false".into()),
            ] {
                set(h, k, val);
            }
            if let Some(t) = conn.secret() {
                set(h, "Authorization", format!("Bearer {}", cline_token(t)));
            }
        }
        "kilocode" => {
            if let Some(org) = conn.data.pointer("/providerSpecificData/orgId").filter(|v| !v.is_null()) {
                let org = org.as_str().map(str::to_string).unwrap_or_else(|| org.to_string());
                set(h, "X-Kilocode-OrganizationID", org);
            }
        }
        "github" => {
            let token = psd_str(conn, "copilotToken").or_else(|| conn.secret()).unwrap_or("");
            set(h, "Authorization", format!("Bearer {token}"));
            set(h, "x-request-id", uuid::Uuid::new_v4().to_string());
            set(h, "anthropic-version", ANTHROPIC_VERSION);
        }
        "claude" => {
            claude_betas(h, model, body);
            if is_claude_oauth(conn) && !has(h, "x-claude-code-session-id") {
                if let Some(sid) = claude_session_id(body) {
                    set(h, "x-claude-code-session-id", sid);
                }
            }
        }
        p if is_mimo(p) => {
            let jwt = conn.data.get("accessToken").and_then(Value::as_str).unwrap_or("");
            set(h, "Authorization", format!("Bearer {jwt}"));
            set(h, "X-Mimo-Source", "mimocode-cli-free");
            set(h, "User-Agent", MIMO_USER_AGENTS[rand::random::<usize>() % MIMO_USER_AGENTS.len()]);
            set(h, "x-session-affinity", MIMO_SESSION.as_str());
        }
        _ => {}
    }
}

/// Cline OAuth tokens are WorkOS JWTs sent as `workos:<jwt>`; API keys go verbatim.
fn cline_token(t: &str) -> String {
    let t = t.trim();
    if t.to_ascii_lowercase().starts_with("workos:") || !util::is_jwt(t) {
        t.to_string()
    } else {
        format!("workos:{t}")
    }
}

/// Heavy-agent betas only for Opus/Sonnet; keep thinking summaries when asked for.
fn claude_betas(h: &mut HashMap<String, String>, model: &str, body: &Value) {
    let Some(key) = h.keys().find(|k| k.eq_ignore_ascii_case("anthropic-beta")).cloned() else { return };
    let heavy = model.starts_with("claude-opus") || model.starts_with("claude-sonnet");
    let summaries = body.pointer("/thinking/display").and_then(Value::as_str) == Some("summarized");
    let mut flags: Vec<&str> = Vec::new();
    for f in h[&key].split(',').map(str::trim).filter(|f| !f.is_empty()) {
        let drop = (!heavy && HEAVY_AGENT_BETAS.contains(&f)) || (summaries && f == REDACT_THINKING_BETA);
        if !drop && !flags.contains(&f) {
            flags.push(f);
        }
    }
    if heavy {
        for f in HEAVY_AGENT_BETAS {
            if !flags.contains(&f) {
                flags.push(f);
            }
        }
    }
    let joined = flags.join(",");
    h.insert(key, joined);
}

fn claude_session_id(body: &Value) -> Option<String> {
    let uid = body.pointer("/metadata/user_id").and_then(Value::as_str)?;
    let sid = if uid.starts_with('{') {
        serde_json::from_str::<Value>(uid).ok()?.get("session_id")?.as_str()?.to_string()
    } else {
        uid.to_string()
    };
    let sid = sid.trim_start_matches("claude:").trim().to_string();
    (!sid.is_empty()).then_some(sid)
}

/// Provider-specific body rewrites, applied per account right before sending.
/// Returns the new body and which tool names in the response must be restored.
pub fn body(
    provider: &str,
    conn: &ProviderConnection,
    target: Format,
    model: &str,
    body: &Value,
) -> Option<(Value, Restore)> {
    match provider {
        "claude" if target == Format::Claude && is_claude_oauth(conn) => {
            let mut b = body.clone();
            claude_identity(&mut b, conn.secret().unwrap_or(""));
            let cloaked = cloak_tools(&mut b);
            Some((b, if cloaked { Restore::ClaudeSuffix } else { Restore::None }))
        }
        "github" if target == Format::Openai => {
            let mut b = body.clone();
            github_body(model, &mut b);
            Some((b, Restore::None))
        }
        p if is_mimo(p) && target == Format::Openai => {
            let mut b = body.clone();
            mimo_marker(&mut b);
            Some((b, Restore::None))
        }
        opencode::ID => {
            let mut b = body.clone();
            let renamed = opencode::body(&mut b, target);
            Some((b, if renamed.is_empty() { Restore::None } else { Restore::Names(renamed) }))
        }
        _ => None,
    }
}

fn text(b: &Value) -> &str {
    b.get("text").and_then(Value::as_str).unwrap_or("")
}

/// What Anthropic expects from Claude Code: its system prompt, the billing header
/// block first, and a `metadata.user_id` stable per account.
fn claude_identity(body: &mut Value, token: &str) {
    let hash = util::sha256_hex(body.to_string().as_bytes());
    let mut blocks: Vec<Value> = match body.get("system") {
        Some(Value::String(s)) if !s.is_empty() => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    if !blocks.iter().any(|b| text(b).contains("You are Claude Code")) {
        let at = usize::from(blocks.first().is_some_and(|b| text(b).starts_with("x-anthropic-billing-header:")));
        blocks.insert(at, json!({"type": "text", "text": CLAUDE_SYSTEM_PROMPT}));
    }
    if !blocks.first().is_some_and(|b| text(b).starts_with("x-anthropic-billing-header:")) {
        let build = &util::random_hex(2)[..3];
        let billing = format!(
            "x-anthropic-billing-header: cc_version={CLAUDE_CLI_VERSION}.{build}; cc_entrypoint=sdk-cli; cch={};",
            &hash[..5]
        );
        blocks.insert(0, json!({"type": "text", "text": billing}));
    }
    body["system"] = Value::Array(blocks);
    if body.pointer("/metadata/user_id").is_none() {
        if !body.get("metadata").is_some_and(Value::is_object) {
            body["metadata"] = json!({});
        }
        body["metadata"]["user_id"] = json!(fake_user_id(token));
    }
}

/// `{"device_id","account_uuid","session_id"}` as current Claude Code sends it; the
/// first two derive from the token so they stay stable per account.
fn fake_user_id(token: &str) -> String {
    let device = util::sha256_hex(format!("device:{token}").as_bytes());
    let h = util::sha256_hex(format!("account:{token}").as_bytes());
    let variant = (u8::from_str_radix(&h[16..17], 16).unwrap_or(0) & 0x3) | 0x8;
    let account = format!("{}-{}-4{}-{:x}{}-{}", &h[0..8], &h[8..12], &h[13..16], variant, &h[17..20], &h[20..32]);
    let session = uuid::Uuid::new_v4();
    format!(r#"{{"device_id":"{device}","account_uuid":"{account}","session_id":"{session}"}}"#)
}

/// Suffix client tool names and add Claude Code's tools as decoys (upstream's
/// anti-ban cloaking). Built-in tools (those with a `type`) keep their names.
fn cloak_tools(body: &mut Value) -> bool {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut).filter(|t| !t.is_empty()) else {
        return false;
    };
    let mut renamed: HashSet<String> = HashSet::new();
    for t in tools.iter_mut() {
        if t.get("type").is_some() {
            continue;
        }
        if let Some(n) = t.get("name").and_then(Value::as_str).map(str::to_string) {
            t["name"] = json!(format!("{n}{CLAUDE_TOOL_SUFFIX}"));
            renamed.insert(n);
        }
    }
    tools.extend(CC_DECOY_TOOLS.iter().map(|n| {
        json!({"name": n, "description": "This tool is currently unavailable.", "input_schema": {"type": "object", "properties": {}}})
    }));
    for m in body.get_mut("messages").and_then(Value::as_array_mut).into_iter().flatten() {
        for b in m.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let Some(n) = b.get("name").and_then(Value::as_str).map(str::to_string) {
                    b["name"] = json!(format!("{n}{CLAUDE_TOOL_SUFFIX}"));
                }
            }
        }
    }
    if let Some(tc) = body.get_mut("tool_choice").filter(|tc| tc.get("type").and_then(Value::as_str) == Some("tool")) {
        if let Some(n) = tc.get("name").and_then(Value::as_str).filter(|n| renamed.contains(*n)).map(str::to_string) {
            tc["name"] = json!(format!("{n}{CLAUDE_TOOL_SUFFIX}"));
        }
    }
    !renamed.is_empty()
}

/// Restore tool names in an upstream stream event (still in the upstream format).
pub fn decloak_event(ev: &mut Event, restore: &Restore) {
    if !restore.mentioned_in(&ev.data) {
        return;
    }
    let Ok(mut v) = serde_json::from_str::<Value>(&ev.data) else { return };
    if decloak_json(&mut v, restore) {
        ev.data = v.to_string();
    }
}

/// Restore tool names in a Claude event or message, or an OpenAI chunk or completion.
/// Returns whether anything changed.
pub fn decloak_json(v: &mut Value, restore: &Restore) -> bool {
    let mut changed = false;
    let mut fix = |slot: Option<&mut Value>| {
        let Some(slot) = slot else { return };
        if let Some(n) = slot.as_str().and_then(|s| restore.original(s)).map(str::to_string) {
            *slot = json!(n);
            changed = true;
        }
    };
    let is_tool_use = |b: &Value| b.get("type").and_then(Value::as_str) == Some("tool_use");
    // Claude: streamed content_block_start, complete message content.
    if v.get("type").and_then(Value::as_str) == Some("content_block_start") && is_tool_use(&v["content_block"]) {
        fix(v.pointer_mut("/content_block/name"));
    }
    for b in v.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
        if is_tool_use(b) {
            fix(b.get_mut("name"));
        }
    }
    // OpenAI: tool calls in stream deltas and complete messages.
    for c in v.get_mut("choices").and_then(Value::as_array_mut).into_iter().flatten() {
        for holder in ["delta", "message"] {
            let calls = c.get_mut(holder).and_then(|h| h.get_mut("tool_calls")).and_then(Value::as_array_mut);
            for call in calls.into_iter().flatten() {
                fix(call.pointer_mut("/function/name"));
            }
        }
    }
    changed
}

/// Response fix-ups: Cline wraps non-streamed answers in `{success, data}`.
pub fn fix_response(provider: &str, v: &mut Value) {
    if provider == "cline" || provider == "clinepass" {
        if v.get("choices").is_none() && v.pointer("/data/choices").is_some() {
            *v = v["data"].take();
        }
    }
}

/// Copilot chat completions: newer OpenAI models want `max_completion_tokens`, and
/// only text / image_url content parts are accepted.
fn github_body(model: &str, body: &mut Value) {
    let m = model.to_ascii_lowercase();
    let newer = m.contains("gpt-5") || ["o1-", "o3-", "o4-"].iter().any(|p| m.contains(p));
    if let Some(o) = body.as_object_mut() {
        if newer {
            if let Some(mt) = o.remove("max_tokens") {
                o.insert("max_completion_tokens".into(), mt);
            }
        }
        if o.get("reasoning_effort").and_then(Value::as_str) == Some("none") {
            o.remove("reasoning_effort");
        }
    }
    for msg in body.get_mut("messages").and_then(Value::as_array_mut).into_iter().flatten() {
        let Some(parts) = msg.get_mut("content").and_then(Value::as_array_mut) else { continue };
        let cleaned: Vec<Value> = parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text" | "image_url") => Some(p.clone()),
                _ => {
                    let t = p
                        .get("text")
                        .or_else(|| p.get("content"))
                        .map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))
                        .unwrap_or_else(|| p.to_string());
                    (!t.is_empty()).then(|| json!({"type": "text", "text": t}))
                }
            })
            .filter(|p| p.get("text").and_then(Value::as_str) != Some(""))
            .collect();
        msg["content"] = if cleaned.is_empty() { Value::Null } else { Value::Array(cleaned) };
    }
}

// ----- MiMo free tier ----------------------------------------------------------------

const MIMO_BOOTSTRAP: &str = "https://api.xiaomimimo.com/api/free-ai/bootstrap";
const MIMO_SYSTEM_MARKER: &str =
    "You are MiMoCode, an interactive CLI tool that helps users with software engineering tasks.";
const MIMO_USER_AGENTS: [&str; 3] = [
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
];

static MIMO_SESSION: LazyLock<String> = LazyLock::new(|| {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let tail: String = (0..24).map(|_| CHARS[rand::random::<usize>() % CHARS.len()] as char).collect();
    format!("ses_{tail}")
});
/// (jwt, expires at unix seconds)
static MIMO_JWT: LazyLock<Mutex<Option<(String, i64)>>> = LazyLock::new(|| Mutex::new(None));

fn mimo_marker(body: &mut Value) {
    let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) else { return };
    let present = msgs.iter().any(|m| {
        m.get("role").and_then(Value::as_str) == Some("system")
            && m.get("content").and_then(Value::as_str).is_some_and(|c| c.contains(MIMO_SYSTEM_MARKER))
    });
    if !present {
        msgs.insert(0, json!({"role": "system", "content": MIMO_SYSTEM_MARKER}));
    }
}

/// The free tier hands out a JWT per machine fingerprint; cached until 5 min before
/// it expires, or dropped with `reset` after a 401/403.
pub async fn mimo_jwt(state: &AppState, reset: bool) -> Result<String> {
    if reset {
        *MIMO_JWT.lock() = None;
    }
    let cached = MIMO_JWT.lock().clone();
    if let Some((jwt, exp)) = cached {
        if util::unix_now() < exp - 300 {
            return Ok(jwt);
        }
    }
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|c| {
            c.lines()
                .find(|l| l.starts_with("model name") || l.starts_with("Model") || l.starts_with("Hardware"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown-cpu".into());
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown-user".into());
    let seed = format!("{}|linux|{}|{cpu}|{user}", util::hostname(), util::node_arch());
    let resp = state
        .http
        .post(MIMO_BOOTSTRAP)
        .header("User-Agent", MIMO_USER_AGENTS[rand::random::<usize>() % MIMO_USER_AGENTS.len()])
        .json(&json!({"client": util::sha256_hex(seed.as_bytes())}))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("MiMo bootstrap failed: {}", resp.status());
    }
    let v: Value = resp.json().await?;
    let Some(jwt) = v.get("jwt").and_then(Value::as_str).map(str::to_string) else {
        bail!("MiMo bootstrap returned no JWT");
    };
    let exp = util::jwt_payload(&jwt)
        .and_then(|p| p.get("exp").and_then(Value::as_i64))
        .unwrap_or_else(|| util::unix_now() + 3000);
    *MIMO_JWT.lock() = Some((jwt.clone(), exp));
    Ok(jwt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(provider: &str, data: Value) -> ProviderConnection {
        ProviderConnection {
            id: "c".into(),
            provider: provider.into(),
            auth_type: "oauth".into(),
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
    fn claude_oauth_gets_identity_and_cloaked_tools() {
        let c = conn("claude", json!({"accessToken": "sk-ant-oat01-x"}));
        let body = json!({
            "system": "be brief",
            "messages": [{"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "lookup", "input": {}}]}],
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}, {"type": "web_search_20250305", "name": "web_search"}],
            "tool_choice": {"type": "tool", "name": "lookup"},
        });
        let (b, cloaked) = super::body("claude", &c, Format::Claude, "claude-sonnet-5", &body).unwrap();
        assert_eq!(cloaked, Restore::ClaudeSuffix);
        assert!(text(&b["system"][0]).starts_with("x-anthropic-billing-header: cc_version="));
        assert_eq!(text(&b["system"][1]), CLAUDE_SYSTEM_PROMPT);
        assert_eq!(text(&b["system"][2]), "be brief");
        assert_eq!(b["tools"][0]["name"], "lookup_ide");
        assert_eq!(b["tools"][1]["name"], "web_search");
        assert_eq!(b["tools"].as_array().unwrap().len(), 2 + CC_DECOY_TOOLS.len());
        assert_eq!(b["messages"][0]["content"][0]["name"], "lookup_ide");
        assert_eq!(b["tool_choice"]["name"], "lookup_ide");
        let sid = claude_session_id(&b).unwrap();
        assert_eq!(sid.len(), 36);
        // An API-key account is left alone.
        let k = conn("claude", json!({"apiKey": "sk-ant-api03-x"}));
        assert!(super::body("claude", &k, Format::Claude, "m", &body).is_none());

        let mut ev = Event::data(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t","name":"lookup_ide","input":{}}}"#,
        );
        decloak_event(&mut ev, &Restore::ClaudeSuffix);
        assert!(ev.data.contains(r#""name":"lookup""#));
        let mut doc = json!({"content": [{"type": "tool_use", "name": "Read_ide"}, {"type": "text", "text": "x_ide"}]});
        assert!(decloak_json(&mut doc, &Restore::ClaudeSuffix));
        assert_eq!(doc["content"][0]["name"], "Read");
    }

    #[test]
    fn provider_headers_and_bodies() {
        let mut h = HashMap::from([("Anthropic-Beta".to_string(), format!("a,{}", HEAVY_AGENT_BETAS[0]))]);
        claude_betas(&mut h, "claude-haiku-4-5", &json!({}));
        assert_eq!(h["Anthropic-Beta"], "a");
        claude_betas(&mut h, "claude-opus-5", &json!({}));
        assert!(h["Anthropic-Beta"].contains(HEAVY_AGENT_BETAS[1]));

        let reg = crate::providers::Registry::load(None).unwrap();
        let mut h = HashMap::from([("authorization".to_string(), "Bearer old".to_string())]);
        if let Some(gh) = reg.get("github") {
            let c = conn("github", json!({"accessToken": "gho", "providerSpecificData": {"copilotToken": "cp"}}));
            headers(gh, &c, &HeaderMap::new(), "gpt-4o", &json!({}), &mut h);
            assert_eq!(h.get("Authorization").map(String::as_str), Some("Bearer cp"));
            assert!(!h.contains_key("authorization"));
            assert_eq!(gh.transport_for(Format::Claude, "gpt-4o").unwrap().format, Format::Openai);
            assert_eq!(gh.transport_for(Format::Openai, "claude-sonnet-5").unwrap().format, Format::Claude);
        }

        let mut b = json!({"max_tokens": 5, "reasoning_effort": "none",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "tool_result", "content": "r"}]}]});
        github_body("gpt-5", &mut b);
        assert_eq!(b["max_completion_tokens"], 5);
        assert!(b.get("reasoning_effort").is_none());
        assert_eq!(b["messages"][0]["content"][1], json!({"type": "text", "text": "r"}));

        let mut m = json!({"messages": [{"role": "user", "content": "hi"}]});
        mimo_marker(&mut m);
        mimo_marker(&mut m);
        assert_eq!(m["messages"].as_array().unwrap().len(), 2);
        assert_eq!(cline_token("eyJa.b.c"), "workos:eyJa.b.c");
        assert_eq!(cline_token("clp_123"), "clp_123");
        let mut env = json!({"success": true, "data": {"choices": []}});
        fix_response("clinepass", &mut env);
        assert!(env.get("choices").is_some());
    }
}
