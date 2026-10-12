// Ported from the log setup in CLIProxyAPI internal/logging and
// internal/util's SetLogLevel (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Logging, at debug level when the config's `debug` is on and at info
//! level otherwise. A reload can change the level. Where the lines go, and
//! how they are written, is the main log's output (see
//! [`crate::file_log`]).
//!
//! Deviations from upstream:
//! - Debug level applies to this project's crates only; libraries such as
//!   the HTTP client stay at info, as their debug output would bury ours.

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Registry, reload};

use crate::file_log::{self, FileLog, LogControls};

/// Changes the installed logger: its level, and through
/// [`LogLevel::file_log`] where its lines go.
#[derive(Clone)]
pub struct LogLevel {
    level: reload::Handle<Targets, Registry>,
    file_log: FileLog,
}

impl LogLevel {
    /// Logs at debug level when `debug`, and at info level otherwise.
    pub fn set_debug(&self, debug: bool) {
        if let Err(error) = self.level.reload(targets(debug)) {
            tracing::warn!("failed to change the log level: {error}");
        }
    }

    /// Where the lines go.
    pub fn file_log(&self) -> &FileLog {
        &self.file_log
    }

    /// A level for a logger that was never installed, for tests.
    #[cfg(test)]
    pub fn detached() -> Self {
        Self {
            level: reload::Layer::new(targets(false)).1,
            file_log: FileLog::default(),
        }
    }

    /// Not upstream's: like [`detached`](Self::detached), with controls of its
    /// own that start as a supervised worker's do (rotation and cleaning
    /// off), so a test's state is its own, not the process's.
    #[cfg(test)]
    pub fn detached_supervised() -> Self {
        Self {
            level: reload::Layer::new(targets(false)).1,
            file_log: FileLog::with_controls(LogControls::supervised()),
        }
    }
}

/// Installs the logger at info level, writing to the main log's output.
pub fn init() -> LogLevel {
    let (filter, level) = reload::Layer::new(targets(false));
    let (output, file_log) = file_log::init();
    tracing_subscriber::registry()
        .with(filter)
        .with(output)
        .init();
    LogLevel { level, file_log }
}

/// Installs the logger as `init` does, for a supervised worker: the main
/// log's rotation and cleaning start off (`LogControls::supervised`), and
/// the worker turns them on through `LogLevel::file_log`'s controls.
pub fn init_supervised() -> LogLevel {
    let (filter, level) = reload::Layer::new(targets(false));
    let (output, file_log) = file_log::init_with_controls(LogControls::supervised());
    tracing_subscriber::registry()
        .with(filter)
        .with(output)
        .init();
    LogLevel { level, file_log }
}

/// The level of the supervisor's logger, which writes to standard error
/// only. The supervisor never writes `main.log`: that is the workers'.
#[derive(Clone)]
pub struct StderrLevel {
    level: reload::Handle<Targets, Registry>,
}

impl StderrLevel {
    /// Logs at debug level when `debug`, and at info level otherwise.
    pub fn set_debug(&self, debug: bool) {
        if let Err(error) = self.level.reload(targets(debug)) {
            tracing::warn!("failed to change the log level: {error}");
        }
    }
}

/// Installs the supervisor's logger at info level: lines go to standard
/// error, with the same level targets as `init`. Nothing is written to a
/// file, and no thread is started.
pub fn init_stderr() -> StderrLevel {
    let (filter, level) = reload::Layer::new(targets(false));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        )
        .init();
    StderrLevel { level }
}

fn targets(debug: bool) -> Targets {
    let level = if debug {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("open_ferry", level)
}

#[cfg(test)]
mod tests {
    use tracing::Level;

    use super::*;

    #[test]
    fn debug_is_for_this_projects_crates() {
        let debug = targets(true);
        assert!(debug.would_enable("open_ferry_core::auth", &Level::DEBUG));
        assert!(debug.would_enable("open_ferry", &Level::DEBUG));
        assert!(!debug.would_enable("hyper::proto", &Level::DEBUG));
        assert!(debug.would_enable("hyper::proto", &Level::INFO));
        assert!(!targets(false).would_enable("open_ferry_server", &Level::DEBUG));
    }
}
