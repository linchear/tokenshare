use serde::Serialize;
use serde_json::{json, Value};

use crate::proxy::AppState;
use crate::table::Status;

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

pub fn status_json(s: &AppState) -> Value {
    let hosts: Vec<Value> = s
        .table
        .snapshot()
        .into_iter()
        .map(|h| {
            json!({
                "host": h.host,
                "up": h.up,
                "models": h.models.iter().map(|m| &m.name).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "id": s.id,
        "state": health(s),
        "draining": s.is_draining(),
        "sharing": s.config.shares_models(),
        "masquerade": s.config.masquerade,
        "port": s.config.port,
        "ports": s.config.listen_ports(),
        "pool": { "size": s.pool.size(), "in_use": s.pool.in_use(), "forwarding": s.pool.forwarding() },
        "ollama": hosts,
        "delegates": s.delegates.shared_entries(),
    })
}
