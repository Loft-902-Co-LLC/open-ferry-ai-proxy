//! Not upstream's: injectable log switches for a blue-green worker.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};

use open_ferry_core::observe::request_log::CleaningControls;

#[derive(Debug)]
pub(super) struct Cleaning {
    pub enabled: Mutex<bool>,
    pub changed: Condvar,
    #[cfg(test)]
    pub observed: Mutex<(u64, u64, bool)>,
    #[cfg(test)]
    pub generation: AtomicU64,
    #[cfg(test)]
    pub deletion: Mutex<Option<Arc<Pass>>>,
    #[cfg(test)]
    pub observed_changed: Condvar,
    #[cfg(test)]
    pub pass: Mutex<Option<Arc<Pass>>>,
}

impl Default for Cleaning {
    fn default() -> Self {
        Self {
            enabled: Mutex::new(true),
            changed: Condvar::new(),
            #[cfg(test)]
            observed: Mutex::new((0, 0, true)),
            #[cfg(test)]
            generation: AtomicU64::new(0),
            #[cfg(test)]
            deletion: Mutex::new(None),
            #[cfg(test)]
            observed_changed: Condvar::new(),
            #[cfg(test)]
            pass: Mutex::new(None),
        }
    }
}

impl Cleaning {
    pub fn set(&self, enabled: bool) -> bool {
        let previous = std::mem::replace(
            &mut *self.enabled.lock().unwrap_or_else(PoisonError::into_inner),
            enabled,
        );
        self.changed.notify_all();
        previous
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> u64 {
        self.observed.lock().unwrap().1
    }

    #[cfg(test)]
    pub fn observe(&self, generation: u64, enabled: bool) {
        let mut observed = self.observed.lock().unwrap();
        if generation == self.generation.load(Ordering::Acquire) {
            observed.0 = generation;
            observed.1 += 1;
            observed.2 = enabled;
            self.observed_changed.notify_all();
        }
    }

    #[cfg(test)]
    pub fn wait_observed(&self, snapshot: u64) {
        self.wait_observed_state(snapshot, None);
    }

    #[cfg(test)]
    pub fn wait_observed_enabled(&self, snapshot: u64, enabled: bool) {
        self.wait_observed_state(snapshot, Some(enabled));
    }

    #[cfg(test)]
    fn wait_observed_state(&self, snapshot: u64, enabled: Option<bool>) {
        let generation = self.generation.load(Ordering::Acquire);
        let seen = self.observed.lock().unwrap();
        let (seen, timeout) = self
            .observed_changed
            .wait_timeout_while(seen, std::time::Duration::from_secs(5), |seen| {
                seen.0 != generation
                    || seen.1 <= snapshot
                    || enabled.is_some_and(|enabled| seen.2 != enabled)
            })
            .unwrap();
        assert!(
            !timeout.timed_out()
                && seen.0 == generation
                && seen.1 > snapshot
                && enabled.is_none_or(|enabled| seen.2 == enabled),
            "cleaner did not observe controls after snapshot"
        );
    }
}

/// Not upstream's: one worker's controls. Clones share state; `new` and
/// `Default` create private enabled handles, preserving ordinary-run defaults.
/// `FileLog` and core's `CleaningControls` instead default to the process
/// handle for legacy callers; worker controls must never share that handle.
///
/// Every supervised worker (active first starts too) uses `supervised()` before
/// configure, with rotation and cleaning off. Rotation goes on only after the
/// lease: at first-start activation, go after take over, or resume.
/// Inject its clone at open-ferry/src/logging.rs:55 (`file_log::init`) via
/// `init_with_controls`, and its `request_cleaning()` at
/// open-ferry/src/observability.rs:34 (`RequestLogger::new`) via
/// `RequestLogger::with_cleaning`.
/// Only a valid peers_gone in active/watching enables cleaning; log the event
/// exactly when set_cleaning(true) returns false. Hand over's first, infallible
/// step disables cleaning, before mgmt 503, so every first enable after a start
/// or a resumed hand over returns false. This cleaning-off step runs to completion
/// outside the continuation resume aborts; resume first awaits it if still running.
/// Rotation goes off with background work.
/// Both kinds of resume reopen and turn rotation on; after handed_over, retake
/// the lease before reopening. Go enables rotation, never cleaning; cleaning
/// waits for peers_gone even after resume. All controls and reopen run off runtime
/// threads.
/// At the stop deadline use process::exit: dropping a runtime can wait for a
/// held blocking barrier thread. While a barrier holds the mutation read
/// guard, a queued reload also blocks later mutation readers (not get/status).
#[derive(Clone, Debug)]
pub struct LogControls {
    pub(super) rotation: Arc<Mutex<bool>>,
    pub(super) rotation_epoch: Arc<AtomicU64>,
    pub(super) cleaning: Arc<Cleaning>,
    request: CleaningControls,
    // Serialize the entire pair, including waiting for in-flight passes.
    pub(super) cleaning_pair: Arc<Mutex<()>>,
    #[cfg(test)]
    pub(super) pair_pause: Arc<Mutex<Option<Arc<Pass>>>>,
}

impl LogControls {
    pub fn new() -> Self {
        Self {
            rotation: Arc::new(Mutex::new(true)),
            rotation_epoch: Arc::new(AtomicU64::new(0)),
            cleaning: Arc::new(Cleaning::default()),
            request: CleaningControls::new(),
            cleaning_pair: Arc::new(Mutex::new(())),
            #[cfg(test)]
            pair_pause: Arc::new(Mutex::new(None)),
        }
    }

    /// Every supervised worker, including an active first start, starts with
    /// both cleaners off until a valid peers_gone in active or watching.
    /// Rotation starts off before configure, and goes on only after the lease:
    /// at first-start activation, go after take over, or resume.
    pub fn supervised() -> Self {
        let controls = Self::new();
        controls.set_rotation(false);
        controls.set_cleaning(false);
        controls
    }

    pub fn process_default() -> Self {
        static CONTROLS: OnceLock<LogControls> = OnceLock::new();
        CONTROLS
            .get_or_init(|| Self {
                request: CleaningControls::process_default(),
                ..Self::new()
            })
            .clone()
    }

    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the request logger's wiring, in main, takes it")
    )]
    pub fn request_cleaning(&self) -> CleaningControls {
        self.request.clone()
    }

    /// Whether rotation is on, for the worker's tests.
    #[cfg(test)]
    pub(crate) fn rotation_enabled(&self) -> bool {
        *self.rotation.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether cleaning is on, for the worker's tests.
    #[cfg(test)]
    pub(crate) fn cleaning_enabled(&self) -> bool {
        *self
            .cleaning
            .enabled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Blocking: waits for any in-flight write. Call off runtime threads.
    /// Standby/hand over: off. Take over: reopen after the lease and before
    /// ready; go: on. Resume: reopen before turning rotation on (L9.18).
    pub fn set_rotation(&self, enabled: bool) {
        let mut rotation = self.rotation.lock().unwrap_or_else(PoisonError::into_inner);
        if enabled && !*rotation {
            self.rotation_epoch.fetch_add(1, Ordering::Relaxed);
        }
        *rotation = enabled;
    }

    /// Blocking: waits for both in-flight cleaners. Off before supervised
    /// configure and as hand over's first, infallible step before mgmt 503;
    /// on at the first peers_gone since last off,
    /// including after resume. Repeated enable is idempotent; never on at go.
    /// Returns the previous state; log peers_gone only when enabling returns false.
    pub fn set_cleaning(&self, enabled: bool) -> bool {
        let _pair = self
            .cleaning_pair
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.request.set_cleaning(enabled);
        #[cfg(test)]
        {
            let pause = self.pair_pause.lock().unwrap().take();
            if let Some(pause) = pause {
                pause.arrived.send(()).unwrap();
                pause.release.lock().unwrap().recv().unwrap();
            }
        }
        self.cleaning.set(enabled)
    }
}

impl Default for LogControls {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
pub(super) fn rotation() -> Arc<Mutex<bool>> {
    LogControls::process_default().rotation
}

#[cfg(test)]
pub(super) fn cleaning() -> Arc<Cleaning> {
    LogControls::process_default().cleaning
}

// Not upstream's: a deterministic test-only held pass, under the real IO lock.
#[cfg(test)]
#[derive(Debug)]
pub(super) struct Pass {
    pub arrived: std::sync::mpsc::Sender<()>,
    pub release: Mutex<std::sync::mpsc::Receiver<()>>,
}
