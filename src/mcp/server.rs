//! The gateway's own MCP endpoint (Streamable HTTP, stateless, JSON answers).
//!
//! * `POST /mcp`: four meta tools (`search_tools`, `get_tool_schemas`, `call_tool`,
//!   `list_servers`), so the agent's context holds four tool definitions however many
//!   servers sit behind the gateway.
//! * `POST /mcp/all`: every tool of every server, named `<server>__<tool>`.
//! * `POST /skills`: only the skill tools.
//!
//! Once a skill is enabled, all three also offer `list_skills`, `load_skill` and
//! `read_skill_file`, name the skills in their instructions and list them as prompts
//! (see `crate::skills`).
//!
//! Upstream servers that are themselves meta-tool gateways (Composio) are flattened on
//! `/mcp`: `search_tools` also searches their apps and `call_tool` runs those tools
//! directly (see `super::nested`). A server of its own for a service (`github`) ranks
//! above a nested gateway's tools for it, and the nested gateway stands in when that
//! server fails.
//!
//! Authentication is the API key of `/v1` (or a loopback client when keys are not
//! required); a restricted key only sees the servers on its allow list (`mcp/<id>`).

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::future::join_all;
use serde_json::{json, Value};

use super::client::RpcError as UpstreamRpcError;
use super::control;
use super::nested::{self, Adapter, Found, VTool};
use super::usage::{self, Call};
use super::{params_summary, search, tool_description, tool_name, ServerConfig, SEP};
use crate::auth::{authenticate, AuthError, Caller};
use crate::skills;
use crate::state::AppState;

const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
/// How long `search_tools` waits for a nested gateway's own search.
const NESTED_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Meta,
    All,
    /// `/skills`: only the skill tools (see `crate::skills`).
    Skills,
}

pub async fn meta(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    handle(state, peer, headers, uri, Mode::Meta, body).await
}

pub async fn all(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    handle(state, peer, headers, uri, Mode::All, body).await
}

pub async fn skills(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    handle(state, peer, headers, uri, Mode::Skills, body).await
}

/// No server-initiated stream and no sessions to end (both optional in the spec).
pub async fn not_allowed() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response()
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

async fn handle(
    state: Arc<AppState>,
    peer: SocketAddr,
    headers: HeaderMap,
    uri: Uri,
    mode: Mode,
    body: Bytes,
) -> Response {
    let require = state.db.settings().get("requireApiKey").and_then(Value::as_bool).unwrap_or(false);
    let caller = match authenticate(&state.cfg, &state.db, require, peer.ip(), &headers, uri.query()) {
        Ok(c) => c,
        Err(e) => {
            let (msg, invalid) = match e {
                AuthError::Missing => ("API key required: send Authorization: Bearer <api key>, or sign in", false),
                AuthError::Invalid => ("Invalid API key or token", true),
            };
            // Points OAuth-capable clients at the sign-in (see `oauth_server`).
            let challenge = super::oauth_server::challenge(&headers, uri.path(), invalid);
            let body = Json(rpc_error(Value::Null, -32001, msg));
            return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, challenge)], body).into_response();
        }
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let error = rpc_error(Value::Null, -32700, format!("parse error: {e}"));
            return (StatusCode::BAD_REQUEST, Json(error)).into_response();
        }
    };
    let ctx = Ctx { state: &state, caller: &caller, mode };
    let out = match msg {
        Value::Array(batch) => {
            let mut answers = Vec::new();
            for m in batch {
                answers.extend(ctx.dispatch(m).await);
            }
            (!answers.is_empty()).then_some(Value::Array(answers))
        }
        m => ctx.dispatch(m).await,
    };
    match out {
        Some(v) => Json(v).into_response(),
        // Only notifications (or responses) were sent.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// A JSON-RPC error: code and message.
struct RpcError(i64, String);

fn invalid(msg: impl Into<String>) -> RpcError {
    RpcError(-32602, msg.into())
}

/// A tool result with one text block (`isError` marks failures the agent should see).
fn text_result(text: String, is_error: bool) -> Value {
    let mut r = json!({"content": [{"type": "text", "text": text}]});
    if is_error {
        r["isError"] = json!(true);
    }
    r
}

fn json_result(v: Value) -> Value {
    text_result(v.to_string(), false)
}

/// One request's view of the gateway: what this caller may reach, and how tools are named.
struct Ctx<'a> {
    state: &'a Arc<AppState>,
    caller: &'a Caller,
    mode: Mode,
}

impl Ctx<'_> {
    async fn dispatch(&self, m: Value) -> Option<Value> {
        // Notifications (no id) and responses (no method) get no answer.
        let id = m.get("id").cloned()?;
        let method = m.get("method").and_then(Value::as_str)?;
        let params = m.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.list_tools().await),
            "tools/call" => self.call(&params).await,
            "prompts/list" => Ok(skills::prompts(&self.state.db)),
            "prompts/get" => self.get_prompt(&params),
            _ => Err(RpcError(-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(RpcError(code, message)) => rpc_error(id, code, message),
        })
    }

    /// Enabled servers this caller may use.
    fn servers(&self) -> Vec<ServerConfig> {
        super::servers(&self.state.db).into_iter().filter(|s| s.enabled && self.caller.may_use_mcp(&s.id)).collect()
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        let version = SUPPORTED_VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(SUPPORTED_VERSIONS[0]);
        let servers = self.servers();
        let mut names: Vec<String> = servers
            .iter()
            .map(|s| if s.description.is_empty() { s.id.clone() } else { format!("{} ({})", s.id, s.description) })
            .collect();
        if self.control_on() {
            names.push(format!("{} ({})", control::SERVER, control::DESCRIPTION));
        }
        let skill_note = skills::instructions(&skills::enabled(&self.state.db));
        let mut instructions = match self.mode {
            Mode::Skills => {
                "zilong-router skills gateway: packaged instructions and files for specific tasks.".to_string()
            }
            Mode::Meta => format!(
                "zilong-router MCP gateway in front of {} MCP server(s): {}. Their tools are not listed directly: \
                 call search_tools with what you want to do, get_tool_schemas for the exact parameters if needed, \
                 then call_tool with the tool name (server__tool) and its arguments.",
                names.len(),
                names.join(", ")
            ),
            Mode::All => format!(
                "zilong-router MCP gateway: the tools of {} MCP server(s) ({}), named server__tool.",
                names.len(),
                names.join(", ")
            ),
        };
        let nested: Vec<&str> = servers
            .iter()
            .filter(|s| {
                let cached = super::cached_tools(&self.state.db, &s.id);
                cached.is_some_and(|c| nested::detect(s, &c.tools).is_some())
            })
            .map(|s| s.id.as_str())
            .collect();
        if self.mode == Mode::Meta && !nested.is_empty() {
            instructions.push_str(&format!(
                " search_tools also finds the tools of the apps behind {} (nested gateways): call_tool runs the \
                 names it returns directly.",
                nested.join(", ")
            ));
        }
        match skill_note {
            Some(s) => instructions = format!("{instructions}\n\n{s}"),
            None if self.mode == Mode::Skills => instructions.push_str(" No skill is enabled yet."),
            None => {}
        }
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}, "prompts": {"listChanged": false}},
            "serverInfo": {"name": "zilong-router", "version": env!("CARGO_PKG_VERSION")},
            "instructions": instructions,
        })
    }

    /// Tool lists of `servers`, fetched in parallel (cached ones come from the db), and
    /// the servers that could not be reached.
    async fn catalogs(&self, servers: &[ServerConfig]) -> (Vec<(String, Vec<Value>)>, Vec<Value>) {
        let lists = join_all(servers.iter().map(|s| self.state.mcp.tools(self.state, s, false))).await;
        let (mut ok, mut failed) = (Vec::new(), Vec::new());
        for (s, r) in servers.iter().zip(lists) {
            match r {
                Ok(tools) => ok.push((s.id.clone(), tools)),
                Err(e) => failed.push(json!({"server": s.id, "error": format!("{e:#}")})),
            }
        }
        (ok, failed)
    }

    /// The skill tools, shown once a skill is enabled (always on `/skills`).
    fn skill_tools(&self) -> Vec<Value> {
        if self.mode == Mode::Skills || !skills::enabled(&self.state.db).is_empty() {
            skills::tool_defs()
        } else {
            Vec::new()
        }
    }

    async fn list_tools(&self) -> Value {
        let mut tools = match self.mode {
            Mode::Skills => Vec::new(),
            Mode::Meta => match meta_tools() {
                Value::Array(tools) => tools,
                _ => Vec::new(),
            },
            Mode::All => self.all_tools().await,
        };
        tools.extend(self.skill_tools());
        json!({"tools": tools})
    }

    /// Every tool of every server this caller may use, named `server__tool`.
    async fn all_tools(&self) -> Vec<Value> {
        let (mut catalogs, _) = self.catalogs(&self.servers()).await;
        if self.control_on() {
            catalogs.push((control::SERVER.to_string(), control::tool_defs()));
        }
        catalogs
            .iter()
            .flat_map(|(server, tools)| {
                tools.iter().map(move |t| {
                    let mut t = t.clone();
                    t["name"] = json!(format!("{server}{SEP}{}", tool_name(&t)));
                    t["description"] = json!(format!("[{server}] {}", tool_description(&t)));
                    t
                })
            })
            .collect()
    }

    /// Whether this caller may use the built-in `zilong` control server (an API key with
    /// the flag set; see `control`). It is not offered on `/skills`.
    fn control_on(&self) -> bool {
        self.mode != Mode::Skills && control::allowed(&self.state.db, self.caller)
    }

    /// The control tool `name` (`zilong__tool`) stands for, when this caller may use it.
    fn control_tool<'n>(&self, name: &'n str) -> Option<&'n str> {
        let rest = name.strip_prefix(control::SERVER)?;
        let tool = rest.strip_prefix(SEP).or_else(|| rest.strip_prefix('/'))?;
        (control::TOOLS.contains(&tool) && self.control_on()).then_some(tool)
    }

    fn call_control(&self, tool: &str, args: &Value) -> Value {
        let started = Instant::now();
        let out = control::call(self.state, tool, args);
        let error = super::tool_error(&out);
        let status = if error.is_some() { usage::TOOL_ERROR } else { usage::OK };
        let (request_bytes, response_bytes) = (usage::json_len(args), usage::json_len(&out));
        let (kind, server) = (usage::CALL, Some(control::SERVER));
        self.record(Call { kind, server, tool, status, error, started, request_bytes, response_bytes });
        out
    }

    async fn call(&self, params: &Value) -> Result<Value, RpcError> {
        let name = params.get("name").and_then(Value::as_str).ok_or_else(|| invalid("name is required"))?;
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let started = Instant::now();
        if let Some(out) = skills::call_tool(&self.state.db, name, &args) {
            self.record_meta(name, &args, &out, started);
            return Ok(out);
        }
        match self.mode {
            Mode::Meta => {}
            Mode::Skills => return Err(invalid(format!("unknown tool: {name}"))),
            Mode::All => {
                if let Some(tool) = self.control_tool(name) {
                    return Ok(self.call_control(tool, &args));
                }
                let unknown = || invalid(format!("unknown tool: {name}"));
                let (server, tool) = name.split_once(SEP).ok_or_else(unknown)?;
                let cfg = self.servers().into_iter().find(|s| s.id == server).ok_or_else(unknown)?;
                return Ok(self.call_upstream(&cfg, tool, args).await);
            }
        }
        let out = match name {
            "search_tools" => self.search_tools(&args).await,
            "get_tool_schemas" => self.get_tool_schemas(&args).await,
            "call_tool" => match self.call_tool(&args).await {
                // Recorded as the server's tool call.
                Ok(upstream) => return Ok(upstream),
                Err(refused) => refused,
            },
            "list_servers" => self.list_servers().await,
            _ => return Err(invalid(format!("unknown tool: {name}"))),
        };
        self.record_meta(name, &args, &out, started);
        Ok(out)
    }

    /// `prompts/get`: an enabled skill as a prompt (counted like `load_skill`).
    fn get_prompt(&self, params: &Value) -> Result<Value, RpcError> {
        let started = Instant::now();
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
        let result = skills::prompt(&self.state.db, name);
        let out = match &result {
            Ok(v) => v.clone(),
            Err(e) => text_result(e.clone(), true),
        };
        self.record_meta("prompts/get", params, &out, started);
        result.map_err(invalid)
    }

    /// Saves a usage row for a call the gateway answered itself.
    fn record_meta(&self, tool: &str, args: &Value, out: &Value, started: Instant) {
        let error = super::tool_error(out);
        let status = if error.is_some() { usage::TOOL_ERROR } else { usage::OK };
        let (request_bytes, response_bytes) = (usage::json_len(args), usage::json_len(out));
        let kind = usage::META;
        self.record(Call { kind, server: None, tool, status, error, started, request_bytes, response_bytes });
    }

    /// Saves a usage row for this caller.
    fn record(&self, c: Call<'_>) {
        let endpoint = match self.mode {
            Mode::Meta => "/mcp",
            Mode::All => "/mcp/all",
            Mode::Skills => "/skills",
        };
        usage::record(self.state, self.caller, endpoint, c);
    }

    async fn call_upstream(&self, cfg: &ServerConfig, tool: &str, args: Value) -> Value {
        self.call_upstream_raw(cfg, tool, args).await.0
    }

    /// `tools/call` on `cfg`, recorded; with why when the server rather than the tool failed.
    async fn call_upstream_raw(&self, cfg: &ServerConfig, tool: &str, args: Value) -> (Value, Option<String>) {
        let started = Instant::now();
        let request_bytes = usage::json_len(&args);
        let result = self.state.mcp.call(self.state, cfg, tool, args).await;
        self.finish(cfg, tool, started, request_bytes, result)
    }

    /// Records a call to `cfg` (under the name `tool`) and turns its outcome into the
    /// agent's result, with why when the server itself failed (see [`infrastructure`]).
    fn finish(
        &self,
        cfg: &ServerConfig,
        tool: &str,
        started: Instant,
        request_bytes: usize,
        result: anyhow::Result<Value>,
    ) -> (Value, Option<String>) {
        let (status, error) = usage::outcome(&result);
        let response_bytes = result.as_ref().map_or(0, usage::json_len);
        let (kind, server) = (usage::CALL, Some(cfg.id.as_str()));
        self.record(Call { kind, server, tool, status, error, started, request_bytes, response_bytes });
        match result {
            Ok(r) => (r, None),
            Err(e) => {
                let reason = infrastructure(&e).then(|| short(&format!("{e:#}")));
                (text_result(format!("{}{SEP}{tool} failed: {e:#}", cfg.id), true), reason)
            }
        }
    }

    /// Runs `tool` on `cfg` for `call_tool`: a tool of an app behind a nested gateway
    /// through the gateway's execute tool; and when a direct server cannot be used, the
    /// same service through a nested gateway (see [`Ctx::fallback`]).
    async fn route(&self, cfg: &ServerConfig, tool: &str, args: Value) -> Value {
        let started = Instant::now();
        let (out, reason) = match self.state.mcp.tools(self.state, cfg, false).await {
            Ok(tools) => {
                if !tools.iter().any(|t| tool_name(t) == tool) {
                    if let Some(adapter) = nested::detect(cfg, &tools) {
                        return self.call_nested(cfg, adapter, tool, args).await;
                    }
                }
                self.call_upstream_raw(cfg, tool, args.clone()).await
            }
            // Not even listed: that is the failed call.
            Err(e) if infrastructure(&e) => self.finish(cfg, tool, started, usage::json_len(&args), Err(e)),
            Err(_) => self.call_upstream_raw(cfg, tool, args.clone()).await,
        };
        match reason {
            Some(why) => self.fallback(cfg, tool, &args, &why).await.unwrap_or(out),
            None => out,
        }
    }

    /// Runs `slug`, a tool behind the nested gateway `cfg`, recorded under its own name.
    async fn call_nested(&self, cfg: &ServerConfig, adapter: Adapter, slug: &str, args: Value) -> Value {
        let started = Instant::now();
        let request_bytes = usage::json_len(&args);
        let session = self.state.mcp.nested.session(&self.session_key(&cfg.id));
        let (tool, wrapped) = adapter.exec_request(slug, args, session.as_deref());
        let result = self.state.mcp.call(self.state, cfg, tool, wrapped).await.map(|r| adapter.unwrap_exec(slug, r));
        self.finish(cfg, slug, started, request_bytes, result).0
    }

    /// The nested gateways this caller may use (from their cached tool lists).
    fn nested_gateways(&self, except: &str) -> Vec<(ServerConfig, Adapter)> {
        self.servers()
            .into_iter()
            .filter(|s| s.id != except)
            .filter_map(|s| {
                let adapter = nested::detect(&s, &super::cached_tools(&self.state.db, &s.id)?.tools)?;
                Some((s, adapter))
            })
            .collect()
    }

    /// When the direct server `cfg` failed (`reason`) running `tool`: the same service
    /// through a nested gateway. The equivalent tool is the best match of a nested search
    /// for the tool's description; it runs when `args` fit its schema, else the agent gets
    /// its name and parameters. `None` when no nested gateway has an equivalent.
    async fn fallback(&self, cfg: &ServerConfig, tool: &str, args: &Value, reason: &str) -> Option<Value> {
        let gateways = self.nested_gateways(&cfg.id);
        if gateways.is_empty() {
            return None;
        }
        let service = if cfg.service.is_empty() { cfg.id.as_str() } else { cfg.service.as_str() };
        let described = super::cached_tools(&self.state.db, &cfg.id)
            .and_then(|c| c.tools.iter().find(|t| tool_name(t) == tool).map(|t| tool_description(t).to_string()))
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| tool.replace(['_', '-'], " "));
        let use_case = format!("{service}: {}", described.chars().take(200).collect::<String>());
        for (g, adapter) in &gateways {
            let search = tokio::time::timeout(NESTED_TIMEOUT, self.nested_search(g, *adapter, &use_case)).await;
            let Ok(Ok(found)) = search else { continue };
            let candidates: Vec<&VTool> = found
                .tools
                .iter()
                .take(found.primary)
                .filter(|t| match t.service.as_deref() {
                    Some(s) => nested::serves(cfg, s),
                    None => true,
                })
                .collect();
            let Some(best) = candidates.first() else { continue };
            if best.schema.as_ref().is_some_and(|s| nested::args_fit(s, args)) {
                let out = self.call_nested(g, *adapter, &best.slug, args.clone()).await;
                let note = format!(
                    "[{}{SEP}{tool} failed ({reason}), so this ran as {}{SEP}{} through the nested gateway {}.]",
                    cfg.id, g.id, best.slug, g.id
                );
                return Some(prepend(out, note));
            }
            let options: Vec<String> = candidates
                .iter()
                .take(3)
                .map(|t| match t.parameters() {
                    Some(p) => format!("{}{SEP}{} ({p})", g.id, t.slug),
                    None => format!("{}{SEP}{} (parameters: get_tool_schemas)", g.id, t.slug),
                })
                .collect();
            let text = format!(
                "{}{SEP}{tool} failed: {reason}. The same service is reachable through the nested gateway {}: {}. \
                 Call one of these with call_tool and arguments matching its parameters.",
                cfg.id,
                g.id,
                options.join("; ")
            );
            return Some(text_result(text, true));
        }
        None
    }

    /// A disabled server this caller may use, named by `server__tool`, and the tool.
    fn disabled(&self, name: &str) -> Option<(ServerConfig, String)> {
        let (server, tool) = name.split_once(SEP).or_else(|| name.split_once('/'))?;
        let cfg = super::server(&self.state.db, server).filter(|s| !s.enabled && self.caller.may_use_mcp(&s.id))?;
        Some((cfg, tool.to_string()))
    }

    /// Why the direct server `cfg` cannot be used right now, if it cannot.
    fn unusable(&self, cfg: &ServerConfig) -> Option<&'static str> {
        if super::auth::load(&self.state.db, &cfg.id).is_some_and(|a| a.needs_sign_in) {
            return Some("sign-in needed");
        }
        self.state.mcp.stats(&cfg.id).connect_failed.then_some("cannot connect")
    }

    /// Nested gateway sessions are kept per server and caller.
    fn session_key(&self, server: &str) -> String {
        match self.caller {
            Caller::Key(k) => format!("{server}|{}", k.id),
            Caller::Local => format!("{server}|local"),
        }
    }

    /// The nested gateway `cfg`'s own search for `query`, with the schemas it left out
    /// fetched in one batch; the tools found are remembered for `get_tool_schemas`.
    async fn nested_search(&self, cfg: &ServerConfig, adapter: Adapter, query: &str) -> Result<Found, String> {
        let cache = &self.state.mcp.nested;
        let key = self.session_key(&cfg.id);
        let session = cache.session(&key);
        let (tool, args) = adapter.search_request(query, session.as_deref());
        let result = self.state.mcp.call(self.state, cfg, tool, args).await.map_err(|e| format!("{e:#}"));
        let mut found = match result.and_then(|r| adapter.parse_search(&cfg.id, &r)) {
            Ok(f) => f,
            Err(e) => {
                // Start a new session next time, in case this one expired.
                cache.set_session(&key, None);
                return Err(e);
            }
        };
        let session = found.session.clone().or(session);
        cache.set_session(&key, session.clone());
        // Schemas an earlier search or batch already found.
        for t in found.tools.iter_mut().filter(|t| t.parameters().is_none()) {
            if let Some(known) = cache.tool(&format!("{}{SEP}{}", cfg.id, t.slug)) {
                t.schema = known.schema;
            }
        }
        let missing: Vec<String> = found
            .tools
            .iter()
            .filter(|t| t.parameters().is_none())
            .map(|t| t.slug.clone())
            .take(nested::SCHEMA_BATCH)
            .collect();
        let batch = if missing.is_empty() { None } else { adapter.schemas_request(&missing, session.as_deref()) };
        if let Some((tool, args)) = batch {
            if let Ok(r) = self.state.mcp.call(self.state, cfg, tool, args).await {
                for s in adapter.parse_schemas(&r).unwrap_or_default() {
                    if let Some(t) = found.tools.iter_mut().find(|t| t.slug.eq_ignore_ascii_case(&s.slug)) {
                        if t.schema.is_none() {
                            t.schema = s.schema;
                        }
                        if t.description.is_empty() {
                            t.description = s.description;
                        }
                    }
                }
            }
        }
        cache.remember(&cfg.id, &found.tools);
        Ok(found)
    }

    /// `server__tool` (or `server/tool`, or a tool name only one server has) to the
    /// server and the tool's own name.
    async fn resolve(&self, name: &str, servers: &[ServerConfig]) -> Result<(ServerConfig, String), String> {
        let split = name.split_once(SEP).or_else(|| name.split_once('/'));
        if let Some((server, tool)) = split {
            if let Some(s) = servers.iter().find(|s| s.id == server) {
                return Ok((s.clone(), tool.to_string()));
            }
        }
        let (catalogs, _) = self.catalogs(servers).await;
        let owners: Vec<&String> =
            catalogs.iter().filter(|(_, tools)| tools.iter().any(|t| tool_name(t) == name)).map(|(s, _)| s).collect();
        match owners.as_slice() {
            [one] => Ok((servers.iter().find(|s| &s.id == *one).cloned().unwrap_or_default(), name.to_string())),
            [] => {
                // A tool a nested search found, named without its server.
                let known = self.state.mcp.nested.owners(name);
                match servers.iter().filter(|s| known.contains(&s.id)).collect::<Vec<_>>().as_slice() {
                    [one] => Ok(((*one).clone(), name.to_string())),
                    _ => Err(format!("unknown tool `{name}`: use search_tools to find tool names (server__tool)")),
                }
            }
            many => Err(format!(
                "`{name}` exists on several servers ({}): call it as server__tool",
                many.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    async fn search_tools(&self, args: &Value) -> Value {
        let query = args.get("query").and_then(Value::as_str).unwrap_or_default();
        let limit = args.get("limit").and_then(Value::as_u64).map_or(DEFAULT_LIMIT, |n| n as usize).clamp(1, MAX_LIMIT);
        let mut servers = self.servers();
        let mut with_control = self.control_on();
        if let Some(only) = args.get("server").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            servers.retain(|s| s.id == only);
            with_control = with_control && only == control::SERVER;
            if servers.is_empty() && !with_control {
                return text_result(format!("unknown server `{only}`; call list_servers"), true);
            }
        }
        let (mut catalogs, unavailable) = self.catalogs(&servers).await;
        if with_control {
            catalogs.push((control::SERVER.to_string(), control::tool_defs()));
        }
        let nested: Vec<(ServerConfig, Adapter)> = catalogs
            .iter()
            .filter_map(|(id, tools)| {
                let cfg = servers.iter().find(|s| &s.id == id)?;
                Some((cfg.clone(), nested::detect(cfg, tools)?))
            })
            .collect();
        // A nested gateway's own search, schema and execute tools are not offered.
        let catalogs: Vec<(String, Vec<Value>)> = catalogs
            .into_iter()
            .map(|(id, tools)| {
                let adapter = nested.iter().find(|(c, _)| c.id == id).map(|(_, a)| *a);
                let tools = tools.into_iter().filter(|t| !adapter.is_some_and(|a| a.hides(tool_name(t)))).collect();
                (id, tools)
            })
            .collect();
        let mut notes = Vec::new();
        let mut found = Vec::new();
        if query.trim().is_empty() {
            for (cfg, a) in &nested {
                notes.push(format!(
                    "{} is a nested {} gateway: search with a task description to also find its apps' tools.",
                    cfg.id,
                    a.label()
                ));
            }
        } else {
            let searches = nested.iter().map(|(cfg, a)| async move {
                (cfg, tokio::time::timeout(NESTED_TIMEOUT, self.nested_search(cfg, *a, query)).await)
            });
            for (cfg, r) in join_all(searches).await {
                match r {
                    Ok(Ok(mut f)) => {
                        notes.append(&mut f.notes);
                        found.push((cfg.id.clone(), f));
                    }
                    Ok(Err(e)) => notes.push(format!(
                        "{}: app search failed ({}); only its own tools were searched.",
                        cfg.id,
                        short(&e)
                    )),
                    Err(_) => {
                        notes.push(format!("{}: app search timed out; only its own tools were searched.", cfg.id))
                    }
                }
            }
        }
        let local = search(&catalogs, query, limit);
        let results = self.rank(&servers, &nested, local, &found, &mut notes, limit);
        let empty = results.is_empty();
        let mut out = json!({"results": results});
        if empty {
            out["hint"] = json!("No match. Try other words, an empty query to list every tool, or list_servers.");
        }
        if !notes.is_empty() {
            out["notes"] = json!(notes);
        }
        if !unavailable.is_empty() {
            out["unavailable"] = json!(unavailable);
        }
        json_result(out)
    }

    /// Search results in order: the matches of direct servers that cover a service the
    /// nested results also cover, the nested gateways' best matches, the other direct
    /// matches mixed with the nested related tools, the nested tools those direct servers
    /// make a fallback (marked `fallback`), and last the matches of direct servers that
    /// cannot be used now (their service is reached through the nested gateway instead).
    fn rank(
        &self,
        servers: &[ServerConfig],
        nested: &[(ServerConfig, Adapter)],
        local: Vec<(&str, &Value)>,
        found: &[(String, Found)],
        notes: &mut Vec<String>,
        limit: usize,
    ) -> Vec<Value> {
        let direct: Vec<&ServerConfig> = servers.iter().filter(|s| !nested.iter().any(|(n, _)| n.id == s.id)).collect();
        let (mut preferred, mut demoted) = (HashSet::new(), HashSet::new());
        let (mut primary, mut related, mut fallback) = (Vec::new(), Vec::new(), Vec::new());
        for (server, f) in found {
            for (i, t) in f.tools.iter().enumerate() {
                let mut row = virtual_row(server, t);
                let svc = t.service.as_deref().unwrap_or_default();
                let matching: Vec<&ServerConfig> = direct.iter().copied().filter(|s| nested::serves(s, svc)).collect();
                let usable: Vec<&ServerConfig> =
                    matching.iter().copied().filter(|s| self.unusable(s).is_none()).collect();
                if !usable.is_empty() {
                    preferred.extend(usable.iter().map(|s| s.id.clone()));
                    row["fallback"] = json!(true);
                    fallback.push(row);
                    continue;
                }
                for s in matching {
                    if demoted.insert(s.id.clone()) {
                        let why = self.unusable(s).unwrap_or("unavailable");
                        notes.push(format!(
                            "{} cannot be used now ({why}): its {svc} tools are reached through {server}.",
                            s.id
                        ));
                    }
                }
                if i < f.primary {
                    primary.push(row);
                } else {
                    related.push(row);
                }
            }
        }
        if !fallback.is_empty() {
            notes.push(
                "fallback: a nested gateway's tool for a service that has its own server; prefer that server's tools."
                    .into(),
            );
        }
        let (mut first, mut middle, mut last) = (Vec::new(), Vec::new(), Vec::new());
        for (server, t) in local {
            let row = native_row(server, t);
            if preferred.contains(server) {
                first.push(row);
            } else if demoted.contains(server) {
                last.push(row);
            } else {
                middle.push(row);
            }
        }
        let mut out = first;
        out.extend(primary);
        let (mut a, mut b) = (middle.into_iter(), related.into_iter());
        loop {
            let (x, y) = (a.next(), b.next());
            if x.is_none() && y.is_none() {
                break;
            }
            out.extend(x);
            out.extend(y);
        }
        out.extend(fallback);
        out.extend(last);
        let mut seen = HashSet::new();
        out.retain(|r| seen.insert(r["tool"].as_str().unwrap_or_default().to_string()));
        out.truncate(limit);
        out
    }

    async fn get_tool_schemas(&self, args: &Value) -> Value {
        let names: Vec<&str> = match args.get("tools") {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(s)) => vec![s.as_str()],
            _ => return text_result("tools (a list of tool names) is required".into(), true),
        };
        let servers = self.servers();
        let mut out = Vec::new();
        for name in names {
            out.push(self.schema(name, &servers).await.unwrap_or_else(|e| json!({"tool": name, "error": e})));
        }
        json_result(json!({"tools": out}))
    }

    async fn schema(&self, name: &str, servers: &[ServerConfig]) -> Result<Value, String> {
        if let Some(tool) = self.control_tool(name) {
            let def = control::tool_defs().into_iter().find(|t| tool_name(t) == tool).unwrap_or_default();
            return Ok(json!({
                "tool": format!("{}{SEP}{tool}", control::SERVER),
                "description": tool_description(&def),
                "inputSchema": def.get("inputSchema").cloned().unwrap_or_else(|| json!({"type": "object"})),
                "annotations": def.get("annotations").cloned().unwrap_or(Value::Null),
            }));
        }
        let (cfg, tool) = self.resolve(name, servers).await?;
        let tools = self.state.mcp.tools(self.state, &cfg, false).await.map_err(|e| format!("{e:#}"))?;
        let Some(t) = tools.iter().find(|t| tool_name(t) == tool) else {
            let adapter = nested::detect(&cfg, &tools).ok_or("no such tool on this server")?;
            return self.nested_schema(&cfg, adapter, &tool).await;
        };
        let mut entry = json!({
            "tool": format!("{}{SEP}{tool}", cfg.id),
            "description": tool_description(t),
            "inputSchema": t.get("inputSchema").cloned().unwrap_or_else(|| json!({"type": "object"})),
        });
        if let Some(a) = t.get("annotations") {
            entry["annotations"] = a.clone();
        }
        Ok(entry)
    }

    /// The schema of `slug` behind the nested gateway `cfg`: as a search found it, or
    /// from the gateway's own schema tool.
    async fn nested_schema(&self, cfg: &ServerConfig, adapter: Adapter, slug: &str) -> Result<Value, String> {
        let name = format!("{}{SEP}{slug}", cfg.id);
        let cache = &self.state.mcp.nested;
        let t = match cache.tool(&name).filter(|t| t.schema.is_some()) {
            Some(t) => t,
            None => {
                let session = cache.session(&self.session_key(&cfg.id));
                let (tool, args) = adapter
                    .schemas_request(&[slug.to_string()], session.as_deref())
                    .ok_or("this nested gateway has no schema tool: search for the tool to see its parameters")?;
                let r = self.state.mcp.call(self.state, cfg, tool, args).await.map_err(|e| format!("{e:#}"))?;
                let t = adapter
                    .parse_schemas(&r)?
                    .into_iter()
                    .find(|t| t.slug.eq_ignore_ascii_case(slug) && t.schema.is_some())
                    .ok_or("the nested gateway returned no schema for this tool")?;
                cache.remember(&cfg.id, std::slice::from_ref(&t));
                t
            }
        };
        Ok(json!({
            "tool": name,
            "description": t.description,
            "inputSchema": t.schema.unwrap_or_else(|| json!({"type": "object"})),
        }))
    }

    /// The server's result (`Ok`), or the gateway's error when the call never reached a
    /// server (`Err`).
    async fn call_tool(&self, args: &Value) -> Result<Value, Value> {
        let Some(name) = args.get("tool").and_then(Value::as_str) else {
            return Err(text_result("tool is required (server__tool, from search_tools)".into(), true));
        };
        let arguments = match args.get("arguments") {
            None | Some(Value::Null) => json!({}),
            // Some models send the arguments as a JSON string.
            Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                Ok(v) if v.is_object() => v,
                _ => return Err(text_result("arguments must be a JSON object".into(), true)),
            },
            Some(v) if v.is_object() => v.clone(),
            Some(_) => return Err(text_result("arguments must be a JSON object".into(), true)),
        };
        if let Some(tool) = self.control_tool(name) {
            return Ok(self.call_control(tool, &arguments));
        }
        match self.resolve(name, &self.servers()).await {
            Ok((cfg, tool)) => Ok(self.route(&cfg, &tool, arguments).await),
            Err(e) => {
                if let Some((cfg, tool)) = self.disabled(name) {
                    if let Some(out) = self.fallback(&cfg, &tool, &arguments, "the server is disabled").await {
                        return Ok(out);
                    }
                }
                Err(text_result(e, true))
            }
        }
    }

    async fn list_servers(&self) -> Value {
        let servers = self.servers();
        let mut list: Vec<Value> = servers
            .iter()
            .map(|s| {
                let cached = super::cached_tools(&self.state.db, &s.id);
                let tools = cached.as_ref().map(|c| c.tools.len());
                let mut v = json!({"server": s.id, "name": s.name, "description": s.description, "tools": tools});
                if let Some(a) = cached.and_then(|c| nested::detect(s, &c.tools)) {
                    v["nestedGateway"] = json!(a.label());
                }
                v
            })
            .collect();
        if self.control_on() {
            let (name, tools) = (control::SERVER, control::TOOLS.len());
            list.push(json!({"server": name, "name": name, "description": control::DESCRIPTION, "tools": tools}));
        }
        json_result(json!({"servers": list}))
    }
}

/// Whether `e` means the server could not be used (unreachable, down, timed out, signed
/// out, failing to connect) rather than that the tool refused the call.
fn infrastructure(e: &anyhow::Error) -> bool {
    if e.chain().any(|c| c.is::<UpstreamRpcError>()) {
        return false;
    }
    let text = format!("{e:#}");
    !["HTTP 400", "HTTP 422"].iter().any(|s| text.contains(s))
}

fn short(s: &str) -> String {
    s.chars().take(160).collect()
}

/// `note` as a first text block of a tool result.
fn prepend(mut out: Value, note: String) -> Value {
    if let Some(content) = out.get_mut("content").and_then(Value::as_array_mut) {
        content.insert(0, json!({"type": "text", "text": note}));
    }
    out
}

/// A `search_tools` result for a server's own tool.
fn native_row(server: &str, t: &Value) -> Value {
    let desc: String = tool_description(t).chars().take(300).collect();
    json!({
        "tool": format!("{server}{SEP}{}", tool_name(t)),
        "description": desc,
        "parameters": params_summary(t.get("inputSchema").unwrap_or(&Value::Null)),
    })
}

/// A `search_tools` result for a tool behind the nested gateway `server`.
fn virtual_row(server: &str, t: &VTool) -> Value {
    let desc: String = t.description.chars().take(300).collect();
    let mut row = json!({"tool": format!("{server}{SEP}{}", t.slug), "description": desc});
    match t.parameters() {
        Some(p) => row["parameters"] = json!(p),
        None => row["hint"] = json!("call get_tool_schemas for its parameters"),
    }
    row
}

fn meta_tools() -> Value {
    json!([
        {
            "name": "search_tools",
            "description": "Search the tools of every MCP server behind this gateway. Returns the best matches with a \
                one-line description and their parameters (`name?` = optional). Also searches the apps behind nested \
                gateways such as Composio. Call this first, then call_tool with a returned name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What you want to do, e.g. \"create github issue\" or \"read file\". \
                            Empty lists every tool."
                    },
                    "server": {"type": "string", "description": "Only search this server (ids from list_servers)."},
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Maximum results (default 10)."
                    }
                }
            },
            "annotations": {"title": "Search tools", "readOnlyHint": true}
        },
        {
            "name": "get_tool_schemas",
            "description": "Full description and JSON input schema of tools found with search_tools. Use it when the \
                parameter summary is not enough.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tools": {"type": "array", "items": {"type": "string"}, "description": "Tool names (server__tool)."}
                },
                "required": ["tools"]
            },
            "annotations": {"title": "Get tool schemas", "readOnlyHint": true}
        },
        {
            "name": "call_tool",
            "description": "Run a tool found with search_tools and return its result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tool": {"type": "string", "description": "Tool name (server__tool)."},
                    "arguments": {"type": "object", "description": "Arguments matching the tool's input schema."}
                },
                "required": ["tool"]
            },
            "annotations": {"title": "Call tool"}
        },
        {
            "name": "list_servers",
            "description": "The MCP servers behind this gateway, with what they are for and how many tools they have.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"title": "List servers", "readOnlyHint": true}
        }
    ])
}
