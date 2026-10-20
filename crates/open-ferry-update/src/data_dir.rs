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
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o755);
        }
        builder.create(&self.root)?;
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

impl Drop for UpdateLock {
    fn drop(&mut self) {
        // flock follows the open file description. A concurrent fork can
        // inherit it until exec closes CLOEXEC descriptors, so closing just
        // our descriptor can leave a released update lock spuriously busy.
        #[cfg(unix)]
        let _ = self._file.unlock();
        #[cfg(windows)]
        unlock_file(&self._file);
    }
}

/// Release the exclusive range used by `File::try_lock` before closing its
/// synchronous handle; Windows does not guarantee immediate unlock on close.
#[cfg(windows)]
pub(crate) fn unlock_file(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    // SAFETY: the file owns a live synchronous handle. The zeroed OVERLAPPED
    // specifies offset zero and lives through the call; the range matches
    // std's exclusive LockFileEx (u64::MAX bytes from offset zero).
    unsafe {
        let mut overlapped: OVERLAPPED = std::mem::zeroed();
        let _ = UnlockFileEx(file.as_raw_handle(), 0, u32::MAX, u32::MAX, &mut overlapped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_released_update_lock_is_not_retained_by_an_inherited_description() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path());
        let lock = data.try_lock().unwrap().unwrap();
        // A dup shares the description exactly as a descriptor inherited
        // across fork does, without racing another test's process creation.
        let inherited = lock._file.try_clone().unwrap();
        drop(lock);
        let next = data
            .try_lock()
            .unwrap()
            .expect("Drop releases the update lock even before the inherited descriptor closes");
        drop(inherited);
        assert!(
            data.try_lock().unwrap().is_none(),
            "closing the inherited descriptor must not release the next owner's lock"
        );
        drop(next);
        assert!(data.try_lock().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn the_data_directory_chain_and_lock_are_not_writable_by_others_with_umask_zero() {
        use crate::journal::creation_tests::{assert_mode, with_umask_zero};
        with_umask_zero(
            "data_dir::tests::the_data_directory_chain_and_lock_are_not_writable_by_others_with_umask_zero",
            |root| {
                let data = DataDir::at(root.join("parent/data"));
                let _lock = data.try_lock().unwrap().unwrap();
                assert_mode(&root.join("parent"), 0o755);
                assert_mode(data.root(), 0o755);
                assert_mode(&data.root().join(LOCK_FILE), 0o600);
                let installed = root.join("installed");
                let _install_lock =
                    crate::install_lock::InstallLock::try_acquire(&installed).unwrap();
                assert_mode(
                    &crate::install_lock::InstallLock::path_for(&installed).unwrap(),
                    0o600,
                );
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_data_directory_chain_and_lock_are_not_writable_by_others_with_default_acl() {
        use crate::journal::creation_tests::{assert_acl_mode, with_default_acl};
        with_default_acl(
            "data_dir::tests::the_data_directory_chain_and_lock_are_not_writable_by_others_with_default_acl",
            |root| {
                let data = DataDir::at(root.join("parent/data"));
                let _lock = data.try_lock().unwrap().unwrap();
                assert_acl_mode(&root.join("parent"), 0o755);
                assert_acl_mode(data.root(), 0o755);
                assert_acl_mode(&data.root().join(LOCK_FILE), 0o600);
                let installed = root.join("installed");
                let _install_lock =
                    crate::install_lock::InstallLock::try_acquire(&installed).unwrap();
                assert_acl_mode(
                    &crate::install_lock::InstallLock::path_for(&installed).unwrap(),
                    0o600,
                );
            },
        );
    }

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
