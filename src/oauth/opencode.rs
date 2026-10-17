//! OpenCode Free (upstream `executors/opencode.js`, `utils/opencodeFingerprint.js`):
//! keyless access to opencode.ai's free models, as long as requests look like the
//! OpenCode v2 CLI: its user agent and session headers, always streamed, and its
//! `read` / `shell` tool definitions declared next to the caller's own tools.
//!
//! Chat models use `/inference/v1/chat/completions`, `union-alpha` uses the Anthropic
//! `/inference/v1/messages`. The Muse Spark models need the OpenAI Responses API,
//! which zilong-router does not speak, so they are dropped from the catalogue.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::providers::{Format, Provider, Transport};

pub const ID: &str = "opencode";
/// Chat endpoint path; the provider's base URL is the host root.
const CHAT_PATH: &str = "/inference/v1/chat/completions";
/// OpenCode v2 UA shape; the v1 "opencode/1.18.x" form is rejected with 403 FreeTierError.
pub const USER_AGENT: &str = "opencode/latest/2.0.26/cli";
const MESSAGES_MODELS: [&str; 1] = ["union-alpha"];
/// Free models whose id lacks the "-free" suffix.
const KNOWN_FREE: [&str; 1] = ["big-pickle"];
/// Free chat models from the opencode.ai/docs/zen pricing table (2026-10-17). Some are not in
/// the live list, so both are offered (and these alone when it cannot be fetched).
const DOCUMENTED_FREE: [&str; 13] = [
    "big-pickle",
    "glm-5.3-flash-free",
    "deepseek-v4.1-flash-free",
    "space-bunny-free",
    "beam-free",
    "ll-alpha-free",
    "longcat-2.5-preview-free",
    "exo-free",
    "mimo-v2.6-flash-free",
    "mimo-v2.5-free",
    "nemotron-3-ultra-free",
    "nemotron-3.5-lightning-free",
    "nemotron-3.5-super-vl-free",
];
/// Free model families served by OpenCode's own `/inference/systemone` protocol, not chat.
const OTHER_PROTOCOL: [&str; 1] = ["jev-"];
/// Listed but answering "Model is unavailable" upstream.
const DEAD_FREE: [&str; 1] = ["deepseek-v4-flash-free"];
const SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_SESSION_LEN: usize = 256;
const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const PREFIX_MASK: u64 = (1 << 48) - 1;
const TIME_MASK: u64 = (1 << 36) - 1;

/// Tool names the free-tier gate requires, with OpenCode v2.0.26's exact definitions.
/// Upstream compares description and parameters, so do not reword these.
const FINGERPRINT: [&str; 2] = ["read", "shell"];

static DEFINITIONS: LazyLock<[(String, Value); 2]> = LazyLock::new(|| {
    [
        (
            "Read the contents of a file or directory. Supports text files, images, and PDFs. Images and PDFs are presented directly to the model. Each text line is prefixed by its 1-based line number as <line>: <content>. The prefix is for reference and is not part of the file content. Directory entries are returned one per line. Use offset and limit to read large files or directories in sections. Prefer one larger read over many small slices, and use grep to find specific content in large files.".into(),
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File or directory to read"},
                    "offset": {"type": "integer", "minimum": 0, "description": "The line or directory entry to start reading from (1-based)"},
                    "limit": {"type": "integer", "minimum": 0, "description": "The maximum number of lines or directory entries to read (defaults to and capped at 2000)"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        (
            "Execute a shell command and return its output. Commands run on Linux using bash. Quote file paths containing spaces or special characters. Prefer dedicated tools over shell commands when possible. When output is large, the full result is saved to a file and a truncated preview is returned. Rely on automatic truncation unless filtering the output is more useful. Commands accept an optional timeout, background commands have no timeout by default. Background commands return immediately, and you will be notified when they complete.".into(),
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command string to execute"},
                    "workdir": {"type": "string", "description": "Working directory to execute the command in. Defaults to the current working directory. When possible, avoid changing directories in the command and set the working directory here instead."},
                    "timeout": {"type": "integer", "minimum": 0, "description": "Timeout in milliseconds. Set to 0 to disable the timeout. Defaults to 120000 for foreground commands. Background commands have no timeout by default."},
                    "background": {"type": "boolean", "description": "Run the command in the background and return immediately (useful for dev servers and long-running builds). You do not need to use '&' at the end of the command when using this parameter. You will be notified when it completes. DO NOT poll for completion."}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
    ]
});

fn definition(name: &str) -> &'static (String, Value) {
    &DEFINITIONS[usize::from(name == "shell")]
}

pub fn is_messages_model(model: &str) -> bool {
    MESSAGES_MODELS.contains(&base_model(model))
}

/// Strip a thinking suffix: "model(high)" -> "model".
fn base_model(model: &str) -> &str {
    let m = model.trim();
    match (m.ends_with(')'), m.rfind('(')) {
        (true, Some(i)) => m[..i].trim(),
        _ => m,
    }
}

fn is_muse_spark(model: &str) -> bool {
    let m = base_model(model).rsplit('/').next().unwrap_or("").to_ascii_lowercase();
    let rest = m.strip_prefix("muse").map(|r| r.strip_prefix(['-', '_']).unwrap_or(r));
    match rest.and_then(|r| r.strip_prefix("spark")) {
        Some(tail) => tail.is_empty() || tail.starts_with(['-', '_', ':', '.', ' ']),
        None => false,
    }
}

/// Endpoints per wire format, all forced to stream (non-streamed free requests get 403).
pub fn patch(p: &mut Provider) {
    p.no_auth = true;
    p.models.retain(|m| m.target_format.map_or(true, |f| f.supported()) && !is_muse_spark(&m.id));
    let Some(t) = p.transport.clone() else { return };
    let root = t.base_url.trim_end_matches('/').to_string();
    let chat =
        Transport { base_url: format!("{root}{CHAT_PATH}"), format: Format::Openai, force_stream: true, ..t.clone() };
    let messages = Transport {
        base_url: format!("{root}/inference/v1/messages"),
        format: Format::Claude,
        force_stream: true,
        ..t
    };
    p.transport = Some(chat.clone());
    p.transports = Some(vec![chat, messages]);
}

/// The free model list (upstream `modelsFetcher`) on the provider's host, normally
/// `https://opencode.ai/zen/v1/models`.
pub fn models_url(p: &Provider) -> Option<String> {
    let chat = p.transport.as_ref()?.base_url.as_str();
    Some(format!("{}/zen/v1/models", chat.strip_suffix(CHAT_PATH).unwrap_or(chat)))
}

pub fn transport_for<'a>(p: &'a Provider, model: &str) -> Option<&'a Transport> {
    let want = if is_messages_model(model) { Format::Claude } else { Format::Openai };
    p.transports.as_ref()?.iter().find(|t| t.format == want)
}

/// Free chat models from the `/zen/v1/models` id list (upstream "opencode-free" filter).
pub fn free_models(ids: Vec<String>) -> Vec<String> {
    ids.into_iter()
        .filter(|id| (id.ends_with("-free") || KNOWN_FREE.contains(&id.as_str())) && !DEAD_FREE.contains(&id.as_str()))
        .filter(|id| !is_muse_spark(id) && !OTHER_PROTOCOL.iter().any(|p| id.starts_with(p)))
        .collect()
}

/// The live free list followed by documented free models it lacks.
pub fn with_documented(mut live: Vec<String>) -> Vec<String> {
    for id in DOCUMENTED_FREE {
        if !live.iter().any(|m| m == id) {
            live.push(id.to_string());
        }
    }
    live
}

// ----- Headers and sessions ---------------------------------------------------------

fn client_header<'a>(client: &'a HeaderMap, k: &str) -> Option<&'a str> {
    client.get(k).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty())
}

/// "opencode/<channel>/<major>.<minor>.<patch>/<client>" with major >= 2.
fn is_v2_user_agent(ua: &str) -> bool {
    let parts: Vec<&str> = ua.split('/').collect();
    let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if parts.len() != 4 || !parts[0].eq_ignore_ascii_case("opencode") || !word(parts[1]) || !word(parts[3]) {
        return false;
    }
    let v: Vec<&str> = parts[2].split('.').collect();
    v.len() == 3
        && v.iter().all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        && v[0].parse::<u32>().is_ok_and(|major| major >= 2)
}

/// Headers as the v2 CLI sends them. Upstream wants x-opencode-session-id next to
/// x-opencode-session and rejects the v1-only x-opencode-request.
pub fn headers(client: &HeaderMap, model: &str, h: &mut HashMap<String, String>) {
    let session = session_for(client);
    let ua = client_header(client, "user-agent").filter(|ua| is_v2_user_agent(ua)).unwrap_or(USER_AGENT);
    h.retain(|k, _| !k.eq_ignore_ascii_case("x-opencode-client"));
    for (k, v) in [
        ("Authorization", "Bearer public"),
        ("User-Agent", ua),
        ("x-opencode-client", client_header(client, "x-opencode-client").unwrap_or("cli")),
        ("x-opencode-project", client_header(client, "x-opencode-project").unwrap_or("global")),
    ] {
        h.insert(k.into(), v.into());
    }
    for k in ["x-opencode-session", "x-opencode-session-id", "x-session-affinity", "x-session-id"] {
        h.insert(k.into(), session.clone());
    }
    if is_messages_model(model) {
        h.insert("anthropic-version".into(), super::hooks::ANTHROPIC_VERSION.into());
    }
}

fn is_session_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("ses_") else { return false };
    let b = rest.as_bytes();
    b.len() == 26
        && b[..12].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        && b[12..].iter().all(u8::is_ascii_alphanumeric)
}

/// Free-tier quota is counted per session and the real CLI keeps one per
/// conversation, so reuse the client's session when it sends one, otherwise one
/// long-lived session (a fresh one per request ends in 429 FreeUsageLimitError).
fn session_for(client: &HeaderMap) -> String {
    if let Some(s) = client_header(client, "x-opencode-session").filter(|s| s.len() <= MAX_SESSION_LEN) {
        return if is_session_id(s) { s.to_string() } else { translate_session(s, "generic") };
    }
    for (k, tool) in [("x-claude-code-session-id", "claude-code"), ("x-session-id", "generic")] {
        if let Some(s) = client_header(client, k).filter(|s| s.len() <= MAX_SESSION_LEN) {
            return translate_session(s, tool);
        }
    }
    stable_session()
}

static STABLE: LazyLock<Mutex<Option<(String, Instant)>>> = LazyLock::new(|| Mutex::new(None));

fn stable_session() -> String {
    let mut g = STABLE.lock();
    if let Some((id, used)) = g.as_mut() {
        if used.elapsed() < SESSION_TTL {
            *used = Instant::now();
            return id.clone();
        }
    }
    let id = new_session_id();
    *g = Some((id.clone(), Instant::now()));
    id
}

/// 12 hex of the low 48 bits of !(ms * 0x1000 + counter), like the CLI. Upstream
/// decodes it and rejects a large counter, so the counter is a real per-ms count.
fn descending_hex(time: u64, counter: u64) -> String {
    format!("{:012x}", !time.wrapping_mul(0x1000).wrapping_add(counter) & PREFIX_MASK)
}

static COUNTER: LazyLock<Mutex<(u64, u64)>> = LazyLock::new(|| Mutex::new((0, 0)));

fn new_session_id() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let counter = {
        let mut c = COUNTER.lock();
        if c.0 != now {
            *c = (now, 0);
        }
        c.1 += 1;
        c.1
    };
    let random: String = (0..14).map(|_| BASE62[rand::random::<u8>() as usize % 62] as char).collect();
    format!("ses_{}{random}", descending_hex(now, counter))
}

/// Map a foreign session id onto a stable OpenCode-shaped one.
fn translate_session(id: &str, tool: &str) -> String {
    let d = Sha256::digest(format!("opencode\0{tool}\0{id}").as_bytes());
    let time = d[..5].iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)) & TIME_MASK;
    let random: String = d[6..20].iter().map(|b| BASE62[*b as usize % 62] as char).collect();
    format!("ses_{}{random}", descending_hex(time, 1))
}

// ----- Body: fingerprint tools ----------------------------------------------------

/// Request rewrite. Returns sent tool name -> caller's name for tools that had to
/// be renamed (empty when nothing needs restoring in the response).
pub fn body(body: &mut Value, target: Format) -> HashMap<String, String> {
    body["stream"] = json!(true);
    let claude = target == Format::Claude;
    if !claude {
        inject_reasoning(body);
    }
    let map = conceal(body, claude);
    let appended = append_missing(body, claude);
    retarget_choice(body, &map, claude);
    alias_history(body, &map, claude);
    if !appended.is_empty() {
        note_unavailable(body, &appended, claude);
    }
    map
}

/// Reasoning models reject history whose assistant turns lack reasoning_content,
/// which OpenAI clients never send back; give them a placeholder.
fn inject_reasoning(body: &mut Value) {
    for m in body.get_mut("messages").and_then(Value::as_array_mut).into_iter().flatten() {
        let missing = m.get("reasoning_content").and_then(Value::as_str).map_or(true, str::is_empty);
        if m.get("role").and_then(Value::as_str) == Some("assistant") && missing {
            m["reasoning_content"] = json!(" ");
        }
    }
}

fn tool_name(t: &Value) -> &str {
    t.get("name").or_else(|| t.pointer("/function/name")).and_then(Value::as_str).map(str::trim).unwrap_or("")
}

fn fingerprint_key(name: &str) -> Option<&'static str> {
    FINGERPRINT.iter().copied().find(|k| *k == name.trim())
}

fn is_fingerprint(t: &Value, key: &str, claude: bool) -> bool {
    let (desc, params) = definition(key);
    let def = if claude { t } else { t.get("function").unwrap_or(t) };
    let p = if claude { def.get("input_schema") } else { def.get("parameters") };
    def.get("description").and_then(Value::as_str) == Some(desc.as_str()) && p == Some(params)
}

fn set_tool_name(t: &mut Value, name: &str) {
    match t.get_mut("function").filter(|f| f.is_object()) {
        Some(f) => f["name"] = json!(name),
        None => t["name"] = json!(name),
    }
}

/// Move caller tools off the fingerprint names when their definition differs, so
/// OpenCode's can be added without a duplicate name; duplicates are dropped.
fn conceal(body: &mut Value, claude: bool) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else { return map };
    let mut taken: Vec<String> = tools.iter().map(|t| tool_name(t).to_string()).collect();
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::with_capacity(tools.len());
    for mut t in std::mem::take(tools) {
        let Some(key) = t.is_object().then(|| fingerprint_key(tool_name(&t))).flatten() else {
            out.push(t);
            continue;
        };
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if !is_fingerprint(&t, key, claude) {
            let mut alias = format!("{key}_client");
            let mut i = 2;
            while taken.contains(&alias) {
                alias = format!("{key}_client{i}");
                i += 1;
            }
            taken.push(alias.clone());
            set_tool_name(&mut t, &alias);
            map.insert(alias, key.to_string());
        }
        out.push(t);
    }
    *tools = out;
    map
}

fn append_missing(body: &mut Value, claude: bool) -> Vec<&'static str> {
    if !body.get("tools").is_some_and(Value::is_array) {
        body["tools"] = json!([]);
    }
    let tools = body["tools"].as_array_mut().expect("tools is an array");
    let mut appended = Vec::new();
    for name in FINGERPRINT {
        if tools.iter().any(|t| fingerprint_key(tool_name(t)) == Some(name)) {
            continue;
        }
        let (desc, params) = definition(name);
        tools.push(if claude {
            json!({"name": name, "description": desc, "input_schema": params})
        } else {
            json!({"type": "function", "function": {"name": name, "description": desc, "parameters": params}})
        });
        appended.push(name);
    }
    appended
}

fn alias_of<'a>(map: &'a HashMap<String, String>, original: &str) -> Option<&'a String> {
    map.iter().find(|(_, o)| o.as_str() == original).map(|(a, _)| a)
}

/// An explicit tool_choice of a renamed caller tool follows it to the alias.
fn retarget_choice(body: &mut Value, map: &HashMap<String, String>, claude: bool) {
    let ptr = if claude { "/tool_choice/name" } else { "/tool_choice/function/name" };
    let Some(name) = body.pointer(ptr).and_then(Value::as_str).map(str::trim) else { return };
    if let Some(alias) = alias_of(map, name).cloned() {
        *body.pointer_mut(ptr).expect("checked above") = json!(alias);
    }
}

/// Earlier calls to a renamed caller tool get the alias too, or the model reads them
/// as calls to OpenCode's tool of the same name.
fn alias_history(body: &mut Value, map: &HashMap<String, String>, claude: bool) {
    if map.is_empty() {
        return;
    }
    for m in body.get_mut("messages").and_then(Value::as_array_mut).into_iter().flatten() {
        let (list, ptr) = if claude { ("content", "/name") } else { ("tool_calls", "/function/name") };
        for c in m.get_mut(list).and_then(Value::as_array_mut).into_iter().flatten() {
            if claude && c.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let alias = c.pointer(ptr).and_then(Value::as_str).and_then(|n| alias_of(map, n)).cloned();
            if let (Some(a), Some(slot)) = (alias, c.pointer_mut(ptr)) {
                *slot = json!(a);
            }
        }
    }
}

/// Without this the model does call the added tools. Wording from upstream: "not
/// available" made it refuse the caller's own Read/Bash too.
fn note_unavailable(body: &mut Value, names: &[&str], claude: bool) {
    let list = names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(" or ");
    let note = if names.len() > 1 {
        format!("Do not call the {list} tools; they are placeholders that do nothing. Use the other tools you have been given instead.")
    } else {
        format!("Do not call the {list} tool; it is a placeholder that does nothing. Use the other tools you have been given instead.")
    };
    if claude {
        let system = match body.get_mut("system").map(Value::take) {
            Some(Value::String(s)) if !s.trim().is_empty() => json!(format!("{s}\n\n{note}")),
            Some(Value::Array(mut a)) if !a.is_empty() => {
                a.push(json!({"type": "text", "text": note}));
                Value::Array(a)
            }
            _ => json!(note),
        };
        body["system"] = system;
        return;
    }
    let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) else { return };
    let first_system = msgs.first().and_then(|m| {
        (m.get("role").and_then(Value::as_str) == Some("system")).then(|| m.get("content").and_then(Value::as_str))?
    });
    match first_system.map(str::to_string) {
        Some(c) => msgs[0]["content"] = json!(format!("{c}\n\n{note}")),
        None => msgs.insert(0, json!({"role": "system", "content": note})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids() {
        let s = new_session_id();
        assert!(is_session_id(&s), "{s}");
        let t = translate_session("abc", "generic");
        assert!(is_session_id(&t), "{t}");
        assert_eq!(t, translate_session("abc", "generic"));
        assert_ne!(t, translate_session("abd", "generic"));
        assert_eq!(stable_session(), stable_session());
        assert!(!is_session_id("ses_XYZ"));
        assert_eq!(descending_hex(0, 1), "fffffffffffe");
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn free_models_keep_only_free_chat_models() {
        let live = ids(&[
            "big-pickle",
            "gpt-5",
            "claude-opus-5",
            "minimax-m3-free",
            "deepseek-v4-flash-free",
            "muse-spark-1.2-contributor-free",
            "muse-spark-1.3",
            "jev-1.13-free",
            "jev-1.13",
            "glm-5.3-flash",
            "glm-5.3-flash-free",
        ]);
        // Paid ids, the dead one, Responses-only Muse Spark and systemone-only Jev are dropped;
        // the order of the live list is kept.
        assert_eq!(free_models(live), ["big-pickle", "minimax-m3-free", "glm-5.3-flash-free"]);
        assert!(free_models(Vec::new()).is_empty());
    }

    #[test]
    fn documented_free_models_are_merged_after_the_live_list() {
        let all = with_documented(ids(&["brand-new-free", "beam-free"]));
        assert_eq!(all[..2], ["brand-new-free", "beam-free"]);
        // beam-free is in both lists and appears once.
        assert_eq!(all.len(), 2 + DOCUMENTED_FREE.len() - 1);
        assert_eq!(all.iter().filter(|m| *m == "beam-free").count(), 1);
        assert!(DOCUMENTED_FREE.iter().all(|id| all.contains(&id.to_string())));

        // The live list unavailable: the documented list alone, in its own order.
        assert_eq!(with_documented(Vec::new()), DOCUMENTED_FREE);
    }

    #[test]
    fn documented_free_models_pass_the_live_filter() {
        // Every documented id must be one the live filter would also accept, so the two
        // lists never disagree about what is free and routable.
        let documented = ids(&DOCUMENTED_FREE);
        assert_eq!(free_models(documented.clone()), documented);
        let mut unique = documented.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), documented.len());
        assert!(documented.iter().all(|id| !is_messages_model(id)));
    }

    #[test]
    fn models_url_follows_the_base_url() {
        let reg = crate::providers::Registry::load(None).unwrap();
        let Some(base) = reg.get(ID) else { return };
        let mut p = base.clone();
        let t = p.transport.as_mut().unwrap();
        t.base_url = "http://127.0.0.1:9/inference/v1/chat/completions".into();
        assert_eq!(models_url(&p).as_deref(), Some("http://127.0.0.1:9/zen/v1/models"));
        p.transport = None;
        assert_eq!(models_url(&p), None);
    }

    #[test]
    fn headers_and_models() {
        let mut client = HeaderMap::new();
        client.insert("x-opencode-session", "my-chat".parse().unwrap());
        client.insert("user-agent", "opencode/latest/2.1.0/desktop".parse().unwrap());
        let mut h = HashMap::from([("x-opencode-client".to_string(), "desktop".to_string())]);
        headers(&client, "union-alpha", &mut h);
        assert_eq!(h["Authorization"], "Bearer public");
        assert_eq!(h["User-Agent"], "opencode/latest/2.1.0/desktop");
        assert_eq!(h["x-opencode-client"], "cli");
        assert_eq!(h["x-opencode-session"], translate_session("my-chat", "generic"));
        assert_eq!(h["x-session-id"], h["x-opencode-session-id"]);
        assert!(h.contains_key("anthropic-version"));
        assert!(!is_v2_user_agent("opencode/1.18.31"));

        assert!(is_messages_model("union-alpha(high)"));
        assert!(is_muse_spark("muse-spark-1.3-contributor-free"));
        assert!(!is_muse_spark("musespark2"));
        let reg = crate::providers::Registry::load(None).unwrap();
        if let Some(p) = reg.get(ID) {
            assert!(p.no_auth);
            assert!(p.models.iter().all(|m| !is_muse_spark(&m.id)));
            let t = p.transport_for(Format::Claude, "big-pickle").unwrap();
            assert_eq!(t.base_url, "https://opencode.ai/inference/v1/chat/completions");
            assert_eq!(models_url(p).as_deref(), Some("https://opencode.ai/zen/v1/models"));
            assert!(t.force_stream);
            assert_eq!(p.transport_for(Format::Openai, "union-alpha").unwrap().format, Format::Claude);
        }
    }

    #[test]
    fn fingerprint_tools_openai() {
        let mut b = json!({
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "1", "type": "function", "function": {"name": "read", "arguments": "{}"}}]}
            ],
            "tools": [
                {"type": "function", "function": {"name": "read", "description": "mine", "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "read", "description": "dup"}},
                {"type": "function", "function": {"name": "grep"}}
            ],
            "tool_choice": {"type": "function", "function": {"name": "read"}}
        });
        let map = body(&mut b, Format::Openai);
        assert_eq!(map.get("read_client").map(String::as_str), Some("read"));
        let names: Vec<&str> = b["tools"].as_array().unwrap().iter().map(tool_name).collect();
        assert_eq!(names, ["read_client", "grep", "read", "shell"]);
        assert!(is_fingerprint(&b["tools"][2], "read", false));
        assert_eq!(b["tool_choice"]["function"]["name"], "read_client");
        assert_eq!(b["messages"][1]["tool_calls"][0]["function"]["name"], "read_client");
        assert_eq!(b["messages"][1]["reasoning_content"], " ");
        assert!(b["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("sys\n\nDo not call the `read` or `shell` tools"));
        assert_eq!(b["stream"], true);

        let restore = crate::oauth::hooks::Restore::Names(map);
        let mut chunk =
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "read_client"}}]}}]});
        assert!(crate::oauth::hooks::decloak_json(&mut chunk, &restore));
        assert_eq!(chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "read");
        let mut own = json!({"choices": [{"message": {"tool_calls": [{"function": {"name": "read"}}]}}]});
        assert!(!crate::oauth::hooks::decloak_json(&mut own, &restore));
    }

    #[test]
    fn fingerprint_tools_claude() {
        let (desc, params) = definition("shell");
        let mut b = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "shell", "description": desc, "input_schema": params}]
        });
        let map = body(&mut b, Format::Claude);
        assert!(map.is_empty());
        let names: Vec<&str> = b["tools"].as_array().unwrap().iter().map(tool_name).collect();
        assert_eq!(names, ["shell", "read"]);
        assert!(is_fingerprint(&b["tools"][1], "read", true));
        assert!(b["system"].as_str().unwrap().contains("`read` tool; it is a placeholder"));
        assert!(b["messages"][0].get("reasoning_content").is_none());
    }
}
