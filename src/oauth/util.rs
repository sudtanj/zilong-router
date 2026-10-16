//! Small helpers for the OAuth flows: base64, PKCE, JWT payloads, timestamps.

use rand::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Unpadded base64url.
pub fn b64url(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 4 / 3 + 3);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | ((*b as u32) << (16 - 8 * i)));
        for i in 0..chunk.len() + 1 {
            let c = B64[((n >> (18 - 6 * i)) & 63) as usize];
            out.push(match c {
                b'+' => '-',
                b'/' => '_',
                c => c as char,
            });
        }
    }
    out
}

/// Decode standard or url-safe base64, padded or not. Invalid characters end the input.
pub fn b64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => break,
        } as u32;
        acc = ((acc << 6) | v) & 0xff_ffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    out
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

pub fn random_hex(n: usize) -> String {
    random_bytes(n).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// (verifier, S256 challenge), as upstream `generatePKCE` (32 random bytes by default).
pub fn pkce(verifier_bytes: usize) -> (String, String) {
    let verifier = b64url(&random_bytes(verifier_bytes));
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// The payload of a JWT, without verifying it.
pub fn jwt_payload(token: &str) -> Option<Value> {
    let part = token.split('.').nth(1)?;
    serde_json::from_slice(&b64_decode(part)).ok()
}

pub fn is_jwt(token: &str) -> bool {
    let t = token.trim();
    t.starts_with("eyJ") && t.matches('.').count() >= 2
}

pub fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

pub fn iso_in(secs: i64) -> String {
    (time::OffsetDateTime::now_utc() + time::Duration::seconds(secs))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Seconds until an ISO timestamp, a unix timestamp in seconds, or one in ms.
pub fn secs_until(v: &Value) -> Option<i64> {
    let now = unix_now();
    match v {
        Value::String(s) => time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .ok()
            .map(|t| t.unix_timestamp() - now)
            .or_else(|| s.parse::<i64>().ok().map(|n| from_unix(n) - now)),
        Value::Number(n) => n.as_i64().map(|n| from_unix(n) - now),
        _ => None,
    }
}

fn from_unix(n: i64) -> i64 {
    if n > 1_000_000_000_000 {
        n / 1000
    } else {
        n
    }
}

/// Node-style arch names, as upstream sends them in device headers.
pub fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        a => a,
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// `X-Msh-*` headers Kimi expects on every call (upstream `buildKimiHeaders`).
pub fn kimi_headers(device_id: Option<&str>) -> Vec<(&'static str, String)> {
    let id = device_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("kimi-{}", unix_now() * 1000));
    vec![
        ("X-Msh-Platform", "9router".into()),
        ("X-Msh-Version", env!("CARGO_PKG_VERSION").into()),
        ("X-Msh-Device-Name", hostname()),
        ("X-Msh-Device-Model", format!("Linux {}", node_arch())),
        ("X-Msh-Device-Id", id),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_and_pkce_matches_rfc7636() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(b64_decode(&b64url(s.as_bytes())), s.as_bytes());
        }
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64_decode("Zm9vYg=="), b"foob");
        // RFC 7636 appendix B
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(b64url(&Sha256::digest(v.as_bytes())), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let (v, c) = pkce(32);
        assert_eq!(v.len(), 43);
        assert_eq!(c.len(), 43);
    }

    #[test]
    fn jwt_and_times() {
        let tok = format!("eyJhbGciOiJub25lIn0.{}.sig", b64url(br#"{"email":"a@b.c","exp":1}"#));
        assert!(is_jwt(&tok));
        assert_eq!(jwt_payload(&tok).unwrap()["email"], "a@b.c");
        assert!(secs_until(&serde_json::json!(iso_in(600))).unwrap() > 590);
        assert!(secs_until(&serde_json::json!(1)).unwrap() < 0);
    }
}
