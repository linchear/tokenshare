//! Starting and stopping a whole delegate: server plus background loops.
//! Shared by the headless daemon and the tray app.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::info;

use crate::config::{load_or_create_id, Config};
use crate::mesh::{join_loop, keepalive_loop, sync_loop};
use crate::proxy::{poll_local_table, serve, AppState};

pub struct Running {
    pub state: Arc<AppState>,
    shutdown: Option<oneshot::Sender<()>>,
    server: JoinHandle<std::io::Result<()>>,
    background: Vec<JoinHandle<()>>,
}

/// Bind the port and start serving. Fails if the port is taken.
pub async fn start(config: Config, data_dir: PathBuf) -> Result<Running> {
    let id = load_or_create_id(&data_dir)?;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port))
        .await
        .with_context(|| format!("binding port {}", config.port))?;
    info!(id, port = config.port, hosts = ?config.ollama_hosts, pool = config.pool_size, "delegate starting");

    let state = AppState::new(id, config, Some(data_dir));
    state.refresh_local().await;
    let background = vec![
        tokio::spawn(poll_local_table(state.clone())),
        tokio::spawn(join_loop(state.clone())),
        tokio::spawn(keepalive_loop(state.clone())),
        tokio::spawn(sync_loop(state.clone())),
    ];
    let (tx, rx) = oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, state.clone(), async move {
        let _ = rx.await;
    }));
    Ok(Running {
        state,
        shutdown: Some(tx),
        server,
        background,
    })
}

impl Running {
    /// Stop accepting connections, let in-flight requests finish, stop the loops.
    pub async fn stop(mut self) -> Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        for h in &self.background {
            h.abort();
        }
        self.server.await.context("server task")??;
        Ok(())
    }
}
