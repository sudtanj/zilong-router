//! DeepSeekHashV1, the proof of work chat.deepseek.com asks for before every completion:
//! SHA3-256 (Keccak-f[1600], rate 136, pad byte 0x06) with the first of the 24 rounds
//! skipped. The server hands out `challenge`, a digest, plus a `salt` and an `expire_at`;
//! the answer is the nonce below `difficulty` whose digest of `{salt}_{expire_at}_{nonce}`
//! (nonce in decimal) equals the challenge. Ported from 9router's `deepseek-pow-hash.js`.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use super::util;

pub const ALGORITHM: &str = "DeepSeekHashV1";
/// Real challenges ask for about 144000 hashes; the cap only guards against nonsense.
const MAX_DIFFICULTY: u64 = 1_000_000;
const MAX_SALT_LEN: usize = 1024;
const BUDGET: Duration = Duration::from_secs(20);
const RATE: usize = 136;
/// DeepSeekHashV1 runs rounds 1..24; plain SHA3 starts at 0.
const FIRST_ROUND: usize = 1;

const RC: [u64; 24] = [
    0x0000000000000001,
    0x0000000000008082,
    0x800000000000808a,
    0x8000000080008000,
    0x000000000000808b,
    0x0000000080000001,
    0x8000000080008081,
    0x8000000000008009,
    0x000000000000008a,
    0x0000000000000088,
    0x0000000080008009,
    0x000000008000000a,
    0x000000008000808b,
    0x800000000000008b,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x000000000000800a,
    0x800000008000000a,
    0x8000000080008081,
    0x8000000000008080,
    0x0000000080000001,
    0x8000000080008008,
];
const RHO: [u32; 24] = [1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44];
const PI: [usize; 24] = [10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1];

fn permute(a: &mut [u64; 25], first_round: usize) {
    for rc in &RC[first_round..] {
        let mut c = [0u64; 5];
        for (x, cx) in c.iter_mut().enumerate() {
            *cx = a[x] ^ a[x + 5] ^ a[x + 10] ^ a[x + 15] ^ a[x + 20];
        }
        for x in 0..5 {
            let d = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
            for y in 0..5 {
                a[x + 5 * y] ^= d;
            }
        }
        let mut last = a[1];
        for (rho, pi) in RHO.iter().zip(PI) {
            let next = a[pi];
            a[pi] = last.rotate_left(*rho);
            last = next;
        }
        for y in 0..5 {
            let row = [a[5 * y], a[5 * y + 1], a[5 * y + 2], a[5 * y + 3], a[5 * y + 4]];
            for x in 0..5 {
                a[5 * y + x] = row[x] ^ (!row[(x + 1) % 5] & row[(x + 2) % 5]);
            }
        }
        a[0] ^= rc;
    }
}

/// A SHA3-256 sponge that can be cloned after the shared prefix was absorbed.
#[derive(Clone)]
struct Sponge {
    state: [u64; 25],
    pos: usize,
    first_round: usize,
}

impl Sponge {
    fn new(first_round: usize) -> Sponge {
        Sponge { state: [0; 25], pos: 0, first_round }
    }

    fn xor_byte(&mut self, pos: usize, b: u8) {
        self.state[pos / 8] ^= u64::from(b) << (8 * (pos % 8));
    }

    fn absorb(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.xor_byte(self.pos, b);
            self.pos += 1;
            if self.pos == RATE {
                permute(&mut self.state, self.first_round);
                self.pos = 0;
            }
        }
    }

    /// The 32-byte digest as four little-endian lanes.
    fn finish(mut self) -> [u64; 4] {
        self.xor_byte(self.pos, 0x06);
        self.xor_byte(RATE - 1, 0x80);
        permute(&mut self.state, self.first_round);
        [self.state[0], self.state[1], self.state[2], self.state[3]]
    }
}

fn digest_lanes(hex: &str) -> Option<[u64; 4]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    let mut lanes = [0u64; 4];
    for (lane, chunk) in lanes.iter_mut().zip(bytes.chunks_exact(8)) {
        *lane = u64::from_le_bytes(chunk.try_into().ok()?);
    }
    Some(lanes)
}

/// What `/api/v0/chat/create_pow_challenge` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub signature: String,
    pub difficulty: u64,
    pub expire_at: i64,
}

impl Challenge {
    pub fn parse(v: &Value) -> Result<Challenge> {
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Ok(Challenge {
            algorithm: text("algorithm").ok_or_else(|| anyhow!("challenge has no algorithm"))?,
            challenge: text("challenge").ok_or_else(|| anyhow!("challenge has no digest"))?,
            salt: text("salt").ok_or_else(|| anyhow!("challenge has no salt"))?,
            signature: text("signature").ok_or_else(|| anyhow!("challenge has no signature"))?,
            difficulty: v.get("difficulty").and_then(Value::as_u64).ok_or_else(|| anyhow!("challenge has no difficulty"))?,
            expire_at: v.get("expire_at").and_then(Value::as_i64).ok_or_else(|| anyhow!("challenge has no expiry"))?,
        })
    }
}

/// The nonce whose hash is the challenge, searched below `difficulty` within `budget`.
fn solve_within(ch: &Challenge, budget: Duration) -> Result<u64> {
    if ch.algorithm != ALGORITHM {
        bail!("unsupported proof-of-work algorithm {}", ch.algorithm);
    }
    let target = digest_lanes(&ch.challenge).ok_or_else(|| anyhow!("challenge digest is not 64 hex characters"))?;
    if ch.salt.is_empty() || ch.salt.len() > MAX_SALT_LEN || ch.difficulty == 0 || ch.difficulty > MAX_DIFFICULTY {
        bail!("proof-of-work challenge out of range");
    }
    let mut base = Sponge::new(FIRST_ROUND);
    base.absorb(format!("{}_{}_", ch.salt, ch.expire_at).as_bytes());
    let deadline = Instant::now() + budget;
    for nonce in 0..ch.difficulty {
        if nonce % 4096 == 0 && Instant::now() > deadline {
            bail!("proof of work not solved within {}s", budget.as_secs());
        }
        let mut s = base.clone();
        s.absorb(nonce.to_string().as_bytes());
        if s.finish() == target {
            return Ok(nonce);
        }
    }
    bail!("proof of work has no answer below difficulty {}", ch.difficulty)
}

/// Solve on the blocking pool: it is CPU work of up to a few hundred milliseconds.
pub async fn solve(ch: Challenge) -> Result<u64> {
    tokio::task::spawn_blocking(move || solve_within(&ch, BUDGET)).await?
}

/// Value of the `x-ds-pow-response` header: the answer plus the challenge fields, as
/// base64 of a JSON object (key order as the web client writes it).
pub fn response_header(ch: &Challenge, answer: u64, target_path: &str) -> String {
    let v = json!({
        "algorithm": ch.algorithm,
        "challenge": ch.challenge,
        "salt": ch.salt,
        "answer": answer,
        "signature": ch.signature,
        "target_path": target_path,
    });
    util::b64_std(v.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(lanes: [u64; 4]) -> String {
        lanes.iter().flat_map(|l| l.to_le_bytes()).map(|b| format!("{b:02x}")).collect()
    }

    fn hash(first_round: usize, msg: &[u8]) -> String {
        let mut s = Sponge::new(first_round);
        s.absorb(msg);
        hex(s.finish())
    }

    fn challenge(digest: &str, salt: &str) -> Challenge {
        Challenge {
            algorithm: ALGORITHM.into(),
            challenge: digest.into(),
            salt: salt.into(),
            signature: "sig".into(),
            difficulty: 5000,
            expire_at: 1790000000,
        }
    }

    #[test]
    fn full_round_sponge_is_sha3_256() {
        assert_eq!(hash(0, b""), "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a");
    }

    // Digests computed with an independent implementation of the 23-round variant
    // (the 9router reference skips round 0), one of them crossing the 136-byte block.
    #[test]
    fn skipping_round_zero_changes_the_digest() {
        assert_eq!(
            hash(FIRST_ROUND, b"a1b2c3d4e5f6_1790000000_4242"),
            "56dee5d520c404d592d44b55fda4b74ba48b500afb4e5b3b32ae11a5b3d6b2ae"
        );
        let long = format!("{}_1790000000_777", "s".repeat(130));
        assert_eq!(
            hash(FIRST_ROUND, long.as_bytes()),
            "99841f7cd4898ace952a939abc1fe80c7b5a2e6e2713d2cdf55a9b9da47f5f49"
        );
    }

    #[test]
    fn solver_finds_the_known_nonce() {
        let ch = challenge("56dee5d520c404d592d44b55fda4b74ba48b500afb4e5b3b32ae11a5b3d6b2ae", "a1b2c3d4e5f6");
        assert_eq!(solve_within(&ch, BUDGET).unwrap(), 4242);
        let long = challenge("99841f7cd4898ace952a939abc1fe80c7b5a2e6e2713d2cdf55a9b9da47f5f49", &"s".repeat(130));
        assert_eq!(solve_within(&long, BUDGET).unwrap(), 777);
    }

    #[test]
    fn rejects_bad_challenges() {
        let ok = "56dee5d520c404d592d44b55fda4b74ba48b500afb4e5b3b32ae11a5b3d6b2ae";
        let mut ch = challenge(ok, "a1b2c3d4e5f6");
        ch.algorithm = "Other".into();
        assert!(solve_within(&ch, BUDGET).is_err());
        assert!(solve_within(&challenge("zz", "a1b2c3d4e5f6"), BUDGET).is_err());
        let mut hard = challenge(ok, "a1b2c3d4e5f6");
        hard.difficulty = 100;
        assert!(solve_within(&hard, BUDGET).unwrap_err().to_string().contains("no answer"));
    }

    #[test]
    fn answer_header_is_base64_json() {
        let ch = challenge("ab", "s");
        let h = response_header(&ch, 7, "/api/v0/chat/completion");
        let v: Value = serde_json::from_slice(&util::b64_decode(&h)).unwrap();
        assert_eq!(v["answer"], 7);
        assert_eq!(v["target_path"], "/api/v0/chat/completion");
        assert_eq!(v["signature"], "sig");
    }
}
