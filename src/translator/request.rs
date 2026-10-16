//! Request translation: OpenAI Chat Completions <-> Anthropic Messages.

use serde_json::{json, Map, Value};

use super::{parse_data_uri, safe_parse_json, text_of};

pub const DEFAULT_MAX_TOKENS: i64 = 64_000;
pub const DEFAULT_MIN_TOKENS_WITH_TOOLS: i64 = 32_000;

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

// ---------------------------------------------------------------------------
// OpenAI -> Claude
// ---------------------------------------------------------------------------

/// Convert an OpenAI chat request into an Anthropic Messages request.
pub fn openai_to_claude(body: &Value, model: &str) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));

    let has_tools = body.get("tools").and_then(Value::as_array).map(|t| !t.is_empty()).unwrap_or(false);
    let mut max_tokens = body
        .get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    if has_tools && max_tokens < DEFAULT_MIN_TOKENS_WITH_TOOLS && body.get("max_tokens").is_none() {
        max_tokens = DEFAULT_MIN_TOKENS_WITH_TOOLS;
    }
    out.insert("max_tokens".into(), json!(max_tokens));
    out.insert("stream".into(), json!(body.get("stream").and_then(Value::as_bool).unwrap_or(false)));
    for k in ["temperature", "top_p", "metadata"] {
        if let Some(v) = body.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(stop) = body.get("stop") {
        let seqs: Vec<Value> = match stop {
            Value::String(x) => vec![json!(x)],
            Value::Array(a) => a.clone(),
            _ => vec![],
        };
        if !seqs.is_empty() {
            out.insert("stop_sequences".into(), Value::Array(seqs));
        }
    }

    // System: every system/developer message, in order, plus response_format hints.
    let mut system_parts: Vec<String> = Vec::new();
    let empty = vec![];
    let messages = body.get("messages").and_then(Value::as_array).unwrap_or(&empty);
    for m in messages {
        if matches!(s(m, "role").as_deref(), Some("system" | "developer")) {
            let t = text_of(m.get("content").unwrap_or(&Value::Null));
            if !t.trim().is_empty() {
                system_parts.push(t);
            }
        }
    }
    if let Some(rf) = body.get("response_format") {
        match s(rf, "type").as_deref() {
            Some("json_schema") => {
                let schema = rf.pointer("/json_schema/schema").cloned().unwrap_or(Value::Null);
                system_parts.push(format!(
                    "You must respond with valid JSON only, matching this JSON schema:\n```json\n{}\n```",
                    serde_json::to_string_pretty(&schema).unwrap_or_default()
                ));
            }
            Some("json_object") => system_parts.push("You must respond with a valid JSON object only.".into()),
            _ => {}
        }
    }
    if !system_parts.is_empty() {
        out.insert(
            "system".into(),
            json!([{"type": "text", "text": system_parts.join("\n"), "cache_control": {"type": "ephemeral"}}]),
        );
    }

    // Messages: user/tool -> user, everything else -> assistant; merge same-role runs.
    let mut msgs: Vec<Value> = Vec::new();
    let mut push = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = msgs.last_mut() {
            if last["role"] == role {
                if let Some(arr) = last["content"].as_array_mut() {
                    arr.extend(blocks);
                    return;
                }
            }
        }
        msgs.push(json!({"role": role, "content": blocks}));
    };
    for m in messages {
        let role = s(m, "role").unwrap_or_else(|| "user".into());
        match role.as_str() {
            "system" | "developer" => continue,
            "tool" => {
                let content = m.get("content").cloned().unwrap_or(Value::String(String::new()));
                let content = match content {
                    Value::String(_) => content,
                    Value::Array(_) => Value::String(text_of(&content)),
                    other => Value::String(other.to_string()),
                };
                push(
                    "user",
                    vec![
                        json!({"type": "tool_result", "tool_use_id": s(m, "tool_call_id").unwrap_or_default(), "content": content}),
                    ],
                );
            }
            "assistant" => {
                let mut blocks = assistant_blocks(m);
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for tc in calls {
                        if s(tc, "type").as_deref().unwrap_or("function") != "function" {
                            continue;
                        }
                        let args = tc.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": s(tc, "id").unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::new_v4().simple())),
                            "name": tc.pointer("/function/name").and_then(Value::as_str).unwrap_or(""),
                            "input": safe_parse_json(args),
                        }));
                    }
                }
                push("assistant", blocks);
            }
            _ => push("user", user_blocks(m)),
        }
    }
    // Anthropic requires the conversation to start with a user turn.
    if msgs.first().map(|m| m["role"] == "assistant").unwrap_or(false) {
        msgs.insert(0, json!({"role": "user", "content": [{"type": "text", "text": "Continue."}]}));
    }
    out.insert("messages".into(), Value::Array(msgs));

    // Tools.
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools
            .iter()
            .map(|t| {
                let ty = s(t, "type");
                if ty.is_some() && ty.as_deref() != Some("function") && t.get("function").is_none() {
                    return t.clone(); // built-in tool such as web_search_20250305
                }
                let f = t.get("function").unwrap_or(t);
                json!({
                    "name": s(f, "name").unwrap_or_default(),
                    "description": s(f, "description").unwrap_or_default(),
                    "input_schema": f.get("parameters").or_else(|| f.get("input_schema")).cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                })
            })
            .collect();
        if !converted.is_empty() {
            out.insert("tools".into(), Value::Array(converted));
            if let Some(tc) = body.get("tool_choice") {
                let choice = match tc {
                    Value::String(x) if x == "required" => Some(json!({"type": "any"})),
                    Value::String(x) if x == "none" => None, // handled below by dropping tools
                    Value::String(_) => Some(json!({"type": "auto"})),
                    Value::Object(o) => match o.pointer("/function/name").and_then(Value::as_str) {
                        Some(n) => Some(json!({"type": "tool", "name": n})),
                        None if matches!(s(tc, "type").as_deref(), Some("auto" | "any" | "tool" | "none")) => {
                            Some(tc.clone())
                        }
                        None => Some(json!({"type": "auto"})),
                    },
                    _ => Some(json!({"type": "auto"})),
                };
                match choice {
                    Some(c) => {
                        out.insert("tool_choice".into(), c);
                    }
                    None => {
                        // `none`: Anthropic has no equivalent that keeps definitions; drop tools.
                        out.remove("tools");
                    }
                }
                if body.get("parallel_tool_calls") == Some(&Value::Bool(false)) {
                    if let Some(c) = out.get_mut("tool_choice") {
                        c["disable_parallel_tool_use"] = json!(true);
                    }
                }
            }
        }
    }

    // Reasoning: reasoning_effort -> thinking budget (OpenAI-side "none" disables).
    if let Some(effort) =
        body.get("reasoning_effort").or_else(|| body.pointer("/reasoning/effort")).and_then(Value::as_str)
    {
        let budget = match effort {
            "none" => 0,
            "minimal" | "low" => 2_048,
            "medium" => 8_192,
            "high" => 16_384,
            "xhigh" | "max" => 32_000,
            _ => 8_192,
        };
        if budget > 0 {
            let budget = budget.min(max_tokens - 1024).max(1024);
            out.insert("thinking".into(), json!({"type": "enabled", "budget_tokens": budget}));
        }
    } else if let Some(th) = body.get("thinking") {
        out.insert("thinking".into(), th.clone());
    }
    if let Some(Value::Object(th)) = out.get("thinking") {
        // Anthropic rejects temperature/top_p overrides while thinking is on.
        if th.get("type").and_then(Value::as_str) == Some("enabled") {
            out.remove("temperature");
            out.remove("top_p");
        }
    }

    Value::Object(out)
}

fn user_blocks(m: &Value) -> Vec<Value> {
    match m.get("content") {
        Some(Value::String(t)) => {
            if t.is_empty() {
                vec![]
            } else {
                vec![json!({"type": "text", "text": t})]
            }
        }
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match s(p, "type").as_deref() {
                Some("text") | None => {
                    let t = p.get("text").and_then(Value::as_str).unwrap_or("");
                    (!t.is_empty()).then(|| json!({"type": "text", "text": t}))
                }
                Some("tool_result") | Some("image") | Some("document") => Some(p.clone()),
                Some("image_url") => {
                    let url = p
                        .pointer("/image_url/url")
                        .and_then(Value::as_str)
                        .or_else(|| p.get("image_url").and_then(Value::as_str))?;
                    if let Some((media, data)) = parse_data_uri(url) {
                        Some(json!({"type": "image", "source": {"type": "base64", "media_type": media, "data": data}}))
                    } else if url.starts_with("http") {
                        Some(json!({"type": "image", "source": {"type": "url", "url": url}}))
                    } else {
                        None
                    }
                }
                Some("file") => {
                    let data = p.pointer("/file/file_data").and_then(Value::as_str)?;
                    let (media, b64) = parse_data_uri(data)?;
                    (media == "application/pdf").then(
                        || json!({"type": "document", "source": {"type": "base64", "media_type": media, "data": b64}}),
                    )
                }
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

fn assistant_blocks(m: &Value) -> Vec<Value> {
    match m.get("content") {
        Some(Value::String(t)) if !t.is_empty() => vec![json!({"type": "text", "text": t})],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match s(p, "type").as_deref() {
                Some("text") | None => {
                    let t = p.get("text").and_then(Value::as_str).unwrap_or("");
                    (!t.is_empty()).then(|| json!({"type": "text", "text": t}))
                }
                Some("tool_use") => Some(p.clone()),
                Some("thinking") | Some("redacted_thinking") => {
                    let mut b = p.clone();
                    if let Some(o) = b.as_object_mut() {
                        o.remove("cache_control");
                    }
                    Some(b)
                }
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Claude -> OpenAI
// ---------------------------------------------------------------------------

/// Convert an Anthropic Messages request into an OpenAI chat request.
pub fn claude_to_openai(body: &Value, model: &str) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("stream".into(), json!(body.get("stream").and_then(Value::as_bool).unwrap_or(false)));
    if out["stream"] == true {
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(mt) = body.get("max_tokens").and_then(Value::as_i64) {
        out.insert("max_tokens".into(), json!(mt.min(DEFAULT_MAX_TOKENS)));
    }
    for k in ["temperature", "top_p"] {
        if let Some(v) = body.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(stop) = body.get("stop_sequences").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        out.insert("stop".into(), Value::Array(stop.iter().take(4).cloned().collect()));
    }

    let mut msgs: Vec<Value> = Vec::new();
    if let Some(sys) = body.get("system") {
        let t = match sys {
            Value::String(t) => strip_billing_header(t),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .map(strip_billing_header)
                .filter(|t| !t.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !t.trim().is_empty() {
            msgs.push(json!({"role": "system", "content": t}));
        }
    }

    let empty = vec![];
    for m in body.get("messages").and_then(Value::as_array).unwrap_or(&empty) {
        let role = s(m, "role").unwrap_or_else(|| "user".into());
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        let blocks: Vec<Value> = match content {
            Value::String(t) => vec![json!({"type": "text", "text": t})],
            Value::Array(a) => a,
            Value::Object(o) => vec![Value::Object(o)],
            _ => vec![],
        };
        if role == "system" {
            msgs.push(json!({"role": "user", "content": format!("<instructions>\n{}\n</instructions>", text_of(&Value::Array(blocks)))}));
            continue;
        }

        let mut parts: Vec<Value> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_results: Vec<Value> = Vec::new();
        let mut trailing_images: Vec<Value> = Vec::new();
        for b in &blocks {
            match s(b, "type").as_deref() {
                Some("text") => {
                    let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                    if !t.is_empty() {
                        parts.push(json!({"type": "text", "text": t}));
                    }
                }
                Some("image") => {
                    if let Some(p) = image_part(b) {
                        parts.push(p);
                    }
                }
                Some("tool_use") => tool_calls.push(json!({
                    "id": s(b, "id").unwrap_or_default(),
                    "type": "function",
                    "function": {"name": s(b, "name").unwrap_or_default(),
                                 "arguments": b.get("input").map(|i| i.to_string()).unwrap_or_else(|| "{}".into())},
                })),
                Some("tool_result") => {
                    let id = s(b, "tool_use_id").unwrap_or_default();
                    let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                    let mut text = match b.get("content") {
                        Some(Value::String(t)) => t.clone(),
                        Some(Value::Array(a)) => {
                            for c in a {
                                if s(c, "type").as_deref() == Some("image") {
                                    if let Some(p) = image_part(c) {
                                        trailing_images.push(
                                            json!({"type": "text", "text": format!("[Image from tool result {id}]")}),
                                        );
                                        trailing_images.push(p);
                                    }
                                }
                            }
                            let t = text_of(&Value::Array(a.clone()));
                            if t.is_empty() && trailing_images.is_empty() {
                                Value::Array(a.clone()).to_string()
                            } else {
                                t
                            }
                        }
                        Some(other) if !other.is_null() => other.to_string(),
                        _ => String::new(),
                    };
                    if is_error && !text.starts_with("Error:") {
                        text = format!("Error: {text}");
                    }
                    tool_results.push(json!({"role": "tool", "tool_call_id": id, "content": text}));
                }
                // thinking / redacted_thinking / document: not representable, dropped.
                _ => {}
            }
        }

        if !tool_results.is_empty() {
            msgs.extend(tool_results);
            parts.extend(trailing_images);
            if !parts.is_empty() {
                msgs.push(json!({"role": "user", "content": collapse(parts)}));
            }
        } else if !tool_calls.is_empty() {
            let mut m = json!({"role": "assistant", "tool_calls": tool_calls});
            m["content"] = if parts.is_empty() { Value::Null } else { collapse(parts) };
            msgs.push(m);
        } else {
            msgs.push(json!({"role": role, "content": if parts.is_empty() { json!("") } else { collapse(parts) }}));
        }
    }
    fix_missing_tool_responses(&mut msgs);
    out.insert("messages".into(), Value::Array(msgs));

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools
            .iter()
            .filter(|t| t.get("name").is_some())
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": s(t, "name").unwrap_or_default(),
                    "description": s(t, "description").unwrap_or_default(),
                    "parameters": t.get("input_schema").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                }})
            })
            .collect();
        if !converted.is_empty() {
            out.insert("tools".into(), Value::Array(converted));
        }
        if let Some(tc) = body.get("tool_choice") {
            let choice = match s(tc, "type").as_deref() {
                Some("any") => json!("required"),
                Some("tool") => json!({"type": "function", "function": {"name": s(tc, "name").unwrap_or_default()}}),
                Some("none") => json!("none"),
                _ => json!("auto"),
            };
            out.insert("tool_choice".into(), choice);
            if tc.get("disable_parallel_tool_use") == Some(&Value::Bool(true)) {
                out.insert("parallel_tool_calls".into(), json!(false));
            }
        }
    }

    if let Some(th) = body.get("thinking") {
        let effort = match (s(th, "type").as_deref(), th.get("budget_tokens").and_then(Value::as_i64)) {
            (Some("disabled"), _) => Some("none"),
            (Some("enabled"), Some(b)) if b <= 2_048 => Some("low"),
            (Some("enabled"), Some(b)) if b <= 8_192 => Some("medium"),
            (Some("enabled"), Some(_)) => Some("high"),
            (Some("enabled" | "adaptive"), None) => Some("medium"),
            _ => None,
        };
        if let Some(e) = effort {
            out.insert("reasoning_effort".into(), json!(e));
        }
    }
    if let Some(e) = body.pointer("/output_config/effort").and_then(Value::as_str) {
        out.insert("reasoning_effort".into(), json!(e));
    }

    Value::Object(out)
}

fn image_part(b: &Value) -> Option<Value> {
    let src = b.get("source")?;
    match s(src, "type").as_deref() {
        Some("base64") => Some(json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{}",
            s(src, "media_type").unwrap_or_else(|| "image/png".into()), s(src, "data").unwrap_or_default())}})),
        Some("url") => Some(json!({"type": "image_url", "image_url": {"url": s(src, "url").unwrap_or_default()}})),
        _ => None,
    }
}

/// One text part collapses to a plain string (what most OpenAI-compatible servers expect).
fn collapse(parts: Vec<Value>) -> Value {
    if parts.len() == 1 && parts[0].get("type").and_then(Value::as_str) == Some("text") {
        return parts[0]["text"].clone();
    }
    Value::Array(parts)
}

fn strip_billing_header(t: &str) -> String {
    t.lines().filter(|l| !l.starts_with("x-anthropic-billing-header:")).collect::<Vec<_>>().join("\n")
}

/// Every assistant tool_call must be answered by a tool message before the next
/// non-tool message; insert placeholders where the client left gaps.
fn fix_missing_tool_responses(msgs: &mut Vec<Value>) {
    let mut i = 0;
    while i < msgs.len() {
        if let Some(calls) = msgs[i].get("tool_calls").and_then(Value::as_array).cloned() {
            let ids: Vec<String> = calls.iter().filter_map(|c| s(c, "id")).collect();
            let mut answered = std::collections::HashSet::new();
            let mut j = i + 1;
            while j < msgs.len() && msgs[j]["role"] == "tool" {
                if let Some(id) = s(&msgs[j], "tool_call_id") {
                    answered.insert(id);
                }
                j += 1;
            }
            let mut insert_at = j;
            for id in ids {
                if !answered.contains(&id) {
                    msgs.insert(
                        insert_at,
                        json!({"role": "tool", "tool_call_id": id, "content": "[No response received]"}),
                    );
                    insert_at += 1;
                }
            }
            i = insert_at;
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_to_claude_basic() {
        let req = json!({
            "model": "x", "stream": true, "max_tokens": 100, "stop": ["END"],
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "42"},
                {"role": "user", "content": [{"type": "text", "text": "ok"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}]}
            ],
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}],
            "tool_choice": "required"
        });
        let out = openai_to_claude(&req, "claude-x");
        assert_eq!(out["model"], "claude-x");
        assert_eq!(out["max_tokens"], 100);
        assert_eq!(out["stop_sequences"][0], "END");
        assert_eq!(out["system"][0]["text"], "be brief");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["input"]["a"], 1);
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][2]["source"]["media_type"], "image/png");
        assert_eq!(out["tool_choice"]["type"], "any");
        assert_eq!(out["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn claude_to_openai_basic() {
        let req = json!({
            "model": "x", "max_tokens": 10, "system": [{"type": "text", "text": "sys"}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "..."}, {"type": "tool_use", "id": "t1", "name": "f", "input": {"a": 1}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "boom", "is_error": true}, {"type": "text", "text": "next"}]}
            ],
            "tools": [{"name": "f", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any"},
            "thinking": {"type": "enabled", "budget_tokens": 1024}
        });
        let out = claude_to_openai(&req, "gpt-x");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["content"], "Error: boom");
        assert_eq!(msgs[4]["content"], "next");
        assert_eq!(out["tool_choice"], "required");
        assert_eq!(out["reasoning_effort"], "low");
        assert_eq!(out["tools"][0]["function"]["name"], "f");
    }

    #[test]
    fn inserts_missing_tool_responses() {
        let mut msgs = vec![
            json!({"role": "assistant", "tool_calls": [{"id": "a"}, {"id": "b"}]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "1"}),
            json!({"role": "user", "content": "x"}),
        ];
        fix_missing_tool_responses(&mut msgs);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2]["tool_call_id"], "b");
    }
}
