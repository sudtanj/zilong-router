//! OpenAI Responses API (`/v1/responses`) on top of Chat Completions. Codex and other
//! Responses-only clients are served by translating the request to a chat request,
//! routing it like any other, and turning the chat answer (JSON or SSE chunks) back
//! into a Responses document or event stream.
//!
//! Stateless: `previous_response_id`, `store` and server-side tools (web search, file
//! search, ...) are not supported; clients that send the full history (Codex with
//! `store: false`) work. Freeform `custom` tools (Codex's `apply_patch`) become function
//! tools with one `input` string and are turned back into `custom_tool_call` items.

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use super::text_of;
use crate::sse;

/// What the answer translation needs to know about the request.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub model: String,
    /// Names of `custom` (freeform) tools, answered as `custom_tool_call` items.
    pub custom_tools: HashSet<String>,
    pub id: String,
    pub created_at: i64,
}

impl Ctx {
    fn new(model: &str) -> Ctx {
        Ctx {
            model: model.to_string(),
            custom_tools: HashSet::new(),
            id: new_id("resp"),
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        }
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Responses request -> Chat Completions request.
pub fn request_to_chat(body: &Value) -> Result<(Value, Ctx), String> {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let mut ctx = Ctx::new(&model);

    let mut system: Vec<String> = Vec::new();
    if let Some(s) = body.get("instructions").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        system.push(s.to_string());
    }
    let mut msgs: Vec<Value> = Vec::new();
    match body.get("input") {
        Some(Value::String(s)) => msgs.push(json!({"role": "user", "content": s})),
        Some(Value::Array(items)) => {
            for item in items {
                input_item(item, &mut msgs, &mut system);
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err("input must be a string or an array of items".into()),
    }
    // One leading system message: some providers reject system messages anywhere else.
    if !system.is_empty() {
        msgs.insert(0, json!({"role": "system", "content": system.join("\n\n")}));
    }
    if msgs.iter().all(|m| m["role"] == "system") {
        return Err("input has no messages".into());
    }

    let mut out = json!({"model": model, "messages": msgs});
    let o = out.as_object_mut().unwrap();
    o.insert("stream".into(), json!(body.get("stream").and_then(Value::as_bool).unwrap_or(false)));

    let mut tools = Vec::new();
    for t in body.get("tools").and_then(Value::as_array).into_iter().flatten() {
        if let Some(ct) = chat_tool(t, &mut ctx) {
            tools.push(ct);
        }
    }
    if !tools.is_empty() {
        o.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = body.get("tool_choice") {
            if let Some(tc) = chat_tool_choice(tc) {
                o.insert("tool_choice".into(), tc);
            }
        }
        if let Some(p) = body.get("parallel_tool_calls").filter(|p| p.is_boolean()) {
            o.insert("parallel_tool_calls".into(), p.clone());
        }
    }
    if let Some(n) = body.get("max_output_tokens").filter(|n| n.is_u64()) {
        o.insert("max_tokens".into(), n.clone());
    }
    for k in ["temperature", "top_p"] {
        if let Some(v) = body.get(k).filter(|v| v.is_number()) {
            o.insert(k.into(), v.clone());
        }
    }
    if let Some(f) = body.pointer("/text/format") {
        match f.get("type").and_then(Value::as_str) {
            Some("json_schema") => {
                let mut js = Map::new();
                js.insert("name".into(), f.get("name").cloned().unwrap_or(json!("output")));
                if let Some(s) = f.get("schema") {
                    js.insert("schema".into(), s.clone());
                }
                if let Some(s) = f.get("strict") {
                    js.insert("strict".into(), s.clone());
                }
                o.insert("response_format".into(), json!({"type": "json_schema", "json_schema": js}));
            }
            Some("json_object") => {
                o.insert("response_format".into(), json!({"type": "json_object"}));
            }
            _ => {}
        }
    }
    Ok((out, ctx))
}

fn input_item(item: &Value, msgs: &mut Vec<Value>, system: &mut Vec<String>) {
    let ty = item.get("type").and_then(Value::as_str).unwrap_or("message");
    match ty {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            let content = item.get("content").cloned().unwrap_or(Value::Null);
            match role {
                "system" | "developer" => {
                    let t = text_of(&content);
                    if !t.trim().is_empty() {
                        system.push(t);
                    }
                }
                "assistant" => {
                    let t = text_of(&content);
                    if !t.is_empty() {
                        msgs.push(json!({"role": "assistant", "content": t}));
                    }
                }
                _ => msgs.push(json!({"role": "user", "content": user_content(&content)})),
            }
        }
        "function_call" | "custom_tool_call" => {
            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("").to_string();
            let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let arguments = if ty == "function_call" {
                item.get("arguments").and_then(Value::as_str).unwrap_or("{}").to_string()
            } else {
                json!({"input": item.get("input").and_then(Value::as_str).unwrap_or("")}).to_string()
            };
            let call = json!({"id": call_id, "type": "function", "function": {"name": name, "arguments": arguments}});
            // Calls of one turn go on one assistant message (with its text, if any).
            match msgs.last_mut() {
                Some(m) if m["role"] == "assistant" => {
                    if !m["tool_calls"].is_array() {
                        m["tool_calls"] = json!([]);
                    }
                    m["tool_calls"].as_array_mut().unwrap().push(call);
                }
                _ => msgs.push(json!({"role": "assistant", "content": Value::Null, "tool_calls": [call]})),
            }
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
            let output = match item.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(v @ Value::Array(_)) => text_of(v),
                Some(Value::Null) | None => String::new(),
                Some(v) => v.to_string(),
            };
            msgs.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
        }
        // Reasoning is not replayed (chat has no slot for it); server-side tool calls
        // (web search, ...) have nothing to replay.
        _ => {}
    }
}

/// User content: a plain string when it is only text, else chat content parts.
fn user_content(content: &Value) -> Value {
    let Value::Array(parts) = content else { return json!(text_of(content)) };
    let has_image = parts.iter().any(|p| p.get("type").and_then(Value::as_str) == Some("input_image"));
    if !has_image {
        return json!(text_of(content));
    }
    let out: Vec<Value> = parts
        .iter()
        .filter_map(|p| match p.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("text") | Some("output_text") => {
                Some(json!({"type": "text", "text": p.get("text").and_then(Value::as_str).unwrap_or("")}))
            }
            Some("input_image") => {
                let url = p.get("image_url").and_then(|u| {
                    u.as_str().map(str::to_string).or_else(|| u.get("url").and_then(Value::as_str).map(str::to_string))
                })?;
                let mut img = json!({"url": url});
                if let Some(d) = p.get("detail").and_then(Value::as_str) {
                    img["detail"] = json!(d);
                }
                Some(json!({"type": "image_url", "image_url": img}))
            }
            _ => None,
        })
        .collect();
    Value::Array(out)
}

fn chat_tool(t: &Value, ctx: &mut Ctx) -> Option<Value> {
    let name = t.get("name").and_then(Value::as_str)?;
    let description = t.get("description").and_then(Value::as_str).unwrap_or("");
    match t.get("type").and_then(Value::as_str)? {
        "function" => {
            let params = t
                .get("parameters")
                .filter(|p| p.is_object())
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(
                json!({"type": "function", "function": {"name": name, "description": description, "parameters": params}}),
            )
        }
        "custom" => {
            ctx.custom_tools.insert(name.to_string());
            let mut desc =
                format!("{description}\n\nPass the raw tool input as the `input` string (not JSON-encoded inside it).");
            if let Some(def) = t.pointer("/format/definition").and_then(Value::as_str) {
                let syntax = t.pointer("/format/syntax").and_then(Value::as_str).unwrap_or("grammar");
                desc.push_str(&format!("\nThe input must match this {syntax} grammar:\n{def}"));
            }
            Some(json!({"type": "function", "function": {"name": name, "description": desc.trim(), "parameters": {
                "type": "object",
                "properties": {"input": {"type": "string", "description": "The raw tool input."}},
                "required": ["input"]
            }}}))
        }
        // Server-side tools (web_search, file_search, local_shell, ...) need OpenAI.
        _ => None,
    }
}

fn chat_tool_choice(tc: &Value) -> Option<Value> {
    match tc {
        Value::String(s) if matches!(s.as_str(), "auto" | "none" | "required") => Some(tc.clone()),
        Value::Object(o) => {
            let name = o.get("name").and_then(Value::as_str)?;
            Some(json!({"type": "function", "function": {"name": name}}))
        }
        _ => None,
    }
}

fn usage(u: Option<&Value>) -> Value {
    let n = |p: &str| u.and_then(|u| u.pointer(p)).and_then(Value::as_i64).unwrap_or(0);
    let input = n("/prompt_tokens");
    let output = n("/completion_tokens");
    let total = u.and_then(|u| u.get("total_tokens")).and_then(Value::as_i64).unwrap_or(input + output);
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": n("/prompt_tokens_details/cached_tokens")},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": n("/completion_tokens_details/reasoning_tokens")},
        "total_tokens": total,
    })
}

fn reasoning_item(id: &str, text: &str) -> Value {
    json!({"type": "reasoning", "id": id, "summary": [{"type": "summary_text", "text": text}]})
}

fn message_item(id: &str, text: &str, status: &str) -> Value {
    json!({"type": "message", "id": id, "status": status, "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}]})
}

fn tool_item(ctx: &Ctx, call_id: &str, name: &str, arguments: &str) -> Value {
    if ctx.custom_tools.contains(name) {
        let input = serde_json::from_str::<Value>(arguments)
            .ok()
            .and_then(|v| v.get("input").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| arguments.to_string());
        json!({"type": "custom_tool_call", "id": new_id("ctc"), "status": "completed", "call_id": call_id, "name": name, "input": input})
    } else {
        json!({"type": "function_call", "id": new_id("fc"), "status": "completed", "call_id": call_id, "name": name, "arguments": arguments})
    }
}

fn response_doc(ctx: &Ctx, status: &str, output: Vec<Value>, usage: Value, finish: &str) -> Value {
    let mut r = json!({
        "id": ctx.id, "object": "response", "created_at": ctx.created_at, "status": status,
        "model": ctx.model, "output": output, "usage": usage,
        "error": Value::Null, "incomplete_details": Value::Null,
    });
    if finish == "length" {
        r["status"] = json!("incomplete");
        r["incomplete_details"] = json!({"reason": "max_output_tokens"});
    }
    r
}

/// Complete chat completion -> Responses document.
pub fn chat_to_response(v: &Value, ctx: &Ctx) -> Value {
    let msg = v.pointer("/choices/0/message").cloned().unwrap_or(json!({}));
    let mut output = Vec::new();
    let reasoning =
        ["reasoning_content", "reasoning"].iter().find_map(|k| msg.get(*k).and_then(Value::as_str)).unwrap_or("");
    if !reasoning.is_empty() {
        output.push(reasoning_item(&new_id("rs"), reasoning));
    }
    let text = text_of(msg.get("content").unwrap_or(&Value::Null));
    if !text.is_empty() {
        output.push(message_item(&new_id("msg"), &text, "completed"));
    }
    for tc in msg.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
        let call_id = tc.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| new_id("call"));
        let name = tc.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
        let args = tc.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
        output.push(tool_item(ctx, &call_id, name, args));
    }
    let finish = v.pointer("/choices/0/finish_reason").and_then(Value::as_str).unwrap_or("stop");
    response_doc(ctx, "completed", output, usage(v.get("usage")), finish)
}

/// The item being streamed right now (one at a time: reasoning, then text).
enum Open {
    None,
    Reasoning { id: String, text: String },
    Text { id: String, text: String },
}

/// Chat Completions SSE chunks -> Responses SSE events.
pub struct StreamConverter {
    ctx: Ctx,
    seq: u64,
    started: bool,
    pub finished: bool,
    open: Open,
    output: Vec<Value>,
    /// Tool calls by chat `index`: (call_id, name, arguments). Emitted whole at the end.
    tools: Vec<(String, String, String)>,
    usage: Option<Value>,
    finish: String,
}

impl StreamConverter {
    pub fn new(ctx: Ctx) -> StreamConverter {
        StreamConverter {
            ctx,
            seq: 0,
            started: false,
            finished: false,
            open: Open::None,
            output: Vec::new(),
            tools: Vec::new(),
            usage: None,
            finish: String::new(),
        }
    }

    fn ev(&mut self, ty: &str, mut v: Value) -> sse::Event {
        v["type"] = json!(ty);
        v["sequence_number"] = json!(self.seq);
        self.seq += 1;
        sse::Event::named_json(ty, &v)
    }

    fn start(&mut self, out: &mut Vec<sse::Event>) {
        if self.started {
            return;
        }
        self.started = true;
        let r = response_doc(&self.ctx, "in_progress", vec![], Value::Null, "");
        out.push(self.ev("response.created", json!({"response": r})));
        out.push(self.ev("response.in_progress", json!({"response": r})));
    }

    fn close_open(&mut self, out: &mut Vec<sse::Event>) {
        let idx = self.output.len();
        match std::mem::replace(&mut self.open, Open::None) {
            Open::None => {}
            Open::Reasoning { id, text } => {
                out.push(self.ev(
                    "response.reasoning_summary_text.done",
                    json!({"item_id": id, "output_index": idx, "summary_index": 0, "text": text}),
                ));
                out.push(self.ev("response.reasoning_summary_part.done", json!({"item_id": id, "output_index": idx, "summary_index": 0, "part": {"type": "summary_text", "text": text}})));
                let item = reasoning_item(&id, &text);
                out.push(self.ev("response.output_item.done", json!({"output_index": idx, "item": item})));
                self.output.push(item);
            }
            Open::Text { id, text } => {
                out.push(self.ev(
                    "response.output_text.done",
                    json!({"item_id": id, "output_index": idx, "content_index": 0, "text": text}),
                ));
                out.push(self.ev("response.content_part.done", json!({"item_id": id, "output_index": idx, "content_index": 0, "part": {"type": "output_text", "text": text, "annotations": []}})));
                let item = message_item(&id, &text, "completed");
                out.push(self.ev("response.output_item.done", json!({"output_index": idx, "item": item})));
                self.output.push(item);
            }
        }
    }

    fn reasoning_delta(&mut self, d: &str, out: &mut Vec<sse::Event>) {
        if !matches!(self.open, Open::Reasoning { .. }) {
            self.close_open(out);
            let id = new_id("rs");
            let idx = self.output.len();
            out.push(self.ev(
                "response.output_item.added",
                json!({"output_index": idx, "item": {"type": "reasoning", "id": id, "summary": []}}),
            ));
            out.push(self.ev("response.reasoning_summary_part.added", json!({"item_id": id, "output_index": idx, "summary_index": 0, "part": {"type": "summary_text", "text": ""}})));
            self.open = Open::Reasoning { id, text: String::new() };
        }
        let idx = self.output.len();
        let id = match &mut self.open {
            Open::Reasoning { id, text } => {
                text.push_str(d);
                id.clone()
            }
            _ => unreachable!(),
        };
        out.push(self.ev(
            "response.reasoning_summary_text.delta",
            json!({"item_id": id, "output_index": idx, "summary_index": 0, "delta": d}),
        ));
    }

    fn text_delta(&mut self, d: &str, out: &mut Vec<sse::Event>) {
        if !matches!(self.open, Open::Text { .. }) {
            self.close_open(out);
            let id = new_id("msg");
            let idx = self.output.len();
            out.push(self.ev("response.output_item.added", json!({"output_index": idx, "item": {"type": "message", "id": id, "status": "in_progress", "role": "assistant", "content": []}})));
            out.push(self.ev("response.content_part.added", json!({"item_id": id, "output_index": idx, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}})));
            self.open = Open::Text { id, text: String::new() };
        }
        let idx = self.output.len();
        let id = match &mut self.open {
            Open::Text { id, text } => {
                text.push_str(d);
                id.clone()
            }
            _ => unreachable!(),
        };
        out.push(self.ev(
            "response.output_text.delta",
            json!({"item_id": id, "output_index": idx, "content_index": 0, "delta": d}),
        ));
    }

    /// Feed one chat SSE event.
    pub fn feed(&mut self, ev: &sse::Event) -> Vec<sse::Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.start(&mut out);
        if ev.data.trim() == "[DONE]" {
            out.extend(self.finish());
            return out;
        }
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return out };
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            let message =
                err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string());
            out.extend(self.fail(&message));
            return out;
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        let Some(c) = v.pointer("/choices/0") else { return out };
        let delta = c.get("delta").cloned().unwrap_or(json!({}));
        for k in ["reasoning_content", "reasoning"] {
            if let Some(d) = delta.get(k).and_then(Value::as_str).filter(|d| !d.is_empty()) {
                self.reasoning_delta(d, &mut out);
                break;
            }
        }
        if let Some(d) = delta.get("content").and_then(Value::as_str).filter(|d| !d.is_empty()) {
            self.text_delta(d, &mut out);
        }
        for tc in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let i =
                tc.get("index").and_then(Value::as_u64).unwrap_or(self.tools.len().saturating_sub(1) as u64) as usize;
            while self.tools.len() <= i {
                self.tools.push((String::new(), String::new(), String::new()));
            }
            if let Some(id) = tc.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                self.tools[i].0 = id.to_string();
            }
            if let Some(n) = tc.pointer("/function/name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                self.tools[i].1 = n.to_string();
            }
            if let Some(a) = tc.pointer("/function/arguments").and_then(Value::as_str) {
                self.tools[i].2.push_str(a);
            }
        }
        if let Some(f) = c.get("finish_reason").and_then(Value::as_str) {
            self.finish = f.to_string();
        }
        out
    }

    /// End of the chat stream: close items and send `response.completed`.
    pub fn finish(&mut self) -> Vec<sse::Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.start(&mut out);
        self.close_open(&mut out);
        for (call_id, name, args) in std::mem::take(&mut self.tools) {
            if name.is_empty() {
                continue;
            }
            let call_id = if call_id.is_empty() { new_id("call") } else { call_id };
            let args = if args.trim().is_empty() { "{}".to_string() } else { args };
            let item = tool_item(&self.ctx, &call_id, &name, &args);
            let idx = self.output.len();
            let mut added = item.clone();
            added["status"] = json!("in_progress");
            out.push(self.ev("response.output_item.added", json!({"output_index": idx, "item": added})));
            if item["type"] == "function_call" {
                out.push(self.ev(
                    "response.function_call_arguments.done",
                    json!({"item_id": item["id"], "output_index": idx, "arguments": args}),
                ));
            }
            out.push(self.ev("response.output_item.done", json!({"output_index": idx, "item": item})));
            self.output.push(item);
        }
        let finish = if self.finish.is_empty() { "stop".to_string() } else { self.finish.clone() };
        let r = response_doc(&self.ctx, "completed", self.output.clone(), usage(self.usage.as_ref()), &finish);
        out.push(self.ev("response.completed", json!({"response": r})));
        self.finished = true;
        out
    }

    /// The stream broke after it started: `response.failed`.
    pub fn fail(&mut self, message: &str) -> Vec<sse::Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.start(&mut out);
        let mut r = response_doc(&self.ctx, "failed", self.output.clone(), usage(self.usage.as_ref()), "");
        r["error"] = json!({"code": "server_error", "message": message});
        out.push(self.ev("response.failed", json!({"response": r})));
        self.finished = true;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex_request() -> Value {
        json!({
            "model": "auto-coding",
            "instructions": "You are Codex.",
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "sandbox: full"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
                {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "x"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Running ls."}]},
                {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":[\"ls\"]}"},
                {"type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.txt"},
                {"type": "custom_tool_call_output", "call_id": "call_2", "output": "Done"}
            ],
            "tools": [
                {"type": "function", "name": "shell", "description": "Run", "parameters": {"type": "object", "properties": {"command": {"type": "array"}}}, "strict": false},
                {"type": "custom", "name": "apply_patch", "description": "Edit files", "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}},
                {"type": "web_search"}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": {"effort": "medium"},
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "abc"
        })
    }

    #[test]
    fn codex_request_becomes_chat() {
        let (c, ctx) = request_to_chat(&codex_request()).unwrap();
        let m = c["messages"].as_array().unwrap();
        assert_eq!(m[0], json!({"role": "system", "content": "You are Codex.\n\nsandbox: full"}));
        assert_eq!(m[1], json!({"role": "user", "content": "list files"}));
        assert_eq!(m[2]["role"], "assistant");
        assert_eq!(m[2]["content"], "Running ls.");
        let calls = m[2]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[1]["function"]["arguments"], "{\"input\":\"*** Begin Patch\"}");
        assert_eq!(m[3], json!({"role": "tool", "tool_call_id": "call_1", "content": "a.txt"}));
        assert_eq!(m[4], json!({"role": "tool", "tool_call_id": "call_2", "content": "Done"}));
        assert_eq!(m.len(), 5);
        let tools = c["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2, "web_search is dropped");
        assert_eq!(tools[1]["function"]["parameters"]["required"], json!(["input"]));
        assert!(ctx.custom_tools.contains("apply_patch"));
        assert_eq!(c["stream"], true);
        assert_eq!(c["parallel_tool_calls"], false);
        for k in ["store", "include", "reasoning", "prompt_cache_key", "input", "instructions"] {
            assert!(c.get(k).is_none(), "{k} must not be forwarded");
        }
    }

    #[test]
    fn string_input_and_max_tokens() {
        let (c, _) = request_to_chat(&json!({"model": "m", "input": "hi", "max_output_tokens": 16})).unwrap();
        assert_eq!(c["messages"], json!([{"role": "user", "content": "hi"}]));
        assert_eq!(c["max_tokens"], 16);
        assert!(request_to_chat(&json!({"model": "m", "instructions": "x"})).is_err());
    }

    #[test]
    fn json_answer_becomes_response() {
        let (_, ctx) = request_to_chat(&codex_request()).unwrap();
        let chat = json!({"choices": [{"message": {"role": "assistant", "content": "ok", "reasoning_content": "think",
            "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "apply_patch", "arguments": "{\"input\":\"P\"}"}},
                           {"id": "c2", "type": "function", "function": {"name": "shell", "arguments": "{}"}}]},
            "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}});
        let r = chat_to_response(&chat, &ctx);
        assert_eq!(r["object"], "response");
        assert_eq!(r["status"], "completed");
        let o = r["output"].as_array().unwrap();
        assert_eq!(o[0]["type"], "reasoning");
        assert_eq!(o[1]["content"][0]["text"], "ok");
        assert_eq!(
            o[2],
            json!({"type": "custom_tool_call", "id": o[2]["id"], "status": "completed", "call_id": "c1", "name": "apply_patch", "input": "P"})
        );
        assert_eq!(o[3]["type"], "function_call");
        assert_eq!(r["usage"]["input_tokens"], 3);
        assert_eq!(r["usage"]["total_tokens"], 5);
    }

    fn types(evs: &[sse::Event]) -> Vec<String> {
        evs.iter().map(|e| e.event.clone().unwrap()).collect()
    }

    #[test]
    fn stream_text_and_tool_call() {
        let (_, ctx) = request_to_chat(&codex_request()).unwrap();
        let mut s = StreamConverter::new(ctx);
        let mut evs = Vec::new();
        for d in [
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "reasoning_content": "hm"}}]}),
            json!({"choices": [{"index": 0, "delta": {"content": "Hel"}}]}),
            json!({"choices": [{"index": 0, "delta": {"content": "lo"}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "shell", "arguments": "{\"comm"}}]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "and\":[]}"}}]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 7, "completion_tokens": 3}}),
        ] {
            evs.extend(s.feed(&sse::Event::json(&d)));
        }
        evs.extend(s.feed(&sse::Event::data("[DONE]")));
        assert!(s.finished);
        let t = types(&evs);
        assert_eq!(t[0], "response.created");
        assert_eq!(t.last().unwrap(), "response.completed");
        assert_eq!(t.iter().filter(|x| *x == "response.output_item.done").count(), 3);
        let last: Value = serde_json::from_str(&evs.last().unwrap().data).unwrap();
        let out = last["response"]["output"].as_array().unwrap();
        assert_eq!(out[0]["summary"][0]["text"], "hm");
        assert_eq!(out[1]["content"][0]["text"], "Hello");
        assert_eq!(out[2]["arguments"], "{\"command\":[]}");
        assert_eq!(out[2]["call_id"], "c1");
        assert_eq!(last["response"]["usage"]["total_tokens"], 10);
        // Sequence numbers are increasing and every event names its type.
        for (i, e) in evs.iter().enumerate() {
            let v: Value = serde_json::from_str(&e.data).unwrap();
            assert_eq!(v["sequence_number"], i as u64);
            assert_eq!(v["type"], e.event.clone().unwrap());
        }
        assert!(s.finish().is_empty(), "finishing twice emits nothing");
    }

    #[test]
    fn stream_error_fails_the_response() {
        let mut s = StreamConverter::new(Ctx::new("m"));
        let mut evs = s.feed(&sse::Event::json(&json!({"choices": [{"index": 0, "delta": {"content": "x"}}]})));
        evs.extend(s.feed(&sse::Event::json(
            &json!({"error": {"message": "upstream stream stalled", "type": "server_error"}}),
        )));
        evs.extend(s.feed(&sse::Event::data("[DONE]")));
        let t = types(&evs);
        assert_eq!(t.last().unwrap(), "response.failed");
        let v: Value = serde_json::from_str(&evs.last().unwrap().data).unwrap();
        assert_eq!(v["response"]["error"]["message"], "upstream stream stalled");
    }

    #[test]
    fn stream_ended_without_done_still_completes() {
        let mut s = StreamConverter::new(Ctx::new("m"));
        let _ = s.feed(&sse::Event::json(
            &json!({"choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": "length"}]}),
        ));
        let evs = s.finish();
        let v: Value = serde_json::from_str(&evs.last().unwrap().data).unwrap();
        assert_eq!(v["type"], "response.completed");
        assert_eq!(v["response"]["status"], "incomplete");
    }
}
