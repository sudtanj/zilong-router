//! SQLite persistence. The schema is 9router's (`src/lib/db/schema.js`) so an
//! existing `~/.9router/db/data.sqlite` opens unchanged. Payloads are JSON blobs in
//! a `data` column, exactly like upstream.
//!
//! A single connection behind a mutex is enough here: every query is sub-millisecond
//! and the box serves one developer, not a fleet.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConnection {
    pub id: String,
    pub provider: String,
    pub auth_type: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub priority: Option<i64>,
    pub is_active: bool,
    /// apiKey / accessToken / refreshToken / expiresAt / providerSpecificData /
    /// modelLock_* / testStatus / lastError / errorCode / backoffLevel / lastUsedAt /
    /// consecutiveUseCount — same keys as upstream.
    pub data: Value,
    pub created_at: String,
    pub updated_at: String,
}

impl ProviderConnection {
    pub fn secret(&self) -> Option<&str> {
        self.data
            .get("apiKey")
            .or_else(|| self.data.get("accessToken"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }
    pub fn base_url_override(&self) -> Option<&str> {
        self.data
            .pointer("/providerSpecificData/baseUrl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
    pub fn label(&self) -> String {
        self.name.clone().or_else(|| self.email.clone()).unwrap_or_else(|| self.id.chars().take(8).collect())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKey {
    pub id: String,
    pub key: String,
    pub name: Option<String>,
    pub is_active: bool,
    pub created_at: String,
    pub access_restricted: bool,
    pub access_allow: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    pub connection_id: Option<String>,
    pub api_key: Option<String>,
    pub endpoint: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost: f64,
    pub status: String,
    pub tokens: Value,
    pub meta: Value,
}

pub fn now_iso() -> String {
    time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS _meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK (id = 1), data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS providerConnections (
  id TEXT PRIMARY KEY, provider TEXT NOT NULL, authType TEXT, name TEXT, email TEXT,
  priority INTEGER, isActive INTEGER NOT NULL DEFAULT 1, data TEXT NOT NULL DEFAULT '{}',
  createdAt TEXT, updatedAt TEXT);
CREATE INDEX IF NOT EXISTS idx_pc_provider ON providerConnections(provider);
CREATE INDEX IF NOT EXISTS idx_pc_provider_active ON providerConnections(provider, isActive);
CREATE INDEX IF NOT EXISTS idx_pc_provider_priority ON providerConnections(provider, priority);
CREATE TABLE IF NOT EXISTS providerNodes (id TEXT PRIMARY KEY, type TEXT, name TEXT, data TEXT, createdAt TEXT, updatedAt TEXT);
CREATE TABLE IF NOT EXISTS proxyPools (id TEXT PRIMARY KEY, isActive INTEGER, testStatus TEXT, data TEXT, createdAt TEXT, updatedAt TEXT);
CREATE TABLE IF NOT EXISTS apiKeys (
  id TEXT PRIMARY KEY, key TEXT UNIQUE NOT NULL, name TEXT, machineId TEXT,
  isActive INTEGER NOT NULL DEFAULT 1, createdAt TEXT,
  accessRestricted INTEGER NOT NULL DEFAULT 0, accessAllow TEXT);
CREATE TABLE IF NOT EXISTS combos (id TEXT PRIMARY KEY, name TEXT UNIQUE, kind TEXT, models TEXT, createdAt TEXT, updatedAt TEXT);
CREATE TABLE IF NOT EXISTS kv (scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT, PRIMARY KEY (scope, key));
CREATE TABLE IF NOT EXISTS usageHistory (
  id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp TEXT, provider TEXT, model TEXT,
  connectionId TEXT, apiKey TEXT, endpoint TEXT, promptTokens INTEGER, completionTokens INTEGER,
  cost REAL, status TEXT, tokens TEXT, meta TEXT);
CREATE INDEX IF NOT EXISTS idx_usage_ts ON usageHistory(timestamp);
CREATE TABLE IF NOT EXISTS usageDaily (dateKey TEXT PRIMARY KEY, data TEXT);
CREATE TABLE IF NOT EXISTS requestDetails (id TEXT PRIMARY KEY, timestamp TEXT, provider TEXT, model TEXT, connectionId TEXT, status TEXT, data TEXT);
"#;

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-4000; PRAGMA mmap_size=8388608; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        // Columns added by upstream after 001-initial; harmless if present.
        for (table, col, ty) in [
            ("apiKeys", "accessRestricted", "INTEGER NOT NULL DEFAULT 0"),
            ("apiKeys", "accessAllow", "TEXT"),
            ("usageHistory", "endpoint", "TEXT"),
            ("usageHistory", "apiKey", "TEXT"),
        ] {
            let _ = conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {ty}"), []);
        }
        Ok(Db { conn: Mutex::new(conn) })
    }

    /// Run a closure against the connection. Keep closures short: the mutex is held.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.conn.lock();
        Ok(f(&conn)?)
    }

    pub fn checkpoint(&self) {
        let _ = self.conn.lock().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }

    // ----- raw rows (used by the Turso mirror) -----------------------------------

    /// Run a SELECT and return every row as dynamic cells.
    pub fn rows(&self, sql: &str, args: &[SqlValue]) -> Result<Vec<Vec<SqlValue>>> {
        self.with(|c| {
            let mut st = c.prepare(sql)?;
            let n = st.column_count();
            let mut rows = st.query(rusqlite::params_from_iter(args.iter()))?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                out.push((0..n).map(|i| r.get::<_, SqlValue>(i)).collect::<rusqlite::Result<Vec<_>>>()?);
            }
            Ok(out)
        })
    }

    /// `INSERT OR REPLACE` a batch of rows in one transaction.
    pub fn put_rows(&self, table: &str, cols: &[&str], rows: &[Vec<SqlValue>]) -> Result<()> {
        let names = cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",");
        let marks = vec!["?"; cols.len()].join(",");
        let sql = format!("INSERT OR REPLACE INTO \"{table}\" ({names}) VALUES ({marks})");
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            {
                let mut st = tx.prepare(&sql)?;
                for r in rows {
                    st.execute(rusqlite::params_from_iter(r.iter()))?;
                }
            }
            tx.commit()
        })
    }

    /// Make new `usageHistory` ids start after `id` (restored history may be partial).
    pub fn set_usage_seq(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM sqlite_sequence WHERE name='usageHistory'", [])?;
            c.execute("INSERT INTO sqlite_sequence(name, seq) VALUES('usageHistory', ?1)", params![id])
        })?;
        Ok(())
    }

    /// True when the database holds no user data (fresh install or wiped SD card).
    pub fn is_blank(&self) -> bool {
        self.with(|c| {
            c.query_row(
                "SELECT (SELECT COUNT(*) FROM providerConnections) + (SELECT COUNT(*) FROM apiKeys)
                      + (SELECT COUNT(*) FROM usageHistory) + (SELECT COUNT(*) FROM kv)",
                [],
                |r| r.get::<_, i64>(0),
            )
        })
        .map(|n| n == 0)
        .unwrap_or(false)
    }

    // ----- settings ------------------------------------------------------------

    pub fn default_settings() -> Value {
        json!({
            "requireLogin": false,
            "requireApiKey": false,
            "fallbackStrategy": "fill-first",
            "stickyRoundRobinLimit": 3,
            "providerStrategies": {},
            "providerOverrides": {},
            "pricing": {},
            "rtkEnabled": false,
        })
    }

    pub fn settings(&self) -> Value {
        let stored: Option<String> = self
            .with(|c| c.query_row("SELECT data FROM settings WHERE id=1", [], |r| r.get(0)).optional())
            .ok()
            .flatten();
        let mut base = Self::default_settings();
        if let Some(Value::Object(m)) = stored.and_then(|s| serde_json::from_str(&s).ok()) {
            for (k, v) in m {
                base[k] = v;
            }
        }
        base
    }

    pub fn save_settings(&self, patch: &Value) -> Result<Value> {
        let mut cur = self.settings();
        if let Value::Object(m) = patch {
            for (k, v) in m {
                cur[k.clone()] = v.clone();
            }
        }
        let text = cur.to_string();
        self.with(|c| {
            c.execute(
                "INSERT INTO settings(id,data) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
                params![text],
            )
        })?;
        Ok(cur)
    }

    // ----- provider connections --------------------------------------------------

    fn row_to_connection(r: &rusqlite::Row) -> rusqlite::Result<ProviderConnection> {
        let data: String = r.get::<_, Option<String>>(7)?.unwrap_or_else(|| "{}".into());
        Ok(ProviderConnection {
            id: r.get(0)?,
            provider: r.get(1)?,
            auth_type: r.get::<_, Option<String>>(2)?.unwrap_or_else(|| "apikey".into()),
            name: r.get(3)?,
            email: r.get(4)?,
            priority: r.get(5)?,
            is_active: r.get::<_, i64>(6)? != 0,
            data: serde_json::from_str(&data).unwrap_or(Value::Object(Default::default())),
            created_at: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            updated_at: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
        })
    }

    const CONN_COLS: &str = "id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt";

    /// Connections for one provider, active first, ordered by priority (NULL last).
    pub fn connections_for(&self, provider: &str, only_active: bool) -> Result<Vec<ProviderConnection>> {
        self.with(|c| {
            let sql = format!(
                "SELECT {} FROM providerConnections WHERE provider=?1 {} ORDER BY COALESCE(priority, 999), createdAt",
                Self::CONN_COLS,
                if only_active { "AND isActive=1" } else { "" }
            );
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(params![provider], Self::row_to_connection)?;
            rows.collect()
        })
    }

    pub fn all_connections(&self) -> Result<Vec<ProviderConnection>> {
        self.with(|c| {
            let sql = format!(
                "SELECT {} FROM providerConnections ORDER BY provider, COALESCE(priority, 999), createdAt",
                Self::CONN_COLS
            );
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map([], Self::row_to_connection)?;
            rows.collect()
        })
    }

    pub fn get_connection(&self, id: &str) -> Result<Option<ProviderConnection>> {
        self.with(|c| {
            let sql = format!("SELECT {} FROM providerConnections WHERE id=?1", Self::CONN_COLS);
            c.query_row(&sql, params![id], Self::row_to_connection).optional()
        })
    }

    pub fn insert_connection(&self, conn: &ProviderConnection) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO providerConnections(id,provider,authType,name,email,priority,isActive,data,createdAt,updatedAt)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    conn.id, conn.provider, conn.auth_type, conn.name, conn.email, conn.priority,
                    conn.is_active as i64, conn.data.to_string(), conn.created_at, conn.updated_at
                ],
            )
        })?;
        Ok(())
    }

    /// Shallow-merge `patch` into the connection's JSON blob. `null` values delete keys.
    pub fn patch_connection_data(&self, id: &str, patch: &Value) -> Result<()> {
        let conn = self.conn.lock();
        let cur: Option<String> =
            conn.query_row("SELECT data FROM providerConnections WHERE id=?1", params![id], |r| r.get(0)).optional()?;
        let Some(cur) = cur else { return Ok(()) };
        let mut data: Value = serde_json::from_str(&cur).unwrap_or_else(|_| json!({}));
        if let (Value::Object(dst), Value::Object(src)) = (&mut data, patch) {
            for (k, v) in src {
                if v.is_null() {
                    dst.remove(k);
                } else {
                    dst.insert(k.clone(), v.clone());
                }
            }
        }
        conn.execute(
            "UPDATE providerConnections SET data=?2, updatedAt=?3 WHERE id=?1",
            params![id, data.to_string(), now_iso()],
        )?;
        Ok(())
    }

    pub fn set_connection_active(&self, id: &str, active: bool) -> Result<bool> {
        let n = self.with(|c| {
            c.execute(
                "UPDATE providerConnections SET isActive=?2, updatedAt=?3 WHERE id=?1",
                params![id, active as i64, now_iso()],
            )
        })?;
        Ok(n > 0)
    }

    pub fn delete_connection(&self, id: &str) -> Result<bool> {
        Ok(self.with(|c| c.execute("DELETE FROM providerConnections WHERE id=?1", params![id]))? > 0)
    }

    /// Update the editable metadata of a connection (dashboard edit form).
    pub fn update_connection(&self, id: &str, name: Option<&str>, priority: Option<Option<i64>>) -> Result<bool> {
        let n = self.with(|c| {
            c.execute(
                "UPDATE providerConnections SET
                    name = CASE WHEN ?2 IS NULL THEN name ELSE ?2 END,
                    priority = CASE WHEN ?3 THEN ?4 ELSE priority END,
                    updatedAt = ?5
                 WHERE id=?1",
                params![id, name, priority.is_some(), priority.flatten(), now_iso()],
            )
        })?;
        Ok(n > 0)
    }

    /// Patch that clears every cooldown lock and error marker on a connection.
    pub fn unlock_patch(c: &ProviderConnection) -> Value {
        let mut patch = serde_json::Map::new();
        for k in c.data.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
            if k.starts_with("modelLock_") {
                patch.insert(k, Value::Null);
            }
        }
        for k in ["testStatus", "lastError", "errorCode", "backoffLevel", "lastErrorAt"] {
            patch.insert(k.into(), Value::Null);
        }
        Value::Object(patch)
    }

    // ----- api keys --------------------------------------------------------------

    fn row_to_key(r: &rusqlite::Row) -> rusqlite::Result<ApiKey> {
        let allow: Option<String> = r.get(6)?;
        Ok(ApiKey {
            id: r.get(0)?,
            key: r.get(1)?,
            name: r.get(2)?,
            is_active: r.get::<_, i64>(3)? != 0,
            created_at: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            access_restricted: r.get::<_, Option<i64>>(5)?.unwrap_or(0) != 0,
            access_allow: allow.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or_default(),
        })
    }

    const KEY_COLS: &str = "id, key, name, isActive, createdAt, accessRestricted, accessAllow";

    pub fn api_key(&self, key: &str) -> Result<Option<ApiKey>> {
        self.with(|c| {
            let sql = format!("SELECT {} FROM apiKeys WHERE key=?1", Self::KEY_COLS);
            c.query_row(&sql, params![key], Self::row_to_key).optional()
        })
    }

    pub fn api_keys(&self) -> Result<Vec<ApiKey>> {
        self.with(|c| {
            let sql = format!("SELECT {} FROM apiKeys ORDER BY createdAt", Self::KEY_COLS);
            let mut st = c.prepare(&sql)?;
            let keys = st.query_map([], Self::row_to_key)?.collect();
            keys
        })
    }

    pub fn insert_api_key(&self, k: &ApiKey, machine_id: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO apiKeys(id,key,name,machineId,isActive,createdAt,accessRestricted,accessAllow)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    k.id,
                    k.key,
                    k.name,
                    machine_id,
                    k.is_active as i64,
                    k.created_at,
                    k.access_restricted as i64,
                    serde_json::to_string(&k.access_allow).unwrap_or_default()
                ],
            )
        })?;
        Ok(())
    }

    pub fn set_api_key_active(&self, id: &str, active: bool) -> Result<bool> {
        Ok(self
            .with(|c| c.execute("UPDATE apiKeys SET isActive=?2 WHERE id=?1 OR key=?1", params![id, active as i64]))?
            > 0)
    }

    pub fn delete_api_key(&self, id: &str) -> Result<bool> {
        Ok(self.with(|c| c.execute("DELETE FROM apiKeys WHERE id=?1 OR key=?1", params![id]))? > 0)
    }

    /// Update name / active flag / access list of a client key.
    pub fn update_api_key(
        &self,
        id: &str,
        name: Option<&str>,
        active: Option<bool>,
        access: Option<(bool, &[String])>,
    ) -> Result<bool> {
        let allow = access.map(|(_, a)| serde_json::to_string(a).unwrap_or_default());
        let restricted = access.map(|(r, _)| r as i64);
        let n = self.with(|c| {
            c.execute(
                "UPDATE apiKeys SET
                    name = CASE WHEN ?2 IS NULL THEN name ELSE ?2 END,
                    isActive = CASE WHEN ?3 IS NULL THEN isActive ELSE ?3 END,
                    accessRestricted = CASE WHEN ?4 IS NULL THEN accessRestricted ELSE ?4 END,
                    accessAllow = CASE WHEN ?5 IS NULL THEN accessAllow ELSE ?5 END
                 WHERE id=?1 OR key=?1",
                params![id, name, active.map(|a| a as i64), restricted, allow],
            )
        })?;
        Ok(n > 0)
    }

    /// Lifetime request counter maintained by `insert_usage`.
    pub fn total_requests(&self) -> i64 {
        self.with(|c| {
            c.query_row("SELECT value FROM _meta WHERE key='totalRequestsLifetime'", [], |r| r.get::<_, String>(0))
                .optional()
        })
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
    }

    // ----- kv (model aliases etc.) ------------------------------------------------

    pub fn kv_scope(&self, scope: &str) -> Result<serde_json::Map<String, Value>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT key, value FROM kv WHERE scope=?1")?;
            let rows =
                st.query_map(params![scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
            let mut m = serde_json::Map::new();
            for row in rows {
                let (k, v) = row?;
                let v = v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
                m.insert(k, v);
            }
            Ok(m)
        })
    }

    pub fn kv_set(&self, scope: &str, key: &str, value: Option<&Value>) -> Result<()> {
        self.with(|c| match value {
            Some(v) => c.execute(
                "INSERT INTO kv(scope,key,value) VALUES(?1,?2,?3) ON CONFLICT(scope,key) DO UPDATE SET value=excluded.value",
                params![scope, key, v.to_string()],
            ),
            None => c.execute("DELETE FROM kv WHERE scope=?1 AND key=?2", params![scope, key]),
        })?;
        Ok(())
    }

    // ----- usage -------------------------------------------------------------------

    pub fn insert_usage(&self, u: &UsageRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO usageHistory(timestamp,provider,model,connectionId,apiKey,endpoint,promptTokens,completionTokens,cost,status,tokens,meta)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    u.timestamp, u.provider, u.model, u.connection_id, u.api_key, u.endpoint,
                    u.prompt_tokens, u.completion_tokens, u.cost, u.status, u.tokens.to_string(), u.meta.to_string()
                ],
            )?;
            c.execute(
                "INSERT INTO _meta(key,value) VALUES('totalRequestsLifetime','1')
                 ON CONFLICT(key) DO UPDATE SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT)",
                [],
            )
        })?;
        Ok(())
    }

    /// Dashboard statistics for rows with `timestamp >= since` (all rows when None),
    /// shaped like upstream `/api/usage/stats`: totals, per-provider, per-model,
    /// per-account, per-key and per-endpoint groups plus a time series.
    pub fn usage_stats(&self, since: Option<&str>, hourly: bool) -> Result<Value> {
        let since = since.unwrap_or("").to_string();
        self.with(|c| {
            let cached = "COALESCE(SUM(COALESCE(json_extract(tokens,'$.cached_tokens'),0)),0)";
            let totals = c.query_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM(promptTokens),0), COALESCE(SUM(completionTokens),0), {cached}, COALESCE(SUM(cost),0),
                            COALESCE(SUM(CASE WHEN status IS NOT NULL AND status NOT IN ('success','ok') THEN 1 ELSE 0 END),0)
                     FROM usageHistory WHERE timestamp >= ?1"
                ),
                params![since],
                |r| {
                    Ok(json!({
                        "totalRequests": r.get::<_, i64>(0)?, "totalPromptTokens": r.get::<_, i64>(1)?,
                        "totalCompletionTokens": r.get::<_, i64>(2)?, "totalCachedTokens": r.get::<_, i64>(3)?,
                        "totalCost": r.get::<_, f64>(4)?, "totalErrors": r.get::<_, i64>(5)?,
                    }))
                },
            )?;
            // Group by an arbitrary set of key columns; keys come back as strings.
            let group = |keys: &[&str]| -> rusqlite::Result<Vec<Value>> {
                let sel = keys.join(", ");
                let mut st = c.prepare(&format!(
                    "SELECT {sel}, COUNT(*), COALESCE(SUM(promptTokens),0), COALESCE(SUM(completionTokens),0), {cached},
                            COALESCE(SUM(cost),0), MAX(timestamp)
                     FROM usageHistory WHERE timestamp >= ?1 GROUP BY {sel} ORDER BY COUNT(*) DESC LIMIT 400"
                ))?;
                let n = keys.len();
                let rows = st.query_map(params![since], |r| {
                    let mut o = serde_json::Map::new();
                    for (i, k) in keys.iter().enumerate() {
                        let name = k.rsplit(' ').next().unwrap_or(k).trim_start_matches("AS ");
                        o.insert(name.to_string(), json!(r.get::<_, Option<String>>(i)?));
                    }
                    o.insert("requests".into(), json!(r.get::<_, i64>(n)?));
                    o.insert("promptTokens".into(), json!(r.get::<_, i64>(n + 1)?));
                    o.insert("completionTokens".into(), json!(r.get::<_, i64>(n + 2)?));
                    o.insert("cachedTokens".into(), json!(r.get::<_, i64>(n + 3)?));
                    o.insert("cost".into(), json!(r.get::<_, f64>(n + 4)?));
                    o.insert("lastUsed".into(), json!(r.get::<_, Option<String>>(n + 5)?));
                    Ok(Value::Object(o))
                })?;
                rows.collect()
            };
            let bucket = if hourly { "substr(timestamp,1,13) AS bucket" } else { "substr(timestamp,1,10) AS bucket" };
            Ok(json!({
                "since": since,
                "totals": totals,
                "byProvider": group(&["provider"])?,
                "byModel": group(&["provider", "model"])?,
                "byAccount": group(&["connectionId", "provider", "model"])?,
                "byApiKey": group(&["apiKey", "provider", "model"])?,
                "byEndpoint": group(&["endpoint", "provider", "model"])?,
                "chart": group(&[bucket])?,
            }))
        })
    }

    pub fn usage_recent(&self, limit: i64) -> Result<Vec<Value>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT timestamp, provider, model, connectionId, endpoint, promptTokens, completionTokens, cost, status, tokens, meta, apiKey
                 FROM usageHistory ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = st.query_map(params![limit], |r| {
                let tokens: Option<String> = r.get(9)?;
                let meta: Option<String> = r.get(10)?;
                Ok(json!({
                    "timestamp": r.get::<_, Option<String>>(0)?, "provider": r.get::<_, Option<String>>(1)?,
                    "model": r.get::<_, Option<String>>(2)?, "connectionId": r.get::<_, Option<String>>(3)?,
                    "endpoint": r.get::<_, Option<String>>(4)?, "promptTokens": r.get::<_, Option<i64>>(5)?,
                    "completionTokens": r.get::<_, Option<i64>>(6)?, "cost": r.get::<_, Option<f64>>(7)?,
                    "status": r.get::<_, Option<String>>(8)?,
                    "tokens": tokens.and_then(|s| serde_json::from_str::<Value>(&s).ok()),
                    "meta": meta.and_then(|s| serde_json::from_str::<Value>(&s).ok()),
                    "apiKey": r.get::<_, Option<String>>(11)?,
                }))
            })?;
            rows.collect()
        })
    }

    pub fn prune_usage(&self, keep_days: i64) -> Result<usize> {
        let cutoff = (time::OffsetDateTime::now_utc() - time::Duration::days(keep_days))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        self.with(|c| c.execute("DELETE FROM usageHistory WHERE timestamp < ?1", params![cutoff]))
    }
}
