use serde::Serialize;
use serde_json::{json, Value};

use crate::proxy::AppState;
use crate::table::{DelegateEntry, Status};

/// What the tray icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Health {
    /// Green: up and idle.
    Active,
    /// Blue: at least one request in flight, served here or forwarded.
    InUse,
    /// Yellow: no model is available anywhere, but other delegates are online,
    /// so requests will work again as soon as some ollama comes back.
    NoModels,
    /// Gray: unavailable. No model is available and no other delegate is online.
    Down,
}

/// Can this delegate get a request answered? Only if some model is available:
/// on its own ollama, or on a peer that is online. A reachable ollama or an
/// online peer with no models does not count. With no model anywhere, the
/// mesh being alive (some peer online) is yellow; being alone is red.
pub fn health(s: &AppState) -> Health {
    let local_ready = !s.table.all_models().is_empty();
    let peers = s.delegates.peers();
    let online = |p: &&crate::table::DelegateEntry| p.status == Status::Online;
    let peer_ready = peers.iter().filter(online).any(|p| !p.models.is_empty());
    if local_ready || peer_ready {
        if s.pool.active() > 0 {
            Health::InUse
        } else {
            Health::Active
        }
    } else if peers.iter().any(|p| online(&p)) {
        Health::NoModels
    } else {
        Health::Down
    }
}

/// Models offered under one name with different contents (digests) by online
/// delegates, e.g. `llama3:latest` pulled months apart on two machines.
/// Routing treats them as the same model, so answers can differ depending on
/// which machine serves them. Delegates that publish no digest are left out.
pub fn differing_models(entries: &[DelegateEntry]) -> Vec<Value> {
    let mut by_model: std::collections::BTreeMap<&str, Vec<(String, &str)>> = Default::default();
    for e in entries.iter().filter(|e| e.status == Status::Online) {
        for m in &e.model_info {
            if let Some(d) = m.digest_key() {
                by_model.entry(&m.name).or_default().push((d, &e.id));
            }
        }
    }
    by_model
        .into_iter()
        .filter_map(|(model, seen)| {
            let mut variants: Vec<(String, Vec<&str>)> = vec![];
            for (digest, id) in seen {
                match variants.iter_mut().find(|(d, _)| *d == digest) {
                    Some((_, ids)) => ids.push(id),
                    None => variants.push((digest, vec![id])),
                }
            }
            (variants.len() > 1).then(|| {
                json!({
                    "model": model,
                    "variants": variants
                        .into_iter()
                        .map(|(digest, ids)| json!({ "digest": digest, "delegates": ids }))
                        .collect::<Vec<_>>(),
                })
            })
        })
        .collect()
}

pub fn status_json(s: &AppState) -> Value {
    s.publish_load();
    let hosts: Vec<Value> = s
        .table
        .snapshot()
        .into_iter()
        .map(|h| {
            json!({
                "host": h.host,
                "up": h.up,
                "models": h.models.iter().map(|m| &m.name).collect::<Vec<_>>(),
                "loaded": h.loaded,
                "version": h.version,
            })
        })
        .collect();
    json!({
        "id": s.id,
        "name": s.delegates.self_entry().name,
        "state": health(s),
        "draining": s.is_draining(),
        "sharing": s.config.shares_models(),
        "masquerade": s.config.masquerade,
        "auth": s.secret.is_some(),
        "port": s.config.port,
        "ports": s.config.listen_ports(),
        "pool": {
            "size": s.pool.size(),
            "in_use": s.pool.in_use(),
            "forwarding": s.pool.forwarding(),
            "peer_limit": s.pool.peer_size(),
            "peer_in_use": s.pool.peer_in_use(),
            "waiting": s.pool.waiting(),
            "queue_timeout_secs": s.config.queue_timeout_secs,
        },
        "shared_models": s.config.shared_models,
        "ollama": hosts,
        "version": env!("CARGO_PKG_VERSION"),
        "ollama_version": s.table.ollama_version(),
        "differing_models": differing_models(&s.delegates.shared_entries()),
        "delegates": s.delegates.shared_entries(),
        "activity": s.activity.snapshot(),
        "ledger": {
            "since": s.ledger.since(),
            "totals": s.ledger.totals(),
            "peers": s.ledger.balances(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::ModelMeta;

    fn delegate(id: &str, models: &[(&str, Option<&str>)]) -> DelegateEntry {
        let mut e = DelegateEntry::new(
            id.into(),
            format!("http://{id}:1"),
            models.iter().map(|(m, _)| m.to_string()).collect(),
        );
        e.model_info = models
            .iter()
            .map(|(m, d)| ModelMeta {
                name: m.to_string(),
                digest: d.map(str::to_string),
                ..Default::default()
            })
            .collect();
        e
    }

    #[test]
    fn same_name_different_digest_is_reported() {
        let old = "aaaaaaaaaaaa";
        let new = "bbbbbbbbbbbb";
        let mut gone = delegate("gone", &[("llama3:latest", Some("cccccccccccc"))]);
        gone.status = Status::Unavailable;
        let entries = [
            delegate(
                "a",
                &[("llama3:latest", Some(old)), ("qwen3:8b", Some(new))],
            ),
            delegate(
                "b",
                &[("llama3:latest", Some(new)), ("qwen3:8b", Some(new))],
            ),
            delegate(
                "c",
                &[(
                    "llama3:latest",
                    Some(&format!("sha256:{}", old.to_uppercase())),
                )],
            ),
            delegate("d", &[("llama3:latest", None)]),
            gone,
        ];
        assert_eq!(
            differing_models(&entries),
            vec![json!({"model": "llama3:latest", "variants": [
                {"digest": old, "delegates": ["a", "c"]},
                {"digest": new, "delegates": ["b"]},
            ]})],
            "same model under another spelling, no digest, and offline delegates do not count"
        );
        assert!(differing_models(&entries[1..2]).is_empty());
    }
}
