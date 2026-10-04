//! The main log's output: with `logging-to-file`, log lines go to
//! `main.log` in the log directory, rotated at 10 MB, and with
//! `logs-max-total-size-mb` a cleaner keeps the directory's logs under the
//! limit (upstream's internal/logging/global_logger.go `SetupBaseLogger`,
//! `LogFormatter` and `ConfigureLogOutput`, and log_dir_cleaner.go). Not
//! ported yet (P3 WP-B).
//!
//! What is here are the hooks the rest of the binary calls, with the
//! signatures the port keeps: the logger installs the layer [`init`] gives,
//! which writes every log line, and [`reconfigure`] applies each config as
//! it is loaded. For now lines go to standard output in tracing's format,
//! coloured only on a terminal, and the config changes nothing.
//!
//! Deviations from upstream: `logging-to-file` isn't ported yet, so logs
//! always go to standard output, and the lines aren't upstream's.

use std::io::IsTerminal;

use open_ferry_core::config::Config;
use tracing::Subscriber;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, fmt};

#[cfg(test)]
mod tests;

/// Where the log lines go, which [`reconfigure`] changes. Cloning gives
/// another handle to the same output.
#[derive(Clone, Debug, Default)]
pub struct FileLog {}

/// The layer that writes every log line, and the handle that changes where
/// the lines go. They go to standard output until a config says otherwise.
pub fn init<S>() -> (Box<dyn Layer<S> + Send + Sync>, FileLog)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let layer = fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(std::io::stdout().is_terminal());
    (Box::new(layer), FileLog::default())
}

/// Applies `config` to the output (upstream's `ConfigureLogOutput`, at
/// start and on a reload that changes `logging-to-file` or
/// `logs-max-total-size-mb`). `previous` is the config before, `None` at
/// start.
pub fn reconfigure(file_log: &FileLog, previous: Option<&Config>, config: &Config) {
    let _ = (file_log, previous, config);
}
