//! Command line: `zilong-router` serves; subcommands manage accounts, client API keys
//! and model aliases directly in the SQLite file, mirroring the dashboard.

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use crate::config::Config;
use crate::db::{Db, ProviderConnection};
use crate::providers::Registry;

#[derive(Parser, Debug)]
#[command(name = "zilong-router", version, about = "Lightweight AI API router")]
pub struct Cli {
    #[command(flatten)]
    pub cfg: Config,
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Run the server (default when no subcommand is given).
    Serve,
    /// List known providers and how many accounts each has.
    Providers {
        /// Also print each provider's model ids.
        #[arg(long)]
        models: bool,
    },
    /// Manage provider accounts (API keys for upstream providers).
    #[command(subcommand)]
    Connection(ConnCmd),
    /// Manage client API keys (what your coding tool sends to zilong-router).
    #[command(subcommand)]
    Apikey(KeyCmd),
    /// Manage model aliases ("fast" -> "groq/llama-3.3-70b-versatile").
    #[command(subcommand)]
    Alias(AliasCmd),
    /// Read or change settings (stored as JSON).
    Settings {
        /// `key=value` pairs; values are parsed as JSON when possible.
        #[arg(value_name = "KEY=VALUE")]
        set: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConnCmd {
    /// Add an account: `connection add deepseek sk-xxx --name work`.
    Add {
        /// Provider id or alias (see `providers`), or openai-compatible / anthropic-compatible.
        provider: String,
        /// The upstream API key.
        api_key: String,
        #[arg(long)]
        name: Option<String>,
        /// Lower runs first under fill-first (default 999).
        #[arg(long)]
        priority: Option<i64>,
        /// Base URL for openai-compatible / anthropic-compatible endpoints
        /// (e.g. http://ollama:11434/v1 or https://gateway.example.com/anthropic/v1).
        #[arg(long)]
        base_url: Option<String>,
        /// Model prefix routed to this endpoint (e.g. `local` for `local/qwen3`).
        #[arg(long)]
        prefix: Option<String>,
    },
    List,
    Remove {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    /// Clear cooldown locks and error state on one account (or all with `--all`).
    Unlock {
        id: Option<String>,
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum KeyCmd {
    Create {
        #[arg(long)]
        name: Option<String>,
    },
    List,
    Revoke {
        id_or_key: String,
    },
    Enable {
        id_or_key: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum AliasCmd {
    Set { alias: String, target: String },
    List,
    Remove { alias: String },
}

/// Returns Ok(true) when a subcommand ran and the process should exit.
pub fn run(cli: &Cli) -> anyhow::Result<bool> {
    let Some(cmd) = &cli.cmd else { return Ok(false) };
    if matches!(cmd, Cmd::Serve) {
        return Ok(false);
    }
    let cfg = &cli.cfg;
    if cfg.turso_only {
        anyhow::bail!("TURSO_ONLY keeps the database in memory; manage it from the dashboard instead of the CLI");
    }
    let db = Db::open(&cfg.db_path())?;
    let registry = Registry::load(Some(&cfg.resolved_data_dir.join("providers.json")))?;

    match cmd {
        Cmd::Serve => unreachable!(),
        Cmd::Providers { models } => {
            let conns = db.all_connections()?;
            let mut list: Vec<_> = registry.all().iter().collect();
            list.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.id.cmp(&b.id)));
            println!("{:<24} {:<10} {:<8} {}", "ID", "FORMAT", "ACCTS", "NAME");
            for p in list {
                let n = conns.iter().filter(|c| c.provider == p.id).count();
                let fmt = p.transport.as_ref().map(|t| t.format.as_str()).unwrap_or("-");
                println!("{:<24} {:<10} {:<8} {}", p.id, fmt, n, p.name());
                if *models {
                    for m in &p.models {
                        println!("    {}/{}", p.alias, m.id);
                    }
                }
            }
        }
        Cmd::Connection(c) => match c {
            ConnCmd::Add { provider, api_key, name, priority, base_url, prefix } => {
                let conn = crate::dashboard::api::new_connection(
                    &registry,
                    &crate::dashboard::api::NewConnection {
                        provider: provider.clone(),
                        api_key: api_key.clone(),
                        name: name.clone(),
                        priority: *priority,
                        base_url: base_url.clone(),
                        prefix: prefix.clone(),
                    },
                )?;
                db.insert_connection(&conn)?;
                println!("added {} account {}", conn.provider, conn.id);
            }
            ConnCmd::List => {
                println!("{:<38} {:<20} {:<6} {:<8} {}", "ID", "PROVIDER", "ACTIVE", "PRIO", "NAME / STATUS");
                for c in db.all_connections()? {
                    let status = c.data.get("testStatus").and_then(Value::as_str).unwrap_or("ok");
                    let err = c.data.get("lastError").and_then(Value::as_str).unwrap_or("");
                    println!(
                        "{:<38} {:<20} {:<6} {:<8} {} [{}] {}",
                        c.id,
                        c.provider,
                        c.is_active,
                        c.priority.map(|p| p.to_string()).unwrap_or("-".into()),
                        c.label(),
                        status,
                        err
                    );
                }
            }
            ConnCmd::Remove { id } => println!("{}", if db.delete_connection(id)? { "removed" } else { "not found" }),
            ConnCmd::Enable { id } => {
                println!("{}", if db.set_connection_active(id, true)? { "enabled" } else { "not found" })
            }
            ConnCmd::Disable { id } => {
                println!("{}", if db.set_connection_active(id, false)? { "disabled" } else { "not found" })
            }
            ConnCmd::Unlock { id, all } => {
                let targets: Vec<ProviderConnection> = if *all {
                    db.all_connections()?
                } else {
                    let id = id.as_ref().ok_or_else(|| anyhow::anyhow!("give an id or --all"))?;
                    db.get_connection(id)?.into_iter().collect()
                };
                for c in &targets {
                    db.patch_connection_data(&c.id, &Db::unlock_patch(c))?;
                }
                println!("unlocked {} account(s)", targets.len());
            }
        },
        Cmd::Apikey(k) => match k {
            KeyCmd::Create { name } => {
                let key = crate::auth::create_key(cfg, &db, name.clone())?;
                println!("{}", key.key);
            }
            KeyCmd::List => {
                for k in db.api_keys()? {
                    println!("{:<38} {:<6} {:<40} {}", k.id, k.is_active, k.key, k.name.unwrap_or_default());
                }
            }
            KeyCmd::Revoke { id_or_key } => {
                println!("{}", if db.set_api_key_active(id_or_key, false)? { "revoked" } else { "not found" })
            }
            KeyCmd::Enable { id_or_key } => {
                println!("{}", if db.set_api_key_active(id_or_key, true)? { "enabled" } else { "not found" })
            }
        },
        Cmd::Alias(a) => match a {
            AliasCmd::Set { alias, target } => {
                if !target.contains('/') {
                    anyhow::bail!("target must be provider/model");
                }
                db.kv_set("modelAliases", alias, Some(&json!(target)))?;
                println!("{alias} -> {target}");
            }
            AliasCmd::List => {
                for (k, v) in db.kv_scope("modelAliases")? {
                    println!("{k} -> {}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
                }
            }
            AliasCmd::Remove { alias } => {
                db.kv_set("modelAliases", alias, None)?;
                println!("removed");
            }
        },
        Cmd::Settings { set } => {
            if set.is_empty() {
                println!("{}", serde_json::to_string_pretty(&redact(db.settings()))?);
            } else {
                let mut patch = serde_json::Map::new();
                for kv in set {
                    let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("expected KEY=VALUE, got {kv}"))?;
                    let val = serde_json::from_str::<Value>(v).unwrap_or_else(|_| Value::String(v.to_string()));
                    patch.insert(k.to_string(), val);
                }
                let saved = db.save_settings(&Value::Object(patch))?;
                println!("{}", serde_json::to_string_pretty(&redact(saved))?);
            }
        }
    }
    Ok(true)
}

fn redact(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        if o.contains_key("password") {
            o.insert("password".into(), json!("<set>"));
        }
    }
    v
}
