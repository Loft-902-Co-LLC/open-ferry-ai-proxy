//! Writing a config file safely: the writer's own steps around upstream's
//! `os.WriteFile`.
//!
//! [`commit`] checks that the new contents load as a config, keeps the
//! file's current contents as `<file name>.bak`, and replaces the file
//! atomically: it writes a temporary file in the same directory, flushes
//! it to disk and renames it over the file. On Unix the new file and the
//! backup get the file's permissions. A symbolic link at the path is
//! refused, before anything is read and again before anything is written.
//! A file that can't be renamed over, such as a file bind-mounted on its
//! own into a container, is written in place instead, as upstream does.
//!
//! Deviations from upstream:
//! - Upstream writes the file in place with no check and no backup, and
//!   follows a symbolic link. The writer writes in place only when the
//!   rename is refused.
//! - A new file is created readable by its owner only (0600 on Unix),
//!   where upstream's management `WriteConfig` creates it 0644.
//! - I/O errors are worded `open <path>: <error>` with the platform's
//!   error text, which differs from Go's.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::super::load::load_bytes;
use super::{SaveError, SaveErrorKind};

/// The file name of the backup of `path`: `<file name>.bak`.
pub(crate) fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// Fails when `path` is a symbolic link (or, on Windows, another reparse
/// point) or a directory. A missing file is fine.
pub(crate) fn refuse_link(path: &Path) -> Result<(), SaveError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("open", path, error)),
    };
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(SaveError::new(
            SaveErrorKind::Symlink,
            format!(
                "refusing to write {}: it is a symbolic link",
                path.display()
            ),
        ));
    }
    if metadata.is_dir() {
        return Err(SaveError::new(
            SaveErrorKind::Io,
            format!("open {}: is a directory", path.display()),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

pub(crate) fn io_error(op: &str, path: &Path, error: io::Error) -> SaveError {
    SaveError::io(format!("{op} {}: {error}", path.display()), error)
}

/// Reads the config file at `path`, refusing a symbolic link.
pub(crate) fn read(path: &Path) -> Result<Vec<u8>, SaveError> {
    refuse_link(path)?;
    fs::read(path).map_err(|error| io_error("open", path, error))
}

/// Writes `data` to the config file at `path`: checks that it loads as a
/// config, backs up the file's current contents to `<file name>.bak`, and
/// replaces the file atomically. Nothing is written if a step fails before
/// the replacement.
pub(crate) fn commit(path: &Path, data: &[u8]) -> Result<(), SaveError> {
    load_bytes(data).map_err(|error| SaveError::new(SaveErrorKind::Check, error.to_string()))?;
    refuse_link(path)?;
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_error("open", path, error)),
    };
    if permissions.is_some() {
        let current = fs::read(path).map_err(|error| io_error("open", path, error))?;
        let backup = backup_path(path);
        replace(dir, &backup, &current, permissions.as_ref())?;
    }
    replace(dir, path, data, permissions.as_ref())
}

/// Writes `data` to a temporary file in `dir`, with `permissions` on Unix,
/// flushes it and renames it to `target`.
fn replace(
    dir: &Path,
    target: &Path,
    data: &[u8],
    permissions: Option<&fs::Permissions>,
) -> Result<(), SaveError> {
    let mut file = tempfile::Builder::new()
        .prefix(".")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(|error| io_error("open", target, error))?;
    set_permissions(file.as_file(), permissions)
        .map_err(|error| io_error("chmod", target, error))?;
    file.write_all(data)
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| io_error("write", target, error))?;
    match file.persist(target) {
        Ok(_) => {
            sync_dir(dir);
            Ok(())
        }
        // The temporary file is deleted as the error drops.
        Err(error) if rename_refused(&error.error) => write_in_place(target, data),
        Err(error) => Err(io_error("rename", target, error.error)),
    }
}

/// Whether a rename over a file failed because the file can't be renamed
/// over, though it can be written: Linux refuses with EBUSY, or EXDEV,
/// when the file is a mount point of its own, as a container's config.yaml
/// is when it's bind-mounted by itself.
fn rename_refused(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ResourceBusy | io::ErrorKind::CrossesDevices
    )
}

/// Writes `data` over the file at `target`, as upstream's `os.WriteFile`
/// does: the file keeps its identity and permissions, but a failure part of
/// the way through can leave it partly written. The caller has kept a
/// backup.
fn write_in_place(target: &Path, data: &[u8]) -> Result<(), SaveError> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(target)
        .map_err(|error| io_error("open", target, error))?;
    file.write_all(data)
        .and_then(|()| file.sync_all())
        .map_err(|error| io_error("write", target, error))
}

#[cfg(unix)]
fn set_permissions(file: &fs::File, permissions: Option<&fs::Permissions>) -> io::Result<()> {
    match permissions {
        Some(permissions) => file.set_permissions(permissions.clone()),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn set_permissions(_file: &fs::File, _permissions: Option<&fs::Permissions>) -> io::Result<()> {
    Ok(())
}

/// Flushes the directory entry of a rename to disk, where the platform
/// allows it; a failure only weakens durability, so it is ignored.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testing::TempDir;

    // Not upstream's: a write replaces the file and keeps one backup of what
    // it held.
    #[test]
    fn commit_replaces_and_backs_up() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\n").expect("seed");
        commit(&path, b"port: 2\n").expect("first");
        commit(&path, b"port: 3\n").expect("second");
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 3\n");
        assert_eq!(
            fs::read_to_string(backup_path(&path)).expect("backup"),
            "port: 2\n"
        );
        let names: Vec<String> = fs::read_dir(dir.path())
            .expect("list")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    // Not upstream's: contents that don't load are refused and nothing is
    // written.
    #[test]
    fn commit_refuses_what_does_not_load() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\n").expect("seed");
        let error = commit(&path, b"port: [\n").expect_err("refused");
        assert_eq!(error.kind(), SaveErrorKind::Check);
        assert!(
            error
                .to_string()
                .starts_with("failed to parse config file: yaml: "),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 1\n");
        assert!(!backup_path(&path).exists());
    }

    // Not upstream's: a new file is created without a backup.
    #[test]
    fn commit_creates_a_new_file() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        commit(&path, b"port: 2\n").expect("write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 2\n");
        assert!(!backup_path(&path).exists());
    }

    // Not upstream's: the backup and the new file keep the file's mode.
    #[cfg(unix)]
    #[test]
    fn commit_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\n").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");
        commit(&path, b"port: 2\n").expect("write");
        let mode = |p: &Path| fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o640);
        assert_eq!(mode(&backup_path(&path)), 0o640);
    }

    // Not upstream's: a symbolic link at the path is refused.
    #[cfg(unix)]
    #[test]
    fn commit_refuses_a_symlink() {
        let dir = TempDir::new();
        let target = dir.path().join("real.yaml");
        let path = dir.path().join("config.yaml");
        fs::write(&target, "port: 1\n").expect("seed");
        std::os::unix::fs::symlink(&target, &path).expect("link");
        let error = commit(&path, b"port: 2\n").expect_err("refused");
        assert_eq!(error.kind(), SaveErrorKind::Symlink);
        assert_eq!(fs::read_to_string(&target).expect("read"), "port: 1\n");
    }

    // Not upstream's: a directory at the path is refused.
    #[test]
    fn commit_refuses_a_directory() {
        let dir = TempDir::new();
        let error = commit(dir.path(), b"port: 2\n").expect_err("refused");
        assert_eq!(error.kind(), SaveErrorKind::Io);
    }

    // Not upstream's: only a rename the file refuses falls back to a write
    // in place; other failures are reported.
    #[test]
    fn only_a_refused_rename_writes_in_place() {
        for kind in [io::ErrorKind::ResourceBusy, io::ErrorKind::CrossesDevices] {
            assert!(rename_refused(&io::Error::from(kind)), "{kind:?}");
        }
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::NotFound,
            io::ErrorKind::ReadOnlyFilesystem,
            io::ErrorKind::Other,
        ] {
            assert!(!rename_refused(&io::Error::from(kind)), "{kind:?}");
        }
    }

    // Upstream's os.WriteFile: the write replaces the contents, shorter
    // ones included, and leaves no other file.
    #[test]
    fn write_in_place_replaces_the_contents() {
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\nhost: example\n").expect("seed");
        write_in_place(&path, b"port: 2\n").expect("write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "port: 2\n");
        assert_eq!(fs::read_dir(dir.path()).expect("list").count(), 1);
    }

    // Not upstream's: a write in place keeps the file, and so its mode, as a
    // bind mount needs; it creates no file.
    #[cfg(unix)]
    #[test]
    fn write_in_place_keeps_the_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "port: 1\n").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");
        let before = fs::metadata(&path).expect("stat");
        write_in_place(&path, b"port: 2\n").expect("write");
        let after = fs::metadata(&path).expect("stat");
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(after.permissions().mode() & 0o777, 0o640);
        let missing = dir.path().join("missing.yaml");
        let error = write_in_place(&missing, b"port: 2\n").expect_err("no file");
        assert_eq!(error.kind(), SaveErrorKind::Io);
        assert!(!missing.exists());
    }
}
