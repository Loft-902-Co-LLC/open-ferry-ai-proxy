// Ported from CLIProxyAPI sdk/cliproxy/auth/codex_siwc_bind.go
// (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Binds use global reload admission with FIFO turns and bounded drains.
//! FIFO turns and reservations remain on one running, dedicated worker.
//! After a bind commits, fresh-read adoption uses lifecycle's common credential
//! replacement recovery: a changed SIWC bearer clears refresh failures; an
//! unchanged bearer retains them. No recovery state is changed before commit.

use super::{Manager, lock};
use crate::auth::{Auth, AuthKind, AuthSource};
use std::io;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::TryLockError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Maximum shared-drain wait per production attempt, independent of Tokio.
pub const SIWC_BIND_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
const CANCEL_POLL: Duration = Duration::from_millis(10);

/// The outcome distinguishes a slow drain from reload/cancellation.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiwcBindDrain {
    Ready,
    TimedOut,
    Interrupted,
}

/// A FIFO place retained across attempts, but never across an await.
#[doc(hidden)]
pub struct SiwcBindTurn<'a> {
    manager: &'a Manager,
    id: String,
    ticket: u64,
    _thread: PhantomData<Rc<()>>,
}

/// Thread-confined reservation, separate from the longer-lived FIFO turn.
#[doc(hidden)]
pub struct SiwcBindReservation<'a> {
    turn: &'a SiwcBindTurn<'a>,
    exclusive: bool,
}

pub(super) struct BindTicket {
    number: u64,
    id: String,
    not_before: Option<Instant>,
}

impl super::ReloadState {
    fn bind_front(&self) -> Option<u64> {
        let now = Instant::now();
        self.tickets
            .iter()
            .find(|ticket| {
                ticket.not_before.is_none_or(|deadline| now >= deadline)
                    && !self
                        .tickets
                        .iter()
                        .any(|older| older.id == ticket.id && older.number < ticket.number)
            })
            .map(|ticket| ticket.number)
    }
}

impl SiwcBindTurn<'_> {
    /// The oldest ready turn may reserve. A credential-lock miss defers its
    /// turn for other IDs; turns on that same ID keep their original order.
    /// Parked shared callers enter before the next reservation.
    pub fn reserve_siwc_bind(&self) -> Option<SiwcBindReservation<'_>> {
        let mut state = match self.manager.shared.load_gate.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        // The writer count includes the current bind. Refuse ordinary reload
        // waiters independently from the one-reservation rule.
        if state.bind_reserved
            || state.exclusive
            || state.waiting_exclusive != usize::from(state.bind_reserved)
            || state.waiting_shared != 0
            || state.bind_front() != Some(self.ticket)
        {
            return None;
        }
        state.bind_reserved = true;
        state.waiting_exclusive += 1;
        Some(SiwcBindReservation {
            turn: self,
            exclusive: false,
        })
    }

    /// A busy credential yields to other credentials during its backoff.
    /// Keep its original number so a later turn for the same ID cannot pass it.
    /// Call only after dropping the reservation and all transaction guards.
    pub fn defer_file_retry(&self, duration: Duration) {
        let barrier = &self.manager.shared.load_gate;
        let mut state = lock(&barrier.state);
        let index = state
            .tickets
            .iter()
            .position(|ticket| ticket.number == self.ticket)
            .expect("live bind ticket");
        let mut ticket = state.tickets.remove(index).expect("live bind ticket");
        ticket.not_before = Some(Instant::now() + duration);
        state.tickets.push_back(ticket);
        barrier.changed.notify_all();
    }

    /// Back off with no reservation/locks. Does not occupy Tokio's blocking
    /// pool; cancellation and ticket cleanup need only this worker.
    pub fn wait_retry(&self, duration: Duration, cancelled: &AtomicBool) -> bool {
        let barrier = &self.manager.shared.load_gate;
        let deadline = Instant::now() + duration;
        let mut state = lock(&barrier.state);
        while !cancelled.load(Ordering::Acquire) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            (state, _) = barrier
                .changed
                .wait_timeout(state, CANCEL_POLL.min(remaining))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        false
    }
}

impl SiwcBindReservation<'_> {
    /// Waits for existing global shared holders. The budget ends
    /// when draining completes: later filesystem tries don't consume it.
    pub fn wait_drained(&self, cancelled: &AtomicBool, budget: Duration) -> SiwcBindDrain {
        let barrier = &self.turn.manager.shared.load_gate;
        let deadline = Instant::now() + budget;
        let mut state = lock(&barrier.state);
        loop {
            if cancelled.load(Ordering::Acquire) || state.exclusive || state.waiting_exclusive != 1
            {
                return SiwcBindDrain::Interrupted;
            }
            if state.shared == 0 {
                return SiwcBindDrain::Ready;
            }
            if Instant::now() >= deadline {
                return SiwcBindDrain::TimedOut;
            }
            let wait = CANCEL_POLL.min(deadline.saturating_duration_since(Instant::now()));
            (state, _) = barrier
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Converts to global exclusive admission without a lock wait. The drain
    /// deadline does not charge subsequent filesystem tries or the commit.
    pub fn try_commit<T>(
        &mut self,
        expected: &Auth,
        cancelled: &AtomicBool,
        commit: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        if expected.id != self.turn.id {
            return Err(io::Error::other("SIWC bind credential changed"));
        }
        if !self.try_exclusive(cancelled) {
            return Ok(None);
        }
        let current = {
            let state = match self.turn.manager.shared.state.try_lock() {
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
    fn try_exclusive(&mut self, cancelled: &AtomicBool) -> bool {
        if cancelled.load(Ordering::Acquire) {
            return false;
        }
        {
            let mut state = match self.turn.manager.shared.load_gate.state.try_lock() {
                Ok(state) => state,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => return false,
            };
            if state.shared != 0 || state.exclusive || state.waiting_exclusive != 1 {
                return false;
            }
            state.waiting_exclusive -= 1;
            state.exclusive = true;
            self.exclusive = true;
        }
        true
    }
}

impl Drop for SiwcBindReservation<'_> {
    fn drop(&mut self) {
        let barrier = &self.turn.manager.shared.load_gate;
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

impl Drop for SiwcBindTurn<'_> {
    fn drop(&mut self) {
        let barrier = &self.manager.shared.load_gate;
        let mut state = lock(&barrier.state);
        state.tickets.retain(|ticket| ticket.number != self.ticket);
        barrier.changed.notify_all();
    }
}

impl Manager {
    /// Create only on the running worker that owns the complete retry loop.
    #[doc(hidden)]
    pub fn siwc_bind_turn(&self, id: &str) -> SiwcBindTurn<'_> {
        let mut state = lock(&self.shared.load_gate.state);
        state.next_ticket = state
            .next_ticket
            .checked_add(1)
            .expect("bind ticket exhausted");
        let ticket = state.next_ticket;
        state.tickets.push_back(BindTicket {
            number: ticket,
            id: id.to_owned(),
            not_before: None,
        });
        SiwcBindTurn {
            manager: self,
            id: id.to_owned(),
            ticket,
            _thread: PhantomData,
        }
    }

    /// Nonwaiting bridge for synchronous callers/tests.
    #[doc(hidden)]
    pub fn try_commit_siwc_bind<T>(
        &self,
        expected: &Auth,
        commit: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        let turn = self.siwc_bind_turn(&expected.id);
        let Some(mut reservation) = turn.reserve_siwc_bind() else {
            return Ok(None);
        };
        let cancelled = AtomicBool::new(false);
        if reservation.wait_drained(&cancelled, Duration::ZERO) != SiwcBindDrain::Ready {
            return Ok(None);
        }
        reservation.try_commit(expected, &cancelled, commit)
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
    fn round3_only_one_global_reservation() {
        let (manager, expected) = setup();
        let turn = manager.siwc_bind_turn(&expected.id);
        let reservation = turn.reserve_siwc_bind().unwrap();
        assert!(
            turn.reserve_siwc_bind().is_none(),
            "one turn obtained two reservations"
        );
        drop(reservation);
        assert!(turn.reserve_siwc_bind().is_some());
    }

    #[test]
    fn round3_waiting_shared_is_admitted_before_the_next_reservation() {
        let (manager, expected) = setup();
        let turn = manager.siwc_bind_turn(&expected.id);
        let reservation = turn.reserve_siwc_bind().unwrap();
        let changing = manager.clone();
        let waiter = std::thread::spawn(move || {
            let _gate = changing.mutation_gate();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut state = lock(&manager.shared.load_gate.state);
            if state.waiting_shared != 0 {
                // Only controls when the real waiter enters, without making
                // another production admission predicate refuse the bind.
                state.pause_shared = true;
                break;
            }
            drop(state);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        drop(reservation);
        // A single None could be bookkeeping-mutex contention after notify.
        // Retry while the real waiter remains parked to test the policy.
        let overtook = (0..100).any(|_| {
            let admitted = turn.reserve_siwc_bind().is_some();
            std::thread::yield_now();
            admitted
        });
        lock(&manager.shared.load_gate.state).pause_shared = false;
        manager.shared.load_gate.changed.notify_all();
        waiter.join().unwrap();
        assert!(!overtook, "new reservation overtook a parked shared waiter");
    }

    #[test]
    fn round3_fifo_turn_survives_withdrawal_and_backoff() {
        let (manager, expected) = setup();
        let older = manager.siwc_bind_turn(&expected.id);
        let newer = manager.siwc_bind_turn(&expected.id);
        drop(older.reserve_siwc_bind().unwrap());
        assert!(
            newer.reserve_siwc_bind().is_none(),
            "newer turn overtook older after withdrawal"
        );
        drop(older.reserve_siwc_bind().unwrap());
        drop(older);
        assert!(newer.reserve_siwc_bind().is_some());
        drop(newer);
        assert!(lock(&manager.shared.load_gate.state).tickets.is_empty());
    }

    #[test]
    fn round3_panicking_turn_releases_the_fifo_head() {
        let (manager, expected) = setup();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let turn = manager.siwc_bind_turn(&expected.id);
            let _reservation = turn.reserve_siwc_bind().unwrap();
            panic!("test FIFO unwind");
        }));
        assert!(result.is_err());
        let turn = manager.siwc_bind_turn(&expected.id);
        assert!(turn.reserve_siwc_bind().is_some());
    }

    #[test]
    fn round3_global_fifo_orders_different_credentials() {
        let (manager, expected) = setup();
        let older = manager.siwc_bind_turn(&expected.id);
        let newer = manager.siwc_bind_turn("another-credential");
        drop(older.reserve_siwc_bind().unwrap());
        assert!(newer.reserve_siwc_bind().is_none());
        drop(older);
        assert!(newer.reserve_siwc_bind().is_some());
    }

    #[test]
    fn round4_deferred_file_turn_keeps_same_credential_order() {
        let (manager, expected) = setup();
        let older = manager.siwc_bind_turn(&expected.id);
        let same_id = manager.siwc_bind_turn(&expected.id);
        let other = manager.siwc_bind_turn("another-credential");
        older.defer_file_retry(Duration::from_secs(1));
        assert!(same_id.reserve_siwc_bind().is_none());
        drop(
            other
                .reserve_siwc_bind()
                .expect("other credential must bypass the deferred head"),
        );
        drop(other);
        older.defer_file_retry(Duration::ZERO);
        assert!(
            same_id.reserve_siwc_bind().is_none(),
            "requeue reversed same-credential order"
        );
        drop(older.reserve_siwc_bind().unwrap());
        drop(older);
        assert!(same_id.reserve_siwc_bind().is_some());
    }

    #[test]
    fn round3_drain_deadline_does_not_charge_slow_tries() {
        let (manager, expected) = setup();
        let turn = manager.siwc_bind_turn(&expected.id);
        let mut reservation = turn.reserve_siwc_bind().unwrap();
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            reservation.wait_drained(&cancelled, SIWC_BIND_DRAIN_TIMEOUT),
            SiwcBindDrain::Ready
        );
        std::thread::sleep(SIWC_BIND_DRAIN_TIMEOUT + Duration::from_millis(20));
        assert_eq!(
            reservation
                .try_commit(&expected, &cancelled, || Ok(7))
                .unwrap(),
            Some(7)
        );
    }

    #[test]
    fn bounded_bind_wait_releases_parked_shared_mutations() {
        let (manager, expected) = setup();
        let active = manager.mutation_gate();
        let (started, ready) = std::sync::mpsc::channel();
        let reserving = manager.clone();
        let id = expected.id.clone();
        let task = std::thread::spawn(move || {
            let turn = reserving.siwc_bind_turn(&id);
            let reservation = turn.reserve_siwc_bind().unwrap();
            started.send(()).unwrap();
            assert_eq!(
                reservation.wait_drained(&AtomicBool::new(false), SIWC_BIND_DRAIN_TIMEOUT),
                SiwcBindDrain::TimedOut
            );
        });
        ready.recv().unwrap();
        let changing = manager.clone();
        let shared = std::thread::spawn(move || {
            let _gate = changing.mutation_gate();
        });
        task.join().unwrap();
        shared.join().unwrap();
        drop(active);
    }
}
