//! Shared application state and the periodic housekeeping task.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::config::Config;
use crate::db::Db;
use crate::pricing::Table;
use crate::providers::Registry;
use crate::router::RoundRobin;

pub struct AppState {
    pub cfg: Config,
    pub db: Db,
    pub http: reqwest::Client,
    pub registry: Registry,
    pub pricing: Table,
    pub machine_id: String,
    /// Signs dashboard session cookies and the stored password hash.
    pub session_secret: String,
    /// Per-provider round-robin cursor (upstream keeps this in the DB; memory is fine
    /// for a single process and avoids two writes per request).
    pub rr: Mutex<HashMap<String, RoundRobin>>,
    /// Live `/models` lists for accounts without a catalogue (see `discovery`).
    pub model_cache: crate::discovery::Cache,
    /// Mirror health shown on the dashboard (see `turso::Status`).
    pub turso: Mutex<crate::turso::Status>,
    /// Connections to the MCP servers behind `/mcp` (see `mcp`).
    pub mcp: crate::mcp::Gateway,
    pub started_at: Instant,
}

impl AppState {
    pub async fn new(cfg: Config) -> anyhow::Result<AppState> {
        let db = Db::open(&cfg.db_path())?;
        let registry = Registry::load(Some(&cfg.resolved_data_dir.join("providers.json")))?;
        let pricing = Table::load(Some(&cfg.resolved_data_dir.join("pricing.json")))?;
        tracing::info!(
            "loaded {} providers (upstream 9router {} @ {})",
            registry.all().len(),
            registry.meta.upstream_version.as_deref().unwrap_or("?"),
            registry.meta.upstream_commit.as_deref().map(|c| &c[..c.len().min(8)]).unwrap_or("?")
        );

        let mut http = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(cfg.connect_timeout_ms))
            // No overall timeout: streams can legitimately run for minutes. Stall
            // detection lives in the stream pump instead.
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(4)
            .tcp_keepalive(Duration::from_secs(60))
            .user_agent(concat!("zilong-router/", env!("CARGO_PKG_VERSION")))
            .use_rustls_tls();
        if let Some(p) = cfg.outbound_proxy.as_deref().filter(|p| !p.is_empty()) {
            http = http.proxy(reqwest::Proxy::all(p)?);
        }
        let machine_id = crate::auth::machine_id(&cfg);
        let session_secret = crate::dashboard::session_secret(&cfg);
        Ok(AppState {
            cfg,
            db,
            http: http.build()?,
            registry,
            pricing,
            machine_id,
            session_secret,
            rr: Mutex::new(HashMap::new()),
            model_cache: Mutex::new(HashMap::new()),
            turso: Mutex::new(Default::default()),
            mcp: Default::default(),
            started_at: Instant::now(),
        })
    }
}

/// Hourly: WAL checkpoint so the db file stays small on SD cards, drop expired
/// cooldown locks, and prune usage history older than 180 days.
pub async fn housekeeping(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    tick.tick().await;
    loop {
        tick.tick().await;
        state.db.checkpoint();
        if let Err(e) = crate::router::clear_expired_locks(&state) {
            tracing::warn!("lock cleanup failed: {e}");
        }
        match state.db.prune_usage(state.cfg.usage_keep_days()) {
            Ok(n) if n > 0 => tracing::info!("pruned {n} old usage rows"),
            Err(e) => tracing::warn!("usage prune failed: {e}"),
            _ => {}
        }
    }
}
