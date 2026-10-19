//! Format translation between the OpenAI Chat Completions and Anthropic Messages
//! APIs, for requests, JSON responses and SSE streams. Field mappings follow
//! upstream's `open-sse/translator` with its known lossy spots fixed where cheap
//! (stop sequences, top_p and `tool_choice: none` are preserved; `is_error` on tool
//! results survives as a text marker).

pub mod request;
pub mod response;
pub mod responses;
pub mod stream;

use serde_json::Value;

pub use crate::providers::Format;
pub use stream::StreamTranslator;

/// Which API the client is speaking, from the path first, then the body shape
/// (upstream `detectFormatByEndpoint` + `detectFormat`).
pub fn detect_source(path: &str, body: &Value) -> Format {
    if path.ends_with("/messages") || path.ends_with("/messages/count_tokens") {
        return Format::Claude;
    }
    if path.ends_with("/chat/completions") {
        return Format::Openai;
    }
    if path.contains(":generateContent") || path.contains(":streamGenerateContent") {
        return Format::Gemini;
    }
    if body.get("contents").map(Value::is_array).unwrap_or(false) {
        return Format::Gemini;
    }
    let openai_only =
        ["stream_options", "response_format", "n", "presence_penalty", "frequency_penalty", "logprobs", "user"];
    if openai_only.iter().any(|k| body.get(k).is_some()) {
        return Format::Openai;
    }
    if body.get("system").is_some()
        || body.get("anthropic_version").is_some()
        || body.get("max_tokens").is_some()
            && body.get("messages").is_some()
            && body.get("model").is_some()
            && has_claude_blocks(body)
    {
        return Format::Claude;
    }
    Format::Openai
}

fn has_claude_blocks(body: &Value) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .map(|ms| {
            ms.iter().any(|m| {
                m.get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks.iter().any(|b| {
                            matches!(
                                b.get("type").and_then(Value::as_str),
                                Some("tool_use" | "tool_result" | "thinking")
                            )
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Translate a request body from the client's format to the upstream's and set the
/// upstream model id. Same-format requests are only re-labelled.
pub fn translate_request(from: Format, to: Format, mut body: Value, upstream_model: &str) -> Result<Value, String> {
    let out = match (from, to) {
        (a, b) if a == b => {
            body["model"] = Value::String(upstream_model.to_string());
            body
        }
        (Format::Openai, Format::Claude) => request::openai_to_claude(&body, upstream_model),
        (Format::Claude, Format::Openai) => request::claude_to_openai(&body, upstream_model),
        (a, b) => return Err(format!("translation {} -> {} is not supported", a.as_str(), b.as_str())),
    };
    Ok(out)
}

/// Translate a complete (non-streaming) JSON response from upstream format `from`
/// to client format `to`.
pub fn translate_response(from: Format, to: Format, v: &Value, model: &str) -> Value {
    match (from, to) {
        (a, b) if a == b => v.clone(),
        (Format::Claude, Format::Openai) => response::claude_to_openai(v, model),
        (Format::Openai, Format::Claude) => response::openai_to_claude(v, model),
        _ => v.clone(),
    }
}

/// Text helpers shared by the request/response modules.
pub(crate) fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                if let Some(s) = p.as_str() {
                    return Some(s.to_string());
                }
                let ty = p.get("type").and_then(Value::as_str).unwrap_or("text");
                if ty == "text" || ty == "input_text" || ty == "output_text" {
                    p.get("text").and_then(Value::as_str).map(str::to_string)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(crate) fn finish_to_stop(reason: &str) -> &'static str {
    match reason {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

pub(crate) fn stop_to_finish(reason: &str) -> &'static str {
    match reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    }
}

/// `data:` URI -> (media_type, base64 payload).
pub(crate) fn parse_data_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media = meta.split(';').next().unwrap_or("application/octet-stream").to_string();
    if !meta.contains("base64") {
        return None;
    }
    Some((media, data.to_string()))
}

pub(crate) fn safe_parse_json(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| {
        if s.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            Value::String(s.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_by_path_then_body() {
        assert_eq!(detect_source("/v1/messages", &json!({})), Format::Claude);
        assert_eq!(detect_source("/v1/chat/completions", &json!({})), Format::Openai);
        assert_eq!(detect_source("/x", &json!({"contents": []})), Format::Gemini);
        assert_eq!(detect_source("/x", &json!({"system": "hi", "messages": []})), Format::Claude);
        assert_eq!(detect_source("/x", &json!({"messages": []})), Format::Openai);
    }

    #[test]
    fn helpers() {
        assert_eq!(
            text_of(&json!([{"type":"text","text":"a"},{"type":"image_url"},{"type":"text","text":"b"}])),
            "a\nb"
        );
        assert_eq!(parse_data_uri("data:image/png;base64,AAAA").unwrap().0, "image/png");
        assert_eq!(finish_to_stop("tool_calls"), "tool_use");
        assert_eq!(stop_to_finish("max_tokens"), "length");
    }
}
