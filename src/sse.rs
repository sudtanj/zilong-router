//! Minimal Server-Sent Events framing: parse incoming byte chunks into events and
//! serialise outgoing events.

use bytes::{Bytes, BytesMut};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub event: Option<String>,
    pub data: String,
}

impl Event {
    pub fn data(data: impl Into<String>) -> Event {
        Event { event: None, data: data.into() }
    }
    pub fn named(event: &str, data: impl Into<String>) -> Event {
        Event { event: Some(event.to_string()), data: data.into() }
    }
    pub fn json(v: &serde_json::Value) -> Event {
        Event::data(v.to_string())
    }
    pub fn named_json(event: &str, v: &serde_json::Value) -> Event {
        Event::named(event, v.to_string())
    }

    /// Wire form: `event: x\ndata: y\n\n`.
    pub fn to_bytes(&self) -> Bytes {
        let mut s = String::with_capacity(self.data.len() + 32);
        if let Some(e) = &self.event {
            s.push_str("event: ");
            s.push_str(e);
            s.push('\n');
        }
        for line in self.data.split('\n') {
            s.push_str("data: ");
            s.push_str(line);
            s.push('\n');
        }
        s.push('\n');
        Bytes::from(s)
    }
}

/// Incremental parser. Feed raw bytes, drain complete events.
#[derive(Default)]
pub struct Parser {
    buf: BytesMut,
}

impl Parser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            // Find the end of an event: a blank line (\n\n or \r\n\r\n).
            let Some((end, skip)) = find_boundary(&self.buf) else { break };
            let raw = self.buf.split_to(end);
            let _ = self.buf.split_to(skip);
            if let Some(ev) = parse_block(std::str::from_utf8(&raw).unwrap_or("")) {
                out.push(ev);
            }
        }
        out
    }

    /// Flush a trailing event without a terminating blank line (some servers end the
    /// body right after the last `data:` line).
    pub fn finish(&mut self) -> Option<Event> {
        let raw = std::mem::take(&mut self.buf);
        parse_block(std::str::from_utf8(&raw).unwrap_or(""))
    }
}

fn find_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == b'\n' {
            if i + 1 < buf.len() && buf[i + 1] == b'\n' {
                return Some((i, 2));
            }
            if i + 2 < buf.len() && buf[i + 1] == b'\r' && buf[i + 2] == b'\n' {
                return Some((i, 3));
            }
        }
        i += 1;
    }
    None
}

fn parse_block(block: &str) -> Option<Event> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(Event { event, data: data.join("\n") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_split_chunks() {
        let mut p = Parser::default();
        assert!(p.push(b"event: message_start\ndata: {\"a\":").is_empty());
        let evs = p.push(b"1}\n\ndata: [DONE]\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event.as_deref(), Some("message_start"));
        assert_eq!(evs[0].data, "{\"a\":1}");
        assert_eq!(evs[1].data, "[DONE]");
        assert_eq!(Event::data("x").to_bytes(), Bytes::from("data: x\n\n"));
    }

    #[test]
    fn crlf_and_comments() {
        let mut p = Parser::default();
        let evs = p.push(b": keepalive\r\ndata: a\r\ndata: b\r\n\r\n");
        assert_eq!(evs[0].data, "a\nb");
    }
}
