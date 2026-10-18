use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Bounds concurrent requests sent to ollama. "Full" is a local counter.
#[derive(Clone)]
pub struct Pool {
    sem: Arc<Semaphore>,
    size: usize,
}

impl Pool {
    pub fn new(size: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(size)),
            size,
        }
    }

    /// A slot, or `None` if the pool is full. The slot frees when the permit drops.
    pub fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.sem.clone().try_acquire_owned().ok()
    }

    pub fn size(&self) -> usize {
        self.size
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
}
