// Ported from CLIProxyAPI internal/logging/global_logger.go (SetupBaseLogger,
// ConfigureLogOutput) and internal/api/server_reload.go (the log output's
// part of the reload) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The main log's output: every log line, in upstream's format (see
//! [`format`](mod@format)), goes to standard output, or with
//! `logging-to-file` to `main.log` in the log directory, rotated at 10 MB
//! (see [`writer`]). With `logs-max-total-size-mb` a cleaner keeps the
//! directory's logs under the limit (see [`cleaner`]).
//!
//! The logger installs the layer [`init`] gives, which writes every log
//! line, and [`reconfigure`] applies each config as it is loaded: at start,
//! and on a reload that changes `logging-to-file` or
//! `logs-max-total-size-mb`, as upstream's reload does.
//!
//! Lines go through a bounded queue to a thread of their own, for
//! `main.log` or for standard output, which does the I/O, so logging never
//! waits on the disk or on whatever reads standard output. When the queue
//! is full a line is dropped and counted, and the writer says how many it
//! lost (upstream writes each line as it is logged). A switch away from a
//! file returns once the lines queued for it are written and it is closed.
//!
//! Deviations from upstream:
//! - A log directory that can't be made, at start as on a reload, is
//!   logged and the output stays as it was. Upstream's start exits.
//! - Lines are queued, and dropped when the queue is full, for standard
//!   output as for `main.log`. At exit, the process waits up to a second
//!   for the queued lines to be written; any left then are lost.
//! - When `logging-to-file` stays on and the directory is the same, the
//!   file stays open, where upstream reopens it.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use open_ferry_core::config::Config;
use open_ferry_core::observe::dirs::resolve_log_directory;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;

mod cleaner;
mod format;
mod writer;

#[cfg(test)]
mod tests;

use cleaner::Cleaner;
use format::FormatLayer;
use writer::Output;

/// The main log's file name in the log directory (upstream's
/// `defaultLogFileName`).
pub const MAIN_LOG: &str = "main.log";

/// Where the log lines go, which [`reconfigure`] changes. Cloning gives
/// another handle to the same output.
#[derive(Clone, Debug, Default)]
pub struct FileLog {
    output: Arc<Output>,
    cleaner: Arc<Mutex<Option<Cleaner>>>,
}

impl FileLog {
    /// Sends the log lines to `main.log` in `dir` when `to_file`, else to
    /// standard output, and keeps `dir` under `max_total_size_mb` when
    /// that is positive (upstream's `ConfigureLogOutput`, with the
    /// directory resolved).
    pub(crate) fn configure(&self, dir: &Path, to_file: bool, max_total_size_mb: i64) {
        let mut cleaner = self.cleaner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut protected = None;
        if to_file {
            if let Err(error) = std::fs::create_dir_all(dir) {
                tracing::error!(
                    "failed to configure log output: logging: failed to create log directory: {error}"
                );
                return;
            }
            let path = dir.join(MAIN_LOG);
            if let Err(error) = self.output.to_file(&path) {
                tracing::error!("failed to configure log output: {error}");
                return;
            }
            protected = Some(path);
        } else {
            self.output.to_stdout();
        }
        // Stops the previous cleaner, as dropping it does.
        *cleaner = None;
        *cleaner = Cleaner::start(dir, max_total_size_mb, protected);
    }

    /// Where the lines go: `main.log`'s path, or `None` for standard
    /// output.
    #[cfg(test)]
    pub(crate) fn file(&self) -> Option<std::path::PathBuf> {
        self.output.file()
    }

    /// Waits, until `timeout` has passed at most, for the lines logged so
    /// far to be written.
    pub fn flush(&self, timeout: std::time::Duration) {
        self.output.flush(timeout);
    }

    /// Waits until the lines logged so far are written.
    #[cfg(test)]
    pub(crate) fn sync(&self) {
        self.output.flush(std::time::Duration::from_secs(60));
    }
}

/// The layer that writes every log line, and the handle that changes where
/// the lines go. They go to standard output until a config says otherwise
/// (upstream's `SetupBaseLogger`).
pub fn init<S>() -> (Box<dyn Layer<S> + Send + Sync>, FileLog)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let file_log = FileLog::default();
    let layer = FormatLayer::new(Arc::clone(&file_log.output));
    (Box::new(layer), file_log)
}

/// Applies `config` to the output (upstream's `ConfigureLogOutput`, at
/// start and on a reload that changes `logging-to-file` or
/// `logs-max-total-size-mb`). `previous` is the config before, `None` at
/// start.
pub fn reconfigure(file_log: &FileLog, previous: Option<&Config>, config: &Config) {
    let changed = previous.is_none_or(|previous| {
        previous.logging_to_file != config.logging_to_file
            || previous.logs_max_total_size_mb != config.logs_max_total_size_mb
    });
    if !changed {
        return;
    }
    let dir = resolve_log_directory(config);
    file_log.configure(&dir, config.logging_to_file, config.logs_max_total_size_mb);
}

/// Runs `op` again, up to three more times with a growing pause, while it
/// fails as Windows fails on a file another process has open: a sharing
/// violation, a lock violation or access denied (an antivirus or the
/// indexer). Elsewhere `op` runs once.
pub(crate) fn retry_shared<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut pause = std::time::Duration::from_millis(50);
    for _ in 0..3 {
        match op() {
            Err(error) if is_sharing_error(&error) => {
                std::thread::sleep(pause);
                pause *= 2;
            }
            result => return result,
        }
    }
    op()
}

/// Whether `error` is how Windows refuses a file another process has open.
fn is_sharing_error(error: &std::io::Error) -> bool {
    /// `ERROR_SHARING_VIOLATION` and `ERROR_LOCK_VIOLATION`.
    const SHARING: [i32; 2] = [32, 33];
    cfg!(windows)
        && (error.kind() == std::io::ErrorKind::PermissionDenied
            || error
                .raw_os_error()
                .is_some_and(|code| SHARING.contains(&code)))
}

/// `path`'s file name, for messages.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}
