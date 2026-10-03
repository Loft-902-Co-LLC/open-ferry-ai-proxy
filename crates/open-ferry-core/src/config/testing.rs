//! Helpers for this module's tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fs, process};

/// A fresh directory under the system temp directory, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("open-ferry-config-{}-{n}", process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Writes `contents` to `name` inside the directory and returns its path.
    pub(crate) fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.join(name);
        fs::write(&path, contents).expect("write temp file");
        path
    }

    /// Creates the directory `name` inside this one and returns its path.
    pub(crate) fn mkdir(&self, name: &str) -> PathBuf {
        let path = self.join(name);
        fs::create_dir_all(&path).expect("create temp subdir");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
