//! Not upstream's: what `status` and `cancel` answer from (v3 5.1 step 0).
//!
//! The core publishes a [`Snapshot`] whenever something it shows changes. The
//! endpoint's handlers read it and never wait for the core, so a switch that
//! is busy loading can still be asked about and cancelled. The status
//! snapshot is completed by a later run (item 6); this holds the fields the
//! switch knows.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

/// One worker as status shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerView {
    pub pid: u32,
    pub version: String,
    /// `active`, `standby`, `draining`, or `survivor` (a child of a failed
    /// handshake whose exit has not been seen).
    pub role: String,
    /// Its control channel closed while the process lives (6.2).
    pub lost: bool,
    /// Live connections, from its last `status` message.
    pub live: u64,
    pub signins_pending: u64,
}

/// The switch in progress, or the open watch window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchView {
    pub id: String,
    pub how: String,
    pub from: String,
    pub to: String,
    /// `loading`, `handing-over`, `taking-over` or `watching`.
    pub phase: String,
    /// v2 11.3's `switch.status`: `waiting-for-signins` (an automatic switch
    /// holding for sign-ins), then the phase (`loading`, `handing-over`,
    /// `taking-over`, `watching`), or `cancelled` once a cancel was accepted
    /// and the switch has not yet ended. (`committing` is the installed
    /// file's replacement, shown as `commit_pending`, not here.)
    #[serde(default)]
    pub status: String,
    /// When the switch started, RFC 3339 UTC.
    #[serde(default)]
    pub since: String,
    /// A `cancel` can still stop it (5.3: not past `take_over`).
    pub cancellable: bool,
    pub watch_until: Option<String>,
}

/// The released workers still finishing their connections (v2 11.3 `drain`).
/// The worker's `upgraded` count (connections that moved to the new version)
/// is not here: the supervisor is not told it. The worker's `status` message
/// would have to carry it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainView {
    /// Live connections on the draining workers, from their last `status`.
    pub live: u64,
    /// When the oldest drain began, RFC 3339 UTC.
    pub since: String,
    /// When the earliest drain reaches its cap and the worker is stopped.
    pub cap_at: String,
}

/// What the install step has left to do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallView {
    pub target: String,
    pub attempts: u32,
    pub next_try: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// `idle`, `switching`, `watching`, `degraded` or `stopping`.
    pub state: String,
    /// The supervisor's own version.
    #[serde(default)]
    pub supervisor_version: String,
    /// The version the installed file holds, as the journal records it (`null`
    /// when there is no journal).
    #[serde(default)]
    pub installed_version: Option<String>,
    /// The installed file still has to be replaced by the confirmed version
    /// (the install step is pending, see `install_pending`).
    #[serde(default)]
    pub commit_pending: bool,
    /// A worker needed a newer supervisor than this one: the service must be
    /// restarted (v2 11.3). Set for this supervisor's life.
    #[serde(default)]
    pub needs_restart: bool,
    /// `ok`, or `stale` when a journal write failed and the disk is older
    /// than memory (v3 11.3). The same fact as `journal_stale`.
    #[serde(default)]
    pub journal: String,
    /// `ok`, or `pending` when the state file lags the journal's outcomes
    /// (v3 11.3). The same fact as `state_write_pending`.
    #[serde(default)]
    pub state_write: String,
    /// Released workers still draining, or `null`.
    #[serde(default)]
    pub drain: Option<DrainView>,
    pub switch: Option<SwitchView>,
    pub active_version: Option<String>,
    #[serde(default)]
    pub workers: Vec<WorkerView>,
    /// A journal write failed and the disk is older than memory (3.6).
    #[serde(default)]
    pub journal_stale: bool,
    /// The state file lags behind the journal's outcomes (3.6, S1 to S3).
    #[serde(default)]
    pub state_write_pending: bool,
    pub switching_disabled: Option<String>,
    pub degraded: Option<String>,
    pub install_pending: Option<InstallView>,
    /// A prune of `versions/` is waiting: the update lock was busy, or it was
    /// refused and is retried on a timer.
    #[serde(default)]
    pub prune_pending: bool,
    /// The versions that must not be pruned (v3 3.5), as `keep_set` answers.
    /// The core computes it. It may lag a pin file written since the last
    /// publish by up to `status_every`. `null` is "not available": before the
    /// core's first publish, while it starts over after a panic, when the
    /// versions on disk could not be listed, and while a journal or state write
    /// is not durable (the prune holds off for the same reason). `keep_set`
    /// then answers an error, never an empty list (a caller prunes nothing
    /// without a reply, see `recovery.rs`). A keep set that cannot be computed
    /// (an unreadable or newer journal, an unparseable pin, or a state file that
    /// cannot be read) is not `null`: the field then lists every version
    /// directory present and every tracked worker's version, which protects
    /// everything.
    #[serde(default)]
    pub keep: Option<Vec<String>>,
}

/// What a `cancel` request found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelAnswer {
    /// The switch will end as cancelled.
    Cancelled,
    /// It is past the point where cancelling is possible.
    TooLate,
    /// Nothing is switching.
    Nothing,
}

/// Shared between the core and the endpoint's handlers.
#[derive(Debug, Default)]
pub struct Shared {
    snapshot: Mutex<Snapshot>,
    /// The id of the open operation whose cancel was accepted. Always taken
    /// after `snapshot` when both are held, so `request_cancel` and the end of
    /// an operation see one order.
    cancel: Mutex<Option<String>>,
    wake: Notify,
    /// Why the endpoint task is gone, once it is. Every later snapshot says so.
    endpoint_down: Mutex<Option<String>>,
    /// Runs once at the end of an operation, between its cancel window being
    /// retired and the picture that follows (tests only).
    #[cfg(test)]
    end_hook: Mutex<EndHook>,
    /// Runs once inside [`Shared::restart_over`], between the cancel being
    /// retired and the new picture being put in place (tests only).
    #[cfg(test)]
    restart_hook: Mutex<EndHook>,
    /// Runs once at the point a switch is told `Accepted` (tests only).
    #[cfg(test)]
    accept_hook: Mutex<EndHook>,
}

#[cfg(test)]
#[derive(Default)]
struct EndHook(Option<Box<dyn FnOnce() + Send>>);

#[cfg(test)]
impl std::fmt::Debug for EndHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EndHook({})", self.0.is_some())
    }
}

impl Shared {
    /// Records that the endpoint task has ended and shows it in the snapshot
    /// now and in every one published after. The core and the workers go on.
    pub fn set_endpoint_down(&self, reason: String) {
        *self
            .endpoint_down
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason);
        let current = self.snapshot();
        self.publish(current);
    }

    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn publish(&self, snapshot: Snapshot) {
        let mut snapshot = self.with_endpoint_state(snapshot);
        let mut held = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // A cancel that was accepted for the open operation stays shown in
        // every picture of that operation until it ends (which retires it): the
        // core rebuilds the switch from its own view, which says `loading` or
        // `handing-over` until the operation is unwound. Only the switch with
        // the cancelled operation's id says so; another switch (a restored
        // watch window) never does. Read under the same mutex `request_cancel`
        // takes, so it cannot slip between the two.
        let accepted = self
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(id) = accepted
            && let Some(switch) = snapshot.switch.as_mut()
            && switch.id == id
        {
            "cancelled".clone_into(&mut switch.status);
        }
        *held = snapshot;
    }

    /// The picture with the endpoint's end said in it, once the endpoint is gone.
    fn with_endpoint_state(&self, mut snapshot: Snapshot) -> Snapshot {
        let down = self
            .endpoint_down
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(reason) = down {
            let reason = format!("the endpoint is no longer served: {reason}");
            if snapshot.state != "stopping" {
                "degraded".clone_into(&mut snapshot.state);
            }
            snapshot.degraded = Some(match snapshot.degraded.take() {
                // Republishing a picture that already says it.
                Some(said) if said.contains(&reason) => said,
                Some(said) => format!("{said}; {reason}"),
                None => reason,
            });
        }
        snapshot
    }

    /// The supervisor starts over after a panic: retires the cancel of the
    /// operation that died and puts `snapshot` (the restarting picture) in
    /// place, as one step under the mutex `request_cancel` takes. No request
    /// can find the dead operation's cancellable switch between the two; one
    /// that comes after finds no switch (`Nothing`).
    pub(crate) fn restart_over(&self, snapshot: Snapshot) {
        let snapshot = self.with_endpoint_state(snapshot);
        let mut held = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *self
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        #[cfg(test)]
        {
            let hook = self
                .restart_hook
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
        *held = snapshot;
    }

    /// Sets the hook `restart_over` runs once (tests only).
    #[cfg(test)]
    pub(crate) fn on_restart_gap(&self, hook: impl FnOnce() + Send + 'static) {
        self.restart_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0 = Some(Box::new(hook));
    }

    /// Whether the snapshot mutex is held right now (tests only).
    #[cfg(test)]
    pub(crate) fn snapshot_lock_is_held(&self) -> bool {
        matches!(
            self.snapshot.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }

    /// Asks the open switch to cancel. Answered from the snapshot: it never
    /// waits for the core.
    pub fn request_cancel(&self) -> CancelAnswer {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &snapshot.switch {
            Some(switch) if switch.cancellable => {
                // Recorded with the operation it was accepted for.
                *self
                    .cancel
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(switch.id.clone());
                // Shown at once, without waiting for the core's next picture.
                if let Some(switch) = snapshot.switch.as_mut() {
                    "cancelled".clone_into(&mut switch.status);
                }
                self.wake.notify_waiters();
                CancelAnswer::Cancelled
            }
            Some(_) => CancelAnswer::TooLate,
            None => CancelAnswer::Nothing,
        }
    }

    /// Checkpoint 2: observe an accepted cancel or close this operation's
    /// cancel window under the same mutex used by `request_cancel`.
    pub(crate) fn close_cancel_window(&self, id: &str) -> bool {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_deref()
            == Some(id)
        {
            return true;
        }
        if let Some(switch) = snapshot.switch.as_mut()
            && switch.id == id
        {
            switch.cancellable = false;
        }
        false
    }

    /// Whether a cancel is waiting for the open operation. It is recorded with
    /// the operation's id and retired when that operation ends.
    pub fn cancel_requested(&self) -> bool {
        self.cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Forgets a cancel (a new switch starts, or the supervisor starts over).
    pub fn clear_cancel(&self) {
        *self
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// The operation `id` ends: retires its cancel and closes its cancel
    /// window in the snapshot, together under the mutex `request_cancel` takes.
    /// A request that comes after finds a switch it can no longer cancel
    /// (`TooLate`), not one it could accept a cancel for, and no cancel is left
    /// to be shown on the switch that follows.
    pub(crate) fn end_operation(&self, id: &str) {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *self
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        if let Some(switch) = snapshot.switch.as_mut()
            && switch.id == id
        {
            switch.cancellable = false;
        }
    }

    /// Runs the test hook for the end of an operation, if one is set.
    #[cfg(test)]
    pub(crate) fn run_end_hook(&self) {
        let hook = self
            .end_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Runs the test hook for the moment `Accepted` is sent, if one is set.
    #[cfg(test)]
    pub(crate) fn run_accept_hook(&self) {
        let hook = self
            .accept_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Sets the hook `run_accept_hook` runs once (tests only).
    #[cfg(test)]
    pub(crate) fn on_accept(&self, hook: impl FnOnce() + Send + 'static) {
        self.accept_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0 = Some(Box::new(hook));
    }

    /// Sets the hook `run_end_hook` runs once (tests only).
    #[cfg(test)]
    pub(crate) fn on_operation_end(&self, hook: impl FnOnce() + Send + 'static) {
        self.end_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0 = Some(Box::new(hook));
    }

    /// Resolves when a cancel is asked for.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.wake.notified();
            if self.cancel_requested() {
                return;
            }
            notified.await;
        }
    }
}
