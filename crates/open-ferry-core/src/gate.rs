//! Not upstream's: the gate that says whether this process may write the
//! credentials and the config.
//!
//! During a blue-green switch only one process may change those files. The
//! [`ActiveGate`] is that process's switch:
//!
//! - `Open`: every writer may write. A process that is not part of a switch
//!   has a gate that is always open, so it behaves as it did before.
//! - `Quiescing`: no new [`Operation`] begins, and a writer outside an
//!   admitted operation is refused. Work an operation already owns finishes,
//!   so a rotated refresh token is saved and not lost.
//! - `Closed`: nothing writes. A standby process starts here.
//!
//! [`close`](ActiveGate::close) goes from `Open` to `Quiescing`, waits for
//! every live operation and permit to end, then goes to `Closed` only if the
//! epoch is still the one it was given. [`reopen`](ActiveGate::reopen) goes
//! back to `Open` and bumps the epoch, so a `close` that was waiting gives
//! up.
//!
//! A writer takes a [`WritePermit`] before it changes a file. Work that
//! must survive a `Quiescing` gate (a refresh, an upload) is an
//! [`Operation`]: it is admitted while the gate is `Open`, travels with the
//! task through [`Operation::scope`], [`Operation::run_blocking`] and
//! [`ActiveGate::spawn`], and lets writers inside it through.
//!
//! An [`Activation`] lets the take-over path bind writers (the auth
//! directory) while the gate is `Closed`. Only [`ActivationKey::activate`]
//! makes one, and only a standby gate hands out the key, so a request
//! handler can't get one.

use std::cell::RefCell;
use std::fmt;
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Where a gate is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateState {
    /// Writers may write.
    Open,
    /// Only work inside an admitted [`Operation`] may write.
    Quiescing,
    /// Nothing writes.
    Closed,
}

impl fmt::Display for GateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "open",
            Self::Quiescing => "quiescing",
            Self::Closed => "closed",
        })
    }
}

/// The gate refused: this process isn't the active one, or is handing over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotActive {
    /// The state the gate was in.
    pub state: GateState,
}

impl fmt::Display for NotActive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "open-ferry is not the active version ({})", self.state)
    }
}

impl std::error::Error for NotActive {}

impl From<NotActive> for io::Error {
    fn from(error: NotActive) -> Self {
        Self::other(error)
    }
}

/// Why [`ActiveGate::close`] didn't close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseError {
    /// The gate was not open, or its epoch is not the one given.
    NotOpen {
        /// The state the gate was in.
        state: GateState,
        /// The gate's epoch.
        epoch: u64,
    },
    /// The gate was reopened while `close` waited.
    Reopened,
}

impl fmt::Display for CloseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotOpen { state, epoch } => {
                write!(
                    f,
                    "the gate is {state} at epoch {epoch}, not the one to close"
                )
            }
            Self::Reopened => f.write_str("the gate was reopened while closing"),
        }
    }
}

impl std::error::Error for CloseError {}

#[derive(Debug)]
struct State {
    state: GateState,
    epoch: u64,
    /// Live operations and write permits.
    live: usize,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<State>,
    idle: Notify,
    /// Told whenever the gate reopens.
    opened: Notify,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn release(&self) {
        let mut state = self.lock();
        state.live = state.live.saturating_sub(1);
        let idle = state.live == 0;
        drop(state);
        if idle {
            self.idle.notify_waiters();
        }
    }
}

/// Whether this process may write the credentials and the config. Cloning
/// gives another handle to the same gate.
#[derive(Clone, Debug)]
pub struct ActiveGate {
    inner: Arc<Inner>,
}

impl Default for ActiveGate {
    fn default() -> Self {
        Self::open()
    }
}

impl ActiveGate {
    fn with_state(state: GateState) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    state,
                    epoch: 0,
                    live: 0,
                }),
                idle: Notify::new(),
                opened: Notify::new(),
            }),
        }
    }

    /// A gate that starts open. A process outside a switch uses one that is
    /// never closed.
    #[must_use]
    pub fn open() -> Self {
        Self::with_state(GateState::Open)
    }

    /// A gate that starts closed, and the key that activates the writers
    /// while it is. The standby process keeps the key for its take-over.
    #[must_use]
    pub fn standby() -> (Self, ActivationKey) {
        let gate = Self::with_state(GateState::Closed);
        let key = ActivationKey {
            gate: Arc::clone(&gate.inner),
        };
        (gate, key)
    }

    /// Where the gate is.
    #[must_use]
    pub fn state(&self) -> GateState {
        self.inner.lock().state
    }

    /// The gate's epoch, which every [`reopen`](Self::reopen) raises.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.inner.lock().epoch
    }

    /// Whether `other` is a handle to this gate.
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Admits a unit of work that keeps its writes through a `Quiescing`
    /// gate. Refused unless the gate is `Open`.
    ///
    /// # Errors
    /// [`NotActive`] when the gate is not open.
    pub fn begin_operation(&self) -> Result<Operation, NotActive> {
        let mut state = self.inner.lock();
        if state.state != GateState::Open {
            return Err(NotActive { state: state.state });
        }
        state.live += 1;
        drop(state);
        Ok(Operation {
            held: Arc::new(Held {
                inner: Arc::clone(&self.inner),
            }),
        })
    }

    /// Permission to change one file. Granted while the gate is `Open`, and
    /// while it is `Quiescing` to code running inside an [`Operation`] of
    /// this gate.
    ///
    /// # Errors
    /// [`NotActive`] when the gate is `Closed`, or `Quiescing` and the
    /// caller is outside an operation.
    pub fn write_permit(&self) -> Result<WritePermit, NotActive> {
        let mut state = self.inner.lock();
        match state.state {
            GateState::Open => {}
            GateState::Quiescing => {
                let inside = Operation::current().is_some_and(|op| op.gate().same(self));
                if !inside {
                    return Err(NotActive { state: state.state });
                }
            }
            GateState::Closed => return Err(NotActive { state: state.state }),
        }
        state.live += 1;
        drop(state);
        Ok(WritePermit {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Starts the hand-over: from `Open` at `epoch` to `Quiescing`, waits
    /// until no operation or permit lives, then goes to `Closed`. The wait
    /// has no limit of its own; bound it with a timeout and call
    /// [`reopen`](Self::reopen) when it expires.
    ///
    /// # Errors
    /// [`CloseError::NotOpen`] when the gate isn't open at `epoch`, and
    /// [`CloseError::Reopened`] when it was reopened while waiting.
    pub async fn close(&self, epoch: u64) -> Result<(), CloseError> {
        self.quiesce(epoch)?;
        self.settle(epoch).await
    }

    /// The first half of [`close`](Self::close): from `Open` at `epoch` to
    /// `Quiescing`, at once. Not upstream's: a caller that must act between
    /// the two states (the worker's barrier, once the gate is closing) calls
    /// this, then [`settle`](Self::settle).
    ///
    /// # Errors
    /// [`CloseError::NotOpen`] when the gate isn't open at `epoch`.
    pub fn quiesce(&self, epoch: u64) -> Result<(), CloseError> {
        let mut state = self.inner.lock();
        if state.state != GateState::Open || state.epoch != epoch {
            return Err(CloseError::NotOpen {
                state: state.state,
                epoch: state.epoch,
            });
        }
        state.state = GateState::Quiescing;
        Ok(())
    }

    /// The second half of [`close`](Self::close): waits, while `Quiescing`
    /// at `epoch`, until nothing lives, then goes to `Closed`.
    ///
    /// # Errors
    /// [`CloseError::Reopened`] when the gate was reopened (or isn't
    /// quiescing at `epoch`) while waiting.
    pub async fn settle(&self, epoch: u64) -> Result<(), CloseError> {
        loop {
            let notified = self.inner.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.inner.lock();
                if state.epoch != epoch || state.state != GateState::Quiescing {
                    return Err(CloseError::Reopened);
                }
                if state.live == 0 {
                    state.state = GateState::Closed;
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    /// Goes back to `Open` from `Quiescing` or `Closed`, raising the epoch
    /// so a `close` still waiting gives up. Returns the new epoch.
    pub fn reopen(&self) -> u64 {
        let mut state = self.inner.lock();
        state.epoch = state.epoch.wrapping_add(1);
        state.state = GateState::Open;
        let epoch = state.epoch;
        drop(state);
        self.inner.idle.notify_waiters();
        self.inner.opened.notify_waiters();
        epoch
    }

    /// Waits until the gate is `Open`; returns at once when it is. A
    /// background loop that must not work while the gate isn't open parks
    /// here instead of polling.
    pub async fn wait_open(&self) {
        loop {
            let notified = self.inner.opened.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state() == GateState::Open {
                return;
            }
            notified.await;
        }
    }

    /// Spawns `future` as a task that owns `operation`: it counts as live
    /// until the task and every other clone of the operation ends, and its
    /// writes pass a `Quiescing` gate.
    ///
    /// # Errors
    /// [`NotActive`] when `operation` belongs to another gate or no runtime
    /// is running.
    pub fn spawn<F>(
        &self,
        operation: &Operation,
        future: F,
    ) -> Result<JoinHandle<F::Output>, NotActive>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if !operation.gate().same(self) {
            return Err(NotActive {
                state: self.state(),
            });
        }
        let handle = tokio::runtime::Handle::try_current().map_err(|_| NotActive {
            state: self.state(),
        })?;
        // This is the spawn the others are told to use: the task carries the
        // operation.
        #[allow(clippy::disallowed_methods)]
        let task = handle.spawn(operation.clone().scope(future));
        Ok(task)
    }
}

/// The live count one operation holds, released when its last clone drops.
#[derive(Debug)]
struct Held {
    inner: Arc<Inner>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.inner.release();
    }
}

tokio::task_local! {
    static CURRENT: Operation;
}

thread_local! {
    static BLOCKING: RefCell<Option<Operation>> = const { RefCell::new(None) };
}

/// A unit of work the gate admitted while open. Cloning shares the same
/// admission; it ends when the last clone drops.
#[derive(Clone, Debug)]
pub struct Operation {
    held: Arc<Held>,
}

impl Operation {
    /// The operation the calling task or blocking closure runs in, if any.
    #[must_use]
    pub fn current() -> Option<Self> {
        CURRENT
            .try_with(Clone::clone)
            .ok()
            .or_else(|| BLOCKING.with(|slot| slot.borrow().clone()))
    }

    /// The operation carried by `run_blocking`, excluding task-local scopes.
    #[must_use]
    pub fn current_blocking() -> Option<Self> {
        BLOCKING.with(|slot| slot.borrow().clone())
    }

    /// The gate that admitted this operation.
    #[must_use]
    pub fn gate(&self) -> ActiveGate {
        ActiveGate {
            inner: Arc::clone(&self.held.inner),
        }
    }

    /// Runs `future` as this operation: [`Operation::current`] answers with
    /// it inside.
    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        CURRENT.scope(self, future).await
    }

    /// Runs `work` on a blocking thread as this operation.
    pub fn run_blocking<T, F>(&self, work: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let operation = self.clone();
        #[allow(clippy::disallowed_methods)] // this is the way to start one with the operation
        tokio::task::spawn_blocking(move || {
            // Puts the previous value back on an unwind too, so a panic
            // doesn't leave the operation on a reused pool thread.
            struct Restore(Option<Operation>);
            impl Drop for Restore {
                fn drop(&mut self) {
                    let previous = self.0.take();
                    BLOCKING.with(|slot| *slot.borrow_mut() = previous);
                }
            }
            let _restore = Restore(BLOCKING.with(|slot| slot.replace(Some(operation))));
            work()
        })
    }
}

/// Permission to change one file; held while the change is made.
#[derive(Debug)]
#[must_use = "the permit counts as live only while it is held"]
pub struct WritePermit {
    inner: Arc<Inner>,
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        self.inner.release();
    }
}

/// What activates the writers of a standby process. Only
/// [`ActiveGate::standby`] makes one, and it works for that gate alone.
#[derive(Debug)]
pub struct ActivationKey {
    gate: Arc<Inner>,
}

impl ActivationKey {
    /// An [`Activation`], while `gate` is `Closed`.
    ///
    /// # Errors
    /// [`NotActive`] (with the gate's state) when the gate is not closed, or
    /// is not the gate this key was made with.
    pub fn activate(&self, gate: &ActiveGate) -> Result<Activation, NotActive> {
        let state = gate.inner.lock();
        if !Arc::ptr_eq(&self.gate, &gate.inner) || state.state != GateState::Closed {
            return Err(NotActive { state: state.state });
        }
        Ok(Activation {
            gate: Arc::clone(&gate.inner),
            epoch: state.epoch,
        })
    }
}

/// The right to bind writers (the auth directory and the like) while the
/// gate is `Closed`, for the take-over path. It has no public constructor,
/// and is for one gate at one epoch.
#[derive(Debug)]
pub struct Activation {
    gate: Arc<Inner>,
    epoch: u64,
}

impl Activation {
    /// Whether this activation is for `gate` and still holds: the gate is
    /// `Closed` at the epoch it was given at.
    ///
    /// # Errors
    /// [`NotActive`] when it is for another gate, or the gate has opened
    /// since.
    pub fn check(&self, gate: &ActiveGate) -> Result<(), NotActive> {
        let state = gate.inner.lock();
        if Arc::ptr_eq(&self.gate, &gate.inner)
            && state.state == GateState::Closed
            && state.epoch == self.epoch
        {
            Ok(())
        } else {
            Err(NotActive { state: state.state })
        }
    }

    /// Runs `bind` while this activation holds: the check and `bind` are one
    /// step under the gate's own mutex, which [`ActiveGate::reopen`] takes
    /// too, so the gate stays `Closed` at this activation's epoch for the
    /// whole of `bind`, or `bind` doesn't run. `bind` must be short and must
    /// not wait, so it takes no lock that anything holding the gate's mutex
    /// waits on.
    ///
    /// # Errors
    /// [`NotActive`] when it is for another gate, or the gate has opened
    /// since; `bind` has not run then.
    pub fn bind<T>(&self, gate: &ActiveGate, bind: impl FnOnce() -> T) -> Result<T, NotActive> {
        let state = gate.inner.lock();
        if Arc::ptr_eq(&self.gate, &gate.inner)
            && state.state == GateState::Closed
            && state.epoch == self.epoch
        {
            Ok(bind())
        } else {
            Err(NotActive { state: state.state })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    // Not upstream's: an always-open gate admits operations and permits.
    #[test]
    fn open_gate_allows_everything() {
        let gate = ActiveGate::open();
        let op = gate.begin_operation().unwrap();
        let permit = gate.write_permit().unwrap();
        drop((op, permit));
        assert_eq!(gate.state(), GateState::Open);
    }

    // Not upstream's: Quiescing admits no operation and refuses a write
    // outside one (R4).
    #[tokio::test]
    async fn quiescing_refuses_new_operations_and_unscoped_writes() {
        let gate = ActiveGate::open();
        let op = gate.begin_operation().unwrap();
        let closing = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.close(0).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(gate.state(), GateState::Quiescing);
        assert!(gate.begin_operation().is_err());
        assert!(gate.write_permit().is_err());
        drop(op);
        closing.await.unwrap().unwrap();
        assert_eq!(gate.state(), GateState::Closed);
    }

    // Not upstream's: a write inside an admitted operation passes while
    // Quiescing, including a save nested in another (the nested-save case),
    // and the gate closes only once both end.
    #[tokio::test]
    async fn quiescing_allows_writes_inside_an_operation_including_nested() {
        let gate = ActiveGate::open();
        let op = gate.begin_operation().unwrap();
        let closing = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.close(0).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let inner_gate = gate.clone();
        op.clone()
            .scope(async move {
                let outer = inner_gate.write_permit().unwrap();
                let nested = inner_gate.write_permit().unwrap();
                drop((outer, nested));
            })
            .await;
        assert_eq!(gate.state(), GateState::Quiescing);
        drop(op);
        closing.await.unwrap().unwrap();
        assert_eq!(gate.state(), GateState::Closed);
    }

    // Not upstream's: an operation of another gate lets nothing through.
    #[tokio::test]
    async fn another_gates_operation_does_not_pass() {
        let gate = ActiveGate::open();
        let other = ActiveGate::open();
        let foreign = other.begin_operation().unwrap();
        let op = gate.begin_operation().unwrap();
        let closing = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.close(0).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let probe = gate.clone();
        let refused = foreign
            .scope(async move { probe.write_permit().is_err() })
            .await;
        assert!(refused);
        drop(op);
        closing.await.unwrap().unwrap();
    }

    // Not upstream's: a closed gate refuses everything.
    #[tokio::test]
    async fn closed_refuses_everything() {
        let gate = ActiveGate::open();
        gate.close(0).await.unwrap();
        assert_eq!(gate.state(), GateState::Closed);
        assert!(gate.begin_operation().is_err());
        assert!(gate.write_permit().is_err());
    }

    // Not upstream's: the budget expires, the gate reopens, and the
    // waiting close gives up; an operation that outlived it still ends
    // cleanly.
    #[tokio::test]
    async fn reopen_cancels_a_waiting_close_and_bumps_the_epoch() {
        let gate = ActiveGate::open();
        let op = gate.begin_operation().unwrap();
        let waited = tokio::time::timeout(Duration::from_millis(30), gate.close(0)).await;
        assert!(waited.is_err());
        assert_eq!(gate.state(), GateState::Quiescing);
        let epoch = gate.reopen();
        assert_eq!(epoch, 1);
        assert_eq!(gate.state(), GateState::Open);
        assert!(gate.begin_operation().is_ok());
        drop(op);
        assert!(matches!(
            gate.close(0).await,
            Err(CloseError::NotOpen { epoch: 1, .. })
        ));
        gate.close(1).await.unwrap();
    }

    // Not upstream's: a close waiting when the gate is reopened reports it.
    #[tokio::test]
    async fn a_close_waiting_during_a_reopen_reports_it() {
        let gate = ActiveGate::open();
        let op = gate.begin_operation().unwrap();
        let closing = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.close(0).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.reopen();
        assert_eq!(closing.await.unwrap(), Err(CloseError::Reopened));
        drop(op);
        assert_eq!(gate.state(), GateState::Open);
    }

    // Not upstream's: the operation reaches a spawned task and a blocking
    // closure, and holds the gate from closing until they end.
    #[tokio::test]
    async fn operation_travels_through_spawn_and_blocking() {
        let gate = ActiveGate::open();
        let op = gate.begin_operation().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = {
            let gate = gate.clone();
            gate.clone()
                .spawn(&op, async move {
                    let current = Operation::current().expect("in scope");
                    assert!(current.gate().same(&gate));
                    let seen = Operation::current()
                        .expect("in scope")
                        .run_blocking(|| Operation::current().is_some())
                        .await
                        .unwrap();
                    let _ = rx.await;
                    seen
                })
                .unwrap()
        };
        drop(op);
        let closing = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.close(0).await })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(gate.state(), GateState::Quiescing);
        tx.send(()).unwrap();
        assert!(task.await.unwrap());
        closing.await.unwrap().unwrap();
        assert!(Operation::current().is_none());
    }

    // Not upstream's: only a closed standby gate gives an activation.
    #[tokio::test]
    async fn activation_needs_a_closed_gate() {
        let (gate, key) = ActiveGate::standby();
        assert_eq!(gate.state(), GateState::Closed);
        assert!(key.activate(&gate).is_ok());
        gate.reopen();
        assert!(key.activate(&gate).is_err());
    }

    // Not upstream's: a panic in a blocking closure leaves no operation on
    // the pool thread, so the gate closes and later work sees none.
    #[test]
    fn a_panic_in_run_blocking_leaks_no_operation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let gate = ActiveGate::open();
            let op = gate.begin_operation().unwrap();
            let result = op.run_blocking(|| panic!("boom")).await;
            assert!(result.is_err());
            drop(op);
            tokio::time::timeout(Duration::from_secs(2), gate.close(0))
                .await
                .expect("the gate closes")
                .unwrap();
            let seen = tokio::task::spawn_blocking(|| Operation::current().is_none())
                .await
                .unwrap();
            assert!(seen);
        });
    }

    // Not upstream's: a key opens its own gate only, and an activation is
    // refused once its gate opens.
    #[tokio::test]
    async fn a_key_and_an_activation_are_bound_to_their_gate() {
        let (gate, key) = ActiveGate::standby();
        let (other, other_key) = ActiveGate::standby();
        assert!(key.activate(&other).is_err());
        assert!(other_key.activate(&gate).is_err());
        let activation = key.activate(&gate).unwrap();
        assert!(activation.check(&gate).is_ok());
        assert!(activation.check(&other).is_err());
        gate.reopen();
        assert!(activation.check(&gate).is_err());
        gate.close(1).await.unwrap();
        assert!(activation.check(&gate).is_err(), "a new closed epoch");
    }

    // Not upstream's (C2-O4): a binding made under an activation and a
    // reopen of the gate are one step each. A reopen that starts while the
    // binding runs waits for it, so the gate is Closed at the activation's
    // epoch throughout, and a binding that comes after the reopen is refused
    // without running.
    #[test]
    fn a_binding_and_a_reopen_do_not_interleave() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;

        let (gate, key) = ActiveGate::standby();
        let activation = key.activate(&gate).unwrap();
        let reopening = Arc::new(AtomicBool::new(false));
        let reopened = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = mpsc::channel();

        let binder = {
            let gate = gate.clone();
            let reopening = Arc::clone(&reopening);
            let reopened = Arc::clone(&reopened);
            std::thread::spawn(move || {
                let outcome = activation.bind(&gate, || {
                    started_tx.send(()).unwrap();
                    // Hold the binding until the reopen is under way, and a
                    // while after.
                    while !reopening.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    reopened.load(Ordering::SeqCst)
                });
                (outcome, activation)
            })
        };
        started_rx.recv().unwrap();
        let reopener = {
            let gate = gate.clone();
            let reopening = Arc::clone(&reopening);
            let reopened = Arc::clone(&reopened);
            std::thread::spawn(move || {
                reopening.store(true, Ordering::SeqCst);
                gate.reopen();
                reopened.store(true, Ordering::SeqCst);
            })
        };

        let (outcome, activation) = binder.join().unwrap();
        assert!(
            !outcome.expect("the binding ran while the gate was closed"),
            "the reopen finished during the binding"
        );
        reopener.join().unwrap();
        assert_eq!(gate.state(), GateState::Open);
        let mut ran = false;
        assert!(activation.bind(&gate, || ran = true).is_err());
        assert!(!ran, "a refused binding doesn't run");
    }

    // Not upstream's: wait_open returns at once on an open gate and parks
    // on a closed one until it reopens.
    #[tokio::test]
    async fn wait_open_parks_until_the_gate_opens() {
        let (gate, _key) = ActiveGate::standby();
        let waiting = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.wait_open().await })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiting.is_finished());
        gate.reopen();
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        ActiveGate::open().wait_open().await;
    }
}
