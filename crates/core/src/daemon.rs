//! Starting and stopping a whole delegate: server plus background loops.
//! Shared by the headless daemon and the tray app.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::{load_or_create_id, Config};
use crate::mesh::{join_loop, keepalive_loop, sync_loop};
use crate::proxy::{poll_local_table, serve, AppState};

pub struct Running {
    pub state: Arc<AppState>,
    /// The address we are listening on (useful when the port was 0).
    pub addr: SocketAddr,
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
    let addr = listener.local_addr()?;
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
        addr,
        shutdown: Some(tx),
        server,
        background,
    })
}

impl Running {
    /// Shut down gracefully: refuse new model requests, wait up to `grace` for
    /// the ones in flight (local or forwarded) to finish, then stop serving.
    /// Anything still streaming after `grace` is cut off.
    pub async fn stop(mut self, grace: Duration) -> Result<()> {
        // Silence our gossip first so peers stop picking us.
        for h in &self.background {
            h.abort();
        }
        self.state.begin_drain();
        let deadline = tokio::time::Instant::now() + grace;
        while self.state.pool.active() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let cut_off = self.state.pool.active();
        if cut_off > 0 {
            warn!(cut_off, "grace period over, cutting off in-flight requests");
            self.state.cut_off_all();
        }
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // Graceful shutdown waits for open connections; don't wait on stragglers.
        let wait = if cut_off > 0 {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(5)
        };
        match tokio::time::timeout(wait, &mut self.server).await {
            Ok(joined) => joined.context("server task")??,
            Err(_) => self.server.abort(),
        }
        Ok(())
    }

    /// `stop` using the configured grace period.
    pub async fn stop_gracefully(self) -> Result<()> {
        let grace = Duration::from_secs(self.state.config.shutdown_grace_secs);
        self.stop(grace).await
    }
}
