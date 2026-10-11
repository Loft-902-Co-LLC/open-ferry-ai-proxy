//! Where updates keep their files.
//!
//! The data directory is `$XDG_DATA_HOME/open-ferry` when `XDG_DATA_HOME`
//! is an absolute path, else `~/.local/share/open-ferry`; on Windows
//! `%LOCALAPPDATA%\open-ferry`. It holds:
//! - `versions/<version>/open-ferry[.exe]`: the binaries kept: the one
//!   staged for the next switch, the one running since the last switch,
//!   and the one before it, for a rollback;
//! - `update-state.json`: what the last check found (see [`crate::state`]);
//!   no secrets;
//! - `update.lock`: held while a check, a stage or a switch runs, so two
//!   never overlap;
//! - `install-receipt.json`: written by `install.sh` and `install.ps1`
//!   (see [`crate::receipt`]).

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// The data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

/// Why there is no data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoDataDir(pub &'static str);

impl fmt::Display for NoDataDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for NoDataDir {}

/// The state file's name.
pub const STATE_FILE: &str = "update-state.json";
/// The lock file's name.
pub const LOCK_FILE: &str = "update.lock";
/// The install receipt's name.
pub const RECEIPT_FILE: &str = "install-receipt.json";
/// The versions directory's name.
pub const VERSIONS_DIR: &str = "versions";

impl DataDir {
    /// The data directory at `root`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The data directory for this user, from the environment.
    pub fn for_this_user() -> Result<Self, NoDataDir> {
        Self::locate(|name| std::env::var_os(name), cfg!(windows))
    }

    /// The data directory from the variables `var` gives, on Windows when
    /// `windows`.
    pub fn locate(
        var: impl Fn(&str) -> Option<OsString>,
        windows: bool,
    ) -> Result<Self, NoDataDir> {
        let absolute = |name: &str| {
            var(name)
                .map(PathBuf::from)
                .filter(|path| is_absolute(path, windows))
        };
        if windows {
            return absolute("LOCALAPPDATA")
                .map(|dir| Self::at(dir.join("open-ferry")))
                .ok_or(NoDataDir("LOCALAPPDATA isn't set to an absolute path"));
        }
        if let Some(dir) = absolute("XDG_DATA_HOME") {
            return Ok(Self::at(dir.join("open-ferry")));
        }
        absolute("HOME")
            .map(|home| Self::at(home.join(".local/share/open-ferry")))
            .ok_or(NoDataDir("HOME isn't set to an absolute path"))
    }

    /// The directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the kept binaries are.
    pub fn versions(&self) -> PathBuf {
        self.root.join(VERSIONS_DIR)
    }

    /// The directory of `version`'s kept binary.
    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.versions().join(version)
    }

    /// `version`'s kept binary, named `binary`.
    pub fn binary(&self, version: &str, binary: &str) -> PathBuf {
        self.version_dir(version).join(binary)
    }

    /// The state file.
    pub fn state_file(&self) -> PathBuf {
        self.root.join(STATE_FILE)
    }

    /// The install receipt.
    pub fn receipt_file(&self) -> PathBuf {
        self.root.join(RECEIPT_FILE)
    }

    /// The versions kept, by their directories' names.
    pub fn kept_versions(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.versions()) else {
            return Vec::new();
        };
        let mut versions: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| semver::Version::parse(name).is_ok())
            .collect();
        versions.sort();
        versions
    }

    /// Takes the update lock, creating the directory: `Ok(None)` while
    /// another process holds it.
    pub fn try_lock(&self) -> io::Result<Option<UpdateLock>> {
        fs::create_dir_all(&self.root)?;
        self.try_lock_existing_dir()
    }

    /// Takes the update lock without creating any directories. Refuses links
    /// and non-regular lock files, including a link whose target is absent.
    pub fn try_lock_existing_dir(&self) -> io::Result<Option<UpdateLock>> {
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Share READ and WRITE so other contenders can open and try the
            // same OS lock, but not DELETE: a held lock's directory entry must
            // not be renamed or removed. The updater and supervisor retain
            // update.lock permanently; their cleanup targets only versions,
            // pins, journal/state temps and installed-file asides.
            options
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
                .share_mode(
                    windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                        | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
                );
        }
        let file = options.open(self.root.join(LOCK_FILE))?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            check_lock_attributes(file.metadata()?.file_attributes())?;
        }
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "update lock is not a regular file",
            ));
        }
        match file.try_lock() {
            Ok(()) => Ok(Some(UpdateLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

/// The Windows no-follow open's attribute check, kept separate so a test
/// exercises the refusal even on hosts that refuse opening directory junctions.
#[cfg(windows)]
fn check_lock_attributes(attributes: u32) -> io::Result<()> {
    if attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "update lock is a reparse point",
        ));
    }
    Ok(())
}

/// Whether `path` is absolute on Windows (when `windows`) or on Unix,
/// whichever this runs on.
fn is_absolute(path: &Path, windows: bool) -> bool {
    let text = path.to_string_lossy();
    if windows {
        let bytes = text.as_bytes();
        path.is_absolute()
            || (bytes.len() >= 3
                && bytes.first().is_some_and(u8::is_ascii_alphabetic)
                && bytes.get(1) == Some(&b':')
                && matches!(bytes.get(2), Some(b'\\' | b'/')))
    } else {
        text.starts_with('/')
    }
}

/// The update lock, held until dropped.
#[derive(Debug)]
pub struct UpdateLock {
    _file: File,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_lock_link_to_a_missing_target_is_refused_without_creating_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        fs::create_dir(&root).unwrap();
        let target = dir.path().join("must-not-exist");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, root.join(LOCK_FILE)).unwrap();
        #[cfg(windows)]
        {
            let result = std::process::Command::new(std::env::var_os("COMSPEC").unwrap())
                .args(["/c", "mklink", "/J"])
                .arg(root.join(LOCK_FILE))
                .arg(&target)
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
        }
        assert!(DataDir::at(root).try_lock().is_err());
        assert!(!target.exists());
    }

    #[cfg(windows)]
    #[test]
    fn the_reparse_attribute_has_its_own_lock_refusal() {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        };
        let error = check_lock_attributes(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "update lock is a reparse point");
        assert!(check_lock_attributes(0).is_ok());
    }

    #[test]
    fn locking_an_existing_root_never_creates_its_directory_chain() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("absent").join("data");
        assert!(DataDir::at(&root).try_lock_existing_dir().is_err());
        assert!(!dir.path().join("absent").exists());
    }
}
