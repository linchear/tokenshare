use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Bounds concurrent requests sent to ollama. "Full" is a local counter.
///
/// Requests from other delegates take a slot like any other, and also one of
/// the peer slots (`max_peer_requests`), so peers can be kept to part of the
/// pool and the rest left for this machine's own users.
#[derive(Clone)]
pub struct Pool {
    sem: Arc<Semaphore>,
    size: usize,
    peer_sem: Arc<Semaphore>,
    peer_size: usize,
    forwarding: Arc<AtomicUsize>,
}

/// A slot in the pool, freed when dropped.
pub struct Slot {
    _local: OwnedSemaphorePermit,
    _peer: Option<OwnedSemaphorePermit>,
}

/// Counts one request we are forwarding to a peer, for as long as it is held.
pub struct ForwardGuard(Arc<AtomicUsize>);

impl Drop for ForwardGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Pool {
    /// A pool of `size` slots, all of which peers may use.
    pub fn new(size: usize) -> Self {
        Self::with_peer_limit(size, size)
    }

    /// A pool of `size` slots, at most `peer_size` of them used by peers.
    pub fn with_peer_limit(size: usize, peer_size: usize) -> Self {
        let peer_size = peer_size.min(size);
        Self {
            sem: Arc::new(Semaphore::new(size)),
            size,
            peer_sem: Arc::new(Semaphore::new(peer_size)),
            peer_size,
            forwarding: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A slot, or `None` if the pool is full (or, for a peer's request, if
    /// peers already have all the slots they may use).
    pub fn try_acquire(&self, for_peer: bool) -> Option<Slot> {
        let peer = if for_peer {
            Some(self.peer_sem.clone().try_acquire_owned().ok()?)
        } else {
            None
        };
        Some(Slot {
            _local: self.sem.clone().try_acquire_owned().ok()?,
            _peer: peer,
        })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// How many slots peers may use at once.
    pub fn peer_size(&self) -> usize {
        self.peer_size
    }

    /// Slots in use by peers' requests.
    pub fn peer_in_use(&self) -> usize {
        self.peer_size - self.peer_sem.available_permits()
    }

    /// How many more requests a peer could start right now: limited by both
    /// the peer slots and the pool as a whole.
    pub fn peer_free(&self) -> usize {
        self.peer_sem
            .available_permits()
            .min(self.sem.available_permits())
    }

    pub fn track_forward(&self) -> ForwardGuard {
        self.forwarding.fetch_add(1, Ordering::SeqCst);
        ForwardGuard(self.forwarding.clone())
    }

    /// Requests we are forwarding to peers right now.
    pub fn forwarding(&self) -> usize {
        self.forwarding.load(Ordering::SeqCst)
    }

    /// Requests in flight through this delegate, served locally or forwarded.
    pub fn active(&self) -> usize {
        self.in_use() + self.forwarding()
    }

    pub fn in_use(&self) -> usize {
        self.size - self.sem.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_and_released() {
        let p = Pool::new(2);
        let a = p.try_acquire(false).unwrap();
        let _b = p.try_acquire(true).unwrap();
        assert!(p.try_acquire(false).is_none());
        assert_eq!(p.in_use(), 2);
        drop(a);
        assert_eq!(p.in_use(), 1);
        assert!(p.try_acquire(false).is_some());
    }

    #[test]
    fn peers_are_kept_to_their_share_and_local_users_get_the_rest() {
        let p = Pool::with_peer_limit(3, 1);
        assert_eq!((p.peer_size(), p.peer_free()), (1, 1));
        let peer = p.try_acquire(true).unwrap();
        assert_eq!((p.in_use(), p.peer_in_use(), p.peer_free()), (1, 1, 0));
        assert!(p.try_acquire(true).is_none(), "peers have their one slot");
        let _a = p.try_acquire(false).unwrap();
        let b = p.try_acquire(false).unwrap();
        assert!(p.try_acquire(false).is_none());
        drop(peer);
        assert_eq!(p.peer_in_use(), 0);
        assert_eq!(p.peer_free(), 1);
        let _c = p.try_acquire(false).unwrap();
        assert_eq!(p.peer_free(), 0, "a full pool has nothing for peers either");
        assert!(
            p.try_acquire(true).is_none(),
            "and a refused peer request holds nothing"
        );
        assert_eq!(p.peer_in_use(), 0);
        drop(b);
        assert!(p.try_acquire(true).is_some());
    }

    #[test]
    fn the_peer_limit_never_exceeds_the_pool() {
        let p = Pool::with_peer_limit(2, 5);
        assert_eq!(p.peer_size(), 2);
        assert_eq!(Pool::new(4).peer_size(), 4);
    }

    #[test]
    fn forwarded_requests_count_as_active_but_not_as_slots() {
        let p = Pool::new(1);
        let g = p.track_forward();
        assert_eq!((p.in_use(), p.forwarding(), p.active()), (0, 1, 1));
        assert!(p.try_acquire(false).is_some());
        drop(g);
        assert_eq!(p.forwarding(), 0);
    }
}
