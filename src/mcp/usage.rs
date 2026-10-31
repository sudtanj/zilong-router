//! Usage counters for the MCP gateway: one `mcpUsage` row per tool call a client makes
//! through `/mcp` or `/mcp/all` (who called, which server and tool, how it ended, how
//! long it took, how many bytes went each way). Written on the request path like LLM
//! usage (`crate::usage`): a single indexed INSERT, well under a millisecond.

use std::time::Instant;

use serde_json::Value;

use super::client::{RpcError, Timeout};
use crate::auth::Caller;
use crate::db::{now_iso, McpUsageRow};
use crate::state::AppState;

/// A `tools/call` sent to an upstream server.
pub const CALL: &str = "call";
/// A gateway meta tool (`search_tools`, `get_tool_schemas`, `list_servers`, or a
/// `call_tool` that never reached a server).
pub const META: &str = "meta";

pub const OK: &str = "ok";
pub const TOOL_ERROR: &str = "tool_error";

/// One call, as seen by the gateway; who made it is passed to [`record`] separately.
pub struct Call<'a> {
    pub kind: &'a str,
    pub server: Option<&'a str>,
    pub tool: &'a str,
    pub status: &'a str,
    pub error: Option<String>,
    pub started: Instant,
    pub request_bytes: usize,
    pub response_bytes: usize,
}

/// How an upstream call ended: `ok`, `tool_error` (an `isError` result), `rpc_error`
/// (a JSON-RPC error), `timeout`, or `transport` (anything else: unreachable, HTTP
/// errors, sign-in needed); and the error text.
pub fn outcome(result: &anyhow::Result<Value>) -> (&'static str, Option<String>) {
    match result {
        Ok(r) => match super::tool_error(r) {
            Some(e) => (TOOL_ERROR, Some(e)),
            None => (OK, None),
        },
        Err(e) => {
            let status = if e.chain().any(|c| c.is::<Timeout>()) {
                "timeout"
            } else if e.chain().any(|c| c.is::<RpcError>()) {
                "rpc_error"
            } else {
                "transport"
            };
            (status, Some(format!("{e:#}")))
        }
    }
}

/// Size of `v` as JSON, without building the string.
pub fn json_len(v: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, v);
    count.0
}

/// Saves one call; a failure is logged, never returned to the client.
pub fn record(state: &AppState, caller: &Caller, endpoint: &str, c: Call<'_>) {
    let (api_key_id, api_key_name) = match caller {
        Caller::Key(k) => {
            let name = k.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| crate::usage::mask_key(&k.key));
            (Some(k.id.clone()), name)
        }
        Caller::Local => (None, "local".to_string()),
    };
    let row = McpUsageRow {
        timestamp: now_iso(),
        kind: c.kind.to_string(),
        server: c.server.map(str::to_string),
        tool: c.tool.to_string(),
        api_key_id,
        api_key_name,
        endpoint: endpoint.to_string(),
        status: c.status.to_string(),
        error: c.error.map(|e| e.chars().take(300).collect()),
        duration_ms: c.started.elapsed().as_millis() as i64,
        request_bytes: c.request_bytes as i64,
        response_bytes: c.response_bytes as i64,
    };
    if let Err(e) = state.db.insert_mcp_usage(&row) {
        tracing::warn!("mcp usage insert failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_outcomes() {
        assert_eq!(outcome(&Ok(json!({"content": []}))).0, OK);
        let failed = json!({"content": [{"type": "text", "text": "boom"}], "isError": true});
        assert_eq!(outcome(&Ok(failed)), (TOOL_ERROR, Some("boom".to_string())));
        let rpc = anyhow::Error::from(RpcError { code: Some(-32601), message: "nope".into() });
        assert_eq!(outcome(&Err(rpc.context("initialize"))).0, "rpc_error");
        assert_eq!(outcome(&Err(Timeout(5).into())), ("timeout", Some("no answer within 5 s".to_string())));
        assert_eq!(outcome(&Err(anyhow::anyhow!("connection refused"))).0, "transport");
    }

    #[test]
    fn measures_json() {
        let v = json!({"a": [1, "x"]});
        assert_eq!(json_len(&v), v.to_string().len());
    }
}
