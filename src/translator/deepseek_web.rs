//! chat.deepseek.com's web API in terms of OpenAI chat completions: the request (one flat
//! `prompt`, no `messages` and no native tools, so history and tool definitions go into
//! the text), and the reply (JSON-patch style SSE with thinking and answer fragments),
//! turned into chat completion chunks. Tool calls are emulated as 9router does: the
//! prompt asks for `<tool>{"name":..,"arguments":{..}}</tool>` blocks, which are parsed
//! back out of the answer text.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::sse::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}

impl Turn {
    fn labeled(&self) -> String {
        match self.role {
            Role::User => format!("User: {}", self.text),
            Role::Assistant => format!("Assistant: {}", self.text),
            Role::Tool => self.text.clone(),
        }
    }
}

/// What a chat completion request asks of the web endpoint.
#[derive(Debug, Clone)]
pub struct Request {
    /// System prompts and the tool contract; only the first message of a chat carries them.
    pub preamble: String,
    pub turns: Vec<Turn>,
    pub tools: Vec<String>,
    /// `expert` (Pro) or `default` (Flash) as the web client's model switch.
    pub model_type: &'static str,
    pub thinking: bool,
    pub search: bool,
}

const CONTINUE_HINT: &str = "Continue the task using the tool results above. Do NOT repeat tool calls that already \
     succeeded; perform the next step or give the final answer.";

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.trim().to_string(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

/// `deepseek-v4-pro-think-search` and friends: Pro / Flash, thinking and search come from
/// the model id (as in the web client's toggles), thinking also from a reasoning request.
fn options(model: &str, body: &Value) -> (&'static str, bool, bool) {
    let m = model.to_ascii_lowercase();
    let model_type = if m.contains("pro") || m.contains("expert") { "expert" } else { "default" };
    let effort = body.get("reasoning_effort").and_then(Value::as_str).is_some_and(|e| !e.is_empty() && e != "none");
    let thinking = m.contains("r1")
        || m.contains("think")
        || m.contains("reason")
        || effort
        || body.pointer("/thinking/type").and_then(Value::as_str).is_some_and(|t| t != "disabled");
    (model_type, thinking, m.contains("search"))
}

fn tools_prompt(tools: &[Value]) -> String {
    let lines: Vec<String> = tools
        .iter()
        .filter_map(|t| {
            let f = t.get("function").unwrap_or(t);
            let name = f.get("name").and_then(Value::as_str)?;
            let mut line = format!("- {name}");
            if let Some(d) = f.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                line.push_str(&format!(": {d}"));
            }
            if let Some(p) = f.get("parameters").filter(|p| p.is_object()) {
                line.push_str(&format!("\n  parameters: {p}"));
            }
            Some(line)
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    [
        "You can call tools. To call a tool, output ONLY this exact block (no markdown fence):",
        r#"<tool>{"name": "<tool_name>", "arguments": { ... }}</tool>"#,
        "Rules:",
        "- Use exactly <tool>...</tool>. Do NOT use <tool:name>, <tool_call>, <parameter>, attributes or code fences.",
        r#"- "name" must be one of the tools below; "arguments" must be a JSON object."#,
        "- When a tool is needed, emit the <tool> block instead of only describing the plan.",
        "- Emit one <tool> block per call; you may put several blocks back to back.",
        "- If no tool is needed, just answer normally without any <tool> block.",
        "",
        "Available tools:",
        &lines.join("\n"),
    ]
    .join("\n")
}

/// An assistant turn as the transcript shows it: its text, then one block per tool call.
/// The reply side renders with the same function, so a client echoing the reply back
/// reproduces the turn exactly (that is what finds the chat again).
pub fn assistant_text(content: &str, calls: &[(String, String)]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !content.trim().is_empty() {
        parts.push(content.trim().to_string());
    }
    for (name, args) in calls {
        let args = serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!(args));
        parts.push(format!("<tool>{}</tool>", json!({"name": name, "arguments": args})));
    }
    parts.join("\n")
}

impl Request {
    pub fn from_body(model: &str, body: &Value) -> Request {
        let (model_type, thinking, search) = options(model, body);
        let tools_in = body.get("tools").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        let tools: Vec<String> = tools_in
            .iter()
            .filter_map(|t| t.get("function").unwrap_or(t).get("name").and_then(Value::as_str).map(str::to_string))
            .collect();
        let mut system: Vec<String> = Vec::new();
        let mut turns: Vec<Turn> = Vec::new();
        let mut call_names: Vec<(String, String)> = Vec::new();
        for m in body.get("messages").and_then(Value::as_array).into_iter().flatten() {
            let text = text_of(&m["content"]);
            match m.get("role").and_then(Value::as_str).unwrap_or("") {
                "system" | "developer" if !text.is_empty() => system.push(text),
                "user" if !text.is_empty() => turns.push(Turn { role: Role::User, text }),
                "assistant" => {
                    let mut calls = Vec::new();
                    for c in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                        let name = c.pointer("/function/name").and_then(Value::as_str).unwrap_or("").to_string();
                        let args = c.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
                        if let Some(id) = c.get("id").and_then(Value::as_str) {
                            call_names.push((id.to_string(), name.clone()));
                        }
                        calls.push((name, args.to_string()));
                    }
                    let text = assistant_text(&text, &calls);
                    if !text.is_empty() {
                        turns.push(Turn { role: Role::Assistant, text });
                    }
                }
                "tool" => {
                    let id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                    let name = call_names.iter().find(|(i, _)| i == id).map_or("tool", |(_, n)| n.as_str());
                    let out = if text.is_empty() { "(no output)" } else { &text };
                    turns.push(Turn { role: Role::Tool, text: format!("Tool result ({name}): {out}") });
                }
                _ => {}
            }
        }
        if !tools.is_empty() {
            system.push(tools_prompt(tools_in));
        }
        Request { preamble: system.join("\n\n"), turns, tools, model_type, thinking, search }
    }

    /// Where an earlier chat of this conversation would have ended: just after the last
    /// assistant turn, provided something new follows it.
    pub fn resume_point(&self) -> Option<usize> {
        let k = self.turns.iter().rposition(|t| t.role == Role::Assistant)? + 1;
        (k < self.turns.len()).then_some(k)
    }

    /// Identity of the conversation up to `upto` turns, within one account (`scope`).
    pub fn key(&self, scope: &str, upto: usize) -> String {
        let mut h = Sha256::new();
        h.update(scope.as_bytes());
        h.update(self.preamble.as_bytes());
        for t in &self.turns[..upto] {
            h.update([0u8, t.role as u8]);
            h.update(t.text.as_bytes());
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The `prompt` for the turns from `from` on (the preamble rides along with the first).
    pub fn prompt(&self, from: usize) -> String {
        let turns = &self.turns[from..];
        let mut body = match turns {
            [Turn { role: Role::User, text }] => text.clone(),
            _ => turns.iter().map(Turn::labeled).collect::<Vec<_>>().join("\n\n"),
        };
        if turns.last().is_some_and(|t| t.role == Role::Tool) {
            body.push_str("\n\n");
            body.push_str(CONTINUE_HINT);
        }
        match (from, self.preamble.is_empty()) {
            (0, false) if body.is_empty() => self.preamble.clone(),
            (0, false) => format!("{}\n\n{body}", self.preamble),
            _ => body,
        }
    }
}

// ----- tool calls out of text ----------------------------------------------------------

/// Match a name the model wrote against the declared tools, ignoring case and `_`/`-`.
fn declared_name(name: &str, tools: &[String]) -> Option<String> {
    let norm = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    tools.iter().find(|t| t.as_str() == name).or_else(|| tools.iter().find(|t| norm(t) == norm(name))).cloned()
}

/// Name from an opening tag's head: `tool:todo_write` or `tool name="todo_write"`.
fn head_name(head: &str) -> Option<String> {
    let rest = head.strip_prefix("tool_call").or_else(|| head.strip_prefix("tool"))?;
    if let Some(n) = rest.strip_prefix(':') {
        return Some(n.split_whitespace().next()?.to_string());
    }
    let at = rest.find("name=\"")? + 6;
    let len = rest[at..].find('"')?;
    Some(rest[at..at + len].to_string())
}

fn unfence(body: &str) -> &str {
    let b = body.trim();
    match b.strip_prefix("```") {
        Some(inner) => {
            let inner = inner.split_once('\n').map_or(inner, |(_, rest)| rest);
            inner.strip_suffix("```").unwrap_or(inner).trim()
        }
        None => b,
    }
}

fn call_from(body: &str, tag_name: Option<String>, tools: &[String]) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(unfence(body)).ok()?;
    let obj = v.as_object()?;
    let (name, args) = match obj.get("name").and_then(Value::as_str) {
        Some(n) => {
            let args = ["arguments", "parameters", "params", "input"].iter().find_map(|k| obj.get(*k)).cloned();
            (n.to_string(), args.unwrap_or_else(|| json!({})))
        }
        None => (tag_name?, v.clone()),
    };
    let name = if tools.is_empty() { name } else { declared_name(&name, tools)? };
    let args = match args {
        Value::String(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
        Value::Null => json!({}),
        other => other,
    };
    Some((name, args.to_string()))
}

/// Pull `<tool>` blocks out of an answer. Returns the text around them and the calls as
/// (name, arguments JSON); with no call found the text comes back unchanged. Besides the
/// requested shape it accepts what DeepSeek tends to write instead: `<tool_call>`,
/// `<tool:name>{args}`, `<tool name="x">{args}`, code fences, nested wrappers.
pub fn parse_tool_calls(text: &str, tools: &[String]) -> (String, Vec<(String, String)>) {
    let mut calls = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("<tool") {
        let after = &rest[i + 1..];
        let Some(gt) = after.find('>') else { break };
        let head = after[..gt].trim_end_matches('/');
        let known = ["tool>", "tool_call>", "tool:", "tool ", "tool_call "].iter().any(|p| after.starts_with(p));
        if !known {
            plain.push_str(&rest[..i + 5]);
            rest = &rest[i + 5..];
            continue;
        }
        let inner = &after[gt + 1..];
        let end = ["</tool_call>", "</tool>"].iter().filter_map(|c| inner.find(c).map(|p| (p, c.len()))).min();
        let (body, next) = match end {
            Some((p, len)) => (&inner[..p], &inner[p + len..]),
            None => (inner, ""),
        };
        plain.push_str(&rest[..i]);
        let found = if body.trim_start().starts_with("<tool") {
            parse_tool_calls(body, tools).1
        } else {
            call_from(body, head_name(head), tools).into_iter().collect()
        };
        if found.is_empty() {
            plain.push_str(&rest[i..rest.len() - next.len()]);
        }
        calls.extend(found);
        rest = next;
    }
    plain.push_str(rest);
    if calls.is_empty() {
        return (text.to_string(), calls);
    }
    let cleaned = plain.replace("</tool_call>", "").replace("</tool>", "");
    (cleaned.trim().to_string(), calls)
}

// ----- the reply -----------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Think,
    Answer,
    Other,
}

const TOOL_OPEN: &str = "<tool";

fn replace_citations(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("[citation:") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 10..];
        let n = after.bytes().take_while(u8::is_ascii_digit).count();
        if n > 0 && after[n..].starts_with(']') {
            out.push('[');
            out.push_str(&after[..n]);
            out.push(']');
            rest = &after[n + 1..];
        } else {
            out.push_str("[citation:");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Search answers cite `[citation:3]`; the web client shows `[3]`. A marker may arrive in
/// pieces, so a possible start is held back until it is complete.
fn citations(pending: &mut String, new: &str) -> String {
    pending.push_str(new);
    let keep = match pending.rfind('[') {
        Some(i) if !pending[i..].contains(']') && pending.len() - i < 14 => i,
        _ => pending.len(),
    };
    let ready: String = pending.drain(..keep).collect();
    replace_citations(&ready)
}

/// DeepSeek web events -> chat completion chunks.
pub struct FromDeepseek {
    id: String,
    model: String,
    tools: Vec<String>,
    cite: bool,
    sent_role: bool,
    kind: Kind,
    /// Whether a bare `{"v": "text"}` continues a content field.
    in_content: bool,
    cite_hold: String,
    tool_hold: String,
    in_tool: bool,
    tool_buf: String,
    /// The answer text sent so far (without tool blocks).
    answer: String,
    done: bool,
    /// Upstream said the message is complete.
    pub finished: bool,
    /// The id the next message of this chat must name as its parent.
    pub response_id: Option<i64>,
    /// Tool calls found in the answer, as (name, arguments JSON).
    pub calls: Vec<(String, String)>,
}

impl FromDeepseek {
    pub fn new(model: &str, req: &Request) -> FromDeepseek {
        FromDeepseek {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            tools: req.tools.clone(),
            cite: req.search,
            sent_role: false,
            kind: Kind::Answer,
            in_content: true,
            cite_hold: String::new(),
            tool_hold: String::new(),
            in_tool: false,
            tool_buf: String::new(),
            answer: String::new(),
            done: false,
            finished: false,
            response_id: None,
            calls: Vec::new(),
        }
    }

    /// How the assistant turn reads in a later request's transcript.
    pub fn transcript_text(&self) -> String {
        assistant_text(&self.answer, &self.calls)
    }

    fn chunk(&mut self, mut delta: Value, finish: Option<&str>) -> Event {
        if !self.sent_role {
            self.sent_role = true;
            delta["role"] = json!("assistant");
        }
        Event::json(&json!({
            "id": self.id, "object": "chat.completion.chunk", "created": time::OffsetDateTime::now_utc().unix_timestamp(),
            "model": self.model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }))
    }

    fn say(&mut self, text: &str, out: &mut Vec<Event>) {
        if !text.is_empty() {
            self.answer.push_str(text);
            out.push(self.chunk(json!({"content": text}), None));
        }
    }

    /// Hold back text that may be the start of a tool block; once one starts, keep the rest.
    fn tool_gate(&mut self, new: &str) -> String {
        if self.in_tool {
            self.tool_buf.push_str(new);
            return String::new();
        }
        self.tool_hold.push_str(new);
        if let Some(i) = self.tool_hold.find(TOOL_OPEN) {
            self.in_tool = true;
            self.tool_buf = self.tool_hold.split_off(i);
            return std::mem::take(&mut self.tool_hold);
        }
        let keep = (1..TOOL_OPEN.len()).rev().find(|&n| self.tool_hold.ends_with(&TOOL_OPEN[..n])).unwrap_or(0);
        let tail = self.tool_hold.split_off(self.tool_hold.len() - keep);
        std::mem::replace(&mut self.tool_hold, tail)
    }

    fn answer_text(&mut self, text: &str, out: &mut Vec<Event>) {
        let text = if self.cite { citations(&mut self.cite_hold, text) } else { text.to_string() };
        let text = if self.tools.is_empty() { text } else { self.tool_gate(&text) };
        self.say(&text, out);
    }

    fn text(&mut self, text: &str, out: &mut Vec<Event>) {
        match self.kind {
            Kind::Think if !text.is_empty() => out.push(self.chunk(json!({"reasoning_content": text}), None)),
            Kind::Answer => self.answer_text(text, out),
            _ => {}
        }
    }

    fn fragment(&mut self, f: &Value, out: &mut Vec<Event>) {
        self.kind = match f.get("type").and_then(Value::as_str).unwrap_or("").to_ascii_uppercase().as_str() {
            "THINK" => Kind::Think,
            "RESPONSE" | "ANSWER" => Kind::Answer,
            _ => Kind::Other,
        };
        self.in_content = true;
        if let Some(c) = f.get("content").and_then(Value::as_str) {
            self.text(c, out);
        }
    }

    fn data(&mut self, v: &Value, out: &mut Vec<Event>) {
        let p = v.get("p").and_then(Value::as_str);
        let val = v.get("v");
        if let Some(resp) = val.and_then(|x| x.get("response")) {
            if let Some(id) = resp.get("message_id").and_then(Value::as_i64) {
                self.response_id = Some(id);
            }
            for f in resp.get("fragments").and_then(Value::as_array).into_iter().flatten() {
                self.fragment(f, out);
            }
        }
        if let Some(p) = p {
            self.in_content = p.ends_with("/content") || p == "response/fragments";
        }
        match (p, val) {
            (Some("response/fragments"), Some(Value::Array(frags))) => {
                for f in frags {
                    self.fragment(f, out);
                }
            }
            (Some("response/status"), Some(Value::String(s))) if s == "FINISHED" => self.finished = true,
            (Some("response"), Some(Value::Array(ops))) => {
                let status = |o: &&Value| matches!(o.get("p").and_then(Value::as_str), Some("status" | "quasi_status"));
                if ops.iter().filter(status).any(|o| o.get("v").and_then(Value::as_str) == Some("FINISHED")) {
                    self.finished = true;
                }
            }
            // The first generation of the web API: separate fields for thinking and answer.
            (Some("response/thinking_content"), Some(Value::String(s))) => {
                self.kind = Kind::Think;
                self.text(s, out);
            }
            (Some("response/content"), Some(Value::String(s))) => {
                self.kind = Kind::Answer;
                self.text(s, out);
            }
            (_, Some(Value::String(s))) if self.in_content => self.text(s, out),
            _ => {}
        }
    }

    pub fn feed(&mut self, ev: &Event) -> Vec<Event> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return out };
        match ev.event.as_deref() {
            Some("ready") => self.response_id = v.get("response_message_id").and_then(Value::as_i64),
            Some("finish" | "close") => self.finished = true,
            Some("hint") => {
                if v.get("type").and_then(Value::as_str) == Some("error") {
                    let msg = v.get("content").and_then(Value::as_str).unwrap_or("request refused");
                    self.say(&format!("\n\n[DeepSeek: {msg}]"), &mut out);
                    self.finished = true;
                }
            }
            Some(_) => {}
            None => self.data(&v, &mut out),
        }
        out
    }

    /// The closing chunks: held-back text, tool calls and the finish reason.
    pub fn finish(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        self.done = true;
        let tail = replace_citations(&std::mem::take(&mut self.cite_hold));
        let tail = if self.tools.is_empty() { tail } else { self.tool_gate(&tail) };
        self.say(&tail, &mut out);
        let held = std::mem::take(&mut self.tool_hold);
        self.say(&held, &mut out);
        if self.in_tool {
            let buf = std::mem::take(&mut self.tool_buf);
            let (text, calls) = parse_tool_calls(&buf, &self.tools);
            self.say(&text, &mut out);
            self.calls = calls;
        }
        for (i, (name, args)) in self.calls.clone().into_iter().enumerate() {
            let id = format!("call_{}", uuid::Uuid::new_v4().simple());
            let call = json!({"index": i, "id": id, "type": "function", "function": {"name": name, "arguments": args}});
            out.push(self.chunk(json!({"tool_calls": [call]}), None));
        }
        let reason = if self.calls.is_empty() { "stop" } else { "tool_calls" };
        out.push(self.chunk(json!({}), Some(reason)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn model_ids_select_type_thinking_and_search() {
        let b = json!({});
        assert_eq!(options("deepseek-v4-pro-think-search", &b), ("expert", true, true));
        assert_eq!(options("deepseek-v4-flash", &b), ("default", false, false));
        assert_eq!(options("deepseek-reasoner", &b), ("default", true, false));
        assert_eq!(options("DeepSeek-R1-Search", &b), ("default", true, true));
        assert_eq!(options("deepseek-chat", &json!({"reasoning_effort": "high"})), ("default", true, false));
        assert_eq!(options("deepseek-chat", &json!({"reasoning_effort": "none"})), ("default", false, false));
    }

    fn body() -> Value {
        json!({
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [{"type": "text", "text": "list files"}, {"type": "image_url", "image_url": {"url": "data:x"}}]},
                {"role": "assistant", "content": "Looking.", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "ls", "arguments": "{\"path\":\".\"}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "a.txt"}
            ],
            "tools": [{"type": "function", "function": {"name": "ls", "description": "List", "parameters": {"type": "object"}}}]
        })
    }

    #[test]
    fn single_user_message_is_sent_as_is() {
        let r = Request::from_body("deepseek-chat", &json!({"messages": [{"role": "user", "content": "hi"}]}));
        assert_eq!(r.prompt(0), "hi");
        assert_eq!(r.resume_point(), None);
        let r = Request::from_body(
            "deepseek-chat",
            &json!({"messages": [{"role": "system", "content": "Be brief."}, {"role": "user", "content": "hi"}]}),
        );
        assert_eq!(r.prompt(0), "Be brief.\n\nhi");
    }

    #[test]
    fn transcript_carries_tools_history_and_results() {
        let r = Request::from_body("deepseek-v4-pro", &body());
        assert_eq!((r.model_type, r.tools.clone()), ("expert", names(&["ls"])));
        assert_eq!(r.turns.len(), 3);
        let p = r.prompt(0);
        assert!(p.starts_with("Be brief.\n\nYou can call tools."));
        assert!(p.contains("- ls: List\n  parameters: {\"type\":\"object\"}"));
        assert!(p.contains("User: list files\n\nAssistant: Looking.\n<tool>{\"name\":\"ls\",\"arguments\":{\"path\":\".\"}}</tool>"));
        assert!(p.contains("Tool result (ls): a.txt\n\nContinue the task"));
        assert!(!p.contains("data:x"));
    }

    #[test]
    fn follow_up_resumes_the_chat_that_ended_with_the_echoed_reply() {
        let first = Request::from_body("m", &json!({"messages": [{"role": "user", "content": "hi"}]}));
        let mut tr = FromDeepseek::new("m", &first);
        tr.answer = "Hello there".into();
        let reply = tr.transcript_text();
        let second = Request::from_body(
            "m",
            &json!({"messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "Hello there"},
                {"role": "user", "content": "and now?"}
            ]}),
        );
        let k = second.resume_point().unwrap();
        assert_eq!(k, 2);
        assert_eq!(second.prompt(k), "and now?");
        // The chat stored after the first reply is found under the key of the echoed history.
        let mut stored = first.clone();
        stored.turns.push(Turn { role: Role::Assistant, text: reply });
        assert_eq!(stored.key("acct", 2), second.key("acct", k));
        assert_ne!(stored.key("other", 2), second.key("acct", k));
    }

    #[test]
    fn parses_the_requested_tool_block() {
        let t = names(&["ls", "read_file"]);
        let (text, calls) = parse_tool_calls(
            "Sure.\n<tool>{\"name\": \"ls\", \"arguments\": {\"path\": \".\"}}</tool>\n<tool>{\"name\":\"read_file\",\"arguments\":\"{\\\"p\\\":1}\"}</tool>",
            &t,
        );
        assert_eq!(text, "Sure.");
        assert_eq!(calls, vec![("ls".into(), "{\"path\":\".\"}".into()), ("read_file".into(), "{\"p\":1}".into())]);
    }

    #[test]
    fn parses_the_shapes_deepseek_improvises() {
        let t = names(&["read_file"]);
        let c = |s: &str| parse_tool_calls(s, &t).1;
        let want = vec![("read_file".to_string(), "{\"p\":\"a\"}".to_string())];
        assert_eq!(c("<tool:readfile>{\"p\":\"a\"}</tool>"), want);
        assert_eq!(c("<tool name=\"read_file\">{\"p\":\"a\"}</tool>"), want);
        assert_eq!(c("<tool_call>{\"name\":\"READ_FILE\",\"params\":{\"p\":\"a\"}}</tool_call>"), want);
        assert_eq!(c("<tool>\n```json\n{\"name\":\"read_file\",\"arguments\":{\"p\":\"a\"}}\n```\n</tool>"), want);
        assert_eq!(c("<tool><tool>{\"name\":\"read_file\",\"arguments\":{\"p\":\"a\"}}</tool></tool>"), want);
        assert_eq!(c("<tool>{\"name\":\"read_file\",\"arguments\":{\"p\":\"a\"}}"), want);
    }

    #[test]
    fn ignores_what_is_not_a_declared_call() {
        let t = names(&["ls"]);
        let text = "Use the <tool> tag like <tool>{\"name\":\"rm\",\"arguments\":{}}</tool> and <tools> too";
        let (out, calls) = parse_tool_calls(text, &t);
        assert!(calls.is_empty());
        assert_eq!(out, text);
    }

    fn ev(v: Value) -> Event {
        Event::json(&v)
    }

    fn run(req: &Request, events: Vec<Event>) -> (Vec<Value>, FromDeepseek) {
        let mut tr = FromDeepseek::new("deepseek-v4-pro-think", req);
        let mut out: Vec<Event> = events.iter().flat_map(|e| tr.feed(e)).collect();
        out.extend(tr.finish());
        (out.iter().map(|e| serde_json::from_str(&e.data).unwrap()).collect(), tr)
    }

    fn deltas(chunks: &[Value], key: &str) -> String {
        chunks.iter().filter_map(|c| c.pointer(&format!("/choices/0/delta/{key}")).and_then(Value::as_str)).collect()
    }

    #[test]
    fn thinking_and_answer_fragments_become_chunks() {
        let req = Request::from_body("deepseek-v4-pro-think", &json!({"messages": [{"role": "user", "content": "hi"}]}));
        let (chunks, tr) = run(
            &req,
            vec![
                Event::named("ready", r#"{"request_message_id":1,"response_message_id":2}"#),
                ev(json!({"v": {"response": {"message_id": 2, "fragments": [{"id": 1, "type": "THINK", "content": "Hm"}]}}})),
                ev(json!({"p": "response/fragments/-1/content", "o": "APPEND", "v": "m."})),
                ev(json!({"v": " ok"})),
                ev(json!({"p": "response/fragments", "o": "APPEND", "v": [{"id": 2, "type": "RESPONSE", "content": "Hel"}]})),
                ev(json!({"v": "lo"})),
                ev(json!({"p": "response", "o": "BATCH", "v": [{"p": "accumulated_token_usage", "v": 9}, {"p": "quasi_status", "v": "FINISHED"}]})),
                ev(json!({"p": "response/status", "o": "SET", "v": "FINISHED"})),
                Event::named("close", "{}"),
            ],
        );
        assert_eq!(deltas(&chunks, "reasoning_content"), "Hmm. ok");
        assert_eq!(deltas(&chunks, "content"), "Hello");
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks.last().unwrap()["choices"][0]["finish_reason"], "stop");
        assert!(tr.finished);
        assert_eq!(tr.response_id, Some(2));
        assert_eq!(tr.transcript_text(), "Hello");
    }

    #[test]
    fn search_fragments_are_skipped_and_citations_rewritten() {
        let req = Request::from_body("deepseek-v4-flash-search", &json!({"messages": [{"role": "user", "content": "q"}]}));
        let (chunks, _) = run(
            &req,
            vec![
                ev(json!({"v": {"response": {"fragments": [{"type": "SEARCH", "content": "", "results": []}]}}})),
                ev(json!({"p": "response/fragments", "o": "APPEND", "v": [{"type": "RESPONSE", "content": "It is [cita"}]})),
                ev(json!({"v": "tion:"})),
                ev(json!({"v": "2] true [x"})),
            ],
        );
        assert_eq!(deltas(&chunks, "content"), "It is [2] true [x");
    }

    #[test]
    fn tool_block_in_the_answer_becomes_a_tool_call() {
        let req = Request::from_body("deepseek-chat", &body());
        let (chunks, tr) = run(
            &req,
            vec![
                ev(json!({"p": "response/fragments", "o": "APPEND", "v": [{"type": "RESPONSE", "content": "Checking <to"}]})),
                ev(json!({"v": "ol>{\"name\":\"ls\","})),
                ev(json!({"v": "\"arguments\":{\"path\":\"/\"}}</tool>"})),
                ev(json!({"p": "response/status", "v": "FINISHED"})),
            ],
        );
        assert_eq!(deltas(&chunks, "content"), "Checking ");
        let call = &chunks[chunks.len() - 2]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["function"]["name"], "ls");
        assert_eq!(call["function"]["arguments"], "{\"path\":\"/\"}");
        assert_eq!(chunks.last().unwrap()["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(tr.transcript_text(), "Checking\n<tool>{\"name\":\"ls\",\"arguments\":{\"path\":\"/\"}}</tool>");
    }

    #[test]
    fn a_tool_tag_that_is_no_call_is_shown_as_text() {
        let req = Request::from_body("deepseek-chat", &body());
        let (chunks, _) = run(
            &req,
            vec![ev(json!({"p": "response/fragments", "o": "APPEND", "v": [{"type": "RESPONSE", "content": "Write <tool> to call."}]}))],
        );
        assert_eq!(deltas(&chunks, "content"), "Write <tool> to call.");
        assert_eq!(chunks.last().unwrap()["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn upstream_refusal_is_reported_in_the_answer() {
        let req = Request::from_body("deepseek-chat", &json!({"messages": [{"role": "user", "content": "q"}]}));
        let (chunks, tr) =
            run(&req, vec![Event::named("hint", r#"{"type":"error","content":"Messages too frequent","finish_reason":"rate_limit_reached"}"#)]);
        assert!(deltas(&chunks, "content").contains("Messages too frequent"));
        assert!(tr.finished);
    }
}
