//! Account selection, cooldown locks and error classification. Rules are upstream's
//! (`open-sse/config/errorConfig.js`, `services/accountFallback.js`,
//! `src/sse/services/auth.js`), with locks stored in the connection's JSON blob
//! under `modelLock_<model>` / `modelLock___all` so the data stays compatible.

use std::collections::HashSet;

use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};

use crate::db::ProviderConnection;
use crate::state::AppState;

pub const BACKOFF_BASE_MS: i64 = 2_000;
pub const BACKOFF_MAX_MS: i64 = 5 * 60 * 1_000;
pub const BACKOFF_MAX_LEVEL: i64 = 15;
pub const COOLDOWN_LONG_MS: i64 = 2 * 60 * 1_000;
pub const COOLDOWN_SHORT_MS: i64 = 5 * 1_000;
pub const TRANSIENT_COOLDOWN_MS: i64 = 30 * 1_000;
pub const MAX_RATE_LIMIT_COOLDOWN_MS: i64 = 30 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Return the error to the client as-is; do not try another account.
    NoFallback,
    /// Lock this account for `cooldown_ms` and try the next one.
    Fallback { cooldown_ms: i64, backoff_level: i64 },
}

/// Classify an upstream failure. `level` is the account's current backoff level.
pub fn classify(status: u16, body: &str, level: i64) -> Verdict {
    let text = body.to_ascii_lowercase();
    let fixed = |ms| Verdict::Fallback { cooldown_ms: ms, backoff_level: level };
    let backoff = || {
        let new_level = (level + 1).min(BACKOFF_MAX_LEVEL);
        let ms = (BACKOFF_BASE_MS.saturating_mul(1i64 << (new_level - 1).min(20))).min(BACKOFF_MAX_MS);
        Verdict::Fallback { cooldown_ms: ms, backoff_level: new_level }
    };

    // Text rules first, in priority order.
    const TEXT_RULES: &[(&str, Option<i64>)] = &[
        ("no credentials", Some(COOLDOWN_LONG_MS)),
        ("request not allowed", Some(COOLDOWN_SHORT_MS)),
        ("improperly formed request", Some(COOLDOWN_LONG_MS)),
        ("rate limit", None),
        ("too many requests", None),
        ("quota exceeded", None),
        ("capacity", None),
        ("overloaded", None),
    ];
    for (needle, cooldown) in TEXT_RULES {
        if text.contains(needle) {
            return match cooldown {
                Some(ms) => fixed(*ms),
                None => backoff(),
            };
        }
    }
    match status {
        401 | 402 | 403 | 404 => fixed(COOLDOWN_LONG_MS),
        429 => backoff(),
        400..=499 => Verdict::NoFallback,
        _ => fixed(TRANSIENT_COOLDOWN_MS),
    }
}

fn lock_key(model: Option<&str>) -> String {
    match model {
        Some(m) => format!("modelLock_{m}"),
        None => "modelLock___all".to_string(),
    }
}

fn parse_ts(v: &Value) -> Option<OffsetDateTime> {
    v.as_str().and_then(|s| OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok())
}

/// Earliest future unlock time for this connection + model, if any lock is active.
pub fn active_lock_until(c: &ProviderConnection, model: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
    [lock_key(Some(model)), lock_key(None)]
        .iter()
        .filter_map(|k| c.data.get(k).and_then(parse_ts))
        .filter(|t| *t > now)
        .min()
}

#[derive(Debug, Default, Clone)]
pub struct RoundRobin {
    pub last_id: Option<String>,
    pub consecutive: i64,
}

pub enum Pick {
    Connection(ProviderConnection),
    /// Everything is cooling down; retry after this many seconds.
    AllLocked {
        retry_after_secs: u64,
    },
    None,
}

/// Choose an account for `provider` + `model`, skipping `exclude` and locked ones.
///
/// Strategy comes from `settings.providerStrategies[provider].fallbackStrategy`,
/// then `settings.fallbackStrategy`, default `fill-first`. Round-robin is sticky for
/// `settings.stickyRoundRobinLimit` (default 3) consecutive requests.
pub fn pick(
    state: &AppState,
    provider: &str,
    model: &str,
    exclude: &HashSet<String>,
    pinned: Option<&str>,
) -> anyhow::Result<Pick> {
    let now = OffsetDateTime::now_utc();
    let prov = state.registry.get(provider);
    if prov.map(|p| p.no_auth).unwrap_or(false) {
        return Ok(Pick::Connection(virtual_connection(provider)));
    }
    let all = state.db.connections_for(provider, true)?;
    let mut earliest: Option<OffsetDateTime> = None;
    let candidates: Vec<ProviderConnection> = all
        .into_iter()
        .filter(|c| pinned.map(|p| p == c.id).unwrap_or(true))
        .filter(|c| !exclude.contains(&c.id))
        .filter(|c| match active_lock_until(c, model, now) {
            Some(t) => {
                earliest = Some(earliest.map_or(t, |e| e.min(t)));
                false
            }
            None => true,
        })
        .collect();

    if candidates.is_empty() {
        return Ok(match earliest {
            Some(t) if exclude.is_empty() || pinned.is_some() => {
                Pick::AllLocked { retry_after_secs: ((t - now).whole_seconds().max(1)) as u64 }
            }
            _ => Pick::None,
        });
    }

    let settings = state.db.settings();
    let strategy = settings
        .pointer(&format!("/providerStrategies/{provider}/fallbackStrategy"))
        .or_else(|| settings.get("fallbackStrategy"))
        .and_then(Value::as_str)
        .unwrap_or("fill-first")
        .to_string();
    if strategy != "round-robin" || candidates.len() == 1 {
        return Ok(Pick::Connection(candidates.into_iter().next().unwrap()));
    }

    let sticky = settings.get("stickyRoundRobinLimit").and_then(Value::as_i64).unwrap_or(3).max(1);
    let mut rr = state.rr.lock();
    let entry = rr.entry(provider.to_string()).or_default();
    if let Some(last) = &entry.last_id {
        if entry.consecutive < sticky {
            if let Some(c) = candidates.iter().find(|c| &c.id == last) {
                entry.consecutive += 1;
                return Ok(Pick::Connection(c.clone()));
            }
        }
    }
    // Next candidate after the last-used one in priority order.
    let idx = entry
        .last_id
        .as_ref()
        .and_then(|l| candidates.iter().position(|c| &c.id == l))
        .map(|i| (i + 1) % candidates.len())
        .unwrap_or(0);
    let chosen = candidates[idx].clone();
    entry.last_id = Some(chosen.id.clone());
    entry.consecutive = 1;
    Ok(Pick::Connection(chosen))
}

fn virtual_connection(provider: &str) -> ProviderConnection {
    ProviderConnection {
        id: "noauth".into(),
        provider: provider.into(),
        auth_type: "none".into(),
        name: None,
        email: None,
        priority: None,
        is_active: true,
        data: json!({"accessToken": "public"}),
        created_at: String::new(),
        updated_at: String::new(),
    }
}

/// Record a failure on the account: lock the model, bump backoff, keep the error.
pub fn mark_unavailable(
    state: &AppState,
    conn: &ProviderConnection,
    model: &str,
    status: u16,
    error: &str,
    verdict: Verdict,
) {
    if conn.id == "noauth" {
        return;
    }
    let Verdict::Fallback { cooldown_ms, backoff_level } = verdict else { return };
    let until = OffsetDateTime::now_utc() + Duration::milliseconds(cooldown_ms.min(MAX_RATE_LIMIT_COOLDOWN_MS));
    let patch = json!({
        lock_key(Some(model)): until.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
        "testStatus": "unavailable",
        "lastError": error.chars().take(200).collect::<String>(),
        "errorCode": status,
        "lastErrorAt": crate::db::now_iso(),
        "backoffLevel": backoff_level,
    });
    if let Err(e) = state.db.patch_connection_data(&conn.id, &patch) {
        tracing::warn!("failed to persist lock for {}: {e}", conn.id);
    }
    tracing::warn!(
        "{}/{} account {} locked for {}s after {} ({})",
        conn.provider,
        model,
        conn.label(),
        cooldown_ms / 1000,
        status,
        error.chars().take(120).collect::<String>().replace('\n', " ")
    );
}

/// Clear this model's lock, the all-models lock and any expired locks; reset the
/// error fields when nothing is locked any more.
pub fn mark_success(state: &AppState, conn: &ProviderConnection, model: &str) {
    if conn.id == "noauth" {
        return;
    }
    let Ok(Some(fresh)) = state.db.get_connection(&conn.id) else { return };
    let now = OffsetDateTime::now_utc();
    let mut patch = serde_json::Map::new();
    patch.insert(lock_key(Some(model)), Value::Null);
    patch.insert(lock_key(None), Value::Null);
    let mut remaining = 0;
    if let Some(obj) = fresh.data.as_object() {
        for (k, v) in obj {
            if k.starts_with("modelLock_") && !patch.contains_key(k) {
                match parse_ts(v) {
                    Some(t) if t > now => remaining += 1,
                    _ => {
                        patch.insert(k.clone(), Value::Null);
                    }
                }
            }
        }
    }
    if remaining == 0 {
        for k in ["testStatus", "lastError", "errorCode", "backoffLevel"] {
            patch.insert(k.into(), Value::Null);
        }
    }
    patch.insert("lastUsedAt".into(), Value::String(crate::db::now_iso()));
    let _ = state.db.patch_connection_data(&conn.id, &Value::Object(patch));
}

/// Housekeeping: drop expired `modelLock_*` keys everywhere.
pub fn clear_expired_locks(state: &AppState) -> anyhow::Result<()> {
    let now = OffsetDateTime::now_utc();
    for c in state.db.all_connections()? {
        let mut patch = serde_json::Map::new();
        if let Some(obj) = c.data.as_object() {
            for (k, v) in obj {
                if k.starts_with("modelLock_") && parse_ts(v).map(|t| t <= now).unwrap_or(true) {
                    patch.insert(k.clone(), Value::Null);
                }
            }
        }
        if !patch.is_empty() {
            state.db.patch_connection_data(&c.id, &Value::Object(patch))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_matches_upstream_rules() {
        assert_eq!(classify(400, "bad json", 0), Verdict::NoFallback);
        assert!(matches!(classify(401, "", 0), Verdict::Fallback { cooldown_ms: COOLDOWN_LONG_MS, .. }));
        assert!(matches!(classify(500, "boom", 0), Verdict::Fallback { cooldown_ms: TRANSIENT_COOLDOWN_MS, .. }));
        assert_eq!(classify(429, "", 0), Verdict::Fallback { cooldown_ms: 2_000, backoff_level: 1 });
        assert_eq!(classify(429, "", 3), Verdict::Fallback { cooldown_ms: 16_000, backoff_level: 4 });
        assert_eq!(
            classify(200, "Rate limit hit", 14),
            Verdict::Fallback { cooldown_ms: BACKOFF_MAX_MS, backoff_level: 15 }
        );
        assert!(matches!(
            classify(400, "Request not allowed", 0),
            Verdict::Fallback { cooldown_ms: COOLDOWN_SHORT_MS, .. }
        ));
    }

    #[test]
    fn lock_detection() {
        let now = OffsetDateTime::now_utc();
        let future = (now + Duration::seconds(30)).format(&time::format_description::well_known::Rfc3339).unwrap();
        let past = (now - Duration::seconds(30)).format(&time::format_description::well_known::Rfc3339).unwrap();
        let mut c = virtual_connection("x");
        c.data = json!({"modelLock_a": future, "modelLock_b": past});
        assert!(active_lock_until(&c, "a", now).is_some());
        assert!(active_lock_until(&c, "b", now).is_none());
        assert!(active_lock_until(&c, "c", now).is_none());
    }
}
