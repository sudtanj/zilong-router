//! Client API keys. Format and storage are 9router's so existing keys keep working:
//! `sk-{machineId16}-{keyId6}-{crc8}` where crc8 = hex(HMAC-SHA256(API_KEY_SECRET,
//! machineId + keyId))[..8]. Validation is a DB lookup plus the isActive flag; the
//! checksum only guards against typos, as upstream.

use std::net::IpAddr;

use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use rand::{distributions::Alphanumeric, Rng};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::db::{ApiKey, Db};

/// Stable per-install id: sha256(raw + salt)[..16]. The raw id is /etc/machine-id when
/// readable, else a random value persisted under the data dir.
pub fn machine_id(cfg: &Config) -> String {
    let raw = std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let p = cfg.resolved_data_dir.join("auth").join("machine-id");
            if let Ok(s) = std::fs::read_to_string(&p) {
                return s.trim().to_string();
            }
            let fresh: String = rand::thread_rng().sample_iter(&Alphanumeric).take(32).map(char::from).collect();
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(&p, &fresh);
            fresh
        });
    let digest = Sha256::digest(format!("{raw}{}", cfg.machine_id_salt).as_bytes());
    hex(&digest)[..16].to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn generate_key(cfg: &Config, machine_id: &str) -> String {
    let key_id: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .map(|c| (c as char).to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphanumeric())
        .take(6)
        .collect();
    let mut mac = Hmac::<Sha256>::new_from_slice(cfg.api_key_secret.as_bytes()).expect("hmac key");
    mac.update(format!("{machine_id}{key_id}").as_bytes());
    let crc = hex(&mac.finalize().into_bytes())[..8].to_string();
    format!("sk-{machine_id}-{key_id}-{crc}")
}

pub fn create_key(cfg: &Config, db: &Db, name: Option<String>) -> anyhow::Result<ApiKey> {
    let mid = machine_id(cfg);
    let k = ApiKey {
        id: uuid::Uuid::new_v4().to_string(),
        key: generate_key(cfg, &mid),
        name,
        is_active: true,
        created_at: crate::db::now_iso(),
        access_restricted: false,
        access_allow: vec![],
    };
    db.insert_api_key(&k, &mid)?;
    Ok(k)
}

/// Key lookup order matches upstream: Bearer, x-api-key, x-goog-api-key, ?key=.
pub fn extract_key(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(rest) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) {
            let t = rest.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    for h in ["x-api-key", "x-goog-api-key"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty()) {
            return Some(v.to_string());
        }
    }
    query.and_then(|q| {
        q.split('&').find_map(|kv| kv.strip_prefix("key=")).map(|v| v.to_string()).filter(|v| !v.is_empty())
    })
}

/// Loopback TCP peer without forwarding headers counts as local, like upstream's
/// `isLocalRequest`. Forwarded headers are only honoured from a loopback reverse proxy.
pub fn is_local(peer: IpAddr, headers: &HeaderMap) -> bool {
    let loopback = peer.is_loopback();
    let via_proxy = headers.contains_key("x-forwarded-for") || headers.contains_key("x-real-ip");
    if !loopback {
        return false;
    }
    if via_proxy {
        return false;
    }
    match headers.get("origin").and_then(|v| v.to_str().ok()) {
        None => true,
        Some(o) => o.contains("://localhost") || o.contains("://127.0.0.1") || o.contains("://[::1]"),
    }
}

/// Outcome of authenticating a proxy request.
#[derive(Debug, Clone)]
pub enum Caller {
    Local,
    Key(ApiKey),
}

impl Caller {
    pub fn key_value(&self) -> Option<&str> {
        match self {
            Caller::Key(k) => Some(k.key.as_str()),
            Caller::Local => None,
        }
    }

    /// Per-key allow list (upstream `keyAccess`): exact, case-insensitive match on
    /// `provider/model`.
    pub fn may_use(&self, provider: &str, model: &str) -> bool {
        match self {
            Caller::Local => true,
            Caller::Key(k) if !k.access_restricted => true,
            Caller::Key(k) => {
                let want = format!("{provider}/{model}").to_ascii_lowercase();
                k.access_allow.iter().any(|a| a.to_ascii_lowercase() == want)
            }
        }
    }
}

pub enum AuthError {
    Missing,
    Invalid,
}

pub fn authenticate(
    cfg: &Config,
    db: &Db,
    settings_require_key: bool,
    peer: IpAddr,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<Caller, AuthError> {
    let key = extract_key(headers, query);
    if let Some(k) = key {
        return match db.api_key(&k) {
            Ok(Some(row)) if row.is_active => Ok(Caller::Key(row)),
            _ => Err(AuthError::Invalid),
        };
    }
    let require = cfg.require_api_key || settings_require_key;
    if !require && is_local(peer, headers) {
        return Ok(Caller::Local);
    }
    Err(AuthError::Missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn key_shape() {
        let cfg = Config::parse_from(["mini-router"]);
        let k = generate_key(&cfg, "0123456789abcdef");
        let parts: Vec<_> = k.split('-').collect();
        assert_eq!(parts[0], "sk");
        assert_eq!(parts[1].len(), 16);
        assert_eq!(parts[2].len(), 6);
        assert_eq!(parts[3].len(), 8);
    }

    #[test]
    fn extract_order() {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", "k2".parse().unwrap());
        h.insert("authorization", "Bearer k1".parse().unwrap());
        assert_eq!(extract_key(&h, None).as_deref(), Some("k1"));
        assert_eq!(extract_key(&HeaderMap::new(), Some("a=1&key=k3")).as_deref(), Some("k3"));
    }
}
