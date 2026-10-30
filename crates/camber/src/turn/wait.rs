//! Queue accounting for one registered, undelivered acquisition.

use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct TurnWait<'a> {
    count: &'a AtomicUsize,
    registered: bool,
}

impl<'a> TurnWait<'a> {
    pub(super) const fn new(count: &'a AtomicUsize) -> Self {
        Self {
            count,
            registered: false,
        }
    }

    pub(super) fn register(&mut self) {
        if !self.registered {
            self.count.fetch_add(1, Ordering::AcqRel);
            self.registered = true;
        }
    }

    pub(super) fn finish(&mut self) {
        if self.registered {
            self.count.fetch_sub(1, Ordering::AcqRel);
            self.registered = false;
        }
    }
}

impl Drop for TurnWait<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}
