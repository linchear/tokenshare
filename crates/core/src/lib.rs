pub mod config;
pub mod daemon;
pub mod mesh;
pub mod ollama;
pub mod pool;
pub mod proxy;
pub mod settings;
pub mod status;
pub mod table;

pub use config::Config;
pub use proxy::{poll_local_table, router, router_for, serve, serve_face, AppState};
