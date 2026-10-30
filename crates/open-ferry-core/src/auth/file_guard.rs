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
    check: Option<Arc<Check>>,
    probe_state: ProbeState,
    warned: Arc<Mutex<HashSet<(PathBuf, String)>>>,
}

impl FileGuard {
    /// Checks each credential path before any read or write.
    pub fn new(check: impl Fn(&Path) -> Result<(), String> + Send + Sync + 'static) -> Self {
        Self {
            check: Some(Arc::new(check)),
            probe_state: Default::default(),
            warned: Arc::default(),
        }
    }

    /// Authorizes a file, including a new save target. A configured guard
    /// always refuses links, even if its caller's check accepts the target.
    pub fn check(&self, path: &Path) -> Result<(), String> {
        let Some(check) = &self.check else {
            return Ok(());
        };
        if std::fs::symlink_metadata(path).is_ok_and(|meta| is_link(&meta)) {
            return Err("credential file is a link".to_owned());
        }
        check(path)
    }

    pub(crate) fn with_probe_state(mut self, probe_state: ProbeState) -> Self {
        self.probe_state = probe_state;
        self
    }

    pub(crate) fn presence_only(probe_state: ProbeState) -> Self {
        Self {
            check: None,
            probe_state,
            warned: Arc::default(),
        }
    }

    pub(crate) fn probe_state(&self) -> ProbeState {
        self.probe_state.clone()
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

/// Shared watcher/store fence after an inconclusive credential-presence probe.
/// A runtime save cannot recreate that path until existence is proven again.
#[derive(Clone, Debug, Default)]
pub struct ProbeState {
    uncertain: Arc<Mutex<HashSet<String>>>,
}

impl ProbeState {
    fn key(path: &Path) -> String {
        let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let clean = crate::auth::path::clean(&absolute);
        let value = clean.to_string_lossy();
        #[cfg(windows)]
        {
            open_ferry_translate::go::to_lower(value.strip_prefix(r"\\?\").unwrap_or(&value))
        }
        #[cfg(not(windows))]
        {
            value.into_owned()
        }
    }

    pub(crate) fn unknown(&self, path: &Path) {
        self.uncertain
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(Self::key(path));
    }

    pub(crate) fn present(&self, path: &Path) {
        self.uncertain
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&Self::key(path));
    }

    /// Hold through the filesystem mutation, ordered after the store save lock.
    /// Explicit login creation remains intentional creation, not a runtime save.
    pub(crate) fn write_permit(
        &self,
        path: &Path,
        creation: bool,
    ) -> std::io::Result<std::sync::MutexGuard<'_, HashSet<String>>> {
        let mut uncertain = self
            .uncertain
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = Self::key(path);
        if !creation && uncertain.contains(&key) {
            if std::fs::metadata(path).is_ok() {
                uncertain.remove(&key);
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "credential presence is unresolved",
                ));
            }
        }
        Ok(uncertain)
    }
}
