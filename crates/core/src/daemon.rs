//! Starting and stopping a whole delegate: server plus background loops.
//! Shared by the headless daemon and the tray app.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::{load_or_create_id, Config, Listener};
use crate::mesh::{join_loop, keepalive_loop, sync_loop};
use crate::proxy::{poll_local_table, serve_face, AppState};

pub struct Running {
    pub state: Arc<AppState>,
    /// The tokenshare (mesh) address we listen on (useful when the port was 0).
    pub addr: SocketAddr,
    /// Every address we listen on, the tokenshare one first.
    pub addrs: Vec<SocketAddr>,
    shutdown: Option<watch::Sender<bool>>,
    servers: Vec<JoinHandle<std::io::Result<()>>>,
    background: Vec<JoinHandle<()>>,
}

impl Running {
    pub fn ports(&self) -> Vec<u16> {
        self.addrs.iter().map(SocketAddr::port).collect()
    }
}

/// Bind the port(s) and start serving. Fails if a port is taken. With
/// masquerade that is ollama's port 11434 for ollama clients plus the
/// tokenshare port for other delegates.
pub async fn start(config: Config, data_dir: PathBuf) -> Result<Running> {
    let listeners = config.listeners();
    start_on(config, data_dir, listeners).await
}

/// Like [`start`], on an explicit set of listeners (the first is the tokenshare
/// one). Port 0 asks the OS for a free port.
pub async fn start_on(
    config: Config,
    data_dir: PathBuf,
    listeners: Vec<Listener>,
) -> Result<Running> {
    let id = load_or_create_id(&data_dir)?;
    if let Some(h) = config.self_loop_hosts().first() {
        let ports: Vec<String> = listeners.iter().map(|l| l.port.to_string()).collect();
        anyhow::bail!(
            "ollama host {h} is this delegate itself (it listens on port {}); \
             run ollama on another port and point the host there",
            ports.join(" and ")
        );
    }
    // Check every port before binding any, so a failure leaves nothing half-started.
    for l in &listeners {
        if crate::config::port_in_use(l.port) {
            let hint = if l.port == crate::config::OLLAMA_PORT {
                " (is ollama running? it uses this port by default)"
            } else {
                ""
            };
            anyhow::bail!("port {} is already in use{hint}", l.port);
        }
    }
    let mut bound = vec![];
    for l in &listeners {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", l.port))
            .await
            .with_context(|| format!("binding port {}", l.port))?;
        bound.push((listener, l.face));
    }
    let addrs: Vec<SocketAddr> = bound
        .iter()
        .map(|(l, _)| l.local_addr())
        .collect::<std::io::Result<_>>()?;
    info!(
        id,
        ports = ?addrs.iter().map(SocketAddr::port).collect::<Vec<_>>(),
        masquerade = config.masquerade,
        hosts = ?config.ollama_hosts,
        pool = config.pool_size,
        "delegate starting"
    );

    let state = AppState::new(id, config, Some(data_dir));
    state.refresh_local().await;
    let background = vec![
        tokio::spawn(poll_local_table(state.clone())),
        tokio::spawn(join_loop(state.clone())),
        tokio::spawn(keepalive_loop(state.clone())),
        tokio::spawn(sync_loop(state.clone())),
    ];
    let (tx, rx) = watch::channel(false);
    let servers = bound
        .into_iter()
        .map(|(listener, face)| {
            let mut rx = rx.clone();
            tokio::spawn(serve_face(listener, state.clone(), face, async move {
                let _ = rx.wait_for(|stop| *stop).await;
            }))
        })
        .collect();
    Ok(Running {
        state,
        addr: addrs[0],
        addrs,
        shutdown: Some(tx),
        servers,
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
            let _ = tx.send(true);
        }
        // Graceful shutdown waits for open connections; don't wait on stragglers.
        let wait = if cut_off > 0 {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(5)
        };
        for server in &mut self.servers {
            match tokio::time::timeout(wait, &mut *server).await {
                Ok(joined) => joined.context("server task")??,
                Err(_) => server.abort(),
            }
        }
        Ok(())
    }

    /// `stop` using the configured grace period.
    pub async fn stop_gracefully(self) -> Result<()> {
        let grace = Duration::from_secs(self.state.config.shutdown_grace_secs);
        self.stop(grace).await
    }
}
