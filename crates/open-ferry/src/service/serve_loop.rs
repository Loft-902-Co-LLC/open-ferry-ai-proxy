//! open-ferry's own: the one loop that accepts and serves connections, for
//! the proxy's listener and the management address's, with and without
//! TLS. Upstream has nothing like it: it serves with Go's `net/http`.
//!
//! Each connection holds two OS handles, the stream and the duplicate of it
//! a cut shuts down, so the open-file limit has to allow twice the
//! connections. A duplicate that fails (for one, at the limit) drops the
//! connection and makes the loop wait as it does after an accept error.
//!
//! The loop replaces `axum::serve` so that a supervised worker can do what
//! `axum::serve` can't:
//! - [`ServeControl::pause_accept`] drops the listener the loop accepts on
//!   and keeps a duplicate of the socket, so the socket and its backlog
//!   persist; [`ServeControl::resume_accept`] accepts on it again.
//! - [`ServeControl::release`] stops accepting for good and finishes the
//!   connections: it gives each hyper's graceful shutdown (HTTP/1 sends
//!   `Connection: close` with the answer in flight and closes an idle
//!   connection; HTTP/2 sends GOAWAY and lets its streams finish), and
//!   its future completes once the last connection is gone.
//! - [`ServeControl::live_count`] counts connections from the TCP accept,
//!   one still in its TLS handshake included, and an upgraded connection
//!   (a WebSocket over HTTP/1, or over HTTP/2 by extended CONNECT) until
//!   it closes, however long its hyper connection future has been done.
//! - [`ServeControl::cut`] closes everything that is left, and finishes the
//!   control: a connection that registers after it is cut at once, and
//!   [`ServeControl::resume_accept`] fails. A cut shuts the connection's
//!   socket down through a duplicate of it kept from the accept, so it
//!   closes even when whoever holds the IO (an upgraded handler waiting on
//!   something else) never polls it, and it leaves the count at once.
//!
//! A connection that has carried no request when the release comes, or
//! that finishes its handshake after it, is served until its first request
//! is in, which then gets `Connection: close` or GOAWAY. [`FIRST_REQUEST_DEADLINE`]
//! bounds the wait, counted once from the release (a handshake that ends
//! late doesn't get a fresh one, and one still running at the deadline is
//! ended). The loop enforces it independently of normal ingress deadlines.
//! HTTP/2 can admit more
//! than one stream after a release: h2 lets the streams in flight arrive
//! for about a round trip during its two-stage GOAWAY. They are served.
//!
//! The loop does what `axum::serve` (axum 0.8.9) does for each connection:
//! hyper-util's auto builder with HTTP/2's extended CONNECT enabled (for
//! WebSockets over HTTP/2), the peer's address as `ConnectInfo<SocketAddr>`,
//! TCP_NODELAY (`send_at_once`) and, on a failed accept, a connection error
//! ignored and any other error logged and followed by a second's wait.
//! open-ferry adds per-listener admission, TLS/header/idle deadlines and
//! progress-based response write deadlines (see [`ingress`]). The server
//! router adds upload deadlines and a shared buffered-body budget. New
//! connections snapshot deadlines; admission and body storage follow reloads.
//!
//! The loop's stop, [`ServeControl::stop`], is `axum::serve`'s graceful
//! shutdown: every connection gets hyper's graceful shutdown at once, and
//! the loop ends when their tasks have. An upgraded connection's IO is no
//! longer a task's and doesn't hold it up.
//!
//! Deviations from upstream: upstream has no such loop. The TLS path now
//! accepts as the plain one does, so a failed accept waits a second where
//! the old TLS loop waited 100 ms.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::service::service_fn;
#[cfg(test)]
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder;
use socket2::{SockRef, Socket};
use std::net::Shutdown;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio_rustls::TlsAcceptor;
use tower_service::Service;

mod ingress;
use ingress::{
    Admission, AdmissionGuard, Flight, Guarded, ResponseBody, SocketIo, StreamExecutor, Traffic,
};
use open_ferry_server::config::IngressConfig;
use open_ferry_server::ingress::Ingress;

/// How long a connection that has carried no request gets, once the
/// release has come, to carry one. Then it is closed.
pub(crate) const FIRST_REQUEST_DEADLINE: Duration = Duration::from_secs(10);

/// How long to wait after an accept fails for a reason other than the
/// client's connection, as `axum::serve` does.
const ACCEPT_ERROR_WAIT: Duration = Duration::from_secs(1);

/// What the loops that share a [`ServeControl`] are told to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Accept and serve.
    Running,
    /// The listener is dropped, the socket kept.
    Paused,
    /// Accept no more; finish the connections, waiting for a first request
    /// on those that have carried none.
    Released,
    /// Accept no more; finish the connections at once.
    Stopping,
}

/// What the loops and their connections add up to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Stats {
    /// Loops that haven't ended.
    loops: usize,
    /// Loops whose future has been polled. A loop that is registered and
    /// not yet polled holds no listener, so a resume doesn't wait for it.
    started: usize,
    /// Loops that hold a listener.
    accepting: usize,
    /// Loops that ended because they couldn't make a listener from their
    /// socket. Never decreases.
    failed: usize,
    /// Connections whose IO is alive, from the TCP accept.
    live: usize,
    /// Of those, the upgraded ones.
    upgraded: usize,
    /// Connection tasks, handshakes included, which end before the IO of
    /// an upgraded connection does.
    tasks: usize,
}

/// The connections alive, and whether the control has been cut.
#[derive(Default)]
struct Connections {
    map: HashMap<u64, Weak<ConnCore>>,
    /// Set by a cut and never cleared: a connection that registers after it
    /// is cut as it registers.
    cut: bool,
}

struct Shared {
    mode: watch::Sender<Mode>,
    stats: watch::Sender<Stats>,
    connections: Mutex<Connections>,
    /// Serializes `pause_accept` and `resume_accept`, which hold it across
    /// their waits. Nothing else takes it. It holds how many loop failures
    /// a resume has reported.
    gate: tokio::sync::Mutex<usize>,
    next_id: AtomicU64,
    first_request_deadline: Duration,
    /// When the release came. A connection with no request yet must have its
    /// first by this plus `first_request_deadline`.
    released_at: OnceLock<tokio::time::Instant>,
    /// Makes the next duplicate of an accepted socket fail.
    #[cfg(test)]
    fail_next_duplicate: AtomicBool,
    /// Makes the next listener a loop makes from its socket fail.
    #[cfg(test)]
    fail_next_listener: AtomicBool,
    #[cfg(test)]
    fail_listener_after: Mutex<Option<(u16, Arc<tokio::sync::Notify>)>>,
    /// Makes the next resume count a loop as started once its wait has
    /// settled, as a loop polled for the first time just then would be.
    #[cfg(test)]
    start_a_loop_after_the_resume_settles: AtomicBool,
}

impl Shared {
    fn update(&self, change: impl FnOnce(&mut Stats)) {
        self.stats.send_modify(change);
    }

    fn connections(&self) -> MutexGuard<'_, Connections> {
        self.connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The instant a connection with no request yet is closed at, once the
    /// release has come.
    fn first_request_due(&self) -> tokio::time::Instant {
        self.released_at
            .get()
            .copied()
            .unwrap_or_else(tokio::time::Instant::now)
            + self.first_request_deadline
    }

    /// Moves to `to` from a mode `from` allows. Whether it moved.
    fn move_mode(&self, from: &[Mode], to: Mode) -> bool {
        self.mode.send_if_modified(|mode| {
            if from.contains(mode) {
                *mode = to;
                true
            } else {
                false
            }
        })
    }
}

/// The handle on the loops started with it: pauses, resumes, releases and
/// cuts them, and counts their connections.
#[derive(Clone)]
pub(crate) struct ServeControl {
    shared: Arc<Shared>,
}

impl ServeControl {
    pub(crate) fn new() -> Self {
        Self::with_first_request_deadline(FIRST_REQUEST_DEADLINE)
    }

    /// As [`ServeControl::new`], with the wait for a first request after a
    /// release set to `first_request_deadline`.
    pub(crate) fn with_first_request_deadline(first_request_deadline: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                mode: watch::channel(Mode::Running).0,
                stats: watch::channel(Stats::default()).0,
                connections: Mutex::new(Connections::default()),
                gate: tokio::sync::Mutex::new(0),
                next_id: AtomicU64::new(0),
                first_request_deadline,
                released_at: OnceLock::new(),
                #[cfg(test)]
                fail_next_duplicate: AtomicBool::new(false),
                #[cfg(test)]
                fail_next_listener: AtomicBool::new(false),
                #[cfg(test)]
                fail_listener_after: Mutex::new(None),
                #[cfg(test)]
                start_a_loop_after_the_resume_settles: AtomicBool::new(false),
            }),
        }
    }

    async fn wait_for(&self, condition: impl FnMut(&Stats) -> bool) {
        let mut stats = self.shared.stats.subscribe();
        // The sender is in `shared`, which this handle keeps.
        let _ = stats.wait_for(condition).await;
    }

    fn stats(&self) -> Stats {
        *self.shared.stats.borrow()
    }

    /// Waits until `done` holds of the stats or the mode is no longer
    /// `expected`, so a call that something else superseded still ends.
    /// Returns the stats it saw last. A caller judges those rather than
    /// reading them again: a loop polled since may have counted itself as
    /// started and not have its listener yet.
    async fn wait_settled(&self, expected: Mode, done: impl Fn(&Stats) -> bool) -> Stats {
        let mut stats = self.shared.stats.subscribe();
        let mut mode = self.shared.mode.subscribe();
        loop {
            let moved = *mode.borrow_and_update() != expected;
            let seen = *stats.borrow_and_update();
            if moved || done(&seen) {
                return seen;
            }
            // The senders are in `shared`, which this handle keeps.
            tokio::select! {
                _ = stats.changed() => {}
                _ = mode.changed() => {}
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn accepting_is_paused(&self) -> bool {
        *self.shared.mode.borrow() == Mode::Paused && self.stats().accepting == 0
    }

    #[cfg(test)]
    pub(crate) fn accepting_is_stopped(&self) -> bool {
        *self.shared.mode.borrow() == Mode::Stopping && self.stats().accepting == 0
    }

    /// Stops accepting once the loops have dropped their listeners, which
    /// are the std sockets' duplicates: the sockets and their backlogs
    /// persist, so a client that connects meanwhile waits. Connections
    /// already accepted are served on. It returns only when no loop holds a
    /// listener, so none accepts after it returns, a release or stop that
    /// came first or meanwhile included. Pauses and resumes take turns.
    pub(crate) async fn pause_accept(&self) {
        let _turn = self.shared.gate.lock().await;
        self.shared.move_mode(&[Mode::Running], Mode::Paused);
        // Not until the mode moves on: a loop that hasn't seen the move yet
        // may still accept once. Every mode but `Running` drops listeners.
        self.wait_for(|stats| stats.accepting == 0).await;
    }

    #[cfg(test)]
    pub(crate) fn fail_listener_after(&self, port: u16, queued: Arc<tokio::sync::Notify>) {
        *self.shared.fail_listener_after.lock().unwrap() = Some((port, queued));
    }

    /// Accepts again, once every loop has made a listener from its socket
    /// again. An error if a loop couldn't since a resume last said so, if
    /// the loops were released or stopped meanwhile, or if the control was
    /// cut, which finishes it.
    pub(crate) async fn resume_accept(&self) -> io::Result<()> {
        let mut reported = self.shared.gate.lock().await;
        if self.shared.connections().cut {
            return Err(io::Error::other("the control was cut"));
        }
        // A loop that can't make its listener leaves the counts, which then
        // look settled, so its failure is counted apart. One that no resume
        // has reported counts, so a failure while a resume that was dropped
        // waited, or while this one finishes a pause, isn't missed.
        let failed = *reported;
        // A pause that was dropped is finished first. A loop still holding
        // the listener it is about to drop would otherwise look settled, and
        // a failure to make it again would come after the resume returned.
        if *self.shared.mode.borrow() == Mode::Paused {
            self.wait_for(|stats| stats.accepting == 0).await;
        }
        self.shared.move_mode(&[Mode::Paused], Mode::Running);
        if *self.shared.mode.borrow() != Mode::Running {
            return Err(io::Error::other("the listeners were released"));
        }
        let settled = self
            .wait_settled(Mode::Running, |stats| {
                stats.failed > failed || stats.accepting >= stats.started
            })
            .await;
        #[cfg(test)]
        if self
            .shared
            .start_a_loop_after_the_resume_settles
            .swap(false, Ordering::AcqRel)
        {
            self.shared.update(|stats| stats.started += 1);
        }
        if self.shared.connections().cut {
            return Err(io::Error::other("the control was cut"));
        }
        if *self.shared.mode.borrow() != Mode::Running {
            return Err(io::Error::other("the listeners were released"));
        }
        if settled.failed > failed {
            *reported = settled.failed;
            return Err(io::Error::other("a listener couldn't accept again"));
        }
        Ok(())
    }

    /// Waits until `expected` loops hold a listener: the first accept of a
    /// start, which `resume_accept` doesn't wait for, since it leaves out
    /// the loops that haven't been polled yet. An error if a loop couldn't
    /// make its listener since a resume or this last said so, or if the
    /// loops were released, stopped or cut first. It has no timer of its
    /// own: a caller that must be bounded is, by whoever ends it.
    pub(crate) async fn await_listeners(&self, expected: usize) -> io::Result<()> {
        let mut reported = self.shared.gate.lock().await;
        let failed = *reported;
        let settled = self
            .wait_settled(Mode::Running, |stats| {
                stats.failed > failed || stats.accepting >= expected
            })
            .await;
        if self.shared.connections().cut {
            return Err(io::Error::other("the control was cut"));
        }
        if *self.shared.mode.borrow() != Mode::Running {
            return Err(io::Error::other("the listeners were released"));
        }
        if settled.failed > failed {
            *reported = settled.failed;
            return Err(io::Error::other("a listener couldn't be made"));
        }
        Ok(())
    }

    /// Stops accepting for good and finishes every connection (see the
    /// module's description). The listeners are dropped before the future
    /// completes, which it does once no connection is left, an upgraded
    /// one included. The future may be dropped; the release goes on.
    pub(crate) fn release(&self) -> impl Future<Output = ()> + Send + 'static {
        // The instant is set before the mode is, so whoever sees `Released`
        // sees it. Only the first release sets it.
        let _ = self.shared.released_at.set(tokio::time::Instant::now());
        self.shared
            .move_mode(&[Mode::Running, Mode::Paused], Mode::Released);
        let control = self.clone();
        async move {
            control
                .wait_for(|stats| stats.loops == 0 && stats.live == 0)
                .await;
        }
    }

    /// Stops as `axum::serve`'s graceful shutdown does: accepts no more
    /// and gives every connection hyper's graceful shutdown at once.
    pub(crate) fn stop(&self) {
        self.shared.move_mode(
            &[Mode::Running, Mode::Paused, Mode::Released],
            Mode::Stopping,
        );
    }

    /// How many connections are alive: counted from the TCP accept, and
    /// until the IO of an upgraded one closes.
    pub(crate) fn live_count(&self) -> usize {
        self.stats().live
    }

    /// How many of the live connections have been upgraded.
    #[cfg(test)]
    pub(crate) fn upgraded_count(&self) -> usize {
        self.stats().upgraded
    }

    /// Closes every connection that is left, handshakes and upgraded ones
    /// included, and finishes the control: a connection that registers
    /// later is cut as it does. A cut also stops accepting, as a stop does,
    /// so the listeners go and a resume fails. How many it closed.
    pub(crate) fn cut(&self) -> usize {
        // The lock isn't held while these are cut, which takes it again.
        let alive: Vec<Arc<ConnCore>> = {
            let mut connections = self.shared.connections();
            connections.cut = true;
            connections.map.values().filter_map(Weak::upgrade).collect()
        };
        // The listeners go too: left accepting, they would take connections
        // only to cut them, from a socket another worker may share.
        self.shared.move_mode(
            &[Mode::Running, Mode::Paused, Mode::Released],
            Mode::Stopping,
        );
        alive.iter().filter(|core| core.cut()).count()
    }

    fn register_loop(&self) -> LoopRegistration {
        self.shared.update(|stats| stats.loops += 1);
        LoopRegistration {
            shared: self.shared.clone(),
            accepting: false,
            started: false,
        }
    }
}

/// Cuts the connections of its control when dropped, unless disarmed: a
/// serve future that is dropped before it ends takes its connections with it.
pub(crate) struct CutOnDrop(Option<ServeControl>);

impl CutOnDrop {
    pub(crate) fn new(control: &ServeControl) -> Self {
        Self(Some(control.clone()))
    }

    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CutOnDrop {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            control.cut();
        }
    }
}

/// A loop in the control's counts.
struct LoopRegistration {
    shared: Arc<Shared>,
    accepting: bool,
    started: bool,
}

impl LoopRegistration {
    fn start(&mut self) {
        if !self.started {
            self.started = true;
            self.shared.update(|stats| stats.started += 1);
        }
    }

    /// Counts the loop's new listener if the mode is still `Running`, read
    /// while the counts are locked. Whether it did. A pause returns while a
    /// loop makes a listener, since it holds none yet. Counted regardless,
    /// that listener would look settled to a resume that came next, though
    /// the loop, having seen the pause, would drop it.
    fn count_listener(&mut self) -> bool {
        let shared = &self.shared;
        self.accepting = shared.stats.send_if_modified(|stats| {
            if *shared.mode.borrow() == Mode::Running {
                stats.accepting += 1;
                true
            } else {
                false
            }
        });
        self.accepting
    }

    fn set_accepting(&mut self, accepting: bool) {
        if self.accepting == accepting {
            return;
        }
        self.accepting = accepting;
        self.shared.update(|stats| {
            if accepting {
                stats.accepting += 1;
            } else {
                stats.accepting = stats.accepting.saturating_sub(1);
            }
        });
    }

    /// Counts the loop as failed. It is called before the registration is
    /// dropped, so whoever sees the loop leave the counts sees why.
    fn fail(&mut self) {
        self.shared.update(|stats| stats.failed += 1);
    }
}

impl Drop for LoopRegistration {
    fn drop(&mut self) {
        let (accepting, started) = (self.accepting, self.started);
        self.shared.update(|stats| {
            stats.loops = stats.loops.saturating_sub(1);
            if started {
                stats.started = stats.started.saturating_sub(1);
            }
            if accepting {
                stats.accepting = stats.accepting.saturating_sub(1);
            }
        });
    }
}

/// A connection task in the control's counts.
struct TaskGuard(Arc<Shared>);

impl TaskGuard {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.update(|stats| stats.tasks += 1);
        Self(shared.clone())
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0
            .update(|stats| stats.tasks = stats.tasks.saturating_sub(1));
    }
}

/// What a connection's count depends on.
#[derive(Default)]
struct CoreState {
    /// It has left the counts: cut, or dropped.
    retired: bool,
    upgraded: bool,
}

/// What a connection shares with its loop, its task and its IO. It is
/// counted while any of them has it, so the count follows the IO: an
/// upgraded connection is counted until its IO closes, or is cut.
struct ConnCore {
    /// Admission follows the IO owner, even after a cut retires live_count.
    admission: OnceLock<AdmissionGuard>,
    traffic: OnceLock<Arc<Traffic>>,
    shared: Arc<Shared>,
    id: u64,
    /// A request has reached the service.
    seen_request: AtomicBool,
    seen_request_wake: Notify,
    cut: AtomicBool,
    /// Cuts after the next flag snapshot, to test registration ordering.
    #[cfg(test)]
    cut_after_flag_read: AtomicBool,
    cut_wake: Notify,
    state: Mutex<CoreState>,
    /// A duplicate of the accepted socket, which a cut shuts down so the
    /// transport closes though nobody polls the IO. It is the core's own
    /// handle, never a raw number, so it can't close one that was reused.
    socket: Mutex<Option<Socket>>,
    /// The tasks waiting to read and to write, to wake when it is cut.
    wakers: Mutex<[Option<Waker>; 2]>,
}

impl ConnCore {
    /// A connection, counted from now. A connection of a cut control is cut
    /// before it is returned. An error if the socket can't be duplicated or
    /// made non-inheritable, and then the connection isn't counted.
    fn register(shared: &Arc<Shared>, stream: &TcpStream) -> io::Result<Arc<Self>> {
        #[cfg(test)]
        if shared.fail_next_duplicate.swap(false, Ordering::AcqRel) {
            return Err(io::Error::other("a duplicate that fails, for a test"));
        }
        let socket = SockRef::from(stream).try_clone()?;
        #[cfg(windows)]
        open_ferry_supervisor::sockets::windows::set_no_inherit(&socket)?;
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let core = Arc::new(Self {
            admission: OnceLock::new(),
            traffic: OnceLock::new(),
            shared: shared.clone(),
            id,
            seen_request: AtomicBool::new(false),
            seen_request_wake: Notify::new(),
            cut: AtomicBool::new(false),
            #[cfg(test)]
            cut_after_flag_read: AtomicBool::new(false),
            cut_wake: Notify::new(),
            state: Mutex::new(CoreState::default()),
            socket: Mutex::new(Some(socket)),
            wakers: Mutex::new([None, None]),
        });
        // The count and the registry change under one lock, so a cut sees
        // the connection or this sees the cut.
        let control_cut = {
            let mut connections = shared.connections();
            shared.update(|stats| stats.live += 1);
            connections.map.insert(id, Arc::downgrade(&core));
            connections.cut
        };
        if control_cut {
            core.cut();
        }
        Ok(core)
    }

    fn state(&self) -> MutexGuard<'_, CoreState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the connection out of the counts, once.
    fn retire(&self) {
        {
            let mut state = self.state();
            if state.retired {
                return;
            }
            state.retired = true;
            let upgraded = state.upgraded;
            self.shared.update(|stats| {
                stats.live = stats.live.saturating_sub(1);
                if upgraded {
                    stats.upgraded = stats.upgraded.saturating_sub(1);
                }
            });
        }
        self.shared.connections().map.remove(&self.id);
    }

    fn note_request(&self) {
        if !self.seen_request.swap(true, Ordering::AcqRel) {
            self.seen_request_wake.notify_one();
        }
    }

    fn has_seen_request(&self) -> bool {
        self.seen_request.load(Ordering::Acquire)
    }

    fn note_upgrade(&self) {
        if let Some(traffic) = self.traffic.get() {
            traffic.upgrade();
        }
        let mut state = self.state();
        if !state.retired && !state.upgraded {
            state.upgraded = true;
            self.shared.update(|stats| stats.upgraded += 1);
        }
    }

    fn is_cut(&self) -> bool {
        let cut = self.cut.load(Ordering::Acquire);
        #[cfg(test)]
        if self.cut_after_flag_read.swap(false, Ordering::AcqRel) {
            self.cut();
        }
        cut
    }

    /// Whether this call cut it. The socket is shut down, so the peer sees
    /// the connection close and a pending read or write of the owner ends,
    /// whether or not the owner is polling; the connection leaves the
    /// counts at once.
    fn cut(&self) -> bool {
        if self.cut.swap(true, Ordering::AcqRel) {
            return false;
        }
        let socket = self
            .socket
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(socket) = socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
        self.cut_wake.notify_one();
        let wakers =
            std::mem::take(&mut *self.wakers.lock().unwrap_or_else(PoisonError::into_inner));
        for waker in wakers.into_iter().flatten() {
            waker.wake();
        }
        self.retire();
        true
    }

    fn watch(&self, slot: usize, waker: &Waker) {
        let mut wakers = self.wakers.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = wakers.get_mut(slot)
            && !entry.as_ref().is_some_and(|known| known.will_wake(waker))
        {
            *entry = Some(waker.clone());
        }
    }
}

impl Drop for ConnCore {
    fn drop(&mut self) {
        self.retire();
        if let Some(traffic) = self.traffic.get() {
            traffic.wake();
        }
    }
}

const READ: usize = 0;
const WRITE: usize = 1;

fn cut_error() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "the connection was cut")
}

/// The IO of a connection, which fails once the connection is cut. It is
/// what keeps a connection counted.
struct Tracked<T> {
    inner: T,
    core: Arc<ConnCore>,
    wakers: [Option<Waker>; 2],
}

impl<T> Tracked<T> {
    fn watch(&mut self, slot: usize, waker: &Waker) {
        let known = &mut self.wakers[slot];
        if !known.as_ref().is_some_and(|known| known.will_wake(waker)) {
            self.core.watch(slot, waker);
            *known = Some(waker.clone());
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Tracked<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Register a changed waker before reading the flag, so a cut between
        // the two still wakes this task. An unchanged waker is already in the
        // core; caching it here avoids locking the core on every IO poll.
        this.watch(READ, cx.waker());
        if this.core.is_cut() {
            return Poll::Ready(Err(cut_error()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Tracked<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.watch(WRITE, cx.waker());
        if this.core.is_cut() {
            return Poll::Ready(Err(cut_error()));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.watch(WRITE, cx.waker());
        if this.core.is_cut() {
            return Poll::Ready(Err(cut_error()));
        }
        Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.watch(WRITE, cx.waker());
        if this.core.is_cut() {
            return Poll::Ready(Err(cut_error()));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.watch(WRITE, cx.waker());
        if this.core.is_cut() {
            return Poll::Ready(Err(cut_error()));
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

/// Makes the loop's accepting handle without letting child processes keep it.
fn duplicate_listener(listener: &std::net::TcpListener) -> io::Result<TcpListener> {
    let duplicate = listener.try_clone()?;
    #[cfg(windows)]
    open_ferry_supervisor::sockets::windows::set_no_inherit(&duplicate)?;
    // A duplicate starts in blocking mode on Windows, where the mode isn't
    // shared with the original; a blocking accept would stall the worker.
    duplicate.set_nonblocking(true)?;
    TcpListener::from_std(duplicate)
}

/// Accepts connections on `listener` and serves `app` on them, with `tls`
/// when it is given, until `control` releases or stops the loop and every
/// connection task has ended. `control` may be shared by several loops.
///
/// The loop registers when this is called, not when the future is first
/// polled, so a release can't pass a loop that has yet to start.
pub(crate) fn run(
    listener: std::net::TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    mut app: Router,
    control: &ServeControl,
) -> impl Future<Output = io::Result<()>> + Send + 'static {
    let mut registration = control.register_loop();
    let shared = control.shared.clone();
    let acceptor = tls.map(TlsAcceptor::from);
    async move {
        registration.start();
        let ingress = ingress::router_settings(&mut app).await;
        let _default_cap = ingress::default_connection_cap();
        let admission = Arc::new(Admission::default());
        let mut settings = ingress.subscribe();
        // Counted as started, a loop that leaves must be counted as failed.
        if let Err(error) = listener.set_nonblocking(true) {
            registration.fail();
            return Err(error);
        }
        let mut mode = shared.mode.subscribe();
        let mut accepting: Option<TcpListener> = None;
        let outcome = loop {
            let current = *mode.borrow_and_update();
            match current {
                Mode::Running => {
                    if accepting.is_none() {
                        let duplicate = duplicate_listener(&listener);
                        #[cfg(test)]
                        let duplicate = if shared.fail_next_listener.swap(false, Ordering::AcqRel) {
                            Err(io::Error::other("a listener that fails, for a test"))
                        } else {
                            duplicate
                        };
                        #[cfg(test)]
                        let delayed_failure = {
                            let mut failure = shared.fail_listener_after.lock().unwrap();
                            if failure.as_ref().is_some_and(|(port, _)| {
                                listener
                                    .local_addr()
                                    .is_ok_and(|address| address.port() == *port)
                            }) {
                                failure.take().map(|(_, queued)| queued)
                            } else {
                                None
                            }
                        };
                        #[cfg(test)]
                        let duplicate = if let Some(queued) = delayed_failure {
                            queued.notified().await;
                            Err(io::Error::other(
                                "a listener failed after a management write queued",
                            ))
                        } else {
                            duplicate
                        };
                        match duplicate {
                            Ok(tokio_listener) => {
                                // A pause that came while this was built
                                // has been told no loop holds a listener, so
                                // it is kept only if the mode is still
                                // `Running` once it is counted.
                                if !registration.count_listener() {
                                    continue;
                                }
                                accepting = Some(tokio_listener);
                            }
                            Err(error) => {
                                registration.fail();
                                break Err(error);
                            }
                        }
                    }
                }
                Mode::Paused => {
                    if accepting.take().is_some() {
                        registration.set_accepting(false);
                    }
                }
                Mode::Released | Mode::Stopping => break Ok(()),
            }
            let accepted = async {
                match &accepting {
                    Some(tokio_listener) => tokio_listener.accept().await,
                    None => std::future::pending().await,
                }
            };
            let limits = settings.borrow_and_update().limits.clone();
            // Keep the listener/backlog, but never accept or duplicate a
            // socket above the cap. A mode change still wins immediately.
            if accepting.is_some() && admission.full(&limits) {
                ingress.warn("connections per listener");
                tokio::select! {
                    biased;
                    _ = mode.changed() => {}
                    _ = admission.changed.notified() => {}
                    _ = settings.changed() => {}
                    () = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
                continue;
            }
            tokio::select! {
                // A change of mode wins over a connection waiting to be
                // accepted.
                biased;
                _ = mode.changed() => {}
                _ = settings.changed() => {}
                accepted = accepted => match accepted {
                    Ok((stream, peer)) => {
                        if let Err(error) =
                            spawn_connection(&shared, stream, peer, acceptor.clone(), app.clone(),
                                (&admission, ingress.clone(), limits.clone()))
                        {
                            tracing::error!(
                                "dropping {peer}: its socket can't be duplicated or made non-inheritable: {error}"
                            );
                            tokio::select! {
                                () = tokio::time::sleep(ACCEPT_ERROR_WAIT) => {}
                                _ = mode.changed() => {}
                            }
                        }
                    }
                    Err(error) if is_connection_error(&error) => {}
                    Err(error) => {
                        tracing::error!("accept error: {error}");
                        tokio::select! {
                            () = tokio::time::sleep(ACCEPT_ERROR_WAIT) => {}
                            _ = mode.changed() => {}
                        }
                    }
                },
            }
        };
        // No more accepting: the listeners go, and the loop leaves the
        // count a release waits on.
        drop(accepting);
        drop(listener);
        drop(registration);
        let mut stats = shared.stats.subscribe();
        let _ = stats.wait_for(|stats| stats.tasks == 0).await;
        outcome
    }
}

/// A client that went away before it was accepted.
fn is_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// Starts the task of a connection just accepted. It is counted from here.
fn spawn_connection(
    shared: &Arc<Shared>,
    stream: TcpStream,
    peer: SocketAddr,
    acceptor: Option<TlsAcceptor>,
    app: Router,
    settings: (&Arc<Admission>, Ingress, IngressConfig),
) -> io::Result<()> {
    let (admission, ingress, limits) = settings;
    let accepted_at = tokio::time::Instant::now();
    let core = ConnCore::register(shared, &stream)?;
    let _ = core.admission.set(admission.admit());
    let task = TaskGuard::new(shared);
    super::send_at_once(&stream);
    let mut mode = shared.mode.subscribe();
    let shared = shared.clone();
    let traffic = Traffic::new(limits.clone(), ingress.clone(), accepted_at);
    let stream = SocketIo {
        inner: stream,
        traffic: traffic.clone(),
    };
    tokio::spawn(async move {
        let _task = task;
        match acceptor {
            None => {
                serve_connection(
                    stream,
                    peer,
                    app,
                    core,
                    mode,
                    (ingress, limits, accepted_at, traffic),
                )
                .await
            }
            Some(acceptor) => {
                // A stop or a cut ends a handshake; a release lets it run,
                // to the same deadline as the first request.
                let stream = tokio::select! {
                    biased;
                    () = core.cut_wake.notified() => None,
                    () = tokio::task::unconstrained(async {
                        match limits.tls_handshake_timeout {
                            Some(duration) => tokio::time::sleep_until(accepted_at + duration).await,
                            None => std::future::pending().await,
                        }
                    }) => { ingress.warn("TLS handshake"); None },
                    // The timer and the watch are unconstrained: a handshake
                    // that fills the task's budget mustn't starve them.
                    () = tokio::task::unconstrained(async { let _ = mode.wait_for(|mode| *mode == Mode::Stopping).await; }) => None,
                    () = tokio::task::unconstrained(first_request_due(&shared)) => {
                        tracing::debug!("closing {peer}: no handshake within the first-request deadline");
                        None
                    }
                    stream = async {
                        match acceptor.accept(stream).await {
                            Ok(stream) => Some(stream),
                            Err(error) => { tracing::debug!(%error, "inbound TLS handshake failed"); None }
                        }
                    } => stream,
                };
                if let Some(stream) = stream {
                    serve_connection(
                        stream,
                        peer,
                        app,
                        core,
                        mode,
                        (ingress, limits, tokio::time::Instant::now(), traffic),
                    )
                    .await;
                }
            }
        }
    });
    Ok(())
}

/// Completes at the instant a connection with no request yet is closed at,
/// once the release has come; never before it.
async fn first_request_due(shared: &Shared) {
    let mut mode = shared.mode.subscribe();
    if mode.wait_for(|mode| *mode == Mode::Released).await.is_ok() {
        tokio::time::sleep_until(shared.first_request_due()).await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// Where a connection is in being finished.
#[derive(PartialEq, Eq)]
enum Finishing {
    /// Nothing has asked it to.
    Not,
    /// Released, with no request yet: waiting for the first.
    WaitingForRequest,
    /// Hyper's graceful shutdown is on.
    Graceful,
}

/// Serves `app` on `io` until the connection ends, finishing it as `mode`
/// says.
async fn serve_connection<I>(
    io: I,
    peer: SocketAddr,
    app: Router,
    core: Arc<ConnCore>,
    mut mode: watch::Receiver<Mode>,
    settings: (Ingress, IngressConfig, tokio::time::Instant, Arc<Traffic>),
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (ingress, limits, started_at, traffic) = settings;
    if limits
        .header_timeout
        .is_some_and(|duration| started_at + duration <= tokio::time::Instant::now())
    {
        ingress.warn("request headers");
        core.cut();
        return;
    }
    traffic.established(started_at);
    let _ = core.traffic.set(traffic.clone());
    tokio::spawn(ingress::watch_traffic(
        Arc::downgrade(&core),
        traffic.clone(),
    ));
    let io = TokioIo::new(Guarded {
        inner: Tracked {
            inner: io,
            core: core.clone(),
            wakers: [None, None],
        },
        traffic: traffic.clone(),
    });
    let service = {
        let core = core.clone();
        let mode = mode.clone();
        let traffic = traffic.clone();
        service_fn(move |request: hyper::Request<Incoming>| {
            core.note_request();
            let flight = Flight::new(traffic.clone());
            let connect = request.method() == hyper::Method::CONNECT;
            // An answer that is ready at once is written before the task
            // can ask hyper to shut down gracefully, so a request that
            // arrives once the loop is released says it closes by itself.
            // HTTP/2 has no such header; its GOAWAY comes from the shutdown.
            let version = request.version();
            let mode = mode.clone();
            let mut router = app.clone();
            let mut request = request.map(Body::new);
            request.extensions_mut().insert(ConnectInfo(peer));
            let core = core.clone();
            async move {
                flight.bind_task();
                std::future::poll_fn(|cx| {
                    Service::<hyper::Request<Body>>::poll_ready(&mut router, cx)
                })
                .await
                .unwrap_or_else(|error: Infallible| match error {});
                let response = router
                    .call(request)
                    .await
                    .unwrap_or_else(|error| match error {});
                let status = response.status();
                // A release may arrive while the handler waits, and the
                // connection may write before its mode watcher is polled.
                let closing = matches!(*mode.borrow(), Mode::Released | Mode::Stopping)
                    && version <= hyper::Version::HTTP_11;
                if response
                    .extensions()
                    .get::<open_ferry_server::ingress::IngressRejection>()
                    .is_some()
                {
                    flight.close_after_response();
                }
                let mut response = response;
                if status == StatusCode::SWITCHING_PROTOCOLS || (connect && status.is_success()) {
                    core.note_upgrade();
                } else if closing {
                    response.headers_mut().insert(
                        hyper::header::CONNECTION,
                        hyper::header::HeaderValue::from_static("close"),
                    );
                }
                Ok::<_, Infallible>(response.map(|body| ResponseBody::new(body, flight)))
            }
        })
    };
    let mut builder = Builder::new(StreamExecutor(traffic.clone()));
    // The transport watcher covers detection, the first head and keep-alive
    // gaps without resetting on trickled bytes, for both HTTP versions.
    builder.http1().header_read_timeout(None);
    // The CONNECT protocol HTTP/2 WebSockets need, as `axum::serve` has it.
    builder.http2().enable_connect_protocol();
    let mut connection = pin!(builder.serve_connection_with_upgrades(io, service));
    let mut finishing = Finishing::Not;
    let mut deadline: Option<Pin<Box<tokio::time::Sleep>>> = None;
    loop {
        let current = *mode.borrow_and_update();
        match current {
            Mode::Stopping if finishing != Finishing::Graceful => {
                connection.as_mut().graceful_shutdown();
                finishing = Finishing::Graceful;
                deadline = None;
            }
            Mode::Released if finishing == Finishing::Not => {
                if core.has_seen_request() {
                    connection.as_mut().graceful_shutdown();
                    finishing = Finishing::Graceful;
                } else {
                    finishing = Finishing::WaitingForRequest;
                    deadline = Some(Box::pin(tokio::time::sleep_until(
                        core.shared.first_request_due(),
                    )));
                }
            }
            _ => {}
        }
        let deadline_passed = async {
            match deadline.as_mut() {
                Some(sleep) => sleep.as_mut().await,
                None => std::future::pending().await,
            }
        };
        // The order is the priority: a cut, the connection, a request that
        // arrived, a change of mode, and last the deadline, which also
        // looks at whether a request arrived.
        tokio::select! {
            biased;
            () = core.cut_wake.notified() => break,
            () = traffic.idle_wake.notified() => {
                if traffic.idle_expired.load(Ordering::Acquire) {
                    connection.as_mut().graceful_shutdown();
                    finishing = Finishing::Graceful;
                    deadline = None;
                }
            }
            result = connection.as_mut() => {
                if let Err(error) = result {
                    tracing::trace!("failed to serve connection from {peer}: {error:#}");
                }
                break;
            }
            () = core.seen_request_wake.notified(), if finishing == Finishing::WaitingForRequest => {
                connection.as_mut().graceful_shutdown();
                finishing = Finishing::Graceful;
                deadline = None;
            }
            // Unconstrained, so a connection that uses up the task's budget
            // can't starve the watch and the deadline.
            changed = tokio::task::unconstrained(mode.changed()) => {
                if changed.is_err() {
                    break;
                }
            }
            () = tokio::task::unconstrained(deadline_passed) => {
                if core.has_seen_request() {
                    // The request is in and being answered: not the
                    // connection to close.
                    connection.as_mut().graceful_shutdown();
                    finishing = Finishing::Graceful;
                    deadline = None;
                } else {
                    tracing::debug!("closing {peer}: no request within the first-request deadline");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
