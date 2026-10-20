//! Not upstream's: one switch, from admission to the start of the watch window
//! (v3 5.1 steps 0 to 9) and the failure rows of 6.1 that end before it (a to
//! e, l). The watch window, its confirmation and the returns of rows f, g and j
//! are in `core.rs`, because they outlive the switch's task.
//!
//! The switch runs inside the core's own task, so it is the only thing that
//! touches the journal while it runs. While it waits for a worker it still
//! serves the other workers' events and the core's timers (`wait_for`).
//! Disk calls are synchronous and run inline.

use std::time::Duration;

use open_ferry_update::journal::{
    Admitted, Artifact, Blame, How, MarkFailed, Outcome, Phase, Switch, WorkerRole,
};
use open_ferry_update::state::SwitchRecord;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;

use crate::core::{Core, Drain, Open, SwitchRequest, TICK, Tracked, Watch};
use crate::disk::Disk;
use crate::driver::{Driver, Event, ExitInfo, SpawnError, WorkerId};
use crate::host::Host;
use crate::protocol::{Admission, EndpointErrorCode, EndpointReply, Message, Role, VERSION};
use crate::snapshot::SwitchView;
use crate::startup::Run;

/// What a wait for one worker ended with.
#[derive(Debug)]
pub(crate) enum Waited {
    Msg(Message),
    Exited(ExitInfo),
    /// Its channel closed; the process may live (6.2).
    Lost,
    /// The standby failed while the active worker was handing over.
    PeerGone {
        lost: bool,
        why: String,
    },
    /// The active worker exited or was lost before its hand over (row d).
    AGone {
        lost: bool,
        why: String,
    },
    Timeout,
    Cancel,
    /// A service stop arrived.
    Stop,
}

/// How the spawn's handshake ended.
enum Shaken {
    Done(Result<Result<crate::driver::WorkerPort, SpawnError>, tokio::time::error::Elapsed>),
    Stop,
    /// An explicit cancel, or mode off: the spawn was dropped.
    Cancel,
    /// The active worker failed before the new one was up.
    AGone {
        lost: bool,
        why: String,
    },
}

/// How a switch ended.
#[derive(Debug)]
pub(crate) enum Ended {
    Result(Value),
    Stopped,
}

/// Admission failures keep their kind from validation through the endpoint reply.
struct Refusal {
    code: Option<EndpointErrorCode>,
    reason: String,
}

impl Refusal {
    fn coded(code: EndpointErrorCode, reason: &str) -> Self {
        Self {
            code: Some(code),
            reason: reason.to_owned(),
        }
    }
}

impl From<String> for Refusal {
    fn from(reason: String) -> Self {
        Self { code: None, reason }
    }
}

impl From<&str> for Refusal {
    fn from(reason: &str) -> Self {
        reason.to_owned().into()
    }
}

type Replies = UnboundedSender<EndpointReply>;

fn say(replies: &Replies, reply: EndpointReply) {
    // A client that went away does not stop the switch.
    let _ = replies.send(reply);
}

fn never(_: &Message) -> bool {
    false
}

impl<D: Driver, H: Host, S: Disk> Core<'_, D, H, S> {
    // ---- waiting ----------------------------------------------------------

    /// Waits until worker `id` sends a message `matches` accepts, exits, is
    /// lost, or `deadline` passes. Every other event goes to the background
    /// handlers; a second `switch` is refused; the one-second timers run.
    pub(crate) async fn wait_for(
        &mut self,
        id: WorkerId,
        deadline: Instant,
        cancellable: bool,
        matches: impl Fn(&Message) -> bool,
    ) -> Waited {
        self.wait_impl(id, None, true, deadline, cancellable, matches)
            .await
    }

    /// A wait of an exit cleanup (a kill, a stop, a dismissal). It never ends
    /// early on the active worker's failure: it keeps polling the process
    /// being reaped, Stop and the timers, and the enclosing switch acts on A's
    /// failure afterwards (`a_failed_in_cleanup`). Otherwise the first wait
    /// after A's failure would return at once, for ever (reviewer H1).
    pub(crate) async fn wait_cleanup(
        &mut self,
        id: WorkerId,
        deadline: Instant,
        matches: impl Fn(&Message) -> bool,
    ) -> Waited {
        self.wait_impl(id, None, false, deadline, false, matches)
            .await
    }

    async fn wait_with_peer(
        &mut self,
        id: WorkerId,
        peer: Option<WorkerId>,
        deadline: Instant,
        cancellable: bool,
        matches: impl Fn(&Message) -> bool,
    ) -> Waited {
        self.wait_impl(id, peer, true, deadline, cancellable, matches)
            .await
    }

    async fn wait_impl(
        &mut self,
        id: WorkerId,
        peer: Option<WorkerId>,
        early_a: bool,
        deadline: Instant,
        cancellable: bool,
        matches: impl Fn(&Message) -> bool,
    ) -> Waited {
        loop {
            // A-N2: the panic fault also fires inside a switch.
            self.fault_check();
            if early_a
                && let Some(a) = self.watch_a
                && let Some((lost, why)) = self.a_failure(a)
            {
                return Waited::AGone { lost, why };
            }
            if let Some(peer) = peer {
                match self.rt.workers.get(&peer) {
                    None => {
                        return Waited::PeerGone {
                            lost: false,
                            why: "the new version exited before take over".to_owned(),
                        };
                    }
                    Some(worker) if worker.lost => {
                        return Waited::PeerGone {
                            lost: true,
                            why: "the new version's control channel was lost before take over"
                                .to_owned(),
                        };
                    }
                    _ => {}
                }
            }
            let tick = self.next_tick;
            tokio::select! {
                (wid, event) = self.registry.next_event() => {
                    // A survivor's `Exited` leaves the journal's workers now.
                    self.survivors_changed();
                    if Some(wid) == peer {
                        match event {
                            Event::Exited(info) => {
                                self.rt.workers.remove(&wid);
                                self.sync_workers();
                                return Waited::PeerGone { lost: false, why: format!("the new version exited before take over ({})", info.text) };
                            }
                            Event::Lost(why) => {
                                self.worker_lost(wid, &why);
                                return Waited::PeerGone { lost: true, why: "the new version's control channel was lost before take over".to_owned() };
                            }
                            Event::Message(message) => self.note_message(wid, &message),
                        }
                        continue;
                    }
                    if wid != id {
                        self.background_event(wid, event).await;
                        continue;
                    }
                    match event {
                        // A message that arrives after `Lost` is not a live worker's.
                        Event::Message(message)
                            if matches(&message) && !self.is_lost(wid) =>
                        {
                            return Waited::Msg(message);
                        }
                        Event::Message(message) => self.note_message(wid, &message),
                        Event::Lost(why) => {
                            self.worker_lost(wid, &why);
                            return Waited::Lost;
                        }
                        Event::Exited(info) => {
                            self.save_a_progress(wid);
                            self.rt.workers.remove(&wid);
                            if self.rt.degraded.as_deref() == Some(&format!("worker {wid} did not exit after termination")) {
                                self.rt.degraded = None;
                            }
                            return Waited::Exited(info);
                        }
                    }
                }
                command = self.commands.recv() => match command {
                    Some(crate::core::Command::Switch { replies, .. }) => {
                        say(&replies, EndpointReply::Error {
                            v: VERSION,
                            code: None,
                            reason: "busy: switching".to_owned(),
                        });
                    }
                    Some(crate::core::Command::Stop) | None => {
                        self.rt.stop_requested = true;
                        return Waited::Stop;
                    }
                },
                () = tokio::time::sleep_until(deadline) => return Waited::Timeout,
                () = self.shared.cancelled(), if cancellable => return Waited::Cancel,
                () = tokio::time::sleep_until(tick) => {
                    self.next_tick = Instant::now() + TICK;
                    // 5.3: in a cancellable wait the mode, re-read here every
                    // tick, ends the wait as an explicit cancel does, so mode
                    // off acts at once (an immediate `resume` during the hand
                    // over) and not at the next checkpoint. The checkpoints
                    // stay: they are what decides. It is read before the
                    // background work, which can await an install that has
                    // nothing to do with the cancel.
                    if cancellable && self.mode_cancels() {
                        return Waited::Cancel;
                    }
                    self.tick_background().await;
                    self.publish();
                    if cancellable && self.mode_cancels() {
                        return Waited::Cancel;
                    }
                }
            }
        }
    }

    /// Not upstream's, test builds only: a named barrier of the supervisor's
    /// own (`<name>.hold`, then `.arrived`, `.release`; see
    /// `open_ferry_update::test_seams`), run off the runtime's thread so the
    /// rest of the supervisor, the endpoint included, goes on while it holds.
    #[cfg(feature = "blue-green-test")]
    async fn barrier(&self, name: &'static str) {
        let root = self.cfg.data.root().to_path_buf();
        let _ =
            tokio::task::spawn_blocking(move || open_ferry_update::test_seams::hit(&root, name))
                .await;
    }

    #[cfg(not(feature = "blue-green-test"))]
    #[allow(clippy::unused_async)]
    async fn barrier(&self, _name: &'static str) {}

    /// The active worker is gone or lost, before its hand over.
    pub(crate) fn a_failure(&self, a: WorkerId) -> Option<(bool, String)> {
        match self.rt.workers.get(&a) {
            None => Some((
                false,
                "the active worker exited before the hand over".to_owned(),
            )),
            Some(worker) if worker.lost => Some((
                true,
                "the active worker's control channel was lost before the hand over".to_owned(),
            )),
            _ => None,
        }
    }

    /// A worker the switch has a port for, or a child the registry still
    /// tracks from a handshake that has not had its exit observed.
    fn is_tracked(&self, id: WorkerId) -> bool {
        self.rt.workers.contains_key(&id) || self.registry.survivors().contains(&id)
    }

    /// A failed while an exit cleanup waited: the enclosing switch takes row d
    /// now, with the cleaned worker already gone. A cancel that is ready (or
    /// the mode) is arbitrated first, once, as `end_load_failed` does: it
    /// wins over A's failure, and the switch ends cancelled, with no record,
    /// no `mark_failed` and no return. A's failure is then the active
    /// worker's ordinary exit, and the restart ladder handles it. The window
    /// closes here too, so a cancel that comes later is too late.
    async fn a_failed_in_cleanup(&mut self, open: &Open) -> Option<Ended> {
        let a = self.watch_a?;
        self.a_failed_after(open, a).await
    }

    /// The one arbitration of an A that failed during a clean-up, for every
    /// route that has the clean-up done: `None` when A did not fail. A cancel
    /// that is ready wins (`cancelled_after_cleanup`); otherwise A's failure
    /// takes row d (`a_gone_early`, which arbitrates once more after the waits
    /// of its own).
    async fn a_failed_after(&mut self, open: &Open, a: WorkerId) -> Option<Ended> {
        let (lost, why) = self.a_failure(a)?;
        if self.close_window(open) || self.mode_cancels() {
            return Some(self.cancelled_after_cleanup(open, a, lost).await);
        }
        Some(self.a_gone_early(open, a, None, lost, &why).await)
    }

    /// The arbitration for a failure that ends a switch after a clean-up or a
    /// resume while the cancel window is still open (before checkpoint 2):
    /// the window first, then A. A cancel that was accepted wins whether or
    /// not A failed. A failed A: `cancelled_after_cleanup`. A healthy A: the
    /// switch ends cancelled and A is left running (no record, no
    /// `mark_failed`, no return, no restart). No cancel: A's failure takes
    /// row d (`a_gone_early`); a healthy A is `None`, and the caller's
    /// failure stands.
    async fn cancel_then_a(&mut self, open: &Open, a: WorkerId) -> Option<Ended> {
        if self.close_window(open) || self.mode_cancels() {
            return Some(match self.a_failure(a) {
                Some((lost, _)) => self.cancelled_after_cleanup(open, a, lost).await,
                None => self.end_cancelled(open),
            });
        }
        let (lost, why) = self.a_failure(a)?;
        Some(self.a_gone_early(open, a, None, lost, &why).await)
    }

    /// A's ladder, for a restart of A: what was read when A was removed, or
    /// A's own entry while it is still tracked; the default only for an A that
    /// had no rung at all.
    fn a_progress_of(&self, a: WorkerId) -> crate::ladder::Progress {
        self.a_progress
            .clone()
            .or_else(|| self.rt.workers.get(&a).map(Tracked::progress_after_exit))
            .unwrap_or_default()
    }

    /// Closes the cancel window of `open`: in the shared snapshot, under the
    /// mutex a cancel is accepted under, and in the core's own picture of the
    /// switch, which the next tick would otherwise publish with the window open
    /// again. `true` when a cancel had already been accepted.
    fn close_window(&mut self, open: &Open) -> bool {
        let cancel = self.shared.close_cancel_window(&open.id);
        if let Some(view) = self.rt.view.as_mut() {
            view.cancellable = false;
        }
        cancel
    }

    /// The switch ends cancelled although A failed during the clean-up. An A
    /// that exited has been through `worker_exited`, which scheduled its
    /// restart. A lost one is awaited (it holds the lease), forgotten, and
    /// restarted from the rung it was on.
    async fn cancelled_after_cleanup(&mut self, open: &Open, a: WorkerId, lost: bool) -> Ended {
        self.watch_a = None;
        let progress = self.a_progress_of(a);
        if lost {
            if !self.wait_exit(a).await {
                return Ended::Stopped;
            }
            self.forget_active(a);
        }
        // A that exited through `worker_exited` has its restart scheduled; one
        // that was lost, or removed by a wait, has none yet.
        if self.rt.restart.is_none() {
            self.schedule_restart(progress);
            self.rt.degraded = Some(if lost {
                "the active worker's control channel was lost during a cancelled switch".to_owned()
            } else {
                "the active worker ended during a cancelled switch".to_owned()
            });
        }
        self.end_cancelled(open)
    }

    fn is_lost(&self, id: WorkerId) -> bool {
        self.rt.workers.get(&id).is_some_and(|worker| worker.lost)
    }

    /// Waits for a worker's process to be gone. Terminates it by its handle
    /// when it has not exited after the stop budget (6.2). `false` when a
    /// stop arrived first.
    pub(crate) async fn wait_exit(&mut self, id: WorkerId) -> bool {
        if !self.is_tracked(id) {
            return true;
        }
        for kill in [false, true] {
            if kill {
                self.registry.kill(id);
            }
            let budget = if kill {
                self.cfg.timings.stop
            } else {
                self.cfg.timings.stop + Duration::from_secs(5)
            };
            match self.wait_cleanup(id, Instant::now() + budget, never).await {
                Waited::Exited(_) => return true,
                Waited::Stop => return false,
                _ => {}
            }
        }
        tracing::error!(worker = id, "a worker did not exit after it was terminated");
        self.worker_lost(id, "did not exit after termination");
        if let Some(tracked) = self.rt.workers.get_mut(&id) {
            tracked.kill_at = None;
        }
        self.rt.degraded = Some(format!("worker {id} did not exit after termination"));
        let _ = self.write_journal("J-workers");
        self.publish();
        // A deadline is not an exit. The worker may still hold the lease and
        // handover file; retain it and continue only on the handle's event.
        loop {
            match self
                .wait_cleanup(id, Instant::now() + Duration::from_secs(10), never)
                .await
            {
                Waited::Exited(_) => return true,
                Waited::Stop => return false,
                _ => tracing::warn!(worker = id, "still waiting for a terminated worker to exit"),
            }
        }
    }

    /// Kills a worker by its handle and waits for its exit.
    pub(crate) async fn kill_and_wait(&mut self, id: WorkerId) -> bool {
        self.registry.kill(id);
        self.wait_exit(id).await
    }

    /// The effective mode is `off` and the user turned it off after the
    /// request was admitted (5.3).
    pub(crate) fn mode_cancels(&mut self) -> bool {
        let Some(admitted) = self.admitted.clone() else {
            return false;
        };
        let mode = self.eval_mode();
        mode.off && (!admitted.while_off || self.mode_gen != admitted.mode_gen)
    }

    /// A checkpoint (5.3): an explicit cancel, or the mode.
    pub(crate) fn checkpoint(&mut self) -> bool {
        self.shared.cancel_requested() || self.mode_cancels()
    }

    fn show(&mut self, open: &Open, cancellable: bool) {
        self.rt.view = Some(SwitchView {
            id: open.id.clone(),
            how: open.how.as_str().to_owned(),
            from: open.from.version.clone(),
            to: open.to.version.clone(),
            phase: open.phase.as_str().to_owned(),
            status: open.phase.as_str().to_owned(),
            since: open.started.clone(),
            cancellable,
            watch_until: None,
        });
        self.publish();
    }

    fn progress(&mut self, replies: &Replies, open: &Open, cancellable: bool) {
        self.show(open, cancellable);
        say(
            replies,
            EndpointReply::Progress {
                v: VERSION,
                id: open.id.clone(),
                phase: open.phase.as_str().to_owned(),
            },
        );
    }

    // ---- admission and the whole switch -----------------------------------

    /// The public entry point: one `switch` request (5.1 step 0).
    pub(crate) async fn handle_switch(&mut self, request: SwitchRequest, replies: Replies) {
        let refuse = |refusal: Refusal| {
            say(
                &replies,
                EndpointReply::Error {
                    v: VERSION,
                    code: refusal.code,
                    reason: refusal.reason,
                },
            );
        };
        if self.rt.stop_requested {
            return refuse("the supervisor is stopping".into());
        }
        if self.rt.switching {
            return refuse("busy: switching".into());
        }
        if self.rt.returning.is_some() {
            return refuse("busy: recovering from a failed switch".into());
        }
        // An install that fell due while an earlier switch's handler ran goes
        // first: back-to-back switches can't starve it.
        if self.rt.install_due.is_some_and(|due| due <= Instant::now()) {
            self.rt.install_due = None;
            self.try_install().await;
        }
        if self.rt.watch.is_some() && request.how == How::Update {
            return refuse("busy: watching".into());
        }
        if let Some(why) = &self.rt.disabled {
            tracing::debug!("switching is disabled: {why}");
            return refuse(Refusal::coded(
                EndpointErrorCode::SwitchingDisabled,
                "switching is disabled; see the supervisor log",
            ));
        }
        if self.rt.journal.is_none() {
            return refuse("there is no journal".into());
        }
        // The last switch's cleanup waits for a durable journal (its hand over
        // file, its pin). A new switch would write its own file under it, so
        // none is admitted until the cleanup is done; the write is tried again
        // first (oracle H2, reviewer M3).
        if self.cleanup_pending() {
            let _ = self.write_journal("J-retry");
            if self.cleanup_pending() {
                return refuse("busy: finishing the last switch's cleanup".into());
            }
        }
        let Some((active, from)) = self
            .rt
            .active
            .and_then(|id| self.rt.workers.get(&id).map(|w| (id, w)))
            .filter(|(_, w)| !w.lost)
            .map(|(id, w)| (id, w.artifact.clone()))
        else {
            return refuse("there is no active worker".into());
        };
        // 5.3: the supervisor stamps the admission from its own reading; what
        // the client sent is not trusted.
        let reading = self.eval_mode();
        let admitted = Admission {
            while_off: reading.off,
            mode_gen: self.mode_gen,
        };
        if request.admitted != admitted {
            tracing::debug!(
                "ignored the client's admission {:?}; using {admitted:?}",
                request.admitted
            );
        }
        if request.auto && !reading.auto {
            return refuse("automatic updates are not on".into());
        }
        let to = match self.validate_target(&request, &from).await {
            Ok(to) => to,
            Err(reason) => return refuse(reason),
        };
        let id = self.new_id();
        let superseding = if request.how == How::Rollback {
            self.rt.watch.as_ref().map(|watch| watch.open.id.clone())
        } else {
            None
        };
        self.rt.switching = true;
        self.shared.clear_cancel();
        self.admitted = Some(admitted);
        let mut open = Open {
            id: id.clone(),
            how: request.how,
            from,
            to,
            started: self.stamp(),
            phase: Phase::Loading,
            reached_ready: false,
        };
        // The switch is in the snapshot, and cancellable, from admission
        // (A-M2), and before the client is told `Accepted`: a cancel it sends
        // at once must find this switch.
        self.show(&open, true);
        #[cfg(test)]
        self.shared.run_accept_hook();
        say(
            &replies,
            EndpointReply::Accepted {
                v: VERSION,
                id: id.clone(),
            },
        );
        self.watch_a = Some(active);
        self.a_progress = None;
        let ended = self
            .run_switch(&request, &mut open, active, superseding, &replies)
            .await;
        self.watch_a = None;
        self.a_progress = None;
        if matches!(ended, Ended::Stopped) {
            self.begin_stop_all();
            self.stop_mid_switch(&open).await;
        }
        self.rt.switching = false;
        // A reset A's `loaded` could not write, and the switching-off a failed
        // ladder write waited for: with the outcome decided and nothing yet
        // spawned after it (round 8b).
        self.settle_ladder_after_switch(!matches!(ended, Ended::Stopped));
        self.rt.view = None;
        self.admitted = None;
        self.displaced_watch = None;
        self.shared.end_operation(&open.id);
        #[cfg(test)]
        self.shared.run_end_hook();
        self.peers_gone_check();
        self.publish();
        match ended {
            Ended::Result(result) => say(
                &replies,
                EndpointReply::Done {
                    v: VERSION,
                    id,
                    result,
                },
            ),
            Ended::Stopped => say(
                &replies,
                EndpointReply::Error {
                    v: VERSION,
                    code: None,
                    reason: "the supervisor is stopping".to_owned(),
                },
            ),
        }
    }

    /// Step 1's checks of `to`: a pin, a kept file, its hash.
    async fn validate_target(
        &mut self,
        request: &SwitchRequest,
        from: &Artifact,
    ) -> Result<Artifact, Refusal> {
        self.prune_automatic_cooldowns();
        if request.auto {
            let journal = self.rt.journal.as_ref().ok_or("there is no journal")?;
            let state = open_ferry_update::state::State::load_strict(&self.cfg.data.state_file())
                .map_err(|why| {
                tracing::warn!("could not read automatic switch exclusions: {why}");
                "automatic switch exclusions could not be read".to_owned()
            })?;
            let mut failed = state.has_failed(&request.to);
            let mut rolled_back = state.rolled_back.as_deref() == Some(&request.to);
            for outcome in journal
                .outcomes
                .iter()
                .chain(self.rt.pending.iter().map(|pending| &pending.outcome))
            {
                failed |= outcome
                    .mark_failed
                    .as_ref()
                    .is_some_and(|failed| failed.version == request.to);
                rolled_back |= outcome.record.how == "rollback"
                    && outcome.record.result.as_deref() == Some("rolled-back")
                    && outcome.record.from == request.to;
            }
            if failed {
                return Err("that version failed here; automatic switching skips it".into());
            }
            if rolled_back {
                return Err(
                    "that version was rolled back from; automatic switching skips it".into(),
                );
            }
            let records: Vec<_> = state
                .history
                .iter()
                .chain(state.last_switch.iter())
                .chain(journal.outcomes.iter().map(|outcome| &outcome.record))
                .chain(
                    self.rt
                        .pending
                        .iter()
                        .map(|pending| &pending.outcome.record),
                )
                .filter(|record| record.to == request.to)
                .cloned()
                .collect();
            for record in records {
                self.remember_outcome(&record);
            }
            self.prune_automatic_cooldowns();
            if self.automatic_cooldowns.contains_key(&request.to) {
                return Err(
                    "a recent switch to that version did not finish; trying again later".into(),
                );
            }
        }
        let journal = self.rt.journal.as_ref().ok_or("there is no journal")?;
        match request.how {
            How::Update => {
                self.disk
                    .check_pin(&request.pin, &request.to, &request.sha256, self.clock.now())
                    .map_err(|why| {
                        tracing::warn!("the switch pin was refused: {why}");
                        "the pin was refused".to_owned()
                    })?;
                if request.sha256.eq_ignore_ascii_case(&from.sha256) {
                    return Err("that version is already running".into());
                }
            }
            How::Rollback => {
                // No pin check here, on purpose: a rollback validates against the
                // journal's own artifact (v3 line 236) and `pins` apply to updates
                // only (v3 line 287).
                // The target comes from the journal, never from the request's
                // word (5.4): `switch.from` during a window, else `previous`.
                let expected = match &self.rt.watch {
                    Some(watch) => Some(watch.open.from.clone()),
                    None => journal.previous.clone(),
                };
                let Some(expected) = expected else {
                    return Err(Refusal::coded(
                        EndpointErrorCode::NoPrevious,
                        "there is no previous version to roll back to",
                    ));
                };
                if expected.version != request.to
                    || !expected.sha256.eq_ignore_ascii_case(&request.sha256)
                {
                    return Err(Refusal::coded(
                        EndpointErrorCode::RollbackTargetMismatch,
                        "that is not the version to roll back to",
                    ));
                }
            }
        }
        let artifact = self
            .disk
            .artifact(&request.to, &request.sha256)
            .map_err(|why| {
                tracing::warn!("the kept switch target was refused: {why}");
                "the kept copy is missing or doesn't match".to_owned()
            })?;
        if request.how == How::Rollback {
            // 5.4: a rollback validates its own target, the hash above and
            // then the binary's own `--version`.
            let binary = self.cfg.data.binary(&request.to, &self.cfg.binary_name);
            match self
                .cfg
                .runner
                .version(&binary, self.cfg.detect_timeout)
                .await
            {
                // The exact text, as the updater's own binary check does:
                // `open-ferry 11.0.0` and `open-ferry 1.0.0-rc.1` contain `1.0.0`.
                Ok(text) if text == format!("open-ferry {}", request.to) => {}
                Ok(text) => {
                    tracing::warn!("the rollback target's --version printed {text:?}");
                    return Err(
                        "the rollback target's --version printed a different version".into(),
                    );
                }
                Err(why) => {
                    tracing::warn!("the rollback target's --version failed: {why}");
                    return Err("the rollback target's --version failed".into());
                }
            }
        }
        Ok(artifact)
    }

    async fn run_switch(
        &mut self,
        request: &SwitchRequest,
        open: &mut Open,
        a: WorkerId,
        superseding: Option<String>,
        replies: &Replies,
    ) -> Ended {
        // Step 1: an automatic switch waits for pending sign-ins.
        if request.auto {
            if let Some(view) = self.rt.view.as_mut() {
                "waiting-for-signins".clone_into(&mut view.status);
            }
            self.publish();
            match self.signin_wait(a).await {
                SignIn::Go => {}
                SignIn::Cancel => return cancelled(&open.id, None),
                SignIn::Off => return cancelled(&open.id, Some("updates were turned off")),
                SignIn::Stop => return Ended::Stopped,
                SignIn::AGone(why) => {
                    // A cancel accepted before A's failure was seen wins; A's
                    // failure is then its ordinary exit. Nothing is cleaned up
                    // after the failure is seen, so there is no later window.
                    if self.close_window(open) || self.mode_cancels() {
                        return cancelled(&open.id, None);
                    }
                    // No switch is open yet: the ordinary restart brings A back.
                    return self.end_failed(
                        open,
                        Phase::Loading,
                        Blame::Environment,
                        &why,
                        false,
                        false,
                    );
                }
            }
        }
        self.progress(replies, open, true);

        // Step 2: J1.
        if let Err(why) = self.write_j1(open, superseding.as_deref()) {
            tracing::warn!("could not write the switch journal: {why}");
            return self.end_failed(
                open,
                Phase::Loading,
                Blame::Environment,
                "could not write the journal",
                false,
                false,
            );
        }
        // Item 5's hook: the pin is consumed only after J1 has landed.
        self.consume_pin_when_durable(&request.pin);

        // Step 3: load B.
        let b = match self.load(open).await {
            Ok(b) => b,
            Err(ended) => return ended,
        };

        if let Some((lost, why)) = self.a_failure(a) {
            return self.a_gone_early(open, a, Some(b), lost, &why).await;
        }
        if let Some((lost, why)) = self.standby_failure(b).await {
            return self.standby_gone(open, a, b, lost, &why).await;
        }

        // Step 4: checkpoint 1.
        if self.checkpoint() {
            if !self.kill_and_wait(b).await {
                return Ended::Stopped;
            }
            if let Some(ended) = self.a_failed_in_cleanup(open).await {
                return ended;
            }
            return self.end_cancelled(open);
        }

        // Step 5: J2, then hand over. From here the hand over's own wait
        // watches A.
        if let Some((lost, why)) = self.a_failure(a) {
            return self.a_gone_early(open, a, Some(b), lost, &why).await;
        }
        self.watch_a = None;
        open.phase = Phase::HandingOver;
        if let Err(why) = self.write_phase(Phase::HandingOver, "J2") {
            if !self.kill_and_wait(b).await {
                return Ended::Stopped;
            }
            // A may have failed while B was reaped, or a cancel may have been
            // accepted: the one arbitration.
            if let Some(ended) = self.cancel_then_a(open, a).await {
                return ended;
            }
            tracing::warn!("could not write the switch journal: {why}");
            return self.end_failed(
                open,
                Phase::HandingOver,
                Blame::Environment,
                "could not write the journal",
                true,
                false,
            );
        }
        self.progress(replies, open, true);
        self.registry.send(
            a,
            Message::HandOver {
                budget_ms: millis(self.cfg.timings.handover),
            },
        );
        let deadline = Instant::now() + self.cfg.timings.handover;
        match self
            .wait_with_peer(a, Some(b), deadline, true, |m| {
                matches!(m, Message::HandedOver { .. })
            })
            .await
        {
            Waited::Msg(_) => {}
            Waited::Stop => return Ended::Stopped,
            Waited::AGone { lost, why } => {
                return self.a_gone_early(open, a, Some(b), lost, &why).await;
            }
            Waited::Exited(info) => {
                let why = format!(
                    "the active worker exited during the hand over ({})",
                    info.text
                );
                return self.a_gone(open, b, &why).await;
            }
            Waited::Lost => {
                if !self.wait_exit(a).await {
                    return Ended::Stopped;
                }
                let why = "the active worker's channel was lost during the hand over".to_owned();
                return self.a_gone(open, b, &why).await;
            }
            Waited::PeerGone { lost, why } => {
                if lost && !self.wait_exit(b).await {
                    return Ended::Stopped;
                }
                return self
                    .back_to_a(open, a, Some((Blame::NewVersion, &why)))
                    .await;
            }
            Waited::Timeout => {
                if !self.kill_and_wait(b).await {
                    return Ended::Stopped;
                }
                let why = "the active worker did not hand over within the budget";
                return self
                    .back_to_a(open, a, Some((Blame::Environment, why)))
                    .await;
            }
            Waited::Cancel => {
                if !self.kill_and_wait(b).await {
                    return Ended::Stopped;
                }
                return self.back_to_a(open, a, None).await;
            }
        }
        let handed_over = Instant::now();
        if let Some((lost, why)) = self.standby_failure(b).await {
            return self.standby_gone(open, a, b, lost, &why).await;
        }

        // Test builds only: a test can turn the mode off exactly here, after
        // `handed_over` and before checkpoint 2 and `take_over`.
        self.barrier("switch.before_checkpoint_two").await;

        // Step 6: checkpoint 2. Accepting a cancel and closing its window
        // share the snapshot mutex; J3 must never run with the window open.
        if self.close_window(open) || self.mode_cancels() {
            if !self.kill_and_wait(b).await {
                return Ended::Stopped;
            }
            return self.back_to_a(open, a, None).await;
        }

        // Step 7: J3. Past this point a cancel is too late.
        open.phase = Phase::TakingOver;
        if let Err(why) = self.write_phase(Phase::TakingOver, "J3") {
            if !self.kill_and_wait(b).await {
                return Ended::Stopped;
            }
            tracing::warn!("could not write the switch journal: {why}");
            return self
                .back_to_a(
                    open,
                    a,
                    Some((Blame::Environment, "could not write the journal")),
                )
                .await;
        }
        self.progress(replies, open, false);

        // Step 8: take over.
        if let Some((lost, why)) = self.standby_failure(b).await {
            return self.standby_gone(open, a, b, lost, &why).await;
        }
        self.registry.send(b, Message::TakeOver);
        let deadline = Instant::now() + self.cfg.timings.ready;
        let waited = self
            .wait_for(b, deadline, false, |m| {
                matches!(m, Message::Ready | Message::TakeFailed { .. })
            })
            .await;
        match waited {
            Waited::Msg(Message::Ready) => {}
            Waited::Stop => return Ended::Stopped,
            other => return self.take_over_failed(open, a, b, other).await,
        }

        // Step 9: the commit point.
        let gap_ms = millis(handed_over.elapsed());
        self.commit(open, a, b, gap_ms)
    }

    /// Check queued failures at the synchronous phase boundaries too: a
    /// `loaded` or `handed_over` must not hide B's already queued exit.
    async fn standby_failure(&mut self, b: WorkerId) -> Option<(bool, String)> {
        while let Some((id, event)) = self.registry.try_next_event() {
            if id == b {
                match event {
                    Event::Exited(info) => {
                        self.rt.workers.remove(&b);
                        self.sync_workers();
                        return Some((
                            false,
                            format!("the new version exited before take over ({})", info.text),
                        ));
                    }
                    Event::Lost(why) => {
                        self.worker_lost(b, &why);
                        return Some((
                            true,
                            "the new version's control channel was lost before take over"
                                .to_owned(),
                        ));
                    }
                    Event::Message(message) => self.note_message(b, &message),
                }
            } else {
                self.background_event(id, event).await;
            }
        }
        match self.rt.workers.get(&b) {
            None => Some((false, "the new version exited before take over".to_owned())),
            Some(worker) if worker.lost => Some((
                true,
                "the new version's control channel was lost before take over".to_owned(),
            )),
            _ => None,
        }
    }

    async fn standby_gone(
        &mut self,
        open: &Open,
        a: WorkerId,
        b: WorkerId,
        lost: bool,
        why: &str,
    ) -> Ended {
        if lost && !self.wait_exit(b).await {
            return Ended::Stopped;
        }
        self.back_to_a(open, a, Some((Blame::NewVersion, why)))
            .await
    }

    // ---- the sign-in wait -------------------------------------------------

    pub(crate) async fn signin_wait(&mut self, a: WorkerId) -> SignIn {
        let deadline = Instant::now() + self.cfg.timings.signin_wait;
        let mut last_ping: Option<Instant> = None;
        // The status count when the last ping went out: only a status after it
        // answers it (A-L5).
        let mut ping_seq = 0;
        loop {
            if self.shared.cancel_requested() {
                return SignIn::Cancel;
            }
            if self.host.effective_mode().off {
                return SignIn::Off;
            }
            let (pending, seq) = self
                .rt
                .workers
                .get(&a)
                .map_or((0, 0), |w| (w.signins, w.status_seq));
            if last_ping.is_some() && seq > ping_seq && pending == 0 {
                return SignIn::Go;
            }
            if Instant::now() >= deadline {
                return SignIn::Go;
            }
            if last_ping.is_none_or(|at| at.elapsed() >= self.cfg.timings.status_every) {
                // A status already queued on the registry would otherwise be
                // counted after the baseline and taken for the answer. Taking
                // the queue first leaves only a status that arrives between
                // this drain and the ping, a window of a few milliseconds in
                // which a worker's own periodic status can still be mistaken
                // for the reply: it is at worst one extra five-second round
                // when it said "no sign-ins" a moment before one began.
                while let Some((wid, event)) = self.registry.try_next_event() {
                    self.background_event(wid, event).await;
                }
                last_ping = Some(Instant::now());
                ping_seq = self.rt.workers.get(&a).map_or(0, |w| w.status_seq);
                self.registry.send(a, Message::Ping);
            }
            let until = (Instant::now() + TICK).min(deadline);
            match self.wait_for(u64::MAX, until, true, never).await {
                Waited::Cancel => return SignIn::Cancel,
                Waited::Stop => return SignIn::Stop,
                Waited::AGone { why, .. } => return SignIn::AGone(why),
                _ => {}
            }
        }
    }

    // ---- the journal writes of the opening steps --------------------------

    /// J1: the switch is open at `loading`. A rollback that ends a watch
    /// window early carries the forward record's `superseded_by` in the same
    /// write (T5).
    fn write_j1(&mut self, open: &Open, superseding: Option<&str>) -> Result<(), String> {
        let admitted = self.admitted.clone().unwrap_or(Admission {
            while_off: false,
            mode_gen: 0,
        });
        let Some(journal) = self.rt.journal.as_mut() else {
            return Err("there is no journal".to_owned());
        };
        let before = journal.clone();
        // A new switch starts new ladders: what the last one counted is not
        // this one's.
        journal.ladder = None;
        journal.switch = Some(Switch {
            id: open.id.clone(),
            how: open.how,
            from: open.from.clone(),
            to: open.to.clone(),
            phase: Phase::Loading,
            started: open.started.clone(),
            watch_until: None,
            admitted: Admitted {
                while_off: admitted.while_off,
                mode_gen: admitted.mode_gen,
            },
            return_: None,
        });
        let mut carried = None;
        if let Some(forward) = superseding {
            let mut record = SwitchRecord {
                id: forward.to_owned(),
                ..SwitchRecord::default()
            };
            record.supersede(&open.id);
            let outcome = Outcome {
                record,
                mark_failed: None,
            };
            journal.push_outcome(outcome.clone());
            carried = Some(outcome);
        }
        let watch = if superseding.is_some() {
            self.rt.watch.take()
        } else {
            None
        };
        if let Err(why) = self.write_journal_or_undo(&before, "J1") {
            self.rt.watch = watch;
            return Err(why);
        }
        self.displaced_watch = watch.zip(before.switch);
        if let Some(outcome) = carried {
            self.write_state("S3", &outcome, true);
        }
        Ok(())
    }

    /// A phase change of the open switch (J2, J3).
    fn write_phase(&mut self, phase: Phase, name: &str) -> Result<(), String> {
        let Some(journal) = self.rt.journal.as_mut() else {
            return Err("there is no journal".to_owned());
        };
        let before = journal.clone();
        if let Some(switch) = journal.switch.as_mut() {
            switch.phase = phase;
        }
        self.write_journal_or_undo(&before, name)
    }

    // ---- step 3: load -----------------------------------------------------

    /// The spawn and its handshake, with the core still alive: worker events
    /// and the one-second tick are served, a second `switch` is answered
    /// "busy", a Stop ends it (A-L6, M4), and the active worker's failure ends
    /// it at once. The spawn future is owned here, across all of those; when
    /// it is dropped the driver's cleanup of a created child goes on, and
    /// the registry keeps the child as a survivor until its exit.
    async fn handshake(&mut self, b: WorkerId, request: crate::driver::SpawnRequest) -> Shaken {
        let driver = self.driver;
        let events = self.registry.events();
        let mut spawn = Box::pin(tokio::time::timeout(
            self.cfg.timings.load,
            driver.spawn(request, events),
        ));
        loop {
            self.fault_check();
            if let Some(a) = self.watch_a
                && let Some((lost, why)) = self.a_failure(a)
            {
                return Shaken::AGone { lost, why };
            }
            let tick = self.next_tick;
            tokio::select! {
                done = &mut spawn => return Shaken::Done(done),
                (wid, event) = self.registry.next_event() => {
                    // The child's own events can only be its cleanup's exit,
                    // which the registry has already noted.
                    if wid != b {
                        self.background_event(wid, event).await;
                    }
                }
                command = self.commands.recv() => match command {
                    Some(crate::core::Command::Switch { replies, .. }) => {
                        say(&replies, EndpointReply::Error {
                            v: VERSION,
                            code: None,
                            reason: "busy: switching".to_owned(),
                        });
                    }
                    Some(crate::core::Command::Stop) | None => {
                        self.rt.stop_requested = true;
                        return Shaken::Stop;
                    }
                },
                () = self.shared.cancelled() => return Shaken::Cancel,
                () = tokio::time::sleep_until(tick) => {
                    self.next_tick = Instant::now() + TICK;
                    // As in `wait_impl`: the mode first, before background
                    // work that can await an unrelated install.
                    if self.mode_cancels() {
                        return Shaken::Cancel;
                    }
                    self.tick_background().await;
                    self.publish();
                    if self.mode_cancels() {
                        return Shaken::Cancel;
                    }
                }
            }
        }
    }

    /// Waits, against Stop only and with no deadline, until the registry
    /// tracks no survivor (a child of a failed or cancelled handshake whose
    /// `Exited` has not been seen). `false` when a Stop arrived first.
    ///
    /// Boxed: a wait ticks the background, which may restart a worker, which
    /// waits here.
    pub(crate) fn wait_survivors_gone(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            while let Some(id) = self.registry.survivors().first().copied() {
                if !self.wait_exit(id).await {
                    return false;
                }
            }
            true
        })
    }

    /// After a `SurvivorPending` refusal: waits for the survivors the registry
    /// knows, or, when it knows none, a moment, and retries. `false` on Stop.
    pub(crate) fn pause_for_survivor(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            if !self.registry.survivors().is_empty() {
                return self.wait_survivors_gone().await;
            }
            // No worker id is waited for: this only polls Stop, events and timers.
            !matches!(
                self.wait_cleanup(0, Instant::now() + Duration::from_secs(1), never)
                    .await,
                Waited::Stop
            )
        })
    }

    /// Waits for the exit of the child of a spawn that ended without a port,
    /// by the registry's tracking. A deadline is never an exit. `false` when a
    /// Stop arrived first; `spawning` is kept then, for `stop_mid_switch`.
    async fn wait_survivor(&mut self, b: WorkerId) -> bool {
        // The survivor is a worker of the data dir until its exit: show it
        // (status) and list it in the journal's workers.
        self.survivors_changed();
        if !self.kill_and_wait(b).await {
            return false;
        }
        self.spawning = None;
        true
    }

    /// A load failure, unless a cancel was accepted: the handshake's wait can
    /// pick the failure (or the timeout) while a cancel is ready too, and the
    /// cancel was answered first. The window is closed under its lock, so a
    /// cancel that arrives after this is too late; the mode counts as a
    /// cancel. The switch then ends cancelled, with no record and no
    /// `mark_failed`.
    fn end_load_failed(
        &mut self,
        open: &Open,
        blame: Blame,
        why: &str,
        restart_needed: bool,
    ) -> Ended {
        if self.close_window(open) || self.mode_cancels() {
            return self.end_cancelled(open);
        }
        self.end_failed(open, Phase::Loading, blame, why, true, restart_needed)
    }

    /// Starts B and waits for `loaded`. `Err` carries how the switch ended.
    async fn load(&mut self, open: &mut Open) -> Result<WorkerId, Ended> {
        let binary = self
            .cfg
            .data
            .binary(&open.to.version, &self.cfg.binary_name);
        let (b, spawned) = loop {
            // No spawn starts while a child of an earlier failed one may live
            // (L9.15): wait for its exit, against Stop only.
            if !self.wait_survivors_gone().await {
                return Err(Ended::Stopped);
            }
            if let Some(ended) = self.a_failed_in_cleanup(open).await {
                return Err(ended);
            }
            let request = self.spawn_request(
                &open.to,
                binary.clone(),
                Role::Standby,
                Some(open.id.clone()),
            );
            let b = request.id;
            self.spawning = Some(b);
            match self.handshake(b, request).await {
                // A refusal that made no child: wait and retry. It is no
                // attempt and no version's fault.
                Shaken::Done(Ok(Err(SpawnError::SurvivorPending))) => {
                    self.spawning = None;
                    if !self.pause_for_survivor().await {
                        return Err(Ended::Stopped);
                    }
                }
                Shaken::Done(done) => break (b, done),
                // `spawning` stays: `stop_mid_switch` waits for the child's exit.
                Shaken::Stop => return Err(Ended::Stopped),
                // The spawn future was dropped by `handshake`. The child it
                // may have made is a survivor: wait for its exit, then end
                // with no record and no `mark_failed`.
                Shaken::Cancel => {
                    if !self.wait_survivor(b).await {
                        return Err(Ended::Stopped);
                    }
                    if let Some(ended) = self.a_failed_in_cleanup(open).await {
                        return Err(ended);
                    }
                    return Err(self.end_cancelled(open));
                }
                Shaken::AGone { lost, why } => {
                    if !self.wait_survivor(b).await {
                        return Err(Ended::Stopped);
                    }
                    let a = self.watch_a.unwrap_or_default();
                    return Err(self.a_gone_early(open, a, None, lost, &why).await);
                }
            }
        };
        let port = match spawned {
            Ok(Ok(port)) => port,
            // Each failure below may have left a child the registry tracks.
            // J1 is cleared and the next spawn starts only once it is gone.
            Ok(Err(SpawnError::Incompatible { needs_supervisor })) => {
                if !self.wait_survivor(b).await {
                    return Err(Ended::Stopped);
                }
                // A may have failed while the child was reaped: row d.
                if let Some(ended) = self.a_failed_in_cleanup(open).await {
                    return Err(ended);
                }
                let why = format!("the new version needs supervisor protocol {needs_supervisor}");
                return Err(self.end_load_failed(open, Blame::Environment, &why, true));
            }
            // The driver refused the binary (2.5): the environment's. Switching
            // goes off, and nothing is marked failed.
            Ok(Err(SpawnError::Untrusted(why))) => {
                if !self.wait_survivor(b).await {
                    return Err(Ended::Stopped);
                }
                if let Some(ended) = self.a_failed_in_cleanup(open).await {
                    return Err(ended);
                }
                self.refuse_untrusted(&binary, &open.to.version, &why);
                return Err(self.end_load_failed(
                    open,
                    Blame::Environment,
                    "the new version is not a trusted binary",
                    false,
                ));
            }
            Ok(Err(error)) => {
                if !self.wait_survivor(b).await {
                    return Err(Ended::Stopped);
                }
                // A may have failed while the child was reaped: row d.
                if let Some(ended) = self.a_failed_in_cleanup(open).await {
                    return Err(ended);
                }
                // A worker refused for a missing capability is a descriptive
                // `Handshake` error: the new version's, like any greeting
                // that is wrong. Only a missing file is the environment's.
                let blame = if matches!(error, SpawnError::Missing) {
                    Blame::Environment
                } else {
                    Blame::NewVersion
                };
                tracing::warn!("the new version could not start: {error}");
                let why = match &error {
                    SpawnError::Missing => "the worker binary is missing",
                    SpawnError::HashMismatch { .. } => {
                        "the worker binary no longer matches its hash"
                    }
                    SpawnError::Timeout => "the worker did not connect in time",
                    SpawnError::Handshake(reason)
                        if reason.contains("missing required capabilities") =>
                    {
                        "the worker is missing required capabilities"
                    }
                    SpawnError::Handshake(_) => "the worker's greeting failed",
                    _ => "the worker could not start",
                };
                return Err(self.end_load_failed(open, blame, why, false));
            }
            Err(_) => {
                if !self.wait_survivor(b).await {
                    return Err(Ended::Stopped);
                }
                // A may have failed while the child was reaped: row d.
                if let Some(ended) = self.a_failed_in_cleanup(open).await {
                    return Err(ended);
                }
                let why = "the new version did not start within the load timeout";
                return Err(self.end_load_failed(open, Blame::NewVersion, why, false));
            }
        };
        let tracked = Tracked::new(
            open.to.clone(),
            WorkerRole::Standby,
            &port,
            Instant::now(),
            false,
        );
        self.registry.add(port);
        self.spawning = None;
        self.rt.workers.insert(b, tracked);
        self.sync_workers();

        let deadline = Instant::now() + self.cfg.timings.load;
        let waited = self
            .wait_for(b, deadline, true, |m| {
                matches!(
                    m,
                    Message::Loaded { .. }
                        | Message::LoadFailed { .. }
                        | Message::Incompatible { .. }
                )
            })
            .await;
        let (blame, why, incompatible) = match waited {
            Waited::Msg(Message::Loaded { .. }) => return Ok(b),
            Waited::Stop => return Err(Ended::Stopped),
            Waited::AGone { lost, why } => {
                let a = self.watch_a.unwrap_or_default();
                return Err(self.a_gone_early(open, a, Some(b), lost, &why).await);
            }
            Waited::Cancel => {
                if !self.kill_and_wait(b).await {
                    return Err(Ended::Stopped);
                }
                if let Some(ended) = self.a_failed_in_cleanup(open).await {
                    return Err(ended);
                }
                return Err(self.end_cancelled(open));
            }
            Waited::Msg(Message::LoadFailed { reason }) => {
                tracing::warn!("the new version could not load: {reason}");
                (
                    Blame::NewVersion,
                    "the new version could not load".to_owned(),
                    false,
                )
            }
            Waited::Msg(Message::Incompatible { needs_supervisor }) => (
                Blame::Environment,
                format!("the new version needs supervisor protocol {needs_supervisor}"),
                true,
            ),
            Waited::Msg(_) => (Blame::NewVersion, "an unexpected message".to_owned(), false),
            Waited::PeerGone { why, .. } => (Blame::NewVersion, why, false),
            Waited::Exited(info) => (
                Blame::NewVersion,
                format!("the new version exited while loading ({})", info.text),
                false,
            ),
            Waited::Lost => (
                Blame::NewVersion,
                "the new version's control channel was lost while loading".to_owned(),
                false,
            ),
            Waited::Timeout => (
                Blame::NewVersion,
                "the new version did not report loaded within the load timeout".to_owned(),
                false,
            ),
        };
        if !self.kill_and_wait(b).await {
            return Err(Ended::Stopped);
        }
        if let Some(ended) = self.a_failed_in_cleanup(open).await {
            return Err(ended);
        }
        Err(self.end_load_failed(open, blame, &why, incompatible))
    }

    // ---- ending without a switch ------------------------------------------

    /// The switch ends `failed-to-load`: the record carries the phase and the
    /// blame; the outcome rides in the write that clears `switch` (J7, T5).
    /// `journal_open` is false when J1 itself failed, so the outcome goes
    /// straight to the state file.
    pub(crate) fn end_failed(
        &mut self,
        open: &Open,
        phase: Phase,
        blame: Blame,
        why: &str,
        journal_open: bool,
        restart_needed: bool,
    ) -> Ended {
        tracing::warn!(switch = %open.id, "the switch failed in {}: {why}", phase.as_str());
        let mut record = SwitchRecord::failed_to_load(
            &open.id,
            open.how.as_str(),
            &open.from.version,
            &open.to.version,
            &open.started,
            phase.as_str(),
            blame.as_str(),
            why,
        );
        record.restart_needed = restart_needed;
        record.ended_at = Some(self.stamp());
        if restart_needed {
            self.rt.needs_restart = true;
            // This supervisor decided it, so a replay by another version
            // can tell whose requirement the record carries.
            record.restart_needed_by = Some(self.host.own_version());
        }
        let mark_failed =
            (blame == Blame::NewVersion && open.how == How::Update).then(|| MarkFailed {
                version: open.to.version.clone(),
                why: why.to_owned(),
            });
        let outcome = Outcome {
            record,
            mark_failed,
        };
        let result = outcome_json(&outcome, None);
        if journal_open {
            if let Some(journal) = self.rt.journal.as_mut() {
                journal.push_outcome(outcome.clone());
                journal.switch = None;
            }
            self.restore_displaced_watch();
            // Rolled forward in memory if the write fails (J7).
            let _ = self.write_journal("J7");
            self.write_state("S3", &outcome, true);
        } else {
            self.write_state("S3", &outcome, false);
        }
        Ended::Result(result)
    }

    /// A failed or cancelled rollback leaves the forward worker running.
    /// Restore its window in the same write that closes the rollback. Row j
    /// instead follows T4: J-R2 confirms the returned-to artifact directly.
    fn restore_displaced_watch(&mut self) {
        let Some((mut watch, mut switch)) = self.displaced_watch.take() else {
            return;
        };
        if self.rt.active != Some(watch.b) {
            return;
        }
        watch.deadline = Instant::now() + self.cfg.timings.watch;
        watch.confirm_due = None;
        watch.confirm_attempts = 0;
        switch.watch_until = Some(self.stamp_after(self.cfg.timings.watch));
        if let Some(journal) = self.rt.journal.as_mut() {
            journal.switch = Some(switch);
        }
        self.rt.watch = Some(watch);
    }

    /// A cancel adds no record: the journal is cleared and that is all.
    pub(crate) fn end_cancelled(&mut self, open: &Open) -> Ended {
        tracing::info!(switch = %open.id, "the switch was cancelled");
        if let Some(journal) = self.rt.journal.as_mut() {
            journal.switch = None;
        }
        self.restore_displaced_watch();
        let _ = self.write_journal("J7");
        self.remove_handover_when_durable(&open.id);
        Ended::Result(json!({ "result": "cancelled", "id": open.id }))
    }

    /// Row c: A held the lease all along. Resume it. `failure` is the budget
    /// case; `None` is a cancel. If A will not resume, row l.
    async fn back_to_a(
        &mut self,
        open: &Open,
        a: WorkerId,
        failure: Option<(Blame, &str)>,
    ) -> Ended {
        match self.resume(a).await {
            Resumed::Yes => {}
            Resumed::Stop => return Ended::Stopped,
            Resumed::No => {
                // A cancel that was accepted wins (the window is closed, or
                // closes here): it ends cancelled with no record and no
                // return, and A is restarted from its own progress.
                let Some((blame, why)) = failure else {
                    self.close_window(open);
                    return self.cancelled_after_cleanup(open, a, false).await;
                };
                // A failure of B's or the budget's, with the window still open
                // (it closes at checkpoint 2, before J3): a cancel accepted
                // during the clean-up wins here too.
                if open.phase != Phase::TakingOver
                    && (self.close_window(open) || self.mode_cancels())
                {
                    return self.cancelled_after_cleanup(open, a, false).await;
                }
                return self.fresh_from(open, a, blame, why).await;
            }
        }
        self.remove_handover_when_durable(&open.id);
        match failure {
            None => self.end_cancelled(open),
            Some((blame, why)) => {
                // A resumed. A cancel accepted during the clean-up still wins
                // while the window is open (it closes at checkpoint 2).
                if open.phase != Phase::TakingOver
                    && (self.close_window(open) || self.mode_cancels())
                {
                    return self.end_cancelled(open);
                }
                self.end_failed(open, open.phase, blame, why, true, false)
            }
        }
    }

    /// Sends `resume` to A and waits for `resumed` (6.1 c). A that does not
    /// answer is lost: stopped by its handle (row l).
    async fn resume(&mut self, a: WorkerId) -> Resumed {
        self.registry.send(a, Message::Resume);
        let deadline = Instant::now() + self.cfg.timings.resume;
        match self
            .wait_for(a, deadline, false, |m| matches!(m, Message::Resumed))
            .await
        {
            Waited::Msg(_) => {
                // A may be told `peers_gone` again once B is gone (L9.15).
                if let Some(tracked) = self.rt.workers.get_mut(&a) {
                    tracked.peers_gone_sent = false;
                }
                Resumed::Yes
            }
            Waited::Stop => Resumed::Stop,
            Waited::Exited(_) => {
                self.forget_active(a);
                Resumed::No
            }
            // 6.2: a lost A gets the stop budget to exit before its handle is used.
            Waited::Lost => {
                if !self.wait_exit(a).await {
                    return Resumed::Stop;
                }
                self.forget_active(a);
                Resumed::No
            }
            // One that does not answer is told to stop, then terminated.
            Waited::Timeout | Waited::Cancel | Waited::PeerGone { .. } | Waited::AGone { .. } => {
                if !self.dismiss(a).await {
                    return Resumed::Stop;
                }
                self.forget_active(a);
                Resumed::No
            }
        }
    }

    fn forget_active(&mut self, a: WorkerId) {
        self.save_a_progress(a);
        if self.rt.active == Some(a) {
            self.rt.active = None;
        }
        self.rt.workers.remove(&a);
        self.sync_workers();
    }

    /// Row d: A exited during the hand over. B is killed, and the fresh
    /// `from` starts once A is gone (its lease is free).
    ///
    /// A cancel accepted while B was cleaned up (or the mode) is arbitrated
    /// after that clean-up, as `a_failed_in_cleanup` does: it wins, the switch
    /// ends cancelled with no record and no return, and A's exit is the
    /// ordinary exit of the active worker, restarted from the rung it was on.
    async fn a_gone(&mut self, open: &Open, b: WorkerId, why: &str) -> Ended {
        let a = self.rt.active.unwrap_or(0);
        self.forget_active(a);
        if !self.kill_and_wait(b).await {
            return Ended::Stopped;
        }
        if self.close_window(open) || self.mode_cancels() {
            return self.cancelled_after_cleanup(open, a, false).await;
        }
        self.fresh_from(open, a, Blame::Environment, why).await
    }

    /// Row d before the hand over: A exited or was lost while B loaded (or
    /// earlier). B is stopped by its handle and awaited, a lost A is awaited
    /// too (it holds the lease), and the fresh `from` starts. B is not
    /// promoted. The blame is the environment's.
    async fn a_gone_early(
        &mut self,
        open: &Open,
        a: WorkerId,
        b: Option<WorkerId>,
        lost: bool,
        why: &str,
    ) -> Ended {
        self.watch_a = None;
        if let Some(b) = b
            && self.rt.workers.contains_key(&b)
            && !self.kill_and_wait(b).await
        {
            return Ended::Stopped;
        }
        if lost && !self.wait_exit(a).await {
            return Ended::Stopped;
        }
        // A cancel accepted while B (or a lost A) was cleaned up wins over A's
        // failure, as in `a_failed_in_cleanup`; nothing is recorded.
        if self.close_window(open) || self.mode_cancels() {
            return self.cancelled_after_cleanup(open, a, lost).await;
        }
        self.forget_active(a);
        self.fresh_from(open, a, Blame::Environment, why).await
    }

    /// An accepted Stop starts the stop of every worker at once (9.1): `stop`
    /// with the budget to each, with the handle's kill due after the stop
    /// budget. Nothing waits for B's cleanup first. A worker that cannot be
    /// sent to is killed by its handle.
    fn begin_stop_all(&mut self) {
        let budget_ms = millis(self.cfg.timings.stop_message_budget());
        let kill_at = Instant::now() + self.cfg.timings.stop;
        for id in self.registry.running() {
            if !self.registry.send(id, Message::Stop { budget_ms }) {
                self.registry.kill(id);
            }
        }
        for tracked in self.rt.workers.values_mut() {
            tracked.kill_at.get_or_insert(kill_at);
        }
    }

    /// A service stop before `take_over` (9.1): the standby and every child of
    /// a failed handshake are killed by their handles and awaited, and the
    /// journal is cleared as for a cancel, with no outcome, only after every
    /// one has exited (a deadline is not an exit). From `take_over` on the
    /// journal is left for recovery.
    pub(crate) async fn stop_mid_switch(&mut self, open: &Open) {
        let before_take_over = self
            .rt
            .journal
            .as_ref()
            .and_then(|journal| journal.switch.as_ref())
            .is_some_and(|switch| {
                switch.id == open.id
                    && switch.return_.is_none()
                    && matches!(switch.phase, Phase::Loading | Phase::HandingOver)
            });
        if !before_take_over {
            return;
        }
        // The spawn the switch was waiting for: its child may have no port yet.
        if let Some(b) = self.spawning
            && !self.wait_survivor(b).await
        {
            return;
        }
        let standby: Vec<WorkerId> = self
            .rt
            .workers
            .iter()
            .filter(|(_, tracked)| tracked.role == WorkerRole::Standby)
            .map(|(id, _)| *id)
            .collect();
        for b in standby {
            if !self.kill_and_wait(b).await {
                // Killed twice without an exit: leave the journal for recovery.
                return;
            }
        }
        // Every other child the registry still tracks (a handshake survivor).
        for survivor in self.registry.survivors() {
            if !self.kill_and_wait(survivor).await {
                return;
            }
        }
        if let Some(journal) = self.rt.journal.as_mut() {
            journal.switch = None;
        }
        self.restore_displaced_watch();
        let _ = self.write_journal("J7");
    }

    /// Rows d and l end the switch as `failed-to-load` and start the fresh
    /// `from` (J-R1, then J-R2 once it runs).
    ///
    /// The restart goes on from A's own progress, read before A was
    /// forgotten (round 8, item 4).
    async fn fresh_from(&mut self, open: &Open, a: WorkerId, blame: Blame, why: &str) -> Ended {
        let progress = self.a_progress_of(a);
        self.rt.active = None;
        self.remove_handover_when_durable(&open.id);
        let ended_phase = open.phase;
        self.begin_return(open.clone(), blame, why.to_owned(), Some(progress))
            .await;
        let returned = self.rt.returning.is_none();
        Ended::Result(json!({
            "result": "failed-to-load",
            "id": open.id,
            "phase": ended_phase.as_str(),
            "blame": blame.as_str(),
            "why": why,
            "returned": returned,
        }))
    }

    // ---- row e ------------------------------------------------------------

    /// B did not take over (`take_failed`, an exit, a lost channel, or no
    /// `ready`). After `take_failed` B is stopped; after a `ready` timeout its
    /// gate may be open, so it hands over, or is dismissed, before A resumes.
    async fn take_over_failed(
        &mut self,
        open: &Open,
        a: WorkerId,
        b: WorkerId,
        waited: Waited,
    ) -> Ended {
        let (blame, why) = match &waited {
            Waited::Msg(Message::TakeFailed { reason }) => {
                // L9.1: only a reason that starts `lease:` is the lease conflict; a
                // missing or corrupt handover (`handover:`) or anything else is the
                // new version's.
                let lease = reason.starts_with("lease:");
                tracing::warn!("the new version could not take over: {reason}");
                (
                    if lease {
                        Blame::Environment
                    } else {
                        Blame::NewVersion
                    },
                    if lease {
                        "the new version could not take over: the worker lease conflicts".to_owned()
                    } else {
                        "the new version could not take over".to_owned()
                    },
                )
            }
            Waited::Exited(info) => (
                Blame::NewVersion,
                format!("the new version exited while taking over ({})", info.text),
            ),
            Waited::Lost => (
                Blame::NewVersion,
                "the new version's control channel was lost while taking over".to_owned(),
            ),
            _ => (
                Blame::NewVersion,
                "the new version was not ready within the ready timeout".to_owned(),
            ),
        };
        // The blame of a hand over that fails after a `ready` timeout is the new
        // version's too (L9.16).
        match &waited {
            Waited::Exited(_) => {}
            Waited::Lost => {
                if !self.wait_exit(b).await {
                    return Ended::Stopped;
                }
            }
            Waited::Msg(Message::TakeFailed { .. }) => {
                // L9.16: B's gate never opened (L9.1 opens it only with
                // `ready`), so there is nothing to hand over. Dismiss it.
                if !self.dismiss(b).await {
                    return Ended::Stopped;
                }
            }
            _ => {
                // A `ready` timeout: B may have started to serve. It hands over
                // what it holds; if that fails it is never resumed, only
                // dismissed (L9.16).
                self.registry.send(
                    b,
                    Message::HandOver {
                        budget_ms: millis(self.cfg.timings.handover),
                    },
                );
                let deadline = Instant::now() + self.cfg.timings.handover;
                match self
                    .wait_for(b, deadline, false, |m| {
                        matches!(m, Message::HandedOver { .. })
                    })
                    .await
                {
                    Waited::Msg(_) => self.release_as_drain(b),
                    Waited::Stop => return Ended::Stopped,
                    Waited::Exited(_) => {}
                    Waited::Lost
                    | Waited::Timeout
                    | Waited::Cancel
                    | Waited::PeerGone { .. }
                    | Waited::AGone { .. } => {
                        if !self.dismiss(b).await {
                            return Ended::Stopped;
                        }
                    }
                }
            }
        }
        match self.resume(a).await {
            Resumed::Yes => {}
            Resumed::Stop => return Ended::Stopped,
            // Row e followed by row l keeps B's blame, and so its mark (A-L1).
            Resumed::No => return self.fresh_from(open, a, blame, &why).await,
        }
        // `handover.json` goes only now, with A back or gone.
        self.remove_handover_when_durable(&open.id);
        self.end_failed(open, Phase::TakingOver, blame, &why, true, false)
    }

    /// Dismisses B without resuming it: `stop`, up to the stop budget for its
    /// exit, then its handle (it may hold the lease). `false` when a service
    /// stop arrived first.
    async fn dismiss(&mut self, b: WorkerId) -> bool {
        if !self.rt.workers.contains_key(&b) {
            return true;
        }
        let budget = self.cfg.timings.stop;
        self.registry.send(
            b,
            Message::Stop {
                budget_ms: millis(budget.saturating_sub(Duration::from_secs(5))),
            },
        );
        match self.wait_cleanup(b, Instant::now() + budget, never).await {
            Waited::Exited(_) => true,
            Waited::Stop => false,
            _ => self.kill_and_wait(b).await,
        }
    }

    /// B handed over after a failed take over: `release` it so it drains what
    /// it accepted, as a worker that leaves.
    fn release_as_drain(&mut self, b: WorkerId) {
        self.registry.send(b, Message::Release);
        let drain = self.cfg.timings.drain;
        if let Some(tracked) = self.rt.workers.get_mut(&b) {
            tracked.role = WorkerRole::Draining;
            tracked.drain = Some(Drain {
                since: Instant::now(),
                deadline: Instant::now() + drain,
                record: None,
                pinged: false,
                stop_at: None,
                stopped: false,
                cut: 0,
            });
        }
        self.sync_workers();
    }

    // ---- step 9: the commit point -----------------------------------------

    fn commit(&mut self, open: &mut Open, a: WorkerId, b: WorkerId, gap_ms: u64) -> Ended {
        open.phase = Phase::Watching;
        open.reached_ready = true;
        let watch_until = self.stamp_after(self.cfg.timings.watch);
        if let Some(journal) = self.rt.journal.as_mut() {
            journal.active = open.to.clone();
            if let Some(switch) = journal.switch.as_mut() {
                switch.phase = Phase::Watching;
                switch.watch_until = Some(watch_until);
            }
        }
        // J4. A failure rolls forward in memory: the disk still says
        // `taking-over`, which a restart reverts to `from`.
        let _ = self.write_journal("J4");
        let mut record = SwitchRecord::started(
            &open.id,
            open.how.as_str(),
            &open.from.version,
            &open.to.version,
            &open.started,
        );
        record.gap_ms = Some(gap_ms);
        let outcome = Outcome {
            record,
            mark_failed: None,
        };
        self.write_state("S1", &outcome, false);

        let drain = self.cfg.timings.drain;
        if let Some(tracked) = self.rt.workers.get_mut(&b) {
            tracked.role = WorkerRole::Active;
            tracked.wants_peers_gone = true;
        }
        if let Some(tracked) = self.rt.workers.get_mut(&a) {
            tracked.role = WorkerRole::Draining;
            tracked.drain = Some(Drain {
                since: Instant::now(),
                deadline: Instant::now() + drain,
                record: Some(open.record()),
                pinged: false,
                stop_at: None,
                stopped: false,
                cut: 0,
            });
        }
        self.rt.active = Some(b);
        self.rt.run = Some(Run {
            binary: self
                .cfg
                .data
                .binary(&open.to.version, &self.cfg.binary_name),
            artifact: open.to.clone(),
        });
        self.rt.degraded = None;
        self.sync_workers();
        self.registry.send(b, Message::Go);
        self.registry.send(a, Message::Release);
        self.rt.watch = Some(Watch {
            open: open.clone(),
            b,
            deadline: Instant::now() + self.cfg.timings.watch,
            confirm_due: None,
            confirm_attempts: 0,
        });
        Ended::Result(json!({
            "result": if open.how == How::Rollback { "rolled-back" } else { "switched" },
            "id": open.id,
            "phase": "watching",
            "gap_ms": gap_ms,
        }))
    }
}

pub(crate) enum SignIn {
    AGone(String),
    Go,
    Cancel,
    Off,
    Stop,
}

enum Resumed {
    Yes,
    No,
    Stop,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn cancelled(id: &str, why: Option<&str>) -> Ended {
    Ended::Result(json!({ "result": "cancelled", "id": id, "why": why }))
}

/// What `done` shows of an ended switch's record.
fn outcome_json(outcome: &Outcome, returned_to: Option<&str>) -> Value {
    let mut value = serde_json::to_value(&outcome.record).unwrap_or(Value::Null);
    if let (Some(returned_to), Some(object)) = (returned_to, value.as_object_mut()) {
        object.insert("returned_to".to_owned(), json!(returned_to));
    }
    value
}
