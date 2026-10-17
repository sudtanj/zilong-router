//! The last few hundred log lines, for the dashboard's Console Log page (upstream
//! `consoleLogBuffer.js` keeps the last 200 console lines in memory; same here).
//!
//! The tracing formatter writes every event through [`Tee`], which copies it to stdout
//! as before and into a small ring buffer. The page polls with the sequence number of
//! the last line it has, so no long-lived connection is held open on the board.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::LazyLock;

use parking_lot::Mutex;

/// Same as upstream `CONSOLE_LOG_CONFIG.maxLines`.
pub const MAX_LINES: usize = 200;

struct Buffer {
    lines: VecDeque<(u64, String)>,
    /// Sequence number of the newest line ever pushed; survives a clear.
    last: u64,
}

static BUF: LazyLock<Mutex<Buffer>> =
    LazyLock::new(|| Mutex::new(Buffer { lines: VecDeque::with_capacity(MAX_LINES), last: 0 }));

/// `io::Write` handed to `tracing_subscriber::fmt` (one `write` per formatted event).
pub struct Tee;

impl Write for Tee {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let _ = io::stdout().lock().write_all(b);
        push(&String::from_utf8_lossy(b));
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

fn push(text: &str) {
    let mut buf = BUF.lock();
    for line in strip_ansi(text).lines().filter(|l| !l.trim().is_empty()) {
        buf.last += 1;
        let seq = buf.last;
        buf.lines.push_back((seq, line.to_string()));
        if buf.lines.len() > MAX_LINES {
            buf.lines.pop_front();
        }
    }
}

/// Drop terminal colour codes (`ESC [ ... letter`) so they don't show up in the page.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Lines newer than `after`, the newest sequence number, and whether the caller's
/// cursor is from before a restart (it should then drop what it has).
pub fn since(after: u64) -> (Vec<String>, u64, bool) {
    let buf = BUF.lock();
    let reset = after > buf.last;
    let from = if reset { 0 } else { after };
    let lines = buf.lines.iter().filter(|(seq, _)| *seq > from).map(|(_, l)| l.clone()).collect();
    (lines, buf.last, reset)
}

pub fn clear() {
    BUF.lock().lines.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colours_and_pages() {
        assert_eq!(strip_ansi("\x1b[2m2026\x1b[0m \x1b[32m INFO\x1b[0m hi"), "2026  INFO hi");
        let (_, start, _) = since(0);
        push("one\ntwo\n");
        let (lines, last, reset) = since(start);
        assert!(!reset);
        assert_eq!(last, start + 2);
        assert!(lines.ends_with(&["one".to_string(), "two".to_string()]));
        assert!(since(last + 10).2);
        for i in 0..MAX_LINES + 5 {
            push(&format!("l{i}\n"));
        }
        assert_eq!(since(0).0.len(), MAX_LINES);
    }
}
