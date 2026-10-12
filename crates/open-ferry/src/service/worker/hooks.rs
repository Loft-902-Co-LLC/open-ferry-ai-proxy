//! Not upstream's: the worker's one view of the cross-process test seams
//! (L7). With the `blue-green-test` feature it forwards to `crate::seams`;
//! without it every call does nothing, and the names the seams go by are
//! not in the binary: they are in `seams.rs`, which isn't built.

use std::io;
use std::path::Path;
#[cfg(any(feature = "blue-green-test", test))]
use std::sync::Arc;

/// The fault kinds of L9.10.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    FailLoad,
    FailBeforeReady,
    #[cfg_attr(
        not(feature = "blue-green-test"),
        allow(dead_code, reason = "only the feature's seams name it")
    )]
    ExitAfter,
    SlowReady,
    IgnoreHandover,
    HoldRefresh,
    #[cfg_attr(
        not(feature = "blue-green-test"),
        allow(dead_code, reason = "only the feature's seams name it")
    )]
    DropChannel,
}

/// The barriers of L7.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Barrier {
    GateClosing,
    RefreshBeforeSave,
    GateClosed,
    LeaseRelease,
    BeforeReload,
}

/// The events of L9.12 (the barriers' arrivals are logged by the barrier).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Loaded,
    Active,
    Standby,
    HandoverWritten,
    HandoverFailed,
    HandedOver,
    Resumed,
    Ready,
    Go,
    Draining,
    Drained,
    PeersGone,
}

/// The points `exit-after` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    Welcome,
    Loaded,
    HandedOver,
    Ready,
    Go,
}

/// The places `drop-channel` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    TakingOver,
    Watching,
    HandingOver,
}

impl Point {
    #[cfg(feature = "blue-green-test")]
    fn value(self) -> &'static str {
        match self {
            Self::Welcome => "welcome",
            Self::Loaded => "loaded",
            Self::HandedOver => "handed_over",
            Self::Ready => "ready",
            Self::Go => "go",
        }
    }
}

impl Place {
    #[cfg(feature = "blue-green-test")]
    fn value(self) -> &'static str {
        match self {
            Self::TakingOver => "taking-over",
            Self::Watching => "watching",
            Self::HandingOver => "handing-over",
        }
    }
}

/// A worker's seams. Cloning gives another handle to the same files.
#[derive(Clone, Debug, Default)]
pub(crate) struct Hooks {
    #[cfg(feature = "blue-green-test")]
    seams: Option<Arc<crate::seams::Seams>>,
    /// Where a test's worker records the points it would have exited at: a
    /// test can't survive `process::exit`.
    #[cfg(all(feature = "blue-green-test", test))]
    exits: Arc<std::sync::Mutex<Vec<&'static str>>>,
    /// Holds one mutating step of the hand over inside its blocking closure.
    #[cfg(test)]
    pub(crate) step_hold: Option<Arc<StepHold>>,
}

/// A test's hold on one blocking step of the hand over: the step reports
/// that it arrived, then waits until the test releases it.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct StepHold {
    step: &'static str,
    arrived: std::sync::Mutex<std::sync::mpsc::Sender<()>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
impl StepHold {
    /// A hold on `step`, the receiver that gets a message when the step
    /// arrives, and the sender that releases it.
    pub(crate) fn new(
        step: &'static str,
    ) -> (
        Arc<Self>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (arrived, arrival) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let hold = Arc::new(Self {
            step,
            arrived: std::sync::Mutex::new(arrived),
            release: std::sync::Mutex::new(released),
        });
        (hold, arrival, release)
    }
}

#[cfg(test)]
impl Hooks {
    /// Called inside a blocking step: waits there when a test holds `step`.
    /// A released or dropped hold lets the step go.
    pub(crate) fn hold_step(&self, step: &str) {
        let Some(hold) = self.step_hold.as_ref().filter(|hold| hold.step == step) else {
            return;
        };
        let _ = hold
            .arrived
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send(());
        let _ = hold
            .release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv();
    }
}

#[cfg(feature = "blue-green-test")]
impl Hooks {
    /// The seams in `data_dir`, the supervisor's. A directory that can't be
    /// made leaves the worker without seams, and says so.
    pub(crate) fn in_data_dir(data_dir: &Path) -> Self {
        match crate::seams::Seams::new(data_dir) {
            Ok(seams) => Self {
                seams: Some(Arc::new(seams)),
                #[cfg(test)]
                exits: Arc::default(),
                #[cfg(test)]
                step_hold: None,
            },
            Err(error) => {
                tracing::error!("the test seams can't start: {error}");
                Self::default()
            }
        }
    }

    /// Like [`in_data_dir`](Self::in_data_dir), with `marker` as the
    /// worker's.
    #[cfg(test)]
    pub(crate) fn in_data_dir_marked(data_dir: &Path, marker: &str) -> Self {
        Self {
            seams: crate::seams::Seams::with_marker(data_dir, marker.to_owned())
                .ok()
                .map(Arc::new),
            ..Self::default()
        }
    }

    /// The points `exit-after` would have exited at, in order.
    #[cfg(test)]
    pub(crate) fn exits(&self) -> Vec<&'static str> {
        self.exits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The fault `fault`'s file holds for this worker: its value, empty for
    /// a presence fault.
    pub(crate) fn fault(&self, fault: Fault) -> Option<String> {
        self.seams.as_ref()?.fault(fault)
    }

    pub(crate) fn event(&self, event: Event, lease: bool) {
        if let Some(seams) = &self.seams {
            seams.event(crate::seams::event_name(event), lease);
        }
    }

    /// The name of `fault` when it is set, which is the reason a step it
    /// fails gives.
    pub(crate) fn failure(&self, fault: Fault) -> Option<String> {
        self.fault(fault)
            .map(|_| crate::seams::fault_name(fault).to_owned())
    }

    /// Adds the test marker to every response of `router`.
    pub(crate) fn mark(&self, router: axum::Router) -> axum::Router {
        match &self.seams {
            Some(seams) => crate::seams::mark(router, seams.marker()),
            None => router,
        }
    }

    /// Exits at once, no cleanup, with 42, at `point`, when the fault says.
    pub(crate) fn exit_after(&self, point: Point) {
        if self.fault(Fault::ExitAfter).as_deref() == Some(point.value()) {
            #[cfg(test)]
            self.exits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(point.value());
            #[cfg(not(test))]
            std::process::exit(42);
        }
    }

    /// Whether the `drop-channel` fault names `place`.
    pub(crate) fn drops_channel_at(&self, place: Place) -> bool {
        self.fault(Fault::DropChannel).as_deref() == Some(place.value())
    }

    /// Logs the arrival at `barrier` and waits there, off the runtime's
    /// threads. Dropping the future leaves the blocking poll to finish alone.
    pub(crate) async fn barrier(&self, barrier: Barrier, lease: bool) -> io::Result<()> {
        let Some(seams) = self.seams.clone() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || seams.barrier_blocking(barrier, lease))
            .await
            .map_err(io::Error::other)?
    }

    pub(crate) fn clear_arrivals(&self, barriers: &[Barrier]) {
        if let Some(seams) = &self.seams {
            seams.clear_arrivals(barriers);
        }
    }
}

#[cfg(not(feature = "blue-green-test"))]
impl Hooks {
    pub(crate) fn in_data_dir(_data_dir: &Path) -> Self {
        Self::default()
    }

    pub(crate) fn fault(&self, _fault: Fault) -> Option<String> {
        None
    }

    pub(crate) fn event(&self, _event: Event, _lease: bool) {}

    pub(crate) fn failure(&self, _fault: Fault) -> Option<String> {
        None
    }

    pub(crate) fn mark(&self, router: axum::Router) -> axum::Router {
        router
    }

    pub(crate) fn exit_after(&self, _point: Point) {}

    pub(crate) fn drops_channel_at(&self, _place: Place) -> bool {
        false
    }

    #[allow(clippy::unused_async, reason = "the same call as the feature's")]
    pub(crate) async fn barrier(&self, _barrier: Barrier, _lease: bool) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn clear_arrivals(&self, _barriers: &[Barrier]) {}
}
