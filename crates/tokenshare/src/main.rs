use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokenshare_core::config::{default_config_path, default_data_dir};
use tokenshare_core::{daemon, Config};
use tracing::info;

mod cli;

#[derive(Parser)]
#[command(version, about = "Route ollama calls amongst several hosts")]
struct Args {
    #[command(subcommand)]
    command: Option<Cmd>,
    /// Config file (TOML). Defaults to the per-user config dir.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Listen port (default 11343). With a command: the port of the delegate to ask.
    #[arg(long, global = true)]
    port: Option<u16>,
    /// First-contact delegate URL.
    #[arg(long)]
    fc: Option<String>,
    /// URL peers use to reach this delegate (learned automatically if unset).
    #[arg(long)]
    advertise_url: Option<String>,
    /// Ollama host; repeat for several.
    #[arg(long = "ollama-host")]
    ollama_hosts: Vec<String>,
    /// Also listen on the ollama port so ollama clients work unchanged.
    #[arg(long)]
    masquerade: bool,
    /// Port for ollama clients when masquerading (default 11434, ollama's own).
    #[arg(long)]
    ollama_port: Option<u16>,
    /// Use no local ollama; only borrow other delegates' (overrides --ollama-host).
    #[arg(long)]
    client_only: bool,
    /// Max concurrent ollama connections (default 3).
    #[arg(long)]
    pool_size: Option<usize>,
}

/// Commands for the delegate already running on this machine. Without one,
/// `tokenshare` runs a delegate.
#[derive(Subcommand)]
enum Cmd {
    /// List the delegates this machine knows about.
    Delegates,
    /// Forget a delegate, for example one that is gone for good. It stays out
    /// until it is heard from again.
    Forget {
        /// Its id, the start of its id (4+ characters), or its URL.
        target: String,
    },
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
    if let Some(cmd) = &args.command {
        return match cmd {
            Cmd::Delegates => cli::list(config.port).await,
            Cmd::Forget { target } => cli::forget(config.port, target).await,
        };
    }
    if args.fc.is_some() {
        config.fc = args.fc;
    }
    if args.advertise_url.is_some() {
        config.advertise_url = args.advertise_url;
    }
    if args.masquerade {
        config.masquerade = true;
    }
    if let Some(p) = args.ollama_port {
        config.ollama_port = p;
    }
    if args.client_only {
        config.ollama_hosts.clear();
    } else if !args.ollama_hosts.is_empty() {
        config.ollama_hosts = args.ollama_hosts;
    }
    if let Some(n) = args.pool_size {
        config.pool_size = n;
    }

    let data_dir = default_data_dir().context("no home directory for data dir")?;
    let running = daemon::start(config, data_dir).await?;
    shutdown_signal().await;
    running.stop_gracefully().await
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
