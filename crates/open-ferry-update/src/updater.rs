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

use std::collections::BTreeSet;
use std::fmt;
use std::io::Write;

use crate::journal::{Journal, JournalView, add_newest_other};
use crate::pins::PinStore;
use crate::staging::{StagingDir, hash_file, rename_directory_exclusive, sync_directory};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use open_ferry_core::config::{SelfUpdateChannel, SelfUpdateMode};
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
use crate::switch::{KeepPolicy, ReplaceOnDisk, Switch, SwitchError, SwitchPlan};

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
    /// The journal or a pin prevents this mutation.
    Journal(String),
    /// Neither release source could be used, or a pointer was malformed.
    Discovery(String),
    /// The configured channel changed during preparation.
    ChannelChanged,
    /// An immutable version is in use with different bytes.
    VersionInUse(String),
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
    /// The latest release is already running, or the running version is newer.
    NotNewer { running: String, latest: String },
    /// Nothing is staged to switch to.
    NothingStaged,
    /// The rollback target changed after the caller confirmed it.
    RollbackChanged {
        expected: String,
        /// The current version, when known; absent or undisclosed otherwise.
        actual: Option<String>,
    },
    /// There is no earlier version to roll back to.
    NoPrevious,
    /// The switch failed; the installed binary is as it was.
    Switch(SwitchError),
    /// A supervisor runs from the installed binary, or whether one does can't
    /// be told: nothing was changed. The text says why and what to do.
    Supervised(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDataDir(error) => write!(f, "updates have no data directory: {error}"),
            Self::Io { what, error } => write!(f, "{what}: {error}"),
            Self::Busy => f.write_str("another update is running"),
            Self::Journal(error) | Self::Discovery(error) => f.write_str(error),
            Self::ChannelChanged => f.write_str("the update channel changed while preparing the release; check again before installing"),
            Self::VersionInUse(version) => write!(f, "version {version} is in use and differs"),
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
                "the staged open-ferry {version} changed since it was checked"
            ),
            Self::NotSelfUpdating(why) => why.fmt(f),
            Self::NotNewer { running, latest } if running == latest => write!(f, "already running the latest release, open-ferry {running}"),
            Self::NotNewer { running, latest } => write!(f, "open-ferry {running} is already running; the latest release is {latest}, which is not newer"),
            Self::NothingStaged => f.write_str("no version is staged"),
            Self::RollbackChanged { expected, actual: Some(actual) } => write!(f,
                "rollback target changed: expected {expected}, now {actual}; run `open-ferry update -check`"),
            Self::RollbackChanged { expected, actual: None } => write!(f,
                "rollback target changed: the confirmed target {expected} no longer matches; run `open-ferry update -check`"),
            Self::NoPrevious => f.write_str(
                "there is no earlier version to roll back to: a rollback needs a switch made by open-ferry update",
            ),
            Self::Switch(error) => write!(f, "the switch failed, and nothing changed: {error}"),
            Self::Supervised(message) => f.write_str(message),
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

fn blocking_error(what: &'static str) -> impl FnOnce(tokio::task::JoinError) -> UpdateError {
    move |error| UpdateError::Io {
        what,
        error: error.to_string(),
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
    /// Channel used for discovery.
    pub channel: SelfUpdateChannel,
    /// latest or preview.
    pub source: &'static str,
    /// A source failure that did not prevent discovery.
    pub warning: Option<String>,
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
    /// Channel used for every discovery, including supervised preparation.
    pub channel: SelfUpdateChannel,
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
            channel: Default::default(),
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

    /// Atomically mutate fresh reporting state while holding the update lock.
    /// Callers must not already hold that lock, or wait on an endpoint here.
    pub fn mutate_state<T>(&self, change: impl FnOnce(&mut State) -> T) -> Result<T, UpdateError> {
        let _lock = self.lock()?;
        self.mutate_state_locked("mutation", change)
    }

    /// Like `mutate_state`, with the state-write name used by fault tests.
    pub fn mutate_state_named<T>(
        &self,
        write: &str,
        change: impl FnOnce(&mut State) -> T,
    ) -> Result<T, UpdateError> {
        let _lock = self.lock()?;
        self.mutate_state_locked(write, change)
    }

    fn mutate_state_locked<T>(
        &self,
        write: &str,
        change: impl FnOnce(&mut State) -> T,
    ) -> Result<T, UpdateError> {
        let mut state = self.state();
        let result = change(&mut state);
        state
            .save_named(&self.data.state_file(), write)
            .map_err(io_error("write the update state"))?;
        Ok(result)
    }

    /// Only after a binary replacement has committed: a landed reporting
    /// write is success even if its final durability acknowledgement fails.
    fn mutate_state_committed_locked<T>(
        &self,
        write: &str,
        change: impl FnOnce(&mut State) -> T,
    ) -> Result<T, UpdateError> {
        let mut state = self.state();
        let result = change(&mut state);
        state
            .save_committed_named(&self.data.state_file(), write)
            .map_err(io_error("write the committed switch state"))?;
        Ok(result)
    }

    /// The same freshly computed retention policy guards every destructive
    /// staging site and pruning. The caller holds the update lock.
    fn retention_keep(&self, state: &State) -> Result<BTreeSet<String>, UpdateError> {
        let fresh = self.state();
        let mut extra = vec![
            self.running.clone(),
            self.installed_version(state),
            self.installed_version(&fresh),
        ];
        extra.extend(state.previous.iter().cloned());
        extra.extend(state.staged.iter().cloned());
        retention_keep(&self.data, extra)
    }

    /// Cleanup under the update lock, only after journal/pin preflight.
    fn cleanup_stale_artifacts(&self, keep: &BTreeSet<String>) -> Result<(), UpdateError> {
        cleanup_stale_artifacts_in(&self.data, self.binary_name(), keep)
    }

    async fn cleanup_stale_artifacts_async(
        &self,
        keep: &BTreeSet<String>,
    ) -> Result<(), UpdateError> {
        let data = self.data.clone();
        let name = self.binary_name();
        let keep = keep.clone();
        let planned = tokio::task::spawn_blocking(move || {
            #[cfg(feature = "blue-green-test")]
            crate::test_seams::hit(data.root(), "stage.before_cleanup_hash")
                .map_err(io_error("wait before checking retained binaries"))?;
            let verified = verified_stale_artifacts(&data, name, &keep)?;
            let planned = if let Ok(Some(receipt)) = Receipt::load(&data.receipt_file()) {
                #[cfg(feature = "blue-green-test")]
                crate::test_seams::before_prev_hash(data.root())
                    .map_err(io_error("wait before checking stale installed links"))?;
                let plan = crate::switch::plan_stale_prev(Path::new(&receipt.binary), &verified);
                Some((receipt.binary, plan))
            } else {
                None
            };
            Ok::<_, UpdateError>(planned)
        })
        .await
        .map_err(blocking_error("check retained binaries"))??;
        // Destructive cleanup stays with the lock-owning caller. The blocking
        // preflight above can outlive cancellation but cannot delete anything.
        cleanup_stale_staging_in(&self.data);
        if let Some((installed, plan)) = planned {
            log_stale_prev_cleanup(&installed, crate::switch::apply_stale_prev(plan));
        }
        Ok(())
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
        let latest = self.read_release(state, None).await;
        if self.channel != SelfUpdateChannel::Preview {
            return latest;
        }
        let preview = self.preview(state).await;
        let (mut found, warning) = match (latest, preview) {
            (Ok(latest), Ok(Some(preview))) => {
                if preview.release.semver > latest.release.semver {
                    (preview, None)
                } else {
                    (latest, None)
                }
            }
            (Ok(latest), Ok(None)) => (latest, None),
            (Ok(latest), Err(error)) => (latest, Some(format!("preview source: {error}"))),
            (Err(error), Ok(Some(preview))) => (preview, Some(format!("latest source: {error}"))),
            (Err(error), Ok(None)) => return Err(error),
            (Err(latest), Err(preview)) => {
                return Err(UpdateError::Discovery(format!(
                    "latest source: {latest}; preview source: {preview}"
                )));
            }
        };
        if let Some(warning) = &warning {
            tracing::warn!("{warning}");
        }
        found.warning = warning;
        Ok(found)
    }

    async fn preview(&self, state: &State) -> Result<Option<Found>, UpdateError> {
        let url = fetch::join(&self.base, "download/channel-preview/VERSION").map_err(|error| {
            UpdateError::Fetch {
                what: "preview VERSION",
                error,
            }
        })?;
        let bytes = match self.fetch.get(&url, 64, self.limits.list_timeout).await {
            Ok(bytes) => bytes,
            Err(FetchError::Status(404)) => return Ok(None),
            Err(error) => {
                return Err(UpdateError::Fetch {
                    what: "preview VERSION",
                    error,
                });
            }
        };
        let version = std::str::from_utf8(&bytes).ok()
            .and_then(|text| text.strip_suffix('\n'))
            .filter(|version| bytes.len() <= 64 && release::valid_version(version)
                && semver::Version::parse(version).is_ok())
            .ok_or_else(|| UpdateError::Discovery(
                "VERSION must contain one semver version followed by a newline, at most 64 bytes".into()))?;
        self.read_release(state, Some(version)).await.map(Some)
    }

    async fn read_release(
        &self,
        state: &State,
        version: Option<&str>,
    ) -> Result<Found, UpdateError> {
        let fetch_error = |what| move |error| UpdateError::Fetch { what, error };
        let path = version.map_or_else(
            || "latest/download".to_owned(),
            |v| format!("download/v{v}"),
        );
        let sums_url = fetch::join(&self.base, &format!("{path}/SHA256SUMS"))
            .map_err(fetch_error("SHA256SUMS"))?;
        let signature_url = fetch::join(&self.base, &format!("{path}/SHA256SUMS.minisig"))
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
        if let Some(expected) = version
            && release.version != expected
        {
            return Err(UpdateError::Discovery(format!(
                "preview VERSION names {expected}, but the signed release is {}",
                release.version
            )));
        }
        let installed = self.installed_version(state);
        let verdict = release::compare_channel(
            &installed,
            &release.semver,
            self.channel == SelfUpdateChannel::Preview,
        );
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
            channel: self.channel,
            source: if version.is_some() {
                "preview"
            } else {
                "latest"
            },
            warning: None,
        })
    }

    /// Record discovery alongside the version, so stale state never changes policy.
    pub fn record_found(state: &mut State, found: &Found) {
        state.latest = Some(found.release.version.clone());
        state.channel = Some(found.channel.as_str().to_owned());
        state.release_source = Some(found.source.to_owned());
        state.channel_error = found.warning.clone();
    }

    /// The staged binary of `version`, if it is there and unchanged.
    fn staged_binary(&self, state: &State, version: &str) -> Option<PathBuf> {
        if state.staged.as_deref() != Some(version) {
            return None;
        }
        let path = self.data.binary(version, self.binary_name());
        (Some(hash_file(&path).ok()?) == state.staged_sha256).then_some(path)
    }

    async fn staged_binary_async(
        &self,
        state: &State,
        version: &str,
    ) -> Result<Option<PathBuf>, UpdateError> {
        if state.staged.as_deref() != Some(version) {
            return Ok(None);
        }
        let updater = self.clone();
        let state = state.clone();
        let version = version.to_owned();
        tokio::task::spawn_blocking(move || {
            #[cfg(feature = "blue-green-test")]
            crate::test_seams::hit(updater.data.root(), "stage.before_staged_hash")
                .map_err(io_error("wait before checking the staged binary"))?;
            Ok(updater.staged_binary(&state, &version))
        })
        .await
        .map_err(blocking_error("check the staged binary"))?
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

    /// Hashing and decompression must not occupy the serving runtime. Only
    /// owned bytes cross this boundary: if the check is cancelled and drops
    /// its update lock, the blocking task cannot publish files or state.
    async fn unpack(
        &self,
        found: &Found,
        archive: Vec<u8>,
    ) -> Result<(Vec<u8>, String), UpdateError> {
        let release = found.release.clone();
        let target = self.target.clone();
        let limit = self.limits.archive;
        #[cfg(feature = "blue-green-test")]
        let root = self.data.root().to_path_buf();
        tokio::task::spawn_blocking(move || {
            #[cfg(feature = "blue-green-test")]
            crate::test_seams::hit(&root, "stage.before_unpack")
                .map_err(io_error("wait before unpacking the archive"))?;
            if release::sha256_hex(&archive) != release.sha256 {
                return Err(UpdateError::Checksum {
                    archive: release.archive,
                });
            }
            let binary = archive::extract_binary(
                &archive,
                ArchiveKind::of(&target),
                &format!("open-ferry-{}-{target}", release.version),
                release::binary_name(&target),
                limit,
            )?;
            drop(archive);
            let sha256 = release::sha256_hex(&binary);
            Ok((binary, sha256))
        })
        .await
        .map_err(|error| UpdateError::Io {
            what: "unpack the release archive",
            error: error.to_string(),
        })?
    }

    /// Downloads, checks and stages `found`'s release, recording it in
    /// `state`, unless it is staged already. The caller holds the lock.
    pub async fn stage(&self, found: &Found, state: &mut State) -> Result<PathBuf, UpdateError> {
        let release = &found.release;
        let version = release.version.as_str();
        if let Some(path) = self.staged_binary_async(state, version).await? {
            return Ok(path);
        }
        let keep = self.retention_keep(state)?;
        self.cleanup_stale_artifacts_async(&keep).await?;
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
        let (binary, sha256) = self.unpack(found, archive).await?;
        let fresh = self.state();
        if fresh.staged.as_deref() == Some(version) {
            if fresh.staged_sha256.as_deref() == Some(sha256.as_str())
                && let Some(path) = self.staged_binary_async(&fresh, version).await?
            {
                *state = fresh;
                return Ok(path);
            }
            // A staged pointer cannot protect its own replacement. Retire it
            // durably first on this restaging path; the independent previous,
            // pin and journal protections remain intact.
            self.mutate_state_locked("staged-changed", |fresh| {
                fresh.staged = None;
                fresh.staged_sha256 = None;
            })?;
            *state = self.state();
        } else if state.staged.as_deref() == Some(version) {
            // A stale caller snapshot must not restore the retired protection.
            *state = fresh;
        }
        let keep = self.retention_keep(state)?;
        let dir = self.data.version_dir(version);
        let path = self.data.binary(version, self.binary_name());
        // A version in the keep set is immutable, even if its current bytes
        // are corrupt: only recovery may decide what is safe to run instead.
        if dir.exists()
            && keep.contains(version)
            && !self.hash_matches(path.clone(), sha256.clone()).await?
        {
            return Err(UpdateError::VersionInUse(version.to_owned()));
        }
        let staging = self.write_staging(binary).await?;
        let staged = staging.0.join(self.binary_name());

        if let Err(reason) = self.try_binary(&staged, version).await {
            self.mutate_state_locked("stage-failed", |fresh| {
                fresh.mark_failed(version);
                if fresh.staged.as_deref() == Some(version) {
                    fresh.staged = None;
                    fresh.staged_sha256 = None;
                }
            })?;
            *state = self.state();
            return Err(UpdateError::StagedBinaryFailed {
                version: version.to_owned(),
                reason,
            });
        }
        if dir.exists() {
            if !self.hash_matches(path.clone(), sha256.clone()).await? {
                // Recompute under the caller's lock immediately before deletion.
                if self.retention_keep(state)?.contains(version) {
                    return Err(UpdateError::VersionInUse(version.to_owned()));
                }
                fs::remove_dir_all(&dir).map_err(io_error("clear an unused version directory"))?;
                rename_directory_exclusive(&staging.0, &dir)
                    .map_err(io_error("publish the staged binary"))?;
            }
        } else {
            rename_directory_exclusive(&staging.0, &dir)
                .map_err(io_error("publish the staged binary"))?;
        }
        let versions = self.data.versions();
        tokio::task::spawn_blocking(move || sync_directory(&versions))
            .await
            .map_err(blocking_error("sync the versions directory"))?
            .map_err(io_error("sync the versions directory"))?;
        self.mutate_state_locked("stage", |fresh| {
            fresh.latest = Some(version.to_owned());
            Self::record_found(fresh, found);
            fresh.staged = Some(version.to_owned());
            fresh.staged_sha256 = Some(sha256);
        })?;
        *state = self.state();
        if let Err(error) = self.prune_async(state).await {
            tracing::warn!(%error, "the update committed, but version cleanup was refused");
        }
        Ok(path)
    }

    /// Own only a private, unpublished directory in the blocking task. On
    /// cancellation the task (or its discarded return value) drops that guard
    /// and removes its files. It cannot rename a version or write state after
    /// the caller has released its update lock. Probe and publication remain
    /// with the caller, after all writes and syncs have completed.
    async fn write_staging(&self, binary: Vec<u8>) -> Result<StagingDir, UpdateError> {
        let versions = self.data.versions();
        let name = self.binary_name();
        #[cfg(feature = "blue-green-test")]
        let root = self.data.root().to_owned();
        tokio::task::spawn_blocking(move || {
            let staging =
                StagingDir::create(&versions).map_err(io_error("make the staging directory"))?;
            #[cfg(feature = "blue-green-test")]
            crate::test_seams::hit(&root, "stage.before_write")
                .map_err(io_error("wait before writing the staged binary"))?;
            let staged = staging.0.join(name);
            let mut file = crate::staging::create_staged_file(&staged)
                .map_err(io_error("write the staged binary"))?;
            file.write_all(&binary)
                .map_err(io_error("write the staged binary"))?;
            make_executable(&staged).map_err(io_error("make the staged binary executable"))?;
            #[cfg(feature = "blue-green-test")]
            crate::test_seams::hit(&root, "stage.before_sync")
                .map_err(io_error("wait before syncing the staged binary"))?;
            file.sync_all()
                .map_err(io_error("sync the staged binary"))?;
            drop(file);
            sync_directory(&staging.0).map_err(io_error("sync the staging directory"))?;
            Ok(staging)
        })
        .await
        .map_err(blocking_error("write and sync the staged binary"))?
    }

    /// Read-only owned work is safe even if the check drops its lock while
    /// the hash is in flight. Recheck every byte; metadata is not integrity.
    async fn hash_matches(&self, path: PathBuf, expected: String) -> Result<bool, UpdateError> {
        #[cfg(feature = "blue-green-test")]
        let root = self.data.root().to_path_buf();
        tokio::task::spawn_blocking(move || {
            #[cfg(feature = "blue-green-test")]
            let _ = crate::test_seams::hit(&root, "stage.before_hash_match");
            hash_file(&path).ok().as_deref() == Some(expected.as_str())
        })
        .await
        .map_err(blocking_error("hash the staged binary"))
    }

    /// The automatic check, in `mode` (`auto` or `notify`): finds the
    /// latest release and, in `auto` on an install that updates itself,
    /// stages it. It records the outcome in the state, holding the lock.
    pub async fn check(&self, mode: SelfUpdateMode) -> Result<CheckResult, UpdateError> {
        self.check_with_pending(mode, false, &self.install()).await
    }

    /// Supervised checks include pending retry exclusions, read-only. Ordinary
    /// check reporting still updates only its own fields in fresh state.
    pub(crate) async fn check_with_pending(
        &self,
        mode: SelfUpdateMode,
        supervised: bool,
        install: &Install,
    ) -> Result<CheckResult, UpdateError> {
        let _lock = self.lock()?;
        let mut state = self.state();
        let pending = if supervised {
            crate::switching::pending_exclusions(self)
        } else {
            Default::default()
        };
        let outcome = self.check_locked(mode, install, &mut state, &pending).await;
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
        self.mutate_state_locked("check", |fresh| {
            fresh.last_check = state.last_check;
            fresh.last_result = state.last_result;
            fresh.last_error = state.last_error;
            fresh.latest = state.latest;
            fresh.channel = state.channel;
            fresh.release_source = state.release_source;
            fresh.channel_error = state.channel_error;
            fresh.notified = state.notified;
        })?;
        outcome
    }

    async fn check_locked(
        &self,
        mode: SelfUpdateMode,
        install: &Install,
        state: &mut State,
        pending: &crate::switching::Exclusions,
    ) -> Result<CheckResult, UpdateError> {
        let found = self.latest(state).await?;
        let version = found.release.version.clone();
        Self::record_found(state, &found);
        let report = if !found.is_newer() {
            Report::UpToDate { latest: version }
        } else if let Some(report) = pending.report(state, &version) {
            report
        } else {
            match (mode, install) {
                (SelfUpdateMode::Auto, Install::SelfUpdating { .. }) => {
                    let staged = self.stage(&found, state).await;
                    state.latest = Some(version.clone());
                    staged?;
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

    /// Validate a supervised request without changing state or deleting files.
    /// The caller holds the update lock and supplies the requested hash.
    pub fn validate_staged(&self, version: &str, sha256: &str) -> Result<PathBuf, UpdateError> {
        self.retention_keep(&self.state())?;
        let state = self.state();
        if state.staged.as_deref() != Some(version)
            || state.staged_sha256.as_deref() != Some(sha256)
        {
            return Err(UpdateError::NothingStaged);
        }
        self.staged_binary(&state, version)
            .ok_or_else(|| UpdateError::StagedChanged {
                version: version.to_owned(),
            })
    }

    /// Hash-check and version-check the journal's rollback artifact.
    /// The caller holds the update lock; this does not change the state.
    pub async fn validate_previous(
        &self,
        artifact: &crate::journal::Artifact,
    ) -> Result<PathBuf, UpdateError> {
        self.retention_keep(&self.state())?;
        let binary = self.data.binary(&artifact.version, self.binary_name());
        if !self
            .hash_matches(binary.clone(), artifact.sha256.clone())
            .await?
        {
            return Err(UpdateError::StagedChanged {
                version: artifact.version.clone(),
            });
        }
        self.try_binary(&binary, &artifact.version)
            .await
            .map_err(|reason| UpdateError::StagedBinaryFailed {
                version: artifact.version.clone(),
                reason,
            })?;
        Ok(binary)
    }

    /// Protect a staged artifact before releasing the update lock. Never call
    /// the supervisor endpoint until the caller's lock has been dropped.
    pub fn pin_staged(&self, version: &str, sha256: &str) -> Result<crate::pins::Pin, UpdateError> {
        self.validate_staged(version, sha256)?;
        PinStore::new(&self.data.versions())
            .create(version, sha256, Utc::now())
            .map_err(io_error("pin the staged binary"))
    }

    /// Switches the installed binary to the staged version. The caller
    /// holds the lock.
    pub fn switch_to_staged(&self, state: &mut State) -> Result<SwitchRecord, UpdateError> {
        self.retention_keep(state)?;
        *state = self.state();
        let installed = match self.install() {
            Install::SelfUpdating { binary } => binary,
            Install::NotifyOnly(why) => return Err(UpdateError::NotSelfUpdating(why)),
        };
        let version = state.staged.clone().ok_or(UpdateError::NothingStaged)?;
        let Some(staged) = self.staged_binary(state, &version) else {
            // Retire the corrupt staging reference durably before evaluating
            // deletion. Other state retention, pins and journal references
            // still protect it through the same policy used by prune.
            self.mutate_state_locked("staged-changed", |fresh| {
                fresh.staged = None;
                fresh.staged_sha256 = None;
            })?;
            *state = self.state();
            if !self.retention_keep(state)?.contains(&version) {
                let _ = fs::remove_dir_all(self.data.version_dir(&version));
            }
            return Err(UpdateError::StagedChanged { version });
        };
        let sha256 = state.staged_sha256.clone().unwrap_or_default();
        let from = self.installed_version(state);
        let record = self.switch(&from, &version, &staged, &installed, &sha256, "update")?;
        self.mutate_state_committed_locked("unsupervised-switch", |fresh| {
            fresh.previous = Some(from);
            fresh.staged = None;
            fresh.staged_sha256 = None;
            if fresh.rolled_back.as_deref() == Some(version.as_str()) {
                fresh.rolled_back = None;
            }
            fresh.upsert_record(record.clone());
        })?;
        *state = self.state();
        if let Err(error) = self.prune(state) {
            tracing::warn!(%error, "the update committed, but version cleanup was refused");
        }
        Ok(record)
    }

    /// Switches back to the version before the last switch, from its copy
    /// in the data directory. The caller holds the lock.
    pub async fn rollback(&self, state: &mut State) -> Result<SwitchRecord, UpdateError> {
        self.retention_keep(&self.state())?;
        *state = self.state();
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
        self.mutate_state_committed_locked("unsupervised-rollback", |fresh| {
            fresh.previous = Some(from.clone());
            fresh.rolled_back = Some(from.clone());
            if fresh.staged.as_deref() == Some(from.as_str()) {
                fresh.staged = None;
                fresh.staged_sha256 = None;
            }
            fresh.upsert_record(record.clone());
        })?;
        *state = self.state();
        if let Err(error) = self.prune(state) {
            tracing::warn!(%error, "the update committed, but version cleanup was refused");
        }
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
        let state = self.state();
        self.retention_keep(&state)?;
        let kept = self.data.binary(from, self.binary_name());
        let journal_present = self
            .data
            .root()
            .join(crate::journal::JOURNAL_FILE)
            .try_exists()
            .map_err(|error| UpdateError::Journal(error.to_string()))?;
        if !journal_present {
            let pins = PinStore::new(&self.data.versions())
                .live(Utc::now())
                .map_err(|error| UpdateError::Journal(error.to_string()))?;
            if from == to
                || state.previous.as_deref() == Some(from)
                || state.staged.as_deref() == Some(from)
                || pins.iter().any(|pin| pin.version == from)
            {
                return Err(UpdateError::VersionInUse(from.to_owned()));
            }
            let new_path = fs::canonicalize(new_binary)
                .map_err(io_error("resolve the new binary before switching"))?;
            match fs::canonicalize(&kept) {
                Ok(kept_path) if kept_path == new_path => {
                    return Err(UpdateError::VersionInUse(from.to_owned()));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io_error("resolve the kept binary before switching")(error));
                }
            }
        }
        let installed_hash = crate::installed_file::hash_installed(installed)
            .map_err(io_error("hash the installed binary"))?;
        if journal_present
            && kept.exists()
            && let Some(installed_hash) = &installed_hash
            && hash_file(&kept).ok().as_deref() != Some(installed_hash.as_str())
        {
            return Err(UpdateError::VersionInUse(from.to_owned()));
        }
        // Even an existing immutable copy needs durability acknowledged again.
        let keep_installed_at = Some(kept);
        let plan = SwitchPlan {
            from: from.to_owned(),
            to: to.to_owned(),
            new_binary: new_binary.to_path_buf(),
            installed: installed.to_path_buf(),
            keep_installed_at,
            keep_root: Some(self.data.root().to_path_buf()),
            keep_policy: if journal_present {
                KeepPolicy::Immutable
            } else {
                KeepPolicy::ReplaceMismatched
            },
            preferred_aside: None,
            expected_installed_sha256: installed_hash,
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
            ..SwitchRecord::default()
        })
    }

    /// Prune under the caller's update lock, retaining journal, pins, running,
    /// installed, staged and previous versions from both the caller and fresh state.
    /// An unreadable journal or pin refuses cleanup without deleting a version.
    pub fn prune(&self, state: &State) -> Result<(), UpdateError> {
        let keep = self.retention_keep(state)?;
        self.cleanup_stale_artifacts(&keep)?;
        self.prune_versions(&keep)
    }

    async fn prune_async(&self, state: &State) -> Result<(), UpdateError> {
        let keep = self.retention_keep(state)?;
        self.cleanup_stale_artifacts_async(&keep).await?;
        self.prune_versions(&keep)
    }

    fn prune_versions(&self, keep: &BTreeSet<String>) -> Result<(), UpdateError> {
        let on_disk = self.data.kept_versions();
        for version in on_disk {
            if !keep.contains(&version) {
                let _ = fs::remove_dir_all(self.data.version_dir(&version));
            }
        }
        // Review3 T3 names the supervisor as the expiry cleaner. Any prune
        // may do it too: the update lock is held and expired pins are outside
        // every keep set, so removing them cannot release a live protection.
        PinStore::new(&self.data.versions())
            .remove_expired(Utc::now())
            .map_err(|error| UpdateError::Journal(error.to_string()))?;
        Ok(())
    }

    /// The status, for `settings`.
    pub fn status(&self, settings: &Settings, install: &Install) -> Status {
        let state = self.state();
        self.status_with_state(settings, install, &state)
    }

    pub(crate) fn status_with_state(
        &self,
        settings: &Settings,
        install: &Install,
        state: &State,
    ) -> Status {
        let installed = self.installed_version(state);
        let same_channel =
            state.channel.as_deref().unwrap_or("stable") == settings.channel.as_str();
        let update_available = same_channel
            && state.latest.as_deref().is_some_and(|latest| {
                semver::Version::parse(latest).is_ok_and(|latest| {
                    release::compare_channel(
                        &installed,
                        &latest,
                        settings.channel == SelfUpdateChannel::Preview,
                    ) == Verdict::Newer
                })
            });
        Status {
            channel: settings.channel.as_str(),
            release_source: same_channel.then(|| state.release_source.clone()).flatten(),
            checked_channel: state.channel.clone(),
            channel_error: same_channel.then(|| state.channel_error.clone()).flatten(),
            mode: settings.mode.as_str(),
            mode_source: settings.source.as_str(),
            updates: settings.describe(),
            check_every_seconds: settings.check_every.as_secs(),
            running_version: self.running.clone(),
            installed_version: installed.clone(),
            restart_needed: installed != self.running,
            target: self.target.clone(),
            latest_version: same_channel.then(|| state.latest.clone()).flatten(),
            update_available,
            staged_version: same_channel.then(|| state.staged.clone()).flatten(),
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

/// One retention policy for staging, adoption and pruning. The caller holds
/// the update lock; unreadable journal or pins fail closed. Extra versions
/// include the caller's running/installed identity and earlier state snapshot.
pub(crate) fn retention_keep(
    data: &DataDir,
    extra: impl IntoIterator<Item = String>,
) -> Result<BTreeSet<String>, UpdateError> {
    // Every recorded worker, including draining ones, is conservatively
    // retained: only the supervisor can say which pids are alive.
    retention_keep_with(data, extra, Utc::now(), |_, _| true, None, None)
}

/// [`retention_keep`] with the clock and the liveness check supplied, and
/// optionally the supervisor's in-memory journal, whose keep set is added to
/// the on-disk one (the supervisor's picture may be ahead of its last write).
///
/// `strict` is the supervised path's one listing of `versions/` ([`all_versions`]):
/// when given, the newest-other candidates come from it, and the state file
/// is read with [`State::load_strict`], so a listing or a state read that
/// fails is an error and never a smaller keep set. The CLI passes `None` and
/// keeps its lenient reads ([`DataDir::kept_versions`], [`State::load`]).
pub(crate) fn retention_keep_with(
    data: &DataDir,
    extra: impl IntoIterator<Item = String>,
    now: DateTime<Utc>,
    is_live: impl Fn(u32, u64) -> bool,
    memory: Option<&Journal>,
    strict: Option<&BTreeSet<String>>,
) -> Result<BTreeSet<String>, UpdateError> {
    let view = JournalView::load(&data.root().join(crate::journal::JOURNAL_FILE))
        .map_err(|error| UpdateError::Journal(error.to_string()))?;
    view.ensure_supported()
        .map_err(|error| UpdateError::Journal(error.to_string()))?;
    let pins = PinStore::new(&data.versions())
        .live(now)
        .map_err(|error| UpdateError::Journal(error.to_string()))?;
    let mut keep = view.keep_set(&pins, now, &is_live);
    if let Some(memory) = memory {
        keep.extend(memory.keep_set(&pins, now, &is_live));
    }
    keep.extend(extra);
    let fresh = match strict {
        Some(_) => State::load_strict(&data.state_file()).map_err(|error| {
            UpdateError::Journal(format!(
                "could not read the update state {}: {error}",
                data.state_file().display()
            ))
        })?,
        None => State::load(&data.state_file()),
    };
    keep.extend(fresh.previous);
    keep.extend(fresh.staged);
    if view.journal().is_some() || memory.is_some() {
        // A positively empty directory may be a crashed publication claim,
        // not an artifact. Unreadable and non-empty candidates remain protected.
        let listed: Vec<String> = match strict {
            Some(versions) => versions.iter().cloned().collect(),
            None => data.kept_versions(),
        };
        let candidates: Vec<_> = listed
            .into_iter()
            .filter(|version| {
                fs::read_dir(data.version_dir(version))
                    .map_or(true, |mut entries| entries.next().is_some())
            })
            .collect();
        add_newest_other(&mut keep, &candidates);
    }
    Ok(keep)
}

/// Stale staging directories and installed links, under the update lock, only
/// after the journal and pin preflight. [`Updater::prune`] and
/// [`supervised_prune`] share it.
fn cleanup_stale_artifacts_in(
    data: &DataDir,
    binary_name: &str,
    keep: &BTreeSet<String>,
) -> Result<(), UpdateError> {
    let verified = verified_stale_artifacts(data, binary_name, keep)?;
    cleanup_stale_artifacts_verified(data, &verified)
}

fn verified_stale_artifacts(
    data: &DataDir,
    binary_name: &str,
    keep: &BTreeSet<String>,
) -> Result<Vec<String>, UpdateError> {
    let view = JournalView::load(&data.root().join(crate::journal::JOURNAL_FILE))
        .map_err(|error| UpdateError::Journal(error.to_string()))?;
    view.ensure_supported()
        .map_err(|error| UpdateError::Journal(error.to_string()))?;
    let pins = PinStore::new(&data.versions())
        .live(Utc::now())
        .map_err(|error| UpdateError::Journal(error.to_string()))?;
    let fresh = State::load(&data.state_file());
    let mut recorded = Vec::new();
    if let (Some(version), Some(hash)) = (&fresh.staged, &fresh.staged_sha256) {
        recorded.push((version.clone(), hash.clone()));
    }
    for record in fresh.last_switch.iter().chain(fresh.history.iter()) {
        recorded.push((record.to.clone(), record.sha256.clone()));
    }
    for pin in pins {
        recorded.push((pin.version, pin.sha256));
    }
    if let Some(journal) = view.journal() {
        for artifact in [&journal.active, &journal.confirmed]
            .into_iter()
            .chain(journal.previous.iter())
            .chain(
                journal
                    .install_pending
                    .iter()
                    .map(|pending| &pending.target),
            )
            .chain(
                journal
                    .switch
                    .iter()
                    .flat_map(|switch| [&switch.from, &switch.to]),
            )
            .chain(
                journal
                    .switch
                    .iter()
                    .filter_map(|switch| switch.return_.as_ref().map(|return_| &return_.target)),
            )
        {
            recorded.push((artifact.version.clone(), artifact.sha256.clone()));
        }
        if let Some(installed) = &journal.installed {
            recorded.push((installed.version.clone(), installed.sha256.clone()));
        }
    }
    let verified: Vec<_> = recorded
        .into_iter()
        .filter_map(|(version, hash)| {
            (keep.contains(&version)
                && semver::Version::parse(&version).is_ok()
                && hash_file(&data.binary(&version, binary_name))
                    .ok()
                    .as_deref()
                    == Some(hash.as_str()))
            .then_some(hash)
        })
        .collect();
    Ok(verified)
}

fn cleanup_stale_artifacts_verified(
    data: &DataDir,
    verified: &[String],
) -> Result<(), UpdateError> {
    cleanup_stale_staging_in(data);
    if let Ok(Some(receipt)) = Receipt::load(&data.receipt_file()) {
        log_stale_prev_cleanup(
            &receipt.binary,
            crate::switch::cleanup_stale_prev(Path::new(&receipt.binary), verified),
        );
    }
    Ok(())
}

fn cleanup_stale_staging_in(data: &DataDir) {
    if let Err(error) = crate::staging::cleanup_stale_staging(&data.versions()) {
        tracing::warn!(%error, path = %data.versions().display(), "could not finish stale staging cleanup; continuing");
    }
}

fn log_stale_prev_cleanup(installed: &str, result: io::Result<crate::switch::PrevCleanup>) {
    match result {
        Ok(cleanup) => {
            for path in cleanup.removed {
                tracing::info!(path = %path.display(), "removed a stale installed link");
            }
        }
        Err(error) => {
            tracing::warn!(%error, path = %installed, "could not finish stale installed-link cleanup; continuing");
        }
    }
}

/// What the supervisor's prune removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SupervisedPruned {
    /// The version directories deleted.
    pub removed: Vec<String>,
    /// The expired pin files deleted.
    pub pins_removed: usize,
    /// Why the expired pins could not all be removed, after the versions above
    /// had been deleted. `None` when they could.
    pub pins_error: Option<String>,
}

/// The supervisor's keep set (v3 3.5): what the journal on disk and in memory
/// protect (a worker counts when `is_live(pid, started)` says it exists), live
/// pins, `state.staged` and `state.previous`, the newest other version, and the
/// versions of the workers the supervisor tracks (`tracked`). Reads only; the
/// caller need not hold the update lock for a reply that may lag. An unreadable
/// or newer-format journal and an unparseable pin are errors: the caller must
/// treat every version as kept. Additive: nothing the CLI calls uses it.
pub fn supervised_keep_set(
    data: &DataDir,
    tracked: impl IntoIterator<Item = String>,
    memory: Option<&Journal>,
    now: DateTime<Utc>,
    is_live: impl Fn(u32, u64) -> bool,
) -> Result<BTreeSet<String>, UpdateError> {
    // The newest-other rule lists `versions/`. [`DataDir::kept_versions`] hides
    // a listing error, which would drop that extra version without a word, so
    // a listing that fails is an error here, and the one strict listing is
    // the one the candidates come from (never a second, lenient one).
    let versions = all_versions(data)?;
    retention_keep_with(data, tracked, now, is_live, memory, Some(&versions))
}

/// Every version directory present (the fail-closed keep set). Unlike
/// [`DataDir::kept_versions`], which the CLI uses and which treats an
/// unreadable directory or entry as absent, any listing error is an error: a
/// partial list must never stand for "every version". A `versions/` that does
/// not exist is an empty list.
pub fn all_versions(data: &DataDir) -> Result<BTreeSet<String>, UpdateError> {
    let listing = |error: io::Error| {
        UpdateError::Journal(format!(
            "could not list {}: {error}",
            data.versions().display()
        ))
    };
    let entries = match fs::read_dir(data.versions()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(listing(error)),
    };
    let mut versions = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(listing)?;
        if !entry.file_type().map_err(listing)?.is_dir() {
            continue;
        }
        if let Ok(name) = entry.file_name().into_string()
            && semver::Version::parse(&name).is_ok()
        {
            versions.insert(name);
        }
    }
    Ok(versions)
}

/// The supervisor's prune: [`supervised_keep_set`], then the stale-artifact
/// cleanup, the deletion of every other version directory, and the removal of
/// expired pins, exactly as [`Updater::prune`] does it. The caller holds the
/// update lock. An unreadable or newer journal, an unparseable pin or a
/// `versions/` that cannot be listed refuses before anything is deleted; a
/// failure to remove the expired pins comes after the deletions and is
/// [`SupervisedPruned::pins_error`], not an error.
pub fn supervised_prune(
    data: &DataDir,
    binary_name: &str,
    tracked: impl IntoIterator<Item = String>,
    memory: Option<&Journal>,
    now: DateTime<Utc>,
    is_live: impl Fn(u32, u64) -> bool,
) -> Result<SupervisedPruned, UpdateError> {
    let keep = supervised_keep_set(data, tracked, memory, now, is_live)?;
    cleanup_stale_artifacts_in(data, binary_name, &keep)?;
    let mut done = SupervisedPruned::default();
    for version in all_versions(data)? {
        if !keep.contains(&version) && fs::remove_dir_all(data.version_dir(&version)).is_ok() {
            done.removed.push(version);
        }
    }
    // Versions may be gone by now: a failure here is not "nothing was
    // deleted", so it is reported next to what was.
    match PinStore::new(&data.versions()).remove_expired(now) {
        Ok(removed) => done.pins_removed = removed,
        Err(error) => done.pins_error = Some(error.to_string()),
    }
    Ok(done)
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
    /// Configured channel: stable or preview.
    pub channel: &'static str,
    /// Channel used for the recorded discovery.
    pub checked_channel: Option<String>,
    /// Source of the found release: latest or preview.
    pub release_source: Option<String>,
    /// Source warning when the other source was usable.
    pub channel_error: Option<String>,
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

#[cfg(all(test, unix))]
mod creation_tests {
    use super::*;

    #[test]
    fn the_staged_binary_and_directories_are_safe_at_creation_with_umask_zero() {
        use crate::journal::creation_tests::{assert_mode, with_umask_zero};
        with_umask_zero(
            "updater::creation_tests::the_staged_binary_and_directories_are_safe_at_creation_with_umask_zero",
            |root| {
                let versions = root.join("data/versions");
                let staging = StagingDir::create(&versions).unwrap();
                for path in [root.join("data"), versions, staging.0.clone()] {
                    assert_mode(&path, 0o755);
                }
                let staged = staging.0.join("open-ferry");
                let _file = crate::staging::create_staged_file(&staged).unwrap();
                assert_mode(&staged, 0o700);
                make_executable(&staged).unwrap();
                assert_mode(&staged, 0o755);
            },
        );
    }

    #[test]
    fn the_staged_binary_and_directories_are_safe_at_creation_with_default_acl() {
        use crate::journal::creation_tests::{assert_acl_mode, with_default_acl};
        with_default_acl(
            "updater::creation_tests::the_staged_binary_and_directories_are_safe_at_creation_with_default_acl",
            |root| {
                let versions = root.join("data/versions");
                let staging = StagingDir::create(&versions).unwrap();
                for path in [root.join("data"), versions, staging.0.clone()] {
                    assert_acl_mode(&path, 0o755);
                }
                let staged = staging.0.join("open-ferry");
                let _file = crate::staging::create_staged_file(&staged).unwrap();
                assert_acl_mode(&staged, 0o700);
                make_executable(&staged).unwrap();
                assert_acl_mode(&staged, 0o755);
            },
        );
    }
}
