use super::{ArgumentError, MAX_WORK, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Shared by compatibility probes, argument assembly, and schema cleaning.
/// Exhaustion is sticky even when a compatibility check only returns a bool.
#[derive(Clone, Default)]
pub(crate) struct WorkBudget(Arc<Counters>);

#[derive(Default)]
struct Counters {
    total: AtomicUsize,
    predicate: AtomicUsize,
    #[cfg(test)]
    predicate_allowance: AtomicUsize,
    #[cfg(test)]
    analyses: std::sync::Mutex<std::collections::HashSet<(usize, usize, usize, usize)>>,
}

#[cfg(test)]
thread_local! {
    static TEST_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_WORK) };
    static AUDIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl WorkBudget {
    pub(crate) fn charge(&self, amount: usize) -> Result<()> {
        let _ = self
            .0
            .total
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |work| {
                Some(work.saturating_add(amount))
            });
        self.check()
    }

    pub(crate) fn charge_predicate(&self, amount: usize) -> Result<()> {
        self.0.predicate.fetch_add(amount, Ordering::Relaxed);
        self.charge(amount)
    }

    #[cfg(test)]
    pub(crate) fn test_unbounded() -> bool {
        TEST_LIMIT.get() == usize::MAX
    }

    #[cfg(test)]
    pub(crate) fn with_index_audit<T>(run: impl FnOnce() -> T) -> T {
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                AUDIT.set(self.0);
            }
        }
        let _reset = Reset(AUDIT.replace(true));
        run()
    }

    #[cfg(test)]
    pub(crate) fn record_analysis(&self, node: usize, scope: usize, depth: usize, active: usize) {
        if AUDIT.get() {
            assert!(
                self.0
                    .analyses
                    .lock()
                    .unwrap()
                    .insert((node, scope, depth, active)),
                "resolved schema node was re-analyzed in one guard context"
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn predicate_used(&self) -> usize {
        self.0.predicate.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn add_predicate_allowance(&self, amount: usize) {
        self.0
            .predicate_allowance
            .fetch_add(amount, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn predicate_allowance(&self) -> usize {
        self.0.predicate_allowance.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn with_test_limit<T>(limit: usize, run: impl FnOnce() -> T) -> T {
        struct Reset(usize);
        impl Drop for Reset {
            fn drop(&mut self) {
                TEST_LIMIT.set(self.0);
            }
        }
        let _reset = Reset(TEST_LIMIT.replace(limit));
        run()
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.0.total.load(Ordering::Relaxed)
    }

    pub(crate) fn check(&self) -> Result<()> {
        #[cfg(test)]
        let limit = TEST_LIMIT.get();
        #[cfg(not(test))]
        let limit = MAX_WORK;
        if self.0.total.load(Ordering::Relaxed) > limit {
            Err(ArgumentError("Vertex argument work limit exceeded"))
        } else {
            Ok(())
        }
    }
}
