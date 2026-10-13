//! Not upstream's: the supervisor's life after startup (design v3 9, lead answer 1).
//!
//! [`supervise`] starts the supervisor, serves the endpoint, and runs the
//! core. A panic in the core (or anywhere in the future it runs in) is
//! caught, logged at error, and answered by [`startup::reenter`](crate::startup::reenter): every worker
//! stops, the in-memory state is dropped, and recovery runs again over the
//! disk, with the lock, the listening sockets and the endpoint kept. After
//! [`ReentryLimit`] allows no more (3 in 10 minutes), the workers are stopped
//! and `Exit::TooManyPanics` is returned, which the caller turns into a
//! non-zero exit.
//!
//! 2c must run this on a multi-thread runtime: the serialized journal fsyncs
//! are synchronous, while endpoint status and cancel must keep running.

use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

use crate::clock::Clock;
use crate::core::{Command, Core, Exit, Runtime};
use crate::disk::RealDisk;
use crate::driver::{Driver, Registry};
use crate::host::Host;
use crate::reentry::{CatchUnwind, ReentryLimit};
use crate::serve::{Serve, accept_loop};
use crate::snapshot::{Shared, Snapshot};
use crate::startup::{Config, EarlyCommands, StartupError, reenter, start_watching};
use crate::timings::{PANIC_REENTRIES, PANIC_WINDOW};

/// The limit the supervisor runs with: [`PANIC_REENTRIES`] in [`PANIC_WINDOW`].
pub fn default_limit() -> ReentryLimit {
    ReentryLimit::new(PANIC_REENTRIES, PANIC_WINDOW)
}

/// What the endpoint shows between a panic and the new core's first picture.
pub fn restarting_snapshot() -> Snapshot {
    Snapshot {
        state: "degraded".to_owned(),
        degraded: Some("the supervisor is starting over after an internal error".to_owned()),
        ..Snapshot::default()
    }
}

/// The core panicked: its picture (a switch, workers) is stale until the new
/// one publishes its own (A-L8), and the cancel it accepted belonged to an
/// operation that is gone. Both go in one step, so no cancel is accepted for
/// the dead operation in between.
fn forget_the_old_picture(shared: &Shared) {
    shared.restart_over(restarting_snapshot());
}

/// Runs `run` (the core) to its end while watching the endpoint task on its
/// own: when the task ends that is logged at error and shown as degraded, and
/// the core and the workers go on. Starting the endpoint again is not done
/// here. `ended` stays set once the end was seen, so the finished handle is
/// not polled again.
async fn watch_endpoint<T>(
    run: impl std::future::Future<Output = T>,
    serving: &mut Option<tokio::task::JoinHandle<()>>,
    ended: &mut bool,
    shared: &Shared,
) -> T {
    let mut run = std::pin::pin!(run);
    loop {
        tokio::select! {
            outcome = &mut run => return outcome,
            result = async {
                match serving.as_mut() {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            }, if !*ended => {
                *ended = true;
                let reason = match result {
                    Ok(()) => "the accept loop returned".to_owned(),
                    Err(error) if error.is_panic() => "the accept loop panicked".to_owned(),
                    Err(error) => format!("the accept loop was cancelled: {error}"),
                };
                tracing::error!(
                    "the endpoint task has ended ({reason}): status and switch requests are no longer served"
                );
                shared.set_endpoint_down(reason);
            }
        }
    }
}

/// Runs the supervisor until it is told to stop (`Exit::Stopped`) or panics
/// too often (`Exit::TooManyPanics`). `commands` is the receiving end of the
/// channel whose sender is `command_tx`; the caller keeps a clone of the
/// sender to send `Command::Stop` on a service stop.
pub async fn supervise<H: Host, D: Driver>(
    cfg: &Config,
    host: &H,
    driver: &D,
    clock: &dyn Clock,
    command_tx: UnboundedSender<Command>,
    mut commands: UnboundedReceiver<Command>,
    mut limit: ReentryLimit,
) -> Result<Exit, StartupError> {
    let mut registry = Registry::new();
    // A stop while startup waits for the update lock ends it before any
    // worker exists; a command that is not a stop is kept for the core.
    let mut early = EarlyCommands::new(&mut commands);
    let started = start_watching(cfg, host, driver, &mut registry, clock, Some(&mut early)).await;
    let kept = early.into_kept();
    let mut started = match started {
        Ok(started) => started,
        Err(StartupError::Stopped) => return Ok(Exit::Stopped),
        Err(error) => return Err(error),
    };
    // The core reads the kept commands first, then whatever the old queue
    // still gets, in the order sent: they are not sent back through a queue
    // that others write to.
    let (mut commands, forwarder) = core_input(kept, commands);
    let forwarder = AbortOnDrop(forwarder);
    let shared = Shared::new();
    let mut serving = started.endpoint.take().map(|server| {
        tokio::spawn(accept_loop(
            server,
            Serve::new(command_tx.clone(), shared.clone()),
        ))
    });
    // Set once the endpoint task's end was seen: its handle is not polled again.
    let mut endpoint_ended = serving.is_none();
    let disk = RealDisk::new(
        cfg.data.clone(),
        cfg.installed.clone(),
        cfg.binary_name.clone(),
        started.store.clone(),
    )
    .with_trust(cfg.kind)
    .with_supervisor_version(host.own_version())
    .with_install_lock(started.install_lock.clone());
    let exit = loop {
        let mut runtime = Runtime::from_started(&started, &registry, cfg, clock, Instant::now());
        // After recovery: a restart need that this supervisor version set
        // still holds; one an older version set is cleared (the restart came).
        runtime.needs_restart = needs_restart_after_recovery(cfg, host, &started);
        let outcome = {
            let mut core = Core::new(
                cfg,
                driver,
                host,
                &disk,
                clock,
                &mut registry,
                shared.clone(),
                &mut commands,
                runtime,
            );
            watch_endpoint(
                CatchUnwind::new(core.run()),
                &mut serving,
                &mut endpoint_ended,
                &shared,
            )
            .await
        };
        match outcome {
            Ok(exit) => break exit,
            Err(message) => {
                tracing::error!("the supervisor panicked and starts over: {message}");
                forget_the_old_picture(&shared);
                if serving.as_ref().is_some_and(|task| task.is_finished()) {
                    tracing::error!(
                        "the endpoint task has ended: status and switch requests are no longer served"
                    );
                }
                if !limit.allow(Instant::now()) {
                    tracing::error!("too many panics in a short time: the supervisor exits");
                    registry
                        .stop_all(cfg.timings.stop, Duration::from_secs(2))
                        .await;
                    registry.wait_until_empty().await;
                    break Exit::TooManyPanics;
                }
                if let Err(error) =
                    reenter(cfg, host, driver, &mut registry, clock, &mut started).await
                {
                    // The disk can't be read: nothing can be recovered, and
                    // starting over would only repeat this.
                    tracing::error!("re-entry failed: {error}");
                    break Exit::TooManyPanics;
                }
            }
        }
    };
    if let Some(serving) = serving {
        serving.abort();
    }
    forwarder.abort();
    Ok(exit)
}

/// Aborts its task when dropped, so cancelling `supervise` or an unwind out
/// of it does not leave the forwarder holding the command receiver.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl AbortOnDrop {
    fn abort(&self) {
        self.0.abort();
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The core's command input: `kept` first, then everything that arrives on
/// `commands`, in order. The returned task forwards from `commands`; it runs
/// until aborted.
fn core_input(
    kept: Vec<Command>,
    mut commands: UnboundedReceiver<Command>,
) -> (UnboundedReceiver<Command>, tokio::task::JoinHandle<()>) {
    let (input, output) = tokio::sync::mpsc::unbounded_channel();
    for command in kept {
        let _ = input.send(command);
    }
    let forwarder = tokio::spawn(async move {
        while let Some(command) = commands.recv().await {
            if input.send(command).is_err() {
                break;
            }
        }
    });
    (output, forwarder)
}

/// The restart need at the start of a pass: one an older supervisor version set
/// is cleared in the file (the restart came). With switching off nothing is
/// written, so the check is read-only (`statefile::restart_flag_read_only`).
pub(crate) fn needs_restart_after_recovery<H: Host>(
    cfg: &Config,
    host: &H,
    started: &crate::startup::Started,
) -> bool {
    if started.switched_off {
        crate::statefile::restart_flag_read_only(&cfg.data, &host.own_version())
    } else {
        crate::statefile::restart_flag_at_start(&cfg.data, &host.own_version())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn switch_to(to: &str) -> Command {
        use crate::core::SwitchRequest;
        use crate::protocol::Admission;
        use open_ferry_update::journal::How;
        let (replies, _) = tokio::sync::mpsc::unbounded_channel();
        Command::Switch {
            request: SwitchRequest {
                to: to.to_owned(),
                sha256: String::new(),
                pin: String::new(),
                how: How::Update,
                auto: false,
                admitted: Admission {
                    while_off: false,
                    mode_gen: 0,
                },
            },
            replies,
        }
    }

    fn target(command: Option<Command>) -> String {
        match command {
            Some(Command::Switch { request, .. }) => request.to,
            Some(Command::Stop) => "stop".to_owned(),
            None => "none".to_owned(),
        }
    }

    /// A sender writes between the extraction of the kept commands and the
    /// core's start: the kept ones are still read first, then the new one,
    /// then those sent later, in order.
    #[tokio::test]
    async fn kept_commands_reach_the_core_ahead_of_everything_sent_after_them() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let kept = vec![switch_to("a"), switch_to("b")];
        tx.send(switch_to("c")).unwrap();
        let (mut input, forwarder) = core_input(kept, rx);
        tx.send(switch_to("d")).unwrap();
        let mut seen = Vec::new();
        for _ in 0..4 {
            let next = tokio::time::timeout(Duration::from_secs(5), input.recv())
                .await
                .expect("a command");
            seen.push(target(next));
        }
        assert_eq!(seen, ["a", "b", "c", "d"]);
        forwarder.abort();
    }

    #[tokio::test]
    async fn the_forwarder_is_aborted_when_its_guard_is_dropped() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (_input, forwarder) = core_input(Vec::new(), rx);
        let probe = forwarder.abort_handle();
        let guard = AbortOnDrop(forwarder);
        tokio::task::yield_now().await;
        assert!(!probe.is_finished(), "the forwarder waits for commands");
        drop(guard);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !probe.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the guard aborts the forwarder");
    }

    /// When startup took nothing, the commands left in the receiver reach the
    /// core in the order they were sent.
    #[tokio::test]
    async fn commands_left_in_the_receiver_reach_the_core_in_order() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(switch_to("a")).unwrap();
        tx.send(switch_to("b")).unwrap();
        let (mut input, forwarder) = core_input(Vec::new(), rx);
        tx.send(Command::Stop).unwrap();
        let mut seen = Vec::new();
        for _ in 0..3 {
            let next = tokio::time::timeout(Duration::from_secs(5), input.recv())
                .await
                .expect("a command");
            seen.push(target(next));
        }
        assert_eq!(seen, ["a", "b", "stop"]);
        forwarder.abort();
    }

    #[test]
    fn the_snapshot_between_a_panic_and_the_new_core_shows_no_switch_and_says_why() {
        let shared = Shared::new();
        shared.publish(Snapshot {
            state: "switching".to_owned(),
            switch: Some(crate::snapshot::SwitchView::default()),
            ..Snapshot::default()
        });
        shared.publish(restarting_snapshot());
        let shown = shared.snapshot();
        assert_eq!(shown.state, "degraded");
        assert!(shown.switch.is_none());
        assert!(shown.degraded.is_some());
    }

    #[test]
    fn a_panic_retires_the_cancel_and_the_old_picture_in_one_step() {
        use crate::snapshot::{CancelAnswer, SwitchView};
        let shared = Shared::new();
        shared.publish(Snapshot {
            state: "switching".to_owned(),
            switch: Some(SwitchView {
                id: "dead".to_owned(),
                status: "loading".to_owned(),
                cancellable: true,
                ..SwitchView::default()
            }),
            ..Snapshot::default()
        });
        assert!(matches!(shared.request_cancel(), CancelAnswer::Cancelled));
        // Inside the recovery step the snapshot mutex that `request_cancel`
        // needs is held, and the cancel is already gone: nothing can be
        // accepted for the dead operation there.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (hooked, record) = (shared.clone(), seen.clone());
        shared.on_restart_gap(move || {
            *record.lock().unwrap() =
                Some((hooked.snapshot_lock_is_held(), hooked.cancel_requested()));
        });
        forget_the_old_picture(&shared);
        assert_eq!(
            *seen.lock().unwrap(),
            Some((true, false)),
            "recovery's step has a gap in which a request is accepted"
        );
        assert!(matches!(shared.request_cancel(), CancelAnswer::Nothing));
        assert!(!shared.cancel_requested());
        assert!(shared.snapshot().switch.is_none());
        assert_eq!(shared.snapshot().state, "degraded");
    }

    #[tokio::test(start_paused = true)]
    async fn an_endpoint_task_that_ends_is_shown_as_degraded_while_the_core_runs_on() {
        use crate::testkit::{Rig, core_of};
        use std::time::Duration;
        let mut rig = Rig::new().await;
        let command_tx = rig.command_tx.clone();
        let shared = rig.shared.clone();
        let mut core = core_of!(rig);
        // The endpoint task ends by itself after 2 s.
        let mut serving = Some(tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }));
        let mut ended = false;
        let script = async {
            tokio::time::sleep(Duration::from_secs(4)).await;
            // Detected promptly, with its reason, and the core is still up.
            let shown = shared.snapshot();
            let stopped = command_tx.send(crate::core::Command::Stop);
            (shown, stopped)
        };
        let (outcome, (shown, _)) = tokio::join!(
            watch_endpoint(core.run(), &mut serving, &mut ended, &shared),
            script
        );
        assert_eq!(outcome, Exit::Stopped, "the core was not stopped by it");
        assert!(ended);
        assert_eq!(shown.state, "degraded");
        assert!(
            shown
                .degraded
                .as_deref()
                .is_some_and(|why| why.contains("endpoint") && why.contains("accept loop returned")),
            "{shown:?}"
        );
        assert!(!shown.workers.is_empty(), "the workers went on");
    }
}
