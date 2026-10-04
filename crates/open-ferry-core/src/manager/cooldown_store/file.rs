// Ported from CLIProxyAPI sdk/cliproxy/auth/cooldown_state.go
// (FileCooldownStateStore: Load, readCooldownStateFile, Save,
// writeCooldownStateGroup, removeStaleStateFiles, statePath,
// stateRelativePath, cdsPathForRel and sanitizeCooldownFileName)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `.cds` files in the auth directory: one per credential, beside its
//! auth file, holding its saved cooldowns.
//!
//! A credential's file is named after its auth file: `nested/xai.json`
//! under the auth directory saves to `nested/xai.cds`, a file outside it to
//! the directory's own top level, and a credential with no file (such as a
//! config API key) after its sanitized ID. A save writes every credential's
//! file through a temporary file and a rename, then removes every other
//! `.cds` file under the directory; with nothing to save it removes them
//! all. A load reads every `.cds` file, and one that doesn't parse fails
//! it; one that is over [`MAX_FILE_BYTES`], or holds more than
//! [`MAX_FILE_RECORDS`] records, is skipped with a warning, and a save then
//! replaces or removes it as it does any other.
//!
//! Nothing here deletes, renames over or writes anything but a `.cds` file:
//! removal checks the name first, and the temporary files are
//! `.<name>.<random>.tmp.cds`, which a load skips.
//!
//! Deviations from upstream:
//! - Temporary files end in `.tmp.cds` (upstream's end in `.tmp`), so that
//!   only `.cds` files are ever made or removed; a save removes one a crash
//!   left.
//! - Symbolic links are skipped, files and directories alike; upstream
//!   reads and removes a linked file.
//! - A file is never written or removed through a link: before either, each
//!   directory from the auth directory down to the file's is looked at
//!   without being followed (a symbolic link, or on Windows a junction, is
//!   refused), the missing ones are made one at a time, and the real path of
//!   the file's directory must lie under the auth directory's. A file whose
//!   directory fails this is skipped with a warning and the rest are saved;
//!   upstream follows the link and writes outside the directory. The auth
//!   directory itself may be a link.
//! - A file is read only up to [`MAX_FILE_BYTES`] and restores at most
//!   [`MAX_FILE_RECORDS`] records; upstream reads a file whole, of any size,
//!   and restores every record in it. A file over either is skipped with a
//!   warning and the rest are loaded.
//! - On Windows, paths that differ only in case are the same file, and a
//!   rename or removal the file system refuses for a moment (a sharing
//!   violation or access denied, as from a virus scanner) is tried three
//!   more times.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;

use super::record::{self, Record};
use super::{StateStore, StoreError};
use crate::auth::Timestamp;
use crate::auth::path::{clean, join, rel};
use crate::manager::lock;

/// The most a load reads of one `.cds` file: a credential's file holds a
/// record for each model it has cooling, a few hundred bytes each, so this
/// is well over any file the store writes.
pub(crate) const MAX_FILE_BYTES: u64 = 4 << 20;

/// The most records a load restores from one `.cds` file.
pub(crate) const MAX_FILE_RECORDS: usize = 10_000;

/// What a load reads of a file.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) bytes: u64,
    pub(crate) records: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: MAX_FILE_BYTES,
            records: MAX_FILE_RECORDS,
        }
    }
}

/// The store of `.cds` files under one auth directory (upstream's
/// `FileCooldownStateStore` with its directory and auth directory the
/// same).
pub(crate) struct FileStore {
    dir: PathBuf,
    /// Serializes saves.
    mu: Mutex<()>,
    limits: Limits,
}

impl FileStore {
    /// The store in `dir`, an absolute auth directory.
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            mu: Mutex::new(()),
            limits: Limits::default(),
        }
    }

    /// The store in `dir`, reading no more of a file than `limits`.
    #[cfg(test)]
    pub(crate) fn with_limits(dir: PathBuf, limits: Limits) -> Self {
        Self {
            limits,
            ..Self::new(dir)
        }
    }

    /// Where `record` is saved, or `None` when it names no file (upstream's
    /// `statePath`).
    pub(crate) fn state_path(&self, record: &Record) -> Option<PathBuf> {
        let rel = self.state_relative_path(record);
        if rel.as_os_str().is_empty() {
            return None;
        }
        Some(join(&self.dir, &rel))
    }

    /// `record`'s file relative to the directory, or empty (upstream's
    /// `stateRelativePath`).
    pub(crate) fn state_relative_path(&self, record: &Record) -> PathBuf {
        let auth_file = record.auth_file.trim();
        if auth_file.is_empty() {
            return PathBuf::from(sanitize(record.auth_id.trim()));
        }
        let path = Path::new(auth_file);
        if path.is_absolute() {
            if !self.dir.as_os_str().is_empty()
                && let Some(relative) = rel(&self.dir, path)
                && !escapes(&relative)
            {
                return cds_path_for_rel(&relative);
            }
            return PathBuf::from(sanitize(&go_base(auth_file)));
        }
        cds_path_for_rel(path)
    }

    /// Checks that `dir` is the auth directory or a real directory below it,
    /// with no link between them, making the directories that are missing
    /// when `create` is set. A link is where a write would leave the auth
    /// directory (upstream follows it).
    ///
    /// Each directory below the auth directory is looked at without following
    /// it, and made one at a time, so that a link can't be made through; the
    /// real path of `dir` must then lie under the real path of the auth
    /// directory, which also catches a link of a kind that isn't known to be
    /// one. The auth directory itself may be a link.
    fn check_dir(&self, dir: &Path, create: bool) -> io::Result<DirCheck> {
        let Some(relative) = rel(&self.dir, dir) else {
            return Ok(DirCheck::Refused(dir.to_path_buf()));
        };
        let mut current = self.dir.clone();
        for component in relative.components() {
            match component {
                Component::CurDir => continue,
                Component::Normal(part) => current.push(part),
                _ => return Ok(DirCheck::Refused(dir.to_path_buf())),
            }
            let mut found = inspect(&current)?;
            if found == Found::Missing {
                if !create {
                    return Ok(DirCheck::Missing);
                }
                match create_dir(&current) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(err) => return Err(err),
                }
                // Looked at again: it is what it is now, not what it was made
                // as.
                found = inspect(&current)?;
            }
            match found {
                Found::Directory => {}
                Found::Link => return Ok(DirCheck::Refused(current)),
                Found::Missing => return Ok(DirCheck::Missing),
                Found::Other => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        format!("{} is not a directory", current.display()),
                    ));
                }
            }
        }
        let real_dir = match fs::canonicalize(dir) {
            Ok(real_dir) => real_dir,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(DirCheck::Missing),
            Err(err) => return Err(err),
        };
        if real_dir.starts_with(fs::canonicalize(&self.dir)?) {
            Ok(DirCheck::Ready)
        } else {
            Ok(DirCheck::Refused(dir.to_path_buf()))
        }
    }

    fn write_group(&self, path: &Path, records: &mut [Record], now: Timestamp) -> io::Result<()> {
        records.sort_by(|a, b| a.model.as_bytes().cmp(b.model.as_bytes()));
        let data = record::encode_file(records, now);
        let dir = path.parent().unwrap_or(&self.dir);
        match self
            .check_dir(dir, true)
            .map_err(|err| context("create cooldown state directory", err))?
        {
            DirCheck::Ready => {}
            DirCheck::Refused(at) => {
                tracing::warn!(
                    path = %at.display(),
                    "not saving a cooldown state file through a link out of the auth directory"
                );
                return Ok(());
            }
            DirCheck::Missing => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "create cooldown state directory: the directory went away",
                ));
            }
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut temp = tempfile::Builder::new()
            .prefix(&format!(".{name}."))
            .suffix(TEMP_SUFFIX)
            .rand_bytes(8)
            .tempfile_in(dir)
            .map_err(|err| context("create cooldown state temp file", err))?;
        temp.write_all(data.as_bytes())
            .map_err(|err| context("write cooldown state temp file", err))?;
        let mut temp = temp.into_temp_path();
        let mut attempt = 0;
        loop {
            match temp.persist(path) {
                Ok(()) => return Ok(()),
                Err(err) if attempt < RETRIES && transient(&err.error) => {
                    temp = err.path;
                    std::thread::sleep(backoff(attempt));
                    attempt += 1;
                }
                // The temporary file is removed as `err.path` drops.
                Err(err) => return Err(context("replace cooldown state file", err.error)),
            }
        }
    }

    /// Removes every `.cds` file under the directory but those `desired`
    /// keys, or all of them (upstream's `removeStaleStateFiles`).
    fn remove_stale(&self, desired: Option<&HashSet<OsString>>) -> Result<(), StoreError> {
        walk(&self.dir, &mut |path, name| {
            if !is_cds(name) {
                return Ok(());
            }
            if desired.is_some_and(|desired| desired.contains(&key(&clean(path)))) {
                return Ok(());
            }
            // The walk didn't go through a link, but one may have been made
            // since, and a file's removal goes through its directory.
            match self.check_dir(path.parent().unwrap_or(&self.dir), false)? {
                DirCheck::Ready => {}
                DirCheck::Missing => return Ok(()),
                DirCheck::Refused(at) => {
                    tracing::warn!(
                        path = %at.display(),
                        "not removing a cooldown state file through a link out of the auth directory"
                    );
                    return Ok(());
                }
            }
            remove_cds(path).map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("remove stale cooldown state {}: {err}", path.display()),
                )
            })
        })
        .map_err(|err| StoreError(format!("clean cooldown state directory: {err}")))
    }
}

impl StateStore for FileStore {
    /// Reads every `.cds` file; a missing directory is empty (upstream's
    /// `Load`).
    fn load(&self) -> Result<Vec<Record>, StoreError> {
        let mut records = Vec::new();
        walk(&self.dir, &mut |path, name| {
            if !is_cds(name) || is_temp(name) {
                return Ok(());
            }
            records.extend(read_file(path, self.limits)?);
            Ok(())
        })
        .map_err(|err| StoreError(format!("read cooldown state directory: {err}")))?;
        Ok(records)
    }

    /// Writes one file per credential and removes the rest (upstream's
    /// `Save`).
    fn save(&self, records: &[Record], now: Timestamp) -> Result<(), StoreError> {
        let _guard = lock(&self.mu);
        let mut groups: BTreeMap<OsString, (PathBuf, Vec<Record>)> = BTreeMap::new();
        for record in records {
            if record.auth_id.trim().is_empty() {
                continue;
            }
            if !record::writable(record) {
                tracing::warn!(
                    auth_id = %record.auth_id,
                    "skipping a cooldown state record with a time outside years 0 to 9999"
                );
                continue;
            }
            let path = self.state_path(record).ok_or_else(|| {
                StoreError("cooldown state path: missing auth identity".to_owned())
            })?;
            groups
                .entry(key(&path))
                .or_insert_with(|| (path, Vec::new()))
                .1
                .push(record.clone());
        }
        if groups.is_empty() {
            return self.remove_stale(None);
        }
        create_dir_all(&self.dir)
            .map_err(|err| StoreError(format!("create cooldown state directory: {err}")))?;
        let mut desired = HashSet::with_capacity(groups.len());
        for (group_key, (path, mut group)) in groups {
            self.write_group(&path, &mut group, now)
                .map_err(|err| StoreError(err.to_string()))?;
            desired.insert(group_key);
        }
        self.remove_stale(Some(&desired))
    }
}

/// The end of a temporary file's name.
const TEMP_SUFFIX: &str = ".tmp.cds";

/// How many more times a refused rename or removal is tried.
const RETRIES: u32 = 3;

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(20u64 << attempt.min(4))
}

/// Whether `err` is a refusal Windows may lift in a moment: a sharing
/// violation (32) or access denied (5), as while a scanner holds the file.
fn transient(err: &io::Error) -> bool {
    cfg!(windows) && matches!(err.raw_os_error(), Some(5 | 32))
}

fn context(what: &str, err: io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("{what}: {err}"))
}

/// Whether `name` ends in `.cds`, whatever its case (upstream's
/// `strings.EqualFold(filepath.Ext(name), ".cds")`).
fn is_cds(name: &str) -> bool {
    name.len() >= 4
        && name
            .get(name.len() - 4..)
            .is_some_and(|ext| ext.eq_ignore_ascii_case(".cds"))
}

/// Whether `name` is one of the store's temporary files.
fn is_temp(name: &str) -> bool {
    name.starts_with('.')
        && name.len() >= TEMP_SUFFIX.len()
        && name
            .get(name.len() - TEMP_SUFFIX.len()..)
            .is_some_and(|end| end.eq_ignore_ascii_case(TEMP_SUFFIX))
}

/// Removes the `.cds` file at `path`, and refuses any other. A file
/// already gone is fine.
fn remove_cds(path: &Path) -> io::Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !is_cds(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to remove a file that isn't a .cds file",
        ));
    }
    let mut attempt = 0;
    loop {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) if attempt < RETRIES && transient(&err) => {
                std::thread::sleep(backoff(attempt));
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// A file's records; an empty or missing file has none (upstream's
/// `readCooldownStateFile`), and neither has one over `limits`, which is
/// skipped with a warning.
fn read_file(path: &Path, limits: Limits) -> io::Result<Vec<Record>> {
    let read_error = |err: io::Error| {
        io::Error::new(
            err.kind(),
            format!("read cooldown state {}: {err}", path.display()),
        )
    };
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(read_error(err)),
    };
    // One byte past the limit tells a file that is over it, however long the
    // file system says it is, and is as much as is ever read.
    let mut data = Vec::new();
    file.take(limits.bytes.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(read_error)?;
    if u64::try_from(data.len()).is_ok_and(|len| len > limits.bytes) {
        tracing::warn!(
            path = %path.display(),
            limit = limits.bytes,
            "skipping a cooldown state file over the size limit"
        );
        return Ok(Vec::new());
    }
    if data.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    let records = record::decode_file(&data).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parse cooldown state {}: {err}", path.display()),
        )
    })?;
    if records.len() > limits.records {
        tracing::warn!(
            path = %path.display(),
            records = records.len(),
            limit = limits.records,
            "skipping a cooldown state file with too many records"
        );
        return Ok(Vec::new());
    }
    Ok(records)
}

/// Calls `visit` with each regular file under `dir`, in name order, as Go's
/// `filepath.WalkDir` walks; symbolic links are skipped. A missing `dir` has
/// no files.
fn walk(dir: &Path, visit: &mut dyn FnMut(&Path, &str) -> io::Result<()>) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let mut entries = entries.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let path = entry.path();
        if file_type.is_dir() {
            walk(&path, visit)?;
        } else if file_type.is_file() {
            let name = entry.file_name();
            visit(&path, &name.to_string_lossy())?;
        }
    }
    Ok(())
}

/// `path` as a key naming its file: as it is, or on Windows, where names
/// differ only in case name the same file, in lower case.
fn key(path: &Path) -> OsString {
    if cfg!(windows) {
        OsString::from(path.to_string_lossy().to_lowercase())
    } else {
        path.as_os_str().to_owned()
    }
}

/// What [`FileStore::check_dir`] found.
enum DirCheck {
    /// A real directory in the auth directory, or the auth directory.
    Ready,
    /// Not there, and not made.
    Missing,
    /// The path of a link, or of a directory that is outside the auth
    /// directory in some other way.
    Refused(PathBuf),
}

/// What a path is, looked at without following it.
#[derive(PartialEq, Eq)]
enum Found {
    Missing,
    Directory,
    /// A symbolic link, or on Windows a junction (a reparse point that names
    /// another path, as Rust's `is_symlink` does).
    Link,
    Other,
}

fn inspect(path: &Path) -> io::Result<Found> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Ok(Found::Link),
        Ok(meta) if meta.is_dir() => Ok(Found::Directory),
        Ok(_) => Ok(Found::Other),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Found::Missing),
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn create_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir(dir)
}

#[cfg(unix)]
fn create_dir_all(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dir_all(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Whether a relative path leaves its base: `.`, `..` or under `..`.
fn escapes(path: &Path) -> bool {
    let mut components = path.components();
    match components.next() {
        None | Some(Component::CurDir | Component::ParentDir) => true,
        Some(_) => false,
    }
}

/// The `.cds` path for an auth file relative to the directory, or empty
/// when it leaves the directory (upstream's `cdsPathForRel`).
fn cds_path_for_rel(path: &Path) -> PathBuf {
    let cleaned = clean(path);
    if escapes(&cleaned)
        || cleaned.has_root()
        || cleaned
            .components()
            .any(|component| matches!(component, Component::Prefix(_)))
    {
        return PathBuf::new();
    }
    let base = sanitize(
        &cleaned
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    if base.is_empty() {
        return PathBuf::new();
    }
    match cleaned.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => join(parent, Path::new(&base)),
        _ => PathBuf::from(base),
    }
}

/// Go's `filepath.Base`: the last element, trailing separators dropped.
fn go_base(path: &str) -> String {
    let trimmed = path.trim_end_matches(is_separator);
    if trimmed.is_empty() {
        return if path.is_empty() {
            ".".to_owned()
        } else {
            std::path::MAIN_SEPARATOR_STR.to_owned()
        };
    }
    match trimmed.rfind(is_separator) {
        Some(i) => trimmed.get(i + 1..).unwrap_or_default().to_owned(),
        None => trimmed.to_owned(),
    }
}

fn is_separator(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

static UNSAFE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new("[^A-Za-z0-9._-]+").ok());

/// A file name made safe and given the `.cds` extension, or empty
/// (upstream's `sanitizeCooldownFileName`): the extension is dropped, runs
/// of anything but ASCII letters, digits, `.`, `_` and `-` become `_`, and
/// leading and trailing `.`, `_` and `-` go.
pub(crate) fn sanitize(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    let name = strip_ext(name);
    let replaced = match UNSAFE.as_ref() {
        Some(unsafe_chars) => unsafe_chars.replace_all(name, "_").into_owned(),
        None => name.to_owned(),
    };
    let trimmed = replaced.trim_matches(['.', '_', '-']);
    if trimmed.is_empty() {
        return String::new();
    }
    format!("{trimmed}.cds")
}

/// `name` without Go's `filepath.Ext`: from the last `.` after the last
/// separator.
fn strip_ext(name: &str) -> &str {
    for (i, c) in name.char_indices().rev() {
        if is_separator(c) {
            break;
        }
        if c == '.' {
            return name.get(..i).unwrap_or(name);
        }
    }
    name
}
