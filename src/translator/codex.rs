//! The OpenAI Responses API as an upstream, for ChatGPT-subscription Codex
//! (`chatgpt.com/backend-api/codex/responses`). The router speaks chat completions
//! internally, so a chat request becomes a Responses request here and the Responses
//! event stream is turned back into chat chunks, which the usual stream translator then
//! maps to whatever the client speaks. (The inbound `/v1/responses` direction is in
//! `responses.rs`.) Request shape follows upstream's `executors/codex.js`: always
//! `stream: true` and `store: false`, system prompts as `instructions` (Codex's own
//! default when there is none), reasoning effort from the request or a model suffix such
//! as `-high`, and only the fields the Codex backend accepts.

use serde_json::{json, Map, Value};

use super::text_of;
use crate::sse::Event;

/// Codex CLI's stock prompt; the backend wants `instructions` to be present.
pub const DEFAULT_INSTRUCTIONS: &str = include_str!("codex_instructions.txt");

/// Models served over the "Responses Lite" transport: tools and instructions travel as
/// input items instead of top-level fields.
const LITE_MODELS: [&str; 4] = ["gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-sol", "gpt-6-luna"];
const EFFORTS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];
const MAX_NAME: usize = 128;
const MAX_CALL_ID: usize = 64;

/// `gpt-5.3-codex-high` -> (`gpt-5.3-codex`, Some("high")).
fn split_effort(model: &str) -> (&str, Option<&'static str>) {
    for level in EFFORTS {
        if let Some(base) = model.strip_suffix(level).and_then(|b| b.strip_suffix('-')) {
            return (base, Some(level));
        }
    }
    (model, None)
}

fn base_model(model: &str) -> (&str, Option<&'static str>) {
    split_effort(model.strip_suffix("[1m]").unwrap_or(model))
}

pub fn is_lite(model: &str) -> bool {
    LITE_MODELS.contains(&base_model(model).0)
}

fn normalize_effort(effort: &str, lite: bool) -> &str {
    match effort {
        "none" | "minimal" if lite => "low",
        "max" | "ultra" if lite => "max",
        "max" | "ultra" => "xhigh",
        other => other,
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn call_id(v: Option<&Value>) -> String {
    let id = v.and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        format!("call_{}", uuid::Uuid::new_v4().simple())
    } else {
        truncate(id, MAX_CALL_ID)
    }
}

/// Chat message content as Responses content parts. Images only ride on user turns.
fn parts(content: &Value, text_type: &str) -> Vec<Value> {
    match content {
        Value::String(s) if !s.is_empty() => vec![json!({"type": text_type, "text": s})],
        Value::Array(a) => a
            .iter()
            .filter_map(|c| match c.get("type").and_then(Value::as_str) {
                Some("image_url") if text_type == "input_text" => {
                    let img = c.get("image_url")?;
                    let url = img.as_str().or_else(|| img.get("url").and_then(Value::as_str))?;
                    let detail = img.get("detail").and_then(Value::as_str).unwrap_or("auto");
                    Some(json!({"type": "input_image", "image_url": url, "detail": detail}))
                }
                Some("text" | "input_text" | "output_text") => c
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(|t| json!({"type": text_type, "text": t})),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The Codex backend's schema validator has no Unicode property escapes: a `pattern`
/// using `\p{..}` fails the whole request, so such patterns are dropped.
fn strip_unicode_patterns(v: &mut Value) {
    match v {
        Value::Object(o) => {
            if o.get("pattern").and_then(Value::as_str).is_some_and(|p| p.contains("\\p{") || p.contains("\\P{")) {
                o.remove("pattern");
            }
            for c in o.values_mut() {
                strip_unicode_patterns(c);
            }
        }
        Value::Array(a) => {
            for c in a {
                strip_unicode_patterns(c);
            }
        }
        _ => {}
    }
}

fn tool_of(t: &Value) -> Option<Value> {
    if t.get("type").and_then(Value::as_str).is_some_and(|ty| ty != "function") {
        return None;
    }
    let f = t.get("function").unwrap_or(t);
    let name = f.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty())?;
    let mut params = f
        .get("parameters")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    if params.get("type").and_then(Value::as_str) == Some("object") && params.get("properties").is_none() {
        params["properties"] = json!({});
    }
    strip_unicode_patterns(&mut params);
    let mut tool = json!({
        "type": "function",
        "name": truncate(name, MAX_NAME),
        "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
        "parameters": params,
    });
    if let Some(strict) = f.get("strict").and_then(Value::as_bool) {
        tool["strict"] = json!(strict);
    }
    Some(tool)
}

/// Chat Completions request -> Codex Responses request. `model` is the upstream model
/// id (it may carry an effort suffix); `session` keys the prompt cache.
pub fn chat_to_responses(body: &Value, model: &str, session: &str) -> Value {
    let (base, suffix) = base_model(model);
    let lite = is_lite(model);
    let mut system: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();
    let null = Value::Null;

    for m in body.get("messages").and_then(Value::as_array).into_iter().flatten() {
        let content = m.get("content").unwrap_or(&null);
        match m.get("role").and_then(Value::as_str).unwrap_or("user") {
            "system" | "developer" => {
                let t = text_of(content);
                if !t.trim().is_empty() {
                    system.push(t);
                }
            }
            "tool" => input.push(json!({
                "type": "function_call_output",
                "call_id": call_id(m.get("tool_call_id")),
                "output": text_of(content),
            })),
            "assistant" => {
                let c = parts(content, "output_text");
                if !c.is_empty() {
                    input.push(json!({"type": "message", "role": "assistant", "content": c}));
                }
                for tc in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let name = tc.pointer("/function/name").and_then(Value::as_str).unwrap_or("").trim();
                    if name.is_empty() {
                        continue;
                    }
                    let args = match tc.pointer("/function/arguments") {
                        Some(Value::String(s)) if !s.is_empty() => s.clone(),
                        Some(v @ Value::Object(_)) => v.to_string(),
                        _ => "{}".to_string(),
                    };
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call_id(tc.get("id")),
                        "name": truncate(name, MAX_NAME),
                        "arguments": args,
                    }));
                }
            }
            _ => {
                let c = parts(content, "input_text");
                if !c.is_empty() {
                    input.push(json!({"type": "message", "role": "user", "content": c}));
                }
            }
        }
    }
    if input.is_empty() {
        input.push(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "..."}]}));
    }

    let tools: Vec<Value> =
        body.get("tools").and_then(Value::as_array).into_iter().flatten().filter_map(tool_of).collect();
    let tool_choice = match body.get("tool_choice") {
        Some(Value::String(s)) if matches!(s.as_str(), "auto" | "none" | "required") => Some(json!(s)),
        Some(Value::Object(o)) => o
            .get("function")
            .and_then(|f| f.get("name"))
            .or_else(|| o.get("name"))
            .and_then(Value::as_str)
            .filter(|n| tools.iter().any(|t| t["name"].as_str() == Some(*n)))
            .map(|n| json!({"type": "function", "name": n})),
        _ => None,
    };

    let effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| body.pointer("/reasoning/effort").and_then(Value::as_str))
        .or(suffix)
        .unwrap_or(if lite { "medium" } else { "low" });
    let effort = normalize_effort(effort, lite).to_string();

    let own = system.join("\n\n");
    let instructions = if own.trim().is_empty() { DEFAULT_INSTRUCTIONS.to_string() } else { own };

    let mut out = Map::new();
    out.insert("model".into(), json!(base));
    out.insert("stream".into(), json!(true));
    out.insert("store".into(), json!(false));
    out.insert(
        "prompt_cache_key".into(),
        json!(body.get("prompt_cache_key").and_then(Value::as_str).filter(|k| !k.is_empty()).unwrap_or(session)),
    );
    if lite {
        // Codex 0.155+: tools and instructions are the first input items.
        let prefix = [
            json!({"type": "additional_tools", "role": "developer", "tools": tools}),
            json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": instructions}]}),
        ];
        for item in prefix.into_iter().rev() {
            input.insert(0, item);
        }
        out.insert("instructions".into(), json!(""));
        out.insert("tools".into(), Value::Null);
        out.insert("tool_choice".into(), tool_choice.unwrap_or_else(|| json!("auto")));
        out.insert("parallel_tool_calls".into(), json!(false));
        out.insert("reasoning".into(), json!({"effort": effort, "context": "all_turns"}));
    } else {
        out.insert("instructions".into(), json!(instructions));
        if !tools.is_empty() {
            out.insert("tools".into(), json!(tools));
            if let Some(tc) = tool_choice {
                out.insert("tool_choice".into(), tc);
            }
        }
        out.insert("reasoning".into(), json!({"effort": effort, "summary": "auto"}));
    }
    out.insert("input".into(), Value::Array(input));
    if effort != "none" {
        out.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    if matches!(body.get("service_tier").and_then(Value::as_str), Some("fast" | "priority")) {
        out.insert("service_tier".into(), json!("priority"));
    }
    Value::Object(out)
}

/// Responses event stream -> Chat Completions chunks.
pub struct FromResponses {
    id: String,
    model: String,
    sent_role: bool,
    /// Function call item ids in output order (the chat tool index) and whether any
    /// argument delta arrived for them.
    tools: Vec<(String, bool)>,
    done: bool,
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn chat_usage(u: Option<&Value>) -> Option<Value> {
    let u = u?;
    let n = |p: &str| u.pointer(p).and_then(Value::as_i64).unwrap_or(0);
    let (input, output) = (n("/input_tokens"), n("/output_tokens"));
    Some(json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output,
        "prompt_tokens_details": {"cached_tokens": n("/input_tokens_details/cached_tokens")},
        "completion_tokens_details": {"reasoning_tokens": n("/output_tokens_details/reasoning_tokens")},
    }))
}

impl FromResponses {
    pub fn new(model: &str) -> FromResponses {
        FromResponses {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            sent_role: false,
            tools: Vec::new(),
            done: false,
        }
    }

    fn chunk(&mut self, mut delta: Value, finish: Option<&str>, usage: Option<Value>) -> Event {
        if !self.sent_role {
            self.sent_role = true;
            delta["role"] = json!("assistant");
        }
        let mut v = json!({
            "id": self.id, "object": "chat.completion.chunk", "created": time::OffsetDateTime::now_utc().unix_timestamp(),
            "model": self.model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        if let Some(u) = usage {
            v["usage"] = u;
        }
        Event::json(&v)
    }

    fn tool_delta(&mut self, idx: usize, call: Option<(&str, &str)>, args: &str) -> Event {
        let mut tc = json!({"index": idx, "function": {"arguments": args}});
        if let Some((id, name)) = call {
            tc["id"] = json!(id);
            tc["type"] = json!("function");
            tc["function"]["name"] = json!(name);
        }
        self.chunk(json!({"tool_calls": [tc]}), None, None)
    }

    pub fn feed(&mut self, ev: &Event) -> Vec<Event> {
        if self.done {
            return vec![];
        }
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return vec![] };
        let ty = v.get("type").and_then(Value::as_str).or(ev.event.as_deref()).unwrap_or("");
        match ty {
            "response.output_item.added" => {
                let item = &v["item"];
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return vec![];
                }
                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                let call = item.get("call_id").and_then(Value::as_str).unwrap_or(id);
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                let idx = self.tools.len();
                self.tools.push((id.to_string(), false));
                vec![self.tool_delta(idx, Some((call, name)), "")]
            }
            "response.function_call_arguments.delta" => {
                let d = text(&v, "delta");
                if d.is_empty() || self.tools.is_empty() {
                    return vec![];
                }
                let item = text(&v, "item_id");
                let idx = self.tools.iter().position(|t| t.0 == item).unwrap_or(self.tools.len() - 1);
                self.tools[idx].1 = true;
                vec![self.tool_delta(idx, None, d)]
            }
            // Some backends only send the arguments with the finished item.
            "response.output_item.done" => {
                let item = &v["item"];
                let args = item.get("arguments").and_then(Value::as_str).unwrap_or("");
                if item.get("type").and_then(Value::as_str) != Some("function_call") || args.is_empty() {
                    return vec![];
                }
                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                match self.tools.iter().position(|t| t.0 == id) {
                    Some(idx) if !self.tools[idx].1 => {
                        self.tools[idx].1 = true;
                        vec![self.tool_delta(idx, None, args)]
                    }
                    _ => vec![],
                }
            }
            "response.output_text.delta" => {
                let d = text(&v, "delta");
                if d.is_empty() {
                    return vec![];
                }
                vec![self.chunk(json!({"content": d}), None, None)]
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let d = text(&v, "delta");
                if d.is_empty() {
                    return vec![];
                }
                vec![self.chunk(json!({"reasoning_content": d}), None, None)]
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.done = true;
                let r = &v["response"];
                let finish = if !self.tools.is_empty() {
                    "tool_calls"
                } else if ty == "response.incomplete" {
                    match r.pointer("/incomplete_details/reason").and_then(Value::as_str) {
                        Some("content_filter") => "content_filter",
                        _ => "length",
                    }
                } else {
                    "stop"
                };
                vec![self.chunk(json!({}), Some(finish), chat_usage(r.get("usage")))]
            }
            "response.failed" | "error" => {
                self.done = true;
                let msg = [v.pointer("/response/error/message"), v.pointer("/error/message"), v.get("message")]
                    .into_iter()
                    .flatten()
                    .find_map(Value::as_str)
                    .unwrap_or("Codex request failed");
                vec![Event::json(&json!({"error": {"message": msg, "type": "server_error"}}))]
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(c: &mut FromResponses, evs: &[Value]) -> Vec<Value> {
        evs.iter()
            .flat_map(|e| c.feed(&Event::json(e)))
            .map(|e| serde_json::from_str::<Value>(&e.data).unwrap())
            .collect()
    }

    #[test]
    fn chat_request_becomes_codex_request() {
        let body = json!({
            "model": "x", "max_tokens": 10, "temperature": 0.2,
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA"}}]},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "ls", "arguments": "{\"p\":1}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "a b"}
            ],
            "tools": [{"type": "function", "function": {"name": "ls", "description": "d", "parameters": {"type": "object", "properties": {"p": {"type": "string", "pattern": "^\\p{L}+$"}}}}}],
            "tool_choice": {"type": "function", "function": {"name": "ls"}}
        });
        let r = chat_to_responses(&body, "gpt-5.3-codex-high", "sess");
        assert_eq!(r["model"], "gpt-5.3-codex");
        assert_eq!(r["stream"], true);
        assert_eq!(r["store"], false);
        assert_eq!(r["instructions"], "Be brief.");
        assert_eq!(r["reasoning"], json!({"effort": "high", "summary": "auto"}));
        assert_eq!(r["prompt_cache_key"], "sess");
        assert!(r.get("max_tokens").is_none() && r.get("temperature").is_none());
        let input = r["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["content"][0], json!({"type": "input_text", "text": "hi"}));
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(input[1], json!({"type": "function_call", "call_id": "c1", "name": "ls", "arguments": "{\"p\":1}"}));
        assert_eq!(input[2], json!({"type": "function_call_output", "call_id": "c1", "output": "a b"}));
        assert_eq!(r["tools"][0]["name"], "ls");
        assert!(r["tools"][0]["parameters"]["properties"]["p"].get("pattern").is_none());
        assert_eq!(r["tool_choice"], json!({"type": "function", "name": "ls"}));
    }

    #[test]
    fn default_instructions_and_lite_models() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        let r = chat_to_responses(&body, "gpt-5.6-sol", "s");
        assert!(r["instructions"].as_str().unwrap().starts_with("You are Codex"));
        assert!(r.get("tools").is_none());
        let lite = chat_to_responses(&body, "gpt-6-sol[1m]", "s");
        assert_eq!(lite["instructions"], "");
        assert_eq!(lite["input"][0]["type"], "additional_tools");
        assert_eq!(lite["input"][1]["role"], "developer");
        assert_eq!(lite["reasoning"]["effort"], "medium");
        assert!(is_lite("gpt-6-luna-high") && !is_lite("gpt-6-astra"));
    }

    #[test]
    fn stream_becomes_chat_chunks() {
        let mut c = FromResponses::new("gpt-5.6-sol");
        let out = feed_all(
            &mut c,
            &[
                json!({"type": "response.created", "response": {}}),
                json!({"type": "response.reasoning_summary_text.delta", "delta": "think"}),
                json!({"type": "response.output_text.delta", "delta": "Hel"}),
                json!({"type": "response.output_text.delta", "delta": "lo"}),
                json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "ls"}}),
                json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"a\":"}),
                json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "1}"}),
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 5, "output_tokens": 7, "input_tokens_details": {"cached_tokens": 2}}}}),
                json!({"type": "response.output_text.delta", "delta": "ignored"}),
            ],
        );
        assert_eq!(out.len(), 7);
        assert_eq!(out[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(out[0]["choices"][0]["delta"]["reasoning_content"], "think");
        assert_eq!(out[1]["choices"][0]["delta"]["content"], "Hel");
        assert_eq!(out[3]["choices"][0]["delta"]["tool_calls"][0]["id"], "call_1");
        assert_eq!(out[4]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"a\":");
        let last = &out[6];
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(last["usage"]["prompt_tokens"], 5);
        assert_eq!(last["usage"]["prompt_tokens_details"]["cached_tokens"], 2);
    }

    #[test]
    fn stream_failure_becomes_error_chunk() {
        let mut c = FromResponses::new("m");
        let out = feed_all(&mut c, &[json!({"type": "response.failed", "response": {"error": {"message": "boom"}}})]);
        assert_eq!(out[0]["error"]["message"], "boom");
        assert!(c.feed(&Event::json(&json!({"type": "response.output_text.delta", "delta": "x"}))).is_empty());
    }
}
