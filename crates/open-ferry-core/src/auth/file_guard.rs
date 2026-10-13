//! Optional authorization before credential files are read or written.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Check = dyn Fn(&Path) -> Result<(), String> + Send + Sync;

/// A file check shared by credential readers and writers. Refused reads
/// warn once for each path and reason, including across clones.
#[derive(Clone)]
pub struct FileGuard {
    check: Arc<Check>,
    warned: Arc<Mutex<HashSet<(PathBuf, String)>>>,
}

impl FileGuard {
    /// Checks each credential path before any read or write.
    pub fn new(check: impl Fn(&Path) -> Result<(), String> + Send + Sync + 'static) -> Self {
        Self {
            check: Arc::new(check),
            warned: Arc::default(),
        }
    }

    /// Authorizes a file, including a new save target. A configured guard
    /// always refuses links, even if its caller's check accepts the target.
    pub fn check(&self, path: &Path) -> Result<(), String> {
        if std::fs::symlink_metadata(path).is_ok_and(|meta| is_link(&meta)) {
            return Err("credential file is a link".to_owned());
        }
        (self.check)(path)
    }

    /// Checks a read, logging only the path and refusal reason.
    pub fn allows_read(&self, path: &Path) -> bool {
        self.check_read(path).is_ok()
    }

    /// Authorizes a read and returns its refusal after warning once.
    pub fn check_read(&self, path: &Path) -> Result<(), String> {
        match self.check(path) {
            Ok(()) => Ok(()),
            Err(reason) => {
                if self
                    .warned
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert((path.to_path_buf(), reason.clone()))
                {
                    tracing::warn!(path = %path.display(), %reason, "credential file not read");
                }
                Err(reason)
            }
        }
    }
}

#[cfg(windows)]
fn is_link(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0 || meta.file_type().is_symlink()
}

#[cfg(not(windows))]
fn is_link(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

impl fmt::Debug for FileGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileGuard").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_reads_warn_once_per_path_and_reason_across_clones() {
        let reason = Arc::new(Mutex::new("untrusted owner".to_owned()));
        let checked = reason.clone();
        let guard = FileGuard::new(move |_| Err(checked.lock().unwrap().clone()));
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch");
        let path = root.join("never-read.json");
        assert!(!guard.allows_read(&path));
        assert!(!guard.clone().allows_read(&path));
        assert_eq!(guard.warned.lock().unwrap().len(), 1);
        *reason.lock().unwrap() = "untrusted ancestry".to_owned();
        assert!(!guard.allows_read(&path));
        assert!(!guard.allows_read(&root.join("other.json")));
        assert_eq!(guard.warned.lock().unwrap().len(), 3);
    }
}
