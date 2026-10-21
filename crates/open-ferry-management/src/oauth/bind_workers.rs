//! Independent admission and shutdown accounting for dedicated SIWC workers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

pub(super) const WORKER_LIMIT: usize = 32;
pub(super) const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug)]
pub(crate) struct BindWorkers {
    permits: Arc<Semaphore>,
    workers: AtomicUsize,
    completions: AtomicUsize,
    changed: Notify,
}

impl Default for BindWorkers {
    fn default() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(WORKER_LIMIT)),
            workers: AtomicUsize::new(0),
            completions: AtomicUsize::new(0),
            changed: Notify::new(),
        }
    }
}

impl BindWorkers {
    pub(super) fn available(&self) -> bool {
        self.permits.available_permits() != 0
    }

    pub(super) fn try_worker(self: &Arc<Self>) -> Result<Worker, super::siwc_bind::SaveError> {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| super::siwc_bind::SaveError::Busy)?;
        self.workers.fetch_add(1, Ordering::AcqRel);
        Ok(Worker {
            registry: self.clone(),
            permit: Some(permit),
        })
    }

    pub(super) fn completion(self: &Arc<Self>) -> Completion {
        self.completions.fetch_add(1, Ordering::AcqRel);
        Completion(self.clone())
    }

    pub(crate) fn counts(&self) -> (usize, usize) {
        (
            self.workers.load(Ordering::Acquire),
            self.completions.load(Ordering::Acquire),
        )
    }

    pub(super) async fn idle(&self) {
        loop {
            // Register before checking: the last drop must not be lost.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.counts() == (0, 0) {
                return;
            }
            changed.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn limit_for_test(&self, limit: usize) {
        assert_eq!(self.counts(), (0, 0));
        assert!(limit <= WORKER_LIMIT);
        self.permits.forget_permits(WORKER_LIMIT - limit);
    }
}

pub(super) struct Worker {
    registry: Arc<BindWorkers>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        drop(self.permit.take());
        self.registry.workers.fetch_sub(1, Ordering::AcqRel);
        self.registry.changed.notify_waiters();
    }
}

pub(super) struct Completion(Arc<BindWorkers>);

impl Drop for Completion {
    fn drop(&mut self) {
        self.0.completions.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}
