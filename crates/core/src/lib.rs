pub mod config;
pub mod mesh;
pub mod ollama;
pub mod pool;
pub mod proxy;
pub mod table;

pub use config::Config;
pub use proxy::{poll_local_table, router, serve, AppState};
