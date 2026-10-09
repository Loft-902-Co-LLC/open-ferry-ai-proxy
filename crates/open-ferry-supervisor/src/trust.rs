//! Trust checks on the directories and files a supervisor runs binaries from.
//!
//! A supervisor that runs as root or LocalSystem executes whatever it finds in its data
//! directory, so anything another account can write there is a way to escalate. The checks
//! here are library functions: the caller decides when to run them (at start, and before a
//! worker is spawned from a path that was not checked yet) and what a failure means. The
//! design says a failure disables switching and never fails the service.
//!
//! Unix walks every path component from `/` with `openat(O_NOFOLLOW | O_DIRECTORY)` and
//! `fstat`, so a symlink anywhere in the chain is refused. Windows checks each component for
//! a reparse point, its owner, and the DACL's write-class allow entries.
//!
//! The function names are the same on both systems so callers need no `cfg`.

use std::fmt;
use std::path::{Path, PathBuf};

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(unix)]
pub use unix::{TrustRules, check_dirs, check_file, create_private_dir};
#[cfg(windows)]
pub use windows::{TrustRules, check_dirs, check_file, create_private_dir, create_system_data_dir};

/// A path that failed a trust check, and why. The reason names the component at fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustError {
    pub path: PathBuf,
    pub reason: String,
}

impl TrustError {
    pub(crate) fn new(path: &Path, reason: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is not safe to run binaries from: {}",
            self.path.display(),
            self.reason
        )
    }
}

impl std::error::Error for TrustError {}
