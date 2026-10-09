//! What to do about the installed file, by its hash (v3 3.9).
//!
//! After a switch is confirmed, the installed file is replaced by the new
//! version. Before that the file is hashed and compared with `old`, the hash
//! the journal recorded as installed, and `new`, the hash of the install's
//! target. Four cases follow, at the commit (the supervisor is running) and
//! at recovery (an install was found pending at start):
//!
//! | Installed file | Commit | Recovery |
//! |---|---|---|
//! | hash = old | replace it | replace it, idempotent |
//! | hash = new | nothing to do; `installed = target` | the same: finish the record |
//! | another hash | skip; `changed-elsewhere`; clear the install | the same |
//! | missing | restore it from the aside, then replace | place the target, or else restore the installed version |
//!
//! These are pure functions of hashes, so the tests need no files. The
//! retry schedule (1 minute, doubling to 1 hour) is [`install_backoff`].
//!
//! Not upstream's: nothing here is a port.

use std::io;
use std::path::Path;

pub use crate::journal::{INSTALL_RETRY_FIRST, INSTALL_RETRY_MAX, install_backoff};
use crate::state::installed_binary;

/// Which of the four cases the installed file is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashCase {
    /// It is the version that was installed.
    Old,
    /// It is already the target.
    New,
    /// It is something else.
    Other,
    /// It isn't there.
    Missing,
}

/// The case of an installed file with this hash (`None` if missing). A hash
/// that is both `old` and `new` is `New`: the target is in place.
pub fn classify(installed: Option<&str>, old: &str, new: &str) -> HashCase {
    match installed {
        None => HashCase::Missing,
        Some(hash) if hash.eq_ignore_ascii_case(new) => HashCase::New,
        Some(hash) if hash.eq_ignore_ascii_case(old) => HashCase::Old,
        Some(_) => HashCase::Other,
    }
}

/// The hash of the installed file, `None` if it is missing.
pub fn hash_installed(path: &Path) -> io::Result<Option<String>> {
    match crate::staging::hash_file(path) {
        Ok(hash) => Ok(Some(hash)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// What the supervisor does at a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitAction {
    /// Replace the installed file.
    Replace,
    /// It is the target already: set `installed = target`, clear the
    /// install.
    AlreadyReplaced,
    /// Something else replaced it: leave it, clear the install, undo
    /// nothing.
    ChangedElsewhere,
    /// Restore the installed file from the aside, then replace it.
    RestoreThenReplace,
}

/// What to do at a commit for `case`.
pub fn commit_action(case: HashCase) -> CommitAction {
    match case {
        HashCase::Old => CommitAction::Replace,
        HashCase::New => CommitAction::AlreadyReplaced,
        HashCase::Other => CommitAction::ChangedElsewhere,
        HashCase::Missing => CommitAction::RestoreThenReplace,
    }
}

impl CommitAction {
    /// Whether the install is over without a replacement (the journal
    /// clears `install_pending`).
    pub fn is_settled(self) -> bool {
        matches!(self, Self::AlreadyReplaced | Self::ChangedElsewhere)
    }

    /// The record's `installed_binary` once this action has been taken
    /// (for a replacement, once it succeeded).
    pub fn installed_binary(self) -> &'static str {
        match self {
            Self::ChangedElsewhere => installed_binary::CHANGED_ELSEWHERE,
            Self::Replace | Self::AlreadyReplaced | Self::RestoreThenReplace => {
                installed_binary::REPLACED
            }
        }
    }
}

/// The record's `installed_binary` for a switch that failed with `error`,
/// if the failure says anything: `changed-elsewhere` when the installed file
/// was not the one that was classified (the switch refused it, or put it
/// back, and installed nothing), so the caller clears the install and
/// leaves the file as the other writer made it. Any other failure says
/// nothing; the install stays pending.
pub fn installed_binary_for_switch_error(
    error: &crate::switch::SwitchError,
) -> Option<&'static str> {
    error
        .is_changed_elsewhere()
        .then_some(installed_binary::CHANGED_ELSEWHERE)
}

/// What recovery does when it finds an install pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Replace the installed file (idempotent).
    Replace,
    /// The target is in place: finish the record, clear the install.
    FinishRecord,
    /// Something else replaced it: clear the install; the file is adopted at
    /// the next start if no switch is open.
    ChangedElsewhere,
    /// The file is missing: place the target from `versions/`.
    PlaceTarget,
    /// The file is missing and the target isn't verified: restore the
    /// installed version from `versions/`.
    RestoreInstalled,
    /// The file is missing and neither copy is verified: nothing can be
    /// placed.
    Degraded,
}

/// What recovery does for `case`. `target_verified` is whether
/// `versions/<target.version>/` holds a file with the target's hash, and
/// `installed_verified` the same for the version recorded as installed;
/// they matter only when the file is missing, and `Replace` also needs
/// `target_verified`, else it is [`RecoveryAction::Degraded`] (the file is
/// left as it is).
pub fn recovery_action(
    case: HashCase,
    target_verified: bool,
    installed_verified: bool,
) -> RecoveryAction {
    match case {
        HashCase::Old if target_verified => RecoveryAction::Replace,
        HashCase::Old => RecoveryAction::Degraded,
        HashCase::New => RecoveryAction::FinishRecord,
        HashCase::Other => RecoveryAction::ChangedElsewhere,
        HashCase::Missing if target_verified => RecoveryAction::PlaceTarget,
        HashCase::Missing if installed_verified => RecoveryAction::RestoreInstalled,
        HashCase::Missing => RecoveryAction::Degraded,
    }
}

impl RecoveryAction {
    /// The record's `installed_binary` once this action has been taken, if
    /// it says anything.
    pub fn installed_binary(self) -> Option<&'static str> {
        match self {
            Self::Replace | Self::PlaceTarget | Self::FinishRecord => {
                Some(installed_binary::REPLACED)
            }
            Self::ChangedElsewhere => Some(installed_binary::CHANGED_ELSEWHERE),
            Self::RestoreInstalled | Self::Degraded => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const OLD: &str = "aa";
    const NEW: &str = "bb";

    // Not upstream's: the four cases, by hash.
    #[test]
    fn the_installed_file_is_old_new_other_or_missing() {
        assert_eq!(classify(Some("aa"), OLD, NEW), HashCase::Old);
        assert_eq!(classify(Some("bb"), OLD, NEW), HashCase::New);
        assert_eq!(classify(Some("cc"), OLD, NEW), HashCase::Other);
        assert_eq!(classify(None, OLD, NEW), HashCase::Missing);
        // The target in place wins over a target equal to the old file.
        assert_eq!(classify(Some("aa"), "aa", "aa"), HashCase::New);
        assert_eq!(classify(Some("BB"), OLD, NEW), HashCase::New);
    }

    // Not upstream's: the commit column of 3.9.
    #[test]
    fn a_commit_decides_by_the_four_cases() {
        let action = |case| commit_action(case);
        assert_eq!(action(HashCase::Old), CommitAction::Replace);
        assert_eq!(action(HashCase::New), CommitAction::AlreadyReplaced);
        assert_eq!(action(HashCase::Other), CommitAction::ChangedElsewhere);
        assert_eq!(action(HashCase::Missing), CommitAction::RestoreThenReplace);
        assert!(CommitAction::AlreadyReplaced.is_settled());
        assert!(CommitAction::ChangedElsewhere.is_settled());
        assert!(!CommitAction::Replace.is_settled());
        assert!(!CommitAction::RestoreThenReplace.is_settled());
        assert_eq!(CommitAction::Replace.installed_binary(), "replaced");
        assert_eq!(CommitAction::AlreadyReplaced.installed_binary(), "replaced");
        assert_eq!(
            CommitAction::ChangedElsewhere.installed_binary(),
            "changed-elsewhere"
        );
    }

    // Not upstream's: the recovery column of 3.9.
    #[test]
    fn recovery_decides_by_the_four_cases_and_what_is_verified() {
        use RecoveryAction::*;
        assert_eq!(recovery_action(HashCase::Old, true, true), Replace);
        assert_eq!(recovery_action(HashCase::Old, false, true), Degraded);
        assert_eq!(recovery_action(HashCase::New, false, false), FinishRecord);
        assert_eq!(
            recovery_action(HashCase::Other, true, true),
            ChangedElsewhere
        );
        assert_eq!(recovery_action(HashCase::Missing, true, true), PlaceTarget);
        assert_eq!(recovery_action(HashCase::Missing, true, false), PlaceTarget);
        assert_eq!(
            recovery_action(HashCase::Missing, false, true),
            RestoreInstalled
        );
        assert_eq!(recovery_action(HashCase::Missing, false, false), Degraded);
        assert_eq!(FinishRecord.installed_binary(), Some("replaced"));
        assert_eq!(
            ChangedElsewhere.installed_binary(),
            Some("changed-elsewhere")
        );
        assert_eq!(Degraded.installed_binary(), None);
    }

    // Not upstream's: a switch that found the installed file changed maps to
    // changed-elsewhere, and any other failure to nothing.
    #[test]
    fn a_changed_elsewhere_switch_error_maps_to_the_record_value() {
        use crate::switch::SwitchError;
        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join("open-ferry");
        let new = temp.path().join("new");
        std::fs::write(&installed, "other").unwrap();
        std::fs::write(&new, "new").unwrap();
        let plan = crate::switch::SwitchPlan {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            new_binary: new,
            installed,
            keep_installed_at: None,
            keep_root: None,
            keep_policy: crate::switch::KeepPolicy::Immutable,
            preferred_aside: None,
            expected_installed_sha256: Some("not-the-hash".into()),
        };
        let error = crate::switch::ReplaceOnDisk.prepare(&plan).unwrap_err();
        assert_eq!(
            installed_binary_for_switch_error(&error),
            Some("changed-elsewhere")
        );
        let plain = SwitchError {
            step: "x",
            error: std::io::Error::other("y"),
        };
        assert_eq!(installed_binary_for_switch_error(&plain), None);
    }

    // Not upstream's: one minute, doubling to one hour.
    #[test]
    fn the_install_retry_starts_at_a_minute_and_doubles_to_an_hour() {
        let minutes: Vec<_> = (0..9)
            .map(|attempts| install_backoff(attempts).as_secs() / 60)
            .collect();
        assert_eq!(minutes, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(INSTALL_RETRY_FIRST, Duration::from_secs(60));
        assert_eq!(INSTALL_RETRY_MAX, Duration::from_secs(3600));
    }

    // Not upstream's: a file is hashed, and a missing one is `None`.
    #[test]
    fn a_file_is_hashed_and_a_missing_one_is_none() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("open-ferry");
        assert_eq!(hash_installed(&path).unwrap(), None);
        std::fs::write(&path, "abc").unwrap();
        assert_eq!(
            hash_installed(&path).unwrap().as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }
}
