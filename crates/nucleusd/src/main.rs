mod ai;
mod auth;
mod cron;
mod backups;
mod config;
mod console;
mod docker;
mod files;
mod installer;
mod routes;
mod scheduler;
mod sftp_server;
mod state;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::Arc;

#[derive(Parser)]
struct Args {
    /// Path to daemon configuration file
    #[arg(long, default_value = "/etc/nucleus/daemon.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nucleusd=debug".into()),
        )
        .init();

    let args = Args::parse();
    let cfg = config::Config::load(&args.config)?;
    let bind = cfg.bind.clone();
    let data_dir = cfg.data_dir.clone();

    if cfg.data_dir.starts_with(std::path::Path::new("/tmp")) {
        tracing::warn!(
            data_dir = %cfg.data_dir.display(),
            "data_dir is under /tmp — server files will NOT survive a reboot; move data_dir to persistent storage"
        );
    }

    let docker = bollard::Docker::connect_with_local_defaults()
        .map_err(|e| anyhow::anyhow!("connecting to Docker: {e}"))?;

    let (exit_tx, mut exit_rx) = tokio::sync::mpsc::unbounded_channel::<(String, i64)>();
    let st = Arc::new(state::AppState::new(cfg, docker, exit_tx));

    // Restore known servers from the registry.
    for (id, spec) in state::load_registry(&st.cfg) {
        if spec.id != id {
            continue;
        }
        st.servers
            .insert(id, Arc::new(state::ServerRuntime::new(spec)));
    }
    tracing::info!(servers = st.servers.len(), data_dir = %data_dir.display(), "nucleusd starting");

    // Reap ghost containers: install sidecars from a previous run and
    // runtime containers whose server is no longer in the registry.
    docker::reap_unmanaged_containers(&st).await;

    scheduler::spawn(st.clone());

    // Embedded SFTP server (per-server jailed file access).
    if st.cfg.sftp.enabled {
        let sftp_state = st.clone();
        tokio::spawn(async move {
            if let Err(e) = sftp_server::run(sftp_state).await {
                tracing::error!(error = %e, "sftp server stopped");
            }
        });
    }

    // Auto-heal worker: reacts to abnormal container exits.
    let worker_state = st.clone();
    tokio::spawn(async move {
        while let Some((id, code)) = exit_rx.recv().await {
            if !worker_state.cfg.ai.enabled || !worker_state.cfg.ai.auto_heal || code == 0 {
                continue;
            }
            if let Err(e) =
                ai::diagnose(worker_state.clone(), &id, "auto-heal: crashed", None).await
            {
                tracing::warn!(server = %id, error = %e, "auto-heal failed");
            }
        }
    });

    let app = routes::router(st.clone());
    if st.cfg.tls.enabled {
        // The tree enables more than one rustls crypto provider (aws-lc-rs via
        // axum-server, ring via russh), so pick one explicitly.
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("installing rustls provider");
        let cert = st
            .cfg
            .tls
            .cert_path
            .clone()
            .context("tls.enabled requires tls.cert_path")?;
        let key = st
            .cfg
            .tls
            .key_path
            .clone()
            .context("tls.enabled requires tls.key_path")?;
        let rustls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?;
        let addr: std::net::SocketAddr = bind.parse()?;
        tracing::info!(%addr, "nucleusd listening (https)");
        axum_server::bind_rustls(addr, rustls)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(&bind).await?;
        tracing::info!(%bind, "listening");
        axum::serve(listener, app).await?;
    }
    Ok(())
}
