// Ported from CLIProxyAPI internal/api/handlers/management/logs.go
// (logDirectory, isAllowedLogCursorFile, safeLogFilePath) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Where the log routes find their files: the log directory, and a file in
//! it by name. The main log's routes (P3 WP-B) and the request log's (P3
//! WP-A) share them.
//!
//! Deviations from upstream: [`safe_log_file_path`] takes the names it
//! allows, where upstream's allows those of the main log and its
//! rotations; each caller passes its own.

#![cfg_attr(
    not(test),
    allow(dead_code, reason = "for the log routes, which aren't ported yet")
)]

use std::path::{Path, PathBuf};

use open_ferry_core::observe::dirs::resolve_log_directory;

use crate::state::ManagementState;

/// The directory the logs are in: the one the binary resolved at start, or
/// else the config's (upstream's `logDirectory`).
pub(crate) fn log_directory(state: &ManagementState) -> PathBuf {
    match &state.observability().log_dir {
        Some(dir) => dir.clone(),
        None => resolve_log_directory(&state.config()),
    }
}

/// The path of the file `name` in `dir`, if `name` is a bare file name
/// that `allowed` accepts (upstream's `safeLogFilePath`, with
/// `isAllowedLogCursorFile`'s checks of the name); else Go's error text.
pub(crate) fn safe_log_file_path(
    dir: &Path,
    name: &str,
    allowed: impl Fn(&str) -> bool,
) -> Result<PathBuf, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err("invalid log file".to_owned());
    }
    if !allowed(name) {
        return Err("invalid log file".to_owned());
    }
    let dir =
        std::path::absolute(dir).map_err(|error| format!("resolve log directory: {error}"))?;
    Ok(dir.join(name))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use open_ferry_core::config::Config;
    use open_ferry_core::manager::{Manager, Settings};
    use open_ferry_core::observe::Observability;
    use open_ferry_core::registry::ModelRegistry;

    use super::*;

    /// Not upstream's: the directory resolved at start wins.
    #[test]
    fn the_directory_resolved_at_start_wins() {
        let registry = Arc::new(ModelRegistry::new());
        let manager = Manager::new(Settings::default(), Arc::clone(&registry) as _, None);
        let state = ManagementState::new(Arc::new(Config::default()), manager, registry, None)
            .with_observability(Observability {
                log_dir: Some(PathBuf::from("resolved-logs")),
                ..Observability::default()
            });
        assert_eq!(log_directory(&state), PathBuf::from("resolved-logs"));
    }

    /// Not upstream's: a name that isn't a bare file name, or that the
    /// caller doesn't allow, is refused.
    #[test]
    fn only_allowed_bare_names_resolve() {
        let dir = Path::new("logs");
        let main = |name: &str| name == "main.log";
        for name in [
            "",
            ".",
            "..",
            "../main.log",
            "a/main.log",
            "a\\main.log",
            "other.log",
        ] {
            assert_eq!(
                safe_log_file_path(dir, name, main),
                Err("invalid log file".to_owned()),
                "{name:?}"
            );
        }
        let path = safe_log_file_path(dir, "main.log", main).unwrap();
        assert!(path.is_absolute());
        assert!(path.ends_with(Path::new("logs").join("main.log")));
    }
}
