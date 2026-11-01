//! Whole-document helpers shared by the server and the engine: collapse an upstream
//! event stream into one JSON answer, replay a JSON answer as stream events, and the
//! error event that ends a stream which already started.

use bytes::Bytes;
use serde_json::{json, Value};

use super::{Format, StreamTranslator};
use crate::oauth::hooks::Restore;
use crate::sse;
use crate::usage::TokenCounts;

/// Replay a full SSE body through the stream translator and assemble the final JSON.
pub fn collapse_sse(text: &str, from: Format, to: Format, model: &str, decloak: &Restore) -> Value {
    let mut parser = sse::Parser::default();
    let mut tr = StreamTranslator::new(from, to, model);
    let mut events = Vec::new();
    for mut ev in parser.push(text.as_bytes()).into_iter().chain(parser.finish()) {
        crate::oauth::hooks::decloak_event(&mut ev, decloak);
        events.extend(tr.feed(&ev));
    }
    events.extend(tr.finish());
    assemble(&events, to, model, &tr.usage)
}

/// Build a complete response document from translated client-format events.
pub fn assemble(events: &[sse::Event], fmt: Format, model: &str, usage: &TokenCounts) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tools: Vec<(String, String, String)> = Vec::new(); // id, name, args
    let mut finish = String::new();
    let mut id = String::new();
    for ev in events {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { continue };
        match fmt {
            Format::Openai => {
                if id.is_empty() {
                    id = v.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                }
                let Some(c) = v.pointer("/choices/0") else { continue };
                if let Some(t) = c.pointer("/delta/content").and_then(Value::as_str) {
                    text.push_str(t);
                }
                if let Some(t) = c.pointer("/delta/reasoning_content").and_then(Value::as_str) {
                    reasoning.push_str(t);
                }
                for tc in c.pointer("/delta/tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    while tools.len() <= idx {
                        tools.push((String::new(), String::new(), String::new()));
                    }
                    if let Some(i) = tc.get("id").and_then(Value::as_str) {
                        tools[idx].0 = i.to_string();
                    }
                    if let Some(n) = tc.pointer("/function/name").and_then(Value::as_str) {
                        tools[idx].1 = n.to_string();
                    }
                    if let Some(a) = tc.pointer("/function/arguments").and_then(Value::as_str) {
                        tools[idx].2.push_str(a);
                    }
                }
                if let Some(f) = c.get("finish_reason").and_then(Value::as_str) {
                    finish = f.to_string();
                }
            }
            _ => match v.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    id = v.pointer("/message/id").and_then(Value::as_str).unwrap_or("").to_string()
                }
                Some("content_block_start") => {
                    if v.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use") {
                        tools.push((
                            v.pointer("/content_block/id").and_then(Value::as_str).unwrap_or("").into(),
                            v.pointer("/content_block/name").and_then(Value::as_str).unwrap_or("").into(),
                            String::new(),
                        ));
                    }
                }
                Some("content_block_delta") => match v.pointer("/delta/type").and_then(Value::as_str) {
                    Some("text_delta") => text.push_str(v.pointer("/delta/text").and_then(Value::as_str).unwrap_or("")),
                    Some("thinking_delta") => {
                        reasoning.push_str(v.pointer("/delta/thinking").and_then(Value::as_str).unwrap_or(""))
                    }
                    Some("input_json_delta") => {
                        if let Some(t) = tools.last_mut() {
                            t.2.push_str(v.pointer("/delta/partial_json").and_then(Value::as_str).unwrap_or(""));
                        }
                    }
                    _ => {}
                },
                Some("message_delta") => {
                    finish = v.pointer("/delta/stop_reason").and_then(Value::as_str).unwrap_or("").to_string()
                }
                _ => {}
            },
        }
    }
    match fmt {
        Format::Openai => {
            let mut msg = json!({"role": "assistant", "content": if text.is_empty() && !tools.is_empty() { Value::Null } else { json!(text) }});
            if !reasoning.is_empty() {
                msg["reasoning_content"] = json!(reasoning);
            }
            if !tools.is_empty() {
                msg["tool_calls"] = tools
                    .iter()
                    .map(|(i, n, a)| json!({"id": i, "type": "function", "function": {"name": n, "arguments": a}}))
                    .collect();
            }
            json!({"id": if id.is_empty() { format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()) } else { id },
                "object": "chat.completion", "created": time::OffsetDateTime::now_utc().unix_timestamp(), "model": model,
                "choices": [{"index": 0, "message": msg, "finish_reason": if finish.is_empty() { "stop" } else { &finish }}],
                "usage": super::response::openai_usage(usage)})
        }
        _ => {
            let mut content = Vec::new();
            if !reasoning.is_empty() {
                content.push(json!({"type": "thinking", "thinking": reasoning, "signature": ""}));
            }
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
            }
            for (i, n, a) in &tools {
                content.push(json!({"type": "tool_use", "id": i, "name": n, "input": super::safe_parse_json(a)}));
            }
            json!({"id": if id.is_empty() { format!("msg_{}", uuid::Uuid::new_v4().simple()) } else { id }, "type": "message", "role": "assistant",
                "model": model, "content": content, "stop_reason": if finish.is_empty() { "end_turn" } else { &finish },
                "stop_sequence": Value::Null, "usage": super::response::claude_usage(usage)})
        }
    }
}

/// Turn a complete response document into the events a stream would have carried.
pub fn synthesize_events(v: &Value, fmt: Format) -> Vec<sse::Event> {
    match fmt {
        Format::Openai => {
            let msg = v.pointer("/choices/0/message").cloned().unwrap_or(json!({}));
            let mut delta = msg.clone();
            if let Some(o) = delta.as_object_mut() {
                o.remove("role");
            }
            let mut tc_delta = json!({});
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                tc_delta["tool_calls"] = calls
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let mut c = c.clone();
                        c["index"] = json!(i);
                        c
                    })
                    .collect();
                delta.as_object_mut().unwrap().remove("tool_calls");
            }
            let base = |d: Value, f: Option<&str>| json!({"id": v["id"], "object": "chat.completion.chunk", "created": v["created"], "model": v["model"], "choices": [{"index": 0, "delta": d, "finish_reason": f}]});
            let mut evs =
                vec![sse::Event::json(&base(json!({"role": "assistant"}), None)), sse::Event::json(&base(delta, None))];
            if tc_delta.get("tool_calls").is_some() {
                evs.push(sse::Event::json(&base(tc_delta, None)));
            }
            let mut last = base(json!({}), v.pointer("/choices/0/finish_reason").and_then(Value::as_str));
            last["usage"] = v.get("usage").cloned().unwrap_or(Value::Null);
            evs.push(sse::Event::json(&last));
            evs.push(sse::Event::data("[DONE]"));
            evs
        }
        _ => {
            let mut evs = vec![];
            let mut start = v.clone();
            start["content"] = json!([]);
            start["stop_reason"] = Value::Null;
            evs.push(sse::Event::named_json("message_start", &json!({"type": "message_start", "message": start})));
            for (i, b) in v.get("content").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let ty = b.get("type").and_then(Value::as_str).unwrap_or("text");
                let (head, delta) = match ty {
                    "tool_use" => (
                        json!({"type": "tool_use", "id": b["id"], "name": b["name"], "input": {}}),
                        json!({"type": "input_json_delta", "partial_json": b["input"].to_string()}),
                    ),
                    "thinking" => (
                        json!({"type": "thinking", "thinking": ""}),
                        json!({"type": "thinking_delta", "thinking": b["thinking"]}),
                    ),
                    _ => (json!({"type": "text", "text": ""}), json!({"type": "text_delta", "text": b["text"]})),
                };
                evs.push(sse::Event::named_json(
                    "content_block_start",
                    &json!({"type": "content_block_start", "index": i, "content_block": head}),
                ));
                evs.push(sse::Event::named_json(
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": i, "delta": delta}),
                ));
                evs.push(sse::Event::named_json(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": i}),
                ));
            }
            evs.push(sse::Event::named_json("message_delta", &json!({"type": "message_delta", "delta": {"stop_reason": v["stop_reason"], "stop_sequence": Value::Null}, "usage": v["usage"]})));
            evs.push(sse::Event::named_json("message_stop", &json!({"type": "message_stop"})));
            evs
        }
    }
}

/// Bytes to append when the stream breaks after 200 was already sent.
pub fn terminal_error(fmt: Format, message: &str) -> Bytes {
    match fmt {
        Format::Claude => sse::Event::named_json(
            "error",
            &json!({"type": "error", "error": {"type": "api_error", "message": message}}),
        )
        .to_bytes(),
        _ => {
            let mut b =
                sse::Event::json(&json!({"error": {"message": message, "type": "server_error"}})).to_bytes().to_vec();
            b.extend_from_slice(&sse::Event::data("[DONE]").to_bytes());
            Bytes::from(b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_openai_from_events() {
        let evs = vec![
            sse::Event::json(
                &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hel"}}]}),
            ),
            sse::Event::json(&json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "lo"}}]})),
            sse::Event::json(
                &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ),
        ];
        let v = assemble(&evs, Format::Openai, "m", &TokenCounts { prompt: 1, completion: 2, ..Default::default() });
        assert_eq!(v["choices"][0]["message"]["content"], "Hello");
        assert_eq!(v["usage"]["total_tokens"], 3);
        let back = synthesize_events(&v, Format::Openai);
        assert_eq!(back.last().unwrap().data, "[DONE]");
    }
}
