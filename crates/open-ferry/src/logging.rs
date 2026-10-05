// Ported from the log setup in CLIProxyAPI internal/logging and
// internal/util's SetLogLevel (v8.0.15, MIT).
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

use crate::file_log::{self, FileLog};

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
