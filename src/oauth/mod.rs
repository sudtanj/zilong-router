//! Providers that sign in with OAuth instead of an API key, ported from upstream
//! `src/lib/oauth/providers/*.js` (login) and `open-sse/services/tokenRefresh`
//! (refresh). Request-time quirks of these and the free providers live in `hooks`.
//!
//! Two login shapes, both driven from the dashboard:
//! * redirect (claude, xai, cline, clinepass): the user opens `authUrl`, signs in and
//!   pastes the URL the browser lands on (the redirect points at localhost, which is
//!   not this board) back into the dashboard, which calls `exchange`;
//! * device code (github, kimi, kilocode, minimax-code, nous, muse): the user enters
//!   `userCode` at `verificationUri` while the dashboard calls `poll`.
//!
//! Tokens are stored on the connection with upstream's field names (`accessToken`,
//! `refreshToken`, `expiresAt`, `providerSpecificData`), `authType` "oauth".

pub mod hooks;
pub mod opencode;
pub mod util;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::db::{now_iso, ProviderConnection};
use crate::state::AppState;
use util::{b64url, pkce, random_bytes};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Redirect,
    Device,
}

/// Providers whose login this build implements.
pub fn flow(provider: &str) -> Option<Flow> {
    match provider {
        "claude" | "xai" | "cline" | "clinepass" => Some(Flow::Redirect),
        "github" | "kimi" | "kilocode" | "minimax-code" | "minimax-code-global" | "nous" | "muse" => Some(Flow::Device),
        _ => None,
    }
}

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

const CLAUDE_CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_AUTHORIZE: &str = "https://claude.ai/oauth/authorize";
const CLAUDE_TOKEN: &str = "https://api.anthropic.com/v1/oauth/token";
const CLAUDE_SCOPES: &str = "org:create_api_key user:profile user:inference";

const XAI_CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_AUTHORIZE: &str = "https://auth.x.ai/oauth2/authorize";
const XAI_TOKEN: &str = "https://auth.x.ai/oauth2/token";
/// Fixed loopback redirect registered for the xAI client.
const XAI_REDIRECT: &str = "http://127.0.0.1:56121/callback";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";

const CLINE_AUTHORIZE: &str = "https://api.cline.bot/api/v1/auth/authorize";
const CLINE_TOKEN: &str = "https://api.cline.bot/api/v1/auth/token";
const CLINE_REFRESH: &str = "https://api.cline.bot/api/v1/auth/refresh";

const GITHUB_CLIENT: &str = "Iv1.b507a08c87ecfe98";
const GITHUB_DEVICE: &str = "https://github.com/login/device/code";
const GITHUB_TOKEN: &str = "https://github.com/login/oauth/access_token";
const GITHUB_USER: &str = "https://api.github.com/user";
const GITHUB_COPILOT_TOKEN: &str = "https://api.github.com/copilot_internal/v2/token";
/// Copilot client identity (registry `github.transport.copilot`).
pub const COPILOT_VSCODE: &str = "1.110.0";
pub const COPILOT_CHAT: &str = "0.38.0";
pub const COPILOT_UA: &str = "GitHubCopilotChat/0.38.0";
pub const COPILOT_API_VERSION: &str = "2025-04-01";

const KIMI_CLIENT: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const KIMI_DEVICE: &str = "https://auth.kimi.com/api/oauth/device_authorization";
const KIMI_TOKEN: &str = "https://auth.kimi.com/api/oauth/token";
const KIMI_AUTHORIZE_DEVICE: &str = "https://www.kimi.com/code/authorize_device";

const KILO_CODES: &str = "https://api.kilo.ai/api/device-auth/codes";
const KILO_PROFILE: &str = "https://api.kilo.ai/api/profile";

const MINIMAX_CLIENT: &str = "mcode-public";
const MINIMAX_SCOPE: &str = "agent.default";
const MINIMAX_AUDIENCE: &str = "agent-backend";

const NOUS_CLIENT: &str = "hermes-cli";
const NOUS_DEVICE: &str = "https://portal.nousresearch.com/api/oauth/device/code";
const NOUS_TOKEN: &str = "https://portal.nousresearch.com/api/oauth/token";
const NOUS_AGENT_KEY: &str = "https://portal.nousresearch.com/api/oauth/agent-key";
const NOUS_SCOPE: &str = "inference:invoke inference:mint_agent_key";

const MUSE_CLIENT: &str = "1031625952748946";
const MUSE_DEVICE: &str = "https://auth.meta.com/oidc/device/authorization/";
const MUSE_TOKEN: &str = "https://auth.meta.com/oidc/device/token/";
const MUSE_KEY: &str = "https://api.meta.ai/muse-code/key";

fn minimax_account(provider: &str) -> &'static str {
    if provider == "minimax-code-global" {
        "https://account.minimax.io"
    } else {
        "https://account.minimax.cn"
    }
}

// ----- pending logins --------------------------------------------------------------

struct Session {
    provider: String,
    verifier: String,
    state: String,
    redirect_uri: String,
    device_code: String,
    extra: Value,
    created: Instant,
}

const SESSION_TTL: Duration = Duration::from_secs(15 * 60);

static SESSIONS: LazyLock<Mutex<HashMap<String, Session>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn take_session(id: &str, provider: &str) -> Result<Session> {
    let mut s = SESSIONS.lock();
    s.retain(|_, v| v.created.elapsed() < SESSION_TTL);
    match s.remove(id) {
        Some(v) if v.provider == provider => Ok(v),
        _ => bail!("login session expired; start again"),
    }
}

fn put_session(id: String, s: Session) {
    SESSIONS.lock().insert(id, s);
}

// ----- HTTP helpers -----------------------------------------------------------------

type Headers<'a> = &'a [(&'a str, String)];

async fn read(resp: reqwest::Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let v = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| {
        json!({"error": format!("invalid_json_response (HTTP {status})"), "raw": text.chars().take(300).collect::<String>()})
    });
    (status, v)
}

fn with_headers(mut rb: reqwest::RequestBuilder, headers: Headers<'_>) -> reqwest::RequestBuilder {
    rb = rb.header("Accept", "application/json").timeout(Duration::from_secs(30));
    for (k, v) in headers {
        rb = rb.header(*k, v);
    }
    rb
}

async fn post_form(state: &AppState, url: &str, fields: &[(&str, &str)], headers: Headers<'_>) -> Result<(u16, Value)> {
    let rb = with_headers(state.http.post(url).form(fields), headers);
    Ok(read(rb.send().await?).await)
}

async fn post_json(state: &AppState, url: &str, body: &Value, headers: Headers<'_>) -> Result<(u16, Value)> {
    let rb = with_headers(state.http.post(url).json(body), headers);
    Ok(read(rb.send().await?).await)
}

async fn get_json(state: &AppState, url: &str, headers: Headers<'_>) -> Result<(u16, Value)> {
    let rb = with_headers(state.http.get(url), headers);
    Ok(read(rb.send().await?).await)
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string)
}

fn n(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))).filter(|x| *x > 0)
}

fn err_text(v: &Value) -> String {
    [s(v, "error_description"), s(v, "message"), s(v, "detail"), s(v, "error"), s(v, "raw")]
        .into_iter()
        .flatten()
        .next()
        .or_else(|| v.get("error").filter(|e| e.is_object()).map(|e| e.to_string()))
        .unwrap_or_else(|| v.to_string().chars().take(300).collect())
}

// ----- start / exchange / poll -------------------------------------------------------

/// Begin a login. Redirect flows answer `{flow, authUrl, redirectUri}`, device flows
/// `{flow, userCode, verificationUri, verificationUriComplete, interval, expiresIn}`;
/// both carry the `session` id for `exchange` / `poll`.
pub async fn start(state: &AppState, provider: &str, redirect_uri: Option<&str>) -> Result<Value> {
    let flow = flow(provider).ok_or_else(|| anyhow!("{provider} has no sign-in flow in this build"))?;
    let id = util::random_hex(16);
    let mut sess = Session {
        provider: provider.to_string(),
        verifier: String::new(),
        state: b64url(&random_bytes(32)),
        redirect_uri: redirect_uri
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("http://localhost:20128/callback")
            .to_string(),
        device_code: String::new(),
        extra: json!({}),
        created: Instant::now(),
    };
    let mut out = if flow == Flow::Redirect {
        let url = match provider {
            "claude" => {
                let (v, c) = pkce(32);
                sess.verifier = v;
                reqwest::Url::parse_with_params(
                    CLAUDE_AUTHORIZE,
                    &[
                        ("code", "true"),
                        ("client_id", CLAUDE_CLIENT),
                        ("response_type", "code"),
                        ("redirect_uri", sess.redirect_uri.as_str()),
                        ("scope", CLAUDE_SCOPES),
                        ("code_challenge", c.as_str()),
                        ("code_challenge_method", "S256"),
                        ("state", sess.state.as_str()),
                    ],
                )?
            }
            "xai" => {
                let (v, c) = pkce(96);
                sess.verifier = v;
                sess.redirect_uri = XAI_REDIRECT.into();
                let nonce = util::random_hex(16);
                reqwest::Url::parse_with_params(
                    XAI_AUTHORIZE,
                    &[
                        ("response_type", "code"),
                        ("client_id", XAI_CLIENT),
                        ("redirect_uri", XAI_REDIRECT),
                        ("scope", XAI_SCOPE),
                        ("code_challenge", c.as_str()),
                        ("code_challenge_method", "S256"),
                        ("state", sess.state.as_str()),
                        ("nonce", nonce.as_str()),
                        ("plan", "generic"),
                        ("referrer", "cli-proxy-api"),
                    ],
                )?
            }
            _ => reqwest::Url::parse_with_params(
                CLINE_AUTHORIZE,
                &[
                    ("client_type", "extension"),
                    ("callback_url", sess.redirect_uri.as_str()),
                    ("redirect_uri", sess.redirect_uri.as_str()),
                ],
            )?,
        };
        json!({"flow": "redirect", "authUrl": url.to_string(), "redirectUri": sess.redirect_uri})
    } else {
        device_start(state, provider, &mut sess).await?
    };
    out["session"] = json!(id);
    put_session(id, sess);
    Ok(out)
}

async fn device_start(state: &AppState, provider: &str, sess: &mut Session) -> Result<Value> {
    let fail = |status: u16, v: &Value| anyhow!("device code request failed ({status}): {}", err_text(v));
    let (status, v) = match provider {
        "github" => {
            post_form(state, GITHUB_DEVICE, &[("client_id", GITHUB_CLIENT), ("scope", "read:user")], &[]).await?
        }
        "kimi" => {
            let device_id = uuid::Uuid::new_v4().to_string();
            let h = util::kimi_headers(Some(&device_id));
            sess.extra = json!({"deviceId": device_id});
            post_form(state, KIMI_DEVICE, &[("client_id", KIMI_CLIENT)], &h).await?
        }
        "kilocode" => {
            let (status, v) = post_json(state, KILO_CODES, &json!({}), &[]).await?;
            if status == 429 {
                bail!("Too many pending authorization requests. Please try again later.");
            }
            if status >= 300 {
                return Err(fail(status, &v));
            }
            let code = s(&v, "code").ok_or_else(|| fail(status, &v))?;
            sess.device_code = code.clone();
            let url = s(&v, "verificationUrl").unwrap_or_default();
            return Ok(
                json!({"flow": "device", "userCode": code, "verificationUri": url, "verificationUriComplete": url,
                "interval": 3, "expiresIn": n(&v, "expiresIn").unwrap_or(300)}),
            );
        }
        "minimax-code" | "minimax-code-global" => {
            let (verifier, challenge) = pkce(32);
            sess.verifier = verifier;
            let url = format!("{}/oauth2/device/code", minimax_account(provider));
            let (status, v) = post_form(
                state,
                &url,
                &[
                    ("client_id", MINIMAX_CLIENT),
                    ("scope", MINIMAX_SCOPE),
                    ("audience", MINIMAX_AUDIENCE),
                    ("code_challenge", challenge.as_str()),
                    ("code_challenge_method", "S256"),
                ],
                &[],
            )
            .await?;
            let user_code = s(&v, "user_code");
            let uri = s(&v, "verification_uri_complete").or(s(&v, "verification_uri")).or(s(&v, "verification_url"));
            let (Some(user_code), Some(uri)) = (user_code, uri) else { return Err(fail(status, &v)) };
            // Some deployments only hand out a user code and expect polling with it.
            let by_user = s(&v, "device_code").is_none();
            sess.device_code = s(&v, "device_code").unwrap_or_else(|| user_code.clone());
            sess.extra = json!({"pollByUser": by_user});
            let interval = match n(&v, "interval") {
                None => 5,
                Some(i) if by_user && i >= 60 => (i + 500) / 1000,
                Some(i) => i,
            };
            let expires = n(&v, "expires_in")
                .or_else(|| {
                    n(&v, "expired_in").map(|e| {
                        if e < 1_000_000_000_000 {
                            e
                        } else {
                            (e / 1000 - util::unix_now()).max(1)
                        }
                    })
                })
                .unwrap_or(600);
            return Ok(
                json!({"flow": "device", "userCode": user_code, "verificationUri": uri, "verificationUriComplete": uri,
                "interval": interval.max(1), "expiresIn": expires}),
            );
        }
        "nous" => post_form(state, NOUS_DEVICE, &[("client_id", NOUS_CLIENT), ("scope", NOUS_SCOPE)], &[]).await?,
        "muse" => {
            post_form(state, MUSE_DEVICE, &[("client_id", MUSE_CLIENT)], &[("x-api-version", "1.0.0".into())]).await?
        }
        _ => bail!("{provider} has no device flow"),
    };
    let device_code = s(&v, "device_code").ok_or_else(|| fail(status, &v))?;
    let user_code = s(&v, "user_code").unwrap_or_default();
    sess.device_code = device_code;
    let mut uri = s(&v, "verification_uri").or(s(&v, "verification_url")).unwrap_or_default();
    let mut complete = s(&v, "verification_uri_complete");
    if provider == "kimi" {
        if uri.is_empty() {
            uri = KIMI_AUTHORIZE_DEVICE.into();
        }
        complete = complete.or_else(|| Some(format!("{KIMI_AUTHORIZE_DEVICE}?user_code={user_code}")));
    }
    Ok(
        json!({"flow": "device", "userCode": user_code, "verificationUriComplete": complete.unwrap_or_else(|| uri.clone()),
        "verificationUri": uri, "interval": n(&v, "interval").unwrap_or(5), "expiresIn": n(&v, "expires_in").unwrap_or(900)}),
    )
}

/// What a finished login yields; becomes the connection's fields.
#[derive(Default, Debug)]
pub struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_in: Option<i64>,
    pub api_key: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub psd: Map<String, Value>,
}

impl Tokens {
    fn standard(v: &Value) -> Tokens {
        Tokens {
            access: s(v, "access_token").unwrap_or_default(),
            refresh: s(v, "refresh_token"),
            expires_in: n(v, "expires_in"),
            ..Default::default()
        }
    }
}

/// Finish a redirect login with what the user pasted: the full callback URL, or the
/// bare code (Claude shows `code#state`).
pub async fn exchange(state: &AppState, provider: &str, session: &str, pasted: &str) -> Result<Value> {
    let sess = take_session(session, provider)?;
    let (code, got_state) = parse_callback(pasted);
    if code.is_empty() {
        bail!("no authorization code found in what was pasted");
    }
    if let Some(st) = got_state.as_deref() {
        if st != sess.state {
            bail!("state does not match this login; start again");
        }
    }
    let tokens = match provider {
        "claude" => {
            let body = json!({
                "code": code, "state": got_state.as_deref().unwrap_or(&sess.state), "grant_type": "authorization_code",
                "client_id": CLAUDE_CLIENT, "redirect_uri": sess.redirect_uri, "code_verifier": sess.verifier,
            });
            let (status, v) = post_json(state, CLAUDE_TOKEN, &body, &[]).await?;
            if status >= 300 {
                bail!("token exchange failed ({status}): {}", err_text(&v));
            }
            let mut t = Tokens::standard(&v);
            t.email = v.pointer("/account/email_address").and_then(Value::as_str).map(str::to_string);
            t
        }
        "xai" => {
            let (status, v) = post_form(
                state,
                XAI_TOKEN,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", XAI_CLIENT),
                    ("code", code.as_str()),
                    ("redirect_uri", sess.redirect_uri.as_str()),
                    ("code_verifier", sess.verifier.as_str()),
                ],
                &[],
            )
            .await?;
            if status >= 300 {
                bail!("xAI token exchange failed ({status}): {}", err_text(&v));
            }
            let mut t = Tokens::standard(&v);
            if let Some(id) = s(&v, "id_token") {
                t.email = util::jwt_payload(&id).and_then(|p| s(&p, "email"));
                t.psd.insert("idToken".into(), json!(id));
            }
            t
        }
        _ => cline_exchange(state, &code, &sess.redirect_uri).await?,
    };
    let conn = store(state, provider, tokens)?;
    Ok(conn_summary(&conn))
}

/// Code and state from a pasted callback URL, query string or bare code.
fn parse_callback(pasted: &str) -> (String, Option<String>) {
    let p = pasted.trim();
    if p.contains("code=") {
        let query = p.split_once('?').map(|(_, q)| q).unwrap_or(p);
        let query = query.split('#').next().unwrap_or(query);
        let url = format!("http://x/?{query}");
        if let Ok(u) = reqwest::Url::parse(&url) {
            let get = |k: &str| u.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
            if let Some(code) = get("code") {
                let (code, frag) = split_hash(&code);
                return (code, get("state").or(frag));
            }
        }
    }
    let (code, state) = split_hash(p);
    (code, state)
}

fn split_hash(code: &str) -> (String, Option<String>) {
    match code.split_once('#') {
        Some((c, st)) if !st.is_empty() => (c.to_string(), Some(st.to_string())),
        Some((c, _)) => (c.to_string(), None),
        None => (code.to_string(), None),
    }
}

async fn cline_exchange(state: &AppState, code: &str, redirect_uri: &str) -> Result<Tokens> {
    // The callback code is usually base64 JSON with the tokens already inside.
    let decoded = String::from_utf8_lossy(&util::b64_decode(code)).to_string();
    let data: Option<Value> = decoded.rfind('}').and_then(|i| serde_json::from_str(&decoded[..=i]).ok());
    let (access, refresh, email, expires_at, first, last) = match data.filter(|d| d.get("accessToken").is_some()) {
        Some(d) => (
            s(&d, "accessToken"),
            s(&d, "refreshToken"),
            s(&d, "email"),
            d.get("expiresAt").cloned(),
            s(&d, "firstName"),
            s(&d, "lastName"),
        ),
        None => {
            let body = json!({"grant_type": "authorization_code", "code": code, "client_type": "extension", "redirect_uri": redirect_uri});
            let (status, v) = post_json(state, CLINE_TOKEN, &body, &[]).await?;
            if status >= 300 {
                bail!("Cline token exchange failed ({status}): {}", err_text(&v));
            }
            let d = v.get("data").cloned().unwrap_or(v);
            (
                s(&d, "accessToken"),
                s(&d, "refreshToken"),
                d.pointer("/userInfo/email").and_then(Value::as_str).map(str::to_string),
                d.get("expiresAt").cloned(),
                None,
                None,
            )
        }
    };
    let mut t = Tokens {
        access: access.ok_or_else(|| anyhow!("Cline returned no access token"))?,
        refresh,
        expires_in: Some(expires_at.as_ref().and_then(util::secs_until).unwrap_or(3600)),
        email,
        ..Default::default()
    };
    if let Some(f) = first {
        t.psd.insert("firstName".into(), json!(f));
    }
    if let Some(l) = last {
        t.psd.insert("lastName".into(), json!(l));
    }
    Ok(t)
}

/// One poll of a device login: `{status: "pending" | "slow_down" | "done", connection?}`.
pub async fn poll(state: &AppState, provider: &str, session: &str) -> Result<Value> {
    let sess = take_session(session, provider)?;
    match poll_once(state, &sess).await {
        Ok(Some(tokens)) => {
            let conn = store(state, provider, tokens)?;
            Ok(json!({"status": "done", "connection": conn_summary(&conn)}))
        }
        Ok(None) => {
            put_session(session.to_string(), sess);
            Ok(json!({"status": "pending"}))
        }
        Err(PollError::SlowDown) => {
            put_session(session.to_string(), sess);
            Ok(json!({"status": "slow_down"}))
        }
        Err(PollError::Fatal(e)) => Err(e),
    }
}

enum PollError {
    SlowDown,
    Fatal(anyhow::Error),
}

impl From<anyhow::Error> for PollError {
    fn from(e: anyhow::Error) -> Self {
        PollError::Fatal(e)
    }
}

impl From<reqwest::Error> for PollError {
    fn from(e: reqwest::Error) -> Self {
        PollError::Fatal(e.into())
    }
}

/// Standard RFC 8628 token poll result: `Ok(None)` while pending.
fn device_result(v: &Value) -> std::result::Result<Option<Value>, PollError> {
    match s(v, "error").as_deref() {
        Some("authorization_pending") => Ok(None),
        Some("slow_down") => Err(PollError::SlowDown),
        Some(_) => Err(PollError::Fatal(anyhow!("sign-in failed: {}", err_text(v)))),
        None if s(v, "access_token").is_some() => Ok(Some(v.clone())),
        None => Err(PollError::Fatal(anyhow!("no access token received: {}", err_text(v)))),
    }
}

async fn poll_once(state: &AppState, sess: &Session) -> std::result::Result<Option<Tokens>, PollError> {
    let dc = sess.device_code.as_str();
    let p = sess.provider.as_str();
    match p {
        "github" => {
            let (_, v) = post_form(
                state,
                GITHUB_TOKEN,
                &[("client_id", GITHUB_CLIENT), ("device_code", dc), ("grant_type", DEVICE_GRANT)],
                &[],
            )
            .await?;
            let Some(v) = device_result(&v)? else { return Ok(None) };
            let mut t = Tokens::standard(&v);
            let gh = [
                ("X-GitHub-Api-Version", "2022-11-28".to_string()),
                ("User-Agent", "GitHubCopilotChat/0.26.7".to_string()),
            ];
            let auth = [("Authorization", format!("Bearer {}", t.access))];
            let headers: Vec<(&str, String)> = gh.iter().chain(auth.iter()).cloned().collect();
            if let Ok((200, user)) = get_json(state, GITHUB_USER, &headers).await {
                t.name = s(&user, "login").or(s(&user, "name"));
                t.email = s(&user, "email");
                t.psd.insert("githubUserId".into(), user.get("id").cloned().unwrap_or(Value::Null));
                t.psd.insert("githubLogin".into(), json!(s(&user, "login")));
            }
            if let Ok((tok, exp)) = copilot_token(state, &t.access).await {
                t.psd.insert("copilotToken".into(), json!(tok));
                t.psd.insert("copilotTokenExpiresAt".into(), exp);
            }
            Ok(Some(t))
        }
        "kimi" => {
            let device_id = s(&sess.extra, "deviceId");
            let h = util::kimi_headers(device_id.as_deref());
            let (_, v) = post_form(
                state,
                KIMI_TOKEN,
                &[("grant_type", DEVICE_GRANT), ("client_id", KIMI_CLIENT), ("device_code", dc)],
                &h,
            )
            .await?;
            let Some(v) = device_result(&v)? else { return Ok(None) };
            let mut t = Tokens::standard(&v);
            t.psd.insert("authMethod".into(), json!("device_code"));
            if let Some(d) = device_id {
                t.psd.insert("deviceId".into(), json!(d));
            }
            Ok(Some(t))
        }
        "kilocode" => {
            let resp = state.http.get(format!("{KILO_CODES}/{dc}")).timeout(Duration::from_secs(30)).send().await?;
            match resp.status().as_u16() {
                202 => return Ok(None),
                403 => return Err(PollError::Fatal(anyhow!("authorization denied"))),
                410 => return Err(PollError::Fatal(anyhow!("authorization code expired; start again"))),
                st if st >= 300 => return Err(PollError::Fatal(anyhow!("poll failed: HTTP {st}"))),
                _ => {}
            }
            let (_, v) = read(resp).await;
            let (Some("approved"), Some(token)) = (v.get("status").and_then(Value::as_str), s(&v, "token")) else {
                return Ok(None);
            };
            let mut t = Tokens { access: token.clone(), email: s(&v, "userEmail"), ..Default::default() };
            if let Ok((200, prof)) =
                get_json(state, KILO_PROFILE, &[("Authorization", format!("Bearer {token}"))]).await
            {
                if let Some(org) = prof.pointer("/organizations/0/id").filter(|o| !o.is_null()) {
                    t.psd.insert("orgId".into(), org.clone());
                }
            }
            Ok(Some(t))
        }
        "minimax-code" | "minimax-code-global" => {
            let by_user = sess.extra.get("pollByUser").and_then(Value::as_bool).unwrap_or(false);
            let url = format!("{}/oauth2/token", minimax_account(p));
            let code_field = if by_user { "user_code" } else { "device_code" };
            let (_, v) = post_form(
                state,
                &url,
                &[
                    ("grant_type", DEVICE_GRANT),
                    (code_field, dc),
                    ("client_id", MINIMAX_CLIENT),
                    ("code_verifier", sess.verifier.as_str()),
                ],
                &[],
            )
            .await?;
            match s(&v, "status").as_deref() {
                Some("pending") => return Ok(None),
                Some("slow_down") => return Err(PollError::SlowDown),
                Some("denied" | "access_denied") => return Err(PollError::Fatal(anyhow!("authorization denied"))),
                Some("expired" | "expired_token") => {
                    return Err(PollError::Fatal(anyhow!("sign-in expired; start again")))
                }
                Some(other) if s(&v, "access_token").is_none() => {
                    return Err(PollError::Fatal(anyhow!("unexpected status {other}")))
                }
                _ => {}
            }
            let Some(v) = device_result(&v)? else { return Ok(None) };
            let mut t = Tokens::standard(&v);
            t.expires_in = t.expires_in.or(Some(3600));
            Ok(Some(t))
        }
        "nous" => {
            let (_, v) = post_form(
                state,
                NOUS_TOKEN,
                &[("grant_type", DEVICE_GRANT), ("client_id", NOUS_CLIENT), ("device_code", dc)],
                &[],
            )
            .await?;
            let Some(v) = device_result(&v)? else { return Ok(None) };
            let mut t = Tokens::standard(&v);
            nous_agent_key(state, &mut t).await;
            Ok(Some(t))
        }
        "muse" => {
            let h = [("x-api-version", "1.0.0".to_string())];
            let (_, v) = post_form(
                state,
                MUSE_TOKEN,
                &[("grant_type", DEVICE_GRANT), ("device_code", dc), ("client_id", MUSE_CLIENT)],
                &h,
            )
            .await?;
            let Some(v) = device_result(&v)? else { return Ok(None) };
            let oauth_token = s(&v, "access_token").unwrap_or_default();
            Ok(Some(muse_key(state, &oauth_token).await.map_err(PollError::Fatal)?))
        }
        _ => Err(PollError::Fatal(anyhow!("{p} has no device flow"))),
    }
}

/// Nous inference runs on a short-lived agent key minted from the OAuth token.
async fn nous_agent_key(state: &AppState, t: &mut Tokens) {
    let h = [("Authorization", format!("Bearer {}", t.access))];
    match post_json(state, NOUS_AGENT_KEY, &json!({"min_ttl_seconds": 7200}), &h).await {
        Ok((st, v)) if st < 300 && s(&v, "api_key").is_some() => {
            t.api_key = s(&v, "api_key");
            for (k, f) in [("agentKeyId", "key_id"), ("agentKeyExpiresAt", "expires_at")] {
                if let Some(x) = v.get(f).filter(|x| !x.is_null()) {
                    t.psd.insert(k.into(), x.clone());
                }
            }
        }
        Ok((st, v)) => tracing::warn!("nous agent key mint failed ({st}): {}", err_text(&v)),
        Err(e) => tracing::warn!("nous agent key mint failed: {e}"),
    }
    if t.api_key.is_none() {
        t.api_key = Some(t.access.clone());
    }
    t.psd.insert("inferenceBaseUrl".into(), json!("https://inference-api.nousresearch.com/v1"));
}

/// Muse Code trades the OAuth token for a subscription API key.
async fn muse_key(state: &AppState, oauth_token: &str) -> Result<Tokens> {
    let h = [("Authorization", format!("Bearer {oauth_token}")), ("x-api-version", "1.0.0".to_string())];
    let mut last = (0, Value::Null);
    for attempt in 0..3u64 {
        last = post_json(state, MUSE_KEY, &json!({"onboard": true}), &h).await?;
        if !(last.0 == 429 || last.0 >= 500) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(5 * (attempt + 1))).await;
    }
    let (status, v) = last;
    if status >= 300 {
        let mut msg = [s(&v, "title"), s(&v, "detail")].into_iter().flatten().collect::<Vec<_>>().join(": ");
        if msg.is_empty() {
            msg = err_text(&v);
        }
        if let Some(u) = s(&v, "action_url") {
            msg.push_str(&format!(" — {u}"));
        }
        bail!("Muse Code key mint failed: {msg}");
    }
    if v.get("is_subs_active") == Some(&Value::Bool(false)) {
        bail!("Muse Code subscription is inactive — activate it on muse.ai first");
    }
    let Some(key) = s(&v, "api_key") else {
        let action = s(&v, "action_url").or(s(&v, "require_payment_action_url"));
        bail!("Muse Code subscription required{}", action.map(|a| format!(": {a}")).unwrap_or_default());
    };
    let mut t = Tokens { access: key, email: s(&v, "user_email").map(|e| e.to_lowercase()), ..Default::default() };
    t.psd.insert("authMethod".into(), json!("device_code"));
    t.psd.insert("oauthAccessToken".into(), json!(oauth_token));
    t.psd.insert("subscriptionTier".into(), v.get("subs_tier_name").cloned().unwrap_or(Value::Null));
    Ok(t)
}

/// Exchange a GitHub OAuth token for a Copilot API token: (token, expires_at).
async fn copilot_token(state: &AppState, github_token: &str) -> Result<(String, Value)> {
    let h = [
        ("Authorization", format!("token {github_token}")),
        ("User-Agent", COPILOT_UA.to_string()),
        ("Editor-Version", format!("vscode/{COPILOT_VSCODE}")),
        ("Editor-Plugin-Version", format!("copilot-chat/{COPILOT_CHAT}")),
        ("x-github-api-version", COPILOT_API_VERSION.to_string()),
    ];
    let (status, v) = get_json(state, GITHUB_COPILOT_TOKEN, &h).await?;
    match s(&v, "token") {
        Some(t) if status < 300 => Ok((t, v.get("expires_at").cloned().unwrap_or(Value::Null))),
        _ => bail!("Copilot token request failed ({status}): {} (does this account have Copilot?)", err_text(&v)),
    }
}

// ----- storage -------------------------------------------------------------------------

fn token_data(t: &Tokens) -> Value {
    let mut d = json!({"accessToken": t.access, "testStatus": "active", "lastRefreshAt": now_iso()});
    if let Some(r) = &t.refresh {
        d["refreshToken"] = json!(r);
    }
    if let Some(e) = t.expires_in {
        d["expiresAt"] = json!(util::iso_in(e));
        d["expiresIn"] = json!(e);
    }
    if let Some(k) = &t.api_key {
        d["apiKey"] = json!(k);
    }
    d
}

/// Save a finished login. Signing in again with the same account updates it in place.
fn store(state: &AppState, provider: &str, t: Tokens) -> Result<ProviderConnection> {
    if t.access.is_empty() {
        bail!("sign-in finished without an access token");
    }
    let existing = state
        .db
        .connections_for(provider, false)?
        .into_iter()
        .find(|c| c.auth_type == "oauth" && t.email.is_some() && c.email == t.email);
    let mut data = token_data(&t);
    if let Some(c) = existing {
        let mut psd = c.data.get("providerSpecificData").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
        for (k, v) in &t.psd {
            psd[k] = v.clone();
        }
        data["providerSpecificData"] = psd;
        let mut patch = crate::db::Db::unlock_patch(&c);
        if let (Some(p), Some(d)) = (patch.as_object_mut(), data.as_object()) {
            p.extend(d.clone());
        }
        state.db.patch_connection_data(&c.id, &patch)?;
        state.db.set_connection_active(&c.id, true)?;
        return Ok(state.db.get_connection(&c.id)?.unwrap_or(c));
    }
    data["providerSpecificData"] = Value::Object(t.psd.clone());
    let conn = ProviderConnection {
        id: uuid::Uuid::new_v4().to_string(),
        provider: provider.to_string(),
        auth_type: "oauth".into(),
        name: t.name.clone().or_else(|| t.email.clone()),
        email: t.email.clone(),
        priority: None,
        is_active: true,
        data,
        created_at: now_iso(),
        updated_at: now_iso(),
    };
    state.db.insert_connection(&conn)?;
    tracing::info!("{provider}: signed in as {}", conn.label());
    Ok(conn)
}

fn conn_summary(c: &ProviderConnection) -> Value {
    json!({"id": c.id, "provider": c.provider, "email": c.email, "name": c.name})
}

// ----- refresh -----------------------------------------------------------------------

/// One refresh at a time per account: refresh tokens rotate, so two concurrent
/// refreshes would burn each other's token.
static LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn refresh_lead_secs(provider: &str) -> i64 {
    match provider {
        "claude" => 4 * 3600,
        "minimax-code" | "minimax-code-global" => 600,
        _ => 300,
    }
}

fn psd<'a>(c: &'a ProviderConnection, k: &str) -> Option<&'a Value> {
    c.data.pointer(&format!("/providerSpecificData/{k}")).filter(|v| !v.is_null())
}

fn needs_refresh(c: &ProviderConnection) -> bool {
    if c.provider == "github" {
        let fresh = psd(c, "copilotToken").is_some()
            && psd(c, "copilotTokenExpiresAt").and_then(util::secs_until).map_or(true, |s| s > 300);
        if !fresh {
            return true;
        }
    }
    if c.provider == "nous" {
        if let Some(left) = psd(c, "agentKeyExpiresAt").and_then(util::secs_until) {
            if left < 300 {
                return true;
            }
        }
    }
    let has_refresh = c.data.get("refreshToken").and_then(Value::as_str).is_some_and(|r| !r.is_empty());
    has_refresh
        && c.data.get("expiresAt").and_then(util::secs_until).is_some_and(|left| left < refresh_lead_secs(&c.provider))
}

/// Make the account ready for a request: refresh OAuth tokens that are about to
/// expire, mint the GitHub Copilot token, fetch the MiMo free-tier JWT.
pub async fn prepare(state: &AppState, conn: ProviderConnection) -> Result<ProviderConnection> {
    if hooks::is_mimo(&conn.provider) {
        let mut conn = conn;
        conn.data["accessToken"] = json!(hooks::mimo_jwt(state, false).await?);
        return Ok(conn);
    }
    let token_based = conn.auth_type == "oauth" || conn.provider == "github";
    if !token_based || !needs_refresh(&conn) {
        return Ok(conn);
    }
    refresh_locked(state, conn, false).await
}

/// After a 401/403: refresh even if the token looks valid. `None` when the account
/// has nothing to refresh with.
pub async fn force_refresh(state: &AppState, conn: &ProviderConnection) -> Option<ProviderConnection> {
    if hooks::is_mimo(&conn.provider) {
        let mut c = conn.clone();
        c.data["accessToken"] = json!(hooks::mimo_jwt(state, true).await.ok()?);
        return Some(c);
    }
    let refreshable = (conn.auth_type == "oauth" || conn.provider == "github")
        && (conn.provider == "github"
            || conn.provider == "nous"
            || conn.data.get("refreshToken").and_then(Value::as_str).is_some());
    if !refreshable {
        return None;
    }
    match refresh_locked(state, conn.clone(), true).await {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!("{} ({}): token refresh failed: {e}", conn.provider, conn.label());
            None
        }
    }
}

async fn refresh_locked(state: &AppState, conn: ProviderConnection, force: bool) -> Result<ProviderConnection> {
    let lock = LOCKS.lock().entry(conn.id.clone()).or_default().clone();
    let _g = lock.lock().await;
    // Another request may have refreshed while we waited; the stored copy wins.
    let cur = state.db.get_connection(&conn.id)?.unwrap_or(conn);
    if !force && !needs_refresh(&cur) {
        return Ok(cur);
    }
    match refresh(state, &cur).await {
        Ok(patch) => {
            state.db.patch_connection_data(&cur.id, &patch)?;
            tracing::info!("{} ({}): token refreshed", cur.provider, cur.label());
            Ok(state.db.get_connection(&cur.id)?.unwrap_or(cur))
        }
        Err(e) => {
            let _ = state.db.patch_connection_data(
                &cur.id,
                &json!({"testStatus": "expired", "lastError": format!("token refresh failed: {e}"), "lastErrorAt": now_iso()}),
            );
            Err(e)
        }
    }
}

/// Patch for the connection's data after a refresh.
async fn refresh(state: &AppState, c: &ProviderConnection) -> Result<Value> {
    let rt = c.data.get("refreshToken").and_then(Value::as_str).unwrap_or("").to_string();
    let access = c.secret().unwrap_or("").to_string();
    let mut psd_map = c.data.get("providerSpecificData").and_then(Value::as_object).cloned().unwrap_or_default();
    let (status, v) = match c.provider.as_str() {
        "github" => {
            // The Copilot token is what expires; the GitHub token usually does not.
            let mut gh = access.clone();
            let mut patch = json!({});
            let tok = match copilot_token(state, &gh).await {
                Ok(t) => t,
                Err(e) if rt.is_empty() => return Err(e),
                Err(_) => {
                    let (st, v) = post_form(
                        state,
                        GITHUB_TOKEN,
                        &[
                            ("grant_type", "refresh_token"),
                            ("refresh_token", rt.as_str()),
                            ("client_id", GITHUB_CLIENT),
                        ],
                        &[],
                    )
                    .await?;
                    if st >= 300 || s(&v, "access_token").is_none() {
                        bail!("GitHub refresh failed ({st}): {}", err_text(&v));
                    }
                    patch = standard_patch(&v, &rt);
                    gh = s(&v, "access_token").unwrap_or_default();
                    copilot_token(state, &gh).await?
                }
            };
            psd_map.insert("copilotToken".into(), json!(tok.0));
            psd_map.insert("copilotTokenExpiresAt".into(), tok.1);
            patch["providerSpecificData"] = Value::Object(psd_map);
            return Ok(patch);
        }
        _ if rt.is_empty() => bail!("no refresh token; sign in again"),
        "claude" => {
            let body = json!({"grant_type": "refresh_token", "refresh_token": rt, "client_id": CLAUDE_CLIENT});
            post_json(state, CLAUDE_TOKEN, &body, &[]).await?
        }
        "xai" => {
            post_form(
                state,
                XAI_TOKEN,
                &[("grant_type", "refresh_token"), ("refresh_token", rt.as_str()), ("client_id", XAI_CLIENT)],
                &[],
            )
            .await?
        }
        "kimi" => {
            let h = util::kimi_headers(psd(c, "deviceId").and_then(Value::as_str));
            post_form(
                state,
                KIMI_TOKEN,
                &[("grant_type", "refresh_token"), ("refresh_token", rt.as_str()), ("client_id", KIMI_CLIENT)],
                &h,
            )
            .await?
        }
        "minimax-code" | "minimax-code-global" => {
            let url = format!("{}/oauth2/token", minimax_account(&c.provider));
            post_form(
                state,
                &url,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", rt.as_str()),
                    ("client_id", MINIMAX_CLIENT),
                    ("scope", MINIMAX_SCOPE),
                    ("audience", MINIMAX_AUDIENCE),
                ],
                &[],
            )
            .await?
        }
        "cline" | "clinepass" => {
            let body = json!({"refreshToken": rt, "grantType": "refresh_token", "clientType": "extension"});
            let (st, v) = post_json(state, CLINE_REFRESH, &body, &[]).await?;
            let d = v.get("data").cloned().unwrap_or(v);
            if st >= 300 || s(&d, "accessToken").is_none() {
                bail!("Cline refresh failed ({st}): {}", err_text(&d));
            }
            let mut patch = json!({"accessToken": s(&d, "accessToken"), "refreshToken": s(&d, "refreshToken").unwrap_or(rt),
                "lastRefreshAt": now_iso(), "testStatus": "active"});
            if let Some(left) = d.get("expiresAt").and_then(util::secs_until) {
                patch["expiresAt"] = json!(util::iso_in(left.max(1)));
            }
            return Ok(patch);
        }
        "nous" => {
            let h = [("x-nous-refresh-token", rt.clone())];
            let (st, v) = post_form(
                state,
                NOUS_TOKEN,
                &[("grant_type", "refresh_token"), ("refresh_token", rt.as_str()), ("client_id", NOUS_CLIENT)],
                &h,
            )
            .await?;
            if st >= 300 || s(&v, "access_token").is_none() {
                bail!("Nous refresh failed ({st}): {}", err_text(&v));
            }
            let mut t = Tokens::standard(&v);
            nous_agent_key(state, &mut t).await;
            let mut patch = standard_patch(&v, &rt);
            patch["apiKey"] = json!(t.api_key);
            psd_map.extend(t.psd);
            patch["providerSpecificData"] = Value::Object(psd_map);
            return Ok(patch);
        }
        p => bail!("{p} tokens cannot be refreshed; sign in again"),
    };
    if status >= 300 || s(&v, "access_token").is_none() {
        bail!("refresh failed ({status}): {}", err_text(&v));
    }
    Ok(standard_patch(&v, &rt))
}

fn standard_patch(v: &Value, old_refresh: &str) -> Value {
    let mut patch = json!({
        "accessToken": s(v, "access_token"),
        "refreshToken": s(v, "refresh_token").unwrap_or_else(|| old_refresh.to_string()),
        "lastRefreshAt": now_iso(),
        "testStatus": "active",
    });
    if let Some(e) = n(v, "expires_in") {
        patch["expiresAt"] = json!(util::iso_in(e));
        patch["expiresIn"] = json!(e);
    }
    if let Some(id) = s(v, "id_token") {
        patch["idToken"] = json!(id);
    }
    patch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pasted_callbacks() {
        let (c, st) = parse_callback("http://localhost:20129/callback?code=abc%2Bd&state=xyz");
        assert_eq!((c.as_str(), st.as_deref()), ("abc+d", Some("xyz")));
        assert_eq!(parse_callback("  abc#st "), ("abc".into(), Some("st".into())));
        assert_eq!(parse_callback("code=q1&state=s1").0, "q1");
        assert_eq!(parse_callback("plain").1, None);
    }

    #[test]
    fn refresh_window_follows_expiry_and_provider() {
        let mk = |provider: &str, data: Value| ProviderConnection {
            id: "c".into(),
            provider: provider.into(),
            auth_type: "oauth".into(),
            name: None,
            email: None,
            priority: None,
            is_active: true,
            data,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(needs_refresh(&mk("kimi", json!({"refreshToken": "r", "expiresAt": util::iso_in(60)}))));
        assert!(!needs_refresh(&mk("kimi", json!({"refreshToken": "r", "expiresAt": util::iso_in(3600)}))));
        // Claude refreshes 4 h ahead.
        assert!(needs_refresh(&mk("claude", json!({"refreshToken": "r", "expiresAt": util::iso_in(3600)}))));
        assert!(!needs_refresh(&mk("kimi", json!({"expiresAt": util::iso_in(1)}))));
        assert!(needs_refresh(&mk("github", json!({"accessToken": "gh"}))));
        let exp = util::unix_now() + 3600;
        assert!(!needs_refresh(&mk(
            "github",
            json!({"providerSpecificData": {"copilotToken": "t", "copilotTokenExpiresAt": exp}})
        )));
        assert_eq!(flow("claude"), Some(Flow::Redirect));
        assert_eq!(flow("github"), Some(Flow::Device));
        assert_eq!(flow("openai"), None);
    }
}
