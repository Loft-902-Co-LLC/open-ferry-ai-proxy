//! Reading a release from its signed SHA256SUMS: which version it is, and
//! which archive is this target's.
//!
//! The names and the version are read as `install.sh` reads them: a line
//! of SHA256SUMS is `<hash> <name>`, split on spaces and tabs, the name
//! maybe marked `*` for binary mode; this target's archive is the first
//! name that starts with `open-ferry-`, ends with `-<target>.tar.gz`
//! (`-<target>.zip` for Windows) and has something between; the version is
//! what is between, `MAJOR.MINOR.PATCH` with an optional pre-release part.
//! Its hash is the first listed for the name, lowercased, and must be 64
//! hex digits.
//!
//! The signature's trusted comment must be `open-ferry <version>
//! SHA256SUMS`, and every archive SHA256SUMS lists must be of that
//! version, so a signed list can't be passed off as another release's.

use std::cmp::Ordering;
use std::fmt;

use semver::Version;

/// How every archive's name starts.
const PREFIX: &str = "open-ferry-";

/// A release's archive for one target, as its SHA256SUMS lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The version, as the archive's name has it.
    pub version: String,
    /// The version, parsed.
    pub semver: Version,
    /// The archive's file name.
    pub archive: String,
    /// The archive's SHA-256, 64 lowercase hex digits.
    pub sha256: String,
}

/// How an archive is packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `.tar.gz`, for every target but Windows.
    TarGz,
    /// `.zip`, for Windows.
    Zip,
}

impl ArchiveKind {
    /// The kind of `target`'s archives.
    pub fn of(target: &str) -> Self {
        if target.contains("-windows-") || target.ends_with("-windows") {
            Self::Zip
        } else {
            Self::TarGz
        }
    }

    /// The file name's ending.
    pub fn extension(self) -> &'static str {
        match self {
            Self::TarGz => ".tar.gz",
            Self::Zip => ".zip",
        }
    }
}

/// The binary's file name for `target`.
pub fn binary_name(target: &str) -> &'static str {
    match ArchiveKind::of(target) {
        ArchiveKind::Zip => "open-ferry.exe",
        ArchiveKind::TarGz => "open-ferry",
    }
}

/// Why a release couldn't be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReleaseError {
    /// SHA256SUMS isn't text.
    NotText,
    /// The trusted comment isn't `open-ferry <version> SHA256SUMS`.
    TrustedComment(String),
    /// An archive is of another version than the trusted comment names.
    OtherVersion { signed: String, archive: String },
    /// No archive for the target.
    NoArchive { version: String, target: String },
    /// The archive's version isn't one.
    BadVersion(String),
    /// The archive's hash isn't 64 hex digits, or is given twice
    /// differently.
    BadHash(String),
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotText => f.write_str("SHA256SUMS isn't text"),
            Self::TrustedComment(comment) => write!(
                f,
                "SHA256SUMS's signed comment is {comment:?}, not \"open-ferry <version> SHA256SUMS\""
            ),
            Self::OtherVersion { signed, archive } => write!(
                f,
                "SHA256SUMS is signed as open-ferry {signed} but lists {archive}"
            ),
            Self::NoArchive { version, target } => {
                write!(f, "open-ferry {version} has no archive for {target}")
            }
            Self::BadVersion(name) => {
                write!(f, "the release's archive has an unexpected name: {name}")
            }
            Self::BadHash(name) => write!(f, "SHA256SUMS lists no usable hash for {name}"),
        }
    }
}

impl std::error::Error for ReleaseError {}

/// The fields of a SHA256SUMS line as awk splits them: on runs of spaces
/// and tabs, leading ones skipped.
fn fields(line: &str) -> impl Iterator<Item = &str> {
    line.split([' ', '\t']).filter(|field| !field.is_empty())
}

/// Each line's hash and name, the name without a leading `*`.
fn entries(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.split('\n').map(|line| {
        let mut fields = fields(line);
        let hash = fields.next().unwrap_or_default();
        let name = fields.next().unwrap_or_default();
        (hash, name.strip_prefix('*').unwrap_or(name))
    })
}

/// Whether `text` is a version as `install.sh` takes one:
/// `MAJOR.MINOR.PATCH`, no leading zeros, and an optional `-` part of
/// letters, digits, dots and dashes.
pub fn valid_version(text: &str) -> bool {
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let number = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'))
    };
    let mut parts = core.split('.');
    let core_ok = (0..3).all(|_| parts.next().is_some_and(number)) && parts.next().is_none();
    let pre_ok = pre.is_none_or(|pre| {
        !pre.is_empty()
            && pre
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    });
    core_ok && pre_ok
}

/// The version a trusted comment names.
fn signed_version(comment: &str) -> Result<&str, ReleaseError> {
    comment
        .strip_prefix("open-ferry ")
        .and_then(|rest| rest.strip_suffix(" SHA256SUMS"))
        .filter(|version| valid_version(version))
        .ok_or_else(|| ReleaseError::TrustedComment(comment.to_owned()))
}

/// The release `sums` lists for `target`, checked against the trusted
/// comment of its signature.
pub fn read(sums: &[u8], trusted_comment: &str, target: &str) -> Result<Release, ReleaseError> {
    let text = std::str::from_utf8(sums).map_err(|_| ReleaseError::NotText)?;
    let signed = signed_version(trusted_comment)?;

    // Every archive must be of the signed version.
    let own_prefix = format!("{PREFIX}{signed}-");
    for (_, name) in entries(text) {
        let archive = name.ends_with(".tar.gz") || name.ends_with(".zip");
        if archive && name.starts_with(PREFIX) && !name.starts_with(&own_prefix) {
            return Err(ReleaseError::OtherVersion {
                signed: signed.to_owned(),
                archive: name.to_owned(),
            });
        }
    }

    // install.sh's pick: the first name with the prefix, the target's
    // suffix and something between.
    let suffix = format!("-{target}{}", ArchiveKind::of(target).extension());
    let archive = entries(text)
        .map(|(_, name)| name)
        .find(|name| {
            name.starts_with(PREFIX)
                && name.len().saturating_sub(suffix.len()) > PREFIX.len()
                && name.ends_with(&suffix)
        })
        .ok_or_else(|| ReleaseError::NoArchive {
            version: signed.to_owned(),
            target: target.to_owned(),
        })?;
    let version = archive
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(&suffix))
        .filter(|version| valid_version(version))
        .ok_or_else(|| ReleaseError::BadVersion(archive.to_owned()))?;
    if version != signed {
        return Err(ReleaseError::OtherVersion {
            signed: signed.to_owned(),
            archive: archive.to_owned(),
        });
    }
    let semver =
        Version::parse(version).map_err(|_| ReleaseError::BadVersion(archive.to_owned()))?;

    // install.sh's hash: the first line for the name. A second, different
    // one is refused rather than ignored.
    let mut hashes = entries(text)
        .filter(|(_, name)| *name == archive)
        .map(|(hash, _)| hash.to_ascii_lowercase());
    let sha256 = hashes
        .next()
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| ReleaseError::BadHash(archive.to_owned()))?;
    if hashes.any(|other| other != sha256) {
        return Err(ReleaseError::BadHash(archive.to_owned()));
    }
    Ok(Release {
        version: version.to_owned(),
        semver,
        archive: archive.to_owned(),
        sha256,
    })
}

/// How a release compares with the running version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A newer stable release: an update.
    Newer,
    /// The running version.
    Same,
    /// An older release: not an update, but a rollback may go to it.
    Older,
    /// A pre-release, which updates ignore.
    Prerelease,
}

impl Verdict {
    /// The verdict's name in the state file and the status.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Newer => "newer",
            Self::Same => "same",
            Self::Older => "older",
            Self::Prerelease => "prerelease",
        }
    }
}

/// How `latest` compares with `running` (a version that doesn't parse
/// counts as 0.0.0, older than any release).
pub fn compare(running: &str, latest: &Version) -> Verdict {
    if !latest.pre.is_empty() {
        return Verdict::Prerelease;
    }
    let running = Version::parse(running).unwrap_or_else(|_| Version::new(0, 0, 0));
    match latest.cmp_precedence(&running) {
        Ordering::Greater => Verdict::Newer,
        Ordering::Equal => Verdict::Same,
        Ordering::Less => Verdict::Older,
    }
}

/// The lowercase hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest: [u8; 32] = Sha256::digest(data).into();
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
