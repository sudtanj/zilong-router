//! Process configuration: CLI flags, environment variables and an optional TOML file.
//!
//! Precedence: CLI flag > environment variable > config file > default.
//! Environment variable names follow 9router where one existed (`PORT`, `DATA_DIR`,
//! `INITIAL_PASSWORD`, `JWT_SECRET`, `API_KEY_SECRET`, `MACHINE_ID_SALT`,
//! `REQUIRE_API_KEY`, `STREAM_STALL_TIMEOUT_MS`, `FETCH_CONNECT_TIMEOUT_MS`).

use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;

#[derive(Parser, Debug, Clone)]
#[command(name = "zilong-router")]
pub struct Config {
    /// Address to bind. Use 0.0.0.0 to serve the LAN.
    #[arg(long, env = "HOSTNAME", default_value = "127.0.0.1")]
    pub host: String,

    /// TCP port.
    #[arg(long, short, env = "PORT", default_value_t = 20128)]
    pub port: u16,

    /// Data directory (SQLite database, secrets). Defaults to ~/.9router so an
    /// existing 9router data dir keeps working.
    #[arg(long, env = "DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Optional TOML config file.
    #[arg(long, short, env = "ZILONG_ROUTER_CONFIG")]
    pub config: Option<PathBuf>,

    /// Password accepted for the dashboard until one is saved in settings.
    #[arg(long, env = "INITIAL_PASSWORD", default_value = "123456", hide_env_values = true)]
    pub initial_password: String,

    /// Secret used to sign dashboard session tokens. Generated and persisted to
    /// `<data_dir>/jwt-secret` when unset.
    #[arg(long, env = "JWT_SECRET", hide_env_values = true)]
    pub jwt_secret: Option<String>,

    /// HMAC secret for the API-key checksum segment.
    #[arg(long, env = "API_KEY_SECRET", default_value = "endpoint-proxy-api-key-secret", hide_env_values = true)]
    pub api_key_secret: String,

    /// Salt for the machine id embedded in API keys.
    #[arg(long, env = "MACHINE_ID_SALT", default_value = "endpoint-proxy-salt", hide_env_values = true)]
    pub machine_id_salt: String,

    /// Require a valid API key even from loopback clients.
    #[arg(long, env = "REQUIRE_API_KEY", default_value_t = false, value_parser = clap::builder::BoolishValueParser::new())]
    pub require_api_key: bool,

    /// Tokio worker threads.
    #[arg(long, env = "ZILONG_ROUTER_WORKERS", default_value_t = 2)]
    pub workers: usize,

    /// Log filter (tracing EnvFilter syntax) when RUST_LOG is unset.
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Abort an upstream request if headers do not arrive within this time.
    #[arg(long, env = "FETCH_CONNECT_TIMEOUT_MS", default_value_t = 60_000)]
    pub connect_timeout_ms: u64,

    /// Abort a stream after this much silence between chunks.
    #[arg(long, env = "STREAM_STALL_TIMEOUT_MS", default_value_t = 360_000)]
    pub stall_timeout_ms: u64,

    /// Maximum accepted request body (bytes).
    #[arg(long, env = "MAX_BODY_BYTES", default_value_t = 64 * 1024 * 1024)]
    pub max_body_bytes: usize,

    /// Keep at most this many rows in requestDetails / usageHistory pruning.
    #[arg(long, env = "OBSERVABILITY_MAX_RECORDS", default_value_t = 1000)]
    pub max_request_details: usize,

    /// Outbound HTTP(S) proxy for upstream calls (http://, https:// or socks5://).
    #[arg(long, env = "HTTPS_PROXY")]
    pub outbound_proxy: Option<String>,

    /// Turso (libSQL) database URL, e.g. libsql://name-org.turso.io. When set, the local
    /// SQLite file is mirrored there and restored from it if it starts out empty.
    #[arg(long, env = "TURSO_DATABASE_URL")]
    pub turso_url: Option<String>,

    /// Turso auth token (`turso db tokens create <db>`).
    #[arg(long, env = "TURSO_AUTH_TOKEN", hide_env_values = true)]
    pub turso_token: Option<String>,

    /// Keep no database file on disk: run from memory and treat Turso as the only
    /// durable copy (restored at start-up, pushed every sync interval).
    #[arg(long, env = "TURSO_ONLY", default_value_t = false, value_parser = clap::builder::BoolishValueParser::new())]
    pub turso_only: bool,

    /// Seconds between pushes to Turso.
    #[arg(long, env = "TURSO_SYNC_SECS", default_value_t = 60)]
    pub turso_sync_secs: u64,

    /// Stop an MCP server's process (or drop its HTTP session) after this many idle
    /// seconds; 0 keeps them running.
    #[arg(long, env = "MCP_IDLE_SECS", default_value_t = 300)]
    pub mcp_idle_secs: u64,

    #[arg(skip)]
    pub resolved_data_dir: PathBuf,
}

/// Subset of fields that may be set from the TOML file.
#[derive(Deserialize, Default, Debug)]
struct FileConfig {
    host: Option<String>,
    port: Option<u16>,
    data_dir: Option<PathBuf>,
    initial_password: Option<String>,
    jwt_secret: Option<String>,
    api_key_secret: Option<String>,
    machine_id_salt: Option<String>,
    require_api_key: Option<bool>,
    workers: Option<usize>,
    log_level: Option<String>,
    connect_timeout_ms: Option<u64>,
    stall_timeout_ms: Option<u64>,
    outbound_proxy: Option<String>,
    turso_url: Option<String>,
    turso_token: Option<String>,
}

impl Config {
    /// Merge the config file (if any), resolve the data dir and create it.
    pub fn finalize(mut self) -> anyhow::Result<Self> {
        if let Some(path) = &self.config {
            let text =
                std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
            let f: FileConfig = toml::from_str(&text)?;
            // CLI/env values win because clap already applied them; only fill the
            // fields the user left at their defaults.
            let d = Config::parse_from(["zilong-router"]);
            macro_rules! take {
                ($field:ident) => {
                    if let Some(v) = f.$field {
                        if self.$field == d.$field {
                            self.$field = v;
                        }
                    }
                };
            }
            take!(host);
            take!(port);
            take!(initial_password);
            take!(api_key_secret);
            take!(machine_id_salt);
            take!(workers);
            take!(log_level);
            take!(connect_timeout_ms);
            take!(stall_timeout_ms);
            if let Some(v) = f.require_api_key {
                self.require_api_key = self.require_api_key || v;
            }
            if self.data_dir.is_none() {
                self.data_dir = f.data_dir;
            }
            if self.jwt_secret.is_none() {
                self.jwt_secret = f.jwt_secret;
            }
            if self.turso_url.is_none() {
                self.turso_url = f.turso_url;
            }
            if self.turso_token.is_none() {
                self.turso_token = f.turso_token;
            }
            if self.outbound_proxy.is_none() {
                self.outbound_proxy = f.outbound_proxy;
            }
        }
        if self.workers == 0 {
            self.workers = 1;
        }

        let unset = |v: &Option<String>| v.as_deref().map_or(true, |s| s.trim().is_empty());
        if self.turso_only && (unset(&self.turso_url) || unset(&self.turso_token)) {
            anyhow::bail!("TURSO_ONLY needs TURSO_DATABASE_URL and TURSO_AUTH_TOKEN");
        }
        self.resolved_data_dir = match &self.data_dir {
            Some(d) => d.clone(),
            None => default_data_dir(),
        };
        std::fs::create_dir_all(self.resolved_data_dir.join("db"))?;
        Ok(self)
    }

    pub fn db_path(&self) -> PathBuf {
        if self.turso_only {
            return PathBuf::from(":memory:");
        }
        self.resolved_data_dir.join("db").join("data.sqlite")
    }

    /// Days of usage history kept: short when the database lives in RAM.
    pub fn usage_keep_days(&self) -> i64 {
        if self.turso_only {
            30
        } else {
            180
        }
    }
}

fn default_data_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h).join(".9router");
    }
    if let Some(h) = std::env::var_os("APPDATA") {
        return PathBuf::from(h).join("9router");
    }
    PathBuf::from("./data")
}
