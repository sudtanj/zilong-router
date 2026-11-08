//! Freebuff (Codebuff's free product, `github.com/CodebuffAI/codebuff`): sign-in as the
//! Freebuff CLI does it (a browser login the CLI polls for), the server model catalogue
//! with its rotating signed model handles, hour-long model-bound sessions kept alive by a
//! 45 s heartbeat while the account is in use, one agent run per burst of requests, the
//! per-install Ed25519 device key that signs catalogue, session and completion calls, and
//! the Buffy system prompt the free tier requires. The completions endpoint itself speaks
//! plain OpenAI chat completions, so nothing needs translating.
//!
//! Approximated: the terminal environment descriptor, the Bun user agent of session calls
//! and the Buffy prompt (only its required opening, not the CLI's full prompt) are fixed.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use reqwest::{Client, Method, Url};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Map, Value};

use super::util;
use crate::db::ProviderConnection;
use crate::state::AppState;

pub const ID: &str = "freebuff";
const API: &str = "https://www.codebuff.com";
const LOGIN_API: &str = "https://freebuff.com";
/// The one root agent every catalogue model runs under.
const AGENT_ID: &str = "base3-free-catalog";
const CLIENT_VERSION: &str = "0.2.36";
/// What Bun's `fetch` sends on the CLI's session and catalogue calls.
const BUN_USER_AGENT: &str = "Bun/1.3.0";
/// The AI SDK's user agent on completions; the server accepts only this SDK build.
const SDK_USER_AGENT: &str = "ai-sdk/openai-compatible/0.0.0-test/codebuff";
const COMPLETIONS_PATH: &str = "/api/v1/chat/completions";
const CLIENT_ENV: &str =
    "v1;in=1;out=1;tp=none;term=1;ct=1;sz=120x40;ci=0;ssh=0;l=1;p=shell;g=terminal;osc=1;tzo=0;px=none;tls=1;ca=0";
/// Required opening of every first-party root prompt (`hasFreebuffRootSystemPromptOpening`).
const BUFFY_PROMPT: &str = "You are Buffy, the coding agent behind Codebuff. You help users with software engineering tasks: fixing bugs, adding functionality, refactoring, and explaining code.";

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const HEARTBEAT: Duration = Duration::from_secs(45);
/// Heartbeats stop, and the session is left to lapse, after this long without a request.
const IDLE_STOP: Duration = Duration::from_secs(10 * 60);
/// A run is finished after this long without a request.
const RUN_IDLE: Duration = Duration::from_secs(2 * 60);
const SESSION_MARGIN: Duration = Duration::from_secs(60);
const MIN_REFRESH: Duration = Duration::from_secs(30);
const MAX_REFRESH: Duration = Duration::from_secs(6 * 3600);
const STALE: &str = "freebuff_catalog_stale";

static CLIENT_ID: LazyLock<String> = LazyLock::new(|| uuid::Uuid::new_v4().to_string());
static ACCOUNTS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<Account>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The install's Ed25519 key (PKCS#8) and the id the server registered it under.
type Device = (Vec<u8>, String);

#[derive(Clone)]
struct Row {
    key: String,
    handle: String,
    name: String,
}

struct Catalog {
    rows: Vec<Row>,
    fetch_id: Option<String>,
    refresh_at: Instant,
}

struct Session {
    instance_id: String,
    model_key: String,
    expires: Instant,
}

#[derive(Default)]
struct Account {
    catalog: Option<Catalog>,
    session: Option<Session>,
    run: Option<String>,
    steps: u32,
    last_used: Option<Instant>,
    beating: bool,
}

// ----- sign-in ------------------------------------------------------------------------

/// Ask for a login code. Returns what polling needs, the page to open and its lifetime in seconds.
pub async fn login_start(state: &AppState) -> Result<(Value, String, i64)> {
    let fingerprint = format!("codebuff-cli-{}", &util::b64url(&util::random_bytes(6))[..8]);
    let resp = state
        .http
        .post(format!("{LOGIN_API}/api/auth/cli/code"))
        .header("User-Agent", BUN_USER_AGENT)
        .json(&json!({"fingerprintId": fingerprint}))
        .timeout(HTTP_TIMEOUT)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Freebuff login code request failed ({})", resp.status());
    }
    let v: Value = resp.json().await?;
    let url = v.get("loginUrl").and_then(Value::as_str).ok_or_else(|| anyhow!("Freebuff returned no login URL"))?;
    let expires_at = v.get("expiresAt").map(|e| e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string()));
    let extra = json!({
        "fingerprintId": fingerprint,
        "fingerprintHash": v.get("fingerprintHash").cloned().unwrap_or(Value::Null),
        "expiresAt": expires_at,
    });
    let secs = v.get("expiresInMs").and_then(Value::as_i64).map_or(3600, |ms| ms / 1000);
    Ok((extra, url.to_string(), secs))
}

/// One poll of the login: the signed-in user once the browser step is done.
pub async fn login_poll(state: &AppState, extra: &Value) -> Result<Option<Value>> {
    let field = |k: &str| extra.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let url = Url::parse_with_params(
        &format!("{LOGIN_API}/api/auth/cli/status"),
        &[
            ("fingerprintId", field("fingerprintId")),
            ("fingerprintHash", field("fingerprintHash")),
            ("expiresAt", field("expiresAt")),
        ],
    )?;
    let resp = state.http.get(url).header("User-Agent", BUN_USER_AGENT).timeout(HTTP_TIMEOUT).send().await?;
    // 401 and friends mean the browser step is not finished yet.
    if !resp.status().is_success() {
        return Ok(None);
    }
    let v: Value = resp.json().await?;
    Ok(v.get("user").filter(|u| u.is_object()).cloned())
}

// ----- device key and signing ------------------------------------------------------------

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

/// `x-freebuff-device-*` headers: Ed25519 over `freebuff-device-v1\nMETHOD\npath\nts\nsha256(body)\nfetchId`.
fn sign(dev: &Device, method: &str, path: &str, body: &[u8], fetch_id: Option<&str>) -> Vec<(&'static str, String)> {
    let Ok(pair) = Ed25519KeyPair::from_pkcs8(&dev.0) else { return Vec::new() };
    let ts = now_ms();
    let payload = format!(
        "freebuff-device-v1\n{}\n{path}\n{ts}\n{}\n{}",
        method.to_ascii_uppercase(),
        util::sha256_hex(body),
        fetch_id.unwrap_or("")
    );
    vec![
        ("x-freebuff-device-key", dev.1.clone()),
        ("x-freebuff-device-ts", ts.to_string()),
        ("x-freebuff-device-sig", util::b64url(pair.sign(payload.as_bytes()).as_ref())),
    ]
}

fn psd_str<'a>(c: &'a ProviderConnection, k: &str) -> Option<&'a str> {
    c.data.pointer(&format!("/providerSpecificData/{k}")).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn device_of(c: &ProviderConnection) -> Option<Device> {
    Some((util::b64_decode(psd_str(c, "deviceKey")?), psd_str(c, "deviceKeyId")?.to_string()))
}

/// The account's device key, generated and registered on first use. Requests go out
/// unsigned (`None`) when registration fails; it is retried on the next request.
async fn ensure_device(state: &AppState, conn: &mut ProviderConnection, token: &str) -> Option<Device> {
    if let Some(d) = device_of(conn) {
        return Some(d);
    }
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).ok()?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).ok()?;
    let resp = state
        .http
        .post(format!("{API}/api/v1/freebuff/device-keys"))
        .bearer_auth(token)
        .header("User-Agent", BUN_USER_AGENT)
        .json(&json!({"publicKey": util::b64url(pair.public_key().as_ref()), "client": "cli"}))
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|e| tracing::warn!("freebuff: device key registration failed: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        tracing::warn!("freebuff: device key registration refused ({}); sending unsigned", resp.status());
        return None;
    }
    let answer: Value = resp.json().await.ok()?;
    let key_id = answer.get("keyId")?.as_str()?.to_string();
    let mut psd = conn.data.get("providerSpecificData").and_then(Value::as_object).cloned().unwrap_or_default();
    psd.insert("deviceKey".into(), json!(util::b64url(pkcs8.as_ref())));
    psd.insert("deviceKeyId".into(), json!(key_id));
    if let Err(e) = state.db.patch_connection_data(&conn.id, &json!({"providerSpecificData": psd})) {
        tracing::warn!("freebuff: could not store the device key: {e}");
    }
    conn.data["providerSpecificData"] = Value::Object(psd);
    device_of(conn)
}

// ----- API calls -------------------------------------------------------------------------

async fn send(rb: reqwest::RequestBuilder) -> Result<(u16, Value)> {
    let resp = rb.timeout(HTTP_TIMEOUT).send().await?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// A catalogue-aware session call (admission POST, heartbeat GET, release DELETE).
#[allow(clippy::too_many_arguments)]
async fn session_call(
    http: &Client,
    token: &str,
    dev: Option<&Device>,
    method: Method,
    path: &str,
    fetch_id: Option<&str>,
    instance: &str,
    extra: &[(&str, String)],
) -> Result<(u16, Value)> {
    let mut rb = http
        .request(method.clone(), format!("{API}{path}"))
        .bearer_auth(token)
        .header("User-Agent", BUN_USER_AGENT)
        .header("x-fb-timezone", "UTC")
        .header("x-freebuff-env", CLIENT_ENV)
        .header("x-freebuff-client-version", CLIENT_VERSION)
        .header("x-freebuff-multi-session", "1")
        .header("x-freebuff-purchase-continuity", "1")
        .header("x-freebuff-catalog-protocol", "1")
        .header("x-freebuff-instance-id", instance);
    if method != Method::GET {
        rb = rb.header("x-freebuff-desktop-attempt-id", instance.strip_prefix("cli:").unwrap_or(instance));
    }
    if let Some(f) = fetch_id {
        rb = rb.header("x-freebuff-catalog-fetch", f);
    }
    for (k, v) in extra {
        rb = rb.header(*k, v.as_str());
    }
    for (k, v) in dev.map(|d| sign(d, method.as_str(), path, b"", fetch_id)).unwrap_or_default() {
        rb = rb.header(k, v);
    }
    send(rb).await
}

fn failure(what: &str, status: u16, v: &Value) -> anyhow::Error {
    let code = ["error", "status"].iter().find_map(|k| v.get(*k).and_then(Value::as_str)).unwrap_or("");
    let msg = v.get("message").and_then(Value::as_str).unwrap_or("");
    anyhow!("Freebuff {what} failed ({status}) {code} {msg}")
}

fn parse_catalog(v: &Value, now: Instant) -> Option<Catalog> {
    let issued = v.get("issuedAt").and_then(Value::as_i64)?;
    let refresh = v.get("refreshAt").and_then(Value::as_i64)?;
    let rows = v
        .get("rows")?
        .as_array()?
        .iter()
        .filter(|r| r.get("access").and_then(Value::as_str) == Some("open"))
        .filter(|r| r.get("opensAt").and_then(Value::as_i64).map_or(true, |t| t <= issued))
        .filter_map(|r| {
            let s = |k: &str| r.get(k).and_then(Value::as_str).map(str::to_string);
            Some(Row { key: s("key")?, handle: s("handle")?, name: s("displayName")? })
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return None;
    }
    // Server-relative, so a wrong local clock cannot skip or loop refreshes.
    let wait = Duration::from_millis((refresh - issued).max(0) as u64).clamp(MIN_REFRESH, MAX_REFRESH);
    let fetch_id = v.get("fetchId").and_then(Value::as_str).map(str::to_string);
    Some(Catalog { rows, fetch_id, refresh_at: now + wait })
}

async fn fetch_catalog(http: &Client, token: &str, dev: Option<&Device>) -> Result<Catalog> {
    let path = "/api/v1/freebuff/models";
    let mut rb = http
        .get(format!("{API}{path}"))
        .bearer_auth(token)
        .header("User-Agent", BUN_USER_AGENT)
        .header("x-freebuff-catalog-protocol", "1")
        .header("x-freebuff-client", "cli");
    for (k, v) in dev.map(|d| sign(d, "GET", path, b"", None)).unwrap_or_default() {
        rb = rb.header(k, v);
    }
    let (status, v) = send(rb).await?;
    if status >= 300 {
        return Err(failure("model catalogue", status, &v));
    }
    parse_catalog(&v, Instant::now())
        .ok_or_else(|| anyhow!("Freebuff returned a model catalogue this build cannot read"))
}

fn norm(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// The catalogue row for a router model id ("glm-5.3-flash" matches "GLM 5.3 Flash"). Failing an
/// exact match, the shortest name starting with it wins: the CLI's names carry a suffix
/// ("DeepSeek V4.1 Flash 10/17", "MiMo v2.6 Flash Fast") that the router ids leave out.
fn find_row(c: &Catalog, model: &str) -> Option<Row> {
    let want = norm(model);
    let exact = c.rows.iter().find(|r| norm(&r.name) == want || norm(&r.key) == want);
    let prefixed = || c.rows.iter().filter(|r| norm(&r.name).starts_with(&want)).min_by_key(|r| r.name.len());
    exact.or_else(prefixed).cloned()
}

async fn admit(http: &Client, token: &str, dev: Option<&Device>, fetch_id: Option<&str>, row: &Row) -> Result<Session> {
    let instance = format!("cli:{}", uuid::Uuid::new_v4());
    let extra = [("x-freebuff-model", row.handle.clone()), ("x-freebuff-wallet-spend-limit", "session".to_string())];
    let path = "/api/v1/freebuff/session/admission";
    let (status, v) = session_call(http, token, dev, Method::POST, path, fetch_id, &instance, &extra).await?;
    if status >= 300 || v.get("status").and_then(Value::as_str) != Some("active") {
        return Err(failure("session", status, &v));
    }
    let left = Duration::from_millis(v.get("remainingMs").and_then(Value::as_u64).unwrap_or(3_600_000));
    let instance_id = v.get("instanceId").and_then(Value::as_str).unwrap_or(&instance).to_string();
    Ok(Session { instance_id, model_key: row.key.clone(), expires: Instant::now() + left })
}

/// Whether the session is still active on the server; also the liveness beat.
async fn beat(http: &Client, token: &str, dev: Option<&Device>, fetch_id: Option<&str>, instance: &str) -> bool {
    let extra = [("x-freebuff-heartbeat", "1".to_string()), ("x-freebuff-compact-session", "1".to_string())];
    let r = session_call(http, token, dev, Method::GET, "/api/v1/freebuff/session", fetch_id, instance, &extra).await;
    matches!(r, Ok((200, v)) if v.get("status").and_then(Value::as_str) == Some("active"))
}

async fn run_call(http: &Client, token: &str, body: &Value) -> Result<(u16, Value)> {
    let rb = http
        .post(format!("{API}/api/v1/agent-runs"))
        .bearer_auth(token)
        .header("User-Agent", BUN_USER_AGENT)
        .json(body);
    send(rb).await
}

async fn start_run(http: &Client, token: &str) -> Result<String> {
    let body = json!({"action": "START", "agentId": AGENT_ID, "ancestorRunIds": []});
    let (status, v) = run_call(http, token, &body).await?;
    match v.get("runId").and_then(Value::as_str) {
        Some(id) if status < 300 => Ok(id.to_string()),
        _ => Err(failure("agent run start", status, &v)),
    }
}

async fn finish_run(http: &Client, token: &str, run_id: &str, steps: u32) {
    let body = json!({"action": "FINISH", "runId": run_id, "status": "completed", "totalSteps": steps,
        "directCredits": 0, "totalCredits": 0, "steps": []});
    if let Err(e) = run_call(http, token, &body).await {
        tracing::debug!("freebuff: finishing the run failed: {e}");
    }
}

// ----- per-request preparation -------------------------------------------------------------

/// Make the account ready for a request on `model`: catalogue, model-bound session, run,
/// heartbeat. The connection comes back with the values the headers and body hooks need.
pub async fn prepare(state: &AppState, mut conn: ProviderConnection, model: &str) -> Result<ProviderConnection> {
    let token = conn.secret().ok_or_else(|| anyhow!("no Freebuff token; sign in again"))?.to_string();
    let acct = ACCOUNTS.lock().entry(conn.id.clone()).or_default().clone();
    let mut a = acct.lock().await;
    let dev = ensure_device(state, &mut conn, &token).await;
    let http = &state.http;

    if a.catalog.as_ref().map_or(true, |c| Instant::now() >= c.refresh_at) {
        match fetch_catalog(http, &token, dev.as_ref()).await {
            Ok(c) => a.catalog = Some(c),
            // Handles outlive their refresh time, so a failed refresh is survivable.
            Err(e) if a.catalog.is_some() => tracing::warn!("freebuff: catalogue refresh failed: {e}"),
            Err(e) => return Err(e),
        }
    }
    let found = a.catalog.as_ref().and_then(|c| find_row(c, model));
    let mut row = match found {
        Some(r) => r,
        None => {
            let names: Vec<&str> = a.catalog.iter().flat_map(|c| c.rows.iter().map(|r| r.name.as_str())).collect();
            bail!("Freebuff has no model '{model}' open to this account (available: {})", names.join(", "));
        }
    };
    let mut fetch_id = a.catalog.as_ref().and_then(|c| c.fetch_id.clone());

    let usable =
        a.session.as_ref().is_some_and(|s| s.model_key == row.key && s.expires > Instant::now() + SESSION_MARGIN);
    let mut live = usable;
    if usable && !a.beating {
        let instance = a.session.as_ref().map(|s| s.instance_id.clone()).unwrap_or_default();
        live = beat(http, &token, dev.as_ref(), fetch_id.as_deref(), &instance).await;
    }
    if !live {
        if let Some(old) = a.session.take() {
            let path = "/api/v1/freebuff/session/attempt";
            let fetch = fetch_id.as_deref();
            let _ = session_call(http, &token, dev.as_ref(), Method::DELETE, path, fetch, &old.instance_id, &[]).await;
        }
        if let Some(run) = a.run.take() {
            finish_run(http, &token, &run, a.steps).await;
        }
        a.steps = 0;
        let mut admitted = admit(http, &token, dev.as_ref(), fetch_id.as_deref(), &row).await;
        if admitted.as_ref().is_err_and(|e| e.to_string().contains(STALE)) {
            // A rotated handle: one fresh catalogue, one more try.
            let c = fetch_catalog(http, &token, dev.as_ref()).await?;
            row = find_row(&c, model).ok_or_else(|| anyhow!("Freebuff no longer offers '{model}'"))?;
            fetch_id = c.fetch_id.clone();
            a.catalog = Some(c);
            admitted = admit(http, &token, dev.as_ref(), fetch_id.as_deref(), &row).await;
        }
        a.session = Some(admitted?);
    }
    if a.run.is_none() {
        a.run = Some(start_run(http, &token).await?);
    }
    a.steps += 1;
    a.last_used = Some(Instant::now());
    if !a.beating {
        a.beating = true;
        tokio::spawn(heartbeat(http.clone(), token, dev, acct.clone()));
    }
    let instance = a.session.as_ref().map(|s| s.instance_id.clone()).unwrap_or_default();
    conn.data["freebuffRt"] = json!({
        "handle": row.handle, "instanceId": instance, "runId": a.run, "fetchId": fetch_id, "clientId": *CLIENT_ID,
    });
    Ok(conn)
}

/// Beats every 45 s while requests keep coming; finishes the idle run and stops after
/// `IDLE_STOP`, or as soon as the server no longer knows the session.
async fn heartbeat(http: Client, token: String, dev: Option<Device>, acct: Arc<tokio::sync::Mutex<Account>>) {
    loop {
        tokio::time::sleep(HEARTBEAT).await;
        let (keep, instance, fetch_id, finished) = {
            let mut a = acct.lock().await;
            let idle = a.last_used.map_or(Duration::MAX, |t| t.elapsed());
            let mut finished = None;
            if idle > RUN_IDLE {
                finished = a.run.take().map(|r| (r, a.steps));
                a.steps = 0;
            }
            let keep = idle <= IDLE_STOP && a.session.is_some();
            if !keep {
                a.beating = false;
            }
            let instance = a.session.as_ref().map(|s| s.instance_id.clone()).unwrap_or_default();
            (keep, instance, a.catalog.as_ref().and_then(|c| c.fetch_id.clone()), finished)
        };
        if let Some((run, steps)) = finished {
            finish_run(&http, &token, &run, steps).await;
        }
        if !keep {
            return;
        }
        if !beat(&http, &token, dev.as_ref(), fetch_id.as_deref(), &instance).await {
            let mut a = acct.lock().await;
            if a.session.as_ref().is_some_and(|s| s.instance_id == instance) {
                a.session = None;
                a.run = None;
            }
            a.beating = false;
            return;
        }
    }
}

/// The server said the session or the handle is gone (409 / 410 / 428, or a stale
/// catalogue): forget them so the next request admits again.
pub fn note_error(conn_id: &str, status: u16, msg: &str) {
    let stale = msg.contains(STALE);
    if !stale && !matches!(status, 409 | 410 | 428) {
        return;
    }
    let acct = ACCOUNTS.lock().get(conn_id).cloned();
    if let Some(acct) = acct {
        if let Ok(mut a) = acct.try_lock() {
            a.session = None;
            a.run = None;
            if stale {
                a.catalog = None;
            }
        }
    }
}

// ----- request hooks -------------------------------------------------------------------------

/// Headers of a completions call: the AI SDK's user agent, the catalogue fetch id and the
/// device signature over the exact body bytes (the executor serialises the same value).
pub fn headers(conn: &ProviderConnection, body: &Value, h: &mut HashMap<String, String>) {
    let fetch_id = conn.data.pointer("/freebuffRt/fetchId").and_then(Value::as_str);
    h.retain(|k, _| !k.eq_ignore_ascii_case("user-agent"));
    h.insert("User-Agent".into(), SDK_USER_AGENT.into());
    if let Some(f) = fetch_id {
        h.insert("x-freebuff-catalog-fetch".into(), f.into());
    }
    if let (Some(dev), Ok(bytes)) = (device_of(conn), serde_json::to_vec(body)) {
        for (k, v) in sign(&dev, "POST", COMPLETIONS_PATH, &bytes, fetch_id) {
            h.insert(k.into(), v);
        }
    }
}

/// The completions body as the CLI's agent run sends it: the model handle, the Buffy
/// prompt ahead of the caller's own system prompt, and the run metadata.
pub fn body(conn: &ProviderConnection, body: &Value) -> Value {
    let rt = conn.data.get("freebuffRt").cloned().unwrap_or(Value::Null);
    let mut b = body.clone();
    b["model"] = rt["handle"].clone();
    let prompt = format!("{BUFFY_PROMPT}\n\nCurrent date: {}.", time::OffsetDateTime::now_utc().date());
    if let Some(msgs) = b.get_mut("messages").and_then(Value::as_array_mut) {
        msgs.insert(0, json!({"role": "system", "content": prompt}));
    }
    let mut meta = Map::new();
    meta.insert("freebuff_instance_id".into(), rt["instanceId"].clone());
    meta.insert("freebuff_multi_session".into(), json!("1"));
    meta.insert("surface".into(), json!("cli"));
    meta.insert("freebuff_client_env".into(), json!(CLIENT_ENV));
    meta.insert("run_id".into(), rt["runId"].clone());
    meta.insert("client_id".into(), rt["clientId"].clone());
    meta.insert("cost_mode".into(), json!("free"));
    b["codebuff_metadata"] = Value::Object(meta);
    b["provider"] = json!({"data_collection": "deny"});
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(rt: Value, psd: Value) -> ProviderConnection {
        ProviderConnection {
            id: "c".into(),
            provider: ID.into(),
            auth_type: "oauth".into(),
            name: None,
            email: None,
            priority: None,
            is_active: true,
            data: json!({"accessToken": "t", "providerSpecificData": psd, "freebuffRt": rt}),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn catalog_json() -> Value {
        json!({"protocol": 1, "version": "v", "issuedAt": 1000, "refreshAt": 601000, "fetchId": "f1", "rows": [
            {"key": "glm", "handle": "fbm1.aaa", "displayName": "GLM 5.3 Flash", "access": "open"},
            {"key": "ds", "handle": "fbm1.bbb", "displayName": "DeepSeek V4.1 Flash", "access": "open"},
            {"key": "pro", "handle": "fbm1.ccc", "displayName": "Big Pro", "access": "locked"},
            {"key": "soon", "handle": "fbm1.ddd", "displayName": "Soon", "access": "open", "opensAt": 5000}
        ]})
    }

    #[test]
    fn catalog_keeps_open_rows_and_matches_models() {
        let c = parse_catalog(&catalog_json(), Instant::now()).unwrap();
        assert_eq!(c.rows.len(), 2);
        assert_eq!(c.fetch_id.as_deref(), Some("f1"));
        assert_eq!(find_row(&c, "glm-5.3-flash").unwrap().handle, "fbm1.aaa");
        assert_eq!(find_row(&c, "deepseek-v4.1-flash").unwrap().key, "ds");
        assert!(find_row(&c, "big-pro").is_none());
        let mut c = c;
        let row = |key: &str, name: &str| Row { key: key.into(), handle: format!("fbm1.{key}"), name: name.into() };
        c.rows = vec![row("a", "DeepSeek V4.1 Flash Fast 10/17"), row("b", "DeepSeek V4.1 Flash 10/17")];
        assert_eq!(find_row(&c, "deepseek-v4.1-flash").unwrap().key, "b");
        assert!(parse_catalog(&json!({"issuedAt": 1, "refreshAt": 2, "rows": []}), Instant::now()).is_none());
    }

    #[test]
    fn body_puts_buffy_first_and_adds_run_metadata() {
        let rt =
            json!({"handle": "fbm1.aaa", "instanceId": "cli:x", "runId": "r1", "fetchId": "f1", "clientId": "cid"});
        let c = conn(rt, json!({}));
        let src = json!({"model": "glm-5.3-flash", "stream": true, "tools": [{"type": "function"}],
            "messages": [{"role": "system", "content": "mine"}, {"role": "user", "content": "hi"}]});
        let b = body(&c, &src);
        assert_eq!(b["model"], "fbm1.aaa");
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert!(msgs[0]["content"].as_str().unwrap().starts_with("You are Buffy, the coding agent behind Codebuff."));
        assert_eq!(msgs[1]["content"], "mine");
        assert_eq!(b["tools"], src["tools"]);
        assert_eq!(b["codebuff_metadata"]["run_id"], "r1");
        assert_eq!(b["codebuff_metadata"]["cost_mode"], "free");
        assert_eq!(b["codebuff_metadata"]["freebuff_instance_id"], "cli:x");
    }

    #[test]
    fn completions_are_signed_over_the_body() {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let psd = json!({"deviceKey": util::b64url(pkcs8.as_ref()), "deviceKeyId": "k1"});
        let c = conn(json!({"fetchId": "f1"}), psd);
        let mut h = HashMap::from([("user-agent".to_string(), "x".to_string())]);
        headers(&c, &json!({"a": 1}), &mut h);
        assert_eq!(h["User-Agent"], SDK_USER_AGENT);
        assert_eq!(h["x-freebuff-device-key"], "k1");
        assert_eq!(h["x-freebuff-catalog-fetch"], "f1");
        let ts = &h["x-freebuff-device-ts"];
        let payload =
            format!("freebuff-device-v1\nPOST\n{COMPLETIONS_PATH}\n{ts}\n{}\nf1", util::sha256_hex(br#"{"a":1}"#));
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        assert_eq!(h["x-freebuff-device-sig"], util::b64url(pair.sign(payload.as_bytes()).as_ref()));
        let mut plain = HashMap::new();
        headers(&conn(json!({}), json!({})), &json!({}), &mut plain);
        assert!(!plain.contains_key("x-freebuff-device-sig"));
    }

    #[test]
    fn stale_errors_drop_the_session() {
        let acct = Arc::new(tokio::sync::Mutex::new(Account {
            catalog: parse_catalog(&catalog_json(), Instant::now()),
            session: Some(Session { instance_id: "cli:x".into(), model_key: "glm".into(), expires: Instant::now() }),
            ..Default::default()
        }));
        ACCOUNTS.lock().insert("note-test".into(), acct.clone());
        note_error("note-test", 500, "boom");
        assert!(acct.try_lock().unwrap().session.is_some());
        note_error("note-test", 409, STALE);
        let a = acct.try_lock().unwrap();
        assert!(a.session.is_none() && a.catalog.is_none());
    }
}
