//! Not upstream's: the switch journal, `switch-journal.json`.
//!
//! The journal is what the supervisor trusts after a crash: which artifact
//! is active, which one last ended a watch window cleanly (`confirmed`),
//! the switch in progress, the installed file still to be replaced, the
//! workers it started and the state-file records not yet written
//! (`outcomes`). It holds no secrets: versions, hashes, pids and messages.
//! One process writes it (the supervisor); every updater path reads it
//! through [`JournalView`] under the update lock.
//!
//! There is no `pins` field: a pin is a file of its own ([`crate::pins`]),
//! because a caller must not wait on the supervisor while it holds the
//! update lock.
//!
//! ```json
//! {
//!   "format": 1, "seq": 7,
//!   "active": {"version": "0.3.0", "sha256": "<hex>", "size": 31457280},
//!   "confirmed": {"version": "0.2.0", "sha256": "<hex>", "size": 31000000},
//!   "previous": null,
//!   "installed": {"version": "0.2.0", "sha256": "<hex>"},
//!   "install_pending": null,
//!   "workers": [{"pid": 4242, "started": 1791460800, "version": "0.3.0", "role": "active"}],
//!   "switch": {"id": "7c1f", "how": "update", "from": {}, "to": {}, "phase": "watching",
//!              "started": "2026-10-09T10:00:00Z", "watch_until": "2026-10-09T10:10:00Z",
//!              "admitted": {"while_off": false, "mode_gen": 0}, "return": null},
//!   "outcomes": []
//! }
//! ```

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::pins::Pin;
use crate::publication::{create_dir_all_durable, publish_file, replace_file, sync_parent};
use crate::state::SwitchRecord;

/// The journal's format. A binary that reads a newer one refuses to change
/// anything.
pub const FORMAT: u32 = 1;

/// The journal's file name, in the data directory.
pub const JOURNAL_FILE: &str = "switch-journal.json";

/// How many times a journal write is tried, and the wait between tries.
pub const JOURNAL_WRITE_TRIES: u32 = 3;
pub const JOURNAL_WRITE_WAIT: Duration = Duration::from_millis(100);

/// The first wait before an install is retried, and the longest.
pub const INSTALL_RETRY_FIRST: Duration = Duration::from_secs(60);
pub const INSTALL_RETRY_MAX: Duration = Duration::from_secs(3600);

/// The wait before try number `attempts + 1` of a failed install: one
/// minute, doubling to one hour.
pub fn install_backoff(attempts: u32) -> Duration {
    let doubled =
        INSTALL_RETRY_FIRST.saturating_mul(1u32.checked_shl(attempts).unwrap_or(u32::MAX));
    doubled.min(INSTALL_RETRY_MAX)
}

/// A kept binary: `versions/<version>/open-ferry[.exe]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub version: String,
    pub sha256: String,
    pub size: u64,
}

/// What the installed path held at the last verified commit or adoption.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub version: String,
    pub sha256: String,
}

impl From<&Artifact> for Installed {
    fn from(artifact: &Artifact) -> Self {
        Self {
            version: artifact.version.clone(),
            sha256: artifact.sha256.clone(),
        }
    }
}

/// The installed file still has to be replaced by `target`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallPending {
    pub target: Artifact,
    /// Where `target` is copied from, as a path.
    pub source: String,
    /// The name the old file is moved to, chosen and journalled before the
    /// first rename.
    pub aside: String,
    pub attempts: u32,
    /// When the next try is due, RFC 3339; `None` is now.
    #[serde(default)]
    pub next_try: Option<String>,
    /// When the install was first due, RFC 3339.
    pub started: String,
    /// The id of the state record that reports this install.
    #[serde(default)]
    pub record_id: Option<String>,
}

/// What a worker does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerRole {
    Active,
    Standby,
    Draining,
}

/// A worker the supervisor started. `started` is the process start time as
/// the process runner reports it, opaque here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worker {
    pub pid: u32,
    pub started: u64,
    pub version: String,
    pub role: WorkerRole,
    /// Its control channel closed while the process lives (6.2).
    #[serde(default)]
    pub lost: bool,
}

/// What a switch is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum How {
    Update,
    Rollback,
}

impl How {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Rollback => "rollback",
        }
    }
}

/// Where a switch is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Loading,
    HandingOver,
    TakingOver,
    Watching,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::HandingOver => "handing-over",
            Self::TakingOver => "taking-over",
            Self::Watching => "watching",
        }
    }
}

/// Who is to blame for a failed switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Blame {
    NewVersion,
    Environment,
}

impl Blame {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewVersion => "new-version",
            Self::Environment => "environment",
        }
    }
}

/// The mode when the switch was admitted (5.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admitted {
    pub while_off: bool,
    pub mode_gen: u64,
}

/// The no-oscillation decision (6.2), journalled before it is acted on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Return {
    pub target: Artifact,
    pub blame: Blame,
    pub why: String,
    /// When it was decided, RFC 3339.
    pub decided: String,
    /// The target is the installed file, not the kept copy of the same
    /// version: a rung is a location and its artifact, and the artifact alone
    /// can't tell the two apart.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub installed: bool,
}

/// The switch in progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Switch {
    pub id: String,
    pub how: How,
    pub from: Artifact,
    pub to: Artifact,
    pub phase: Phase,
    /// RFC 3339.
    pub started: String,
    #[serde(default)]
    pub watch_until: Option<String>,
    #[serde(default)]
    pub admitted: Admitted,
    #[serde(default, rename = "return")]
    pub return_: Option<Return>,
}

/// A rung of a ladder: a location (a kept copy of the artifact, or the
/// installed file) and the artifact run from it. The artifact alone can't say
/// which: a kept copy and the installed file may hold the same bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rung {
    pub installed: bool,
    pub artifact: Artifact,
}

/// Where a ladder stands (6.1 g), so a supervisor that restarts goes on from
/// it and does not give a rung three more tries every time it starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LadderRecord {
    /// The ladder it belongs to: `None` is the restart ladder, `Some(id)` the
    /// return of switch `id`. A count never applies to another ladder.
    pub switch: Option<String>,
    /// The rung being tried.
    pub current: Option<Rung>,
    /// Starts of `current` since it last reported `loaded`, the one in flight
    /// included: a crash while it runs counts it as failed.
    pub attempts: u32,
    /// The rungs this cycle has used up.
    #[serde(default)]
    pub spent: Vec<Rung>,
}

/// A release to mark failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkFailed {
    pub version: String,
    pub why: String,
}

/// A terminal outcome the state file has not been given yet. It travels in
/// the same atomic write that clears or replaces `switch`, and is removed by
/// id only after its state upsert succeeded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub record: SwitchRecord,
    #[serde(default)]
    pub mark_failed: Option<MarkFailed>,
}

/// `switch-journal.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    pub format: u32,
    /// Grows with every write.
    pub seq: u64,
    /// What workers run from.
    pub active: Artifact,
    /// The newest artifact whose window ended cleanly, or that was adopted:
    /// what recovery runs.
    pub confirmed: Artifact,
    /// The manual rollback target.
    #[serde(default)]
    pub previous: Option<Artifact>,
    #[serde(default)]
    pub installed: Option<Installed>,
    #[serde(default)]
    pub install_pending: Option<InstallPending>,
    #[serde(default)]
    pub workers: Vec<Worker>,
    #[serde(default)]
    pub switch: Option<Switch>,
    #[serde(default)]
    pub outcomes: Vec<Outcome>,
    /// Where the ladder a worker is being started from stands. Absent when
    /// nothing is being retried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ladder: Option<LadderRecord>,
}

impl Journal {
    /// The journal of a first start (or of a rebuild) running `artifact`.
    pub fn first_start(artifact: Artifact, installed: Option<Installed>) -> Self {
        Self {
            format: FORMAT,
            seq: 0,
            active: artifact.clone(),
            confirmed: artifact,
            previous: None,
            installed,
            install_pending: None,
            workers: Vec::new(),
            switch: None,
            outcomes: Vec::new(),
            ladder: None,
        }
    }

    /// Adds `outcome`. One with the same record id already held is merged
    /// into, as the state does ([`SwitchRecord::merge`]), so a partial later
    /// outcome (J6 after J5) keeps the fields the earlier one carried; its
    /// `mark_failed` replaces the held one only when it has one.
    pub fn push_outcome(&mut self, outcome: Outcome) {
        match self
            .outcomes
            .iter_mut()
            .find(|held| held.record.id == outcome.record.id)
        {
            Some(held) => {
                held.record.merge(&outcome.record);
                if outcome.mark_failed.is_some() {
                    held.mark_failed = outcome.mark_failed;
                }
            }
            None => self.outcomes.push(outcome),
        }
    }

    /// Removes the outcome with `id`, after its state upsert succeeded.
    pub fn remove_outcome(&mut self, id: &str) -> bool {
        let before = self.outcomes.len();
        self.outcomes.retain(|outcome| outcome.record.id != id);
        self.outcomes.len() != before
    }

    /// J5, the window ended cleanly: `confirmed = to`, `previous` = the old
    /// `confirmed`, `install_pending` set, `switch` cleared and the outcome
    /// carried, in this one value. `false` when no switch is open.
    pub fn confirm(&mut self, outcome: Outcome, install: Option<InstallPending>) -> bool {
        let Some(switch) = self.switch.take() else {
            return false;
        };
        // `previous` is written only when `confirmed` moves.
        if self.confirmed != switch.to {
            self.previous = Some(std::mem::replace(&mut self.confirmed, switch.to.clone()));
        }
        self.active = switch.to;
        self.ladder = None;
        self.install_pending = install;
        self.push_outcome(outcome);
        true
    }

    /// J-R2, after a return: `active = confirmed = return.target`, `switch`
    /// cleared and the outcome carried. `previous` does not change, except
    /// that a kept copy that was `previous` and has now become `confirmed` is
    /// cleared here: recovery leaves it in place while the return is open, so
    /// the return ladder still holds it.
    /// `false` when no return is journalled.
    pub fn finish_return(&mut self, outcome: Outcome) -> bool {
        let Some((target, installed)) = self
            .switch
            .as_ref()
            .and_then(|switch| switch.return_.as_ref())
            .map(|ret| (ret.target.clone(), ret.installed))
        else {
            return false;
        };
        self.switch = None;
        // The return's ladder ends here: a new cycle starts from nothing.
        self.ladder = None;
        self.active = target.clone();
        if !installed && self.previous.as_ref() == Some(&target) {
            self.previous = None;
        }
        self.confirmed = target;
        self.push_outcome(outcome);
        true
    }

    /// Adopts `artifact`, the file an installer or `update` put in place:
    /// it becomes `active` and `confirmed`, and the old `confirmed` becomes
    /// `previous`.
    pub fn adopt(&mut self, artifact: Artifact) {
        self.installed = Some(Installed::from(&artifact));
        if artifact != self.confirmed {
            self.previous = Some(std::mem::replace(&mut self.confirmed, artifact.clone()));
        }
        self.active = artifact;
    }

    /// The versions that are never pruned or overwritten (3.5 and T3):
    /// `active`, `confirmed`, `previous`, `installed`, the install target,
    /// `switch.from`, `switch.to`, `switch.return.target`, the version of
    /// every worker `is_live(pid, started)` says still exists, and every pin
    /// that has not expired. Not `state.staged`, and not the newest other
    /// version ([`add_newest_other`]): the caller adds those.
    pub fn keep_set(
        &self,
        pins: &[Pin],
        now: DateTime<Utc>,
        is_live: impl Fn(u32, u64) -> bool,
    ) -> BTreeSet<String> {
        let mut keep = BTreeSet::new();
        keep.insert(self.active.version.clone());
        keep.insert(self.confirmed.version.clone());
        if let Some(previous) = &self.previous {
            keep.insert(previous.version.clone());
        }
        if let Some(installed) = &self.installed {
            keep.insert(installed.version.clone());
        }
        if let Some(pending) = &self.install_pending {
            keep.insert(pending.target.version.clone());
        }
        if let Some(switch) = &self.switch {
            keep.insert(switch.from.version.clone());
            keep.insert(switch.to.version.clone());
            if let Some(ret) = &switch.return_ {
                keep.insert(ret.target.version.clone());
            }
        }
        for worker in &self.workers {
            if is_live(worker.pid, worker.started) {
                keep.insert(worker.version.clone());
            }
        }
        for pin in pins.iter().filter(|pin| pin.live_at(now)) {
            keep.insert(pin.version.clone());
        }
        keep
    }
}

/// Adds the newest version of `on_disk` that `keep` lacks, so two or three
/// versions stay.
pub fn add_newest_other(keep: &mut BTreeSet<String>, on_disk: &[String]) {
    let newest = on_disk
        .iter()
        .filter(|version| !keep.contains(*version))
        .max_by(
            |a, b| match (semver::Version::parse(a), semver::Version::parse(b)) {
                (Ok(a), Ok(b)) => a.cmp(&b),
                (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
                (Err(_), Ok(_)) => std::cmp::Ordering::Less,
                (Err(_), Err(_)) => a.cmp(b),
            },
        );
    if let Some(newest) = newest {
        keep.insert(newest.clone());
    }
}

/// Why the journal could not be used.
#[derive(Debug)]
pub enum JournalError {
    Io(io::Error),
    /// Its `format` is newer than this binary knows.
    Newer {
        found: u32,
        supported: u32,
    },
    /// It doesn't parse.
    Unreadable(String),
    /// `seq` is at its maximum and can't grow.
    SeqOverflow,
    /// The new journal was moved into place but syncing its directory failed
    /// every try. The file on disk is the new one (and `seq` is advanced to
    /// match), but a crash now could still lose it: not "not written", and
    /// not a durable success either.
    NotDurable(io::Error),
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SeqOverflow => write!(f, "the switch journal's sequence number is exhausted"),
            Self::Io(error) => write!(f, "the switch journal couldn't be used: {error}"),
            Self::NotDurable(error) => write!(
                f,
                "the switch journal was written but its directory couldn't be synced: {error}"
            ),
            Self::Newer { found, supported } => write!(
                f,
                "journal format {found} is newer than this version supports ({supported})"
            ),
            Self::Unreadable(why) => write!(f, "the switch journal is unreadable: {why}"),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<io::Error> for JournalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// What reading the journal file found.
// A journal is read once at start; boxing it would only make every caller
// unwrap it.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum JournalLoad {
    /// There is no file.
    Missing,
    Loaded(Journal),
    /// A newer `format`: not parsed further.
    Newer {
        found: u32,
    },
    /// The file doesn't parse (or has no usable `format`).
    Unreadable {
        reason: String,
    },
}

/// Reads a journal from `bytes`.
pub fn parse(bytes: &[u8]) -> JournalLoad {
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(error) => {
            return JournalLoad::Unreadable {
                reason: error.to_string(),
            };
        }
    };
    match value.get("format").and_then(serde_json::Value::as_u64) {
        None | Some(0) => JournalLoad::Unreadable {
            reason: "it has no valid format".to_owned(),
        },
        Some(found) if found > u64::from(FORMAT) => JournalLoad::Newer {
            found: u32::try_from(found).unwrap_or(u32::MAX),
        },
        Some(_) => match serde_json::from_value::<Journal>(value) {
            Ok(journal) => JournalLoad::Loaded(journal),
            Err(error) => JournalLoad::Unreadable {
                reason: error.to_string(),
            },
        },
    }
}

fn read_load(path: &Path) -> io::Result<JournalLoad> {
    match fs::read(path) {
        Ok(bytes) => Ok(parse(&bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(JournalLoad::Missing),
        Err(error) => Err(error),
    }
}

/// The journal file in a data directory, for the one process that writes it.
#[derive(Clone, Debug)]
pub struct JournalStore {
    root: PathBuf,
    retry_wait: Duration,
}

impl JournalStore {
    /// The journal of the data directory at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            retry_wait: JOURNAL_WRITE_WAIT,
        }
    }

    /// The same store with another wait between write tries (tests).
    pub fn with_retry_wait(mut self, wait: Duration) -> Self {
        self.retry_wait = wait;
        self
    }

    pub fn path(&self) -> PathBuf {
        self.root.join(JOURNAL_FILE)
    }

    /// Reads the journal, changing nothing.
    pub fn load(&self) -> Result<JournalLoad, JournalError> {
        Ok(read_load(&self.path())?)
    }

    /// Reads the journal; a file that doesn't parse is renamed to
    /// `switch-journal.json.bad.<seq>` first and reported as unreadable.
    pub fn load_quarantining(&self) -> Result<(JournalLoad, Option<PathBuf>), JournalError> {
        let load = self.load()?;
        if matches!(load, JournalLoad::Unreadable { .. }) {
            let moved = self.quarantine()?;
            return Ok((load, Some(moved)));
        }
        Ok((load, None))
    }

    /// Renames the journal file to `.bad.<seq>`, `seq` being the one it
    /// records when that can still be read, else counting up from 0, and
    /// never over an earlier one.
    pub fn quarantine(&self) -> io::Result<PathBuf> {
        let path = self.path();
        let bytes = fs::read(&path)?;
        let mut seq = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value.get("seq").and_then(serde_json::Value::as_u64))
            .unwrap_or(0);
        loop {
            let target = self.root.join(format!("{JOURNAL_FILE}.bad.{seq}"));
            // An exclusive publish: a `.bad.<seq>` that appears meanwhile is
            // never replaced, the next number is tried.
            match publish_file(&path, &target) {
                Ok(()) => {
                    // The move has happened: the journal is already gone
                    // from its path. A failed sync is only a warning, so
                    // the caller doesn't fail closed on a journal that no
                    // longer exists (as `commit_with` treats it).
                    let synced = retry_sync(SYNC_TRIES, SYNC_WAIT, || {
                        landed_seam(&self.root, &format!("{JOURNAL_FILE}.bad"))?;
                        sync_parent(&target)
                    });
                    if let Err(error) = synced {
                        tracing::warn!(
                            "the journal was set aside as {} but syncing its directory failed, \
                             so the move may not survive a power loss: {error}",
                            target.display()
                        );
                    }
                    return Ok(target);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            seq = seq
                .checked_add(1)
                .ok_or_else(|| io::Error::other("no free name to set the journal aside"))?;
        }
    }

    /// Writes `journal` atomically and durably, `write` naming the step
    /// (`J1`, `J-R1`, ...) for the test seams. `seq` grows by one when it
    /// lands, and is left as it was when it doesn't. Tried
    /// [`JOURNAL_WRITE_TRIES`] times. Refuses to replace a journal of a newer
    /// format. When the new file has been moved into place but syncing its
    /// directory failed (after its own tries), `seq` stays advanced and the
    /// error is [`JournalError::NotDurable`]: the disk holds the new journal.
    pub fn write(&self, journal: &mut Journal, write: &str) -> Result<(), JournalError> {
        let path = self.path();
        // Fail closed: neither a newer nor an unreadable journal is
        // overwritten. An unreadable one is set aside first, by
        // `load_quarantining`.
        match read_load(&path)? {
            JournalLoad::Newer { found } => {
                return Err(JournalError::Newer {
                    found,
                    supported: FORMAT,
                });
            }
            JournalLoad::Unreadable { reason } => return Err(JournalError::Unreadable(reason)),
            JournalLoad::Missing | JournalLoad::Loaded(_) => {}
        }
        let _ = self.seam("before", write);
        let previous = (journal.format, journal.seq);
        journal.format = FORMAT;
        let Some(next) = journal.seq.checked_add(1) else {
            journal.format = previous.0;
            return Err(JournalError::SeqOverflow);
        };
        journal.seq = next;
        let data = match serde_json::to_vec_pretty(journal) {
            Ok(mut data) => {
                data.push(b'\n');
                data
            }
            Err(error) => {
                (journal.format, journal.seq) = previous;
                return Err(io::Error::other(error).into());
            }
        };
        let mut last = None;
        for attempt in 0..JOURNAL_WRITE_TRIES {
            if attempt > 0 {
                std::thread::sleep(self.retry_wait);
            }
            let result = match self.seam("fail", write) {
                Ok(()) => atomic_write_landed(&path, &data, true),
                Err(error) => Err(AtomicFailure::not_landed(error)),
            };
            match result {
                Ok(()) => {
                    let _ = self.seam("after", write);
                    return Ok(());
                }
                // The new file is in place, so this isn't a failed write:
                // `seq` stays advanced to match the disk, and the write is
                // not tried again (it would only move the same bytes).
                Err(failure) if failure.landed => {
                    return Err(JournalError::NotDurable(failure.error));
                }
                Err(failure) => last = Some(failure.error),
            }
        }
        (journal.format, journal.seq) = previous;
        Err(JournalError::Io(
            last.unwrap_or_else(|| io::Error::other("no write was tried")),
        ))
    }

    #[cfg(feature = "blue-green-test")]
    fn seam(&self, kind: &str, write: &str) -> io::Result<()> {
        crate::test_seams::hit(&self.root, &format!("journal.{kind}:{write}"))
    }

    #[cfg(not(feature = "blue-green-test"))]
    fn seam(&self, _kind: &str, _write: &str) -> io::Result<()> {
        Ok(())
    }
}

/// A read-only look at the journal, for every updater path that deletes or
/// changes anything, used under the update lock.
#[derive(Clone, Debug)]
pub struct JournalView {
    format: u32,
    journal: Option<Journal>,
}

impl JournalView {
    /// Reads the journal file at `path`. No file is an empty view. A file
    /// that can't be read or doesn't parse is an error: the caller must not
    /// delete or overwrite anything then. A newer `format` loads (with no
    /// journal), and [`Self::ensure_supported`] refuses it.
    pub fn load(path: &Path) -> Result<Self, JournalError> {
        match read_load(path)? {
            JournalLoad::Missing => Ok(Self {
                format: FORMAT,
                journal: None,
            }),
            JournalLoad::Loaded(journal) => Ok(Self {
                format: journal.format,
                journal: Some(journal),
            }),
            JournalLoad::Newer { found } => Ok(Self {
                format: found,
                journal: None,
            }),
            JournalLoad::Unreadable { reason } => Err(JournalError::Unreadable(reason)),
        }
    }

    /// The file's `format` ([`FORMAT`] when there is no file).
    pub fn format(&self) -> u32 {
        self.format
    }

    /// Refuses a journal of a newer format, naming both.
    pub fn ensure_supported(&self) -> Result<(), JournalError> {
        if self.format > FORMAT {
            return Err(JournalError::Newer {
                found: self.format,
                supported: FORMAT,
            });
        }
        Ok(())
    }

    /// The journal, when there is one this binary can read.
    pub fn journal(&self) -> Option<&Journal> {
        self.journal.as_ref()
    }

    /// [`Journal::keep_set`]; empty without a journal. Call
    /// [`Self::ensure_supported`] first: a newer journal's keep set is
    /// unknown, not empty.
    pub fn keep_set(
        &self,
        pins: &[Pin],
        now: DateTime<Utc>,
        is_live: impl Fn(u32, u64) -> bool,
    ) -> BTreeSet<String> {
        match &self.journal {
            Some(journal) => journal.keep_set(pins, now, is_live),
            None => pins
                .iter()
                .filter(|pin| pin.live_at(now))
                .map(|pin| pin.version.clone())
                .collect(),
        }
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many temp names one write tries before giving up, and how many times
/// the final directory sync is tried once the file has landed.
const TEMP_TRIES: u32 = 64;
const SYNC_TRIES: u32 = 3;
const SYNC_WAIT: Duration = Duration::from_millis(50);

/// A write that failed, and whether the file was already in place when it did.
pub(crate) struct AtomicFailure {
    pub(crate) error: io::Error,
    /// The move into place succeeded; only the directory sync after it
    /// failed. The target holds the new bytes.
    pub(crate) landed: bool,
    /// The temporary file this write made, and why it could not be removed
    /// after the write failed: it is still beside the target.
    pub(crate) left_behind: Option<(PathBuf, io::Error)>,
}

impl AtomicFailure {
    fn not_landed(error: io::Error) -> Self {
        Self {
            error,
            landed: false,
            left_behind: None,
        }
    }
}

/// A tag for this run of the process, in every temp name. PID 1 in a
/// container is the same every run and the counter restarts at 0, so a temp
/// file left by a crash would otherwise be hit first by every later run.
pub(crate) fn run_tag() -> u64 {
    static TAG: OnceLock<u64> = OnceLock::new();
    *TAG.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok())
            .unwrap_or(0);
        // A stack address adds a little per-run randomness (ASLR).
        let local = 0u8;
        let address = u64::try_from((&raw const local).addr()).unwrap_or(0);
        nanos ^ address.rotate_left(32)
    })
}

fn temp_name(name: &str, tag: u64, counter: u64) -> String {
    format!(".{name}.{}.{tag:x}.{counter}.tmp", std::process::id())
}

/// Runs `sync` up to `tries` times, waiting `wait` between tries; the last
/// error when none works.
fn retry_sync(
    tries: u32,
    wait: Duration,
    mut sync: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let mut last = None;
    for attempt in 0..tries {
        if attempt > 0 {
            std::thread::sleep(wait);
        }
        match sync() {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no sync was tried")))
}

/// The seam between the move into place and the directory sync: a failure
/// file here makes every sync try fail, as a disk that moved the file and
/// then refused the sync would.
#[cfg(feature = "blue-green-test")]
fn landed_seam(dir: &Path, name: &str) -> io::Result<()> {
    crate::test_seams::hit(dir, &format!("atomic.landed:{name}"))
}

#[cfg(not(feature = "blue-green-test"))]
fn landed_seam(_dir: &Path, _name: &str) -> io::Result<()> {
    Ok(())
}

/// Writes `bytes` to `path` through a temporary file beside it: written,
/// fsynced, then moved into place. With `replace` it replaces any file
/// there. Without it the file is created only if absent, atomically, and
/// `ErrorKind::AlreadyExists` says it was not. (A clash on the temp name is
/// never reported that way: another name is tried.) The moves and directory
/// syncs use the shared filesystem publication primitives.
pub fn atomic_write(path: &Path, bytes: &[u8], replace: bool) -> io::Result<()> {
    atomic_write_landed(path, bytes, replace).map_err(|failure| failure.error)
}

/// [`atomic_write`], telling a failure before the move from one after it.
pub(crate) fn atomic_write_landed(
    path: &Path,
    bytes: &[u8],
    replace: bool,
) -> Result<(), AtomicFailure> {
    atomic_write_removing(path, bytes, replace, &mut |temp| fs::remove_file(temp))
}

/// [`atomic_write_landed`] with the removal of a failed write's temporary
/// file given, so a test can make it fail.
pub(crate) fn atomic_write_removing(
    path: &Path,
    bytes: &[u8],
    replace: bool,
    remove: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> Result<(), AtomicFailure> {
    atomic_write_in(
        path,
        bytes,
        replace,
        run_tag(),
        &mut || TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
        remove,
    )
}

#[cfg(test)]
fn atomic_write_with(
    path: &Path,
    bytes: &[u8],
    replace: bool,
    tag: u64,
    next_counter: &mut dyn FnMut() -> u64,
) -> Result<(), AtomicFailure> {
    atomic_write_in(path, bytes, replace, tag, next_counter, &mut |temp| {
        fs::remove_file(temp)
    })
}

fn atomic_write_in(
    path: &Path,
    bytes: &[u8],
    replace: bool,
    tag: u64,
    next_counter: &mut dyn FnMut() -> u64,
    remove: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> Result<(), AtomicFailure> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // The file's own directory is the root: nothing above it is synced unless
    // this call makes it (callers that know a higher root make it first).
    create_dir_all_durable(dir, dir).map_err(AtomicFailure::not_landed)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A `create_new` collision means the file is someone else's (a crashed
    // run's, say): touch nothing, try the next name. Only a temp this call
    // created is ever removed.
    let mut opened = None;
    for _ in 0..TEMP_TRIES {
        let temp = dir.join(temp_name(&name, tag, next_counter()));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => {
                opened = Some((temp, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(AtomicFailure::not_landed(error)),
        }
    }
    let Some((temp, mut file)) = opened else {
        // Not `AlreadyExists`: that is the destination's word.
        return Err(AtomicFailure::not_landed(io::Error::other(format!(
            "no free temporary name beside {} after {TEMP_TRIES} tries",
            path.display()
        ))));
    };
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let placed = written.and_then(|()| {
        if replace {
            replace_file(&temp, path)
        } else {
            publish_file(&temp, path)
        }
    });
    if let Err(error) = placed {
        // A temp that is already gone is gone; one that stays is named.
        let left_behind = match remove(&temp) {
            Err(removal) if removal.kind() != io::ErrorKind::NotFound => Some((temp, removal)),
            _ => None,
        };
        return Err(AtomicFailure {
            error,
            landed: false,
            left_behind,
        });
    }
    // The file is in place. A failure from here on is not "not written".
    retry_sync(SYNC_TRIES, SYNC_WAIT, || {
        landed_seam(dir, &name)?;
        sync_parent(path)
    })
    .map_err(|error| AtomicFailure {
        error,
        landed: true,
        left_behind: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pins::Pin;

    pub(crate) fn artifact(version: &str) -> Artifact {
        Artifact {
            version: version.to_owned(),
            sha256: format!("{:0<64}", version.replace('.', "")),
            size: 10,
        }
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sample() -> Journal {
        let mut journal =
            Journal::first_start(artifact("0.2.0"), Some(Installed::from(&artifact("0.2.0"))));
        journal.previous = Some(artifact("0.1.0"));
        journal.workers.push(Worker {
            pid: 42,
            started: 7,
            version: "0.2.0".into(),
            role: WorkerRole::Active,
            lost: false,
        });
        journal.switch = Some(Switch {
            id: "s1".into(),
            how: How::Update,
            from: artifact("0.2.0"),
            to: artifact("0.3.0"),
            phase: Phase::HandingOver,
            started: "2026-10-09T10:00:00Z".into(),
            watch_until: None,
            admitted: Admitted::default(),
            return_: None,
        });
        journal
    }

    #[test]
    fn serializes_without_a_pins_field_and_round_trips() {
        // Not upstream's: the journal has no `pins` (a pin is a file) and
        // the switch's `return` keeps its name.
        let journal = sample();
        let text = serde_json::to_string(&journal).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("pins").is_none());
        assert!(value.get("outcome").is_none());
        assert!(value["outcomes"].is_array());
        assert!(value["switch"].get("return").is_some());
        assert_eq!(value["switch"]["phase"], "handing-over");
        assert_eq!(value["workers"][0]["role"], "active");
        assert_eq!(parse(text.as_bytes()), JournalLoad::Loaded(journal));
    }

    #[test]
    fn parse_refuses_newer_and_reports_unreadable() {
        // Not upstream's: a newer format is not parsed further; junk and
        // a missing format are unreadable.
        assert_eq!(
            parse(br#"{"format":2,"anything":true}"#),
            JournalLoad::Newer { found: 2 }
        );
        assert!(matches!(parse(b"{nope"), JournalLoad::Unreadable { .. }));
        assert!(matches!(parse(b"{}"), JournalLoad::Unreadable { .. }));
        assert!(matches!(
            parse(br#"{"format":1}"#),
            JournalLoad::Unreadable { .. }
        ));
    }

    #[test]
    fn writes_bump_seq_and_load_back() {
        // Not upstream's: each landed write grows `seq` and is durable.
        let dir = tempfile::tempdir().unwrap();
        let store = JournalStore::new(dir.path());
        assert_eq!(store.load().unwrap(), JournalLoad::Missing);
        let mut journal = sample();
        store.write(&mut journal, "J1").unwrap();
        store.write(&mut journal, "J2").unwrap();
        assert_eq!(journal.seq, 2);
        assert_eq!(store.load().unwrap(), JournalLoad::Loaded(journal));
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn a_failed_write_keeps_the_old_file_and_seq() {
        // Not upstream's: JOURNAL_WRITE_TRIES tries, then an error that
        // leaves seq alone. The journal path is a directory so the move
        // fails every time.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(JOURNAL_FILE)).unwrap();
        let store = JournalStore::new(dir.path()).with_retry_wait(Duration::ZERO);
        let mut journal = sample();
        journal.seq = 4;
        let error = store.write(&mut journal, "J1").unwrap_err();
        assert!(matches!(error, JournalError::Io(_)), "{error}");
        assert_eq!(journal.seq, 4);
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_newer_journal_is_never_overwritten() {
        // Not upstream's: 3.8, a write refuses a newer format.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(JOURNAL_FILE);
        fs::write(&path, br#"{"format":9}"#).unwrap();
        let store = JournalStore::new(dir.path());
        let error = store.write(&mut sample(), "J1").unwrap_err();
        assert!(matches!(
            error,
            JournalError::Newer {
                found: 9,
                supported: FORMAT
            }
        ));
        assert!(error.to_string().contains("9") && error.to_string().contains('1'));
        assert_eq!(fs::read(&path).unwrap(), br#"{"format":9}"#);
    }

    #[test]
    fn an_unreadable_journal_is_quarantined_by_seq() {
        // Not upstream's: 3.3, renamed to `.bad.<seq>` and never over an
        // earlier one.
        let dir = tempfile::tempdir().unwrap();
        let store = JournalStore::new(dir.path());
        fs::write(store.path(), br#"{"format":1,"seq":5,"active":"broken"}"#).unwrap();
        let (load, moved) = store.load_quarantining().unwrap();
        assert!(matches!(load, JournalLoad::Unreadable { .. }));
        assert_eq!(moved.unwrap(), dir.path().join("switch-journal.json.bad.5"));
        assert!(!store.path().exists());
        fs::write(store.path(), br#"{"format":1,"seq":5,"active":"broken"}"#).unwrap();
        let (_, moved) = store.load_quarantining().unwrap();
        assert_eq!(moved.unwrap(), dir.path().join("switch-journal.json.bad.6"));
        assert_eq!(store.load_quarantining().unwrap().0, JournalLoad::Missing);
        fs::write(store.path(), b"not json").unwrap();
        let (_, moved) = store.load_quarantining().unwrap();
        assert_eq!(moved.unwrap(), dir.path().join("switch-journal.json.bad.0"));
    }

    #[test]
    fn atomic_write_replaces_or_refuses() {
        // Not upstream's: replace=true replaces; replace=false is a
        // no-replace create that leaves the old file and no temp file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("f.json");
        atomic_write(&path, b"one", false).unwrap();
        let error = atomic_write(&path, b"two", false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"one");
        atomic_write(&path, b"three", true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"three");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn a_stale_temp_with_the_first_name_does_not_stop_a_write() {
        // Not upstream's: P3. A crashed run leaves the exact name the first
        // write of the next run would pick (PID 1 in a container, counter
        // at 0). The write takes the next name and the stale file is
        // untouched. The counter is injected, so no other test can race it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.json");
        let stale = dir.path().join(temp_name("f.json", 7, 0));
        fs::write(&stale, b"theirs").unwrap();
        let mut counter = 0..;
        atomic_write_with(&path, b"mine", true, 7, &mut || counter.next().unwrap())
            .map_err(|failure| failure.error)
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"mine");
        assert_eq!(fs::read(&stale).unwrap(), b"theirs");
        // The no-replace mode takes the next name too.
        let other = dir.path().join("g.json");
        let stale = dir.path().join(temp_name("g.json", 7, 0));
        fs::write(&stale, b"theirs").unwrap();
        let mut counter = 0..;
        atomic_write_with(&other, b"mine", false, 7, &mut || counter.next().unwrap())
            .map_err(|failure| failure.error)
            .unwrap();
        assert_eq!(fs::read(&other).unwrap(), b"mine");
        assert_eq!(fs::read(&stale).unwrap(), b"theirs");
    }

    #[test]
    fn running_out_of_temp_names_is_not_a_clash_on_the_target() {
        // Not upstream's: P3. Every name tried is someone else's: the write
        // fails after TEMP_TRIES, with an error that is not `AlreadyExists`
        // (that means the target exists), and deletes nothing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.json");
        let mut others = Vec::new();
        for n in 0..u64::from(TEMP_TRIES) {
            let other = dir.path().join(temp_name("f.json", 7, n));
            fs::write(&other, b"theirs").unwrap();
            others.push(other);
        }
        let mut counter = 0..;
        let failure = atomic_write_with(&path, b"mine", false, 7, &mut || counter.next().unwrap())
            .err()
            .unwrap();
        assert!(!failure.landed);
        assert_ne!(failure.error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!path.exists());
        for other in &others {
            assert_eq!(fs::read(other).unwrap(), b"theirs", "{other:?}");
        }
        // A real name is also never one the process-wide counter's first
        // value would repeat across runs: it carries the run tag.
        assert!(temp_name("f.json", run_tag(), 0).contains(&format!("{:x}", run_tag())));
    }

    #[test]
    fn a_directory_sync_is_tried_three_times() {
        // Not upstream's: P4. Two failures then success is success; three
        // failures give the last error.
        let mut calls = 0;
        retry_sync(SYNC_TRIES, Duration::ZERO, || {
            calls += 1;
            if calls < 3 {
                Err(io::Error::other("busy"))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(calls, 3);
        let mut calls = 0;
        let error = retry_sync(SYNC_TRIES, Duration::ZERO, || {
            calls += 1;
            Err(io::Error::other(format!("no {calls}")))
        })
        .unwrap_err();
        assert_eq!(calls, 3);
        assert_eq!(error.to_string(), "no 3");
    }

    #[test]
    fn quarantine_never_replaces_an_earlier_set_aside_file() {
        // Not upstream's: P3/P2. The set-aside is an exclusive publish: an
        // existing `.bad.<seq>` is kept and the next number is used.
        let dir = tempfile::tempdir().unwrap();
        let store = JournalStore::new(dir.path());
        let earlier = dir.path().join("switch-journal.json.bad.5");
        fs::write(&earlier, b"earlier").unwrap();
        fs::write(store.path(), br#"{"format":1,"seq":5}"#).unwrap();
        let moved = store.quarantine().unwrap();
        assert_eq!(moved, dir.path().join("switch-journal.json.bad.6"));
        assert_eq!(fs::read(&earlier).unwrap(), b"earlier");
        assert!(!store.path().exists());
    }

    #[cfg(feature = "blue-green-test")]
    #[test]
    fn a_sync_failure_after_the_quarantine_rename_is_still_success() {
        // Not upstream's: U2-P3. The journal has moved to `.bad.N` when the
        // directory sync fails; that is a warning, not an error.
        let dir = tempfile::tempdir().unwrap();
        let seams = dir.path().join(".blue-green-test");
        fs::create_dir_all(&seams).unwrap();
        fs::write(
            seams.join(format!("atomic.landed_{JOURNAL_FILE}.bad.fail")),
            b"",
        )
        .unwrap();
        let store = JournalStore::new(dir.path());
        fs::write(store.path(), b"not json").unwrap();
        let moved = store.quarantine().unwrap();
        assert_eq!(moved, dir.path().join(format!("{JOURNAL_FILE}.bad.0")));
        assert!(moved.exists());
        assert!(!store.path().exists());
        // The named sync seam fired: its receipt has one byte per failed try.
        let receipt = seams.join(format!("atomic.landed_{JOURNAL_FILE}.bad.failed"));
        let fired = fs::read(&receipt).expect("the quarantine sync seam left no receipt");
        assert!(!fired.is_empty(), "the quarantine sync seam never fired");
    }

    #[test]
    fn a_sequence_at_its_maximum_is_a_typed_error() {
        // Not upstream's: `seq` overflow is `SeqOverflow`, not a panic, and
        // leaves the journal and the file as they were.
        let dir = tempfile::tempdir().unwrap();
        let store = JournalStore::new(dir.path());
        let mut journal = Journal::first_start(
            Artifact {
                version: "0.1.0".into(),
                sha256: "a".repeat(64),
                size: 1,
            },
            None,
        );
        journal.seq = u64::MAX;
        let error = store.write(&mut journal, "J1").unwrap_err();
        assert!(matches!(error, JournalError::SeqOverflow), "{error}");
        assert_eq!(journal.seq, u64::MAX);
        assert!(!store.path().exists());
    }

    #[test]
    fn the_view_reads_refuses_and_fails_closed() {
        // Not upstream's: test 20 and the fail-closed read. No file is an
        // empty view; a newer format loads and is refused; junk is an error.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(JOURNAL_FILE);
        let view = JournalView::load(&path).unwrap();
        assert!(view.journal().is_none() && view.ensure_supported().is_ok());
        assert_eq!(view.format(), FORMAT);

        fs::write(&path, br#"{"format":3,"future":1}"#).unwrap();
        let view = JournalView::load(&path).unwrap();
        assert_eq!(view.format(), 3);
        assert!(matches!(
            view.ensure_supported(),
            Err(JournalError::Newer {
                found: 3,
                supported: FORMAT
            })
        ));

        fs::write(&path, b"garbage").unwrap();
        assert!(matches!(
            JournalView::load(&path),
            Err(JournalError::Unreadable(_))
        ));

        let store = JournalStore::new(dir.path());
        fs::remove_file(&path).unwrap();
        let mut journal = sample();
        store.write(&mut journal, "J1").unwrap();
        let view = JournalView::load(&path).unwrap();
        assert_eq!(view.journal(), Some(&journal));
    }

    fn pin(version: &str, until: &str) -> Pin {
        Pin {
            version: version.into(),
            sha256: artifact(version).sha256,
            id: format!("pin-{version}"),
            until: until.into(),
        }
    }

    #[test]
    fn the_keep_set_is_every_named_version_and_live_workers_and_pins() {
        // Not upstream's: 3.5 plus T3. A dead worker's version is not kept;
        // an unexpired pin is, an expired one is not.
        let mut journal = sample();
        journal.install_pending = Some(InstallPending {
            target: artifact("0.4.0"),
            source: "x".into(),
            aside: "open-ferry.old".into(),
            attempts: 0,
            next_try: None,
            started: "2026-10-09T10:00:00Z".into(),
            record_id: None,
        });
        journal.switch.as_mut().unwrap().return_ = Some(Return {
            target: artifact("0.2.5"),
            blame: Blame::Environment,
            why: "w".into(),
            decided: "2026-10-09T10:00:00Z".into(),
            installed: false,
        });
        journal.workers.push(Worker {
            pid: 1,
            started: 1,
            version: "0.9.0".into(),
            role: WorkerRole::Draining,
            lost: false,
        });
        journal.workers.push(Worker {
            pid: 2,
            started: 2,
            version: "0.8.0".into(),
            role: WorkerRole::Draining,
            lost: false,
        });
        let now = at("2026-10-09T10:05:00Z");
        let pins = [
            pin("0.7.0", "2026-10-09T10:10:00Z"),
            pin("0.6.0", "2026-10-09T10:04:00Z"),
        ];
        let keep = journal.keep_set(&pins, now, |pid, started| pid == 1 && started == 1);
        let expected: BTreeSet<String> = [
            "0.1.0", "0.2.0", "0.2.5", "0.3.0", "0.4.0", "0.7.0", "0.9.0",
        ]
        .iter()
        .map(|version| (*version).to_owned())
        .collect();
        assert_eq!(keep, expected);

        let mut keep = keep;
        add_newest_other(
            &mut keep,
            &[
                "0.2.0".into(),
                "0.5.0".into(),
                "0.10.0".into(),
                "0.6.0".into(),
            ],
        );
        assert!(keep.contains("0.10.0") && !keep.contains("0.5.0"));
    }

    #[test]
    fn the_view_keep_set_without_a_journal_still_keeps_pins() {
        // Not upstream's: a pin written before the first journal.
        let view = JournalView {
            format: FORMAT,
            journal: None,
        };
        let keep = view.keep_set(
            &[pin("0.7.0", "2026-10-09T10:10:00Z")],
            at("2026-10-09T10:00:00Z"),
            |_, _| false,
        );
        assert_eq!(keep.len(), 1);
    }

    #[test]
    fn confirm_keeps_previous_when_confirmed_does_not_move() {
        // Not upstream's: a rollback that ended a forward watch has
        // `confirmed == to`; `previous` is written only when `confirmed`
        // moves, so it is not set to the same artifact.
        let mut journal = Journal::first_start(artifact("0.2.0"), None);
        journal.previous = Some(artifact("0.1.0"));
        journal.active = artifact("0.3.0");
        journal.switch = Some(Switch {
            id: "s1".into(),
            how: How::Rollback,
            from: artifact("0.3.0"),
            to: artifact("0.2.0"),
            phase: Phase::Watching,
            started: "t".into(),
            watch_until: None,
            admitted: Admitted::default(),
            return_: None,
        });
        let outcome = Outcome {
            record: SwitchRecord::default(),
            mark_failed: None,
        };
        assert!(journal.confirm(outcome, None));
        assert_eq!(journal.confirmed.version, "0.2.0");
        assert_eq!(journal.previous, Some(artifact("0.1.0")));
        assert_eq!(journal.active.version, "0.2.0");
    }

    #[test]
    fn a_write_refuses_an_unreadable_journal_until_it_is_set_aside() {
        // Not upstream's: fail closed, the file is kept as it was.
        let dir = tempfile::tempdir().unwrap();
        let store = JournalStore::new(dir.path());
        fs::write(store.path(), b"garbage").unwrap();
        let error = store.write(&mut sample(), "J1").unwrap_err();
        assert!(matches!(error, JournalError::Unreadable(_)));
        assert_eq!(fs::read(store.path()).unwrap(), b"garbage");
        store.load_quarantining().unwrap();
        store.write(&mut sample(), "J1").unwrap();
    }

    #[test]
    fn confirm_and_return_are_one_value_each() {
        // Not upstream's: T5, J5 and J-R2 carry the outcome and clear
        // `switch` together.
        let outcome = |id: &str| Outcome {
            record: SwitchRecord {
                id: id.into(),
                ..SwitchRecord::default()
            },
            mark_failed: None,
        };
        let mut journal = sample();
        assert!(journal.confirm(outcome("s1"), None));
        assert!(journal.switch.is_none());
        assert_eq!(journal.confirmed.version, "0.3.0");
        assert_eq!(journal.previous.as_ref().unwrap().version, "0.2.0");
        assert_eq!(journal.outcomes.len(), 1);
        assert!(!journal.confirm(outcome("s2"), None));
        assert!(journal.remove_outcome("s1") && !journal.remove_outcome("s1"));

        let mut journal = sample();
        assert!(!journal.finish_return(outcome("s1")));
        journal.switch.as_mut().unwrap().return_ = Some(Return {
            target: artifact("0.2.0"),
            blame: Blame::NewVersion,
            why: "w".into(),
            decided: "d".into(),
            installed: false,
        });
        journal.confirmed = artifact("0.3.0");
        assert!(journal.finish_return(outcome("s1")));
        assert_eq!(journal.active.version, "0.2.0");
        assert_eq!(journal.confirmed.version, "0.2.0");
        assert_eq!(journal.previous.as_ref().unwrap().version, "0.1.0");
        assert!(journal.switch.is_none());
    }

    // Not upstream's: Opus 9. A later outcome with the same id is merged into
    // the held one, so a partial J6 keeps what J5 carried, including its
    // `mark_failed` when J6 has none.
    #[test]
    fn a_partial_outcome_merges_into_the_held_one_with_the_same_id() {
        let mut j5 =
            SwitchRecord::started("s1", "update", "0.2.0", "0.3.0", "2026-10-09T10:00:00Z");
        j5.binary = "/bin/open-ferry".into();
        j5.gap_ms = Some(143);
        j5.confirm();
        let mut journal = sample();
        journal.push_outcome(Outcome {
            record: j5.clone(),
            mark_failed: Some(MarkFailed {
                version: "0.3.0".into(),
                why: "earlier".into(),
            }),
        });
        let j6 = SwitchRecord {
            id: "s1".into(),
            installed_binary: Some("replaced".into()),
            ..SwitchRecord::default()
        };
        journal.push_outcome(Outcome {
            record: j6,
            mark_failed: None,
        });
        assert_eq!(journal.outcomes.len(), 1);
        let held = &journal.outcomes[0];
        assert_eq!(held.record.from, "0.2.0");
        assert_eq!(held.record.at, j5.at);
        assert_eq!(held.record.binary, "/bin/open-ferry");
        assert_eq!(held.record.gap_ms, Some(143));
        assert!(held.record.confirmed && held.record.blue_green);
        assert_eq!(held.record.installed_binary.as_deref(), Some("replaced"));
        assert_eq!(held.mark_failed.as_ref().unwrap().why, "earlier");

        // A later mark_failed replaces the held one.
        journal.push_outcome(Outcome {
            record: SwitchRecord {
                id: "s1".into(),
                ..SwitchRecord::default()
            },
            mark_failed: Some(MarkFailed {
                version: "0.3.0".into(),
                why: "later".into(),
            }),
        });
        assert_eq!(
            journal.outcomes[0].mark_failed.as_ref().unwrap().why,
            "later"
        );
    }

    #[test]
    fn install_backoff_doubles_to_an_hour() {
        // Not upstream's: 1 min doubling to 1 h.
        let waits: Vec<u64> = (0..9).map(|n| install_backoff(n).as_secs()).collect();
        assert_eq!(waits, [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600]);
        assert_eq!(install_backoff(200), INSTALL_RETRY_MAX);
    }

    #[cfg(feature = "blue-green-test")]
    #[test]
    fn the_journal_write_seams_fail_and_hold_a_write() {
        // Not upstream's: `journal.fail:<write>` forces a write error (every
        // try), and `journal.before:<write>` holds a write until released.
        let dir = tempfile::tempdir().unwrap();
        let seams = dir.path().join(".blue-green-test");
        fs::create_dir_all(&seams).unwrap();
        let store = JournalStore::new(dir.path()).with_retry_wait(Duration::ZERO);

        fs::write(seams.join("journal.fail_J1.fail"), b"").unwrap();
        let mut journal = sample();
        let error = store.write(&mut journal, "J1").unwrap_err();
        assert!(matches!(error, JournalError::Io(_)));
        assert_eq!(journal.seq, 0);
        assert!(!store.path().exists());
        store.write(&mut journal, "J2").unwrap();
        assert!(store.path().exists());
        fs::remove_file(store.path()).unwrap();

        fs::write(seams.join("journal.before_J3.hold"), b"").unwrap();
        let held = {
            let store = store.clone();
            let mut journal = sample();
            std::thread::spawn(move || store.write(&mut journal, "J3"))
        };
        while !seams.join("journal.before_J3.arrived").exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!store.path().exists(), "held before the write lands");
        fs::write(seams.join("journal.before_J3.release"), b"").unwrap();
        held.join().unwrap().unwrap();
        assert!(store.path().exists());
    }

    #[cfg(feature = "blue-green-test")]
    #[test]
    fn a_failure_after_the_rename_is_not_durable_but_not_lost() {
        // Not upstream's: O-test and P4. The file is moved into place, then
        // every directory sync fails. The write is `NotDurable`, not "not
        // written": `seq` stays advanced, nothing is retried over it, no
        // temp file is left, and a reload reads the whole new journal (never
        // the old one, never a torn one).
        let dir = tempfile::tempdir().unwrap();
        let seams = dir.path().join(".blue-green-test");
        fs::create_dir_all(&seams).unwrap();
        let store = JournalStore::new(dir.path()).with_retry_wait(Duration::ZERO);

        let mut journal = sample();
        store.write(&mut journal, "J1").unwrap();
        assert_eq!(journal.seq, 1);

        fs::write(
            seams.join(format!("atomic.landed_{JOURNAL_FILE}.fail")),
            b"",
        )
        .unwrap();
        journal.active = artifact("0.3.0");
        let error = store.write(&mut journal, "J2").unwrap_err();
        assert!(matches!(error, JournalError::NotDurable(_)), "{error}");
        assert_eq!(journal.seq, 2, "memory matches the disk");
        match store.load().unwrap() {
            JournalLoad::Loaded(on_disk) => {
                assert_eq!(on_disk.seq, 2);
                assert_eq!(on_disk, journal);
            }
            other => panic!("{other:?}"),
        }
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);

        // The next write, once the disk is back, carries on from seq 2.
        fs::remove_file(seams.join(format!("atomic.landed_{JOURNAL_FILE}.fail"))).unwrap();
        store.write(&mut journal, "J3").unwrap();
        assert_eq!(journal.seq, 3);
        assert_eq!(store.load().unwrap(), JournalLoad::Loaded(journal));
    }
}
