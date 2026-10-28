//! `update-state.json`: what the last check found and what updates did.
//!
//! The file is JSON, written whole to a temporary file and renamed over the
//! old, so a reader never sees half of it. One that is missing or doesn't
//! parse reads as empty: the state is only a record, and the next check
//! writes it again. It holds no secrets: versions, times, hashes and
//! messages.
//!
//! ```json
//! {
//!   "format": 1,
//!   "last_check": "2026-10-08T12:00:00Z",
//!   "last_result": "staged",
//!   "last_error": null,
//!   "latest": "0.2.0",
//!   "notified": "0.2.0",
//!   "staged": "0.2.0",
//!   "staged_sha256": "<64 hex digits of the staged binary>",
//!   "previous": "0.1.0",
//!   "failed": ["0.1.5"],
//!   "rolled_back": null,
//!   "last_switch": {
//!     "from": "0.1.0", "to": "0.2.0", "at": "2026-10-08T12:05:00Z",
//!     "how": "update", "binary": "/home/me/.local/bin/open-ferry",
//!     "sha256": "<64 hex digits>", "size": 31457280,
//!     "modified_ms": 1791460800000, "restart_needed": true
//!   }
//! }
//! ```
//!
//! A supervised switch (blue-green) writes one record per operation, keyed
//! by `id`, when the new version is ready, and updates it in place when its
//! window ends or fails. The last 5 are in `history`, newest first;
//! `last_switch` stays the newest. The record's extra fields, each left out
//! when it has no value:
//!
//! ```json
//! {
//!   "id": "7c1f", "how": "update", "from": "0.2.0", "to": "0.3.0",
//!   "at": "2026-10-09T10:00:00Z", "result": "failed-after-switch",
//!   "phase": "watching", "blame": "new-version",
//!   "why": "the new version exited with code 101 four minutes after the switch",
//!   "blue_green": true, "returned_to": "0.2.0",
//!   "gap_ms": 143, "installed_binary": "unchanged"
//! }
//! ```
//!
//! `supervisor_version` and `needs_restart` say that a worker needed a newer
//! supervisor than the one running.
//!
//! Not upstream's: nothing here is a port.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The state file's format.
pub const FORMAT: u32 = 1;

/// How many failed versions are remembered.
const MAX_FAILED: usize = 10;

/// How many switch records `history` keeps.
pub const MAX_HISTORY: usize = 5;

/// The values of [`SwitchRecord::how`].
pub mod how {
    /// A forward update.
    pub const UPDATE: &str = "update";
    /// A requested rollback.
    pub const ROLLBACK: &str = "rollback";
}

/// The values of [`SwitchRecord::result`].
pub mod result {
    /// A forward update's new version became active.
    pub const SWITCHED: &str = "switched";
    /// A requested rollback completed (and nothing else is ever this).
    pub const ROLLED_BACK: &str = "rolled-back";
    /// The new version never became ready.
    pub const FAILED_TO_LOAD: &str = "failed-to-load";
    /// The new version became ready and then failed inside its window.
    pub const FAILED_AFTER_SWITCH: &str = "failed-after-switch";
}

/// The values of [`SwitchRecord::phase`]: where a switch failed.
pub mod phase {
    /// The new version was loading.
    pub const LOADING: &str = "loading";
    /// The old version was handing over.
    pub const HANDING_OVER: &str = "handing-over";
    /// The new version was taking over.
    pub const TAKING_OVER: &str = "taking-over";
    /// The new version ran inside its window.
    pub const WATCHING: &str = "watching";
}

/// The values of [`SwitchRecord::blame`].
pub mod blame {
    /// The new version is at fault: it is marked failed.
    pub const NEW_VERSION: &str = "new-version";
    /// The machine or the service is: the version is not marked failed.
    pub const ENVIRONMENT: &str = "environment";
}

/// The values of [`SwitchRecord::installed_binary`]: what became of the
/// installed file.
pub mod installed_binary {
    /// The confirm was written and the file is yet to be replaced.
    pub const PENDING: &str = "pending";
    /// The installed file is now the new version.
    pub const REPLACED: &str = "replaced";
    /// The installed file was never touched.
    pub const UNCHANGED: &str = "unchanged";
    /// Something else replaced the installed file; it was left alone.
    pub const CHANGED_ELSEWHERE: &str = "changed-elsewhere";
    /// The version it would have installed is no longer the one that will be
    /// confirmed: the install was dropped, the file never touched.
    pub const SUPERSEDED: &str = "superseded";

    /// The replacement failed, and is retried.
    pub fn failed(why: &str) -> String {
        format!("failed: {why}")
    }
}

/// What updates know between runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// [`FORMAT`].
    pub format: u32,
    /// When the last check ended, RFC 3339.
    pub last_check: Option<String>,
    /// How it ended: `up-to-date`, `update-available`, `staged`,
    /// `skipped`, `cannot-update` or `error`.
    pub last_result: Option<String>,
    /// Its error, when it failed.
    pub last_error: Option<String>,
    /// The latest release's version, when one was found.
    pub latest: Option<String>,
    /// Channel used by the last discovery, absent for older state files.
    pub channel: Option<String>,
    /// Where the found release came from: latest or preview.
    pub release_source: Option<String>,
    /// A failed source when another source remained usable.
    pub channel_error: Option<String>,
    /// The last version a check reported, so each is reported once.
    pub notified: Option<String>,
    /// The version staged for the next switch.
    pub staged: Option<String>,
    /// The staged binary's SHA-256, checked again before a switch.
    pub staged_sha256: Option<String>,
    /// The version before the last switch, kept for a rollback.
    pub previous: Option<String>,
    /// Versions whose binary failed its `--version` run; skipped until a
    /// newer release.
    pub failed: Vec<String>,
    /// A version rolled back from; automatic updates skip it until a newer
    /// release, a manual one may go to it again.
    pub rolled_back: Option<String>,
    /// The last switch.
    pub last_switch: Option<SwitchRecord>,
    /// The last [`MAX_HISTORY`] switch records, newest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<SwitchRecord>,
    /// The version of the supervisor that wrote the last record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supervisor_version: Option<String>,
    /// Whether the supervisor must be restarted to run the newest version
    /// (a worker needed a newer one).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub needs_restart: bool,
}

/// A switch from one binary to another.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SwitchRecord {
    /// The version switched from.
    pub from: String,
    /// The version switched to.
    pub to: String,
    /// When, RFC 3339.
    pub at: String,
    /// When the supervisor decided the outcome, RFC 3339. Older records
    /// only have `at`, the operation's start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// `update` or `rollback`.
    pub how: String,
    /// The binary replaced.
    pub binary: String,
    /// The SHA-256 of the binary put in its place.
    pub sha256: String,
    /// That binary's size, and its modification time in milliseconds
    /// since the epoch, to tell whether it is still the one installed.
    pub size: u64,
    pub modified_ms: i64,
    /// Whether open-ferry must be restarted to run it.
    pub restart_needed: bool,
    /// The supervisor version that decided `restart_needed` (a worker needed
    /// a newer supervisor than that one). Left out when unknown, and a
    /// merge never relabels it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_needed_by: Option<String>,
    /// The operation's id; empty for a switch that no supervisor made.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// See [`result`]; `None` for a switch that no supervisor made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Where it failed, see [`phase`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Why it failed, in words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// Who is at fault, see [`blame`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blame: Option<String>,
    /// Whether the switch ran under the supervisor without a restart. Left
    /// out when false, so a record the plain updater makes gains no field.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub blue_green: bool,
    /// Whether the new version's window ended cleanly. Left out when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub confirmed: bool,
    /// The version the supervisor went back to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned_to: Option<String>,
    /// The id of a rollback that ended this switch's window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// How long no worker accepted connections, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap_ms: Option<u64>,
    /// How long the old worker took to drain, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drained_ms: Option<u64>,
    /// How many connections were cut when the drain ran out of time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cut_connections: Option<u64>,
    /// When the new version's window ends, RFC 3339.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch_until: Option<String>,
    /// What became of the installed file, see [`installed_binary`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_binary: Option<String>,
}

impl SwitchRecord {
    /// The record written when the new version is ready (S1). A forward
    /// update is `switched` and a requested rollback is `rolled-back`,
    /// both with `confirmed: false` until the window ends.
    pub fn started(id: &str, how: &str, from: &str, to: &str, at: &str) -> Self {
        let result = if how == self::how::ROLLBACK {
            result::ROLLED_BACK
        } else {
            result::SWITCHED
        };
        Self {
            id: id.to_owned(),
            how: how.to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
            at: at.to_owned(),
            result: Some(result.to_owned()),
            blue_green: true,
            ..Self::default()
        }
    }

    /// The record of a switch whose new version never became ready.
    #[allow(clippy::too_many_arguments)]
    pub fn failed_to_load(
        id: &str,
        how: &str,
        from: &str,
        to: &str,
        at: &str,
        phase: &str,
        blame: &str,
        why: &str,
    ) -> Self {
        Self {
            result: Some(result::FAILED_TO_LOAD.to_owned()),
            phase: Some(phase.to_owned()),
            blame: Some(blame.to_owned()),
            why: Some(why.to_owned()),
            installed_binary: Some(installed_binary::UNCHANGED.to_owned()),
            ..Self::started(id, how, from, to, at)
        }
    }

    /// The record of a switch that failed inside its window, made from
    /// scratch for a failure found at a restart when no record was written.
    #[allow(clippy::too_many_arguments)]
    pub fn failed_after_switch(
        id: &str,
        how: &str,
        from: &str,
        to: &str,
        at: &str,
        phase: &str,
        blame: &str,
        why: &str,
        returned_to: &str,
    ) -> Self {
        let mut record = Self::started(id, how, from, to, at);
        record.fail_after_switch(phase, blame, why, returned_to);
        record
    }

    /// Ends the record as failed inside the window (the same `id`).
    pub fn fail_after_switch(&mut self, phase: &str, blame: &str, why: &str, returned_to: &str) {
        self.result = Some(result::FAILED_AFTER_SWITCH.to_owned());
        self.phase = Some(phase.to_owned());
        self.blame = Some(blame.to_owned());
        self.why = Some(why.to_owned());
        self.returned_to = Some(returned_to.to_owned());
        self.confirmed = false;
        self.installed_binary = Some(installed_binary::UNCHANGED.to_owned());
    }

    /// Marks the window as ended cleanly (S2). The installed file is still
    /// to be replaced.
    pub fn confirm(&mut self) {
        self.confirmed = true;
        self.installed_binary = Some(installed_binary::PENDING.to_owned());
    }

    /// Whether this is a partial record: one that leaves `from`, `to` and
    /// `at` empty, to be merged into a fuller one with the same `id`.
    pub fn is_partial(&self) -> bool {
        self.from.is_empty() && self.to.is_empty() && self.at.is_empty()
    }

    /// Notes the rollback (by id) that ended this switch's window.
    pub fn supersede(&mut self, by: &str) {
        self.superseded_by = Some(by.to_owned());
    }

    /// Merges `incoming`, a record with the same `id`, into this one. It
    /// may be partial: whatever it leaves empty or unset keeps its value
    /// here.
    ///
    /// - What S1 wrote stays: `from`, `to`, `at`, `how`, `binary`, `sha256`,
    ///   `size`, `modified_ms` and `restart_needed` are taken from `incoming`
    ///   only where they are still empty, zero or false here, and
    ///   `blue_green` only ever turns on.
    /// - A record with a `result` is an explicit one: its `result`, and its
    ///   `phase`, `why`, `blame` and `returned_to` where it has them, replace
    ///   these, and `confirmed` is taken as it says.
    /// - A record without one (a patch) leaves all that alone, except that
    ///   `confirmed` only ever turns on.
    /// - `superseded_by`, the measurements, `watch_until` and
    ///   `installed_binary` are replaced where `incoming` has them, except
    ///   that `pending` doesn't replace a later value, so replaying an old
    ///   record can't undo `replaced`.
    pub fn merge(&mut self, incoming: &Self) {
        fn fill(own: &mut String, other: &str) {
            if own.is_empty() {
                other.clone_into(own);
            }
        }
        fn replace<T: Clone>(own: &mut Option<T>, other: &Option<T>) {
            if other.is_some() {
                own.clone_from(other);
            }
        }
        fill(&mut self.from, &incoming.from);
        fill(&mut self.to, &incoming.to);
        fill(&mut self.at, &incoming.at);
        fill(&mut self.how, &incoming.how);
        fill(&mut self.binary, &incoming.binary);
        fill(&mut self.sha256, &incoming.sha256);
        fill(&mut self.id, &incoming.id);
        if self.size == 0 {
            self.size = incoming.size;
        }
        if self.modified_ms == 0 {
            self.modified_ms = incoming.modified_ms;
        }
        self.restart_needed |= incoming.restart_needed;
        // Whoever decided the requirement keeps it: a later record of the
        // same switch from another supervisor version never relabels it.
        if self.restart_needed_by.is_none() {
            self.restart_needed_by
                .clone_from(&incoming.restart_needed_by);
        }
        self.blue_green |= incoming.blue_green;

        if incoming.result.is_some() {
            replace(&mut self.result, &incoming.result);
            replace(&mut self.ended_at, &incoming.ended_at);
            replace(&mut self.phase, &incoming.phase);
            replace(&mut self.why, &incoming.why);
            replace(&mut self.blame, &incoming.blame);
            replace(&mut self.returned_to, &incoming.returned_to);
            self.confirmed = incoming.confirmed;
        } else {
            self.confirmed |= incoming.confirmed;
        }

        replace(&mut self.superseded_by, &incoming.superseded_by);
        replace(&mut self.gap_ms, &incoming.gap_ms);
        replace(&mut self.drained_ms, &incoming.drained_ms);
        replace(&mut self.cut_connections, &incoming.cut_connections);
        replace(&mut self.watch_until, &incoming.watch_until);
        let stale_pending = incoming.installed_binary.as_deref() == Some(installed_binary::PENDING)
            && self
                .installed_binary
                .as_deref()
                .is_some_and(|own| own != installed_binary::PENDING);
        if !stale_pending {
            replace(&mut self.installed_binary, &incoming.installed_binary);
        }
    }
}

/// The operation order an id carries: a supervisor's ids are
/// `sw-<12 hex digits: its start in UNIX ms><4 random hex digits>-<counter>`,
/// the run part drawn once per Core and the counter growing with each
/// operation of that Core. Returns (start ms, suffix, counter), or `None`
/// for any other id (including the older form with 12 random digits).
fn operation_order(id: &str) -> Option<(u64, &str, u64)> {
    let rest = id.strip_prefix("sw-")?;
    let (run, counter) = rest.rsplit_once('-')?;
    if run.len() != 16 || !run.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let (start, suffix) = run.split_at(12);
    Some((
        u64::from_str_radix(start, 16).ok()?,
        suffix,
        counter.parse().ok()?,
    ))
}

/// Whether `other` is newer than `new`: a later `at`, or the same second
/// and a later operation. Of two ids in the current form, the one from the
/// Core that started later is ahead; the same Core's counter orders its own;
/// two Cores that started in the same millisecond are not ordered. This does
/// not depend on which record was delivered first. A wall clock that steps
/// backwards across a restart can misorder records of two runs.
fn is_ahead(other: &SwitchRecord, new: &SwitchRecord) -> bool {
    if other.at != new.at {
        return other.at > new.at;
    }
    match (operation_order(&other.id), operation_order(&new.id)) {
        (Some((start_a, suffix_a, a)), Some((start_b, suffix_b, b))) => {
            if start_a != start_b {
                start_a > start_b
            } else {
                suffix_a == suffix_b && a > b
            }
        }
        _ => false,
    }
}

impl State {
    /// Puts `record` in `history` and, when it is the newest, in
    /// `last_switch`. A record with this `id` already there (or in
    /// `last_switch`) is merged into it in place, see
    /// [`SwitchRecord::merge`], so the same upsert twice (a replayed
    /// outcome) changes nothing, and a partial record (a terminal result
    /// made at recovery, or an `installed_binary` patch) keeps what the
    /// fuller one recorded. A record with no `id` is always new.
    pub fn upsert_record(&mut self, record: SwitchRecord) {
        let id = record.id.clone();
        let same = |other: &SwitchRecord| !id.is_empty() && other.id == id;
        let slot = self.history.iter().position(same);
        let base = slot
            .and_then(|slot| self.history.get(slot))
            .or_else(|| self.last_switch.as_ref().filter(|last| same(last)));
        let merged = match base {
            Some(base) => {
                let mut merged = base.clone();
                merged.merge(&record);
                merged
            }
            None if record.is_partial() => {
                // A patch or terminal result for a record this state never
                // had (or one with no id, which can't be matched): it would
                // become a junk record with no from, to or at, and possibly
                // `last_switch`. Drop it.
                tracing::warn!(
                    id = %id,
                    "dropping a partial switch record whose id is not in the state"
                );
                return;
            }
            None => record,
        };
        if let Some(slot) = slot.and_then(|slot| self.history.get_mut(slot)) {
            *slot = merged.clone();
        } else {
            // Newest first, by time. A tie in the same second goes by the
            // operation order the ids carry (see `operation_order`), so a
            // late retry of an older operation's record can't pass a newer
            // one; where the ids say nothing, the new one goes before.
            let at = self
                .history
                .iter()
                .position(|other| !is_ahead(other, &merged))
                .unwrap_or(self.history.len());
            self.history.insert(at, merged.clone());
            self.history.truncate(MAX_HISTORY);
        }
        let newest = self
            .history
            .first()
            .is_some_and(|first| first.id == merged.id && first.at == merged.at);
        if newest || self.last_switch.as_ref().is_some_and(same) {
            self.last_switch = Some(merged);
        }
    }

    /// Gives the state an outcome the journal carried: upserts its record
    /// and marks the version failed, if the outcome says so. Idempotent.
    pub fn apply_outcome(&mut self, record: &SwitchRecord, mark_failed: Option<&str>) {
        if record.how == how::UPDATE
            && record.result.as_deref() == Some(result::SWITCHED)
            && self.rolled_back.as_deref() == Some(record.to.as_str())
        {
            self.rolled_back = None;
        }
        if record.how == how::ROLLBACK
            && record.result.as_deref() == Some(result::ROLLED_BACK)
            && !record.from.is_empty()
        {
            self.rolled_back = Some(record.from.clone());
        }
        self.upsert_record(record.clone());
        if let Some(version) = mark_failed {
            self.mark_failed(version);
        }
    }

    /// Writes the state like [`State::save`] for the write called `write`
    /// (`S1`, `S2` or `S3`), which a test build can make fail or hold at
    /// `state.fail:<write>`.
    pub fn save_named(&self, path: &Path, write: &str) -> io::Result<()> {
        #[cfg(feature = "blue-green-test")]
        crate::test_seams::hit(
            path.parent().unwrap_or_else(|| Path::new(".")),
            &format!("state.fail:{write}"),
        )?;
        #[cfg(not(feature = "blue-green-test"))]
        let _ = write;
        self.save(path)
    }

    /// [`State::save_named`] for a state that records a switch which has
    /// already committed. When the file has been moved into place and only
    /// the directory sync after it failed, the state on disk holds the switch,
    /// so that is a warning and `Ok`; every other failure is still an error.
    /// Ordinary saves keep failing on a landed-but-not-durable write.
    pub fn save_committed_named(&self, path: &Path, write: &str) -> io::Result<()> {
        #[cfg(feature = "blue-green-test")]
        crate::test_seams::hit(
            path.parent().unwrap_or_else(|| Path::new(".")),
            &format!("state.fail:{write}"),
        )?;
        #[cfg(not(feature = "blue-green-test"))]
        let _ = write;
        let data = self.encode()?;
        match crate::journal::atomic_write_landed(path, &data, true) {
            Ok(()) => Ok(()),
            Err(failure) if failure.landed => {
                tracing::warn!(
                    "the update state {} was written but syncing its directory failed, so it \
                     may not survive a power loss: {}",
                    path.display(),
                    failure.error
                );
                Ok(())
            }
            Err(failure) => Err(failure.error),
        }
    }

    fn encode(&self) -> io::Result<Vec<u8>> {
        let mut state = self.clone();
        state.format = FORMAT;
        let mut data = serde_json::to_vec_pretty(&state).map_err(io::Error::other)?;
        data.push(b'\n');
        Ok(data)
    }
}

impl State {
    /// The state in `path`; empty when there is none or it doesn't parse.
    pub fn load(path: &Path) -> Self {
        match fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|error| {
                tracing::warn!(
                    "update state {} doesn't parse, so it starts over: {error}",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// The state in `path`, for a caller that must not act on a state it
    /// could not read (the supervisor's keep set): a missing file is the empty
    /// state, and any other read error or a parse error is an error. Additive;
    /// [`State::load`] and its callers are unchanged.
    pub fn load_strict(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the update state {} doesn't parse: {error}", path.display()),
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Writes the state to `path`, whole.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        write_atomically(path, &self.encode()?)
    }

    /// Whether `version` failed its check and is skipped.
    pub fn has_failed(&self, version: &str) -> bool {
        self.failed.iter().any(|failed| failed == version)
    }

    /// Remembers that `version` failed its check.
    pub fn mark_failed(&mut self, version: &str) {
        if !self.has_failed(version) {
            self.failed.push(version.to_owned());
        }
        let extra = self.failed.len().saturating_sub(MAX_FAILED);
        self.failed.drain(..extra);
    }
}

/// Writes `data` to `path` through a temporary file beside it: fsynced,
/// moved over the old file, and the directory made durable, so a caller that
/// then clears a journal can rely on the write having happened.
pub fn write_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    crate::journal::atomic_write(path, data, true)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn record(id: &str, at: &str) -> SwitchRecord {
        SwitchRecord::started(id, how::UPDATE, "0.1.0", "0.2.0", at)
    }

    // Not upstream's: the record the design shows, as the dashboard reads it.
    #[test]
    fn a_failed_switch_record_is_golden_json() {
        let mut record = SwitchRecord::failed_after_switch(
            "7c1f",
            how::UPDATE,
            "0.2.0",
            "0.3.0",
            "2026-10-09T10:00:00Z",
            phase::WATCHING,
            blame::NEW_VERSION,
            "the new version exited with code 101 four minutes after the switch",
            "0.2.0",
        );
        record.gap_ms = Some(143);
        assert_eq!(
            serde_json::to_value(&record).unwrap(),
            json!({
                "from": "0.2.0", "to": "0.3.0", "at": "2026-10-09T10:00:00Z",
                "how": "update", "binary": "", "sha256": "", "size": 0,
                "modified_ms": 0, "restart_needed": false,
                "id": "7c1f", "result": "failed-after-switch",
                "phase": "watching", "blame": "new-version",
                "why": "the new version exited with code 101 four minutes after the switch",
                "blue_green": true, "returned_to": "0.2.0",
                "gap_ms": 143, "installed_binary": "unchanged"
            })
        );
    }

    // Not upstream's: Opus 12. A record the plain updater makes (no
    // supervisor) serializes with neither new flag, and a stored record
    // with them true round-trips.
    #[test]
    fn a_legacy_record_serializes_without_the_flags() {
        let legacy = SwitchRecord {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            at: "2026-10-09T10:00:00Z".into(),
            how: "update".into(),
            ..SwitchRecord::default()
        };
        let value = serde_json::to_value(&legacy).unwrap();
        assert!(value.get("blue_green").is_none());
        assert!(value.get("confirmed").is_none());
        let back: SwitchRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, legacy);

        let mut confirmed = SwitchRecord::started("a", how::UPDATE, "1", "2", "t");
        confirmed.confirm();
        let value = serde_json::to_value(&confirmed).unwrap();
        assert_eq!(value["blue_green"], json!(true));
        assert_eq!(value["confirmed"], json!(true));
        assert_eq!(
            serde_json::from_value::<SwitchRecord>(value).unwrap(),
            confirmed
        );
    }

    // Not upstream's: Opus 10. A partial record whose id is unknown is
    // dropped and never becomes `last_switch`; a known one merges as before;
    // a full one with a new id is inserted.
    #[test]
    fn a_partial_record_with_an_unknown_id_is_dropped() {
        let partial = SwitchRecord::failed_after_switch(
            "ghost",
            how::UPDATE,
            "",
            "",
            "",
            phase::WATCHING,
            blame::NEW_VERSION,
            "crashed",
            "0.1.0",
        );
        assert!(partial.is_partial());

        // Nothing there: nothing added.
        let mut state = State::default();
        state.upsert_record(partial.clone());
        assert_eq!(state, State::default());

        // With another record as `last_switch`, it stays that.
        let mut state = State::default();
        state.upsert_record(full("known"));
        let before = state.clone();
        state.upsert_record(partial.clone());
        assert_eq!(state, before);
        assert_eq!(
            state.last_switch.as_ref().map(|r| r.id.as_str()),
            Some("known")
        );

        // A partial record with no id can't be matched: dropped too.
        let idless = SwitchRecord {
            result: Some(result::FAILED_AFTER_SWITCH.into()),
            ..SwitchRecord::default()
        };
        assert!(idless.is_partial() && idless.id.is_empty());
        state.upsert_record(idless.clone());
        assert_eq!(state, before);
        let mut empty = State::default();
        empty.upsert_record(idless);
        assert_eq!(empty, State::default());

        // A known id still merges.
        let mut known = partial;
        known.id = "known".into();
        state.upsert_record(known);
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].sha256, "ab".repeat(32));
        assert_eq!(
            state.history[0].result.as_deref(),
            Some(result::FAILED_AFTER_SWITCH)
        );

        // A full record with a new id is inserted.
        state.upsert_record(record("fresh", "2026-10-09T12:00:00Z"));
        assert_eq!(state.history.len(), 2);
        assert_eq!(
            state.last_switch.as_ref().map(|r| r.id.as_str()),
            Some("fresh")
        );
    }

    // Not upstream's: M4, a requested rollback's S1 is `rolled-back`.
    #[test]
    fn s1_of_a_rollback_is_rolled_back_and_of_an_update_is_switched() {
        let update = SwitchRecord::started("a", how::UPDATE, "1", "2", "t");
        assert_eq!(update.result.as_deref(), Some(result::SWITCHED));
        assert!(!update.confirmed && update.blue_green);
        let rollback = SwitchRecord::started("b", how::ROLLBACK, "2", "1", "t");
        assert_eq!(rollback.result.as_deref(), Some(result::ROLLED_BACK));
        assert!(!rollback.confirmed);
        let mut confirmed = rollback.clone();
        confirmed.confirm();
        assert!(confirmed.confirmed);
        assert_eq!(confirmed.result, rollback.result);
        assert_eq!(
            confirmed.installed_binary.as_deref(),
            Some(installed_binary::PENDING)
        );
        let mut state = State::default();
        state.apply_outcome(&update, None);
        assert_eq!(state.rolled_back, None);
        state.apply_outcome(&rollback, None);
        assert_eq!(state.rolled_back.as_deref(), Some("2"));
        state.apply_outcome(
            &SwitchRecord {
                result: Some(result::FAILED_TO_LOAD.into()),
                from: "3".into(),
                ..rollback
            },
            None,
        );
        assert_eq!(state.rolled_back.as_deref(), Some("2"));
    }

    #[test]
    fn reaching_the_rolled_back_version_clears_its_exclusion_as_in_the_plain_form() {
        for version in ["0.3.0", "0.4.0"] {
            let mut state = State {
                rolled_back: Some("0.3.0".into()),
                ..Default::default()
            };
            state.apply_outcome(
                &SwitchRecord::started("manual", how::UPDATE, "0.2.0", version, "now"),
                None,
            );
            // A newer version leaves the old marker, which excludes only
            // that exact release. The plain switch uses the same rule.
            assert_eq!(
                state.rolled_back.as_deref(),
                (version != "0.3.0").then_some("0.3.0")
            );
        }
    }

    // Not upstream's: the failure constructors carry phase, blame and why.
    #[test]
    fn a_failure_to_load_never_confirms_and_leaves_the_file_unchanged() {
        let record = SwitchRecord::failed_to_load(
            "c",
            how::ROLLBACK,
            "2",
            "1",
            "t",
            phase::LOADING,
            blame::ENVIRONMENT,
            "could not write the journal",
        );
        assert_eq!(record.result.as_deref(), Some(result::FAILED_TO_LOAD));
        assert_eq!(record.how, how::ROLLBACK);
        assert_eq!(record.phase.as_deref(), Some("loading"));
        assert_eq!(record.blame.as_deref(), Some("environment"));
        assert!(!record.confirmed);
        assert_eq!(record.installed_binary.as_deref(), Some("unchanged"));
        assert_eq!(installed_binary::failed("busy"), "failed: busy");
    }

    #[test]
    fn an_outcome_time_survives_state_upserts_and_later_patches() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        let mut state = State::default();
        let mut record = SwitchRecord::started("one", "update", "1", "2", "2026-10-19T00:00:00Z");
        state.upsert_record(record.clone());
        record.result = Some(result::FAILED_AFTER_SWITCH.into());
        record.ended_at = Some("2026-10-19T00:10:00Z".into());
        state.upsert_record(record.clone());
        state.upsert_record(SwitchRecord {
            id: "one".into(),
            ..Default::default()
        });
        state.save(&path).unwrap();
        let loaded = State::load_strict(&path).unwrap();
        assert_eq!(loaded.last_switch, Some(record.clone()));
        assert_eq!(loaded.history, [record]);
    }

    // Not upstream's: a record updated in place keeps its place and its id.
    #[test]
    fn a_record_is_updated_in_place_and_an_upsert_is_idempotent() {
        let mut state = State::default();
        let mut one = record("one", "2026-10-09T10:00:00Z");
        state.upsert_record(one.clone());
        one.confirm();
        state.upsert_record(one.clone());
        state.upsert_record(one.clone());
        assert_eq!(state.history, [one.clone()]);
        assert_eq!(state.last_switch.as_ref(), Some(&one));

        let two = record("two", "2026-10-09T11:00:00Z");
        state.upsert_record(two.clone());
        let mut old = one.clone();
        old.supersede("two");
        state.upsert_record(old.clone());
        let ids: Vec<_> = state.history.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["two", "one"]);
        assert_eq!(state.history[1].superseded_by.as_deref(), Some("two"));
        // The older record doesn't take `last_switch` from the newer.
        assert_eq!(state.last_switch.as_ref(), Some(&two));
    }

    // Not upstream's: a late retry of an older operation's record, in the
    // same second as a newer one, does not become `last_switch`.
    #[test]
    fn a_late_record_of_an_older_operation_in_the_same_second_is_not_last_switch() {
        let at = "2026-10-09T10:00:00Z";
        let (one, two) = ("sw-0000000000aabbcc-1", "sw-0000000000aabbcc-2");
        let mut state = State::default();
        state.upsert_record(record(two, at));
        state.upsert_record(record(one, at));
        let ids: Vec<_> = state.history.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, [two, one]);
        assert_eq!(state.last_switch.as_ref().map(|r| r.id.as_str()), Some(two));
        // The same, with the newer one arriving after.
        let mut other = State::default();
        other.upsert_record(record(one, at));
        other.upsert_record(record(two, at));
        assert_eq!(other.last_switch.as_ref().map(|r| r.id.as_str()), Some(two));
    }

    // The older id form (12 random hex digits) and any other id keep the old
    // rule: the new record goes before.
    #[test]
    fn an_id_of_the_old_form_against_one_of_the_new_form_keeps_the_old_rule() {
        let at = "2026-10-09T10:00:00Z";
        for (first, second) in [
            ("sw-0123456789ab-9", "sw-0000000000aabbcc-1"),
            ("sw-0000000000aabbcc-9", "sw-0123456789ab-1"),
            ("sw-abc-2", "sw-abc-1"),
        ] {
            let mut state = State::default();
            state.upsert_record(record(first, at));
            state.upsert_record(record(second, at));
            let ids: Vec<_> = state.history.iter().map(|r| r.id.as_str()).collect();
            assert_eq!(ids, [second, first], "the new record goes before");
        }
    }

    // Not upstream's: the last 5, newest first.
    #[test]
    fn history_keeps_the_last_five_newest_first() {
        let mut state = State::default();
        for n in 0..8 {
            state.upsert_record(record(&format!("r{n}"), &format!("2026-10-09T10:0{n}:00Z")));
        }
        let ids: Vec<_> = state.history.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["r7", "r6", "r5", "r4", "r3"]);
        assert_eq!(
            state.last_switch.as_ref().map(|r| r.id.as_str()),
            Some("r7")
        );
        // Replaying one that fell off the end adds nothing.
        state.upsert_record(record("r0", "2026-10-09T10:00:00Z"));
        assert_eq!(state.history.len(), MAX_HISTORY);
        assert!(state.history.iter().all(|r| r.id != "r0"));
        // Records with no id are all new.
        for _ in 0..2 {
            // Full legacy records (from, to and at set), as the plain
            // updater writes them; an empty one would be dropped.
            state.upsert_record(SwitchRecord {
                from: "0.1.0".into(),
                to: "0.2.0".into(),
                at: "2026-10-09T12:00:00Z".into(),
                ..SwitchRecord::default()
            });
        }
        assert_eq!(state.history.iter().filter(|r| r.id.is_empty()).count(), 2);
    }

    /// A record S1 and the later steps wrote, with its metadata.
    fn full(id: &str) -> SwitchRecord {
        let mut full = record(id, "2026-10-09T10:00:00Z");
        full.binary = "/bin/open-ferry".into();
        full.sha256 = "ab".repeat(32);
        full.size = 1234;
        full.modified_ms = 99;
        full.restart_needed = true;
        full.gap_ms = Some(143);
        full.drained_ms = Some(2000);
        full.cut_connections = Some(1);
        full.watch_until = Some("2026-10-09T10:05:00Z".into());
        full
    }

    // Not upstream's: a terminal record made from scratch at recovery
    // (nothing but the id, times and verdict) keeps the metadata the fuller
    // record had, and replaying it changes nothing.
    #[test]
    fn a_partial_terminal_record_keeps_the_metadata_of_the_full_one() {
        let mut state = State::default();
        let before = full("m");
        state.upsert_record(before.clone());
        let partial = SwitchRecord::failed_after_switch(
            "m",
            how::UPDATE,
            "0.1.0",
            "0.2.0",
            "2030-01-01T00:00:00Z",
            phase::WATCHING,
            blame::NEW_VERSION,
            "crashed",
            "0.1.0",
        );
        state.upsert_record(partial.clone());
        let merged = state.history[0].clone();
        assert_eq!(state.history.len(), 1);
        // S1 stays, even `at`.
        assert_eq!(merged.at, before.at);
        assert_eq!(merged.binary, before.binary);
        assert_eq!(merged.sha256, before.sha256);
        assert_eq!((merged.size, merged.modified_ms), (1234, 99));
        assert!(merged.restart_needed && merged.blue_green);
        // The measurements stay.
        assert_eq!(merged.gap_ms, Some(143));
        assert_eq!(merged.drained_ms, Some(2000));
        assert_eq!(merged.cut_connections, Some(1));
        assert_eq!(merged.watch_until, before.watch_until);
        // The verdict is the new one.
        assert_eq!(merged.result.as_deref(), Some(result::FAILED_AFTER_SWITCH));
        assert_eq!(merged.phase.as_deref(), Some("watching"));
        assert_eq!(merged.blame.as_deref(), Some("new-version"));
        assert_eq!(merged.why.as_deref(), Some("crashed"));
        assert_eq!(merged.returned_to.as_deref(), Some("0.1.0"));
        assert!(!merged.confirmed);
        assert_eq!(merged.installed_binary.as_deref(), Some("unchanged"));
        assert_eq!(state.last_switch.as_ref(), Some(&merged));

        let once = state.clone();
        state.upsert_record(partial);
        assert_eq!(state, once);
    }

    // Not upstream's: a patch with only an id and `installed_binary` changes
    // that and nothing else, and an old `pending` can't undo it.
    #[test]
    fn an_installed_binary_patch_changes_only_that() {
        let mut state = State::default();
        let mut before = full("p");
        before.confirm();
        state.upsert_record(before.clone());
        let patch = SwitchRecord {
            id: "p".into(),
            installed_binary: Some(installed_binary::REPLACED.into()),
            ..SwitchRecord::default()
        };
        state.upsert_record(patch.clone());
        let expected = SwitchRecord {
            installed_binary: Some("replaced".into()),
            ..before.clone()
        };
        assert_eq!(state.history.as_slice(), std::slice::from_ref(&expected));
        assert_eq!(state.last_switch.as_ref(), Some(&expected));
        assert!(expected.confirmed && expected.blue_green);

        let once = state.clone();
        state.upsert_record(patch);
        assert_eq!(state, once);
        // Replaying S2 (`pending`) afterwards doesn't undo `replaced`.
        state.upsert_record(before);
        assert_eq!(state, once);
    }

    // Not upstream's: a patch for a record not in `history` merges into
    // `last_switch`, and with neither it is just added.
    #[test]
    fn a_patch_merges_into_last_switch_when_history_lacks_the_record() {
        let mut state = State {
            last_switch: Some(full("q")),
            ..State::default()
        };
        state.upsert_record(SwitchRecord {
            id: "q".into(),
            installed_binary: Some(installed_binary::CHANGED_ELSEWHERE.into()),
            ..SwitchRecord::default()
        });
        let last = state.last_switch.clone().unwrap();
        assert_eq!(last.sha256, "ab".repeat(32));
        assert_eq!(last.installed_binary.as_deref(), Some("changed-elsewhere"));
        assert_eq!(state.history, [last]);
    }

    // Not upstream's: the state file is written through the journal's
    // fsyncing atomic write and replaces what was there.
    #[test]
    fn the_state_is_written_atomically_over_the_old_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("update-state.json");
        write_atomically(&path, b"one").unwrap();
        write_atomically(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            1,
            "no temp left"
        );
    }

    // Not upstream's: a replayed outcome changes the state once.
    #[test]
    fn an_outcome_is_applied_once_however_often_it_is_replayed() {
        let mut state = State::default();
        let mut failed = record("x", "2026-10-09T10:00:00Z");
        failed.fail_after_switch(phase::WATCHING, blame::NEW_VERSION, "crashed", "0.1.0");
        state.apply_outcome(&failed, Some("0.2.0"));
        let once = state.clone();
        state.apply_outcome(&failed, Some("0.2.0"));
        assert_eq!(state, once);
        assert_eq!(state.failed, ["0.2.0"]);
        state.apply_outcome(&failed, None);
        assert_eq!(state, once);
    }

    // Not upstream's: a state with none of the new fields has none in its
    // JSON, and one from before them reads.
    #[test]
    fn the_new_fields_are_absent_until_used_and_old_files_read() {
        let json = serde_json::to_value(State::default()).unwrap();
        for key in ["history", "supervisor_version", "needs_restart"] {
            assert!(json.get(key).is_none(), "{key}");
        }
        let old = r#"{"format":1,"staged":"0.2.0","last_switch":{"from":"a","to":"b","at":"t",
            "how":"update","binary":"x","sha256":"s","size":1,"modified_ms":2,"restart_needed":true}}"#;
        let state: State = serde_json::from_str(old).unwrap();
        let switch = state.last_switch.unwrap();
        assert!(switch.restart_needed && switch.id.is_empty() && switch.result.is_none());
        assert!(!switch.confirmed && !switch.blue_green && state.history.is_empty());
        let json = serde_json::to_value(&switch).unwrap();
        assert!(json.get("result").is_none() && json.get("id").is_none());
    }

    // Not upstream's: the supervisor's fields round trip through the file.
    #[test]
    fn history_and_the_supervisor_fields_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("update-state.json");
        let mut state = State {
            supervisor_version: Some("0.2.0".into()),
            needs_restart: true,
            ..State::default()
        };
        state.upsert_record(record("one", "2026-10-09T10:00:00Z"));
        state.save_named(&path, "S1").unwrap();
        let loaded = State::load(&path);
        assert_eq!(loaded.history, state.history);
        assert_eq!(loaded.supervisor_version.as_deref(), Some("0.2.0"));
        assert!(loaded.needs_restart);
    }

    // Not upstream's: U2-P4. A landed rename whose directory sync fails is a
    // success for a state recording a committed switch, an error otherwise.
    #[cfg(feature = "blue-green-test")]
    #[test]
    fn a_landed_state_save_is_success_only_when_committed() {
        let temp = tempfile::tempdir().unwrap();
        let seams = temp.path().join(".blue-green-test");
        fs::create_dir_all(&seams).unwrap();
        fs::write(seams.join("atomic.landed_update-state.json.fail"), b"").unwrap();
        let path = temp.path().join("update-state.json");
        let mut state = State::default();
        state.upsert_record(record("one", "2026-10-09T10:00:00Z"));

        assert!(state.save_named(&path, "S1").is_err());
        assert!(state.save(&path).is_err());
        state.save_committed_named(&path, "S1").unwrap();
        assert_eq!(State::load(&path).history, state.history);
    }
}
