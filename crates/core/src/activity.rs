//! What this delegate has seen first-hand of the upstreams it sends requests to
//! (its own ollama hosts and peer delegates): requests in flight, how they
//! ended, how fast the first byte came back, and recent refusals.
//!
//! Routing reads it to avoid piling onto a peer it is already busy with, or one
//! that just turned it away; the dashboard reads it to show what is going on.
//! Like the response log, it holds metrics only, never request or response content.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::table::now_ms;

/// How many finished responses to keep for the dashboard.
const RECENT: usize = 100;

/// After a peer refuses a request or cannot be reached, it is tried after the
/// others for this long.
pub const COOLDOWN: Duration = Duration::from_secs(10);

/// Weight of the newest sample in the time-to-first-byte moving average.
const TTFB_ALPHA: f64 = 0.3;

/// How a peer turned a request away before streaming anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// It answered with an error status (busy, not sharing, shutting down...).
    Declined,
    /// It could not be reached at all.
    Unreachable,
}

/// Counters for requests that never reached an upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Every candidate was busy: answered 503.
    Busy,
    /// No delegate has the model: answered 404.
    NotFound,
}

#[derive(Debug, Default, Clone)]
struct TargetStats {
    inflight: u32,
    ok: u64,
    problems: u64,
    declined: u64,
    unreachable: u64,
    ttfb_ewma_ms: Option<f64>,
    last_refusal: Option<Instant>,
}

/// One response that has finished streaming, as the dashboard lists it.
#[derive(Debug, Clone, Serialize)]
pub struct Finished {
    /// Unix milliseconds when it ended.
    pub at: u64,
    pub method: String,
    pub path: String,
    pub model: Option<String>,
    pub route: &'static str,
    pub target: String,
    pub status: u16,
    pub outcome: &'static str,
    /// Empty when the response ended properly.
    pub problem: &'static str,
    pub bytes: u64,
    pub ttfb_ms: u64,
    pub total_ms: u64,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

#[derive(Default)]
struct Inner {
    targets: HashMap<String, TargetStats>,
    recent: VecDeque<Finished>,
    busy: u64,
    not_found: u64,
}

#[derive(Default)]
pub struct Activity {
    inner: Mutex<Inner>,
}

/// Counts one request in flight to a target for as long as it is held.
pub struct InFlight {
    activity: Arc<Activity>,
    target: String,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut g = self.activity.inner.lock().unwrap();
        if let Some(t) = g.targets.get_mut(&self.target) {
            t.inflight = t.inflight.saturating_sub(1);
        }
    }
}

impl Activity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A request is being sent to `target`; it counts until the guard drops.
    pub fn start(self: &Arc<Self>, target: &str) -> InFlight {
        let mut g = self.inner.lock().unwrap();
        g.targets.entry(target.to_string()).or_default().inflight += 1;
        InFlight {
            activity: self.clone(),
            target: target.to_string(),
        }
    }

    /// Requests in flight from this delegate to `target`.
    pub fn inflight(&self, target: &str) -> u32 {
        let g = self.inner.lock().unwrap();
        g.targets.get(target).map_or(0, |t| t.inflight)
    }

    /// `target` refused or could not take a request within the last [`COOLDOWN`].
    pub fn cooling(&self, target: &str) -> bool {
        let g = self.inner.lock().unwrap();
        g.targets
            .get(target)
            .and_then(|t| t.last_refusal)
            .is_some_and(|at| at.elapsed() < COOLDOWN)
    }

    pub fn refused(&self, target: &str, how: Refusal) {
        let mut g = self.inner.lock().unwrap();
        let t = g.targets.entry(target.to_string()).or_default();
        t.last_refusal = Some(Instant::now());
        match how {
            Refusal::Declined => t.declined += 1,
            Refusal::Unreachable => t.unreachable += 1,
        }
    }

    pub fn rejected(&self, why: Rejection) {
        let mut g = self.inner.lock().unwrap();
        match why {
            Rejection::Busy => g.busy += 1,
            Rejection::NotFound => g.not_found += 1,
        }
    }

    /// A response finished streaming (or the client left). Passthrough traffic
    /// (pulls and the like) is listed but does not count toward target stats.
    pub fn finished(&self, f: Finished) {
        let mut g = self.inner.lock().unwrap();
        if f.route != "passthrough" {
            let t = g.targets.entry(f.target.clone()).or_default();
            if f.problem.is_empty() {
                t.ok += 1;
            } else {
                t.problems += 1;
            }
            if (200..300).contains(&f.status) {
                let sample = f.ttfb_ms as f64;
                t.ttfb_ewma_ms = Some(match t.ttfb_ewma_ms {
                    Some(avg) => avg + TTFB_ALPHA * (sample - avg),
                    None => sample,
                });
            }
        }
        if g.recent.len() == RECENT {
            g.recent.pop_front();
        }
        g.recent.push_back(f);
    }

    /// Everything above, for `/_ts/status`. Recent responses are newest first.
    pub fn snapshot(&self) -> Value {
        let g = self.inner.lock().unwrap();
        let now = now_ms();
        let mut targets: Vec<Value> = g
            .targets
            .iter()
            .map(|(target, t)| {
                json!({
                    "target": target,
                    "inflight": t.inflight,
                    "ok": t.ok,
                    "problems": t.problems,
                    "declined": t.declined,
                    "unreachable": t.unreachable,
                    "ttfb_ms_avg": t.ttfb_ewma_ms.map(|v| v.round() as u64),
                    "cooling": t.last_refusal.is_some_and(|at| at.elapsed() < COOLDOWN),
                    "last_refusal_ms_ago": t.last_refusal.map(|at| at.elapsed().as_millis() as u64),
                })
            })
            .collect();
        targets.sort_by(|a, b| a["target"].as_str().cmp(&b["target"].as_str()));
        json!({
            "now": now,
            "targets": targets,
            "rejected": { "busy": g.busy, "not_found": g.not_found },
            "recent": g.recent.iter().rev().collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(target: &str, problem: &'static str, ttfb_ms: u64) -> Finished {
        Finished {
            at: 0,
            method: "POST".into(),
            path: "/api/chat".into(),
            model: Some("m".into()),
            route: "peer",
            target: target.into(),
            status: 200,
            outcome: "complete",
            problem,
            bytes: 1,
            ttfb_ms,
            total_ms: ttfb_ms,
            prompt_tokens: None,
            completion_tokens: None,
        }
    }

    #[test]
    fn inflight_counts_until_the_guard_drops() {
        let a = Activity::new();
        let g1 = a.start("p");
        let _g2 = a.start("p");
        assert_eq!(a.inflight("p"), 2);
        assert_eq!(a.inflight("other"), 0);
        drop(g1);
        assert_eq!(a.inflight("p"), 1);
    }

    #[test]
    fn a_refusal_cools_a_target_down() {
        let a = Activity::new();
        assert!(!a.cooling("p"));
        a.refused("p", Refusal::Declined);
        assert!(a.cooling("p"));
        assert!(!a.cooling("q"));
        let s = a.snapshot();
        assert_eq!(s["targets"][0]["declined"], 1);
        assert_eq!(s["targets"][0]["cooling"], true);
    }

    #[test]
    fn finished_responses_update_stats_and_the_recent_list() {
        let a = Activity::new();
        a.finished(done("p", "", 100));
        a.finished(done("p", "no_final_chunk", 200));
        let s = a.snapshot();
        let t = &s["targets"][0];
        assert_eq!(
            (t["ok"].as_u64(), t["problems"].as_u64()),
            (Some(1), Some(1))
        );
        // 100, then 100 + 0.3 * (200 - 100).
        assert_eq!(t["ttfb_ms_avg"], 130);
        assert_eq!(s["recent"][0]["problem"], "no_final_chunk", "newest first");
        for _ in 0..RECENT + 5 {
            a.finished(done("p", "", 1));
        }
        assert_eq!(a.snapshot()["recent"].as_array().unwrap().len(), RECENT);
    }
}
