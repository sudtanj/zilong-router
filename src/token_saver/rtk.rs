//! RTK: compress tool output (git, grep, find, ls, tree, build logs, …) inside
//! request bodies. Port of upstream `open-sse/rtk/` (itself a port of
//! https://github.com/rtk-ai/rtk), with the regexes rewritten as plain string
//! checks so the binary needs no regex engine.

use std::collections::BTreeMap;

use serde_json::Value;

pub const RAW_CAP: usize = 10 * 1024 * 1024;
pub const MIN_COMPRESS_SIZE: usize = 500;
const DETECT_WINDOW: usize = 1024;
const GIT_DIFF_HUNK_MAX_LINES: usize = 100;
const GIT_DIFF_MAX_LINES: usize = 500;
const GIT_LOG_MAX_LINES: usize = 200;
const DEDUP_LINE_MAX: usize = 2000;
const GREP_PER_FILE_MAX: usize = 10;
const FIND_PER_DIR_MAX: usize = 10;
const FIND_TOTAL_DIR_MAX: usize = 20;
const STATUS_MAX_FILES: usize = 10;
const STATUS_MAX_UNTRACKED: usize = 10;
const LS_EXT_SUMMARY_TOP: usize = 5;
const LS_NOISE_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "__pycache__",
    ".next",
    "dist",
    "build",
    ".cache",
    ".turbo",
    ".vercel",
    ".pytest_cache",
    ".mypy_cache",
    ".tox",
    ".venv",
    "venv",
    "env",
    "coverage",
    ".nyc_output",
    ".DS_Store",
    "Thumbs.db",
    ".idea",
    ".vscode",
    ".vs",
    "*.egg-info",
    ".eggs",
];
const TREE_MAX_LINES: usize = 200;
const SEARCH_LIST_PER_DIR_MAX: usize = 10;
const SEARCH_LIST_TOTAL_DIR_MAX: usize = 20;
const SMART_TRUNCATE_HEAD: usize = 120;
const SMART_TRUNCATE_TAIL: usize = 60;
const SMART_TRUNCATE_MIN_LINES: usize = 250;
const READ_NUMBERED_MIN_HIT_RATIO: f64 = 0.7;
const DEPRECATION_KEEP: usize = 3;

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub hits: Vec<&'static str>,
}

type Filter = (&'static str, fn(&str) -> String);

// ----- body walk ---------------------------------------------------------------------

/// Compress tool results in place. Handles OpenAI chat (`role: tool`), Claude
/// (`tool_result` blocks; error results are left alone) and Responses
/// (`function_call_output`).
pub fn compress_messages(body: &mut Value) -> Stats {
    let mut stats = Stats::default();
    let items = match body.as_object_mut() {
        Some(o) if o.get("messages").map_or(false, Value::is_array) => o.get_mut("messages"),
        Some(o) => o.get_mut("input"),
        None => None,
    };
    let Some(Value::Array(items)) = items else { return stats };
    for msg in items.iter_mut() {
        let Some(m) = msg.as_object_mut() else { continue };
        if m.get("type").and_then(Value::as_str) == Some("function_call_output") {
            match m.get_mut("output") {
                Some(Value::String(s)) => compress_string(s, &mut stats),
                Some(Value::Array(parts)) => {
                    for p in parts {
                        if p.get("type").and_then(Value::as_str) == Some("input_text") {
                            compress_text_field(p, &mut stats);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let is_tool = m.get("role").and_then(Value::as_str) == Some("tool");
        match m.get_mut("content") {
            Some(Value::String(s)) if is_tool => compress_string(s, &mut stats),
            Some(Value::Array(parts)) if is_tool => {
                for p in parts {
                    if p.get("type").and_then(Value::as_str) == Some("text") {
                        compress_text_field(p, &mut stats);
                    }
                }
            }
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) != Some("tool_result")
                        || b.get("is_error").and_then(Value::as_bool) == Some(true)
                    {
                        continue;
                    }
                    match b.get_mut("content") {
                        Some(Value::String(s)) => compress_string(s, &mut stats),
                        Some(Value::Array(parts)) => {
                            for p in parts {
                                if p.get("type").and_then(Value::as_str) == Some("text") {
                                    compress_text_field(p, &mut stats);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    stats
}

fn compress_text_field(part: &mut Value, stats: &mut Stats) {
    if let Some(Value::String(s)) = part.get_mut("text") {
        compress_string(s, stats);
    }
}

fn compress_string(s: &mut String, stats: &mut Stats) {
    if let Some(out) = compress_text(s, stats) {
        *s = out;
    }
}

/// Returns the compressed text, or `None` to keep the original.
pub fn compress_text(text: &str, stats: &mut Stats) -> Option<String> {
    let n = text.len();
    stats.bytes_before += n;
    let keep = |stats: &mut Stats| {
        stats.bytes_after += n;
        None
    };
    if !(MIN_COMPRESS_SIZE..=RAW_CAP).contains(&n) {
        return keep(stats);
    }
    let Some((name, f)) = auto_detect(text) else { return keep(stats) };
    let out = f(text);
    // Never return empty, never grow the input.
    if out.is_empty() || out.len() >= n {
        return keep(stats);
    }
    stats.bytes_after += out.len();
    stats.hits.push(name);
    Some(out)
}

// ----- detection -----------------------------------------------------------------------

fn head_of(text: &str) -> &str {
    if text.len() <= DETECT_WINDOW {
        return text;
    }
    let mut end = DETECT_WINDOW;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Order: git-log, git-diff, git-status, build-output, porcelain status, grep,
/// find, tree, ls, search-list, read-numbered, dedup-log, smart-truncate.
fn auto_detect(text: &str) -> Option<Filter> {
    let head = head_of(text);
    let lines: Vec<&str> = head.split('\n').collect();
    if lines.iter().any(|l| is_git_log_header_line(l)) {
        return Some(("git-log", git_log));
    }
    if lines.iter().any(|l| l.starts_with("diff --git ") || l.starts_with("@@ ")) {
        return Some(("git-diff", git_diff));
    }
    if lines.iter().any(|l| {
        l.starts_with("On branch ")
            || l.starts_with("nothing to commit")
            || l.starts_with("Changes not ")
            || l.starts_with("Changes to be ")
            || l.starts_with("Untracked files:")
    }) {
        return Some(("git-status", git_status));
    }
    if lines.iter().any(|l| is_build_line(l)) {
        return Some(("build-output", build_output));
    }
    let non_empty: Vec<&str> = lines.iter().copied().filter(|l| !l.trim().is_empty()).collect();
    if non_empty.len() >= 3 {
        let hits = non_empty.iter().filter(|l| is_porcelain(l)).count();
        if hits as f64 / non_empty.len() as f64 >= 0.6 {
            return Some(("git-status", git_status));
        }
    }
    if non_empty.iter().take(5).any(|l| grep_parts(l).is_some()) {
        return Some(("grep", grep));
    }
    if non_empty.len() >= 3 && non_empty.iter().all(|l| is_path_like(l)) {
        return Some(("find", find));
    }
    if head.contains("├──") || head.contains("└──") || head.contains("│  ") {
        return Some(("tree", tree));
    }
    if lines.iter().any(|l| is_ls_total(l)) || lines.iter().filter(|l| is_ls_row(l)).count() >= 3 {
        return Some(("ls", ls));
    }
    if search_list_header(head) {
        return Some(("search-list", search_list));
    }
    if lines.len() >= SMART_TRUNCATE_MIN_LINES && is_line_numbered(&lines) {
        return Some(("read-numbered", read_numbered));
    }
    if non_empty.len() >= 5 {
        return Some(("dedup-log", dedup_log));
    }
    if text.split('\n').count() >= SMART_TRUNCATE_MIN_LINES {
        return Some(("smart-truncate", smart_truncate));
    }
    None
}

const GRAPH: &[u8] = b"*|/\\ ";

fn graph_prefix_len(s: &str) -> usize {
    s.bytes().take_while(|b| GRAPH.contains(b)).count()
}

/// Leading run of hex digits (any case when `ci`), 7..=40 long.
fn hex_run(s: &str, ci: bool) -> Option<usize> {
    let n = s
        .bytes()
        .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b) || (ci && (b'A'..=b'F').contains(b)))
        .count();
    (7..=40).contains(&n).then_some(n)
}

/// `^[*|/\\ ]*commit [0-9a-f]{7,40}$`
fn is_git_log_header_line(l: &str) -> bool {
    let rest = &l[graph_prefix_len(l)..];
    let Some(sha) = rest.strip_prefix("commit ") else { return false };
    hex_run(sha, false).map_or(false, |n| n == sha.len())
}

fn is_build_line(l: &str) -> bool {
    let low = l.to_ascii_lowercase();
    let t = low.trim_start();
    let word_then_token = |w: &str| {
        t.strip_prefix(w).map(|r| r.starts_with(char::is_whitespace) && !r.trim_start().is_empty()).unwrap_or(false)
    };
    low.starts_with("npm warn")
        || low.starts_with("npm error")
        || low.starts_with("npm err!")
        || low.starts_with("yarn warn")
        || low.starts_with("yarn error")
        || word_then_token("compiling")
        || word_then_token("downloading")
        || low
            .strip_prefix("added ")
            .map(|r| {
                let d = r.bytes().take_while(u8::is_ascii_digit).count();
                d > 0 && r[d..].starts_with(" package")
            })
            .unwrap_or(false)
        || low.starts_with("[error]")
        || low.starts_with("build success")
        || low.starts_with("build failed")
        || t.strip_prefix("finished").map(|r| r.starts_with(char::is_whitespace)).unwrap_or(false)
        || low.starts_with("successfully installed")
        || low.starts_with("successfully built")
        || low.starts_with("error:")
}

const PORCELAIN: &[u8] = b" MADRCU?!";

/// `^[ MADRCU?!][ MADRCU?!] \S`
fn is_porcelain(l: &str) -> bool {
    let b = l.as_bytes();
    b.len() >= 4
        && PORCELAIN.contains(&b[0])
        && PORCELAIN.contains(&b[1])
        && b[2] == b' '
        && !b[3].is_ascii_whitespace()
}

/// `file:lineno:content` split on the first two colons.
fn grep_parts(l: &str) -> Option<(&str, &str, &str)> {
    let (file, rest) = l.split_once(':')?;
    let (num, content) = rest.split_once(':')?;
    (!num.is_empty() && num.bytes().all(|b| b.is_ascii_digit())).then_some((file, num, content))
}

fn is_path_like(l: &str) -> bool {
    let t = l.trim();
    if t.is_empty() {
        return false;
    }
    let b = t.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        return true;
    }
    if t.contains(':') {
        return false;
    }
    t.starts_with('.') || t.starts_with('/') || t.contains('/')
}

fn is_ls_total(l: &str) -> bool {
    l.strip_prefix("total ").map_or(false, |r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
}

/// `^[-dlbcps][rwx-]{9}`
fn is_ls_row(l: &str) -> bool {
    let b = l.as_bytes();
    b.len() >= 10 && b"-dlbcps".contains(&b[0]) && b[1..10].iter().all(|c| b"rwx-".contains(c))
}

/// `^Result of search in '[^']*' \(total (\d+) files?\):`
fn search_list_header(s: &str) -> bool {
    let Some(r) = s.strip_prefix("Result of search in '") else { return false };
    let Some(q) = r.find('\'') else { return false };
    let Some(r) = r[q + 1..].strip_prefix(" (total ") else { return false };
    let d = r.bytes().take_while(u8::is_ascii_digit).count();
    if d == 0 {
        return false;
    }
    let Some(r) = r[d..].strip_prefix(" file") else { return false };
    r.strip_prefix('s').unwrap_or(r).starts_with("):")
}

/// `^\s*\d+\|`
fn is_numbered_line(l: &str) -> bool {
    let t = l.trim_start();
    let d = t.bytes().take_while(u8::is_ascii_digit).count();
    d > 0 && t[d..].starts_with('|')
}

fn is_line_numbered(lines: &[&str]) -> bool {
    let (mut hits, mut non_empty) = (0usize, 0usize);
    for l in lines.iter().take(100) {
        if l.is_empty() {
            continue;
        }
        non_empty += 1;
        if is_numbered_line(l) {
            hits += 1;
        }
    }
    non_empty >= 5 && hits as f64 / non_empty as f64 >= READ_NUMBERED_MIN_HIT_RATIO
}

// ----- filters -------------------------------------------------------------------------

fn git_diff(diff: &str) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut current_file = String::new();
    let (mut added, mut removed) = (0usize, 0usize);
    let mut in_hunk = false;
    let (mut shown, mut skipped) = (0usize, 0usize);
    let mut truncated = false;
    for line in diff.split('\n') {
        if line.starts_with("diff --git") {
            if skipped > 0 {
                result.push(format!("  ... ({skipped} lines truncated)"));
                truncated = true;
                skipped = 0;
            }
            if !current_file.is_empty() && (added > 0 || removed > 0) {
                result.push(format!("  +{added} -{removed}"));
            }
            current_file = line.split_once(" b/").map(|(_, f)| f.to_string()).unwrap_or_else(|| "unknown".into());
            result.push(format!("\n{current_file}"));
            added = 0;
            removed = 0;
            in_hunk = false;
            shown = 0;
        } else if line.starts_with("@@") {
            if skipped > 0 {
                result.push(format!("  ... ({skipped} lines truncated)"));
                truncated = true;
                skipped = 0;
            }
            in_hunk = true;
            shown = 0;
            result.push(format!("  {line}"));
        } else if in_hunk {
            let plus = line.starts_with('+') && !line.starts_with("+++");
            let minus = line.starts_with('-') && !line.starts_with("---");
            if plus || minus {
                if plus {
                    added += 1;
                } else {
                    removed += 1;
                }
                if shown < GIT_DIFF_HUNK_MAX_LINES {
                    result.push(format!("  {line}"));
                    shown += 1;
                } else {
                    skipped += 1;
                }
            } else if shown < GIT_DIFF_HUNK_MAX_LINES && !line.starts_with('\\') && shown > 0 {
                result.push(format!("  {line}"));
                shown += 1;
            }
        }
        if result.len() >= GIT_DIFF_MAX_LINES {
            result.push("\n... (more changes truncated)".into());
            truncated = true;
            break;
        }
    }
    if skipped > 0 {
        result.push(format!("  ... ({skipped} lines truncated)"));
        truncated = true;
    }
    if !current_file.is_empty() && (added > 0 || removed > 0) {
        result.push(format!("  +{added} -{removed}"));
    }
    if truncated {
        result.push("[full diff: rtk git diff --no-compact]".into());
    }
    result.join("\n")
}

/// `^[*|/\\ ]*    \S` (four spaces, possibly after graph decoration, then text).
fn is_indented_subject(line: &str) -> bool {
    let b = line.as_bytes();
    let p = graph_prefix_len(line);
    (4..=p.min(b.len().saturating_sub(1)))
        .any(|j| b[j - 4..j].iter().all(|&c| c == b' ') && !b[j].is_ascii_whitespace())
}

fn git_log(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut skipped = 0usize;
    let mut push = |out: &mut Vec<String>, l: String| {
        if out.len() < GIT_LOG_MAX_LINES {
            out.push(l);
        } else {
            skipped += 1;
        }
    };
    let (mut in_commit, mut subject_seen) = (false, false);
    for raw in input.split('\n') {
        let line = raw.trim_end();
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        let commit_hdr = {
            let g = graph_prefix_len(&lower);
            let rest = &lower[g..];
            rest.strip_prefix("commit ").and_then(|sha| hex_run(sha, false).map(|n| (n, sha.len()))).map_or(
                false,
                |(n, len)| {
                    // `^commit <sha>$` without decoration; any tail is fine after decoration.
                    g > 0 || n == len
                },
            )
        };
        if commit_hdr {
            in_commit = true;
            subject_seen = false;
            push(&mut out, line.to_string());
            continue;
        }
        if in_commit {
            let after_graph = &lower[graph_prefix_len(&lower)..];
            if after_graph.starts_with("author:") || after_graph.starts_with("date:") {
                push(&mut out, trimmed.to_string());
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            if !subject_seen && is_indented_subject(line) {
                push(&mut out, format!("  Subject: {trimmed}"));
                subject_seen = true;
                continue;
            }
            let d = trimmed.bytes().take_while(u8::is_ascii_digit).count();
            if d > 0 {
                if let Some(r) = trimmed[d..].strip_prefix(" file") {
                    let w = r.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
                    if r[w..].starts_with(" changed") {
                        push(&mut out, format!("  {trimmed}"));
                        continue;
                    }
                }
            }
            if trimmed.starts_with("diff --git ") {
                push(&mut out, "  ... diff body omitted".into());
            }
            continue;
        }
        // --oneline / --graph modes.
        let g = graph_prefix_len(trimmed);
        if g > 0 {
            let rest = &trimmed[g..];
            if let Some(n) = hex_run(rest, true) {
                let tail = &rest[n..];
                let ws = tail.len() - tail.trim_start().len();
                if ws > 0 && !tail.trim_start().is_empty() {
                    push(&mut out, rest.to_string());
                    continue;
                }
            }
        }
        if let Some(n) = hex_run(trimmed, false) {
            if trimmed[n..].starts_with(char::is_whitespace) {
                push(&mut out, trimmed.to_string());
                continue;
            }
        }
        if g == trimmed.len() && trimmed.bytes().any(|b| b"*|/\\".contains(&b)) {
            continue;
        }
        push(&mut out, trimmed.to_string());
    }
    if skipped > 0 {
        out.push(format!("... ({skipped} more lines)"));
    }
    let result = out.join("\n");
    if result.is_empty() || result.len() > input.len() {
        return input.to_string();
    }
    result
}

fn git_status(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    if lines.len() == 1 && lines[0].trim().is_empty() {
        return "Clean working tree".into();
    }
    let mut branch = String::new();
    let (mut staged_f, mut modified_f, mut untracked_f) = (Vec::new(), Vec::new(), Vec::new());
    let (mut staged, mut modified, mut untracked, mut conflicts) = (0usize, 0usize, 0usize, 0usize);
    for raw in lines {
        if raw.trim().is_empty() {
            continue;
        }
        if let Some(r) = raw.strip_prefix("On branch ") {
            if let Some(b) = r.split_whitespace().next().filter(|_| !r.starts_with(char::is_whitespace)) {
                branch = b.to_string();
                continue;
            }
        }
        if raw.starts_with("##") {
            branch = raw[2..].trim_start().to_string();
            continue;
        }
        let b = raw.as_bytes();
        if b.len() >= 3 && PORCELAIN.contains(&b[0]) && PORCELAIN.contains(&b[1]) && b[2] == b' ' {
            let file = raw[3..].to_string();
            if &raw[..2] == "??" {
                untracked += 1;
                untracked_f.push(file);
                continue;
            }
            let (x, y) = (b[0], b[1]);
            if b"MADRC".contains(&x) {
                staged += 1;
                staged_f.push(file.clone());
            } else if x == b'U' {
                conflicts += 1;
            }
            if y == b'M' || y == b'D' {
                modified += 1;
                modified_f.push(file);
            }
            continue;
        }
        let t = raw.trim_start();
        for kind in ["modified", "new file", "deleted", "renamed", "both modified"] {
            let Some(r) = t.strip_prefix(kind).and_then(|r| r.strip_prefix(':')) else { continue };
            if !r.starts_with(char::is_whitespace) || r.trim().is_empty() {
                continue;
            }
            let path = r.trim().to_string();
            match kind {
                "both modified" => conflicts += 1,
                "modified" | "deleted" => {
                    modified += 1;
                    modified_f.push(path);
                }
                _ => {
                    staged += 1;
                    staged_f.push(path);
                }
            }
            break;
        }
    }
    let mut out = String::new();
    if !branch.is_empty() {
        out += &format!("* {branch}\n");
    }
    let list = |out: &mut String, title: &str, n: usize, files: &[String], cap: usize| {
        if n == 0 {
            return;
        }
        *out += &format!("{title}: {n} files\n");
        for f in files.iter().take(cap) {
            *out += &format!("   {f}\n");
        }
        if files.len() > cap {
            *out += &format!("   ... +{} more\n", files.len() - cap);
        }
    };
    list(&mut out, "+ Staged", staged, &staged_f, STATUS_MAX_FILES);
    list(&mut out, "~ Modified", modified, &modified_f, STATUS_MAX_FILES);
    list(&mut out, "? Untracked", untracked, &untracked_f, STATUS_MAX_UNTRACKED);
    if conflicts > 0 {
        out += &format!("conflicts: {conflicts} files\n");
    }
    if staged == 0 && modified == 0 && untracked == 0 && conflicts == 0 {
        out += "clean — nothing to commit\n";
    }
    out.trim_end_matches('\n').to_string()
}

/// `^\s*(-->|\||\d+\s*\||=)`: continuation of a cargo/rustc error block.
fn is_cargo_continuation(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("-->") || t.starts_with('|') || t.starts_with('=') {
        return true;
    }
    let d = t.bytes().take_while(u8::is_ascii_digit).count();
    d > 0 && t[d..].trim_start().starts_with('|')
}

fn build_output(input: &str) -> String {
    let (mut errors, mut warnings, mut deprecations): (Vec<&str>, Vec<&str>, Vec<&str>) =
        (Vec::new(), Vec::new(), Vec::new());
    let mut summary: Vec<&str> = Vec::new();
    let (mut compiling, mut downloading) = (0usize, 0usize);
    let mut in_cargo_error = false;
    for line in input.split('\n') {
        let trimmed = line.trim();
        if in_cargo_error {
            if trimmed.is_empty() {
                in_cargo_error = false;
                continue;
            }
            if is_cargo_continuation(line) {
                errors.push(line);
                continue;
            }
            in_cargo_error = false;
        }
        if trimmed.is_empty() {
            continue;
        }
        let low = trimmed.to_ascii_lowercase();
        let word_ws = |w: &str| low.strip_prefix(w).map_or(false, |r| r.starts_with(char::is_whitespace));
        if low.starts_with("npm err!") || low.starts_with("npm error") || low.starts_with("yarn error") {
            errors.push(line);
        } else if low.starts_with("npm warn deprecated") {
            deprecations.push(line);
        } else if low.starts_with("npm warn") || low.starts_with("yarn warn") {
            warnings.push(line);
        } else if low.starts_with("error[") || low.starts_with("error:") || trimmed.starts_with("error -->") {
            errors.push(line);
            in_cargo_error = true;
        } else if low.starts_with("warning[") || low.starts_with("warning:") || trimmed.starts_with("warning -->") {
            warnings.push(line);
            in_cargo_error = true;
        } else if low.starts_with("[error]") || low.starts_with("build failed") {
            errors.push(line);
        } else if low.starts_with("[warning]") {
            warnings.push(line);
        } else if word_ws("compiling") {
            compiling += 1;
        } else if word_ws("downloading") || word_ws("fetching") {
            downloading += 1;
        } else if is_summary_line(&low) {
            summary.push(line);
        }
    }
    let mut out = String::new();
    for d in deprecations.iter().take(DEPRECATION_KEEP) {
        out += &format!("{d}\n");
    }
    if deprecations.len() > DEPRECATION_KEEP {
        out += &format!("... +{} more deprecated packages\n", deprecations.len() - DEPRECATION_KEEP);
    }
    if compiling > 0 {
        out += &format!("Compiled {compiling} packages\n");
    }
    if downloading > 0 {
        out += &format!("Downloaded {downloading} packages\n");
    }
    for e in &errors {
        out += &format!("{e}\n");
    }
    for w in warnings.iter().take(5) {
        out += &format!("{w}\n");
    }
    if warnings.len() > 5 {
        out += &format!("... +{} more warnings\n", warnings.len() - 5);
    }
    if !summary.is_empty() {
        out += &format!("{}\n", summary.join("\n"));
    }
    let out = out.trim_end_matches('\n');
    if out.is_empty() {
        input.to_string()
    } else {
        out.to_string()
    }
}

fn is_summary_line(low: &str) -> bool {
    fn num_then(s: &str) -> Option<&str> {
        let d = s.bytes().take_while(u8::is_ascii_digit).count();
        if d == 0 {
            return None;
        }
        let r = &s[d..];
        let r2 = r.trim_start();
        (r2.len() < r.len()).then_some(r2)
    }
    for verb in ["added", "removed", "changed", "audited", "installed"] {
        if let Some(r) = low.strip_prefix(verb) {
            let r2 = r.trim_start();
            if r2.len() < r.len() && num_then(r2).map_or(false, |x| x.starts_with("package")) {
                return true;
            }
        }
    }
    if low.strip_prefix("finished").map_or(false, |r| r.starts_with(char::is_whitespace)) {
        return true;
    }
    if low.starts_with("build success")
        || low.starts_with("successfully installed")
        || low.starts_with("successfully built")
    {
        return true;
    }
    if let Some(r) = num_then(low) {
        if ["vulnerabilities", "package", "warning", "error"].iter().any(|w| r.starts_with(w)) {
            return true;
        }
    }
    if low.strip_prefix("to address ").map_or(false, |r| r.contains(" issues")) {
        return true;
    }
    low.starts_with("run `npm audit`")
        || low.starts_with("run `npm fund`")
        || low.contains("packages are looking for funding")
}

fn grep(input: &str) -> String {
    let mut by_file: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    let mut total = 0usize;
    for line in input.split('\n') {
        let Some((file, num, content)) = grep_parts(line) else { continue };
        total += 1;
        by_file.entry(file).or_default().push((num, content));
    }
    if total == 0 {
        return input.to_string();
    }
    let mut out = format!("{total} matches in {}F:\n\n", by_file.len());
    for (file, matches) in &by_file {
        out += &format!("[file] {file} ({}):\n", matches.len());
        for (num, content) in matches.iter().take(GREP_PER_FILE_MAX) {
            out += &format!("  {num:>4}: {}\n", content.trim());
        }
        if matches.len() > GREP_PER_FILE_MAX {
            out += &format!("  +{}\n", matches.len() - GREP_PER_FILE_MAX);
        }
        out += "\n";
    }
    out
}

fn group_by_dir<'a>(paths: impl Iterator<Item = &'a str>, windows: bool) -> BTreeMap<String, Vec<&'a str>> {
    let mut by_dir: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for p in paths {
        let sep = if windows { p.rfind(['/', '\\']) } else { p.rfind('/') };
        let (dir, name) = match sep {
            None => (".".to_string(), p),
            Some(i) => (if i == 0 { "/".to_string() } else { p[..i].to_string() }, &p[i + 1..]),
        };
        by_dir.entry(dir).or_default().push(name);
    }
    by_dir
}

fn find(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return input.to_string();
    }
    let by_dir = group_by_dir(lines.iter().copied(), true);
    let mut out = format!("{} files in {} dirs:\n\n", lines.len(), by_dir.len());
    for (dir, files) in by_dir.iter().take(FIND_TOTAL_DIR_MAX) {
        out += &format!("{}/  ({})\n", dir.replace('\\', "/"), files.len());
        for f in files.iter().take(FIND_PER_DIR_MAX) {
            out += &format!("  {f}\n");
        }
        if files.len() > FIND_PER_DIR_MAX {
            out += &format!("  +{}\n", files.len() - FIND_PER_DIR_MAX);
        }
    }
    if by_dir.len() > FIND_TOTAL_DIR_MAX {
        out += &format!("\n+{} more dirs\n", by_dir.len() - FIND_TOTAL_DIR_MAX);
    }
    out
}

fn dedup_log(input: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut prev: Option<&str> = None;
    let mut run = 0usize;
    let mut blanks = 0usize;
    let flush = |out: &mut Vec<String>, prev: Option<&str>, run: usize| {
        if prev.is_some() && run > 1 {
            out.push(format!("  ... ({} duplicate lines)", run - 1));
        }
    };
    for line in input.split('\n') {
        if line.trim().is_empty() {
            if blanks < 1 {
                out.push(line.to_string());
            }
            blanks += 1;
            flush(&mut out, prev, run);
            prev = None;
            run = 0;
            continue;
        }
        blanks = 0;
        if Some(line) == prev {
            run += 1;
            continue;
        }
        flush(&mut out, prev, run);
        out.push(line.to_string());
        prev = Some(line);
        run = 1;
        if out.len() >= DEDUP_LINE_MAX {
            out.push(format!("... (truncated at {DEDUP_LINE_MAX} lines)"));
            return out.join("\n");
        }
    }
    flush(&mut out, prev, run);
    out.join("\n")
}

const MONTHS: &[&str] = &["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Find `\s+<Mon>\s+\d{1,2}\s+(\d{4}|\d{2}:\d{2})\s+`; returns (match start, match end).
fn ls_date(line: &str) -> Option<(usize, usize)> {
    let b = line.as_bytes();
    let ws = |i: usize| i < b.len() && b[i].is_ascii_whitespace();
    let digits = |i: usize| (i..b.len()).take_while(|&j| b[j].is_ascii_digit()).count();
    let mut i = 0;
    while i < b.len() {
        if !ws(i) {
            i += 1;
            continue;
        }
        let start = i;
        while ws(i) {
            i += 1;
        }
        let try_at = |mut k: usize| -> Option<usize> {
            if !MONTHS.iter().any(|m| line.get(k..k + 3) == Some(*m)) {
                return None;
            }
            k += 3;
            let s = k;
            while ws(k) {
                k += 1;
            }
            if k == s {
                return None;
            }
            let d = digits(k);
            if !(1..=2).contains(&d) {
                return None;
            }
            k += d;
            let s = k;
            while ws(k) {
                k += 1;
            }
            if k == s {
                return None;
            }
            let d = digits(k);
            if d == 4 {
                k += 4;
            } else if d == 2 && b.get(k + 2) == Some(&b':') && digits(k + 3) == 2 {
                k += 5;
            } else {
                return None;
            }
            let s = k;
            while ws(k) {
                k += 1;
            }
            (k > s).then_some(k)
        };
        if let Some(end) = try_at(i) {
            return Some((start, end));
        }
    }
    None
}

fn human_size(n: u64) -> String {
    if n >= 1_048_576 {
        format!("{:.1}M", n as f64 / 1_048_576.0)
    } else if n >= 1024 {
        format!("{:.1}K", n as f64 / 1024.0)
    } else {
        format!("{n}B")
    }
}

fn ls(input: &str) -> String {
    let mut dirs: Vec<&str> = Vec::new();
    let mut files: Vec<(&str, String)> = Vec::new();
    let mut by_ext: Vec<(String, usize)> = Vec::new();
    for line in input.split('\n') {
        if line.starts_with("total ") || line.is_empty() {
            continue;
        }
        let Some((s, e)) = ls_date(line) else { continue };
        let name = &line[e..];
        let before: Vec<&str> = line[..s].split_whitespace().collect();
        if before.len() < 4 || name == "." || name == ".." || LS_NOISE_DIRS.contains(&name) {
            continue;
        }
        let kind = before[0].as_bytes()[0];
        let size = before
            .iter()
            .rev()
            .find(|t| t.bytes().all(|b| b.is_ascii_digit()) && (*t == &"0" || !t.starts_with('0')))
            .and_then(|t| t.parse::<u64>().ok())
            .unwrap_or(0);
        if kind == b'd' {
            dirs.push(name);
        } else if kind == b'-' || kind == b'l' {
            let ext = match name.rfind('.') {
                Some(i) if i > 0 => name[i..].to_string(),
                _ => "no ext".to_string(),
            };
            match by_ext.iter_mut().find(|(e, _)| *e == ext) {
                Some(x) => x.1 += 1,
                None => by_ext.push((ext, 1)),
            }
            files.push((name, human_size(size)));
        }
    }
    if dirs.is_empty() && files.is_empty() {
        return input.to_string();
    }
    let mut out = String::new();
    for d in &dirs {
        out += &format!("{d}/\n");
    }
    for (n, s) in &files {
        out += &format!("{n}  {s}\n");
    }
    out += &format!("\nSummary: {} files, {} dirs", files.len(), dirs.len());
    if !by_ext.is_empty() {
        by_ext.sort_by(|a, b| b.1.cmp(&a.1)); // stable, like Array.prototype.sort
        let parts: Vec<String> = by_ext.iter().take(LS_EXT_SUMMARY_TOP).map(|(e, c)| format!("{c} {e}")).collect();
        out += &format!(" ({}", parts.join(", "));
        if by_ext.len() > LS_EXT_SUMMARY_TOP {
            out += &format!(", +{} more", by_ext.len() - LS_EXT_SUMMARY_TOP);
        }
        out += ")";
    }
    out
}

fn tree(input: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for line in input.split('\n') {
        if line.contains("director") && line.contains("file") {
            continue;
        }
        if line.trim().is_empty() && kept.is_empty() {
            continue;
        }
        kept.push(line);
    }
    while kept.last().map_or(false, |l| l.trim().is_empty()) {
        kept.pop();
    }
    if kept.len() > TREE_MAX_LINES {
        let cut = kept.len() - TREE_MAX_LINES;
        return format!("{}\n... +{cut} more lines", kept[..TREE_MAX_LINES].join("\n"));
    }
    kept.join("\n")
}

fn head_tail(input: &str, marker: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    if lines.len() < SMART_TRUNCATE_MIN_LINES {
        return input.to_string();
    }
    let cut = lines.len() - SMART_TRUNCATE_HEAD - SMART_TRUNCATE_TAIL;
    let mut out: Vec<String> = lines[..SMART_TRUNCATE_HEAD].iter().map(|s| s.to_string()).collect();
    out.push(format!("... +{cut} {marker}"));
    out.extend(lines[lines.len() - SMART_TRUNCATE_TAIL..].iter().map(|s| s.to_string()));
    out.join("\n")
}

fn smart_truncate(input: &str) -> String {
    head_tail(input, "lines truncated")
}

fn read_numbered(input: &str) -> String {
    head_tail(input, "lines truncated (file continues)")
}

fn search_list(input: &str) -> String {
    let mut lines = input.split('\n');
    let header = lines.next().unwrap_or("");
    let paths: Vec<&str> = lines.filter_map(|l| l.trim().strip_prefix("- ")).collect();
    if paths.is_empty() {
        return input.to_string();
    }
    let by_dir = group_by_dir(paths.iter().copied(), false);
    let mut out = format!("{header}\n{} files in {} dirs:\n\n", paths.len(), by_dir.len());
    for (dir, names) in by_dir.iter().take(SEARCH_LIST_TOTAL_DIR_MAX) {
        out += &format!("{dir}/ ({}):\n", names.len());
        for n in names.iter().take(SEARCH_LIST_PER_DIR_MAX) {
            out += &format!("  {n}\n");
        }
        if names.len() > SEARCH_LIST_PER_DIR_MAX {
            out += &format!("  +{}\n", names.len() - SEARCH_LIST_PER_DIR_MAX);
        }
        out += "\n";
    }
    if by_dir.len() > SEARCH_LIST_TOTAL_DIR_MAX {
        out += &format!("+{} more dirs\n", by_dir.len() - SEARCH_LIST_TOTAL_DIR_MAX);
    }
    out.trim_end_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detect(s: &str) -> Option<&'static str> {
        auto_detect(s).map(|(n, _)| n)
    }

    #[test]
    fn detects_common_shapes() {
        assert_eq!(detect("commit 0123456789abcdef\nAuthor: a\n"), Some("git-log"));
        assert_eq!(detect("diff --git a/x b/x\n@@ -1 +1 @@\n"), Some("git-diff"));
        assert_eq!(detect("On branch main\nnothing to commit"), Some("git-status"));
        assert_eq!(detect("   Compiling serde v1.0\n"), Some("build-output"));
        assert_eq!(detect(" M a.rs\n M b.rs\n?? c.rs\n"), Some("git-status"));
        assert_eq!(detect("src/a.rs:10:fn main() {}\n"), Some("grep"));
        assert_eq!(detect("./a/b.rs\n./a/c.rs\n./d/e.rs\n"), Some("find"));
        assert_eq!(detect(".\n├── a\n└── b\n"), Some("tree"));
        assert_eq!(detect("total 8\ndrwxr-xr-x 2 u g 4096 Jan  1 12:00 x\n"), Some("ls"));
        assert_eq!(detect("Result of search in '/x' (total 2 files):\n- a/b\n- a/c"), Some("search-list"));
        assert_eq!(detect("hello"), None);
    }

    #[test]
    fn grep_groups_by_file() {
        let input: String = (1..=30).map(|i| format!("src/a.rs:{i}:    let x = {i};\n")).collect();
        let out = grep(&input);
        assert!(out.starts_with("30 matches in 1F:"));
        assert!(out.contains("[file] src/a.rs (30):"));
        assert!(out.contains("  +20"));
    }

    #[test]
    fn ls_parses_long_listing() {
        let input = "total 16\n\
            drwxr-xr-x  5 user staff  160 Jan 12 10:00 src\n\
            drwxr-xr-x  5 user staff  160 Jan 12 10:00 node_modules\n\
            -rw-r--r--  1 user staff 2048 Feb  3  2024 Cargo.toml\n\
            -rw-r--r--  1 user staff   12 Feb  3 09:15 a.rs\n";
        let out = ls(input);
        assert!(out.contains("src/\n"));
        assert!(!out.contains("node_modules"));
        assert!(out.contains("Cargo.toml  2.0K"));
        assert!(out.contains("a.rs  12B"));
        assert!(out.contains("Summary: 2 files, 1 dirs (1 .toml, 1 .rs)"));
    }

    #[test]
    fn git_status_summarises_porcelain() {
        let out = git_status("## main\n M a.rs\nA  b.rs\n?? c.rs\n");
        assert_eq!(
            out,
            "* main\n+ Staged: 1 files\n   b.rs\n~ Modified: 1 files\n   a.rs\n? Untracked: 1 files\n   c.rs"
        );
    }

    #[test]
    fn compresses_claude_tool_results_but_not_errors() {
        let big: String =
            (0..40).map(|i| format!("src/main.rs:{i}:    let value_{i} = compute(input, {i});\n")).collect();
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "content": big.clone()},
            {"type": "tool_result", "is_error": true, "content": big.clone()},
        ]}]});
        let stats = compress_messages(&mut body);
        assert_eq!(stats.hits, vec!["grep"]);
        assert!(stats.bytes_after < stats.bytes_before);
        assert_ne!(body["messages"][0]["content"][0]["content"], json!(big));
        assert_eq!(body["messages"][0]["content"][1]["content"], json!(big));
    }

    #[test]
    fn compresses_openai_tool_messages() {
        let big: String = (0..300).map(|_| "same noisy log line\n").collect();
        let mut body = json!({"messages": [{"role": "tool", "tool_call_id": "1", "content": big}]});
        let stats = compress_messages(&mut body);
        assert_eq!(stats.hits.len(), 1);
        let out = body["messages"][0]["content"].as_str().unwrap();
        assert!(out.len() < 200);
    }
}
