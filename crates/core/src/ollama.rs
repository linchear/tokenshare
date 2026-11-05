use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, warn};

use crate::table::ModelMeta;

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    /// The entry exactly as ollama reported it in `/api/tags`.
    pub raw: Value,
}

impl ModelInfo {
    /// What peers are told about this model.
    pub fn meta(&self) -> ModelMeta {
        ModelMeta::from_tags_entry(&self.name, &self.raw)
    }
}

#[derive(Debug, Clone)]
pub struct HostEntry {
    pub host: String,
    pub up: bool,
    pub models: Vec<ModelInfo>,
    /// Models loaded in memory right now (`/api/ps`), so a request starts
    /// without a load. Empty when unknown.
    pub loaded: Vec<String>,
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
                loaded: vec![],
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
        let results = futures_util::future::join_all(hosts.iter().map(|h| async move {
            let models = fetch_models(client, h).await;
            // Which models are warm is a hint for routing; an ollama that cannot
            // say (an older one, or a stand-in) is simply treated as all cold.
            let loaded = match &models {
                Ok(_) => fetch_loaded(client, h).await.unwrap_or_default(),
                Err(_) => vec![],
            };
            (models, loaded)
        }))
        .await;
        let mut guard = self.inner.write().unwrap();
        for (entry, (result, loaded)) in guard.iter_mut().zip(results) {
            match result {
                Ok(models) => {
                    debug!(host = %entry.host, models = models.len(), loaded = loaded.len(), "ollama host up");
                    entry.up = true;
                    entry.models = models;
                    entry.loaded = loaded;
                }
                Err(e) => {
                    if entry.up {
                        warn!(host = %entry.host, error = %e, "ollama host down");
                    }
                    entry.up = false;
                    entry.models.clear();
                    entry.loaded.clear();
                }
            }
        }
    }

    /// A host that has `model`, preferring one where it is already loaded.
    pub fn host_for_model(&self, model: &str) -> Option<String> {
        let g = self.inner.read().unwrap();
        let has = |e: &&HostEntry| e.up && e.models.iter().any(|m| model_matches(model, &m.name));
        let warm = |e: &&HostEntry| e.loaded.iter().any(|m| model_matches(model, m));
        g.iter()
            .filter(has)
            .find(warm)
            .or_else(|| g.iter().find(has))
            .map(|e| e.host.clone())
    }

    /// Models loaded in memory on any host, deduplicated.
    pub fn loaded_models(&self) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        for e in self.inner.read().unwrap().iter().filter(|e| e.up) {
            for m in &e.loaded {
                if !out.contains(m) {
                    out.push(m.clone());
                }
            }
        }
        out
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

/// `unix_ms` as an RFC 3339 UTC timestamp, e.g. `2026-10-18T13:51:26Z`.
pub fn rfc3339_utc(unix_ms: u64) -> String {
    let secs = unix_ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// An `/api/tags` entry for a model that lives on a peer, from what the peer
/// published about it. Clients index into every field (`ollama ls` slices the
/// digest and panics if it is empty), so all are filled in. For a peer that
/// published only the name (one older than model info), the digest is a
/// stable hash of the name, a stand-in rather than the real content digest,
/// and the size is unknown (0). The time is when the peer was last heard from.
pub fn peer_model_entry(name: &str, meta: Option<&ModelMeta>, last_seen_ms: u64) -> Value {
    use sha2::{Digest, Sha256};
    let digest = meta.and_then(|m| m.digest_key()).unwrap_or_else(|| {
        Sha256::digest(format!("tokenshare:{name}"))
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    });
    let d = meta.map(|m| m.details.clone()).unwrap_or_default();
    serde_json::json!({
        "name": name,
        "model": name,
        "modified_at": rfc3339_utc(last_seen_ms),
        "size": meta.and_then(|m| m.size).unwrap_or(0),
        "digest": digest,
        "details": {
            "parent_model": d.parent_model,
            "format": d.format,
            "family": d.family,
            "families": (!d.families.is_empty()).then_some(d.families),
            "parameter_size": d.parameter_size,
            "quantization_level": d.quantization_level,
        },
    })
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

/// Names of the models `host` has loaded in memory, from `/api/ps`.
async fn fetch_loaded(client: &reqwest::Client, host: &str) -> anyhow::Result<Vec<String>> {
    let body: Value = client
        .get(format!("{host}/api/ps"))
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
                .filter_map(|m| m["name"].as_str().or_else(|| m["model"].as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400_000), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(rfc3339_utc(1_792_331_486_000), "2026-10-18T13:51:26Z");
        assert_eq!(rfc3339_utc(4_102_444_799_999), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn peer_entries_have_every_field_clients_index_into() {
        let e = peer_model_entry("qwen3:14b", None, 1_792_331_486_000);
        assert_eq!(e["name"], "qwen3:14b");
        assert_eq!(e["model"], "qwen3:14b");
        assert!(
            e["digest"].as_str().unwrap().len() >= 12,
            "ollama ls slices the first 12"
        );
        assert_eq!(
            e["digest"],
            peer_model_entry("qwen3:14b", None, 1)["digest"],
            "stable per name"
        );
        assert_ne!(
            e["digest"],
            peer_model_entry("llama3:8b", None, 1)["digest"]
        );
        assert_eq!(e["modified_at"], "2026-10-18T13:51:26Z");
        assert_eq!(e["size"], 0);
        assert!(e["details"].is_object());
        assert_eq!(e["details"]["families"], Value::Null);
    }

    #[test]
    fn peer_entries_carry_what_the_peer_published() {
        let meta = ModelMeta::from_tags_entry(
            "qwen3:14b",
            &serde_json::json!({
                "size": 9_276_198_565u64,
                "digest": "sha256:BB7E9A8F1C0C2D3E4F5061728394A5B6C7D8E9F00112233445566778899AABB",
                "details": {"family": "qwen3", "families": ["qwen3"],
                            "parameter_size": "14.8B", "quantization_level": "Q4_K_M"},
            }),
        );
        let e = peer_model_entry("qwen3:14b", Some(&meta), 1);
        assert_eq!(e["size"], 9_276_198_565u64);
        assert_eq!(
            e["digest"], "bb7e9a8f1c0c2d3e4f5061728394a5b6c7d8e9f00112233445566778899aabb",
            "written as ollama writes its own"
        );
        assert_eq!(e["details"]["parameter_size"], "14.8B");
        assert_eq!(e["details"]["quantization_level"], "Q4_K_M");
        assert_eq!(e["details"]["families"], serde_json::json!(["qwen3"]));
        assert_eq!(e["details"]["format"], "", "still present when unknown");
    }

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
