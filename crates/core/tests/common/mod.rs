#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Json;
use axum::routing::{get, post};
use axum::Router;
use futures_util::stream;
use serde_json::{json, Value};
use tokenshare_core::{serve, AppState, Config};
use tokio::net::TcpListener;

pub async fn serve_plain(app: Router) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

/// Sets a flag when dropped; lets a mock observe the client hanging up.
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub struct Mock {
    pub url: String,
    /// True once a `/api/chat` response stream has been dropped mid-way.
    pub chat_dropped: Arc<AtomicBool>,
}

/// A fake ollama serving `models`. `/api/chat` streams 3 NDJSON lines
/// (`chunk_delay_ms` apart) and `/api/generate` sends one line then fails.
pub async fn mock_ollama(models: &[&str], chunk_delay_ms: u64) -> Mock {
    let tags: Vec<Value> = models
        .iter()
        .map(|m| json!({"name": m, "size": 1}))
        .collect();
    let dropped = Arc::new(AtomicBool::new(false));
    let dropped2 = dropped.clone();
    let app = Router::new()
        .route(
            "/api/tags",
            get(move || {
                let tags = tags.clone();
                async move { Json(json!({ "models": tags })) }
            }),
        )
        .route(
            "/api/chat",
            post(move |Json(req): Json<Value>| {
                let dropped = dropped2.clone();
                async move {
                    let model = req["model"].as_str().unwrap().to_string();
                    let state = (0, DropFlag(dropped));
                    let s = stream::unfold(state, move |(i, flag)| {
                        let model = model.clone();
                        async move {
                            if i == 3 {
                                // Finished normally: don't report a drop.
                                std::mem::forget(flag);
                                return None;
                            }
                            tokio::time::sleep(Duration::from_millis(chunk_delay_ms)).await;
                            let line = format!(
                                "{}\n",
                                json!({"model": model, "chunk": i, "done": i == 2})
                            );
                            Some((Ok::<_, std::io::Error>(line), (i + 1, flag)))
                        }
                    });
                    Body::from_stream(s)
                }
            }),
        )
        .route(
            "/api/generate",
            post(|| async {
                // Headers and one chunk go out, then the connection fails.
                let s = stream::unfold(0, |i| async move {
                    match i {
                        0 => Some((Ok::<_, std::io::Error>("{\"chunk\":0}\n".to_string()), 1)),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Some((Err(std::io::Error::other("upstream died")), 2))
                        }
                        _ => None,
                    }
                });
                Body::from_stream(s)
            }),
        )
        .route(
            "/api/version",
            get(|| async { Json(json!({"version": "mock"})) }),
        );
    Mock {
        url: serve_plain(app).await,
        chat_dropped: dropped,
    }
}

pub struct Node {
    pub url: String,
    pub state: Arc<AppState>,
    pub mock: Mock,
    addr: std::net::SocketAddr,
    server: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    )>,
}

fn start_server(
    l: TcpListener,
    state: Arc<AppState>,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let _ = serve(l, state, async move {
            // Only an explicit `kill` stops the server; dropping the Node doesn't.
            if rx.await.is_err() {
                std::future::pending::<()>().await;
            }
        })
        .await;
    });
    (tx, handle)
}

impl Node {
    /// Stop serving: the port refuses connections afterwards.
    pub async fn kill(&mut self) {
        if let Some((tx, handle)) = self.server.take() {
            let _ = tx.send(());
            let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
        }
    }

    /// Serve again on the same address with the same state.
    pub async fn restart(&mut self) {
        assert!(self.server.is_none(), "node is running");
        for _ in 0..50 {
            if let Ok(l) = TcpListener::bind(self.addr).await {
                self.server = Some(start_server(l, self.state.clone()));
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("could not rebind {}", self.addr);
    }
}

pub struct NodeOpts<'a> {
    pub models: &'a [&'a str],
    pub fc: Option<&'a str>,
    pub pool: usize,
    pub chunk_delay_ms: u64,
    pub data_dir: Option<std::path::PathBuf>,
    pub id: Option<String>,
    pub keepalive_interval_secs: Option<u64>,
    /// Let peers use this node's ollama (default true).
    pub share: bool,
}

impl Default for NodeOpts<'_> {
    fn default() -> Self {
        Self {
            models: &[],
            fc: None,
            pool: 3,
            chunk_delay_ms: 0,
            data_dir: None,
            id: None,
            keepalive_interval_secs: None,
            share: true,
        }
    }
}

/// Start a delegate (and its mock ollama) on a free port. It does not join
/// the mesh until the test calls `mesh::rejoin`.
pub async fn node(opts: NodeOpts<'_>) -> Node {
    let mock = mock_ollama(opts.models, opts.chunk_delay_ms).await;
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let config = Config {
        port: addr.port(),
        fc: opts.fc.map(str::to_string),
        ollama_hosts: vec![mock.url.clone()],
        pool_size: opts.pool,
        share_with_peers: opts.share,
        keepalive_interval_secs: opts.keepalive_interval_secs.unwrap_or(30),
        ..Config::default()
    };
    let state = AppState::new(opts.id.unwrap_or_else(uuid_like), config, opts.data_dir);
    state.refresh_local().await;
    let server = Some(start_server(l, state.clone()));
    Node {
        url: format!("http://{addr}"),
        addr,
        server,
        state,
        mock,
    }
}

pub fn uuid_like() -> String {
    format!("{:016x}", rand_u64())
}

fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    t ^ (N.fetch_add(1, Ordering::SeqCst) << 48)
}

/// Poll `f` until it is true, panicking after `secs`.
pub async fn eventually(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..secs * 50 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for: {what}");
}
