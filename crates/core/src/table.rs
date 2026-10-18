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

/// Where forgotten delegates are kept, next to the table file.
fn forgotten_path(table_path: &std::path::Path) -> PathBuf {
    table_path.with_file_name("forgotten.json")
}

/// This delegate's view of every delegate (itself included), persisted to disk.
#[derive(Clone)]
pub struct DelegateTable {
    self_id: String,
    path: Option<PathBuf>,
    inner: Arc<RwLock<HashMap<String, DelegateEntry>>>,
    /// Entries whose own `last_seen` is older than this are dropped and refused,
    /// so a dead delegate ages out of the whole mesh. 0 means never.
    expiry_ms: u64,
    /// Delegates the user forgot, with the `last_seen` they had. They stay out
    /// until a newer one arrives, i.e. until they are genuinely heard from again.
    forgotten: Arc<RwLock<HashMap<String, u64>>>,
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
        // Our own address is never taken from disk: it may be stale (a network or
        // port change, a test run). It comes from `advertise_url` or is learned
        // again from the delegate we join.
        let url = advertise_url.unwrap_or_default();
        map.insert(
            self_id.clone(),
            DelegateEntry::new(self_id.clone(), url, vec![]),
        );
        let forgotten = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(forgotten_path(p)).ok())
            .and_then(|t| serde_json::from_str::<HashMap<String, u64>>(&t).ok())
            .unwrap_or_default();
        Self {
            self_id,
            path,
            inner: Arc::new(RwLock::new(map)),
            expiry_ms: 0,
            forgotten: Arc::new(RwLock::new(forgotten)),
        }
    }

    /// Drop (and refuse) entries not heard from in `secs`; 0 disables expiry.
    pub fn with_expiry_secs(mut self, secs: u64) -> Self {
        self.expiry_ms = secs.saturating_mul(1000);
        self
    }

    fn expired(&self, last_seen: u64) -> bool {
        self.expiry_ms > 0 && now_ms().saturating_sub(last_seen) > self.expiry_ms
    }

    /// Remove peers not heard from within the expiry. Only call right after a
    /// successful sync: a table that was saved long ago looks stale everywhere
    /// until fresh data has been merged in. Returns the entries removed.
    pub fn prune(&self) -> Vec<DelegateEntry> {
        if self.expiry_ms == 0 {
            return vec![];
        }
        let removed: Vec<DelegateEntry> = {
            let mut g = self.inner.write().unwrap();
            let dead: Vec<String> = g
                .values()
                .filter(|e| e.id != self.self_id && self.expired(e.last_seen))
                .map(|e| e.id.clone())
                .collect();
            dead.iter().filter_map(|id| g.remove(id)).collect()
        };
        if !removed.is_empty() {
            self.persist();
        }
        removed
    }

    /// Forget a delegate (not ourselves): remove it and keep it out until it is
    /// heard from again. Returns whether there was such a peer.
    pub fn forget(&self, id: &str) -> bool {
        if id == self.self_id {
            return false;
        }
        let Some(e) = self.inner.write().unwrap().remove(id) else {
            return false;
        };
        self.forgotten.write().unwrap().insert(e.id, e.last_seen);
        self.persist();
        self.persist_forgotten();
        true
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

    /// Bump our own `last_seen` (we are alive) without touching anything else.
    pub fn touch_self(&self) {
        {
            let mut g = self.inner.write().unwrap();
            let me = g.get_mut(&self.self_id).unwrap();
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
            let mut forgotten = self.forgotten.write().unwrap();
            for e in entries {
                if e.url.is_empty() {
                    continue;
                }
                if e.id != self.self_id {
                    // Too old to be alive, or forgotten and not heard from since.
                    if self.expired(e.last_seen) {
                        continue;
                    }
                    match forgotten.get(&e.id) {
                        Some(&at) if e.last_seen <= at => continue,
                        Some(_) => {
                            forgotten.remove(&e.id);
                        }
                        None => {}
                    }
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

    fn persist_forgotten(&self) {
        let Some(path) = &self.path else { return };
        let path = forgotten_path(path);
        let map = self.forgotten.read().unwrap().clone();
        let write = || -> std::io::Result<()> {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&map)?)?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            warn!(path = %path.display(), error = %e, "cannot persist forgotten delegates");
        }
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

    fn temp_table_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("ts-table-{}", uuid::Uuid::new_v4()))
            .join("table.json")
    }

    /// Entries are stamped "now" so they are not stale under a real expiry.
    fn fresh(id: &str) -> DelegateEntry {
        entry(id, now_ms())
    }

    #[test]
    fn persists_and_reloads_peers_but_not_our_own_address() {
        let path = temp_table_path();
        let t = DelegateTable::new("me".into(), Some("http://me:1".into()), Some(path.clone()));
        t.merge([entry("a", 10)]);
        // A restart with no advertise_url must not resurrect the old address,
        // which may be wrong by now; it is learned again when we join.
        let t2 = DelegateTable::new("me".into(), None, Some(path.clone()));
        assert_eq!(t2.peers().len(), 1);
        assert_eq!(t2.self_entry().url, "");
        t2.set_self_url_if_unknown("http://100.1.2.3:11343");
        assert_eq!(t2.self_entry().url, "http://100.1.2.3:11343");
        // An explicit advertise_url always wins.
        let t3 = DelegateTable::new("me".into(), Some("http://me:9".into()), Some(path.clone()));
        assert_eq!(t3.self_entry().url, "http://me:9");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_stale_self_url_on_disk_is_ignored() {
        let path = temp_table_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut stale = entry("me", 1);
        stale.url = "http://10.0.0.5:11399".into();
        std::fs::write(
            &path,
            serde_json::to_vec(&vec![stale, entry("a", 1)]).unwrap(),
        )
        .unwrap();
        let t = DelegateTable::new("me".into(), None, Some(path.clone()));
        assert_eq!(t.self_entry().url, "");
        assert!(
            t.shared_entries().iter().all(|e| e.id != "me"),
            "not shared until known"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn stale_entries_are_refused_and_pruned_but_fresh_ones_stay() {
        let t = table().with_expiry_secs(3600);
        let two_hours_ago = now_ms() - 2 * 3600 * 1000;
        // Gossip about a long-dead delegate must not bring it back.
        assert!(!t.merge([entry("ghost", two_hours_ago)]));
        assert!(t.peers().is_empty());
        assert!(t.merge([fresh("alive")]));

        // One that goes quiet after being added ages out once fresh data is in.
        let quiet = table().with_expiry_secs(3600);
        quiet.merge([fresh("alive")]);
        quiet
            .inner
            .write()
            .unwrap()
            .get_mut("alive")
            .unwrap()
            .last_seen = two_hours_ago;
        quiet.merge([fresh("other")]);
        let gone = quiet.prune();
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].id, "alive");
        assert_eq!(quiet.peers().len(), 1);
        assert_eq!(quiet.self_entry().id, "me", "never prunes ourselves");
    }

    #[test]
    fn expiry_zero_keeps_everything() {
        let t = table().with_expiry_secs(0);
        assert!(t.merge([entry("ancient", 1)]));
        assert!(t.prune().is_empty());
        assert_eq!(t.peers().len(), 1);
    }

    #[test]
    fn forgetting_keeps_a_peer_out_until_it_is_heard_from_again() {
        let path = temp_table_path();
        let t = DelegateTable::new("me".into(), Some("http://me:1".into()), Some(path.clone()));
        t.merge([entry("ghost", 100)]);
        assert!(t.forget("ghost"));
        assert!(t.peers().is_empty());
        assert!(!t.forget("ghost"), "already gone");
        assert!(!t.forget("me"), "cannot forget ourselves");

        // The same stale entry coming back through gossip is refused...
        assert!(!t.merge([entry("ghost", 100)]));
        assert!(!t.merge([entry("ghost", 50)]));
        assert!(t.peers().is_empty());
        // ...and so after a restart, since the tombstone is saved...
        let t2 = DelegateTable::new("me".into(), Some("http://me:1".into()), Some(path.clone()));
        assert!(!t2.merge([entry("ghost", 100)]));
        // ...but a genuine sign of life (a newer last_seen) brings it back.
        assert!(t2.merge([entry("ghost", 101)]));
        assert_eq!(t2.peers().len(), 1);
        assert!(t2.merge([entry("ghost", 102)]), "and the tombstone is gone");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
