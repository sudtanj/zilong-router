//! OAuth sign-in to upstream MCP servers, per the MCP authorization spec (2025-06-18,
//! with the 2025-03-26 fallbacks): a 401 points at the protected resource metadata
//! (RFC 9728), which names the authorization server; its metadata (RFC 8414 / OIDC)
//! gives the endpoints; the router registers itself (RFC 7591) unless a client id is
//! configured, then runs authorization code + PKCE with `resource` (RFC 8707).
//!
//! The dashboard starts a sign-in and opens `authUrl`; the authorization server sends
//! the browser back to `/mcp/oauth/callback` on the dashboard's origin (or the user
//! pastes that URL into the dashboard). Tokens live in `kv` scope `mcpAuth`, are sent
//! as `Authorization: Bearer` and refreshed shortly before they expire or after a 401.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{ServerConfig, Transport};
use crate::db::Db;
use crate::oauth::util::{b64url, pkce, random_bytes};
use crate::state::AppState;

const SCOPE: &str = "mcpAuth";
/// The path the dashboard's origin serves the redirect on.
pub const CALLBACK_PATH: &str = "/mcp/oauth/callback";
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Refresh this long before the access token expires.
const REFRESH_LEAD_SECS: i64 = 60;

/// What is stored per server: where to sign in, the client, and the tokens.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct AuthState {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
    /// `client_secret_basic` or `client_secret_post` when there is a secret.
    pub token_auth_method: String,
    pub client_id: String,
    pub client_secret: String,
    /// The redirect URI a dynamically registered client was registered with.
    pub registered_redirect: String,
    pub resource: String,
    pub scope: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds; 0 when the server did not say.
    pub expires_at: i64,
    /// The server rejected us and there is no (working) refresh token.
    pub needs_sign_in: bool,
    pub last_error: String,
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

pub fn load(db: &Db, id: &str) -> Option<AuthState> {
    db.kv_get(SCOPE, id).ok().flatten().and_then(|v| serde_json::from_value(v).ok())
}

fn save(db: &Db, id: &str, a: &AuthState) -> Result<()> {
    db.kv_set(SCOPE, id, Some(&serde_json::to_value(a)?))
}

/// Signs out: forgets the tokens (and the registered client).
pub fn forget(db: &Db, id: &str) -> Result<()> {
    db.kv_set(SCOPE, id, None)
}

/// The dashboard's view: never the tokens themselves.
pub fn summary(db: &Db, id: &str) -> Value {
    let Some(a) = load(db, id) else { return Value::Null };
    let status = if a.needs_sign_in {
        "needsSignIn"
    } else if !a.access_token.is_empty() {
        "signedIn"
    } else {
        return Value::Null;
    };
    let expires = (a.expires_at > 0).then(|| {
        time::OffsetDateTime::from_unix_timestamp(a.expires_at)
            .ok()
            .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
    });
    json!({
        "status": status,
        "expiresAt": expires.flatten(),
        "scope": a.scope,
        "refreshable": !a.refresh_token.is_empty(),
        "error": (!a.last_error.is_empty()).then_some(a.last_error),
    })
}

/// What agents and the dashboard are told when a server wants a (new) sign-in.
fn sign_in_error(id: &str) -> anyhow::Error {
    anyhow!(
        "MCP server `{id}` requires OAuth sign-in: open the zilong-router dashboard, MCP Gateway page, \
         and click Sign in on `{id}`"
    )
}

/// Records that `id` needs a sign-in, and returns the error to report.
pub fn mark_needs_sign_in(db: &Db, id: &str, why: &str) -> anyhow::Error {
    let mut a = load(db, id).unwrap_or_default();
    a.needs_sign_in = true;
    a.last_error = why.chars().take(300).collect();
    a.access_token.clear();
    a.refresh_token.clear();
    let _ = save(db, id, &a);
    sign_in_error(id)
}

// ----- tokens at request time ----------------------------------------------------------

/// Refreshes are serialised so two calls don't both spend a rotating refresh token.
static REFRESH: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// The access token to send to `cfg`, refreshed first when it is about to expire.
/// `None` when the server has no OAuth sign-in.
pub async fn token(state: &AppState, cfg: &ServerConfig) -> Result<Option<String>> {
    let Some(a) = load(&state.db, &cfg.id) else { return Ok(None) };
    if a.access_token.is_empty() {
        return Ok(None);
    }
    let expiring = a.expires_at > 0 && a.expires_at - REFRESH_LEAD_SECS <= now();
    if !expiring || a.refresh_token.is_empty() {
        return Ok(Some(a.access_token));
    }
    refreshed(state, cfg, &a.access_token).await.map(Some)
}

/// After a 401 for `failed` (the token we sent, if any): a refreshed token, or the
/// "sign in" error (and the server marked as needing one).
pub async fn recover(state: &AppState, cfg: &ServerConfig, failed: Option<&str>) -> Result<String> {
    let a = load(&state.db, &cfg.id).unwrap_or_default();
    if a.refresh_token.is_empty() {
        return Err(mark_needs_sign_in(&state.db, &cfg.id, "the server answered 401 and there is no token to refresh"));
    }
    refreshed(state, cfg, failed.unwrap_or_default()).await
}

/// Refreshes the token unless another call already replaced `stale` meanwhile.
async fn refreshed(state: &AppState, cfg: &ServerConfig, stale: &str) -> Result<String> {
    let _guard = REFRESH.lock().await;
    let mut a = load(&state.db, &cfg.id).ok_or_else(|| sign_in_error(&cfg.id))?;
    let fresh = a.expires_at == 0 || a.expires_at - REFRESH_LEAD_SECS > now();
    if !a.access_token.is_empty() && a.access_token != stale && fresh {
        return Ok(a.access_token);
    }
    let fields = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", a.refresh_token.clone()),
        ("resource", a.resource.clone()),
    ];
    match token_request(state, &a, fields).await {
        Ok(t) => {
            apply_tokens(&mut a, &t);
            save(&state.db, &cfg.id, &a)?;
            tracing::info!("mcp {}: OAuth token refreshed", cfg.id);
            Ok(a.access_token)
        }
        // The authorization server said no (revoked, expired): a new sign-in is needed.
        Err(TokenError::Rejected(why)) => Err(mark_needs_sign_in(&state.db, &cfg.id, &format!("refresh: {why}"))),
        Err(TokenError::Other(e)) => Err(e.context("refreshing the OAuth token")),
    }
}

enum TokenError {
    /// A 4xx answer such as `invalid_grant`.
    Rejected(String),
    Other(anyhow::Error),
}

/// POSTs to the token endpoint with the client's credentials.
async fn token_request(
    state: &AppState,
    a: &AuthState,
    mut fields: Vec<(&str, String)>,
) -> std::result::Result<Value, TokenError> {
    fields.retain(|(_, v)| !v.is_empty());
    let basic = !a.client_secret.is_empty() && a.token_auth_method == "client_secret_basic";
    if !basic {
        fields.push(("client_id", a.client_id.clone()));
        if !a.client_secret.is_empty() {
            fields.push(("client_secret", a.client_secret.clone()));
        }
    }
    let mut rb = state.http.post(&a.token_endpoint).header("Accept", "application/json").timeout(HTTP_TIMEOUT);
    if basic {
        rb = rb.basic_auth(&a.client_id, Some(&a.client_secret));
    }
    let resp = rb.form(&fields).send().await.map_err(|e| TokenError::Other(e.into()))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
        // Some servers (GitHub) answer form-encoded.
        let pairs = url_pairs(&text);
        if pairs.is_empty() {
            json!({"raw": text.chars().take(300).collect::<String>()})
        } else {
            Value::Object(pairs.into_iter().map(|(k, v)| (k, Value::String(v))).collect())
        }
    });
    let error = v.get("error").and_then(Value::as_str);
    if status.is_success() && error.is_none() && v.get("access_token").and_then(Value::as_str).is_some() {
        return Ok(v);
    }
    let why = [v.get("error_description"), v.get("error"), v.get("raw")]
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {status}"));
    if status.is_client_error() || error.is_some() {
        Err(TokenError::Rejected(why))
    } else {
        Err(TokenError::Other(anyhow!("token endpoint: {why}")))
    }
}

fn url_pairs(s: &str) -> Vec<(String, String)> {
    Url::parse(&format!("http://x/?{}", s.trim()))
        .map(|u| u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
        .unwrap_or_default()
}

fn apply_tokens(a: &mut AuthState, t: &Value) {
    let text = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    a.access_token = text("access_token");
    // Servers that don't rotate refresh tokens leave it out of the refresh answer.
    if !text("refresh_token").is_empty() {
        a.refresh_token = text("refresh_token");
    }
    let expires_in = t.get("expires_in").and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()));
    a.expires_at = expires_in.filter(|s| *s > 0).map_or(0, |s| now() + s);
    if !text("scope").is_empty() {
        a.scope = text("scope");
    }
    a.needs_sign_in = false;
    a.last_error.clear();
}

// ----- discovery -----------------------------------------------------------------------

/// The parameters of the `Bearer` challenge in a `WWW-Authenticate` header, with
/// lowercase names: `resource_metadata`, `scope`, `error`, ...
pub fn bearer_challenge(header: &str) -> Option<HashMap<String, String>> {
    let lower = header.to_ascii_lowercase();
    let start = lower.match_indices("bearer").map(|(i, _)| i).find(|&i| {
        let before = lower[..i].trim_end();
        (before.is_empty() || before.ends_with(',')) && !lower[i + 6..].starts_with(|c: char| c.is_alphanumeric())
    })?;
    let mut rest = &header[start + 6..];
    let mut out = HashMap::new();
    loop {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        let Some(eq) = rest.find('=') else { break };
        let name = rest[..eq].trim();
        // `Basic realm=...`: the next challenge starts.
        if name.is_empty() || name.contains(' ') {
            break;
        }
        rest = rest[eq + 1..].trim_start();
        let value = if let Some(quoted) = rest.strip_prefix('"') {
            let (mut value, mut escaped, mut end) = (String::new(), false, quoted.len());
            for (i, c) in quoted.char_indices() {
                match c {
                    _ if escaped => {
                        value.push(c);
                        escaped = false;
                    }
                    '\\' => escaped = true,
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    _ => value.push(c),
                }
            }
            rest = &quoted[end..];
            value
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let value = rest[..end].trim().to_string();
            rest = &rest[end..];
            value
        };
        out.insert(name.to_ascii_lowercase(), value);
    }
    Some(out)
}

/// Well-known URLs for `name` under `url`'s origin: with the path inserted after the
/// well-known prefix (RFC 9728 / RFC 8414), then at the root.
pub fn well_known(url: &Url, name: &str) -> Vec<String> {
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/{name}{path}"));
    }
    out.push(format!("{origin}/.well-known/{name}"));
    out
}

/// Where an authorization server (issuer URL) may publish its metadata: RFC 8414 and
/// OpenID Connect, path-inserted and (OIDC) path-appended.
pub fn as_metadata_urls(issuer: &Url) -> Vec<String> {
    let mut out = well_known(issuer, "oauth-authorization-server");
    let path = issuer.path().trim_end_matches('/');
    let oidc = well_known(issuer, "openid-configuration");
    if path.is_empty() {
        out.extend(oidc);
    } else {
        out.push(oidc[0].clone());
        out.push(format!("{}{path}/.well-known/openid-configuration", issuer.origin().ascii_serialization()));
    }
    out
}

/// The canonical URI of an MCP server, used as the `resource` parameter.
pub fn canonical_resource(url: &str) -> Result<String> {
    let mut u = Url::parse(url.trim()).context("invalid URL")?;
    u.set_fragment(None);
    u.set_query(None);
    let s = u.to_string();
    Ok(if u.path() == "/" { s.trim_end_matches('/').to_string() } else { s })
}

async fn get_json(state: &AppState, url: &str) -> Option<Value> {
    let resp = state.http.get(url).header("Accept", "application/json").timeout(HTTP_TIMEOUT).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok().filter(Value::is_object)
}

/// Asks the server without a token, for its `WWW-Authenticate` challenge.
async fn probe(state: &AppState, cfg: &ServerConfig) -> Option<String> {
    let rb = match cfg.transport {
        Transport::Http => {
            let init = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
                "protocolVersion": super::client::PROTOCOL_VERSION, "capabilities": {},
                "clientInfo": {"name": "zilong-router", "version": env!("CARGO_PKG_VERSION")}}});
            state
                .http
                .post(cfg.url.trim())
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .body(init.to_string())
        }
        Transport::Sse => state.http.get(cfg.url.trim()).header("Accept", "text/event-stream"),
    };
    let resp = rb.timeout(HTTP_TIMEOUT).send().await.ok()?;
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return None;
    }
    resp.headers().get(reqwest::header::WWW_AUTHENTICATE)?.to_str().ok().map(str::to_string)
}

/// Endpoints, scope and resource for `cfg`, found as the spec describes.
async fn discover(state: &AppState, cfg: &ServerConfig) -> Result<AuthState> {
    let server = Url::parse(cfg.url.trim()).context("invalid URL")?;
    let challenge = probe(state, cfg).await.and_then(|h| bearer_challenge(&h)).unwrap_or_default();

    let mut prm_urls: Vec<String> = challenge.get("resource_metadata").cloned().into_iter().collect();
    prm_urls.extend(well_known(&server, "oauth-protected-resource"));
    let mut prm = None;
    for u in prm_urls {
        if let Some(v) = get_json(state, &u).await {
            prm = Some(v);
            break;
        }
    }
    let prm = prm.unwrap_or_default();
    let issuer = prm
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find_map(Value::as_str))
        .map(str::to_string)
        // 2025-03-26: the MCP server's origin is the authorization server.
        .unwrap_or_else(|| server.origin().ascii_serialization());
    let issuer_url = Url::parse(&issuer).with_context(|| format!("invalid authorization server `{issuer}`"))?;

    let mut meta = None;
    for u in as_metadata_urls(&issuer_url) {
        if let Some(v) = get_json(state, &u).await.filter(|v| v.get("token_endpoint").is_some()) {
            meta = Some(v);
            break;
        }
    }
    let origin = issuer_url.origin().ascii_serialization();
    let meta = meta.unwrap_or_else(|| {
        json!({"authorization_endpoint": format!("{origin}/authorize"), "token_endpoint": format!("{origin}/token"),
            "registration_endpoint": format!("{origin}/register")})
    });
    let text = |k: &str| meta.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    if let Some(methods) = meta.get("code_challenge_methods_supported").and_then(Value::as_array) {
        if !methods.iter().any(|m| m == "S256") {
            bail!("the authorization server does not support PKCE (S256)");
        }
    }
    let methods = meta.get("token_endpoint_auth_methods_supported").and_then(Value::as_array);
    let post_ok = methods.map(|a| a.iter().any(|x| x == "client_secret_post")).unwrap_or(true);
    let token_auth_method = if post_ok { "client_secret_post" } else { "client_secret_basic" };

    let scope = if !cfg.oauth_scope.trim().is_empty() {
        cfg.oauth_scope.trim().to_string()
    } else if let Some(s) = challenge.get("scope").filter(|s| !s.is_empty()) {
        s.clone()
    } else {
        let scopes = prm.get("scopes_supported").and_then(Value::as_array);
        scopes.map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).unwrap_or_default()
    };
    let resource = match prm.get("resource").and_then(Value::as_str) {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => canonical_resource(&cfg.url)?,
    };
    Ok(AuthState {
        issuer,
        authorization_endpoint: text("authorization_endpoint"),
        token_endpoint: text("token_endpoint"),
        registration_endpoint: text("registration_endpoint"),
        token_auth_method: token_auth_method.to_string(),
        resource,
        scope,
        ..Default::default()
    })
}

/// Registers zilong-router with the authorization server (RFC 7591).
async fn register(state: &AppState, a: &mut AuthState, redirect_uri: &str) -> Result<()> {
    if a.registration_endpoint.is_empty() {
        bail!(
            "the authorization server does not support dynamic client registration; \
             enter an OAuth client id (and secret) in this server's settings"
        );
    }
    let body = json!({
        "client_name": "zilong-router",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let resp = state.http.post(&a.registration_endpoint).json(&body).timeout(HTTP_TIMEOUT).send().await?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or_default();
    let Some(id) = v.get("client_id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        let why = v.get("error_description").or(v.get("error")).and_then(Value::as_str).unwrap_or_default();
        bail!("client registration failed (HTTP {status}) {why}");
    };
    a.client_id = id.to_string();
    a.client_secret = v.get("client_secret").and_then(Value::as_str).unwrap_or_default().to_string();
    let method = v.get("token_endpoint_auth_method").and_then(Value::as_str).unwrap_or_default();
    if method.starts_with("client_secret") {
        a.token_auth_method = method.to_string();
    }
    a.registered_redirect = redirect_uri.to_string();
    Ok(())
}

// ----- sign-in ---------------------------------------------------------------------------

struct Pending {
    server: String,
    verifier: String,
    redirect_uri: String,
    draft: AuthState,
    created: Instant,
}

/// Sign-ins waiting for their callback, by `state`.
static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `uri` with its host swapped for `localhost` when it is plain http on another host.
/// Many authorization servers (Composio's, for one) register only https and loopback
/// redirects (RFC 8252), so a dashboard opened at `http://pi:20128` registers
/// `http://localhost:20128/...` instead and the user pastes the callback address.
fn loopback_redirect(uri: &str) -> Option<String> {
    let mut url = Url::parse(uri).ok()?;
    let host = url.host_str()?.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if url.scheme() != "http" || host == "localhost" || host == "127.0.0.1" || host == "::1" {
        return None;
    }
    url.set_host(Some("localhost")).ok()?;
    Some(url.to_string())
}

/// Begins a sign-in to `cfg`; the browser must reach `{origin}/mcp/oauth/callback`.
/// Answers `{session, authUrl, redirectUri}`.
pub async fn start(state: &AppState, cfg: &ServerConfig, origin: &str) -> Result<Value> {
    let mut redirect_uri = format!("{}{CALLBACK_PATH}", origin.trim().trim_end_matches('/'));
    Url::parse(&redirect_uri).context("invalid dashboard origin")?;
    let loopback = loopback_redirect(&redirect_uri);
    let mut a = discover(state, cfg).await?;
    if !cfg.oauth_client_id.trim().is_empty() {
        a.client_id = cfg.oauth_client_id.trim().to_string();
        a.client_secret = cfg.oauth_client_secret.clone();
    } else {
        // Reuse the client registered earlier with the same server and redirect URI.
        let reuse = load(&state.db, &cfg.id)
            .filter(|old| {
                old.issuer == a.issuer
                    && (old.registered_redirect == redirect_uri || loopback.as_ref() == Some(&old.registered_redirect))
            })
            .filter(|old| !old.client_id.is_empty());
        match reuse {
            Some(old) => {
                a.client_id = old.client_id;
                a.client_secret = old.client_secret;
                a.token_auth_method = old.token_auth_method;
                redirect_uri = old.registered_redirect.clone();
                a.registered_redirect = old.registered_redirect;
            }
            None => match (register(state, &mut a, &redirect_uri).await, loopback) {
                (Ok(()), _) => {}
                (Err(e), Some(loopback)) if !a.registration_endpoint.is_empty() => {
                    register(state, &mut a, &loopback)
                        .await
                        .with_context(|| format!("{e:#}; also with redirect {loopback}"))?;
                    redirect_uri = loopback;
                }
                (Err(e), _) => return Err(e),
            },
        }
    }

    let (verifier, challenge) = pkce(32);
    let session = b64url(&random_bytes(32));
    let mut url = Url::parse(&a.authorization_endpoint).context("invalid authorization endpoint")?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &a.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &session)
            .append_pair("resource", &a.resource);
        if !a.scope.is_empty() {
            q.append_pair("scope", &a.scope);
        }
    }
    let mut pending = PENDING.lock();
    pending.retain(|_, p| p.created.elapsed() < PENDING_TTL);
    let sign_in = Pending {
        server: cfg.id.clone(),
        verifier,
        redirect_uri: redirect_uri.clone(),
        draft: a,
        created: Instant::now(),
    };
    pending.insert(session.clone(), sign_in);
    Ok(json!({"session": session, "authUrl": url.to_string(), "redirectUri": redirect_uri}))
}

/// Completes the sign-in `session` with the authorization `code`; returns the server id.
/// `server`, when given, must be the server the sign-in was started for.
pub async fn finish(state: &AppState, session: &str, code: &str, server: Option<&str>) -> Result<String> {
    let p = {
        let mut pending = PENDING.lock();
        pending.retain(|_, p| p.created.elapsed() < PENDING_TTL);
        match pending.remove(session) {
            Some(p) if server.is_none() || server == Some(p.server.as_str()) => p,
            Some(p) => {
                pending.insert(session.to_string(), p);
                bail!("this sign-in belongs to another server");
            }
            None => bail!("sign-in session expired or unknown; start again"),
        }
    };
    if code.trim().is_empty() {
        bail!("no authorization code in the callback");
    }
    let mut a = p.draft;
    let fields = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.trim().to_string()),
        ("redirect_uri", p.redirect_uri),
        ("code_verifier", p.verifier),
        ("resource", a.resource.clone()),
    ];
    let t = match token_request(state, &a, fields).await {
        Ok(t) => t,
        Err(TokenError::Rejected(why)) => bail!("the authorization server refused the code: {why}"),
        Err(TokenError::Other(e)) => return Err(e),
    };
    apply_tokens(&mut a, &t);
    save(&state.db, &p.server, &a)?;
    tracing::info!("mcp {}: signed in with OAuth", p.server);
    // Reconnect with the token on next use.
    state.mcp.stop(&p.server).await;
    Ok(p.server)
}

/// The code and state in a pasted callback URL (or query string, or bare code).
pub fn parse_callback(pasted: &str) -> (String, Option<String>) {
    let p = pasted.trim();
    if !p.contains("code=") {
        return (p.to_string(), None);
    }
    let query = p.split_once('?').map_or(p, |(_, q)| q);
    let query = query.split('#').next().unwrap_or(query);
    let pairs: HashMap<String, String> = url_pairs(query).into_iter().collect();
    (pairs.get("code").cloned().unwrap_or_default(), pairs.get("state").cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_redirects_fall_back_to_localhost() {
        let lan = loopback_redirect("http://192.168.1.50:20128/mcp/oauth/callback");
        assert_eq!(lan.as_deref(), Some("http://localhost:20128/mcp/oauth/callback"));
        assert_eq!(loopback_redirect("http://pi/cb").as_deref(), Some("http://localhost/cb"));
        assert_eq!(loopback_redirect("https://pi.example.ts.net/cb"), None);
        assert_eq!(loopback_redirect("http://localhost:20128/cb"), None);
        assert_eq!(loopback_redirect("http://127.0.0.1:20128/cb"), None);
    }

    #[test]
    fn parses_bearer_challenges() {
        let h = r#"Bearer realm="mcp", resource_metadata="https://x.dev/prm/mcp", scope="read write""#;
        let c = bearer_challenge(h).unwrap();
        assert_eq!(c["resource_metadata"], "https://x.dev/prm/mcp");
        assert_eq!(c["scope"], "read write");
        assert_eq!(c["realm"], "mcp");

        let h = r#"Basic realm="a", Bearer error=invalid_token, error_description="a \"b\"""#;
        let c = bearer_challenge(h).unwrap();
        assert_eq!(c["error"], "invalid_token");
        assert_eq!(c["error_description"], "a \"b\"");
        assert!(!c.contains_key("realm"));
        assert!(bearer_challenge("Basic realm=\"a\"").is_none());
        assert!(bearer_challenge("Bearer").unwrap().is_empty());
    }

    #[test]
    fn builds_well_known_urls() {
        let u = Url::parse("https://mcp.example.com/v1/mcp/").unwrap();
        assert_eq!(
            well_known(&u, "oauth-protected-resource"),
            [
                "https://mcp.example.com/.well-known/oauth-protected-resource/v1/mcp",
                "https://mcp.example.com/.well-known/oauth-protected-resource"
            ]
        );
        let root = Url::parse("https://auth.example.com").unwrap();
        assert_eq!(
            as_metadata_urls(&root),
            [
                "https://auth.example.com/.well-known/oauth-authorization-server",
                "https://auth.example.com/.well-known/openid-configuration"
            ]
        );
        let tenant = Url::parse("https://auth.example.com/tenant1").unwrap();
        assert_eq!(
            as_metadata_urls(&tenant),
            [
                "https://auth.example.com/.well-known/oauth-authorization-server/tenant1",
                "https://auth.example.com/.well-known/oauth-authorization-server",
                "https://auth.example.com/.well-known/openid-configuration/tenant1",
                "https://auth.example.com/tenant1/.well-known/openid-configuration"
            ]
        );
    }

    #[test]
    fn canonical_resources() {
        assert_eq!(canonical_resource("HTTPS://MCP.Example.com/mcp#x").unwrap(), "https://mcp.example.com/mcp");
        assert_eq!(canonical_resource("https://mcp.example.com/").unwrap(), "https://mcp.example.com");
    }

    #[test]
    fn parses_pasted_callbacks() {
        let (code, state) = parse_callback("http://pi:20128/mcp/oauth/callback?code=abc&state=xyz");
        assert_eq!((code.as_str(), state.as_deref()), ("abc", Some("xyz")));
        assert_eq!(parse_callback("  abc ").0, "abc");
    }
}
