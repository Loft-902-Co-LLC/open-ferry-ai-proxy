//! Finding, checking, staging and switching to a release, and rolling
//! back.
//!
//! A check downloads the latest release's `SHA256SUMS` (at most 64 KiB)
//! and `SHA256SUMS.minisig` (at most 4 KiB), checks the signature with a
//! built-in key, reads the release from the list (see [`crate::release`])
//! and compares its version with the installed one. A build with no key
//! makes no request at all.
//!
//! Staging downloads this target's archive (at most 256 MiB, within 15
//! minutes), checks its SHA-256 against the list, takes the binary out
//! (see [`crate::archive`]), writes it to `versions/<version>/` and runs
//! its `--version`, which must print `open-ferry <version>` within 30
//! seconds. A binary that fails is deleted and its version remembered as
//! failed; automatic checks skip it until a newer release.
//!
//! A switch puts the staged binary in place of the installed one (see
//! [`crate::switch`]), keeping the installed one in `versions/` for a
//! rollback, and only on an install that updates itself (see
//! [`crate::install`]). A rollback switches back to that copy, with no
//! download. Neither restarts anything.
//!
//! Everything that writes holds the update lock; the state file records
//! the outcome (see [`crate::state`]).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use open_ferry_core::config::SelfUpdateMode;
use serde::Serialize;
use url::Url;

use crate::archive::{self, ArchiveError};
use crate::data_dir::{DataDir, NoDataDir, UpdateLock};
use crate::fetch::{self, Fetch, FetchError};
use crate::install::{self, Install, NotSelfUpdating, RealSystem, System};
use crate::keys::{ReleaseKeys, VerifyError};
use crate::receipt::Receipt;
use crate::release::{self, ArchiveKind, Release, ReleaseError, Verdict};
use crate::runner::{ProcessRunner, Runner};
use crate::settings::Settings;
use crate::state::{State, SwitchRecord};
use crate::switch::{ReplaceOnDisk, Switch, SwitchError, SwitchPlan};

/// How large and how long downloads and checks may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most bytes of `SHA256SUMS`.
    pub sums: u64,
    /// The most bytes of `SHA256SUMS.minisig`.
    pub signature: u64,
    /// The most bytes of an archive, and of the binary in it.
    pub archive: u64,
    /// How long downloading the list or its signature may take.
    pub list_timeout: Duration,
    /// How long downloading an archive may take.
    pub archive_timeout: Duration,
    /// How long a binary's `--version` may take.
    pub version_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            sums: 64 * 1024,
            signature: 4 * 1024,
            archive: 256 * 1024 * 1024,
            list_timeout: Duration::from_secs(60),
            archive_timeout: Duration::from_secs(15 * 60),
            version_timeout: Duration::from_secs(30),
        }
    }
}

/// Why an update step failed.
#[derive(Debug)]
pub enum UpdateError {
    /// There is no data directory.
    NoDataDir(NoDataDir),
    /// A file of the data directory couldn't be read or written.
    Io {
        /// What was being done.
        what: &'static str,
        /// What went wrong.
        error: String,
    },
    /// Another update holds the lock.
    Busy,
    /// A download failed.
    Fetch {
        /// What was downloaded.
        what: &'static str,
        /// Why it failed.
        error: FetchError,
    },
    /// The signature wasn't accepted.
    Verify(VerifyError),
    /// The list doesn't give a usable release.
    Release(ReleaseError),
    /// The archive doesn't match its hash.
    Checksum {
        /// The archive's name.
        archive: String,
    },
    /// The archive was refused.
    Archive(ArchiveError),
    /// The staged binary didn't run.
    StagedBinaryFailed {
        /// Its version.
        version: String,
        /// What happened.
        reason: String,
    },
    /// The staged binary isn't the one staged.
    StagedChanged {
        /// Its version.
        version: String,
    },
    /// The install doesn't update itself.
    NotSelfUpdating(NotSelfUpdating),
    /// Nothing is staged to switch to.
    NothingStaged,
    /// There is no earlier version to roll back to.
    NoPrevious,
    /// The switch failed; the installed binary is as it was.
    Switch(SwitchError),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDataDir(error) => write!(f, "updates have no data directory: {error}"),
            Self::Io { what, error } => write!(f, "{what}: {error}"),
            Self::Busy => f.write_str("another update is running"),
            Self::Fetch { what, error } => write!(f, "downloading {what}: {error}"),
            Self::Verify(error) => error.fmt(f),
            Self::Release(error) => error.fmt(f),
            Self::Checksum { archive } => {
                write!(f, "{archive} doesn't match its SHA-256 in SHA256SUMS")
            }
            Self::Archive(error) => error.fmt(f),
            Self::StagedBinaryFailed { version, reason } => write!(
                f,
                "open-ferry {version} didn't run on this machine ({reason}); it is skipped until a newer release"
            ),
            Self::StagedChanged { version } => write!(
                f,
                "the staged open-ferry {version} changed since it was checked; it was removed"
            ),
            Self::NotSelfUpdating(why) => why.fmt(f),
            Self::NothingStaged => f.write_str("no version is staged"),
            Self::NoPrevious => f.write_str(
                "there is no earlier version to roll back to: a rollback needs a switch made by open-ferry update",
            ),
            Self::Switch(error) => write!(f, "the switch failed, and nothing changed: {error}"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<VerifyError> for UpdateError {
    fn from(error: VerifyError) -> Self {
        Self::Verify(error)
    }
}

impl From<ReleaseError> for UpdateError {
    fn from(error: ReleaseError) -> Self {
        Self::Release(error)
    }
}

impl From<ArchiveError> for UpdateError {
    fn from(error: ArchiveError) -> Self {
        Self::Archive(error)
    }
}

fn io_error(what: &'static str) -> impl FnOnce(io::Error) -> UpdateError {
    move |error| UpdateError::Io {
        what,
        error: error.to_string(),
    }
}

/// The latest release, as a check found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The release for this target.
    pub release: Release,
    /// The version installed now.
    pub installed: String,
    /// How the release compares with it.
    pub verdict: Verdict,
    /// The archive's URL.
    pub archive_url: Url,
}

impl Found {
    /// Whether the release is an update.
    pub fn is_newer(&self) -> bool {
        self.verdict == Verdict::Newer
    }
}

/// What an automatic check did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    /// The installed version is the latest (or newer).
    UpToDate {
        /// The latest release's version.
        latest: String,
    },
    /// A newer release is out; this mode or install only reports it.
    Available {
        /// Its version.
        version: String,
        /// Why it wasn't staged: `None` in notify mode on an install that
        /// updates itself.
        why_not: Option<NotSelfUpdating>,
    },
    /// A newer release is staged, for `open-ferry update` to switch to.
    Staged {
        /// Its version.
        version: String,
    },
    /// A newer release failed its check here before, so it is skipped.
    SkippedFailed {
        /// Its version.
        version: String,
    },
    /// A newer release was rolled back from, so it is skipped.
    SkippedRolledBack {
        /// Its version.
        version: String,
    },
}

/// A check's report, and whether it is news: the first report of its
/// version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    /// What the check did.
    pub report: Report,
    /// Whether this version wasn't reported before.
    pub news: bool,
}

impl Report {
    /// The result's name in the state file.
    pub fn result_name(&self) -> &'static str {
        match self {
            Self::UpToDate { .. } => "up-to-date",
            Self::Available { why_not: None, .. } => "update-available",
            Self::Available { .. } => "cannot-update",
            Self::Staged { .. } => "staged",
            Self::SkippedFailed { .. } | Self::SkippedRolledBack { .. } => "skipped",
        }
    }

    /// A line for the log.
    pub fn describe(&self, running: &str) -> String {
        match self {
            Self::UpToDate { latest } => {
                format!("open-ferry is up to date (running {running}, latest {latest})")
            }
            Self::Available {
                version,
                why_not: None,
            } => format!(
                "open-ferry {version} is out (running {running}); run `open-ferry update` to install it"
            ),
            Self::Available {
                version,
                why_not: Some(why),
            } => format!(
                "open-ferry {version} is out (running {running}), but this install doesn't update itself: {why}; see docs/updates.md"
            ),
            Self::Staged { version } => format!(
                "open-ferry {version} is downloaded, checked and staged (running {running}); run `open-ferry update` to switch to it, then restart open-ferry"
            ),
            Self::SkippedFailed { version } => format!(
                "open-ferry {version} is skipped: it didn't run on this machine when it was staged"
            ),
            Self::SkippedRolledBack { version } => format!(
                "open-ferry {version} is skipped: it was rolled back from; `open-ferry update` installs it again"
            ),
        }
    }
}

/// Everything updates use.
#[derive(Clone)]
pub struct Updater {
    /// How files are downloaded.
    pub fetch: Arc<dyn Fetch>,
    /// The release base URL.
    pub base: Url,
    /// The keys trusted.
    pub keys: ReleaseKeys,
    /// The data directory.
    pub data: DataDir,
    /// The machine.
    pub system: Arc<dyn System>,
    /// How a binary's `--version` is run.
    pub runner: Arc<dyn Runner>,
    /// How the binary is replaced.
    pub switch: Arc<dyn Switch>,
    /// The target triple whose archive is used.
    pub target: String,
    /// The running version.
    pub running: String,
    /// The limits.
    pub limits: Limits,
}

/// The base URL `OPEN_FERRY_UPDATE_BASE_URL` sets, else the default.
pub fn base_url_from_environment() -> Result<Url, FetchError> {
    match std::env::var(crate::BASE_URL_ENV) {
        Ok(value) if !value.trim().is_empty() => fetch::parse_base_url(&value),
        _ => fetch::parse_base_url(crate::DEFAULT_BASE_URL),
    }
}

/// Now, as RFC 3339 in UTC.
pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

impl Updater {
    /// An updater for this binary: built-in keys, this machine, its own
    /// target and version, and the base URL from the environment.
    pub fn for_this_binary(fetch: Arc<dyn Fetch>, data: DataDir) -> Result<Self, UpdateError> {
        let base = base_url_from_environment().map_err(|error| UpdateError::Fetch {
            what: crate::BASE_URL_ENV,
            error,
        })?;
        Ok(Self {
            fetch,
            base,
            keys: ReleaseKeys::built_in().unwrap_or_else(|error| {
                tracing::error!("{error}; this build trusts no release key");
                ReleaseKeys::none()
            }),
            data,
            system: Arc::new(RealSystem { write_probe: true }),
            runner: Arc::new(ProcessRunner),
            switch: Arc::new(ReplaceOnDisk),
            target: crate::TARGET.to_owned(),
            running: crate::CURRENT_VERSION.to_owned(),
            limits: Limits::default(),
        })
    }

    /// The binary's file name for the target.
    pub fn binary_name(&self) -> &'static str {
        release::binary_name(&self.target)
    }

    /// Takes the update lock.
    pub fn lock(&self) -> Result<UpdateLock, UpdateError> {
        self.data
            .try_lock()
            .map_err(io_error("take the update lock"))?
            .ok_or(UpdateError::Busy)
    }

    /// The state.
    pub fn state(&self) -> State {
        State::load(&self.data.state_file())
    }

    /// Writes `state`.
    pub fn save(&self, state: &State) -> Result<(), UpdateError> {
        state
            .save(&self.data.state_file())
            .map_err(io_error("write the update state"))
    }

    /// Whether this install updates itself.
    pub fn install(&self) -> Install {
        install::assess(
            self.system.as_ref(),
            Receipt::load(&self.data.receipt_file()),
        )
    }

    /// The version installed now: the last switch's, while the binary is
    /// still the one it put there, else the running one.
    pub fn installed_version(&self, state: &State) -> String {
        let Some(switch) = &state.last_switch else {
            return self.running.clone();
        };
        if switch.to == self.running {
            return self.running.clone();
        }
        match fs::metadata(&switch.binary) {
            Ok(metadata)
                if metadata.len() == switch.size
                    && modified_ms(&metadata) == Some(switch.modified_ms) =>
            {
                switch.to.clone()
            }
            _ => self.running.clone(),
        }
    }

    /// Downloads and checks the latest release's list, and reads this
    /// target's release from it. With no trusted key, nothing is
    /// downloaded.
    pub async fn latest(&self, state: &State) -> Result<Found, UpdateError> {
        if self.keys.is_empty() {
            return Err(VerifyError::NoTrustedKey.into());
        }
        let fetch_error = |what| move |error| UpdateError::Fetch { what, error };
        let sums_url = fetch::join(&self.base, "latest/download/SHA256SUMS")
            .map_err(fetch_error("SHA256SUMS"))?;
        let signature_url = fetch::join(&self.base, "latest/download/SHA256SUMS.minisig")
            .map_err(fetch_error("SHA256SUMS.minisig"))?;
        let sums = self
            .fetch
            .get(&sums_url, self.limits.sums, self.limits.list_timeout)
            .await
            .map_err(fetch_error("SHA256SUMS"))?;
        let signature = self
            .fetch
            .get(
                &signature_url,
                self.limits.signature,
                self.limits.list_timeout,
            )
            .await
            .map_err(fetch_error("SHA256SUMS.minisig"))?;
        let signature = String::from_utf8(signature).map_err(|_| VerifyError::Malformed)?;
        let trusted_comment = self.keys.verify(&sums, &signature)?;
        let release = release::read(&sums, &trusted_comment, &self.target)?;
        let installed = self.installed_version(state);
        let verdict = release::compare(&installed, &release.semver);
        let archive_url = fetch::join(
            &self.base,
            &format!("download/v{}/{}", release.version, release.archive),
        )
        .map_err(fetch_error("the archive"))?;
        Ok(Found {
            release,
            installed,
            verdict,
            archive_url,
        })
    }

    /// The staged binary of `version`, if it is there and unchanged.
    fn staged_binary(&self, state: &State, version: &str) -> Option<PathBuf> {
        if state.staged.as_deref() != Some(version) {
            return None;
        }
        let path = self.data.binary(version, self.binary_name());
        let data = fs::read(&path).ok()?;
        (Some(release::sha256_hex(&data)) == state.staged_sha256).then_some(path)
    }

    /// Runs `binary --version` and checks it is `version`.
    async fn try_binary(&self, binary: &Path, version: &str) -> Result<(), String> {
        let printed = self
            .runner
            .version(binary, self.limits.version_timeout)
            .await?;
        let expected = format!("open-ferry {version}");
        if printed == expected {
            Ok(())
        } else {
            Err(format!("it printed {printed:?}, not {expected:?}"))
        }
    }

    /// Downloads, checks and stages `found`'s release, recording it in
    /// `state`, unless it is staged already. The caller holds the lock.
    pub async fn stage(&self, found: &Found, state: &mut State) -> Result<PathBuf, UpdateError> {
        let release = &found.release;
        let version = release.version.as_str();
        if let Some(path) = self.staged_binary(state, version) {
            return Ok(path);
        }
        let archive = self
            .fetch
            .get(
                &found.archive_url,
                self.limits.archive,
                self.limits.archive_timeout,
            )
            .await
            .map_err(|error| UpdateError::Fetch {
                what: "the archive",
                error,
            })?;
        if release::sha256_hex(&archive) != release.sha256 {
            return Err(UpdateError::Checksum {
                archive: release.archive.clone(),
            });
        }
        let binary = archive::extract_binary(
            &archive,
            ArchiveKind::of(&self.target),
            &format!("open-ferry-{version}-{}", self.target),
            self.binary_name(),
            self.limits.archive,
        )?;
        drop(archive);

        let dir = self.data.version_dir(version);
        let path = self.data.binary(version, self.binary_name());
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(io_error("clear the staging directory"))?;
        }
        fs::create_dir_all(&dir).map_err(io_error("make the staging directory"))?;
        let partial = dir.join(format!("{}.partial", self.binary_name()));
        fs::write(&partial, &binary).map_err(io_error("write the staged binary"))?;
        make_executable(&partial).map_err(io_error("make the staged binary executable"))?;
        fs::rename(&partial, &path).map_err(io_error("write the staged binary"))?;

        if let Err(reason) = self.try_binary(&path, version).await {
            let _ = fs::remove_dir_all(&dir);
            state.mark_failed(version);
            if state.staged.as_deref() == Some(version) {
                state.staged = None;
                state.staged_sha256 = None;
            }
            self.save(state)?;
            return Err(UpdateError::StagedBinaryFailed {
                version: version.to_owned(),
                reason,
            });
        }
        state.staged = Some(version.to_owned());
        state.staged_sha256 = Some(release::sha256_hex(&binary));
        self.prune(state);
        self.save(state)?;
        Ok(path)
    }

    /// The automatic check, in `mode` (`auto` or `notify`): finds the
    /// latest release and, in `auto` on an install that updates itself,
    /// stages it. It records the outcome in the state, holding the lock.
    pub async fn check(&self, mode: SelfUpdateMode) -> Result<CheckResult, UpdateError> {
        let _lock = self.lock()?;
        let mut state = self.state();
        let outcome = self.check_locked(mode, &mut state).await;
        state.last_check = Some(now());
        match &outcome {
            Ok(result) => {
                state.last_result = Some(result.report.result_name().to_owned());
                state.last_error = None;
            }
            Err(error) => {
                state.last_result = Some("error".to_owned());
                state.last_error = Some(error.to_string());
            }
        }
        self.save(&state)?;
        outcome
    }

    async fn check_locked(
        &self,
        mode: SelfUpdateMode,
        state: &mut State,
    ) -> Result<CheckResult, UpdateError> {
        let found = self.latest(state).await?;
        let version = found.release.version.clone();
        state.latest = Some(version.clone());
        let report = if !found.is_newer() {
            Report::UpToDate { latest: version }
        } else if state.has_failed(&version) {
            Report::SkippedFailed { version }
        } else if state.rolled_back.as_deref() == Some(version.as_str()) {
            Report::SkippedRolledBack { version }
        } else {
            match (mode, self.install()) {
                (SelfUpdateMode::Auto, Install::SelfUpdating { .. }) => {
                    self.stage(&found, state).await?;
                    Report::Staged { version }
                }
                (_, install) => Report::Available {
                    version,
                    why_not: install.why_not().cloned(),
                },
            }
        };
        let news = match &report {
            Report::UpToDate { .. } => false,
            Report::Available { version, .. }
            | Report::Staged { version }
            | Report::SkippedFailed { version }
            | Report::SkippedRolledBack { version } => {
                let news = state.notified.as_deref() != Some(version.as_str());
                state.notified = Some(version.clone());
                news
            }
        };
        Ok(CheckResult { report, news })
    }

    /// Switches the installed binary to the staged version. The caller
    /// holds the lock.
    pub fn switch_to_staged(&self, state: &mut State) -> Result<SwitchRecord, UpdateError> {
        let installed = match self.install() {
            Install::SelfUpdating { binary } => binary,
            Install::NotifyOnly(why) => return Err(UpdateError::NotSelfUpdating(why)),
        };
        let version = state.staged.clone().ok_or(UpdateError::NothingStaged)?;
        let Some(staged) = self.staged_binary(state, &version) else {
            let _ = fs::remove_dir_all(self.data.version_dir(&version));
            state.staged = None;
            state.staged_sha256 = None;
            self.save(state)?;
            return Err(UpdateError::StagedChanged { version });
        };
        let sha256 = state.staged_sha256.clone().unwrap_or_default();
        let from = self.installed_version(state);
        let record = self.switch(&from, &version, &staged, &installed, &sha256, "update")?;
        state.previous = Some(from);
        state.staged = None;
        state.staged_sha256 = None;
        if state.rolled_back.as_deref() == Some(version.as_str()) {
            state.rolled_back = None;
        }
        state.last_switch = Some(record.clone());
        self.prune(state);
        self.save(state)?;
        Ok(record)
    }

    /// Switches back to the version before the last switch, from its copy
    /// in the data directory. The caller holds the lock.
    pub async fn rollback(&self, state: &mut State) -> Result<SwitchRecord, UpdateError> {
        let installed = match self.install() {
            Install::SelfUpdating { binary } => binary,
            Install::NotifyOnly(why) => return Err(UpdateError::NotSelfUpdating(why)),
        };
        let previous = state.previous.clone().ok_or(UpdateError::NoPrevious)?;
        let binary = self.data.binary(&previous, self.binary_name());
        if !binary.is_file() {
            return Err(UpdateError::NoPrevious);
        }
        self.try_binary(&binary, &previous)
            .await
            .map_err(|reason| UpdateError::StagedBinaryFailed {
                version: previous.clone(),
                reason,
            })?;
        let data = fs::read(&binary).map_err(io_error("read the earlier binary"))?;
        let sha256 = release::sha256_hex(&data);
        let from = self.installed_version(state);
        let record = self.switch(&from, &previous, &binary, &installed, &sha256, "rollback")?;
        state.previous = Some(from.clone());
        state.rolled_back = Some(from.clone());
        if state.staged.as_deref() == Some(from.as_str()) {
            state.staged = None;
            state.staged_sha256 = None;
        }
        state.last_switch = Some(record.clone());
        self.prune(state);
        self.save(state)?;
        Ok(record)
    }

    fn switch(
        &self,
        from: &str,
        to: &str,
        new_binary: &Path,
        installed: &Path,
        sha256: &str,
        how: &str,
    ) -> Result<SwitchRecord, UpdateError> {
        let plan = SwitchPlan {
            from: from.to_owned(),
            to: to.to_owned(),
            new_binary: new_binary.to_path_buf(),
            installed: installed.to_path_buf(),
            keep_installed_at: Some(self.data.binary(from, self.binary_name())),
        };
        self.switch.switch(&plan).map_err(UpdateError::Switch)?;
        let metadata = fs::metadata(installed).ok();
        Ok(SwitchRecord {
            from: from.to_owned(),
            to: to.to_owned(),
            at: now(),
            how: how.to_owned(),
            binary: installed.display().to_string(),
            sha256: sha256.to_owned(),
            size: metadata.as_ref().map_or(0, fs::Metadata::len),
            modified_ms: metadata.as_ref().and_then(modified_ms).unwrap_or(0),
            restart_needed: to != self.running,
        })
    }

    /// Removes the kept versions but the installed one, the one before it
    /// and the staged one.
    fn prune(&self, state: &State) {
        let installed = self.installed_version(state);
        let keep = [
            Some(installed.as_str()),
            Some(self.running.as_str()),
            state.previous.as_deref(),
            state.staged.as_deref(),
        ];
        for version in self.data.kept_versions() {
            if !keep.contains(&Some(version.as_str())) {
                let _ = fs::remove_dir_all(self.data.version_dir(&version));
            }
        }
    }

    /// The status, for `settings`.
    pub fn status(&self, settings: &Settings, install: &Install) -> Status {
        let state = self.state();
        let installed = self.installed_version(&state);
        let update_available = state.latest.as_deref().is_some_and(|latest| {
            semver::Version::parse(latest)
                .is_ok_and(|latest| release::compare(&installed, &latest) == Verdict::Newer)
        });
        Status {
            mode: settings.mode.as_str(),
            mode_source: settings.source.as_str(),
            updates: settings.describe(),
            check_every_seconds: settings.check_every.as_secs(),
            running_version: self.running.clone(),
            installed_version: installed.clone(),
            restart_needed: installed != self.running,
            target: self.target.clone(),
            latest_version: state.latest.clone(),
            update_available,
            staged_version: state.staged.clone(),
            previous_version: state.previous.clone(),
            failed_versions: state.failed.clone(),
            rolled_back_version: state.rolled_back.clone(),
            last_check: state.last_check.clone(),
            last_result: state.last_result.clone(),
            last_error: state.last_error.clone(),
            next_check: None,
            checking: false,
            can_update_itself: install.can_update_itself(),
            why_not: install.why_not().map(ToString::to_string),
            why_not_code: install.why_not().map(NotSelfUpdating::code),
            trusts_release_key: !self.keys.is_empty(),
            notes: settings.notes.clone(),
        }
    }
}

/// A file's modification time, in milliseconds since the epoch.
fn modified_ms(metadata: &fs::Metadata) -> Option<i64> {
    let since = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// Makes `path` executable by everyone, as the installers do.
#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// What updates are doing, for the status route and `open-ferry update
/// --check --json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    /// `auto`, `notify` or `off`.
    pub mode: &'static str,
    /// What set it: `default`, `config` or `environment`.
    pub mode_source: &'static str,
    /// How the mode reads: `on`, `notify-only` or `off`.
    pub updates: &'static str,
    /// The interval between checks, in seconds.
    pub check_every_seconds: u64,
    /// The version running.
    pub running_version: String,
    /// The version installed, after a switch the one a restart runs.
    pub installed_version: String,
    /// Whether a restart is needed to run the installed version.
    pub restart_needed: bool,
    /// The target triple.
    pub target: String,
    /// The latest release the last check found.
    pub latest_version: Option<String>,
    /// Whether that is newer than the installed version.
    pub update_available: bool,
    /// The version staged for `open-ferry update` to switch to.
    pub staged_version: Option<String>,
    /// The version kept for a rollback.
    pub previous_version: Option<String>,
    /// Versions that failed their check here, skipped until a newer one.
    pub failed_versions: Vec<String>,
    /// A version rolled back from, which automatic updates skip.
    pub rolled_back_version: Option<String>,
    /// When the last check ended.
    pub last_check: Option<String>,
    /// How it ended.
    pub last_result: Option<String>,
    /// Its error.
    pub last_error: Option<String>,
    /// When the server checks next; `None` when it doesn't.
    pub next_check: Option<String>,
    /// Whether a check is running.
    pub checking: bool,
    /// Whether the install replaces its own binary.
    pub can_update_itself: bool,
    /// Why it doesn't.
    pub why_not: Option<String>,
    /// The reason's code: `container`, `no-receipt`, `bad-receipt`,
    /// `unknown-binary`, `other-name`, `other-binary` or `read-only`.
    pub why_not_code: Option<&'static str>,
    /// Whether the build trusts a release key; without one it can't
    /// update.
    pub trusts_release_key: bool,
    /// Notes on the settings.
    pub notes: Vec<String>,
}
