//! Token Saver, as in upstream's dashboard page of the same name: RTK tool-output
//! compression plus the Caveman and Ponytail system prompts. Headroom ("Compress
//! context") is left out on purpose: it needs a Python proxy and ML models, far
//! too heavy for a small board. PXPIPE is hidden upstream and skipped too.
//!
//! Applied to the translated upstream body, right before dispatch (upstream
//! `chatCore.js`). A client opts out per request with `x-9router-token-saver: off`.

pub mod prompts;
pub mod rtk;

use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::providers::Format;

pub const OPT_OUT_HEADER: &str = "x-9router-token-saver";
const SEP: &str = "\n\n";

/// Settings keys match upstream: `rtkEnabled` (default on), `cavemanEnabled`,
/// `cavemanLevel` (default full), `ponytailEnabled`, `ponytailLevel` (default full).
pub struct Options {
    pub rtk: bool,
    pub caveman: Option<String>,
    pub ponytail: Option<String>,
}

impl Options {
    pub fn from_settings(s: &Value, headers: &HeaderMap) -> Options {
        let off = headers
            .get(OPT_OUT_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().eq_ignore_ascii_case("off"))
            .unwrap_or(false);
        let flag = |k: &str, default: bool| !off && s.get(k).and_then(Value::as_bool).unwrap_or(default);
        let level = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("full");
        Options {
            rtk: flag("rtkEnabled", true),
            caveman: flag("cavemanEnabled", false).then(|| prompts::caveman(level("cavemanLevel"))).flatten(),
            ponytail: flag("ponytailEnabled", false).then(|| prompts::ponytail(level("ponytailLevel"))).flatten(),
        }
    }
}

// Process-wide counters for the dashboard (reset on restart).
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
static BYTES_BEFORE: AtomicU64 = AtomicU64::new(0);
static BYTES_AFTER: AtomicU64 = AtomicU64::new(0);

pub fn stats() -> Value {
    let before = BYTES_BEFORE.load(Ordering::Relaxed);
    let after = BYTES_AFTER.load(Ordering::Relaxed);
    json!({
        "requests": REQUESTS.load(Ordering::Relaxed),
        "hits": HITS.load(Ordering::Relaxed),
        "bytesBefore": before,
        "bytesAfter": after,
        "bytesSaved": before.saturating_sub(after),
        // ~4 bytes per token, the same estimate the usage page uses.
        "tokensSaved": before.saturating_sub(after) / 4,
    })
}

/// Apply every enabled saver to an upstream body in `target` format.
pub fn apply(body: &mut Value, target: Format, opts: &Options) {
    let mut flags: Vec<String> = Vec::new();
    if opts.rtk {
        let s = rtk::compress_messages(body);
        if !s.hits.is_empty() {
            REQUESTS.fetch_add(1, Ordering::Relaxed);
            HITS.fetch_add(s.hits.len() as u64, Ordering::Relaxed);
            BYTES_BEFORE.fetch_add(s.bytes_before as u64, Ordering::Relaxed);
            BYTES_AFTER.fetch_add(s.bytes_after as u64, Ordering::Relaxed);
            let saved = s.bytes_before - s.bytes_after;
            let mut names = s.hits.clone();
            names.sort_unstable();
            names.dedup();
            tracing::info!(
                "[RTK] saved {saved}B / {}B ({:.1}%) via [{}] hits={}",
                s.bytes_before,
                saved as f64 * 100.0 / s.bytes_before.max(1) as f64,
                names.join(","),
                s.hits.len()
            );
            flags.push(format!("RTK:{}", s.hits.len()));
        }
    }
    if let Some(p) = &opts.caveman {
        inject_system(body, target, p);
        flags.push("CAVEMAN".into());
    }
    if let Some(p) = &opts.ponytail {
        inject_system(body, target, p);
        flags.push("PONYTAIL".into());
    }
    if !flags.is_empty() {
        tracing::debug!("token saver: {}", flags.join(" "));
    }
}

/// The prompt already present as its own `\n\n`-separated segment.
fn has_prompt(haystack: &str, prompt: &str) -> bool {
    haystack == prompt || haystack.split(SEP).any(|s| s == prompt)
}

fn append(curr: &str, prompt: &str) -> Option<String> {
    if curr.is_empty() {
        Some(prompt.to_string())
    } else if has_prompt(curr, prompt) {
        None
    } else {
        Some(format!("{curr}{SEP}{prompt}"))
    }
}

/// Append `prompt` to the system instructions, idempotently (retries reuse the body).
pub fn inject_system(body: &mut Value, target: Format, prompt: &str) {
    let Some(o) = body.as_object_mut() else { return };
    if target == Format::Claude {
        // Anthropic has no system role inside messages[]; use the system field.
        match o.get_mut("system") {
            Some(Value::String(s)) => {
                if let Some(n) = append(s, prompt) {
                    *s = n;
                }
            }
            Some(Value::Array(blocks)) => {
                if blocks.iter().any(|b| b.get("text").and_then(Value::as_str) == Some(prompt)) {
                    return;
                }
                let block = json!({"type": "text", "text": prompt});
                // Keep the prompt-cache breakpoint last: insert before the last cached block.
                match blocks.iter().rposition(|b| b.get("cache_control").is_some()) {
                    Some(i) => blocks.insert(i, block),
                    None => blocks.push(block),
                }
            }
            _ => {
                o.insert("system".into(), json!(prompt));
            }
        }
        return;
    }
    if let Some(Value::String(s)) = o.get_mut("instructions") {
        if let Some(n) = append(s, prompt) {
            *s = n;
        }
        return;
    }
    let responses = !o.get("messages").map_or(false, Value::is_array);
    let Some(Value::Array(msgs)) = o.get_mut(if responses { "input" } else { "messages" }) else { return };
    let is_sys = |m: &Value| {
        let role = m.get("role").and_then(Value::as_str);
        (role == Some("system") || role == Some("developer"))
            && (!responses || m.get("type").and_then(Value::as_str) == Some("message"))
    };
    let present = msgs.iter().filter(|m| is_sys(m)).any(|m| match m.get("content") {
        Some(Value::String(s)) => has_prompt(s, prompt),
        Some(Value::Array(parts)) => {
            parts.iter().any(|p| p.get("text").and_then(Value::as_str).map_or(false, |t| has_prompt(t, prompt)))
        }
        _ => false,
    });
    if present {
        return;
    }
    let part_type = if responses { "input_text" } else { "text" };
    match msgs.iter_mut().find(|m| is_sys(m)) {
        Some(m) => match m.get_mut("content") {
            Some(Value::String(s)) => {
                if let Some(n) = append(s, prompt) {
                    *s = n;
                }
            }
            Some(Value::Array(parts)) => parts.push(json!({"type": part_type, "text": prompt})),
            _ => m["content"] = json!(prompt),
        },
        None if responses => msgs.insert(
            0,
            json!({"type": "message", "role": "system", "content": [{"type": "input_text", "text": prompt}]}),
        ),
        None => msgs.insert(0, json!({"role": "system", "content": prompt})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_once_per_format() {
        let p = prompts::caveman("full").unwrap();
        let mut claude = json!({"system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}}], "messages": []});
        inject_system(&mut claude, Format::Claude, &p);
        inject_system(&mut claude, Format::Claude, &p);
        assert_eq!(claude["system"].as_array().unwrap().len(), 3);
        assert_eq!(claude["system"][1]["text"], json!(p));

        let mut openai = json!({"messages": [{"role": "user", "content": "hi"}]});
        inject_system(&mut openai, Format::Openai, &p);
        inject_system(&mut openai, Format::Openai, &p);
        assert_eq!(openai["messages"][0], json!({"role": "system", "content": p}));
        assert_eq!(openai["messages"].as_array().unwrap().len(), 2);

        let mut with_sys = json!({"messages": [{"role": "system", "content": "be nice"}]});
        inject_system(&mut with_sys, Format::Openai, &p);
        assert_eq!(with_sys["messages"][0]["content"], json!(format!("be nice\n\n{p}")));
    }

    #[test]
    fn options_follow_settings_and_opt_out_header() {
        let s = json!({"cavemanEnabled": true, "cavemanLevel": "ultra"});
        let o = Options::from_settings(&s, &HeaderMap::new());
        assert!(o.rtk && o.caveman.is_some() && o.ponytail.is_none());
        let mut h = HeaderMap::new();
        h.insert(OPT_OUT_HEADER, "off".parse().unwrap());
        let o = Options::from_settings(&s, &h);
        assert!(!o.rtk && o.caveman.is_none());
    }
}
