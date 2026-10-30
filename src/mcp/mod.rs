//! MCP gateway: one MCP endpoint in front of many MCP servers, like Composio's tool
//! router. Agents connect to `/mcp` once and get a few meta tools (search the tools of
//! every server, read a tool's schema, call it) instead of hundreds of tool
//! definitions; `/mcp/all` lists every tool directly as `<server>__<tool>`.
//!
//! Server definitions live in the `kv` table (scope `mcpServers`, so the Turso mirror
//! carries them) and each server's tool list is cached under `mcpTools`, so searching
//! does not start idle servers. Stdio servers are started on first use and stopped
//! after `MCP_IDLE_SECS` without calls.

pub mod client;
pub mod server;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::db::{now_iso, Db};
use crate::state::AppState;
use client::Client;

const SCOPE: &str = "mcpServers";
const TOOLS_SCOPE: &str = "mcpTools";
/// Joins server id and tool name in `/mcp/all` and in meta-tool results. Server ids
/// cannot contain `_`, so the first `__` always ends the server id.
pub const SEP: &str = "__";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Shown instead of secret env/header values; sending it back keeps the stored value.
pub const MASK: &str = "••••••••";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Stdio,
    Http,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    /// What the server is for; shown to agents by `list_servers`.
    pub description: String,
    pub transport: Transport,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub enabled: bool,
    /// Per-call timeout; 0 means the default (120 s).
    pub timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            id: String::new(),
            name: String::new(),
            description: String::new(),
            transport: Transport::Stdio,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: String::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            enabled: true,
            timeout_secs: 0,
        }
    }
}

pub fn valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
}

/// A server id made from a display name: `GitHub Tools` -> `github-tools`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.chars().take(32).collect();
    out.trim_end_matches('-').to_string()
}

fn is_secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["key", "token", "secret", "password", "passwd", "auth", "cookie", "credential", "session"]
        .iter()
        .any(|s| k.contains(s))
}

impl ServerConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(if self.timeout_secs == 0 { DEFAULT_TIMEOUT_SECS } else { self.timeout_secs })
    }

    /// Trims and checks the fields; the error is shown in the dashboard.
    pub fn validate(&mut self) -> Result<(), String> {
        self.id = self.id.trim().to_ascii_lowercase();
        if !valid_id(&self.id) {
            return Err("id must be 1-32 characters: a-z, 0-9 and -".into());
        }
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            self.name = self.id.clone();
        }
        self.command = self.command.trim().to_string();
        self.url = self.url.trim().to_string();
        self.args.retain(|a| !a.is_empty());
        self.env.retain(|k, _| !k.trim().is_empty());
        self.headers.retain(|k, _| !k.trim().is_empty());
        match self.transport {
            Transport::Stdio if self.command.is_empty() => Err("a command is required for a stdio server".into()),
            Transport::Http if !(self.url.starts_with("http://") || self.url.starts_with("https://")) => {
                Err("the URL must start with http:// or https://".into())
            }
            _ => Ok(()),
        }
    }

    /// Copy for the dashboard with secret-looking env and header values hidden.
    pub fn masked(&self) -> ServerConfig {
        let mut c = self.clone();
        for map in [&mut c.env, &mut c.headers] {
            for (k, v) in map.iter_mut() {
                if is_secret(k) && !v.is_empty() {
                    *v = MASK.to_string();
                }
            }
        }
        c
    }

    /// Puts back the stored values the dashboard sent as [`MASK`].
    pub fn unmask(&mut self, old: &ServerConfig) {
        for (map, old_map) in [(&mut self.env, &old.env), (&mut self.headers, &old.headers)] {
            for (k, v) in map.iter_mut() {
                if v == MASK {
                    *v = old_map.get(k).cloned().unwrap_or_default();
                }
            }
        }
    }

    /// Whether `other` reaches a different server (so cached tools no longer apply).
    pub fn same_target(&self, other: &ServerConfig) -> bool {
        (self.transport, &self.command, &self.args, &self.env, &self.cwd, &self.url, &self.headers)
            == (other.transport, &other.command, &other.args, &other.env, &other.cwd, &other.url, &other.headers)
    }
}

// ----- storage ----------------------------------------------------------------------

pub fn servers(db: &Db) -> Vec<ServerConfig> {
    let mut list: Vec<ServerConfig> = db
        .kv_scope(SCOPE)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(id, v)| {
            let mut c: ServerConfig = serde_json::from_value(v).ok()?;
            c.id = id;
            Some(c)
        })
        .collect();
    list.sort_by(|a, b| a.id.cmp(&b.id));
    list
}

pub fn server(db: &Db, id: &str) -> Option<ServerConfig> {
    servers(db).into_iter().find(|s| s.id == id)
}

/// Saves `cfg`, dropping its cached tool list when it now points somewhere else.
pub fn save_server(db: &Db, cfg: &ServerConfig) -> Result<()> {
    if server(db, &cfg.id).is_some_and(|old| !old.same_target(cfg)) {
        db.kv_set(TOOLS_SCOPE, &cfg.id, None)?;
    }
    db.kv_set(SCOPE, &cfg.id, Some(&serde_json::to_value(cfg)?))
}

pub fn delete_server(db: &Db, id: &str) -> Result<()> {
    db.kv_set(SCOPE, id, None)?;
    db.kv_set(TOOLS_SCOPE, id, None)
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Catalog {
    pub fetched_at: String,
    pub tools: Vec<Value>,
}

pub fn cached_tools(db: &Db, id: &str) -> Option<Catalog> {
    db.kv_get(TOOLS_SCOPE, id).ok().flatten().and_then(|v| serde_json::from_value(v).ok())
}

/// `mcpServers` JSON as Claude Desktop, Cursor, Windsurf and VS Code (`servers`) write
/// it, or a bare `{name: {...}}` map, as server configs.
pub fn parse_import(v: &Value) -> Result<Vec<ServerConfig>, String> {
    let map = v
        .get("mcpServers")
        .or_else(|| v.get("servers"))
        .or_else(|| v.get("mcp").and_then(|m| m.get("servers")))
        .unwrap_or(v)
        .as_object()
        .ok_or("expected an object like {\"mcpServers\": {\"name\": {\"command\": ...}}}")?;
    let strings = |v: Option<&Value>| -> BTreeMap<String, String> {
        v.and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for (name, s) in map {
        let text = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let url = ["url", "serverUrl", "httpUrl"].into_iter().map(&text).find(|u| !u.is_empty());
        let mut cfg = ServerConfig {
            id: slug(name),
            name: name.clone(),
            description: text("description"),
            enabled: s.get("disabled").and_then(Value::as_bool) != Some(true),
            ..Default::default()
        };
        match url {
            Some(url) => {
                cfg.transport = Transport::Http;
                cfg.url = url;
                cfg.headers = strings(s.get("headers"));
            }
            None => {
                cfg.command = text("command");
                cfg.args = s
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                cfg.env = strings(s.get("env"));
                cfg.cwd = text("cwd");
            }
        }
        cfg.validate().map_err(|e| format!("{name}: {e}"))?;
        out.push(cfg);
    }
    Ok(out)
}

// ----- search -----------------------------------------------------------------------

const STOP_WORDS: [&str; 16] =
    ["a", "an", "the", "to", "for", "of", "in", "on", "and", "or", "with", "my", "me", "i", "tool", "tools"];

fn stem(word: &str) -> &str {
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
        &word[..word.len() - 1]
    } else {
        word
    }
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !STOP_WORDS.contains(w))
        .map(|w| stem(w).to_string())
        .collect()
}

pub fn tool_name(tool: &Value) -> &str {
    tool.get("name").and_then(Value::as_str).unwrap_or_default()
}

pub fn tool_description(tool: &Value) -> &str {
    tool.get("description").and_then(Value::as_str).unwrap_or_default()
}

/// Keyword match over tool names (weighted most), server ids and descriptions; an
/// empty query lists everything. Best first.
pub fn search<'a>(catalogs: &'a [(String, Vec<Value>)], query: &str, limit: usize) -> Vec<(&'a str, &'a Value)> {
    let terms = words(query);
    let mut hits: Vec<(u32, &str, &Value)> = Vec::new();
    for (server, tools) in catalogs {
        for tool in tools {
            let name = tool_name(tool).to_lowercase();
            let name_words = words(&name);
            let desc = tool_description(tool).to_lowercase();
            let mut score = 0;
            for t in &terms {
                score += if name_words.contains(t) {
                    6
                } else if name.contains(t.as_str()) {
                    4
                } else if server.contains(t.as_str()) {
                    2
                } else if desc.contains(t.as_str()) {
                    1
                } else {
                    0
                };
            }
            if terms.is_empty() || score > 0 {
                hits.push((score, server.as_str(), tool));
            }
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)).then(tool_name(a.2).cmp(tool_name(b.2))));
    hits.into_iter().take(limit).map(|(_, s, t)| (s, t)).collect()
}

/// `path: string, recursive?: boolean`: the parameters of an input schema in one line.
pub fn params_summary(schema: &Value) -> String {
    let required: HashSet<&str> =
        schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else { return String::new() };
    props
        .iter()
        .map(|(name, p)| {
            let ty = match p.get("type") {
                Some(Value::String(t)) => t.clone(),
                Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("|"),
                _ if p.get("enum").is_some() => "enum".into(),
                _ => "any".into(),
            };
            format!("{name}{}: {ty}", if required.contains(name.as_str()) { "" } else { "?" })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ----- running servers ----------------------------------------------------------------

struct Live {
    client: Client,
    last_used: Mutex<Instant>,
    inflight: AtomicUsize,
}

/// Marks a server busy for the reaper while a request is running.
struct Busy<'a>(&'a Live);

impl Live {
    fn busy(&self) -> Busy<'_> {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        Busy(self)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        *self.0.last_used.lock() = Instant::now();
        self.0.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

type Slot = Arc<tokio::sync::Mutex<Option<Arc<Live>>>>;

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub calls: u64,
    pub errors: u64,
    pub total_ms: u64,
    pub last_call_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Default)]
pub struct Gateway {
    slots: Mutex<HashMap<String, Slot>>,
    stats: Mutex<HashMap<String, Stats>>,
}

impl Gateway {
    fn slot(&self, id: &str) -> Slot {
        self.slots.lock().entry(id.to_string()).or_default().clone()
    }

    fn record(&self, id: &str, ms: u64, error: Option<String>, call: bool) {
        let mut stats = self.stats.lock();
        let s = stats.entry(id.to_string()).or_default();
        if call {
            s.calls += 1;
            s.total_ms += ms;
            s.last_call_at = Some(now_iso());
        }
        if let Some(e) = error {
            s.errors += 1;
            s.last_error = Some(e);
        }
    }

    pub fn stats(&self, id: &str) -> Stats {
        self.stats.lock().get(id).cloned().unwrap_or_default()
    }

    /// `running`, `stopped`, or `busy` while it is starting or being stopped.
    pub fn status(&self, id: &str) -> &'static str {
        let Some(slot) = self.slots.lock().get(id).cloned() else { return "stopped" };
        let Ok(g) = slot.try_lock() else { return "busy" };
        match g.as_ref() {
            Some(l) if l.client.alive() => "running",
            _ => "stopped",
        }
    }

    pub fn stderr_tail(&self, id: &str) -> Vec<String> {
        let Some(slot) = self.slots.lock().get(id).cloned() else { return Vec::new() };
        let Ok(g) = slot.try_lock() else { return Vec::new() };
        g.as_ref().map(|l| l.client.stderr_tail()).unwrap_or_default()
    }

    /// The running connection to `cfg`, started if needed.
    async fn live(&self, state: &AppState, cfg: &ServerConfig) -> Result<Arc<Live>> {
        let slot = self.slot(&cfg.id);
        let mut guard = slot.lock().await;
        if let Some(l) = guard.as_ref().filter(|l| l.client.alive()) {
            return Ok(l.clone());
        }
        let started = Instant::now();
        let client = match Client::connect(cfg, &state.http).await {
            Ok(c) => c,
            Err(e) => {
                let e = format!("{e:#}");
                tracing::warn!("mcp {}: cannot start: {e}", cfg.id);
                self.record(&cfg.id, 0, Some(e.clone()), false);
                anyhow::bail!(e);
            }
        };
        tracing::info!("mcp {}: connected in {} ms", cfg.id, started.elapsed().as_millis());
        let live = Arc::new(Live { client, last_used: Mutex::new(Instant::now()), inflight: AtomicUsize::new(0) });
        *guard = Some(live.clone());
        Ok(live)
    }

    /// Stops a running server (kills the process / ends the HTTP session).
    pub async fn stop(&self, id: &str) {
        let slot = self.slots.lock().get(id).cloned();
        if let Some(slot) = slot {
            if slot.lock().await.take().is_some() {
                tracing::info!("mcp {id}: stopped");
            }
        }
    }

    pub async fn forget(&self, id: &str) {
        self.stop(id).await;
        self.slots.lock().remove(id);
        self.stats.lock().remove(id);
    }

    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.slots.lock().keys().cloned().collect();
        for id in ids {
            self.stop(&id).await;
        }
    }

    fn tools_changed(&self, id: &str) -> bool {
        let Some(slot) = self.slots.lock().get(id).cloned() else { return false };
        let Ok(g) = slot.try_lock() else { return false };
        g.as_ref().is_some_and(|l| l.client.take_tools_changed())
    }

    /// The tools of one server: the cached list, unless `refresh` is set, there is
    /// none yet, or the server announced that its list changed.
    pub async fn tools(&self, state: &AppState, cfg: &ServerConfig, refresh: bool) -> Result<Vec<Value>> {
        if !refresh && !self.tools_changed(&cfg.id) {
            if let Some(c) = cached_tools(&state.db, &cfg.id) {
                return Ok(c.tools);
            }
        }
        let live = self.live(state, cfg).await?;
        let _busy = live.busy();
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = live.client.request("tools/list", params, cfg.timeout()).await.map_err(|e| {
                self.record(&cfg.id, 0, Some(format!("tools/list: {e:#}")), false);
                e
            })?;
            tools.extend(page.get("tools").and_then(Value::as_array).cloned().unwrap_or_default());
            cursor = page.get("nextCursor").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        let catalog = Catalog { fetched_at: now_iso(), tools };
        state.db.kv_set(TOOLS_SCOPE, &cfg.id, Some(&serde_json::to_value(&catalog)?))?;
        tracing::info!("mcp {}: {} tools", cfg.id, catalog.tools.len());
        Ok(catalog.tools)
    }

    /// `tools/call` on the server; the result is the server's `CallToolResult`.
    pub async fn call(&self, state: &AppState, cfg: &ServerConfig, tool: &str, args: Value) -> Result<Value> {
        let started = Instant::now();
        let result = async {
            let live = self.live(state, cfg).await?;
            let _busy = live.busy();
            live.client.request("tools/call", json!({"name": tool, "arguments": args}), cfg.timeout()).await
        }
        .await;
        let ms = started.elapsed().as_millis() as u64;
        let failed = match &result {
            Ok(r) if r.get("isError").and_then(Value::as_bool) == Some(true) => {
                let text = r["content"]
                    .as_array()
                    .and_then(|c| c.iter().find_map(|p| p.get("text").and_then(Value::as_str)))
                    .unwrap_or("tool reported an error");
                Some(text.chars().take(300).collect::<String>())
            }
            Ok(_) => None,
            Err(e) => Some(format!("{e:#}")),
        };
        match &failed {
            None => tracing::info!("mcp {}{SEP}{tool}: ok in {ms} ms", cfg.id),
            Some(e) => tracing::warn!("mcp {}{SEP}{tool}: failed in {ms} ms: {e}", cfg.id),
        }
        self.record(&cfg.id, ms, failed, true);
        result
    }

    /// Stops servers that have been idle for `idle`.
    async fn reap(&self, idle: Duration) {
        let slots: Vec<(String, Slot)> = self.slots.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (id, slot) in slots {
            let Ok(mut g) = slot.try_lock() else { continue };
            let stale = g.as_ref().is_some_and(|l| {
                l.inflight.load(Ordering::Relaxed) == 0 && (l.last_used.lock().elapsed() >= idle || !l.client.alive())
            });
            if stale {
                g.take();
                tracing::info!("mcp {id}: stopped after {} s idle", idle.as_secs());
            }
        }
    }
}

/// Background task: stops idle MCP servers so they don't hold memory on a small board.
pub async fn reaper(state: Arc<AppState>) {
    if state.cfg.mcp_idle_secs == 0 {
        return;
    }
    let idle = Duration::from_secs(state.cfg.mcp_idle_secs);
    let mut tick = tokio::time::interval((idle / 2).clamp(Duration::from_secs(1), Duration::from_secs(30)));
    loop {
        tick.tick().await;
        state.mcp.reap(idle).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, desc: &str) -> Value {
        json!({"name": name, "description": desc, "inputSchema": {"type": "object"}})
    }

    #[test]
    fn search_ranks_names_first() {
        let github = vec![tool("create_issue", "Create a new issue"), tool("list_repos", "List repositories")];
        let files = vec![tool("read_file", "Read a file; mentions issues"), tool("write_file", "Write")];
        let catalogs = vec![("github".to_string(), github), ("files".to_string(), files)];
        let hits = search(&catalogs, "create issues", 10);
        assert_eq!(tool_name(hits[0].1), "create_issue");
        assert_eq!(hits.len(), 2, "read_file matches on its description");
        assert_eq!(search(&catalogs, "", 3).len(), 3);
        assert_eq!(search(&catalogs, "github", 10).len(), 2, "server id matches");
        assert!(search(&catalogs, "zebra", 10).is_empty());
    }

    #[test]
    fn summarises_params() {
        let props =
            json!({"path": {"type": "string"}, "depth": {"type": ["integer", "null"]}, "mode": {"enum": ["a"]}});
        let schema = json!({"type": "object", "required": ["path"], "properties": props});
        assert_eq!(params_summary(&schema), "path: string, depth?: integer|null, mode?: enum");
    }

    #[test]
    fn imports_desktop_configs() {
        let v = json!({"mcpServers": {
            "File System": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]},
            "remote": {"url": "https://example.com/mcp", "headers": {"Authorization": "Bearer x"}, "disabled": true},
        }});
        let list = parse_import(&v).unwrap();
        assert_eq!(list[0].id, "file-system");
        assert_eq!(list[0].args.len(), 3);
        assert_eq!(list[1].transport, Transport::Http);
        assert!(!list[1].enabled);
        assert!(parse_import(&json!({"x": {"command": ""}})).is_err());
    }

    #[test]
    fn masks_secrets_and_restores_them() {
        let mut c = ServerConfig { id: "gh".into(), command: "x".into(), ..Default::default() };
        c.env.insert("GITHUB_TOKEN".into(), "ghp_1".into());
        c.env.insert("PATH".into(), "/bin".into());
        let mut m = c.masked();
        assert_eq!(m.env["GITHUB_TOKEN"], MASK);
        assert_eq!(m.env["PATH"], "/bin");
        m.unmask(&c);
        assert_eq!(m, c);
    }

    #[test]
    fn ids() {
        assert_eq!(slug("GitHub Tools!"), "github-tools");
        assert!(valid_id("a-1") && !valid_id("-a") && !valid_id("a_b") && !valid_id(""));
    }
}
