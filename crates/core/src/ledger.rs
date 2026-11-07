//! What this machine and each peer have done for each other: requests and
//! tokens this machine *lent* (served from its ollama for a peer) and
//! *borrowed* (had a peer serve for one of its users). Kept per peer id and
//! saved next to the delegate table, so it survives restarts.
//!
//! It is this delegate's own view, from responses it saw end. It is for
//! people to look at; nothing is enforced with it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::table::now_ms;

/// Requests and tokens in one direction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Tally {
    pub fn tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }

    fn add(&mut self, other: &Tally) {
        self.requests += other.requests;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
    }
}

/// Both directions with one peer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    /// Served here for the peer.
    pub lent: Tally,
    /// Served by the peer for this machine.
    pub borrowed: Tally,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Lent,
    Borrowed,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    /// Unix ms when counting started.
    since: u64,
    peers: BTreeMap<String, Balance>,
}

pub struct Ledger {
    path: Option<PathBuf>,
    book: Mutex<Book>,
    dirty: AtomicBool,
}

impl Ledger {
    /// The ledger saved at `path`, or a new one. `None` keeps it in memory.
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut book = Book::default();
        if let Some(p) = &path {
            match std::fs::read_to_string(p) {
                Ok(text) => match serde_json::from_str(&text) {
                    Ok(b) => book = b,
                    Err(e) => warn!(path = %p.display(), error = %e, "ignoring unreadable ledger"),
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!(path = %p.display(), error = %e, "cannot read ledger"),
            }
        }
        if book.since == 0 {
            book.since = now_ms();
        }
        Self {
            path,
            book: Mutex::new(book),
            dirty: AtomicBool::new(false),
        }
    }

    /// A response for or from `peer` ended well. Token counts are those the
    /// final chunk reported, if it did.
    pub fn record(
        &self,
        peer: &str,
        direction: Direction,
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
    ) {
        let one = Tally {
            requests: 1,
            prompt_tokens: prompt_tokens.unwrap_or(0),
            completion_tokens: completion_tokens.unwrap_or(0),
        };
        let mut b = self.book.lock().unwrap();
        let balance = b.peers.entry(peer.to_string()).or_default();
        match direction {
            Direction::Lent => balance.lent.add(&one),
            Direction::Borrowed => balance.borrowed.add(&one),
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// When counting started (unix ms).
    pub fn since(&self) -> u64 {
        self.book.lock().unwrap().since
    }

    /// Every peer's balance, by id.
    pub fn balances(&self) -> BTreeMap<String, Balance> {
        self.book.lock().unwrap().peers.clone()
    }

    /// Everything lent and borrowed, all peers together.
    pub fn totals(&self) -> Balance {
        let mut t = Balance::default();
        for b in self.book.lock().unwrap().peers.values() {
            t.lent.add(&b.lent);
            t.borrowed.add(&b.borrowed);
        }
        t
    }

    /// Save if anything changed since the last save. Cheap when nothing did.
    pub fn flush(&self) {
        let Some(path) = &self.path else { return };
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let text = serde_json::to_vec_pretty(&*self.book.lock().unwrap());
        let write = || -> std::io::Result<()> {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, text?)?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            self.dirty.store(true, Ordering::SeqCst);
            warn!(path = %path.display(), error = %e, "cannot save ledger");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_directions_add_up_per_peer() {
        let l = Ledger::load(None);
        l.record("a", Direction::Lent, Some(10), Some(90));
        l.record("a", Direction::Lent, None, None);
        l.record("a", Direction::Borrowed, Some(5), Some(7));
        l.record("b", Direction::Borrowed, Some(1), Some(2));
        let a = l.balances()["a"];
        assert_eq!(
            a.lent,
            Tally {
                requests: 2,
                prompt_tokens: 10,
                completion_tokens: 90
            }
        );
        assert_eq!(a.borrowed.tokens(), 12);
        let t = l.totals();
        assert_eq!((t.lent.requests, t.borrowed.requests), (2, 2));
        assert_eq!(t.borrowed.tokens(), 15);
    }

    #[test]
    fn it_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("ledger-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.json");

        let l = Ledger::load(Some(path.clone()));
        let since = l.since();
        l.flush();
        assert!(!path.exists(), "nothing to save yet");
        l.record("a", Direction::Lent, Some(1), Some(2));
        l.flush();
        drop(l);

        let again = Ledger::load(Some(path.clone()));
        assert_eq!(again.since(), since, "counting since the first start");
        assert_eq!(again.balances()["a"].lent.tokens(), 3);

        std::fs::write(&path, "not json").unwrap();
        assert!(Ledger::load(Some(path)).balances().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
