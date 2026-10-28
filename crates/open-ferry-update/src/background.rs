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
use open_ferry_core::config::{Config, SelfUpdateChannel, SelfUpdateMode};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::instrument::WithSubscriber;

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
    /// Updates are off or the service is stopped, so nothing was checked.
    Off,
}

/// The update check's state while the server runs.
#[derive(Default)]
struct Live {
    checking: bool,
    next_check: Option<chrono::DateTime<Utc>>,
    install: Option<Install>,
    automatic_error: Option<&'static str>,
    local_check: Option<Arc<LocalCheck>>,
}

/// A check can fail before writing state. Any subsequent state publication
/// supersedes this process-local report, including a check in another process.
struct LocalCheck {
    at: String,
    error: String,
    persisted: crate::state::State,
    modified: Option<std::time::SystemTime>,
}

fn state_modified(updater: &Updater) -> Option<std::time::SystemTime> {
    std::fs::metadata(updater.data.state_file())
        .ok()?
        .modified()
        .ok()
}

#[derive(Default)]
struct Tasks {
    timer: Option<JoinHandle<()>>,
    spawned: Vec<JoinHandle<()>>,
    followers: Vec<JoinHandle<()>>,
    stopped: bool,
}

impl Tasks {
    fn spawn(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.spawned.retain(|task| !task.is_finished());
        if !self.stopped {
            self.spawned.push(tokio::spawn(future));
        }
    }

    fn follow(&mut self, handle: SwitchHandle, version: String) {
        self.followers.retain(|task| !task.is_finished());
        self.followers.push(tokio::spawn(
            follow_switch(handle, version).with_current_subscriber(),
        ));
    }

    fn abort_checks(&mut self) {
        self.stopped = true;
        for task in self.timer.iter().chain(&self.spawned) {
            task.abort();
        }
    }

    fn abort_all(&mut self) {
        self.abort_checks();
        for task in &self.followers {
            task.abort();
        }
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        self.abort_all();
    }
}

/// Retains join handles across a cancelled wait or its five-second escape.
struct StoppedChecks<'a> {
    tasks: &'a Mutex<Tasks>,
    unfinished: Vec<JoinHandle<()>>,
    // Drop returns the handles before releasing the next stop's gate.
    _stopping: tokio::sync::MutexGuard<'a, ()>,
}

impl<'a> StoppedChecks<'a> {
    fn new(tasks: &'a Mutex<Tasks>, stopping: tokio::sync::MutexGuard<'a, ()>) -> Self {
        let unfinished = {
            let mut tasks = tasks.lock().unwrap_or_else(PoisonError::into_inner);
            // A start may have replaced the timer while we waited for the gate.
            // Only take handles we abort in this same critical section.
            tasks.abort_checks();
            let mut unfinished = std::mem::take(&mut tasks.spawned);
            unfinished.extend(tasks.timer.take());
            unfinished
        };
        Self {
            tasks,
            unfinished,
            _stopping: stopping,
        }
    }

    async fn wait(&mut self) {
        while let Some(task) = self.unfinished.last_mut() {
            let _ = task.await;
            self.unfinished.pop();
        }
    }
}

impl Drop for StoppedChecks<'_> {
    fn drop(&mut self) {
        self.unfinished.retain(|task| !task.is_finished());
        if !self.unfinished.is_empty() {
            self.tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .spawned
                .append(&mut self.unfinished);
        }
    }
}

/// Public service clones own the tasks; the tasks themselves only own Inner.
struct Owner(std::sync::Weak<Inner>);

impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            inner
                .tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .abort_all();
        }
    }
}

pub(crate) struct Inner {
    /// The updater, but for its downloader, which each check makes.
    pub(crate) updater: Updater,
    make_fetch: MakeFetch,
    snapshot: watch::Sender<Snapshot>,
    live: Mutex<Live>,
    /// Held while a check runs, so two never do.
    running: Arc<tokio::sync::Mutex<()>>,
    /// Held through switch preparation and the endpoint's admission reply.
    requesting: Arc<tokio::sync::Mutex<()>>,
    tasks: Mutex<Tasks>,
    stopping: tokio::sync::Mutex<()>,
    switcher: Mutex<Option<Arc<dyn Switcher>>>,
    installed: Mutex<Option<PathBuf>>,
}

/// The server's update check: cheap to clone, one loop for all clones.
#[derive(Clone)]
pub struct UpdateService {
    pub(crate) inner: Arc<Inner>,
    _owner: Option<Arc<Owner>>,
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
        let inner = Arc::new(Inner {
            updater,
            make_fetch,
            snapshot,
            live: Mutex::new(Live::default()),
            running: Arc::new(tokio::sync::Mutex::new(())),
            requesting: Arc::new(tokio::sync::Mutex::new(())),
            tasks: Mutex::new(Tasks::default()),
            stopping: tokio::sync::Mutex::new(()),
            switcher: Mutex::new(None),
            installed: Mutex::new(None),
        });
        let service = Self {
            _owner: Some(Arc::new(Owner(Arc::downgrade(&inner)))),
            inner,
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
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        tasks.stopped = false;
        if let Some(old) = tasks.timer.replace(tokio::spawn(run(
            inner,
            first_check,
            self.settings().channel,
        ))) {
            old.abort();
            tasks.spawned.push(old);
        }
    }

    /// Stops the loop and explicit checks. Accepted switches are still followed
    /// if this worker resumes after a failed handover; they never send requests.
    pub fn stop(&self) {
        self.abort_checks();
    }

    /// Stops checks and waits for any poll already running on another thread.
    /// Accepted switches keep their followers. A stuck poll gets five seconds.
    pub async fn stop_and_wait(&self) {
        self.abort_checks();
        let stopping = self.inner.stopping.lock().await;
        let mut stopped = StoppedChecks::new(&self.inner.tasks, stopping);
        if tokio::time::timeout(Duration::from_secs(5), stopped.wait())
            .await
            .is_err()
        {
            tracing::warn!("update checks have not stopped after five seconds; continuing");
        }
    }

    fn abort_checks(&self) {
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        tasks.abort_checks();
        drop(tasks);
        set_checking(&self.inner, false);
        set_next_check(&self.inner, None);
    }

    /// Follows a reloaded config: its mode, interval and proxy.
    pub fn set_config(&self, config: &Config) {
        let next = snapshot_of(config);
        let previous = self.inner.snapshot.borrow().clone();
        if previous == next {
            return;
        }
        if previous.settings.channel != next.settings.channel {
            tracing::info!(
                from = previous.settings.channel.as_str(),
                to = next.settings.channel.as_str(),
                "update channel changed; scheduling a fresh check"
            );
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
        self.install_with_target(auto)
            .await
            .map(|(handle, _)| handle)
    }

    async fn install_with_target(&self, auto: bool) -> Result<(SwitchHandle, String), SwitchError> {
        let switcher = self.switcher()?;
        let request = Arc::clone(&self.inner.requesting)
            .try_lock_owned()
            .map_err(|_| UpdateError::Busy)?;
        let snapshot = self.inner.snapshot.borrow().clone();
        if auto && snapshot.settings.mode != SelfUpdateMode::Auto {
            return Err(SwitchError::UpdatesOff);
        }
        let running = Arc::clone(&self.inner.running)
            .try_lock_owned()
            .map_err(|_| UpdateError::Busy)?;
        let selected_channel = snapshot.settings.channel;
        let inner = Arc::clone(&self.inner);
        let runtime = tokio::runtime::Handle::current();
        // The task owns the guard even if the caller stops waiting. Blocking
        // hashes, file writes and fsyncs must not hold up the async runtime.
        let (pin, _request) = tokio::task::spawn_blocking(move || {
            let prepared = runtime.block_on(async move {
                let _running = running;
                let updater = &Updater {
                    channel: snapshot.settings.channel,
                    ..inner.updater.clone()
                };
                let _lock = updater.lock()?;
                let mut state = switching::read_state(updater)?;
                let staged = state.staged.clone().filter(|version| {
                    state.channel.as_deref().unwrap_or("stable")
                        == snapshot.settings.channel.as_str()
                        && state.latest.as_ref().is_none_or(|latest| latest == version)
                        && (auto
                            || version.parse().is_ok_and(|latest| {
                                crate::release::compare_channel(
                                    &updater.running,
                                    &latest,
                                    updater.channel == SelfUpdateChannel::Preview,
                                ) == crate::release::Verdict::Newer
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
                            crate::release::compare_channel(
                                &updater.running,
                                &found.release.semver,
                                updater.channel == SelfUpdateChannel::Preview,
                            ) == crate::release::Verdict::Newer
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
            });
            prepared.map(|pin| (pin, request))
        })
        .await
        .map_err(|error| UpdateError::Io {
            what: "prepare the update",
            error: error.to_string(),
        })??;
        if self.settings().channel != selected_channel {
            return Err(UpdateError::ChannelChanged.into());
        }
        if auto && self.settings().mode != SelfUpdateMode::Auto {
            return Err(SwitchError::UpdatesOff);
        }
        let version = pin.version.clone();
        let handle = switcher
            .update(
                &pin,
                auto,
                std::time::Instant::now() + Duration::from_secs(3),
            )
            .await?;
        Ok((handle, version))
    }

    /// Validate the journal's `switch.from` during a watch, else `previous`,
    /// under the lock; require the target the caller confirmed, then drop
    /// the lock before requesting a rollback. No pin is sent.
    pub async fn roll_back(&self, expected: &str) -> Result<SwitchHandle, SwitchError> {
        let switcher = self.switcher()?;
        let request = Arc::clone(&self.inner.requesting)
            .try_lock_owned()
            .map_err(|_| UpdateError::Busy)?;
        let updater = self.inner.updater.clone();
        // The supervisor validates --version before Accepted. Allow the
        // updater's full version-probe budget plus transport time.
        let acceptance = updater.limits.version_timeout + Duration::from_secs(3);
        let runtime = tokio::runtime::Handle::current();
        let expected = expected.to_owned();
        let (target, _request) = tokio::task::spawn_blocking(move || {
            let prepared = runtime.block_on(async move {
                let _lock = updater.lock()?;
                let target = switching::rollback_target(&updater)?;
                if target.version != expected {
                    return Err(UpdateError::RollbackChanged {
                        expected,
                        actual: Some(target.version),
                    }
                    .into());
                }
                updater.validate_previous(&target).await?;
                Ok::<_, SwitchError>(target)
            });
            prepared.map(|target| (target, request))
        })
        .await
        .map_err(|error| UpdateError::Io {
            what: "prepare the rollback",
            error: error.to_string(),
        })??;
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
        let (checking, next_check, install, local_check) = {
            let live = self
                .inner
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (
                live.checking,
                live.next_check,
                live.install.clone(),
                live.local_check.clone(),
            )
        };
        let install = install.unwrap_or_else(|| assessed_install(&self.inner));
        #[cfg(feature = "blue-green-test")]
        let _ = crate::test_seams::hit(self.inner.updater.data.root(), "auto.before_status_state");
        let state = self.inner.updater.state();
        let mut status = self
            .inner
            .updater
            .status_with_state(&settings, &install, &state);
        if let Some(local) = local_check {
            if local.persisted == state && local.modified == state_modified(&self.inner.updater) {
                status.last_check = Some(local.at.clone());
                status.last_result = Some("error".to_owned());
                status.last_error = Some(local.error.clone());
            } else {
                let mut live = self
                    .inner
                    .live
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                // The state read may race a newer process-local result.
                if live
                    .local_check
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &local))
                {
                    live.local_check = None;
                }
            }
        }
        status.checking = checking;
        status.next_check = next_check
            .filter(|_| settings.mode != SelfUpdateMode::Off)
            .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true));
        status
    }

    /// Starts a check now, unless updates are off or one is running.
    pub fn check_now(&self) -> CheckNow {
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if tasks.stopped || self.settings().mode == SelfUpdateMode::Off {
            return CheckNow::Off;
        }
        let Ok(guard) = Arc::clone(&self.inner.running).try_lock_owned() else {
            return CheckNow::Running;
        };
        let inner = Arc::clone(&self.inner);
        tasks.spawn(async move {
            checked(&inner, guard, true).await;
        });
        CheckNow::Started
    }

    /// Runs one check now and waits for it, as the loop would: `None` when
    /// updates are off, or turn off while it runs.
    pub async fn check_once(&self) -> Option<Result<CheckResult, UpdateError>> {
        let guard = Arc::clone(&self.inner.running).lock_owned().await;
        checked(&self.inner, guard, true).await
    }
}

fn snapshot_of(config: &Config) -> Snapshot {
    Snapshot {
        settings: Settings::from_environment(&config.self_update),
        proxy_url: config.proxy_url.clone(),
    }
}

/// The loop: waits, checks, and waits again, following the config.
async fn run(inner: Arc<Inner>, first_check: Duration, mut channel: SelfUpdateChannel) {
    let mut changes = inner.snapshot.subscribe();
    let mut due = Instant::now() + first_check;
    loop {
        let snapshot = changes.borrow_and_update().clone();
        if snapshot.settings.channel != channel {
            due = Instant::now();
            channel = snapshot.settings.channel;
        }
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
            checked(&inner, guard, false).await;
        }
        due = if changes.borrow().settings.channel != snapshot.settings.channel {
            Instant::now()
        } else {
            Instant::now() + settings::jittered(changes.borrow().settings.check_every)
        };
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
    manual: bool,
) -> Option<Result<CheckResult, UpdateError>> {
    let mut changes = inner.snapshot.subscribe();
    let snapshot = changes.borrow_and_update().clone();
    if snapshot.settings.mode == SelfUpdateMode::Off {
        return None;
    }
    set_checking(inner, true);
    let outcome = tokio::select! {
        outcome = async {
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                let outcome = check(inner, &snapshot).await;
                if !manual || !matches!(outcome, Err(UpdateError::Busy)) || Instant::now() >= until {
                    break outcome;
                }
                tokio::time::sleep_until(until.min(Instant::now() + Duration::from_millis(250))).await;
            }
        } => Some(outcome),
        () = turned_off(&mut changes, snapshot.settings.channel) => {
            tracing::info!("automatic update policy changed; the update check in progress stopped");
            None
        }
    };
    match &outcome {
        Some(Ok(result)) => {
            inner
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .local_check = None;
            let message = match &result.report {
                Report::Staged { version }
                    if inner.snapshot.borrow().settings.mode == SelfUpdateMode::Auto
                        && inner
                            .switcher
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .is_some() =>
                {
                    format!(
                        "open-ferry {version} is downloaded, checked and staged (running {}); asking the supervisor to switch to it",
                        inner.updater.running
                    )
                }
                report => report.describe(&inner.updater.running),
            };
            if result.news {
                tracing::info!("{message}");
            } else {
                tracing::debug!("{message}");
            }
        }
        Some(Err(UpdateError::Busy)) => {
            if manual {
                record_local_check(
                    inner,
                    "another update is running; try again shortly".to_owned(),
                );
            }
            // Scheduled checks wait for their next turn, but a lock held over
            // many intervals must remain visible at the normal log level.
            if manual {
                tracing::warn!(
                    "update check deferred: another update is running; try again shortly"
                );
            } else {
                tracing::warn!(
                    "update check deferred: another update is running; trying again at the next interval"
                );
            }
        }
        Some(Err(error)) => {
            record_local_check(inner, error.to_string());
            tracing::warn!("update check failed: {error}");
        }
        None => {}
    }
    set_checking(inner, false);
    drop(guard);
    if let Some(Ok(CheckResult {
        report: Report::Staged { version },
        ..
    })) = &outcome
        && inner.snapshot.borrow().settings.mode == SelfUpdateMode::Auto
    {
        let service = UpdateService {
            inner: Arc::clone(inner),
            _owner: None,
        };
        if service.switcher().is_ok() {
            match service.install_with_target(true).await {
                Ok((handle, target)) => {
                    inner
                        .live
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .automatic_warning(None);
                    tracing::info!(operation = %handle.id, %version, "automatic switch accepted");
                    inner
                        .tasks
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .follow(handle, target);
                }
                Err(error) => {
                    let warning = inner
                        .live
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .automatic_warning(Some(&error));
                    // Unreachable and Update mean nothing was sent. Uncertain
                    // requests are never resent here; future checks still pass
                    // the supervisor's admission and outcome cooldown checks.
                    match error {
                        SwitchError::Disabled(_) => {
                            if let Some(reason) = warning {
                                tracing::info!(%version, %reason, "automatic switches are off");
                            }
                        }
                        SwitchError::Ended(reason) => {
                            tracing::info!(%version, %reason, "automatic switch ended");
                        }
                        SwitchError::TimedOut | SwitchError::Stopped | SwitchError::Protocol(_) => {
                            tracing::info!(%version, state = "uncertain", "no answer from the supervisor; it may have taken the automatic switch; see its status");
                        }
                        SwitchError::Update(UpdateError::Busy) => {
                            tracing::debug!("automatic switch deferred: another update is running");
                        }
                        SwitchError::Update(
                            UpdateError::NothingStaged | UpdateError::NotNewer { .. },
                        ) => {
                            tracing::debug!(
                                "automatic switch deferred: no newer version is staged"
                            );
                        }
                        SwitchError::Untrusted(_)
                        | SwitchError::Unreachable(_)
                        | SwitchError::Update(_) => {
                            if let Some(reason) = warning {
                                tracing::warn!(%version, %reason, "automatic switch could not start");
                            }
                        }
                        _ => tracing::debug!("automatic switch deferred: {error}"),
                    }
                }
            }
        }
    }
    outcome
}

fn record_local_check(inner: &Inner, error: String) {
    // Snapshot at completion: another lock holder may have published state
    // while this attempt waited. Read before taking the live mutex.
    let local = Arc::new(LocalCheck {
        at: crate::updater::now(),
        error,
        persisted: inner.updater.state(),
        modified: state_modified(&inner.updater),
    });
    inner
        .live
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .local_check = Some(local);
}

/// Keep local errors out of logs: their detail can contain endpoint or file paths.
impl Live {
    fn automatic_warning(&mut self, error: Option<&SwitchError>) -> Option<&'static str> {
        let reason = match error {
            Some(SwitchError::Disabled(_)) => Some("the supervisor has switching disabled"),
            Some(SwitchError::Untrusted(_)) => Some(
                "the supervisor's endpoint failed its ownership check; automatic switches are paused",
            ),
            Some(SwitchError::Unreachable(_)) => Some(
                "the supervisor cannot be reached; automatic switching will try again at the next check",
            ),
            Some(SwitchError::Update(error))
                if !matches!(
                    error,
                    UpdateError::Busy | UpdateError::NothingStaged | UpdateError::NotNewer { .. }
                ) =>
            {
                Some("the automatic switch could not be prepared; trying again at the next check")
            }
            _ => None,
        };
        let changed = self.automatic_error != reason;
        self.automatic_error = reason;
        reason.filter(|_| changed)
    }
}

async fn follow_switch(mut handle: SwitchHandle, version: String) {
    // Only automatic Updates are followed today. A rollback caller must pass
    // its rollback target as `version`, rather than the staged update version.
    // Sign-ins can wait five minutes and the watch lasts ten. Quiet intervals
    // do not say whether the switch is still running.
    let until = Instant::now() + Duration::from_secs(30 * 60);
    loop {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let deadline = std::time::Instant::now() + remaining.min(Duration::from_secs(120));
        match tokio::time::timeout_at(until, handle.next(deadline)).await {
            Ok(Ok(Some(SwitchEvent::Progress { phase }))) => {
                tracing::debug!(operation = %handle.id, state = %phase, "automatic switch progress");
            }
            Ok(Ok(Some(SwitchEvent::Done { result }))) => {
                let outcome = result["result"].as_str().unwrap_or("unknown");
                let phase = result["phase"].as_str().unwrap_or("unspecified");
                let gap_ms = result["gap_ms"].as_u64();
                if phase == "watching" && matches!(outcome, "switched" | "rolled-back") {
                    tracing::info!(operation = %handle.id, result = outcome, %version, phase, gap_ms,
                        "automatic switch: open-ferry {version} took over; watching it before the switch is final");
                } else {
                    tracing::info!(operation = %handle.id, result = outcome, %version, phase, gap_ms,
                        "automatic switch reported a result");
                }
                return;
            }
            Ok(Ok(None)) => return,
            Ok(Err(SwitchError::TimedOut)) => continue,
            Ok(Err(SwitchError::Ended(reason))) => {
                tracing::info!(operation = %handle.id, %reason, "automatic switch ended");
                return;
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
    tracing::info!(operation = %handle.id, "automatic switch is no longer being followed; it may still be running; see supervisor status and update history");
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
    check_barrier(inner, "auto.before_check").await?;
    // Busy retries need no client construction or install assessment. The
    // real check still acquires its own lock, so a contender winning after
    // this probe simply returns Busy and follows the same retry policy.
    drop(inner.updater.lock()?);
    #[cfg(all(test, feature = "blue-green-test"))]
    check_barrier(inner, "auto.after_lock_probe").await?;
    let fetch = (inner.make_fetch)(&snapshot.proxy_url).map_err(|error| UpdateError::Fetch {
        what: "the release list",
        error,
    })?;
    let updater = Updater {
        fetch,
        channel: snapshot.settings.channel,
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

#[cfg(feature = "blue-green-test")]
async fn check_barrier(inner: &Inner, name: &'static str) -> Result<(), UpdateError> {
    let root = inner.updater.data.root().to_path_buf();
    tokio::task::spawn_blocking(move || crate::test_seams::hit(&root, name))
        .await
        .map_err(|error| UpdateError::Io {
            what: "wait at the test check barrier",
            error: error.to_string(),
        })?
        .map_err(|error| UpdateError::Io {
            what: "wait at the test check barrier",
            error: error.to_string(),
        })
}

/// Resolves once the mode is `off`.
async fn turned_off(changes: &mut watch::Receiver<Snapshot>, channel: SelfUpdateChannel) {
    loop {
        if changes.changed().await.is_err() {
            // The service is gone; let the check finish.
            std::future::pending::<()>().await;
        }
        let settings = &changes.borrow_and_update().settings;
        if settings.mode == SelfUpdateMode::Off || settings.channel != channel {
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

#[cfg(test)]
#[path = "tests/auto_log.rs"]
pub(crate) mod test_log;

#[cfg(test)]
mod tests {
    use super::*;

    struct FinishedProgress;

    impl crate::SwitchProgress for FinishedProgress {
        fn next(
            &mut self,
            _deadline: std::time::Instant,
        ) -> crate::fetch::BoxFuture<'_, Result<SwitchEvent, SwitchError>> {
            Box::pin(async {
                Ok(SwitchEvent::Done {
                    result: serde_json::json!({
                        "result": "switched", "to": "0.2.0", "from": "0.1.0",
                        "reason": "/private/verbose-record", "at": "2026-10-19T00:00:00Z",
                    }),
                })
            })
        }
    }

    #[tokio::test]
    async fn a_follower_logs_only_the_finished_result_and_target() {
        use tracing::instrument::WithSubscriber;
        let logs = test_log::Capture::default();
        follow_switch(
            SwitchHandle::new("finished".into(), Box::new(FinishedProgress)),
            "0.2.0".into(),
        )
        .with_subscriber(logs.clone())
        .await;
        let events = logs.0.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].starts_with("INFO "), "{events:?}");
        assert!(events[0].contains("automatic switch reported a result"));
        assert!(events[0].contains("result=\"switched\""), "{events:?}");
        assert!(events[0].contains("version=0.2.0"), "{events:?}");
        assert!(
            !events[0].contains("/private/")
                && !events[0].contains("from=")
                && !events[0].contains("2026-10-19"),
            "{events:?}"
        );
    }

    struct EndedProgress;

    impl crate::SwitchProgress for EndedProgress {
        fn next(
            &mut self,
            _deadline: std::time::Instant,
        ) -> crate::fetch::BoxFuture<'_, Result<SwitchEvent, SwitchError>> {
            Box::pin(async { Err(SwitchError::Ended("the new version could not load".into())) })
        }
    }

    #[tokio::test]
    async fn a_follower_logs_the_supervisors_ended_reason_once_at_info() {
        use tracing::instrument::WithSubscriber;

        let logs = test_log::Capture::default();
        follow_switch(
            SwitchHandle::new("ended".into(), Box::new(EndedProgress)),
            "0.2.0".into(),
        )
        .with_subscriber(logs.clone())
        .await;
        let events = logs.0.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].starts_with("INFO "), "{events:?}");
        assert!(events[0].contains("automatic switch ended"), "{events:?}");
        assert!(
            events[0].contains("the new version could not load"),
            "{events:?}"
        );
        assert!(!events[0].contains("may still be running"), "{events:?}");
    }

    #[test]
    fn automatic_warnings_are_path_free_and_repeat_only_when_the_reason_changes() {
        let mut live = Live::default();
        for error in [
            SwitchError::Disabled("/private/policy".into()),
            SwitchError::Untrusted("/private/endpoint".into()),
            SwitchError::Unreachable("/private/socket".into()),
            SwitchError::Update(UpdateError::Io {
                what: "read /private/state",
                error: "denied".into(),
            }),
        ] {
            let reason = live.automatic_warning(Some(&error)).unwrap();
            assert!(!reason.contains("/private"));
            assert!(live.automatic_warning(Some(&error)).is_none());
            assert!(live.automatic_warning(None).is_none());
            assert_eq!(live.automatic_warning(Some(&error)), Some(reason));
        }
        for error in [
            SwitchError::Refused("watching".into()),
            SwitchError::Update(UpdateError::Busy),
            SwitchError::Update(UpdateError::NothingStaged),
            SwitchError::Update(UpdateError::NotNewer {
                running: "0.2.0".into(),
                latest: "0.2.0".into(),
            }),
            SwitchError::UpdatesOff,
            SwitchError::Skipped(Report::SkippedFailed {
                version: "0.2.0".into(),
            }),
            SwitchError::TimedOut,
            SwitchError::Stopped,
            SwitchError::Protocol("/private/reply".into()),
        ] {
            assert!(live.automatic_warning(Some(&error)).is_none());
        }
        let first = SwitchError::Unreachable("NotFound".into());
        let second = SwitchError::Unreachable("ConnectionRefused".into());
        assert!(live.automatic_warning(Some(&first)).is_some());
        assert!(live.automatic_warning(Some(&second)).is_none());
        assert!(live.automatic_warning(Some(&first)).is_none());
        assert!(
            live.automatic_warning(Some(&SwitchError::Untrusted("different category".into())))
                .is_some()
        );
        assert!(live.automatic_warning(Some(&second)).is_some());
    }

    struct QuietProgress {
        done_at: Instant,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        done: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::SwitchProgress for QuietProgress {
        fn next(
            &mut self,
            _deadline: std::time::Instant,
        ) -> crate::fetch::BoxFuture<'_, Result<SwitchEvent, SwitchError>> {
            Box::pin(async move {
                use std::sync::atomic::Ordering;
                self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep_until(
                    self.done_at.min(Instant::now() + Duration::from_secs(120)),
                )
                .await;
                if Instant::now() < self.done_at {
                    return Err(SwitchError::TimedOut);
                }
                self.done.store(true, Ordering::SeqCst);
                Ok(SwitchEvent::Done {
                    result: serde_json::json!({"result": "switched"}),
                })
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_waits_through_signins_and_the_watch_but_has_an_overall_cap() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        for minutes in [16, 60] {
            let start = Instant::now();
            let calls = Arc::new(AtomicUsize::new(0));
            let done = Arc::new(AtomicBool::new(false));
            let progress = QuietProgress {
                done_at: start + Duration::from_secs(minutes * 60),
                calls: calls.clone(),
                done: done.clone(),
            };
            follow_switch(
                SwitchHandle::new("quiet".into(), Box::new(progress)),
                "0.2.0".into(),
            )
            .await;
            assert!(
                calls.load(Ordering::SeqCst) > 1,
                "quiet timeouts were retried"
            );
            assert_eq!(done.load(Ordering::SeqCst), minutes == 16);
            assert_eq!(
                Instant::now() - start,
                Duration::from_secs(minutes.min(30) * 60)
            );
        }
    }
}
