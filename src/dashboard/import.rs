//! Import from an old 9router install: the JSON from its Settings → Download Backup
//! (same shape as the legacy `~/.9router/db.json`), or its `~/.9router/db/data.sqlite`.
//!
//! Rows are merged by id rather than replacing everything like upstream's import, so
//! importing twice is harmless and accounts added here are kept. Differences from
//! upstream's storage are translated on the way in:
//!
//! * custom endpoints are a `providerNodes` row plus connections whose provider is the
//!   node id (`openai-compatible-<uuid>`); here the connection itself carries the
//!   prefix and base URL under provider `openai-compatible` / `anthropic-compatible`;
//! * custom models are one kv row per model (`alias|id|type`); here one list per prefix;
//! * the dashboard password is a bcrypt hash there, so it (and the login switch) is
//!   left as it is here.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};

use crate::db::now_iso;
use crate::providers::{ANTHROPIC_COMPATIBLE, OPENAI_COMPATIBLE};
use crate::state::AppState;

/// Largest JSON backup accepted (a SQLite file may be larger: only a few tables are read).
const MAX_JSON: u64 = 32 << 20;

/// Settings kept as they are here (see the module docs).
const KEEP_SETTINGS: &[&str] = &["password", "passwordHash", "requireLogin", "authMode"];

const CONN_COLS: &[&str] =
    &["id", "provider", "authType", "name", "email", "priority", "isActive", "data", "createdAt", "updatedAt"];
const KEY_COLS: &[&str] =
    &["id", "key", "name", "machineId", "isActive", "createdAt", "accessRestricted", "accessAllow"];

pub struct Options {
    /// Accounts, custom endpoints, custom models and model aliases.
    pub providers: bool,
    pub keys: bool,
    pub settings: bool,
    /// Only report what would be imported.
    pub dry_run: bool,
}

/// Read an uploaded file into upstream's backup shape; returns it and the format name.
pub fn read_payload(path: &Path) -> Result<(Value, &'static str)> {
    let mut head = [0u8; 16];
    let n = std::fs::File::open(path)?.read(&mut head)?;
    if n == 16 && &head == b"SQLite format 3\0" {
        return Ok((from_sqlite(path)?, "data.sqlite"));
    }
    if std::fs::metadata(path)?.len() > MAX_JSON {
        bail!("file too large for a 9router backup");
    }
    let text = std::fs::read_to_string(path).context("not a 9router backup (.json) or data.sqlite file")?;
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).context("the file is not valid JSON")?;
    if !["providerConnections", "apiKeys", "settings"].iter().any(|k| v.get(k).is_some()) {
        bail!("this JSON is not a 9router backup (no providerConnections, apiKeys or settings)");
    }
    Ok((v, "backup JSON"))
}

fn sql_to_json(v: SqlValue) -> Value {
    match v {
        SqlValue::Integer(i) => json!(i),
        SqlValue::Real(f) => json!(f),
        SqlValue::Text(s) => Value::String(s),
        SqlValue::Null | SqlValue::Blob(_) => Value::Null,
    }
}

fn select_all(c: &Connection, sql: &str) -> rusqlite::Result<Vec<Map<String, Value>>> {
    let mut st = c.prepare(sql)?;
    let names: Vec<String> = st.column_names().into_iter().map(str::to_string).collect();
    let rows = st
        .query_map([], |r| {
            let mut m = Map::new();
            for (i, name) in names.iter().enumerate() {
                m.insert(name.clone(), sql_to_json(r.get::<_, SqlValue>(i)?));
            }
            Ok(m)
        })?
        .collect::<rusqlite::Result<Vec<_>>>();
    rows
}

fn parse(v: Option<&Value>) -> Option<Value> {
    v.and_then(Value::as_str).and_then(|s| serde_json::from_str(s).ok())
}

/// Same as upstream's `exportDb`: the `data` column spread, then the real columns.
fn spread_data(rows: Vec<Map<String, Value>>) -> Value {
    Value::Array(
        rows.into_iter()
            .map(|mut r| {
                let mut out = match parse(r.remove("data").as_ref()) {
                    Some(Value::Object(m)) => m,
                    _ => Map::new(),
                };
                out.extend(r);
                Value::Object(out)
            })
            .collect(),
    )
}

fn from_sqlite(path: &Path) -> Result<Value> {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .context("cannot open the SQLite file")?;
    let all = |table: &str| select_all(&c, &format!("SELECT * FROM \"{table}\"")).unwrap_or_default();
    let connections = select_all(&c, "SELECT * FROM providerConnections")
        .context("this SQLite file is not a 9router database (no providerConnections table)")?;
    let mut aliases = Map::new();
    let mut custom = Vec::new();
    for r in all("kv") {
        let value = parse(r.get("value")).unwrap_or(Value::Null);
        match (r.get("scope").and_then(Value::as_str), r.get("key").and_then(Value::as_str)) {
            (Some("modelAliases"), Some(k)) => {
                aliases.insert(k.to_string(), value);
            }
            (Some("customModels"), _) => custom.push(value),
            _ => {}
        }
    }
    let settings = all("settings").into_iter().find_map(|r| parse(r.get("data"))).unwrap_or(Value::Null);
    Ok(json!({
        "settings": settings,
        "providerConnections": spread_data(connections),
        "providerNodes": spread_data(all("providerNodes")),
        "apiKeys": all("apiKeys"),
        "modelAliases": aliases,
        "customModels": custom,
    }))
}

fn list<'a>(p: &'a Value, key: &str) -> impl Iterator<Item = &'a Map<String, Value>> {
    p.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object)
}

/// Booleans arrive as JSON booleans (backup) or 0/1 (SQLite).
fn flag(v: Option<&Value>, default: bool) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().map_or(default, |n| n != 0),
        _ => default,
    }
}

fn text(m: &Map<String, Value>, key: &str) -> SqlValue {
    m.get(key).and_then(Value::as_str).map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string()))
}

fn text_or(m: &Map<String, Value>, key: &str, default: &str) -> SqlValue {
    SqlValue::Text(m.get(key).and_then(Value::as_str).unwrap_or(default).to_string())
}

fn id_of(m: &Map<String, Value>) -> String {
    m.get("id").and_then(Value::as_str).map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string)
}

fn compatible(provider: &str) -> Option<&'static str> {
    if provider.starts_with(OPENAI_COMPATIBLE) {
        Some(OPENAI_COMPATIBLE)
    } else if provider.starts_with(ANTHROPIC_COMPATIBLE) {
        Some(ANTHROPIC_COMPATIBLE)
    } else {
        None
    }
}

/// Work out what the payload holds and, unless `dry_run`, write the chosen parts.
pub fn apply(state: &AppState, p: &Value, o: &Options) -> Result<Value> {
    let now = now_iso();
    let nodes: HashMap<&str, &Map<String, Value>> =
        list(p, "providerNodes").filter_map(|n| Some((n.get("id")?.as_str()?, n))).collect();
    // Custom endpoint node id -> its prefix, to file custom models under it.
    let mut node_prefix: HashMap<String, String> =
        nodes.iter().filter_map(|(id, n)| Some((id.to_string(), n.get("prefix")?.as_str()?.to_string()))).collect();

    let mut conn_rows = Vec::new();
    let mut by_provider: BTreeMap<String, usize> = BTreeMap::new();
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    for c in list(p, "providerConnections") {
        let Some(provider) = c.get("provider").and_then(Value::as_str) else { continue };
        let mut data = c.clone();
        for k in CONN_COLS {
            data.remove(*k);
        }
        let (target, label) = match compatible(provider) {
            Some(kind) => {
                let psd = data.entry("providerSpecificData").or_insert_with(|| json!({}));
                if let (Some(node), Value::Object(psd)) = (nodes.get(provider), psd) {
                    for (from, to) in
                        [("prefix", "prefix"), ("baseUrl", "baseUrl"), ("apiType", "apiType"), ("name", "nodeName")]
                    {
                        if let (false, Some(v)) = (psd.contains_key(to), node.get(from)) {
                            psd.insert(to.to_string(), v.clone());
                        }
                    }
                }
                let prefix = data.get("providerSpecificData").and_then(|v| v.get("prefix")).and_then(Value::as_str);
                if let Some(prefix) = prefix {
                    node_prefix.entry(provider.to_string()).or_insert_with(|| prefix.to_string());
                }
                (kind, format!("{kind} ({})", prefix.unwrap_or("no prefix")))
            }
            None if state.registry.get(provider).is_some() => (provider, provider.to_string()),
            None => {
                *skipped.entry(provider.to_string()).or_default() += 1;
                continue;
            }
        };
        let auth_type = c.get("authType").and_then(Value::as_str).unwrap_or(if data.contains_key("apiKey") {
            "apikey"
        } else {
            "oauth"
        });
        *by_provider.entry(label).or_default() += 1;
        conn_rows.push(vec![
            SqlValue::Text(id_of(c)),
            SqlValue::Text(target.to_string()),
            SqlValue::Text(auth_type.to_string()),
            text(c, "name"),
            text(c, "email"),
            c.get("priority").and_then(Value::as_i64).map_or(SqlValue::Null, SqlValue::Integer),
            SqlValue::Integer(flag(c.get("isActive"), true) as i64),
            SqlValue::Text(Value::Object(data).to_string()),
            text_or(c, "createdAt", &now),
            text_or(c, "updatedAt", &now),
        ]);
    }

    let mut key_rows = Vec::new();
    for k in list(p, "apiKeys") {
        let Some(key) = k.get("key").and_then(Value::as_str).filter(|s| !s.is_empty()) else { continue };
        let (restricted, allow) = match k.get("access") {
            Some(a) => (flag(a.get("restricted"), false), a.get("allow").cloned().unwrap_or_else(|| json!([]))),
            None => (flag(k.get("accessRestricted"), false), parse(k.get("accessAllow")).unwrap_or_else(|| json!([]))),
        };
        key_rows.push(vec![
            SqlValue::Text(id_of(k)),
            SqlValue::Text(key.to_string()),
            text(k, "name"),
            text(k, "machineId"),
            SqlValue::Integer(flag(k.get("isActive"), true) as i64),
            text_or(k, "createdAt", &now),
            SqlValue::Integer(restricted as i64),
            SqlValue::Text(if restricted { allow.to_string() } else { "[]".into() }),
        ]);
    }

    let aliases: Vec<(&String, &Value)> =
        p.get("modelAliases").and_then(Value::as_object).into_iter().flatten().filter(|(_, v)| !v.is_null()).collect();

    let mut custom: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for m in list(p, "customModels") {
        // Embedding / image / TTS models have no use here.
        if m.get("type").and_then(Value::as_str).map_or(false, |t| t != "llm") {
            continue;
        }
        let (Some(owner), Some(id)) =
            (m.get("providerAlias").and_then(Value::as_str), m.get("id").and_then(Value::as_str))
        else {
            continue;
        };
        let key = node_prefix.get(owner).cloned().unwrap_or_else(|| owner.to_string());
        custom.entry(key).or_default().push(id.to_string());
    }

    let settings: Map<String, Value> = p
        .get("settings")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(k, _)| !KEEP_SETTINGS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    if !o.dry_run {
        if o.providers {
            state.db.put_rows("providerConnections", CONN_COLS, &conn_rows)?;
            for (alias, target) in &aliases {
                state.db.kv_set("modelAliases", alias, Some(*target))?;
            }
            let existing = state.db.kv_scope("customModels")?;
            for (prefix, models) in &custom {
                let mut merged: Vec<String> =
                    existing.get(prefix).cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
                for m in models {
                    if !merged.contains(m) {
                        merged.push(m.clone());
                    }
                }
                state.db.kv_set("customModels", prefix, Some(&json!(merged)))?;
            }
        }
        if o.keys {
            state.db.put_rows("apiKeys", KEY_COLS, &key_rows)?;
        }
        if o.settings && !settings.is_empty() {
            state.db.save_settings(&Value::Object(settings.clone()))?;
        }
        tracing::info!(
            "imported from 9router: {} accounts, {} API keys, {} aliases, {} custom models{}",
            if o.providers { conn_rows.len() } else { 0 },
            if o.keys { key_rows.len() } else { 0 },
            if o.providers { aliases.len() } else { 0 },
            if o.providers { custom.values().map(Vec::len).sum() } else { 0 },
            if o.settings && !settings.is_empty() { ", settings" } else { "" }
        );
    }

    Ok(json!({
        "connections": conn_rows.len(),
        "byProvider": by_provider,
        "skipped": skipped,
        "keys": key_rows.len(),
        "aliases": aliases.len(),
        "customModels": custom.values().map(Vec::len).sum::<usize>(),
        "settings": settings.len(),
        "dryRun": o.dry_run,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_helpers() {
        assert!(flag(Some(&json!(1)), false));
        assert!(!flag(Some(&json!(false)), true));
        assert!(flag(None, true));
        assert_eq!(compatible("openai-compatible-chat-1234"), Some(OPENAI_COMPATIBLE));
        assert_eq!(compatible("anthropic-compatible-9"), Some(ANTHROPIC_COMPATIBLE));
        assert_eq!(compatible("openrouter"), None);
        let rows = vec![Map::from_iter([
            ("id".to_string(), json!("a")),
            ("data".to_string(), json!("{\"apiKey\":\"k\",\"id\":\"ignored\"}")),
        ])];
        assert_eq!(spread_data(rows), json!([{"apiKey": "k", "id": "a"}]));
    }
}
