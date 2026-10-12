//! open-ferry's own: the worker side of a blue-green switch (v3 4, 5.1, 7).
//!
//! A supervised worker serves the sockets its supervisor gives it and takes
//! its orders over the control channel. This part has the states a worker is
//! in before any switch: loading, standby (loaded and held), active and
//! draining, and what the supervisor may ask in
//! them:
//! - `loaded`, once, in either role: a standby sends it when it has loaded,
//!   a worker started active once it holds the lease and every listener
//!   its topology names, and both before their first `active` or `standby`
//!   `status`. Statuses in the `loading` phase (the answer to a `ping`, or
//!   the timer's) may come before it. The supervisor counts a start until it.
//! - `status` and `ping` in every state. The supervisor's `ping` is answered
//!   with one `status`; the worker never sends `ping`, ignores a `status`
//!   from the supervisor, and sends `status` every [`STATUS_EVERY`] and when
//!   its phase changes.
//! - `release`: ignored while loading or standby; an active worker stops
//!   accepting, finishes its connections, and answers
//!   `drained` when the last is gone. A connection that carried no request
//!   gets `budgets.drain_ms` (at most the serve loop's own deadline) to
//!   carry one, then is closed.
//! - `stop{budget_ms}`, and the channel's EOF, which is a stop with
//!   `budgets.stop_ms`: stop serving gracefully within the budget, cut what
//!   is left, and exit. EOF never gives the lease up early: only exiting
//!   does.
//! - A message this build doesn't know is answered `unsupported`, and a
//!   message that makes no sense in the state is logged and ignored.
//!
//! The switch messages (`hand_over`, `resume`, `take_over` and `go`) are
//! W3's: they are answered `unsupported` here. `peers_gone` is taken silently
//! in State::Active and ignored elsewhere (L9.15).
//!
//! The service loop (watcher events, credential changes and config reloads)
//! runs on a task of its own, so a call in it that waits (a reload waits
//! behind a credential write in flight, v3 7.6) never keeps the worker from
//! answering the supervisor.
//!
//! A standby serves nothing: it holds the sockets and doesn't accept on
//! them, so a client waits in the backlog, as it does while a listener is
//! paused. Its serve loops are built at take over (W3), with the final
//! routers.

//! W3 startup orders must remain distinct:
//! - First start: take the process lease, reopen logs off-runtime, activate
//!   credentials/config, attach updater before routers, bind listeners paused,
//!   start the watcher, then open the write gate, resume accepting and send
//!   active status. Update checks, refresh timers and rotation start at
//!   activation, without `go`; cleaning still waits for `peers_gone`.
//! - Take over (L9.1): take the lease; pass `take_over.before_reload`; reload
//!   credentials and config; read `handover.json` then delete it; build routers
//!   and listeners; in one final step open the gate, accept and send `ready`.
//!   Log `reopen()` runs off-runtime after the lease and before readiness.
//!   Serving starts at ready, not at go. Update checks, refresh timers and
//!   rotation start only at `go`; cleaning waits for `peers_gone`.
//!
//! A startup failure after opening the gate closes it before failure shutdown.
//! At take over, a failed `resume_accept` means the gate has already opened:
//! W3 must close the gate and clean up, send `take_failed`, and stay alive
//! holding the lease until `stop` (L9.16). A filesystem-blocked `close_sync`
//! at take over is bounded only by the supervisor's kill by handle. A failed
//! take over that stays alive until `stop` exits with failure, intentionally:
//! it failed.
//! A hand over first turns cleaning off, a step that cannot fail (L9.3).
//! This is also where W3 resets the `peers_gone` flag.
//! W3 awaits that step before resume; it is outside the aborted continuation.
//! A failed hand over follows L9.4/L9.16: restore only the prior gate and
//! accept state where required, keep management at 503 until `resume`,
//! and restart background work only on resume. Either kind of resume reopens
//! logs off-runtime and turns rotation on with the background work; after
//! `handed_over`, reopen only once the lease is retaken (L9.5, L9.18).
//! Ending management's 503 is the last step before `resumed` (L9.5).
//! Log controls are blocking: turn rotation on through `spawn_blocking`
//! (L9.18); cleaning waits for `peers_gone`.
//! `activation_stop_budget` comes from the welcome on every activation path,
//! including W3's take over, not from Timing's default.
//! The process-owned lease remains held until exit or a successful hand over.

use std::fs::{File, OpenOptions, TryLockError};
use std::future::{Future, pending};
use std::io;
use std::net::TcpListener as StdListener;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use open_ferry_core::config::{Config, ConfigWatcher, WatchEvent};
use open_ferry_core::gate::{ActivationKey, ActiveGate};
use open_ferry_providers::claude_cli;
use open_ferry_supervisor::protocol::{
    Budgets, Channel, Frame, Installation, ListenerTopology, Message, Role,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Interval, MissedTickBehavior};

use super::serve_loop::{FIRST_REQUEST_DEADLINE, ServeControl};
use super::{Options, Server, Service, management_listener, serve_with};
use crate::logging::LogLevel;
use crate::tls;

mod topology;
use topology::{Served, Topology};

#[cfg(test)]
mod tests;

/// How often the worker sends `status` while nothing changes.
pub(crate) const STATUS_EVERY: Duration = Duration::from_secs(5);

/// A cancelled send can leave a partial frame. Never reuse that channel.
const CONTROL_SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// How long an active worker waits between tries for the lease.
const LEASE_RETRY: Duration = Duration::from_millis(100);

/// The lease's file, in the data directory.
const LEASE_FILE: &str = "active-worker.lock";

/// What `welcome` says, as 2c takes it from the supervisor's first message.
#[derive(Debug, Clone)]
pub(crate) struct Welcome {
    pub(crate) role: Role,
    pub(crate) installation: Installation,
    pub(crate) budgets: Budgets,
    pub(crate) topology: Vec<ListenerTopology>,
    // W3: a switch's workers log and answer by its id.
    #[allow(dead_code)]
    pub(crate) switch_id: Option<String>,
}

impl Welcome {
    /// The fields of the `welcome` that the supervisor crate's
    /// `Pending::attach` returned.
    #[allow(dead_code, reason = "2c's -worker entry converts attach's welcome")]
    pub(crate) fn from_attached(welcome: open_ferry_supervisor::worker::Welcome) -> Self {
        let open_ferry_supervisor::worker::Welcome {
            role,
            installation,
            budgets,
            topology,
            switch_id,
        } = welcome;
        Self {
            role,
            installation,
            budgets,
            topology,
            switch_id,
        }
    }
}

#[cfg(test)]
type WatcherHook = Arc<dyn Fn(&ServeControl, &mut Config) + Send + Sync>;
#[cfg(test)]
type WatcherErrorHook = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
#[cfg(test)]
type ResumeErrorHook = Arc<dyn Fn(&open_ferry_core::gate::ActiveGate) + Send + Sync>;

/// The worker's own intervals, which a test shortens. The supervisor's
/// budgets come from `welcome`.
#[derive(Clone)]
pub(super) struct Timing {
    failed: Arc<AtomicBool>,
    pub(super) status_every: Duration,
    pub(super) lease_retry: Duration,
    pub(super) send_timeout: Duration,
    pub(super) shutdown_reserve: Duration,
    activation_stop_budget: Duration,
    #[cfg(test)]
    pub(super) routes: axum::Router,
    /// Runs on the service loop's task before it starts, to wedge it.
    #[cfg(test)]
    pub(super) wedge_service: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    pub(super) wedge_startup: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    pub(super) wedge_lease: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    pub(super) wedge_watcher: Option<WatcherHook>,
    #[cfg(test)]
    pub(super) fail_server: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    #[cfg(test)]
    pub(super) updates: Option<open_ferry_update::UpdateService>,
    #[cfg(test)]
    pub(super) fail_resume_accept: bool,
    #[cfg(test)]
    pub(super) fail_listener_when_sync_queued: bool,
    #[cfg(test)]
    pub(super) resume_error: Option<ResumeErrorHook>,
    #[cfg(test)]
    pub(super) watcher_error: Option<WatcherErrorHook>,
    /// Holds the spawned server task before its first poll until
    /// `activate` reaches the barrier for the listeners.
    #[cfg(test)]
    pub(super) hold_server: Option<Arc<tokio::sync::Notify>>,
    /// Which listener, by the topology's name, fails when it is made.
    #[cfg(test)]
    pub(super) fail_listener: Option<&'static str>,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            failed: Arc::new(AtomicBool::new(false)),
            status_every: STATUS_EVERY,
            lease_retry: LEASE_RETRY,
            send_timeout: CONTROL_SEND_TIMEOUT,
            activation_stop_budget: Duration::from_secs(5),
            shutdown_reserve: super::LEDGER_SHUTDOWN_TIMEOUT + Duration::from_millis(100),
            #[cfg(test)]
            routes: axum::Router::new(),
            #[cfg(test)]
            wedge_service: None,
            #[cfg(test)]
            wedge_startup: None,
            #[cfg(test)]
            wedge_lease: None,
            #[cfg(test)]
            wedge_watcher: None,
            #[cfg(test)]
            fail_server: None,
            #[cfg(test)]
            updates: None,
            #[cfg(test)]
            fail_resume_accept: false,
            #[cfg(test)]
            fail_listener_when_sync_queued: false,
            #[cfg(test)]
            resume_error: None,
            #[cfg(test)]
            watcher_error: None,
            #[cfg(test)]
            hold_server: None,
            #[cfg(test)]
            fail_listener: None,
        }
    }
}

/// Runs a supervised worker whose `welcome` is `welcome`, on `channel`,
/// serving `listeners` (each the socket for the topology entry of its
/// name), until it is told to stop, is released and drained, or fails.
/// 2c's `main` calls it, after reading `welcome` and making the sockets.
/// This wrapper exits the process itself; 2c must call it rather than
/// `run_with`, without dropping the runtime or waiting for blocking tasks. Abandoned writers
/// retain the process-owned lease until the OS ends the process.
// 2c wires it.
#[allow(dead_code)]
pub(crate) async fn run<R, W>(
    config_path: PathBuf,
    log_level: LogLevel,
    options: Options,
    channel: Channel<R, W>,
    welcome: Welcome,
    listeners: Vec<(String, StdListener)>,
) -> !
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let file_log = log_level.file_log().clone();
    let (code, deadline) = run_with_deadline(
        config_path,
        log_level,
        options,
        channel,
        welcome,
        listeners,
        Timing::default(),
    )
    .await;
    std::process::exit(finish(code, deadline, &file_log).await);
}

/// The returning test seam for [`run`], with the worker's own intervals given.
#[cfg(test)]
pub(super) async fn run_with<R, W>(
    config_path: PathBuf,
    log_level: LogLevel,
    options: Options,
    channel: Channel<R, W>,
    welcome: Welcome,
    listeners: Vec<(String, StdListener)>,
    timing: Timing,
) -> ExitCode
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    run_with_deadline(
        config_path,
        log_level,
        options,
        channel,
        welcome,
        listeners,
        timing,
    )
    .await
    .0
}

/// Flushes queued diagnostics off-runtime, with no time beyond the stop deadline.
async fn finish(
    code: ExitCode,
    deadline: tokio::time::Instant,
    file_log: &crate::file_log::FileLog,
) -> i32 {
    let file_log = file_log.clone();
    finish_with(code, deadline, move |left| file_log.flush(left)).await
}

async fn finish_with(
    code: ExitCode,
    deadline: tokio::time::Instant,
    flush: impl FnOnce(Duration) + Send + 'static,
) -> i32 {
    if deadline > tokio::time::Instant::now() {
        let flush = tokio::task::spawn_blocking(move || {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if !left.is_zero() {
                flush(left);
            }
        });
        let _ = tokio::time::timeout_at(deadline, flush).await;
    }
    if code == ExitCode::SUCCESS { 0 } else { 1 }
}

async fn run_with_deadline<R, W>(
    config_path: PathBuf,
    log_level: LogLevel,
    options: Options,
    channel: Channel<R, W>,
    welcome: Welcome,
    listeners: Vec<(String, StdListener)>,
    timing: Timing,
) -> (ExitCode, tokio::time::Instant)
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let started_at = Instant::now();
    let budgets = welcome.budgets.clone();
    // A connection that has carried no request when the release comes gets
    // the drain budget, and never more than the loop's own deadline.
    let control = ServeControl::with_first_request_deadline(
        FIRST_REQUEST_DEADLINE.min(Duration::from_millis(budgets.drain_ms)),
    );
    let mut status = tokio::time::interval_at(
        tokio::time::Instant::now() + timing.status_every,
        timing.status_every,
    );
    status.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let startup: Startup = Box::pin(start(
        config_path,
        log_level,
        options,
        welcome,
        listeners,
        timing.clone(),
        control.clone(),
    ));
    let mut worker = Worker {
        channel,
        channel_lost: false,
        stop_deadline: None,
        failed: timing.failed.clone(),
        send_timeout: timing.send_timeout,
        budgets,
        control,
        started_at,
        mode: "unknown".to_owned(),
        peers_gone: false,
        state: State::Loading,
    };
    let code = worker.drive(startup, &mut status).await;
    let deadline = worker.stop_deadline.unwrap_or_else(|| {
        tokio::time::Instant::now() + Duration::from_millis(worker.budgets.stop_ms)
    });
    (code, deadline)
}

/// The start of a worker, standby or active: it ends in the state the
/// welcome's role calls for, or in the reason it couldn't.
type Startup = Pin<Box<dyn Future<Output = Result<Started, Failure>> + Send>>;

/// How a start failed.
struct Failure {
    role: Role,
    reason: String,
}

/// What a start made.
struct Started {
    /// The `self-update` mode of the config the worker loaded.
    mode: String,
    state: Ready,
}

enum Ready {
    Standby(Box<Standby>),
    Active(Box<Active>),
}

/// The state a worker is in.
enum State {
    /// The start is running.
    Loading,
    /// Loaded, and held: the gate is closed, nothing runs, and the sockets
    /// aren't accepted on.
    #[allow(dead_code)] // W3: take_over consumes the prepared standby
    Standby(Box<Standby>),
    /// Serving.
    Active(Box<Active>),
    /// Released: no new connections, and the existing ones finishing.
    Draining(Box<Draining>),
}

impl State {
    fn phase(&self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Standby(_) => "standby",
            Self::Active(_) => "active",
            Self::Draining(_) => "draining",
        }
    }
}

/// A loaded standby.
// W3's `take_over` activates it: it needs all of these.
#[allow(dead_code)]
struct Standby {
    service: Service,
    key: ActivationKey,
    topology: Topology,
    config: Arc<Config>,
    tls: Option<Arc<rustls::ServerConfig>>,
}

/// A serving worker: the lease, and the task that runs the service loop and
/// owns the service and the server.
#[must_use = "an active worker must be stopped explicitly"]
struct Active {
    armed: bool,
    failed: Arc<AtomicBool>,
    control: ServeControl,
    /// Gives the task its stop budget.
    stop: Option<oneshot::Sender<tokio::time::Instant>>,
    /// Says when the server ended: whether it ended without an error.
    ended: mpsc::UnboundedReceiver<bool>,
    task: Option<JoinHandle<()>>,
    file_log: crate::file_log::FileLog,
    #[cfg(test)]
    stop_timeout_hook: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    /// Held until the process exits, or (W3) a hand over completes.
    _lease: ProcessLease,
}

fn failure_code(code: ExitCode, failed: &AtomicBool) -> ExitCode {
    if failed.load(Ordering::SeqCst) {
        ExitCode::FAILURE
    } else {
        code
    }
}

impl Active {
    /// Not cancel-safe: await this to completion; it is already bounded by
    /// the deadline. Dropping it cuts acceptance, flushes, and exits the process.
    async fn stop(mut self, deadline: tokio::time::Instant) -> bool {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(deadline);
        }
        let task = self.task.as_mut().expect("active service task");
        let stopped = tokio::time::timeout_at(deadline, &mut *task).await;
        if matches!(&stopped, Ok(Err(_))) {
            self.failed.store(true, Ordering::SeqCst);
        }
        if !matches!(stopped, Ok(Ok(()))) {
            let cut = self.control.cut();
            tracing::warn!("the service loop didn't stop cleanly; cut {cut} connection(s)");
            #[cfg(test)]
            if stopped.is_err()
                && let Some((arrived, release)) = &self.stop_timeout_hook
            {
                arrived.notify_one();
                release.notified().await;
            }
            task.abort();
            // No new grace period: completed panics are observed even at the
            // deadline, and unfinished cancellation gets only the time left.
            if stopped.is_err()
                && let Ok(Err(error)) = tokio::time::timeout_at(deadline, &mut *task).await
                && error.is_panic()
            {
                self.failed.store(true, Ordering::SeqCst);
            }
        }
        self.task.take();
        self.armed = false;
        self.failed.load(Ordering::SeqCst)
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        if self.armed {
            self.failed.store(true, Ordering::SeqCst);
            tracing::error!("active worker dropped without stop; emergency cut");
            self.control.stop();
            self.control.cut();
            if let Some(task) = &self.task {
                task.abort();
            }
            // The OS, never this drop, releases the process-owned lease.
            let file_log = self.file_log.clone();
            emergency_exit_with(move |cap| file_log.flush(cap));
        }
    }
}

/// Flushes on an independent helper thread: even a wedged log writer or
/// runtime cannot prevent the emergency exit after the fixed cap.
fn emergency_exit_with(flush: impl FnOnce(Duration) + Send + 'static) {
    const CAP: Duration = Duration::from_millis(250);
    let deadline = Instant::now() + CAP;
    let (finished, done) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("worker-emergency-flush".into())
        .spawn(move || {
            flush(deadline.saturating_duration_since(Instant::now()));
            let _ = finished.send(());
        });
    let _ = done.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    #[cfg(not(test))]
    std::process::exit(1);
}

/// Records an unwind before a service panic reaches its join handle.
struct PanicFailure(Arc<AtomicBool>);
impl Drop for PanicFailure {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, Ordering::SeqCst);
        }
    }
}

struct Draining {
    active: Active,
    drained: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// What the worker does next.
enum Flow {
    Continue,
    /// Stop serving within the budget, then exit with the code.
    Stop(Duration, ExitCode),
    /// Exit at once with the code: nothing is serving.
    Exit(ExitCode),
}

enum Event {
    Frame(io::Result<Option<Frame<Message>>>),
    Tick,
    Started(Result<Started, Failure>),
    Drained,
    /// The server ended, without an error when true.
    ServerEnded(bool),
}

struct Worker<R, W> {
    channel: Channel<R, W>,
    channel_lost: bool,
    stop_deadline: Option<tokio::time::Instant>,
    failed: Arc<AtomicBool>,
    send_timeout: Duration,
    budgets: Budgets,
    control: ServeControl,
    started_at: Instant,
    mode: String,
    /// Whether the supervisor has said no other worker is alive while Active
    /// (active or watching). W3 resets this when hand over turns cleaning off.
    #[allow(dead_code)] // W-logs: log cleaning starts at the first one
    peers_gone: bool,
    state: State,
}

impl<R, W> Worker<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    /// Takes events until one ends the worker, and does what it says.
    async fn drive(&mut self, startup: Startup, status: &mut Interval) -> ExitCode {
        let mut startup = Some(startup);
        loop {
            let event = self.next_event(&mut startup, status).await;
            let flow = match event {
                Event::Frame(Ok(Some(frame))) => self.on_message(frame).await,
                Event::Frame(Ok(None)) => {
                    tracing::info!("the supervisor closed the control channel; stopping");
                    self.on_eof()
                }
                Event::Frame(Err(error)) => {
                    tracing::error!("the control channel failed: {error}; stopping");
                    self.on_eof()
                }
                Event::Tick => {
                    self.send_status().await;
                    Flow::Continue
                }
                Event::Started(result) => {
                    startup = None;
                    self.on_started(result).await
                }
                Event::Drained => self.on_drained().await,
                Event::ServerEnded(clean) => self.on_server_ended(clean),
            };
            let flow = if self.channel_lost && !matches!(flow, Flow::Exit(_)) {
                self.on_eof()
            } else {
                flow
            };
            match flow {
                Flow::Continue => {}
                Flow::Exit(code) => return failure_code(code, &self.failed),
                Flow::Stop(budget, code) => {
                    let failed = self.stop_serving(budget).await;
                    if failed {
                        self.failed.store(true, Ordering::SeqCst);
                    }
                    return failure_code(code, &self.failed);
                }
            }
        }
    }

    async fn next_event(&mut self, startup: &mut Option<Startup>, status: &mut Interval) -> Event {
        let Self { channel, state, .. } = self;
        let starting = async {
            match startup {
                Some(startup) => startup.await,
                None => pending().await,
            }
        };
        let in_state = async {
            match state {
                State::Draining(draining) => tokio::select! {
                    () = draining.drained.as_mut() => Event::Drained,
                    clean = draining.active.ended.recv() => Event::ServerEnded(clean.unwrap_or(false)),
                },
                State::Active(active) => match active.ended.recv().await {
                    Some(clean) => Event::ServerEnded(clean),
                    None => Event::ServerEnded(false),
                },
                _ => pending().await,
            }
        };
        tokio::select! {
            frame = channel.receive::<Message>() => Event::Frame(frame),
            _ = status.tick() => Event::Tick,
            started = starting => Event::Started(started),
            event = in_state => event,
        }
    }

    /// Answers one message by what the worker is doing.
    async fn on_message(&mut self, frame: Frame<Message>) -> Flow {
        match frame.message {
            Message::Ping => {
                self.send_status().await;
                Flow::Continue
            }
            Message::Status { .. } => {
                tracing::debug!("ignoring a status from the supervisor");
                Flow::Continue
            }
            Message::Release => self.on_release().await,
            Message::Stop { budget_ms } => {
                Flow::Stop(Duration::from_millis(budget_ms), ExitCode::SUCCESS)
            }
            // W3: the switch messages.
            Message::HandOver { .. } | Message::Resume | Message::TakeOver | Message::Go => {
                tracing::error!(
                    "the {} message isn't supported by this worker yet",
                    frame.message_type
                );
                self.send(&Message::Unsupported {
                    message_type: frame.message_type,
                })
                .await;
                Flow::Continue
            }
            // No reply. Only Active (active or watching) counts the first.
            // This guard is right only while W3 keeps handing-over,
            // handed-over, taking-over and ready out of State::Active.
            // W3 resets the flag when hand over turns cleaning off.
            Message::PeersGone if matches!(self.state, State::Active(_)) => {
                if std::mem::replace(&mut self.peers_gone, true) {
                    tracing::debug!("ignoring a repeated peers_gone");
                } else {
                    tracing::info!("no other worker is running");
                }
                Flow::Continue
            }
            Message::Unknown => {
                tracing::warn!("unsupported message {:?}", frame.message_type);
                self.send(&Message::Unsupported {
                    message_type: frame.message_type,
                })
                .await;
                Flow::Continue
            }
            // A message that is the worker's to send, or that the supervisor
            // sends before the worker is running.
            _ => {
                tracing::debug!(
                    "ignoring {:?}, which makes no sense while {}",
                    frame.message_type,
                    self.state.phase()
                );
                Flow::Continue
            }
        }
    }

    /// The channel closed or failed: stop, as `stop` with the stop budget.
    fn on_eof(&self) -> Flow {
        Flow::Stop(
            Duration::from_millis(self.budgets.stop_ms),
            ExitCode::SUCCESS,
        )
    }

    async fn on_release(&mut self) -> Flow {
        match std::mem::replace(&mut self.state, State::Loading) {
            State::Active(active) => {
                let drained = Box::pin(self.control.release());
                self.state = State::Draining(Box::new(Draining {
                    active: *active,
                    drained,
                }));
                self.send_status().await;
                Flow::Continue
            }
            other => {
                tracing::debug!("ignoring release while {}", other.phase());
                self.state = other;
                Flow::Continue
            }
        }
    }

    /// The last connection of a released worker ended.
    async fn on_drained(&mut self) -> Flow {
        self.send(&Message::Drained).await;
        Flow::Stop(
            Duration::from_millis(self.budgets.stop_ms),
            ExitCode::SUCCESS,
        )
    }

    fn on_server_ended(&self, clean: bool) -> Flow {
        if clean && matches!(self.state, State::Draining(_)) {
            // The loops end when the listeners are dropped: expected.
            tracing::debug!("the server's loops ended");
            return Flow::Continue;
        }
        self.failed.store(true, Ordering::SeqCst);
        tracing::error!(
            "the server ended while the worker is active{}; stopping",
            if clean { "" } else { " with an error" }
        );
        Flow::Stop(
            Duration::from_millis(self.budgets.stop_ms),
            ExitCode::FAILURE,
        )
    }

    async fn on_started(&mut self, result: Result<Started, Failure>) -> Flow {
        match result {
            Ok(Started { mode, state }) => {
                self.mode = mode;
                match state {
                    Ready::Standby(standby) => self.state = State::Standby(standby),
                    Ready::Active(active) => self.state = State::Active(active),
                }
                // The supervisor counts a start until `loaded`, in either role. It comes
                // before the first active or standby status; a loading one may precede it.
                let ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                self.send(&Message::Loaded { ms }).await;
                self.send_status().await;
                Flow::Continue
            }
            Err(Failure { role, reason }) => {
                self.failed.store(true, Ordering::SeqCst);
                tracing::error!("the {role:?} worker failed to load: {reason}");
                self.send(&Message::LoadFailed { reason }).await;
                Flow::Exit(ExitCode::FAILURE)
            }
        }
    }

    /// Stops serving, if the worker does, within `budget`: gracefully, then
    /// cutting what is left.
    async fn stop_serving(&mut self, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        self.stop_deadline = Some(deadline);
        self.control.stop();
        let active = match std::mem::replace(&mut self.state, State::Loading) {
            State::Active(active) => *active,
            State::Draining(draining) => draining.active,
            _ => return false,
        };
        active.stop(deadline).await
    }

    async fn send_status(&mut self) {
        let message = Message::Status {
            mode: self.mode.clone(),
            live: u64::try_from(self.control.live_count()).unwrap_or(u64::MAX),
            // The sign-ins in progress are in the management state, which
            // only the service owns.
            signins_pending: 0,
            phase: self.state.phase().to_owned(),
        };
        self.send(&message).await;
    }

    async fn send(&mut self, message: &Message) {
        if self.channel_lost {
            return;
        }
        match tokio::time::timeout(self.send_timeout, self.channel.send(message)).await {
            Ok(Ok(())) => {}
            result => {
                tracing::debug!("couldn't send to the supervisor: {result:?}; stopping");
                self.channel_lost = true;
            }
        }
    }
}

/// The lease on the active role: a lock on `<data dir>/active-worker.lock`,
/// held by the active worker for the life of the process. The handle isn't
/// inherited by children (std opens files as non-inheritable on Windows and
/// close-on-exec on Unix), and the lock goes with the handle.
#[derive(Debug)]
pub(super) struct Lease {
    _file: File,
}

/// A lease owned by the process, including cancelled or failed startup.
/// W3 may explicitly take the lease out only after a completed hand over.
struct ProcessLease(Option<Lease>);

impl Drop for ProcessLease {
    fn drop(&mut self) {
        if let Some(lease) = self.0.take() {
            std::mem::forget(lease);
        }
    }
}

impl Lease {
    /// Takes the lease, creating the directory: `Ok(None)` while another
    /// handle holds it.
    pub(super) fn try_take(data_dir: &Path) -> io::Result<Option<Self>> {
        std::fs::create_dir_all(data_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(data_dir.join(LEASE_FILE))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    /// Takes the lease, trying again every `retry` for up to `within`.
    async fn take(
        data_dir: &Path,
        within: Duration,
        timing: &Timing,
    ) -> Result<ProcessLease, String> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let data_dir = data_dir.to_path_buf();
            #[cfg(test)]
            let wedge = timing.wedge_lease.clone();
            let attempt = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some(wedge) = wedge {
                    wedge();
                }
                // Construct the process owner here, before a cancellation
                // can discard the blocking task's result.
                Self::try_take(&data_dir).map(|lease| lease.map(|lease| ProcessLease(Some(lease))))
            });
            match tokio::time::timeout_at(deadline, attempt).await {
                Ok(Ok(Ok(Some(lease)))) => return Ok(lease),
                Ok(Ok(Ok(None))) => {}
                Ok(Ok(Err(error))) => {
                    return Err(format!("couldn't take the active lease: {error}"));
                }
                Ok(Err(error)) => return Err(format!("lease task failed: {error}")),
                Err(_) => return Err("active lease acquisition timed out".to_owned()),
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("another worker holds the active lease".to_owned());
            }
            tokio::time::sleep(timing.lease_retry.min(deadline - now)).await;
        }
    }
}

/// What a start has made when it comes to serve or hold.
struct Prepared {
    service: Service,
    key: ActivationKey,
    topology: Topology,
    config: Arc<Config>,
    tls: Option<Arc<rustls::ServerConfig>>,
}

/// The start: the lease first for an active worker, then the config and the
/// service, then, for a standby, the dry build of the routes, or for an
/// active worker the first activation and serving.
async fn start(
    config_path: PathBuf,
    log_level: LogLevel,
    options: Options,
    welcome: Welcome,
    listeners: Vec<(String, StdListener)>,
    timing: Timing,
    control: ServeControl,
) -> Result<Started, Failure> {
    let mut timing = timing;
    timing.activation_stop_budget = Duration::from_millis(welcome.budgets.stop_ms);
    let role = welcome.role.clone();
    let failed = |reason: String| Failure {
        role: role.clone(),
        reason,
    };
    let lease = match role {
        Role::Active => Some(
            Lease::take(
                &welcome.installation.data_dir,
                Duration::from_millis(welcome.budgets.load_ms),
                &timing,
            )
            .await
            .map_err(failed)?,
        ),
        Role::Standby => None,
    };
    let prepare_path = config_path.clone();
    let prepare_options = options.clone();
    let standby = role == Role::Standby;
    #[cfg(test)]
    let wedge = timing.wedge_startup.clone();
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let prepared = tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            #[cfg(test)]
            if let Some(wedge) = wedge {
                wedge();
            }
            let prepared = prepare(
                &prepare_path,
                log_level,
                prepare_options,
                &welcome.topology,
                listeners,
                standby,
            )?;
            if standby {
                dry_build(&prepared)?;
            }
            Ok::<_, String>(prepared)
        })
    })
    .await
    .map_err(|error| failed(format!("preparation failed: {error}")))?
    .map_err(failed)?;
    let mode = match prepared.config.self_update.mode.trim() {
        "" => "auto".to_owned(),
        mode => mode.to_owned(),
    };
    let Some(lease) = lease else {
        let Prepared {
            service,
            key,
            topology,
            config,
            tls,
        } = prepared;
        return Ok(Started {
            mode,
            state: Ready::Standby(Box::new(Standby {
                service,
                key,
                topology,
                config,
                tls,
            })),
        });
    };
    let active = activate(prepared, &options, &config_path, &control, lease, &timing)
        .await
        .map_err(failed)?;
    Ok(Started {
        mode,
        state: Ready::Active(Box::new(active)),
    })
}

/// Loads the config, and builds the service on a closed gate. A standby
/// holds the cooldown store off.
fn prepare(
    config_path: &Path,
    log_level: LogLevel,
    mut options: Options,
    welcome: &[ListenerTopology],
    listeners: Vec<(String, StdListener)>,
    standby: bool,
) -> Result<Prepared, String> {
    let (config, sha256) = Config::load_with_sha256(config_path)
        .map_err(|error| format!("failed to load config: {error}"))?;
    log_level.set_debug(config.debug);
    let auth_dir = config
        .resolve_auth_dir()
        .map_err(|error| format!("failed to resolve auth directory: {error}"))?;
    if config.has_example_api_keys() {
        tracing::error!(
            api_keys = %config.example_api_keys().join(","),
            "unsafe example API key configured; proxy API endpoints disabled until api-keys is updated"
        );
    }
    let topology = Topology::from_welcome(welcome, listeners)?;
    topology.warn_once(&config, &mut false);
    let tls = if topology.needs_tls() {
        Some(
            tls::load(&config.tls.cert, &config.tls.key)
                .map_err(|error| format!("failed to load the TLS certificate: {error}"))?,
        )
    } else {
        None
    };
    options.config_sha256 = Some(sha256);
    let (gate, key) = ActiveGate::standby();
    let config = Arc::new(config);
    let mut service = Service::prepared(
        Arc::clone(&config),
        config_path.to_path_buf(),
        auth_dir,
        log_level,
        &options,
        gate,
    );
    if standby {
        service.hold_cooldown_store();
    }
    Ok(Prepared {
        service,
        key,
        topology,
        config,
        tls,
    })
}

/// Builds the routes a standby will serve, with the usage ledger
/// unavailable, to find out now whether they build. A panic is a failed load.
fn dry_build(prepared: &Prepared) -> Result<(), String> {
    let service = &prepared.service;
    let built = catch_unwind(AssertUnwindSafe(|| {
        let extra = axum::Router::new();
        if prepared.topology.management.is_some() {
            drop(management_listener::apps(service, extra));
        } else {
            drop(service.app_with(extra));
        }
    }));
    built.map_err(|_| "building the routes panicked".to_owned())
}

/// Makes a prepared worker the active one: the first activation, the
/// background work, serving the topology, and the service loop's task.
async fn activate(
    prepared: Prepared,
    options: &Options,
    config_path: &Path,
    control: &ServeControl,
    lease: ProcessLease,
    timing: &Timing,
) -> Result<Active, String> {
    let Prepared {
        mut service,
        key,
        topology,
        config,
        tls,
    } = prepared;
    let self_update = options.self_update;
    #[cfg(test)]
    let injected_updates = timing.updates.clone();
    let (mut service, activated) = tokio::task::spawn_blocking(move || {
        if self_update {
            #[cfg(test)]
            if let Some(updates) = injected_updates {
                service.management = service.management.clone().with_updates(updates);
            } else {
                service.prepare_update_checks();
            }
            #[cfg(not(test))]
            service.prepare_update_checks();
        }
        let activated = service.first_activation(&key);
        (service, activated)
    })
    .await
    .map_err(|error| format!("activation task failed: {error}"))?;
    if let Err(error) = activated {
        timing.failed.store(true, Ordering::SeqCst);
        let (stop, _) = watch::channel(false);
        super::shut_down_by(
            &service,
            &stop,
            None,
            control,
            tokio::time::Instant::now(),
            timing.shutdown_reserve,
        )
        .await;
        return Err(format!("failed to activate the service: {error}"));
    }
    // Bind fallibly now, but start the accept loops paused. W3 must copy
    // this order: reopen the write gate, resume accepting, then send ready
    // as one final take-over step (L9.1.6). This is distinct from reopening
    // the log file under L9.18, which happens earlier, after taking the lease.
    control.pause_accept().await;
    #[cfg(test)]
    let extra = timing.routes.clone();
    #[cfg(not(test))]
    let extra = axum::Router::new();
    // The listeners the loops make: the main one, and the management one.
    let expected_listeners = 1 + usize::from(topology.management.is_some());
    #[cfg(test)]
    let fail_port = timing.fail_listener.and_then(|name| {
        let served = if name == topology::MANAGEMENT {
            topology.management.as_ref()
        } else {
            Some(&topology.main)
        };
        served
            .and_then(|served| served.listener.local_addr().ok())
            .map(|address| address.port())
    });
    let (server, stop_server) = match serve(
        &service,
        topology,
        tls,
        control,
        extra,
        timing.failed.clone(),
        #[cfg(test)]
        timing.hold_server.clone(),
    ) {
        Ok(serving) => serving,
        Err(error) => {
            timing.failed.store(true, Ordering::SeqCst);
            let (stop, _) = watch::channel(false);
            super::shut_down_by(
                &service,
                &stop,
                None,
                control,
                tokio::time::Instant::now(),
                timing.shutdown_reserve,
            )
            .await;
            return Err(format!("failed to serve the sockets: {error}"));
        }
    };
    #[cfg(test)]
    let server = if let Some((fail, _)) = timing.fail_server.clone() {
        tokio::spawn(record_server(
            async move {
                let mut server = server;
                tokio::select! {
                    _ = fail.notified() => {
                        server.abort();
                        Err(io::Error::other("injected API server failure"))
                    }
                    result = &mut server => result.unwrap_or_else(|error| Err(io::Error::other(error))),
                }
            },
            timing.failed.clone(),
        ))
    } else {
        server
    };
    #[cfg(test)]
    if let Some(port) = fail_port {
        let queued = Arc::new(tokio::sync::Notify::new());
        queued.notify_one();
        control.fail_listener_after(port, queued);
    }
    #[cfg(test)]
    let listener_failure = timing.fail_listener_when_sync_queued.then(|| {
        let queued = Arc::new(tokio::sync::Notify::new());
        control.fail_listener_after(service.config.port as u16, queued.clone());
        queued
    });
    let watcher_path = config_path.to_path_buf();
    let watcher_config = Arc::clone(&config);
    #[cfg(test)]
    let wedge = timing.wedge_watcher.clone();
    #[cfg(test)]
    let watcher_control = control.clone();
    let watcher = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        let watcher_config = {
            let mut config = Config::clone(&watcher_config);
            if let Some(wedge) = wedge {
                wedge(&watcher_control, &mut config);
            }
            Arc::new(config)
        };
        ConfigWatcher::start(watcher_path, &watcher_config)
    })
    .await
    .map_err(|error| format!("watcher task failed: {error}"));
    let watcher = watcher.and_then(|result| result.map_err(|error| error.to_string()));
    let events = match watcher {
        Ok((watcher, events)) => {
            service.watcher = Some(watcher);
            events
        }
        Err(error) => {
            timing.failed.store(true, Ordering::SeqCst);
            #[cfg(test)]
            if let Some(observed) = &timing.watcher_error {
                observed().await;
            }
            service.close_sync();
            service.close_reloads();
            super::shut_down_by(
                &service,
                &stop_server,
                Some(server),
                control,
                tokio::time::Instant::now(),
                timing.shutdown_reserve,
            )
            .await;
            return Err(format!("failed to create watcher: {error}"));
        }
    };
    tracing::info!("file watcher started for config and auth directory changes");
    service.gate.reopen();
    #[cfg(test)]
    if timing.fail_resume_accept {
        control.cut();
    }
    let resume = control.resume_accept();
    #[cfg(test)]
    let resume = async {
        tokio::select! {
            result = resume => result,
            _ = async {
                let Some(queued) = listener_failure else {
                    pending::<()>().await;
                    return;
                };
                while service.sync_requests.is_empty() {
                    tokio::task::yield_now().await;
                }
                queued.notify_one();
                pending::<()>().await;
            } => unreachable!(),
        }
    };
    let resumed = async {
        resume.await?;
        // The spawned server task is first polled when it is scheduled, so
        // `resume_accept` can return before any loop holds a listener (it
        // leaves out the loops not yet polled). `loaded` follows this, so
        // every listener must exist, or a failure to make one is a failed
        // start. No timer here: the supervisor's load timeout ends a loading
        // worker by its handle.
        #[cfg(test)]
        if let Some(hold) = &timing.hold_server {
            hold.notify_one();
        }
        control.await_listeners(expected_listeners).await
    };
    if let Err(error) = resumed.await {
        timing.failed.store(true, Ordering::SeqCst);
        #[cfg(test)]
        if let Some(observed) = &timing.resume_error {
            observed(&service.gate);
        }
        // Another listener may already have accepted an owned management
        // write. Cut acceptance and settle its queues before closing the
        // gate; no background work started. Retain the lease until exit.
        let deadline = tokio::time::Instant::now() + timing.activation_stop_budget;
        control.cut();
        // Settlement does synchronous credential work on this task. A
        // stalled filesystem here is bounded by the supervisor's load timeout,
        // which ends a loading worker by its handle. At take over, a blocked
        // close_sync is bounded only by the supervisor's kill by handle. A
        // failed take over held until stop intentionally exits with failure.
        service.close_sync();
        service.close_reloads();
        let _ = tokio::time::timeout_at(deadline, service.gate.close(service.gate.epoch())).await;
        super::shut_down_by(
            &service,
            &stop_server,
            Some(server),
            control,
            deadline,
            timing.shutdown_reserve,
        )
        .await;
        return Err(format!("failed to resume accepting: {error}"));
    }
    if let Some(updates) = service.management.updates() {
        let first_check =
            super::update_settings::between(super::FIRST_CHECK.0, super::FIRST_CHECK.1);
        #[cfg(test)]
        let first_check = if timing.updates.is_some() {
            Duration::ZERO
        } else {
            first_check
        };
        updates.start(first_check);
    }
    claude_cli::warn_outdated(&config.claude_cli);
    service.start_auto_refresh();
    let (stop, stop_rx) = oneshot::channel();
    let (ended_tx, ended) = mpsc::unbounded_channel();
    let failed = timing.failed.clone();
    let file_log = service.log_level.file_log().clone();
    let serving = Serving {
        failed: failed.clone(),
        service,
        config_path: config_path.to_path_buf(),
        events: Some(events),
        server,
        stop_server,
        control: control.clone(),
        ended: ended_tx,
        shutdown_reserve: timing.shutdown_reserve,
        #[cfg(test)]
        wedge: timing.wedge_service.clone(),
        #[cfg(test)]
        server_failure_seen: timing.fail_server.as_ref().map(|(_, seen)| seen.clone()),
    };
    let _ = timing;
    let task = tokio::spawn(service_loop(serving, stop_rx));
    Ok(Active {
        armed: true,
        failed,
        control: control.clone(),
        stop: Some(stop),
        ended,
        task: Some(task),
        file_log,
        #[cfg(test)]
        stop_timeout_hook: None,
        _lease: lease,
    })
}

/// Serves `topology`'s sockets on `control`: the main one, and the
/// management one, each with TLS as its flag says. The sender stops the
/// server gracefully.
fn serve(
    service: &Service,
    topology: Topology,
    tls: Option<Arc<rustls::ServerConfig>>,
    control: &ServeControl,
    extra: axum::Router,
    failed: Arc<AtomicBool>,
    #[cfg(test)] hold: Option<Arc<tokio::sync::Notify>>,
) -> io::Result<(Server, watch::Sender<bool>)> {
    let (stop_server, stopped) = watch::channel(false);
    let tls_for = |served: &Served| tls.clone().filter(|_| served.tls);
    let main_tls = tls_for(&topology.main);
    let main = tokio_listener(topology.main.listener)?;
    let server = match topology.management {
        None => tokio::spawn(record_server(
            held(
                serve_with(
                    control.clone(),
                    main,
                    main_tls,
                    service.app_with(extra),
                    stopped,
                ),
                #[cfg(test)]
                hold,
            ),
            failed.clone(),
        )),
        Some(served) => {
            let management_tls = tls_for(&served);
            let management = tokio_listener(served.listener)?;
            let (main_app, management_app) = management_listener::apps(service, extra);
            tokio::spawn(record_server(
                held(
                    management_listener::serve_both(
                        serve_with(control.clone(), main, main_tls, main_app, stopped.clone()),
                        serve_with(
                            control.clone(),
                            management,
                            management_tls,
                            management_app,
                            stopped,
                        ),
                    ),
                    #[cfg(test)]
                    hold,
                ),
                failed,
            ))
        }
    };
    Ok((server, stop_server))
}

/// The server's future, held before its first poll until `hold` is
/// notified, in a test that needs the activation to get ahead of the loops.
#[cfg(test)]
async fn held(
    server: impl Future<Output = io::Result<()>>,
    hold: Option<Arc<tokio::sync::Notify>>,
) -> io::Result<()> {
    if let Some(hold) = hold {
        hold.notified().await;
    }
    server.await
}

#[cfg(not(test))]
fn held(server: impl Future<Output = io::Result<()>>) -> impl Future<Output = io::Result<()>> {
    server
}

/// Records API failures at their source, even when the service loop is
/// blocked or cancelled before observing the server's completion.
async fn record_server(
    future: impl Future<Output = io::Result<()>>,
    failed: Arc<AtomicBool>,
) -> io::Result<()> {
    let _panic_failure = PanicFailure(failed.clone());
    let result = future.await;
    if result.is_err() {
        failed.store(true, Ordering::SeqCst);
    }
    result
}

fn tokio_listener(listener: StdListener) -> io::Result<TcpListener> {
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

/// What the service loop's task owns.
struct Serving {
    failed: Arc<AtomicBool>,
    service: Service,
    config_path: PathBuf,
    events: Option<mpsc::Receiver<WatchEvent>>,
    server: Server,
    stop_server: watch::Sender<bool>,
    control: ServeControl,
    /// Says the server ended, without an error when true.
    ended: mpsc::UnboundedSender<bool>,
    shutdown_reserve: Duration,
    #[cfg(test)]
    wedge: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    server_failure_seen: Option<Arc<tokio::sync::Notify>>,
}

/// The service loop of a worker, as the unsupervised run's: it applies the
/// watcher's events, the management API's credential changes and config
/// reloads, and the catalogs' changes, until told to stop with a budget. It
/// then gives the server that long, and cuts what is left.
async fn service_loop(serving: Serving, stop: oneshot::Receiver<tokio::time::Instant>) {
    let Serving {
        failed,
        mut service,
        config_path,
        mut events,
        server,
        stop_server,
        control,
        ended,
        shutdown_reserve,
        #[cfg(test)]
        wedge,
        #[cfg(test)]
        server_failure_seen,
    } = serving;
    let _panic_failure = PanicFailure(failed.clone());
    #[cfg(test)]
    if let Some(wedge) = wedge {
        wedge();
    }
    let mut server = Some(server);
    tokio::pin!(stop);
    let deadline = loop {
        tokio::select! {
            deadline = &mut stop => match deadline {
                Ok(deadline) => break deadline,
                Err(_) => {
                    // An invariant violation, not an implicit zero-budget
                    // graceful stop. W3 must send its deadline explicitly.
                    failed.store(true, Ordering::SeqCst);
                    tracing::error!("worker stop sender dropped without a deadline; emergency cut");
                    control.stop();
                    control.cut();
                    let _ = ended.send(false);
                    break tokio::time::Instant::now();
                }
            },
            result = async {
                match &mut server {
                    Some(server) => server.await,
                    None => pending().await,
                }
            } => {
                server = None;
                let clean = matches!(result, Ok(Ok(())));
                if !clean { failed.store(true, Ordering::SeqCst); }
                #[cfg(test)]
                if !clean && let Some(seen) = &server_failure_seen { seen.notify_one(); }
                if let Ok(Err(error)) = &result {
                    tracing::error!("API server failed: {error}");
                }
                let _ = ended.send(clean);
            }
            () = service.work(&config_path, &mut events) => {}
        }
    };
    service.close_sync();
    service.close_reloads();
    if super::shut_down_by(
        &service,
        &stop_server,
        server,
        &control,
        deadline,
        shutdown_reserve,
    )
    .await
    {
        failed.store(true, Ordering::SeqCst);
    }
    // What is left of the budget has been spent: nothing stays open.
    let cut = control.cut();
    if cut > 0 {
        tracing::warn!("cut {cut} connection(s) left at the stop");
    }
}
