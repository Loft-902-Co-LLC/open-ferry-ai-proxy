//! Not upstream's: supervisor startup (v3 10.4 with T6, 3.3 with T4 and T5,
//! 3.5, 2.5 and M9). The order:
//!
//! 1. `supervisor.lock` for life, then directory-chain trust checks;
//! 2. the journal's and the lock's trust checks (a failure of either disables
//!    switching; the service keeps running);
//! 3. the update lock, waited for with no limit, warning every 30 s and
//!    calling the host (T6). Nothing else happens before it is held;
//! 4. quarantine and hashing, the lock held through the first journal write;
//! 5. recovery (`open_ferry_update::recovery`) and adoption, executed here;
//! 6. the endpoint is bound;
//! 7. `supervisor.json` is written;
//! 8. the update lock is released;
//! 9. the sockets are bound and the active worker is started.
//!
//! A journal of a newer format, a failed trust check, or a location the
//! endpoint policy refuses (M9) runs one worker from the installed file, only
//! when its hash is this binary's own, with `switching_disabled` and no
//! journal write.

use std::collections::BTreeMap;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use open_ferry_update::data_dir::DataDir;
use open_ferry_update::journal::{
    Artifact, Installed, JOURNAL_FILE, Journal, JournalError, JournalLoad, JournalStore, Outcome,
    Worker, WorkerRole,
};
use open_ferry_update::recovery::{
    Facts, InstallAction, PlanStatus, RecoveryPlan, RunChoice, plan_startup, return_outcome,
};
use open_ferry_update::runner::Runner;
use open_ferry_update::staging::{StageExclusiveError, stage_exclusive_checked};
use open_ferry_update::state::SwitchRecord;

use crate::clock::Clock;
use crate::driver::{Driver, Registry, SpawnRequest, WorkerId};
use crate::endpoint::{EndpointError, Identity, Location, Server, ServiceKind};
use crate::host::Host;
use crate::install::{InstallPaths, InstallResult, place_with_prepared};
use crate::process::SupervisorLock;
use crate::protocol::{Installation, Role, VERSION};
use crate::statefile::{apply_by, apply_locked_by, change_locked, orphaned_partial};
use crate::timings::Timings;
use crate::trust::{TrustRules, check_dirs, check_file};

/// What startup needs to know that is not the host's.
#[derive(Clone)]
pub struct Config {
    pub data: DataDir,
    /// Where the installed binary is.
    pub installed: PathBuf,
    /// The absolute config path workers are given.
    pub config: PathBuf,
    pub kind: ServiceKind,
    pub timings: Timings,
    /// The install receipt, handed to workers in `welcome`.
    pub receipt: serde_json::Value,
    /// The binary's file name in `versions/<v>/`.
    pub binary_name: String,
    /// How a copied binary's `--version` is run.
    pub runner: Arc<dyn Runner>,
    /// How long that may take.
    pub detect_timeout: Duration,
    /// Where the endpoint is bound. `None` resolves it as production does
    /// ([`Location::resolve`]); on Unix that is the per-user socket, which is the
    /// real supervisor's. A caller that must not touch it (a test, a fixture)
    /// passes its own; the path policy still applies at bind and at connect.
    pub endpoint_location: Option<Location>,
}

impl Config {
    pub fn paths(&self) -> InstallPaths {
        InstallPaths {
            data: self.data.clone(),
            installed: self.installed.clone(),
            binary_name: self.binary_name.clone(),
        }
    }
}

/// What runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub artifact: Artifact,
    /// The file the worker is started from.
    pub binary: PathBuf,
}

/// What startup leaves the supervisor with.
pub struct Started {
    /// Held for life, even when switching is disabled. `None` is accepted only
    /// for callers reconstructing an older disabled startup state.
    pub lock: Option<SupervisorLock>,
    pub store: JournalStore,
    /// The journal as it is now, in memory. `None` while switching is disabled.
    pub journal: Option<Journal>,
    /// The journal on disk is older than `journal` (a write failed).
    pub journal_stale: bool,
    /// An outcome the state file has not been given is still in the journal.
    pub state_pending: bool,
    pub run: Option<Run>,
    /// Why switching is disabled, if it is.
    pub switching_disabled: Option<String>,
    /// `switch_off` ran: a failed ladder write turned switching off. Nothing
    /// is written to the journal or the state from here until a restart.
    pub switched_off: bool,
    /// The reason in `switching_disabled` is the ladder switch-off's own, set
    /// by `switch_off_state` (not an earlier reason it found there).
    pub ladder_off_reason: bool,
    /// Why no worker can run, if so. The supervisor retries with backoff.
    pub degraded: Option<String>,
    /// No version is pruned, for the supervisor's whole life (3.3).
    pub keep_all_versions: bool,
    pub endpoint: Option<Server>,
    pub listeners: Arc<Vec<(String, TcpListener)>>,
    /// The active worker, if it started.
    pub worker: Option<WorkerId>,
    /// When that worker's handshake ended: its `loaded` deadline counts from
    /// here, not from when the Core is built after start-up's journal writes.
    pub worker_greeted: Option<tokio::time::Instant>,
    /// Start-up tried `run` and the spawn failed: that is one failed attempt
    /// on that rung, which the restart ladder counts.
    pub run_failed: bool,
    /// The installed file as a rung, when its hash is a version the journal
    /// (or this binary) vouches for: the same rung the core's lists hold, so
    /// start-up checks a ladder record against the list the core will use.
    pub installed_rung: Option<Run>,
    /// The installed file's bytes as measured when `installed_rung` was: the
    /// one location a ladder record may add to the list is the installed path,
    /// and only when its bytes hash to that rung's artifact (`ladder`).
    pub installed_sha256: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug)]
pub enum StartupError {
    /// Another supervisor holds `supervisor.lock`.
    AlreadyRunning,
    Io(io::Error),
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning => {
                write!(f, "another supervisor already runs on this data directory")
            }
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StartupError {}

impl From<io::Error> for StartupError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Waits for the update lock with no limit (T6). Warns, and calls the host,
/// when the wait begins and then every `lock_warn_every`.
async fn wait_for_update_lock<H: Host>(
    cfg: &Config,
    host: &H,
) -> io::Result<open_ferry_update::data_dir::UpdateLock> {
    wait_for_update_lock_in(cfg, host, true).await
}

async fn wait_for_update_lock_in<H: Host>(
    cfg: &Config,
    host: &H,
    create_dirs: bool,
) -> io::Result<open_ferry_update::data_dir::UpdateLock> {
    let began = tokio::time::Instant::now();
    let mut next_warning = began;
    loop {
        let lock = if create_dirs {
            cfg.data.try_lock()?
        } else {
            cfg.data.try_lock_existing_dir()?
        };
        if let Some(lock) = lock {
            return Ok(lock);
        }
        let now = tokio::time::Instant::now();
        if now >= next_warning {
            let waited = now - began;
            tracing::warn!(
                "waiting for the update lock {} for {} s; another open-ferry is updating",
                cfg.data.root().join("update.lock").display(),
                waited.as_secs()
            );
            host.waiting_for_update_lock(waited);
            next_warning = now + cfg.timings.lock_warn_every;
        }
        tokio::time::sleep(cfg.timings.lock_poll).await;
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| io::Error::other(error.to_string()))
}

/// Keeps the lock in the blocking operation, not only in its cancellable caller.
async fn blocking_with_lock<T: Send + 'static>(
    lock: Arc<open_ferry_update::data_dir::UpdateLock>,
    work: impl FnOnce() -> T + Send + 'static,
) -> io::Result<T> {
    blocking(move || {
        let _held = lock;
        work()
    })
    .await
}

/// What is on disk, for recovery.
async fn gather_facts<H: Host>(
    cfg: &Config,
    host: &H,
    clock: &dyn Clock,
) -> io::Result<(Facts, BTreeMap<String, String>)> {
    gather_facts_in(cfg, host, clock, true).await
}

async fn gather_facts_in<H: Host>(
    cfg: &Config,
    host: &H,
    clock: &dyn Clock,
    include_versions: bool,
) -> io::Result<(Facts, BTreeMap<String, String>)> {
    let identity = Identity::current(cfg.kind)?;
    let rules = TrustRules::for_identity(&identity);
    let data = cfg.data.clone();
    let installed = cfg.installed.clone();
    let name = cfg.binary_name.clone();
    let (installed_sha256, installed_size, versions, refused) = blocking(move || {
        let (sha, size) = if include_versions {
            (
                open_ferry_update::installed_file::hash_installed(&installed).unwrap_or(None),
                std::fs::metadata(&installed)
                    .map(|meta| meta.len())
                    .unwrap_or(0),
            )
        } else {
            hash_disabled_installed(&installed).unwrap_or((None, 0))
        };
        let mut versions = BTreeMap::new();
        let mut refused = BTreeMap::new();
        if include_versions {
            for version in data.kept_versions() {
                let binary = data.binary(&version, &name);
                // Missing is a recovery fact, not a trust refusal. Do not follow
                // a linked leaf, including a dangling link, to classify it.
                if std::fs::symlink_metadata(&binary)
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                {
                    continue;
                }
                if let Err(error) = check_file(&binary, &rules) {
                    refused.insert(version, error.to_string());
                    continue;
                }
                match hash_disabled_installed(&binary) {
                    Ok((Some(hash), _)) => {
                        versions.insert(version, hash);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        refused.insert(
                            version,
                            format!("{} is unverified: {error}", binary.display()),
                        );
                    }
                }
            }
        }
        (sha, size, versions, refused)
    })
    .await?;
    Ok((
        Facts {
            now: clock.now(),
            self_sha256: host.own_sha256(),
            self_version: host.own_version(),
            installed_sha256,
            installed_size,
            installed_path: cfg.installed.clone(),
            versions,
        },
        refused,
    ))
}

/// The disabled fallback may have an untrusted directory chain by design, but
/// hashing must not follow a linked leaf or block on a FIFO. Hash and size come
/// from the same opened regular file.
pub(crate) fn hash_disabled_installed(path: &Path) -> io::Result<(Option<String>, u64)> {
    let mut file = open_regular_file(path)?;
    let metadata = file.metadata()?;
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    use std::io::Read;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok((Some(format!("{:x}", hash.finalize())), metadata.len()))
}

fn open_regular_file(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::other(
                "the installed fallback is a reparse point",
            ));
        }
    }
    if !metadata.is_file() {
        return Err(io::Error::other(
            "the installed fallback is not a regular file",
        ));
    }
    Ok(file)
}

/// The installed file as the one worker that runs when there is no journal:
/// only when it is this binary's own bytes.
fn installed_as_own(cfg: &Config, facts: &Facts) -> Result<Run, String> {
    installed_as_own_by(
        cfg,
        facts.installed_sha256.as_deref(),
        facts.installed_size,
        facts.self_sha256.as_deref(),
        &facts.self_version,
    )
}

/// [`installed_as_own`] from the pieces it reads, for a caller that has no
/// `Facts` (`launch` turning switching off).
pub(crate) fn installed_as_own_by(
    cfg: &Config,
    installed_sha256: Option<&str>,
    installed_size: u64,
    self_sha256: Option<&str>,
    self_version: &str,
) -> Result<Run, String> {
    match (installed_sha256, self_sha256) {
        (Some(installed), Some(own)) if installed.eq_ignore_ascii_case(own) => Ok(Run {
            artifact: Artifact {
                version: self_version.to_owned(),
                sha256: installed.to_owned(),
                size: installed_size,
            },
            binary: cfg.installed.clone(),
        }),
        (None, _) => Err(format!("{} is missing", cfg.installed.display())),
        _ => Err(format!(
            "{} is not the binary that is running this supervisor, and no journal vouches for it",
            cfg.installed.display()
        )),
    }
}

/// What executing a plan produced.
pub(crate) struct Executed {
    pub run: Option<Run>,
    pub journal: Option<Journal>,
    pub journal_stale: bool,
    pub state_pending: bool,
    pub disabled: Option<String>,
    pub degraded: Option<String>,
    pub keep_all: bool,
    pub notes: Vec<String>,
}

fn apply_install_disabled(executed: &mut Executed, why: String) {
    if executed.disabled.is_none() {
        executed.disabled = Some(why);
    }
}

fn disabled_execution(cfg: &Config, facts: &Facts, why: String) -> Executed {
    let (run, degraded) = match installed_as_own(cfg, facts) {
        Ok(run) => (Some(run), None),
        Err(why) => (None, Some(why)),
    };
    Executed {
        run,
        journal: None,
        journal_stale: false,
        state_pending: false,
        disabled: Some(why),
        degraded,
        keep_all: true,
        notes: Vec::new(),
    }
}

/// A linked install prefix deliberately disables switching: refusing to stage
/// or replace binaries through that prefix is fail-safe. The installed file may
/// still run as its own verified bytes on the disabled path.
fn untrusted_dirs(cfg: &Config, rules: &TrustRules) -> Option<String> {
    let Some(installed_dir) = cfg.installed.parent() else {
        return Some("the installed binary has no directory".to_owned());
    };
    let mut dirs = vec![cfg.data.root().to_path_buf(), installed_dir.to_path_buf()];
    if cfg.data.versions().exists() {
        dirs.push(cfg.data.versions());
    }
    let refs: Vec<_> = dirs.iter().map(PathBuf::as_path).collect();
    check_dirs(&refs, rules).err().map(|error| {
        format!(
            "{} is not private enough: {}",
            error.path.display(),
            error.reason
        )
    })
}

async fn disabled_recovery<H: Host>(
    cfg: &Config,
    host: &H,
    clock: &dyn Clock,
    why: String,
) -> io::Result<Recovery> {
    let mut notes = Vec::new();
    let update_lock = match wait_for_update_lock_in(cfg, host, false).await {
        Ok(lock) => Some(Arc::new(lock)),
        Err(error) => {
            let note = format!(
                "could not safely open {} while switching is disabled: {error}; running the installed file without the update lock",
                cfg.data.root().join("update.lock").display()
            );
            tracing::warn!("{note}");
            notes.push(note);
            None
        }
    };
    let identity = Identity::current(cfg.kind)?;
    let rules = TrustRules::for_identity(&identity);
    // A directory-chain refusal is the reason for this fallback, not a reason
    // to stop the service. The leaf is opened no-follow and checked below.
    if let Err(error) = check_file(&cfg.installed, &rules) {
        notes.push(error.to_string());
    }
    let (facts, _) = gather_facts_in(cfg, host, clock, false).await?;
    Ok(Recovery {
        executed: disabled_execution(cfg, &facts, why),
        update_lock,
        untrusted: None,
        notes,
    })
}

async fn stage(cfg: &Config) -> Result<Artifact, StageExclusiveError> {
    let identity = Identity::current(cfg.kind).map_err(StageExclusiveError::Io)?;
    let rules = TrustRules::for_identity(&identity);
    stage_exclusive_checked(
        &cfg.data,
        &cfg.installed,
        &cfg.binary_name,
        cfg.runner.as_ref(),
        cfg.detect_timeout,
        &|path| {
            check_file(path, &rules).map_err(|error| error.to_string())?;
            // check_file's shared unit-returning API drops its handle; the no-follow reopen is safe in the trusted chain (2.5).
            open_regular_file(path)
                .map_err(|error| format!("{} is unverified: {error}", path.display()))
        },
    )
    .await
}

fn kept_run(cfg: &Config, artifact: &Artifact) -> Result<Run, String> {
    let identity = Identity::current(cfg.kind).map_err(|error| error.to_string())?;
    let binary = cfg.data.binary(&artifact.version, &cfg.binary_name);
    check_file(&binary, &TrustRules::for_identity(&identity)).map_err(|error| error.to_string())?;
    Ok(Run {
        binary,
        artifact: artifact.clone(),
    })
}

/// The installed file as the rung to run, after the one check the core makes
/// before it spawns from that path: its bytes are the artifact's (the driver's
/// `verify_binary`). The driver deliberately skips `check_file` for the
/// installed path (`request.binary == request.installation.installed`): the
/// installer's own location may be linked or owned by an administrator, and
/// the hash is what makes it the file recovery verified. A trust check here
/// would send a real install to the disabled path.
fn installed_run(cfg: &Config, artifact: &Artifact) -> Result<Run, String> {
    let found = open_ferry_update::staging::hash_file(&cfg.installed)
        .map_err(|error| format!("{} can't be read: {error}", cfg.installed.display()))?;
    if !found.eq_ignore_ascii_case(&artifact.sha256) {
        return Err(format!(
            "{} is not the file recovery verified",
            cfg.installed.display()
        ));
    }
    Ok(Run {
        binary: cfg.installed.clone(),
        artifact: artifact.clone(),
    })
}

fn refused_execution(cfg: &Config, facts: &Facts, why: String, mut notes: Vec<String>) -> Executed {
    tracing::warn!("switching is disabled: {why}");
    notes.push(why.clone());
    let mut executed = disabled_execution(cfg, facts, why);
    executed.notes = notes;
    executed
}

/// A hash match alone does not prove the previous rename is durable. If
/// recovery is about to acknowledge it without doing an install, sync its
/// directory first. On failure keep unrelated recovery decisions, but retain
/// this pending install and defer only its acknowledgement for the core's retry.
fn sync_recovered_install(
    installed: &Path,
    load: &JournalLoad,
    facts: &Facts,
    plan: &mut RecoveryPlan,
    sync: &dyn Fn(&Path) -> io::Result<()>,
) {
    let JournalLoad::Loaded(old) = load else {
        return;
    };
    let Some(pending) = old.install_pending.as_ref() else {
        return;
    };
    let Some(next) = plan.journal_write.as_ref() else {
        return;
    };
    if facts.installed_sha256.as_deref() != Some(pending.target.sha256.as_str())
        || !matches!(plan.install, InstallAction::None)
        || next.install_pending.is_some()
        || coming_confirmed(next) != &pending.target
    {
        return;
    }
    let result = installed
        .parent()
        .ok_or_else(|| io::Error::other("installed file has no parent directory"))
        .and_then(sync);
    if let Err(error) = result {
        let note = format!(
            "could not sync the installed directory before completing recovery: {error}; retaining the pending install"
        );
        tracing::error!("{note}");
        plan.notes.push(note);
        if let Some(next) = plan.journal_write.as_mut() {
            next.install_pending = old.install_pending.clone();
            next.installed = old.installed.clone();
            if let Some(id) = &pending.record_id {
                defer_install_report(&mut next.outcomes, old, id);
            }
        }
        if let Some(id) = &pending.record_id {
            defer_install_report(&mut plan.upserts, old, id);
            plan.record_patches.retain(|patch| &patch.id != id);
        }
    }
}

/// What `confirmed` is once the plan's return, if it keeps one, has started:
/// J-R2 confirms the return's target, so a pending install of anything else
/// is superseded even though `confirmed` has not moved yet.
fn coming_confirmed(next: &Journal) -> &Artifact {
    next.switch
        .as_ref()
        .and_then(|switch| switch.return_.as_ref())
        .map_or(&next.confirmed, |ret| &ret.target)
}

fn defer_install_report(outcomes: &mut Vec<Outcome>, old: &Journal, id: &str) {
    let previous = old.outcomes.iter().find(|outcome| outcome.record.id == id);
    outcomes.retain_mut(|outcome| {
        if outcome.record.id != id {
            return true;
        }
        if outcome.record.is_partial() {
            if let Some(previous) = previous {
                *outcome = previous.clone();
            } else {
                return false;
            }
        } else {
            outcome.record.installed_binary =
                previous.and_then(|outcome| outcome.record.installed_binary.clone());
        }
        true
    });
}

/// Executes `plan` (3.3). The update lock is held by the caller.
async fn execute<H: Host>(
    cfg: &Config,
    host: &H,
    store: &JournalStore,
    load: &JournalLoad,
    mut plan: RecoveryPlan,
    facts: &Facts,
    update_lock: &Arc<open_ferry_update::data_dir::UpdateLock>,
) -> Executed {
    sync_recovered_install(&cfg.installed, load, facts, &mut plan, &|path| {
        #[cfg(feature = "blue-green-test")]
        open_ferry_update::test_seams::hit(cfg.data.root(), "startup.recovered_install_sync")?;
        crate::install::sync_directory(path)
    });
    let mut notes = std::mem::take(&mut plan.notes);
    let mut disabled = match &plan.status {
        PlanStatus::SwitchingDisabled(why) => Some(why.clone()),
        _ => None,
    };
    let mut degraded = match &plan.status {
        PlanStatus::Degraded(why) => Some(why.clone()),
        _ => None,
    };
    let mut keep_all = plan.keep_all_versions;

    // 1. An unreadable journal is set aside first.
    if plan.quarantine {
        match store.quarantine() {
            Ok(moved) => notes.push(format!("the journal was set aside as {}", moved.display())),
            Err(error) => {
                tracing::error!("could not set the unreadable journal aside: {error}");
                disabled = Some(format!(
                    "the journal is unreadable and could not be set aside: {error}"
                ));
                return Executed {
                    run: installed_as_own(cfg, facts).ok(),
                    journal: None,
                    journal_stale: false,
                    state_pending: false,
                    disabled,
                    degraded,
                    keep_all: true,
                    notes,
                };
            }
        }
    }

    // 2. The artifact that runs, and the journal to write.
    let mut run: Option<Run> = None;
    let mut journal: Option<Journal> = match load {
        JournalLoad::Loaded(journal) => Some(journal.clone()),
        _ => None,
    };
    let mut write_journal = false;
    match plan.run.clone() {
        RunChoice::Version(artifact) => {
            run = Some(match kept_run(cfg, &artifact) {
                Ok(run) => run,
                Err(why) => return refused_execution(cfg, facts, why, notes),
            });
            if let Some(next) = plan.journal_write.clone() {
                journal = Some(next);
                write_journal = true;
            }
        }
        RunChoice::Installed(reason) => match reason {
            open_ferry_update::recovery::InstalledReason::FirstStart => match stage(cfg).await {
                Ok(artifact) => {
                    let checked = match kept_run(cfg, &artifact) {
                        Ok(run) => run,
                        Err(why) => return refused_execution(cfg, facts, why, notes),
                    };
                    let installed = Installed::from(&artifact);
                    journal = Some(Journal::first_start(artifact.clone(), Some(installed)));
                    write_journal = true;
                    run = Some(checked);
                }
                Err(StageExclusiveError::Untrusted(why)) => {
                    return refused_execution(cfg, facts, why, notes);
                }
                Err(error) => {
                    tracing::error!("could not keep a copy of the installed binary: {error}");
                    disabled = Some(format!(
                        "the installed binary could not be kept in versions/: {error}"
                    ));
                    keep_all = true;
                    match installed_as_own(cfg, facts) {
                        Ok(own) => run = Some(own),
                        Err(why) => degraded = Some(why),
                    }
                }
            },
            open_ferry_update::recovery::InstalledReason::NewerJournal => {
                match installed_as_own(cfg, facts) {
                    Ok(own) => run = Some(own),
                    Err(why) => degraded = Some(why),
                }
            }
        },
        RunChoice::VerifiedInstalled(artifact) => {
            let staged = stage(cfg).await;
            let conflict = match &staged {
                Ok(staged) if staged.sha256 == artifact.sha256 => None,
                Ok(_) => Some(cfg.data.version_dir(&artifact.version)),
                Err(StageExclusiveError::Untrusted(why)) => {
                    return refused_execution(cfg, facts, why.clone(), notes);
                }
                Err(StageExclusiveError::SwitchingDisabled { version }) => {
                    Some(cfg.data.version_dir(version))
                }
                Err(error) => {
                    tracing::error!("could not keep a copy of the installed binary: {error}");
                    Some(cfg.data.version_dir(&artifact.version))
                }
            };
            match conflict {
                None => {
                    // The installed rung is `cfg.installed`: the staged copy
                    // is for the journal and the keep set, and never decides
                    // what runs.
                    run = Some(match installed_run(cfg, &artifact) {
                        Ok(run) => run,
                        Err(why) => return refused_execution(cfg, facts, why, notes),
                    });
                    if let Some(next) = plan.journal_write.clone() {
                        journal = Some(next);
                        write_journal = true;
                    }
                }
                Some(dir) => match plan.on_stage_conflict(&dir) {
                    Ok(next) => {
                        notes.extend(next.notes.clone());
                        keep_all = true;
                        disabled = match &next.status {
                            PlanStatus::SwitchingDisabled(why) => Some(why.clone()),
                            _ => None,
                        };
                        if let RunChoice::InstalledSwitchingDisabled { artifact, .. } = next.run {
                            run = Some(Run {
                                artifact,
                                binary: cfg.installed.clone(),
                            });
                        }
                        // Nothing else the plan held is done (O2).
                        return Executed {
                            run,
                            journal: None,
                            journal_stale: false,
                            state_pending: false,
                            disabled,
                            degraded,
                            keep_all,
                            notes,
                        };
                    }
                    Err(original) => plan = *original,
                },
            }
        }
        RunChoice::InstalledSwitchingDisabled { artifact, reason } => {
            disabled = Some(reason);
            run = Some(Run {
                artifact,
                binary: cfg.installed.clone(),
            });
        }
        RunChoice::Nothing => {
            // No worker can run, but a journalled return's outcome is still
            // owed to the journal and the state: the plan holds it.
            if let Some(next) = plan.journal_write.clone() {
                journal = Some(next);
                write_journal = true;
            }
        }
    }

    let mut journal_stale = false;
    let mut state_pending = false;
    let write = |journal: &mut Option<Journal>, name: &str, stale: &mut bool| {
        if let Some(journal) = journal.as_mut() {
            match store.write(journal, name) {
                Ok(()) => *stale = false,
                Err(error) => {
                    tracing::error!("journal write {name} failed: {error}");
                    *stale = true;
                }
            }
        }
    };

    // 3. The first journal write.
    if write_journal {
        write(&mut journal, plan.journal_write_name, &mut journal_stale);
    }

    // 4. Outcomes the state file has not been given, replayed in order.
    let mut replayed = false;
    for outcome in plan.upserts.clone() {
        if orphaned_partial(&cfg.data, &outcome) {
            let note = format!(
                "orphaned partial outcome {}: its state record is missing; retaining it for replay",
                outcome.record.id
            );
            // Re-entry repeats recovery in this process; do not repeat the
            // orphan warning on every recovery or retry.
            static WARNED_ORPHANS: std::sync::Mutex<std::collections::BTreeSet<String>> =
                std::sync::Mutex::new(std::collections::BTreeSet::new());
            let first = WARNED_ORPHANS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(outcome.record.id.clone());
            if first {
                tracing::warn!("{note}");
                notes.push(note);
            }
            state_pending = true;
            continue;
        }
        match apply_locked_by(&cfg.data, "replay", &outcome, &host.own_version()) {
            Ok(()) => {
                if let Some(journal) = journal.as_mut() {
                    replayed |= journal.remove_outcome(&outcome.record.id);
                }
            }
            Err(error) => {
                tracing::error!("could not give the state file an outcome: {error}");
                state_pending = true;
            }
        }
    }
    for patch in &plan.record_patches {
        let record = SwitchRecord {
            id: patch.id.clone(),
            installed_binary: Some(patch.installed_binary.clone()),
            ..SwitchRecord::default()
        };
        if let Err(error) = change_locked(&cfg.data, "replay", |state| state.upsert_record(record))
        {
            tracing::warn!("could not patch a state record: {error}");
        }
    }
    if replayed {
        write(&mut journal, "J8", &mut journal_stale);
    }

    // 5. The installed file (3.9).
    let pending = journal.as_ref().and_then(|j| j.install_pending.clone());
    let from = journal
        .as_ref()
        .and_then(|j| j.installed.as_ref().map(|i| i.version.clone()))
        .unwrap_or_default();
    let (target, expected, aside, replaces) = match &plan.install {
        InstallAction::None => (None, None, None, false),
        InstallAction::Replace { target, aside } => (
            Some(target.clone()),
            facts.installed_sha256.clone(),
            Some(aside.clone()),
            true,
        ),
        InstallAction::Place { target, aside } => {
            (Some(target.clone()), None, Some(aside.clone()), true)
        }
        InstallAction::Restore { installed } => (
            Some(Artifact {
                version: installed.version.clone(),
                sha256: installed.sha256.clone(),
                size: 0,
            }),
            None,
            None,
            false,
        ),
    };
    let mut install_written = false;
    if let Some(target) = target {
        let paths = cfg.paths();
        let aside_path = aside.map(PathBuf::from);
        let mut prepared_journal = journal.clone();
        let prepared_store = store.clone();
        let prepared = {
            let (target, from) = (target.clone(), from.clone());
            blocking_with_lock(update_lock.clone(), move || {
                #[cfg(feature = "blue-green-test")]
                if let Err(error) =
                    open_ferry_update::test_seams::hit(paths.data.root(), "startup.before_install")
                {
                    return (InstallResult::Failed(error.to_string()), prepared_journal);
                }
                let result = place_with_prepared(
                    &paths,
                    &target,
                    expected.as_deref(),
                    &from,
                    aside_path.as_deref(),
                    |actual| {
                        if replaces && let Some(journal) = prepared_journal.as_mut() {
                            if let Some(pending) = journal.install_pending.as_mut() {
                                pending.aside = actual
                                    .map(|path| path.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                            }
                            prepared_store
                                .write(journal, "J-install")
                                .map_err(|error| {
                                    format!("could not journal the prepared install: {error}")
                                })?;
                            #[cfg(feature = "blue-green-test")]
                            if let Err(error) = open_ferry_update::test_seams::hit(
                                paths.data.root(),
                                "startup.after_prepared_journal",
                            ) {
                                panic!("install task ended after its journal write: {error}");
                            }
                        }
                        Ok(())
                    },
                );
                (result, prepared_journal)
            })
            .await
        };
        let result = match prepared {
            Ok((result, prepared_journal)) => {
                journal = prepared_journal;
                result
            }
            Err(error) => {
                // J-install may have landed before the blocking task panicked.
                // Reload its sequence and prepared aside before any later write.
                match journal_after_join_error(store) {
                    Ok(on_disk) => journal = Some(on_disk),
                    Err(why) => return refused_execution(cfg, facts, why, notes),
                }
                InstallResult::Failed(error.to_string())
            }
        };
        if replaces && let Some(journal) = journal.as_mut() {
            let text = match &result {
                InstallResult::Replaced => {
                    journal.installed = Some(Installed::from(&target));
                    journal.install_pending = None;
                    Some("replaced".to_owned())
                }
                InstallResult::ChangedElsewhere => {
                    journal.install_pending = None;
                    Some("changed-elsewhere".to_owned())
                }
                InstallResult::Failed(why) => {
                    tracing::error!("the install of {} failed: {why}", target.version);
                    if let Some(held) = journal.install_pending.as_mut() {
                        held.attempts += 1;
                        let wait = cfg
                            .timings
                            .install_retry
                            .saturating_mul(
                                1u32.checked_shl(held.attempts.saturating_sub(1))
                                    .unwrap_or(u32::MAX),
                            )
                            .min(cfg.timings.install_retry_max);
                        let next = facts.now + chrono::Duration::from_std(wait).unwrap_or_default();
                        held.next_try =
                            Some(next.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                    }
                    Some(open_ferry_update::state::installed_binary::failed(why))
                }
            };
            if let (Some(text), Some(id)) = (text, pending.and_then(|p| p.record_id)) {
                let outcome = Outcome {
                    record: SwitchRecord {
                        id: id.clone(),
                        installed_binary: Some(text),
                        ..SwitchRecord::default()
                    },
                    mark_failed: None,
                };
                journal.push_outcome(outcome);
                let merged = journal
                    .outcomes
                    .iter()
                    .find(|outcome| outcome.record.id == id)
                    .expect("the install outcome was just merged");
                if apply_locked_by(&cfg.data, "replay", merged, &host.own_version()).is_ok() {
                    journal.remove_outcome(&id);
                } else {
                    state_pending = true;
                }
            }
            install_written = true;
        } else if let InstallResult::Failed(why) = &result {
            tracing::error!("could not restore the installed file: {why}");
        }
    }
    if install_written {
        write(&mut journal, "J6", &mut journal_stale);
    }

    // 6. Adoption (3.1 item 1): the installed file nobody told us about.
    if let Some(adopt) = &plan.adopt
        && journal.is_some()
    {
        match stage(cfg).await {
            Ok(artifact) if artifact.sha256 == adopt.sha256 => {
                let checked = match kept_run(cfg, &artifact) {
                    Ok(run) => run,
                    Err(why) => return refused_execution(cfg, facts, why, notes),
                };
                if let Some(journal) = journal.as_mut() {
                    journal.adopt(artifact.clone());
                }
                run = Some(checked);
                write(&mut journal, "adopt", &mut journal_stale);
            }
            Ok(_) => notes.push(
                "the installed file changed while it was being adopted: not adopted".to_owned(),
            ),
            Err(StageExclusiveError::Untrusted(why)) => {
                return refused_execution(cfg, facts, why, notes);
            }
            Err(error) => {
                tracing::warn!("the installed file could not be adopted: {error}");
                notes.push(format!("the installed file could not be adopted: {error}"));
            }
        }
    }

    let _ = host;
    Executed {
        run,
        journal,
        journal_stale,
        state_pending: state_pending || false,
        disabled,
        degraded,
        keep_all,
        notes,
    }
}

fn journal_after_join_error(store: &JournalStore) -> Result<Journal, String> {
    match store.load() {
        Ok(JournalLoad::Loaded(journal)) => Ok(journal),
        other => Err(format!(
            "could not reload the journal after an install task ended: {other:?}"
        )),
    }
}

fn own_identity() -> io::Result<crate::process::ProcessIdentity> {
    #[cfg(windows)]
    {
        crate::process::windows::current_identity()
    }
    #[cfg(unix)]
    {
        let pid = std::process::id();
        let started = crate::process::unix::start_time(pid)?
            .ok_or_else(|| io::Error::other("this process has no start time"))?;
        Ok(crate::process::ProcessIdentity { pid, started })
    }
}

/// The record clients read to find the endpoint (v3 2.5).
fn write_record(cfg: &Config, version: &str, endpoint: &str) -> io::Result<()> {
    let me = own_identity()?;
    let record = serde_json::json!({
        "pid": me.pid,
        "started": me.started,
        "endpoint": endpoint,
        "version": version,
        "protocol": VERSION,
    });
    let bytes = serde_json::to_vec_pretty(&record).map_err(io::Error::other)?;
    open_ferry_update::state::write_atomically(&cfg.data.root().join("supervisor.json"), &bytes)
}

/// What recovery left, and the update lock it ran under.
pub(crate) struct Recovery {
    pub executed: Executed,
    /// Held by the caller until the endpoint is bound and the record written.
    pub update_lock: Option<Arc<open_ferry_update::data_dir::UpdateLock>>,
    /// Why the journal or the lock file is not trusted, if so.
    pub untrusted: Option<String>,
    pub notes: Vec<String>,
}

fn untrusted_files(cfg: &Config, rules: &TrustRules) -> Option<String> {
    let mut untrusted = None;
    for file in [
        cfg.data.root().join(JOURNAL_FILE),
        cfg.data.root().join("supervisor.lock"),
    ] {
        if file.exists()
            && let Err(error) = check_file(&file, rules)
        {
            untrusted = Some(format!(
                "{} is not trusted: {}",
                error.path.display(),
                error.reason
            ));
        }
    }
    untrusted
}

/// Steps 2 to 5 of the module comment: the update lock (T6), facts, the
/// journal, the plan, and executing it. A start does this once; a panic
/// re-entry (lead answer 1) does it again, "exactly as after a restart".
pub(crate) async fn recover<H: Host>(
    cfg: &Config,
    host: &H,
    store: &JournalStore,
    clock: &dyn Clock,
    rules: &TrustRules,
) -> io::Result<Recovery> {
    let mut notes = Vec::new();
    let untrusted = untrusted_files(cfg, rules);
    if let Some(why) = untrusted {
        return disabled_recovery(cfg, host, clock, why).await;
    }
    let untrusted = None;
    let update_lock = Arc::new(wait_for_update_lock(cfg, host).await?);
    let load = match store.load() {
        Ok(load) => load,
        Err(JournalError::Newer { found, .. }) => JournalLoad::Newer { found },
        Err(JournalError::Unreadable(reason)) => JournalLoad::Unreadable { reason },
        Err(error) => {
            // Not readable for a reason that is not the file's: change nothing.
            tracing::error!("the journal could not be read: {error}");
            notes.push(format!("the journal could not be read: {error}"));
            JournalLoad::Newer { found: u32::MAX }
        }
    };
    // A newer journal already requires the disabled path: do not inspect
    // versions that cannot influence it.
    let (facts, refused) = if matches!(load, JournalLoad::Newer { .. }) {
        gather_facts_in(cfg, host, clock, false).await?
    } else {
        gather_facts(cfg, host, clock).await?
    };
    for reason in refused.values() {
        tracing::warn!("version is unverified: {reason}");
        notes.push(format!("version is unverified: {reason}"));
    }
    // Trust is not a failed hash. A journal's preferred run, or a missing
    // installed file's recorded/pending source, must disable switching rather
    // than silently fall down the hash-failure ladder.
    let selected_refusal = match &load {
        JournalLoad::Loaded(journal) => {
            let wanted = journal
                .switch
                .as_ref()
                .and_then(|switch| switch.return_.as_ref())
                .map(|returning| &returning.target)
                .unwrap_or(&journal.confirmed);
            refused.get(&wanted.version).cloned()
        }
        _ => None,
    };
    let mut plan = plan_startup(&load, &facts);
    // Refuse every source before executing any part of the plan, including a
    // restore whose source differs from the artifact that will run.
    let install_version = match &plan.install {
        InstallAction::Replace { target, .. } | InstallAction::Place { target, .. } => {
            Some(target.version.clone())
        }
        InstallAction::Restore { installed } => Some(installed.version.clone()),
        InstallAction::None if facts.installed_sha256.is_none() => match &load {
            // Fact gathering refused these sources, so recovery could not offer
            // an install. Still report its trust failure, without disabling the
            // unrelated, verified run recovery selected.
            JournalLoad::Loaded(journal) => journal
                .install_pending
                .as_ref()
                .filter(|pending| refused.contains_key(&pending.target.version))
                .map(|pending| pending.target.version.clone())
                .or_else(|| {
                    journal
                        .installed
                        .as_ref()
                        .filter(|installed| refused.contains_key(&installed.version))
                        .map(|installed| installed.version.clone())
                }),
            _ => None,
        },
        InstallAction::None => None,
    };
    let install_refusal = install_version.as_ref().and_then(|version| {
        check_file(&cfg.data.binary(version, &cfg.binary_name), rules)
            .err()
            .map(|error| error.to_string())
    });
    let untrusted = untrusted.or(selected_refusal).or_else(|| match &plan.run {
        RunChoice::Version(artifact) => {
            check_file(&cfg.data.binary(&artifact.version, &cfg.binary_name), rules)
                .err()
                .map(|error| error.to_string())
        }
        _ => None,
    });
    let mut install_disabled = None;
    if untrusted.is_none()
        && let Some(why) = install_refusal
    {
        plan.install = InstallAction::None;
        install_disabled = Some(why.clone());
        plan.notes.push(why);
        plan.keep_all_versions = true;
        // Defensive and currently a no-op: recovery keeps a non-superseded
        // pending install unchanged. Preserve it if that recovery policy changes.
        if let JournalLoad::Loaded(old) = &load
            && let Some(next) = plan.journal_write.as_mut()
            && let Some(pending) = &old.install_pending
            && &pending.target == coming_confirmed(next)
        {
            next.install_pending = old.install_pending.clone();
            next.installed = old.installed.clone();
            if let Some(id) = old
                .install_pending
                .as_ref()
                .and_then(|pending| pending.record_id.as_deref())
            {
                defer_install_report(&mut next.outcomes, old, id);
                defer_install_report(&mut plan.upserts, old, id);
                plan.record_patches.retain(|patch| patch.id != id);
            }
        }
    }
    if let Some(why) = &untrusted {
        tracing::warn!("switching is disabled: {why}");
        notes.push(why.clone());
    }
    let mut executed = if let Some(why) = &untrusted {
        disabled_execution(cfg, &facts, why.clone())
    } else {
        // Boxed, as `recover` is where it is called: `execute`'s future is about
        // 70 KB and `recover`'s about 143 KB, and `start` held both inline (a 287 KB
        // future) on a thread of 2 MiB.
        Box::pin(execute(cfg, host, store, &load, plan, &facts, &update_lock)).await
    };
    // An install-source refusal disables switching, not degraded restart
    // backoff: the executed result can carry both independently.
    if let Some(why) = install_disabled {
        apply_install_disabled(&mut executed, why);
    }
    notes.extend(std::mem::take(&mut executed.notes));
    Ok(Recovery {
        executed,
        update_lock: Some(update_lock),
        untrusted,
        notes,
    })
}

/// Runs startup (see the module comment) and starts the active worker.
pub async fn start<H: Host, D: Driver>(
    cfg: &Config,
    host: &H,
    driver: &D,
    registry: &mut Registry,
    clock: &dyn Clock,
) -> Result<Started, StartupError> {
    let identity = Identity::current(cfg.kind)?;
    let rules = TrustRules::for_identity(&identity);
    let store = JournalStore::new(cfg.data.root());
    let mut notes = Vec::new();

    // 1. The lock is held for life, including on the disabled path.
    if cfg.kind == ServiceKind::User {
        crate::trust::create_private_dir(cfg.data.root())?;
    }
    let lock = match SupervisorLock::try_acquire(cfg.data.root()) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Err(StartupError::AlreadyRunning);
        }
        Err(error) => return Err(error.into()),
    };
    // A cancelled handshake can own a process without ever returning a port.
    registry.wait_until_empty().await;
    let Recovery {
        executed,
        update_lock,
        untrusted,
        notes: recovery_notes,
    } = if let Some(why) = untrusted_dirs(cfg, &rules) {
        tracing::error!("switching is disabled: {why}");
        disabled_recovery(cfg, host, clock, why).await?
    } else {
        Box::pin(recover(cfg, host, &store, clock, &rules)).await?
    };
    notes.extend(recovery_notes);
    let mut disabled = executed.disabled.or(untrusted);

    // 4. The endpoint (M9).
    let mut endpoint = None;
    {
        let resolved = match &cfg.endpoint_location {
            Some(location) => Ok(location.clone()),
            None => Location::resolve(&identity, cfg.data.root()),
        };
        match resolved {
            Ok(location) => match Server::bind(&location, &identity, &lock) {
                Ok(server) => {
                    if let Err(error) =
                        write_record(cfg, &host.own_version(), &location.to_record())
                    {
                        tracing::error!("could not write supervisor.json: {error}");
                        disabled = Some(format!("supervisor.json could not be written: {error}"));
                    } else {
                        endpoint = Some(server);
                    }
                }
                Err(error) => disabled = Some(format!("the endpoint could not be bound: {error}")),
            },
            Err(EndpointError::SwitchingDisabled(_)) => {
                disabled = Some("this location does not allow switching".to_owned());
            }
            Err(error) => disabled = Some(format!("no safe endpoint: {error}")),
        }
    }

    // 5. The update lock is released; the worker starts.
    let disabled_lock = if disabled.is_some() {
        update_lock
    } else {
        drop(update_lock);
        None
    };
    let mut started = Started {
        lock: Some(lock),
        store,
        journal: executed.journal,
        journal_stale: executed.journal_stale,
        state_pending: executed.state_pending,
        run: executed.run,
        switching_disabled: disabled,
        switched_off: false,
        ladder_off_reason: false,
        degraded: executed.degraded,
        keep_all_versions: executed.keep_all,
        endpoint,
        listeners: Arc::new(Vec::new()),
        worker: None,
        worker_greeted: None,
        run_failed: false,
        installed_rung: None,
        installed_sha256: None,
        notes,
    };
    (started.installed_rung, started.installed_sha256) =
        known_installed_rung(cfg, host, started.journal.as_ref());
    launch(cfg, host, driver, registry, &mut started).await;
    drop(disabled_lock);
    Ok(started)
}

/// Binds the sockets and starts the active worker, if there is a run. A
/// failure is logged and left to the restart loop.
pub async fn launch<H: Host, D: Driver>(
    cfg: &Config,
    host: &H,
    driver: &D,
    registry: &mut Registry,
    started: &mut Started,
) {
    started.run_failed = false;
    let Some(run) = started.run.clone() else {
        return;
    };
    if started.listeners.is_empty() {
        match host.bind() {
            Ok(listeners) => started.listeners = Arc::new(listeners),
            Err(error) => {
                tracing::error!("could not bind the listening sockets: {error}");
                started.degraded = Some(format!("could not bind the listening sockets: {error}"));
                return;
            }
        }
    }
    let mut run = run;
    // The update lock a failed count's fallback takes, held until the end of
    // this function: the spawn from the installed path is made under it.
    let mut fallback_lock = None;
    match count_start(cfg, started, &mut run) {
        Count::Counted => {}
        Count::Spent => {
            tracing::error!(
                "{} has had all its tries on this ladder: not started again, the core goes on",
                run.artifact.version
            );
            started.degraded = Some(format!(
                "{} has had all its tries; the next rung is tried",
                run.artifact.version
            ));
            return;
        }
        // The count could not be written: switching is off, and the spawn it
        // was for is not made from the ladder. The installed file runs, as it
        // does for any start-up with switching off.
        Count::Unwritten => {
            let Some(fallback) = switch_off(cfg, host, started, &mut fallback_lock).await else {
                return;
            };
            run = fallback;
        }
    }
    let id = registry.next_id();
    let request = SpawnRequest {
        id,
        version: run.artifact.version.clone(),
        sha256: run.artifact.sha256.clone(),
        binary: run.binary.clone(),
        role: Role::Active,
        switch_id: None,
        config: cfg.config.clone(),
        installation: Installation {
            installed: cfg.installed.clone(),
            data_dir: cfg.data.root().to_path_buf(),
            receipt: cfg.receipt.clone(),
        },
        topology: host.topology(),
        listeners: started.listeners.clone(),
        budgets: cfg.timings.budgets(),
        handshake_timeout: cfg.timings.load,
        log: None,
    };
    let spawned = driver.spawn(request, registry.events()).await;
    let greeted = tokio::time::Instant::now();
    match spawned {
        Ok(port) => {
            let (pid, process_started) = (port.identity.pid, port.identity.started);
            registry.add(port);
            started.worker = Some(id);
            started.worker_greeted = Some(greeted);
            started.degraded = None;
            if let Some(journal) = started.journal.as_mut() {
                // Startup/reentry has already observed every previous worker's exit.
                journal.workers.clear();
                journal.workers.push(Worker {
                    pid,
                    started: process_started,
                    version: run.artifact.version.clone(),
                    role: WorkerRole::Active,
                    lost: false,
                });
                match started.store.write(journal, "workers") {
                    Ok(()) => started.journal_stale = false,
                    Err(error) => {
                        tracing::error!("could not record the active worker: {error}");
                        started.journal_stale = true;
                    }
                }
            }
            finish_recovered_return(cfg, host, started, &run);
        }
        Err(error) => {
            tracing::error!("the active worker did not start: {error}");
            started.degraded = Some(format!("the active worker did not start: {error}"));
            // An untrusted refusal is not an attempt (the core learns of it
            // when it tries the rung and is refused again).
            started.run_failed = !matches!(error, crate::driver::SpawnError::Untrusted(_));
            if !started.run_failed {
                uncount_start(cfg, host, started);
            }
        }
    }
}

/// What counting the start-up try came to.
enum Count {
    /// Counted (or nothing is kept: no journal, or switching is off).
    Counted,
    /// The rung has already had all its tries on this ladder.
    Spent,
    /// The count could not be written.
    Unwritten,
}

/// Counts the start-up try on its rung in the journal's ladder record before
/// it is made, so a crash while it runs has used one of the rung's tries
/// (6.1 g). [`Count::Spent`] when the rung has already had all of them: it is
/// not started, and the core goes on from the record. Nothing is kept when
/// there is no journal or switching is off. A record that fails the one check
/// (`ladder::usable_record`) is ignored and a fresh one is written. A write
/// that fails after the store's retries is [`Count::Unwritten`]; one that
/// landed but could not be synced counts as written, as for J1.
fn count_start(cfg: &Config, started: &mut Started, run: &mut Run) -> Count {
    if started.switching_disabled.is_some() {
        return Count::Counted;
    }
    let installed = started.installed_rung.clone();
    let measured = started.installed_sha256.clone();
    let Some(journal) = started.journal.as_mut() else {
        return Count::Counted;
    };
    let ladder = crate::ladder::ladder_of(journal).map(str::to_owned);
    let known = journal
        .ladder
        .as_ref()
        .map(|record| crate::ladder::progress_of(cfg, record))
        .unwrap_or_default();
    let full = crate::ladder::list_of(cfg, journal, installed, measured.as_deref(), &known);
    // With a usable record the rung is the one the ladder says is next, from
    // the checked progress (the pick the core makes); recovery's run is only
    // for a ladder that holds nothing yet.
    let (progress, picked, before) = match crate::ladder::usable_record(cfg, journal, &full)
        .map(|record| crate::ladder::progress_of(cfg, record))
    {
        Some(progress) => {
            let Some((picked, before)) = progress.pick(&full, |_| true) else {
                return Count::Spent;
            };
            (progress, picked, before)
        }
        None => (crate::ladder::Progress::default(), run.clone(), 0),
    };
    let tried = crate::ladder::Progress {
        current: Some((picked.clone(), before)),
        spent: progress.spent.clone(),
    };
    journal.ladder = Some(crate::ladder::record_start(
        cfg,
        ladder.as_deref(),
        &tried,
        &picked,
    ));
    // An open return stands on the rung that is tried, in the same write, as
    // the core's `retarget_return` does for one of its own choices.
    if let Some(ret) = journal
        .switch
        .as_mut()
        .and_then(|switch| switch.return_.as_mut())
    {
        ret.target = picked.artifact.clone();
        ret.installed = picked.binary == cfg.installed;
    }
    match started.store.write(journal, "J-L") {
        Ok(()) => started.journal_stale = false,
        Err(open_ferry_update::journal::JournalError::NotDurable(error)) => {
            tracing::warn!("the start-up count landed but could not be synced: {error}");
            started.journal_stale = true;
        }
        Err(error) => {
            tracing::error!("could not record the start-up try: {error}");
            return Count::Unwritten;
        }
    }
    started.run = Some(picked.clone());
    *run = picked;
    Count::Counted
}

/// A ladder record could not be written: switching is off for this start
/// (`switching_disabled`, with the reason for the status), and what runs is
/// what a start-up with switching off runs: the installed file, when it is
/// this binary's own bytes. The journal is not kept, as on that path.
///
/// The update lock is taken again, as a start-up with switching off holds it,
/// and goes into `held` for the caller to keep until its spawn is made: an
/// updater replacing the installed file between the hash and the spawn would
/// otherwise run other bytes than the ones tracked. When the lock can't be
/// taken nothing is chosen and nothing is spawned: the start is degraded with
/// the reason, and the core's restart (which has no run) spawns nothing.
async fn switch_off<H: Host>(
    cfg: &Config,
    host: &H,
    started: &mut Started,
    held: &mut Option<open_ferry_update::data_dir::UpdateLock>,
) -> Option<Run> {
    switch_off_state(started);
    match wait_for_update_lock(cfg, host).await {
        Ok(lock) => *held = Some(lock),
        Err(error) => {
            tracing::error!("switching is off and the update lock could not be taken: {error}");
            started.run = None;
            started.degraded = Some(format!(
                "switching is off and the update lock could not be taken: {error}"
            ));
            return None;
        }
    }
    choose_installed(cfg, host, started)
}

/// What turning switching off leaves of the start: the reason, no journal and
/// nothing pending, every version kept.
fn switch_off_state(started: &mut Started) {
    tracing::error!("the ladder record could not be written: switching is off");
    if started.switching_disabled.is_none() {
        started.switching_disabled =
            Some("switching is off: the ladder record could not be written".to_owned());
        started.ladder_off_reason = true;
    }
    started.journal = None;
    started.journal_stale = false;
    started.state_pending = false;
    started.keep_all_versions = true;
    started.switched_off = true;
}

/// The run of a start with switching off: the installed file, when it is this
/// binary's own bytes (`installed_as_own_by`, which the core shares).
fn choose_installed<H: Host>(cfg: &Config, host: &H, started: &mut Started) -> Option<Run> {
    let (sha256, size) = hash_disabled_installed(&cfg.installed).unwrap_or((None, 0));
    match installed_as_own_by(
        cfg,
        sha256.as_deref(),
        size,
        host.own_sha256().as_deref(),
        &host.own_version(),
    ) {
        Ok(run) => {
            started.run = Some(run.clone());
            started.degraded = None;
            Some(run)
        }
        Err(why) => {
            started.run = None;
            started.degraded = Some(why);
            None
        }
    }
}

/// A start-up spawn the driver refused as untrusted was not an attempt: the
/// count made before it is taken back. A take-back that can't be written
/// turns switching off, as a count that can't be written does.
fn uncount_start<H: Host>(cfg: &Config, host: &H, started: &mut Started) {
    if started.switching_disabled.is_some() {
        return;
    }
    let Some(journal) = started.journal.as_mut() else {
        return;
    };
    let Some(ladder) = journal.ladder.as_mut().filter(|ladder| ladder.attempts > 0) else {
        return;
    };
    ladder.attempts -= 1;
    match started.store.write(journal, "J-L") {
        Ok(()) => started.journal_stale = false,
        Err(open_ferry_update::journal::JournalError::NotDurable(error)) => {
            tracing::warn!("the start-up take-back landed but could not be synced: {error}");
            started.journal_stale = true;
        }
        Err(error) => {
            tracing::error!("could not take back the start-up count: {error}");
            // Nothing is spawned from here (the spawn was refused), so there
            // is no spawn for the update lock to cover.
            switch_off_state(started);
            let _ = choose_installed(cfg, host, started);
        }
    }
}

/// The installed file as a rung, when its hash is a version the journal (or
/// this binary) vouches for: the rung the core's lists hold as the installed
/// one. A file that can't be read or measured is no rung. The second value is
/// the bytes' hash as measured now (`None` with no journal or no readable
/// file), which a ladder record's installed rung must match to be on the list.
fn known_installed_rung<H: Host>(
    cfg: &Config,
    host: &H,
    journal: Option<&Journal>,
) -> (Option<Run>, Option<String>) {
    let Some(journal) = journal else {
        return (None, None);
    };
    let Ok(sha256) = open_ferry_update::staging::hash_file(&cfg.installed) else {
        return (None, None);
    };
    let rung = || {
        let version = journal
            .installed
            .as_ref()
            .filter(|installed| installed.sha256.eq_ignore_ascii_case(&sha256))
            .map(|installed| installed.version.clone())
            .or_else(|| {
                host.own_sha256()
                    .filter(|own| own.eq_ignore_ascii_case(&sha256))
                    .map(|_| host.own_version())
            })?;
        let size = std::fs::metadata(&cfg.installed).ok()?.len();
        Some(Run {
            binary: cfg.installed.clone(),
            artifact: Artifact {
                version,
                sha256: sha256.clone(),
                size,
            },
        })
    };
    (rung(), Some(sha256.clone()))
}

/// J-R2 for a return that recovery left open: a worker of `return.target` has
/// now started, which is what `returned_to` means. `switch` is cleared,
/// `confirmed` is what runs, and the forward record already in the state file
/// is given `returned_to` by id. A state write that can't be made now stays in
/// the journal's outcomes for the core to retry. If the worker is not the
/// return's target nothing changes: the core carries the return on.
fn finish_recovered_return<H: Host>(cfg: &Config, host: &H, started: &mut Started, run: &Run) {
    let ran = &run.artifact;
    let installed = started.installed_rung.clone();
    let installed_sha256 = started.installed_sha256.clone();
    let Some(journal) = started.journal.as_mut() else {
        return;
    };
    let Some(switch) = journal.switch.clone() else {
        return;
    };
    let Some(ret) = switch.return_.as_ref().filter(|ret| &ret.target == ran) else {
        return;
    };
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let outcome = return_outcome(&switch, ret, ran, &now);
    let held = journal.ladder.clone();
    if !journal.finish_return(outcome.clone()) {
        return;
    }
    // The return's record becomes the restart ladder's in J-R2's own write:
    // the rung that started, the tries held, not a new cycle.
    journal.ladder = crate::ladder::restart_record_after_return(
        cfg,
        journal,
        &switch.id,
        held.as_ref(),
        run,
        installed,
        installed_sha256.as_deref(),
    );
    match started.store.write(journal, "J-R2") {
        Ok(()) => started.journal_stale = false,
        Err(error) => {
            tracing::error!("J-R2 failed: {error}");
            started.journal_stale = true;
        }
    }
    match apply_by(&cfg.data, "S3", &outcome, &host.own_version()) {
        Ok(()) => {
            if journal.remove_outcome(&outcome.record.id) {
                match started.store.write(journal, "J8") {
                    Ok(()) => started.journal_stale = false,
                    Err(error) => {
                        tracing::error!("J8 failed: {error}");
                        started.journal_stale = true;
                    }
                }
            }
        }
        Err(error) => {
            tracing::warn!("the returned_to record could not be written now: {error}");
            started.state_pending = true;
        }
    }
}

/// Panic re-entry (lead answer 1): every worker is stopped as a service stop
/// stops them, everything in memory is dropped except the lock, the sockets
/// and the endpoint, and startup recovery runs again from the disk. The
/// kept-version flag only ever grows, so a prune can't take what an earlier
/// start kept.
pub async fn reenter<H: Host, D: Driver>(
    cfg: &Config,
    host: &H,
    driver: &D,
    registry: &mut Registry,
    clock: &dyn Clock,
    started: &mut Started,
) -> io::Result<()> {
    registry
        .stop_all(cfg.timings.stop, Duration::from_secs(2))
        .await;
    registry.wait_until_empty().await;
    started.worker = None;
    // Re-entry is an in-process restart and the switch-off persists nothing:
    // it starts as a fresh process on the same disk would. The flag and the
    // ladder switch-off's own reason are dropped; any other reason stays.
    started.switched_off = false;
    if std::mem::take(&mut started.ladder_off_reason) {
        started.switching_disabled = None;
    }
    let identity = Identity::current(cfg.kind)?;
    let rules = TrustRules::for_identity(&identity);
    let Recovery {
        executed,
        update_lock,
        untrusted,
        notes,
    } = if let Some(why) = untrusted_dirs(cfg, &rules)
        .or_else(|| untrusted_files(cfg, &rules))
        .or_else(|| {
            started
                .lock
                .is_none()
                .then(|| "the supervisor lock is not held".to_owned())
        }) {
        disabled_recovery(cfg, host, clock, why).await?
    } else {
        Box::pin(recover(cfg, host, &started.store, clock, &rules)).await?
    };
    started.journal = executed.journal;
    started.journal_stale = executed.journal_stale;
    started.state_pending = executed.state_pending;
    started.run = executed.run;
    started.degraded = executed.degraded;
    started.switching_disabled = executed
        .disabled
        .or(untrusted)
        .or(started.switching_disabled.take());
    started.keep_all_versions |= executed.keep_all;
    (started.installed_rung, started.installed_sha256) =
        known_installed_rung(cfg, host, started.journal.as_ref());
    started.notes.extend(notes);
    let disabled_lock = if started.switching_disabled.is_some() {
        update_lock
    } else {
        drop(update_lock);
        None
    };
    launch(cfg, host, driver, registry, started).await;
    drop(disabled_lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_execution_stage_conflict_keeps_its_reason_and_the_install_refusal_note() {
        struct FixtureHost;
        impl Host for FixtureHost {
            fn topology(&self) -> Vec<crate::protocol::ListenerTopology> {
                Vec::new()
            }
            fn bind(&self) -> io::Result<Vec<(String, TcpListener)>> {
                Ok(Vec::new())
            }
            fn effective_mode(&self) -> crate::host::ModeSnapshot {
                crate::host::ModeSnapshot {
                    off: false,
                    mode_gen: 0,
                }
            }
            fn own_version(&self) -> String {
                "1.0.0".into()
            }
            fn own_sha256(&self) -> Option<String> {
                None
            }
            fn waiting_for_update_lock(&self, _: Duration) {}
        }
        struct FixtureRunner;
        impl Runner for FixtureRunner {
            fn version<'a>(
                &'a self,
                _: &'a Path,
                _: Duration,
            ) -> open_ferry_update::fetch::BoxFuture<'a, Result<String, String>> {
                Box::pin(async { Ok("open-ferry 1.0.0".into()) })
            }
        }
        let dir = if cfg!(unix) {
            tempfile::tempdir_in(
                std::env::var_os("HOME").expect("trust checks require a non-/tmp root"),
            )
            .unwrap()
        } else if let Some(root) = std::env::var_os("OFP_TEST_SCRATCH") {
            tempfile::tempdir_in(root).unwrap()
        } else {
            tempfile::tempdir_in(".").unwrap()
        };
        let data = DataDir::at(dir.path().join("data"));
        let installed = dir.path().join("installed");
        std::fs::write(&installed, b"installed bytes").unwrap();
        std::fs::create_dir_all(data.version_dir("1.0.0")).unwrap();
        std::fs::write(data.binary("1.0.0", "binary"), b"conflicting bytes").unwrap();
        let hash = open_ferry_update::staging::hash_file(&installed).unwrap();
        let artifact = Artifact {
            version: "1.0.0".into(),
            sha256: hash.clone(),
            size: 15,
        };
        let load = JournalLoad::Loaded(Journal::first_start(
            artifact.clone(),
            Some(Installed::from(&artifact)),
        ));
        let facts = Facts {
            now: chrono::Utc::now(),
            self_sha256: Some(hash.clone()),
            self_version: "1.0.0".into(),
            installed_sha256: Some(hash),
            installed_size: 15,
            installed_path: installed.clone(),
            versions: BTreeMap::new(),
        };
        let cfg = Config {
            data,
            installed,
            config: dir.path().join("config"),
            kind: ServiceKind::User,
            timings: Timings::default(),
            receipt: serde_json::json!({}),
            binary_name: "binary".into(),
            runner: Arc::new(FixtureRunner),
            detect_timeout: Duration::from_secs(5),
            endpoint_location: None,
        };
        let rules = TrustRules::for_identity(&Identity::current(cfg.kind).unwrap());
        let install_refusal = check_file(&cfg.data.binary("2.0.0", "binary"), &rules)
            .unwrap_err()
            .to_string();
        let mut plan = plan_startup(&load, &facts);
        assert!(matches!(plan.run, RunChoice::VerifiedInstalled(_)));
        plan.notes.push(install_refusal.clone());
        let lock = Arc::new(cfg.data.try_lock().unwrap().unwrap());
        let mut executed = execute(
            &cfg,
            &FixtureHost,
            &JournalStore::new(cfg.data.root()),
            &load,
            plan,
            &facts,
            &lock,
        )
        .await;
        let specific = executed.disabled.clone().unwrap();
        assert!(specific.contains("can't be kept"), "{specific}");
        apply_install_disabled(&mut executed, install_refusal.clone());
        assert_eq!(executed.disabled.as_deref(), Some(specific.as_str()));
        assert!(executed.notes.contains(&install_refusal));
    }

    #[test]
    fn a_completed_recovery_install_is_not_cleared_until_its_directory_is_durable() {
        use open_ferry_update::journal::InstallPending;
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("installed");
        let target = Artifact {
            version: "2.0.0".into(),
            sha256: "b".repeat(64),
            size: 1,
        };
        let mut old = Journal::first_start(
            target.clone(),
            Some(Installed {
                version: "1.0.0".into(),
                sha256: "a".repeat(64),
            }),
        );
        old.install_pending = Some(InstallPending {
            target: target.clone(),
            source: "kept".into(),
            aside: "real-aside".into(),
            attempts: 1,
            next_try: None,
            started: chrono::Utc::now().to_rfc3339(),
            record_id: Some("install".into()),
        });
        let facts = Facts {
            now: chrono::Utc::now(),
            self_sha256: Some(target.sha256.clone()),
            self_version: "2.0.0".into(),
            installed_sha256: Some(target.sha256.clone()),
            installed_size: 1,
            installed_path: installed.clone(),
            versions: BTreeMap::from([("2.0.0".into(), target.sha256.clone())]),
        };
        let store = JournalStore::new(dir.path());
        store.write(&mut old, "fixture").unwrap();
        let load = JournalLoad::Loaded(old.clone());
        for succeeds in [false, true] {
            let mut plan = plan_startup(&load, &facts);
            assert!(matches!(plan.install, InstallAction::None));
            assert!(
                plan.journal_write
                    .as_ref()
                    .unwrap()
                    .install_pending
                    .is_none()
            );
            let called = std::cell::Cell::new(false);
            sync_recovered_install(&installed, &load, &facts, &mut plan, &|parent| {
                assert_eq!(parent, dir.path());
                called.set(true);
                if succeeds {
                    Ok(())
                } else {
                    Err(io::Error::other("test directory sync failed"))
                }
            });
            assert!(called.get(), "recovery skipped directory durability");
            let mut written = plan.journal_write.clone().unwrap_or_else(|| old.clone());
            store.write(&mut written, "recovery").unwrap();
            let JournalLoad::Loaded(on_disk) = store.load().unwrap() else {
                panic!("no journal")
            };
            if succeeds {
                assert!(on_disk.install_pending.is_none());
            } else {
                assert_eq!(on_disk.install_pending, old.install_pending);
                assert_eq!(on_disk.installed, old.installed);
                assert!(plan.upserts.is_empty());
                assert!(plan.record_patches.is_empty());
                assert!(
                    plan.notes
                        .iter()
                        .any(|note| note.contains("retaining the pending install"))
                );
            }
        }
    }

    #[tokio::test]
    async fn a_join_error_reloads_the_sequence_that_the_install_task_published() {
        let dir = tempfile::tempdir().unwrap();
        let store = JournalStore::new(dir.path());
        let artifact = Artifact {
            version: "1.0.0".into(),
            sha256: "a".repeat(64),
            size: 1,
        };
        let mut journal = Journal::first_start(artifact, None);
        store.write(&mut journal, "test").unwrap();
        let old_seq = journal.seq;
        let writer = store.clone();
        let result = blocking(move || {
            writer.write(&mut journal, "J-install").unwrap();
            panic!("test install task ended after journaling");
        })
        .await;
        assert!(result.is_err());
        assert_eq!(journal_after_join_error(&store).unwrap().seq, old_seq + 1);
    }

    #[tokio::test]
    async fn cancelling_a_blocking_install_keeps_the_update_lock_until_it_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        let lock = Arc::new(data.try_lock().unwrap().unwrap());
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(blocking_with_lock(lock, move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        entered_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let still_held = data.try_lock().unwrap().is_none();
        release_tx.send(()).unwrap();
        assert!(still_held, "cancel released the update lock mid-install");
        // A parallel child's fork-to-exec window can briefly retain our flock fd.
        tokio::time::timeout(Duration::from_secs(5), async {
            while data.try_lock().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
