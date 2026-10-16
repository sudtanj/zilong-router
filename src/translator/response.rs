//! Non-streaming response translation.

use serde_json::{json, Value};

use super::{finish_to_stop, safe_parse_json, stop_to_finish};

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Anthropic Messages response -> OpenAI chat.completion.
pub fn claude_to_openai(v: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for b in v.get("content").and_then(Value::as_array).into_iter().flatten() {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("thinking") => reasoning.push_str(b.get("thinking").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => tool_calls.push(json!({
                "id": b.get("id").cloned().unwrap_or(json!("")),
                "type": "function",
                "function": {"name": b.get("name").cloned().unwrap_or(json!("")),
                             "arguments": b.get("input").map(|i| i.to_string()).unwrap_or_else(|| "{}".into())},
            })),
            _ => {}
        }
    }
    let stop = v.get("stop_reason").and_then(Value::as_str).unwrap_or("end_turn");
    let mut message = json!({"role": "assistant", "content": if text.is_empty() && !tool_calls.is_empty() { Value::Null } else { json!(text) }});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let usage = crate::usage::extract(v).unwrap_or_default();
    json!({
        "id": format!("chatcmpl-{}", v.get("id").and_then(Value::as_str).unwrap_or("msg")),
        "object": "chat.completion",
        "created": now(),
        "model": v.get("model").and_then(Value::as_str).unwrap_or(model),
        "choices": [{"index": 0, "message": message, "finish_reason": stop_to_finish(stop)}],
        "usage": openai_usage(&usage),
    })
}

pub fn openai_usage(u: &crate::usage::TokenCounts) -> Value {
    let mut out = json!({"prompt_tokens": u.prompt, "completion_tokens": u.completion, "total_tokens": u.total()});
    if u.cached > 0 || u.cache_creation > 0 {
        out["prompt_tokens_details"] = json!({"cached_tokens": u.cached, "cache_creation_tokens": u.cache_creation});
    }
    if u.reasoning > 0 {
        out["completion_tokens_details"] = json!({"reasoning_tokens": u.reasoning});
    }
    out
}

pub fn claude_usage(u: &crate::usage::TokenCounts) -> Value {
    let mut out =
        json!({"input_tokens": (u.prompt - u.cached - u.cache_creation).max(0), "output_tokens": u.completion});
    if u.cached > 0 {
        out["cache_read_input_tokens"] = json!(u.cached);
    }
    if u.cache_creation > 0 {
        out["cache_creation_input_tokens"] = json!(u.cache_creation);
    }
    out
}

/// OpenAI chat.completion -> Anthropic Messages response.
pub fn openai_to_claude(v: &Value, model: &str) -> Value {
    let choice = v.pointer("/choices/0").cloned().unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let mut content = Vec::new();
    if let Some(r) =
        msg.get("reasoning_content").or_else(|| msg.get("reasoning")).and_then(Value::as_str).filter(|s| !s.is_empty())
    {
        content.push(json!({"type": "thinking", "thinking": r, "signature": ""}));
    }
    let text = super::text_of(msg.get("content").unwrap_or(&Value::Null));
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    let mut had_tools = false;
    for tc in msg.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
        had_tools = true;
        let args = tc.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
        content.push(json!({
            "type": "tool_use",
            "id": tc.get("id").cloned().unwrap_or_else(|| json!(format!("toolu_{}", uuid::Uuid::new_v4().simple()))),
            "name": tc.pointer("/function/name").cloned().unwrap_or(json!("")),
            "input": match safe_parse_json(args) { Value::Object(o) => Value::Object(o), _ => json!({}) },
        }));
    }
    let finish =
        choice.get("finish_reason").and_then(Value::as_str).unwrap_or(if had_tools { "tool_calls" } else { "stop" });
    let usage = crate::usage::extract(v).unwrap_or_default();
    json!({
        "id": v.get("id").and_then(Value::as_str).map(|s| s.trim_start_matches("chatcmpl-")).map(|s| format!("msg_{s}")).unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4().simple())),
        "type": "message",
        "role": "assistant",
        "model": v.get("model").and_then(Value::as_str).unwrap_or(model),
        "content": content,
        "stop_reason": finish_to_stop(finish),
        "stop_sequence": Value::Null,
        "usage": claude_usage(&usage),
    })
}

/// Error body in the client's dialect.
pub fn error_body(format: super::Format, status: u16, message: &str) -> Value {
    let (ty, code) = match status {
        400 => ("invalid_request_error", "bad_request"),
        401 => ("authentication_error", "invalid_api_key"),
        402 => ("billing_error", "payment_required"),
        403 => ("permission_error", "insufficient_quota"),
        404 => ("invalid_request_error", "model_not_found"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        502 => ("server_error", "bad_gateway"),
        503 => ("server_error", "service_unavailable"),
        504 => ("server_error", "gateway_timeout"),
        _ => ("server_error", "internal_server_error"),
    };
    match format {
        super::Format::Claude => json!({"type": "error", "error": {"type": ty, "message": message}}),
        _ => json!({"error": {"message": message, "type": ty, "code": code}}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_shapes() {
        let claude = json!({"id": "msg_1", "model": "c", "stop_reason": "tool_use",
            "content": [{"type": "text", "text": "hi"}, {"type": "tool_use", "id": "t", "name": "f", "input": {"a": 1}}],
            "usage": {"input_tokens": 3, "output_tokens": 2, "cache_read_input_tokens": 1}});
        let oai = claude_to_openai(&claude, "m");
        assert_eq!(oai["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(oai["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(oai["usage"]["prompt_tokens"], 4);
        let back = openai_to_claude(&oai, "m");
        assert_eq!(back["stop_reason"], "tool_use");
        assert_eq!(back["content"][1]["input"]["a"], 1);
        assert_eq!(back["usage"]["input_tokens"], 3);
        assert_eq!(error_body(super::super::Format::Claude, 429, "slow")["error"]["type"], "rate_limit_error");
    }
}
