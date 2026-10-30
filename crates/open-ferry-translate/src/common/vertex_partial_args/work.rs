use super::{ArgumentError, MAX_WORK, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Shared by compatibility probes, argument assembly, and schema cleaning.
/// Exhaustion is sticky even when a compatibility check only returns a bool.
#[derive(Clone, Default)]
pub(crate) struct WorkBudget(Arc<AtomicUsize>);

impl WorkBudget {
    pub(crate) fn charge(&self, amount: usize) -> Result<()> {
        let _ = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |work| {
                Some(work.saturating_add(amount))
            });
        self.check()
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Relaxed) > MAX_WORK {
            Err(ArgumentError("Vertex argument work limit exceeded"))
        } else {
            Ok(())
        }
    }
}
