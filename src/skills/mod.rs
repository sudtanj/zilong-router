//! Skills Gateway: one place for Agent Skills (folders with a `SKILL.md` whose YAML
//! frontmatter has a `name` and a `description`, plus optional scripts and references, the
//! format of github.com/anthropics/skills), each switched on or off in the dashboard.
//!
//! Every client gets the enabled skills through the router:
//!
//! * the MCP gateway (`/mcp`, `/mcp/all`, and `/skills` with only these tools) offers
//!   `list_skills`, `load_skill` and `read_skill_file`, mentions the skills in its
//!   `initialize` instructions, and lists them as MCP prompts. Like Claude's own skills,
//!   only names and descriptions are shown up front; the body is loaded on demand;
//! * optionally (`skillsIndexEnabled`, off by default) a short index of the enabled skills
//!   is added to the system prompt of `/v1` chat, messages and responses requests.
//!
//! Skills live in the `kv` table, so the Turso mirror carries them: scope `skills` holds
//! the metadata (small, read on every listing), scope `skillFiles` the SKILL.md and the
//! other files (read only when a skill is loaded). Text files only, 1 MB per skill and
//! 50 MB in total.

pub mod github;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::db::{now_iso, Db};
use crate::providers::Format;

const SCOPE: &str = "skills";
const FILES_SCOPE: &str = "skillFiles";
/// Settings key of "Add the skills index to chat requests".
pub const INDEX_SETTING: &str = "skillsIndexEnabled";
pub const MAX_SKILL_BYTES: usize = 1024 * 1024;
pub const MAX_TOTAL_BYTES: usize = 50 * 1024 * 1024;
const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 1024;
pub const MAIN_FILE: &str = "SKILL.md";
/// The MCP tools of the gateway.
pub const TOOLS: [&str; 3] = ["list_skills", "load_skill", "read_skill_file"];

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct FileInfo {
    pub path: String,
    pub bytes: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    /// `paste`, `upload` or `github:<owner>/<repo>/<path>@<ref>`.
    pub source: String,
    pub enabled: bool,
    /// The files besides SKILL.md.
    pub files: Vec<FileInfo>,
    /// SKILL.md plus the other files.
    pub bytes: usize,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Content {
    /// SKILL.md as written, frontmatter included.
    pub skill_md: String,
    /// Path relative to the skill folder -> text.
    pub files: BTreeMap<String, String>,
}

// ----- parsing ------------------------------------------------------------------------

/// The fields of a `---` YAML frontmatter block and the text after it. Only what skills
/// use is understood: top-level `key: value` pairs, quoted values, values continued on
/// indented lines, and `|` / `>` block scalars. Nested keys are skipped. Without a
/// frontmatter block the fields are empty and the body is the whole text.
pub fn frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut fields = BTreeMap::new();
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return (fields, text);
    }
    let mut offset = text.find('\n').map_or(text.len(), |i| i + 1);
    let mut yaml: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines {
        offset += line.len();
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim_end() == "---" || line.trim_end() == "..." {
            closed = true;
            break;
        }
        yaml.push(line);
    }
    if !closed {
        return (fields, text);
    }
    let indented = |l: &str| l.starts_with([' ', '\t']) || l.trim().is_empty();
    let mut i = 0;
    while i < yaml.len() {
        let line = yaml[i];
        i += 1;
        if indented(line) || line.starts_with('#') {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else { continue };
        let mut more: Vec<&str> = Vec::new();
        while i < yaml.len() && indented(yaml[i]) {
            more.push(yaml[i]);
            i += 1;
        }
        let rest = rest.trim();
        let value = if rest.starts_with('|') || rest.starts_with('>') {
            block_scalar(rest.starts_with('|'), &more)
        } else {
            let mut v = rest.to_string();
            for m in more.iter().map(|m| m.trim()).filter(|m| !m.is_empty()) {
                if !v.is_empty() {
                    v.push(' ');
                }
                v.push_str(m);
            }
            unquote(&v)
        };
        fields.insert(key.trim().to_string(), value);
    }
    (fields, &text[offset.min(text.len())..])
}

/// `|` keeps the line breaks, `>` folds lines into one (blank lines stay breaks).
fn block_scalar(literal: bool, lines: &[&str]) -> String {
    if literal {
        return lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join("\n").trim().to_string();
    }
    let mut out = String::new();
    for l in lines.iter().map(|l| l.trim()) {
        if l.is_empty() {
            out.push('\n');
            continue;
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push(' ');
        }
        out.push_str(l);
    }
    out.trim().to_string()
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if let Some(inner) = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return inner.replace("\\\"", "\"").replace("\\n", "\n").replace("\\\\", "\\");
    }
    if let Some(inner) = v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return inner.replace("''", "'");
    }
    // A plain value ends at a ` #` comment.
    match v.find(" #") {
        Some(i) => v[..i].trim_end().to_string(),
        None => v.to_string(),
    }
}

/// A file path inside a skill folder: relative, `/`-separated, without `.` or `..`
/// segments, backslashes, drive letters or control characters.
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.starts_with('/')
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// The id made from a skill name: `PDF Tools` -> `pdf-tools`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.chars().take(MAX_NAME).collect();
    out.trim_end_matches('-').to_string()
}

/// Checks a name and a description; the error is shown in the dashboard.
pub fn check_meta(name: &str, description: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the skill needs a name (`name:` in the SKILL.md frontmatter)".into());
    }
    if name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(format!("the name must be one line of at most {MAX_NAME} characters"));
    }
    if slug(name).is_empty() {
        return Err("the name needs at least one letter or digit".into());
    }
    if description.is_empty() {
        return Err("the skill needs a description (`description:` in the SKILL.md frontmatter)".into());
    }
    if description.chars().count() > MAX_DESCRIPTION {
        return Err(format!("the description is limited to {MAX_DESCRIPTION} characters"));
    }
    Ok(())
}

/// A skill from its SKILL.md and other files; `name` and `description`, when given,
/// replace the frontmatter's.
pub fn build(
    skill_md: &str,
    mut files: BTreeMap<String, String>,
    source: String,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<(Skill, Content), String> {
    let skill_md = skill_md.replace("\r\n", "\n");
    let (fields, _) = frontmatter(&skill_md);
    let pick = |given: Option<&str>, key: &str| {
        let given = given.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        given.or_else(|| fields.get(key).map(|v| v.trim().to_string())).unwrap_or_default()
    };
    let (name, description) = (pick(name, "name"), pick(description, "description"));
    check_meta(&name, &description)?;
    files.remove(MAIN_FILE);
    if let Some(bad) = files.keys().find(|p| !valid_path(p)) {
        return Err(format!("invalid file path `{bad}`: paths must stay inside the skill folder"));
    }
    let bytes = skill_md.len() + files.values().map(String::len).sum::<usize>();
    if bytes > MAX_SKILL_BYTES {
        return Err(format!("the skill is {} KB; the limit is 1 MB per skill", bytes / 1024));
    }
    let now = now_iso();
    let skill = Skill {
        id: slug(&name),
        name,
        description,
        source,
        enabled: true,
        files: files.iter().map(|(p, c)| FileInfo { path: p.clone(), bytes: c.len() }).collect(),
        bytes,
        created_at: now.clone(),
        updated_at: now,
    };
    Ok((skill, Content { skill_md, files }))
}

// ----- storage ------------------------------------------------------------------------

pub fn list(db: &Db) -> Vec<Skill> {
    let mut list: Vec<Skill> = db
        .kv_scope(SCOPE)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(id, v)| {
            let mut s: Skill = serde_json::from_value(v).ok()?;
            s.id = id;
            Some(s)
        })
        .collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

pub fn enabled(db: &Db) -> Vec<Skill> {
    list(db).into_iter().filter(|s| s.enabled).collect()
}

pub fn get(db: &Db, id: &str) -> Option<Skill> {
    let mut s: Skill = serde_json::from_value(db.kv_get(SCOPE, id).ok()??).ok()?;
    s.id = id.to_string();
    Some(s)
}

pub fn content(db: &Db, id: &str) -> Option<Content> {
    serde_json::from_value(db.kv_get(FILES_SCOPE, id).ok()??).ok()
}

pub fn save_meta(db: &Db, s: &Skill) -> Result<()> {
    db.kv_set(SCOPE, &s.id, Some(&serde_json::to_value(s)?))
}

pub fn delete(db: &Db, id: &str) -> Result<()> {
    db.kv_set(SCOPE, id, None)?;
    db.kv_set(FILES_SCOPE, id, None)
}

/// Whether another skill than `except` already has this name (compared as ids).
pub fn name_taken(skills: &[Skill], name: &str, except: Option<&str>) -> bool {
    let id = slug(name);
    skills.iter().any(|s| Some(s.id.as_str()) != except && (s.id == id || slug(&s.name) == id))
}

/// Saves a new skill, or a new version of `old` (same id, switch and creation time).
pub fn store(db: &Db, mut skill: Skill, content: &Content, old: Option<&Skill>) -> Result<Skill, String> {
    if let Some(o) = old {
        skill.id = o.id.clone();
        skill.enabled = o.enabled;
        skill.created_at = o.created_at.clone();
    }
    let all = list(db);
    if name_taken(&all, &skill.name, old.map(|o| o.id.as_str())) || (old.is_none() && get(db, &skill.id).is_some()) {
        return Err(format!("a skill named `{}` already exists", skill.name));
    }
    let others: usize = all.iter().filter(|s| s.id != skill.id).map(|s| s.bytes).sum();
    if others + skill.bytes > MAX_TOTAL_BYTES {
        return Err(format!("skills are limited to 50 MB in total ({} MB are used)", others / (1024 * 1024)));
    }
    db.kv_set(FILES_SCOPE, &skill.id, Some(&json!(content))).map_err(|e| format!("{e:#}"))?;
    save_meta(db, &skill).map_err(|e| format!("{e:#}"))?;
    Ok(skill)
}

// ----- delivery -----------------------------------------------------------------------

fn short(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let cut: String = one_line.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

fn index_lines(skills: &[Skill], max: usize, desc_chars: usize) -> String {
    let mut out: Vec<String> =
        skills.iter().take(max).map(|s| format!("- {}: {}", s.name, short(&s.description, desc_chars))).collect();
    if skills.len() > max {
        out.push(format!("- … and {} more: call list_skills", skills.len() - max));
    }
    out.join("\n")
}

/// The part of the MCP `initialize` instructions about skills.
pub fn instructions(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    Some(format!(
        "Skills: {} folder(s) of instructions and files for specific tasks. Before a task one of them covers, \
         call load_skill with its name and follow the SKILL.md it returns; read_skill_file reads the files it \
         mentions (scripts, references). Available skills:\n{}",
        skills.len(),
        index_lines(skills, 30, 200)
    ))
}

/// The system-prompt block added to chat requests when the index is switched on.
pub fn index_prompt(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    Some(format!(
        "Skills are available through the zilong-router MCP server. When a task matches one, first load it with \
         that server's load_skill tool (by name) and follow it.\n{}",
        index_lines(skills, 50, 160)
    ))
}

static INJECTED: AtomicU64 = AtomicU64::new(0);

/// Requests that got the index since start.
pub fn injected() -> u64 {
    INJECTED.load(Ordering::Relaxed)
}

/// Adds the skills index to an upstream body when `skillsIndexEnabled` is on.
pub fn inject_index(settings: &Value, db: &Db, body: &mut Value, target: Format) {
    if settings.get(INDEX_SETTING).and_then(Value::as_bool) != Some(true) {
        return;
    }
    let Some(prompt) = index_prompt(&enabled(db)) else { return };
    crate::token_saver::inject_system(body, target, &prompt);
    INJECTED.fetch_add(1, Ordering::Relaxed);
}

fn find<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    let name = name.trim();
    skills.iter().find(|s| s.name.eq_ignore_ascii_case(name)).or_else(|| {
        let id = slug(name);
        skills.iter().find(|s| s.id == id)
    })
}

fn unknown(name: &str) -> String {
    if name.trim().is_empty() {
        return "name is required (a skill name from list_skills)".into();
    }
    format!("unknown skill `{name}`: call list_skills for the names of the enabled skills")
}

fn kb(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

/// What `load_skill` returns: the SKILL.md without its frontmatter, and the other files.
pub fn load_text(db: &Db, s: &Skill) -> Result<String, String> {
    let c = content(db, &s.id).ok_or("the files of this skill are missing; add it again in the dashboard")?;
    let mut out = format!("# Skill: {}\n\n{}", s.name, frontmatter(&c.skill_md).1.trim());
    if !c.files.is_empty() {
        out.push_str("\n\n---\nOther files of this skill (read them with read_skill_file; paths are relative to the skill folder):");
        for (path, text) in &c.files {
            out.push_str(&format!("\n- {path} ({})", kb(text.len())));
        }
    }
    Ok(out)
}

fn read_file(db: &Db, s: &Skill, path: &str) -> Result<String, String> {
    let path = path.trim().trim_start_matches("./");
    let c = content(db, &s.id).ok_or("the files of this skill are missing; add it again in the dashboard")?;
    if path == MAIN_FILE {
        return Ok(c.skill_md);
    }
    c.files.get(path).cloned().ok_or_else(|| {
        let names: Vec<&str> = c.files.keys().map(String::as_str).collect();
        let have = if names.is_empty() { "none".to_string() } else { names.join(", ") };
        format!("`{}` has no file `{path}`; its files: SKILL.md, {have}", s.name)
    })
}

fn text_result(text: String, is_error: bool) -> Value {
    let mut r = json!({"content": [{"type": "text", "text": text}]});
    if is_error {
        r["isError"] = json!(true);
    }
    r
}

/// Runs a skill tool for an MCP client; `None` when `name` is not one.
pub fn call_tool(db: &Db, name: &str, args: &Value) -> Option<Value> {
    if !TOOLS.contains(&name) {
        return None;
    }
    let skills = enabled(db);
    let arg = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default();
    let out = match name {
        "list_skills" => {
            let list: Vec<Value> = skills
                .iter()
                .map(|s| json!({"name": s.name, "description": s.description, "files": s.files.len() + 1}))
                .collect();
            let mut v = json!({"skills": list});
            if skills.is_empty() {
                v["hint"] = json!("No skill is enabled. Skills are added and switched on in the router's dashboard.");
            }
            Ok(v.to_string())
        }
        "load_skill" => find(&skills, arg("name")).ok_or_else(|| unknown(arg("name"))).and_then(|s| load_text(db, s)),
        _ => find(&skills, arg("name"))
            .ok_or_else(|| unknown(arg("name")))
            .and_then(|s| if arg("path").trim().is_empty() { Err("path is required".into()) } else { Ok(s) })
            .and_then(|s| read_file(db, s, arg("path"))),
    };
    Some(match out {
        Ok(t) => text_result(t, false),
        Err(e) => text_result(e, true),
    })
}

/// Tool definitions of `list_skills`, `load_skill` and `read_skill_file`.
pub fn tool_defs() -> Vec<Value> {
    let name = json!({"type": "string", "description": "Skill name, from list_skills."});
    vec![
        json!({
            "name": "list_skills",
            "description": "List the skills (packaged instructions and files for specific tasks) available through \
                this gateway: name and description of each.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"title": "List skills", "readOnlyHint": true}
        }),
        json!({
            "name": "load_skill",
            "description": "Load a skill before doing a task it covers: returns its SKILL.md instructions and the \
                list of its other files.",
            "inputSchema": {"type": "object", "properties": {"name": name}, "required": ["name"]},
            "annotations": {"title": "Load skill", "readOnlyHint": true}
        }),
        json!({
            "name": "read_skill_file",
            "description": "Read one file of a skill (a script, reference or template its SKILL.md mentions).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": name,
                    "path": {"type": "string", "description": "Path relative to the skill folder, e.g. scripts/fill.py."}
                },
                "required": ["name", "path"]
            },
            "annotations": {"title": "Read skill file", "readOnlyHint": true}
        }),
    ]
}

/// `prompts/list`: one prompt per enabled skill.
pub fn prompts(db: &Db) -> Value {
    let list: Vec<Value> = enabled(db)
        .iter()
        .map(|s| json!({"name": s.id, "title": s.name, "description": s.description, "arguments": []}))
        .collect();
    json!({"prompts": list})
}

/// `prompts/get`: the skill's instructions as one user message.
pub fn prompt(db: &Db, name: &str) -> Result<Value, String> {
    let skills = enabled(db);
    let s = find(&skills, name).ok_or_else(|| unknown(name))?;
    let text = load_text(db, s)?;
    Ok(json!({
        "description": s.description,
        "messages": [{"role": "user", "content": {"type": "text", "text": text}}],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let md =
            "---\nname: pdf-tools\ndescription: \"Fill PDF forms: text, checkboxes\"\nlicense: MIT\n---\n# PDF\nBody\n";
        let (f, body) = frontmatter(md);
        assert_eq!(f["name"], "pdf-tools");
        assert_eq!(f["description"], "Fill PDF forms: text, checkboxes");
        assert_eq!(f["license"], "MIT");
        assert_eq!(body, "# PDF\nBody\n");

        let folded = "---\r\nname: 'it''s'\r\ndescription: >-\r\n  Line one\r\n  line two\r\nmetadata:\r\n  version: 2\r\n---\r\nx";
        let (f, body) = frontmatter(folded);
        assert_eq!(f["name"], "it's");
        assert_eq!(f["description"], "Line one line two");
        assert_eq!(f["metadata"], "version: 2", "nested keys are kept as text, not parsed");
        assert_eq!(body, "x");

        let literal = "---\nname: a # comment\ndescription: |\n  one\n  two\n---\n";
        let (f, body) = frontmatter(literal);
        assert_eq!((f["name"].as_str(), f["description"].as_str(), body), ("a", "one\ntwo", ""));

        let plain = "description: first\n  continued\nname: x";
        assert_eq!(frontmatter(&format!("---\n{plain}\n---\n")).0["description"], "first continued");
        assert!(frontmatter("# no frontmatter\n").0.is_empty());
        assert_eq!(frontmatter("---\nname: x\nnever closed").1, "---\nname: x\nnever closed");
    }

    #[test]
    fn validates_paths() {
        for ok in ["scripts/fill.py", "reference.md", "a/b/c.txt", ".hidden/x", "LICENSE"] {
            assert!(valid_path(ok), "{ok}");
        }
        for bad in ["", "/etc/passwd", "../x", "a/../../x", "a/./b", "a//b", "a\\b", "C:/x", "a/", "a\0b", "a\nb"] {
            assert!(!valid_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn builds_and_checks_skills() {
        let md = "---\nname: PDF Tools\ndescription: Work with PDFs\n---\nUse scripts/fill.py";
        let files = BTreeMap::from([("scripts/fill.py".to_string(), "print(1)".to_string())]);
        let (s, c) = build(md, files, "paste".into(), None, None).unwrap();
        assert_eq!(
            (s.id.as_str(), s.name.as_str(), s.description.as_str()),
            ("pdf-tools", "PDF Tools", "Work with PDFs")
        );
        assert_eq!(s.files, vec![FileInfo { path: "scripts/fill.py".into(), bytes: 8 }]);
        assert_eq!(s.bytes, md.len() + 8);
        assert_eq!(c.skill_md, md);

        let (s, _) = build("just a body", BTreeMap::new(), "paste".into(), Some("n"), Some("d")).unwrap();
        assert_eq!((s.name.as_str(), s.description.as_str()), ("n", "d"));
        assert!(build("just a body", BTreeMap::new(), "paste".into(), None, None).is_err());
        assert!(build("x", BTreeMap::new(), "paste".into(), Some("n"), None).unwrap_err().contains("description"));
        let evil = BTreeMap::from([("../../etc/x".to_string(), "x".to_string())]);
        assert!(build(md, evil, "paste".into(), None, None).unwrap_err().contains("invalid file path"));
        let big = BTreeMap::from([("big.txt".to_string(), "x".repeat(MAX_SKILL_BYTES))]);
        assert!(build(md, big, "paste".into(), None, None).unwrap_err().contains("1 MB"));
        assert!(check_meta("!!!", "d").is_err());
        assert!(check_meta(&"a".repeat(65), "d").is_err());
        assert_eq!(slug("  My Skill v2! "), "my-skill-v2");
    }

    #[test]
    fn index_lists_names_and_short_descriptions() {
        let skill =
            |n: &str, d: &str| Skill { id: slug(n), name: n.into(), description: d.into(), ..Default::default() };
        assert!(index_prompt(&[]).is_none());
        let p = index_prompt(&[skill("pdf", "Fill\nforms"), skill("long", &"x".repeat(500))]).unwrap();
        assert!(p.contains("- pdf: Fill forms") && p.contains("load_skill"), "{p}");
        assert!(p.lines().all(|l| l.chars().count() < 200), "{p}");
        let s = [skill("a", "d")];
        assert_eq!(find(&s, "A").map(|s| s.id.as_str()), Some("a"));
        assert!(find(&s, "b").is_none());
    }
}
