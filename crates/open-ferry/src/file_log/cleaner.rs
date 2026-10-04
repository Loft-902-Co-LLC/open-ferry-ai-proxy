// Ported from CLIProxyAPI internal/logging/log_dir_cleaner.go
// (configureLogDirCleanerLocked, stopLogDirCleanerLocked, runLogDirCleaner,
// enforceLogDirSizeLimit, isLogFileName) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The log directory's cleaner: with `logs-max-total-size-mb` positive, a
//! thread checks the log directory at once and then every minute, and when
//! its `*.log` and `*.log.gz` files (the main log's, the request logs and
//! the error logs) pass the limit, deletes the oldest until they are under
//! it, never the main log being written.
//!
//! Deviations from upstream:
//! - On Windows, deleting is retried while another process has the file
//!   open, and a file already gone counts as deleted from the total
//!   (Windows can keep a deleted file that is still open listed for a
//!   while).
//! - A limit too large to count in bytes is no limit.
//! - The main log is recognized by the file it is, not only by its path,
//!   so it is kept under a name that differs in case (`MAIN.LOG`, which
//!   Windows opens for `main.log`) or under another name linked to it.
//!   Upstream keeps it on Windows only because its writer has it open
//!   without sharing deletion.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, SystemTime};

use same_file::Handle;

use super::{file_name, retry_shared};

/// How often the cleaner checks the directory (upstream's
/// `logDirCleanerInterval`).
const INTERVAL: Duration = Duration::from_secs(60);

/// A running cleaner. Dropping it stops it.
#[derive(Debug)]
pub(super) struct Cleaner {
    _stop: mpsc::Sender<()>,
}

impl Cleaner {
    /// Starts a cleaner keeping `dir` under `max_total_size_mb` megabytes,
    /// never deleting `protected`, or none when the limit isn't positive or
    /// `dir` is blank (upstream's `configureLogDirCleanerLocked`).
    pub(super) fn start(
        dir: &Path,
        max_total_size_mb: i64,
        protected: Option<PathBuf>,
    ) -> Option<Self> {
        let max_bytes = u64::try_from(max_total_size_mb)
            .ok()
            .filter(|mb| *mb > 0)?
            .checked_mul(1024 * 1024)?;
        if dir.as_os_str().to_string_lossy().trim().is_empty() {
            return None;
        }
        let dir = dir.to_path_buf();
        let (stop, stopped) = mpsc::channel::<()>();
        let spawned = thread::Builder::new()
            .name("log-dir-cleaner".to_owned())
            .spawn(move || run(&dir, max_bytes, protected.as_deref(), &stopped));
        if let Err(error) = spawned {
            tracing::warn!("logging: failed to start the log directory cleaner: {error}");
            return None;
        }
        Some(Self { _stop: stop })
    }
}

/// Cleans `dir` now and then every [`INTERVAL`] until the cleaner is
/// dropped (upstream's `runLogDirCleaner`).
fn run(dir: &Path, max_bytes: u64, protected: Option<&Path>, stopped: &mpsc::Receiver<()>) {
    loop {
        match enforce_log_dir_size_limit(dir, max_bytes, protected) {
            Ok(0) => {}
            Ok(deleted) => tracing::debug!(
                "logging: removed {deleted} old log file(s) to enforce log directory size limit"
            ),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "logging: failed to enforce log directory size limit"
                );
            }
        }
        match stopped.recv_timeout(INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// A log file the cleaner may delete.
struct LogFile {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

/// Deletes the oldest log files in `dir` until those left total at most
/// `max_bytes`, skipping `protected`, and says how many it deleted
/// (upstream's `enforceLogDirSizeLimit`). A missing directory has nothing
/// to delete.
pub(super) fn enforce_log_dir_size_limit(
    dir: &Path,
    max_bytes: u64,
    protected: Option<&Path>,
) -> io::Result<usize> {
    if max_bytes == 0 {
        return Ok(0);
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    let mut total = 0u64;
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        if !is_log_file_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        // The entry's own metadata: a link isn't a regular file.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let size = metadata.len();
        files.push(LogFile {
            path: entry.path(),
            size,
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
        total = total.saturating_add(size);
    }
    if total <= max_bytes {
        return Ok(0);
    }

    files.sort_by_key(|file| file.modified);
    // The protected file itself, which may be listed under a name cased
    // otherwise, or linked to it.
    let protected_file = protected.and_then(|protected| Handle::from_path(protected).ok());
    let mut deleted = 0;
    for file in files {
        if total <= max_bytes {
            break;
        }
        if protected.is_some_and(|protected| protected == file.path)
            || protected_file.as_ref().is_some_and(|protected| {
                Handle::from_path(&file.path).is_ok_and(|handle| handle == *protected)
            })
        {
            continue;
        }
        match retry_shared(|| fs::remove_file(&file.path)) {
            Ok(()) => deleted += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "logging: failed to remove old log file: {}",
                    file_name(&file.path)
                );
                continue;
            }
        }
        total = total.saturating_sub(file.size);
    }
    Ok(deleted)
}

/// Whether `name` is a log file's: `*.log` or `*.log.gz`, in any case,
/// once trimmed (upstream's `isLogFileName`).
pub(super) fn is_log_file_name(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    !lower.is_empty() && (lower.ends_with(".log") || lower.ends_with(".log.gz"))
}
