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
use crate::mcp::{self, ServerConfig};
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

// ----- database & console log ------------------------------------------------------

#[derive(Deserialize)]
pub struct DatabaseQuery {
    /// Round trip to Turso now instead of only reporting the last sync.
    #[serde(default)]
    ping: bool,
}

/// Where the data lives and, with Turso, whether the mirror is reachable.
pub async fn database(State(state): S, Query(q): Query<DatabaseQuery>) -> Json<Value> {
    let cfg = &state.cfg;
    let size =
        state.db.rows("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", &[]).ok().and_then(
            |rows| match rows.first().and_then(|r| r.first()) {
                Some(rusqlite::types::Value::Integer(n)) => Some(*n),
                _ => None,
            },
        );
    let mut turso = Value::Null;
    if let Some(host) = crate::turso::host(cfg) {
        let status = state.turso.lock().clone();
        turso = serde_json::to_value(status).unwrap_or_default();
        turso["host"] = json!(host);
        turso["syncSecs"] = json!(cfg.turso_sync_secs.max(5));
        if q.ping {
            turso["ping"] = match crate::turso::ping(cfg).await {
                Ok(ms) => json!({"ok": true, "latencyMs": ms}),
                Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
            };
        }
    }
    Json(json!({
        "mode": if cfg.turso_only { "turso" } else if turso.is_null() { "local" } else { "mirror" },
        "path": (!cfg.turso_only).then(|| cfg.db_path().display().to_string()),
        "sizeBytes": size,
        "usageKeepDays": cfg.usage_keep_days(),
        "turso": turso,
    }))
}

#[derive(Deserialize)]
pub struct ConsoleQuery {
    #[serde(default)]
    after: u64,
}

pub async fn console_logs(Query(q): Query<ConsoleQuery>) -> Json<Value> {
    let (lines, last, reset) = crate::console_log::since(q.after);
    Json(json!({"lines": lines, "last": last, "reset": reset}))
}

pub async fn clear_console_logs() -> Json<Value> {
    crate::console_log::clear();
    Json(json!({"success": true}))
}

// ----- import from 9router -----------------------------------------------------------

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
pub struct ImportQuery {
    #[serde(default, rename = "dryRun")]
    dry_run: bool,
    #[serde(default = "yes")]
    providers: bool,
    #[serde(default = "yes")]
    keys: bool,
    #[serde(default)]
    settings: bool,
}

/// Upload cap: a 9router `data.sqlite` carries its usage history too.
const MAX_IMPORT: usize = 512 << 20;

/// Body: a 9router backup JSON, legacy `db.json` or `data.sqlite`. Streamed to a file
/// in the data dir so a large database never sits in RAM.
pub async fn import_9router(State(state): S, Query(q): Query<ImportQuery>, body: axum::body::Body) -> R {
    use futures_util::StreamExt;
    use std::io::Write;

    use anyhow::Context;

    let path = state.cfg.resolved_data_dir.join(format!("import-{}.tmp", uuid::Uuid::new_v4()));
    let res = async {
        let mut file = std::fs::File::create(&path).context("cannot write the upload to the data dir")?;
        let mut stream = body.into_data_stream();
        let mut total = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("upload interrupted")?;
            total += chunk.len();
            if total > MAX_IMPORT {
                anyhow::bail!("file larger than {} MB", MAX_IMPORT >> 20);
            }
            file.write_all(&chunk)?;
        }
        file.flush()?;
        drop(file);
        if total == 0 {
            anyhow::bail!("empty file");
        }
        let (state, p) = (state.clone(), path.clone());
        let opts =
            super::import::Options { providers: q.providers, keys: q.keys, settings: q.settings, dry_run: q.dry_run };
        tokio::task::spawn_blocking(move || {
            let (payload, format) = super::import::read_payload(&p)?;
            let mut summary = super::import::apply(&state, &payload, &opts)?;
            summary["format"] = json!(format);
            Ok::<_, anyhow::Error>(summary)
        })
        .await?
    }
    .await;
    let _ = std::fs::remove_file(&path);
    res.map(Json).map_err(|e| bad(format!("{e:#}")))
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
        "authType": c.auth_type,
        "keyMasked": c.secret().map(crate::usage::mask_key),
        "expiresAt": c.data.get("expiresAt"),
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
    // OpenCode Free's chat models come from a live list (cached).
    let opencode_free = crate::discovery::opencode_free(&state).await;
    // Models removed on the provider page stay out of its list.
    let hidden = state.db.kv_scope("hiddenModels")?;
    let models_of = |p: &crate::providers::Provider| {
        let mut out: Vec<(&str, &str)> = p.models.iter().map(|m| (m.id.as_str(), m.name.as_str())).collect();
        if p.id == crate::oauth::opencode::ID {
            out.extend(opencode_free.iter().filter(|id| p.model(id).is_none()).map(|id| (id.as_str(), id.as_str())));
        }
        out.into_iter()
            .filter(|(id, _)| !crate::model::is_hidden(&hidden, &[p.id.as_str(), p.alias.as_str()], id))
            .map(|(id, name)| json!({"id": id, "name": name}))
            .collect::<Vec<Value>>()
    };
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
                "oauth": crate::oauth::flow(&p.id).map(|f| if f == crate::oauth::Flow::Redirect { "redirect" } else { "device" }),
                "acceptsApiKey": !p.no_auth && p.accepts_api_key(),
                "passthroughModels": p.passthrough_models,
                "priority": p.priority,
                "color": d.color,
                "website": d.website,
                "apiKeyUrl": d.api_key_url,
                "riskNotice": d.deprecation_notice.as_ref().filter(|_| d.deprecated),
                "accounts": conns.iter().filter(|c| c.provider == p.id).count(),
                "models": models_of(p),
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

/// What `model: "auto"`, `"auto-strong"` and `"auto-images"` would try right now, in
/// order, and whether each candidate has an account that is not cooling down.
pub async fn auto_preview(State(state): S) -> R {
    use crate::model::AutoKind;
    let explicit = crate::discovery::saved_auto_models(&state, AutoKind::Any);
    let strong_explicit = crate::discovery::saved_auto_models(&state, AutoKind::Strong);
    let image_explicit = crate::discovery::saved_auto_models(&state, AutoKind::Images);
    let aliases = state.db.kv_scope("modelAliases")?;
    let conns = state.db.all_connections()?;
    let now = OffsetDateTime::now_utc();
    let auto = crate::discovery::auto_candidates(&state, &aliases, &conns, &explicit).await;
    // Every model of every account, which the scored kinds filter when they have no list.
    let every = if explicit.is_empty() {
        auto.clone()
    } else {
        crate::discovery::auto_candidates(&state, &aliases, &conns, &[]).await
    };
    let ranked = |kind: AutoKind| {
        let mut all = every.clone();
        crate::model::best_first(&mut all, kind.score().expect("scored kind"));
        all
    };
    let strong = if strong_explicit.is_empty() {
        ranked(AutoKind::Strong)
    } else {
        crate::discovery::auto_candidates(&state, &aliases, &conns, &strong_explicit).await
    };
    let images = if image_explicit.is_empty() {
        ranked(AutoKind::Images)
    } else {
        crate::discovery::auto_candidates(&state, &aliases, &conns, &image_explicit).await
    };
    let describe = |c: crate::model::Resolved| {
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
            "strength": crate::model::strength(&c.model),
            "vision": crate::model::vision(&c.model),
        })
    };
    let candidates: Vec<Value> = auto.into_iter().take(60).map(&describe).collect();
    let strong_candidates: Vec<Value> = strong.into_iter().take(60).map(&describe).collect();
    let image_candidates: Vec<Value> = images.into_iter().take(60).map(&describe).collect();
    Ok(Json(json!({
        "source": if explicit.is_empty() { "accounts" } else { "autoModels" },
        "autoModels": explicit,
        "candidates": candidates,
        "strongSource": if strong_explicit.is_empty() { "accounts" } else { "autoStrongModels" },
        "autoStrongModels": strong_explicit,
        "strongCandidates": strong_candidates,
        "imageSource": if image_explicit.is_empty() { "accounts" } else { "autoImageModels" },
        "autoImageModels": image_explicit,
        "imageCandidates": image_candidates,
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

#[derive(Deserialize)]
pub struct TestModelsBody {
    models: Vec<String>,
}

/// "Test all" on a provider page (upstream `providers/[id]/test-models`): ping each
/// `prefix/model`. The first runs alone so an expiring token is refreshed once, the
/// rest a few at a time to stay light on a small board.
pub async fn test_models(State(state): S, Json(b): Json<TestModelsBody>) -> R {
    use futures_util::StreamExt;
    const PARALLEL: usize = 3;
    let mut models: Vec<String> = Vec::new();
    for m in b.models.iter().map(|m| m.trim()).filter(|m| !m.is_empty()) {
        if !models.iter().any(|x| x == m) {
            models.push(m.to_string());
        }
    }
    if models.is_empty() {
        return Err(bad("no models to test"));
    }
    if models.len() > 300 {
        return Err(bad("too many models (300 at most)"));
    }
    let entry = |m: String, r: Value| {
        let mut r = r;
        r["model"] = json!(m);
        r
    };
    let first = models.remove(0);
    let mut results = vec![entry(first.clone(), crate::handlers::chat::ping(&state, &first).await)];
    let rest: Vec<Value> = futures_util::stream::iter(models)
        .map(|m| {
            let state = state.clone();
            async move {
                let r = crate::handlers::chat::ping(&state, &m).await;
                entry(m, r)
            }
        })
        .buffered(PARALLEL)
        .collect()
        .await;
    results.extend(rest);
    let ok = results.iter().filter(|r| r["ok"] == json!(true)).count();
    Ok(Json(json!({"total": results.len(), "ok": ok, "results": results})))
}

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

// ----- clean up / refresh a provider's model list --------------------------------
//
// Catalogue models can't be deleted, so removing one records it in kv scope
// `hiddenModels` (keyed by prefix like `customModels`); added models that are hidden
// come back, the rest become custom models.

fn kv_list(db: &Db, scope: &str, key: &str) -> anyhow::Result<Vec<String>> {
    Ok(db.kv_scope(scope)?.get(key).and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default())
}

fn kv_save(db: &Db, scope: &str, key: &str, list: &[String]) -> anyhow::Result<()> {
    let value = json!(list);
    db.kv_set(scope, key, if list.is_empty() { None } else { Some(&value) })
}

#[derive(Deserialize)]
pub struct ModelsBody {
    models: Vec<String>,
}

fn model_ids(b: ModelsBody) -> Result<Vec<String>, AdminError> {
    let mut out: Vec<String> = Vec::new();
    for m in b.models.iter().map(|m| m.trim()).filter(|m| !m.is_empty()) {
        if !out.iter().any(|x| x == m) {
            out.push(m.to_string());
        }
    }
    if out.is_empty() {
        return Err(bad("no models given"));
    }
    Ok(out)
}

/// `{custom, hidden}` lists of a prefix after a change.
fn model_lists(db: &Db, prefix: &str) -> R {
    Ok(Json(json!({"custom": kv_list(db, "customModels", prefix)?, "hidden": kv_list(db, "hiddenModels", prefix)?})))
}

/// "Clean up": drop these models from the prefix's list (custom ones are deleted,
/// catalogue and live ones hidden).
pub async fn remove_models(State(state): S, Path(prefix): Path<String>, Json(b): Json<ModelsBody>) -> R {
    let models = model_ids(b)?;
    let mut custom = kv_list(&state.db, "customModels", &prefix)?;
    let mut hidden = kv_list(&state.db, "hiddenModels", &prefix)?;
    for m in models {
        if let Some(i) = custom.iter().position(|c| c == &m) {
            custom.remove(i);
        } else if !hidden.contains(&m) {
            hidden.push(m);
        }
    }
    kv_save(&state.db, "customModels", &prefix, &custom)?;
    kv_save(&state.db, "hiddenModels", &prefix, &hidden)?;
    model_lists(&state.db, &prefix)
}

/// "Refresh models": put these (working) models on the prefix's list.
pub async fn add_models(State(state): S, Path(prefix): Path<String>, Json(b): Json<ModelsBody>) -> R {
    let models = model_ids(b)?;
    let provider = state.registry.resolve_alias(&prefix).and_then(|id| state.registry.get(id));
    // Listed anyway (catalogue or OpenCode Free's live list): only unhide those.
    let listed: Vec<String> = match provider {
        Some(p) if p.id == crate::oauth::opencode::ID => crate::discovery::opencode_free(&state).await,
        _ => Vec::new(),
    };
    let mut custom = kv_list(&state.db, "customModels", &prefix)?;
    let mut hidden = kv_list(&state.db, "hiddenModels", &prefix)?;
    for m in models {
        hidden.retain(|h| h != &m);
        let known = provider.is_some_and(|p| p.model(&m).is_some()) || listed.contains(&m);
        if !known && !custom.contains(&m) {
            custom.push(m);
        }
    }
    kv_save(&state.db, "customModels", &prefix, &custom)?;
    kv_save(&state.db, "hiddenModels", &prefix, &hidden)?;
    model_lists(&state.db, &prefix)
}

pub async fn hidden_models(State(state): S, Path(prefix): Path<String>) -> R {
    Ok(Json(json!(kv_list(&state.db, "hiddenModels", &prefix)?)))
}

/// Restore every model removed from the prefix's list.
pub async fn restore_hidden_models(State(state): S, Path(prefix): Path<String>) -> R {
    state.db.kv_set("hiddenModels", &prefix, None)?;
    Ok(Json(json!([])))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshBody {
    provider: String,
    /// Custom endpoint account (its page lists only its own models).
    node: Option<String>,
}

/// The provider's live model list, fetched now through its best active account.
pub async fn refresh_models(State(state): S, Json(b): Json<RefreshBody>) -> R {
    let conns = state.db.all_connections()?;
    let conn = match b.node.as_deref().filter(|n| !n.is_empty()) {
        Some(node) => Some(conns.iter().find(|c| c.id == node).ok_or_else(not_found)?),
        None => {
            conns.iter().filter(|c| c.is_active && c.provider == b.provider).min_by_key(|c| c.priority.unwrap_or(999))
        }
    };
    let models = crate::discovery::refresh(&state, &b.provider, conn)
        .await
        .map_err(|e| AdminError(StatusCode::BAD_GATEWAY, format!("cannot list the models: {e}")))?;
    Ok(Json(json!({"models": models})))
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

// ----- OAuth sign-in -----------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OauthStart {
    redirect_uri: Option<String>,
}

/// Begin a sign-in; see `crate::oauth::start` for the reply shapes.
pub async fn oauth_start(State(state): S, Path(provider): Path<String>, body: Option<Json<OauthStart>>) -> R {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    crate::oauth::start(&state, &provider, b.redirect_uri.as_deref()).await.map(Json).map_err(|e| bad(e.to_string()))
}

#[derive(Deserialize)]
pub struct OauthStep {
    session: String,
    /// Redirect flows: the callback URL (or code) the user pasted.
    #[serde(default)]
    callback: String,
}

pub async fn oauth_exchange(State(state): S, Path(provider): Path<String>, Json(b): Json<OauthStep>) -> R {
    crate::oauth::exchange(&state, &provider, &b.session, &b.callback).await.map(Json).map_err(|e| bad(e.to_string()))
}

pub async fn oauth_poll(State(state): S, Path(provider): Path<String>, Json(b): Json<OauthStep>) -> R {
    crate::oauth::poll(&state, &provider, &b.session).await.map(Json).map_err(|e| bad(e.to_string()))
}

// ----- MCP gateway ------------------------------------------------------------------

fn mcp_server_json(state: &AppState, s: &ServerConfig) -> Value {
    let mut v = serde_json::to_value(s.masked()).unwrap_or(Value::Null);
    let cached = mcp::cached_tools(&state.db, &s.id);
    v["status"] = json!(state.mcp.status(&s.id));
    v["toolCount"] = json!(cached.as_ref().map(|c| c.tools.len()));
    v["toolsFetchedAt"] = json!(cached.map(|c| c.fetched_at));
    v["stats"] = json!(state.mcp.stats(&s.id));
    v["auth"] = mcp::auth::summary(&state.db, &s.id);
    v
}

fn mcp_tool_rows(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": mcp::tool_name(t),
                "description": mcp::tool_description(t),
                "parameters": mcp::params_summary(t.get("inputSchema").unwrap_or(&Value::Null)),
            })
        })
        .collect()
}

pub async fn mcp_servers(State(state): S) -> R {
    let list: Vec<Value> = mcp::servers(&state.db).iter().map(|s| mcp_server_json(&state, s)).collect();
    Ok(Json(json!({"servers": list, "idleSecs": state.cfg.mcp_idle_secs})))
}

pub async fn mcp_add(State(state): S, Json(mut b): Json<ServerConfig>) -> R {
    b.validate().map_err(bad)?;
    if mcp::server(&state.db, &b.id).is_some() {
        return Err(bad(format!("a server with id `{}` already exists", b.id)));
    }
    mcp::save_server(&state.db, &b)?;
    Ok(Json(mcp_server_json(&state, &b)))
}

/// Partial update: fields left out keep their value, masked secrets stay as stored.
pub async fn mcp_patch(State(state): S, Path(id): Path<String>, Json(patch): Json<Value>) -> R {
    let old = mcp::server(&state.db, &id).ok_or_else(not_found)?;
    let mut merged = serde_json::to_value(&old).map_err(|e| bad(e.to_string()))?;
    if let (Some(m), Some(p)) = (merged.as_object_mut(), patch.as_object()) {
        for (k, v) in p.iter().filter(|(k, _)| *k != "id") {
            m.insert(k.clone(), v.clone());
        }
    }
    let mut cfg: ServerConfig = serde_json::from_value(merged).map_err(|e| bad(e.to_string()))?;
    cfg.id = old.id.clone();
    cfg.unmask(&old);
    cfg.validate().map_err(bad)?;
    mcp::save_server(&state.db, &cfg)?;
    if !cfg.enabled || !cfg.same_target(&old) {
        state.mcp.stop(&id).await;
    }
    Ok(Json(mcp_server_json(&state, &cfg)))
}

pub async fn mcp_delete(State(state): S, Path(id): Path<String>) -> R {
    mcp::server(&state.db, &id).ok_or_else(not_found)?;
    mcp::delete_server(&state.db, &id)?;
    state.mcp.forget(&id).await;
    Ok(Json(json!({"ok": true})))
}

/// Restarts the server and fetches its tool list again.
pub async fn mcp_refresh(State(state): S, Path(id): Path<String>) -> R {
    let cfg = mcp::server(&state.db, &id).ok_or_else(not_found)?;
    state.mcp.stop(&id).await;
    let tools = state.mcp.tools(&state, &cfg, true).await;
    let tools = tools.map_err(|e| AdminError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(json!({"tools": mcp_tool_rows(&tools), "server": mcp_server_json(&state, &cfg)})))
}

pub async fn mcp_tools(State(state): S, Path(id): Path<String>) -> R {
    mcp::server(&state.db, &id).ok_or_else(not_found)?;
    let c = mcp::cached_tools(&state.db, &id).unwrap_or_default();
    Ok(Json(json!({"tools": mcp_tool_rows(&c.tools), "fetchedAt": c.fetched_at})))
}

#[derive(Deserialize)]
pub struct McpOauthStart {
    /// The dashboard's origin as the browser sees it; the authorization server sends
    /// the browser back to `{origin}/mcp/oauth/callback`.
    origin: String,
}

/// Begins an OAuth sign-in to an MCP server: `{session, authUrl, redirectUri}`.
pub async fn mcp_oauth_start(State(state): S, Path(id): Path<String>, Json(b): Json<McpOauthStart>) -> R {
    let cfg = mcp::server(&state.db, &id).ok_or_else(not_found)?;
    mcp::auth::start(&state, &cfg, &b.origin).await.map(Json).map_err(|e| bad(format!("{e:#}")))
}

/// Finishes a sign-in with the callback URL the user pasted (when the redirect could
/// not reach the router).
pub async fn mcp_oauth_exchange(State(state): S, Path(id): Path<String>, Json(b): Json<OauthStep>) -> R {
    let (code, session) = mcp::auth::parse_callback(&b.callback);
    if session.as_deref().is_some_and(|s| s != b.session) {
        return Err(bad("this callback belongs to another sign-in; start again"));
    }
    mcp::auth::finish(&state, &b.session, &code, Some(&id)).await.map_err(|e| bad(format!("{e:#}")))?;
    let cfg = mcp::server(&state.db, &id).ok_or_else(not_found)?;
    Ok(Json(mcp_server_json(&state, &cfg)))
}

/// Signs out of an MCP server: drops its tokens and closes the connection.
pub async fn mcp_oauth_sign_out(State(state): S, Path(id): Path<String>) -> R {
    mcp::server(&state.db, &id).ok_or_else(not_found)?;
    mcp::auth::forget(&state.db, &id)?;
    state.mcp.stop(&id).await;
    Ok(Json(json!({"ok": true})))
}

pub async fn mcp_stop(State(state): S, Path(id): Path<String>) -> R {
    state.mcp.stop(&id).await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct McpImport {
    config: Value,
}

/// Adds the servers of a pasted `mcpServers` config; ids that already exist are skipped.
pub async fn mcp_import(State(state): S, Json(b): Json<McpImport>) -> R {
    let v = match b.config {
        // Also accept the inside of `"mcpServers": {...}` pasted without the outer braces.
        Value::String(s) => serde_json::from_str(&s)
            .or_else(|_| serde_json::from_str(&format!("{{{s}}}")))
            .map_err(|e| bad(format!("invalid JSON: {e}")))?,
        v => v,
    };
    let (mut added, mut skipped) = (Vec::new(), Vec::new());
    let (configs, rejected) = mcp::parse_import(&v).map_err(bad)?;
    for cfg in configs {
        if mcp::server(&state.db, &cfg.id).is_some() {
            skipped.push(cfg.id);
        } else {
            mcp::save_server(&state.db, &cfg)?;
            added.push(cfg.id);
        }
    }
    Ok(Json(json!({"added": added, "skipped": skipped, "rejected": rejected})))
}
