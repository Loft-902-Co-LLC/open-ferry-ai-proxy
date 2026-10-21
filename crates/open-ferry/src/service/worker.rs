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
//! handled by `switch` and the handlers below, and `peers_gone` is taken
//! in an active or watching worker and ignored elsewhere (L9.15).
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
//! holding the lease until `stop` (L9.16). `close_sync` refuses pending
//! credential work without starting filesystem reads. A failed
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

use open_ferry_core::config::{Config, WatchEvent};
use open_ferry_core::gate::{ActivationKey, ActiveGate, GateState};
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

pub(crate) mod hooks;
mod switch;
mod topology;
use hooks::{Barrier, Event as Seen, Fault, Hooks, Place, Point};
use switch::{
    Command, HandOver, Handles, LeaseSlot, Resume, Stage, TakeOver, Taken, cleaning_is_off,
    lease_held, lease_slot, turn_cleaning_off,
};
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
    /// The id of the switch this worker is part of, which its log names.
    pub(crate) switch_id: Option<String>,
}

impl Welcome {
    /// The fields of the `welcome` that the supervisor crate's
    /// `Pending::attach` returned.
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
    /// The test seams of the worker. `None` builds them from the data
    /// directory of the `welcome` (nothing at all without the feature).
    pub(super) hooks: Option<Hooks>,
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
    /// Notified when a stop has taken a take over in progress into its own
    /// hands, and is about to wait for the take over's task.
    #[cfg(test)]
    pub(super) stop_took_the_take_over: Option<Arc<tokio::sync::Notify>>,
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
            // A test's worker has no seams unless the test gives it some.
            hooks: if cfg!(test) {
                Some(Hooks::default())
            } else {
                None
            },
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
            stop_took_the_take_over: None,
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
    #[cfg(feature = "blue-green-test")]
    crate::seams::start_safety_net();
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
    let mut timing = timing;
    timing.activation_stop_budget = Duration::from_millis(budgets.stop_ms);
    // The seams, and every file the switch touches, are in the data
    // directory the supervisor named: never the user's.
    let data_dir = welcome.installation.data_dir.clone();
    let hooks = timing
        .hooks
        .clone()
        .unwrap_or_else(|| Hooks::in_data_dir(&data_dir));
    hooks.exit_after(Point::Welcome);
    if let Some(switch) = &welcome.switch_id {
        tracing::info!("part of switch {switch}");
    }
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
        config_path.clone(),
        log_level,
        options,
        welcome,
        listeners,
        timing.clone(),
        control.clone(),
        hooks.clone(),
    ));
    let mut worker = Worker {
        channel: Some(channel),
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
        hooks,
        data_dir,
        config_path,
        timing,
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
    Standby(Box<Standby>),
    /// Told `take_over`, and holding the lease: the task that takes over is
    /// running, or has failed.
    Taking(Box<Taking>),
    /// Serving: active, ready or watching, by the stage.
    Active(Box<Active>),
    /// Told `hand_over`, and handing over, handed over or waiting for a
    /// `resume` after a failure.
    Handing(Box<Handing>),
    /// Released: no new connections, and the existing ones finishing.
    Draining(Box<Draining>),
}

impl State {
    fn phase(&self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Standby(_) => "standby",
            Self::Taking(_) => "taking-over",
            Self::Active(active) => active.stage.phase(),
            Self::Handing(handing) if handing.handed => "handed-over",
            Self::Handing(_) => "handing-over",
            Self::Draining(_) => "draining",
        }
    }
}

/// A loaded standby: everything a take over needs.
type Standby = Prepared;

/// A take over in progress.
struct Taking {
    handles: Handles,
    lease: LeaseSlot,
    /// Tells the task to stop at its next step before the last.
    cancel: watch::Sender<bool>,
    /// `None` once it has failed: the worker holds the lease and answers
    /// only `ping`, `status` and `stop`.
    task: Option<JoinHandle<Taken>>,
    /// A `hand_over` came while the task ran: the task was told to cancel,
    /// and its end decides what the worker does (L9.16).
    hand_over_pending: bool,
}

/// A hand over in progress, done, or failed and waiting for `resume`.
struct Handing {
    /// The serving worker. `None` for one that never served: a take over
    /// that a hand over cancelled before its last step.
    active: Option<Active>,
    handles: Handles,
    lease: LeaseSlot,
    /// Whether this worker began in `active` or `watching`: only it may
    /// resume.
    resumable: bool,
    /// Whether the gate was open when the hand over began.
    was_open: bool,
    /// Whether `handed_over` is sent.
    handed: bool,
    /// The first step, cleaning off, done when it holds `true`.
    cleaned: watch::Receiver<bool>,
    /// Tells the continuation that a resume is waiting for it.
    cancel: watch::Sender<bool>,
    /// The rest of the hand over. A resume takes it and joins it.
    hand: Option<JoinHandle<Result<String, String>>>,
    /// A resume, which retakes the lease when it was given up.
    resume: Option<JoinHandle<Result<(), String>>>,
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
    stage: Stage,
    handles: Handles,
    /// Asks the service loop for what only it can do.
    commands: mpsc::UnboundedSender<Command>,
    /// Held until the process exits, or a hand over gives it up.
    lease: LeaseSlot,
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
    /// A take over's task ended.
    Taken(Result<Taken, tokio::task::JoinError>),
    /// A hand over's continuation ended.
    HandOverDone(Result<Result<String, String>, tokio::task::JoinError>),
    /// A resume ended.
    ResumeDone(Result<Result<(), String>, tokio::task::JoinError>),
}

struct Worker<R, W> {
    /// `None` once the `drop-channel` fault has dropped it.
    channel: Option<Channel<R, W>>,
    channel_lost: bool,
    stop_deadline: Option<tokio::time::Instant>,
    failed: Arc<AtomicBool>,
    send_timeout: Duration,
    budgets: Budgets,
    control: ServeControl,
    started_at: Instant,
    mode: String,
    /// Whether the supervisor has said no other worker is alive while Active
    /// (active or watching). A hand over resets it with log cleaning, which
    /// it turns off, so the first `peers_gone` after a resume turns it on.
    peers_gone: bool,
    state: State,
    hooks: Hooks,
    /// The supervisor's data directory, from `welcome`.
    data_dir: PathBuf,
    config_path: PathBuf,
    timing: Timing,
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
                Event::Taken(result) => self.on_taken(result).await,
                Event::HandOverDone(result) => self.on_hand_over_done(result).await,
                Event::ResumeDone(result) => self.on_resume_done(result).await,
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
        let receiving = async {
            match channel {
                Some(channel) => channel.receive::<Message>().await,
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
                State::Taking(taking) => match &mut taking.task {
                    Some(task) => Event::Taken(task.await),
                    None => pending().await,
                },
                State::Handing(handing) => {
                    let Handing {
                        active,
                        hand,
                        resume,
                        ..
                    } = &mut **handing;
                    tokio::select! {
                        result = async {
                            match hand {
                                Some(hand) => hand.await,
                                None => pending().await,
                            }
                        } => Event::HandOverDone(result),
                        result = async {
                            match resume {
                                Some(resume) => resume.await,
                                None => pending().await,
                            }
                        } => Event::ResumeDone(result),
                        clean = async {
                            match active {
                                Some(active) => active.ended.recv().await,
                                None => pending().await,
                            }
                        } => Event::ServerEnded(clean.unwrap_or(false)),
                    }
                }
                _ => pending().await,
            }
        };
        tokio::select! {
            frame = receiving => Event::Frame(frame),
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
            Message::HandOver { .. } => self.on_hand_over().await,
            Message::Resume => self.on_resume().await,
            Message::TakeOver => self.on_take_over().await,
            Message::Go => self.on_go().await,
            // No reply.
            Message::PeersGone => self.on_peers_gone().await,
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

    /// Logs that `message` makes no sense now and does nothing.
    fn ignore(&self, message: &str) -> Flow {
        tracing::debug!(
            "ignoring {message}, which makes no sense while {}",
            self.state.phase()
        );
        Flow::Continue
    }

    /// Drops the channel and stops accepting, keeping the lease and staying
    /// alive until killed: the `drop-channel` fault.
    async fn drop_channel(&mut self) {
        drop(self.channel.take());
        self.control.pause_accept().await;
    }

    /// L9.15: the first `peers_gone` since log cleaning was last turned off
    /// turns it on. Only an active or watching worker counts it.
    async fn on_peers_gone(&mut self) -> Flow {
        let State::Active(active) = &self.state else {
            return self.ignore("peers_gone");
        };
        if active.stage == Stage::Ready {
            return self.ignore("peers_gone");
        }
        if std::mem::replace(&mut self.peers_gone, true) {
            tracing::debug!("ignoring a repeated peers_gone");
            return Flow::Continue;
        }
        let (handles, lease) = (active.handles.clone(), lease_held(&active.lease));
        // Logged exactly when this call turns cleaning on.
        if !handles.set_cleaning(true).await {
            tracing::info!("no other worker is running");
            self.hooks.event(Seen::PeersGone, lease);
        }
        Flow::Continue
    }

    /// L9.1: take the lease with one try, then take over on a task of its
    /// own, so the channel keeps being served meanwhile.
    async fn on_take_over(&mut self) -> Flow {
        if !matches!(self.state, State::Standby(_)) {
            return self.ignore("take_over");
        }
        let State::Standby(standby) = std::mem::replace(&mut self.state, State::Loading) else {
            return Flow::Continue;
        };
        self.hooks.clear_arrivals(&[Barrier::BeforeReload]);
        self.send_status_as("taking-over").await;
        let dir = self.data_dir.clone();
        // The owner is made in the blocking task, before a cancellation can
        // discard its result.
        let tried = tokio::task::spawn_blocking(move || {
            Lease::try_take(&dir).map(|lease| lease.map(|lease| ProcessLease(Some(lease))))
        })
        .await;
        let lease = match tried {
            Ok(Ok(Some(lease))) => lease,
            Ok(Ok(None)) => {
                return self
                    .take_refused(standby, "another worker holds the active lease".to_owned())
                    .await;
            }
            Ok(Err(error)) => return self.take_refused(standby, error.to_string()).await,
            Err(error) => {
                return self
                    .take_refused(standby, format!("the task failed: {error}"))
                    .await;
            }
        };
        let handles = standby.handles.clone();
        let lease = lease_slot(Some(lease));
        let (cancel, cancelled) = watch::channel(false);
        let task = tokio::spawn(switch::take_over(TakeOver {
            standby: *standby,
            lease: lease.clone(),
            cancel: cancelled,
            config_path: self.config_path.clone(),
            timing: self.timing.clone(),
        }));
        self.state = State::Taking(Box::new(Taking {
            handles,
            lease,
            cancel,
            task: Some(task),
            hand_over_pending: false,
        }));
        Flow::Continue
    }

    /// A take over that couldn't take the lease: nothing changed, and the
    /// worker is a standby again.
    async fn take_refused(&mut self, standby: Box<Standby>, why: String) -> Flow {
        self.state = State::Standby(standby);
        self.send(&Message::TakeFailed {
            reason: format!("lease: {why}"),
        })
        .await;
        self.send_status().await;
        Flow::Continue
    }

    /// A take over's task ended.
    async fn on_taken(&mut self, result: Result<Taken, tokio::task::JoinError>) -> Flow {
        let State::Taking(taking) = &mut self.state else {
            return Flow::Continue;
        };
        taking.task = None;
        let pending = taking.hand_over_pending;
        let (handles, lease) = (taking.handles.clone(), taking.lease.clone());
        let taken = result
            .unwrap_or_else(|error| Taken::Failed(format!("the take over task failed: {error}")));
        match taken {
            // A hand over came meanwhile: one that failed, or was dropped,
            // is handled as ever and never hands over; one that stopped
            // before its last step, or finished it, does (L9.16). A worker
            // that serves hands over as it is, and says no `ready`.
            Taken::Ready(active) if pending => {
                return self
                    .begin_hand_over(handles, Some(*active), Some(lease), false)
                    .await;
            }
            Taken::Cancelled if pending => {
                return self
                    .begin_hand_over(handles, None, Some(lease), false)
                    .await;
            }
            Taken::Ready(active) => {
                self.state = State::Active(active);
                self.send(&Message::Ready).await;
                self.hooks.event(Seen::Ready, true);
                self.hooks.exit_after(Point::Ready);
                self.send_status().await;
            }
            Taken::Dropped => self.drop_channel().await,
            Taken::Failed(reason) => self.take_failed(reason).await,
            Taken::Cancelled => {
                self.take_failed("the take over was cancelled".to_owned())
                    .await;
            }
        }
        Flow::Continue
    }

    /// A take over failed after the lease: the worker keeps it, answers only
    /// `ping`, `status` and `stop`, and exits with failure when stopped.
    async fn take_failed(&mut self, reason: String) {
        tracing::error!("the take over failed: {reason}");
        self.failed.store(true, Ordering::SeqCst);
        self.send(&Message::TakeFailed { reason }).await;
    }

    /// L9.2: a worker that took over starts its background work.
    async fn on_go(&mut self) -> Flow {
        let State::Active(active) = &mut self.state else {
            return self.ignore("go");
        };
        if active.stage != Stage::Ready {
            return self.ignore("go");
        }
        active.stage = Stage::Watching;
        let handles = active.handles.clone();
        handles.start_background().await;
        self.hooks.event(Seen::Go, true);
        self.hooks.exit_after(Point::Go);
        if self.hooks.drops_channel_at(Place::Watching) {
            self.drop_channel().await;
            return Flow::Continue;
        }
        self.send_status().await;
        Flow::Continue
    }

    /// L9.3: from active, watching, or a take over that hasn't failed. The
    /// first step, cleaning off, can't fail; the rest, from the management
    /// 503 on, runs on a task of its own. A take over is told to cancel, and
    /// the hand over begins when its task ends, if it ends well.
    async fn on_hand_over(&mut self) -> Flow {
        if self.hooks.fault(Fault::IgnoreHandover).is_some() {
            tracing::debug!("ignoring hand_over, as the test fault says");
            return Flow::Continue;
        }
        match &mut self.state {
            State::Active(active) => {
                let (handles, resumable) = (active.handles.clone(), active.stage != Stage::Ready);
                let State::Active(active) = std::mem::replace(&mut self.state, State::Loading)
                else {
                    return Flow::Continue;
                };
                self.begin_hand_over(handles, Some(*active), None, resumable)
                    .await
            }
            // Cancel the take over and go on: its end, which the loop takes
            // like any other, says what happens (L9.16). A task that has
            // ended in failure is gone, and a second hand over is no news.
            State::Taking(taking) if taking.task.is_some() && !taking.hand_over_pending => {
                taking.hand_over_pending = true;
                let _ = taking.cancel.send(true);
                Flow::Continue
            }
            _ => self.ignore("hand_over"),
        }
    }

    /// The hand over's start, for a worker that serves (`active`) and for one
    /// whose take over a hand over cancelled: cleaning off, and the rest on a
    /// task of its own. The management 503 follows cleaning, in that task.
    async fn begin_hand_over(
        &mut self,
        handles: Handles,
        active: Option<Active>,
        lease: Option<LeaseSlot>,
        resumable: bool,
    ) -> Flow {
        let lease = lease
            .or_else(|| active.as_ref().map(|active| active.lease.clone()))
            .unwrap_or_else(|| lease_slot(None));
        self.hooks.clear_arrivals(&[
            Barrier::GateClosing,
            Barrier::RefreshBeforeSave,
            Barrier::GateClosed,
            Barrier::LeaseRelease,
        ]);
        self.peers_gone = false;
        let cleaned = turn_cleaning_off(&handles);
        let was_open = handles.gate.state() == GateState::Open;
        let (cancel, cancelled) = watch::channel(false);
        let mut handing = Handing {
            active,
            handles: handles.clone(),
            lease,
            resumable,
            was_open,
            handed: false,
            cleaned: cleaned.clone(),
            cancel,
            hand: None,
            resume: None,
        };
        if self.hooks.drops_channel_at(Place::HandingOver) {
            // The fault stands in for the continuation, which this worker
            // never runs: the 503 still follows cleaning.
            let mut cleaned = cleaned;
            cleaning_is_off(&mut cleaned).await;
            handles.management.set_updating(true);
            self.state = State::Handing(Box::new(handing));
            self.drop_channel().await;
            return Flow::Continue;
        }
        handing.hand = Some(tokio::spawn(switch::hand_over(HandOver {
            handles,
            cleaned,
            cancel: cancelled,
        })));
        self.state = State::Handing(Box::new(handing));
        self.send_status().await;
        Flow::Continue
    }

    /// The hand over's continuation ended: `handed_over`, or a failure that
    /// gives the gate and accept back where they were (L9.4, L9.16) and
    /// waits for `resume`.
    async fn on_hand_over_done(
        &mut self,
        result: Result<Result<String, String>, tokio::task::JoinError>,
    ) -> Flow {
        let State::Handing(handing) = &mut self.state else {
            return Flow::Continue;
        };
        handing.hand = None;
        let handles = handing.handles.clone();
        let (was_open, resumable) = (handing.was_open, handing.resumable);
        let lease = lease_held(&handing.lease);
        let reason = match result {
            Ok(Ok(id)) => {
                handing.handed = true;
                // The lease goes here and nowhere else: a driver that took
                // `stop` or EOF never gets here, and keeps it until it exits.
                switch::release_lease(&handing.lease);
                let live = u64::try_from(self.control.live_count()).unwrap_or(u64::MAX);
                self.send(&Message::HandedOver { state_id: id, live }).await;
                self.hooks.event(Seen::HandedOver, false);
                self.hooks.exit_after(Point::HandedOver);
                self.send_status().await;
                return Flow::Continue;
            }
            Ok(Err(reason)) => reason,
            Err(error) => format!("the hand over task failed: {error}"),
        };
        tracing::error!("the hand over failed: {reason}; waiting for resume");
        self.hooks.event(Seen::HandoverFailed, lease);
        if was_open {
            handles.gate.reopen();
        }
        // A worker that began active serves while it waits. One that took
        // over never resumes accept: it was never resumable.
        if resumable && let Err(error) = handles.control.resume_accept().await {
            tracing::warn!("couldn't resume accepting after the failed hand over: {error}");
        }
        Flow::Continue
    }

    /// L9.5, both kinds. With no hand over in progress, an active or
    /// watching worker answers `resumed` and changes nothing.
    async fn on_resume(&mut self) -> Flow {
        match &self.state {
            State::Active(active) if active.stage != Stage::Ready => {
                let lease = lease_held(&active.lease);
                self.send(&Message::Resumed).await;
                self.hooks.event(Seen::Resumed, lease);
                return Flow::Continue;
            }
            State::Handing(handing) if handing.resumable && handing.resume.is_none() => {}
            _ => return self.ignore("resume"),
        }
        let State::Handing(handing) = &mut self.state else {
            return Flow::Continue;
        };
        let Some(active) = &handing.active else {
            return Flow::Continue;
        };
        // Tell the continuation, and wait for nothing here: the resume task
        // joins it, so the loop goes on answering the supervisor. The part
        // that is cancelled is waiting at a barrier or for the gate; a
        // blocking step it has started runs to its end first.
        let _ = handing.cancel.send(true);
        handing.resume = Some(tokio::spawn(switch::resume(Resume {
            handles: handing.handles.clone(),
            lease: handing.lease.clone(),
            commands: active.commands.clone(),
            retake_within: Duration::from_millis(self.budgets.resume_ms),
            timing: self.timing.clone(),
            hand: handing.hand.take(),
            cleaned: handing.cleaned.clone(),
        })));
        Flow::Continue
    }

    /// A resume ended: active again, or a worker that can't go on.
    async fn on_resume_done(
        &mut self,
        result: Result<Result<(), String>, tokio::task::JoinError>,
    ) -> Flow {
        let reason = match result {
            Ok(Ok(())) => {
                let State::Handing(handing) = std::mem::replace(&mut self.state, State::Loading)
                else {
                    return Flow::Continue;
                };
                let Handing {
                    active: Some(mut active),
                    ..
                } = *handing
                else {
                    return Flow::Continue;
                };
                active.stage = Stage::Active;
                let lease = lease_held(&active.lease);
                self.state = State::Active(Box::new(active));
                self.send(&Message::Resumed).await;
                self.hooks.event(Seen::Resumed, lease);
                self.send_status().await;
                return Flow::Continue;
            }
            Ok(Err(reason)) => reason,
            Err(error) => format!("the resume task failed: {error}"),
        };
        if let State::Handing(handing) = &mut self.state {
            handing.resume = None;
        }
        tracing::error!("the resume failed: {reason}; stopping");
        self.failed.store(true, Ordering::SeqCst);
        Flow::Stop(
            Duration::from_millis(self.budgets.stop_ms),
            ExitCode::FAILURE,
        )
    }

    async fn on_release(&mut self) -> Flow {
        match std::mem::replace(&mut self.state, State::Loading) {
            State::Active(active) => {
                self.drain(*active).await;
                Flow::Continue
            }
            State::Handing(handing)
                if handing.handed && handing.hand.is_none() && handing.resume.is_none() =>
            {
                match handing.active {
                    Some(active) => self.drain(active).await,
                    // It never served: nothing is left to finish.
                    None => {
                        self.hooks.event(Seen::Draining, false);
                        self.send(&Message::Drained).await;
                        self.hooks.event(Seen::Drained, false);
                        return Flow::Stop(
                            Duration::from_millis(self.budgets.stop_ms),
                            ExitCode::SUCCESS,
                        );
                    }
                }
                Flow::Continue
            }
            other => {
                tracing::debug!("ignoring release while {}", other.phase());
                self.state = other;
                Flow::Continue
            }
        }
    }

    /// Stops accepting and finishes the connections of `active`.
    async fn drain(&mut self, active: Active) {
        let drained = Box::pin(self.control.release());
        self.hooks.event(Seen::Draining, lease_held(&active.lease));
        self.state = State::Draining(Box::new(Draining { active, drained }));
        self.send_status().await;
    }

    /// The last connection of a released worker ended.
    async fn on_drained(&mut self) -> Flow {
        let lease = match &self.state {
            State::Draining(draining) => lease_held(&draining.active.lease),
            _ => false,
        };
        self.send(&Message::Drained).await;
        self.hooks.event(Seen::Drained, lease);
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
                let standby = matches!(state, Ready::Standby(_));
                match state {
                    Ready::Standby(standby) => self.state = State::Standby(standby),
                    Ready::Active(active) => self.state = State::Active(active),
                }
                // The supervisor counts a start until `loaded`, in either role. It comes
                // before the first active or standby status; a loading one may precede it.
                let ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                if standby
                    && self.hooks.fault(Fault::HoldLoad).is_some()
                    && let Err(error) = self.hooks.barrier(Barrier::LoadBeforeLoaded, false).await
                {
                    self.failed.store(true, Ordering::SeqCst);
                    let reason = format!("a test barrier failed: {error}");
                    tracing::error!("the standby worker failed to load: {reason}");
                    self.send(&Message::LoadFailed { reason }).await;
                    return Flow::Exit(ExitCode::FAILURE);
                }
                self.send(&Message::Loaded { ms }).await;
                if standby {
                    self.hooks.event(Seen::Loaded, false);
                    self.hooks.event(Seen::Standby, false);
                } else {
                    self.hooks.event(Seen::Active, true);
                }
                self.hooks.exit_after(Point::Loaded);
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
            State::Taking(taking) => {
                let Taking { cancel, task, .. } = *taking;
                let _ = cancel.send(true);
                #[cfg(test)]
                if let Some(took) = &self.timing.stop_took_the_take_over {
                    took.notify_one();
                }
                let Some(mut task) = task else {
                    return false;
                };
                return match tokio::time::timeout_at(deadline, &mut task).await {
                    // Past its last step: it serves, and stops as any does.
                    Ok(Ok(Taken::Ready(active))) => active.stop(deadline).await,
                    // The cancel above is not a failure; a take over that
                    // failed on its own, or panicked, is one, and the worker
                    // exits as a `take_failed` one does when stopped.
                    Ok(Ok(Taken::Cancelled | Taken::Dropped)) => false,
                    Ok(Ok(Taken::Failed(reason))) => {
                        tracing::error!("the take over failed: {reason}");
                        true
                    }
                    Ok(Err(error)) => {
                        tracing::error!("the take over task failed: {error}");
                        true
                    }
                    Err(_) => {
                        task.abort();
                        false
                    }
                };
            }
            State::Handing(handing) => {
                let Handing {
                    active,
                    hand,
                    resume,
                    ..
                } = *handing;
                for task in [
                    hand.map(|task| task.abort_handle()),
                    resume.map(|task| task.abort_handle()),
                ]
                .into_iter()
                .flatten()
                {
                    task.abort();
                }
                let Some(active) = active else {
                    return false;
                };
                active
            }
            _ => return false,
        };
        active.stop(deadline).await
    }

    async fn send_status(&mut self) {
        let phase = self.state.phase();
        self.send_status_as(phase).await;
    }

    async fn send_status_as(&mut self, phase: &str) {
        let message = Message::Status {
            mode: self.mode.clone(),
            live: u64::try_from(self.control.live_count()).unwrap_or(u64::MAX),
            // The sign-ins in progress are in the management state, which
            // only the service owns.
            signins_pending: 0,
            phase: phase.to_owned(),
        };
        self.send(&message).await;
    }

    async fn send(&mut self, message: &Message) {
        if self.channel_lost {
            return;
        }
        let Some(channel) = self.channel.as_mut() else {
            return;
        };
        match tokio::time::timeout(self.send_timeout, channel.send(message)).await {
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
/// Only [`release`](Self::release), a hand over's, gives it up before the
/// process exits.
struct ProcessLease(Option<Lease>);

impl ProcessLease {
    /// Gives the lease up now: the lock goes with the handle.
    fn release(mut self) {
        drop(self.0.take());
    }
}

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
    /// What the switch shares with the service, cloned before it moves.
    handles: Handles,
}

/// What `prepare` needs besides the config: where the worker's files are
/// and what it shares.
struct Context {
    data_dir: PathBuf,
    installed: PathBuf,
    hooks: Hooks,
    control: ServeControl,
    /// Update checks of a test, in place of the real ones.
    #[cfg(test)]
    updates: Option<open_ferry_update::UpdateService>,
}

/// The start: the lease first for an active worker, then the config and the
/// service, then, for a standby, the dry build of the routes, or for an
/// active worker the first activation and serving.
#[allow(clippy::too_many_arguments)]
async fn start(
    config_path: PathBuf,
    log_level: LogLevel,
    options: Options,
    welcome: Welcome,
    listeners: Vec<(String, StdListener)>,
    timing: Timing,
    control: ServeControl,
    hooks: Hooks,
) -> Result<Started, Failure> {
    let mut timing = timing;
    timing.activation_stop_budget = Duration::from_millis(welcome.budgets.stop_ms);
    let role = welcome.role.clone();
    let failed = |reason: String| Failure {
        role: role.clone(),
        reason,
    };
    // At any load, active first start or standby (L9.10).
    if let Some(reason) = hooks.failure(Fault::FailLoad) {
        return Err(failed(reason));
    }
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
    if lease.is_some() {
        // The first start reopens the log after the lease (L9.18).
        let file_log = log_level.file_log().clone();
        match tokio::task::spawn_blocking(move || file_log.reopen()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("couldn't reopen the log: {error}"),
            Err(error) => tracing::warn!("the log reopen task failed: {error}"),
        }
    }
    let context = Context {
        data_dir: welcome.installation.data_dir.clone(),
        installed: welcome.installation.installed.clone(),
        hooks,
        control,
        #[cfg(test)]
        updates: timing.updates.clone(),
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
                context,
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
        return Ok(Started {
            mode,
            state: Ready::Standby(Box::new(prepared)),
        });
    };
    let active = activate(
        prepared,
        &config_path,
        lease_slot(Some(lease)),
        &timing,
        Launch::First,
    )
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
    context: Context,
) -> Result<Prepared, String> {
    let probe = crate::scope::runtime_probe();
    let (config, sha256) = crate::settings::load_with_sha256(probe.as_ref(), config_path)
        .map_err(|error| format!("failed to load config: {error}"))?;
    log_level.set_debug(config.debug);
    let auth_dir = config
        .resolve_auth_dir()
        .map_err(|error| format!("failed to resolve auth directory: {error}"))?;
    crate::scope::authorize_auth_dir(probe.as_ref(), &auth_dir)
        .map_err(|error| error.to_string())?;
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
    let mut service = Service::prepared_with_probe(
        Arc::clone(&config),
        config_path.to_path_buf(),
        auth_dir,
        log_level,
        &options,
        gate,
        probe,
    );
    if standby {
        service.hold_cooldown_store();
    }
    // The updater is attached before any router clones the management
    // state, and looks in the supervisor's data directory, never the user's.
    #[cfg_attr(
        not(test),
        allow(unused_mut, reason = "only a test's updater is checked at once")
    )]
    let mut first_check =
        super::update_settings::between(super::FIRST_CHECK.0, super::FIRST_CHECK.1);
    if options.self_update {
        #[cfg(test)]
        if let Some(updates) = context.updates {
            service.management = service.management.clone().with_updates(updates);
            first_check = Duration::ZERO;
        } else {
            service.prepare_update_checks(Some((&context.data_dir, &context.installed)));
        }
        #[cfg(not(test))]
        service.prepare_update_checks(Some((&context.data_dir, &context.installed)));
    }
    let handles = Handles {
        gate: service.gate.clone(),
        manager: service.manager.clone(),
        management: service.management.clone(),
        file_log: service.log_level.file_log().clone(),
        control: context.control,
        hooks: context.hooks,
        data_dir: context.data_dir,
        first_check,
    };
    Ok(Prepared {
        service,
        key,
        topology,
        config,
        tls,
        handles,
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

/// How a worker came to be activated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Launch {
    /// Its first start, active: the background work starts with it.
    First,
    /// A take over (L9.1): it serves at `ready`, and its background work
    /// starts at `go`.
    TakeOver,
}

/// Makes a prepared worker the active one: the first activation, serving
/// the topology, and the service loop's task. A first start also starts the
/// background work; a take over leaves it to `go`.
async fn activate(
    prepared: Prepared,
    config_path: &Path,
    lease: LeaseSlot,
    timing: &Timing,
    launch: Launch,
) -> Result<Active, String> {
    activate_unless_cancelled(prepared, config_path, lease, timing, launch, None)
        .await
        .map_err(|aborted| match aborted {
            Aborted::Failed(reason) => reason,
            Aborted::Cancelled => "the activation was cancelled".to_owned(),
        })
}

/// Why an activation didn't end in an active worker.
enum Aborted {
    Failed(String),
    /// A hand over cancelled the take over before its last step. What was
    /// started is cleaned up as a failure's is, but the worker has not
    /// failed: it keeps the lease and hands over.
    Cancelled,
}

impl From<String> for Aborted {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}

/// [`activate`], which a take over's `cancel` can stop right before the last
/// step, the one that can't be taken back.
async fn activate_unless_cancelled(
    prepared: Prepared,
    config_path: &Path,
    lease: LeaseSlot,
    timing: &Timing,
    launch: Launch,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<Active, Aborted> {
    let Prepared {
        mut service,
        key,
        topology,
        config,
        tls,
        handles,
    } = prepared;
    let own_control = handles.control.clone();
    let control = &own_control;
    let (mut service, activated) = tokio::task::spawn_blocking(move || {
        let activated = service.first_activation(&key);
        (service, activated)
    })
    .await
    .map_err(|error| format!("activation task failed: {error}"))?;
    if let Err(error) = activated {
        timing.failed.store(true, Ordering::SeqCst);
        let (stop, _) = watch::channel(false);
        service.close_management_queues();
        super::shut_down_by(
            &service,
            &stop,
            None,
            control,
            tokio::time::Instant::now(),
            timing.shutdown_reserve,
        )
        .await;
        return Err(format!("failed to activate the service: {error}").into());
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
        &handles.hooks,
        timing.failed.clone(),
        #[cfg(test)]
        timing.hold_server.clone(),
    ) {
        Ok(serving) => serving,
        Err(error) => {
            timing.failed.store(true, Ordering::SeqCst);
            let (stop, _) = watch::channel(false);
            service.close_management_queues();
            super::shut_down_by(
                &service,
                &stop,
                None,
                control,
                tokio::time::Instant::now(),
                timing.shutdown_reserve,
            )
            .await;
            return Err(format!("failed to serve the sockets: {error}").into());
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
    let probe = Arc::clone(&service.probe);
    let file_guard = service.store.file_guard();
    let watcher = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        let watcher_config = {
            let mut config = Config::clone(&watcher_config);
            if let Some(wedge) = wedge {
                wedge(&watcher_control, &mut config);
            }
            Arc::new(config)
        };
        super::start_watcher(&watcher_path, &watcher_config, probe, file_guard)
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
            service.close_management_queues();
            super::shut_down_by(
                &service,
                &stop_server,
                Some(server),
                control,
                tokio::time::Instant::now(),
                timing.shutdown_reserve,
            )
            .await;
            return Err(format!("failed to create watcher: {error}").into());
        }
    };
    tracing::info!("file watcher started for config and auth directory changes");
    // The commit point: a hand over's cancel that came during the steps
    // above stops the take over here, before the gate opens and accept
    // resumes. It cleans up as the watcher's failure does, without the
    // failed flag; the worker keeps the lease.
    if cancel.is_some_and(|cancel| *cancel.borrow()) {
        service.close_management_queues();
        super::shut_down_by(
            &service,
            &stop_server,
            Some(server),
            control,
            tokio::time::Instant::now(),
            timing.shutdown_reserve,
        )
        .await;
        return Err(Aborted::Cancelled);
    }
    service.gate.reopen();
    #[cfg(test)]
    if timing.fail_resume_accept {
        control.cut();
    }
    let resumed = async {
        control.resume_accept().await?;
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
    #[cfg(test)]
    let resumed = async {
        tokio::select! {
            result = resumed => result,
            _ = async {
                let Some(queued) = listener_failure else {
                    pending::<()>().await;
                    return;
                };
                // A server not polled yet is absent from resume_accept's
                // counts. Keep the injection alive through await_listeners,
                // which also waits for those late-starting loops.
                while service.sync_requests.is_empty() {
                    tokio::task::yield_now().await;
                }
                queued.notify_one();
                pending::<()>().await;
            } => unreachable!(),
        }
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
        // Refuse queued credential work without waiting for observation I/O.
        // A failed take over held until stop intentionally exits with failure.
        service.close_management_queues();
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
        return Err(format!("failed to resume accepting: {error}").into());
    }
    claude_cli::warn_outdated_guarded(&config.claude_cli, service.claude_guard.clone());
    if launch == Launch::First {
        handles.start_background().await;
    }
    let (stop, stop_rx) = oneshot::channel();
    let (ended_tx, ended) = mpsc::unbounded_channel();
    let (commands, command_rx) = mpsc::unbounded_channel();
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
        commands: command_rx,
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
        stage: match launch {
            Launch::First => Stage::Active,
            Launch::TakeOver => Stage::Ready,
        },
        handles,
        commands,
        lease,
    })
}

/// Serves `topology`'s sockets on `control`: the main one, and the
/// management one, each with TLS as its flag says. The sender stops the
/// server gracefully.
#[allow(clippy::too_many_arguments)]
fn serve(
    service: &Service,
    topology: Topology,
    tls: Option<Arc<rustls::ServerConfig>>,
    control: &ServeControl,
    extra: axum::Router,
    hooks: &Hooks,
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
                    hooks.mark(service.app_with(extra)),
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
            let (main_app, management_app) = (hooks.mark(main_app), hooks.mark(management_app));
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
    /// What the switch asks of the loop, which owns the service.
    commands: mpsc::UnboundedReceiver<Command>,
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
        commands,
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
    let mut commands = Some(commands);
    let mut server = Some(server);
    tokio::pin!(stop);
    let mut settlement = None;
    let deadline = loop {
        if service.reads.is_empty()
            && let Some(command) = settlement.take()
        {
            match command {
                Command::Reload(done) => {
                    let _ = done.send(());
                }
                Command::ApplyDeferred(done) => {
                    let _ = done.send(Ok(()));
                }
            }
        }
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
            command = async {
                match &mut commands {
                    Some(commands) => commands.recv().await,
                    None => pending().await,
                }
            }, if settlement.is_none() => match command {
                Some(Command::Reload(done)) => {
                    service.reload_from_disk(&config_path, &mut events).await;
                    settlement = Some(Command::Reload(done));
                }
                Some(Command::ApplyDeferred(done)) => {
                    match service.apply_deferred(&config_path, &mut events).await {
                        Ok(()) => settlement = Some(Command::ApplyDeferred(done)),
                        Err(error) => { let _ = done.send(Err(error)); }
                    }
                }
                // Every sender is gone: nothing will ask again.
                None => commands = None,
            },
            () = service.work(&config_path, &mut events) => {}
        }
    };
    drop(settlement);
    drop(commands);
    service.close_management_queues();
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

#[cfg(test)]
pub(super) fn settlement_test_loop(
    service: Service,
    relist: bool,
) -> (
    oneshot::Sender<tokio::time::Instant>,
    oneshot::Receiver<Result<(), String>>,
    tokio::task::JoinHandle<()>,
) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (done, settled) = oneshot::channel();
    if relist {
        let _ = commands.send(Command::ApplyDeferred(done));
    } else {
        let (reload_done, reloaded) = oneshot::channel();
        let _ = commands.send(Command::Reload(reload_done));
        tokio::spawn(async move {
            // Dropping this sender on stop preserves the actual command error.
            if reloaded.await.is_ok() {
                let _ = done.send(Ok(()));
            }
        });
    }
    let (stop, stop_rx) = oneshot::channel();
    let (stop_server, _) = watch::channel(false);
    let (ended, _) = mpsc::unbounded_channel();
    let serving = Serving {
        service,
        failed: Arc::new(AtomicBool::new(false)),
        config_path: PathBuf::new(),
        events: None,
        server: tokio::spawn(pending()),
        stop_server,
        control: ServeControl::new(),
        ended,
        commands: command_rx,
        shutdown_reserve: Duration::ZERO,
        wedge: None,
        server_failure_seen: None,
    };
    let task = tokio::spawn(service_loop(serving, stop_rx));
    (stop, settled, task)
}
