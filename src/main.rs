//! mini-router: a small, single-binary AI API router.
//!
//! One OpenAI / Anthropic / Gemini-style request in, any configured provider out,
//! with multi-account fallback, cooldowns, usage accounting and an embedded dashboard.
//! Designed to idle in ~10 MB of RSS on a 1 GB ARM board.

mod auth;
mod cli;
mod config;
mod console_log;
mod dashboard;
mod db;
mod discovery;
mod executor;
mod handlers;
mod icons;
mod model;
mod oauth;
mod pricing;
mod providers;
mod router;
mod sse;
mod state;
mod token_saver;
mod translator;
mod turso;
mod usage;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::AppState;

fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let cfg = cli.cfg.clone().finalize()?;
    if cli::run(&cli::Cli { cfg: cfg.clone(), cmd: cli.cmd })? {
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(cfg.log_level.clone())))
        .with_target(false)
        .with_writer(|| console_log::Tee)
        .compact()
        .init();

    // Small boards: 2 worker threads is plenty for an I/O-bound proxy and keeps
    // the thread stacks / per-thread allocator arenas small.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.workers)
        .thread_stack_size(1024 * 1024)
        .enable_all()
        .build()?;
    rt.block_on(run(cfg))
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse()?;
    let state = Arc::new(AppState::new(cfg).await?);
    let mirror = turso::init(&state).await.map(|m| Arc::new(tokio::sync::Mutex::new(m)));

    let app: Router = handlers::build_router(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        "mini-router v{} listening on http://{}  (dashboard: /dashboard, API: /v1)",
        env!("CARGO_PKG_VERSION"),
        addr
    );
    if state.cfg.host == "0.0.0.0" || state.cfg.host == "::" {
        tracing::warn!("bound to all interfaces; make sure requireApiKey / a login password is set");
    }

    tokio::spawn(state::housekeeping(state.clone()));
    if let Some(m) = &mirror {
        tokio::spawn(turso::sync_loop(state.clone(), m.clone()));
    }

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    if let Some(m) = &mirror {
        turso::flush(&state, m).await;
    }
    state.db.checkpoint();
    tracing::info!("bye");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
