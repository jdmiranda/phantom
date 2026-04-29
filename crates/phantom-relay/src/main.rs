//! `phantom-relay` binary — stateless WebSocket message broker.
//!
//! ## Usage
//!
//! ```text
//! phantom-relay [--config <path>]
//! ```
//!
//! Defaults to `config.toml` in the current directory when no path is given.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use log::info;
use serde::Deserialize;
use tokio::sync::Mutex;

use phantom_relay::router::Router;
use phantom_relay::server;

// ── Operator config ───────────────────────────────────────────────────────────

/// Relay operator configuration loaded from `config.toml`.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address and port to bind the WebSocket listener to.
    pub bind: String,
    /// Maximum simultaneously connected peers (hard cap).
    pub max_peers: usize,
    /// Per-peer token-bucket fill rate (messages per second).
    pub rate_limit_per_peer_per_sec: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7700".into(),
            max_peers: 1_000,
            rate_limit_per_peer_per_sec: 100,
        }
    }
}

impl Config {
    /// Load from a TOML file at `path`, falling back to defaults on missing file.
    fn load(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            info!("config file {:?} not found — using defaults", path);
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {:?}", path))?;
        toml::from_str(&raw).with_context(|| format!("parsing config {:?}", path))
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Very lightweight CLI: just an optional --config flag.
    let config_path = parse_config_flag();
    let config = Config::load(&config_path)?;

    info!(
        "phantom-relay starting — bind={} max_peers={} rate={}/s",
        config.bind, config.max_peers, config.rate_limit_per_peer_per_sec
    );

    let addr = config
        .bind
        .parse()
        .with_context(|| format!("parsing bind address '{}'", config.bind))?;

    let router = Arc::new(Mutex::new(Router::new(
        config.rate_limit_per_peer_per_sec,
        config.max_peers,
    )));

    server::run(addr, router).await
}

fn parse_config_flag() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if (args[i] == "--config" || args[i] == "-c") && i + 1 < args.len() {
            return PathBuf::from(&args[i + 1]);
        }
        i += 1;
    }
    PathBuf::from("config.toml")
}
