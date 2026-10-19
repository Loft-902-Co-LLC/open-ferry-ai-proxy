//! open-ferry's own: the steps of a blue-green switch that run off the
//! worker's message loop (L9.1 to L9.6): the hand over's continuation, the
//! take over's task and both resumes. The worker's state machine in the
//! parent module starts them, takes their results, and sends the messages.
//!
//! None of these steps owns the service: the task that runs the service loop
//! does. They act through [`Handles`] (clones of what the service shares) and
//! ask the loop, over a [`Command`], for what only it can do.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use open_ferry_core::gate::{ActiveGate, GateState};
use open_ferry_core::manager::Manager;
use open_ferry_core::manager::cooldown_store::{self, Thaw};
use open_ferry_management::{Handover, ManagementState};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::hooks::{Barrier, Event, Fault, Hooks, Place};
use super::{Active, Lease, Prepared, ProcessLease, Timing};
use crate::file_log::FileLog;
use crate::service::serve_loop::ServeControl;

/// The file the hand over leaves for the worker that takes over.
pub(super) const HANDOVER_FILE: &str = "handover.json";

/// The lease on the active role, which a hand over takes out and a resume
/// puts back. Empty while another worker may hold it.
pub(super) type LeaseSlot = Arc<Mutex<Option<ProcessLease>>>;

pub(super) fn lease_slot(lease: Option<ProcessLease>) -> LeaseSlot {
    Arc::new(Mutex::new(lease))
}

fn locked(slot: &LeaseSlot) -> MutexGuard<'_, Option<ProcessLease>> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether the lease is held.
pub(super) fn lease_held(slot: &LeaseSlot) -> bool {
    locked(slot).is_some()
}

/// What only the service loop can do.
pub(super) enum Command {
    /// Read the config and the credentials again, and say when done.
    Reload(oneshot::Sender<()>),
    /// Apply the config that waited for the gate to open, read the
    /// credentials of its auth directory, and say when done: `Err` when the
    /// directory the config names isn't the one in use.
    ApplyDeferred(oneshot::Sender<Result<(), String>>),
}

/// How far a serving worker is in a switch, which is what its phase says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    /// Serving since its first start, or since a resume.
    Active,
    /// Took over and serves, and has not yet been told `go`.
    Ready,
    /// Took over, was told `go`.
    Watching,
}

impl Stage {
    pub(super) fn phase(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Ready => "ready",
            Self::Watching => "watching",
        }
    }
}

/// What the service shares, cloned out of it before the loop's task takes
/// it: the switch acts on a worker through these, never through the service.
#[derive(Clone)]
pub(super) struct Handles {
    pub(super) gate: ActiveGate,
    pub(super) manager: Manager,
    pub(super) management: ManagementState,
    pub(super) file_log: FileLog,
    pub(super) control: ServeControl,
    pub(super) hooks: Hooks,
    /// The supervisor's data directory, from `welcome`.
    pub(super) data_dir: PathBuf,
    /// How long after the background work starts the first update check is.
    pub(super) first_check: Duration,
}

impl Handles {
    /// Update checks, the credentials' refresh timer and log rotation: the
    /// background work an active worker starts at its activation, and a
    /// worker that took over at `go` (L9.2). Never log cleaning: that waits
    /// for `peers_gone`.
    pub(super) async fn start_background(&self) {
        if let Some(updates) = self.management.updates() {
            updates.start(self.first_check);
        }
        if let Err(error) = self
            .manager
            .start_auto_refresh(crate::service::AUTO_REFRESH_INTERVAL)
        {
            tracing::warn!("failed to start core auth auto-refresh: {error}");
        } else {
            tracing::info!("core auth auto-refresh started (interval=15m0s)");
        }
        let controls = self.file_log.controls().clone();
        let _ = tokio::task::spawn_blocking(move || controls.set_rotation(true)).await;
    }

    /// Stops what [`start_background`](Self::start_background) starts.
    async fn stop_background(&self) {
        if let Some(updates) = self.management.updates() {
            updates.stop_and_wait().await;
        }
        let manager = self.manager.clone();
        let controls = self.file_log.controls().clone();
        #[cfg(test)]
        let held = self.hooks.clone();
        let _ = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            held.hold_step("stop_background");
            manager.stop_auto_refresh();
            controls.set_rotation(false);
        })
        .await;
    }

    /// Reopens the log by its name, off the runtime's threads (L9.18). A log
    /// that can't be reopened is logged, and the worker goes on.
    pub(super) async fn reopen_logs(&self) {
        let file_log = self.file_log.clone();
        match tokio::task::spawn_blocking(move || file_log.reopen()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("couldn't reopen the log: {error}"),
            Err(error) => tracing::warn!("the log reopen task failed: {error}"),
        }
    }

    /// Turns log cleaning on, off the runtime's threads. True when it was
    /// already on.
    pub(super) async fn set_cleaning(&self, on: bool) -> bool {
        let controls = self.file_log.controls().clone();
        tokio::task::spawn_blocking(move || controls.set_cleaning(on))
            .await
            .unwrap_or(false)
    }
}

/// The first step of a hand over: log cleaning off, so every hand over a
/// worker later resumes from has turned it off (L9.3). It can't fail and it
/// is not part of the continuation a resume aborts: it runs to the end, and
/// whoever needs it done waits on the receiver, which holds `true` then.
pub(super) fn turn_cleaning_off(handles: &Handles) -> watch::Receiver<bool> {
    let (done, wait) = watch::channel(false);
    let controls = handles.file_log.controls().clone();
    drop(tokio::task::spawn_blocking(move || {
        controls.set_cleaning(false);
        let _ = done.send(true);
    }));
    wait
}

/// Waits for [`turn_cleaning_off`]'s step to be done.
pub(super) async fn cleaning_is_off(wait: &mut watch::Receiver<bool>) {
    // A closed sender means the step ended, one way or the other.
    let _ = wait.wait_for(|done| *done).await;
}

/// Gives the lease up: taking it out and dropping it is one step. The worker's
/// driver does this when the continuation ends well, never the continuation:
/// a driver that took `stop` or EOF doesn't process that end, and a stopping
/// worker keeps its lease until it exits (L9.17).
pub(super) fn release_lease(slot: &LeaseSlot) {
    if let Some(held) = locked(slot).take() {
        held.release();
    }
}

/// What the hand over's continuation needs.
pub(super) struct HandOver {
    pub(super) handles: Handles,
    pub(super) cleaned: watch::Receiver<bool>,
    /// Set by a resume. Only the waits that observe are cut short by it; a
    /// mutating step that has started runs to its end, and one that hasn't
    /// started doesn't.
    pub(super) cancel: watch::Receiver<bool>,
}

const CANCELLED: &str = "the hand over was cancelled by a resume";

/// Fails when a resume has cancelled the hand over: called before each step
/// that changes something.
fn not_cancelled(cancel: &watch::Receiver<bool>) -> Result<(), String> {
    if *cancel.borrow() {
        return Err(CANCELLED.to_owned());
    }
    Ok(())
}

/// Waits for `wait`, which only observes, unless a resume cancels first.
async fn observe<T>(
    cancel: &mut watch::Receiver<bool>,
    wait: impl Future<Output = T>,
) -> Result<T, String> {
    tokio::select! {
        out = wait => Ok(out),
        () = cancelled(cancel) => Err(CANCELLED.to_owned()),
    }
}

/// L9.3 from the second step on: the management 503, close the gate, write
/// `handover.json`, freeze the cooldown, stop the background work, pause
/// accept, and wait at the lease barrier. The first step, cleaning off, is the
/// worker's; the lease is given up by the worker on the `Ok`, which is the id
/// of the state written.
///
/// It stops at the first step that fails (L9.4) and leaves the worker as that
/// step found it: the worker restores the gate and accept.
pub(super) async fn hand_over(job: HandOver) -> Result<String, String> {
    let HandOver {
        handles,
        mut cleaned,
        mut cancel,
    } = job;
    let hooks = &handles.hooks;
    cleaning_is_off(&mut cleaned).await;
    // Cleaning is off, and now the management API answers 503 (L9.3).
    handles.management.set_updating(true);
    not_cancelled(&cancel)?;
    // By what the gate is, not by what it was: a take over that failed can
    // leave it closing, and no lease may go while it is.
    let epoch = handles.gate.epoch();
    match handles.gate.state() {
        GateState::Open => {
            handles
                .gate
                .quiesce(epoch)
                .map_err(|error| format!("the gate didn't start closing: {error}"))?;
            observe(&mut cancel, hooks.barrier(Barrier::GateClosing, true))
                .await?
                .map_err(|error| format!("a test barrier failed: {error}"))?;
            if hooks.fault(Fault::HoldRefresh).is_some() {
                observe(&mut cancel, hooks.barrier(Barrier::RefreshBeforeSave, true))
                    .await?
                    .map_err(|error| format!("a test barrier failed: {error}"))?;
            }
            observe(&mut cancel, handles.gate.settle(epoch))
                .await?
                .map_err(|error| format!("the gate didn't close: {error}"))?;
            observe(&mut cancel, hooks.barrier(Barrier::GateClosed, true))
                .await?
                .map_err(|error| format!("a test barrier failed: {error}"))?;
        }
        GateState::Quiescing => {
            observe(&mut cancel, handles.gate.settle(epoch))
                .await?
                .map_err(|error| format!("the gate didn't close: {error}"))?;
        }
        GateState::Closed => {}
    }
    not_cancelled(&cancel)?;
    let id = new_state_id();
    let handover = handles.management.freeze_attempts(id.clone());
    let dir = handles.data_dir.clone();
    #[cfg(test)]
    let held = hooks.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        held.hold_step("write_handover");
        write_handover(&dir, &handover)
    })
    .await
    .map_err(|error| format!("the handover task failed: {error}"))?
    .map_err(|error| format!("couldn't write {HANDOVER_FILE}: {error}"))?;
    hooks.event(Event::HandoverWritten, true);
    not_cancelled(&cancel)?;
    let manager = handles.manager.clone();
    #[cfg(test)]
    let held = hooks.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        held.hold_step("freeze");
        cooldown_store::freeze(&manager)
    })
    .await
    .map_err(|error| format!("the cooldown task failed: {error}"))?
    .map_err(|error| error.to_string())?;
    not_cancelled(&cancel)?;
    handles.stop_background().await;
    not_cancelled(&cancel)?;
    handles.control.pause_accept().await;
    observe(&mut cancel, hooks.barrier(Barrier::LeaseRelease, true))
        .await?
        .map_err(|error| format!("a test barrier failed: {error}"))?;
    Ok(id)
}

/// A fresh id for a hand over: 32 hexadecimal digits.
fn new_state_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// Writes `handover.json` in `data_dir` atomically: the whole file under
/// another name, then a rename (L9.13). Only its owner may read it, where
/// the system has owners.
pub(super) fn write_handover(data_dir: &Path, handover: &Handover) -> io::Result<()> {
    let temp = data_dir.join(format!("handover.{}.tmp", handover.id));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(&handover.to_json())?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temp, data_dir.join(HANDOVER_FILE)));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// Reads `handover.json` and deletes it (L9.1). A file that is missing,
/// doesn't parse or has no valid id is an error, and stays where it is: the
/// supervisor deletes it after a failed switch.
pub(super) fn read_handover(data_dir: &Path) -> Result<Handover, String> {
    let path = data_dir.join(HANDOVER_FILE);
    let data = std::fs::read(&path).map_err(|error| format!("handover: {error}"))?;
    let handover = Handover::from_json(&data).map_err(|error| format!("handover: {error}"))?;
    if handover.id.len() != 32 || !handover.id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("handover: the state has no valid id".to_owned());
    }
    std::fs::remove_file(&path).map_err(|error| format!("handover: {error}"))?;
    Ok(handover)
}

/// What a resume needs.
pub(super) struct Resume {
    pub(super) handles: Handles,
    pub(super) lease: LeaseSlot,
    pub(super) commands: mpsc::UnboundedSender<Command>,
    /// `budgets.resume_ms`: how long to try for the lease.
    pub(super) retake_within: Duration,
    pub(super) timing: Timing,
    /// The hand over's continuation, already told to cancel. `None` when it
    /// ended, or never ran.
    pub(super) hand: Option<JoinHandle<Result<String, String>>>,
    /// The hand over's first step, cleaning off.
    pub(super) cleaned: watch::Receiver<bool>,
}

/// L9.5, both kinds, as one task so the worker answers the supervisor while
/// it waits. First the hand over's continuation is joined, never aborted: a
/// blocking step it has started can't be stopped, and a resume must not run
/// beside it; then cleaning is off. With the lease still held: reopen the
/// log, thaw the cooldown without restoring, and no reload (T1). After
/// `handed_over`: retake the lease first, reload, and thaw with a restore.
/// Then the background work, the gate, accept, and last of all the
/// management API's 503 ends, so a probe that sees `resumed` finds it
/// answering.
pub(super) async fn resume(job: Resume) -> Result<(), String> {
    let Resume {
        handles,
        lease,
        commands,
        retake_within,
        timing,
        hand,
        mut cleaned,
    } = job;
    // No limit of its own: the supervisor's resume timeout and its kill by
    // handle bound a stalled filesystem, and exiting ends the writer.
    if let Some(hand) = hand {
        let _ = hand.await;
    }
    cleaning_is_off(&mut cleaned).await;
    // After the join: a continuation that got as far as its end left the
    // lease where it was, and one that was cancelled never touched it.
    let kept = lease_held(&lease);
    if !kept {
        let taken = Lease::take(&handles.data_dir, retake_within, &timing).await?;
        *locked(&lease) = Some(taken);
    }
    handles.reopen_logs().await;
    if !kept {
        let (done, reloaded) = oneshot::channel();
        commands
            .send(Command::Reload(done))
            .map_err(|_| "the service loop has ended".to_owned())?;
        reloaded
            .await
            .map_err(|_| "the service loop ended before the reload".to_owned())?;
    }
    if handles.gate.state() != GateState::Open {
        handles.gate.reopen();
    }
    if !kept {
        // The reload ran with the gate closed, and a config that changes the
        // auth directory waits for it to open. `resumed` means the whole
        // config is applied and its credentials are loaded.
        let (done, applied) = oneshot::channel();
        commands
            .send(Command::ApplyDeferred(done))
            .map_err(|_| "the service loop has ended".to_owned())?;
        applied
            .await
            .map_err(|_| "the service loop ended before the config was applied".to_owned())?
            .map_err(|reason| format!("the config wasn't applied: {reason}"))?;
    }
    let manager = handles.manager.clone();
    let kind = if kept { Thaw::Keep } else { Thaw::Restore };
    tokio::task::spawn_blocking(move || cooldown_store::thaw(&manager, kind))
        .await
        .map_err(|error| format!("the cooldown task failed: {error}"))?;
    handles.management.unfreeze_attempts();
    handles.start_background().await;
    handles
        .control
        .resume_accept()
        .await
        .map_err(|error| format!("couldn't resume accepting: {error}"))?;
    handles.management.set_updating(false);
    Ok(())
}

/// How a take over's task ended.
pub(super) enum Taken {
    /// Serving: the gate is open and accept resumed.
    Ready(Box<Active>),
    /// It failed, and what it started is cleaned up. The lease is kept.
    Failed(String),
    /// A hand over cancelled it before it opened the gate.
    Cancelled,
    /// The `drop-channel` fault: the worker drops its channel and stays.
    Dropped,
}

/// What a take over's task needs.
pub(super) struct TakeOver {
    pub(super) standby: Prepared,
    pub(super) lease: LeaseSlot,
    pub(super) cancel: watch::Receiver<bool>,
    pub(super) config_path: PathBuf,
    pub(super) timing: Timing,
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    // A dropped sender is a cancel too: nobody waits for this task.
    let _ = cancel.wait_for(|cancelled| *cancelled).await;
}

/// L9.1 from its second step on (the lease is the worker's first): the
/// barrier, the reload, `handover.json`, the log's reopen, then the routers,
/// the listeners and, in one final step, the gate, accept and (by the
/// worker) `ready`. A hand over cancels it at any step before the last, which
/// can't be taken back.
pub(super) async fn take_over(job: TakeOver) -> Taken {
    let TakeOver {
        mut standby,
        lease,
        mut cancel,
        config_path,
        timing,
    } = job;
    let handles = standby.handles.clone();
    let hooks = &handles.hooks;
    tokio::select! {
        biased;
        () = cancelled(&mut cancel) => return Taken::Cancelled,
        result = hooks.barrier(Barrier::BeforeReload, true) => {
            if let Err(error) = result {
                return Taken::Failed(format!("a test barrier failed: {error}"));
            }
        }
    }
    let path = config_path.clone();
    let reloaded = tokio::task::spawn_blocking(move || {
        let result = standby.service.reload_standby(&path);
        (standby, result)
    })
    .await;
    let (back, result) = match reloaded {
        Ok(reloaded) => reloaded,
        Err(error) => return Taken::Failed(format!("reload: the task failed: {error}")),
    };
    standby = back;
    if let Err(error) = result {
        return Taken::Failed(format!("reload: {error}"));
    }
    standby.config = Arc::clone(&standby.service.config);
    // The TLS material stays as the standby loaded it: a take over comes
    // seconds after the load, and upstream loads it once, at start.
    if *cancel.borrow() {
        return Taken::Cancelled;
    }
    let dir = handles.data_dir.clone();
    #[cfg(test)]
    let held = hooks.clone();
    let read = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        held.hold_step("read_handover");
        read_handover(&dir)
    });
    match read.await {
        Ok(Ok(handover)) => handles.management.restore_attempts(&handover),
        Ok(Err(reason)) => return Taken::Failed(reason),
        Err(error) => return Taken::Failed(format!("handover: the task failed: {error}")),
    }
    if hooks.drops_channel_at(Place::TakingOver) {
        return Taken::Dropped;
    }
    if let Some(delay) = hooks.fault(Fault::SlowReady) {
        let Ok(millis) = delay.parse::<u64>() else {
            return Taken::Failed("the delay isn't a number of milliseconds".to_owned());
        };
        tokio::select! {
            biased;
            () = cancelled(&mut cancel) => return Taken::Cancelled,
            () = tokio::time::sleep(Duration::from_millis(millis)) => {}
        }
    }
    if let Some(reason) = hooks.failure(Fault::FailBeforeReady) {
        return Taken::Failed(reason);
    }
    handles.reopen_logs().await;
    if *cancel.borrow() {
        return Taken::Cancelled;
    }
    match super::activate_unless_cancelled(
        standby,
        &config_path,
        lease,
        &timing,
        super::Launch::TakeOver,
        Some(cancel),
    )
    .await
    {
        Ok(active) => Taken::Ready(Box::new(active)),
        Err(super::Aborted::Failed(reason)) => Taken::Failed(reason),
        Err(super::Aborted::Cancelled) => Taken::Cancelled,
    }
}
