// Ported from CLIProxyAPI sdk/cliproxy/auth/codex_siwc_bind.go
// (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Unlike upstream's per-credential mutation gate, bind uses the global
//! reload barrier. Reservation and commit stay on one running blocking
//! thread. Only the bounded shared drain may wait; all other locks are tries.

use super::{Manager, lock};
use crate::auth::{Auth, AuthKind, AuthSource};
use std::io;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::TryLockError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Maximum shared-drain wait per bind attempt, independent of Tokio progress.
pub const SIWC_BIND_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
const CANCEL_POLL: Duration = Duration::from_millis(10);

/// Thread-confined reservation: it cannot be moved into a queued task.
/// A caller must release it before waiting for any file/management lock.
#[doc(hidden)]
pub struct SiwcBindReservation<'a> {
    manager: &'a Manager,
    started: Instant,
    exclusive: bool,
    _thread: PhantomData<Rc<()>>,
}

impl SiwcBindReservation<'_> {
    /// Waits only for existing shared holders, for at most 100 ms. A reload
    /// arriving meanwhile wins, and cancellation is sampled every 10 ms.
    pub fn wait_drained(&self, cancelled: &AtomicBool) -> bool {
        let barrier = &self.manager.shared.load_gate;
        let deadline = self.started + SIWC_BIND_DRAIN_TIMEOUT;
        let mut state = lock(&barrier.state);
        loop {
            if cancelled.load(Ordering::Acquire)
                || Instant::now() >= deadline
                || state.exclusive
                || state.waiting_exclusive != 1
            {
                return false;
            }
            if state.shared == 0 {
                return true;
            }
            let wait = CANCEL_POLL.min(deadline.saturating_duration_since(Instant::now()));
            (state, _) = barrier
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Converts to exclusive admission without waiting. The caller's closure
    /// must only try other locks and must not call back into the manager.
    pub fn try_commit<T>(
        &mut self,
        expected: &Auth,
        cancelled: &AtomicBool,
        commit: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        if cancelled.load(Ordering::Acquire) || self.started.elapsed() >= SIWC_BIND_DRAIN_TIMEOUT {
            return Ok(None);
        }
        {
            let mut state = match self.manager.shared.load_gate.state.try_lock() {
                Ok(state) => state,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => return Ok(None),
            };
            if state.shared != 0 || state.exclusive || state.waiting_exclusive != 1 {
                return Ok(None);
            }
            state.waiting_exclusive -= 1;
            state.exclusive = true;
            self.exclusive = true;
        }
        let current = {
            let state = match self.manager.shared.state.try_lock() {
                Ok(state) => state,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => return Ok(None),
            };
            state
                .auths
                .get(&expected.id)
                .map(|entry| entry.auth.clone())
                .ok_or_else(|| io::Error::other("SIWC bind registration changed"))?
        };
        validate_registration(&current, expected)?;
        commit().map(Some)
    }
}

impl Drop for SiwcBindReservation<'_> {
    fn drop(&mut self) {
        let barrier = &self.manager.shared.load_gate;
        let mut state = lock(&barrier.state);
        if self.exclusive {
            state.exclusive = false;
        } else {
            state.waiting_exclusive -= 1;
        }
        state.bind_reserved = false;
        barrier.changed.notify_all();
    }
}

impl Manager {
    /// Call only inside work already running on a blocking thread. At most
    /// one bind reserves, and parked shared callers get their turn first.
    #[doc(hidden)]
    pub fn reserve_siwc_bind(&self) -> Option<SiwcBindReservation<'_>> {
        let mut state = match self.shared.load_gate.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        if state.bind_reserved
            || state.exclusive
            || state.waiting_exclusive != 0
            || state.waiting_shared != 0
        {
            return None;
        }
        state.bind_reserved = true;
        state.waiting_exclusive += 1;
        Some(SiwcBindReservation {
            manager: self,
            started: Instant::now(),
            exclusive: false,
            _thread: PhantomData,
        })
    }

    /// Non-waiting bridge retained for synchronous callers/tests. Async bind
    /// saves instead reserve and drain inside one already-running worker.
    #[doc(hidden)]
    pub fn try_commit_siwc_bind<T>(
        &self,
        expected: &Auth,
        commit: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        let Some(mut reservation) = self.reserve_siwc_bind() else {
            return Ok(None);
        };
        if lock(&self.shared.load_gate.state).shared != 0 {
            return Ok(None);
        }
        reservation.try_commit(expected, &AtomicBool::new(false), commit)
    }
}

fn validate_registration(current: &Auth, expected: &Auth) -> io::Result<()> {
    if !current.provider.trim().eq_ignore_ascii_case("codex")
        || current.auth_kind() != Some(AuthKind::OAuth)
        || current.auth_source_kind() != Some(AuthSource::File)
        || current
            .attribute("path")
            .unwrap_or_default()
            .trim()
            .is_empty()
        || ["access_token", "refresh_token"].iter().any(|key| {
            current
                .metadata_str(key)
                .is_none_or(|value| value.trim().is_empty())
        })
        || expected.registration_epoch == 0
        || current.registration_epoch != expected.registration_epoch
        || current.provider != expected.provider
        || current.file_name != expected.file_name
        || current.attribute("path") != expected.attribute("path")
        || current.auth_source_kind() != expected.auth_source_kind()
        || current.auth_kind() != expected.auth_kind()
    {
        return Err(io::Error::other("SIWC bind registration changed"));
    }
    crate::auth::file_store::check_bind_metadata(&expected.metadata, &current.metadata)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::Settings;
    use crate::registry::ModelRegistry;
    use std::sync::Arc;

    fn setup() -> (Manager, Arc<Auth>) {
        let manager = Manager::new(Settings::default(), Arc::new(ModelRegistry::new()), None);
        let expected = manager
            .register_unsaved(Auth {
                id: "dummy.json".into(),
                provider: "codex".into(),
                file_name: "dummy.json".into(),
                attributes: [("path".to_owned(), "dummy.json".to_owned())].into(),
                metadata: serde_json::json!({"type":"codex", "access_token":"dummy-access", "refresh_token":"dummy-refresh"}).as_object().unwrap().clone(),
                ..Auth::default()
            })
            .unwrap();
        (manager, expected)
    }

    #[test]
    fn registration_replacement_and_busy_admission_never_commit() {
        let (manager, expected) = setup();
        let shared = manager.mutation_gate();
        assert!(
            manager
                .try_commit_siwc_bind(&expected, || Ok(()))
                .unwrap()
                .is_none()
        );
        drop(shared);
        manager.remove(&expected.id);
        manager.register_unsaved(Auth::clone(&expected)).unwrap();
        assert!(
            manager
                .try_commit_siwc_bind(&expected, || -> io::Result<()> {
                    panic!("stale registration committed")
                })
                .is_err()
        );
    }

    #[test]
    fn registration_mutation_waits_for_a_durable_commit() {
        let (manager, expected) = setup();
        let (started, waiting) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let mut task = None;
        manager
            .try_commit_siwc_bind(&expected, || {
                let changing = manager.clone();
                let id = expected.id.clone();
                task = Some(std::thread::spawn(move || {
                    started.send(()).unwrap();
                    changing.remove(&id);
                    finished.send(()).unwrap();
                }));
                waiting.recv().unwrap();
                assert!(
                    done.recv_timeout(std::time::Duration::from_millis(50))
                        .is_err()
                );
                Ok(())
            })
            .unwrap()
            .unwrap();
        done.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        task.unwrap().join().unwrap();
        assert!(manager.get(&expected.id).is_none());
    }
    #[test]
    fn bounded_bind_wait_releases_parked_shared_mutations() {
        let (manager, _) = setup();
        let active = manager.mutation_gate();
        let (started, ready) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let reserving = manager.clone();
        let task = std::thread::spawn(move || {
            let reservation = reserving.reserve_siwc_bind().unwrap();
            started.send(()).unwrap();
            assert!(!reservation.wait_drained(&AtomicBool::new(false)));
            drop(reservation);
            finished.send(()).unwrap();
        });
        ready.recv().unwrap();
        let changing = manager.clone();
        let shared = std::thread::spawn(move || {
            let _gate = changing.mutation_gate();
        });
        done.recv_timeout(Duration::from_millis(800)).unwrap();
        shared.join().unwrap();
        drop(active);
        task.join().unwrap();
    }

    #[test]
    fn only_one_bind_reserves_and_existing_shared_waiters_go_next() {
        let (manager, _) = setup();
        let reservation = manager.reserve_siwc_bind().unwrap();
        assert!(manager.reserve_siwc_bind().is_none());
        let changing = manager.clone();
        let task = std::thread::spawn(move || {
            let _gate = changing.mutation_gate();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock(&manager.shared.load_gate.state).waiting_shared == 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        drop(reservation);
        // The queued shared caller either still waits (blocking a new bind)
        // or already entered. It cannot be overtaken before admission.
        let state = lock(&manager.shared.load_gate.state);
        if state.waiting_shared != 0 {
            assert!(!state.bind_reserved);
        }
        drop(state);
        task.join().unwrap();
    }
}
