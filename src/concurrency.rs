use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LockResult, PoisonError};

pub(crate) trait RecoverPoison<T> {
    fn recover_poison(self) -> T;
}

impl<T> RecoverPoison<T> for LockResult<T> {
    fn recover_poison(self) -> T {
        self.unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) struct Limiter {
    active: AtomicUsize,
    maximum: usize,
}

impl Limiter {
    pub(crate) fn new(maximum: usize) -> Arc<Self> {
        assert!(maximum > 0);
        Arc::new(Self {
            active: AtomicUsize::new(0),
            maximum,
        })
    }

    pub(crate) fn acquire(self: &Arc<Self>) -> Option<Permit> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.maximum).then_some(active + 1)
            })
            .ok()
            .map(|_| Permit {
                limiter: Arc::clone(self),
            })
    }
}

pub(crate) struct Permit {
    limiter: Arc<Limiter>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.limiter.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn poisoned_lock_state_remains_available() {
        let value = Arc::new(Mutex::new(0));
        let worker_value = Arc::clone(&value);
        let worker = std::thread::spawn(move || {
            let mut value = worker_value.lock().recover_poison();
            *value = 1;
            panic!("poison lock");
        });
        assert!(worker.join().is_err());
        assert_eq!(*value.lock().recover_poison(), 1);
    }

    #[test]
    fn permits_are_bounded_and_released_on_drop() {
        let limiter = Limiter::new(2);
        let first = limiter.acquire().expect("first permit");
        let second = limiter.acquire().expect("second permit");
        assert!(limiter.acquire().is_none());
        drop(first);
        assert!(limiter.acquire().is_some());
        drop(second);
    }
}
