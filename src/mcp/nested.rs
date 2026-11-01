//! Nested gateways: upstream MCP servers that are themselves meta-tool gateways, such as
//! Composio (`COMPOSIO_SEARCH_TOOLS`, `COMPOSIO_MULTI_EXECUTE_TOOL`, …) or another
//! zilong-router (`search_tools`, `call_tool`). Going through two layers of meta tools
//! loses small models, so `/mcp` flattens them: its `search_tools` also runs the
//! upstream's search and returns the apps' tools as `<server>__<SLUG>`, and `call_tool`
//! on such a name runs the upstream's execute tool. The upstream's own search, schema
//! and execute tools are hidden from search results.
//!
//! An adapter per kind of gateway builds the upstream requests and reads the answers;
//! [`Cache`] keeps the tools that searches found (for `get_tool_schemas`) and Composio's
//! session ids.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use super::{params_summary, tool_error, tool_name, Nested, ServerConfig, SEP};

/// How long a nested gateway's session id is reused.
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
/// How long a tool found by a search stays known.
const TOOL_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_TOOLS: usize = 400;
const MAX_SESSIONS: usize = 200;
/// Tools without a schema whose schemas one search fetches in a single batch.
pub const SCHEMA_BATCH: usize = 5;

const COMPOSIO_SEARCH: &str = "COMPOSIO_SEARCH_TOOLS";
const COMPOSIO_SCHEMAS: &str = "COMPOSIO_GET_TOOL_SCHEMAS";
const COMPOSIO_EXECUTE: &str = "COMPOSIO_MULTI_EXECUTE_TOOL";
pub const COMPOSIO_CONNECT: &str = "COMPOSIO_MANAGE_CONNECTIONS";

/// How to talk to one kind of nested gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adapter {
    /// Composio's tool router.
    Composio,
    /// A gateway like this one: `search_tools` (`query`) and `call_tool` naming the tool
    /// with `name_key` (`tool` or `name`) next to `arguments`.
    Gateway { name_key: &'static str, schemas: bool },
}

/// The adapter for a server with these tools, unless flattening is off for it.
pub fn detect(cfg: &ServerConfig, tools: &[Value]) -> Option<Adapter> {
    if cfg.nested == Nested::Off {
        return None;
    }
    detect_tools(tools)
}

pub fn detect_tools(tools: &[Value]) -> Option<Adapter> {
    let find = |n: &str| tools.iter().find(|t| tool_name(t) == n);
    if find(COMPOSIO_SEARCH).is_some() && find(COMPOSIO_EXECUTE).is_some() {
        return Some(Adapter::Composio);
    }
    find("search_tools")?;
    let call = find("call_tool")?;
    let props = call.get("inputSchema").and_then(|s| s.get("properties")).and_then(Value::as_object)?;
    if !props.contains_key("arguments") {
        return None;
    }
    let name_key = ["tool", "name"].into_iter().find(|k| props.contains_key(*k))?;
    Some(Adapter::Gateway { name_key, schemas: find("get_tool_schemas").is_some() })
}

/// A tool found through a nested gateway.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VTool {
    /// Its name on the nested gateway (`GITHUB_LIST_REPOSITORY_ISSUES`).
    pub slug: String,
    pub description: String,
    pub schema: Option<Value>,
    /// A ready-made parameter summary, from gateways that send one instead of a schema.
    pub summary: Option<String>,
    /// The service it belongs to (`github`): Composio's toolkit, or the server part of
    /// another gateway's `server__tool`.
    pub service: Option<String>,
}

impl VTool {
    pub fn parameters(&self) -> Option<String> {
        self.schema.as_ref().map(params_summary).or_else(|| self.summary.clone())
    }
}

/// What one upstream search found: the tools (the first `primary` are the best matches),
/// short notes for the agent, and the session to reuse.
#[derive(Debug, Default)]
pub struct Found {
    pub tools: Vec<VTool>,
    pub primary: usize,
    pub notes: Vec<String>,
    pub session: Option<String>,
}

impl Adapter {
    pub fn label(self) -> &'static str {
        match self {
            Adapter::Composio => "Composio",
            Adapter::Gateway { .. } => "MCP gateway",
        }
    }

    /// The upstream's own search, schema and execute tools, hidden from search results.
    pub fn hides(self, tool: &str) -> bool {
        match self {
            Adapter::Composio => [COMPOSIO_SEARCH, COMPOSIO_SCHEMAS, COMPOSIO_EXECUTE].contains(&tool),
            Adapter::Gateway { .. } => ["search_tools", "get_tool_schemas", "call_tool"].contains(&tool),
        }
    }

    /// The upstream tool and arguments that search for `query`.
    pub fn search_request(self, query: &str, session: Option<&str>) -> (&'static str, Value) {
        match self {
            Adapter::Composio => {
                let session = match session {
                    Some(id) => json!({"id": id}),
                    None => json!({"generate_id": true}),
                };
                let args = json!({"queries": [{"use_case": query}], "session": session, "search_strategy": "auto"});
                (COMPOSIO_SEARCH, args)
            }
            Adapter::Gateway { .. } => ("search_tools", json!({"query": query})),
        }
    }

    /// The upstream tool and arguments that return the schemas of `slugs`, if it has one.
    pub fn schemas_request(self, slugs: &[String], session: Option<&str>) -> Option<(&'static str, Value)> {
        match self {
            Adapter::Composio => {
                let mut args = json!({"tool_slugs": slugs});
                if let Some(id) = session {
                    args["session_id"] = json!(id);
                }
                Some((COMPOSIO_SCHEMAS, args))
            }
            Adapter::Gateway { schemas: true, .. } => Some(("get_tool_schemas", json!({"tools": slugs}))),
            Adapter::Gateway { .. } => None,
        }
    }

    /// The upstream tool and arguments that run `slug` with `arguments`.
    pub fn exec_request(self, slug: &str, arguments: Value, session: Option<&str>) -> (&'static str, Value) {
        match self {
            Adapter::Composio => {
                let mut args = json!({
                    "tools": [{"tool_slug": slug, "arguments": arguments}],
                    "sync_response_to_workbench": false,
                });
                if let Some(id) = session {
                    args["session_id"] = json!(id);
                }
                (COMPOSIO_EXECUTE, args)
            }
            Adapter::Gateway { name_key, .. } => {
                let mut args = Map::new();
                args.insert(name_key.to_string(), json!(slug));
                args.insert("arguments".to_string(), arguments);
                ("call_tool", Value::Object(args))
            }
        }
    }

    /// Reads a search result of the upstream (`server` is its id, for the notes).
    pub fn parse_search(self, server: &str, result: &Value) -> Result<Found, String> {
        if let Some(e) = tool_error(result) {
            return Err(e);
        }
        let v = result_json(result).ok_or("the search result is not JSON")?;
        match self {
            Adapter::Composio => parse_composio_search(server, &v),
            Adapter::Gateway { .. } => {
                let list =
                    v.get("results").or_else(|| v.get("tools")).and_then(Value::as_array).or_else(|| v.as_array());
                let tools: Vec<VTool> = list
                    .into_iter()
                    .flatten()
                    .filter_map(|t| vtool(t, None))
                    .map(|mut t| {
                        t.service = t.slug.split_once(SEP).map(|(s, _)| s.to_lowercase());
                        t
                    })
                    .collect();
                Ok(Found { primary: tools.len(), tools, ..Default::default() })
            }
        }
    }

    /// Reads a schemas result of the upstream.
    pub fn parse_schemas(self, result: &Value) -> Result<Vec<VTool>, String> {
        if let Some(e) = tool_error(result) {
            return Err(e);
        }
        let v = result_json(result).ok_or("the schemas result is not JSON")?;
        if v.get("successful").and_then(Value::as_bool) == Some(false) {
            return Err(error_text(&v));
        }
        Ok(schema_entries(&v))
    }

    /// The result of an execute call as the tool's own result: Composio wraps it in a
    /// per-tool list, which is removed when exactly one tool ran and the list is clearly
    /// that. Anything else is passed through unchanged.
    pub fn unwrap_exec(self, slug: &str, result: Value) -> Value {
        if self != Adapter::Composio || tool_error(&result).is_some() {
            return result;
        }
        let Some(v) = result_json(&result) else { return result };
        let data = v.get("data").filter(|d| d.is_object()).unwrap_or(&v);
        let results = data.get("results").and_then(Value::as_array);
        let Some([one]) = results.map(Vec::as_slice) else {
            if results.is_none() && v.get("successful").and_then(Value::as_bool) == Some(false) {
                return error_result(error_text(&v));
            }
            return result;
        };
        if one.get("tool_slug").and_then(Value::as_str).is_some_and(|s| !s.eq_ignore_ascii_case(slug)) {
            return result;
        }
        let inner = one.get("response").filter(|r| r.is_object()).unwrap_or(one);
        if !["data", "error", "successful"].iter().any(|k| inner.get(*k).is_some()) {
            return result;
        }
        let error = inner.get("error").filter(|e| !e.is_null() && e.as_str() != Some(""));
        let failed = match inner.get("successful").and_then(Value::as_bool) {
            Some(ok) => !ok,
            None => error.is_some(),
        };
        let body = if failed { error.unwrap_or(inner) } else { inner.get("data").unwrap_or(inner) };
        let text = match body {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if failed {
            error_result(text)
        } else {
            json!({"content": [{"type": "text", "text": text}]})
        }
    }
}

fn error_result(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

/// The error of a `{successful: false, error: …}` answer.
fn error_text(v: &Value) -> String {
    match v.get("error") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(e) if !e.is_null() => e.to_string(),
        _ => "the nested gateway reported a failure".to_string(),
    }
}

/// The JSON in a tool result: its first text block parsed, or `structuredContent`.
pub fn result_json(result: &Value) -> Option<Value> {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|c| c.iter().find_map(|p| p.get("text").and_then(Value::as_str)));
    text.and_then(|t| serde_json::from_str::<Value>(t).ok())
        .filter(|v| v.is_object() || v.is_array())
        .or_else(|| result.get("structuredContent").filter(|v| v.is_object()).cloned())
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

fn parse_composio_search(server: &str, v: &Value) -> Result<Found, String> {
    if v.get("successful").and_then(Value::as_bool) == Some(false) {
        return Err(error_text(v));
    }
    let data = v.get("data").filter(|d| d.is_object()).unwrap_or(v);
    let (mut primary, mut related, mut notes) = (Vec::<String>::new(), Vec::<String>::new(), Vec::<String>::new());
    let mut services: HashMap<String, String> = HashMap::new();
    for r in data.get("results").and_then(Value::as_array).into_iter().flatten() {
        let toolkits: Vec<String> = strings(r.get("toolkits")).iter().map(|t| t.to_lowercase()).collect();
        let (first, more) = (strings(r.get("primary_tool_slugs")), strings(r.get("related_tool_slugs")));
        for (slug, is_primary) in first.into_iter().map(|s| (s, true)).chain(more.into_iter().map(|s| (s, false))) {
            let lower = slug.to_lowercase();
            let service = match toolkits.as_slice() {
                [one] => Some(one.clone()),
                many => many.iter().find(|t| lower.starts_with(&format!("{t}_"))).cloned(),
            };
            if let Some(s) = service {
                services.entry(slug.clone()).or_insert(s);
            }
            let list = if is_primary { &mut primary } else { &mut related };
            if !list.contains(&slug) {
                list.push(slug);
            }
        }
        // Composio's advice, when it says more than "found N tools".
        if let Some(g) = r.get("execution_guidance").and_then(Value::as_str) {
            if g.len() <= 200 && !g.starts_with("Found ") && !notes.iter().any(|n| n == g) {
                notes.push(g.to_string());
            }
        }
    }
    related.retain(|s| !primary.contains(s));
    let mut schemas: HashMap<String, VTool> = HashMap::new();
    if let Some(s) = data.get("tool_schemas") {
        for t in schema_entries(s) {
            schemas.insert(t.slug.clone(), t);
        }
    }
    let count = primary.len();
    let tools = primary
        .into_iter()
        .chain(related)
        .map(|slug| {
            let mut t = schemas.remove(&slug).unwrap_or_else(|| VTool { slug: slug.clone(), ..Default::default() });
            if t.service.is_none() {
                t.service = services.get(&slug).cloned();
            }
            t
        })
        .collect();
    let missing: Vec<String> = data
        .get("toolkit_connection_statuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| t.get("has_active_connection").and_then(Value::as_bool) == Some(false))
        .filter_map(|t| {
            let name = t.get("toolkit").and_then(Value::as_str)?;
            let status = t.get("status_message").and_then(Value::as_str).unwrap_or_default();
            Some(if status.is_empty() || status.len() > 80 { name.to_string() } else { format!("{name} ({status})") })
        })
        .collect();
    if !missing.is_empty() {
        notes.push(format!(
            "Not connected: {}. Call {server}{SEP}{COMPOSIO_CONNECT} with the toolkit name to connect, then retry.",
            missing.join(", ")
        ));
    }
    let session = data.get("session").and_then(|s| s.get("id")).and_then(Value::as_str).map(str::to_string);
    Ok(Found { tools, primary: count, notes, session })
}

/// A tool entry of a search or schema answer: named by `tool_slug`, `slug`, `tool` or
/// `name` (else `key`, its key in a map), with a schema when it has one.
fn vtool(v: &Value, key: Option<&str>) -> Option<VTool> {
    let slug = ["tool_slug", "slug", "tool", "name"].iter().find_map(|k| v.get(*k).and_then(Value::as_str)).or(key)?;
    let schema = ["input_schema", "inputSchema", "input_parameters", "parameters"]
        .iter()
        .find_map(|k| v.get(*k).filter(|s| s.is_object()))
        .cloned();
    Some(VTool {
        slug: slug.to_string(),
        description: v.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
        schema,
        summary: v.get("parameters").and_then(Value::as_str).map(str::to_string),
        service: v.get("toolkit").and_then(Value::as_str).map(str::to_lowercase),
    })
}

/// Every tool entry in a schema answer, whether a list or a map keyed by slug.
pub fn schema_entries(v: &Value) -> Vec<VTool> {
    let mut out = Vec::new();
    walk(v, None, &mut out, 0);
    out
}

fn walk(v: &Value, key: Option<&str>, out: &mut Vec<VTool>, depth: usize) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Array(items) => items.iter().for_each(|i| walk(i, None, out, depth + 1)),
        Value::Object(o) => {
            let is_tool = ["input_schema", "inputSchema", "input_parameters", "schemaRef", "hasFullSchema"]
                .iter()
                .any(|k| o.contains_key(*k));
            if is_tool {
                out.extend(vtool(v, key));
                return;
            }
            for (k, child) in o {
                walk(child, Some(k.as_str()), out, depth + 1);
            }
        }
        _ => {}
    }
}

/// `GitHub Tools` -> `githubtools`.
fn norm(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// Whether the direct server `cfg` is for `service` (a nested gateway's toolkit): its
/// `service` setting when set, else its id or name equal to or starting with it.
pub fn serves(cfg: &ServerConfig, service: &str) -> bool {
    let svc = norm(service);
    if svc.is_empty() {
        return false;
    }
    if !cfg.service.trim().is_empty() {
        return norm(&cfg.service) == svc;
    }
    [cfg.id.as_str(), cfg.name.as_str()].into_iter().map(norm).any(|n| n.starts_with(&svc))
}

/// Whether `args` fit `schema` well enough to run without asking: every required
/// property is there, and no name is unknown unless the schema allows extra ones.
pub fn args_fit(schema: &Value, args: &Value) -> bool {
    let Some(args) = args.as_object() else { return false };
    let empty = Map::new();
    let props = schema.get("properties").and_then(Value::as_object).unwrap_or(&empty);
    let required = schema.get("required").and_then(Value::as_array).into_iter().flatten();
    if !required.filter_map(Value::as_str).all(|r| args.contains_key(r)) {
        return false;
    }
    let open = matches!(schema.get("additionalProperties"), Some(Value::Bool(true)) | Some(Value::Object(_)));
    open || args.keys().all(|k| props.contains_key(k))
}

/// Tools found by nested searches, by their `/mcp` name (`server__SLUG`), and session
/// ids by server and caller. Bounded and expiring.
#[derive(Default)]
pub struct Cache {
    tools: Mutex<HashMap<String, (VTool, Instant)>>,
    sessions: Mutex<HashMap<String, (String, Instant)>>,
}

impl Cache {
    pub fn remember(&self, server: &str, tools: &[VTool]) {
        let mut map = self.tools.lock();
        map.retain(|_, (_, at)| at.elapsed() < TOOL_TTL);
        for t in tools {
            let key = format!("{server}{SEP}{}", t.slug);
            let mut t = t.clone();
            // A search that only names a tool keeps the schema an earlier one found.
            if let Some((old, _)) = map.get(&key) {
                if t.schema.is_none() {
                    t.schema = old.schema.clone();
                }
                if t.description.is_empty() {
                    t.description = old.description.clone();
                }
                if t.service.is_none() {
                    t.service = old.service.clone();
                }
            }
            map.insert(key, (t, Instant::now()));
        }
        while map.len() > MAX_TOOLS {
            let Some(oldest) = map.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k.clone()) else { break };
            map.remove(&oldest);
        }
    }

    /// A known tool by its `/mcp` name.
    pub fn tool(&self, name: &str) -> Option<VTool> {
        let mut map = self.tools.lock();
        match map.get_mut(name) {
            Some((t, at)) if at.elapsed() < TOOL_TTL => {
                *at = Instant::now();
                Some(t.clone())
            }
            _ => None,
        }
    }

    /// The servers on which a search found a tool named `slug`.
    pub fn owners(&self, slug: &str) -> Vec<String> {
        let map = self.tools.lock();
        let mut out: Vec<String> = map
            .iter()
            .filter(|(_, (t, at))| t.slug == slug && at.elapsed() < TOOL_TTL)
            .filter_map(|(k, _)| k.split_once(SEP).map(|(s, _)| s.to_string()))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn session(&self, key: &str) -> Option<String> {
        let map = self.sessions.lock();
        map.get(key).filter(|(_, at)| at.elapsed() < SESSION_TTL).map(|(id, _)| id.clone())
    }

    pub fn set_session(&self, key: &str, id: Option<String>) {
        let mut map = self.sessions.lock();
        let Some(id) = id else {
            map.remove(key);
            return;
        };
        if map.len() >= MAX_SESSIONS {
            map.retain(|_, (_, at)| at.elapsed() < SESSION_TTL);
            if map.len() >= MAX_SESSIONS {
                map.clear();
            }
        }
        map.insert(key.to_string(), (id, Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A COMPOSIO_SEARCH_TOOLS answer (trimmed from a real one).
    const SAMPLE: &str = r#"{"successful":true,"data":{"results":[{"index":1,"use_case":"list github issues in a repository","execution_guidance":"Found 4 tools for this query. Look at the schemas to know how to use them.","primary_tool_slugs":["GITHUB_LIST_REPOSITORY_ISSUES"],"related_tool_slugs":["GITHUB_GET_AN_ISSUE","GITHUB_SEARCH_ISSUES","GITHUB_UPDATE_AN_ISSUE"],"toolkits":["github"]}],"toolkit_connection_statuses":[{"toolkit":"github","description":"GitHub is a code hosting platform ...","has_active_connection":true,"accounts":[{"id":"github_belis-neve","status":"ACTIVE","is_default":true}],"status_message":"Connection is active and ready to use"}],"tool_schemas":{"GITHUB_LIST_REPOSITORY_ISSUES":{"toolkit":"GITHUB","tool_slug":"GITHUB_LIST_REPOSITORY_ISSUES","description":"Lists issues (including pull requests) from a GitHub repository. Requires BOTH 'owner' AND 'repo' parameters.","input_schema":{"type":"object","required":["owner","repo"],"properties":{"page":{"type":"integer","default":1},"repo":{"type":"string","description":"REQUIRED. The name of the repository"},"owner":{"type":"string","description":"REQUIRED. The GitHub account owner"},"state":{"enum":["open","closed","all"],"type":"string","default":"open"}}},"hasFullSchema":true},"GITHUB_GET_AN_ISSUE":{"toolkit":"GITHUB","tool_slug":"GITHUB_GET_AN_ISSUE","description":"Retrieves detailed information about a specific issue.","input_schema":{"type":"object","required":["owner","repo","issue_number"],"properties":{"repo":{"type":"string"},"owner":{"type":"string"},"issue_number":{"type":"integer"}}},"hasFullSchema":true},"GITHUB_UPDATE_AN_ISSUE":{"toolkit":"GITHUB","tool_slug":"GITHUB_UPDATE_AN_ISSUE","description":"Updates an existing GitHub issue's title, body, state, milestone, labels, or assignees; `state_reaso...","hasFullSchema":false,"schemaRef":{"tool":"COMPOSIO_GET_TOOL_SCHEMAS","args":{"tool_slugs":["GITHUB_UPDATE_AN_ISSUE"]}}}},"time_info":{"current_time_utc":"2026-11-01T03:06:17.978Z"},"session":{"id":"paid","generate_id":true,"instructions":"REQUIRED: Pass session_id \"paid\" in ALL subsequent meta tool calls for this workflow."},"next_steps_guidance":["Follow these steps carefully to begin the task:","1) Connections active, proceed to next step"],"success":true,"error":null},"error":null,"log_id":"log_x"}"#;

    fn text(t: &str) -> Value {
        json!({"content": [{"type": "text", "text": t}]})
    }

    fn tool(name: &str, schema: Value) -> Value {
        json!({"name": name, "inputSchema": schema})
    }

    #[test]
    fn detects_nested_gateways() {
        let any = json!({"type": "object"});
        let composio: Vec<Value> = [COMPOSIO_SEARCH, COMPOSIO_SCHEMAS, COMPOSIO_EXECUTE, COMPOSIO_CONNECT]
            .iter()
            .map(|n| tool(n, any.clone()))
            .collect();
        assert_eq!(detect_tools(&composio), Some(Adapter::Composio));
        assert_eq!(detect_tools(&composio[..1]), None, "search alone is not enough");

        let call = json!({"type": "object", "properties": {"tool": {}, "arguments": {}}});
        let ours = [tool("search_tools", any.clone()), tool("call_tool", call), tool("get_tool_schemas", any.clone())];
        assert_eq!(detect_tools(&ours), Some(Adapter::Gateway { name_key: "tool", schemas: true }));
        let named = json!({"type": "object", "properties": {"name": {}, "arguments": {}}});
        let other = [tool("search_tools", any.clone()), tool("call_tool", named)];
        assert_eq!(detect_tools(&other), Some(Adapter::Gateway { name_key: "name", schemas: false }));
        let plain = [tool("search_tools", any.clone()), tool("call_tool", any.clone())];
        assert_eq!(detect_tools(&plain), None, "call_tool without tool + arguments");

        let mut cfg = ServerConfig::default();
        assert_eq!(detect(&cfg, &composio), Some(Adapter::Composio));
        cfg.nested = Nested::Off;
        assert_eq!(detect(&cfg, &composio), None);
        let parsed: ServerConfig = serde_json::from_value(json!({"id": "x", "nested": "off"})).unwrap();
        assert_eq!(parsed.nested, Nested::Off);
        let old: ServerConfig = serde_json::from_value(json!({"id": "x"})).unwrap();
        assert_eq!(old.nested, Nested::Auto);
    }

    #[test]
    fn reads_composio_searches() {
        let found = Adapter::Composio.parse_search("composio", &text(SAMPLE)).unwrap();
        let slugs: Vec<&str> = found.tools.iter().map(|t| t.slug.as_str()).collect();
        assert_eq!(
            slugs,
            ["GITHUB_LIST_REPOSITORY_ISSUES", "GITHUB_GET_AN_ISSUE", "GITHUB_SEARCH_ISSUES", "GITHUB_UPDATE_AN_ISSUE"]
        );
        assert_eq!(found.primary, 1);
        assert_eq!(found.session.as_deref(), Some("paid"));
        assert!(found.notes.is_empty(), "connected, and no useful guidance: {:?}", found.notes);
        let list = &found.tools[0];
        assert_eq!(list.parameters().unwrap(), "page?: integer, repo: string, owner: string, state?: string");
        assert_eq!(list.service.as_deref(), Some("github"));
        assert!(list.description.starts_with("Lists issues"));
        let update = &found.tools[3];
        assert!(update.schema.is_none() && update.description.starts_with("Updates"), "{update:?}");
        assert_eq!(
            found.tools[2],
            VTool { slug: "GITHUB_SEARCH_ISSUES".into(), service: Some("github".into()), ..Default::default() }
        );

        let mut v: Value = serde_json::from_str(SAMPLE).unwrap();
        v["data"]["toolkit_connection_statuses"][0]["has_active_connection"] = json!(false);
        v["data"]["toolkit_connection_statuses"][0]["status_message"] = json!("No active connection");
        let found = Adapter::Composio.parse_search("composio", &text(&v.to_string())).unwrap();
        assert_eq!(found.notes.len(), 1);
        assert!(found.notes[0].contains("github (No active connection)"), "{:?}", found.notes);
        assert!(found.notes[0].contains("composio__COMPOSIO_MANAGE_CONNECTIONS"));

        let failed = text(r#"{"successful":false,"error":"bad session","data":{}}"#);
        assert_eq!(Adapter::Composio.parse_search("c", &failed).unwrap_err(), "bad session");
        assert!(Adapter::Composio.parse_search("c", &text("not json")).is_err());
    }

    #[test]
    fn builds_requests() {
        let (tool, args) = Adapter::Composio.search_request("list issues", None);
        assert_eq!(tool, COMPOSIO_SEARCH);
        assert_eq!(args["queries"][0]["use_case"], "list issues");
        assert_eq!(args["session"], json!({"generate_id": true}));
        assert_eq!(Adapter::Composio.search_request("x", Some("s1")).1["session"], json!({"id": "s1"}));
        let (tool, args) = Adapter::Composio.exec_request("GITHUB_X", json!({"a": 1}), Some("s1"));
        assert_eq!(tool, COMPOSIO_EXECUTE);
        assert_eq!(
            args,
            json!({"tools": [{"tool_slug": "GITHUB_X", "arguments": {"a": 1}}], "sync_response_to_workbench": false,
                "session_id": "s1"})
        );
        let gateway = Adapter::Gateway { name_key: "name", schemas: false };
        assert_eq!(gateway.exec_request("gh__x", json!({}), None).1, json!({"name": "gh__x", "arguments": {}}));
        assert!(gateway.schemas_request(&["a".into()], None).is_none());
    }

    #[test]
    fn reads_gateway_searches_and_schemas() {
        let ours = r#"{"results":[{"tool":"github__create_issue","description":"Create an issue","parameters":"title: string"}]}"#;
        let gateway = Adapter::Gateway { name_key: "tool", schemas: true };
        let found = gateway.parse_search("inner", &text(ours)).unwrap();
        assert_eq!(found.primary, 1);
        assert_eq!(found.tools[0].slug, "github__create_issue");
        assert_eq!(found.tools[0].parameters().as_deref(), Some("title: string"));
        assert_eq!(found.tools[0].service.as_deref(), Some("github"));

        let schemas = r#"{"tools":[{"tool":"github__create_issue","inputSchema":{"type":"object","properties":{"title":{"type":"string"}}}}]}"#;
        let list = gateway.parse_schemas(&text(schemas)).unwrap();
        assert_eq!(list[0].parameters().as_deref(), Some("title?: string"));
        let composio = r#"{"successful":true,"data":{"tool_schemas":{"GITHUB_UPDATE_AN_ISSUE":{"input_schema":{"type":"object","properties":{"title":{"type":"string"}}}}}}}"#;
        let list = Adapter::Composio.parse_schemas(&text(composio)).unwrap();
        assert_eq!(list[0].slug, "GITHUB_UPDATE_AN_ISSUE");
        assert!(list[0].schema.is_some());
    }

    #[test]
    fn unwraps_single_executions() {
        let c = Adapter::Composio;
        let ok = r#"{"successful":true,"data":{"results":[{"tool_slug":"GITHUB_X","index":0,"response":{"successful":true,"data":{"issues":[1]},"error":null}}]},"error":null}"#;
        assert_eq!(c.unwrap_exec("GITHUB_X", text(ok)), text(r#"{"issues":[1]}"#));
        let failed = r#"{"data":{"results":[{"response":{"successful":false,"data":{},"error":"Not Found"}}]}}"#;
        let r = c.unwrap_exec("GITHUB_X", text(failed));
        assert_eq!((r["isError"].as_bool(), r["content"][0]["text"].as_str()), (Some(true), Some("Not Found")));
        // Not clearly one result of this tool: unchanged.
        let two = r#"{"data":{"results":[{"response":{"data":1}},{"response":{"data":2}}]}}"#;
        assert_eq!(c.unwrap_exec("GITHUB_X", text(two)), text(two));
        let other = r#"{"data":{"results":[{"tool_slug":"OTHER","response":{"data":1}}]}}"#;
        assert_eq!(c.unwrap_exec("GITHUB_X", text(other)), text(other));
        assert_eq!(c.unwrap_exec("GITHUB_X", text("plain words")), text("plain words"));
        let whole = r#"{"successful":false,"error":"no connection"}"#;
        assert_eq!(c.unwrap_exec("GITHUB_X", text(whole))["isError"], true);
        let gateway = Adapter::Gateway { name_key: "tool", schemas: false };
        assert_eq!(gateway.unwrap_exec("x", text(ok)), text(ok));
    }

    #[test]
    fn checks_arguments_against_schemas() {
        let schema = json!({"type": "object", "required": ["owner", "repo"],
            "properties": {"owner": {}, "repo": {}, "page": {}}});
        assert!(args_fit(&schema, &json!({"owner": "o", "repo": "r"})));
        assert!(args_fit(&schema, &json!({"owner": "o", "repo": "r", "page": 2})));
        assert!(!args_fit(&schema, &json!({"owner": "o"})), "a required one is missing");
        assert!(!args_fit(&schema, &json!({"owner": "o", "repo": "r", "labels": "bug"})), "unknown name");
        assert!(!args_fit(&schema, &json!("x")));
        let mut open = schema.clone();
        open["additionalProperties"] = json!(true);
        assert!(args_fit(&open, &json!({"owner": "o", "repo": "r", "labels": "bug"})));
        assert!(args_fit(&json!({"type": "object"}), &json!({})));
    }

    #[test]
    fn matches_services() {
        let cfg = |id: &str, service: &str| ServerConfig {
            id: id.into(),
            name: id.into(),
            service: service.into(),
            ..Default::default()
        };
        assert!(serves(&cfg("github", ""), "github"));
        assert!(serves(&cfg("github-tools", ""), "GitHub"));
        assert!(!serves(&cfg("git", ""), "github"));
        assert!(serves(&cfg("gh", "github"), "github"));
        assert!(!serves(&cfg("github", "gitlab"), "github"), "the setting wins");
        assert!(!serves(&cfg("github", ""), ""));
    }

    #[test]
    fn cache_is_bounded_and_keeps_schemas() {
        let cache = Cache::default();
        let full = VTool { slug: "A".into(), schema: Some(json!({"type": "object"})), ..Default::default() };
        cache.remember("c", &[full]);
        cache.remember("c", &[VTool { slug: "A".into(), ..Default::default() }]);
        assert!(cache.tool("c__A").unwrap().schema.is_some());
        assert_eq!(cache.owners("A"), ["c"]);
        let many: Vec<VTool> =
            (0..MAX_TOOLS + 10).map(|i| VTool { slug: format!("T{i}"), ..Default::default() }).collect();
        cache.remember("c", &many);
        assert_eq!(cache.tools.lock().len(), MAX_TOOLS);
        cache.set_session("c|local", Some("s1".into()));
        assert_eq!(cache.session("c|local").as_deref(), Some("s1"));
        cache.set_session("c|local", None);
        assert_eq!(cache.session("c|local"), None);
    }
}
