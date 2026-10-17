//! `GET /v1/models`: combos would come first upstream; here it is every model of every
//! provider that has at least one active account, as `alias/model`, plus aliases.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use super::ApiError;
use crate::auth::{authenticate, AuthError, Caller};
use crate::state::AppState;
use crate::translator::Format;

fn auth(state: &AppState, peer: SocketAddr, headers: &HeaderMap, query: &str) -> Result<Caller, ApiError> {
    let require = state.db.settings().get("requireApiKey").and_then(Value::as_bool).unwrap_or(false);
    authenticate(&state.cfg, &state.db, require, peer.ip(), headers, Some(query)).map_err(|e| match e {
        AuthError::Missing => ApiError::new(401, Format::Openai, "API key required for remote API access"),
        AuthError::Invalid => ApiError::new(401, Format::Openai, "Invalid API key"),
    })
}

/// `opencode_free`: OpenCode Free's live chat models, listed next to its catalogue.
fn model_entries(state: &AppState, caller: &Caller, opencode_free: &[String]) -> anyhow::Result<Vec<Value>> {
    let conns = state.db.all_connections()?;
    let active: BTreeSet<&str> = conns.iter().filter(|c| c.is_active).map(|c| c.provider.as_str()).collect();
    let hidden = state.db.kv_scope("hiddenModels")?;
    let created = state.started_at.elapsed().as_secs() as i64;
    let mut out = vec![json!({
        "id": "auto",
        "object": "model",
        "created": created,
        "owned_by": "zilong-router",
        "name": "Auto (first available model)",
    })];
    for p in state.registry.all() {
        if !active.contains(p.id.as_str()) && !p.no_auth {
            continue;
        }
        let live: &[String] = if p.id == crate::oauth::opencode::ID { opencode_free } else { &[] };
        let live = live.iter().filter(|id| p.model(id).is_none()).map(|id| (id.as_str(), id.as_str()));
        for (id, name) in p.models.iter().map(|m| (m.id.as_str(), m.name.as_str())).chain(live) {
            if !caller.may_use(&p.id, id) || crate::model::is_hidden(&hidden, &[p.id.as_str(), p.alias.as_str()], id) {
                continue;
            }
            out.push(json!({
                "id": format!("{}/{}", p.alias, id),
                "object": "model",
                "created": created,
                "owned_by": p.id,
                "name": name,
            }));
        }
    }
    for (alias, target) in state.db.kv_scope("modelAliases")? {
        out.push(json!({"id": alias, "object": "model", "created": created, "owned_by": "alias", "alias_of": target}));
    }
    Ok(out)
}

pub async fn list_openai(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, ApiError> {
    let query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let caller = auth(&state, peer, &headers, &query)?;
    let free = crate::discovery::opencode_free(&state).await;
    let data = model_entries(&state, &caller, &free).map_err(|e| ApiError::new(500, Format::Openai, e.to_string()))?;
    Ok(Json(json!({"object": "list", "data": data})))
}

pub async fn get_openai(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let caller = auth(&state, peer, &headers, "")?;
    let free = crate::discovery::opencode_free(&state).await;
    let data = model_entries(&state, &caller, &free).map_err(|e| ApiError::new(500, Format::Openai, e.to_string()))?;
    data.into_iter()
        .find(|m| m["id"] == id)
        .map(Json)
        .ok_or_else(|| ApiError::new(404, Format::Openai, format!("model {id} not found")))
}
