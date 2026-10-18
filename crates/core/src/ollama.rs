use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    /// The entry exactly as ollama reported it in `/api/tags`.
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct HostEntry {
    pub host: String,
    pub up: bool,
    pub models: Vec<ModelInfo>,
}

/// The ollama instances directly reachable from this delegate, and their models.
#[derive(Clone)]
pub struct LocalTable {
    inner: Arc<RwLock<Vec<HostEntry>>>,
}

pub fn normalize_host(host: &str) -> String {
    let h = host.trim().trim_end_matches('/');
    if h.contains("://") {
        h.to_string()
    } else {
        format!("http://{h}")
    }
}

/// `gemma2` matches `gemma2:latest`; otherwise names must be equal.
pub fn model_matches(requested: &str, available: &str) -> bool {
    requested == available
        || (!requested.contains(':') && available == format!("{requested}:latest"))
}

impl LocalTable {
    pub fn new(hosts: &[String]) -> Self {
        let entries = hosts
            .iter()
            .map(|h| HostEntry {
                host: normalize_host(h),
                up: false,
                models: vec![],
            })
            .collect();
        Self {
            inner: Arc::new(RwLock::new(entries)),
        }
    }

    /// Re-query every host's `/api/tags`.
    pub async fn refresh(&self, client: &reqwest::Client) {
        let hosts: Vec<String> = self
            .inner
            .read()
            .unwrap()
            .iter()
            .map(|e| e.host.clone())
            .collect();
        let results =
            futures_util::future::join_all(hosts.iter().map(|h| fetch_models(client, h))).await;
        let mut guard = self.inner.write().unwrap();
        for (entry, result) in guard.iter_mut().zip(results) {
            match result {
                Ok(models) => {
                    debug!(host = %entry.host, models = models.len(), "ollama host up");
                    entry.up = true;
                    entry.models = models;
                }
                Err(e) => {
                    if entry.up {
                        warn!(host = %entry.host, error = %e, "ollama host down");
                    }
                    entry.up = false;
                    entry.models.clear();
                }
            }
        }
    }

    pub fn host_for_model(&self, model: &str) -> Option<String> {
        self.inner
            .read()
            .unwrap()
            .iter()
            .find(|e| e.up && e.models.iter().any(|m| model_matches(model, &m.name)))
            .map(|e| e.host.clone())
    }

    pub fn first_up(&self) -> Option<String> {
        self.inner
            .read()
            .unwrap()
            .iter()
            .find(|e| e.up)
            .map(|e| e.host.clone())
    }

    /// Union of models across all hosts, deduplicated by name.
    pub fn all_models(&self) -> Vec<ModelInfo> {
        let mut out: Vec<ModelInfo> = vec![];
        for e in self.inner.read().unwrap().iter().filter(|e| e.up) {
            for m in &e.models {
                if !out.iter().any(|o| o.name == m.name) {
                    out.push(m.clone());
                }
            }
        }
        out
    }

    pub fn snapshot(&self) -> Vec<HostEntry> {
        self.inner.read().unwrap().clone()
    }
}

async fn fetch_models(client: &reqwest::Client, host: &str) -> anyhow::Result<Vec<ModelInfo>> {
    let body: Value = client
        .get(format!("{host}/api/tags"))
        .timeout(Duration::from_secs(3))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(body["models"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let name = m["name"].as_str().or_else(|| m["model"].as_str())?;
                    Some(ModelInfo {
                        name: name.to_string(),
                        raw: m.clone(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_normalization() {
        assert_eq!(normalize_host("localhost:11434"), "http://localhost:11434");
        assert_eq!(normalize_host("http://a:1/"), "http://a:1");
    }

    #[test]
    fn model_matching() {
        assert!(model_matches("gemma2", "gemma2:latest"));
        assert!(model_matches("gemma2:9b", "gemma2:9b"));
        assert!(!model_matches("gemma2:9b", "gemma2:latest"));
        assert!(!model_matches("gemma", "gemma2:latest"));
    }
}
