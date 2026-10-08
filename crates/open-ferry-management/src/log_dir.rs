// Ported from CLIProxyAPI internal/api/handlers/management/logs.go
// (logDirectory, isAllowedLogCursorFile, safeLogFilePath) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Where the log routes find their files: the log directory, a file in it
//! by name, and the file opened. The main log's routes (P3 WP-B) and the
//! request log's (P3 WP-A) share them.
//!
//! Deviations from upstream:
//! - [`safe_log_file_path`] takes the names it allows, where upstream's
//!   allows those of the main log and its rotations; each caller passes
//!   its own.
//! - Every file the routes read, empty or remove is opened with
//!   [`open_log_file`], which refuses a symbolic link or other reparse
//!   point, anything but a plain file, and a file with more than one hard
//!   link, on the handle it opened, so nothing outside the directory is
//!   read or emptied through a link in it. Upstream's routes follow links.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use open_ferry_core::observe::dirs::resolve_log_directory;

use crate::state::ManagementState;

/// What a log route opens a file for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// To read it.
    Read,
    /// To read and write it: to empty it.
    Write,
}

/// The error of a file [`open_log_file`] refuses.
#[derive(Debug)]
struct Refused;

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid log file")
    }
}

impl std::error::Error for Refused {}

/// Whether `error` is `open_log_file` refusing a file.
pub fn is_refused(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Refused>())
}

/// The file at `path`, opened for `access`, and its metadata. A symbolic
/// link or other reparse point isn't followed but refused, as is anything
/// but a plain file, and a file with more than one hard link, which may be
/// one outside the log directory as well. The checks are of the handle
/// opened, so the file can't be swapped for a link between the check and
/// its use. A missing file fails as Go's `os.ErrNotExist`; a refused one
/// with `invalid log file`, which [`is_refused`] tells.
pub(crate) fn open_log_file(path: &Path, access: Access) -> io::Result<(File, fs::Metadata)> {
    let mut options = OpenOptions::new();
    options.read(true).write(access == Access::Write);
    platform::no_follow(&mut options);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(error),
        // A link not followed, a directory or a pipe may fail to open: the
        // path tells which, only to choose the error.
        Err(error) => {
            return Err(match fs::symlink_metadata(path) {
                Ok(info) if !info.is_file() => refused(),
                _ => error,
            });
        }
    };
    let info = file.metadata()?;
    if !info.is_file() || platform::is_reparse_point(&info) || platform::links(&file, &info)? != 1 {
        return Err(refused());
    }
    Ok((file, info))
}

/// The error of a refused file.
fn refused() -> io::Error {
    io::Error::other(Refused)
}

/// Makes `link` a symbolic link to the file `target`; false, after saying
/// so, when the system won't let the tests make one (Windows without the
/// privilege or developer mode).
#[cfg(test)]
pub(crate) fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(target, link);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    match made {
        Ok(()) => true,
        Err(error) => {
            eprintln!("skipped: can't make a symbolic link: {error}");
            false
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, Metadata, OpenOptions};
    use std::io;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    /// `FILE_FLAG_OPEN_REPARSE_POINT`: a link is opened itself, not what
    /// it leads to.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    /// `FILE_ATTRIBUTE_REPARSE_POINT`.
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

    /// Opens a link itself.
    pub(super) fn no_follow(options: &mut OpenOptions) {
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    /// Whether the file opened is a reparse point: a symbolic link, a
    /// junction, or anything a filter redirects.
    pub(super) fn is_reparse_point(info: &Metadata) -> bool {
        info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    /// The file's hard links.
    pub(super) fn links(file: &File, _info: &Metadata) -> io::Result<u64> {
        Ok(winapi_util::file::information(file)?.number_of_links())
    }
}

#[cfg(unix)]
mod platform {
    use std::fs::{File, Metadata, OpenOptions};
    use std::io;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    /// `O_NOFOLLOW`, so a link fails to open, and `O_NONBLOCK`, so a pipe
    /// doesn't wait for a writer.
    pub(super) fn no_follow(options: &mut OpenOptions) {
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }

    /// Whether the file opened is a link, which `O_NOFOLLOW` doesn't open.
    pub(super) fn is_reparse_point(info: &Metadata) -> bool {
        info.file_type().is_symlink()
    }

    /// The file's hard links.
    pub(super) fn links(_file: &File, info: &Metadata) -> io::Result<u64> {
        Ok(info.nlink())
    }
}

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
    // Go's `filepath.Abs`, which cleans the path too.
    let dir =
        std::path::absolute(dir).map_err(|error| format!("resolve log directory: {error}"))?;
    Ok(open_ferry_core::auth::path::clean(&dir).join(name))
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

    /// Whether the file at `path` opens for `access`: `Ok` with its
    /// length, or the error's kind, `Refused` for a refused file.
    fn opens(path: &Path, access: Access) -> Result<u64, String> {
        match open_log_file(path, access) {
            Ok((_, info)) => Ok(info.len()),
            Err(error) if is_refused(&error) => {
                assert_eq!(error.to_string(), "invalid log file");
                Err("Refused".to_owned())
            }
            Err(error) => Err(format!("{:?}", error.kind())),
        }
    }

    /// Not upstream's: a plain file opens to read and to write, a missing
    /// one isn't found, and a directory is refused.
    #[test]
    fn only_plain_files_open() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.log");
        fs::write(&file, "line\n").unwrap();
        fs::create_dir(dir.path().join("main.log.1")).unwrap();
        for access in [Access::Read, Access::Write] {
            assert_eq!(opens(&file, access), Ok(5));
            assert_eq!(
                opens(&dir.path().join("missing.log"), access),
                Err("NotFound".to_owned())
            );
            assert_eq!(
                opens(&dir.path().join("main.log.1"), access),
                Err("Refused".to_owned())
            );
        }
    }

    /// Not upstream's: a file with another hard link, which may be outside
    /// the log directory, is refused by both names until it has one.
    #[test]
    fn a_hard_linked_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "OUTSIDE-SECRET\n").unwrap();
        let logs = dir.path().join("logs");
        fs::create_dir(&logs).unwrap();
        let link = logs.join("main.log");
        fs::hard_link(&outside, &link).unwrap();
        for access in [Access::Read, Access::Write] {
            assert_eq!(opens(&link, access), Err("Refused".to_owned()));
            assert_eq!(opens(&outside, access), Err("Refused".to_owned()));
        }
        fs::remove_file(&outside).unwrap();
        assert_eq!(opens(&link, Access::Read), Ok(15));
    }

    /// Not upstream's: a symbolic link to a file is refused, not followed.
    /// Skipped where the tests can't make one.
    #[test]
    fn a_symbolic_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "OUTSIDE-SECRET\n").unwrap();
        let link = dir.path().join("main.log");
        if !symlink_file(&outside, &link) {
            return;
        }
        for access in [Access::Read, Access::Write] {
            assert_eq!(opens(&link, access), Err("Refused".to_owned()));
        }
        assert_eq!(opens(&outside, Access::Read), Ok(15));
    }
}
