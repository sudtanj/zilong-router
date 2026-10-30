//! The gateway's own MCP endpoint (Streamable HTTP, stateless, JSON answers).
//!
//! * `POST /mcp`: four meta tools (`search_tools`, `get_tool_schemas`, `call_tool`,
//!   `list_servers`), so the agent's context holds four tool definitions however many
//!   servers sit behind the gateway.
//! * `POST /mcp/all`: every tool of every server, named `<server>__<tool>`.
//!
//! Authentication is the API key of `/v1` (or a loopback client when keys are not
//! required); a restricted key only sees the servers on its allow list (`mcp/<id>`).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::future::join_all;
use serde_json::{json, Value};

use super::{params_summary, search, tool_description, tool_name, ServerConfig, SEP};
use crate::auth::{authenticate, AuthError, Caller};
use crate::state::AppState;

const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Meta,
    All,
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
        let names: Vec<String> = servers
            .iter()
            .map(|s| if s.description.is_empty() { s.id.clone() } else { format!("{} ({})", s.id, s.description) })
            .collect();
        let instructions = match self.mode {
            Mode::Meta => format!(
                "zilong-router MCP gateway in front of {} MCP server(s): {}. Their tools are not listed directly: \
                 call search_tools with what you want to do, get_tool_schemas for the exact parameters if needed, \
                 then call_tool with the tool name (server__tool) and its arguments.",
                servers.len(),
                names.join(", ")
            ),
            Mode::All => format!(
                "zilong-router MCP gateway: the tools of {} MCP server(s) ({}), named server__tool.",
                servers.len(),
                names.join(", ")
            ),
        };
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
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

    async fn list_tools(&self) -> Value {
        if self.mode == Mode::Meta {
            return json!({"tools": meta_tools()});
        }
        let (catalogs, _) = self.catalogs(&self.servers()).await;
        let tools: Vec<Value> = catalogs
            .iter()
            .flat_map(|(server, tools)| {
                tools.iter().map(move |t| {
                    let mut t = t.clone();
                    t["name"] = json!(format!("{server}{SEP}{}", tool_name(&t)));
                    t["description"] = json!(format!("[{server}] {}", tool_description(&t)));
                    t
                })
            })
            .collect();
        json!({"tools": tools})
    }

    async fn call(&self, params: &Value) -> Result<Value, RpcError> {
        let name = params.get("name").and_then(Value::as_str).ok_or_else(|| invalid("name is required"))?;
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        if self.mode == Mode::All {
            let unknown = || invalid(format!("unknown tool: {name}"));
            let (server, tool) = name.split_once(SEP).ok_or_else(unknown)?;
            let cfg = self.servers().into_iter().find(|s| s.id == server).ok_or_else(unknown)?;
            return Ok(self.call_upstream(&cfg, tool, args).await);
        }
        Ok(match name {
            "search_tools" => self.search_tools(&args).await,
            "get_tool_schemas" => self.get_tool_schemas(&args).await,
            "call_tool" => self.call_tool(&args).await,
            "list_servers" => self.list_servers().await,
            _ => return Err(invalid(format!("unknown tool: {name}"))),
        })
    }

    async fn call_upstream(&self, cfg: &ServerConfig, tool: &str, args: Value) -> Value {
        match self.state.mcp.call(self.state, cfg, tool, args).await {
            Ok(r) => r,
            Err(e) => text_result(format!("{}{SEP}{tool} failed: {e:#}", cfg.id), true),
        }
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
            [] => Err(format!("unknown tool `{name}`: use search_tools to find tool names (server__tool)")),
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
        if let Some(only) = args.get("server").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            servers.retain(|s| s.id == only);
            if servers.is_empty() {
                return text_result(format!("unknown server `{only}`; call list_servers"), true);
            }
        }
        let (catalogs, unavailable) = self.catalogs(&servers).await;
        let results: Vec<Value> = search(&catalogs, query, limit)
            .into_iter()
            .map(|(server, t)| {
                let desc: String = tool_description(t).chars().take(300).collect();
                json!({
                    "tool": format!("{server}{SEP}{}", tool_name(t)),
                    "description": desc,
                    "parameters": params_summary(t.get("inputSchema").unwrap_or(&Value::Null)),
                })
            })
            .collect();
        let empty = results.is_empty();
        let mut out = json!({"results": results});
        if empty {
            out["hint"] = json!("No match. Try other words, an empty query to list every tool, or list_servers.");
        }
        if !unavailable.is_empty() {
            out["unavailable"] = json!(unavailable);
        }
        json_result(out)
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
        let (cfg, tool) = self.resolve(name, servers).await?;
        let tools = self.state.mcp.tools(self.state, &cfg, false).await.map_err(|e| format!("{e:#}"))?;
        let t = tools.iter().find(|t| tool_name(t) == tool).ok_or("no such tool on this server")?;
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

    async fn call_tool(&self, args: &Value) -> Value {
        let Some(name) = args.get("tool").and_then(Value::as_str) else {
            return text_result("tool is required (server__tool, from search_tools)".into(), true);
        };
        let arguments = match args.get("arguments") {
            None | Some(Value::Null) => json!({}),
            // Some models send the arguments as a JSON string.
            Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                Ok(v) if v.is_object() => v,
                _ => return text_result("arguments must be a JSON object".into(), true),
            },
            Some(v) if v.is_object() => v.clone(),
            Some(_) => return text_result("arguments must be a JSON object".into(), true),
        };
        match self.resolve(name, &self.servers()).await {
            Ok((cfg, tool)) => self.call_upstream(&cfg, &tool, arguments).await,
            Err(e) => text_result(e, true),
        }
    }

    async fn list_servers(&self) -> Value {
        let servers = self.servers();
        let list: Vec<Value> = servers
            .iter()
            .map(|s| {
                let tools = super::cached_tools(&self.state.db, &s.id).map(|c| c.tools.len());
                json!({"server": s.id, "name": s.name, "description": s.description, "tools": tools})
            })
            .collect();
        json_result(json!({"servers": list}))
    }
}

fn meta_tools() -> Value {
    json!([
        {
            "name": "search_tools",
            "description": "Search the tools of every MCP server behind this gateway. Returns the best matches with a \
                one-line description and their parameters (`name?` = optional). Call this first, then call_tool.",
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
