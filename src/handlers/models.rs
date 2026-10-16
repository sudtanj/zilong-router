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

fn model_entries(state: &AppState, caller: &Caller) -> anyhow::Result<Vec<Value>> {
    let conns = state.db.all_connections()?;
    let active: BTreeSet<&str> = conns.iter().filter(|c| c.is_active).map(|c| c.provider.as_str()).collect();
    let created = state.started_at.elapsed().as_secs() as i64;
    let mut out = Vec::new();
    for p in state.registry.all() {
        if !active.contains(p.id.as_str()) && !p.no_auth {
            continue;
        }
        for m in &p.models {
            if !caller.may_use(&p.id, &m.id) {
                continue;
            }
            out.push(json!({
                "id": format!("{}/{}", p.alias, m.id),
                "object": "model",
                "created": created,
                "owned_by": p.id,
                "name": m.name,
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
    let data = model_entries(&state, &caller).map_err(|e| ApiError::new(500, Format::Openai, e.to_string()))?;
    Ok(Json(json!({"object": "list", "data": data})))
}

pub async fn get_openai(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let caller = auth(&state, peer, &headers, "")?;
    let data = model_entries(&state, &caller).map_err(|e| ApiError::new(500, Format::Openai, e.to_string()))?;
    data.into_iter()
        .find(|m| m["id"] == id)
        .map(Json)
        .ok_or_else(|| ApiError::new(404, Format::Openai, format!("model {id} not found")))
}
