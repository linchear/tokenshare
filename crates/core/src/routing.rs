//! The order in which peers are tried for a request.
//!
//! Each peer gossips its slot count, how many slots were busy and which models
//! it has loaded in memory. That view can be up to a keepalive interval old, so
//! it is combined with what this delegate knows first-hand from [`Activity`]:
//! how many of its own requests are in flight to the peer, and whether the peer
//! recently refused one. Peers are ordered by, in turn:
//!
//! 1. not cooling down after a refusal or a failed connection;
//! 2. not full (gossiped busy slots plus our own in-flight requests below its
//!    slot count; a peer that does not publish slots counts as not full);
//! 3. has the model loaded already (warm), so it starts without a load;
//! 4. fewest of our own requests in flight to it.
//!
//! Ties are broken at random, so equally good peers share the load. Nothing is
//! ever excluded: a full or cooling peer is still tried, just after the others.

use rand::seq::SliceRandom;

use crate::activity::Activity;
use crate::ollama::model_matches;
use crate::table::DelegateEntry;

/// How a peer compares for one request; lower sorts first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    pub cooling: bool,
    pub full: bool,
    pub cold: bool,
    pub inflight: u32,
}

impl Rank {
    /// Has the model loaded, has room, and has not just refused: a request
    /// sent there should start right away.
    pub fn ready(&self) -> bool {
        !self.cooling && !self.full && !self.cold
    }
}

pub fn rank_of(peer: &DelegateEntry, model: &str, activity: &Activity) -> Rank {
    let inflight = activity.inflight(&peer.url);
    let full = peer
        .slots
        .is_some_and(|slots| peer.busy.unwrap_or(0).saturating_add(inflight) >= slots);
    Rank {
        cooling: activity.cooling(&peer.url),
        full,
        cold: !peer.loaded.iter().any(|m| model_matches(model, m)),
        inflight,
    }
}

/// `peers` in the order to try them for `model`.
pub fn order(peers: Vec<DelegateEntry>, model: &str, activity: &Activity) -> Vec<DelegateEntry> {
    let mut ranked: Vec<(Rank, DelegateEntry)> = peers
        .into_iter()
        .map(|p| (rank_of(&p, model, activity), p))
        .collect();
    ranked.shuffle(&mut rand::rng());
    // Stable, so the shuffle decides between equal ranks.
    ranked.sort_by_key(|(r, _)| *r);
    ranked.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::Refusal;

    fn peer(id: &str, slots: Option<u32>, busy: Option<u32>, loaded: &[&str]) -> DelegateEntry {
        let mut e = DelegateEntry::new(id.into(), format!("http://{id}"), vec!["m:latest".into()]);
        e.slots = slots;
        e.busy = busy;
        e.loaded = loaded.iter().map(|s| s.to_string()).collect();
        e
    }

    fn ids(v: &[DelegateEntry]) -> Vec<&str> {
        v.iter().map(|p| p.id.as_str()).collect()
    }

    #[test]
    fn warm_beats_cold_and_free_beats_full() {
        let a = Activity::new();
        for _ in 0..20 {
            let got = order(
                vec![
                    peer("full-warm", Some(2), Some(2), &["m:latest"]),
                    peer("cold", Some(2), Some(0), &[]),
                    peer("warm", Some(2), Some(1), &["m:latest"]),
                ],
                "m",
                &a,
            );
            assert_eq!(ids(&got), ["warm", "cold", "full-warm"]);
        }
    }

    #[test]
    fn our_own_inflight_requests_fill_a_peer_up() {
        let a = Activity::new();
        let p = peer("p", Some(2), Some(1), &[]);
        assert!(!rank_of(&p, "m", &a).full);
        let _g = a.start("http://p");
        assert!(rank_of(&p, "m", &a).full);
        // And among otherwise equal peers, the one we are less busy with wins.
        let q = peer("q", None, None, &[]);
        let r = peer("r", None, None, &[]);
        let _g2 = a.start("http://q");
        for _ in 0..20 {
            assert_eq!(ids(&order(vec![q.clone(), r.clone()], "m", &a)), ["r", "q"]);
        }
    }

    #[test]
    fn a_peer_that_just_refused_goes_last_but_is_not_dropped() {
        let a = Activity::new();
        a.refused("http://warm", Refusal::Declined);
        let got = order(
            vec![
                peer("warm", Some(4), Some(0), &["m:latest"]),
                peer("full", Some(1), Some(1), &[]),
            ],
            "m",
            &a,
        );
        assert_eq!(ids(&got), ["full", "warm"]);
    }

    #[test]
    fn peers_without_load_info_count_as_free_and_ties_are_shuffled() {
        let a = Activity::new();
        let mut first = std::collections::HashSet::new();
        for _ in 0..200 {
            let got = order(
                vec![peer("x", None, None, &[]), peer("y", None, None, &[])],
                "m",
                &a,
            );
            first.insert(got[0].id.clone());
        }
        assert_eq!(first.len(), 2, "both get picked first sometimes");
    }
}
