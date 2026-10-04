// Ported from CLIProxyAPI internal/logging/global_logger.go
// (ResolveLogDirectory, isDirWritable) and internal/util/util.go
// (WritablePath) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Where the logs live: the main log, the request logs and the error logs
//! all go to the directory [`resolve_log_directory`] picks.
//!
//! Deviations from upstream: an environment value that isn't valid UTF-8 is
//! converted lossily.

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use open_ferry_translate::go::quote;

use crate::config::Config;
use crate::config::paths::{self, Os};

/// The log directory upstream tries first, relative to the working
/// directory.
const LOG_DIR: &str = "logs";

/// The environment variables that name a writable base directory, in the
/// order upstream reads them.
const WRITABLE_PATH_VARS: [&str; 2] = ["WRITABLE_PATH", "writable_path"];

/// Upstream's `ResolveLogDirectory`: `logs` under the writable path when
/// one is set, else `logs` in the working directory when that is a
/// writable directory, else `logs` in the auth directory. When the auth
/// directory can't be resolved, `logs` in the working directory after all.
pub fn resolve_log_directory(config: &Config) -> PathBuf {
    resolve(
        config,
        writable_path(|name| env::var_os(name)),
        Path::new(LOG_DIR),
    )
}

/// [`resolve_log_directory`] with the writable path given, and `local` as
/// the working directory's `logs`.
fn resolve(config: &Config, writable: Option<String>, local: &Path) -> PathBuf {
    if let Some(base) = writable {
        return PathBuf::from(paths::join(Os::HOST, &base, LOG_DIR));
    }
    if is_dir_writable(local) {
        return local.to_path_buf();
    }
    match paths::resolve_auth_dir(Os::HOST, &config.auth_dir, paths::user_home_dir) {
        Ok(auth_dir) => PathBuf::from(paths::join(Os::HOST, &auth_dir, LOG_DIR)),
        Err(error) => {
            tracing::warn!(
                "Failed to resolve auth-dir {} for log directory: {error}",
                quote(&config.auth_dir)
            );
            local.to_path_buf()
        }
    }
}

/// Upstream's `WritablePath`: the first of `WRITABLE_PATH` and
/// `writable_path` that `lookup` finds and isn't blank, trimmed and
/// cleaned.
fn writable_path(lookup: impl Fn(&str) -> Option<OsString>) -> Option<String> {
    WRITABLE_PATH_VARS.into_iter().find_map(|name| {
        let value = lookup(name)?;
        let value = value.to_string_lossy();
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| paths::clean(Os::HOST, trimmed))
    })
}

/// Upstream's `isDirWritable`: whether `dir` is a directory a file can be
/// made in, found by making and removing `.perm_test`.
fn is_dir_writable(dir: &Path) -> bool {
    if !fs::metadata(dir).is_ok_and(|metadata| metadata.is_dir()) {
        return false;
    }
    let probe = dir.join(".perm_test");
    let writable = File::create(&probe).is_ok();
    if writable {
        let _ = fs::remove_file(&probe);
    }
    writable
}

#[cfg(test)]
mod tests {
    use std::process;

    use super::*;

    /// A fresh directory under the system temp directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join(format!("open-ferry-dirs-{}-{name}", process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // Not upstream's: the first non-blank writable path wins, cleaned.
    #[test]
    fn reads_the_writable_path() {
        let lookup = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(writable_path(lookup(&[])), None);
        assert_eq!(writable_path(lookup(&[("WRITABLE_PATH", "  ")])), None);
        assert_eq!(
            writable_path(lookup(&[
                ("WRITABLE_PATH", " "),
                ("writable_path", " data/x/../y ")
            ])),
            Some(paths::clean(Os::HOST, "data/y"))
        );
        assert_eq!(
            writable_path(lookup(&[
                ("WRITABLE_PATH", "base"),
                ("writable_path", "other")
            ])),
            Some("base".to_owned())
        );
    }

    // Not upstream's: the writable path, then a writable local `logs`, then
    // the auth directory's `logs`.
    #[test]
    fn picks_the_log_directory() {
        let scratch = Scratch::new("resolve");
        let auth = scratch.0.join("auth");
        let config = Config {
            auth_dir: auth.to_string_lossy().into_owned(),
            ..Config::default()
        };
        let local = scratch.0.join("logs");

        assert_eq!(
            resolve(&config, Some("base".to_owned()), &local),
            PathBuf::from(paths::join(Os::HOST, "base", "logs"))
        );
        let in_auth = PathBuf::from(paths::join(
            Os::HOST,
            &paths::clean(Os::HOST, &config.auth_dir),
            "logs",
        ));
        assert_eq!(resolve(&config, None, &local), in_auth);

        fs::write(&local, "not a directory").unwrap();
        assert_eq!(resolve(&config, None, &local), in_auth);
        fs::remove_file(&local).unwrap();

        fs::create_dir(&local).unwrap();
        assert_eq!(resolve(&config, None, &local), local);
        assert!(!local.join(".perm_test").exists());
    }
}
