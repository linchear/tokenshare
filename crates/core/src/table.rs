use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::ollama::model_matches;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Online,
    /// Did not answer a recent request; comes back only via a message from it.
    Unavailable,
    /// Could not be contacted when (re)joining.
    Inactive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegateEntry {
    pub id: String,
    /// Empty until the delegate learns how peers reach it.
    pub url: String,
    pub status: Status,
    #[serde(rename = "type")]
    pub kind: String,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram: Option<u64>,
    /// Unix milliseconds. The entry with the later `last_seen` wins a merge.
    pub last_seen: u64,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl DelegateEntry {
    pub fn new(id: String, url: String, models: Vec<String>) -> Self {
        Self {
            id,
            url,
            status: Status::Online,
            kind: "delegate".into(),
            models,
            load: None,
            vram: None,
            ram: None,
            last_seen: now_ms(),
        }
    }
}

/// This delegate's view of every delegate (itself included), persisted to disk.
#[derive(Clone)]
pub struct DelegateTable {
    self_id: String,
    path: Option<PathBuf>,
    inner: Arc<RwLock<HashMap<String, DelegateEntry>>>,
}

impl DelegateTable {
    /// Load the persisted table (if any) and make sure our own entry exists.
    pub fn new(self_id: String, advertise_url: Option<String>, path: Option<PathBuf>) -> Self {
        let mut map: HashMap<String, DelegateEntry> = HashMap::new();
        if let Some(p) = &path {
            match std::fs::read_to_string(p) {
                Ok(text) => match serde_json::from_str::<Vec<DelegateEntry>>(&text) {
                    Ok(entries) => map.extend(entries.into_iter().map(|e| (e.id.clone(), e))),
                    Err(e) => {
                        warn!(path = %p.display(), error = %e, "ignoring unreadable delegate table")
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!(path = %p.display(), error = %e, "cannot read delegate table"),
            }
        }
        let known_url = map.get(&self_id).map(|e| e.url.clone()).unwrap_or_default();
        let url = advertise_url.unwrap_or(known_url);
        map.insert(
            self_id.clone(),
            DelegateEntry::new(self_id.clone(), url, vec![]),
        );
        Self {
            self_id,
            path,
            inner: Arc::new(RwLock::new(map)),
        }
    }

    pub fn self_id(&self) -> &str {
        &self.self_id
    }

    pub fn self_entry(&self) -> DelegateEntry {
        self.inner.read().unwrap()[&self.self_id].clone()
    }

    /// Adopt `url` as our address unless we already have one.
    pub fn set_self_url_if_unknown(&self, url: &str) {
        let changed = {
            let mut g = self.inner.write().unwrap();
            let me = g.get_mut(&self.self_id).unwrap();
            if me.url.is_empty() && !url.is_empty() {
                me.url = url.to_string();
                me.last_seen = now_ms();
                true
            } else {
                false
            }
        };
        if changed {
            self.persist();
        }
    }

    /// Refresh our own entry: models, online, and a new `last_seen`.
    pub fn update_self(&self, models: Vec<String>) {
        {
            let mut g = self.inner.write().unwrap();
            let me = g.get_mut(&self.self_id).unwrap();
            me.models = models;
            me.status = Status::Online;
            me.last_seen = now_ms();
        }
        self.persist();
    }

    /// Entries peers can act on: those with a known URL. Includes ourselves.
    pub fn shared_entries(&self) -> Vec<DelegateEntry> {
        let mut v: Vec<_> = self
            .inner
            .read()
            .unwrap()
            .values()
            .filter(|e| !e.url.is_empty())
            .cloned()
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Every delegate but us that has a known URL.
    pub fn peers(&self) -> Vec<DelegateEntry> {
        self.shared_entries()
            .into_iter()
            .filter(|e| e.id != self.self_id)
            .collect()
    }

    /// Online peers serving `model`, skipping any in `exclude`.
    pub fn peers_with_model(&self, model: &str, exclude: &[String]) -> Vec<DelegateEntry> {
        self.peers()
            .into_iter()
            .filter(|e| {
                e.status == Status::Online
                    && !exclude.contains(&e.id)
                    && e.models.iter().any(|m| model_matches(model, m))
            })
            .collect()
    }

    /// Merge entries from another delegate; the later `last_seen` wins.
    /// For our own entry we only ever learn a URL we were missing.
    /// Returns whether anything changed.
    pub fn merge(&self, entries: impl IntoIterator<Item = DelegateEntry>) -> bool {
        let mut changed = false;
        {
            let mut g = self.inner.write().unwrap();
            for e in entries {
                if e.url.is_empty() {
                    continue;
                }
                match g.get_mut(&e.id) {
                    Some(old) if e.id == self.self_id => {
                        if old.url.is_empty() {
                            old.url = e.url;
                            changed = true;
                        }
                    }
                    Some(old) if old.last_seen >= e.last_seen => {}
                    Some(old) => {
                        *old = e;
                        changed = true;
                    }
                    None => {
                        g.insert(e.id.clone(), e);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.persist();
        }
        changed
    }

    pub fn set_status(&self, id: &str, status: Status) {
        let changed = {
            let mut g = self.inner.write().unwrap();
            match g.get_mut(id) {
                Some(e) if e.status != status && id != self.self_id => {
                    e.status = status;
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.persist();
        }
    }

    pub fn id_for_url(&self, url: &str) -> Option<String> {
        self.peers()
            .into_iter()
            .find(|e| e.url == url)
            .map(|e| e.id)
    }

    /// Hash of membership (ids and URLs), stable across delegates. Deliberately
    /// ignores `last_seen`, status and models, which change constantly.
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        for e in self.shared_entries() {
            h.update(e.id.as_bytes());
            h.update(b"\n");
            h.update(e.url.as_bytes());
            h.update(b"\n");
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn count(&self) -> usize {
        self.shared_entries().len()
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let entries: Vec<DelegateEntry> = {
            let g = self.inner.read().unwrap();
            let mut v: Vec<_> = g.values().cloned().collect();
            v.sort_by(|a, b| a.id.cmp(&b.id));
            v
        };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&entries)?)?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            warn!(path = %path.display(), error = %e, "cannot persist delegate table");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, last_seen: u64) -> DelegateEntry {
        let mut e = DelegateEntry::new(id.into(), format!("http://{id}:1"), vec![]);
        e.last_seen = last_seen;
        e
    }

    fn table() -> DelegateTable {
        DelegateTable::new("me".into(), Some("http://me:1".into()), None)
    }

    #[test]
    fn later_last_seen_wins() {
        let t = table();
        t.merge([entry("a", 10)]);
        let mut newer = entry("a", 20);
        newer.models = vec!["m".into()];
        assert!(t.merge([newer]));
        assert_eq!(t.peers()[0].models, vec!["m".to_string()]);
        assert!(!t.merge([entry("a", 5)]));
        assert_eq!(t.peers()[0].last_seen, 20);
    }

    #[test]
    fn own_entry_is_not_overwritten() {
        let t = table();
        let mut theirs = entry("me", u64::MAX);
        theirs.models = vec!["bogus".into()];
        t.merge([theirs]);
        assert!(t.self_entry().models.is_empty());
    }

    #[test]
    fn hash_ignores_volatile_fields_and_order() {
        let a = table();
        let b = DelegateTable::new("me".into(), Some("http://me:1".into()), None);
        a.merge([entry("x", 1), entry("y", 1)]);
        b.merge([entry("y", 99), entry("x", 50)]);
        assert_eq!(a.hash(), b.hash());
        b.merge([entry("z", 1)]);
        assert_ne!(a.hash(), b.hash());
        assert_eq!(a.count(), 3);
    }

    #[test]
    fn unknown_url_is_not_shared() {
        let t = DelegateTable::new("me".into(), None, None);
        assert_eq!(t.count(), 0);
        t.set_self_url_if_unknown("http://me:1");
        assert_eq!(t.count(), 1);
    }

    #[test]
    fn persists_and_reloads() {
        let path = std::env::temp_dir().join(format!("ts-table-{}.json", uuid::Uuid::new_v4()));
        let t = DelegateTable::new("me".into(), Some("http://me:1".into()), Some(path.clone()));
        t.merge([entry("a", 10)]);
        let t2 = DelegateTable::new("me".into(), None, Some(path.clone()));
        assert_eq!(t2.peers().len(), 1);
        assert_eq!(t2.self_entry().url, "http://me:1");
        std::fs::remove_file(path).ok();
    }
}
