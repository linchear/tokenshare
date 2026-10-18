use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tokenshare_core::config::{default_config_path, default_data_dir, load_or_create_id};
use tokenshare_core::{poll_local_table, router, AppState, Config};
use tracing::info;

#[derive(Parser)]
#[command(version, about = "Route ollama calls amongst several hosts")]
struct Args {
    /// Config file (TOML). Defaults to the per-user config dir.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Listen port (default 11343).
    #[arg(long)]
    port: Option<u16>,
    /// First-contact delegate URL.
    #[arg(long)]
    fc: Option<String>,
    /// Ollama host; repeat for several.
    #[arg(long = "ollama-host")]
    ollama_hosts: Vec<String>,
    /// Max concurrent ollama connections (default 3).
    #[arg(long)]
    pool_size: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let mut config = match args.config.or_else(default_config_path) {
        Some(path) => Config::load(&path)?,
        None => Config::default(),
    };
    if let Some(p) = args.port {
        config.port = p;
    }
    if args.fc.is_some() {
        config.fc = args.fc;
    }
    if !args.ollama_hosts.is_empty() {
        config.ollama_hosts = args.ollama_hosts;
    }
    if let Some(n) = args.pool_size {
        config.pool_size = n;
    }

    let data_dir = default_data_dir().context("no home directory for data dir")?;
    let id = load_or_create_id(&data_dir)?;

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port))
        .await
        .with_context(|| format!("binding port {}", config.port))?;
    info!(id, port = config.port, hosts = ?config.ollama_hosts, pool = config.pool_size, "delegate starting");

    let state = AppState::new(id, config);
    state.table.refresh(&state.client).await;
    tokio::spawn(poll_local_table(state.clone()));

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    info!("shutting down");
}
