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
    /// Red: cannot serve anything, locally or through a peer.
    Down,
}

pub fn health(s: &AppState) -> Health {
    let local_up = s.table.first_up().is_some();
    let peer_online = s
        .delegates
        .peers()
        .iter()
        .any(|p| p.status == Status::Online);
    if !local_up && !peer_online {
        Health::Down
    } else if s.pool.active() > 0 {
        Health::InUse
    } else {
        Health::Active
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
        "sharing": s.config.share_with_peers,
        "masquerade": s.config.masquerade,
        "port": s.config.listen_port(),
        "pool": { "size": s.pool.size(), "in_use": s.pool.in_use(), "forwarding": s.pool.forwarding() },
        "ollama": hosts,
        "delegates": s.delegates.shared_entries(),
    })
}
