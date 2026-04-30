//! `phantom-hub` binary entry point.
//!
//! Reads `PORT` from the environment (Railway convention) and starts the hub
//! server. Falls back to port 8080 when `PORT` is unset (local development).
//!
//! ```text
//! RUST_LOG=phantom_hub=debug cargo run --bin phantom-hub
//! curl localhost:8080/healthz
//! ```

use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "phantom_hub=info,tower_http=debug".into()),
        )
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8080);

    let addr = format!("0.0.0.0:{port}")
        .parse()
        .with_context(|| format!("parsing bind address 0.0.0.0:{port}"))?;

    phantom_hub::serve(addr).await
}
