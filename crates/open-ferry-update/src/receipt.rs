//! The install receipt: `install-receipt.json` in the data directory,
//! written by `install.sh` and `install.ps1` when they install a binary.
//!
//! ```json
//! {"format":1,"installer":"install.sh","version":"0.1.0",
//!  "binary":"/home/me/.local/bin/open-ferry",
//!  "target":"x86_64-unknown-linux-gnu","installed_at":"2026-10-08T12:00:00Z"}
//! ```
//!
//! `installer` is `install.sh` or `install.ps1`; `binary` is the absolute
//! path the installer wrote; `installed_at` is UTC, RFC 3339. Updates
//! replace a binary only when the receipt names the running one (see
//! [`crate::install`]); they never write the receipt. The installers do,
//! through [`Receipt::write_atomic`], from `open-ferry install-self`.

use std::fs;
use std::io;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::journal::{AtomicFailure, atomic_write_removing};

/// The most bytes a receipt may have.
const MAX_SIZE: u64 = 16 * 1024;

/// What an installer recorded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// The receipt's format: 1.
    pub format: u32,
    /// `install.sh` or `install.ps1`.
    pub installer: String,
    /// The version installed.
    pub version: String,
    /// The installed binary's absolute path.
    pub binary: String,
    /// The target triple installed.
    pub target: String,
    /// When, RFC 3339.
    pub installed_at: String,
}

/// Why a receipt couldn't be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptError {
    /// Reading it failed.
    Unreadable(String),
    /// It isn't a receipt.
    Malformed(String),
}

impl std::fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(message) => write!(f, "the install receipt can't be read: {message}"),
            Self::Malformed(message) => write!(f, "the install receipt isn't valid: {message}"),
        }
    }
}

impl std::error::Error for ReceiptError {}

/// Why a receipt couldn't be written, and on which side of the move into
/// place: it decides whether the install it belongs to stands.
#[derive(Debug)]
pub enum WriteError {
    /// The receipt was not published: the old one, or none, is as it was.
    /// No temporary file is left, unless the error names one that could not
    /// be removed.
    NotPublished(io::Error),
    /// The receipt was published, and only the directory sync after the move
    /// failed: a crash before the disk flushes may lose it.
    PublishedNotDurable(io::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPublished(error) => write!(f, "the install receipt wasn't written: {error}"),
            Self::PublishedNotDurable(error) => write!(
                f,
                "the install receipt was written, but the disk didn't confirm it: {error}"
            ),
        }
    }
}

impl std::error::Error for WriteError {}

/// The error of a write that did not publish: when the writer could not remove
/// its temporary file, the file and why, since [`WriteError::NotPublished`]
/// promises no temporary file is left only when none is.
fn not_published(failure: AtomicFailure) -> io::Error {
    let AtomicFailure {
        error, left_behind, ..
    } = failure;
    match left_behind {
        None => error,
        Some((temp, removal)) => io::Error::new(
            error.kind(),
            format!(
                "{error}; the temporary file {} could not be removed ({removal}) and can be \
                 deleted",
                temp.display()
            ),
        ),
    }
}

impl Receipt {
    /// The receipt of an install by `installer` of `version` for `target` as
    /// `binary`, made at `now`.
    pub fn new(
        installer: &str,
        version: &str,
        binary: &str,
        target: &str,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            format: 1,
            installer: installer.to_owned(),
            version: version.to_owned(),
            binary: binary.to_owned(),
            target: target.to_owned(),
            installed_at: now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        }
    }

    /// Writes the receipt to `path`: through a temporary file with a name of
    /// its own beside it, fsynced, then moved into place, then the directory
    /// synced. It is the one line of JSON the scripts wrote, with the same
    /// fields in the same order.
    pub fn write_atomic(&self, path: &Path) -> Result<(), WriteError> {
        self.write_removing(path, &mut |temp| fs::remove_file(temp))
    }

    /// [`Receipt::write_atomic`], with the removal of a failed write's
    /// temporary file given, so a test can make it fail.
    fn write_removing(
        &self,
        path: &Path,
        remove: &mut dyn FnMut(&Path) -> io::Result<()>,
    ) -> Result<(), WriteError> {
        let mut bytes = serde_json::to_vec(self)
            .map_err(|error| WriteError::NotPublished(io::Error::other(error)))?;
        bytes.push(b'\n');
        // The failure says whether its error came after the move.
        atomic_write_removing(path, &bytes, true, remove).map_err(|failure| {
            if failure.landed {
                WriteError::PublishedNotDurable(failure.error)
            } else {
                WriteError::NotPublished(not_published(failure))
            }
        })
    }

    /// The receipt at `path`: `Ok(None)` when there is none.
    pub fn load(path: &Path) -> Result<Option<Self>, ReceiptError> {
        let size = match fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ReceiptError::Unreadable(error.to_string())),
        };
        if size > MAX_SIZE {
            return Err(ReceiptError::Malformed(format!(
                "it is over {MAX_SIZE} bytes"
            )));
        }
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ReceiptError::Unreadable(error.to_string())),
        };
        let receipt: Self = serde_json::from_slice(&data)
            .map_err(|error| ReceiptError::Malformed(error.to_string()))?;
        if receipt.format != 1 {
            return Err(ReceiptError::Malformed(format!(
                "format {} isn't 1",
                receipt.format
            )));
        }
        if receipt.binary.is_empty() {
            return Err(ReceiptError::Malformed("it names no binary".into()));
        }
        Ok(Some(receipt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> Receipt {
        let now = Utc.with_ymd_and_hms(2026, 10, 12, 8, 30, 5).unwrap();
        Receipt::new(
            "install.sh",
            "0.1.0",
            "/home/me/.local/bin/open-ferry",
            "x86_64-unknown-linux-gnu",
            now,
        )
    }

    fn names(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_receipt_has_the_scripts_fields_in_their_order_and_time_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install-receipt.json");
        sample().write_atomic(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            concat!(
                r#"{"format":1,"installer":"install.sh","version":"0.1.0","#,
                r#""binary":"/home/me/.local/bin/open-ferry","#,
                r#""target":"x86_64-unknown-linux-gnu","installed_at":"2026-10-12T08:30:05Z"}"#,
                "\n"
            )
        );
        assert_eq!(Receipt::load(&path).unwrap(), Some(sample()));
    }

    #[test]
    fn writing_a_receipt_replaces_the_old_one_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install-receipt.json");
        let mut old = sample();
        old.version = "0.0.9".into();
        old.write_atomic(&path).unwrap();
        sample().write_atomic(&path).unwrap();
        assert_eq!(Receipt::load(&path).unwrap(), Some(sample()));
        assert_eq!(names(dir.path()), ["install-receipt.json"]);
    }

    #[test]
    fn writers_racing_in_one_directory_never_share_a_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install-receipt.json");
        let writers: Vec<_> = (0..8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut receipt = sample();
                    receipt.version = format!("0.1.{index}");
                    let mut written = 0;
                    for _ in 0..10 {
                        match receipt.write_atomic(&path) {
                            Ok(()) => written += 1,
                            // Windows can refuse a move onto a file another
                            // writer is replacing at that moment (installs
                            // don't race like this: the locks order them).
                            // The refused write still removes its temporary
                            // file, as the names below check.
                            Err(WriteError::NotPublished(error))
                                if cfg!(windows)
                                    && matches!(
                                        error.kind(),
                                        io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                                    ) => {}
                            Err(error) => panic!("{error}"),
                        }
                    }
                    written
                })
            })
            .collect();
        let written: u32 = writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .sum();
        assert!(written > 0, "no write succeeded");
        assert_eq!(names(dir.path()), ["install-receipt.json"]);
        assert!(Receipt::load(&path).unwrap().is_some());
    }

    #[test]
    fn a_receipt_that_cannot_be_moved_into_place_is_not_published() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install-receipt.json");
        // A directory where the receipt goes: the move can't replace it.
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), b"x").unwrap();
        let error = sample().write_atomic(&path).unwrap_err();
        assert!(matches!(error, WriteError::NotPublished(_)), "{error}");
        assert!(error.to_string().contains("wasn't written"), "{error}");
        assert!(path.join("keep").is_file());
        assert_eq!(
            names(dir.path()),
            ["install-receipt.json"],
            "a temporary file is left"
        );
    }

    #[test]
    fn a_receipt_in_a_directory_that_is_a_file_is_not_published() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("open-ferry");
        fs::write(&blocker, b"not a directory").unwrap();
        let error = sample()
            .write_atomic(&blocker.join("install-receipt.json"))
            .unwrap_err();
        assert!(matches!(error, WriteError::NotPublished(_)), "{error}");
        assert_eq!(fs::read(&blocker).unwrap(), b"not a directory");
    }

    #[test]
    fn a_temporary_file_that_could_not_be_removed_is_named_with_the_removal_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install-receipt.json");
        // A directory where the receipt goes: the move fails, and then the
        // removal of the temporary file does too.
        fs::create_dir(&path).unwrap();
        // Another process's temporary file beside it is not this write's.
        fs::write(dir.path().join(".install-receipt.json.1.1f.0.tmp"), b"x").unwrap();
        let mut tried = Vec::new();
        let error = sample()
            .write_removing(&path, &mut |temp| {
                tried.push(temp.to_owned());
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "locked"))
            })
            .unwrap_err();
        assert!(matches!(error, WriteError::NotPublished(_)), "{error}");
        let [temp] = tried.as_slice() else {
            panic!("removals tried: {tried:?}");
        };
        assert!(temp.is_file(), "the writer's temp file is not there");
        let message = error.to_string();
        assert!(message.contains(&temp.display().to_string()), "{message}");
        assert!(
            message.contains("could not be removed (locked)"),
            "{message}"
        );
        assert!(!message.contains(".install-receipt.json.1.1f"), "{message}");
    }

    #[test]
    fn a_removed_temporary_file_is_not_mentioned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install-receipt.json");
        fs::create_dir(&path).unwrap();
        let error = sample().write_atomic(&path).unwrap_err();
        let message = error.to_string();
        assert!(!message.contains("could not be removed"), "{message}");
        assert!(!message.contains(".tmp"), "{message}");
        // A temp that was already gone is gone, not left behind.
        let error = sample()
            .write_removing(&path, &mut |temp| {
                fs::remove_file(temp)?;
                Err(io::Error::from(io::ErrorKind::NotFound))
            })
            .unwrap_err();
        assert!(
            !error.to_string().contains("could not be removed"),
            "{error}"
        );
        assert_eq!(names(dir.path()), ["install-receipt.json"]);
    }

    #[cfg(feature = "blue-green-test")]
    #[test]
    fn a_receipt_whose_directory_sync_fails_is_published_but_not_durable() {
        let dir = tempfile::tempdir().unwrap();
        let seams = dir.path().join(".blue-green-test");
        fs::create_dir_all(&seams).unwrap();
        fs::write(seams.join("atomic.landed_install-receipt.json.fail"), b"").unwrap();
        let path = dir.path().join("install-receipt.json");
        let error = sample().write_atomic(&path).unwrap_err();
        assert!(
            matches!(error, WriteError::PublishedNotDurable(_)),
            "{error}"
        );
        assert!(error.to_string().contains("didn't confirm"), "{error}");
        assert_eq!(Receipt::load(&path).unwrap(), Some(sample()));
    }
}
