// Ported from the log setup in CLIProxyAPI internal/logging and
// internal/util's SetLogLevel (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Logging to standard output, at debug level when the config's `debug` is
//! on and at info level otherwise. A reload can change the level. Lines are
//! coloured only on a terminal.
//!
//! Deviations from upstream:
//! - `logging-to-file` isn't ported: logs always go to standard output.
//! - Debug level applies to this project's crates only; libraries such as
//!   the HTTP client stay at info, as their debug output would bury ours.

use std::io::IsTerminal;

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Registry, fmt, reload};

/// Changes the level of the installed logger.
#[derive(Clone)]
pub struct LogLevel(reload::Handle<Targets, Registry>);

impl LogLevel {
    /// Logs at debug level when `debug`, and at info level otherwise.
    pub fn set_debug(&self, debug: bool) {
        if let Err(error) = self.0.reload(targets(debug)) {
            tracing::warn!("failed to change the log level: {error}");
        }
    }

    /// A level for a logger that was never installed, for tests.
    #[cfg(test)]
    pub fn detached() -> Self {
        Self(reload::Layer::new(targets(false)).1)
    }
}

/// Installs the logger at info level.
pub fn init() -> LogLevel {
    let (filter, handle) = reload::Layer::new(targets(false));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(std::io::stdout)
                .with_ansi(std::io::stdout().is_terminal()),
        )
        .init();
    LogLevel(handle)
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
