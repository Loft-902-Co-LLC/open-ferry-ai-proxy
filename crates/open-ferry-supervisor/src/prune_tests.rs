//! Not upstream's: the keep set and the prune (v3 3.5, 3.8; T3) over real
//! files in a scratch data directory. The disk is the real one for the keep set
//! and the prune (`FakeDisk::real`); journal and state writes stay scripted.
//! Every prune here runs on a temp directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use open_ferry_update::data_dir::DataDir;
use open_ferry_update::journal::{Installed, JOURNAL_FILE, JournalStore, WorkerRole};
use open_ferry_update::pins::PinStore;
use open_ferry_update::state::State;
use tokio::sync::mpsc::unbounded_channel;

use crate::clock::Clock;
use crate::core::{Command, Core, Exit, Tracked};
use crate::disk::RealDisk;
use crate::driver::WorkerId;
use crate::protocol::{Channel, EndpointReply};
use crate::serve::{Serve, handle};
use crate::snapshot::Snapshot;
use crate::testkit::{NAME, Rig, SHA_1, SHA_2, artifact, core_of, update};

/// A scratch data directory (under `$HOME` on Unix, where the trust rules want it).
struct Dir {
    tmp: tempfile::TempDir,
    data: DataDir,
}

impl Dir {
    fn new() -> Self {
        let mut builder = tempfile::Builder::new();
        builder.prefix("ofp-s8-prune-");
        let tmp = match std::env::var_os("HOME").filter(|_| cfg!(unix)) {
            Some(home) => builder.tempdir_in(home).unwrap(),
            None => builder.tempdir().unwrap(),
        };
        let data = DataDir::at(tmp.path());
        Self { tmp, data }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    /// `versions/<version>/open-ferry`, holding the version as its bytes.
    fn version(&self, version: &str) {
        let binary = self.data.binary(version, NAME);
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(binary, version).unwrap();
    }

    fn versions(&self, versions: &[&str]) {
        for version in versions {
            self.version(version);
        }
    }

    /// The version directories that are there, sorted.
    fn present(&self) -> Vec<String> {
        let mut present = self.data.kept_versions();
        present.sort();
        present
    }

    fn store(&self) -> JournalStore {
        JournalStore::new(self.root()).with_retry_wait(Duration::ZERO)
    }

    fn pins(&self) -> PinStore {
        PinStore::new(&self.data.versions())
    }

    fn pin_file(&self, id: &str) -> PathBuf {
        self.pins().dir().join(format!("{id}.json"))
    }
}

/// The rig, with the keep set and the prune over `dir`. Its journal (active and
/// confirmed `1.0.0`, one worker `1.0.0`) is also written into `dir`.
async fn rig_over(dir: &Dir) -> Rig {
    let mut rig = Rig::new().await;
    let mut journal = rig.rt.as_ref().unwrap().journal.clone().unwrap();
    dir.store().write(&mut journal, "init").unwrap();
    rig.disk.real = Some(RealDisk::new(
        dir.data.clone(),
        dir.root().join("installed").join(NAME),
        NAME.to_owned(),
        dir.store(),
    ));
    rig.cfg.timings.install_retry = Duration::from_millis(40);
    rig
}

fn strings(versions: &[&str]) -> Vec<String> {
    versions
        .iter()
        .map(|version| (*version).to_owned())
        .collect()
}

/// A tracked worker of `version` in `role`, cloned from the active one.
fn tracked_like<D, H, S>(core: &mut Core<'_, D, H, S>, version: &str, role: WorkerRole) -> WorkerId
where
    D: crate::driver::Driver,
    H: crate::host::Host,
    S: crate::disk::Disk,
{
    let mut other: Tracked = core.rt.workers.values().next().unwrap().clone();
    other.artifact = artifact(version);
    other.role = role;
    let id = core.rt.workers.keys().max().unwrap() + 100;
    core.rt.workers.insert(id, other);
    id
}

#[tokio::test(start_paused = true)]
async fn the_keep_set_reply_is_the_full_set() {
    let dir = Dir::new();
    dir.versions(&[
        "0.1.0", "0.2.0", "0.3.0", "0.9.0", "1.0.0", "2.0.0", "3.0.0", "4.0.0",
    ]);
    let mut rig = rig_over(&dir).await;
    {
        // The journal's `previous`, in memory and on disk.
        let journal = rig.rt.as_mut().unwrap().journal.as_mut().unwrap();
        journal.previous = Some(artifact("0.9.0"));
        let mut written = journal.clone();
        std::fs::remove_file(dir.root().join(JOURNAL_FILE)).unwrap();
        dir.store().write(&mut written, "previous").unwrap();
    }
    // `state.staged`.
    State {
        staged: Some("0.2.0".to_owned()),
        ..State::default()
    }
    .save(&dir.data.state_file())
    .unwrap();
    // A live pin for a version no journal field names.
    dir.pins().create("2.0.0", SHA_2, rig.clock.now()).unwrap();
    let mut core = core_of!(rig);
    // A draining worker's version.
    tracked_like(&mut core, "3.0.0", WorkerRole::Draining);
    core.publish();
    let keep = rig.shared.snapshot().keep.expect("a keep set");
    // 1.0.0 is active, confirmed and installed; 0.9.0 is `previous`; 0.2.0 is
    // staged; 2.0.0 is pinned; 3.0.0 is a draining worker's; 4.0.0 is the newest
    // other version. 0.1.0 and 0.3.0 are kept by nothing.
    assert_eq!(
        keep,
        strings(&["0.2.0", "0.9.0", "1.0.0", "2.0.0", "3.0.0", "4.0.0"])
    );
}

#[tokio::test(start_paused = true)]
async fn the_reply_follows_the_journal_at_once_and_a_new_pin_at_the_next_refresh() {
    let dir = Dir::new();
    dir.versions(&["0.0.1", "0.2.0", "0.3.0", "1.0.0"]);
    let mut rig = rig_over(&dir).await;
    let now = rig.clock.now();
    let every = rig.cfg.timings.status_every;
    let mut core = core_of!(rig);
    core.publish();
    assert_eq!(
        rig.shared.snapshot().keep.expect("a keep set"),
        strings(&["0.3.0", "1.0.0"])
    );
    // The journal changes: the next publish shows it.
    core.rt.journal.as_mut().unwrap().previous = Some(artifact("0.0.1"));
    core.publish();
    assert_eq!(
        rig.shared.snapshot().keep.expect("a keep set"),
        strings(&["0.0.1", "0.3.0", "1.0.0"])
    );
    // A pin file written since: the reply may lag it, by at most the refresh.
    dir.pins().create("0.2.0", SHA_2, now).unwrap();
    core.publish();
    assert!(
        !rig.shared
            .snapshot()
            .keep
            .expect("a keep set")
            .contains(&"0.2.0".to_owned())
    );
    tokio::time::advance(every + Duration::from_secs(1)).await;
    core.publish();
    assert!(
        rig.shared
            .snapshot()
            .keep
            .expect("a keep set")
            .contains(&"0.2.0".to_owned())
    );
}

#[tokio::test(start_paused = true)]
async fn a_prune_after_another_switchs_confirm_keeps_a_pinned_version_no_journal_field_names() {
    let dir = Dir::new();
    // 0.2.0 is pinned (by another operation) and named by nothing else. Left
    // alone, the newest other version, 0.4.0, would be the one extra kept.
    dir.versions(&["0.0.1", "0.2.0", "0.3.0", "0.4.0", "1.0.0", "2.0.0"]);
    let mut rig = rig_over(&dir).await;
    let watch = rig.cfg.timings.watch;
    dir.pins().create("0.2.0", SHA_1, rig.clock.now()).unwrap();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.handle_switch(update(false), tx).await;
    assert_eq!(
        crate::testkit::done_result(&crate::testkit::drain(&mut rx))["result"],
        "switched"
    );
    // Nothing is pruned until the confirm (the start's prune is `run`'s).
    assert_eq!(dir.present().len(), 6);
    // The window ends: J5, S2, and the prune after it.
    tokio::time::advance(watch + Duration::from_secs(1)).await;
    core.on_tick().await;
    assert_eq!(
        core.rt.journal.as_ref().unwrap().confirmed.version,
        "2.0.0",
        "the switch was confirmed"
    );
    assert_eq!(
        dir.present(),
        ["0.2.0", "0.4.0", "1.0.0", "2.0.0"],
        "the pinned version stayed; the others went"
    );
}

#[tokio::test(start_paused = true)]
async fn a_prune_keeps_a_draining_workers_version_and_drops_it_once_the_worker_is_gone() {
    // v3 13 test 19, the supervisor's part: 0.5.0 is named by the supervisor's
    // draining worker alone (the journal's active and confirmed are 1.0.0).
    let dir = Dir::new();
    dir.versions(&["0.0.1", "0.5.0", "2.0.0"]);
    let mut rig = rig_over(&dir).await;
    let mut core = core_of!(rig);
    let id = tracked_like(&mut core, "0.5.0", WorkerRole::Draining);
    core.try_prune().await;
    assert_eq!(dir.present(), ["0.5.0", "2.0.0"], "0.5.0 drains");
    core.rt.workers.remove(&id);
    core.try_prune().await;
    assert_eq!(dir.present(), ["2.0.0"], "the newest other stays alone");
}

#[tokio::test(start_paused = true)]
async fn the_supervisors_prune_deletes_expired_pins_and_keeps_live_ones() {
    let dir = Dir::new();
    dir.versions(&["0.0.1", "0.0.2", "0.0.3", "1.0.0"]);
    let mut rig = rig_over(&dir).await;
    let now = rig.clock.now();
    let expired = dir
        .pins()
        .create("0.0.1", SHA_1, now - chrono::Duration::days(30))
        .unwrap();
    let live = dir.pins().create("0.0.2", SHA_2, now).unwrap();
    assert!(dir.pin_file(&expired.id).is_file());
    let mut core = core_of!(rig);
    core.try_prune().await;
    assert!(!dir.pin_file(&expired.id).exists(), "expired: deleted");
    assert!(dir.pin_file(&live.id).is_file(), "live: kept");
    // An expired pin holds nothing; the live one holds 0.0.2.
    assert_eq!(dir.present(), ["0.0.2", "0.0.3", "1.0.0"]);
}

#[tokio::test(start_paused = true)]
async fn a_prune_that_cannot_trust_the_disk_refuses_deletes_nothing_and_fails_the_reply_closed() {
    for case in ["a pin", "a journal of a newer format", "a journal"] {
        let dir = Dir::new();
        dir.versions(&["0.0.1", "0.1.0", "1.0.0"]);
        let mut rig = rig_over(&dir).await;
        let good = rig.rt.as_ref().unwrap().journal.clone().unwrap();
        let bad_pin = dir.pins().dir().join("bad.json");
        let journal_file = dir.root().join(JOURNAL_FILE);
        match case {
            "a pin" => {
                std::fs::create_dir_all(dir.pins().dir()).unwrap();
                std::fs::write(&bad_pin, b"{ this is not a pin").unwrap();
            }
            "a journal of a newer format" => {
                std::fs::write(&journal_file, br#"{"format":99,"seq":1}"#).unwrap();
            }
            _ => std::fs::write(&journal_file, b"{ this is not a journal").unwrap(),
        }
        let mut core = core_of!(rig);
        core.try_prune().await;
        assert_eq!(
            dir.present(),
            ["0.0.1", "0.1.0", "1.0.0"],
            "{case}: nothing was deleted"
        );
        core.publish();
        let shown = rig.shared.snapshot();
        assert!(shown.prune_pending, "{case}: the retry is shown");
        assert_eq!(
            shown.keep.expect("a keep set"),
            ["0.0.1", "0.1.0", "1.0.0"],
            "{case}: the reply lists every version"
        );
        // Repaired, the timer's retry prunes.
        if case == "a pin" {
            std::fs::remove_file(&bad_pin).unwrap();
        } else {
            std::fs::remove_file(&journal_file).unwrap();
            let mut good = good;
            dir.store().write(&mut good, "fix").unwrap();
        }
        tokio::time::advance(Duration::from_millis(100)).await;
        core.on_tick().await;
        assert_eq!(dir.present(), ["0.1.0", "1.0.0"], "{case}: pruned after");
        core.publish();
        assert!(!rig.shared.snapshot().prune_pending, "{case}");
    }
}

#[tokio::test(start_paused = true)]
async fn with_keep_all_nothing_is_pruned_for_the_whole_run_and_the_reply_lists_every_version() {
    let dir = Dir::new();
    dir.versions(&["0.0.1", "0.1.0", "1.0.0", "2.0.0"]);
    let mut rig = rig_over(&dir).await;
    rig.rt.as_mut().unwrap().keep_all = true;
    let watch = rig.cfg.timings.watch;
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let script = async {
        // A switch and its confirm, which would each prune.
        let (tx, mut rx) = unbounded_channel();
        command_tx
            .send(Command::Switch {
                request: update(false),
                replies: tx,
            })
            .unwrap();
        while rx.recv().await.is_some() {}
        tokio::time::sleep(watch + Duration::from_secs(30)).await;
        let _ = command_tx.send(Command::Stop);
    };
    let (exit, ()) = tokio::join!(core.run(), script);
    assert_eq!(exit, Exit::Stopped);
    assert_eq!(
        core.rt.journal.as_ref().unwrap().confirmed.version,
        "2.0.0",
        "the confirm happened"
    );
    assert_eq!(dir.present(), ["0.0.1", "0.1.0", "1.0.0", "2.0.0"]);
    assert!(
        rig.disk.state.lock().unwrap().prunes.is_empty(),
        "no prune was even asked of the disk"
    );
    assert_eq!(
        rig.shared.snapshot().keep.expect("a keep set"),
        ["0.0.1", "0.1.0", "1.0.0", "2.0.0"]
    );
}

#[tokio::test(start_paused = true)]
async fn the_supervisor_prunes_at_start_and_after_a_confirm_with_what_it_tracks() {
    let mut rig = Rig::new().await;
    let watch = rig.cfg.timings.watch;
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let (tx, mut rx) = unbounded_channel();
        command_tx
            .send(Command::Switch {
                request: update(false),
                replies: tx,
            })
            .unwrap();
        while rx.recv().await.is_some() {}
        tokio::time::sleep(watch + Duration::from_secs(30)).await;
        let _ = command_tx.send(Command::Stop);
    };
    let (exit, ()) = tokio::join!(core.run(), script);
    assert_eq!(exit, Exit::Stopped);
    let prunes = rig.disk.state.lock().unwrap().prunes.clone();
    assert_eq!(
        prunes.len(),
        2,
        "at start, and after the confirm: {prunes:?}"
    );
    assert_eq!(
        prunes[0].journal.as_ref().unwrap().confirmed.version,
        "1.0.0"
    );
    assert_eq!(prunes[0].tracked, ["1.0.0"]);
    assert_eq!(
        prunes[1].journal.as_ref().unwrap().confirmed.version,
        "2.0.0"
    );
    assert!(prunes[1].tracked.contains(&"2.0.0".to_owned()));
}

/// Asks the endpoint's handler one request and returns what it answered.
async fn ask(serve: &Serve, request: serde_json::Value) -> Vec<EndpointReply> {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let handler = {
        let serve = serve.clone();
        tokio::spawn(async move { handle(server, &serve).await })
    };
    let (reader, writer) = tokio::io::split(client);
    let mut channel = Channel::new(reader, writer);
    channel.send(&request).await.unwrap();
    let mut replies = Vec::new();
    while let Ok(Some(frame)) = channel.receive::<EndpointReply>().await {
        replies.push(frame.message);
    }
    handler.await.unwrap();
    replies
}

/// `status` and `keep_set`, each answered within a second of the paused clock.
async fn endpoint_answers(serve: &Serve) -> (Snapshot, Vec<String>) {
    let one_second = Duration::from_secs(1);
    let status = tokio::time::timeout(
        one_second,
        ask(serve, serde_json::json!({"type": "status", "v": 1})),
    )
    .await
    .expect("status answered promptly");
    let snapshot = match &status[..] {
        [EndpointReply::Status { snapshot, .. }] => {
            serde_json::from_value(snapshot.clone()).unwrap()
        }
        other => panic!("{other:?}"),
    };
    let keep = tokio::time::timeout(
        one_second,
        ask(serve, serde_json::json!({"type": "keep_set", "v": 1})),
    )
    .await
    .expect("keep_set answered promptly");
    let versions = match keep.as_slice() {
        [EndpointReply::KeepSet { versions, .. }] => versions.clone(),
        other => panic!("{other:?}"),
    };
    (snapshot, versions)
}

#[tokio::test(start_paused = true)]
async fn t3_a_caller_holding_the_update_lock_blocks_neither_the_endpoint_nor_the_supervisor_for_good()
 {
    // A regression (a prune or install that waits for the lock) blocks the
    // core's own thread, so the script that would release the lock never runs
    // and the test hangs instead of failing. The case runs in a child of the
    // test binary under a parent deadline that kills it.
    let Some(done) = crate::testkit::isolated::child_dir() else {
        crate::testkit::isolated::run_in_child(
            module_path!(),
            "t3_a_caller_holding_the_update_lock_blocks_neither_the_endpoint_nor_the_supervisor_for_good",
            Duration::from_secs(60),
        );
        return;
    };
    let dir = Dir::new();
    let installed = dir.root().join("installed").join(NAME);
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::write(&installed, b"old binary").unwrap();
    let old_sha = open_ferry_update::staging::hash_file(&installed).unwrap();
    let kept = dir.data.binary("2.0.0", NAME);
    std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
    std::fs::write(&kept, b"new binary").unwrap();
    let new_sha = open_ferry_update::staging::hash_file(&kept).unwrap();
    dir.versions(&["0.0.1", "0.0.2"]);
    let mut rig = rig_over(&dir).await;
    rig.disk.real_install = true;
    rig.disk.add_artifact("2.0.0", &new_sha);
    rig.rt.as_mut().unwrap().journal.as_mut().unwrap().installed = Some(Installed {
        version: "1.0.0".to_owned(),
        sha256: old_sha,
    });
    let watch = rig.cfg.timings.watch;
    let command_tx = rig.command_tx.clone();
    let serve = Serve::new(rig.command_tx.clone(), rig.shared.clone());
    // A caller (the CLI, a stage) holds the update lock for the whole first part.
    let lock = dir.data.try_lock().unwrap().expect("the lock was free");
    let mut core = core_of!(rig);
    let script = async {
        // The first run step's prune found the lock busy and is waiting on its
        // timer; the endpoint answers anyway.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let (shown, _) = endpoint_answers(&serve).await;
        assert!(shown.prune_pending, "the pending prune is shown: {shown:?}");
        // A switch and its confirm: J5 lands, and the install and the prune
        // want the lock too.
        let (tx, mut rx) = unbounded_channel();
        command_tx
            .send(Command::Switch {
                request: crate::core::SwitchRequest {
                    sha256: new_sha.clone(),
                    ..update(false)
                },
                replies: tx,
            })
            .unwrap();
        while rx.recv().await.is_some() {}
        tokio::time::sleep(watch + Duration::from_secs(10)).await;
        let (shown, keep) = endpoint_answers(&serve).await;
        assert!(
            shown.install_pending.is_some(),
            "the install waits: {shown:?}"
        );
        assert!(shown.prune_pending, "the prune waits: {shown:?}");
        assert!(keep.contains(&"2.0.0".to_owned()), "{keep:?}");
        assert_eq!(std::fs::read(&installed).unwrap(), b"old binary");
        assert_eq!(dir.present(), ["0.0.1", "0.0.2", "2.0.0"], "nothing pruned");
        // The caller lets go: both steps run on their timers.
        drop(lock);
        tokio::time::sleep(Duration::from_secs(130)).await;
        let (shown, _) = endpoint_answers(&serve).await;
        assert!(shown.install_pending.is_none(), "{shown:?}");
        assert!(!shown.prune_pending, "{shown:?}");
        assert_eq!(std::fs::read(&installed).unwrap(), b"new binary");
        assert_eq!(
            dir.present(),
            ["0.0.2", "2.0.0"],
            "pruned after the release"
        );
        let _ = command_tx.send(Command::Stop);
    };
    let (exit, ()) = tokio::join!(core.run(), script);
    assert_eq!(exit, Exit::Stopped);
    crate::testkit::isolated::mark_completed(&done);
}

// ---- 5b: the oracle review's rulings --------------------------------------

/// How many times the prune has been asked of the disk.
fn prunes_asked(disk: &crate::testkit::FakeDisk) -> usize {
    disk.state.lock().unwrap().prunes.len()
}

#[tokio::test(start_paused = true)]
async fn h1_no_prune_while_the_journal_is_not_durable_or_a_state_write_is_pending() {
    for case in ["J5 landed but not durable", "S2 failed"] {
        let dir = Dir::new();
        dir.versions(&["0.0.1", "0.2.0", "0.3.0", "0.4.0", "1.0.0", "2.0.0"]);
        let mut rig = rig_over(&dir).await;
        if case.starts_with("J5") {
            rig.disk.not_durable_journal("J5", 1);
            // J8 follows the delivered S2 at once; it must not make the
            // journal durable by itself.
            rig.disk.not_durable_journal("J8", 1);
        }
        let watch = rig.cfg.timings.watch;
        let every = rig.cfg.timings.status_every;
        let (tx, mut rx) = unbounded_channel();
        let mut core = core_of!(rig);
        core.handle_switch(update(false), tx).await;
        assert_eq!(
            crate::testkit::done_result(&crate::testkit::drain(&mut rx))["result"],
            "switched"
        );
        if case.starts_with('S') {
            // The confirm's own S2, and the install's patch of it right after:
            // the second would otherwise carry the first and heal it.
            rig.disk.fail_state("S2", 2);
        }
        tokio::time::advance(watch + Duration::from_secs(1)).await;
        core.on_tick().await;
        assert_eq!(
            core.rt.journal.as_ref().unwrap().confirmed.version,
            "2.0.0",
            "{case}: the switch was confirmed"
        );
        // The confirm's prune is held back: a crash now would bring back the
        // older journal or state, whose protections it would have deleted.
        assert_eq!(prunes_asked(&rig.disk), 0, "{case}: the disk was not asked");
        assert_eq!(dir.present().len(), 6, "{case}: nothing was deleted");
        core.publish();
        assert!(
            rig.shared.snapshot().prune_pending,
            "{case}: the retry is shown"
        );
        // The write that was owed lands on its timer, and the prune goes after.
        tokio::time::advance(every + Duration::from_secs(1)).await;
        core.on_tick().await;
        assert_eq!(prunes_asked(&rig.disk), 1, "{case}: pruned once durable");
        assert_eq!(
            dir.present(),
            ["0.4.0", "1.0.0", "2.0.0"],
            "{case}: pruned after"
        );
        core.publish();
        assert!(!rig.shared.snapshot().prune_pending, "{case}");
    }
}

// Fix 1: the keep set is gated like the prune. While a journal or state write
// is not durably what memory says, the real `keep_set` handler answers an
// error (and the snapshot shows `keep: null`) even when a success was cached
// before; once a retry is durable it answers a set that keeps what the durable
// journal names.
#[tokio::test(start_paused = true)]
async fn the_keep_set_is_not_offered_while_a_write_is_not_durable() {
    for case in ["J5 landed but not durable", "S2 failed"] {
        let dir = Dir::new();
        dir.versions(&["0.0.1", "0.2.0", "0.3.0", "0.4.0", "1.0.0", "2.0.0"]);
        let mut rig = rig_over(&dir).await;
        if case.starts_with("J5") {
            rig.disk.not_durable_journal("J5", 1);
            rig.disk.not_durable_journal("J8", 1);
        }
        let watch = rig.cfg.timings.watch;
        let every = rig.cfg.timings.status_every;
        let serve = Serve::new(rig.command_tx.clone(), rig.shared.clone());
        let (tx, mut rx) = unbounded_channel();
        let mut core = core_of!(rig);
        // A success is cached while everything is durable.
        core.publish();
        assert!(
            rig.shared.snapshot().keep.is_some(),
            "{case}: cached success"
        );
        core.handle_switch(update(false), tx).await;
        assert_eq!(
            crate::testkit::done_result(&crate::testkit::drain(&mut rx))["result"],
            "switched"
        );
        if case.starts_with('S') {
            rig.disk.fail_state("S2", 2);
        }
        tokio::time::advance(watch + Duration::from_secs(1)).await;
        core.on_tick().await;
        core.publish();
        assert_eq!(
            rig.shared.snapshot().keep,
            None,
            "{case}: the status shows none"
        );
        let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
        assert!(
            matches!(&replies[..], [EndpointReply::Error { .. }]),
            "{case}: {replies:?}"
        );
        // The owed write lands on its timer.
        tokio::time::advance(every + Duration::from_secs(1)).await;
        core.on_tick().await;
        core.publish();
        let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
        match &replies[..] {
            [EndpointReply::KeepSet { versions, .. }] => {
                for kept in ["0.4.0", "1.0.0", "2.0.0"] {
                    assert!(versions.contains(&kept.to_owned()), "{case}: {versions:?}");
                }
            }
            other => panic!("{case}: {other:?}"),
        }
    }
}

// The ruled scenario. The durable journal names `previous = C`. J5 sets A and
// lands on disk, but its sync fails; S2 lands in the state file, but its sync
// fails too. A version newer than C that nothing protects exists. While those
// writes are not durable the real `keep_set` handler answers an error (a crash
// would bring back the older journal, whose protections a narrower set would
// delete); once a retry is durable it answers a set that keeps what the durable
// journal names. The supervisor's S2 goes through `save_named` -> `save`, which
// returns an error on a write that landed unsynced, so the gate shuts today;
// this guards against that changing.
#[tokio::test(start_paused = true)]
async fn the_keep_set_waits_for_a_journal_and_a_state_write_that_landed_unsynced() {
    for case in ["J5 and S2 landed unsynced", "S2 landed unsynced alone"] {
        let dir = Dir::new();
        let c = "0.2.0";
        dir.versions(&["0.0.1", c, "0.3.0", "1.0.0", "2.0.0"]);
        let mut rig = rig_over(&dir).await;
        // The durable journal: B (1.0.0) active, `previous = C`.
        {
            let journal = rig.rt.as_mut().unwrap().journal.as_mut().unwrap();
            journal.previous = Some(artifact(c));
            let mut durable = journal.clone();
            dir.store().write(&mut durable, "init-previous").unwrap();
        }
        rig.disk.real_state = true;
        rig.disk.real_journal = true;
        if case.starts_with("J5") {
            rig.disk.not_durable_journal("J5", 1);
            rig.disk.not_durable_journal("J8", 1);
        }
        let watch = rig.cfg.timings.watch;
        let every = rig.cfg.timings.status_every;
        let serve = Serve::new(rig.command_tx.clone(), rig.shared.clone());
        let (tx, mut rx) = unbounded_channel();
        let mut core = core_of!(rig);
        core.handle_switch(update(false), tx).await;
        assert_eq!(
            crate::testkit::done_result(&crate::testkit::drain(&mut rx))["result"],
            "switched"
        );
        // The confirm's own S2, and the install's patch of it, land unsynced.
        rig.disk.not_durable_state("S2", 2);
        tokio::time::advance(watch + Duration::from_secs(1)).await;
        core.on_tick().await;
        core.publish();
        // The bytes really are on disk: J5's journal names A, and the state file
        // holds the records.
        let on_disk = |dir: &Dir| match dir.store().load().unwrap() {
            open_ferry_update::journal::JournalLoad::Loaded(journal) => journal,
            other => panic!("{other:?}"),
        };
        let landed = on_disk(&dir);
        assert_eq!(landed.active.version, "2.0.0", "{case}: J5 landed");
        assert_eq!(landed.confirmed.version, "2.0.0", "{case}: J5 confirmed");
        assert_eq!(
            landed
                .previous
                .as_ref()
                .map(|previous| previous.version.as_str()),
            Some("1.0.0"),
            "{case}: J5 moved previous off C"
        );
        let state = open_ferry_update::state::State::load_strict(&dir.data.state_file())
            .unwrap_or_else(|error| panic!("{case}: S2 landed unreadable: {error}"));
        assert_eq!(state.history.len(), 1, "{case}: {state:?}");
        let record = &state.history[0];
        assert_eq!(
            (
                record.from.as_str(),
                record.to.as_str(),
                record.result.as_deref()
            ),
            ("1.0.0", "2.0.0", Some("switched")),
            "{case}: S2's record"
        );
        assert!(record.confirmed, "{case}: S2 records the confirmation");
        assert_eq!(
            state.last_switch.as_ref().map(|last| last.id.as_str()),
            Some(record.id.as_str()),
            "{case}: S2's last switch"
        );
        assert_eq!(
            landed
                .outcomes
                .first()
                .map(|outcome| outcome.record.id.as_str()),
            Some(record.id.as_str()),
            "{case}: the journal and the state name the same switch"
        );
        let orders = rig.disk.order();
        assert!(
            orders.iter().any(|step| step == "S2~")
                && (orders.iter().any(|step| step == "J5~") || !case.starts_with("J5")),
            "{case}: {orders:?}"
        );
        assert_eq!(
            rig.shared.snapshot().keep,
            None,
            "{case}: the status shows none"
        );
        let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
        assert!(
            matches!(&replies[..], [EndpointReply::Error { .. }]),
            "{case}: not offered while not durable: {replies:?}"
        );
        assert_eq!(prunes_asked(&rig.disk), 0, "{case}: no prune either");
        assert_eq!(dir.present().len(), 5, "{case}: nothing was deleted");
        // The retries land durably on their timers: S2 first, then the journal
        // write J8 that followed it (which also reported not durable once).
        for _ in 0..2 {
            tokio::time::advance(every + Duration::from_secs(1)).await;
            core.on_tick().await;
        }
        core.publish();
        let durable = on_disk(&dir);
        let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
        match &replies[..] {
            [EndpointReply::KeepSet { versions, .. }] => {
                let named = [
                    Some(&durable.active),
                    Some(&durable.confirmed),
                    durable.previous.as_ref(),
                ];
                for artifact in named.into_iter().flatten() {
                    assert!(
                        versions.contains(&artifact.version),
                        "{case}: {}: {versions:?}",
                        artifact.version
                    );
                }
            }
            other => panic!("{other:?}"),
        }
    }
}

// The production save contract, through the real writer and the real seam. The
// state file lands, then its directory sync fails (`atomic.landed:` in the data
// directory's seam dir). The supervisor's state write must report that as an
// error, so the outcome stays owed and nothing is pruned or offered as a keep
// set: a write that said `Ok` here (`save_committed_named`) would let a crash
// bring back an older state whose protections the prune had deleted. Nothing
// is scripted in the fake disk.
#[cfg(feature = "blue-green-test")]
#[tokio::test(start_paused = true)]
async fn a_state_write_that_landed_but_could_not_sync_stays_owed_through_the_real_writer() {
    let dir = Dir::new();
    dir.versions(&["0.0.1", "0.2.0", "0.3.0", "0.4.0", "1.0.0", "2.0.0"]);
    let mut rig = rig_over(&dir).await;
    rig.disk.real_state = true;
    let watch = rig.cfg.timings.watch;
    let every = rig.cfg.timings.status_every;
    let serve = Serve::new(rig.command_tx.clone(), rig.shared.clone());
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.handle_switch(update(false), tx).await;
    assert_eq!(
        crate::testkit::done_result(&crate::testkit::drain(&mut rx))["result"],
        "switched"
    );
    let seams = dir.root().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    let fail = seams.join("atomic.landed_update-state.json.fail");
    std::fs::write(&fail, b"").unwrap();
    tokio::time::advance(watch + Duration::from_secs(1)).await;
    core.on_tick().await;
    core.publish();
    // The seam fired, and the file did land: only the sync was reported failed.
    let fired = std::fs::metadata(seams.join("atomic.landed_update-state.json.failed"))
        .map(|receipt| receipt.len())
        .unwrap_or(0);
    assert!(fired >= 1, "the landed seam fired {fired} times");
    let landed = open_ferry_update::state::State::load_strict(&dir.data.state_file()).unwrap();
    assert!(
        landed.history.iter().any(|record| record.confirmed),
        "the confirmation landed in the file: {landed:?}"
    );
    // Still owed: shown, and holding back the prune and the keep set.
    let shown = rig.shared.snapshot();
    assert!(shown.state_write_pending, "{shown:?}");
    assert!(shown.prune_pending, "{shown:?}");
    assert_eq!(shown.keep, None, "no keep set is offered");
    let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
    assert!(
        matches!(&replies[..], [EndpointReply::Error { .. }]),
        "{replies:?}"
    );
    assert_eq!(prunes_asked(&rig.disk), 0, "the disk was not asked");
    assert_eq!(dir.present().len(), 6, "nothing was deleted");
    // Once the sync works, the retry lands and the prune goes after it.
    std::fs::remove_file(&fail).unwrap();
    tokio::time::advance(every + Duration::from_secs(1)).await;
    core.on_tick().await;
    core.publish();
    let shown = rig.shared.snapshot();
    assert!(!shown.state_write_pending, "{shown:?}");
    assert_eq!(prunes_asked(&rig.disk), 1, "pruned once durable");
}

// A state file the supervisor can't parse is not replaced by defaults when it
// writes the outcome: the write fails, the outcome stays owed (the status shows
// it, and the prune and the keep set hold off), the file's bytes are untouched
// and no version is deleted.
#[tokio::test(start_paused = true)]
async fn a_state_that_does_not_parse_refuses_the_write_and_nothing_is_pruned() {
    let dir = Dir::new();
    dir.versions(&["0.0.1", "0.2.0", "0.3.0", "0.4.0", "1.0.0", "2.0.0"]);
    let garbage: &[u8] = b"\x00\xfenot json, but it may hold a staged version";
    std::fs::write(dir.data.state_file(), garbage).unwrap();
    let mut rig = rig_over(&dir).await;
    rig.disk.real_state = true;
    let watch = rig.cfg.timings.watch;
    let every = rig.cfg.timings.status_every;
    let serve = Serve::new(rig.command_tx.clone(), rig.shared.clone());
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.handle_switch(update(false), tx).await;
    assert_eq!(
        crate::testkit::done_result(&crate::testkit::drain(&mut rx))["result"],
        "switched"
    );
    tokio::time::advance(watch + Duration::from_secs(1)).await;
    core.on_tick().await;
    // Retries on the timer meet the same file.
    tokio::time::advance(every + Duration::from_secs(1)).await;
    core.on_tick().await;
    core.publish();
    let shown = rig.shared.snapshot();
    assert!(
        shown.state_write_pending,
        "the owed write is shown: {shown:?}"
    );
    assert!(shown.prune_pending, "the prune waits: {shown:?}");
    assert_eq!(shown.keep, None, "no keep set is offered");
    let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
    assert!(
        replies.iter().all(
            |reply| !matches!(reply, EndpointReply::KeepSet { versions, .. } if versions.len() < 6)
        ),
        "no narrowed set: {replies:?}"
    );
    assert_eq!(std::fs::read(dir.data.state_file()).unwrap(), garbage);
    assert_eq!(prunes_asked(&rig.disk), 0, "the disk was not asked");
    assert_eq!(dir.present().len(), 6, "nothing was deleted");
    assert!(
        rig.disk
            .state
            .lock()
            .unwrap()
            .state_writes
            .iter()
            .all(|(_, _, landed)| !landed),
        "no write landed"
    );
}

#[tokio::test(start_paused = true)]
async fn h2_a_keep_set_that_is_not_available_is_an_error_not_an_empty_list() {
    let rig = Rig::new().await;
    let shared = crate::snapshot::Shared::new();
    let serve = Serve::new(rig.command_tx.clone(), shared.clone());
    let not_available = |replies: Vec<EndpointReply>, when: &str| match &replies[..] {
        [EndpointReply::Error { reason, .. }] => {
            assert!(reason.contains("keep set"), "{when}: {reason}");
        }
        other => panic!("{when}: {other:?}"),
    };
    // Before the first publish.
    let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
    not_available(replies, "before the first publish");
    // A set that is there is answered.
    shared.publish(Snapshot {
        keep: Some(strings(&["1.0.0"])),
        ..Snapshot::default()
    });
    let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
    assert!(
        matches!(&replies[..], [EndpointReply::KeepSet { versions, .. }] if versions == &strings(&["1.0.0"])),
        "{replies:?}"
    );
    // While the supervisor starts over after an internal error.
    shared.publish(crate::supervise::restarting_snapshot());
    let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
    not_available(replies, "restarting");
}

#[tokio::test(start_paused = true)]
async fn h3_a_listing_that_fails_gives_no_keep_set_rather_than_a_partial_one() {
    // The real disk, with a `versions/` that cannot be listed.
    let dir = Dir::new();
    std::fs::write(dir.data.versions(), b"not a directory").unwrap();
    let mut rig = rig_over(&dir).await;
    let serve = Serve::new(rig.command_tx.clone(), rig.shared.clone());
    let mut core = core_of!(rig);
    core.publish();
    assert_eq!(rig.shared.snapshot().keep, None, "no set at all");
    let replies = ask(&serve, serde_json::json!({"type": "keep_set", "v": 1})).await;
    assert!(
        matches!(&replies[..], [EndpointReply::Error { .. }]),
        "{replies:?}"
    );
    // The prune refuses too, and says it will try again.
    core.try_prune().await;
    core.publish();
    assert!(rig.shared.snapshot().prune_pending);
}

#[tokio::test(start_paused = true)]
async fn h3_the_every_version_list_is_fallible_on_both_paths_to_it() {
    let mut rig = Rig::new().await;
    let every = rig.cfg.timings.status_every;
    rig.disk.state.lock().unwrap().versions = ["0.1.0", "0.2.0"].map(str::to_owned).into();
    let mut core = core_of!(rig);
    // The keep set cannot be computed: every version, with the tracked ones.
    rig.disk.state.lock().unwrap().keep_fails = Some("no keep set".to_owned());
    core.publish();
    assert_eq!(
        rig.shared.snapshot().keep,
        Some(strings(&["0.1.0", "0.2.0", "1.0.0"]))
    );
    // And the listing fails too: nothing, not the tracked versions alone.
    rig.disk.state.lock().unwrap().versions_fail = Some("no listing".to_owned());
    tokio::time::advance(every + Duration::from_secs(1)).await;
    core.publish();
    assert_eq!(rig.shared.snapshot().keep, None);
    // `keep_all` lists every version, so it has the same failure.
    rig.disk.state.lock().unwrap().keep_fails = None;
    core.rt.keep_all = true;
    tokio::time::advance(every + Duration::from_secs(1)).await;
    core.publish();
    assert_eq!(rig.shared.snapshot().keep, None);
    rig.disk.state.lock().unwrap().versions_fail = None;
    tokio::time::advance(every + Duration::from_secs(1)).await;
    core.publish();
    assert_eq!(
        rig.shared.snapshot().keep,
        Some(strings(&["0.1.0", "0.2.0", "1.0.0"]))
    );
}

#[tokio::test(start_paused = true)]
async fn m4_the_keep_reply_names_a_pinned_version_before_its_pin_file_goes() {
    let dir = Dir::new();
    dir.versions(&["0.5.0", "1.0.0"]);
    let mut rig = rig_over(&dir).await;
    // What the endpoint showed at the moment the pin was consumed.
    let seen: std::sync::Arc<std::sync::Mutex<Vec<Option<Vec<String>>>>> = Default::default();
    {
        let (shared, seen) = (rig.shared.clone(), seen.clone());
        rig.disk.state.lock().unwrap().on_consume = Some(std::sync::Arc::new(move || {
            seen.lock().unwrap().push(shared.snapshot().keep);
        }));
    }
    let now = rig.clock.now();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    // A reply cached before the pin was written: it does not name 2.0.0.
    core.publish();
    assert_eq!(
        rig.shared.snapshot().keep.expect("a keep set"),
        strings(&["0.5.0", "1.0.0"])
    );
    let pin = dir.pins().create("2.0.0", SHA_2, now).unwrap();
    rig.disk.add_pin(&pin.id);
    core.handle_switch(
        crate::core::SwitchRequest {
            pin: pin.id,
            ..update(false)
        },
        tx,
    )
    .await;
    assert_eq!(
        crate::testkit::done_result(&crate::testkit::drain(&mut rx))["result"],
        "switched"
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "the pin was consumed once");
    let shown = seen[0].clone().expect("a keep set was published");
    assert!(
        shown.contains(&"2.0.0".to_owned()),
        "the reply named 2.0.0 while its pin file still stood: {shown:?}"
    );
}

// Fix 8: the deferred path of M4 (`run_deferred`). J1 lands but is not synced,
// so the pin waits; a later durable write consumes it, and the keep set is
// republished before the file goes, naming the pinned version.
#[tokio::test(start_paused = true)]
async fn m4_a_deferred_pin_consumption_republishes_the_keep_reply_before_the_pin_goes() {
    let dir = Dir::new();
    dir.versions(&["0.5.0", "1.0.0"]);
    let mut rig = rig_over(&dir).await;
    rig.disk.not_durable_journal("J1", 1);
    let seen: std::sync::Arc<std::sync::Mutex<Vec<Option<Vec<String>>>>> = Default::default();
    {
        let (shared, seen) = (rig.shared.clone(), seen.clone());
        rig.disk.state.lock().unwrap().on_consume = Some(std::sync::Arc::new(move || {
            seen.lock().unwrap().push(shared.snapshot().keep);
        }));
    }
    let now = rig.clock.now();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.publish();
    let pin = dir.pins().create("2.0.0", SHA_2, now).unwrap();
    rig.disk.add_pin(&pin.id);
    core.handle_switch(
        crate::core::SwitchRequest {
            pin: pin.id,
            ..update(false)
        },
        tx,
    )
    .await;
    assert_eq!(
        crate::testkit::done_result(&crate::testkit::drain(&mut rx))["result"],
        "switched"
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "the pin was consumed once, after it waited");
    let shown = seen[0].clone().expect("a keep set was published");
    assert!(
        shown.contains(&"2.0.0".to_owned()),
        "the reply named 2.0.0 while its pin file still stood: {shown:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn l8_a_prune_that_could_not_remove_the_expired_pins_is_reported_as_partial_and_retried() {
    let mut rig = Rig::new().await;
    let every = rig.cfg.timings.status_every;
    let max = rig.cfg.timings.install_retry_max;
    rig.disk
        .state
        .lock()
        .unwrap()
        .prune_script
        .push_back(crate::disk::PruneOutcome::Partial {
            removed: vec!["0.0.1".to_owned()],
            why: "a pin could not be removed".to_owned(),
        });
    let mut core = core_of!(rig);
    core.publish();
    let computed = rig.disk.state.lock().unwrap().keep_calls;
    core.try_prune().await;
    core.publish();
    assert!(rig.shared.snapshot().prune_pending, "it is retried");
    assert_eq!(core.rt.prune_refusals, 1, "on the refusal backoff");
    assert!(
        rig.disk.state.lock().unwrap().keep_calls > computed,
        "what was deleted is gone, so the reply is computed again"
    );
    tokio::time::advance(max.max(every) + Duration::from_secs(1)).await;
    core.on_tick().await;
    assert_eq!(prunes_asked(&rig.disk), 2, "the retry ran");
    assert_eq!(core.rt.prune_refusals, 0, "and finished");
    core.publish();
    assert!(!rig.shared.snapshot().prune_pending);
}

/// Makes removing the pin file `file` (in `dir`) fail until the guard is
/// dropped. `None` when this process cannot make a removal fail (a Unix
/// superuser, to whom a directory's mode means nothing).
struct Unremovable {
    #[cfg(unix)]
    dir: PathBuf,
    #[cfg(windows)]
    _handle: std::fs::File,
}

impl Unremovable {
    #[cfg(windows)]
    fn hold(_dir: &Path, file: &Path) -> Option<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        // Open without FILE_SHARE_DELETE: a removal is a sharing violation.
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(file)
            .unwrap();
        Some(Self { _handle: handle })
    }

    #[cfg(unix)]
    fn hold(dir: &Path, _file: &Path) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let guard = Self {
            dir: dir.to_owned(),
        };
        let probe = dir.join(".probe");
        if std::fs::File::create(&probe).is_ok() {
            let _ = std::fs::remove_file(&probe);
            return None;
        }
        Some(guard)
    }
}

#[cfg(unix)]
impl Drop for Unremovable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
    }
}

#[tokio::test(start_paused = true)]
async fn l8_a_pin_the_real_disk_cannot_remove_leaves_the_prune_pending_after_the_versions_went() {
    let dir = Dir::new();
    dir.versions(&["0.0.1", "0.0.2", "1.0.0"]);
    let mut rig = rig_over(&dir).await;
    let now = rig.clock.now();
    let expired = dir
        .pins()
        .create("0.0.9", SHA_1, now - chrono::Duration::days(30))
        .unwrap();
    let Some(guard) = Unremovable::hold(dir.pins().dir(), &dir.pin_file(&expired.id)) else {
        eprintln!("skipped: this process can remove files from a read-only directory");
        return;
    };
    let max = rig.cfg.timings.install_retry_max;
    let mut core = core_of!(rig);
    core.publish();
    let computed = rig.disk.state.lock().unwrap().keep_calls;
    core.try_prune().await;
    assert_eq!(
        dir.present(),
        ["0.0.2", "1.0.0"],
        "the version directories were deleted"
    );
    // The real disk said so: the versions went, the pin did not.
    let outcomes = rig.disk.state.lock().unwrap().real_outcomes.clone();
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert!(
        outcomes[0].starts_with("Partial { removed: [\"0.0.1\"]"),
        "{outcomes:?}"
    );
    // What was deleted is gone, so the core publishes a freshly computed set.
    core.publish();
    assert!(
        rig.disk.state.lock().unwrap().keep_calls > computed,
        "the keep set was computed again after the partial prune"
    );
    assert!(dir.pin_file(&expired.id).exists(), "the pin is still there");
    core.publish();
    assert!(
        rig.shared.snapshot().prune_pending,
        "the pins are retried, not forgotten"
    );
    drop(guard);
    tokio::time::advance(max + Duration::from_secs(1)).await;
    core.on_tick().await;
    assert!(!dir.pin_file(&expired.id).exists(), "removed on the retry");
    core.publish();
    assert!(!rig.shared.snapshot().prune_pending);
}
