use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 11343;
pub const DEFAULT_OLLAMA_HOST: &str = "http://localhost:11434";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Port for both ollama and coordination traffic.
    pub port: u16,
    /// First-contact delegate, e.g. `http://192.168.2.3:11343`.
    pub fc: Option<String>,
    /// Ollama instances reachable directly from this machine.
    pub ollama_hosts: Vec<String>,
    /// Max concurrent requests sent to ollama.
    pub pool_size: usize,
    /// How often to refresh the local model list.
    pub poll_interval_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            fc: None,
            ollama_hosts: vec![DEFAULT_OLLAMA_HOST.to_string()],
            pool_size: 3,
            poll_interval_secs: 10,
        }
    }
}

impl Config {
    /// Load from `path`; a missing file yields defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading config {}", path.display())),
        }
    }
}

fn dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "tokenshare")
}

pub fn default_config_path() -> Option<PathBuf> {
    dirs().map(|d| d.config_dir().join("config.toml"))
}

pub fn default_data_dir() -> Option<PathBuf> {
    dirs().map(|d| d.data_dir().to_path_buf())
}

/// Load this delegate's persisted ID from `dir`, creating it on first run.
pub fn load_or_create_id(dir: &Path) -> Result<String> {
    let path = dir.join("id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, &id).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let c = Config::load(Path::new("/nonexistent/tokenshare.toml")).unwrap();
        assert_eq!(c.port, 11343);
        assert_eq!(c.pool_size, 3);
    }

    #[test]
    fn partial_toml_overrides_defaults() {
        let c: Config = toml::from_str("pool_size = 5\nfc = \"http://a:1\"").unwrap();
        assert_eq!(c.pool_size, 5);
        assert_eq!(c.fc.as_deref(), Some("http://a:1"));
        assert_eq!(c.port, 11343);
    }

    #[test]
    fn id_is_stable_across_loads() {
        let dir = std::env::temp_dir().join(format!("ts-id-{}", uuid::Uuid::new_v4()));
        let a = load_or_create_id(&dir).unwrap();
        let b = load_or_create_id(&dir).unwrap();
        assert_eq!(a, b);
        std::fs::remove_dir_all(dir).ok();
    }
}
