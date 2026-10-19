//! Not upstream's: the seam between the switch and the disk. Everything the
//! core writes or checks on disk goes through [`Disk`], so the state machine
//! runs in unit tests with no files and with scripted failures, and the real
//! implementation ([`RealDisk`]) is the one place that calls `open-ferry-update`.
//!
//! All the methods are synchronous, as the journal store is (it sleeps between
//! its tries). The core calls them directly. A write is a few milliseconds
//! except under a failing disk, where the three tries add 200 ms; the core is
//! not serving any request, only coordinating, so that is accepted (a
//! deviation, noted in the report).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use open_ferry_update::data_dir::DataDir;
use open_ferry_update::installed_file::hash_installed;
use open_ferry_update::journal::{Artifact, Journal, JournalError, JournalStore, Outcome};
use open_ferry_update::pins::PinStore;
use open_ferry_update::staging::hash_file;
use open_ferry_update::switch::{RealFileOps, choose_aside};

use crate::install::{InstallPaths, InstallResult, place};
use crate::statefile::{StateWriteError, apply_by};

pub trait Disk: Send + Sync + 'static {
    /// Writes the journal as the step `name` (`J1`, `J-R1`, ...), with the
    /// store's own retries. `seq` is restored on a failure, but the caller's
    /// in-memory changes are not: it clones first when it must undo them.
    fn write_journal(&self, journal: &mut Journal, name: &str) -> Result<(), JournalError>;

    /// Gives the state file `outcome` as the step `name` (`S1`, `S2`, `S3`),
    /// taking the update lock without waiting (T3).
    fn write_state(&self, name: &str, outcome: &Outcome) -> Result<(), StateWriteError>;

    /// The kept artifact `versions/<version>/`, hashed. An error names what is
    /// wrong with it (missing, or another hash).
    fn artifact(&self, version: &str, sha256: &str) -> Result<Artifact, ArtifactError>;

    /// Whether a live pin `id` holds `version` and `sha256` (T3).
    fn check_pin(
        &self,
        id: &str,
        version: &str,
        sha256: &str,
        now: DateTime<Utc>,
    ) -> Result<(), String>;

    /// Deletes the pin `id`. Called once J1 has landed, never before
    /// (the addendum's "persist J1 before consuming the pin"). Item 5's hook.
    fn consume_pin(&self, id: &str) -> Result<(), String>;

    /// The hash of the installed file, `None` if it is missing.
    fn installed_sha256(&self) -> Option<String>;

    /// The installed file as the switched-off run is measured, at start-up and
    /// in the Core alike: a regular file opened without following a link, hash
    /// and size from the same open file. `(None, 0)` when it can't be read or
    /// the leaf is a link.
    fn installed_switched_off(&self) -> (Option<String>, u64);

    /// The size in bytes of the installed file, `None` if it can't be read.
    /// The installed rung of a return carries it in its `Artifact`.
    fn installed_size(&self) -> Option<u64>;

    /// A name for the installed file to be moved aside to, chosen before any
    /// rename so the journal can hold it.
    fn choose_aside(&self) -> Result<String, String>;

    /// Replaces the installed file by the kept copy of `target` (3.9).
    fn install(
        &self,
        target: &Artifact,
        expected: Option<&str>,
        from: &str,
        aside: Option<&str>,
    ) -> InstallResult;

    /// Prepares, journals the selected aside, and commits under the caller's
    /// update lock. The default is the scripted disk's atomic install seam.
    fn install_journalled(
        &self,
        journal: &mut Journal,
        target: &Artifact,
        expected: Option<&str>,
        from: &str,
        aside: Option<&str>,
    ) -> InstallResult {
        if let Err(error) = self.write_journal(journal, "J-install") {
            return InstallResult::Failed(format!("could not journal the install: {error}"));
        }
        self.install(target, expected, from, aside)
    }

    /// Classifies and installs one pending journal entry as one transaction.
    /// The real disk takes the update lock without waiting and moves its guard
    /// into the blocking task, so dropping the caller cannot unlock a rename.
    fn install_transaction(
        &self,
        journal: Journal,
    ) -> impl std::future::Future<Output = (Journal, InstallResult)> + Send {
        async move { install_transaction(self, journal) }
    }

    /// Makes the installed file's directory entry durable. A retry that finds
    /// the target already installed (an earlier attempt placed it but its
    /// directory sync failed) calls this before it reports the install done.
    fn sync_installed_directory(&self) -> Result<(), String> {
        Ok(())
    }

    /// Whether the kept copy of `version` may be copied over the installed
    /// file now: under 2.5 (owned by the service's identity, nothing
    /// foreign-writable on its path). Checked at the install, not only at
    /// startup, because the directory can change after startup. `Err` names
    /// what is wrong; the environment is to blame, so the install is retried.
    fn check_install_source(&self, _version: &str) -> Result<(), String> {
        Ok(())
    }

    /// Deletes `handover.json` (L6). The core calls it only once A has resumed
    /// or exited.
    fn remove_handover(&self);

    /// The versions a `keep_set` reply lists (v3 3.5): what the journal on
    /// disk and `memory` (the core's journal) protect, with a worker counted
    /// when its pid and start time are still alive, live pins, `state.staged`
    /// and `state.previous`, the newest other version, and `tracked` (the
    /// versions of the workers the core tracks). Read only and lock free: the
    /// answer may lag a pin file written since. An `Err` (an unreadable or
    /// newer journal, an unparseable pin) means the caller must list every
    /// version instead.
    fn keep_set(
        &self,
        memory: Option<&Journal>,
        tracked: &[String],
        now: DateTime<Utc>,
    ) -> Result<BTreeSet<String>, String> {
        let _ = (memory, now);
        Ok(tracked.iter().cloned().collect())
    }

    /// Every version directory present: the reply with `keep_all`, and when
    /// `keep_set` fails. Any listing error is an `Err` (never a partial list):
    /// the caller then has no keep set to publish.
    fn all_versions(&self) -> Result<BTreeSet<String>, String> {
        Err("this disk cannot list the versions".to_owned())
    }

    /// Prunes `versions/` and expired pins as 3.5 says. The real disk takes
    /// the update lock without waiting (`Busy` when it is held) and moves the
    /// guard into the blocking task, like `install_transaction`. The default
    /// deletes nothing.
    fn prune(
        &self,
        request: PruneRequest,
    ) -> impl std::future::Future<Output = PruneOutcome> + Send {
        async move {
            let _ = request;
            PruneOutcome::Done {
                removed: Vec::new(),
                pins_removed: 0,
            }
        }
    }
}

/// What the core hands a prune besides what the disk records itself.
#[derive(Debug, Clone)]
pub struct PruneRequest {
    /// The core's in-memory journal, which may be ahead of the file.
    pub journal: Option<Journal>,
    /// The versions of every worker the core tracks.
    pub tracked: Vec<String>,
    pub now: DateTime<Utc>,
}

/// How a prune ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneOutcome {
    Done {
        removed: Vec<String>,
        pins_removed: usize,
    },
    /// The update lock was held: nothing was waited for or deleted.
    Busy,
    /// The journal or a pin could not be read, the journal is newer, or
    /// `versions/` could not be listed: nothing was deleted.
    Refused(String),
    /// Versions were deleted (`removed`), and then the expired pins could not
    /// all be removed (`why`). Not a refusal: what was deleted is gone.
    Partial { removed: Vec<String>, why: String },
}

/// Runs `work` on a blocking thread that owns `locks` (the update lock, and
/// the install lock where the work touches the installed file), so that a
/// cancelled caller does not let them go while the work still runs.
async fn blocking_install<T: Send + 'static>(
    locks: impl Send + 'static,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        let _locks = locks;
        work()
    })
    .await
}

pub(crate) fn install_transaction<S: Disk + ?Sized>(
    disk: &S,
    mut journal: Journal,
) -> (Journal, InstallResult) {
    use open_ferry_update::installed_file::{CommitAction, classify, commit_action};
    let Some(pending) = journal.install_pending.clone() else {
        return (
            journal,
            InstallResult::Failed("there is no pending install".to_owned()),
        );
    };
    let old = journal
        .installed
        .as_ref()
        .map(|installed| installed.sha256.as_str())
        .unwrap_or_default();
    let from = journal
        .installed
        .as_ref()
        .map(|installed| installed.version.clone())
        .unwrap_or_default();
    let installed_sha = disk.installed_sha256();
    let action = commit_action(classify(
        installed_sha.as_deref(),
        old,
        &pending.target.sha256,
    ));
    let result = match action {
        // The file may be there from an attempt whose directory sync failed:
        // it is not done until the entry is durable.
        CommitAction::AlreadyReplaced => match disk.sync_installed_directory() {
            Ok(()) => InstallResult::Replaced,
            Err(why) => InstallResult::Failed(format!(
                "the installed file is in place, but its directory sync failed: {why}"
            )),
        },
        CommitAction::ChangedElsewhere => InstallResult::ChangedElsewhere,
        CommitAction::Replace | CommitAction::RestoreThenReplace => {
            let expected = if matches!(action, CommitAction::Replace) {
                installed_sha.as_deref()
            } else {
                None
            };
            // Nothing is copied from a source that is not trusted now.
            match disk.check_install_source(&pending.target.version) {
                Err(why) => InstallResult::Failed(format!(
                    "the environment is not safe to install from: {why}"
                )),
                Ok(()) => {
                    let aside = (!pending.aside.is_empty()).then_some(pending.aside.as_str());
                    disk.install_journalled(&mut journal, &pending.target, expected, &from, aside)
                }
            }
        }
    };
    (journal, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// `install_transaction` on a real disk, retrying a busy update lock up to
    /// 50 times, 20 ms apart. The lock is a `flock`, which belongs to the open
    /// file description: a thread of this test binary that forks a child (the
    /// driver and isolated-child tests do) lets the child hold a copy of the
    /// lock's descriptor until it execs, so a lock this test just dropped can
    /// read as busy for an instant. Production already retries a busy lock.
    async fn install_transaction(disk: &RealDisk, journal: Journal) -> (Journal, InstallResult) {
        let mut answer = Disk::install_transaction(disk, journal.clone()).await;
        for _ in 0..50 {
            if !matches!(&answer.1, InstallResult::Failed(why) if why == "the update lock is busy")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            answer = Disk::install_transaction(disk, journal.clone()).await;
        }
        answer
    }

    /// A cancelled runtime install transaction does not let the install lock
    /// go while its blocking work runs: another data directory's detection
    /// answers `Busy` until the work ends, and only then `Clear`.
    #[tokio::test]
    async fn cancelling_a_runtime_install_keeps_the_install_lock_until_it_finishes() {
        use open_ferry_update::install_lock::InstallLock;
        let rig = crate::testkit::Rig::new().await;
        let journal = rig.disk.on_disk().unwrap();
        let root = scratch();
        let data = DataDir::at(root.join("data"));
        std::fs::create_dir_all(data.root()).unwrap();
        let other = root.join("other-data");
        std::fs::create_dir_all(&other).unwrap();
        let installed = root.join("installed");
        std::fs::write(&installed, b"old binary").unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered_tx = std::sync::Mutex::new(Some(entered_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let mut disk = RealDisk::new(
            data.clone(),
            installed.clone(),
            "open-ferry".to_owned(),
            JournalStore::new(data.root()),
        )
        .with_install_lock(Some(std::sync::Arc::new(
            InstallLock::try_acquire(&installed).unwrap(),
        )));
        disk.before_install = Some(std::sync::Arc::new(move || {
            if let Some(sender) = entered_tx.lock().unwrap().take() {
                sender.send(()).unwrap();
            }
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10));
        }));
        let task = tokio::spawn(async move { Disk::install_transaction(&disk, journal).await });
        entered_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let identity =
            crate::endpoint::Identity::current(crate::endpoint::ServiceKind::User).unwrap();
        let soon = || std::time::Instant::now() + std::time::Duration::from_secs(5);
        let during = crate::client::detect(&other, &identity, &installed, soon()).await;
        release_tx.send(()).unwrap();
        assert!(
            matches!(during, crate::client::Detection::Busy(_)),
            "cancel let the install lock go mid-install: {during:?}"
        );
        // A parallel child's fork-to-exec window can briefly retain our flock fd.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let after = crate::client::detect(&other, &identity, &installed, soon()).await;
            if matches!(after, crate::client::Detection::Clear(_)) {
                break;
            }
            assert!(std::time::Instant::now() < until, "still held: {after:?}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A directory of its own for one test. The name carries a timestamp as
    /// well as the process id and a counter: a test that fails leaves its
    /// directory behind, and a later run whose process id repeats (Windows
    /// reuses them) would otherwise ask for the same name and fail with
    /// "already exists".
    fn scratch() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = if cfg!(unix) {
            PathBuf::from(std::env::var_os("HOME").unwrap())
        } else {
            std::env::temp_dir()
        };
        let path = base.join(format!(
            "ofp-s2-install-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn the_installed_size_is_the_files_length_and_none_when_missing() {
        let root = scratch();
        let data = DataDir::at(&root);
        let installed = root.join("installed");
        let disk = RealDisk::new(
            data.clone(),
            installed.clone(),
            "open-ferry".to_owned(),
            JournalStore::new(&root),
        );
        assert_eq!(disk.installed_size(), None);
        std::fs::write(&installed, b"twelve bytes").unwrap();
        assert_eq!(disk.installed_size(), Some(12));
        std::fs::remove_file(&installed).unwrap();
        std::fs::create_dir(&installed).unwrap();
        assert_eq!(disk.installed_size(), None, "a directory is not a binary");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn each_install_attempt_journals_the_prepared_aside_not_the_j5_preference() {
        use open_ferry_update::journal::{InstallPending, Installed, JournalLoad};
        let rig = crate::testkit::Rig::new().await;
        let mut journal = rig.disk.on_disk().unwrap();
        let root = scratch();
        let data = DataDir::at(&root);
        let installed = root.join("installed");
        std::fs::write(&installed, b"old binary").unwrap();
        let old_sha = hash_file(&installed).unwrap();
        let source = data.binary("2.0.0", "open-ferry");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"new binary").unwrap();
        let target = Artifact {
            version: "2.0.0".to_owned(),
            sha256: hash_file(&source).unwrap(),
            size: 10,
        };
        let preferred = root.join("installed.old.1");
        std::fs::write(root.join("installed.old"), b"stale aside").unwrap();
        journal.installed = Some(Installed {
            version: "1.0.0".to_owned(),
            sha256: old_sha.clone(),
        });
        journal.install_pending = Some(InstallPending {
            target: target.clone(),
            source: source.to_string_lossy().into_owned(),
            aside: preferred.to_string_lossy().into_owned(),
            attempts: 0,
            next_try: None,
            started: "2026-10-09T00:00:00Z".to_owned(),
            record_id: None,
        });
        let disk = RealDisk::new(
            data.clone(),
            installed.clone(),
            "open-ferry".to_owned(),
            JournalStore::new(&root),
        );
        // A failed prepared-journal write must not rename anything. The next
        // attempt prepares again and journals its own selected aside.
        let lock = data.try_lock().unwrap().unwrap();
        let result = crate::install::place_with_prepared(
            &disk.paths(),
            &target,
            Some(&old_sha),
            "1.0.0",
            Some(&preferred),
            |actual| {
                assert_eq!(std::fs::read(&installed).unwrap(), b"old binary");
                if let Some(actual) = actual {
                    assert!(!actual.exists());
                }
                Err("journal write refused".to_owned())
            },
        );
        assert_eq!(
            result,
            InstallResult::Failed("journal write refused".to_owned())
        );
        assert_eq!(std::fs::read(&installed).unwrap(), b"old binary");
        drop(lock);
        wait_until_unlocked(&data);
        let (journal, result) = install_transaction(&disk, journal).await;
        assert_eq!(result, InstallResult::Replaced);
        let actual = &journal.install_pending.as_ref().unwrap().aside;
        assert_ne!(actual, &preferred.to_string_lossy());
        if cfg!(windows) {
            assert_eq!(actual, &root.join("installed.old").to_string_lossy());
        } else {
            assert!(actual.is_empty(), "Unix uses no aside");
        }
        let JournalLoad::Loaded(on_disk) = disk.store.load().unwrap() else {
            panic!("prepared journal missing")
        };
        assert_eq!(on_disk.install_pending.unwrap().aside, *actual);
        assert_eq!(std::fs::read(&installed).unwrap(), b"new binary");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Item 9 on a real disk: the kept copy's directory becomes writable by
    /// others after startup. The install copies nothing, the environment is
    /// blamed, and once the directory is private again the retry installs.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_install_from_a_foreign_writable_directory_installs_nothing_until_it_is_private() {
        use open_ferry_update::journal::{InstallPending, Installed};
        use std::os::unix::fs::PermissionsExt;
        let rig = crate::testkit::Rig::new().await;
        let mut journal = rig.disk.on_disk().unwrap();
        let root = scratch();
        let data = DataDir::at(&root);
        let installed = root.join("installed");
        std::fs::write(&installed, b"old binary").unwrap();
        let old_sha = hash_file(&installed).unwrap();
        let source = data.binary("2.0.0", "open-ferry");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"new binary").unwrap();
        let target = Artifact {
            version: "2.0.0".to_owned(),
            sha256: hash_file(&source).unwrap(),
            size: 10,
        };
        journal.installed = Some(Installed {
            version: "1.0.0".to_owned(),
            sha256: old_sha,
        });
        journal.install_pending = Some(InstallPending {
            target,
            source: source.to_string_lossy().into_owned(),
            aside: String::new(),
            attempts: 0,
            next_try: None,
            started: "2026-10-09T00:00:00Z".to_owned(),
            record_id: None,
        });
        let disk = RealDisk::new(
            data.clone(),
            installed.clone(),
            "open-ferry".to_owned(),
            JournalStore::new(&root),
        )
        .with_trust(crate::endpoint::ServiceKind::User);
        // After startup: the directory holding the kept copy is writable by
        // anyone.
        let dir = source.parent().unwrap().to_owned();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        for _ in 0..2 {
            let (after, result) = install_transaction(&disk, journal.clone()).await;
            assert!(
                matches!(&result, InstallResult::Failed(why)
                    if why.contains("the environment is not safe to install from")),
                "{result:?}"
            );
            assert_eq!(after.install_pending, journal.install_pending);
            assert_eq!(std::fs::read(&installed).unwrap(), b"old binary");
            let leftovers: Vec<_> = std::fs::read_dir(&root)
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            assert!(
                leftovers.iter().all(|name| !name.contains("installed.")),
                "nothing was copied: {leftovers:?}"
            );
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (_, result) = install_transaction(&disk, journal).await;
        assert_eq!(result, InstallResult::Replaced);
        assert_eq!(std::fs::read(&installed).unwrap(), b"new binary");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A pending install of 2.0.0 over a real installed file, with the kept
    /// copy in place and the trust check on.
    #[cfg(unix)]
    struct RealInstall {
        root: PathBuf,
        disk: RealDisk,
        journal: Journal,
        installed: PathBuf,
        source: PathBuf,
    }

    #[cfg(unix)]
    async fn real_install() -> RealInstall {
        use open_ferry_update::journal::{InstallPending, Installed};
        let rig = crate::testkit::Rig::new().await;
        let mut journal = rig.disk.on_disk().unwrap();
        let root = scratch();
        let data = DataDir::at(&root);
        let installed = root.join("installed");
        std::fs::write(&installed, b"old binary").unwrap();
        let old_sha = hash_file(&installed).unwrap();
        let source = data.binary("2.0.0", "open-ferry");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"new binary").unwrap();
        let target = Artifact {
            version: "2.0.0".to_owned(),
            sha256: hash_file(&source).unwrap(),
            size: 10,
        };
        journal.installed = Some(Installed {
            version: "1.0.0".to_owned(),
            sha256: old_sha,
        });
        journal.install_pending = Some(InstallPending {
            target,
            source: source.to_string_lossy().into_owned(),
            aside: String::new(),
            attempts: 0,
            next_try: None,
            started: "2026-10-09T00:00:00Z".to_owned(),
            record_id: None,
        });
        let disk = RealDisk::new(
            data,
            installed.clone(),
            "open-ferry".to_owned(),
            JournalStore::new(&root),
        )
        .with_trust(crate::endpoint::ServiceKind::User);
        RealInstall {
            root,
            disk,
            journal,
            installed,
            source,
        }
    }

    /// The install is bounded: a pipe in the source's place must not hold it.
    #[cfg(unix)]
    async fn install_within_ten_seconds(real: &RealInstall) -> (Journal, InstallResult) {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            install_transaction(&real.disk, real.journal.clone()),
        )
        .await
        .expect("the install was held by the source")
    }

    /// Nothing was published, journalled or left behind.
    #[cfg(unix)]
    fn assert_nothing_installed(real: &RealInstall, after: &Journal) {
        assert_eq!(std::fs::read(&real.installed).unwrap(), b"old binary");
        assert_eq!(after.install_pending, real.journal.install_pending);
        assert!(
            !real
                .root
                .join(open_ferry_update::journal::JOURNAL_FILE)
                .exists()
        );
        let leftovers: Vec<_> = std::fs::read_dir(&real.root)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("installed."))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// O-M3: the replacing copy's final mode is set after the updater's last
    /// sync, so the copy is synced again. A failure there fails the install
    /// before anything is journalled or renamed, and the install stays pending.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_sync_after_the_final_mode_fails_the_install_with_nothing_published() {
        let real = real_install().await;
        crate::install::sync_failures::fail_files_in(&real.root);
        let (after, result) = install_transaction(&real.disk, real.journal.clone()).await;
        crate::install::sync_failures::allow_files_in(&real.root);
        assert!(
            matches!(&result, InstallResult::Failed(why) if why.contains("forced to fail")),
            "{result:?}"
        );
        assert_nothing_installed(&real, &after);
        // Once the sync works the retry installs.
        let (_, result) = install_transaction(&real.disk, real.journal.clone()).await;
        assert_eq!(result, InstallResult::Replaced);
        assert_eq!(std::fs::read(&real.installed).unwrap(), b"new binary");
        std::fs::remove_dir_all(&real.root).unwrap();
    }

    /// R-M2: a pipe in the kept source's place after the trust check is
    /// refused at once, not opened for a read that never ends.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pipe_swapped_in_for_the_kept_source_is_refused_not_waited_on() {
        // A regression (a blocking open of the pipe) hangs, and a hung blocking
        // thread outlives the test's own timeout: so the case runs in a child
        // of the test binary under a parent deadline that kills it.
        let Some(done) = crate::testkit::isolated::child_dir() else {
            crate::testkit::isolated::run_in_child(
                module_path!(),
                "a_pipe_swapped_in_for_the_kept_source_is_refused_not_waited_on",
                std::time::Duration::from_secs(60),
            );
            return;
        };
        let real = real_install().await;
        let source = real.source.clone();
        crate::install::source_seam::once(&real.source, move || {
            use std::os::unix::ffi::OsStrExt;
            std::fs::remove_file(&source).unwrap();
            let path = std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
            // SAFETY: `path` is a NUL-terminated path that outlives the call.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        });
        let (after, result) = install_within_ten_seconds(&real).await;
        assert!(
            matches!(&result, InstallResult::Failed(why) if why.contains("not a regular file")),
            "{result:?}"
        );
        assert_nothing_installed(&real, &after);
        std::fs::remove_dir_all(&real.root).unwrap();
        crate::testkit::isolated::mark_completed(&done);
    }

    /// R-M2: a link to the right bytes in the kept source's place is refused,
    /// not followed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_swapped_in_for_the_kept_source_is_refused_not_followed() {
        let real = real_install().await;
        let source = real.source.clone();
        let elsewhere = real.root.join("elsewhere");
        crate::install::source_seam::once(&real.source, move || {
            std::fs::rename(&source, &elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &source).unwrap();
        });
        let (after, result) = install_within_ten_seconds(&real).await;
        assert!(matches!(&result, InstallResult::Failed(_)), "{result:?}");
        assert_eq!(std::fs::read(&real.installed).unwrap(), b"old binary");
        assert_eq!(after.install_pending, real.journal.install_pending);
        assert!(
            !real
                .root
                .join(open_ferry_update::journal::JOURNAL_FILE)
                .exists()
        );
        std::fs::remove_dir_all(&real.root).unwrap();
    }

    #[tokio::test]
    async fn an_install_defers_without_classifying_when_the_update_lock_is_busy() {
        let rig = crate::testkit::Rig::new().await;
        let journal = rig.disk.on_disk().unwrap();
        let root = scratch();
        let data = DataDir::at(&root);
        let disk = RealDisk::new(
            data.clone(),
            root.join("installed"),
            "open-ferry".to_owned(),
            JournalStore::new(&root),
        );
        let lock = data.try_lock().unwrap().unwrap();
        let (after, result) = install_transaction(&disk, journal.clone()).await;
        assert_eq!(after, journal);
        assert_eq!(
            result,
            InstallResult::Failed("the update lock is busy".to_owned())
        );
        assert!(!root.join(open_ferry_update::journal::JOURNAL_FILE).exists());
        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn dropping_an_install_caller_cannot_unlock_the_running_blocking_transaction() {
        let root = scratch();
        let data = DataDir::at(&root);
        let lock = data.try_lock().unwrap().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel::<()>();
        let caller = tokio::spawn(blocking_install(lock, move || {
            started_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            finished_tx
        }));
        started_rx.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let remains_locked = data.try_lock().unwrap().is_none();
        release_tx.send(()).unwrap();
        // The sender is dropped only once work returned and the lock guard was
        // dropped by the blocking task; no sleep stands in for that event.
        assert!(finished_rx.await.is_err());
        assert!(
            remains_locked,
            "the abandoned caller released the rename's guard"
        );
        wait_until_unlocked(&data);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Waits, for at most 5 s, until the update lock can be taken. `flock`
    /// belongs to the open file description, so a child that a parallel test
    /// forks holds a copy of every descriptor from fork until exec (close-on-exec
    /// or not): for that instant the lock stays held after our guard dropped. In
    /// production the inherited copy closes at exec.
    fn wait_until_unlocked(data: &DataDir) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if data.try_lock().unwrap().is_some() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the update lock was still held 5 s after its guard dropped"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// Why a kept artifact can't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactError {
    /// There is no file. Blamed on the environment: nothing ran.
    Missing,
    /// Another hash than the one asked for.
    Mismatch {
        found: String,
    },
    Io(String),
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "the file is not in versions/"),
            Self::Mismatch { found } => {
                write!(f, "the file hashes to {found}, not the expected hash")
            }
            Self::Io(why) => write!(f, "{why}"),
        }
    }
}

/// The real disk.
#[derive(Clone)]
pub struct RealDisk {
    data: DataDir,
    installed: PathBuf,
    binary_name: String,
    store: JournalStore,
    /// The service kind whose identity the kept copies are checked against
    /// at each install; `None` checks nothing (a bare disk in a test).
    trust: Option<crate::endpoint::ServiceKind>,
    /// The running supervisor's version, noted in the state when an outcome
    /// says a newer supervisor is needed.
    supervisor_version: String,
    /// The supervisor's install lock, which every install transaction keeps
    /// held on its blocking thread. `None` for a bare disk in a test.
    install_lock: Option<std::sync::Arc<open_ferry_update::install_lock::InstallLock>>,
    /// Runs on the blocking thread, locks held, before the transaction: a
    /// test's way to hold the work still.
    #[cfg(test)]
    before_install: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl RealDisk {
    pub fn new(
        data: DataDir,
        installed: PathBuf,
        binary_name: String,
        store: JournalStore,
    ) -> Self {
        Self {
            data,
            installed,
            binary_name,
            store,
            trust: None,
            supervisor_version: open_ferry_update::CURRENT_VERSION.to_owned(),
            install_lock: None,
            #[cfg(test)]
            before_install: None,
        }
    }

    /// Has each install transaction keep the supervisor's install lock held
    /// through its blocking work. The supervisor already holds it for life;
    /// this one handle is what the work holds a clone of.
    #[must_use]
    pub fn with_install_lock(
        mut self,
        lock: Option<std::sync::Arc<open_ferry_update::install_lock::InstallLock>>,
    ) -> Self {
        self.install_lock = lock;
        self
    }

    /// Names the running supervisor's version, which an outcome that needs a
    /// newer supervisor is recorded against (default: this crate's).
    #[must_use]
    pub fn with_supervisor_version(mut self, version: String) -> Self {
        self.supervisor_version = version;
        self
    }

    /// Has each install check the kept copy under 2.5 for a service of `kind`.
    #[must_use]
    pub fn with_trust(mut self, kind: crate::endpoint::ServiceKind) -> Self {
        self.trust = Some(kind);
        self
    }

    fn paths(&self) -> InstallPaths {
        InstallPaths {
            data: self.data.clone(),
            installed: self.installed.clone(),
            binary_name: self.binary_name.clone(),
        }
    }
}

impl Disk for RealDisk {
    fn write_journal(&self, journal: &mut Journal, name: &str) -> Result<(), JournalError> {
        self.store.write(journal, name)
    }

    fn write_state(&self, name: &str, outcome: &Outcome) -> Result<(), StateWriteError> {
        apply_by(&self.data, name, outcome, &self.supervisor_version)
    }

    fn artifact(&self, version: &str, sha256: &str) -> Result<Artifact, ArtifactError> {
        let path = self.data.binary(version, &self.binary_name);
        let found = match hash_file(&path) {
            Ok(found) => found,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ArtifactError::Missing);
            }
            Err(error) => return Err(ArtifactError::Io(error.to_string())),
        };
        if !found.eq_ignore_ascii_case(sha256) {
            return Err(ArtifactError::Mismatch { found });
        }
        let size = std::fs::metadata(&path)
            .map_err(|error| ArtifactError::Io(error.to_string()))?
            .len();
        Ok(Artifact {
            version: version.to_owned(),
            sha256: found,
            size,
        })
    }

    fn check_pin(
        &self,
        id: &str,
        version: &str,
        sha256: &str,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let pins = PinStore::new(&self.data.versions());
        let live = pins.live(now).map_err(|error| error.to_string())?;
        match live.iter().find(|pin| pin.id == id) {
            Some(pin) if pin.version == version && pin.sha256.eq_ignore_ascii_case(sha256) => {
                Ok(())
            }
            Some(_) => Err("the pin holds another version".to_owned()),
            None => Err("there is no live pin with that id".to_owned()),
        }
    }

    fn consume_pin(&self, id: &str) -> Result<(), String> {
        PinStore::new(&self.data.versions())
            .consume(id)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn installed_sha256(&self) -> Option<String> {
        hash_installed(&self.installed).unwrap_or(None)
    }

    fn installed_switched_off(&self) -> (Option<String>, u64) {
        crate::startup::hash_disabled_installed(&self.installed).unwrap_or((None, 0))
    }

    fn installed_size(&self) -> Option<u64> {
        std::fs::metadata(&self.installed)
            .ok()
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len())
    }

    fn choose_aside(&self) -> Result<String, String> {
        choose_aside(&RealFileOps, &self.installed)
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|error| error.to_string())
    }

    fn install(
        &self,
        target: &Artifact,
        expected: Option<&str>,
        from: &str,
        aside: Option<&str>,
    ) -> InstallResult {
        place(&self.paths(), target, expected, from, aside.map(Path::new))
    }

    fn install_journalled(
        &self,
        journal: &mut Journal,
        target: &Artifact,
        expected: Option<&str>,
        from: &str,
        aside: Option<&str>,
    ) -> InstallResult {
        crate::install::place_with_prepared(
            &self.paths(),
            target,
            expected,
            from,
            aside.map(Path::new),
            |actual| {
                if let Some(pending) = journal.install_pending.as_mut() {
                    pending.aside = actual
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_default();
                }
                self.store
                    .write(journal, "J-install")
                    .map_err(|error| format!("could not journal the prepared install: {error}"))
            },
        )
    }

    async fn install_transaction(&self, journal: Journal) -> (Journal, InstallResult) {
        let lock = match self.data.try_lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return (
                    journal,
                    InstallResult::Failed("the update lock is busy".to_owned()),
                );
            }
            Err(error) => return (journal, InstallResult::Failed(error.to_string())),
        };
        let disk = self.clone();
        let fallback = journal.clone();
        let locks = (lock, self.install_lock.clone());
        let work = move || {
            #[cfg(test)]
            if let Some(hook) = &disk.before_install {
                hook();
            }
            let result = install_transaction(&disk, journal);
            // Test builds only: the file is durably replaced, the transaction
            // still owns its locks, and the core has not written J6 yet.
            #[cfg(feature = "blue-green-test")]
            if matches!(&result.1, InstallResult::Replaced) {
                let _ =
                    open_ferry_update::test_seams::hit(disk.data.root(), "install.after_replace");
            }
            result
        };
        match blocking_install(locks, work).await {
            Ok(result) => result,
            Err(error) => (fallback, InstallResult::Failed(error.to_string())),
        }
    }

    fn sync_installed_directory(&self) -> Result<(), String> {
        match self.installed.parent() {
            Some(parent) => {
                crate::install::sync_directory(parent).map_err(|error| error.to_string())
            }
            None => Ok(()),
        }
    }

    fn check_install_source(&self, version: &str) -> Result<(), String> {
        let Some(kind) = self.trust else {
            return Ok(());
        };
        let identity =
            crate::endpoint::Identity::current(kind).map_err(|error| error.to_string())?;
        let rules = crate::trust::TrustRules::for_identity(&identity);
        crate::trust::check_file(&self.data.binary(version, &self.binary_name), &rules)
            .map_err(|error| error.to_string())
    }

    fn keep_set(
        &self,
        memory: Option<&Journal>,
        tracked: &[String],
        now: DateTime<Utc>,
    ) -> Result<BTreeSet<String>, String> {
        open_ferry_update::updater::supervised_keep_set(
            &self.data,
            tracked.iter().cloned(),
            memory,
            now,
            crate::process::process_alive,
        )
        .map_err(|error| error.to_string())
    }

    fn all_versions(&self) -> Result<BTreeSet<String>, String> {
        open_ferry_update::updater::all_versions(&self.data).map_err(|error| error.to_string())
    }

    async fn prune(&self, request: PruneRequest) -> PruneOutcome {
        let lock = match self.data.try_lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => return PruneOutcome::Busy,
            Err(error) => {
                return PruneOutcome::Refused(format!("could not take the update lock: {error}"));
            }
        };
        let data = self.data.clone();
        let binary_name = self.binary_name.clone();
        let work = move || {
            open_ferry_update::updater::supervised_prune(
                &data,
                &binary_name,
                request.tracked,
                request.journal.as_ref(),
                request.now,
                crate::process::process_alive,
            )
        };
        match blocking_install(lock, work).await {
            Ok(Ok(done)) => match done.pins_error {
                None => PruneOutcome::Done {
                    removed: done.removed,
                    pins_removed: done.pins_removed,
                },
                Some(why) => PruneOutcome::Partial {
                    removed: done.removed,
                    why,
                },
            },
            Ok(Err(error)) => PruneOutcome::Refused(error.to_string()),
            Err(error) => PruneOutcome::Refused(format!("the prune task failed: {error}")),
        }
    }

    fn remove_handover(&self) {
        let path = self.data.root().join("handover.json");
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("could not delete {}: {error}", path.display());
        }
    }
}
