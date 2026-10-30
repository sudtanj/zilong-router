//! zilong-router as the OAuth 2.1 authorization server of its own MCP endpoint, so
//! clients that follow the MCP authorization spec (Claude and ChatGPT connectors, Cursor,
//! the MCP Inspector) connect with "Sign in" instead of a pasted API key.
//!
//! * A 401 from `/mcp` points at `/.well-known/oauth-protected-resource/mcp` (RFC 9728),
//!   which names the router itself as the authorization server; its metadata (RFC 8414,
//!   also served as `openid-configuration`) lists the endpoints below.
//! * `POST /oauth/register`: dynamic client registration (RFC 7591).
//! * `/oauth/authorize`: a consent page. The user proves they own the router (dashboard
//!   password or dashboard session) and picks the API key the client will act as.
//! * `POST /oauth/token`: authorization code with PKCE (S256 only), rotating refresh tokens.
//!
//! Clients and tokens live in `kv` (scopes `mcpOAuthClients` and `mcpOAuthTokens`, tokens
//! as sha256 hashes). A token stands for the chosen API key: the client gets that key's
//! restrictions, and disabling or deleting the key ends its tokens. `auth::authenticate`
//! accepts access tokens wherever an API key is accepted (`/mcp` and `/v1`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use parking_lot::Mutex;
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db::{ApiKey, Db};
use crate::oauth::util::{b64_decode, b64url, random_bytes, sha256_hex};
use crate::state::AppState;

const CLIENTS: &str = "mcpOAuthClients";
const TOKENS: &str = "mcpOAuthTokens";
/// Registered clients kept; registering one more forgets the oldest.
const MAX_CLIENTS: usize = 200;
/// Consent pages waiting for an answer.
const MAX_PENDING: usize = 100;
const REQUEST_TTL: Duration = Duration::from_secs(10 * 60);
const CODE_TTL: Duration = Duration::from_secs(60);
const ACCESS_TTL_SECS: i64 = 3600;
const REFRESH_TTL_SECS: i64 = 30 * 24 * 3600;
const ACCESS_PREFIX: &str = "zra_";
const REFRESH_PREFIX: &str = "zrr_";
/// Wrong passwords allowed on one consent page.
const MAX_TRIES: u32 = 5;
/// `INITIAL_PASSWORD`'s default: too well known to approve anything with.
const DEFAULT_PASSWORD: &str = "123456";
/// `lastUsed` is written at most this often per client.
const LAST_USED_EVERY_SECS: i64 = 60;

/// A registered client.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
struct Client {
    client_id: String,
    client_name: String,
    redirect_uris: Vec<String>,
    /// sha256 of the secret of clients registered with `client_secret_*`.
    secret_hash: String,
    created_at: i64,
    /// The API key of the latest approval; empty until the user approves the client.
    key_id: String,
    connected_at: i64,
    last_used: i64,
}

/// What a token (stored under its hash) stands for.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
struct Token {
    refresh: bool,
    client_id: String,
    key_id: String,
    expires_at: i64,
}

/// An authorization request waiting on the consent page, by its form token.
struct Pending {
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    challenge: String,
    /// The user proved they own the router (password or dashboard session).
    signed_in: bool,
    tries: u32,
    created: Instant,
}

struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    key_id: String,
    created: Instant,
}

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static CODES: LazyLock<Mutex<HashMap<String, Code>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn routes() -> Router<Arc<AppState>> {
    let mut r = Router::new()
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize_page).post(authorize_submit))
        .route("/oauth/token", post(token_endpoint));
    for path in ["", "/mcp", "/mcp/all"] {
        r = r
            .route(&format!("/.well-known/oauth-protected-resource{path}"), get(resource_metadata))
            .route(&format!("/.well-known/oauth-authorization-server{path}"), get(server_metadata))
            .route(&format!("/.well-known/openid-configuration{path}"), get(server_metadata));
    }
    r
}

// ----- storage ---------------------------------------------------------------------------

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn iso(t: i64) -> Option<String> {
    if t <= 0 {
        return None;
    }
    let t = time::OffsetDateTime::from_unix_timestamp(t).ok()?;
    t.format(&time::format_description::well_known::Rfc3339).ok()
}

fn load<T: DeserializeOwned>(db: &Db, scope: &str, key: &str) -> Option<T> {
    db.kv_get(scope, key).ok().flatten().and_then(|v| serde_json::from_value(v).ok())
}

fn store<T: Serialize>(db: &Db, scope: &str, key: &str, v: &T) -> anyhow::Result<()> {
    db.kv_set(scope, key, Some(&serde_json::to_value(v)?))
}

fn all<T: DeserializeOwned>(db: &Db, scope: &str) -> Vec<(String, T)> {
    let rows = db.kv_scope(scope).unwrap_or_default();
    rows.into_iter().filter_map(|(k, v)| Some((k, serde_json::from_value(v).ok()?))).collect()
}

fn find_client(db: &Db, id: &str) -> Option<Client> {
    load(db, CLIENTS, id)
}

/// Tokens are stored and looked up by this.
fn token_hash(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

fn active_key(db: &Db, id: &str) -> Option<ApiKey> {
    db.api_keys().ok()?.into_iter().find(|k| k.id == id && k.is_active)
}

/// The API key an access token from `/oauth/token` stands for, while the token is valid,
/// its client is not revoked and the key is active.
pub fn key_for_token(db: &Db, token: &str) -> Option<ApiKey> {
    if !token.starts_with(ACCESS_PREFIX) {
        return None;
    }
    let t: Token = load(db, TOKENS, &token_hash(token))?;
    let at = now();
    if t.refresh || t.expires_at <= at {
        return None;
    }
    let mut client = find_client(db, &t.client_id)?;
    let key = active_key(db, &t.key_id)?;
    if at - client.last_used >= LAST_USED_EVERY_SECS {
        client.last_used = at;
        let _ = store(db, CLIENTS, &client.client_id, &client);
    }
    Some(key)
}

/// A new access and refresh token for `client_id` acting as `key_id`; expired tokens are
/// dropped on the way.
fn issue(db: &Db, client_id: &str, key_id: &str) -> anyhow::Result<Value> {
    let at = now();
    for (hash, t) in all::<Token>(db, TOKENS) {
        if t.expires_at <= at {
            db.kv_set(TOKENS, &hash, None)?;
        }
    }
    let access = format!("{ACCESS_PREFIX}{}", b64url(&random_bytes(32)));
    let refresh = format!("{REFRESH_PREFIX}{}", b64url(&random_bytes(32)));
    for (secret, is_refresh, ttl) in [(&access, false, ACCESS_TTL_SECS), (&refresh, true, REFRESH_TTL_SECS)] {
        let t = Token {
            refresh: is_refresh,
            client_id: client_id.to_string(),
            key_id: key_id.to_string(),
            expires_at: at + ttl,
        };
        store(db, TOKENS, &token_hash(secret), &t)?;
    }
    Ok(json!({"access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TTL_SECS, "refresh_token": refresh}))
}

/// Forgets a client and every token issued to it; false when it was unknown.
pub fn revoke(db: &Db, client_id: &str) -> anyhow::Result<bool> {
    let known = find_client(db, client_id).is_some();
    for (hash, t) in all::<Token>(db, TOKENS) {
        if t.client_id == client_id {
            db.kv_set(TOKENS, &hash, None)?;
        }
    }
    db.kv_set(CLIENTS, client_id, None)?;
    Ok(known)
}

/// The clients the user approved, for the dashboard (never tokens or secrets).
pub fn connected(db: &Db) -> Vec<Value> {
    let keys = db.api_keys().unwrap_or_default();
    let mut list: Vec<Client> = all::<Client>(db, CLIENTS).into_iter().map(|(_, c)| c).collect();
    list.retain(|c| !c.key_id.is_empty());
    list.sort_by_key(|c| std::cmp::Reverse(c.connected_at));
    let mut out = Vec::new();
    for c in list {
        let key = keys.iter().find(|k| k.id == c.key_id);
        out.push(json!({
            "id": c.client_id,
            "name": c.client_name,
            "redirect": redirect_host(c.redirect_uris.first().map_or("", String::as_str)),
            "keyId": c.key_id,
            "keyName": key.map(|k| k.name.clone().unwrap_or_else(|| crate::usage::mask_key(&k.key))),
            "keyActive": key.is_some_and(|k| k.is_active),
            "connectedAt": iso(c.connected_at),
            "lastUsed": iso(c.last_used),
        }));
    }
    out
}

// ----- discovery ---------------------------------------------------------------------------

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let v = headers.get(name)?.to_str().ok()?;
    Some(v.split(',').next().unwrap_or(v).trim()).filter(|v| !v.is_empty())
}

/// The router's origin as the client sees it, also behind a reverse proxy; the issuer.
pub fn origin(headers: &HeaderMap) -> String {
    let proto = header_value(headers, "x-forwarded-proto").filter(|p| matches!(*p, "https" | "http"));
    let host = header_value(headers, "x-forwarded-host").or_else(|| header_value(headers, "host"));
    let valid = |h: &&str| h.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]_".contains(c));
    format!("{}://{}", proto.unwrap_or("http"), host.filter(valid).unwrap_or("localhost"))
}

/// The `WWW-Authenticate` value of a 401 from `path` (`/mcp` or `/mcp/all`).
pub fn challenge(headers: &HeaderMap, path: &str, invalid: bool) -> String {
    let error = if invalid { r#"error="invalid_token", "# } else { "" };
    format!(r#"Bearer {error}resource_metadata="{}/.well-known/oauth-protected-resource{path}""#, origin(headers))
}

/// RFC 9728: the resource is the path after the well-known prefix.
async fn resource_metadata(headers: HeaderMap, uri: Uri) -> Json<Value> {
    let origin = origin(&headers);
    let path = uri.path().strip_prefix("/.well-known/oauth-protected-resource").unwrap_or_default();
    Json(json!({
        "resource": format!("{origin}{path}"),
        "authorization_servers": [origin],
        "bearer_methods_supported": ["header"],
        "resource_name": "zilong-router MCP gateway",
    }))
}

/// RFC 8414 (the same document answers the OpenID Connect discovery path).
async fn server_metadata(headers: HeaderMap) -> Json<Value> {
    let issuer = origin(&headers);
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/oauth/authorize"),
        "token_endpoint": format!("{issuer}/oauth/token"),
        "registration_endpoint": format!("{issuer}/oauth/register"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
        "authorization_response_iss_parameter_supported": true,
    }))
}

// ----- registration --------------------------------------------------------------------------

/// https anywhere, plain http only on loopback (RFC 8252), or an app's own scheme
/// (`cursor://...`); never a fragment.
fn redirect_allowed(uri: &str) -> bool {
    let Ok(u) = Url::parse(uri) else { return false };
    if u.fragment().is_some() {
        return false;
    }
    match u.scheme() {
        "https" => u.host_str().is_some(),
        "http" => matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        "javascript" | "data" | "file" | "vbscript" | "about" | "blob" | "ftp" | "ws" | "wss" => false,
        _ => true,
    }
}

/// What the consent page shows of a redirect URI: the host, with the scheme unless https.
fn redirect_host(uri: &str) -> String {
    let Ok(u) = Url::parse(uri) else { return String::new() };
    let host = u.host_str().unwrap_or_default();
    if u.scheme() == "https" {
        host.to_string()
    } else {
        format!("{}://{host}", u.scheme())
    }
}

fn registration_error(error: &str, description: &str) -> Response {
    token_error(StatusCode::BAD_REQUEST, error, description)
}

async fn register(State(state): State<Arc<AppState>>, Json(b): Json<Value>) -> Response {
    let uris: Vec<String> = match b.get("redirect_uris").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        None => vec![],
    };
    if uris.is_empty() {
        return registration_error("invalid_redirect_uri", "redirect_uris is required");
    }
    if let Some(bad) = uris.iter().find(|u| !redirect_allowed(u)) {
        let why = format!("redirect URI not allowed: {bad} (use https, http on localhost, or an app scheme)");
        return registration_error("invalid_redirect_uri", &why);
    }
    let method = b.get("token_endpoint_auth_method").and_then(Value::as_str).unwrap_or("none");
    if !["none", "client_secret_post", "client_secret_basic"].contains(&method) {
        return registration_error("invalid_client_metadata", "unsupported token_endpoint_auth_method");
    }
    let name = b.get("client_name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let secret = (method != "none").then(|| b64url(&random_bytes(32)));
    let client = Client {
        client_id: format!("zrc_{}", b64url(&random_bytes(16))),
        client_name: name.unwrap_or("MCP client").chars().take(80).collect(),
        redirect_uris: uris,
        secret_hash: secret.as_deref().map(token_hash).unwrap_or_default(),
        created_at: now(),
        ..Default::default()
    };
    // Make room: clients never approved go first, then the oldest.
    let mut clients: Vec<Client> = all::<Client>(&state.db, CLIENTS).into_iter().map(|(_, c)| c).collect();
    if clients.len() >= MAX_CLIENTS {
        clients.sort_by_key(|c| (c.connected_at > 0, c.created_at));
        for old in &clients[..clients.len() + 1 - MAX_CLIENTS] {
            let _ = revoke(&state.db, &old.client_id);
        }
    }
    if let Err(e) = store(&state.db, CLIENTS, &client.client_id, &client) {
        return token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e.to_string());
    }
    tracing::info!("mcp oauth: registered client {} ({})", client.client_name, client.client_id);
    let mut out = json!({
        "client_id": client.client_id,
        "client_id_issued_at": client.created_at,
        "client_name": client.client_name,
        "redirect_uris": client.redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": method,
    });
    if let Some(s) = secret {
        out["client_secret"] = json!(s);
        out["client_secret_expires_at"] = json!(0);
    }
    (StatusCode::CREATED, Json(out)).into_response()
}

// ----- authorization ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct AuthorizeQuery {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    code_challenge: String,
    code_challenge_method: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AuthorizeForm {
    /// The form token of the pending request.
    request: String,
    /// `continue` (after the password), `approve` or `deny`.
    action: String,
    password: String,
    /// The API key id to act as, or `new`.
    key: String,
}

/// Sends the browser back to the client with `params`, `state` and `iss` (RFC 9207).
fn redirect(uri: &str, params: &[(&str, &str)], state: Option<&str>, issuer: &str) -> Response {
    let Ok(mut url) = Url::parse(uri) else {
        return error_page(StatusCode::BAD_REQUEST, "Invalid redirect", "The redirect address is invalid.");
    };
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
        if let Some(s) = state {
            q.append_pair("state", s);
        }
        q.append_pair("iss", issuer);
    }
    (StatusCode::SEE_OTHER, [(header::LOCATION, url.to_string())]).into_response()
}

/// The password may approve clients once it is no longer the well-known default.
fn password_usable(state: &AppState) -> bool {
    !crate::dashboard::check_password(state, DEFAULT_PASSWORD)
}

fn unknown_client() -> Response {
    let text = "This application is not registered with zilong-router, or it was disconnected. Add it again from \
                the application.";
    error_page(StatusCode::BAD_REQUEST, "Unknown application", text)
}

async fn authorize_page(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    let Some(client) = find_client(&state.db, &q.client_id) else { return unknown_client() };
    // The redirect URI may be left out when the client registered only one.
    let redirect_uri = match (q.redirect_uri.is_empty(), client.redirect_uris.as_slice()) {
        (true, [only]) => only.clone(),
        _ => q.redirect_uri.clone(),
    };
    if !client.redirect_uris.contains(&redirect_uri) {
        let text = "The redirect address does not match the ones this application registered.";
        return error_page(StatusCode::BAD_REQUEST, "Invalid redirect", text);
    }
    let issuer = origin(&headers);
    let fail = |error: &str, description: &str| {
        let pairs = [("error", error), ("error_description", description)];
        redirect(&redirect_uri, &pairs, q.state.as_deref(), &issuer)
    };
    if q.response_type != "code" {
        return fail("unsupported_response_type", "only response_type=code is supported");
    }
    if q.code_challenge_method != "S256" || q.code_challenge.is_empty() {
        return fail("invalid_request", "PKCE with code_challenge_method=S256 is required");
    }
    let p = Pending {
        client_id: client.client_id.clone(),
        redirect_uri: redirect_uri.clone(),
        state: q.state.clone(),
        challenge: q.code_challenge.clone(),
        signed_in: crate::dashboard::authorized(&state, peer, &headers),
        tries: 0,
        created: Instant::now(),
    };
    let request = b64url(&random_bytes(24));
    let html = consent(&state, &client, &request, &p, None);
    let mut pending = PENDING.lock();
    pending.retain(|_, p| p.created.elapsed() < REQUEST_TTL);
    if pending.len() >= MAX_PENDING {
        if let Some(oldest) = pending.iter().min_by_key(|(_, p)| p.created).map(|(k, _)| k.clone()) {
            pending.remove(&oldest);
        }
    }
    pending.insert(request, p);
    page(StatusCode::OK, "Connect an application", &html)
}

async fn authorize_submit(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<AuthorizeForm>,
) -> Response {
    // A cross-site form post carries another Origin (the form token already ties the
    // answer to the page; this is belt and braces).
    if let Some(o) = header_value(&headers, "origin").filter(|o| *o != "null") {
        let host = header_value(&headers, "x-forwarded-host").or_else(|| header_value(&headers, "host"));
        if host.is_some_and(|h| o.split("://").nth(1) != Some(h)) {
            return error_page(StatusCode::FORBIDDEN, "Rejected", "Cross-origin request rejected.");
        }
    }
    let taken = PENDING.lock().remove(&f.request);
    let Some(mut p) = taken.filter(|p| p.created.elapsed() < REQUEST_TTL) else {
        let text = "This sign-in request expired or was already answered. Start again from the application.";
        return error_page(StatusCode::BAD_REQUEST, "Request expired", text);
    };
    let Some(mut client) = find_client(&state.db, &p.client_id) else { return unknown_client() };
    let issuer = origin(&headers);
    if f.action == "deny" {
        let denied = [("error", "access_denied"), ("error_description", "the user denied access")];
        return redirect(&p.redirect_uri, &denied, p.state.as_deref(), &issuer);
    }
    if !p.signed_in {
        let session = crate::dashboard::authorized(&state, peer, &headers);
        let typed = !f.password.is_empty();
        let password_ok = typed && password_usable(&state) && crate::dashboard::check_password(&state, &f.password);
        if !session && !password_ok {
            if typed {
                // Same brute-force damper as the dashboard login.
                tokio::time::sleep(Duration::from_millis(400)).await;
                p.tries += 1;
                if p.tries >= MAX_TRIES {
                    return error_page(StatusCode::UNAUTHORIZED, "Too many attempts", "Start again from the app.");
                }
            }
            let error = if typed { "Wrong password." } else { "Enter the dashboard password." };
            let html = consent(&state, &client, &f.request, &p, Some(error));
            PENDING.lock().insert(f.request, p);
            return page(StatusCode::UNAUTHORIZED, "Connect an application", &html);
        }
        p.signed_in = true;
        if f.action != "approve" {
            // Signed in: now pick the key.
            let html = consent(&state, &client, &f.request, &p, None);
            PENDING.lock().insert(f.request, p);
            return page(StatusCode::OK, "Connect an application", &html);
        }
    }
    let key_id = if f.key == "new" {
        match crate::auth::create_key(&state.cfg, &state.db, Some(client.client_name.clone())) {
            Ok(k) => k.id,
            Err(e) => return error_page(StatusCode::INTERNAL_SERVER_ERROR, "Error", &format!("Creating the key: {e}")),
        }
    } else if let Some(k) = active_key(&state.db, &f.key) {
        k.id
    } else {
        let html = consent(&state, &client, &f.request, &p, Some("Choose an API key."));
        PENDING.lock().insert(f.request, p);
        return page(StatusCode::BAD_REQUEST, "Connect an application", &html);
    };
    client.key_id = key_id.clone();
    client.connected_at = now();
    if let Err(e) = store(&state.db, CLIENTS, &client.client_id, &client) {
        return error_page(StatusCode::INTERNAL_SERVER_ERROR, "Error", &e.to_string());
    }
    let code = b64url(&random_bytes(32));
    {
        let mut codes = CODES.lock();
        codes.retain(|_, c| c.created.elapsed() < CODE_TTL);
        let c = Code {
            client_id: p.client_id,
            redirect_uri: p.redirect_uri.clone(),
            challenge: p.challenge,
            key_id,
            created: Instant::now(),
        };
        codes.insert(code.clone(), c);
    }
    tracing::info!("mcp oauth: {} approved ({})", client.client_name, client.client_id);
    redirect(&p.redirect_uri, &[("code", code.as_str())], p.state.as_deref(), &issuer)
}

// ----- tokens --------------------------------------------------------------------------------------

fn token_error(status: StatusCode, error: &str, description: &str) -> Response {
    let body = json!({"error": error, "error_description": description});
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn invalid_grant(description: &str) -> Response {
    token_error(StatusCode::BAD_REQUEST, "invalid_grant", description)
}

fn verify_pkce(verifier: &str, challenge: &str) -> bool {
    (43..=128).contains(&verifier.len()) && b64url(&Sha256::digest(verifier.as_bytes())) == challenge
}

/// The client id and secret, from HTTP Basic auth or the form.
fn client_credentials(headers: &HeaderMap, form: &HashMap<String, String>) -> (String, String) {
    let basic = header_value(headers, "authorization").and_then(|v| v.strip_prefix("Basic "));
    if let Some(b) = basic {
        let decoded = String::from_utf8(b64_decode(b.trim())).unwrap_or_default();
        if let Some((id, secret)) = decoded.split_once(':') {
            return (id.to_string(), secret.to_string());
        }
    }
    let get = |k: &str| form.get(k).cloned().unwrap_or_default();
    (get("client_id"), get("client_secret"))
}

async fn token_endpoint(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(f): Form<HashMap<String, String>>,
) -> Response {
    let get = |k: &str| f.get(k).cloned().unwrap_or_default();
    let (client_id, secret) = client_credentials(&headers, &f);
    let Some(client) = find_client(&state.db, &client_id) else {
        return token_error(StatusCode::UNAUTHORIZED, "invalid_client", "unknown client_id");
    };
    if !client.secret_hash.is_empty() && token_hash(&secret) != client.secret_hash {
        return token_error(StatusCode::UNAUTHORIZED, "invalid_client", "wrong client_secret");
    }
    let key_id = match get("grant_type").as_str() {
        "authorization_code" => {
            let code = CODES.lock().remove(&get("code"));
            let Some(code) = code.filter(|c| c.created.elapsed() < CODE_TTL) else {
                return invalid_grant("unknown, used or expired authorization code");
            };
            let redirect_uri = get("redirect_uri");
            if code.client_id != client.client_id || (!redirect_uri.is_empty() && redirect_uri != code.redirect_uri) {
                return invalid_grant("the code was issued to another client or redirect_uri");
            }
            if !verify_pkce(&get("code_verifier"), &code.challenge) {
                return invalid_grant("code_verifier does not match the code_challenge");
            }
            code.key_id
        }
        "refresh_token" => {
            let hash = token_hash(&get("refresh_token"));
            let old: Option<Token> = load(&state.db, TOKENS, &hash);
            let Some(old) = old.filter(|t| t.refresh && t.expires_at > now() && t.client_id == client.client_id) else {
                return invalid_grant("unknown, revoked or expired refresh token");
            };
            // Rotation: each refresh token works once.
            let _ = state.db.kv_set(TOKENS, &hash, None);
            old.key_id
        }
        _ => return token_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "unsupported grant_type"),
    };
    if active_key(&state.db, &key_id).is_none() {
        return invalid_grant("the API key this application acts as was disabled or deleted");
    }
    match issue(&state.db, &client.client_id, &key_id) {
        Ok(body) => ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response(),
        Err(e) => token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e.to_string()),
    }
}

// ----- pages -----------------------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

const SPEAR: &str = r##"<svg viewBox="0 0 64 64" width="30" height="30" aria-hidden="true"><g transform="rotate(45 32 32)"><rect x="31" y="24" width="2" height="33" rx="1" fill="#fff"/><rect x="30.2" y="55.5" width="3.6" height="3.5" rx="1" fill="#fadccf"/><path d="M30 24.5h4c3 2.5 5 5.5 5.5 9.5-2.5-2-4.5-3-6.5-3.5h-2c-2 .5-4 1.5-6.5 3.5.5-4 2.5-7 5.5-9.5z" fill="#fadccf"/><rect x="28.5" y="21.5" width="7" height="3" rx="1.2" fill="#fff"/><path d="M32 5c4 6 4.6 11.5 2.2 16.5h-4.4C27.4 16.5 28 11 32 5z" fill="#fff"/></g></svg>"##;

const CSS: &str = "
:root{--primary:#E56A4A;--primary-hover:#cc5236;--bg:#FDFAF6;--surface:#fff;--border:#e5e7eb;--text:#0a0a0a;
--muted:#6B7280;--danger:#cf222e;--note:#fdf1ed}
@media (prefers-color-scheme:dark){:root{--bg:#1a1a1a;--surface:#262626;--border:#333;--text:#ededed;--muted:#9ca3af;
--danger:#ef4444;--note:#3a2a25}}
*{box-sizing:border-box}
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;padding:24px;background:var(--bg);
color:var(--text);font:14px/1.5 Inter,-apple-system,BlinkMacSystemFont,system-ui,sans-serif}
.card{width:100%;max-width:420px;background:var(--surface);border:1px solid var(--border);border-radius:14px;
padding:28px;box-shadow:0 12px 28px -4px rgba(60,50,45,.08)}
.mark{width:48px;height:48px;border-radius:12px;background:var(--primary);display:flex;align-items:center;
justify-content:center;margin-bottom:16px;box-shadow:0 2px 12px -2px rgba(229,106,74,.35)}
h1{font-size:19px;margin:0 0 8px;font-weight:600}
p{margin:0 0 12px}.muted{color:var(--muted)}.hint{color:var(--muted);font-size:12px;margin-top:6px}
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12.5px;word-break:break-all}
.error{color:var(--danger);font-weight:500}
.note{background:var(--note);border-radius:10px;padding:10px 12px;font-size:13px}
label{display:block;font-weight:500;margin:16px 0 6px}
input,select{width:100%;padding:9px 11px;border:1px solid var(--border);border-radius:10px;background:var(--bg);
color:var(--text);font:inherit}
input:focus,select:focus{outline:2px solid rgba(229,106,74,.35);border-color:var(--primary)}
.buttons{display:flex;flex-direction:row-reverse;gap:8px;margin-top:20px}
button{flex:1;padding:10px;border-radius:10px;border:1px solid var(--border);background:transparent;color:var(--text);
font:inherit;font-weight:500;cursor:pointer}
button.primary{background:var(--primary);border-color:var(--primary);color:#fff}
button.primary:hover{background:var(--primary-hover)}
";

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>zilong-router: {}</title>
<style>{CSS}</style></head><body><main class="card"><div class="mark">{SPEAR}</div>{body}</main></body></html>"#,
        esc(title)
    );
    let headers = [
        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (header::CACHE_CONTROL, "no-store"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'"),
        (header::REFERRER_POLICY, "no-referrer"),
    ];
    (status, headers, html).into_response()
}

fn error_page(status: StatusCode, title: &str, text: &str) -> Response {
    page(status, title, &format!(r#"<h1>{}</h1><p class="muted">{}</p>"#, esc(title), esc(text)))
}

/// The consent page: first the password (unless signed in), then the key to act as.
fn consent(state: &AppState, client: &Client, request: &str, p: &Pending, error: Option<&str>) -> String {
    let name = esc(&client.client_name);
    let mut html = format!(
        r#"<h1>Connect {name}</h1><p class="muted"><b>{name}</b> wants to use the MCP servers behind this zilong-router.
If you approve, you are sent back to <code>{}</code>.</p>"#,
        esc(&redirect_host(&p.redirect_uri))
    );
    if let Some(e) = error {
        html += &format!(r#"<p class="error">{}</p>"#, esc(e));
    }
    html += &format!(
        r#"<form method="post" action="/oauth/authorize">
<input type="hidden" name="request" value="{}">"#,
        esc(request)
    );
    let (action, label) = if p.signed_in {
        let keys: Vec<ApiKey> = state.db.api_keys().unwrap_or_default().into_iter().filter(|k| k.is_active).collect();
        let mut options = format!(r#"<option value="new">Create a new key for {name}</option>"#);
        for k in &keys {
            let label = k.name.clone().unwrap_or_else(|| crate::usage::mask_key(&k.key));
            let restricted = if k.access_restricted { " (restricted)" } else { "" };
            let selected = if k.id == client.key_id { " selected" } else { "" };
            options += &format!(r#"<option value="{}"{selected}>{}{restricted}</option>"#, esc(&k.id), esc(&label));
        }
        html += &format!(
            r#"<label for="key">Act as API key</label><select id="key" name="key">{options}</select>
<p class="hint">{name} gets this key's access: the MCP servers it may use (and its models on <code>/v1</code>).
Disabling or deleting the key disconnects {name}.</p>"#
        );
        ("approve", "Approve")
    } else {
        if password_usable(state) {
            html += r#"<label for="password">Dashboard password</label>
<input id="password" type="password" name="password" autocomplete="current-password" autofocus>
<p class="hint">Logged in to the dashboard in this browser? Leave it empty and continue.</p>"#;
        } else {
            html += r#"<p class="note">No dashboard password is set, so this page cannot tell that you own the router.
Set one on the dashboard's Profile page, or log in to the dashboard in this browser, then continue.</p>"#;
        }
        ("continue", "Continue")
    };
    html += &format!(
        r#"<div class="buttons"><button class="primary" name="action" value="{action}">{label}</button>
<button name="action" value="deny">Deny</button></div></form>"#
    );
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::util::pkce;

    #[test]
    fn pkce_s256_verifies() {
        let (verifier, challenge) = pkce(32);
        assert!(verify_pkce(&verifier, &challenge));
        assert!(!verify_pkce(&format!("{verifier}x"), &challenge));
        assert!(!verify_pkce("short", &b64url(&Sha256::digest(b"short"))));
        // Computed with sha256sum and base64url.
        let verifier = "a-verifier-that-is-long-enough-for-pkce-0123456789";
        assert!(verify_pkce(verifier, "KHRP9GVarIokoOJJOXkSpx6S8MyZvmBjMOhTer1g-Co"));
    }

    #[test]
    fn redirect_uris_are_checked() {
        for ok in [
            "https://claude.ai/api/mcp/auth_callback",
            "http://localhost:6274/oauth/callback",
            "http://127.0.0.1:33418/cb",
            "http://[::1]:8080/cb",
            "cursor://anysphere.cursor-retrieval/oauth/user-zilong/callback",
        ] {
            assert!(redirect_allowed(ok), "{ok}");
        }
        for bad in [
            "http://evil.example.com/cb",
            "https://claude.ai/cb#frag",
            "javascript:alert(1)",
            "data:text/html,hi",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(!redirect_allowed(bad), "{bad}");
        }
        assert_eq!(redirect_host("https://claude.ai/api/mcp/auth_callback"), "claude.ai");
        assert_eq!(redirect_host("http://localhost:6274/cb"), "http://localhost");
        assert_eq!(redirect_host("cursor://anysphere.cursor-retrieval/x"), "cursor://anysphere.cursor-retrieval");
    }

    #[test]
    fn tokens_are_stored_hashed() {
        let h = token_hash("zra_abc");
        assert_eq!(h.len(), 64);
        assert_eq!(h, token_hash("zra_abc"));
        assert_ne!(h, token_hash("zra_abd"));
        assert!(!h.contains("abc"));
    }

    #[test]
    fn origin_follows_proxy_headers() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "pi:20128".parse().unwrap());
        assert_eq!(origin(&h), "http://pi:20128");
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        h.insert("x-forwarded-host", "pi.tail1234.ts.net, other".parse().unwrap());
        assert_eq!(origin(&h), "https://pi.tail1234.ts.net");
        h.insert("x-forwarded-host", "evil\"host".parse().unwrap());
        assert_eq!(origin(&h), "https://localhost");
        let c = challenge(&h, "/mcp", false);
        assert_eq!(c, r#"Bearer resource_metadata="https://localhost/.well-known/oauth-protected-resource/mcp""#);
    }

    #[test]
    fn pages_escape() {
        assert_eq!(esc(r#"<a href="x">'&"#), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;");
    }
}
