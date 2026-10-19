//! Test-only observations: never wait for the work being diagnosed.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

tokio::task_local! {
    pub(super) static TRACE: Arc<Trace>;
}

#[derive(Default)]
pub(super) struct Trace {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    stages: Vec<(&'static str, Instant)>,
    stop_requested: Option<Instant>,
    ledger_dir: Option<std::path::PathBuf>,
    ledger_join: Option<&'static str>,
}

impl Trace {
    pub(super) fn record(&self, stage: &'static str) {
        self.state
            .lock()
            .unwrap()
            .stages
            .push((stage, Instant::now()));
    }

    pub(super) fn ledger_join(&self, state: &'static str) {
        self.state.lock().unwrap().ledger_join = Some(state);
    }

    pub(super) fn stop_requested(&self) {
        // This event comes from the test task. Keep the service's current
        // stage intact if it is still blocked setting up the watcher.
        self.state.lock().unwrap().stop_requested = Some(Instant::now());
    }

    pub(super) fn ledger_started(&self, dir: &Option<std::path::PathBuf>) {
        self.state.lock().unwrap().ledger_dir.clone_from(dir);
        self.record("start ledger in background");
    }
}

impl fmt::Display for Trace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock().unwrap();
        let now = Instant::now();
        writeln!(
            f,
            "last stage: {:?}",
            state.stages.last().map(|(stage, _)| stage)
        )?;
        for (i, (stage, started)) in state.stages.iter().enumerate() {
            let end = state.stages.get(i + 1).map_or(now, |(_, at)| *at);
            writeln!(f, "{stage}: {:?}", end.duration_since(*started))?;
        }
        writeln!(
            f,
            "stop requested {:?} ago",
            state.stop_requested.map(|at| now.duration_since(at))
        )?;
        writeln!(f, "ledger directory: {:?}", state.ledger_dir)?;
        writeln!(f, "ledger shutdown join: {:?}", state.ledger_join)?;
        // Ledger's native JoinHandle is private in open-ferry-dashboard.
        // A completed join proves exit; a pending join doesn't prove life
        // (it might still be queued in Tokio's blocking pool).
        write!(
            f,
            "ledger writer alive: {}",
            match state.ledger_join {
                Some("joined") => "no (join completed)",
                _ => "unknown (native thread is private)",
            }
        )
    }
}

pub(super) fn current() -> Option<Arc<Trace>> {
    TRACE.try_with(Arc::clone).ok()
}

pub(super) fn record(stage: &'static str) {
    let _ = TRACE.try_with(|trace| trace.record(stage));
}

/// Created before the service, so it also observes completion of the
/// service's synchronous destructors on every return path.
pub(super) struct RunEnd(pub(super) Option<Arc<Trace>>);

impl Drop for RunEnd {
    fn drop(&mut self) {
        if let Some(trace) = &self.0 {
            trace.record("run future ended (locals dropped)");
        }
    }
}
