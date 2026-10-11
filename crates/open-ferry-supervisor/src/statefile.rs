//! Not upstream's: the supervisor's writes to the reporting state file
//! (`update-state.json`). The state is for reporting and history; the journal
//! wins when they disagree. A write is: lock, load, change, save, unlock
//! (v3 3.7). At startup the supervisor already holds the update lock, so
//! [`apply_locked`] does the same without taking it.

use std::io;

use open_ferry_update::data_dir::DataDir;
use open_ferry_update::journal::Outcome;
use open_ferry_update::state::State;

#[derive(Debug)]
pub enum StateWriteError {
    /// Another process holds the update lock. Never waited for here: the
    /// caller retries on its timer (T3: nothing blocks on a lock).
    Busy,
    Io(io::Error),
}

impl std::fmt::Display for StateWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "the update lock is held by another process"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StateWriteError {}

/// A partial outcome whose original record is unavailable. Startup logs this
/// once; retry callers can use the same predicate to avoid repeated warnings.
/// It is never acknowledged or removed until the original record is available.
pub(crate) fn orphaned_partial(data: &DataDir, outcome: &Outcome) -> bool {
    if !outcome.record.is_partial() {
        return false;
    }
    let state = match std::fs::read(data.state_file()) {
        Ok(bytes) => match serde_json::from_slice::<State>(&bytes) {
            Ok(state) => state,
            Err(_) => return false,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => State::default(),
        Err(_) => return false,
    };
    orphaned_in_state(&state, outcome)
}

fn orphaned_in_state(state: &State, outcome: &Outcome) -> bool {
    outcome.record.is_partial()
        && !state
            .history
            .iter()
            .chain(state.last_switch.iter())
            .any(|record| !outcome.record.id.is_empty() && record.id == outcome.record.id)
}

/// Gives the state `outcome` (its record by id, and `mark_failed`), as the
/// write called `write` (`S1`, `S2`, `S3`), when the update lock is already
/// held. A restart the record asks for is noted against this crate's version;
/// use [`apply_locked_by`] to name the running supervisor's own.
pub fn apply_locked(data: &DataDir, write: &str, outcome: &Outcome) -> io::Result<()> {
    apply_locked_by(data, write, outcome, open_ferry_update::CURRENT_VERSION)
}

/// [`apply_locked`] for the supervisor whose version is `supervisor`. Every
/// outcome save sets the state's `supervisor_version` (the version of the
/// supervisor that wrote the last record). A record that says a restart is
/// needed (a worker needed a newer supervisor, row b) and names `supervisor`
/// as the one that decided it also sets `needs_restart` in the same write, so a
/// replay after a crash restores it. Any other outcome keeps `needs_restart`
/// only if it was set while `supervisor` ran; otherwise it is cleared, since
/// stamping the version would make another version's flag look like this one's.
pub fn apply_locked_by(
    data: &DataDir,
    write: &str,
    outcome: &Outcome,
    supervisor: &str,
) -> io::Result<()> {
    let path = data.state_file();
    // Strict: a state that can't be read or parsed fails the write instead of
    // turning into defaults, which would erase `staged`, `previous`, `history`
    // and `failed`. A missing file is still an empty state.
    let mut state = State::load_strict(&path)?;
    if orphaned_in_state(&state, outcome) {
        return Err(io::Error::other(
            "the state has no record for this partial outcome",
        ));
    }
    state.apply_outcome(
        &outcome.record,
        outcome
            .mark_failed
            .as_ref()
            .map(|failed| failed.version.as_str()),
    );
    // The flag is `supervisor`'s: it stays if it was set while this same
    // version ran, or it is set if this version decided the record's
    // requirement. A flag another version left is dropped before the stamp
    // below would hand it to this one, and a requirement with no recorded
    // version never sets it on replay (a deliberate compatibility choice:
    // such a record predates the field, and guessing its owner could make a
    // restart that already came look still owed).
    let own_requirement = outcome.record.restart_needed
        && outcome.record.restart_needed_by.as_deref() == Some(supervisor);
    let kept = state.needs_restart && state.supervisor_version.as_deref() == Some(supervisor);
    state.needs_restart = kept || own_requirement;
    state.supervisor_version = Some(supervisor.to_owned());
    state.save_named(&path, write)
}

/// The start-up rule for `needs_restart`, after recovery. The flag means the
/// service must be restarted for a newer supervisor; it holds only while the
/// supervisor version that set it runs. `true` when the state's flag is set and
/// its `supervisor_version` is `supervisor`: it is loaded into the runtime's
/// snapshot. When the version differs (the restart happened) the flag is
/// cleared in the file, in one write; if that write fails, it is logged, the
/// answer is `false`, and the next start tries again.
pub fn restart_flag_at_start(data: &DataDir, supervisor: &str) -> bool {
    restart_flag(data, supervisor, true)
}

/// `restart_flag_at_start` for a start whose switching was turned off: nothing
/// is written, whatever the file holds (the switch-off lets nothing reach the
/// disk). An older version's flag answers `false` and stays set in the file;
/// the next start clears it.
pub fn restart_flag_read_only(data: &DataDir, supervisor: &str) -> bool {
    restart_flag(data, supervisor, false)
}

fn restart_flag(data: &DataDir, supervisor: &str, clear: bool) -> bool {
    let path = data.state_file();
    let state = match State::load_strict(&path) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!("the state file can't be read for needs_restart: {error}");
            return false;
        }
    };
    if !state.needs_restart {
        return false;
    }
    if state.supervisor_version.as_deref() == Some(supervisor) {
        return true;
    }
    if !clear {
        tracing::warn!("needs_restart is not cleared: switching is off, the next start clears it");
        return false;
    }
    let cleared = (|| -> Result<(), StateWriteError> {
        let _lock = data
            .try_lock()
            .map_err(StateWriteError::Io)?
            .ok_or(StateWriteError::Busy)?;
        change_locked(data, "S4", |state| {
            if state.supervisor_version.as_deref() != Some(supervisor) {
                state.needs_restart = false;
            }
        })
        .map_err(StateWriteError::Io)
    })();
    if let Err(error) = cleared {
        tracing::warn!(
            "needs_restart could not be cleared, it is retried at the next start: {error}"
        );
    }
    false
}

/// Loads the state, changes it and saves it as `write`, the update lock being
/// held by the caller.
pub fn change_locked(
    data: &DataDir,
    write: &str,
    change: impl FnOnce(&mut State),
) -> io::Result<()> {
    let path = data.state_file();
    let mut state = State::load_strict(&path)?;
    change(&mut state);
    state.save_named(&path, write)
}

/// [`apply_locked`] under the update lock, taken without waiting.
pub fn apply(data: &DataDir, write: &str, outcome: &Outcome) -> Result<(), StateWriteError> {
    apply_by(data, write, outcome, open_ferry_update::CURRENT_VERSION)
}

/// [`apply_locked_by`] under the update lock, taken without waiting.
pub fn apply_by(
    data: &DataDir,
    write: &str,
    outcome: &Outcome,
    supervisor: &str,
) -> Result<(), StateWriteError> {
    let _lock = data
        .try_lock()
        .map_err(StateWriteError::Io)?
        .ok_or(StateWriteError::Busy)?;
    apply_locked_by(data, write, outcome, supervisor).map_err(StateWriteError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orphaned_partial_only_identifies_missing_original_records() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        let outcome = Outcome {
            record: open_ferry_update::state::SwitchRecord {
                id: "original".into(),
                installed_binary: Some("replaced".into()),
                ..Default::default()
            },
            mark_failed: None,
        };
        assert!(orphaned_partial(&data, &outcome));
        assert!(!data.state_file().exists(), "predicate must not write");
        let mut state = State::default();
        state.upsert_record(open_ferry_update::state::SwitchRecord {
            id: "original".into(),
            from: "1.0.0".into(),
            to: "2.0.0".into(),
            ..Default::default()
        });
        state.save_named(&data.state_file(), "test").unwrap();
        let before = std::fs::read(data.state_file()).unwrap();
        assert!(!orphaned_partial(&data, &outcome));
        assert_eq!(std::fs::read(data.state_file()).unwrap(), before);
        let full = Outcome {
            record: open_ferry_update::state::SwitchRecord {
                id: "new".into(),
                from: "1.0.0".into(),
                to: "2.0.0".into(),
                ..Default::default()
            },
            mark_failed: None,
        };
        assert!(!orphaned_partial(&data, &full));
    }

    fn full_outcome() -> Outcome {
        Outcome {
            record: open_ferry_update::state::SwitchRecord {
                id: "new".into(),
                from: "1.0.0".into(),
                to: "2.0.0".into(),
                ..Default::default()
            },
            mark_failed: None,
        }
    }

    /// Every supervisor state write refuses a state it can't read, leaving the
    /// file as it was, so state-only protections are not replaced by defaults.
    fn assert_every_write_refuses(data: &DataDir, before: &[u8]) {
        assert!(apply_locked(data, "S2", &full_outcome()).is_err());
        assert!(change_locked(data, "S3", |state| state.staged = None).is_err());
        assert!(matches!(
            apply(data, "S2", &full_outcome()),
            Err(StateWriteError::Io(_))
        ));
        assert_eq!(std::fs::read(data.state_file()).unwrap(), before);
    }

    #[test]
    fn a_state_that_does_not_parse_is_not_overwritten_by_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        std::fs::write(data.state_file(), b"\x00\xffnot json").unwrap();
        assert_every_write_refuses(&data, b"\x00\xffnot json");
    }

    // The save contract: a record that landed in the file but whose directory
    // sync failed is still an error here (`save_committed_named` would call it
    // success), so the caller keeps the outcome owed.
    #[cfg(feature = "blue-green-test")]
    #[test]
    fn a_state_write_that_landed_but_could_not_sync_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        let seams = dir.path().join(".blue-green-test");
        std::fs::create_dir_all(&seams).unwrap();
        std::fs::write(seams.join("atomic.landed_update-state.json.fail"), b"").unwrap();
        assert!(apply_locked(&data, "S2", &full_outcome()).is_err());
        assert!(matches!(
            apply(&data, "S2", &full_outcome()),
            Err(StateWriteError::Io(_))
        ));
        assert!(change_locked(&data, "S3", |state| state.staged = None).is_err());
        let landed = State::load_strict(&data.state_file()).unwrap();
        assert_eq!(landed.history.len(), 1, "it did land: {landed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_state_that_cannot_be_read_is_not_overwritten_by_a_write() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads it anyway
        }
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        let state = State {
            staged: Some("2.0.0".into()),
            ..State::default()
        };
        state.save_named(&data.state_file(), "test").unwrap();
        let before = std::fs::read(data.state_file()).unwrap();
        std::fs::set_permissions(data.state_file(), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let refused = apply_locked(&data, "S2", &full_outcome()).is_err()
            && change_locked(&data, "S3", |state| state.staged = None).is_err();
        std::fs::set_permissions(data.state_file(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert!(refused);
        assert_eq!(std::fs::read(data.state_file()).unwrap(), before);
    }

    // Row b's record asks for a restart: the same write sets the state's
    // `needs_restart`, so a replay restores it. Every outcome sets
    // `supervisor_version`; no other outcome touches `needs_restart`.
    #[test]
    fn an_outcome_that_needs_a_restart_sets_the_states_restart_fields() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        apply_locked_by(&data, "S2", &full_outcome(), "1.2.3").unwrap();
        let plain = State::load_strict(&data.state_file()).unwrap();
        assert!(!plain.needs_restart);
        assert_eq!(plain.supervisor_version.as_deref(), Some("1.2.3"));
        let mut needs = full_outcome();
        needs.record.id = "needs".into();
        needs.record.restart_needed = true;
        needs.record.restart_needed_by = Some("1.2.3".into());
        for _ in 0..2 {
            // A replay writes it again; nothing changes.
            apply_locked_by(&data, "replay", &needs, "1.2.3").unwrap();
            let state = State::load_strict(&data.state_file()).unwrap();
            assert!(state.needs_restart);
            assert_eq!(state.supervisor_version.as_deref(), Some("1.2.3"));
            assert_eq!(state.history.len(), 2);
        }
    }

    fn state_needing_a_restart(data: &DataDir, by: &str) {
        let mut needs = full_outcome();
        needs.record.restart_needed = true;
        needs.record.restart_needed_by = Some(by.to_owned());
        apply_locked_by(data, "S2", &needs, by).unwrap();
    }

    /// An outcome whose record asks for a restart, decided by `by`.
    fn needing(id: &str, by: Option<&str>) -> Outcome {
        let mut outcome = full_outcome();
        outcome.record.id = id.into();
        outcome.record.restart_needed = true;
        outcome.record.restart_needed_by = by.map(str::to_owned);
        outcome
    }

    /// The file's flag and version, and what start-up says of it.
    fn flag(data: &DataDir) -> (bool, Option<String>) {
        let state = State::load_strict(&data.state_file()).unwrap();
        (state.needs_restart, state.supervisor_version)
    }

    // 5(a): V1's flag, and an outcome of V1's that needs a restart still
    // owed when V2 starts: V2's replay must not hand the flag to V2.
    #[test]
    fn a_replay_by_a_newer_version_does_not_make_an_older_ones_flag_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        state_needing_a_restart(&data, "1.0.0");
        let owed = needing("owed", Some("1.0.0"));
        apply_locked_by(&data, "replay", &owed, "2.0.0").unwrap();
        assert_eq!(flag(&data), (false, Some("2.0.0".into())));
        let state = State::load_strict(&data.state_file()).unwrap();
        assert!(state.history.iter().any(|r| r.id == "owed"), "replayed");
        assert!(!restart_flag_at_start(&data, "2.0.0"));
    }

    // 5(b): the clear at start fails; any later save by V2 must not keep it.
    #[test]
    fn a_failed_clear_then_an_ordinary_save_leaves_no_flag() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        state_needing_a_restart(&data, "1.0.0");
        {
            let _held = data.try_lock().unwrap().unwrap();
            assert!(!restart_flag_at_start(&data, "2.0.0"));
        }
        assert_eq!(flag(&data), (true, Some("1.0.0".into())), "still set");
        let mut ordinary = full_outcome();
        ordinary.record.id = "ordinary".into();
        apply_locked_by(&data, "S2", &ordinary, "2.0.0").unwrap();
        assert_eq!(flag(&data), (false, Some("2.0.0".into())));
        assert!(!restart_flag_at_start(&data, "2.0.0"));
    }

    // 5(c): V2's own owed outcome needs a restart and V2 crashed before its
    // state save; V2 restarts and replays it.
    #[test]
    fn a_version_replaying_its_own_requirement_sets_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        apply_locked_by(&data, "replay", &needing("own", Some("2.0.0")), "2.0.0").unwrap();
        assert_eq!(flag(&data), (true, Some("2.0.0".into())));
        assert!(restart_flag_at_start(&data, "2.0.0"));
    }

    // 5(d): a requirement with no recorded version, and no stored flag.
    #[test]
    fn a_requirement_without_a_version_does_not_set_the_flag_on_replay() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        apply_locked_by(&data, "replay", &needing("old", None), "2.0.0").unwrap();
        assert_eq!(flag(&data), (false, Some("2.0.0".into())));
        assert!(!restart_flag_at_start(&data, "2.0.0"));
    }

    // 5(e): the same, but a flag stored by this same version stays.
    #[test]
    fn a_requirement_without_a_version_keeps_a_flag_the_same_version_stored() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        state_needing_a_restart(&data, "2.0.0");
        apply_locked_by(&data, "replay", &needing("old", None), "2.0.0").unwrap();
        assert_eq!(flag(&data), (true, Some("2.0.0".into())));
        assert!(restart_flag_at_start(&data, "2.0.0"));
    }

    // 5(f): V1's requirement, merged with a V2 patch of the same record,
    // stays V1's and does not make the flag V2's.
    #[test]
    fn a_newer_versions_patch_does_not_relabel_the_requirement() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        state_needing_a_restart(&data, "1.0.0");
        let mut patch = Outcome {
            record: open_ferry_update::state::SwitchRecord {
                id: "new".into(),
                installed_binary: Some("replaced".into()),
                restart_needed_by: Some("2.0.0".into()),
                ..Default::default()
            },
            mark_failed: None,
        };
        patch.record.restart_needed = false;
        apply_locked_by(&data, "S5", &patch, "2.0.0").unwrap();
        assert_eq!(flag(&data), (false, Some("2.0.0".into())));
        let state = State::load_strict(&data.state_file()).unwrap();
        let record = state.history.iter().find(|r| r.id == "new").unwrap();
        assert!(record.restart_needed);
        assert_eq!(record.restart_needed_by.as_deref(), Some("1.0.0"));
        assert!(!restart_flag_at_start(&data, "2.0.0"));
    }

    // 5(g): a fresh incompatibility found by V2 itself.
    #[test]
    fn a_fresh_requirement_decided_by_the_running_version_sets_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        state_needing_a_restart(&data, "1.0.0");
        apply_locked_by(&data, "S3", &needing("fresh", Some("2.0.0")), "2.0.0").unwrap();
        assert_eq!(flag(&data), (true, Some("2.0.0".into())));
        assert!(restart_flag_at_start(&data, "2.0.0"));
    }

    // A normal outcome by the same supervisor leaves the flag set and sets the
    // version.
    #[test]
    fn a_normal_outcome_leaves_needs_restart_set_and_sets_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        state_needing_a_restart(&data, "1.2.3");
        let mut later = full_outcome();
        later.record.id = "later".into();
        apply_locked_by(&data, "S2", &later, "1.2.3").unwrap();
        let state = State::load_strict(&data.state_file()).unwrap();
        assert!(state.needs_restart, "{state:?}");
        assert_eq!(state.supervisor_version.as_deref(), Some("1.2.3"));
        assert_eq!(state.history.len(), 2);
    }

    #[test]
    fn the_flag_is_kept_at_the_same_version_and_cleared_at_another() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        assert!(!restart_flag_at_start(&data, "1.2.3"), "no state");
        state_needing_a_restart(&data, "1.2.3");
        let before = std::fs::read(data.state_file()).unwrap();
        assert!(restart_flag_at_start(&data, "1.2.3"));
        assert_eq!(
            std::fs::read(data.state_file()).unwrap(),
            before,
            "no write"
        );
        assert!(!restart_flag_at_start(&data, "1.3.0"));
        let state = State::load_strict(&data.state_file()).unwrap();
        assert!(!state.needs_restart, "cleared in the file: {state:?}");
        assert_eq!(state.history.len(), 1, "nothing else changed");
        // Cleared for good, whatever version asks next.
        assert!(!restart_flag_at_start(&data, "1.2.3"));
    }

    #[test]
    fn a_clear_that_cannot_write_is_retried_at_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        state_needing_a_restart(&data, "1.2.3");
        {
            // Another process holds the update lock: the write fails.
            let _held = data.try_lock().unwrap().unwrap();
            assert!(!restart_flag_at_start(&data, "1.3.0"));
        }
        let state = State::load_strict(&data.state_file()).unwrap();
        assert!(state.needs_restart, "the failed write changed nothing");
        assert!(!restart_flag_at_start(&data, "1.3.0"));
        assert!(
            !State::load_strict(&data.state_file())
                .unwrap()
                .needs_restart,
            "the next start cleared it"
        );
    }

    #[test]
    fn a_missing_state_is_an_empty_one_for_a_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        apply_locked(&data, "S2", &full_outcome()).unwrap();
        assert!(data.state_file().exists());
    }

    #[test]
    fn a_partial_outcome_without_its_state_record_is_not_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        let outcome = Outcome {
            record: open_ferry_update::state::SwitchRecord {
                id: "missing-record".into(),
                installed_binary: Some("failed: install".into()),
                ..Default::default()
            },
            mark_failed: None,
        };
        assert!(apply_locked(&data, "replay", &outcome).is_err());
        assert!(!data.state_file().exists());
    }
}

#[cfg(test)]
mod quiet_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Events(Arc<AtomicUsize>);
    impl tracing::Subscriber for Events {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    #[test]
    fn malformed_state_orphan_predicate_is_quiet_and_false() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        std::fs::write(data.state_file(), b"not json").unwrap();
        let outcome = Outcome {
            record: open_ferry_update::state::SwitchRecord {
                id: "missing".into(),
                installed_binary: Some("replaced".into()),
                ..Default::default()
            },
            mark_failed: None,
        };
        let events = Arc::new(AtomicUsize::new(0));
        tracing::subscriber::with_default(Events(events.clone()), || {
            for _ in 0..3 {
                assert!(!orphaned_partial(&data, &outcome));
            }
            let full = Outcome {
                record: open_ferry_update::state::SwitchRecord {
                    id: "full".into(),
                    from: "1.0.0".into(),
                    to: "2.0.0".into(),
                    ..Default::default()
                },
                mark_failed: None,
            };
            assert!(!orphaned_partial(&data, &full));
        });
        assert_eq!(events.load(Ordering::SeqCst), 0, "predicate must not log");
        assert_eq!(std::fs::read(data.state_file()).unwrap(), b"not json");
    }
}
