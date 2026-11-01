//! What the portable core needs from its host: an HTTP client ([`Http`]) and storage
//! ([`Store`], or [`SqlStore`] over any SQLite-dialect executor implementing [`Sql`]).
//!
//! The binary (`native`) implements them on `reqwest::Client` and the SQLite [`Db`];
//! a Cloudflare Worker implements them on `fetch` and D1 / Turso. With `native` the
//! traits and their futures are `Send` (tokio's multi-thread runtime); without it they
//! are not, since wasm futures holding JS values never are.
//!
//! [`Db`]: crate::db::Db

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use serde_json::{json, Map, Value};

use crate::db::{merge_settings, ApiKey, ProviderConnection, UsageRow};

/// `Send + Sync` with `native`, nothing on wasm.
#[cfg(feature = "native")]
pub trait MaybeSync: Send + Sync {}
#[cfg(feature = "native")]
impl<T: Send + Sync + ?Sized> MaybeSync for T {}
/// `Send + Sync` with `native`, nothing on wasm.
#[cfg(not(feature = "native"))]
pub trait MaybeSync {}
#[cfg(not(feature = "native"))]
impl<T: ?Sized> MaybeSync for T {}

/// A response body, chunk by chunk. Errors are plain text (they end up in logs and
/// in the error event sent to the client).
#[cfg(feature = "native")]
pub type ByteStream = futures_util::stream::BoxStream<'static, Result<Bytes, String>>;
/// A response body, chunk by chunk. Errors are plain text (they end up in logs and
/// in the error event sent to the client).
#[cfg(not(feature = "native"))]
pub type ByteStream = futures_util::stream::LocalBoxStream<'static, Result<Bytes, String>>;

/// One outgoing request. Upstream calls are always `POST` with a JSON body, but the
/// method is kept so hosts can reuse the client.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Deadline until the response headers arrive; `None` leaves it to the host.
    pub timeout_ms: Option<u64>,
}

pub struct HttpResponse {
    pub status: u16,
    /// Header names in any case; look them up with [`HttpResponse::header`].
    pub headers: Vec<(String, String)>,
    pub body: ByteStream,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn is_sse(&self) -> bool {
        self.header("content-type").is_some_and(|ct| ct.starts_with("text/event-stream"))
    }

    /// The whole body as text (lossy UTF-8).
    pub async fn text(self) -> Result<String, String> {
        use futures_util::StreamExt;
        let mut body = self.body;
        let mut buf = Vec::new();
        while let Some(chunk) = body.next().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// Outgoing HTTP. `Err` means no response at all (DNS, connect, TLS, timeout); any
/// HTTP status, 4xx/5xx included, is an `Ok` response.
#[cfg_attr(feature = "native", async_trait::async_trait)]
#[cfg_attr(not(feature = "native"), async_trait::async_trait(?Send))]
pub trait Http: MaybeSync {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, String>;
    /// Wait before retrying an upstream that asked for it (502/503/504 by default).
    async fn sleep(&self, ms: u64);
}

/// Everything a request reads before routing, fetched in one go so a host whose
/// storage is remote (Turso, D1) can answer with a single round trip.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Defaults merged with the stored settings (see [`crate::db::merge_settings`]).
    pub settings: Value,
    /// Every account, ordered by provider, `COALESCE(priority, 999)`, `createdAt`.
    pub connections: Vec<ProviderConnection>,
    /// The row for the API key the caller sent, if it asked for one and it exists.
    pub api_key: Option<ApiKey>,
    /// The requested `kv` scopes (`modelAliases`, `customModels`, `hiddenModels`).
    pub kv: HashMap<String, Map<String, Value>>,
}

impl Snapshot {
    pub fn scope(&self, name: &str) -> Map<String, Value> {
        self.kv.get(name).cloned().unwrap_or_default()
    }
}

/// What the router reads and writes. Tables and JSON shapes are 9router's
/// (`crate::db::SCHEMA`); [`SqlStore`] implements this over plain SQL.
#[cfg_attr(feature = "native", async_trait::async_trait)]
#[cfg_attr(not(feature = "native"), async_trait::async_trait(?Send))]
pub trait Store: MaybeSync {
    async fn snapshot(&self, api_key: Option<&str>, kv_scopes: &[&str]) -> Result<Snapshot>;
    /// Shallow-merge `patch` into the account's `data` JSON; `null` values delete keys.
    async fn patch_connection_data(&self, id: &str, patch: &Value) -> Result<()>;
    /// Append a `usageHistory` row and bump the lifetime request counter.
    async fn insert_usage(&self, row: &UsageRow) -> Result<()>;
}

/// One SQL statement with positional (`?1`, `?2`, …) arguments.
#[derive(Debug, Clone)]
pub struct Stmt {
    pub sql: String,
    /// `Null`, numbers and strings (booleans are sent as 0 / 1).
    pub args: Vec<Value>,
}

impl Stmt {
    pub fn new(sql: impl Into<String>, args: Vec<Value>) -> Stmt {
        Stmt { sql: sql.into(), args }
    }
}

/// Rows of one statement; each row holds its column values in order as JSON
/// (`NULL` -> `Null`, `INTEGER` / `REAL` -> number, or a numeric string as Turso's HTTP
/// API returns them, `TEXT` -> string).
pub type Rows = Vec<Vec<Value>>;

/// A SQLite-dialect executor (Turso / libSQL over HTTP, Cloudflare D1, rusqlite).
#[cfg_attr(feature = "native", async_trait::async_trait)]
#[cfg_attr(not(feature = "native"), async_trait::async_trait(?Send))]
pub trait Sql: MaybeSync {
    /// Run the statements in order, ideally in one round trip, and return the rows of
    /// each (empty for statements without results).
    async fn batch(&self, stmts: Vec<Stmt>) -> Result<Vec<Rows>>;
}

/// [`Store`] over any [`Sql`] executor, with the binary's schema. Call
/// [`SqlStore::migrate`] once (it is idempotent) to create the tables.
pub struct SqlStore<E> {
    pub sql: E,
}

const CONN_COLS: &str = "id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt";
const KEY_COLS: &str = "id, key, name, isActive, createdAt, accessRestricted, accessAllow";

fn cell_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn cell_i64(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

fn cell_json(v: Option<&Value>) -> Option<Value> {
    cell_str(v).and_then(|s| serde_json::from_str(&s).ok())
}

/// A `providerConnections` row selected with the binary's column order.
pub fn row_to_connection(r: &[Value]) -> ProviderConnection {
    ProviderConnection {
        id: cell_str(r.first()).unwrap_or_default(),
        provider: cell_str(r.get(1)).unwrap_or_default(),
        auth_type: cell_str(r.get(2)).unwrap_or_else(|| "apikey".into()),
        name: cell_str(r.get(3)),
        email: cell_str(r.get(4)),
        priority: cell_i64(r.get(5)),
        is_active: cell_i64(r.get(6)).unwrap_or(1) != 0,
        data: cell_json(r.get(7)).unwrap_or_else(|| json!({})),
        created_at: cell_str(r.get(8)).unwrap_or_default(),
        updated_at: cell_str(r.get(9)).unwrap_or_default(),
    }
}

fn row_to_key(r: &[Value]) -> ApiKey {
    ApiKey {
        id: cell_str(r.first()).unwrap_or_default(),
        key: cell_str(r.get(1)).unwrap_or_default(),
        name: cell_str(r.get(2)),
        is_active: cell_i64(r.get(3)).unwrap_or(0) != 0,
        created_at: cell_str(r.get(4)).unwrap_or_default(),
        access_restricted: cell_i64(r.get(5)).unwrap_or(0) != 0,
        access_allow: cell_json(r.get(6)).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default(),
    }
}

impl<E: Sql> SqlStore<E> {
    pub fn new(sql: E) -> SqlStore<E> {
        SqlStore { sql }
    }

    /// Create the tables and indexes if they are missing.
    pub async fn migrate(&self) -> Result<()> {
        let stmts = crate::db::SCHEMA
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Stmt::new(s, vec![]))
            .collect();
        self.sql.batch(stmts).await.map(|_| ())
    }
}

#[cfg_attr(feature = "native", async_trait::async_trait)]
#[cfg_attr(not(feature = "native"), async_trait::async_trait(?Send))]
impl<E: Sql> Store for SqlStore<E> {
    async fn snapshot(&self, api_key: Option<&str>, kv_scopes: &[&str]) -> Result<Snapshot> {
        let mut stmts = vec![
            Stmt::new("SELECT data FROM settings WHERE id=1", vec![]),
            Stmt::new(
                format!(
                    "SELECT {CONN_COLS} FROM providerConnections ORDER BY provider, COALESCE(priority, 999), createdAt"
                ),
                vec![],
            ),
            Stmt::new(format!("SELECT {KEY_COLS} FROM apiKeys WHERE key=?1"), vec![json!(api_key.unwrap_or(""))]),
        ];
        for scope in kv_scopes {
            stmts.push(Stmt::new("SELECT key, value FROM kv WHERE scope=?1", vec![json!(scope)]));
        }
        let mut res = self.sql.batch(stmts).await?.into_iter();
        let mut next = || res.next().ok_or_else(|| anyhow!("SQL batch returned fewer results than statements"));
        let settings = merge_settings(next()?.first().and_then(|r| cell_json(r.first())));
        let connections = next()?.iter().map(|r| row_to_connection(r)).collect();
        let key_rows = next()?;
        let api_key = api_key.and_then(|_| key_rows.first().map(|r| row_to_key(r)));
        let mut kv = HashMap::new();
        for scope in kv_scopes {
            let mut m = Map::new();
            for r in next()? {
                if let Some(k) = cell_str(r.first()) {
                    m.insert(k, cell_json(r.get(1)).unwrap_or(Value::Null));
                }
            }
            kv.insert(scope.to_string(), m);
        }
        Ok(Snapshot { settings, connections, api_key, kv })
    }

    async fn patch_connection_data(&self, id: &str, patch: &Value) -> Result<()> {
        // json_patch (RFC 7396) deletes keys set to null; the router's patches only hold
        // scalars, so this matches `Db::patch_connection_data` in one statement.
        let sql = "UPDATE providerConnections SET data=json_patch(COALESCE(data, '{}'), ?2), updatedAt=?3 WHERE id=?1";
        let args = vec![json!(id), json!(patch.to_string()), json!(crate::db::now_iso())];
        self.sql.batch(vec![Stmt::new(sql, args)]).await.map(|_| ())
    }

    async fn insert_usage(&self, u: &UsageRow) -> Result<()> {
        let insert = Stmt::new(
            "INSERT INTO usageHistory(timestamp,provider,model,connectionId,apiKey,endpoint,promptTokens,completionTokens,cost,status,tokens,meta)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            vec![
                json!(u.timestamp),
                json!(u.provider),
                json!(u.model),
                json!(u.connection_id),
                json!(u.api_key),
                json!(u.endpoint),
                json!(u.prompt_tokens),
                json!(u.completion_tokens),
                json!(u.cost),
                json!(u.status),
                json!(u.tokens.to_string()),
                json!(u.meta.to_string()),
            ],
        );
        let counter = Stmt::new(
            "INSERT INTO _meta(key,value) VALUES('totalRequestsLifetime','1')
             ON CONFLICT(key) DO UPDATE SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT)",
            vec![],
        );
        self.sql.batch(vec![insert, counter]).await.map(|_| ())
    }
}

#[cfg(feature = "native")]
mod native {
    use std::time::Duration;

    use futures_util::StreamExt;
    use rusqlite::types::Value as SqlValue;

    use super::*;
    use crate::db::Db;

    #[async_trait::async_trait]
    impl Http for reqwest::Client {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, String> {
            let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| e.to_string())?;
            let mut rb = self.request(method, &req.url).body(req.body);
            for (k, v) in &req.headers {
                rb = rb.header(k.as_str(), v.as_str());
            }
            if let Some(ms) = req.timeout_ms {
                rb = rb.timeout(Duration::from_millis(ms));
            }
            let resp = rb.send().await.map_err(|e| {
                let kind = if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect"
                } else {
                    "request"
                };
                format!("{kind} error: {e}")
            })?;
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
                .collect();
            let body = resp.bytes_stream().map(|r| r.map_err(|e| e.to_string())).boxed();
            Ok(HttpResponse { status, headers, body })
        }

        async fn sleep(&self, ms: u64) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }

    /// The binary's SQLite database as a [`Store`] (synchronous calls; each is
    /// sub-millisecond).
    #[async_trait::async_trait]
    impl Store for Db {
        async fn snapshot(&self, api_key: Option<&str>, kv_scopes: &[&str]) -> Result<Snapshot> {
            let mut kv = HashMap::new();
            for s in kv_scopes {
                kv.insert(s.to_string(), self.kv_scope(s)?);
            }
            Ok(Snapshot {
                settings: self.settings(),
                connections: self.all_connections()?,
                api_key: match api_key {
                    Some(k) => self.api_key(k)?,
                    None => None,
                },
                kv,
            })
        }

        async fn patch_connection_data(&self, id: &str, patch: &Value) -> Result<()> {
            Db::patch_connection_data(self, id, patch)
        }

        async fn insert_usage(&self, row: &UsageRow) -> Result<()> {
            Db::insert_usage(self, row)
        }
    }

    fn to_sql(v: &Value) -> SqlValue {
        match v {
            Value::Null => SqlValue::Null,
            Value::Bool(b) => SqlValue::Integer(*b as i64),
            Value::Number(n) => match n.as_i64() {
                Some(i) => SqlValue::Integer(i),
                None => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
            },
            Value::String(s) => SqlValue::Text(s.clone()),
            other => SqlValue::Text(other.to_string()),
        }
    }

    fn from_sql(v: SqlValue) -> Value {
        match v {
            SqlValue::Null => Value::Null,
            SqlValue::Integer(i) => Value::from(i),
            SqlValue::Real(f) => Value::from(f),
            SqlValue::Text(s) => Value::String(s),
            SqlValue::Blob(b) => Value::String(String::from_utf8_lossy(&b).into_owned()),
        }
    }

    /// The SQLite database as a raw [`Sql`] executor, so [`SqlStore`] runs (and is
    /// tested) against real SQLite.
    #[async_trait::async_trait]
    impl Sql for Db {
        async fn batch(&self, stmts: Vec<Stmt>) -> Result<Vec<Rows>> {
            self.with(|c| {
                let mut out = Vec::with_capacity(stmts.len());
                for s in &stmts {
                    let mut st = c.prepare(&s.sql)?;
                    let n = st.column_count();
                    let args: Vec<SqlValue> = s.args.iter().map(to_sql).collect();
                    let mut rows = st.query(rusqlite::params_from_iter(args.iter()))?;
                    let mut res = Vec::new();
                    while let Some(r) = rows.next()? {
                        res.push(
                            (0..n).map(|i| r.get::<_, SqlValue>(i).map(from_sql)).collect::<rusqlite::Result<_>>()?,
                        );
                    }
                    out.push(res);
                }
                Ok(out)
            })
        }
    }
}
