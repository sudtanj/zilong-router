//! zilong-router as a library.
//!
//! The portable core (provider registry and model resolution, request / response /
//! stream translation, SSE parsing, account selection and fallback rules, pricing and
//! usage accounting, and [`engine`], which ties them together) builds for any target,
//! `wasm32-unknown-unknown` included. Network and storage go through the small traits
//! in [`host`], so a Cloudflare Worker can supply `fetch` and a SQL client.
//!
//! The `native` feature (on by default) adds what the binary needs: the axum server,
//! dashboard, CLI, SQLite, OAuth sign-in flows, the MCP gateway and the Turso mirror.

pub mod auth;
pub mod db;
pub mod engine;
pub mod executor;
pub mod host;
pub mod model;
pub mod pricing;
pub mod providers;
pub mod router;
pub mod sse;
pub mod token_saver;
pub mod translator;
pub mod usage;

#[cfg(feature = "native")]
pub mod oauth;
/// Provider request hooks only; the sign-in flows and token refresh need `native`.
#[cfg(not(feature = "native"))]
pub mod oauth {
    pub mod hooks;
    pub mod opencode;
    pub mod util;
}

#[cfg(feature = "native")]
pub mod cli;
#[cfg(feature = "native")]
pub mod config;
#[cfg(feature = "native")]
pub mod console_log;
#[cfg(feature = "native")]
pub mod dashboard;
#[cfg(feature = "native")]
pub mod discovery;
#[cfg(feature = "native")]
pub mod handlers;
#[cfg(feature = "native")]
pub mod icons;
#[cfg(feature = "native")]
pub mod mcp;
#[cfg(feature = "native")]
pub mod state;
#[cfg(feature = "native")]
pub mod turso;
