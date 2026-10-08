//! The server's update check, in the background.
//!
//! The first check is a few minutes after the server starts (five to ten,
//! at random), then one every `check-every`, give or take a tenth. In
//! `notify` mode, or on an install that doesn't update itself, a check
//! logs a newer release once per version; in `auto` it also stages it, and
//! logs that once. Nothing is switched or restarted here: `open-ferry
//! update` switches, and a restart runs the new binary.
//!
//! In `off` mode no check runs and no request is made; the loop waits for
//! the config to change. Turning updates off while a check runs stops it
//! where it is: the check is dropped, and what it had begun to stage is
//! cleared at the next one. The config is applied as it is reloaded
//! ([`UpdateService::set_config`]).
//!
//! A failed check is logged once, at warn, and the server goes on; the
//! state file and the status keep its error until the next check.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use open_ferry_core::config::{Config, SelfUpdateMode};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::fetch::{Fetch, FetchError, HttpFetch};
use crate::install::Install;
use crate::settings::{self, Settings};
use crate::updater::{CheckResult, Status, UpdateError, Updater};

/// When the first check runs after the start, at random between these.
pub const FIRST_CHECK: (Duration, Duration) =
    (Duration::from_secs(5 * 60), Duration::from_secs(10 * 60));

/// Makes the downloader for a check, from the config's `proxy-url`.
pub type MakeFetch = Arc<dyn Fn(&str) -> Result<Arc<dyn Fetch>, FetchError> + Send + Sync>;

/// The downloader the server uses: [`HttpFetch`] through the proxy.
pub fn http_fetch() -> MakeFetch {
    Arc::new(|proxy_url| Ok(Arc::new(HttpFetch::new(proxy_url)?) as Arc<dyn Fetch>))
}

/// What the loop follows of the config.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    settings: Settings,
    proxy_url: String,
}

/// What [`UpdateService::check_now`] answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckNow {
    /// A check started.
    Started,
    /// A check was running already.
    Running,
    /// Updates are off, so nothing was checked.
    Off,
}

/// The update check's state while the server runs.
#[derive(Default)]
struct Live {
    checking: bool,
    next_check: Option<chrono::DateTime<Utc>>,
    install: Option<Install>,
}

struct Inner {
    /// The updater, but for its downloader, which each check makes.
    updater: Updater,
    make_fetch: MakeFetch,
    snapshot: watch::Sender<Snapshot>,
    live: Mutex<Live>,
    /// Held while a check runs, so two never do.
    running: Arc<tokio::sync::Mutex<()>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

/// The server's update check: cheap to clone, one loop for all clones.
#[derive(Clone)]
pub struct UpdateService {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for UpdateService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateService").finish_non_exhaustive()
    }
}

impl UpdateService {
    /// A service for `updater` (whose downloader is replaced at each
    /// check by one `make_fetch` makes) following `config`. Call
    /// [`start`](Self::start) to begin checking.
    pub fn new(updater: Updater, make_fetch: MakeFetch, config: &Config) -> Self {
        let (snapshot, _) = watch::channel(snapshot_of(config));
        let service = Self {
            inner: Arc::new(Inner {
                updater,
                make_fetch,
                snapshot,
                live: Mutex::new(Live::default()),
                running: Arc::new(tokio::sync::Mutex::new(())),
                task: Mutex::new(None),
            }),
        };
        service.log_settings(None);
        service
    }

    /// Starts the loop on the current runtime, its first check after
    /// `first_check` (see [`FIRST_CHECK`]).
    pub fn start(&self, first_check: Duration) {
        let inner = Arc::clone(&self.inner);
        let task = tokio::spawn(run(inner, first_check));
        let mut slot = self
            .inner
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = slot.replace(task) {
            old.abort();
        }
    }

    /// Stops the loop and any check it runs.
    pub fn stop(&self) {
        let task = self
            .inner
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.abort();
        }
    }

    /// Follows a reloaded config: its mode, interval and proxy.
    pub fn set_config(&self, config: &Config) {
        let next = snapshot_of(config);
        let previous = self.inner.snapshot.borrow().clone();
        if previous == next {
            return;
        }
        self.inner.snapshot.send_replace(next);
        self.log_settings(Some(&previous.settings));
    }

    /// The settings in force.
    pub fn settings(&self) -> Settings {
        self.inner.snapshot.borrow().settings.clone()
    }

    fn log_settings(&self, previous: Option<&Settings>) {
        let settings = self.settings();
        for note in &settings.notes {
            if previous.is_none_or(|previous| !previous.notes.contains(note)) {
                tracing::warn!("{note}");
            }
        }
        if previous.is_some_and(|previous| previous.mode == settings.mode) {
            return;
        }
        match settings.mode {
            SelfUpdateMode::Off => tracing::info!(
                "automatic updates are off (set by {}); no update check is made",
                settings.source
            ),
            mode => tracing::info!(
                "automatic updates: {} (set by {}), checking every {}h; to turn them off, run `open-ferry update -mode off`",
                settings::describe(mode),
                settings.source,
                settings.check_every.as_secs() / 3600
            ),
        }
    }

    /// The status.
    pub fn status(&self) -> Status {
        let settings = self.settings();
        let (checking, next_check, install) = {
            let live = self
                .inner
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (live.checking, live.next_check, live.install.clone())
        };
        let install = install.unwrap_or_else(|| self.inner.updater.install());
        let mut status = self.inner.updater.status(&settings, &install);
        status.checking = checking;
        status.next_check = next_check
            .filter(|_| settings.mode != SelfUpdateMode::Off)
            .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true));
        status
    }

    /// Starts a check now, unless updates are off or one is running.
    pub fn check_now(&self) -> CheckNow {
        if self.settings().mode == SelfUpdateMode::Off {
            return CheckNow::Off;
        }
        let Ok(guard) = Arc::clone(&self.inner.running).try_lock_owned() else {
            return CheckNow::Running;
        };
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let _guard = guard;
            checked(&inner).await;
        });
        CheckNow::Started
    }

    /// Runs one check now and waits for it, as the loop would: `None` when
    /// updates are off, or turn off while it runs.
    pub async fn check_once(&self) -> Option<Result<CheckResult, UpdateError>> {
        let _guard = self.inner.running.lock().await;
        checked(&self.inner).await
    }
}

fn snapshot_of(config: &Config) -> Snapshot {
    Snapshot {
        settings: Settings::from_environment(&config.self_update),
        proxy_url: config.proxy_url.clone(),
    }
}

/// The loop: waits, checks, and waits again, following the config.
async fn run(inner: Arc<Inner>, first_check: Duration) {
    let mut changes = inner.snapshot.subscribe();
    let mut due = Instant::now() + first_check;
    loop {
        let snapshot = changes.borrow_and_update().clone();
        if snapshot.settings.mode == SelfUpdateMode::Off {
            set_next_check(&inner, None);
            if changes.changed().await.is_err() {
                return;
            }
            continue;
        }
        set_next_check(&inner, Some(due));
        tokio::select! {
            () = tokio::time::sleep_until(due) => {}
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
                // A shorter interval brings the next check closer.
                let interval = changes.borrow().settings.check_every;
                due = due.min(Instant::now() + interval);
                continue;
            }
        }
        {
            let _guard = inner.running.lock().await;
            checked(&inner).await;
        }
        due = Instant::now() + settings::jittered(changes.borrow().settings.check_every);
    }
}

fn set_next_check(inner: &Inner, due: Option<Instant>) {
    let next = due.map(|due| {
        let wait = due.saturating_duration_since(Instant::now());
        Utc::now() + chrono::Duration::from_std(wait).unwrap_or_default()
    });
    inner
        .live
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .next_check = next;
}

/// Runs one check in the mode in force, logging its outcome, unless
/// updates are off; stops it if they are turned off meanwhile. The caller
/// holds `running`.
async fn checked(inner: &Inner) -> Option<Result<CheckResult, UpdateError>> {
    let mut changes = inner.snapshot.subscribe();
    let snapshot = changes.borrow_and_update().clone();
    if snapshot.settings.mode == SelfUpdateMode::Off {
        return None;
    }
    set_checking(inner, true);
    let outcome = tokio::select! {
        outcome = check(inner, &snapshot) => Some(outcome),
        () = turned_off(&mut changes) => {
            tracing::info!("automatic updates were turned off; the update check in progress stopped");
            None
        }
    };
    set_checking(inner, false);
    match &outcome {
        Some(Ok(result)) if result.news => {
            tracing::info!("{}", result.report.describe(&inner.updater.running));
        }
        Some(Ok(result)) => {
            tracing::debug!("{}", result.report.describe(&inner.updater.running));
        }
        Some(Err(UpdateError::Busy)) => {
            tracing::debug!("update check skipped: another update is running");
        }
        Some(Err(error)) => tracing::warn!("update check failed: {error}"),
        None => {}
    }
    outcome
}

async fn check(inner: &Inner, snapshot: &Snapshot) -> Result<CheckResult, UpdateError> {
    let fetch = (inner.make_fetch)(&snapshot.proxy_url).map_err(|error| UpdateError::Fetch {
        what: "the release list",
        error,
    })?;
    let updater = Updater {
        fetch,
        ..inner.updater.clone()
    };
    let install = updater.install();
    inner
        .live
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .install = Some(install);
    updater.check(snapshot.settings.mode).await
}

/// Resolves once the mode is `off`.
async fn turned_off(changes: &mut watch::Receiver<Snapshot>) {
    loop {
        if changes.changed().await.is_err() {
            // The service is gone; let the check finish.
            std::future::pending::<()>().await;
        }
        if changes.borrow_and_update().settings.mode == SelfUpdateMode::Off {
            return;
        }
    }
}

fn set_checking(inner: &Inner, checking: bool) {
    inner
        .live
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .checking = checking;
}
