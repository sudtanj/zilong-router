//! Offsite copy of the database on Turso (hosted libSQL).
//!
//! The router keeps using the local SQLite file for every request, so latency is
//! unchanged. This module talks to Turso over its HTTP pipeline API (`/v2/pipeline`,
//! plain `reqwest`, no extra C dependency) and does two things:
//!
//! * on start-up, if the local database is empty (new board, wiped SD card), it pulls
//!   everything back from Turso;
//! * every `turso_sync_secs` it pushes what changed: config tables row by row, and
//!   `usageHistory` incrementally by id.
//!
//! One writer is assumed: the board is the source of truth, Turso is the replica.
//! Local usage pruning is not propagated, so Turso keeps the full history.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};

use crate::config::Config;
use crate::state::AppState;

struct Table {
    name: &'static str,
    cols: &'static [&'static str],
    /// Indices into `cols` that form the primary key.
    key: &'static [usize],
}

const TABLES: &[Table] = &[
    Table { name: "settings", cols: &["id", "data"], key: &[0] },
    Table {
        name: "providerConnections",
        cols: &[
            "id",
            "provider",
            "authType",
            "name",
            "email",
            "priority",
            "isActive",
            "data",
            "createdAt",
            "updatedAt",
        ],
        key: &[0],
    },
    Table { name: "providerNodes", cols: &["id", "type", "name", "data", "createdAt", "updatedAt"], key: &[0] },
    Table { name: "proxyPools", cols: &["id", "isActive", "testStatus", "data", "createdAt", "updatedAt"], key: &[0] },
    Table {
        name: "apiKeys",
        cols: &["id", "key", "name", "machineId", "isActive", "createdAt", "accessRestricted", "accessAllow"],
        key: &[0],
    },
    Table { name: "combos", cols: &["id", "name", "kind", "models", "createdAt", "updatedAt"], key: &[0] },
    Table { name: "kv", cols: &["scope", "key", "value"], key: &[0, 1] },
    Table { name: "usageDaily", cols: &["dateKey", "data"], key: &[0] },
];

const USAGE_COLS: &[&str] = &[
    "id",
    "timestamp",
    "provider",
    "model",
    "connectionId",
    "apiKey",
    "endpoint",
    "promptTokens",
    "completionTokens",
    "cost",
    "status",
    "tokens",
    "meta",
];

/// Statements per HTTP round trip.
const CHUNK: usize = 200;

pub struct Turso {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Turso {
    pub fn from_config(cfg: &Config) -> Option<Turso> {
        let url = cfg.turso_url.as_deref()?.trim();
        let token = cfg.turso_token.as_deref()?.trim();
        if url.is_empty() || token.is_empty() {
            return None;
        }
        let base = match url.strip_prefix("libsql://") {
            Some(rest) => format!("https://{rest}"),
            None => url.to_string(),
        };
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).use_rustls_tls().build().ok()?;
        Some(Turso { http, base: base.trim_end_matches('/').to_string(), token: token.to_string() })
    }

    async fn pipeline(&self, mut reqs: Vec<Value>) -> Result<Vec<Value>> {
        let n = reqs.len();
        reqs.push(json!({"type": "close"}));
        let resp = self
            .http
            .post(format!("{}/v2/pipeline", self.base))
            .bearer_auth(&self.token)
            .json(&json!({ "requests": reqs }))
            .send()
            .await
            .context("turso request failed")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("turso HTTP {status}: {}", body.get("error").and_then(Value::as_str).unwrap_or("no detail"));
        }
        let results = body.get("results").and_then(Value::as_array).ok_or_else(|| anyhow!("turso: malformed reply"))?;
        let mut out = Vec::with_capacity(n);
        for r in results.iter().take(n) {
            if r.get("type").and_then(Value::as_str) != Some("ok") {
                let m = r.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown error");
                bail!("turso: {m}");
            }
            out.push(r.pointer("/response/result").cloned().unwrap_or(Value::Null));
        }
        Ok(out)
    }

    /// Run statements in order. Not atomic: callers only send idempotent upserts and
    /// deletes and retry the whole batch on failure.
    async fn run(&self, stmts: Vec<(String, Vec<SqlValue>)>) -> Result<()> {
        for chunk in stmts.chunks(CHUNK) {
            let reqs = chunk
                .iter()
                .map(|(sql, args)| {
                    json!({"type": "execute", "stmt": {"sql": sql, "args": args.iter().map(arg).collect::<Vec<_>>()}})
                })
                .collect();
            self.pipeline(reqs).await?;
        }
        Ok(())
    }

    async fn query(&self, sql: &str, args: &[SqlValue]) -> Result<Vec<Vec<SqlValue>>> {
        let req = json!({"type": "execute", "stmt": {"sql": sql, "args": args.iter().map(arg).collect::<Vec<_>>()}});
        let res = self.pipeline(vec![req]).await?.pop().unwrap_or(Value::Null);
        Ok(res
            .get("rows")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter().map(|r| r.as_array().map(|c| c.iter().map(cell).collect()).unwrap_or_default()).collect()
            })
            .unwrap_or_default())
    }

    async fn ensure_schema(&self) -> Result<()> {
        self.pipeline(vec![json!({"type": "sequence", "sql": crate::db::SCHEMA})]).await?;
        Ok(())
    }
}

fn arg(v: &SqlValue) -> Value {
    match v {
        SqlValue::Integer(i) => json!({"type": "integer", "value": i.to_string()}),
        SqlValue::Real(f) => json!({"type": "float", "value": f}),
        SqlValue::Text(s) => json!({"type": "text", "value": s}),
        SqlValue::Null | SqlValue::Blob(_) => json!({"type": "null"}),
    }
}

fn cell(v: &Value) -> SqlValue {
    match v.get("type").and_then(Value::as_str) {
        Some("integer") => v.get("value").and_then(Value::as_str).and_then(|s| s.parse().ok()).map(SqlValue::Integer),
        Some("float") => v.get("value").and_then(Value::as_f64).map(SqlValue::Real),
        Some("text") => Some(SqlValue::Text(v.get("value").and_then(Value::as_str).unwrap_or("").to_string())),
        _ => None,
    }
    .unwrap_or(SqlValue::Null)
}

fn upsert_sql(table: &str, cols: &[&str]) -> String {
    let names = cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",");
    let marks = vec!["?"; cols.len()].join(",");
    format!("INSERT OR REPLACE INTO \"{table}\" ({names}) VALUES ({marks})")
}

fn row_key(t: &Table, row: &[SqlValue]) -> String {
    t.key.iter().map(|&i| format!("{:?}", row[i])).collect::<Vec<_>>().join("\u{1f}")
}

fn where_key(t: &Table) -> String {
    t.key.iter().map(|&i| format!("\"{}\"=?", t.cols[i])).collect::<Vec<_>>().join(" AND ")
}

fn select_all(t: &Table) -> String {
    let cols = t.cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",");
    format!("SELECT {cols} FROM \"{}\"", t.name)
}

/// Push state kept between rounds.
pub struct Mirror {
    remote: Turso,
    /// Per table: (serialised row, primary-key cells) already on Turso, by key. Missing means
    /// absent (first round), so the table is replaced wholesale.
    known: HashMap<&'static str, HashMap<String, (String, Vec<SqlValue>)>>,
    last_usage_id: Option<i64>,
    ready: bool,
}

impl Mirror {
    async fn restore_if_blank(&mut self, state: &AppState) -> Result<()> {
        if !state.db.is_blank() {
            return Ok(());
        }
        let mut restored = 0usize;
        for t in TABLES {
            let rows = self.remote.query(&select_all(t), &[]).await?;
            restored += rows.len();
            if !rows.is_empty() {
                state.db.put_rows(t.name, t.cols, &rows)?;
            }
        }
        let usage_cols = USAGE_COLS.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",");
        // Only the retained window comes back; older history stays on Turso.
        let cutoff = (time::OffsetDateTime::now_utc() - time::Duration::days(state.cfg.usage_keep_days()))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let sql =
            format!("SELECT {usage_cols} FROM usageHistory WHERE id > ? AND timestamp >= ? ORDER BY id LIMIT 1000");
        let mut last = 0i64;
        loop {
            let rows = self.remote.query(&sql, &[SqlValue::Integer(last), SqlValue::Text(cutoff.clone())]).await?;
            let Some(SqlValue::Integer(id)) = rows.last().and_then(|r| r.first()) else { break };
            last = *id;
            restored += rows.len();
            state.db.put_rows("usageHistory", USAGE_COLS, &rows)?;
        }
        // New local ids must continue after everything on Turso, not after the window.
        if let Some(SqlValue::Integer(max)) = self
            .remote
            .query("SELECT COALESCE(MAX(id),0) FROM usageHistory", &[])
            .await?
            .first()
            .and_then(|r| r.first())
        {
            state.db.set_usage_seq(*max)?;
            self.last_usage_id = Some(*max);
        }
        if restored > 0 {
            tracing::info!("restored {restored} rows from Turso into the empty local database");
        }
        Ok(())
    }

    async fn push_table(&mut self, state: &AppState, t: &Table) -> Result<()> {
        let rows = state.db.rows(&select_all(t), &[])?;
        // key -> (serialised row, primary-key cells, row)
        let now: HashMap<String, (String, Vec<SqlValue>, Vec<SqlValue>)> = rows
            .into_iter()
            .map(|r| (row_key(t, &r), (format!("{r:?}"), t.key.iter().map(|&i| r[i].clone()).collect(), r)))
            .collect();
        let mut stmts: Vec<(String, Vec<SqlValue>)> = Vec::new();
        let sql = upsert_sql(t.name, t.cols);
        match self.known.get(t.name) {
            None => {
                stmts.push((format!("DELETE FROM \"{}\"", t.name), vec![]));
                stmts.extend(now.values().map(|(_, _, r)| (sql.clone(), r.clone())));
            }
            Some(prev) => {
                for (k, (ser, _, r)) in &now {
                    if prev.get(k).map(|p| &p.0) != Some(ser) {
                        stmts.push((sql.clone(), r.clone()));
                    }
                }
                let del = format!("DELETE FROM \"{}\" WHERE {}", t.name, where_key(t));
                for (k, (_, key_cells)) in prev {
                    if !now.contains_key(k) {
                        stmts.push((del.clone(), key_cells.clone()));
                    }
                }
            }
        }
        if !stmts.is_empty() {
            self.remote.run(stmts).await?;
        }
        self.known.insert(t.name, now.into_iter().map(|(k, (ser, key, _))| (k, (ser, key))).collect());
        Ok(())
    }

    async fn push_usage(&mut self, state: &AppState) -> Result<()> {
        let mut last = match self.last_usage_id {
            Some(v) => v,
            None => match self.remote.query("SELECT COALESCE(MAX(id),0) FROM usageHistory", &[]).await?.first() {
                Some(r) => match r.first() {
                    Some(SqlValue::Integer(i)) => *i,
                    _ => 0,
                },
                None => 0,
            },
        };
        let cols = USAGE_COLS.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",");
        let sel = format!("SELECT {cols} FROM usageHistory WHERE id > ? ORDER BY id LIMIT 500");
        let sql = upsert_sql("usageHistory", USAGE_COLS);
        loop {
            let rows = state.db.rows(&sel, &[SqlValue::Integer(last)])?;
            let Some(SqlValue::Integer(newest)) = rows.last().and_then(|r| r.first()) else { break };
            let newest = *newest;
            self.remote.run(rows.into_iter().map(|r| (sql.clone(), r)).collect()).await?;
            last = newest;
            self.last_usage_id = Some(last);
        }
        self.last_usage_id = Some(last);
        Ok(())
    }

    async fn round(&mut self, state: &AppState) -> Result<()> {
        for t in TABLES {
            self.push_table(state, t).await.with_context(|| format!("pushing {}", t.name))?;
        }
        self.push_usage(state).await.context("pushing usageHistory")
    }
}

/// Connect to Turso if configured: create the remote schema and restore into a blank
/// local database. Called before the listener opens so an in-memory database is
/// populated before the first request.
pub async fn init(state: &AppState) -> Option<Mirror> {
    let remote = Turso::from_config(&state.cfg)?;
    let mut mirror = Mirror { remote, known: HashMap::new(), last_usage_id: None, ready: false };
    mirror.connect(state).await;
    Some(mirror)
}

impl Mirror {
    /// Pushing is held back until this succeeds once: pushing an unrestored (blank)
    /// local database would wipe the copy on Turso.
    async fn connect(&mut self, state: &AppState) {
        let res = async {
            self.remote.ensure_schema().await?;
            self.restore_if_blank(state).await
        }
        .await;
        match res {
            Ok(()) => {
                self.ready = true;
                tracing::info!("turso: connected, mirroring every {}s", state.cfg.turso_sync_secs);
            }
            Err(e) => tracing::warn!("turso: not connected yet, pushes paused: {e:#}"),
        }
    }
}

/// Push changes forever.
pub type Shared = Arc<tokio::sync::Mutex<Mirror>>;

pub async fn sync_loop(state: Arc<AppState>, shared: Shared) {
    let mut tick = tokio::time::interval(Duration::from_secs(state.cfg.turso_sync_secs.max(5)));
    loop {
        tick.tick().await;
        let mut mirror = shared.lock().await;
        if !mirror.ready {
            mirror.connect(&state).await;
            continue;
        }
        if let Err(e) = mirror.round(&state).await {
            tracing::warn!("turso sync failed: {e:#}");
        }
    }
}

/// Final push on shutdown so a clean stop loses nothing.
pub async fn flush(state: &AppState, shared: &Shared) {
    let mut mirror = shared.lock().await;
    if mirror.ready {
        if let Err(e) = tokio::time::timeout(Duration::from_secs(10), mirror.round(state))
            .await
            .map_err(|_| anyhow!("timed out"))
            .and_then(|r| r)
        {
            tracing::warn!("turso final sync failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_roundtrip() {
        for v in [SqlValue::Integer(-5), SqlValue::Real(1.5), SqlValue::Text("a\"b".into()), SqlValue::Null] {
            assert_eq!(cell(&arg(&v)), v);
        }
    }
}
