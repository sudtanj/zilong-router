//! Streaming translation state machines. Each upstream SSE event is fed in and zero
//! or more client events come out. Usage is accumulated along the way so the
//! handler can record it when the stream ends.

use serde_json::{json, Value};

use super::response::{claude_usage, openai_usage};
use super::{finish_to_stop, safe_parse_json, stop_to_finish, Format};
use crate::sse::Event;
use crate::usage::TokenCounts;

pub struct StreamTranslator {
    from: Format,
    to: Format,
    model: String,
    pub usage: TokenCounts,
    pub output_chars: usize,
    pub finished: bool,
    inner: Inner,
}

enum Inner {
    Passthrough,
    ClaudeToOpenai(C2O),
    OpenaiToClaude(O2C),
}

#[derive(Default)]
struct C2O {
    message_id: String,
    sent_role: bool,
    /// Claude block index -> (openai tool index, tool id)
    tools: Vec<(usize, usize, String)>,
    tool_count: usize,
    sent_finish: bool,
    saw_tool: bool,
}

#[derive(Default)]
struct O2C {
    message_id: String,
    started: bool,
    next_block: usize,
    text_block: Option<usize>,
    thinking_block: Option<usize>,
    /// openai tool index -> (claude block index, buffered arguments)
    tools: Vec<(usize, usize, String)>,
    finish_reason: Option<String>,
    sent_stop: bool,
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl StreamTranslator {
    pub fn new(from: Format, to: Format, model: &str) -> StreamTranslator {
        let inner = match (from, to) {
            (Format::Claude, Format::Openai) => Inner::ClaudeToOpenai(C2O::default()),
            (Format::Openai, Format::Claude) => Inner::OpenaiToClaude(O2C::default()),
            _ => Inner::Passthrough,
        };
        StreamTranslator {
            from,
            to,
            model: model.to_string(),
            usage: TokenCounts::default(),
            output_chars: 0,
            finished: false,
            inner,
        }
    }

    pub fn feed(&mut self, ev: &Event) -> Vec<Event> {
        if ev.data.trim() == "[DONE]" {
            return self.finish();
        }
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return vec![];
        };
        if let Some(u) = crate::usage::extract(&v) {
            self.usage.merge(u);
        }
        match &mut self.inner {
            Inner::Passthrough => {
                if let Some(t) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                    self.output_chars += t.len();
                }
                if let Some(t) = v.pointer("/delta/text").and_then(Value::as_str) {
                    self.output_chars += t.len();
                }
                if self.from == Format::Claude && v.get("type").and_then(Value::as_str) == Some("message_stop") {
                    self.finished = true;
                }
                vec![ev.clone()]
            }
            Inner::ClaudeToOpenai(st) => {
                let model = self.model.clone();
                let (out, done) = c2o(st, &v, &model, &self.usage);
                self.output_chars += out.iter().map(|e| e.data.len() / 2).sum::<usize>();
                if done {
                    self.finished = true;
                }
                out
            }
            Inner::OpenaiToClaude(st) => {
                let model = self.model.clone();
                let out = o2c(st, &v, &model, &self.usage);
                self.output_chars += out.iter().map(|e| e.data.len() / 2).sum::<usize>();
                out
            }
        }
    }

    /// Called when the upstream body ends. Emits whatever is needed to close the
    /// client stream cleanly (e.g. OpenAI->Claude flushes tool args and message_stop).
    pub fn finish(&mut self) -> Vec<Event> {
        if self.finished {
            return vec![];
        }
        self.finished = true;
        match &mut self.inner {
            Inner::Passthrough => match self.to {
                Format::Openai => vec![Event::data("[DONE]")],
                _ => vec![],
            },
            Inner::ClaudeToOpenai(st) => {
                let mut out = Vec::new();
                if !st.sent_finish {
                    st.sent_finish = true;
                    let reason = if st.saw_tool { "tool_calls" } else { "stop" };
                    out.push(chunk(
                        &st.message_id,
                        &self.model,
                        json!({}),
                        Some(reason),
                        Some(openai_usage(&self.usage)),
                    ));
                }
                out.push(Event::data("[DONE]"));
                out
            }
            Inner::OpenaiToClaude(st) => {
                if st.sent_stop {
                    return vec![];
                }
                let reason = st.finish_reason.clone().unwrap_or_else(|| "stop".to_string());
                close_claude(st, &self.model, &self.usage, &reason)
            }
        }
    }
}

fn chunk(id: &str, model: &str, delta: Value, finish: Option<&str>, usage: Option<Value>) -> Event {
    let mut v = json!({
        "id": format!("chatcmpl-{id}"), "object": "chat.completion.chunk", "created": now(), "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    if let Some(u) = usage {
        v["usage"] = u;
    }
    Event::json(&v)
}

/// Claude SSE -> OpenAI chunks. Returns (events, stream_finished).
fn c2o(st: &mut C2O, v: &Value, model: &str, usage: &TokenCounts) -> (Vec<Event>, bool) {
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    let mut out = Vec::new();
    match ty {
        "message_start" => {
            st.message_id = v.pointer("/message/id").and_then(Value::as_str).unwrap_or("msg").to_string();
            st.sent_role = true;
            out.push(chunk(&st.message_id, model, json!({"role": "assistant", "content": ""}), None, None));
        }
        "content_block_start" => {
            let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let block = v.get("content_block").cloned().unwrap_or(json!({}));
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                st.saw_tool = true;
                let tool_index = st.tool_count;
                st.tool_count += 1;
                let id = block.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                st.tools.push((idx, tool_index, id.clone()));
                out.push(chunk(
                    &st.message_id,
                    model,
                    json!({"tool_calls": [{"index": tool_index, "id": id, "type": "function",
                        "function": {"name": block.get("name").cloned().unwrap_or(json!("")), "arguments": ""}}]}),
                    None,
                    None,
                ));
            }
        }
        "content_block_delta" => {
            let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let d = v.get("delta").cloned().unwrap_or(json!({}));
            match d.get("type").and_then(Value::as_str) {
                Some("text_delta") => out.push(chunk(&st.message_id, model, json!({"content": d["text"]}), None, None)),
                Some("thinking_delta") => {
                    out.push(chunk(&st.message_id, model, json!({"reasoning_content": d["thinking"]}), None, None))
                }
                Some("input_json_delta") => {
                    if let Some((_, ti, id)) = st.tools.iter().find(|t| t.0 == idx) {
                        out.push(chunk(
                            &st.message_id,
                            model,
                            json!({"tool_calls": [{"index": ti, "id": id, "function": {"arguments": d["partial_json"]}}]}),
                            None,
                            None,
                        ));
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            if let Some(stop) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                if stop == "refusal" {
                    if let Some(t) = v.pointer("/delta/stop_details/explanation").and_then(Value::as_str) {
                        out.push(chunk(&st.message_id, model, json!({"content": t}), None, None));
                    }
                }
                st.sent_finish = true;
                out.push(chunk(
                    &st.message_id,
                    model,
                    json!({}),
                    Some(stop_to_finish(stop)),
                    Some(openai_usage(usage)),
                ));
            }
        }
        "message_stop" => {
            if !st.sent_finish {
                st.sent_finish = true;
                let reason = if st.saw_tool { "tool_calls" } else { "stop" };
                out.push(chunk(&st.message_id, model, json!({}), Some(reason), Some(openai_usage(usage))));
            }
            out.push(Event::data("[DONE]"));
            return (out, true);
        }
        "error" => {
            let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("upstream error");
            out.push(Event::json(&json!({"error": {"message": msg, "type": "server_error"}})));
            out.push(Event::data("[DONE]"));
            return (out, true);
        }
        _ => {} // ping etc.
    }
    (out, false)
}

fn claude_ev(ty: &str, body: Value) -> Event {
    let mut v = body;
    v["type"] = json!(ty);
    Event::named_json(ty, &v)
}

fn close_block(which: &mut Option<usize>, out: &mut Vec<Event>) {
    if let Some(i) = which.take() {
        out.push(claude_ev("content_block_stop", json!({"index": i})));
    }
}

/// OpenAI chunks -> Claude SSE.
fn o2c(st: &mut O2C, v: &Value, model: &str, usage: &TokenCounts) -> Vec<Event> {
    let mut out = Vec::new();
    if let Some(err) = v.get("error") {
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("upstream error");
        out.push(claude_ev("error", json!({"error": {"type": "api_error", "message": msg}})));
        st.sent_stop = true;
        return out;
    }
    if !st.started {
        st.started = true;
        let raw = v.get("id").and_then(Value::as_str).unwrap_or("").trim_start_matches("chatcmpl-").to_string();
        st.message_id =
            if raw.len() < 8 { format!("msg_{}", uuid::Uuid::new_v4().simple()) } else { format!("msg_{raw}") };
        out.push(claude_ev(
            "message_start",
            json!({"message": {"id": st.message_id, "type": "message", "role": "assistant", "model": model,
                "content": [], "stop_reason": Value::Null, "stop_sequence": Value::Null,
                "usage": {"input_tokens": usage.prompt.max(0), "output_tokens": 0}}}),
        ));
    }
    let Some(choice) = v.pointer("/choices/0") else {
        return out; // usage-only trailing chunk
    };
    let delta = choice.get("delta").cloned().unwrap_or(json!({}));

    if let Some(r) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let mut tb = st.text_block;
        close_block(&mut tb, &mut out);
        st.text_block = tb;
        if st.thinking_block.is_none() {
            let i = st.next_block;
            st.next_block += 1;
            st.thinking_block = Some(i);
            out.push(claude_ev(
                "content_block_start",
                json!({"index": i, "content_block": {"type": "thinking", "thinking": ""}}),
            ));
        }
        out.push(claude_ev(
            "content_block_delta",
            json!({"index": st.thinking_block.unwrap(), "delta": {"type": "thinking_delta", "thinking": r}}),
        ));
    }
    if let Some(t) = delta.get("content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        let mut th = st.thinking_block;
        close_block(&mut th, &mut out);
        st.thinking_block = th;
        if st.text_block.is_none() {
            let i = st.next_block;
            st.next_block += 1;
            st.text_block = Some(i);
            out.push(claude_ev(
                "content_block_start",
                json!({"index": i, "content_block": {"type": "text", "text": ""}}),
            ));
        }
        out.push(claude_ev(
            "content_block_delta",
            json!({"index": st.text_block.unwrap(), "delta": {"type": "text_delta", "text": t}}),
        ));
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for tc in calls {
            let ti = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let pos = st.tools.iter().position(|t| t.0 == ti);
            let args = tc.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("");
            match pos {
                Some(p) => st.tools[p].2.push_str(args),
                None => {
                    let mut th = st.thinking_block;
                    close_block(&mut th, &mut out);
                    st.thinking_block = th;
                    let mut tb = st.text_block;
                    close_block(&mut tb, &mut out);
                    st.text_block = tb;
                    let i = st.next_block;
                    st.next_block += 1;
                    let id = tc
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::new_v4().simple()));
                    out.push(claude_ev(
                        "content_block_start",
                        json!({"index": i, "content_block": {"type": "tool_use", "id": id,
                            "name": tc.pointer("/function/name").cloned().unwrap_or(json!("")), "input": {}}}),
                    ));
                    st.tools.push((ti, i, args.to_string()));
                }
            }
        }
    }
    if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
        st.finish_reason = Some(fr.to_string());
        // Some servers send usage in a later chunk; if this chunk already has it, close now.
        if v.get("usage").map(|u| !u.is_null()).unwrap_or(false) {
            out.extend(close_claude(st, model, usage, fr));
        }
    }
    out
}

fn close_claude(st: &mut O2C, model: &str, usage: &TokenCounts, finish: &str) -> Vec<Event> {
    let mut out = Vec::new();
    if st.sent_stop {
        return out;
    }
    if !st.started {
        st.started = true;
        st.message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        out.push(claude_ev("message_start", json!({"message": {"id": st.message_id, "type": "message", "role": "assistant",
            "model": model, "content": [], "stop_reason": Value::Null, "stop_sequence": Value::Null, "usage": {"input_tokens": 0, "output_tokens": 0}}})));
    }
    let mut th = st.thinking_block;
    close_block(&mut th, &mut out);
    st.thinking_block = th;
    let mut tb = st.text_block;
    close_block(&mut tb, &mut out);
    st.text_block = tb;
    for (_, block, args) in std::mem::take(&mut st.tools) {
        let parsed = match safe_parse_json(&args) {
            Value::Object(o) => Value::Object(o),
            _ => json!({}),
        };
        out.push(claude_ev(
            "content_block_delta",
            json!({"index": block, "delta": {"type": "input_json_delta", "partial_json": parsed.to_string()}}),
        ));
        out.push(claude_ev("content_block_stop", json!({"index": block})));
    }
    let had_tools = finish == "tool_calls";
    let reason = if had_tools { "tool_use" } else { finish_to_stop(finish) };
    out.push(claude_ev(
        "message_delta",
        json!({"delta": {"stop_reason": reason, "stop_sequence": Value::Null}, "usage": claude_usage(usage)}),
    ));
    out.push(claude_ev("message_stop", json!({})));
    st.sent_stop = true;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(t: &mut StreamTranslator, events: &[(&str, Value)]) -> Vec<Event> {
        let mut out = Vec::new();
        for (name, v) in events {
            let ev = if name.is_empty() { Event::json(v) } else { Event::named_json(name, v) };
            out.extend(t.feed(&ev));
        }
        out.extend(t.finish());
        out
    }

    #[test]
    fn claude_stream_to_openai() {
        let mut t = StreamTranslator::new(Format::Claude, Format::Openai, "m");
        let out = feed_all(
            &mut t,
            &[
                (
                    "message_start",
                    json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 5, "output_tokens": 0}}}),
                ),
                (
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                ),
                (
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}),
                ),
                (
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1", "name": "f"}}),
                ),
                (
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"a\":"}}),
                ),
                (
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "1}"}}),
                ),
                (
                    "message_delta",
                    json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
                ),
                ("message_stop", json!({"type": "message_stop"})),
            ],
        );
        let datas: Vec<Value> =
            out.iter().filter(|e| e.data != "[DONE]").map(|e| serde_json::from_str(&e.data).unwrap()).collect();
        assert_eq!(datas[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(datas[1]["choices"][0]["delta"]["content"], "Hi");
        assert_eq!(datas[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(datas[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"a\":");
        let last = datas.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(last["usage"]["prompt_tokens"], 5);
        assert_eq!(last["usage"]["completion_tokens"], 7);
        assert_eq!(out.last().unwrap().data, "[DONE]");
        assert!(t.finished);
    }

    #[test]
    fn openai_stream_to_claude() {
        let mut t = StreamTranslator::new(Format::Openai, Format::Claude, "m");
        let out = feed_all(
            &mut t,
            &[
                (
                    "",
                    json!({"id": "chatcmpl-abcdefgh", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "He"}}]}),
                ),
                ("", json!({"id": "chatcmpl-abcdefgh", "choices": [{"index": 0, "delta": {"content": "llo"}}]})),
                (
                    "",
                    json!({"id": "chatcmpl-abcdefgh", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "f", "arguments": "{\"a\""}}]}}]}),
                ),
                (
                    "",
                    json!({"id": "chatcmpl-abcdefgh", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": ":1}"}}]}}]}),
                ),
                (
                    "",
                    json!({"id": "chatcmpl-abcdefgh", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
                ),
                (
                    "",
                    json!({"id": "chatcmpl-abcdefgh", "choices": [], "usage": {"prompt_tokens": 9, "completion_tokens": 4}}),
                ),
                ("", json!("[DONE]")),
            ],
        );
        let names: Vec<&str> = out.iter().map(|e| e.event.as_deref().unwrap_or("")).collect();
        assert_eq!(names[0], "message_start");
        assert_eq!(names[1], "content_block_start");
        assert_eq!(names[2], "content_block_delta");
        assert!(names.contains(&"message_delta"));
        assert_eq!(*names.last().unwrap(), "message_stop");
        let delta: Value =
            serde_json::from_str(&out.iter().find(|e| e.event.as_deref() == Some("message_delta")).unwrap().data)
                .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
        assert_eq!(delta["usage"]["input_tokens"], 9);
        let args: Value = serde_json::from_str(
            &out.iter().filter(|e| e.event.as_deref() == Some("content_block_delta")).last().unwrap().data,
        )
        .unwrap();
        assert_eq!(args["delta"]["partial_json"], "{\"a\":1}");
        assert_eq!(names.iter().filter(|n| **n == "message_stop").count(), 1);
    }
}
