//! When to show a desktop notification, decided from what the delegate looks
//! like every time the tray polls it. Nothing here talks to the OS, so the
//! rules can be tested on their own.
//!
//! Gossip makes peers flap (one missed keepalive marks a peer unavailable until
//! the next one arrives), so a peer counts as gone only after it has stayed
//! offline for [`OFFLINE_AFTER`], and only one we saw online since the delegate
//! started. Right after a start the table is still being repaired, so for
//! [`WARMUP`] nothing but a refused secret or a failed start is reported.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// How long a peer must stay offline before it is reported.
pub const OFFLINE_AFTER: Duration = Duration::from_secs(60);
/// How long after the delegate starts before peers coming and going count.
pub const WARMUP: Duration = Duration::from_secs(60);

/// One other delegate, as the tray sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub id: String,
    pub url: String,
    /// Its name, or its URL if it has none.
    pub label: String,
    pub online: bool,
}

/// The delegate at one poll.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct View {
    /// Changes whenever the delegate is (re)started, which resets everything.
    pub generation: usize,
    /// Why the delegate is not running, if it is not.
    pub start_error: Option<String>,
    pub peers: Vec<Peer>,
    /// URLs of peers that answered our requests with 401.
    pub secret_refused: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Offline,
    Back,
    Joined,
    Secret,
}

#[derive(Default)]
pub struct Notifier {
    generation: Option<usize>,
    started: Option<Instant>,
    /// Every peer id seen since the start.
    known: HashSet<String>,
    /// Peers seen online after the warm-up: only these are reported as gone.
    seen_online: HashSet<String>,
    down_since: HashMap<String, Instant>,
    reported_down: HashSet<String>,
    reported_secret: HashSet<String>,
    reported_error: Option<String>,
}

impl Notifier {
    pub fn update(&mut self, view: &View, now: Instant) -> Vec<Notice> {
        if self.generation != Some(view.generation) {
            *self = Self {
                generation: Some(view.generation),
                started: Some(now),
                // A failed start stays reported across restarts that fail the same way.
                reported_error: self.reported_error.take(),
                ..Self::default()
            };
        }
        let mut out = vec![];

        match &view.start_error {
            Some(e) if self.reported_error.as_ref() != Some(e) => {
                self.reported_error = Some(e.clone());
                out.push(Notice {
                    title: "Token Share could not start".into(),
                    body: e.clone(),
                });
            }
            Some(_) => {}
            None => self.reported_error = None,
        }

        let warm = self
            .started
            .is_some_and(|t| now.duration_since(t) >= WARMUP);
        let mut events: Vec<(Kind, String)> = vec![];
        let present: HashSet<&str> = view.peers.iter().map(|p| p.id.as_str()).collect();
        for p in &view.peers {
            let new = self.known.insert(p.id.clone());
            if p.online {
                self.down_since.remove(&p.id);
                if self.reported_down.remove(&p.id) {
                    events.push((Kind::Back, p.label.clone()));
                } else if new && warm {
                    events.push((Kind::Joined, p.label.clone()));
                }
                if warm {
                    self.seen_online.insert(p.id.clone());
                }
            } else {
                let since = *self.down_since.entry(p.id.clone()).or_insert(now);
                if self.seen_online.contains(&p.id)
                    && !self.reported_down.contains(&p.id)
                    && now.duration_since(since) >= OFFLINE_AFTER
                {
                    self.reported_down.insert(p.id.clone());
                    events.push((Kind::Offline, p.label.clone()));
                }
            }
        }
        // Forgotten or expired: nothing to say, just stop tracking them.
        self.down_since
            .retain(|id, _| present.contains(id.as_str()));
        self.reported_down
            .retain(|id| present.contains(id.as_str()));
        self.seen_online.retain(|id| present.contains(id.as_str()));

        self.reported_secret
            .retain(|u| view.secret_refused.contains(u));
        for url in &view.secret_refused {
            if self.reported_secret.insert(url.clone()) {
                let label = view
                    .peers
                    .iter()
                    .find(|p| &p.url == url)
                    .map_or(url.clone(), |p| p.label.clone());
                events.push((Kind::Secret, label));
            }
        }

        for kind in [Kind::Secret, Kind::Offline, Kind::Back, Kind::Joined] {
            let who: Vec<&str> = events
                .iter()
                .filter(|(k, _)| *k == kind)
                .map(|(_, l)| l.as_str())
                .collect();
            if !who.is_empty() {
                out.push(notice(kind, &who));
            }
        }
        out
    }
}

/// One notice for every delegate the same thing happened to.
fn notice(kind: Kind, who: &[&str]) -> Notice {
    let one = who.len() == 1;
    let names = match who {
        [a] => a.to_string(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} others", rest.len()),
        [] => String::new(),
    };
    let (title, body) = match kind {
        Kind::Offline => (
            format!("{names} went offline"),
            "Requests for models only it has will fail until it is back.".to_string(),
        ),
        Kind::Back => (
            format!("{names} {} back online", if one { "is" } else { "are" }),
            "Its models are in the pool again.".to_string(),
        ),
        Kind::Joined => (
            format!("{names} joined the pool"),
            "Its models are now available from this machine.".to_string(),
        ),
        Kind::Secret => (
            format!("{names} {} this machine's pool secret", if one { "refuses" } else { "refuse" }),
            "Requests are not sent there. Check that every delegate has the same pool secret in Settings."
                .to_string(),
        ),
    };
    let body = if one {
        body
    } else {
        body.replace("Its", "Their")
            .replace("only it has", "only they have")
            .replace("until it is", "until they are")
    };
    Notice { title, body }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, online: bool) -> Peer {
        Peer {
            id: id.into(),
            url: format!("http://{id}:11343"),
            label: id.into(),
            online,
        }
    }

    fn view(peers: Vec<Peer>) -> View {
        View {
            generation: 1,
            peers,
            ..Default::default()
        }
    }

    fn titles(n: Vec<Notice>) -> Vec<String> {
        n.into_iter().map(|n| n.title).collect()
    }

    /// A notifier past its warm-up that has seen `peers` online.
    fn warmed(peers: &[&str], t0: Instant) -> Notifier {
        let mut n = Notifier::default();
        let all = || view(peers.iter().map(|p| peer(p, true)).collect());
        assert!(n.update(&all(), t0).is_empty());
        assert!(n.update(&all(), t0 + WARMUP).is_empty());
        n
    }

    #[test]
    fn a_peer_is_reported_gone_only_after_staying_offline() {
        let t0 = Instant::now();
        let mut n = warmed(&["gpu-box"], t0);
        let t = t0 + WARMUP;
        // A blip: offline for a moment, then back. Nothing to say.
        assert!(n.update(&view(vec![peer("gpu-box", false)]), t).is_empty());
        assert!(n
            .update(
                &view(vec![peer("gpu-box", true)]),
                t + Duration::from_secs(30)
            )
            .is_empty());

        let t = t + Duration::from_secs(40);
        assert!(n.update(&view(vec![peer("gpu-box", false)]), t).is_empty());
        assert_eq!(
            titles(n.update(&view(vec![peer("gpu-box", false)]), t + OFFLINE_AFTER)),
            vec!["gpu-box went offline"]
        );
        assert!(
            n.update(&view(vec![peer("gpu-box", false)]), t + OFFLINE_AFTER * 5)
                .is_empty(),
            "once"
        );
        assert_eq!(
            titles(n.update(&view(vec![peer("gpu-box", true)]), t + OFFLINE_AFTER * 6)),
            vec!["gpu-box is back online"]
        );
    }

    #[test]
    fn nothing_about_peers_during_warmup() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        // Stale entries from disk look online, then turn out to be gone.
        assert!(n.update(&view(vec![peer("old", true)]), t0).is_empty());
        assert!(n
            .update(
                &view(vec![peer("old", false), peer("new", true)]),
                t0 + Duration::from_secs(5)
            )
            .is_empty());
        assert!(
            n.update(
                &view(vec![peer("old", false), peer("new", true)]),
                t0 + WARMUP * 3
            )
            .is_empty(),
            "never seen online after the warm-up, and new during it"
        );
    }

    #[test]
    fn joins_after_warmup_are_reported_and_grouped() {
        let t0 = Instant::now();
        let mut n = warmed(&["a"], t0);
        let t = t0 + WARMUP * 2;
        let got = n.update(
            &view(vec![peer("a", true), peer("b", true), peer("c", true)]),
            t,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "b and c joined the pool");
        assert!(got[0].body.starts_with("Their models"));
        // A new peer that is not online yet (heard of through gossip) is not news.
        assert!(n
            .update(
                &view(vec![
                    peer("a", true),
                    peer("b", true),
                    peer("c", true),
                    peer("d", false)
                ]),
                t
            )
            .is_empty());
    }

    #[test]
    fn a_refused_secret_is_reported_once_per_episode_even_during_warmup() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        let mut v = view(vec![Peer {
            label: "living-room".into(),
            ..peer("lr", true)
        }]);
        v.secret_refused = vec!["http://lr:11343".into()];
        assert_eq!(
            titles(n.update(&v, t0)),
            vec!["living-room refuses this machine's pool secret"]
        );
        assert!(n.update(&v, t0 + Duration::from_secs(1)).is_empty());
        // Accepted again, then refused again: a new episode.
        let ok = View {
            secret_refused: vec![],
            ..v.clone()
        };
        assert!(n.update(&ok, t0 + Duration::from_secs(2)).is_empty());
        assert_eq!(n.update(&v, t0 + Duration::from_secs(3)).len(), 1);
        // A URL that is not in the table is named by its URL.
        let stranger = View {
            secret_refused: vec!["http://10.0.0.9:11343".into()],
            ..v
        };
        assert_eq!(
            titles(n.update(&stranger, t0 + Duration::from_secs(4))),
            vec!["http://10.0.0.9:11343 refuses this machine's pool secret"]
        );
    }

    #[test]
    fn a_failed_start_is_reported_once_until_it_changes() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        let failed = |generation, e: &str| View {
            generation,
            start_error: Some(e.into()),
            ..Default::default()
        };
        let got = n.update(&failed(0, "port 11343 is already in use"), t0);
        assert_eq!(titles(got), vec!["Token Share could not start"]);
        assert!(n
            .update(&failed(0, "port 11343 is already in use"), t0)
            .is_empty());
        // Saving settings retries; the same failure is not news.
        assert!(n
            .update(&failed(0, "port 11343 is already in use"), t0)
            .is_empty());
        assert_eq!(n.update(&failed(0, "bad config"), t0).len(), 1);
        // Running fine, then failing the same way again later, is.
        assert!(n.update(&view(vec![]), t0).is_empty());
        assert_eq!(n.update(&failed(0, "bad config"), t0).len(), 1);
    }

    #[test]
    fn a_restart_starts_over() {
        let t0 = Instant::now();
        let mut n = warmed(&["a"], t0);
        let mut v = view(vec![peer("a", false)]);
        v.generation = 2;
        // The new delegate has not seen `a` online yet, so its absence is not news.
        assert!(n.update(&v, t0 + WARMUP * 2).is_empty());
        assert!(n.update(&v, t0 + WARMUP * 10).is_empty());
    }

    #[test]
    fn forgotten_peers_are_dropped_quietly() {
        let t0 = Instant::now();
        let mut n = warmed(&["a"], t0);
        let t = t0 + WARMUP;
        assert!(n.update(&view(vec![peer("a", false)]), t).is_empty());
        assert!(n.update(&view(vec![]), t + OFFLINE_AFTER * 2).is_empty());
    }

    #[test]
    fn many_names_are_shortened() {
        let n = notice(Kind::Offline, &["a", "b", "c", "d", "e"]);
        assert_eq!(n.title, "a, b and 3 others went offline");
        assert!(n.body.contains("only they have"));
        assert_eq!(
            notice(Kind::Back, &["a", "b", "c"]).title,
            "a, b and c are back online"
        );
    }
}
