// Ported from CLIProxyAPI sdk/cliproxy/auth/cooldown_state_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The cooldown state store: the `.cds` files, and what the manager saves
//! and restores.
//!
//! Deviations from upstream:
//! - Saves are debounced on the store's worker, so the tests that count
//!   saves make the debounce long and call [`flush`] where upstream's
//!   manager would have saved; [`flush`] saves only after a change, and
//!   only what differs from what was last written.
//! - Upstream's `SetCooldownStateStore` is [`install_store`] then
//!   [`restore_now`]: nothing is saved to a store until it has been
//!   restored from.
//! - `ManagerSetConfigSnapshotDefersCooldownPersistence` and
//!   `ManagerSwapCooldownStateStorePersistsOldStoreBeforeSwap` use
//!   [`Manager::set_settings`], which never saves, and installing another
//!   store, which saves to the old one first.
//! - Dropped: `ManagerApplyConfigWithCooldownStoreSerializesTransitions`,
//!   `ManagerSwapCooldownStateStoreKeepsOldStoreWhenCanceled` and
//!   `ManagerResultSaveWaitsForCooldownStoreTransition`. There are no
//!   contexts to cancel, a result never saves on the caller's task, and the
//!   store's lock serializes its moves with its saves.
//!
//! [`flush`]: crate::manager::cooldown_store::flush

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use chrono::{TimeDelta, TimeZone, Utc};
use serde_json::json;

use super::support::*;
use crate::auth::{AuthError, QuotaState, Status, Timestamp};
use crate::manager::cooldown_store::{
    FileStore, Limits, MAX_FILE_BYTES, Record, StateStore, StoreError, flush, install_store,
    restore_now, sanitize, set_debounce, snapshot,
};
use crate::manager::{CallResult, Manager, Settings, lock};

/// A store that keeps what it was given (upstream's
/// `recordingCooldownStateStore`).
#[derive(Default)]
pub(super) struct RecordingStore {
    saves: AtomicUsize,
    records: Mutex<Vec<Record>>,
    load: Mutex<Vec<Record>>,
}

impl RecordingStore {
    pub(super) fn with_load(load: Vec<Record>) -> Arc<Self> {
        let store = Self::default();
        *lock(&store.load) = load;
        Arc::new(store)
    }

    fn saves(&self) -> usize {
        self.saves.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.saves.store(0, Ordering::SeqCst);
    }

    pub(super) fn saved(&self) -> Vec<Record> {
        lock(&self.records).clone()
    }
}

impl StateStore for RecordingStore {
    fn load(&self) -> Result<Vec<Record>, StoreError> {
        Ok(lock(&self.load).clone())
    }

    fn save(&self, records: &[Record], _now: Timestamp) -> Result<(), StoreError> {
        self.saves.fetch_add(1, Ordering::SeqCst);
        *lock(&self.records) = records.to_vec();
        Ok(())
    }
}

/// A manager with `store` installed and restored from, which saves only
/// when flushed.
fn manager_with(store: &Arc<RecordingStore>) -> Harness {
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(&h.manager, store.clone());
    restore_now(&h.manager);
    h
}

fn register(manager: &Manager, id: &str, provider: &str) {
    let mut auth = auth(id, provider);
    auth.status = Status::Active;
    manager.register_unsaved(auth).expect("register");
}

pub(super) fn failure(
    id: &str,
    provider: &str,
    model: &str,
    status: u16,
    message: &str,
) -> CallResult {
    CallResult {
        auth_id: id.into(),
        provider: provider.into(),
        model: model.into(),
        success: false,
        error: Some(AuthError {
            message: message.into(),
            http_status: status,
            ..AuthError::default()
        }),
        ..CallResult::default()
    }
}

pub(super) fn success(id: &str, provider: &str, model: &str) -> CallResult {
    CallResult {
        auth_id: id.into(),
        provider: provider.into(),
        model: model.into(),
        success: true,
        ..CallResult::default()
    }
}

fn at(h: u32, m: u32, s: u32) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 6, 1, h, m, s)
        .single()
        .expect("time")
}

pub(super) fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

/// The file names under `dir`, relative, with `/` between parts, sorted.
pub(super) fn files(dir: &Path) -> Vec<String> {
    fn visit(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).expect("under root");
                let parts: Vec<String> = rel
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push(parts.join("/"));
            }
        }
    }
    let mut out = Vec::new();
    visit(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn file_cooldown_state_store_state_relative_path() {
    let root = temp_dir();
    let auth_dir = root.path().join("auths");
    let outside = temp_dir();
    let store = FileStore::new(auth_dir.clone());
    let cases: [(&str, Record, PathBuf); 5] = [
        (
            "absolute auth file under auth dir",
            Record {
                auth_id: "auth-1".into(),
                auth_file: auth_dir
                    .join("nested")
                    .join("xai.json")
                    .to_string_lossy()
                    .into_owned(),
                ..Record::default()
            },
            Path::new("nested").join("xai.cds"),
        ),
        (
            "relative auth file",
            Record {
                auth_id: "auth-2".into(),
                auth_file: Path::new("team")
                    .join("xai.json")
                    .to_string_lossy()
                    .into_owned(),
                ..Record::default()
            },
            Path::new("team").join("xai.cds"),
        ),
        (
            "absolute auth file outside auth dir",
            Record {
                auth_id: "auth-3".into(),
                auth_file: outside
                    .path()
                    .join("outside.json")
                    .to_string_lossy()
                    .into_owned(),
                ..Record::default()
            },
            PathBuf::from("outside.cds"),
        ),
        (
            "relative parent escape is rejected",
            Record {
                auth_id: "auth-4".into(),
                auth_file: Path::new("..")
                    .join("escape.json")
                    .to_string_lossy()
                    .into_owned(),
                ..Record::default()
            },
            PathBuf::new(),
        ),
        (
            "auth id fallback",
            Record {
                auth_id: "auth/id 5".into(),
                ..Record::default()
            },
            PathBuf::from("auth_id_5.cds"),
        ),
    ];
    for (name, record, want) in cases {
        assert_eq!(store.state_relative_path(&record), want, "{name}");
    }
}

/// Not upstream's: the names upstream's `sanitizeCooldownFileName` gives.
#[test]
fn file_names_are_sanitized_as_upstream_sanitizes_them() {
    for (name, want) in [
        ("xai.json", "xai.cds"),
        ("a.b.json", "a.b.cds"),
        (" spaced name.json ", "spaced_name.cds"),
        (".hidden", ""),
        ("--x--", "x.cds"),
        ("gemini:apikey:abc", "gemini_apikey_abc.cds"),
        ("ünïcode", "n_code.cds"),
        ("", ""),
    ] {
        assert_eq!(sanitize(name), want, "{name:?}");
    }
}

#[test]
fn file_cooldown_state_store_save_load_and_clean_stale() {
    let dir = temp_dir();
    let auth_dir = dir.path().to_path_buf();
    let store = FileStore::new(auth_dir.clone());
    let stale = auth_dir.join("stale.cds");
    std::fs::write(&stale, "{}\n").expect("write stale");

    let next_retry = at(1, 0, 0);
    let record = Record {
        provider: "xai".into(),
        auth_id: "auth-1".into(),
        auth_file: auth_dir.join("xai.json").to_string_lossy().into_owned(),
        model: "grok-4".into(),
        status: "cooling".into(),
        next_retry_after: Some(next_retry),
        reason: "quota".into(),
        quota: QuotaState {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(next_retry),
            backoff_level: 1,
        },
        last_error: Some(AuthError {
            message: "rate limited".into(),
            http_status: 429,
            ..AuthError::default()
        }),
        updated_at: Some(at(0, 0, 0)),
    };

    store
        .save(std::slice::from_ref(&record), at(0, 0, 0))
        .expect("save");
    assert!(auth_dir.join("xai.cds").is_file(), "expected xai.cds");
    assert!(!stale.exists(), "expected stale.cds to be removed");

    let loaded = store.load().expect("load");
    assert_eq!(loaded.len(), 1);
    let first = &loaded[0];
    assert_eq!(first.auth_id, record.auth_id);
    assert_eq!(first.model, record.model);
    assert_eq!(first.next_retry_after, Some(next_retry));
    assert_eq!(
        first.last_error.as_ref().map(|err| err.http_status),
        Some(429)
    );
    // Everything but the auth file, which isn't written, reads back.
    assert_eq!(
        *first,
        Record {
            auth_file: String::new(),
            ..record
        }
    );

    store.save(&[], at(0, 0, 0)).expect("save nothing");
    assert!(
        !auth_dir.join("xai.cds").exists(),
        "expected xai.cds to be removed"
    );
}

#[test]
fn file_cooldown_state_store_concurrent_save() {
    let dir = temp_dir();
    let auth_dir = dir.path().to_path_buf();
    let store = Arc::new(FileStore::new(auth_dir.clone()));
    let next_retry = at(1, 0, 0);
    let threads: Vec<_> = (0..16)
        .map(|i| {
            let store = store.clone();
            let auth_file = auth_dir.join("xai.json").to_string_lossy().into_owned();
            std::thread::spawn(move || {
                store.save(
                    &[Record {
                        provider: "xai".into(),
                        auth_id: "auth-1".into(),
                        auth_file,
                        model: "grok-4".into(),
                        status: "cooling".into(),
                        next_retry_after: Some(next_retry + TimeDelta::seconds(i)),
                        updated_at: Some(next_retry),
                        ..Record::default()
                    }],
                    next_retry,
                )
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("join").expect("save");
    }
    let loaded = store.load().expect("load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(files(&auth_dir), ["xai.cds"], "leftover temporary files");
}

/// Not upstream's: a file is written as upstream's `MarshalIndent` writes
/// it (checked against Go 1.26.4), and reads back.
#[test]
fn a_file_is_written_as_upstream_writes_it() {
    let dir = temp_dir();
    let store = FileStore::new(dir.path().to_path_buf());
    let next = at(1, 0, 0);
    let records = [
        Record {
            provider: "xai".into(),
            auth_id: "auth-1".into(),
            model: "grok-4".into(),
            status: "cooling".into(),
            next_retry_after: Some(next),
            reason: "quota".into(),
            quota: QuotaState {
                exceeded: true,
                reason: "quota".into(),
                next_recover_at: Some(next),
                backoff_level: 1,
            },
            last_error: Some(AuthError {
                message: "rate limited <x>".into(),
                http_status: 429,
                ..AuthError::default()
            }),
            updated_at: Some(at(0, 0, 0) + TimeDelta::milliseconds(500)),
            ..Record::default()
        },
        Record {
            provider: "xai".into(),
            auth_id: "auth-1".into(),
            status: "cooling".into(),
            next_retry_after: Some(next),
            updated_at: Some(at(0, 0, 0)),
            ..Record::default()
        },
    ];
    store.save(&records, at(0, 0, 0)).expect("save");
    let written = std::fs::read_to_string(dir.path().join("auth-1.cds")).expect("read");
    let want = r#"{
  "version": 1,
  "auth_id": "auth-1",
  "provider": "xai",
  "updated_at": "2026-06-01T00:00:00Z",
  "records": [
    {
      "provider": "xai",
      "auth_id": "auth-1",
      "status": "cooling",
      "next_retry_after": "2026-06-01T01:00:00Z",
      "quota": {
        "exceeded": false,
        "next_recover_at": "0001-01-01T00:00:00Z",
        "observed_at": "0001-01-01T00:00:00Z"
      },
      "updated_at": "2026-06-01T00:00:00Z"
    },
    {
      "provider": "xai",
      "auth_id": "auth-1",
      "model": "grok-4",
      "status": "cooling",
      "next_retry_after": "2026-06-01T01:00:00Z",
      "reason": "quota",
      "quota": {
        "exceeded": true,
        "reason": "quota",
        "next_recover_at": "2026-06-01T01:00:00Z",
        "backoff_level": 1,
        "observed_at": "0001-01-01T00:00:00Z"
      },
      "last_error": {
        "message": "rate limited BSu003cxBSu003e",
        "retryable": false,
        "http_status": 429
      },
      "updated_at": "2026-06-01T00:00:00.5Z"
    }
  ]
}
"#
    .replace("BS", "\\");
    assert_eq!(written, want);
    let mut loaded = store.load().expect("load");
    loaded.sort_by(|a, b| a.model.cmp(&b.model));
    assert_eq!(loaded, [records[1].clone(), records[0].clone()]);
}

/// Not upstream's: files are read as Go's decoder reads them: any case of
/// a key, `null` as nothing, an empty file as no records, and a value of
/// the wrong type failing the load.
#[test]
fn files_are_read_as_go_reads_them() {
    let dir = temp_dir();
    let store = FileStore::new(dir.path().to_path_buf());
    std::fs::write(
        dir.path().join("a.cds"),
        r#"{"Records":[{"AUTH_ID":"a","next_retry_after":null,"Quota":{"Exceeded":true}},null]}"#,
    )
    .expect("write");
    std::fs::write(dir.path().join("b.CDS"), " \n").expect("write");
    std::fs::write(dir.path().join("c.cds"), "null").expect("write");
    let loaded = store.load().expect("load");
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].auth_id, "a");
    assert!(loaded[0].quota.exceeded);
    assert_eq!(loaded[1], Record::default());

    for bad in [
        r#"{"version":1.0}"#,
        "[]",
        r#"{"records":[{"next_retry_after":"soon"}]}"#,
        "{",
    ] {
        std::fs::write(dir.path().join("d.cds"), bad).expect("write");
        let err = store.load().expect_err(bad).to_string();
        assert!(
            err.starts_with("read cooldown state directory: parse cooldown state "),
            "{bad}: {err}"
        );
    }
}

/// Not upstream's, which reads a file whole and restores every record in
/// it: a file over the size limit, or with more records than the limit, is
/// skipped and the others are loaded; a save replaces or removes it.
#[test]
fn files_over_the_limits_are_skipped() {
    let dir = temp_dir();
    let root = dir.path();
    let records = [
        record_for(&root.join("small.json"), "m"),
        record_for(&root.join("three.json"), "m1"),
        record_for(&root.join("three.json"), "m2"),
        record_for(&root.join("three.json"), "m3"),
    ];
    FileStore::new(root.to_path_buf())
        .save(&records, at(0, 0, 0))
        .expect("save");
    let len = |name: &str| std::fs::metadata(root.join(name)).expect("meta").len();
    let (small, three) = (len("small.cds"), len("three.cds"));
    assert!(three > small);

    let load = |bytes: u64, records: usize| {
        let store = FileStore::with_limits(root.to_path_buf(), Limits { bytes, records });
        let mut ids: Vec<String> = store
            .load()
            .expect("load")
            .into_iter()
            .map(|record| format!("{}:{}", record.auth_id, record.model))
            .collect();
        ids.sort();
        ids
    };
    let all = [
        "small.json:m",
        "three.json:m1",
        "three.json:m2",
        "three.json:m3",
    ];
    assert_eq!(load(three, 3), all, "at the limits");
    assert_eq!(load(three, 2), ["small.json:m"], "too many records");
    assert_eq!(load(small, 3), ["small.json:m"], "too big");
    assert!(load(small - 1, 3).is_empty(), "both too big");

    let store = FileStore::with_limits(
        root.to_path_buf(),
        Limits {
            bytes: small,
            records: 3,
        },
    );
    store
        .save(&[record_for(&root.join("small.json"), "m")], at(0, 0, 0))
        .expect("save");
    assert_eq!(files(root), ["small.cds"], "the file left over is removed");
}

/// Not upstream's: at its own limit, a file is read only so far: one with a
/// record and then as many spaces as a file may have is skipped, though
/// all that is in it is a record.
#[test]
fn a_file_is_not_read_past_the_size_limit() {
    let dir = temp_dir();
    let root = dir.path();
    let store = FileStore::new(root.to_path_buf());
    store
        .save(&[record_for(&root.join("a.json"), "m")], at(0, 0, 0))
        .expect("save");
    let record = std::fs::read(root.join("a.cds")).expect("read");
    let pad = |name: &str, len: u64| {
        let mut data = record.clone();
        data.resize(usize::try_from(len).expect("len"), b' ');
        std::fs::write(root.join(name), data).expect("write");
    };
    pad("b.cds", MAX_FILE_BYTES);
    assert_eq!(store.load().expect("load").len(), 2, "at the limit");
    pad("b.cds", MAX_FILE_BYTES + 1);
    assert_eq!(store.load().expect("load").len(), 1, "a byte over");
    pad("b.cds", 3 * MAX_FILE_BYTES);
    assert_eq!(store.load().expect("load").len(), 1, "well over");
}

/// Not upstream's: a save removes `.cds` files and nothing else, under
/// the directory at any depth.
#[test]
fn a_save_removes_only_cds_files() {
    let dir = temp_dir();
    let root = dir.path();
    std::fs::create_dir_all(root.join("nested")).expect("mkdir");
    for name in [
        "a.json",
        "notes.txt",
        "cds",
        "x.cds.json",
        "nested/b.json",
        "old.cds",
        "UPPER.CDS",
        "nested/c.cds",
        ".a.cds.12345678.tmp.cds",
    ] {
        std::fs::write(root.join(name), "{}").expect("write");
    }
    let store = FileStore::new(root.to_path_buf());
    store.save(&[], at(0, 0, 0)).expect("save");
    assert_eq!(
        files(root),
        ["a.json", "cds", "nested/b.json", "notes.txt", "x.cds.json"]
    );
}

/// Not upstream's: a linked file or directory is neither read nor
/// removed.
#[test]
fn symbolic_links_are_skipped() {
    let dir = temp_dir();
    let target = temp_dir();
    std::fs::write(target.path().join("t.cds"), "not json").expect("write");
    std::fs::create_dir_all(target.path().join("sub")).expect("mkdir");
    std::fs::write(target.path().join("sub").join("u.cds"), "{}").expect("write");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(target.path().join("t.cds"), dir.path().join("l.cds"))
        .and_then(|()| std::os::unix::fs::symlink(target.path().join("sub"), dir.path().join("d")));
    #[cfg(windows)]
    let linked =
        std::os::windows::fs::symlink_file(target.path().join("t.cds"), dir.path().join("l.cds"))
            .and_then(|()| {
                std::os::windows::fs::symlink_dir(target.path().join("sub"), dir.path().join("d"))
            });
    match linked {
        Ok(()) => {}
        Err(err) if err.raw_os_error() == Some(1314) => {
            eprintln!("skipping: creating symbolic links needs a privilege: {err}");
            return;
        }
        Err(err) => panic!("symlink: {err}"),
    }
    let store = FileStore::new(dir.path().to_path_buf());
    assert!(store.load().expect("load").is_empty());
    store.save(&[], at(0, 0, 0)).expect("save");
    assert!(target.path().join("t.cds").exists());
    assert!(target.path().join("sub").join("u.cds").exists());
    assert!(dir.path().join("l.cds").symlink_metadata().is_ok());
}

/// The kinds of directory link a platform has.
#[derive(Clone, Copy, Debug)]
enum LinkKind {
    Symlink,
    /// A directory junction, which Windows makes without a privilege.
    #[cfg(windows)]
    Junction,
}

#[cfg(unix)]
const LINK_KINDS: [LinkKind; 1] = [LinkKind::Symlink];
#[cfg(windows)]
const LINK_KINDS: [LinkKind; 2] = [LinkKind::Junction, LinkKind::Symlink];

/// Makes `link`, a directory link to `target`, as `kind`.
fn link_dir(kind: LinkKind, target: &Path, link: &Path) -> std::io::Result<()> {
    match kind {
        #[cfg(unix)]
        LinkKind::Symlink => std::os::unix::fs::symlink(target, link),
        #[cfg(windows)]
        LinkKind::Symlink => std::os::windows::fs::symlink_dir(target, link),
        #[cfg(windows)]
        LinkKind::Junction => {
            let out = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()?;
            if out.status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(
                    String::from_utf8_lossy(&out.stdout).trim().to_owned(),
                ))
            }
        }
    }
}

/// Makes a link, or says why not, for a test to skip: making a symbolic
/// link on Windows takes a privilege.
fn try_link_dir(kind: LinkKind, target: &Path, link: &Path) -> bool {
    match link_dir(kind, target, link) {
        Ok(()) => true,
        Err(err) if err.raw_os_error() == Some(1314) => {
            eprintln!("skipping {kind:?}: creating symbolic links needs a privilege: {err}");
            false
        }
        Err(err) => panic!("{kind:?}: {err}"),
    }
}

fn record_for(auth_file: &Path, model: &str) -> Record {
    Record {
        provider: "xai".into(),
        auth_id: auth_file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        auth_file: auth_file.to_string_lossy().into_owned(),
        model: model.into(),
        status: "cooling".into(),
        next_retry_after: Some(at(1, 0, 0)),
        updated_at: Some(at(0, 0, 0)),
        ..Record::default()
    }
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Not upstream's: a file whose directory is, or is below, a link isn't
/// written through it, and what is behind the link isn't removed; upstream
/// writes outside the auth directory. The other files are saved.
#[test]
fn a_file_is_not_written_or_removed_through_a_link() {
    for kind in LINK_KINDS {
        let dir = temp_dir();
        let outside = temp_dir();
        let root = dir.path();
        std::fs::write(outside.path().join("old.cds"), "{}").expect("write");
        std::fs::create_dir(root.join("sub")).expect("mkdir");
        if !try_link_dir(kind, outside.path(), &root.join("linked"))
            || !try_link_dir(kind, outside.path(), &root.join("sub").join("deeper"))
        {
            continue;
        }
        let store = FileStore::new(root.to_path_buf());
        let records = [
            record_for(&root.join("linked").join("a.json"), "m"),
            record_for(&root.join("linked").join("new").join("b.json"), "m"),
            record_for(&root.join("sub").join("deeper").join("c.json"), "m"),
            record_for(&root.join("sub").join("d.json"), "m"),
            record_for(&root.join("plain").join("e.json"), "m"),
        ];
        store.save(&records, at(0, 0, 0)).expect("save");

        assert_eq!(entries(outside.path()), ["old.cds"], "{kind:?}");
        assert!(root.join("sub").join("d.cds").is_file(), "{kind:?}");
        assert!(root.join("plain").join("e.cds").is_file(), "{kind:?}");
        assert!(
            root.join("linked")
                .symlink_metadata()
                .expect("link")
                .is_symlink(),
            "{kind:?}"
        );

        store.save(&[], at(0, 0, 0)).expect("save nothing");
        assert_eq!(entries(outside.path()), ["old.cds"], "{kind:?}");
        assert!(!root.join("sub").join("d.cds").exists(), "{kind:?}");
        assert!(!root.join("plain").join("e.cds").exists(), "{kind:?}");
    }
}

/// Not upstream's: a link in the file's directory is refused at any depth,
/// including one that is made where a directory was to be.
#[test]
fn a_link_where_a_directory_is_made_is_refused() {
    for kind in LINK_KINDS {
        let dir = temp_dir();
        let outside = temp_dir();
        let root = dir.path();
        let store = FileStore::new(root.to_path_buf());
        let file = root.join("a").join("b").join("c.json");

        // The first save makes `a` and `b`; `b` is then replaced by a link.
        store
            .save(&[record_for(&file, "m")], at(0, 0, 0))
            .expect("save");
        assert!(root.join("a").join("b").join("c.cds").is_file());
        std::fs::remove_dir_all(root.join("a").join("b")).expect("remove");
        if !try_link_dir(kind, outside.path(), &root.join("a").join("b")) {
            continue;
        }
        store
            .save(&[record_for(&file, "m")], at(0, 0, 0))
            .expect("save through a link");
        assert!(entries(outside.path()).is_empty(), "{kind:?}");
    }
}

/// Not upstream's: the auth directory may itself be a link; only what is
/// below it may not.
#[test]
fn a_linked_auth_directory_is_used() {
    for kind in LINK_KINDS {
        let real = temp_dir();
        let parent = temp_dir();
        let link = parent.path().join("auth");
        if !try_link_dir(kind, real.path(), &link) {
            continue;
        }
        let store = FileStore::new(link.clone());
        let records = [
            record_for(&link.join("a.json"), "m"),
            record_for(&link.join("sub").join("b.json"), "m"),
        ];
        store.save(&records, at(0, 0, 0)).expect("save");
        assert!(real.path().join("a.cds").is_file(), "{kind:?}");
        assert!(real.path().join("sub").join("b.cds").is_file(), "{kind:?}");
        assert_eq!(store.load().expect("load").len(), 2, "{kind:?}");
        store.save(&[], at(0, 0, 0)).expect("save nothing");
        assert!(!real.path().join("a.cds").exists(), "{kind:?}");
        assert!(!real.path().join("sub").join("b.cds").exists(), "{kind:?}");
    }
}

/// Not upstream's: on Windows, file names that differ only in case are one
/// file, and the save doesn't remove the file it just wrote.
#[cfg(windows)]
#[test]
fn names_differing_in_case_are_one_file_on_windows() {
    let dir = temp_dir();
    let store = FileStore::new(dir.path().to_path_buf());
    let next = at(1, 0, 0);
    let records = [
        Record {
            auth_id: "Auth-A".into(),
            model: "m1".into(),
            next_retry_after: Some(next),
            ..Record::default()
        },
        Record {
            auth_id: "auth-a".into(),
            model: "m2".into(),
            next_retry_after: Some(next),
            ..Record::default()
        },
    ];
    store.save(&records, at(0, 0, 0)).expect("save");
    assert_eq!(files(dir.path()).len(), 1);
    assert_eq!(store.load().expect("load").len(), 2);
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_persists_cooldown_only_when_state_changes() {
    let store = Arc::new(RecordingStore::default());
    let h = manager_with(&store);
    register(&h.manager, "auth-1", "xai");

    h.manager.mark_result(&success("auth-1", "xai", "grok-4"));
    flush(&h.manager);
    assert_eq!(store.saves(), 0, "healthy success saved cooldown state");

    h.manager.mark_result(&failure(
        "auth-1",
        "xai",
        "grok-4",
        500,
        "upstream unavailable",
    ));
    flush(&h.manager);
    assert_eq!(store.saves(), 1, "cooldown failure");

    h.manager.mark_result(&success("auth-1", "xai", "grok-4"));
    flush(&h.manager);
    assert_eq!(store.saves(), 2, "cooldown clear");

    h.manager.mark_result(&success("auth-1", "xai", "grok-4"));
    flush(&h.manager);
    assert_eq!(store.saves(), 2, "clean success");
}

#[tokio::test(start_paused = true)]
async fn manager_update_clears_persisted_cooldown_when_credentials_change() {
    let store = Arc::new(RecordingStore::default());
    let h = manager_with(&store);
    let mut first = auth_with_metadata("auth-codex-1", "codex", json!({"access_token": "token-1"}));
    first.status = Status::Active;
    h.manager.register_unsaved(first).expect("register");

    // 1. Fail with 401.
    h.manager.mark_result(&failure(
        "auth-codex-1",
        "codex",
        "gpt-6-astra",
        401,
        "invalidated token",
    ));
    flush(&h.manager);
    assert!(
        !store.saved().is_empty(),
        "expected a cooldown record to be saved after the unauthorized failure"
    );

    // 2. An update that keeps the credentials (a metadata note).
    let mut same_cred = auth_with_metadata(
        "auth-codex-1",
        "codex",
        json!({"access_token": "token-1", "note": "updated note"}),
    );
    same_cred.status = Status::Active;
    h.manager.update_unsaved(same_cred).expect("update");
    flush(&h.manager);
    assert!(
        !store.saved().is_empty(),
        "expected the cooldown record to remain when credentials did not change"
    );

    // 3. An update with a new access token.
    let mut new_cred =
        auth_with_metadata("auth-codex-1", "codex", json!({"access_token": "token-2"}));
    new_cred.status = Status::Active;
    h.manager.update_unsaved(new_cred).expect("update");
    flush(&h.manager);
    let saved = store.saved();
    assert!(
        saved.is_empty(),
        "expected cooldown records to be cleared after credential change, got {saved:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_set_config_snapshot_defers_cooldown_persistence() {
    let store = Arc::new(RecordingStore::default());
    let h = manager_with(&store);
    register(&h.manager, "auth-1", "xai");
    h.manager
        .mark_result(&failure("auth-1", "xai", "grok-4", 429, "rate limited"));
    flush(&h.manager);
    store.reset();

    h.manager.set_settings(Settings {
        disable_cooling: true,
        ..Settings::default()
    });
    assert_eq!(store.saves(), 0, "set_settings saved cooldown state");
    flush(&h.manager);
    assert_eq!(store.saves(), 1, "flush");
    assert!(store.saved().is_empty());
}

#[tokio::test(start_paused = true)]
async fn manager_swap_cooldown_state_store_persists_old_store_before_swap() {
    let old_store = Arc::new(RecordingStore::default());
    let new_store = Arc::new(RecordingStore::default());
    let h = manager_with(&old_store);
    register(&h.manager, "auth-1", "xai");
    h.manager
        .mark_result(&failure("auth-1", "xai", "grok-4", 429, "rate limited"));
    flush(&h.manager);
    old_store.reset();
    h.manager.set_settings(Settings {
        disable_cooling: true,
        ..Settings::default()
    });

    install_store(&h.manager, new_store.clone());
    assert_eq!(old_store.saves(), 1, "old store save count");
    assert!(old_store.saved().is_empty(), "old store records");
    assert_eq!(
        new_store.saves(),
        0,
        "nothing goes to the new store before a restore"
    );
    restore_now(&h.manager);
    h.manager.mark_result(&success("auth-1", "xai", "other"));
    flush(&h.manager);
    assert_eq!(old_store.saves(), 1, "the old store is left alone");
}

#[tokio::test(start_paused = true)]
async fn manager_restore_cooldown_states() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let next_retry = now + TimeDelta::hours(1);
    let store = RecordingStore::with_load(vec![Record {
        provider: "xai".into(),
        auth_id: "auth-1".into(),
        model: "grok-4".into(),
        status: "cooling".into(),
        next_retry_after: Some(next_retry),
        reason: "quota".into(),
        quota: QuotaState {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(next_retry),
            ..QuotaState::default()
        },
        last_error: Some(AuthError {
            message: "rate limited".into(),
            http_status: 429,
            ..AuthError::default()
        }),
        updated_at: Some(next_retry - TimeDelta::minutes(1)),
        ..Record::default()
    }]);
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(&h.manager, store.clone());
    h.manager
        .register_unsaved(auth("auth-1", "xai"))
        .expect("register");

    restore_now(&h.manager);

    let auth = h.get("auth-1");
    let state = auth
        .model_states
        .get("grok-4")
        .expect("model state restored");
    assert!(state.unavailable);
    assert_eq!(state.status, Status::Error);
    assert_eq!(state.next_retry_after, Some(next_retry));
    assert_eq!(
        state.last_error.as_ref().map(|err| err.http_status),
        Some(429)
    );
    assert_eq!(store.saves(), 1, "restore cleanup saves");
}

#[tokio::test(start_paused = true)]
async fn manager_restore_cooldown_states_canonicalizes_thinking_suffixes() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let later_retry = now + TimeDelta::hours(2);
    let quota = |until: Timestamp| QuotaState {
        exceeded: true,
        reason: "quota".into(),
        next_recover_at: Some(until),
        ..QuotaState::default()
    };
    let store = RecordingStore::with_load(vec![
        Record {
            provider: "gemini".into(),
            auth_id: "auth-thinking".into(),
            model: "gemini-3.1-pro-preview(high)".into(),
            next_retry_after: Some(now + TimeDelta::hours(1)),
            quota: quota(now + TimeDelta::hours(1)),
            updated_at: Some(now),
            ..Record::default()
        },
        Record {
            provider: "gemini".into(),
            auth_id: "auth-thinking".into(),
            model: "gemini-3.1-pro-preview(low)".into(),
            next_retry_after: Some(later_retry),
            quota: quota(later_retry),
            updated_at: Some(now + TimeDelta::minutes(1)),
            ..Record::default()
        },
    ]);
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(&h.manager, store.clone());
    h.manager
        .register_unsaved(auth("auth-thinking", "gemini"))
        .expect("register");

    restore_now(&h.manager);

    let auth = h.get("auth-thinking");
    assert_eq!(auth.model_states.len(), 1, "{:?}", auth.model_states);
    let state = auth
        .model_states
        .get("gemini-3.1-pro-preview")
        .expect("canonical model state");
    assert!(state.unavailable);
    assert_eq!(state.next_retry_after, Some(later_retry));

    let models: Vec<Record> = store
        .saved()
        .into_iter()
        .filter(|record| !record.model.is_empty())
        .collect();
    assert_eq!(models.len(), 1, "{models:?}");
    assert_eq!(models[0].model, "gemini-3.1-pro-preview");
    assert_eq!(models[0].next_retry_after, Some(later_retry));
}

/// Not upstream's: a restore skips records that ran out, and those of
/// credentials that are unknown, disabled or don't cool down; a
/// credential-wide record comes back with its quota, reason and error.
#[tokio::test(start_paused = true)]
async fn restore_skips_what_upstream_skips() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let until = now + TimeDelta::hours(1);
    let record = |id: &str, next: Timestamp| Record {
        provider: "xai".into(),
        auth_id: id.into(),
        status: "cooling".into(),
        next_retry_after: Some(next),
        reason: "quota".into(),
        quota: QuotaState {
            exceeded: true,
            reason: "quota".into(),
            backoff_level: 2,
            ..QuotaState::default()
        },
        last_error: Some(AuthError {
            message: "limit".into(),
            http_status: 429,
            ..AuthError::default()
        }),
        ..Record::default()
    };
    let store = RecordingStore::with_load(vec![
        record("live", until),
        record("expired", now - TimeDelta::seconds(1)),
        record("unknown", until),
        record("disabled", until),
        record("no-cooling", until),
        Record {
            auth_id: " ".into(),
            ..record("", until)
        },
    ]);
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(&h.manager, store.clone());
    for id in ["live", "expired"] {
        register(&h.manager, id, "xai");
    }
    let mut disabled = auth("disabled", "xai");
    disabled.disabled = true;
    disabled.status = Status::Disabled;
    h.manager.register_unsaved(disabled).expect("register");
    let mut no_cooling = auth("no-cooling", "xai");
    no_cooling
        .metadata
        .insert("disable_cooling".into(), json!(true));
    h.manager.register_unsaved(no_cooling).expect("register");

    restore_now(&h.manager);

    let live = h.get("live");
    assert!(live.unavailable);
    assert_eq!(live.status, Status::Error);
    assert_eq!(live.next_retry_after, Some(until));
    assert_eq!(live.status_message, "quota");
    assert!(live.quota.exceeded);
    assert_eq!(
        live.quota.next_recover_at,
        Some(until),
        "defaults to the retry"
    );
    assert_eq!(live.quota.backoff_level, 2);
    assert_eq!(
        live.last_error.as_ref().map(|err| err.http_status),
        Some(429)
    );
    for id in ["expired", "disabled", "no-cooling"] {
        let auth = h.get(id);
        assert!(!auth.unavailable, "{id}");
        assert!(auth.next_retry_after.is_none(), "{id}");
    }
    let saved: Vec<String> = store.saved().into_iter().map(|r| r.auth_id).collect();
    assert_eq!(saved, ["live"]);
}

/// A saved record for `auth-1` on `model` (credential-wide if empty), that
/// is to run out at `until` and was written at `written`.
fn saved_cooldown(model: &str, until: Timestamp, written: Timestamp) -> Record {
    Record {
        provider: "xai".into(),
        auth_id: "auth-1".into(),
        model: model.into(),
        status: "cooling".into(),
        next_retry_after: Some(until),
        reason: "saved".into(),
        last_error: Some(AuthError {
            message: "saved".into(),
            http_status: 429,
            ..AuthError::default()
        }),
        updated_at: Some(written),
        ..Record::default()
    }
}

/// Not upstream's, which applies a credential-wide record whatever the
/// credential holds: a fresh cooldown keeps its deadline, its time and its
/// error against an older record that would run out sooner.
#[tokio::test(start_paused = true)]
async fn a_restore_keeps_a_fresher_credential_wide_cooldown() {
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    register(&h.manager, "auth-1", "xai");
    h.manager
        .mark_result(&failure("auth-1", "xai", "", 401, "bad token"));
    let fresh = h.get("auth-1");
    let now = h.now();
    assert_eq!(fresh.next_retry_after, Some(now + TimeDelta::minutes(30)));
    let store = RecordingStore::with_load(vec![saved_cooldown(
        "",
        now + TimeDelta::minutes(5),
        now - TimeDelta::hours(1),
    )]);
    install_store(&h.manager, store.clone());

    restore_now(&h.manager);

    let auth = h.get("auth-1");
    assert_eq!(auth.next_retry_after, fresh.next_retry_after, "deadline");
    assert_eq!(auth.updated_at, fresh.updated_at, "time");
    assert_eq!(auth.status_message, "unauthorized");
    assert_eq!(
        auth.last_error.as_ref().map(|err| err.http_status),
        Some(401)
    );
    assert_eq!(auth.generation, fresh.generation, "nothing changed");
}

/// Not upstream's: a credential-wide record newer than the cooldown the
/// credential holds replaces it, and one that is for a credential with
/// nothing is applied whenever it was written.
#[tokio::test(start_paused = true)]
async fn a_restore_applies_a_credential_wide_record_that_is_newer() {
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    register(&h.manager, "auth-1", "xai");
    register(&h.manager, "auth-2", "xai");
    h.manager
        .mark_result(&failure("auth-1", "xai", "", 401, "bad token"));
    h.clock.advance(Duration::from_secs(600));
    let now = h.now();
    let until = now + TimeDelta::minutes(5);
    let store = RecordingStore::with_load(vec![
        saved_cooldown("", until, now - TimeDelta::minutes(1)),
        Record {
            auth_id: "auth-2".into(),
            ..saved_cooldown("", until, now - TimeDelta::days(30))
        },
    ]);
    install_store(&h.manager, store.clone());

    restore_now(&h.manager);

    let newer = h.get("auth-1");
    assert_eq!(newer.next_retry_after, Some(until), "newer replaces");
    assert_eq!(newer.updated_at, Some(now - TimeDelta::minutes(1)));
    assert_eq!(newer.status_message, "saved");
    let empty = h.get("auth-2");
    assert_eq!(empty.next_retry_after, Some(until), "old but nothing held");
    assert!(empty.unavailable);
}

/// Not upstream's: a model's cooldown that a later success cleared isn't
/// brought back by an older record; a model with no state, and one the
/// record is newer than, get theirs.
#[tokio::test(start_paused = true)]
async fn a_restore_does_not_bring_back_a_cooldown_a_later_result_cleared() {
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    register(&h.manager, "auth-1", "xai");
    for model in ["cleared", "cleared-since"] {
        h.manager
            .mark_result(&failure("auth-1", "xai", model, 429, "rate limited"));
        h.manager.mark_result(&success("auth-1", "xai", model));
    }
    let now = h.now();
    let until = now + TimeDelta::minutes(5);
    let store = RecordingStore::with_load(vec![
        saved_cooldown("cleared", until, now - TimeDelta::hours(1)),
        saved_cooldown("cleared-since", until, now + TimeDelta::minutes(1)),
        saved_cooldown("unseen", until, now - TimeDelta::hours(1)),
    ]);
    install_store(&h.manager, store.clone());

    restore_now(&h.manager);

    let auth = h.get("auth-1");
    let state = |model: &str| auth.model_states.get(model).expect(model);
    assert!(!state("cleared").unavailable, "cleared stays cleared");
    assert!(state("cleared").next_retry_after.is_none());
    assert_eq!(state("cleared-since").next_retry_after, Some(until));
    assert_eq!(state("unseen").next_retry_after, Some(until));
}

/// Not upstream's: an empty load saves nothing, and nothing is saved to a
/// store before it has been restored from.
#[tokio::test(start_paused = true)]
async fn nothing_is_saved_before_the_restore() {
    let store = Arc::new(RecordingStore::default());
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    register(&h.manager, "auth-1", "xai");
    install_store(&h.manager, store.clone());
    h.manager
        .mark_result(&failure("auth-1", "xai", "grok-4", 429, "rate limited"));
    flush(&h.manager);
    assert_eq!(store.saves(), 0, "saved before the restore");

    restore_now(&h.manager);
    assert_eq!(store.saves(), 0, "an empty load saves nothing");
    h.manager.mark_result(&success("auth-1", "xai", "other"));
    flush(&h.manager);
    assert_eq!(store.saves(), 1, "the next change saves the cooldown");
    assert_eq!(store.saved().len(), 1);
}

/// Not upstream's: the worker saves on its own once the debounce has
/// passed, all the changes in it at once.
#[test]
fn the_worker_saves_after_the_debounce() {
    let store = Arc::new(RecordingStore::default());
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_millis(200));
    install_store(&h.manager, store.clone());
    restore_now(&h.manager);
    register(&h.manager, "auth-1", "xai");
    h.manager
        .mark_result(&failure("auth-1", "xai", "m1", 429, "rate limited"));
    h.manager
        .mark_result(&failure("auth-1", "xai", "m2", 429, "rate limited"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while store.saves() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(store.saves(), 1);
    let models: Vec<String> = store
        .saved()
        .into_iter()
        .map(|record| record.model)
        .filter(|model| !model.is_empty())
        .collect();
    assert_eq!(models, ["m1", "m2"]);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(store.saves(), 1, "no change, no save");
}

/// A store whose first saves fail, and that remembers when each was tried.
struct FlakyStore {
    fail: Mutex<usize>,
    attempts: Mutex<Vec<Instant>>,
    saved: AtomicUsize,
}

impl FlakyStore {
    fn failing(times: usize) -> Arc<Self> {
        Arc::new(Self {
            fail: Mutex::new(times),
            attempts: Mutex::new(Vec::new()),
            saved: AtomicUsize::new(0),
        })
    }

    fn attempts(&self) -> Vec<Instant> {
        lock(&self.attempts).clone()
    }

    fn saved(&self) -> usize {
        self.saved.load(Ordering::SeqCst)
    }
}

impl StateStore for FlakyStore {
    fn load(&self) -> Result<Vec<Record>, StoreError> {
        Ok(Vec::new())
    }

    fn save(&self, _records: &[Record], _now: Timestamp) -> Result<(), StoreError> {
        lock(&self.attempts).push(Instant::now());
        let failing = {
            let mut left = lock(&self.fail);
            let failing = *left > 0;
            *left = left.saturating_sub(1);
            failing
        };
        if failing {
            return Err(StoreError("the disk is full".to_owned()));
        }
        self.saved.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Not upstream's: a save that failed is made again by the next flush, with
/// nothing changed since, and a save that worked is not made again.
#[test]
fn a_failed_save_is_tried_again_by_the_next_flush() {
    let store = FlakyStore::failing(1);
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(&h.manager, store.clone());
    restore_now(&h.manager);
    register(&h.manager, "auth-1", "xai");
    h.manager
        .mark_result(&failure("auth-1", "xai", "m1", 429, "rate limited"));

    flush(&h.manager);
    assert_eq!((store.attempts().len(), store.saved()), (1, 0), "fails");
    flush(&h.manager);
    assert_eq!((store.attempts().len(), store.saved()), (2, 1), "retried");
    flush(&h.manager);
    assert_eq!(store.attempts().len(), 2, "saved, so left alone");
}

/// Not upstream's, and a case where upstream's explicit save would retry: a
/// `.cds` file that can't be made is made once the obstruction is gone, by
/// the flush at shutdown, with no other change.
#[test]
fn a_cooldown_that_could_not_be_written_is_written_at_shutdown() {
    let dir = temp_dir();
    let obstruction = dir.path().join("review-auth.cds");
    std::fs::create_dir(&obstruction).expect("obstruction");
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(
        &h.manager,
        Arc::new(FileStore::new(dir.path().to_path_buf())),
    );
    restore_now(&h.manager);
    register(&h.manager, "review-auth", "xai");
    h.manager
        .mark_result(&failure("review-auth", "xai", "m1", 429, "rate limited"));

    flush(&h.manager);
    assert!(obstruction.is_dir(), "still in the way");

    std::fs::remove_dir(&obstruction).expect("remove the obstruction");
    flush(&h.manager);
    assert!(obstruction.is_file(), "written by the flush at shutdown");
    let records = FileStore::new(dir.path().to_path_buf())
        .load()
        .expect("load");
    assert_eq!(
        records.iter().filter(|record| record.model == "m1").count(),
        1
    );
}

/// Not upstream's, which writes what an upstream answered as it came: an
/// OpenAI-compatible credential with a `Cookie` header, whose upstream
/// answers 401 and quotes the cookie, leaves no copy of it in the file.
#[test]
fn a_cooldown_file_keeps_no_cookie_the_upstream_echoed() {
    let dir = temp_dir();
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(
        &h.manager,
        Arc::new(FileStore::new(dir.path().to_path_buf())),
    );
    restore_now(&h.manager);
    let mut review = auth("review-auth", "openai-compatibility");
    review.status = Status::Active;
    review.file_name = "review-auth.json".into();
    review.attributes.insert(
        "header:Cookie".into(),
        "session=synthetic-cookie-secret; theme=dark".into(),
    );
    h.manager.register_unsaved(review).expect("register");
    h.manager.mark_result(&failure(
        "review-auth",
        "openai-compatibility",
        "m1",
        401,
        r#"{"error":{"message":"Bad cookie: session=synthetic-cookie-secret"}}"#,
    ));

    flush(&h.manager);
    let text = files(dir.path())
        .iter()
        .map(|name| std::fs::read_to_string(dir.path().join(name)).expect("read"))
        .collect::<String>();
    assert!(text.contains("Bad cookie: session=[redacted]"), "{text}");
    assert!(!text.contains("synthetic-cookie-secret"), "{text}");
    assert!(!text.contains("dark"), "every cookie value goes: {text}");
    let records = FileStore::new(dir.path().to_path_buf())
        .load()
        .expect("load");
    assert!(records.iter().any(|record| record.model.is_empty()));
    assert!(records.iter().any(|record| record.model == "m1"));
}

/// Not upstream's: every free-text field of the credential's record and of
/// a model's, and every secret however short, is scrubbed, and another
/// credential's secret is not.
#[tokio::test(start_paused = true)]
async fn every_free_text_field_of_a_saved_record_is_scrubbed() {
    let h = Harness::new(Settings::default());
    let until = h.now() + TimeDelta::minutes(30);
    let echoing = |what: &str| AuthError {
        code: format!("bad {what} k-1"),
        message: format!("rejected k-1 and {what} tok-9"),
        http_status: 401,
        ..AuthError::default()
    };
    let quota = QuotaState {
        exceeded: true,
        reason: "quota for k-1".into(),
        next_recover_at: Some(until),
        backoff_level: 1,
    };
    let mut first = auth("auth-1", "xai");
    first.status = Status::Active;
    first.attributes.insert("api_key".into(), "k-1".into());
    first.metadata = json!({"access_token": "tok-9"})
        .as_object()
        .cloned()
        .unwrap_or_default();
    first.unavailable = true;
    first.next_retry_after = Some(until);
    first.status_message = "credential k-1".into();
    first.quota = quota.clone();
    first.last_error = Some(echoing("credential"));
    first.model_states.insert(
        "m1".into(),
        crate::auth::ModelState {
            status: Status::Error,
            status_message: "model tok-9".into(),
            unavailable: true,
            next_retry_after: Some(until),
            last_error: Some(echoing("model")),
            quota,
            updated_at: Some(h.now()),
        },
    );
    h.manager.register_unsaved(first).expect("register");
    let mut second = auth("auth-2", "xai");
    second.status = Status::Active;
    second.unavailable = true;
    second.next_retry_after = Some(until);
    second.status_message = "credential k-1".into();
    h.manager.register_unsaved(second).expect("register");

    let records = snapshot(&h.manager, h.now());
    let of = |id: &str, model: &str| {
        records
            .iter()
            .find(|record| record.auth_id == id && record.model == model)
            .cloned()
            .unwrap_or_default()
    };
    for model in ["", "m1"] {
        let record = of("auth-1", model);
        let what = if model.is_empty() {
            "credential"
        } else {
            "model"
        };
        let error = record.last_error.clone().unwrap_or_default();
        assert_eq!(record.quota.reason, "quota for [redacted]", "{model}");
        assert_eq!(error.code, format!("bad {what} [redacted]"), "{model}");
        assert_eq!(
            error.message,
            format!("rejected [redacted] and {what} [redacted]"),
            "{model}"
        );
        assert_eq!(error.http_status, 401, "{model}");
        assert!(record.reason.contains("[redacted]"), "{model}: {record:?}");
        assert!(!record.reason.contains("k-1"), "{model}: {record:?}");
        assert!(!record.reason.contains("tok-9"), "{model}: {record:?}");
    }
    assert_eq!(
        of("auth-2", "").reason,
        "credential k-1",
        "the other credential's text is its own"
    );
}

/// Not upstream's: the worker tries a failed save again, waiting twice as
/// long each time, and then stops once it has been saved.
#[test]
fn the_worker_retries_a_failed_save_after_longer_waits() {
    let debounce = Duration::from_millis(20);
    let store = FlakyStore::failing(3);
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, debounce);
    install_store(&h.manager, store.clone());
    restore_now(&h.manager);
    register(&h.manager, "auth-1", "xai");
    h.manager
        .mark_result(&failure("auth-1", "xai", "m1", 429, "rate limited"));

    let deadline = Instant::now() + Duration::from_secs(20);
    while store.saved() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(store.saved(), 1, "saved in the end");
    let attempts = store.attempts();
    assert_eq!(attempts.len(), 4);
    // Each wait is at least what it is meant to be; a busy machine only
    // makes it longer.
    for (failures, pair) in (1u32..).zip(attempts.windows(2)) {
        let [earlier, later] = pair else {
            continue;
        };
        assert!(
            later.duration_since(*earlier) >= debounce * (1 << failures),
            "wait after failure {failures}"
        );
    }
    std::thread::sleep(debounce * 20);
    assert_eq!(store.attempts().len(), 4, "no more once saved");
}

/// A store whose saves wait to be let go.
struct BlockingStore {
    started: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
    saves: AtomicUsize,
}

impl StateStore for BlockingStore {
    fn load(&self) -> Result<Vec<Record>, StoreError> {
        Ok(Vec::new())
    }

    fn save(&self, _records: &[Record], _now: Timestamp) -> Result<(), StoreError> {
        if let Some(started) = lock(&self.started).take() {
            let _ = started.send(());
        }
        let _ = lock(&self.release).recv_timeout(Duration::from_secs(10));
        self.saves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Not upstream's: while a save is writing, the manager's lock is free:
/// picking a credential and recording a result don't wait for it.
#[test]
fn a_save_never_holds_the_managers_lock() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let store = Arc::new(BlockingStore {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        saves: AtomicUsize::new(0),
    });
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(&h.manager, store.clone());
    restore_now(&h.manager);
    register(&h.manager, "auth-1", "xai");
    h.manager
        .mark_result(&failure("auth-1", "xai", "m1", 429, "rate limited"));

    let manager = h.manager.clone();
    let saver = std::thread::spawn(move || flush(&manager));
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the save started");

    let begun = Instant::now();
    assert_eq!(h.manager.list().len(), 1);
    drop(h.manager.lock());
    h.manager
        .mark_result(&failure("auth-1", "xai", "m2", 429, "rate limited"));
    h.manager.mark_result(&success("auth-1", "xai", "m3"));
    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "the manager waited for the save"
    );
    assert_eq!(store.saves.load(Ordering::SeqCst), 0, "still writing");

    release_tx.send(()).expect("release");
    saver.join().expect("join");
    assert_eq!(store.saves.load(Ordering::SeqCst), 1);
}
