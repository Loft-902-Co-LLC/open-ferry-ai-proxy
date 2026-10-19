//! The server's update check, in the background.
//!
//! The first check is a few minutes after the server starts (five to ten,
//! at random), then one every `check-every`, give or take a tenth. In
//! `notify` mode, or on an install that doesn't update itself, a check
//! logs a newer release once per version; in `auto` it also stages it, and
//! logs that once. A supervised worker also asks its supervisor to switch
//! to the staged release. A plain server leaves it staged for installation.
//!
//! In `off` mode no check runs and no request is made; the loop waits for
//! the config to change. Turning updates off while a check runs stops it
//! where it is: the check is dropped, and what it had begun to stage is
//! cleared at the next one. The config is applied as it is reloaded
//! ([`UpdateService::set_config`]).
//!
//! A failed check is logged once, at warn, and the server goes on; the
//! state file and the status keep its error until the next check.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use open_ferry_core::config::{Config, SelfUpdateMode};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::fetch::{Fetch, FetchError, HttpFetch};
use crate::install::{self, Install};
use crate::receipt::Receipt;
use crate::settings::{self, Settings};
use crate::switching::{self, CancelOutcome, SwitchError, SwitchEvent, SwitchHandle, Switcher};
use crate::updater::{CheckResult, Report, Status, UpdateError, Updater};

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

pub(crate) struct Inner {
    /// The updater, but for its downloader, which each check makes.
    pub(crate) updater: Updater,
    make_fetch: MakeFetch,
    snapshot: watch::Sender<Snapshot>,
    live: Mutex<Live>,
    /// Held while a check runs, so two never do.
    running: Arc<tokio::sync::Mutex<()>>,
    task: Mutex<Option<JoinHandle<()>>>,
    switcher: Mutex<Option<Arc<dyn Switcher>>>,
    installed: Mutex<Option<PathBuf>>,
}

/// The server's update check: cheap to clone, one loop for all clones.
#[derive(Clone)]
pub struct UpdateService {
    pub(crate) inner: Arc<Inner>,
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
                switcher: Mutex::new(None),
                installed: Mutex::new(None),
            }),
        };
        service.log_settings(None);
        service
    }

    /// Starts the loop on the current runtime, its first check after
    /// `first_check` (see [`FIRST_CHECK`]).
    pub fn start(&self, first_check: Duration) {
        #[cfg(feature = "blue-green-test")]
        let first_check = if self
            .inner
            .updater
            .data
            .root()
            .join(".blue-green-test")
            .join("update-now")
            .exists()
        {
            Duration::ZERO
        } else {
            first_check
        };
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

    /// Attach local supervisor access before sharing the service. All clones
    /// use it; this does not connect, check locks, or start a switch.
    pub fn with_switcher(self, switcher: Arc<dyn Switcher>) -> Self {
        *self
            .inner
            .switcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(switcher);
        self
    }

    /// Assess the installed path from the authenticated supervisor welcome.
    /// A worker's own executable may be a kept version of that installation.
    pub fn with_installed_path(self, installed: PathBuf) -> Self {
        *self
            .inner
            .installed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(installed);
        self
    }

    pub(crate) fn switcher(&self) -> Result<Arc<dyn Switcher>, SwitchError> {
        self.inner
            .switcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(SwitchError::NotSupervised)
    }

    /// The supervisor's complete endpoint snapshot. Bounded to 750 ms.
    pub async fn supervisor(&self) -> Result<serde_json::Value, SwitchError> {
        self.switcher()?
            .status(std::time::Instant::now() + Duration::from_millis(750))
            .await
    }

    /// Stage the newest known release if needed, pin under the update lock,
    /// release it, and ask the supervisor to update. Returns at acceptance,
    /// before loading, hand-over, or the watch window. Preparing an unstaged
    /// release can download it; callers with a short response budget should
    /// prepare it in a background task. A timeout may have sent the request:
    /// read status before retrying. Manual installs also work in `off` mode.
    pub async fn install_now(&self, auto: bool) -> Result<SwitchHandle, SwitchError> {
        let switcher = self.switcher()?;
        let snapshot = self.inner.snapshot.borrow().clone();
        if auto && snapshot.settings.mode == SelfUpdateMode::Off {
            return Err(SwitchError::UpdatesOff);
        }
        let running = Arc::clone(&self.inner.running)
            .try_lock_owned()
            .map_err(|_| UpdateError::Busy)?;
        let inner = Arc::clone(&self.inner);
        let runtime = tokio::runtime::Handle::current();
        // The task owns the guard even if the caller stops waiting. Blocking
        // hashes, file writes and fsyncs must not hold up the async runtime.
        let pin = tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                let _running = running;
                let updater = &inner.updater;
                let _lock = updater.lock()?;
                let mut state = switching::read_state(updater)?;
                let staged = state.staged.clone().filter(|version| {
                    state.latest.as_ref().is_none_or(|latest| latest == version)
                        && (auto
                            || version.parse().is_ok_and(|latest| {
                                crate::release::compare(&updater.running, &latest)
                                    == crate::release::Verdict::Newer
                            }))
                });
                let version = match staged {
                    Some(version) => version,
                    None => {
                        let fetch = (inner.make_fetch)(&snapshot.proxy_url).map_err(|error| {
                            UpdateError::Fetch {
                                what: "the release list",
                                error,
                            }
                        })?;
                        let updater = Updater {
                            fetch,
                            ..updater.clone()
                        };
                        let found = updater.latest(&state).await?;
                        let newer = if auto {
                            found.is_newer()
                        } else {
                            crate::release::compare(&updater.running, &found.release.semver)
                                == crate::release::Verdict::Newer
                        };
                        if !newer {
                            return Err(if auto {
                                UpdateError::NothingStaged
                            } else {
                                UpdateError::NotNewer {
                                    running: updater.running.clone(),
                                    latest: found.release.version.clone(),
                                }
                            }
                            .into());
                        }
                        if auto {
                            switching::skipped(&updater, &state, &found.release.version)?;
                        }
                        updater.stage(&found, &mut state).await?;
                        found.release.version
                    }
                };
                if auto {
                    switching::skipped(updater, &state, &version)?;
                }
                let sha256 = state
                    .staged_sha256
                    .as_deref()
                    .ok_or(UpdateError::NothingStaged)?;
                Ok::<_, SwitchError>(updater.pin_staged(&version, sha256)?)
            })
        })
        .await
        .map_err(|error| UpdateError::Io {
            what: "prepare the update",
            error: error.to_string(),
        })??;
        if auto && self.settings().mode == SelfUpdateMode::Off {
            return Err(SwitchError::UpdatesOff);
        }
        switcher
            .update(
                &pin,
                auto,
                std::time::Instant::now() + Duration::from_secs(3),
            )
            .await
    }

    /// Validate the journal's `switch.from` during a watch, else `previous`,
    /// under the lock; require the target the caller confirmed, then drop
    /// the lock before requesting a rollback. No pin is sent.
    pub async fn roll_back(&self, expected: &str) -> Result<SwitchHandle, SwitchError> {
        let switcher = self.switcher()?;
        let updater = self.inner.updater.clone();
        // The supervisor validates --version before Accepted. Allow the
        // updater's full version-probe budget plus transport time.
        let acceptance = updater.limits.version_timeout + Duration::from_secs(3);
        let runtime = tokio::runtime::Handle::current();
        let expected = expected.to_owned();
        let target = tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                let _lock = updater.lock()?;
                let target = switching::rollback_target(&updater)?;
                if target.version != expected {
                    return Err(UpdateError::Supervised(format!(
                        "rollback target changed: expected {expected}, now {}; run `open-ferry update -check`",
                        target.version
                    )).into());
                }
                updater.validate_previous(&target).await?;
                Ok::<_, SwitchError>(target)
            })
        })
        .await
        .map_err(|error| UpdateError::Io { what: "prepare the rollback", error: error.to_string() })??;
        switcher
            .rollback(&target, std::time::Instant::now() + acceptance)
            .await
    }

    /// Ask the supervisor to cancel its current switch, if still possible.
    pub async fn cancel_switch(&self) -> Result<CancelOutcome, SwitchError> {
        self.switcher()?
            .cancel(std::time::Instant::now() + Duration::from_millis(750))
            .await
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
        let install = install.unwrap_or_else(|| assessed_install(&self.inner));
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
            checked(&inner, guard).await;
        });
        CheckNow::Started
    }

    /// Runs one check now and waits for it, as the loop would: `None` when
    /// updates are off, or turn off while it runs.
    pub async fn check_once(&self) -> Option<Result<CheckResult, UpdateError>> {
        let guard = Arc::clone(&self.inner.running).lock_owned().await;
        checked(&self.inner, guard).await
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
            let guard = Arc::clone(&inner.running).lock_owned().await;
            checked(&inner, guard).await;
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
/// passes `running`, which is released before asking for a switch.
async fn checked(
    inner: &Arc<Inner>,
    guard: tokio::sync::OwnedMutexGuard<()>,
) -> Option<Result<CheckResult, UpdateError>> {
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
    set_checking(inner, false);
    drop(guard);
    if let Some(Ok(CheckResult {
        report: Report::Staged { version },
        ..
    })) = &outcome
        && version != &inner.updater.running
        && inner.snapshot.borrow().settings.mode == SelfUpdateMode::Auto
    {
        let service = UpdateService {
            inner: Arc::clone(inner),
        };
        if service.switcher().is_ok() {
            match service.install_now(true).await {
                Ok(handle) => {
                    tracing::info!(id = %handle.id, %version, "automatic switch accepted");
                    tokio::spawn(follow_switch(handle));
                }
                Err(SwitchError::Ended(reason)) => {
                    tracing::info!(%version, %reason, "automatic switch ended");
                }
                Err(error) => tracing::debug!("automatic switch deferred: {error}"),
            }
        }
    }
    outcome
}

async fn follow_switch(mut handle: SwitchHandle) {
    loop {
        match handle
            .next(std::time::Instant::now() + Duration::from_secs(120))
            .await
        {
            Ok(Some(SwitchEvent::Progress { phase })) => {
                tracing::debug!(id = %handle.id, %phase, "automatic switch progress");
            }
            Ok(Some(SwitchEvent::Done { result })) => {
                tracing::info!(id = %handle.id, %result, "automatic switch finished");
                return;
            }
            Ok(None) => return,
            Err(error) => {
                tracing::info!(id = %handle.id, "automatic switch progress ended: {error}; see supervisor status and update history");
                return;
            }
        }
    }
}

fn assessed_install(inner: &Inner) -> Install {
    let installed = inner
        .installed
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    match installed.as_deref() {
        Some(path) => install::assess_for(
            inner.updater.system.as_ref(),
            Receipt::load(&inner.updater.data.receipt_file()),
            path,
        ),
        None => inner.updater.install(),
    }
}

async fn check(inner: &Inner, snapshot: &Snapshot) -> Result<CheckResult, UpdateError> {
    #[cfg(feature = "blue-green-test")]
    {
        let root = inner.updater.data.root().to_path_buf();
        tokio::task::spawn_blocking(move || crate::test_seams::hit(&root, "auto.before_check"))
            .await
            .map_err(|error| UpdateError::Io {
                what: "wait at the test check barrier",
                error: error.to_string(),
            })?
            .map_err(|error| UpdateError::Io {
                what: "wait at the test check barrier",
                error: error.to_string(),
            })?;
    }
    let fetch = (inner.make_fetch)(&snapshot.proxy_url).map_err(|error| UpdateError::Fetch {
        what: "the release list",
        error,
    })?;
    let updater = Updater {
        fetch,
        ..inner.updater.clone()
    };
    let supervised = inner
        .switcher
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some();
    let install = assessed_install(inner);
    inner
        .live
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .install = Some(install.clone());
    updater
        .check_with_pending(snapshot.settings.mode, supervised, &install)
        .await
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
