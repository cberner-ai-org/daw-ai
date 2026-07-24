use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    use super::*;

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
