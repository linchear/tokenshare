//! `GET /_ts/metrics`: the delegate's state and activity in the Prometheus text
//! exposition format (version 0.0.4), for Prometheus, Grafana Agent, Datadog and
//! the like. Written by hand: the format is simple and this needs no dependency.
//!
//! Like the response log and the dashboard, these are metrics only, never content.
//! Label values are bounded by the size of the pool: upstream URLs, model names.

use std::fmt::{Display, Write};

use crate::proxy::AppState;
use crate::status::{health, Health};
use crate::table::Status;

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Builds an exposition. Each family's `# HELP` and `# TYPE` are written once by
/// [`Exposition::family`], and all of its samples must follow before the next.
#[derive(Default)]
pub struct Exposition {
    out: String,
}

/// Escape a label value: backslash, double quote and newline.
fn escape(v: &str) -> String {
    v.replace('\\', r"\\")
        .replace('"', r#"\""#)
        .replace('\n', r"\n")
}

impl Exposition {
    pub fn family(&mut self, name: &str, kind: &str, help: &str) {
        let help = help.replace('\\', r"\\").replace('\n', r"\n");
        let _ = writeln!(self.out, "# HELP {name} {help}");
        let _ = writeln!(self.out, "# TYPE {name} {kind}");
    }

    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: impl Display) {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (k, v)) in labels.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                let _ = write!(self.out, "{k}=\"{}\"", escape(v));
            }
            self.out.push('}');
        }
        let _ = writeln!(self.out, " {value}");
    }

    pub fn finish(self) -> String {
        self.out
    }
}

fn flag(b: bool) -> u8 {
    u8::from(b)
}

/// The whole exposition for this delegate.
pub fn render(s: &AppState) -> String {
    let mut m = Exposition::default();

    m.family(
        "tokenshare_info",
        "gauge",
        "Always 1; labels identify the delegate and its version.",
    );
    let name = s.delegates.self_entry().name.unwrap_or_default();
    m.sample(
        "tokenshare_info",
        &[
            ("id", &s.id),
            ("name", &name),
            ("version", env!("CARGO_PKG_VERSION")),
            (
                "ollama_version",
                &s.table.ollama_version().unwrap_or_default(),
            ),
        ],
        1,
    );

    m.family(
        "tokenshare_state",
        "gauge",
        "1 for the delegate's current state (what the tray icon shows), 0 for the others.",
    );
    let now = health(s);
    for (state, label) in [
        (Health::Active, "active"),
        (Health::InUse, "in-use"),
        (Health::NoModels, "no-models"),
        (Health::Down, "down"),
    ] {
        m.sample("tokenshare_state", &[("state", label)], flag(now == state));
    }

    m.family(
        "tokenshare_draining",
        "gauge",
        "1 while shutting down and refusing new model requests.",
    );
    m.sample("tokenshare_draining", &[], flag(s.is_draining()));

    m.family(
        "tokenshare_sharing",
        "gauge",
        "1 if other delegates may use this machine's ollama.",
    );
    m.sample("tokenshare_sharing", &[], flag(s.config.shares_models()));

    m.family(
        "tokenshare_auth_required",
        "gauge",
        "1 if this delegate requires the pool secret.",
    );
    m.sample("tokenshare_auth_required", &[], flag(s.secret.is_some()));

    m.family(
        "tokenshare_pool_slots",
        "gauge",
        "Concurrent requests this delegate sends to its local ollama (pool_size).",
    );
    m.sample("tokenshare_pool_slots", &[], s.pool.size());
    m.family(
        "tokenshare_pool_slots_in_use",
        "gauge",
        "Local ollama slots in use.",
    );
    m.sample("tokenshare_pool_slots_in_use", &[], s.pool.in_use());
    m.family(
        "tokenshare_pool_peer_slots",
        "gauge",
        "How many of the local slots other delegates may use at once (max_peer_requests).",
    );
    m.sample("tokenshare_pool_peer_slots", &[], s.pool.peer_size());
    m.family(
        "tokenshare_pool_peer_slots_in_use",
        "gauge",
        "Local slots in use by other delegates' requests.",
    );
    m.sample(
        "tokenshare_pool_peer_slots_in_use",
        &[],
        s.pool.peer_in_use(),
    );
    m.family(
        "tokenshare_forwarding",
        "gauge",
        "Requests this delegate is forwarding to peers right now.",
    );
    m.sample("tokenshare_forwarding", &[], s.pool.forwarding());
    m.family(
        "tokenshare_requests_waiting",
        "gauge",
        "Requests waiting for a free slot here or on a peer (see queue_timeout_secs).",
    );
    m.sample("tokenshare_requests_waiting", &[], s.pool.waiting());

    let hosts = s.table.snapshot();
    m.family(
        "tokenshare_ollama_up",
        "gauge",
        "1 if the local ollama host answered its last poll.",
    );
    for h in &hosts {
        m.sample("tokenshare_ollama_up", &[("host", &h.host)], flag(h.up));
    }
    m.family(
        "tokenshare_ollama_models",
        "gauge",
        "Models installed on the local ollama host.",
    );
    for h in &hosts {
        m.sample(
            "tokenshare_ollama_models",
            &[("host", &h.host)],
            h.models.len(),
        );
    }
    m.family(
        "tokenshare_ollama_loaded_models",
        "gauge",
        "Models loaded in memory on the local ollama host.",
    );
    for h in &hosts {
        m.sample(
            "tokenshare_ollama_loaded_models",
            &[("host", &h.host)],
            h.loaded.len(),
        );
    }

    let peers = s.delegates.peers();
    m.family(
        "tokenshare_peers",
        "gauge",
        "Other delegates this one knows, by status.",
    );
    for (status, label) in [
        (Status::Online, "online"),
        (Status::Unavailable, "unavailable"),
        (Status::Inactive, "inactive"),
    ] {
        let n = peers.iter().filter(|p| p.status == status).count();
        m.sample("tokenshare_peers", &[("status", label)], n);
    }
    m.family(
        "tokenshare_peer_up",
        "gauge",
        "1 if the peer is online in this delegate's table.",
    );
    for p in &peers {
        m.sample(
            "tokenshare_peer_up",
            &[("peer", &p.url), ("id", &p.id)],
            flag(p.status == Status::Online),
        );
    }
    m.family(
        "tokenshare_peer_info",
        "gauge",
        "Always 1; labels give the peer's name (which can change and need not be unique) and the versions it runs.",
    );
    for p in &peers {
        m.sample(
            "tokenshare_peer_info",
            &[
                ("peer", &p.url),
                ("id", &p.id),
                ("name", p.name.as_deref().unwrap_or_default()),
                ("version", p.version.as_deref().unwrap_or_default()),
                (
                    "ollama_version",
                    p.ollama_version.as_deref().unwrap_or_default(),
                ),
            ],
            1,
        );
    }
    m.family(
        "tokenshare_peer_models",
        "gauge",
        "Models the peer offers to the pool.",
    );
    for p in &peers {
        m.sample(
            "tokenshare_peer_models",
            &[("peer", &p.url)],
            p.models.len(),
        );
    }
    m.family(
        "tokenshare_peer_loaded_models",
        "gauge",
        "Models the peer last reported loaded in memory.",
    );
    for p in &peers {
        m.sample(
            "tokenshare_peer_loaded_models",
            &[("peer", &p.url)],
            p.loaded.len(),
        );
    }
    m.family(
        "tokenshare_peer_slots",
        "gauge",
        "Slots the peer last published (absent for delegates that do not publish them).",
    );
    for p in peers.iter().filter(|p| p.slots.is_some()) {
        m.sample(
            "tokenshare_peer_slots",
            &[("peer", &p.url)],
            p.slots.unwrap_or(0),
        );
    }
    m.family(
        "tokenshare_peer_slots_busy",
        "gauge",
        "Busy slots the peer last published; as old as its last keepalive.",
    );
    for p in peers.iter().filter(|p| p.slots.is_some()) {
        m.sample(
            "tokenshare_peer_slots_busy",
            &[("peer", &p.url)],
            p.busy.unwrap_or(0),
        );
    }
    m.family(
        "tokenshare_peer_last_seen_timestamp_seconds",
        "gauge",
        "When the peer was last heard from (its own clock), as a Unix timestamp.",
    );
    for p in &peers {
        m.sample(
            "tokenshare_peer_last_seen_timestamp_seconds",
            &[("peer", &p.url)],
            p.last_seen as f64 / 1000.0,
        );
    }

    s.activity.write_metrics(&mut m);
    write_ledger(s, &mut m);
    m.finish()
}

/// Lent and borrowed requests and tokens per peer (by id; join on
/// `tokenshare_peer_info`). Kept across restarts, so they only ever grow.
fn write_ledger(s: &AppState, m: &mut Exposition) {
    use crate::ledger::Balance;
    let balances = s.ledger.balances();
    type Side = fn(&Balance) -> &crate::ledger::Tally;
    let sides: [(&str, &str, Side); 2] = [
        ("lent", "served here for the peer", |b| &b.lent),
        ("borrowed", "served by the peer for this machine", |b| {
            &b.borrowed
        }),
    ];
    for (dir, what, side) in sides {
        let name = format!("tokenshare_{dir}_requests_total");
        m.family(&name, "counter", &format!("Requests {what}, by peer id."));
        for (id, b) in &balances {
            m.sample(&name, &[("id", id)], side(b).requests);
        }
        let name = format!("tokenshare_{dir}_tokens_total");
        m.family(
            &name,
            "counter",
            &format!("Tokens {what}, by peer id and kind (prompt or completion)."),
        );
        for (id, b) in &balances {
            m.sample(
                &name,
                &[("id", id), ("kind", "prompt")],
                side(b).prompt_tokens,
            );
            m.sample(
                &name,
                &[("id", id), ("kind", "completion")],
                side(b).completion_tokens,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::Histogram;

    #[test]
    fn label_values_are_escaped() {
        let mut m = Exposition::default();
        m.family("x", "gauge", "help with \\ and\nnewline");
        m.sample("x", &[("a", "q\"uo\\te\nnl")], 1);
        m.sample("y", &[], 2.5);
        assert_eq!(
            m.finish(),
            "# HELP x help with \\\\ and\\nnewline\n# TYPE x gauge\nx{a=\"q\\\"uo\\\\te\\nnl\"} 1\ny 2.5\n"
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_end_with_inf() {
        let mut h = Histogram::default();
        h.observe(0.01);
        h.observe(0.3);
        h.observe(1000.0); // beyond every bucket: only +Inf
        let c = h.cumulative();
        assert_eq!(c[0], ("0.05".to_string(), 1));
        assert_eq!(c[3], ("0.5".to_string(), 2));
        assert_eq!(c[c.len() - 2].1, 2);
        assert_eq!(c.last().unwrap(), &("+Inf".to_string(), 3));
        assert_eq!(h.count(), 3);
        assert!((h.sum() - 1000.31).abs() < 1e-9);
    }
}
