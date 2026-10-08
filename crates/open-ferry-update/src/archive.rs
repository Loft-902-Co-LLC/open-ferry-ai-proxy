//! Taking the binary out of a release archive, and nothing else.
//!
//! An archive holds a directory `open-ferry-<version>-<target>/` with the
//! binary, `open-ferry` or `open-ferry.exe`, beside the licence, the
//! readme and the example config. Only the binary is read; nothing is
//! written anywhere.
//!
//! The whole archive is refused when any entry:
//! - has an absolute path, a drive letter, a backslash, a NUL or a `..`
//!   part, or a name that isn't UTF-8;
//! - is a symbolic or hard link, or anything but a file or a directory;
//! - is over the size limit.
//!
//! It is also refused with more than [`MAX_ENTRIES`] entries, when a
//! `.tar.gz` expands to more than twice the limit, or when the binary is
//! missing or there twice. A leading `./` is allowed, and a tar's global
//! pax header is skipped.

use std::cell::Cell;
use std::fmt;
use std::io::{self, Cursor, Read};
use std::rc::Rc;

use crate::release::ArchiveKind;

/// The most entries an archive may have.
pub const MAX_ENTRIES: usize = 10_000;

/// Why the binary couldn't be taken out of an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchiveError {
    /// An entry's path is unsafe or isn't UTF-8.
    UnsafePath(String),
    /// An entry is a link or a special file.
    NotAFile(String),
    /// An entry, or the whole, is over the limit.
    TooLarge(String),
    /// More than [`MAX_ENTRIES`] entries.
    TooManyEntries,
    /// The binary isn't there.
    Missing(String),
    /// The binary is there twice.
    Repeated(String),
    /// The archive can't be read.
    Corrupt(String),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath(path) => write!(f, "the archive has an unsafe path: {path:?}"),
            Self::NotAFile(path) => {
                write!(f, "the archive has a link or special file: {path:?}")
            }
            Self::TooLarge(what) => write!(f, "the archive is too large: {what}"),
            Self::TooManyEntries => {
                write!(f, "the archive has more than {MAX_ENTRIES} entries")
            }
            Self::Missing(path) => write!(f, "the archive has no {path}"),
            Self::Repeated(path) => write!(f, "the archive has {path} twice"),
            Self::Corrupt(message) => write!(f, "the archive can't be read: {message}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

/// The binary `binary` in the directory `dir` of `archive`, of at most
/// `limit` bytes, after checking every entry.
pub fn extract_binary(
    archive: &[u8],
    kind: ArchiveKind,
    dir: &str,
    binary: &str,
    limit: u64,
) -> Result<Vec<u8>, ArchiveError> {
    let wanted = format!("{dir}/{binary}");
    match kind {
        ArchiveKind::TarGz => from_tar_gz(archive, &wanted, limit),
        ArchiveKind::Zip => from_zip(archive, &wanted, limit),
    }
}

/// An entry's path, checked, without a leading `./` or a trailing `/`.
fn checked_path(raw: &[u8]) -> Result<String, ArchiveError> {
    let lossy = || String::from_utf8_lossy(raw).into_owned();
    let path = std::str::from_utf8(raw).map_err(|_| ArchiveError::UnsafePath(lossy()))?;
    let unsafe_path = || ArchiveError::UnsafePath(path.to_owned());
    if path.is_empty() || path.starts_with('/') || path.contains(['\\', '\0']) {
        return Err(unsafe_path());
    }
    // No colon: it would be a drive letter (C:) or a stream on Windows.
    if path.contains(':') {
        return Err(unsafe_path());
    }
    let mut trimmed = path;
    while let Some(rest) = trimmed.strip_prefix("./") {
        trimmed = rest;
    }
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.split('/').any(|part| part == "..") {
        return Err(unsafe_path());
    }
    Ok(trimmed.to_owned())
}

/// Reads at most `limit` bytes of `reader`, refusing more.
fn read_limited(reader: impl Read, limit: u64, path: &str) -> Result<Vec<u8>, ArchiveError> {
    let mut data = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(|error| ArchiveError::Corrupt(error.to_string()))?;
    if u64::try_from(data.len()).unwrap_or(u64::MAX) > limit {
        return Err(ArchiveError::TooLarge(path.to_owned()));
    }
    Ok(data)
}

/// Keeps the binary if `path` is it, refusing a second.
fn keep(found: &mut Option<Vec<u8>>, data: Vec<u8>, path: &str) -> Result<(), ArchiveError> {
    if found.is_some() {
        return Err(ArchiveError::Repeated(path.to_owned()));
    }
    *found = Some(data);
    Ok(())
}

/// A reader that fails once more than its limit has been read, and
/// remembers that it did.
struct Capped<R> {
    inner: R,
    left: u64,
    over: Rc<Cell<bool>>,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        let read_u64 = u64::try_from(read).unwrap_or(u64::MAX);
        if read_u64 > self.left {
            self.over.set(true);
            return Err(io::Error::other("the archive expands past the limit"));
        }
        self.left -= read_u64;
        Ok(read)
    }
}

fn from_tar_gz(archive: &[u8], wanted: &str, limit: u64) -> Result<Vec<u8>, ArchiveError> {
    let over = Rc::new(Cell::new(false));
    let reader = Capped {
        inner: flate2::read::GzDecoder::new(archive),
        left: limit.saturating_mul(2),
        over: Rc::clone(&over),
    };
    let corrupt = |error: io::Error| {
        if over.get() {
            ArchiveError::TooLarge("it expands to more than twice the limit".into())
        } else {
            ArchiveError::Corrupt(error.to_string())
        }
    };
    let mut tar = tar::Archive::new(reader);
    let mut found = None;
    for (count, entry) in tar.entries().map_err(corrupt)?.enumerate() {
        if count >= MAX_ENTRIES {
            return Err(ArchiveError::TooManyEntries);
        }
        let entry = entry.map_err(corrupt)?;
        let kind = entry.header().entry_type();
        if kind == tar::EntryType::XGlobalHeader {
            continue;
        }
        let path = checked_path(&entry.path_bytes())?;
        let size = entry.size();
        match kind {
            tar::EntryType::Regular | tar::EntryType::Continuous => {}
            tar::EntryType::Directory => continue,
            _ => return Err(ArchiveError::NotAFile(path)),
        }
        if size > limit {
            return Err(ArchiveError::TooLarge(path));
        }
        if path == wanted {
            let data = read_limited(entry, limit, &path).map_err(|error| match error {
                ArchiveError::Corrupt(_) if over.get() => {
                    ArchiveError::TooLarge("it expands to more than twice the limit".into())
                }
                other => other,
            })?;
            keep(&mut found, data, &path)?;
        }
    }
    found.ok_or_else(|| ArchiveError::Missing(wanted.to_owned()))
}

/// The type bits of a Unix mode.
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

fn from_zip(archive: &[u8], wanted: &str, limit: u64) -> Result<Vec<u8>, ArchiveError> {
    let corrupt = |error: zip::result::ZipError| ArchiveError::Corrupt(error.to_string());
    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).map_err(corrupt)?;
    if zip.len() > MAX_ENTRIES {
        return Err(ArchiveError::TooManyEntries);
    }
    let mut found = None;
    for index in 0..zip.len() {
        let file = zip.by_index(index).map_err(corrupt)?;
        let path = checked_path(file.name_raw())?;
        // A mode's type, when the archive gives one, must be a file's or a
        // directory's.
        let type_bits = file.unix_mode().map_or(0, |mode| mode & S_IFMT);
        if file.is_symlink() || !matches!(type_bits, 0 | S_IFREG | S_IFDIR) {
            return Err(ArchiveError::NotAFile(path));
        }
        if file.is_dir() || type_bits == S_IFDIR {
            continue;
        }
        if file.size() > limit {
            return Err(ArchiveError::TooLarge(path));
        }
        if path == wanted {
            let data = read_limited(file, limit, &path)?;
            keep(&mut found, data, &path)?;
        }
    }
    found.ok_or_else(|| ArchiveError::Missing(wanted.to_owned()))
}
