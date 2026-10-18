use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Bounds concurrent requests sent to ollama. "Full" is a local counter.
#[derive(Clone)]
pub struct Pool {
    sem: Arc<Semaphore>,
    size: usize,
    forwarding: Arc<AtomicUsize>,
}

/// Counts one request we are forwarding to a peer, for as long as it is held.
pub struct ForwardGuard(Arc<AtomicUsize>);

impl Drop for ForwardGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Pool {
    pub fn new(size: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(size)),
            size,
            forwarding: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A slot, or `None` if the pool is full. The slot frees when the permit drops.
    pub fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.sem.clone().try_acquire_owned().ok()
    }

    pub fn size(&self) -> usize {
        self.size
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
        let a = p.try_acquire().unwrap();
        let _b = p.try_acquire().unwrap();
        assert!(p.try_acquire().is_none());
        assert_eq!(p.in_use(), 2);
        drop(a);
        assert_eq!(p.in_use(), 1);
        assert!(p.try_acquire().is_some());
    }

    #[test]
    fn forwarded_requests_count_as_active_but_not_as_slots() {
        let p = Pool::new(1);
        let g = p.track_forward();
        assert_eq!((p.in_use(), p.forwarding(), p.active()), (0, 1, 1));
        assert!(p.try_acquire().is_some());
        drop(g);
        assert_eq!(p.forwarding(), 0);
    }
}
