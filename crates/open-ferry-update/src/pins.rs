//! Not upstream's: pin files, `versions/.pins/<id>.json`.
//!
//! A caller that wants a supervised switch stages a version, and under the
//! update lock writes a pin `{version, sha256, id, until}` here. It then
//! releases the lock and sends `switch {to, sha256, pin: id}`. The pin keeps
//! the version out of every prune until `until`. Nobody waits on the
//! supervisor while holding the update lock, so a pin is a file and not a
//! message.
//!
//! - Pins are created atomically with a no-replace create.
//! - Every keep-set computation includes every pin that has not expired.
//! - A pin file that doesn't parse makes [`PinStore::scan`] fail, which makes
//!   a prune refuse (fail closed).
//! - The supervisor consumes a pin by id once the journal records
//!   `switch.to`, and deletes expired pins when it prunes.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::journal::atomic_write;
use crate::publication::create_dir_all_durable;

/// The pins directory's name, inside `versions/`.
pub const PINS_DIR: &str = ".pins";

/// How long a pin keeps its version.
pub const PIN_TTL: Duration = Duration::from_secs(600);

/// A staged version a caller has asked to switch to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    pub version: String,
    pub sha256: String,
    pub id: String,
    /// When it expires, RFC 3339.
    pub until: String,
}

impl Pin {
    /// Whether the pin still holds its version at `now`. An `until` that
    /// doesn't parse holds it for good (a pin file like that fails the
    /// scan anyway).
    pub fn live_at(&self, now: DateTime<Utc>) -> bool {
        match DateTime::parse_from_rfc3339(&self.until) {
            Ok(until) => until.with_timezone(&Utc) > now,
            Err(_) => true,
        }
    }
}

/// Why pins could not be read.
#[derive(Debug)]
pub enum PinError {
    Io(io::Error),
    /// A pin file doesn't parse or isn't well-formed: prune refuses.
    Unparseable {
        path: PathBuf,
        reason: String,
    },
    /// An id that is not a plain file name.
    BadId(String),
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "the pin files couldn't be used: {error}"),
            Self::Unparseable { path, reason } => write!(
                f,
                "pin file {} can't be read ({reason}), so nothing is pruned",
                path.display()
            ),
            Self::BadId(id) => write!(f, "{id:?} is not a pin id"),
        }
    }
}

impl std::error::Error for PinError {}

impl From<io::Error> for PinError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The pins found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinScan {
    pub live: Vec<Pin>,
    pub expired: Vec<Pin>,
}

/// The pins directory.
#[derive(Clone, Debug)]
pub struct PinStore {
    dir: PathBuf,
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && !version.starts_with('.')
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

fn valid_sha256(sha256: &str) -> bool {
    sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl PinStore {
    /// The pins of the `versions` directory.
    pub fn new(versions_dir: &Path) -> Self {
        Self {
            dir: versions_dir.join(PINS_DIR),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Writes a pin for `version` that expires [`PIN_TTL`] after `now`, as a
    /// new file that replaces nothing.
    pub fn create(&self, version: &str, sha256: &str, now: DateTime<Utc>) -> io::Result<Pin> {
        if !valid_version(version) || !valid_sha256(sha256) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a pin needs a plain version and a sha256",
            ));
        }
        let until = now + chrono::Duration::from_std(PIN_TTL).map_err(io::Error::other)?;
        // Every directory made here (`versions`, `.pins`) has its parent
        // synced before a pin is acknowledged, or a crash could lose a pin a
        // prune was told about. The pin file and `.pins` are synced by the
        // write.
        let versions = self.dir().parent().unwrap_or(self.dir());
        let data_root = versions.parent().unwrap_or(versions);
        create_dir_all_durable(self.dir(), data_root)?;
        for _ in 0..16 {
            let id = format!(
                "{:x}-{:x}-{:x}",
                std::process::id(),
                now.timestamp_nanos_opt().unwrap_or_default(),
                ID_COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let pin = Pin {
                version: version.to_owned(),
                sha256: sha256.to_ascii_lowercase(),
                id,
                until: until.to_rfc3339_opts(SecondsFormat::Millis, true),
            };
            let mut data = serde_json::to_vec_pretty(&pin).map_err(io::Error::other)?;
            data.extend_from_slice(b"\n");
            match atomic_write(&self.file(&pin.id), &data, false) {
                Ok(()) => return Ok(pin),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free pin id",
        ))
    }

    fn read_file(&self, path: &Path) -> Result<Pin, PinError> {
        let unparseable = |reason: String| PinError::Unparseable {
            path: path.to_owned(),
            reason,
        };
        let data = fs::read(path)?;
        let pin: Pin =
            serde_json::from_slice(&data).map_err(|error| unparseable(error.to_string()))?;
        let stem = path.file_stem().and_then(|stem| stem.to_str());
        if stem != Some(pin.id.as_str()) {
            return Err(unparseable("its id is not its file name".to_owned()));
        }
        if !valid_id(&pin.id) || !valid_version(&pin.version) || !valid_sha256(&pin.sha256) {
            return Err(unparseable("a field is malformed".to_owned()));
        }
        if DateTime::parse_from_rfc3339(&pin.until).is_err() {
            return Err(unparseable("`until` is not a time".to_owned()));
        }
        Ok(pin)
    }

    /// Reads every pin file, split by `now`. A file that doesn't parse is an
    /// error: callers that would prune must stop. Temporary files and files
    /// that are not `*.json` are not pins.
    pub fn scan(&self, now: DateTime<Utc>) -> Result<PinScan, PinError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(PinScan::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut scan = PinScan::default();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            if !entry.file_type()?.is_file() {
                return Err(PinError::Unparseable {
                    path: entry.path(),
                    reason: "it is not a file".to_owned(),
                });
            }
            let pin = self.read_file(&entry.path())?;
            if pin.live_at(now) {
                scan.live.push(pin);
            } else {
                scan.expired.push(pin);
            }
        }
        scan.live.sort_by(|a, b| a.id.cmp(&b.id));
        scan.expired.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(scan)
    }

    /// The pins that hold their version at `now`.
    pub fn live(&self, now: DateTime<Utc>) -> Result<Vec<Pin>, PinError> {
        Ok(self.scan(now)?.live)
    }

    /// Reads and deletes the pin `id`: `None` when there is none. Call it
    /// once the journal records `switch.to`.
    pub fn consume(&self, id: &str) -> Result<Option<Pin>, PinError> {
        if !valid_id(id) {
            return Err(PinError::BadId(id.to_owned()));
        }
        let path = self.file(id);
        let pin = match self.read_file(&path) {
            Ok(pin) => pin,
            Err(PinError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        match fs::remove_file(&path) {
            Ok(()) => Ok(Some(pin)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Some(pin)),
            Err(error) => Err(error.into()),
        }
    }

    /// Deletes the pins that have expired at `now`; how many.
    pub fn remove_expired(&self, now: DateTime<Utc>) -> Result<usize, PinError> {
        let scan = self.scan(now)?;
        let mut removed = 0;
        for pin in &scan.expired {
            match fs::remove_file(self.file(&pin.id)) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn pins_and_their_directories_are_not_writable_by_others_with_umask_zero() {
        use crate::journal::creation_tests::{assert_mode, with_umask_zero};
        with_umask_zero(
            "pins::tests::pins_and_their_directories_are_not_writable_by_others_with_umask_zero",
            |root| {
                let versions = root.join("data/versions");
                let store = PinStore::new(&versions);
                let pin = store
                    .create("0.3.0", SHA, at("2026-10-09T10:00:00Z"))
                    .unwrap();
                for path in [root.join("data"), versions, store.dir().to_path_buf()] {
                    assert_mode(&path, 0o755);
                }
                assert_mode(&store.file(&pin.id), 0o644);
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn pins_and_their_directories_are_not_writable_by_others_with_default_acl() {
        use crate::journal::creation_tests::{assert_acl_mode, with_default_acl};
        with_default_acl(
            "pins::tests::pins_and_their_directories_are_not_writable_by_others_with_default_acl",
            |root| {
                let versions = root.join("data/versions");
                let store = PinStore::new(&versions);
                let pin = store
                    .create("0.3.0", SHA, at("2026-10-09T10:00:00Z"))
                    .unwrap();
                for path in [root.join("data"), versions, store.dir().to_path_buf()] {
                    assert_acl_mode(&path, 0o755);
                }
                assert_acl_mode(&store.file(&pin.id), 0o644);
            },
        );
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    const SHA: &str = "ab00000000000000000000000000000000000000000000000000000000000001";

    #[test]
    fn a_pin_is_created_read_and_consumed_once() {
        // Not upstream's: T3. The file is `<id>.json`, lives for PIN_TTL on
        // the caller's clock, and a consume removes it.
        let versions = tempfile::tempdir().unwrap();
        let store = PinStore::new(versions.path());
        let now = at("2026-10-09T10:00:00Z");
        let pin = store.create("0.3.0", SHA, now).unwrap();
        assert_eq!(pin.until, "2026-10-09T10:10:00.000Z");
        assert!(store.dir().join(format!("{}.json", pin.id)).is_file());
        assert_eq!(store.live(now).unwrap(), vec![pin.clone()]);
        assert_eq!(store.consume(&pin.id).unwrap(), Some(pin.clone()));
        assert_eq!(store.consume(&pin.id).unwrap(), None);
        assert!(store.live(now).unwrap().is_empty());
    }

    #[test]
    fn a_pin_creates_the_missing_directories_above_it() {
        // Not upstream's: P2. A data directory with no `versions` yet gets
        // `versions/.pins` through the durable directory helper.
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("data").join("versions");
        let store = PinStore::new(&versions);
        let now = at("2026-10-09T10:00:00Z");
        let pin = store.create("0.3.0", SHA, now).unwrap();
        assert!(store.dir().join(format!("{}.json", pin.id)).is_file());
        assert_eq!(store.live(now).unwrap(), vec![pin]);
    }

    #[test]
    fn two_pins_never_share_a_file() {
        // Not upstream's: the create is no-replace and ids differ even on one
        // clock reading.
        let versions = tempfile::tempdir().unwrap();
        let store = PinStore::new(versions.path());
        let now = at("2026-10-09T10:00:00Z");
        let a = store.create("0.3.0", SHA, now).unwrap();
        let b = store.create("0.3.0", SHA, now).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(store.live(now).unwrap().len(), 2);
        // The no-replace write itself.
        let data = b"x";
        let path = store.dir().join(format!("{}.json", a.id));
        let error = atomic_write(&path, data, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn pins_expire_by_the_clock() {
        // Not upstream's: a pin whose `until` has passed no longer holds
        // its version, and `remove_expired` deletes only those.
        let versions = tempfile::tempdir().unwrap();
        let store = PinStore::new(versions.path());
        let start = at("2026-10-09T10:00:00Z");
        let old = store.create("0.3.0", SHA, start).unwrap();
        let newer = store
            .create("0.4.0", SHA, start + chrono::Duration::minutes(8))
            .unwrap();
        let later = start + chrono::Duration::minutes(11);
        let scan = store.scan(later).unwrap();
        assert_eq!(scan.live, vec![newer.clone()]);
        assert_eq!(scan.expired, vec![old.clone()]);
        assert_eq!(store.remove_expired(later).unwrap(), 1);
        assert_eq!(store.scan(later).unwrap().expired.len(), 0);
        assert_eq!(store.live(later).unwrap(), vec![newer]);
    }

    #[test]
    fn a_pin_that_cannot_be_parsed_fails_closed() {
        // Not upstream's: T3. Junk, a wrong id and a bad field each make the
        // scan fail, which makes prune refuse. Temp files and other
        // extensions are not pins.
        let versions = tempfile::tempdir().unwrap();
        let store = PinStore::new(versions.path());
        let now = at("2026-10-09T10:00:00Z");
        assert_eq!(store.scan(now).unwrap(), PinScan::default());
        store.create("0.3.0", SHA, now).unwrap();
        fs::write(store.dir().join(".x.json.tmp"), b"half").unwrap();
        fs::write(store.dir().join("notes.txt"), b"hi").unwrap();
        assert_eq!(store.scan(now).unwrap().live.len(), 1);

        let bad = store.dir().join("junk.json");
        fs::write(&bad, b"{not json").unwrap();
        assert!(matches!(store.scan(now), Err(PinError::Unparseable { .. })));
        assert!(matches!(
            store.remove_expired(now),
            Err(PinError::Unparseable { .. })
        ));

        let body = |id: &str, version: &str, until: &str| {
            format!(r#"{{"version":"{version}","sha256":"{SHA}","id":"{id}","until":"{until}"}}"#)
        };
        for text in [
            body("other", "0.3.0", "2026-10-09T10:10:00Z"),
            body("junk", "../x", "2026-10-09T10:10:00Z"),
            body("junk", "0.3.0", "tomorrow"),
        ] {
            fs::write(&bad, text).unwrap();
            assert!(matches!(store.scan(now), Err(PinError::Unparseable { .. })));
        }
        fs::write(&bad, body("junk", "0.3.0", "2026-10-09T10:10:00Z")).unwrap();
        assert_eq!(store.scan(now).unwrap().live.len(), 2);
    }

    #[test]
    fn consume_refuses_a_path_and_a_broken_pin() {
        // Not upstream's: an id is a plain name, and a broken pin is an
        // error, not a silent "none".
        let versions = tempfile::tempdir().unwrap();
        let store = PinStore::new(versions.path());
        assert!(matches!(store.consume("../x"), Err(PinError::BadId(_))));
        assert!(matches!(store.consume(""), Err(PinError::BadId(_))));
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.dir().join("broken.json"), b"x").unwrap();
        assert!(matches!(
            store.consume("broken"),
            Err(PinError::Unparseable { .. })
        ));
    }

    #[test]
    fn create_refuses_a_malformed_version_or_hash() {
        // Not upstream's: nothing odd reaches a file name.
        let versions = tempfile::tempdir().unwrap();
        let store = PinStore::new(versions.path());
        let now = at("2026-10-09T10:00:00Z");
        assert!(store.create("../0.3.0", SHA, now).is_err());
        assert!(store.create("0.3.0", "abc", now).is_err());
        assert!(!store.dir().exists());
    }
}
