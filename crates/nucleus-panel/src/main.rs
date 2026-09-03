mod auth;
mod config;
mod daemon;
mod db;
mod eggs;
mod models;
mod perms;
mod routes;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;

#[derive(Parser)]
struct Args {
    /// Path to panel configuration file
    #[arg(long, default_value = "/etc/nucleus/panel.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // The dep tree pulls in more than one rustls crypto provider (ring via
    // tokio-tungstenite/reqwest, aws-lc-rs via our direct rustls dep), so a
    // provider must be picked explicitly before any TLS client is built.
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nucleus_panel=debug".into()),
        )
        .init();

    let args = Args::parse();
    let cfg = config::Config::load(&args.config)?;
    let bind = cfg.bind.clone();

    let database = db::Db::open(cfg.database.to_str().unwrap_or("panel.db"))?;
    database.migrate()?;
    let seeded = eggs::seed(&database)?;
    if seeded > 0 {
        tracing::info!(eggs = seeded, "bundled eggs seeded");
    }
    tracing::info!(db = %cfg.database.display(), "database ready");

    let app = Arc::new(routes::App {
        cfg: cfg.clone(),
        db: database,
        http: reqwest::Client::builder()
            .user_agent("nucleus-panel/0.1")
            .build()?,
        node_clients: std::sync::Mutex::new(std::collections::HashMap::new()),
    });

    let router = routes::router(app);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "nucleus-panel listening");
    axum::serve(listener, router).await?;
    Ok(())
}
