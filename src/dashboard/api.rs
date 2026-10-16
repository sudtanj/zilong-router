//! JSON admin API behind the dashboard. Every handler is a thin wrapper over `Db`
//! and the provider registry; the CLI uses the same helpers.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use time::OffsetDateTime;

use crate::db::{now_iso, Db, ProviderConnection};
use crate::providers::Registry;
use crate::state::AppState;

type S = State<Arc<AppState>>;

pub struct AdminError(StatusCode, String);

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        err(self.0, &self.1)
    }
}

impl From<anyhow::Error> for AdminError {
    fn from(e: anyhow::Error) -> Self {
        AdminError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

fn bad(msg: impl Into<String>) -> AdminError {
    AdminError(StatusCode::BAD_REQUEST, msg.into())
}

fn not_found() -> AdminError {
    AdminError(StatusCode::NOT_FOUND, "not found".into())
}

pub fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"error": msg}))).into_response()
}

type R = Result<Json<Value>, AdminError>;

// ----- session ------------------------------------------------------------------

pub async fn session(State(state): S, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap) -> Json<Value> {
    let settings = state.db.settings();
    Json(json!({
        "authenticated": super::authorized(&state, peer, &headers),
        "requireLogin": settings.get("requireLogin").and_then(Value::as_bool).unwrap_or(false),
        "hasPassword": settings.get("passwordHash").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false),
        "local": crate::auth::is_local(peer.ip(), &headers),
        "version": env!("CARGO_PKG_VERSION"),
        "dataDir": state.cfg.resolved_data_dir.display().to_string(),
        "dbPath": state.cfg.db_path().display().to_string(),
        "host": state.cfg.host, "port": state.cfg.port,
        "upstream": state.registry.meta,
    }))
}

#[derive(Deserialize)]
pub struct LoginBody {
    password: String,
}

pub async fn login(State(state): S, Json(b): Json<LoginBody>) -> Response {
    if !super::check_password(&state, &b.password) {
        // Cheap brute-force damper; the box has one user.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        return err(StatusCode::UNAUTHORIZED, "wrong password");
    }
    let token = super::issue_token(&state);
    ([(header::SET_COOKIE, super::set_cookie(&token))], Json(json!({"ok": true}))).into_response()
}

pub async fn logout() -> Response {
    ([(header::SET_COOKIE, super::clear_cookie())], Json(json!({"ok": true}))).into_response()
}

#[derive(Deserialize)]
pub struct PasswordBody {
    current: String,
    new: String,
}

pub async fn change_password(State(state): S, Json(b): Json<PasswordBody>) -> R {
    if !super::check_password(&state, &b.current) {
        return Err(AdminError(StatusCode::UNAUTHORIZED, "current password is wrong".into()));
    }
    if b.new.len() < 4 {
        return Err(bad("new password must be at least 4 characters"));
    }
    let hash = super::password_hash(&state, &b.new);
    state.db.save_settings(&json!({"passwordHash": hash}))?;
    Ok(Json(json!({"ok": true})))
}

// ----- connections (helpers) ---------------------------------------------------

fn locks_of(c: &ProviderConnection, now: OffsetDateTime) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(o) = c.data.as_object() {
        for (k, v) in o {
            if let Some(model) = k.strip_prefix("modelLock_") {
                let until = v.as_str().unwrap_or("");
                let active = OffsetDateTime::parse(until, &time::format_description::well_known::Rfc3339)
                    .map(|t| t > now)
                    .unwrap_or(false);
                if active {
                    let shown = if model == "__all" { "*" } else { model };
                    out.push(json!({"model": shown, "until": until}));
                }
            }
        }
    }
    out
}

fn connection_json(c: &ProviderConnection, registry: &Registry, now: OffsetDateTime) -> Value {
    let locks = locks_of(c, now);
    json!({
        "id": c.id,
        "provider": c.provider,
        "providerName": registry.get(&c.provider).map(|p| p.name().to_string()).unwrap_or_else(|| c.provider.clone()),
        "name": c.name,
        "email": c.email,
        "priority": c.priority,
        "isActive": c.is_active,
        "keyMasked": c.secret().map(crate::usage::mask_key),
        "baseUrl": c.base_url_override(),
        "prefix": c.data.pointer("/providerSpecificData/prefix").and_then(Value::as_str),
        "testStatus": c.data.get("testStatus"),
        "lastError": c.data.get("lastError"),
        "errorCode": c.data.get("errorCode"),
        "backoffLevel": c.data.get("backoffLevel"),
        "lastUsedAt": c.data.get("lastUsedAt"),
        "locked": !locks.is_empty(),
        "locks": locks,
        "createdAt": c.created_at,
    })
}

// ----- providers -----------------------------------------------------------------

pub async fn providers(State(state): S) -> R {
    let conns = state.db.all_connections()?;
    let mut list: Vec<Value> = state
        .registry
        .all()
        .iter()
        .map(|p| {
            let d = p.display.clone().unwrap_or_default();
            let format = p
                .transport
                .as_ref()
                .map(|t| t.format.as_str())
                .or_else(|| p.transports.as_ref().and_then(|ts| ts.first()).map(|t| t.format.as_str()));
            json!({
                "id": p.id,
                "alias": p.alias,
                "aliases": p.aliases,
                "name": p.name(),
                "category": p.category,
                "format": format,
                "custom": p.id.ends_with("-compatible"),
                "noAuth": p.no_auth,
                "passthroughModels": p.passthrough_models,
                "priority": p.priority,
                "color": d.color,
                "website": d.website,
                "apiKeyUrl": d.api_key_url,
                "accounts": conns.iter().filter(|c| c.provider == p.id).count(),
                "models": p.models.iter().map(|m| json!({"id": m.id, "name": m.name})).collect::<Vec<_>>(),
            })
        })
        .collect();
    list.sort_by(|a, b| {
        b["accounts"].as_u64().cmp(&a["accounts"].as_u64()).then(a["priority"].as_i64().cmp(&b["priority"].as_i64()))
    });
    Ok(Json(Value::Array(list)))
}

// ----- connections ---------------------------------------------------------------

pub async fn connections(State(state): S) -> R {
    let now = OffsetDateTime::now_utc();
    let list: Vec<Value> =
        state.db.all_connections()?.iter().map(|c| connection_json(c, &state.registry, now)).collect();
    Ok(Json(Value::Array(list)))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NewConnection {
    pub provider: String,
    pub api_key: String,
    pub name: Option<String>,
    pub priority: Option<i64>,
    pub base_url: Option<String>,
    pub prefix: Option<String>,
}

/// Build a connection row from user input; shared by the dashboard and the CLI.
pub fn new_connection(registry: &Registry, n: &NewConnection) -> anyhow::Result<ProviderConnection> {
    let id = registry
        .resolve_alias(n.provider.trim())
        .ok_or_else(|| anyhow::anyhow!("unknown provider {}", n.provider))?
        .to_string();
    let is_custom = id.ends_with("-compatible");
    let base_url = n.base_url.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if is_custom && base_url.is_none() {
        anyhow::bail!("base URL is required for {id}");
    }
    if n.api_key.trim().is_empty() {
        anyhow::bail!("API key is required");
    }
    let mut psd = serde_json::Map::new();
    if let Some(u) = base_url {
        psd.insert("baseUrl".into(), json!(u.trim_end_matches('/')));
    }
    if let Some(p) = n.prefix.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        psd.insert("prefix".into(), json!(p));
    }
    Ok(ProviderConnection {
        id: uuid::Uuid::new_v4().to_string(),
        provider: id,
        auth_type: "apikey".into(),
        name: n.name.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
        email: None,
        priority: n.priority,
        is_active: true,
        data: json!({"apiKey": n.api_key.trim(), "providerSpecificData": psd}),
        created_at: now_iso(),
        updated_at: now_iso(),
    })
}

pub async fn add_connection(State(state): S, Json(b): Json<NewConnection>) -> R {
    let conn = new_connection(&state.registry, &b).map_err(|e| bad(e.to_string()))?;
    state.db.insert_connection(&conn)?;
    Ok(Json(connection_json(&conn, &state.registry, OffsetDateTime::now_utc())))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionPatch {
    is_active: Option<bool>,
    name: Option<String>,
    /// `null` clears the priority; absent leaves it alone.
    #[serde(default, deserialize_with = "deserialize_some")]
    priority: Option<Option<i64>>,
    api_key: Option<String>,
    base_url: Option<String>,
    prefix: Option<String>,
}

/// Distinguish a missing field (`None`) from an explicit `null` (`Some(None)`).
fn deserialize_some<'de, T, D>(d: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(d).map(Some)
}

pub async fn patch_connection(State(state): S, Path(id): Path<String>, Json(b): Json<ConnectionPatch>) -> R {
    let cur = state.db.get_connection(&id)?.ok_or_else(not_found)?;
    if let Some(a) = b.is_active {
        state.db.set_connection_active(&id, a)?;
    }
    if b.name.is_some() || b.priority.is_some() {
        state.db.update_connection(&id, b.name.as_deref(), b.priority)?;
    }
    let mut patch = serde_json::Map::new();
    if let Some(k) = b.api_key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        patch.insert("apiKey".into(), json!(k));
    }
    if b.base_url.is_some() || b.prefix.is_some() {
        let mut psd = cur.data.get("providerSpecificData").cloned().unwrap_or_else(|| json!({}));
        if !psd.is_object() {
            psd = json!({});
        }
        if let Some(u) = b.base_url.as_deref() {
            if u.trim().is_empty() {
                psd.as_object_mut().and_then(|o| o.remove("baseUrl"));
            } else {
                psd["baseUrl"] = json!(u.trim().trim_end_matches('/'));
            }
        }
        if let Some(p) = b.prefix.as_deref() {
            if p.trim().is_empty() {
                psd.as_object_mut().and_then(|o| o.remove("prefix"));
            } else {
                psd["prefix"] = json!(p.trim());
            }
        }
        patch.insert("providerSpecificData".into(), psd);
    }
    if !patch.is_empty() {
        state.db.patch_connection_data(&id, &Value::Object(patch))?;
    }
    let c = state.db.get_connection(&id)?.ok_or_else(not_found)?;
    Ok(Json(connection_json(&c, &state.registry, OffsetDateTime::now_utc())))
}

pub async fn delete_connection(State(state): S, Path(id): Path<String>) -> R {
    if !state.db.delete_connection(&id)? {
        return Err(not_found());
    }
    Ok(Json(json!({"ok": true})))
}

pub async fn unlock_connection(State(state): S, Path(id): Path<String>) -> R {
    let c = state.db.get_connection(&id)?.ok_or_else(not_found)?;
    state.db.patch_connection_data(&id, &Db::unlock_patch(&c))?;
    let c = state.db.get_connection(&id)?.ok_or_else(not_found)?;
    Ok(Json(connection_json(&c, &state.registry, OffsetDateTime::now_utc())))
}

// ----- client API keys -----------------------------------------------------------

pub async fn keys(State(state): S) -> R {
    Ok(Json(serde_json::to_value(state.db.api_keys()?).unwrap_or(Value::Array(vec![]))))
}

#[derive(Deserialize, Default)]
pub struct NewKey {
    name: Option<String>,
}

pub async fn create_key(State(state): S, Json(b): Json<NewKey>) -> R {
    let k = crate::auth::create_key(&state.cfg, &state.db, b.name.filter(|s| !s.trim().is_empty()))?;
    Ok(Json(serde_json::to_value(k).unwrap_or(Value::Null)))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KeyPatch {
    name: Option<String>,
    is_active: Option<bool>,
    access_restricted: Option<bool>,
    access_allow: Option<Vec<String>>,
}

pub async fn patch_key(State(state): S, Path(id): Path<String>, Json(b): Json<KeyPatch>) -> R {
    let cur = state.db.api_keys()?.into_iter().find(|k| k.id == id || k.key == id).ok_or_else(not_found)?;
    let access = if b.access_restricted.is_some() || b.access_allow.is_some() {
        Some((b.access_restricted.unwrap_or(cur.access_restricted), b.access_allow.unwrap_or(cur.access_allow)))
    } else {
        None
    };
    state.db.update_api_key(&id, b.name.as_deref(), b.is_active, access.as_ref().map(|(r, a)| (*r, a.as_slice())))?;
    let k = state.db.api_keys()?.into_iter().find(|k| k.id == id || k.key == id).ok_or_else(not_found)?;
    Ok(Json(serde_json::to_value(k).unwrap_or(Value::Null)))
}

pub async fn delete_key(State(state): S, Path(id): Path<String>) -> R {
    if !state.db.delete_api_key(&id)? {
        return Err(not_found());
    }
    Ok(Json(json!({"ok": true})))
}

// ----- aliases -------------------------------------------------------------------

pub async fn aliases(State(state): S) -> R {
    Ok(Json(Value::Object(state.db.kv_scope("modelAliases")?)))
}

#[derive(Deserialize)]
pub struct AliasBody {
    alias: String,
    target: String,
}

pub async fn set_alias(State(state): S, Json(b): Json<AliasBody>) -> R {
    let alias = b.alias.trim();
    let target = b.target.trim();
    if alias.is_empty() || alias.contains('/') || alias.contains(char::is_whitespace) {
        return Err(bad("alias must be a single word without '/'"));
    }
    if !target.contains('/') {
        return Err(bad("target must be provider/model"));
    }
    state.db.kv_set("modelAliases", alias, Some(&json!(target)))?;
    Ok(Json(json!({alias: target})))
}

pub async fn delete_alias(State(state): S, Path(alias): Path<String>) -> R {
    state.db.kv_set("modelAliases", &alias, None)?;
    Ok(Json(json!({"ok": true})))
}

// ----- auto model ----------------------------------------------------------------

/// What `model: "auto"` would try right now, in order, and whether each candidate
/// has an account that is not cooling down.
pub async fn auto_preview(State(state): S) -> R {
    let settings = state.db.settings();
    let explicit: Vec<String> = settings
        .get("autoModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str().map(str::to_string))
        .collect();
    let aliases = state.db.kv_scope("modelAliases")?;
    let conns = state.db.all_connections()?;
    let now = OffsetDateTime::now_utc();
    let candidates: Vec<Value> = crate::discovery::auto_candidates(&state, &aliases, &conns, &explicit)
        .await
        .into_iter()
        .take(60)
        .map(|c| {
            let p = state.registry.get(&c.provider);
            let supported = p
                .and_then(|p| p.transport_for(crate::providers::Format::Openai, &c.model))
                .map(|t| t.format.supported())
                .unwrap_or(false);
            let accounts: Vec<&ProviderConnection> = conns
                .iter()
                .filter(|a| a.provider == c.provider && a.is_active)
                .filter(|a| c.pinned_connection.as_deref().map_or(true, |id| a.id == id))
                .collect();
            let node = c.pinned_connection.as_ref().and_then(|_| accounts.first().copied());
            let prefix = node
                .and_then(crate::model::connection_prefix)
                .or(p.map(|p| p.alias.as_str()))
                .unwrap_or(&c.provider)
                .to_string();
            let provider_name =
                node.map(|n| n.label()).unwrap_or_else(|| p.map(|p| p.name()).unwrap_or(&c.provider).to_string());
            let no_auth = p.map(|p| p.no_auth).unwrap_or(false);
            let free = accounts.iter().any(|a| crate::router::active_lock_until(a, &c.model, now).is_none());
            json!({
                "model": format!("{prefix}/{}", c.model),
                "provider": c.provider,
                "providerName": provider_name,
                "accounts": accounts.len(),
                "supported": supported,
                "ready": supported && (no_auth || free),
            })
        })
        .collect();
    Ok(Json(json!({
        "source": if explicit.is_empty() { "accounts" } else { "autoModels" },
        "autoModels": explicit,
        "candidates": candidates,
    })))
}

// ----- token saver ---------------------------------------------------------------

/// Savings since start plus the option lists; the toggles themselves live in settings.
pub async fn token_saver(State(_state): S) -> R {
    use crate::token_saver::{prompts, stats, OPT_OUT_HEADER};
    Ok(Json(json!({
        "stats": stats(),
        "cavemanLevels": prompts::CAVEMAN_LEVELS,
        "ponytailLevels": prompts::PONYTAIL_LEVELS,
        "optOutHeader": OPT_OUT_HEADER,
    })))
}

// ----- settings ------------------------------------------------------------------

fn redact(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        let has = o.get("passwordHash").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false);
        o.remove("passwordHash");
        o.remove("password");
        o.insert("hasPassword".into(), json!(has));
    }
    v
}

pub async fn settings(State(state): S) -> R {
    Ok(Json(redact(state.db.settings())))
}

pub async fn patch_settings(State(state): S, Json(mut patch): Json<Value>) -> R {
    let Some(o) = patch.as_object_mut() else { return Err(bad("expected a JSON object")) };
    o.remove("passwordHash");
    o.remove("password");
    o.remove("hasPassword");
    if let Some(s) = o.get("fallbackStrategy").and_then(Value::as_str) {
        if s != "fill-first" && s != "round-robin" {
            return Err(bad("fallbackStrategy must be fill-first or round-robin"));
        }
    }
    use crate::token_saver::prompts::{CAVEMAN_LEVELS, PONYTAIL_LEVELS};
    for (key, levels) in [("cavemanLevel", CAVEMAN_LEVELS), ("ponytailLevel", PONYTAIL_LEVELS)] {
        if let Some(v) = o.get(key) {
            if !v.as_str().map_or(false, |s| levels.contains(&s)) {
                return Err(bad(format!("{key} must be one of {}", levels.join(", "))));
            }
        }
    }
    Ok(Json(redact(state.db.save_settings(&patch)?)))
}

// ----- custom models for compatible endpoints --------------------------------------
//
// Compatible endpoints accept any model id, so the list is only a convenience for
// the dashboard (copyable `prefix/model` chips). Stored in kv scope `customModels`.

pub async fn custom_models(State(state): S, Path(prefix): Path<String>) -> R {
    let m = state.db.kv_scope("customModels")?;
    Ok(Json(m.get(&prefix).cloned().unwrap_or_else(|| json!([]))))
}

#[derive(Deserialize)]
pub struct ModelIdBody {
    model_id: String,
}

pub async fn add_custom_model(State(state): S, Path(prefix): Path<String>, Json(b): Json<ModelIdBody>) -> R {
    let id = b.model_id.trim().to_string();
    if id.is_empty() {
        return Err(bad("model id is required"));
    }
    let mut list: Vec<String> = state
        .db
        .kv_scope("customModels")?
        .get(&prefix)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    if !list.iter().any(|m| m == &id) {
        list.push(id);
    }
    state.db.kv_set("customModels", &prefix, Some(&json!(list)))?;
    Ok(Json(json!(list)))
}

pub async fn delete_custom_model(State(state): S, Path((prefix, model)): Path<(String, String)>) -> R {
    let list: Vec<String> = state
        .db
        .kv_scope("customModels")?
        .get(&prefix)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let list: Vec<String> = list.into_iter().filter(|m| m != &model).collect();
    let value = json!(list);
    state.db.kv_set("customModels", &prefix, if list.is_empty() { None } else { Some(&value) })?;
    Ok(Json(json!(list)))
}

// ----- usage ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UsageQuery {
    period: Option<String>,
    limit: Option<i64>,
}

/// Upstream periods: today | 24h | 7d | 30d | 60d | all.
fn period_since(period: &str) -> (Option<String>, bool) {
    let now = OffsetDateTime::now_utc();
    let fmt = |t: OffsetDateTime| t.format(&time::format_description::well_known::Rfc3339).ok();
    match period {
        "today" => (fmt(now.replace_time(time::Time::MIDNIGHT)), true),
        "24h" => (fmt(now - time::Duration::hours(24)), true),
        "7d" => (fmt(now - time::Duration::days(7)), false),
        "30d" => (fmt(now - time::Duration::days(30)), false),
        "60d" => (fmt(now - time::Duration::days(60)), false),
        _ => (None, false),
    }
}

/// Stats plus the display names the upstream UI shows (account labels, key names).
pub async fn usage(State(state): S, Query(q): Query<UsageQuery>) -> R {
    let (since, hourly) = period_since(q.period.as_deref().unwrap_or("today"));
    let mut stats = state.db.usage_stats(since.as_deref(), hourly)?;
    let conns = state.db.all_connections()?;
    let keys = state.db.api_keys()?;
    let masked: Vec<(String, String)> =
        keys.iter().map(|k| (crate::usage::mask_key(&k.key), k.name.clone().unwrap_or_else(|| k.id.clone()))).collect();
    if let Some(rows) = stats.get_mut("byAccount").and_then(Value::as_array_mut) {
        for r in rows {
            let id = r.get("connectionId").and_then(Value::as_str).unwrap_or("");
            let label = conns.iter().find(|c| c.id == id).map(|c| c.label());
            r["accountName"] = json!(label);
        }
    }
    if let Some(rows) = stats.get_mut("byApiKey").and_then(Value::as_array_mut) {
        for r in rows {
            let k = r.get("apiKey").and_then(Value::as_str).unwrap_or("");
            let name = masked.iter().find(|(m, _)| m == k).map(|(_, n)| n.clone());
            r["keyName"] = json!(name);
        }
    }
    stats["recentRequests"] = Value::Array(state.db.usage_recent(20)?);
    Ok(Json(stats))
}

pub async fn usage_recent(State(state): S, Query(q): Query<UsageQuery>) -> R {
    Ok(Json(Value::Array(state.db.usage_recent(q.limit.unwrap_or(50).clamp(1, 500))?)))
}
