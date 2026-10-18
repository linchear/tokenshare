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

/// An `/api/tags` entry for a model that lives on a peer. We only know its name,
/// but clients index into the other fields: `ollama ls` slices the digest and
/// panics if it is empty, so every field ollama sends is filled in. The digest
/// is a stable hash of the name (a stand-in, not the real content digest), the
/// size is unknown (0), and the time is when the peer was last heard from.
pub fn peer_model_entry(name: &str, last_seen_ms: u64) -> Value {
    use sha2::{Digest, Sha256};
    let digest: String = Sha256::digest(format!("tokenshare:{name}"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    serde_json::json!({
        "name": name,
        "model": name,
        "modified_at": rfc3339_utc(last_seen_ms),
        "size": 0,
        "digest": digest,
        "details": {
            "parent_model": "",
            "format": "",
            "family": "",
            "families": null,
            "parameter_size": "",
            "quantization_level": "",
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
        let e = peer_model_entry("qwen3:14b", 1_792_331_486_000);
        assert_eq!(e["name"], "qwen3:14b");
        assert_eq!(e["model"], "qwen3:14b");
        assert!(
            e["digest"].as_str().unwrap().len() >= 12,
            "ollama ls slices the first 12"
        );
        assert_eq!(
            e["digest"],
            peer_model_entry("qwen3:14b", 1)["digest"],
            "stable per name"
        );
        assert_ne!(e["digest"], peer_model_entry("llama3:8b", 1)["digest"]);
        assert_eq!(e["modified_at"], "2026-10-18T13:51:26Z");
        assert_eq!(e["size"], 0);
        assert!(e["details"].is_object());
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
