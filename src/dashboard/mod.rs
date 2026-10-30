//! Embedded dashboard: a framework-free single-page UI compiled into the binary plus
//! a small JSON admin API under `/api/admin`. Sessions are an HMAC-signed cookie, so
//! no JWT library or server-side session table is needed.
//!
//! Access rules follow upstream: with `settings.requireLogin = false` (the default)
//! loopback clients are let in without a password; everyone else must log in.

pub mod api;
pub mod import;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use rand::{distributions::Alphanumeric, Rng};
use serde_json::json;
use sha2::Sha256;

use crate::config::Config;
use crate::state::AppState;

const COOKIE: &str = "zr_session";
const SESSION_TTL_SECS: i64 = 7 * 24 * 3600;

const INDEX_HTML: &str = include_str!("../../web/index.html");
const APP_JS: &str = include_str!("../../web/app.js");
const STYLE_CSS: &str = include_str!("../../web/style.css");

/// `JWT_SECRET` if set, else a random secret persisted to `<data_dir>/jwt-secret`.
pub fn session_secret(cfg: &Config) -> String {
    if let Some(s) = cfg.jwt_secret.as_deref().filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    if cfg.turso_only {
        // No disk to persist a random secret on; derive a stable one from the Turso token.
        return sign(cfg.turso_token.as_deref().unwrap_or_default(), "zilong-router-session-secret");
    }
    let p = cfg.resolved_data_dir.join("jwt-secret");
    if let Ok(s) = std::fs::read_to_string(&p) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let fresh: String = rand::thread_rng().sample_iter(&Alphanumeric).take(48).map(char::from).collect();
    if let Err(e) = std::fs::write(&p, &fresh) {
        tracing::warn!("cannot persist session secret to {}: {e} (sessions reset on restart)", p.display());
    }
    fresh
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sign(secret: &str, msg: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(msg.as_bytes());
    hex(&mac.finalize().into_bytes())
}

fn verify(secret: &str, msg: &str, sig_hex: &str) -> bool {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(msg.as_bytes());
    let Ok(sig) = (0..sig_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(sig_hex.get(i..i + 2).unwrap_or("zz"), 16))
        .collect::<Result<Vec<u8>, _>>()
    else {
        return false;
    };
    mac.verify_slice(&sig).is_ok()
}

/// Stored form of the dashboard password (`settings.passwordHash`).
pub fn password_hash(state: &AppState, password: &str) -> String {
    sign(&state.session_secret, &format!("password:{password}"))
}

/// Accept the saved password, or `INITIAL_PASSWORD` while none has been saved.
pub fn check_password(state: &AppState, password: &str) -> bool {
    let settings = state.db.settings();
    match settings.get("passwordHash").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        Some(stored) => verify(&state.session_secret, &format!("password:{password}"), stored),
        None => password == state.cfg.initial_password,
    }
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

pub fn issue_token(state: &AppState) -> String {
    let exp = now() + SESSION_TTL_SECS;
    format!("{exp}.{}", sign(&state.session_secret, &format!("session:{exp}")))
}

fn token_valid(state: &AppState, token: &str) -> bool {
    let Some((exp, sig)) = token.split_once('.') else { return false };
    let Ok(exp_n) = exp.parse::<i64>() else { return false };
    exp_n > now() && verify(&state.session_secret, &format!("session:{exp}"), sig)
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .map(str::trim)
        .find_map(|kv| kv.strip_prefix(COOKIE).and_then(|r| r.strip_prefix('=')))
        .map(str::to_string)
}

pub fn set_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_TTL_SECS}"))
        .expect("cookie")
}

pub fn clear_cookie() -> HeaderValue {
    HeaderValue::from_static("zr_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// Is this request allowed to use the admin API?
pub fn authorized(state: &AppState, peer: SocketAddr, headers: &HeaderMap) -> bool {
    if let Some(t) = cookie_value(headers) {
        if token_valid(state, &t) {
            return true;
        }
    }
    let require_login = state.db.settings().get("requireLogin").and_then(|v| v.as_bool()).unwrap_or(false);
    !require_login && crate::auth::is_local(peer.ip(), headers)
}

/// Rejects unauthenticated admin calls and cross-site writes (CSRF: the Origin header
/// of a state-changing request must match the Host we were reached on).
async fn guard(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();
    if req.method() != axum::http::Method::GET {
        if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
            let origin_host = origin.split("://").nth(1).unwrap_or(origin);
            if !host.is_empty() && origin_host != host {
                return api::err(StatusCode::FORBIDDEN, "cross-origin request rejected");
            }
        }
    }
    if !authorized(&state, peer, headers) {
        return api::err(StatusCode::UNAUTHORIZED, "login required");
    }
    next.run(req).await
}

pub fn routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let admin = Router::new()
        .route("/providers", get(api::providers))
        .route("/connections", get(api::connections).post(api::add_connection))
        .route("/connections/:id", patch(api::patch_connection).delete(api::delete_connection))
        .route("/connections/:id/unlock", post(api::unlock_connection))
        .route("/oauth/:provider/start", post(api::oauth_start))
        .route("/oauth/:provider/exchange", post(api::oauth_exchange))
        .route("/oauth/:provider/poll", post(api::oauth_poll))
        .route("/keys", get(api::keys).post(api::create_key))
        .route("/keys/:id", patch(api::patch_key).delete(api::delete_key))
        .route("/aliases", get(api::aliases).post(api::set_alias))
        .route("/aliases/:alias", delete(api::delete_alias))
        .route("/settings", get(api::settings).patch(api::patch_settings))
        .route("/auto", get(api::auto_preview))
        .route("/test-models", post(api::test_models))
        .route("/database", get(api::database))
        .route("/import", post(api::import_9router))
        .route("/console-logs", get(api::console_logs).delete(api::clear_console_logs))
        .route("/token-saver", get(api::token_saver))
        .route("/password", post(api::change_password))
        .route("/usage", get(api::usage))
        .route("/custom-models/:prefix", get(api::custom_models).post(api::add_custom_model))
        .route("/custom-models/:prefix/:model", delete(api::delete_custom_model))
        .route("/models/:prefix/remove", post(api::remove_models))
        .route("/models/:prefix/add", post(api::add_models))
        .route("/models/:prefix/hidden", get(api::hidden_models).delete(api::restore_hidden_models))
        .route("/refresh-models", post(api::refresh_models))
        .route("/usage/recent", get(api::usage_recent))
        .route("/mcp/servers", get(api::mcp_servers).post(api::mcp_add))
        .route("/mcp/servers/:id", patch(api::mcp_patch).delete(api::mcp_delete))
        .route("/mcp/servers/:id/refresh", post(api::mcp_refresh))
        .route("/mcp/servers/:id/stop", post(api::mcp_stop))
        .route("/mcp/servers/:id/tools", get(api::mcp_tools))
        .route("/mcp/import", post(api::mcp_import))
        .route("/logout", post(api::logout))
        .route_layer(middleware::from_fn_with_state(state, guard))
        // Added after the guard layer, so these two stay public.
        .route("/session", get(api::session))
        .route("/login", post(api::login));

    Router::new()
        .route("/", get(index))
        .route("/dashboard", get(index))
        .route("/dashboard/*rest", get(index))
        .route("/assets/app.js", get(app_js))
        .route("/assets/style.css", get(style_css))
        .route("/api/info", get(info))
        .route("/callback", get(oauth_callback))
        .route("/providers/:file", get(provider_icon))
        .nest("/api/admin", admin)
}

fn static_response(content_type: &'static str, body: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn index() -> Response {
    static_response("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> Response {
    static_response("application/javascript; charset=utf-8", APP_JS)
}

async fn style_css() -> Response {
    static_response("text/css; charset=utf-8", STYLE_CSS)
}

async fn info() -> impl IntoResponse {
    Json(json!({
        "name": "zilong-router",
        "version": env!("CARGO_PKG_VERSION"),
        "openai_base_url": "/v1",
        "anthropic_base_url": "/v1",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_roundtrip() {
        let s = sign("k", "m");
        assert_eq!(s.len(), 64);
        assert!(verify("k", "m", &s));
        assert!(!verify("k", "x", &s));
        assert!(!verify("k", "m", "nothex"));
    }

    #[test]
    fn cookie_parse() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; zr_session=tok.sig; b=2".parse().unwrap());
        assert_eq!(cookie_value(&h).as_deref(), Some("tok.sig"));
    }
}

/// Provider logos at upstream's path (`/providers/<id>.png`), embedded by build.rs
/// from `data/icons/`. Unknown ids 404 so the UI falls back to the text icon.
async fn provider_icon(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    let id = file.strip_suffix(".png").unwrap_or(&file).to_ascii_lowercase();
    match crate::icons::ICONS.iter().find(|(name, _)| *name == id) {
        Some((_, bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::CACHE_CONTROL, "public, max-age=86400")
            .body(Body::from(*bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Where OAuth redirects land when the browser runs on this machine (or through a
/// tunnel): hand the URL to the dashboard tab that started the sign-in.
async fn oauth_callback() -> Response {
    static_response(
        "text/html; charset=utf-8",
        r#"<!doctype html><meta charset="utf-8"><title>zilong-router sign-in</title>
<body style="font:15px system-ui;margin:40px;max-width:640px">
<h3>Sign-in finished</h3><p id="m">Sending the result back to the dashboard…</p>
<script>
try { new BroadcastChannel('zilong-router-oauth').postMessage(location.href); } catch (e) {}
try { localStorage.setItem('zilong-router-oauth', JSON.stringify({url: location.href, at: Date.now()})); } catch (e) {}
document.getElementById('m').textContent = 'You can close this tab. If the dashboard does not continue, paste this address into it: ' + location.href;
</script>"#,
    )
}
