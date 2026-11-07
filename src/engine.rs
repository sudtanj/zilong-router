//! The request pipeline without a server, for hosts such as a Cloudflare Worker:
//! authenticate, resolve the model (`auto` included), pick an account, translate,
//! call upstream through [`Http`] (falling back across accounts and models), translate
//! the answer back, and record cooldowns and usage through [`Store`].
//!
//! The rules are the binary's (`handlers::chat`); what needs the native runtime is
//! left out: OAuth token refresh and minted tokens (GitHub Copilot, MiMo free tier),
//! live `/models` discovery for accounts (OpenCode Free's list is opt-in, see
//! [`Limits::free_models`]), MCP OAuth tokens as API keys, and the stream
//! stall timer (the host's own limits apply instead).
//!
//! ```ignore
//! let engine = Engine::new(MyFetch, SqlStore::new(MyD1), Limits::default());
//! let reply = engine.route("/v1/messages", &headers, query, &body).await;
//! // reply.status, reply.headers, reply.body (full bytes or a byte stream)
//! ```

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock, OnceLock};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::HeaderMap;
use parking_lot::Mutex;
use serde_json::{json, Value};
use web_time::Instant;

use crate::auth::{extract_key, Caller};
use crate::db::ProviderConnection;
use crate::executor::{self, ExecError};
use crate::host::{ByteStream, Http, HttpRequest, HttpResponse, Snapshot, Store};
use crate::model::{self, AutoKind, Resolved};
use crate::oauth::hooks::{self, Restore};
use crate::oauth::opencode;
use crate::pricing::Table;
use crate::providers::{Format, Registry};
use crate::router::{self, Pick, RoundRobin, Verdict};
use crate::sse;
use crate::translator::collapse::{collapse_sse, synthesize_events, terminal_error};
use crate::translator::responses::{self, StreamConverter};
use crate::translator::{self, StreamTranslator};
use crate::usage::{self, TokenCounts};

/// The embedded provider registry, parsed on first use and kept for the process
/// (a Worker isolate) lifetime.
pub fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Registry::load(None).expect("embedded providers.json parses"))
}

/// The embedded pricing table, parsed on first use.
pub fn pricing() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(|| Table::load(None).expect("embedded pricing.json parses"))
}

/// Round-robin cursors (accounts per provider, `auto` candidates), per process.
static RR: LazyLock<Mutex<HashMap<String, RoundRobin>>> = LazyLock::new(Default::default);

#[derive(Debug, Clone)]
pub struct Limits {
    /// Upstream calls one request may make, retries and fallbacks included. Keep it
    /// under the host's subrequest limit minus the store's calls (Workers Free: 50).
    pub max_upstream_calls: usize,
    /// Deadline for an upstream's response headers.
    pub timeout_ms: u64,
    /// Refuse requests without an API key. The binary lets keyless loopback callers
    /// through; a public host has no loopback, so this defaults to `true`.
    pub require_api_key: bool,
    /// Offer OpenCode Free's live model list to `auto` (and [`Engine::models`]), as the
    /// binary does. The list is fetched through [`Http`] (one upstream call) and kept in
    /// the store's `kv` scope `modelCache` ([`Store::put_kv`]): 30 minutes, 5 after a
    /// failed fetch. Off by default: the fetch needs a store that keeps the cache.
    pub free_models: bool,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_upstream_calls: 40, timeout_ms: 60_000, require_api_key: true, free_models: false }
    }
}

/// `kv` scope of cached model lists and the key of OpenCode Free's.
const MODEL_CACHE: &str = "modelCache";
const OPENCODE_FREE_KEY: &str = "opencode:free";
const FREE_TTL_OK_MS: i64 = 30 * 60_000;
const FREE_TTL_ERR_MS: i64 = 5 * 60_000;
const FREE_TIMEOUT_MS: u64 = 8_000;

fn now_ms() -> i64 {
    web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Whether `auto` may use this provider here. MiMo's free tier needs a JWT minted by the
/// binary (`oauth::hooks::mimo_jwt`); without `native` it can only fail.
fn auto_usable(provider: &str) -> bool {
    cfg!(feature = "native") || !hooks::is_mimo(provider)
}

/// An incoming API request.
#[derive(Clone)]
pub struct Request<'a> {
    /// Dialect of the client: `Openai` for `/v1/chat/completions`, `Claude` for `/v1/messages`.
    pub source: Format,
    /// Path, recorded in usage rows (e.g. `/v1/messages`).
    pub endpoint: &'a str,
    pub headers: &'a HeaderMap,
    /// Raw query string (`?key=` is accepted as an API key, like upstream).
    pub query: Option<&'a str>,
    pub body: &'a [u8],
    /// Set when the host authenticated the caller itself; the key lookup is skipped.
    pub caller: Option<Caller>,
}

pub enum ReplyBody {
    Full(Bytes),
    Stream(ByteStream),
}

/// What to send back to the client.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ReplyBody,
}

impl Reply {
    pub fn json(status: u16, v: &Value) -> Reply {
        Reply {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: ReplyBody::Full(Bytes::from(v.to_string())),
        }
    }

    /// An error in the client's dialect (`{"error": …}` or Anthropic's `{"type": "error", …}`).
    pub fn error(format: Format, status: u16, message: &str) -> Reply {
        Reply::json(status, &translator::response::error_body(format, status, message))
    }

    fn sse(body: ReplyBody) -> Reply {
        Reply {
            status: 200,
            headers: vec![
                ("content-type".into(), "text/event-stream".into()),
                ("cache-control".into(), "no-cache".into()),
                ("x-accel-buffering".into(), "no".into()),
            ],
            body,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn is_sse(&self) -> bool {
        self.header("content-type").is_some_and(|ct| ct.starts_with("text/event-stream"))
    }
}

/// A failure before the first byte went out.
#[derive(Debug)]
struct Fail {
    status: u16,
    message: String,
    retry_after: Option<u64>,
}

impl Fail {
    fn new(status: u16, message: impl Into<String>) -> Fail {
        Fail { status, message: message.into(), retry_after: None }
    }

    fn reply(self, format: Format) -> Reply {
        let mut r = Reply::error(format, self.status, &self.message);
        if let Some(s) = self.retry_after {
            r.headers.push(("retry-after".into(), s.to_string()));
        }
        r
    }
}

#[cfg(feature = "native")]
fn boxed(s: impl Stream<Item = Result<Bytes, String>> + Send + 'static) -> ByteStream {
    s.boxed()
}

#[cfg(not(feature = "native"))]
fn boxed(s: impl Stream<Item = Result<Bytes, String>> + 'static) -> ByteStream {
    s.boxed_local()
}

/// Local estimate (chars / 4) for `/v1/messages/count_tokens`, like upstream; no
/// provider call.
pub fn count_tokens(body: &[u8]) -> Value {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let chars = ["system", "messages", "tools"]
        .iter()
        .map(|k| v.get(k).map(|x| x.to_string().len()).unwrap_or(0))
        .sum::<usize>();
    json!({"input_tokens": chars.div_ceil(4)})
}

/// Routes requests with a host's HTTP client and store. Cheap to build per request;
/// the registry and pricing table are process-wide statics.
pub struct Engine<H, S> {
    pub registry: &'static Registry,
    pub pricing: &'static Table,
    pub http: Arc<H>,
    pub store: Arc<S>,
    pub limits: Limits,
}

impl<H, S> Clone for Engine<H, S> {
    fn clone(&self) -> Self {
        Engine {
            registry: self.registry,
            pricing: self.pricing,
            http: self.http.clone(),
            store: self.store.clone(),
            limits: self.limits.clone(),
        }
    }
}

/// Per-request context.
struct Cx<'a> {
    snap: &'a Snapshot,
    caller: &'a Caller,
    headers: &'a HeaderMap,
    source: Format,
    body: &'a Value,
    client_stream: bool,
    endpoint: &'a str,
    started: Instant,
    body_len: usize,
}

impl<H: Http + 'static, S: Store + 'static> Engine<H, S> {
    pub fn new(http: H, store: S, limits: Limits) -> Engine<H, S> {
        Engine { registry: registry(), pricing: pricing(), http: Arc::new(http), store: Arc::new(store), limits }
    }

    /// Dispatch by path: `/v1/chat/completions`, `/v1/messages`, `/v1/responses`,
    /// `/v1/messages/count_tokens`, `/v1/models` (and the doubled `/v1/v1/…` forms). Anything
    /// else is 404.
    pub async fn route(&self, path: &str, headers: &HeaderMap, query: Option<&str>, body: &[u8]) -> Reply {
        let p = path.strip_prefix("/v1").filter(|r| r.starts_with("/v1/")).unwrap_or(path);
        let req = |source| Request { source, endpoint: path, headers, query, body, caller: None };
        match p {
            "/v1/chat/completions" => self.chat(req(Format::Openai)).await,
            "/v1/messages" => self.chat(req(Format::Claude)).await,
            "/v1/responses" => self.responses(req(Format::Openai)).await,
            "/v1/models" => self.models(req(Format::Openai)).await,
            "/v1/messages/count_tokens" => match self.authorize(&req(Format::Claude), &[]).await {
                Ok(_) => Reply::json(200, &count_tokens(body)),
                Err(e) => e.reply(Format::Claude),
            },
            _ => Reply::error(Format::Openai, 404, &format!("no route for {path}")),
        }
    }

    /// Load the snapshot and check the caller.
    async fn authorize(&self, req: &Request<'_>, scopes: &[&str]) -> Result<(Snapshot, Caller), Fail> {
        let key = if req.caller.is_some() { None } else { extract_key(req.headers, req.query) };
        let snap = self.store.snapshot(key.as_deref(), scopes).await.map_err(|e| Fail::new(500, e.to_string()))?;
        if let Some(c) = &req.caller {
            return Ok((snap, c.clone()));
        }
        if let Some(k) = key {
            return match snap.api_key.clone() {
                Some(row) if row.is_active && row.key == k => Ok((snap, Caller::Key(row))),
                _ => Err(Fail::new(401, "Invalid API key")),
            };
        }
        let require =
            self.limits.require_api_key || snap.settings.get("requireApiKey").and_then(Value::as_bool).unwrap_or(false);
        if require {
            return Err(Fail::new(401, "API key required for remote API access"));
        }
        Ok((snap, Caller::Local))
    }

    /// `/v1/chat/completions` (source `Openai`) or `/v1/messages` (source `Claude`).
    pub async fn chat(&self, req: Request<'_>) -> Reply {
        let source = req.source;
        match self.chat_inner(req).await {
            Ok(r) => r,
            Err(e) => e.reply(source),
        }
    }

    /// The `kv` scopes a chat or model list request reads.
    fn scopes(&self) -> &'static [&'static str] {
        if self.limits.free_models {
            &["modelAliases", "customModels", "hiddenModels", MODEL_CACHE]
        } else {
            &["modelAliases", "customModels", "hiddenModels"]
        }
    }

    /// OpenCode Free's chat models: the cached live list when fresh, else fetched now
    /// (one upstream call from `budget`) and cached, plus the documented free models.
    async fn opencode_free(&self, snap: &Snapshot, budget: &mut usize) -> Vec<String> {
        let Some(url) = self.registry.get(opencode::ID).and_then(opencode::models_url) else {
            return Vec::new();
        };
        let now = now_ms();
        if let Some(c) = snap.kv.get(MODEL_CACHE).and_then(|m| m.get(OPENCODE_FREE_KEY)) {
            let at = c.get("at").and_then(Value::as_i64).unwrap_or(0);
            let list: Vec<String> = c
                .get("models")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|m| m.as_str().map(str::to_string))
                .collect();
            let ttl = if list.is_empty() { FREE_TTL_ERR_MS } else { FREE_TTL_OK_MS };
            if at <= now && now - at < ttl {
                return opencode::with_documented(list);
            }
        }
        if *budget == 0 {
            return opencode::with_documented(Vec::new());
        }
        *budget -= 1;
        let req = HttpRequest {
            method: "GET".into(),
            url,
            headers: vec![
                ("Authorization".into(), "Bearer public".into()),
                ("User-Agent".into(), opencode::USER_AGENT.into()),
            ],
            body: Vec::new(),
            timeout_ms: Some(FREE_TIMEOUT_MS),
        };
        let list = match self.http.send(req).await {
            Ok(r) if (200..300).contains(&r.status) => match r.text().await {
                Ok(t) => opencode::free_models(opencode::model_ids(&serde_json::from_str(&t).unwrap_or(Value::Null))),
                Err(e) => {
                    tracing::info!("auto: cannot read OpenCode Free's model list: {e}");
                    Vec::new()
                }
            },
            Ok(r) => {
                tracing::info!("auto: OpenCode Free's model list answered {}", r.status);
                Vec::new()
            }
            Err(e) => {
                tracing::info!("auto: cannot list OpenCode Free models: {e}");
                Vec::new()
            }
        };
        let entry = json!({"at": now, "models": list});
        if let Err(e) = self.store.put_kv(MODEL_CACHE, OPENCODE_FREE_KEY, &entry).await {
            tracing::warn!("caching OpenCode Free's model list failed: {e}");
        }
        opencode::with_documented(list)
    }

    /// `auto` candidates in their natural order (rotation is left to the caller).
    async fn auto_list(
        &self,
        snap: &Snapshot,
        caller: &Caller,
        source: Format,
        explicit: &[String],
        budget: &mut usize,
    ) -> Vec<Resolved> {
        let aliases = snap.scope("modelAliases");
        let mut custom = snap.scope("customModels");
        if explicit.is_empty() && self.limits.free_models && self.registry.get(opencode::ID).is_some() {
            let free = self.opencode_free(snap, budget).await;
            let entry = custom.entry(opencode::ID.to_string()).or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(list) = entry {
                list.extend(free.into_iter().map(Value::String));
            }
        }
        let mut candidates =
            model::auto_candidates(self.registry, &aliases, &snap.connections, explicit, &custom, &HashMap::new());
        if explicit.is_empty() {
            let hidden = snap.scope("hiddenModels");
            candidates.retain(|r| {
                !model::is_hidden(&hidden, &model::hidden_keys(self.registry, &snap.connections, r), &r.model)
            });
        }
        candidates.retain(|c| {
            auto_usable(&c.provider)
                && self.registry.get(&c.provider).is_some_and(|p| {
                    caller.may_use(&p.id, &c.model)
                        && p.transport_for(source, &c.model).map(|t| t.format.supported()).unwrap_or(false)
                })
        });
        candidates
    }

    /// `GET /v1/models` in OpenAI's list shape: the `auto` models, then every model
    /// `auto` could pick (as `alias/model`), then the model aliases.
    pub async fn models(&self, req: Request<'_>) -> Reply {
        let (snap, caller) = match self.authorize(&req, self.scopes()).await {
            Ok(r) => r,
            Err(e) => return e.reply(Format::Openai),
        };
        let mut budget = self.limits.max_upstream_calls;
        let candidates = self.auto_list(&snap, &caller, Format::Openai, &[], &mut budget).await;
        let entry = |id: String, owner: &str| json!({"id": id, "object": "model", "created": 0, "owned_by": owner});
        let mut data: Vec<Value> =
            ["auto", "auto-strong", "auto-images"].iter().map(|id| entry(id.to_string(), "zilong-router")).collect();
        let mut seen = HashSet::new();
        for c in candidates {
            let Some(p) = self.registry.get(&c.provider) else { continue };
            let id = format!("{}/{}", p.alias, c.model);
            if seen.insert(id.clone()) {
                data.push(entry(id, &p.id));
            }
        }
        for (alias, target) in snap.scope("modelAliases") {
            let mut e = entry(alias, "alias");
            e["alias_of"] = target;
            data.push(e);
        }
        Reply::json(200, &json!({"object": "list", "data": data}))
    }

    async fn chat_inner(&self, req: Request<'_>) -> Result<Reply, Fail> {
        let started = Instant::now();
        let source = req.source;
        let (snap, caller) = self.authorize(&req, self.scopes()).await?;
        let mut body: Value =
            serde_json::from_slice(req.body).map_err(|e| Fail::new(400, format!("invalid JSON body: {e}")))?;
        let raw_model = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.trim().is_empty())
            .ok_or_else(|| Fail::new(400, "model is required"))?
            .to_string();
        let client_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let aliases = snap.scope("modelAliases");
        let auto = model::is_auto(&raw_model);
        // OpenRouter-style fallback list; consumed here, never forwarded upstream.
        let requested: Vec<String> = match body.as_object_mut().and_then(|o| o.remove("models")) {
            Some(Value::Array(a)) if auto => a.into_iter().filter_map(|m| m.as_str().map(str::to_string)).collect(),
            Some(other) if !auto => {
                body["models"] = other;
                Vec::new()
            }
            _ => Vec::new(),
        };
        let cx = Cx {
            snap: &snap,
            caller: &caller,
            headers: req.headers,
            source,
            body: &body,
            client_stream,
            endpoint: req.endpoint,
            started,
            body_len: req.body.len(),
        };
        let mut budget = self.limits.max_upstream_calls;

        if !auto {
            let resolved = model::resolve(self.registry, &aliases, &snap.connections, &raw_model);
            return self.attempt(&cx, &resolved, &raw_model, &mut budget).await;
        }

        // `auto`: walk every candidate until one answers. Upstream errors only surface
        // before the first byte is sent, so moving on to the next model is always safe.
        let kind = model::auto_kind(&raw_model).unwrap_or(AutoKind::Any);
        let explicit = if requested.is_empty() {
            snap.settings
                .get(kind.settings_key())
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|m| m.as_str().map(str::to_string))
                .collect()
        } else {
            requested
        };
        let mut candidates = self.auto_list(&snap, &caller, source, &explicit, &mut budget).await;
        match kind.score() {
            Some(score) if explicit.is_empty() => {
                let top = model::best_first(&mut candidates, score);
                router::rotate(&snap.settings, &RR, &mut candidates[..top], kind.rr_key());
            }
            _ => router::rotate(&snap.settings, &RR, &mut candidates, kind.rr_key()),
        }

        let mut last: Option<Fail> = None;
        let mut failures: Vec<String> = Vec::new();
        for c in candidates {
            if budget == 0 {
                break;
            }
            let Some(p) = self.registry.get(&c.provider) else { continue };
            let label = format!("{}/{}", p.id, c.model);
            match self.attempt(&cx, &c, &label, &mut budget).await {
                Ok(mut resp) => {
                    resp.headers.push(("x-zilong-router-model".into(), label));
                    return Ok(resp);
                }
                Err(e) => {
                    tracing::info!("auto: {label} unavailable ({} {}), trying the next model", e.status, e.message);
                    failures.push(format!("{label}: {} {}", e.status, e.message.chars().take(160).collect::<String>()));
                    last = Some(e);
                }
            }
        }
        Err(match last {
            Some(e) => {
                let skipped = failures.len().saturating_sub(10);
                let more = if skipped > 0 { format!("{skipped} earlier failures, then ") } else { String::new() };
                let message =
                    format!("auto: all {} models failed. {more}{}", failures.len(), failures[skipped..].join(" | "));
                Fail { message, ..e }
            }
            None if kind == AutoKind::Images => Fail::new(
                404,
                "auto-images: no image-capable model is available. Add an account for a provider with vision \
                 models, or list models in settings.autoImageModels",
            ),
            None if kind == AutoKind::Strong => Fail::new(
                404,
                "auto-strong: no strong model is available. Add an account for a provider with strong models, \
                 or list models in settings.autoStrongModels",
            ),
            None => {
                Fail::new(404, "auto: no usable model. Add a provider account, or list models in settings.autoModels")
            }
        })
    }

    async fn attempt(
        &self,
        cx: &Cx<'_>,
        resolved: &Resolved,
        raw_model: &str,
        budget: &mut usize,
    ) -> Result<Reply, Fail> {
        let source = cx.source;
        let settings = &cx.snap.settings;
        let provider = self.registry.get(&resolved.provider).ok_or_else(|| {
            Fail::new(404, format!("Unknown provider '{}' in model '{raw_model}'", resolved.provider))
        })?;
        if !cx.caller.may_use(&provider.id, &resolved.model) {
            return Err(Fail::new(403, format!("This API key may not use {}/{}", provider.id, resolved.model)));
        }
        let transport = provider
            .transport_for(source, &resolved.model)
            .ok_or_else(|| Fail::new(406, format!("Provider {} has no chat endpoint", provider.id)))?;
        if !transport.format.supported() {
            return Err(Fail::new(
                406,
                format!(
                    "Provider {} uses the '{}' wire format, which this build does not implement",
                    provider.id,
                    transport.format.as_str()
                ),
            ));
        }
        let target = transport.format;
        let upstream_model = provider.upstream_model_id(&resolved.model).to_string();
        let mut upstream_body = translator::translate_request(source, target, cx.body.clone(), &upstream_model)
            .map_err(|e| Fail::new(406, e))?;
        // Some upstreams only answer streamed requests; the stream is collapsed for JSON clients.
        let upstream_stream = cx.client_stream || transport.force_stream;
        upstream_body["stream"] = json!(upstream_stream);
        if upstream_stream && target == Format::Openai && upstream_body.get("stream_options").is_none() {
            upstream_body["stream_options"] = json!({"include_usage": true});
        }
        let saver = crate::token_saver::Options::from_settings(settings, cx.headers);
        crate::token_saver::apply(&mut upstream_body, target, &saver);

        let mut exclude: HashSet<String> = HashSet::new();
        let mut last_err: Option<(u16, String)> = None;
        let mut tried = 0usize;
        loop {
            if *budget == 0 {
                break;
            }
            let active = || {
                Ok(cx
                    .snap
                    .connections
                    .iter()
                    .filter(|c| c.provider == provider.id && c.is_active)
                    .cloned()
                    .collect::<Vec<ProviderConnection>>())
            };
            let pick = router::choose(
                Some(provider),
                &provider.id,
                active,
                || settings.clone(),
                &RR,
                &resolved.model,
                &exclude,
                resolved.pinned_connection.as_deref(),
            )
            .map_err(|e| Fail::new(500, e.to_string()))?;
            let conn = match pick {
                Pick::Connection(c) => c,
                Pick::AllLocked { retry_after_secs } => {
                    let msg = match &last_err {
                        Some((_, m)) => format!(
                            "[{}/{}] {m} (all accounts cooling down, retry in {retry_after_secs}s)",
                            provider.id, resolved.model
                        ),
                        None => format!(
                            "[{}/{}] all accounts are rate limited; retry in {retry_after_secs}s",
                            provider.id, resolved.model
                        ),
                    };
                    return Err(Fail { retry_after: Some(retry_after_secs), ..Fail::new(503, msg) });
                }
                Pick::None => {
                    return Err(match last_err {
                        Some((status, m)) => Fail::new(
                            if status >= 500 { 502 } else { status },
                            format!("[{}/{}] {m}", provider.id, resolved.model),
                        ),
                        None if tried == 0 => {
                            Fail::new(404, format!("No active credentials for provider '{}'", provider.id))
                        }
                        None => Fail::new(503, "all accounts failed"),
                    });
                }
            };
            tried += 1;
            let level = conn.data.get("backoffLevel").and_then(Value::as_i64).unwrap_or(0);
            let patched = hooks::body(&provider.id, &conn, target, &upstream_model, &upstream_body);
            let (send_body, decloak) = match &patched {
                Some((b, r)) => (b, r.clone()),
                None => (&upstream_body, Restore::None),
            };

            let sent = executor::send_via(
                &*self.http,
                settings,
                provider,
                transport,
                &conn,
                &upstream_model,
                send_body,
                cx.headers,
                upstream_stream,
                self.limits.timeout_ms,
                budget,
            )
            .await;
            match sent {
                Ok(resp) => {
                    tracing::info!(
                        "{} {}/{} via {} -> {} in {}ms{}",
                        cx.endpoint,
                        provider.id,
                        resolved.model,
                        conn.label(),
                        resp.status,
                        cx.started.elapsed().as_millis(),
                        if cx.client_stream { " (stream)" } else { "" }
                    );
                    let fin = Finish {
                        store: self.store.clone(),
                        pricing: self.pricing,
                        settings: settings.clone(),
                        provider: provider.id.clone(),
                        model: resolved.model.clone(),
                        conn,
                        api_key: cx.caller.key_value().map(str::to_string),
                        endpoint: cx.endpoint.to_string(),
                        started: cx.started,
                        body_len: cx.body_len,
                    };
                    let tr = Translate { source, target, model: resolved.model.clone(), decloak };
                    return if cx.client_stream && resp.is_sse() {
                        Ok(stream_reply(fin, tr, resp))
                    } else {
                        json_reply(fin, tr, resp, cx.client_stream).await
                    };
                }
                Err(e) => {
                    let (status, msg) = (e.status(), e.message());
                    let verdict = router::classify(status, &msg, level);
                    tracing::warn!(
                        "{}/{} via {} failed: {} {}",
                        provider.id,
                        resolved.model,
                        conn.label(),
                        status,
                        msg.chars().take(200).collect::<String>()
                    );
                    match verdict {
                        Verdict::NoFallback => {
                            return Err(Fail::new(status, format!("[{}/{}] {msg}", provider.id, resolved.model)));
                        }
                        Verdict::Fallback { .. } => {
                            if let Some(patch) =
                                router::unavailable_patch(&conn, &resolved.model, status, &msg, verdict)
                            {
                                if let Err(e) = self.store.patch_connection_data(&conn.id, &patch).await {
                                    tracing::warn!("failed to persist lock for {}: {e}", conn.id);
                                }
                                router::log_locked(&conn, &resolved.model, status, &msg, verdict);
                            }
                            exclude.insert(conn.id.clone());
                            last_err = Some((status, msg));
                            if matches!(e, ExecError::Network(_)) && tried >= 3 {
                                break;
                            }
                        }
                    }
                }
            }
        }
        let (status, m) = last_err.unwrap_or((502, "upstream unreachable".into()));
        Err(Fail::new(if status >= 500 { 502 } else { status }, format!("[{}/{}] {m}", provider.id, resolved.model)))
    }

    /// `/v1/responses` (OpenAI Responses API, what Codex speaks), served by [`Engine::chat`].
    pub async fn responses(&self, req: Request<'_>) -> Reply {
        // Key first: a caller without one learns nothing about what the body should look like.
        if let Err(e) = self.authorize(&req, &[]).await {
            return e.reply(Format::Openai);
        }
        let v: Value = match serde_json::from_slice(req.body) {
            Ok(v) => v,
            Err(e) => return Reply::error(Format::Openai, 400, &format!("invalid JSON body: {e}")),
        };
        if v.get("previous_response_id").is_some_and(|p| !p.is_null()) {
            return Reply::error(
                Format::Openai,
                400,
                "previous_response_id is not supported: responses are not stored, send the full input",
            );
        }
        let (chat_body, ctx) = match responses::request_to_chat(&v) {
            Ok(r) => r,
            Err(e) => return Reply::error(Format::Openai, 400, &e),
        };
        let chat_bytes = chat_body.to_string();
        let inner = self.chat(Request { source: Format::Openai, body: chat_bytes.as_bytes(), ..req }).await;
        if inner.status != 200 {
            return inner;
        }
        let is_sse = inner.is_sse();
        let headers: Vec<(String, String)> = inner
            .headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("content-type") && !k.eq_ignore_ascii_case("content-length"))
            .cloned()
            .collect();
        let mut out = match (inner.body, is_sse) {
            (ReplyBody::Full(b), false) => {
                let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                Reply::json(200, &responses::chat_to_response(&v, &ctx))
            }
            (ReplyBody::Full(b), true) => {
                let mut parser = sse::Parser::default();
                let mut conv = StreamConverter::new(ctx);
                let mut evs: Vec<sse::Event> =
                    parser.push(&b).into_iter().chain(parser.finish()).flat_map(|ev| conv.feed(&ev)).collect();
                evs.extend(conv.finish());
                let bytes: Vec<u8> = evs.iter().flat_map(|ev| ev.to_bytes()).collect();
                Reply::sse(ReplyBody::Full(Bytes::from(bytes)))
            }
            (ReplyBody::Stream(s), _) => Reply::sse(ReplyBody::Stream(responses_stream(s, StreamConverter::new(ctx)))),
        };
        for h in headers {
            if out.header(&h.0).is_none() {
                out.headers.push(h);
            }
        }
        out
    }
}

/// Chat-completions SSE to Responses API events, lazily.
fn responses_stream(inner: ByteStream, conv: StreamConverter) -> ByteStream {
    struct St {
        inner: ByteStream,
        parser: sse::Parser,
        conv: StreamConverter,
        out: VecDeque<Bytes>,
        done: bool,
    }
    let st = St { inner, parser: sse::Parser::default(), conv, out: VecDeque::new(), done: false };
    boxed(futures_util::stream::unfold(st, |mut st| async move {
        loop {
            if let Some(b) = st.out.pop_front() {
                return Some((Ok(b), st));
            }
            if st.done {
                return None;
            }
            let evs: Vec<sse::Event> = match st.inner.next().await {
                Some(Ok(c)) => st.parser.push(&c).iter().flat_map(|ev| st.conv.feed(ev)).collect(),
                Some(Err(e)) => {
                    st.done = true;
                    st.conv.fail(&format!("upstream stream error: {e}"))
                }
                None => {
                    st.done = true;
                    let mut evs: Vec<sse::Event> = st.parser.finish().iter().flat_map(|ev| st.conv.feed(ev)).collect();
                    evs.extend(st.conv.finish());
                    evs
                }
            };
            if st.conv.finished {
                st.done = true;
            }
            st.out.extend(evs.iter().map(sse::Event::to_bytes));
        }
    }))
}

/// How to turn the upstream answer into the client's dialect.
struct Translate {
    source: Format,
    target: Format,
    model: String,
    /// Tool names a provider hook changed; restored in the response.
    decloak: Restore,
}

/// What is written once the answer is complete: the account's success marker and the
/// usage row.
struct Finish<S> {
    store: Arc<S>,
    pricing: &'static Table,
    settings: Value,
    provider: String,
    model: String,
    conn: ProviderConnection,
    api_key: Option<String>,
    endpoint: String,
    started: Instant,
    body_len: usize,
}

impl<S: Store> Finish<S> {
    async fn run(&self, tokens: TokenCounts, output_chars: usize, status: &str, ttft: Option<u128>) {
        if self.conn.id != "noauth" {
            let patch = router::success_patch(&self.conn, &self.model);
            if let Err(e) = self.store.patch_connection_data(&self.conn.id, &patch).await {
                tracing::warn!("failed to clear the cooldown of {}: {e}", self.conn.id);
            }
        }
        let tokens = if tokens.is_empty() { usage::estimate(self.body_len, output_chars) } else { tokens };
        let record = usage::Record {
            provider: &self.provider,
            model: &self.model,
            connection_id: Some(&self.conn.id),
            api_key: self.api_key.as_deref(),
            endpoint: &self.endpoint,
            status,
            tokens,
            latency_ms: self.started.elapsed().as_millis(),
            ttft_ms: ttft,
        };
        if let Some(row) = usage::row(self.pricing, &self.settings, record) {
            if let Err(e) = self.store.insert_usage(&row).await {
                tracing::warn!("usage insert failed: {e}");
            }
        }
    }
}

async fn json_reply<S: Store>(
    fin: Finish<S>,
    tr: Translate,
    resp: HttpResponse,
    client_stream: bool,
) -> Result<Reply, Fail> {
    let upstream_sse = resp.is_sse();
    let text = resp.text().await.map_err(|e| Fail::new(502, format!("reading upstream body: {e}")))?;
    let v: Value = if upstream_sse {
        // Upstream insisted on streaming; collapse it into one JSON document.
        collapse_sse(&text, tr.target, tr.source, &tr.model, &tr.decloak)
    } else {
        let mut v: Value = serde_json::from_str(&text).map_err(|_| {
            Fail::new(502, format!("upstream returned non-JSON: {}", text.chars().take(200).collect::<String>()))
        })?;
        hooks::fix_response(&fin.provider, &mut v);
        hooks::decloak_json(&mut v, &tr.decloak);
        translator::translate_response(tr.target, tr.source, &v, &tr.model)
    };
    let tokens = usage::extract(&v).unwrap_or_default();
    fin.run(tokens, text.len(), "200 OK", None).await;
    if client_stream {
        // Client wanted SSE but upstream gave JSON (rare): emit the document as a stream.
        let mut t = StreamTranslator::new(tr.source, tr.source, &tr.model);
        let mut bytes = Vec::new();
        for ev in synthesize_events(&v, tr.source) {
            for e in t.feed(&ev) {
                bytes.extend_from_slice(&e.to_bytes());
            }
        }
        for e in t.finish() {
            bytes.extend_from_slice(&e.to_bytes());
        }
        return Ok(Reply::sse(ReplyBody::Full(Bytes::from(bytes))));
    }
    Ok(Reply::json(200, &v))
}

/// Translate the upstream SSE body into the client's dialect as it arrives; the usage
/// row is written when the stream ends.
fn stream_reply<S: Store + 'static>(fin: Finish<S>, tr: Translate, resp: HttpResponse) -> Reply {
    struct Pump<S> {
        upstream: ByteStream,
        parser: sse::Parser,
        tr: StreamTranslator,
        decloak: Restore,
        source: Format,
        out: VecDeque<Bytes>,
        done: bool,
        status: &'static str,
        ttft: Option<u128>,
        fin: Option<Finish<S>>,
    }
    let p = Pump {
        upstream: resp.body,
        parser: sse::Parser::default(),
        tr: StreamTranslator::new(tr.target, tr.source, &tr.model),
        decloak: tr.decloak,
        source: tr.source,
        out: VecDeque::new(),
        done: false,
        status: "200 OK",
        ttft: None,
        fin: Some(fin),
    };
    let s = futures_util::stream::unfold(p, |mut p| async move {
        loop {
            if let Some(b) = p.out.pop_front() {
                return Some((Ok(b), p));
            }
            if p.done {
                if let Some(fin) = p.fin.take() {
                    fin.run(p.tr.usage, p.tr.output_chars, p.status, p.ttft).await;
                }
                return None;
            }
            match p.upstream.next().await {
                Some(Ok(chunk)) => {
                    if p.ttft.is_none() {
                        p.ttft = p.fin.as_ref().map(|f| f.started.elapsed().as_millis());
                    }
                    for mut ev in p.parser.push(&chunk) {
                        hooks::decloak_event(&mut ev, &p.decloak);
                        for out in p.tr.feed(&ev) {
                            p.out.push_back(out.to_bytes());
                        }
                    }
                    if p.tr.finished {
                        p.done = true;
                    }
                }
                Some(Err(e)) => {
                    tracing::warn!("stream error: {e}");
                    p.status = "FAILED stream";
                    p.out.push_back(terminal_error(p.source, &format!("upstream stream error: {e}")));
                    p.done = true;
                }
                None => {
                    if let Some(mut ev) = p.parser.finish() {
                        hooks::decloak_event(&mut ev, &p.decloak);
                        for out in p.tr.feed(&ev) {
                            p.out.push_back(out.to_bytes());
                        }
                    }
                    for out in p.tr.finish() {
                        p.out.push_back(out.to_bytes());
                    }
                    p.done = true;
                }
            }
        }
    });
    Reply::sse(ReplyBody::Stream(boxed(s)))
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::host::{HttpRequest, SqlStore};

    /// Answers every call with the next canned (status, content type, body) and keeps
    /// the requests.
    struct Fake {
        answers: Mutex<VecDeque<(u16, &'static str, String)>>,
        seen: Mutex<Vec<HttpRequest>>,
    }

    #[async_trait::async_trait]
    impl Http for Fake {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, String> {
            self.seen.lock().push(req);
            let (status, ct, body) = self.answers.lock().pop_front().ok_or("no answer left")?;
            let chunks: Vec<Result<Bytes, String>> =
                body.into_bytes().chunks(7).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
            Ok(HttpResponse {
                status,
                headers: vec![("Content-Type".into(), ct.into())],
                body: futures_util::stream::iter(chunks).boxed(),
            })
        }
        async fn sleep(&self, _ms: u64) {}
    }

    fn fake(answers: Vec<(u16, &'static str, String)>) -> Fake {
        Fake { answers: Mutex::new(answers.into()), seen: Mutex::new(Vec::new()) }
    }

    fn db() -> Db {
        let dir = std::env::temp_dir().join(format!("zr-engine-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.sqlite")).unwrap();
        for (id, prio) in [("a", 1), ("b", 2)] {
            db.insert_connection(&ProviderConnection {
                id: id.into(),
                provider: "openai".into(),
                auth_type: "apikey".into(),
                name: Some(id.into()),
                email: None,
                priority: Some(prio),
                is_active: true,
                data: json!({"apiKey": format!("sk-{id}")}),
                created_at: crate::db::now_iso(),
                updated_at: crate::db::now_iso(),
            })
            .unwrap();
        }
        db
    }

    fn open_limits() -> Limits {
        Limits { require_api_key: false, ..Limits::default() }
    }

    async fn body_text(r: Reply) -> String {
        match r.body {
            ReplyBody::Full(b) => String::from_utf8_lossy(&b).into_owned(),
            ReplyBody::Stream(mut s) => {
                let mut out = Vec::new();
                while let Some(c) = s.next().await {
                    out.extend_from_slice(&c.unwrap());
                }
                String::from_utf8_lossy(&out).into_owned()
            }
        }
    }

    const OPENAI_ANSWER: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"gpt-4o-mini","choices":[{"index":0,"message":{"role":"assistant","content":"hi there"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#;

    #[tokio::test]
    async fn claude_client_openai_upstream_with_fallback() {
        // First account is rate limited, the second answers.
        let http = fake(vec![
            (429, "application/json", r#"{"error":{"message":"slow down"}}"#.into()),
            (200, "application/json", OPENAI_ANSWER.into()),
        ]);
        let e = Engine::new(http, db(), open_limits());
        let body = br#"{"model":"openai/gpt-4o-mini","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#;
        let r = e.route("/v1/messages", &HeaderMap::new(), None, body).await;
        assert_eq!(r.status, 200);
        let v: Value = serde_json::from_str(&body_text(r).await).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "hi there");
        let seen = e.http.seen.lock();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].url.ends_with("/chat/completions"));
        let auth = |i: usize| seen[i].headers.iter().find(|(k, _)| k == "Authorization").map(|(_, v)| v.clone());
        assert_eq!(auth(0).as_deref(), Some("Bearer sk-a"));
        assert_eq!(auth(1).as_deref(), Some("Bearer sk-b"));
        // Account a is cooling down, the usage row is written.
        let a = e.store.get_connection("a").unwrap().unwrap();
        assert!(a.data.get("modelLock_gpt-4o-mini").is_some());
        assert_eq!(e.store.usage_recent(10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn openai_stream_to_claude_client_over_sql_store() {
        let sse = [
            r#"{"id":"c1","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"lo"}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
            "[DONE]",
        ]
        .iter()
        .map(|d| format!("data: {d}\n\n"))
        .collect::<String>();
        let http = fake(vec![(200, "text/event-stream", sse)]);
        let store = SqlStore::new(db());
        store.migrate().await.unwrap();
        let e = Engine::new(http, store, open_limits());
        let body = br#"{"model":"openai/gpt-4o-mini","max_tokens":10,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
        let r = e.route("/v1/messages", &HeaderMap::new(), None, body).await;
        assert_eq!(r.status, 200);
        assert!(r.is_sse());
        let text = body_text(r).await;
        assert!(text.contains("event: message_start"), "{text}");
        assert!(text.contains("\"text\":\"Hel\""), "{text}");
        assert!(text.contains("event: message_stop"), "{text}");
        let rows = e.store.sql.usage_recent(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["promptTokens"], 3);
        let a = e.store.sql.get_connection("a").unwrap().unwrap();
        assert!(a.data.get("lastUsedAt").is_some());
    }

    #[tokio::test]
    async fn api_keys_are_required_by_default() {
        let e = Engine::new(fake(vec![]), db(), Limits::default());
        let body = br#"{"model":"openai/gpt-4o-mini","messages":[]}"#;
        let r = e.route("/v1/chat/completions", &HeaderMap::new(), None, body).await;
        assert_eq!(r.status, 401);
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer sk-nope".parse().unwrap());
        assert_eq!(e.route("/v1/chat/completions", &h, None, body).await.status, 401);
        assert_eq!(e.route("/nope", &h, None, body).await.status, 404);
    }

    #[tokio::test]
    async fn opencode_free_list_is_fetched_once_and_cached_in_kv() {
        let dir = std::env::temp_dir().join(format!("zr-engine-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SqlStore::new(Db::open(&dir.join("t.sqlite")).unwrap());
        store.migrate().await.unwrap();
        let http = fake(vec![(200, "application/json", r#"{"data":[{"id":"brand-new-free"},{"id":"gpt-5"}]}"#.into())]);
        let e = Engine::new(http, store, Limits { free_models: true, ..open_limits() });
        for _ in 0..2 {
            let r = e.route("/v1/models", &HeaderMap::new(), None, b"").await;
            assert_eq!(r.status, 200);
            let v: Value = serde_json::from_str(&body_text(r).await).unwrap();
            let ids: Vec<&str> = v["data"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
            assert_eq!(ids[0], "auto");
            assert!(ids.contains(&"oc/brand-new-free"), "{ids:?}");
            assert!(ids.contains(&"oc/big-pickle"), "{ids:?}");
            assert!(!ids.contains(&"oc/gpt-5"), "{ids:?}");
        }
        // The second listing came from the kv cache.
        let seen = e.http.seen.lock();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].url.ends_with("/zen/v1/models"));
        let cached = e.store.sql.kv_get(MODEL_CACHE, OPENCODE_FREE_KEY).unwrap().unwrap();
        assert_eq!(cached["models"], json!(["brand-new-free"]));
    }

    #[tokio::test]
    async fn free_models_are_off_by_default() {
        let e = Engine::new(fake(vec![]), db(), open_limits());
        let r = e.route("/v1/models", &HeaderMap::new(), None, b"").await;
        assert_eq!(r.status, 200);
        assert!(e.http.seen.lock().is_empty());
    }
}
