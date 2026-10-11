//! Not upstream's: round 6. The real start-up path (recovery, the files, the
//! journal) over a scratch data directory, with the Core built from what it
//! returns, so what start-up leaves behind is checked against what the Core
//! does next. The driver is the scripted one.

use std::time::Duration;

use open_ferry_update::journal::{
    Admitted, Artifact, Blame, How, Installed, Journal, JournalLoad, JournalStore, LadderRecord,
    Phase, Return, Rung, Switch,
};
use open_ferry_update::staging::hash_file;
use tokio::time::Instant;

use crate::core::Runtime;
use crate::disk::RealDisk;
use crate::startup::{Started, start};
use crate::testkit::{NAME, Rig, ScriptedRunner, SpawnFail, core_of};

/// The scratch directories one test owns.
pub(crate) struct Dirs {
    _tmp: tempfile::TempDir,
    _sockets: Option<tempfile::TempDir>,
}

/// A rig whose configuration points at real files. The data directory's trust
/// check looks at every ancestor, so it is under `$HOME` on Unix (not `/tmp`),
/// and under the working directory (the package) elsewhere.
pub(crate) async fn real_rig() -> (Rig, Dirs) {
    let mut rig = Rig::new().await;
    // The rig's own worker is not wanted: start-up begins from nothing.
    rig.rt = None;
    rig.registry = crate::driver::Registry::new();
    rig.driver = crate::testkit::FakeDriver::default();
    let tmp = if cfg!(unix) {
        let home = std::env::var_os("HOME").expect("HOME is set");
        tempfile::Builder::new()
            .prefix("ofp-s8-start-")
            .tempdir_in(home)
            .unwrap()
    } else {
        tempfile::tempdir_in(".").unwrap()
    };
    rig.cfg.data = open_ferry_update::data_dir::DataDir::at(tmp.path().join("data"));
    std::fs::create_dir_all(tmp.path().join("bin")).unwrap();
    rig.cfg.installed = tmp.path().join("bin").join(NAME);
    rig.cfg.config = tmp.path().join("config.yaml");
    // The same answer for every binary: staging asks the installed file.
    rig.cfg.runner = std::sync::Arc::new(ScriptedRunner(Ok("open-ferry 1.0.0".to_owned())));
    let sockets = own_endpoint(&mut rig);
    (
        rig,
        Dirs {
            _tmp: tmp,
            _sockets: sockets,
        },
    )
}

/// On Unix the default endpoint is one per user; the test binds its own. The
/// socket directory is short (a socket path is limited) and under `$HOME`.
#[cfg(unix)]
fn own_endpoint(rig: &mut Rig) -> Option<tempfile::TempDir> {
    let home = std::env::var_os("HOME").expect("HOME is set");
    let dir = tempfile::Builder::new()
        .prefix("o-")
        .tempdir_in(home)
        .unwrap();
    rig.cfg.endpoint_location = Some(crate::endpoint::Location::new(dir.path(), "e").unwrap());
    Some(dir)
}

/// Windows derives the pipe from the data directory.
#[cfg(windows)]
fn own_endpoint(_rig: &mut Rig) -> Option<tempfile::TempDir> {
    None
}

/// `versions/<version>/<binary>` holding `bytes`.
pub(crate) fn keep(rig: &Rig, version: &str, bytes: &[u8]) -> Artifact {
    let binary = rig.cfg.data.binary(version, NAME);
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::write(&binary, bytes).unwrap();
    Artifact {
        version: version.to_owned(),
        sha256: hash_file(&binary).unwrap(),
        size: bytes.len() as u64,
    }
}

pub(crate) fn store(rig: &Rig) -> JournalStore {
    JournalStore::new(rig.cfg.data.root())
}

pub(crate) fn loaded(rig: &Rig) -> Journal {
    match store(rig).load().unwrap() {
        JournalLoad::Loaded(journal) => journal,
        other => panic!("no journal: {other:?}"),
    }
}

/// A journal with a return open: `from` (A) was confirmed, the switch to `to`
/// (B) failed, and the return to `target` is journalled.
pub(crate) fn open_return(
    rig: &Rig,
    from: &Artifact,
    to: &Artifact,
    previous: Option<&Artifact>,
    target: &Artifact,
    on_installed: bool,
    installed: &Artifact,
) {
    let mut journal = Journal::first_start(from.clone(), Some(Installed::from(installed)));
    journal.previous = previous.cloned();
    journal.switch = Some(Switch {
        id: "sw-000000000000aaaa-1".to_owned(),
        how: How::Update,
        from: from.clone(),
        to: to.clone(),
        phase: Phase::TakingOver,
        started: chrono::Utc::now().to_rfc3339(),
        watch_until: None,
        admitted: Admitted::default(),
        return_: Some(Return {
            target: target.clone(),
            blame: Blame::NewVersion,
            why: "B rejected".to_owned(),
            decided: chrono::Utc::now().to_rfc3339(),
            installed: on_installed,
        }),
    });
    store(rig).write(&mut journal, "fixture").unwrap();
}

/// Runs the Core's restart tick `times` times, past the backoff each time,
/// noting how many workers each started.
macro_rules! ticks {
    ($core:ident, $rig:ident, $times:expr) => {{
        let mut started = Vec::new();
        for _ in 0..$times {
            tokio::time::advance(Duration::from_secs(70)).await;
            let before = $rig.driver.places().len();
            $core.on_tick().await;
            started.push($rig.driver.places().len() - before);
        }
        started
    }};
}

pub(crate) fn real_disk(rig: &mut Rig) {
    rig.disk.real = Some(RealDisk::new(
        rig.cfg.data.clone(),
        rig.cfg.installed.clone(),
        NAME.to_owned(),
        store(rig),
    ));
    rig.disk.real_install = true;
    // What the Core writes lands in the real journal too, for the next start-up.
    rig.disk.real_journal = true;
}

/// The fixture of round 6 item 1: A (the return's `from`) fails its hash, P is
/// `previous`, and the installed file is a third version, I.
struct Fallback {
    a: Artifact,
    p: Artifact,
    i: Artifact,
}

fn fallback(rig: &Rig) -> Fallback {
    let a = keep(rig, "1.0.0", b"binary one");
    let p = keep(rig, "0.9.0", b"binary nine");
    let b = keep(rig, "2.0.0", b"binary two");
    std::fs::write(&rig.cfg.installed, b"binary installed").unwrap();
    let i = Artifact {
        version: "0.5.0".to_owned(),
        sha256: hash_file(&rig.cfg.installed).unwrap(),
        size: 16,
    };
    open_return(rig, &a, &b, Some(&p), &a, false, &i);
    // A's kept copy no longer matches its hash.
    std::fs::write(rig.cfg.data.binary("1.0.0", NAME), b"garbage").unwrap();
    Fallback { a, p, i }
}

/// Item 1 (a), (b): real recovery picks P (A fails its hash), then P's
/// start-up spawn fails. The Core built from that start tries P again before
/// the installed rung, and does not begin a new cycle at A.
#[tokio::test]
async fn recovery_keeps_the_fallback_it_chose_on_the_return_ladder() {
    let (mut rig, _dirs) = real_rig().await;
    let f = fallback(&rig);
    rig.driver.fail_spawn("0.9.0", SpawnFail::Start);
    let started = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert_journalled(&started);
    assert_eq!(started.run.as_ref().unwrap().artifact, f.p);
    assert!(started.run_failed);
    let held = loaded(&rig);
    assert_eq!(
        held.switch
            .as_ref()
            .unwrap()
            .return_
            .as_ref()
            .unwrap()
            .target,
        f.p
    );
    tokio::time::pause();
    seat_expecting(&mut rig, &started, true);
    let before = rig.driver.places().len();
    let mut core = core_of!(rig);
    let counts = ticks!(core, rig, 3);
    let places = rig.driver.places()[before..].to_vec();
    // Start-up made P's first try; two more, then the installed rung. Never A.
    assert_eq!(
        places,
        ["0.9.0 kept", "0.9.0 kept", "0.5.0 installed"],
        "{counts:?} {places:?}"
    );
    // Checked after the spawns, so the spawn sequence fails by itself when
    // `previous` is dropped; this then names the cause.
    assert_eq!(
        held.previous.as_ref(),
        Some(&f.p),
        "the return is still open: recovery keeps the fallback it chose"
    );
}

/// Item 1, the durable variant: the journal says P. P then fails its hash
/// while A verifies. Recovery must not journal A in the same cycle: P's failed
/// verification goes on to the installed rung by hash, and with neither the
/// result is degraded.
#[tokio::test]
async fn a_return_journalled_at_the_fallback_never_goes_back_to_the_version_it_left() {
    let (mut rig, _dirs) = real_rig().await;
    // The installed file reports its own version: staging finds it free.
    rig.cfg.runner = std::sync::Arc::new(ScriptedRunner(Ok("open-ferry 0.5.0".to_owned())));
    let f = fallback(&rig);
    // P's worker does not start, so the return stays open.
    rig.driver.fail_spawn("0.9.0", SpawnFail::Start);
    let first = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert_journalled(&first);
    assert_eq!(first.run.as_ref().unwrap().artifact, f.p);
    drop(first);
    rig.registry = crate::driver::Registry::new();
    // Now P is the one that fails its hash, and A is whole again.
    std::fs::write(rig.cfg.data.binary("0.9.0", NAME), b"garbage").unwrap();
    keep(&rig, "1.0.0", b"binary one");
    // The installed file is no version the journal knows.
    std::fs::write(&rig.cfg.installed, b"a stranger").unwrap();
    let second = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert_journalled(&second);
    assert!(second.run.is_none(), "{:?}", second.run);
    assert!(second.degraded.is_some());
    let held = loaded(&rig);
    assert_eq!(
        held.switch
            .as_ref()
            .unwrap()
            .return_
            .as_ref()
            .unwrap()
            .target,
        f.p,
        "A was not journalled as the return's target"
    );
    assert_eq!(held.confirmed, f.a);
    drop(second);
    rig.registry = crate::driver::Registry::new();
    // With the installed file back (by hash), the record still says P: start-up
    // runs the rung the ladder is on, and its garbage bytes are failed tries
    // (the driver's hash check), three in all. Then the installed file is the
    // next rung, and never A.
    std::fs::write(&rig.cfg.installed, b"binary installed").unwrap();
    for tries in [2, 3] {
        let again = boot(&mut rig).await;
        assert_eq!(again.run.as_ref().unwrap().artifact, f.p);
        assert!(again.run_failed, "P's bytes are garbage");
        assert_eq!(recorded_attempts(&rig), Some(tries));
    }
    let last = boot(&mut rig).await;
    let run = last.run.as_ref().expect("the installed rung");
    assert_eq!(run.artifact.sha256, f.i.sha256);
    assert_eq!(run.binary, rig.cfg.installed);
}

/// Item 2: the installed rung runs from `cfg.installed`, not from the kept
/// copy of the same bytes, and a failed start-up try is charged to it.
#[tokio::test]
async fn the_installed_rung_runs_from_the_installed_file_and_its_failure_is_charged_there() {
    let (mut rig, _dirs) = real_rig().await;
    let a = keep(&rig, "1.0.0", b"binary one");
    let b = keep(&rig, "2.0.0", b"binary two");
    std::fs::write(&rig.cfg.installed, b"binary one").unwrap();
    // A return journalled at the installed file; the kept copy of A holds the
    // same bytes.
    open_return(&rig, &a, &b, None, &a, true, &a);
    rig.driver.fail_spawn("1.0.0", SpawnFail::StartInstalled);
    let started = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert_journalled(&started);
    assert_eq!(rig.driver.places(), ["1.0.0 installed"]);
    assert_eq!(
        started.run.as_ref().unwrap().binary,
        rig.cfg.installed,
        "the installed rung is cfg.installed"
    );
    assert!(started.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &started, true);
    let mut core = core_of!(rig);
    let counts = ticks!(core, rig, 4);
    let places = rig.driver.places();
    // Start-up's try and two more are the installed rung's three; then it is
    // spent, and the next tick starts nothing. Rule f: the new cycle after the
    // backoff starts at the first permitted rung, the kept copy of the same
    // bytes, which is a rung of its own and runs from its own file.
    assert_eq!(
        places,
        [
            "1.0.0 installed",
            "1.0.0 installed",
            "1.0.0 installed",
            "1.0.0 kept"
        ],
        "{counts:?}"
    );
    assert_eq!(counts, [1, 1, 0, 1]);
}

/// Makes `path` a file `check_file` refuses: writable by everyone on Unix, and
/// with a write grant to Everyone on Windows.
fn make_untrusted(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    #[cfg(windows)]
    {
        let done = std::process::Command::new("icacls")
            .arg(path)
            .args(["/grant", "*S-1-1-0:(W)"])
            .output()
            .unwrap();
        assert!(done.status.success(), "{done:?}");
    }
}

/// Round 7 item 7: the installed rung is checked as the Core checks it, by
/// hash alone. The driver does not run `check_file` on the installed path (a
/// real install may be linked, or owned by an administrator), so start-up must
/// not refuse it either: switching stays on, the journal is held, and the
/// installed file runs as the rung.
#[tokio::test]
async fn an_installed_file_check_file_would_refuse_still_runs_as_the_installed_rung() {
    let (mut rig, _dirs) = real_rig().await;
    let a = keep(&rig, "1.0.0", b"binary one");
    let b = keep(&rig, "2.0.0", b"binary two");
    std::fs::write(&rig.cfg.installed, b"binary one").unwrap();
    make_untrusted(&rig.cfg.installed);
    let identity = crate::endpoint::Identity::current(rig.cfg.kind).unwrap();
    assert!(
        crate::trust::check_file(
            &rig.cfg.installed,
            &crate::trust::TrustRules::for_identity(&identity)
        )
        .is_err(),
        "the fixture is a file check_file refuses"
    );
    open_return(&rig, &a, &b, None, &a, true, &a);
    let started = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert_journalled(&started);
    assert_eq!(started.degraded, None);
    assert_eq!(
        started.run.as_ref().unwrap().binary,
        rig.cfg.installed,
        "the installed rung is cfg.installed"
    );
    assert_eq!(rig.driver.places(), ["1.0.0 installed"]);
}

// ---- item 3: the attempts survive a restart of the supervisor ---------------

/// A, the kept copy that is tried, P (`previous`), B (the rejected `to`), and
/// the installed file I. With `returning`, a return to A is open; otherwise
/// the journal is idle at A.
fn ladder_fixture(rig: &Rig, returning: bool) -> Fallback {
    let a = keep(rig, "1.0.0", b"binary one");
    let p = keep(rig, "0.9.0", b"binary nine");
    let b = keep(rig, "2.0.0", b"binary two");
    std::fs::write(&rig.cfg.installed, b"binary installed").unwrap();
    let i = Artifact {
        version: "0.5.0".to_owned(),
        sha256: hash_file(&rig.cfg.installed).unwrap(),
        size: 16,
    };
    if returning {
        open_return(rig, &a, &b, Some(&p), &a, false, &i);
    } else {
        let mut journal = Journal::first_start(a.clone(), Some(Installed::from(&i)));
        journal.previous = Some(p.clone());
        store(rig).write(&mut journal, "fixture").unwrap();
    }
    Fallback { a, p, i }
}

/// A test that claims the journalled path starts on it: switching is on and
/// the journal is held. Start-up's disabled and refused arms also run the
/// installed file, with no journal, so "the installed file ran" alone does not
/// tell them apart.
fn assert_journalled(started: &Started) {
    assert_eq!(
        started.switching_disabled, None,
        "this test is meant to run with switching on"
    );
    assert!(started.journal.is_some(), "start-up holds no journal");
}

/// One real start-up, from a fresh registry.
async fn boot(rig: &mut Rig) -> Started {
    rig.registry = crate::driver::Registry::new();
    let started = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert_journalled(&started);
    started
}

/// The Core's runtime, built from what start-up returned, for a test on the
/// journalled path. `returning` says whether a return is open: the Core is
/// then on the return ladder, otherwise on the restart ladder.
fn seat_expecting(rig: &mut Rig, started: &Started, returning: bool) {
    assert_journalled(started);
    seat(rig, started);
    assert_eq!(
        rig.rt.as_ref().unwrap().returning.is_some(),
        returning,
        "the Core is on the wrong ladder"
    );
}

/// The Core's runtime, built from what start-up returned.
fn seat(rig: &mut Rig, started: &Started) {
    real_disk(rig);
    rig.rt = Some(Runtime::from_started(
        started,
        &rig.registry,
        &rig.cfg,
        &rig.clock,
        Instant::now(),
    ));
}

/// The attempts the journal on disk holds for its ladder record.
fn recorded_attempts(rig: &Rig) -> Option<u32> {
    loaded(rig).ladder.map(|ladder| ladder.attempts)
}

/// Item 3 (a), return ladder: A fails twice (start-up's try and one of the
/// Core's), the supervisor restarts. Start-up's try is A's third, so the next
/// spawn is the next rung, P, and not a fourth try of A.
#[tokio::test]
async fn a_rung_that_failed_twice_has_one_try_left_after_a_restart_on_the_return_ladder() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, true);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, true);
    drop(first);
    {
        let mut core = core_of!(rig);
        ticks!(core, rig, 1);
    }
    assert_eq!(rig.driver.places(), ["1.0.0 kept", "1.0.0 kept"]);
    assert_eq!(recorded_attempts(&rig), Some(2));
    // The supervisor goes down and starts again.
    rig.rt = None;
    let second = boot(&mut rig).await;
    assert_eq!(rig.driver.places().len(), 3, "start-up's try is the third");
    assert!(second.run_failed);
    seat_expecting(&mut rig, &second, true);
    drop(second);
    {
        let mut core = core_of!(rig);
        ticks!(core, rig, 1);
    }
    let places = rig.driver.places();
    assert_eq!(
        places,
        ["1.0.0 kept", "1.0.0 kept", "1.0.0 kept", "0.9.0 kept"],
        "the next spawn is the next rung"
    );
    let _ = (f.p, f.i);
}

/// Item 3 (a), restart ladder: the same on a ladder with no switch.
#[tokio::test]
async fn a_rung_that_failed_twice_has_one_try_left_after_a_restart_on_the_restart_ladder() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    {
        let mut core = core_of!(rig);
        ticks!(core, rig, 1);
    }
    assert_eq!(rig.driver.places(), ["1.0.0 kept", "1.0.0 kept"]);
    assert_eq!(recorded_attempts(&rig), Some(2));
    rig.rt = None;
    let second = boot(&mut rig).await;
    assert_eq!(rig.driver.places().len(), 3, "start-up's try is the third");
    seat_expecting(&mut rig, &second, false);
    drop(second);
    {
        let mut core = core_of!(rig);
        ticks!(core, rig, 1);
    }
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "1.0.0 kept", "1.0.0 kept", "0.9.0 kept"]
    );
}

/// Item 3 (b): a worker that started and was running when the supervisor went
/// down never reported `loaded`: the try counts as failed. The third such
/// start is the last; the next start-up does not make a fourth.
#[tokio::test]
async fn a_try_in_flight_at_a_crash_counts_as_failed() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    assert!(first.worker.is_some());
    let held = loaded(&rig);
    assert_eq!(
        held.ladder.as_ref().map(|ladder| ladder.attempts),
        Some(1),
        "counted before the worker was started, and no outcome is held"
    );
    assert!(held.outcomes.is_empty());
    drop(first);
    let second = boot(&mut rig).await;
    assert!(second.worker.is_some());
    assert_eq!(recorded_attempts(&rig), Some(2));
    drop(second);
    let third = boot(&mut rig).await;
    assert!(third.worker.is_some());
    assert_eq!(recorded_attempts(&rig), Some(3));
    // The Core of the third: its worker is the one in flight, not counted twice.
    tokio::time::pause();
    seat_expecting(&mut rig, &third, false);
    let tracked_attempts = rig
        .rt
        .as_ref()
        .unwrap()
        .workers
        .values()
        .map(|tracked| tracked.attempts)
        .collect::<Vec<_>>();
    assert_eq!(tracked_attempts, [2]);
    drop(third);
    rig.rt = None;
    let places = rig.driver.places().len();
    let fourth = boot(&mut rig).await;
    // A is spent: start-up runs the next rung, P, and never a fourth try of A.
    assert_eq!(
        rig.driver.places()[places..],
        ["0.9.0 kept"],
        "A is not started again"
    );
    assert!(fourth.worker.is_some() && !fourth.run_failed);
    assert_eq!(recorded_attempts(&rig), Some(1), "P's first try");
}

/// Item 3 (c): `loaded` resets the count on the disk.
#[tokio::test]
async fn loaded_resets_the_count_on_the_disk() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    rig.driver.heal_spawn("1.0.0");
    let held = store(&rig);
    let attempts = || match held.load().unwrap() {
        JournalLoad::Loaded(journal) => journal.ladder.map(|ladder| ladder.attempts),
        other => panic!("no journal: {other:?}"),
    };
    let mut core = core_of!(rig);
    // A is healed: start-up's try was the first, this is the second and it
    // starts.
    ticks!(core, rig, 1);
    assert_eq!(attempts(), Some(2));
    let id = core.rt.active.expect("A runs");
    core.note_message(id, &crate::protocol::Message::Loaded { ms: 5 });
    assert_eq!(attempts(), Some(0), "loaded");
}

/// Item 3 (d): the third failure was journalled and the supervisor went down
/// before the Core retargeted: the next start does not try the spent rung.
#[tokio::test]
async fn a_crash_after_the_third_failure_does_not_try_the_spent_rung() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, true);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, true);
    drop(first);
    {
        let mut core = core_of!(rig);
        ticks!(core, rig, 2);
    }
    assert_eq!(rig.driver.places().len(), 3);
    let held = loaded(&rig);
    assert_eq!(held.ladder.as_ref().unwrap().attempts, 3);
    assert_eq!(
        held.switch
            .as_ref()
            .unwrap()
            .return_
            .as_ref()
            .unwrap()
            .target,
        f.a,
        "not retargeted yet"
    );
    rig.rt = None;
    let second = boot(&mut rig).await;
    // The spent rung is not tried: start-up runs the next one, P, which
    // journals P as the return's target in the same write that counts it.
    assert_eq!(
        rig.driver.places()[3..],
        ["0.9.0 kept"],
        "the spent rung is not tried"
    );
    assert!(second.worker.is_some() && !second.run_failed);
    let held = loaded(&rig);
    assert!(held.switch.is_none(), "the return ended on P");
    assert_eq!(held.confirmed, f.p, "J-R2 confirms what runs");
    assert_eq!(held.ladder.as_ref().unwrap().attempts, 1);
    assert_eq!(held.ladder.unwrap().current, Some(kept_rung(&f.p)));
}

/// Item 3 (e): a journal written before the field existed reads, and counts
/// from 0.
#[tokio::test]
async fn a_journal_without_the_ladder_field_reads_and_starts_at_zero() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, true);
    let text = std::fs::read_to_string(store(&rig).path()).unwrap();
    assert!(!text.contains("ladder"), "the fixture is the old format");
    assert_eq!(loaded(&rig).ladder, None);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    let ladder = loaded(&rig).ladder.expect("start-up counted its try");
    assert_eq!(ladder.attempts, 1);
    assert_eq!(ladder.switch.as_deref(), Some("sw-000000000000aaaa-1"));
}

/// A record of another ladder is never applied to this one's rungs.
#[tokio::test]
async fn a_record_of_another_ladder_is_ignored() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, true);
    // The restart ladder had A at two tries; the open ladder is a return's.
    let mut journal = loaded(&rig);
    journal.ladder = Some(LadderRecord {
        switch: None,
        current: Some(Rung {
            installed: false,
            artifact: f.a.clone(),
        }),
        attempts: 2,
        spent: Vec::new(),
    });
    store(&rig).write(&mut journal, "fixture").unwrap();
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert_eq!(recorded_attempts(&rig), Some(1), "not 3");
    tokio::time::pause();
    seat_expecting(&mut rig, &first, true);
    drop(first);
    {
        let mut core = core_of!(rig);
        ticks!(core, rig, 2);
    }
    assert_eq!(rig.driver.places(), ["1.0.0 kept"; 3]);
}

/// A record that names a rung the ladder doesn't have is ignored, with the
/// ladder starting from its first rung.
#[tokio::test]
async fn a_record_naming_a_rung_that_is_not_on_the_ladder_is_ignored() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, true);
    let mut journal = loaded(&rig);
    journal.ladder = Some(LadderRecord {
        switch: Some("sw-000000000000aaaa-1".to_owned()),
        current: Some(Rung {
            installed: false,
            artifact: f.a.clone(),
        }),
        attempts: 2,
        spent: vec![Rung {
            installed: false,
            artifact: Artifact {
                version: "7.7.7".to_owned(),
                sha256: "ghost".to_owned(),
                size: 1,
            },
        }],
    });
    store(&rig).write(&mut journal, "fixture").unwrap();
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, true);
    drop(first);
    {
        let mut core = core_of!(rig);
        ticks!(core, rig, 1);
    }
    // Trusted, A would be spent (three tries) and P next. Ignored, the ladder
    // starts again at A.
    assert_eq!(rig.driver.places(), ["1.0.0 kept", "1.0.0 kept"]);
}

/// Item 5 (c), carried from `recovery.rs`'s `a_resume_goes_on_down_the_ladder_
/// and_never_back_to_a_spent_rung` (which checks the plan only), and rule f
/// (round 5): the return is journalled at P, P's hash is no longer valid, A is
/// whole, and recovery runs the installed file. When that spawn fails, the
/// Core built from the start (on the return ladder) gives the installed rung
/// its other two tries. The ladder is then spent and degraded, nothing starts
/// on the next tick, and the new cycle after the backoff starts at the first
/// permitted rung, the kept A, which runs. P is on the return list while the
/// return is open: the journal recovery rebuilds around the installed file
/// keeps `previous` (round 7, item 2), so P is still held when the return ends
/// at A. `a_return_ladder_walks_on_to_p_and_a_garbage_kept_copy_is_three_failed_tries`
/// walks the ladder to P.
#[tokio::test]
async fn a_spent_return_ladder_starts_its_new_cycle_at_the_kept_a_and_p_stays_on_the_list() {
    let (mut rig, _dirs) = real_rig().await;
    // The installed file reports its own version: staging finds it free.
    rig.cfg.runner = std::sync::Arc::new(ScriptedRunner(Ok("open-ferry 0.5.0".to_owned())));
    let a = keep(&rig, "1.0.0", b"binary one");
    let p = keep(&rig, "0.9.0", b"binary nine");
    let b = keep(&rig, "2.0.0", b"binary two");
    std::fs::write(&rig.cfg.installed, b"binary installed").unwrap();
    let i = Artifact {
        version: "0.5.0".to_owned(),
        sha256: hash_file(&rig.cfg.installed).unwrap(),
        size: 16,
    };
    open_return(&rig, &a, &b, Some(&p), &p, false, &i);
    // P's kept copy no longer matches its hash.
    std::fs::write(rig.cfg.data.binary("0.9.0", NAME), b"garbage").unwrap();
    rig.driver.fail_spawn("0.5.0", SpawnFail::StartInstalled);
    let started = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert_journalled(&started);
    let run = started.run.as_ref().expect("the installed rung");
    assert_eq!(run.artifact.sha256, i.sha256);
    assert_eq!(run.binary, rig.cfg.installed);
    assert!(started.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &started, true);
    drop(started);
    let before = rig.driver.places().len();
    let mut core = core_of!(rig);
    let counts = ticks!(core, rig, 4);
    let places = rig.driver.places()[before..].to_vec();
    // Start-up made the installed rung's first try; two more, then it is spent
    // and a tick starts nothing; the tick after starts the new cycle at A.
    assert_eq!(counts, [1, 1, 0, 1], "{counts:?} {places:?}");
    assert_eq!(
        places,
        ["0.5.0 installed", "0.5.0 installed", "1.0.0 kept"],
        "{places:?}"
    );
    assert_eq!(
        core.rt.journal.as_ref().unwrap().previous.as_ref(),
        Some(&p),
        "P is held: it was on the return list while the return was open"
    );
    assert!(core.rt.returning.is_none(), "A runs: the return is done");
}

/// Round 7, item 2 and the coordinator's note: with `previous` kept by the
/// rebuild, P is on the return list between A and the installed file. In the
/// new cycle A fails its three tries, and P's garbage kept copy is reached.
/// The real driver's hash check refuses it (`SpawnError::HashMismatch`,
/// modelled by `SpawnFail::VerifyKept`); the Core takes that as a failed try,
/// not a refusal: `spawn_run`'s catch-all arm returns `RunError::Failed`, so
/// P is spawned three times (charged three times) and then the ladder moves on.
#[tokio::test]
async fn a_return_ladder_walks_on_to_p_and_a_garbage_kept_copy_is_three_failed_tries() {
    let (mut rig, _dirs) = real_rig().await;
    rig.cfg.runner = std::sync::Arc::new(ScriptedRunner(Ok("open-ferry 0.5.0".to_owned())));
    let a = keep(&rig, "1.0.0", b"binary one");
    let p = keep(&rig, "0.9.0", b"binary nine");
    let b = keep(&rig, "2.0.0", b"binary two");
    std::fs::write(&rig.cfg.installed, b"binary installed").unwrap();
    let i = Artifact {
        version: "0.5.0".to_owned(),
        sha256: hash_file(&rig.cfg.installed).unwrap(),
        size: 16,
    };
    open_return(&rig, &a, &b, Some(&p), &p, false, &i);
    std::fs::write(rig.cfg.data.binary("0.9.0", NAME), b"garbage").unwrap();
    rig.driver.fail_spawn("0.5.0", SpawnFail::StartInstalled);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    rig.driver.fail_spawn("0.9.0", SpawnFail::VerifyKept);
    let started = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert!(started.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &started, true);
    drop(started);
    let before = rig.driver.places().len();
    let mut core = core_of!(rig);
    let counts = ticks!(core, rig, 24);
    let places = rig.driver.places()[before..].to_vec();
    assert!(places.len() >= 11, "{counts:?} {places:?}");
    assert_eq!(
        places[..11],
        [
            // The installed rung: start-up's try was the first.
            "0.5.0 installed",
            "0.5.0 installed",
            // The new cycle: A's three tries, then P's three, each charged as
            // a failed try (a refusal would have passed P with none).
            "1.0.0 kept",
            "1.0.0 kept",
            "1.0.0 kept",
            "0.9.0 kept",
            "0.9.0 kept",
            "0.9.0 kept",
            // Then the installed file.
            "0.5.0 installed",
            "0.5.0 installed",
            "0.5.0 installed",
        ],
        "{counts:?} {places:?}"
    );
}

// ---- round 7 ----------------------------------------------------------------

/// The installed rung of a fixture, as a ladder record names it.
fn installed_rung(artifact: &Artifact) -> Rung {
    Rung {
        installed: true,
        artifact: artifact.clone(),
    }
}

fn kept_rung(artifact: &Artifact) -> Rung {
    Rung {
        installed: false,
        artifact: artifact.clone(),
    }
}

/// The journal with its ladder record replaced.
fn write_record(rig: &Rig, record: LadderRecord) {
    let mut journal = loaded(rig);
    journal.ladder = Some(record);
    store(rig).write(&mut journal, "fixture").unwrap();
}

/// Item 4: one record check for start-up and the Core. A record that names a
/// rung the restart ladder doesn't have is ignored by start-up (one fresh
/// record, one try counted), so a worker that exits before `loaded` leaves
/// the Core two more tries of A, not none.
#[tokio::test]
async fn a_stale_record_is_ignored_by_start_up_so_a_worker_that_exits_before_loaded_still_has_two_tries()
 {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    write_record(
        &rig,
        LadderRecord {
            switch: None,
            current: Some(kept_rung(&f.a)),
            attempts: 2,
            spent: vec![kept_rung(&Artifact {
                version: "7.7.7".to_owned(),
                sha256: "ghost".to_owned(),
                size: 1,
            })],
        },
    );
    let started = boot(&mut rig).await;
    assert!(started.worker.is_some(), "A started");
    assert_eq!(recorded_attempts(&rig), Some(1), "a fresh record, not 3");
    tokio::time::pause();
    seat_expecting(&mut rig, &started, false);
    let id = started.worker.unwrap();
    drop(started);
    {
        let mut core = core_of!(rig);
        let mut gone = id;
        for _ in 0..3 {
            core.worker_exited(gone, "gone before loaded", false).await;
            ticks!(core, rig, 1);
            gone = core.rt.active.expect("the next try runs");
        }
    }
    let places = rig.driver.places();
    // The start-up try, then two more: A is not spent after one exit.
    assert_eq!(
        places,
        ["1.0.0 kept", "1.0.0 kept", "1.0.0 kept", "0.9.0 kept"],
        "{places:?}"
    );
    let _ = (f.p, f.i);
}

/// The scenario of item 1: A's bytes are also the installed file's. A kept
/// copy that fails three tries is spent, the installed file (the same bytes)
/// runs. J-R2 turns the return's record into the restart ladder's with the held
/// tries; it does not clear it.
fn same_bytes_fixture(rig: &Rig) -> Artifact {
    let a = keep(rig, "1.0.0", b"binary one");
    let b = keep(rig, "2.0.0", b"binary two");
    std::fs::write(&rig.cfg.installed, b"binary one").unwrap();
    open_return(rig, &a, &b, None, &a, false, &a);
    a
}

/// Item 1, with a crash: J-R2's record is the restart ladder's, so a restart
/// before `loaded` does not start the spent kept A again and gives the
/// installed A its next try (the second), not a fresh three.
#[tokio::test]
async fn j_r2_keeps_the_held_tries_on_the_restart_ladder_across_a_crash_before_loaded() {
    let (mut rig, _dirs) = real_rig().await;
    let a = same_bytes_fixture(&rig);
    rig.driver.fail_spawn(
        "1.0.0",
        SpawnFail::Script(vec![SpawnFail::Start, SpawnFail::Start, SpawnFail::Start]),
    );
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, true);
    drop(first);
    {
        let mut core = core_of!(rig);
        let counts = ticks!(core, rig, 3);
        assert!(
            core.rt.returning.is_none(),
            "the installed A ran: J-R2 {counts:?} {:?}",
            rig.driver.places()
        );
    }
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "1.0.0 kept", "1.0.0 kept", "1.0.0 installed"]
    );
    let record = loaded(&rig).ladder.expect("the restart ladder's record");
    assert_eq!(record.switch, None, "the restart ladder's key");
    assert_eq!(record.current, Some(installed_rung(&a)));
    assert_eq!(record.attempts, 1, "the try in flight is counted");
    assert_eq!(record.spent, vec![kept_rung(&a)]);
    // The supervisor goes down before `loaded` and starts again.
    rig.rt = None;
    let places = rig.driver.places().len();
    let second = boot(&mut rig).await;
    let after = rig.driver.places()[places..].to_vec();
    assert!(
        !after.contains(&"1.0.0 kept".to_owned()),
        "the spent kept A is not tried again: {after:?}"
    );
    // Start-up goes on from the record: the installed A, on its second try.
    assert_eq!(after, ["1.0.0 installed"]);
    assert_eq!(recorded_attempts(&rig), Some(2), "its second try");
    seat_expecting(&mut rig, &second, false);
}

/// Item 1, no crash: the worker J-R2 started exits before `loaded`. The
/// installed A has the tries the record held, not a fresh three: it is
/// started twice more and then the ladder is spent.
#[tokio::test]
async fn a_worker_started_by_j_r2_that_exits_before_loaded_has_the_remaining_tries() {
    let (mut rig, _dirs) = real_rig().await;
    let _a = same_bytes_fixture(&rig);
    rig.driver.fail_spawn(
        "1.0.0",
        SpawnFail::Script(vec![SpawnFail::Start, SpawnFail::Start, SpawnFail::Start]),
    );
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, true);
    drop(first);
    let mut core = core_of!(rig);
    ticks!(core, rig, 3);
    let before = rig.driver.places().len();
    for _ in 0..3 {
        let id = core.rt.active.expect("the installed A runs");
        core.worker_exited(id, "gone before loaded", false).await;
        ticks!(core, rig, 1);
    }
    let places = rig.driver.places()[before..].to_vec();
    assert_eq!(
        places,
        ["1.0.0 installed", "1.0.0 installed"],
        "two more tries, then the ladder is spent: {places:?}"
    );
}

/// Item 2, through real recovery: a return open at the installed rung with two
/// tries held. Recovery rebuilds the journal around the installed file and
/// keeps the ladder record: start-up's try is the third, the installed rung is
/// then spent, and the new cycle after the backoff starts at the first
/// permitted rung, A. P (`previous`) is still held.
#[tokio::test]
async fn a_return_open_at_the_installed_rung_keeps_its_record_and_previous_through_recovery() {
    let (mut rig, _dirs) = real_rig().await;
    rig.cfg.runner = std::sync::Arc::new(ScriptedRunner(Ok("open-ferry 0.5.0".to_owned())));
    let a = keep(&rig, "1.0.0", b"binary one");
    let p = keep(&rig, "0.9.0", b"binary nine");
    let b = keep(&rig, "2.0.0", b"binary two");
    std::fs::write(&rig.cfg.installed, b"binary installed").unwrap();
    let i = Artifact {
        version: "0.5.0".to_owned(),
        sha256: hash_file(&rig.cfg.installed).unwrap(),
        size: 16,
    };
    open_return(&rig, &a, &b, Some(&p), &p, false, &i);
    write_record(
        &rig,
        LadderRecord {
            switch: Some("sw-000000000000aaaa-1".to_owned()),
            current: Some(installed_rung(&i)),
            attempts: 2,
            spent: Vec::new(),
        },
    );
    std::fs::write(rig.cfg.data.binary("0.9.0", NAME), b"garbage").unwrap();
    rig.driver.fail_spawn("0.5.0", SpawnFail::StartInstalled);
    let started = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert_journalled(&started);
    assert!(started.run_failed);
    assert_eq!(
        recorded_attempts(&rig),
        Some(3),
        "start-up's try is the third"
    );
    tokio::time::pause();
    seat_expecting(&mut rig, &started, true);
    drop(started);
    let before = rig.driver.places().len();
    let mut core = core_of!(rig);
    let counts = ticks!(core, rig, 3);
    let places = rig.driver.places()[before..].to_vec();
    assert_eq!(counts, [0, 1, 0], "{counts:?} {places:?}");
    assert_eq!(places, ["1.0.0 kept"], "{places:?}");
    assert_eq!(
        core.rt.journal.as_ref().unwrap().previous.as_ref(),
        Some(&p),
        "P is held"
    );
}

/// Item 2, idle: the rebuild around the installed file keeps the restart
/// ladder's record.
#[tokio::test]
async fn an_idle_rebuild_around_the_installed_file_keeps_the_restart_ladders_record() {
    let (mut rig, _dirs) = real_rig().await;
    rig.cfg.runner = std::sync::Arc::new(ScriptedRunner(Ok("open-ferry 0.5.0".to_owned())));
    let f = ladder_fixture(&rig, false);
    write_record(
        &rig,
        LadderRecord {
            switch: None,
            current: Some(installed_rung(&f.i)),
            attempts: 2,
            spent: Vec::new(),
        },
    );
    std::fs::write(rig.cfg.data.binary("1.0.0", NAME), b"garbage").unwrap();
    std::fs::write(rig.cfg.data.binary("0.9.0", NAME), b"garbage").unwrap();
    rig.driver.fail_spawn("0.5.0", SpawnFail::StartInstalled);
    let started = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert_journalled(&started);
    assert_eq!(rig.driver.places(), ["0.5.0 installed"]);
    assert_eq!(recorded_attempts(&rig), Some(3), "the record survived");
}

/// Item 3 at start-up, and item 9 (a): the count cannot be written (the journal
/// file is unreadable, so the store fails closed). Switching is off, the
/// ladder's rung is not spawned, and the installed file runs as the one worker.
#[tokio::test]
async fn a_start_up_count_that_cannot_be_written_turns_switching_off_and_the_installed_file_runs() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    let mut started = boot(&mut rig).await;
    assert_eq!(rig.driver.places(), ["1.0.0 kept"]);
    std::fs::write(store(&rig).path(), b"not a journal").unwrap();
    crate::startup::launch(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &mut started,
    )
    .await;
    assert!(started.switching_disabled.is_some(), "switching is off");
    assert!(started.journal.is_none());
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "1.0.0 installed"],
        "the ladder's rung is not spawned again; the installed file runs"
    );
    assert!(started.worker.is_some());
    let _ = f.p;
}

/// Item 9 (a): with a start-up that really runs with switching off, a failed
/// spawn is one try on its rung, the installed file's: `Started::run_failed`
/// is how the Core learns of it, since nothing is journalled. One start-up: the
/// journal is of a newer format than this supervisor reads, so recovery's own
/// execution returns the installed file as the run with switching off.
#[tokio::test]
async fn a_failed_startup_spawn_with_switching_really_off_is_one_try_on_the_installed_file() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    std::fs::write(store(&rig).path(), br#"{"format":99}"#).unwrap();
    rig.driver.fail_spawn("1.0.0", SpawnFail::StartInstalled);
    let started = start(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
    )
    .await
    .unwrap();
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 installed"],
        "the one spawn is the installed file's"
    );
    assert!(started.journal.is_none());
    assert!(started.switching_disabled.is_some());
    assert!(started.run_failed, "the real launch saw the spawn fail");
    tokio::time::pause();
    seat(&mut rig, &started);
    drop(started);
    let before = rig.driver.places().len();
    let mut core = core_of!(rig);
    let tries = ticks!(core, rig, 4);
    let places = rig.driver.places()[before..].to_vec();
    // Start-up's was the first try: two more, the rung is spent, a tick passes
    // and the new cycle starts it again.
    assert_eq!(tries, [1, 1, 0, 1], "{places:?}");
    assert_eq!(places, ["1.0.0 installed"; 3]);
}

/// Item 3 in the Core: the +1 for a restart try cannot be written. Switching
/// turns off; the ladder's rung is not spawned; the installed file runs.
#[tokio::test]
async fn a_failed_count_in_the_core_turns_switching_off_and_the_installed_file_runs() {
    let (mut rig, _dirs) = real_rig().await;
    rig.cfg.runner = std::sync::Arc::new(ScriptedRunner(Ok("open-ferry 0.5.0".to_owned())));
    let f = ladder_fixture(&rig, false);
    // The run that switching off chooses is the installed file when it is this
    // binary's own bytes, as start-up's `switch_off` chooses it.
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    rig.disk.fail_journal("J-L", 1);
    let mut core = core_of!(rig);
    ticks!(core, rig, 1);
    assert!(core.rt.disabled.is_some(), "switching is off");
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "1.0.0 installed"],
        "A is not spawned from the ladder; the installed file runs"
    );
}

/// Item 3 in the Core: the reset on `loaded` cannot be written.
#[tokio::test]
async fn a_failed_reset_on_loaded_turns_switching_off() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let started = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &started, false);
    drop(started);
    rig.disk.fail_journal("J-L", 1);
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("A runs");
    core.note_message(id, &crate::protocol::Message::Loaded { ms: 5 });
    assert!(core.rt.disabled.is_some(), "switching is off");
}

/// Item 5: A has two failures, is refused as untrusted (not a try), trust is
/// repaired and the supervisor restarts: A still has its third try.
#[tokio::test]
async fn a_refusal_as_untrusted_gives_the_try_back_before_switching_goes_off() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    {
        let mut core = core_of!(rig);
        ticks!(core, rig, 1);
        assert_eq!(rig.driver.places().len(), 2, "A has two failures");
        rig.driver.fail_spawn("1.0.0", SpawnFail::Untrusted);
        ticks!(core, rig, 1);
        assert!(core.rt.disabled.is_some(), "refused as untrusted");
    }
    assert_eq!(recorded_attempts(&rig), Some(2), "the refusal was no try");
    // Trust is repaired and the supervisor restarts.
    rig.driver.heal_spawn("1.0.0");
    rig.rt = None;
    let second = boot(&mut rig).await;
    assert!(second.worker.is_some(), "A's third try runs");
    assert_eq!(recorded_attempts(&rig), Some(3));
}

/// Item 9 (b): a real replay. The journal holds the forward switch's outcome,
/// recovery really replays it into the state file (once, by id) and clears it
/// from the journal. The Core built from that start then sees the record in
/// the state file, so when the return's spawn fails and nothing can run it
/// writes no second record and no J7, even with S3 and J7 failing.
#[tokio::test]
async fn a_record_really_replayed_by_start_up_is_not_written_a_second_time_by_the_core() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, true);
    let mut journal = loaded(&rig);
    let id = journal.switch.as_ref().unwrap().id.clone();
    journal.outcomes.push(open_ferry_update::journal::Outcome {
        record: open_ferry_update::state::SwitchRecord {
            id: id.clone(),
            result: Some("failed-to-load".to_owned()),
            ..Default::default()
        },
        mark_failed: None,
    });
    store(&rig).write(&mut journal, "fixture").unwrap();
    let records = |rig: &Rig| {
        let state = open_ferry_update::state::State::load(&rig.cfg.data.state_file());
        state
            .history
            .iter()
            .filter(|record| record.id == id)
            .count()
    };
    assert_eq!(records(&rig), 0, "nothing is in the state file yet");
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    rig.driver.fail_spawn("0.9.0", SpawnFail::Start);
    rig.driver.fail_spawn("0.5.0", SpawnFail::StartInstalled);
    let started = boot(&mut rig).await;
    assert!(
        loaded(&rig).outcomes.is_empty(),
        "the replay cleared the outcome"
    );
    let state = open_ferry_update::state::State::load(&rig.cfg.data.state_file());
    let persisted: Vec<_> = state
        .history
        .iter()
        .filter(|record| record.id == id)
        .collect();
    assert_eq!(persisted.len(), 1, "exactly one persisted record");
    tokio::time::pause();
    seat_expecting(&mut rig, &started, true);
    drop(started);
    assert!(
        rig.rt
            .as_ref()
            .unwrap()
            .returning
            .as_ref()
            .unwrap()
            .unstarted_recorded,
        "the Core reads the replayed record from the state file"
    );
    rig.disk.fail_state("S3", 9);
    rig.disk.fail_journal("J7", 9);
    let before = rig.disk.order().len();
    let writes = rig.disk.state.lock().unwrap().state_writes.len();
    {
        let mut core = core_of!(rig);
        let _ = ticks!(core, rig, 12);
        assert!(core.rt.degraded.is_some(), "nothing could start");
    }
    let order = rig.disk.order()[before..].to_vec();
    assert!(
        order.iter().all(|name| name != "J7" && name != "S3"),
        "no second write was attempted: {order:?}"
    );
    assert_eq!(rig.disk.state.lock().unwrap().state_writes.len(), writes);
    assert_eq!(records(&rig), 1, "still exactly one record");
    let _ = (f.p, f.i);
}

/// Item 1, start-up's own J-R2 (`finish_recovered_return`): the return's
/// target starts at start-up. The record written with J-R2 is the restart
/// ladder's: no switch id, the rung that ran, its in-flight try counted, and
/// the tries the return held (two before this one).
#[tokio::test]
async fn start_ups_j_r2_turns_the_returns_record_into_the_restart_ladders() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, true);
    write_record(
        &rig,
        LadderRecord {
            switch: Some("sw-000000000000aaaa-1".to_owned()),
            current: Some(kept_rung(&f.a)),
            attempts: 1,
            spent: Vec::new(),
        },
    );
    let started = boot(&mut rig).await;
    assert!(started.worker.is_some(), "A started");
    let held = loaded(&rig);
    assert!(held.switch.is_none(), "J-R2 cleared the switch");
    let record = held.ladder.expect("a restart ladder record");
    assert_eq!(record.switch, None, "the restart ladder's key");
    assert_eq!(record.current, Some(kept_rung(&f.a)));
    assert_eq!(record.attempts, 2, "the held try and this one");
    assert!(record.spent.is_empty());
    let _ = (f.p, f.i);
}

// ---- round 8 ----------------------------------------------------------------

/// Item 2, through real recovery: J-R2 recorded the installed A (the same bytes
/// as the kept A) with one try, and the supervisor went down before `loaded`.
/// Recovery proposes the kept A, which verifies. Start-up runs the rung the
/// record is on, the installed A at its second try; the kept A is not spawned.
#[tokio::test]
async fn start_up_goes_on_from_the_record_and_not_from_recoverys_run_on_the_restart_ladder() {
    let (mut rig, _dirs) = real_rig().await;
    let a = keep(&rig, "1.0.0", b"binary one");
    std::fs::write(&rig.cfg.installed, b"binary one").unwrap();
    let mut journal = Journal::first_start(a.clone(), Some(Installed::from(&a)));
    store(&rig).write(&mut journal, "fixture").unwrap();
    write_record(
        &rig,
        LadderRecord {
            switch: None,
            current: Some(installed_rung(&a)),
            attempts: 1,
            spent: Vec::new(),
        },
    );
    let started = boot(&mut rig).await;
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 installed"],
        "the installed A runs and the kept A is not spawned"
    );
    let record = loaded(&rig).ladder.expect("a record");
    assert_eq!(record.current, Some(installed_rung(&a)));
    assert_eq!(record.attempts, 2, "its second try");
    assert_eq!(started.run.as_ref().unwrap().binary, rig.cfg.installed);
}

/// Item 2 on the return ladder, with a record from before J-R2: the return's
/// record is on the installed A at one try, recovery's return target is the
/// kept A. Start-up runs the installed A at its second try, journals it as the
/// return's target in the same write, and J-R2 carries the count on.
#[tokio::test]
async fn start_up_goes_on_from_the_record_and_not_from_recoverys_run_on_the_return_ladder() {
    let (mut rig, _dirs) = real_rig().await;
    let a = same_bytes_fixture(&rig);
    write_record(
        &rig,
        LadderRecord {
            switch: Some("sw-000000000000aaaa-1".to_owned()),
            current: Some(installed_rung(&a)),
            attempts: 1,
            spent: Vec::new(),
        },
    );
    let started = boot(&mut rig).await;
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 installed"],
        "the installed A runs and the kept A is not spawned"
    );
    assert_eq!(started.run.as_ref().unwrap().binary, rig.cfg.installed);
    let held = loaded(&rig);
    assert!(held.switch.is_none(), "J-R2 ended the return");
    let record = held.ladder.expect("the restart ladder's record");
    assert_eq!(record.switch, None);
    assert_eq!(record.current, Some(installed_rung(&a)));
    assert_eq!(record.attempts, 2, "its second try");
}

/// Item 5, the oracle's scenario: a spent rung at a kept location that is not
/// on the ladder, with A's hash, is not a way onto the list. The record is
/// ignored whole, and A starts at its first try.
#[tokio::test]
async fn a_record_naming_a_kept_location_that_is_not_on_the_ladder_is_ignored_even_with_a_known_hash()
 {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    // Version 7.7.7 holds A's bytes, and nothing in the journal names it. The
    // installed file holds A's bytes too, so the hash the record names is the
    // installed file's as well: only the location keeps the rung off the list.
    let g = keep(&rig, "7.7.7", b"binary one");
    assert_eq!(g.sha256, f.a.sha256);
    std::fs::write(&rig.cfg.installed, b"binary one").unwrap();
    assert_eq!(hash_file(&rig.cfg.installed).unwrap(), f.a.sha256);
    write_record(
        &rig,
        LadderRecord {
            switch: None,
            current: Some(kept_rung(&f.a)),
            attempts: 2,
            spent: vec![kept_rung(&g)],
        },
    );
    let started = boot(&mut rig).await;
    assert!(started.worker.is_some(), "A started");
    assert_eq!(recorded_attempts(&rig), Some(1), "a fresh record, not 3");
    assert_eq!(
        loaded(&rig).ladder.unwrap().spent,
        Vec::<Rung>::new(),
        "the stale spent rung is gone"
    );
}

/// Item 5: the one location a record may add is the installed path, only when
/// the installed file's bytes measured now are that rung's artifact.
#[tokio::test]
async fn a_record_adds_the_installed_location_only_by_the_bytes_measured_now() {
    let (rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    let journal = loaded(&rig);
    let named = crate::ladder::Progress {
        current: Some((crate::ladder::run_of(&rig.cfg, &installed_rung(&f.a)), 1)),
        spent: Vec::new(),
    };
    let record = LadderRecord {
        switch: None,
        current: Some(installed_rung(&f.a)),
        attempts: 1,
        spent: Vec::new(),
    };
    let mut with = journal.clone();
    with.ladder = Some(record);
    let at_installed = |full: &[crate::startup::Run]| {
        full.iter()
            .filter(|run| run.binary == rig.cfg.installed)
            .count()
    };
    // The installed file holds A's bytes now: the rung is on the list, by path,
    // and the record is usable.
    let list = crate::ladder::restart_list(&rig.cfg, &with, None, Some(&f.a.sha256), &named);
    assert_eq!(at_installed(&list), 1);
    assert!(crate::ladder::usable_record(&rig.cfg, &with, &list).is_some());
    // The file has changed since (other bytes, or nothing readable): the rung
    // is not on the list, and the record is not usable.
    for measured in [Some("other bytes"), None] {
        let list = crate::ladder::restart_list(&rig.cfg, &with, None, measured, &named);
        assert_eq!(at_installed(&list), 0, "{measured:?}");
        assert!(
            crate::ladder::usable_record(&rig.cfg, &with, &list).is_none(),
            "{measured:?}"
        );
    }
}

/// Item 5 at start-up: the installed file has changed since the record named
/// its rung as A. The record is ignored and the ladder starts again at A.
#[tokio::test]
async fn a_record_of_an_installed_rung_whose_file_has_changed_is_ignored() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    write_record(
        &rig,
        LadderRecord {
            switch: None,
            current: Some(installed_rung(&f.a)),
            attempts: 2,
            spent: Vec::new(),
        },
    );
    // The installed file holds I's bytes, not A's.
    let started = boot(&mut rig).await;
    assert_eq!(rig.driver.places(), ["1.0.0 kept"], "recovery's run");
    let record = loaded(&rig).ladder.expect("a fresh record");
    assert_eq!(record.current, Some(kept_rung(&f.a)));
    assert_eq!(record.attempts, 1);
    assert!(started.worker.is_some());
}

/// Item 6: a failed start-up count while another holder has the update lock.
/// The fallback waits for the lock, nothing is spawned until the other holder
/// lets go, and then the installed file runs.
#[tokio::test]
async fn the_fallback_after_a_failed_count_waits_for_the_update_lock() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let mut started = boot(&mut rig).await;
    assert!(started.run_failed);
    rig.driver.heal_spawn("1.0.0");
    let places = rig.driver.places().len();
    std::fs::write(store(&rig).path(), b"not a journal").unwrap();
    let hold = rig
        .cfg
        .data
        .try_lock()
        .unwrap()
        .expect("the update lock is free");
    {
        let launch = crate::startup::launch(
            &rig.cfg,
            &rig.host,
            &rig.driver,
            &mut rig.registry,
            &mut started,
        );
        tokio::pin!(launch);
        let waited = tokio::time::timeout(Duration::from_millis(300), &mut launch).await;
        assert!(
            waited.is_err(),
            "the fallback is still waiting for the lock"
        );
        assert_eq!(
            rig.driver.places().len(),
            places,
            "nothing is spawned while another holder has the lock"
        );
        drop(hold);
        launch.await;
    }
    assert_eq!(rig.driver.places()[places..], ["1.0.0 installed"]);
    assert!(started.switching_disabled.is_some());
    assert!(started.worker.is_some());
    assert!(
        rig.cfg.data.try_lock().unwrap().is_some(),
        "the lock is released after the spawn"
    );
}

// ---- round 8 item 1 and round 8b: a failed ladder write turns switching off ---------

/// A Core on the restart ladder whose A (the kept copy) failed its start-up
/// try, with the installed file I being this binary's own bytes when `own`,
/// as start-up's `switch_off` needs it to choose I.
async fn restart_ladder_after_a_failed_start_up_try(own: bool) -> (Rig, Dirs, Fallback) {
    let (mut rig, dirs) = real_rig().await;
    rig.cfg.runner = std::sync::Arc::new(ScriptedRunner(Ok("open-ferry 0.5.0".to_owned())));
    let f = ladder_fixture(&rig, false);
    if own {
        *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    }
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    // Only start-up's own try fails: what the Core spawns afterwards starts.
    rig.driver.heal_spawn("1.0.0");
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    (rig, dirs, f)
}

fn journal_writes(disk: &crate::testkit::FakeDisk) -> usize {
    disk.state.lock().unwrap().journal_writes.len()
}

/// The journal the real store holds, read by path so a Core may be borrowing
/// the rig.
fn loaded_at(root: &std::path::Path) -> Journal {
    match JournalStore::new(root).load().unwrap() {
        JournalLoad::Loaded(journal) => journal,
        other => panic!("no journal: {other:?}"),
    }
}

/// Item 1: a failed +1. The installed run spawns; the installed file's own
/// failures are retried, and no retry tick writes anything to the disk.
#[tokio::test]
async fn after_a_failed_count_the_installed_run_spawns_and_later_retry_ticks_write_nothing() {
    let (mut rig, _dirs, _f) = restart_ladder_after_a_failed_start_up_try(true).await;
    rig.disk.fail_journal("J-L", 1);
    rig.driver.fail_spawn("1.0.0", SpawnFail::StartInstalled);
    let mut core = core_of!(rig);
    ticks!(core, rig, 1);
    assert!(core.rt.disabled.is_some(), "switching is off");
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "1.0.0 installed"],
        "the installed run is spawned on the strength of the failed write"
    );
    let writes = journal_writes(&rig.disk);
    let order = rig.disk.order();
    let tries = ticks!(core, rig, 6);
    assert!(
        tries.iter().any(|started| *started > 0),
        "the installed run is being retried: {tries:?}"
    );
    assert_eq!(
        journal_writes(&rig.disk),
        writes,
        "a retry tick wrote: {order:?}"
    );
    assert_eq!(rig.disk.order(), order);
}

/// Item 1: after a failed +1 the installed run fails three times and a new
/// cycle comes: every spawn is the installed run and no kept location is
/// ever spawned.
#[tokio::test]
async fn after_a_failed_count_the_installed_run_fails_three_times_and_no_kept_location_is_spawned()
{
    let (mut rig, _dirs, _f) = restart_ladder_after_a_failed_start_up_try(true).await;
    rig.disk.fail_journal("J-L", 1);
    rig.driver.fail_spawn("1.0.0", SpawnFail::StartInstalled);
    let mut core = core_of!(rig);
    ticks!(core, rig, 12);
    let places = rig.driver.places();
    assert!(
        places.len() >= 8,
        "three failures and a new cycle at least: {places:?}"
    );
    assert_eq!(places[0], "1.0.0 kept", "start-up's try");
    assert!(
        places[1..].iter().all(|place| place == "1.0.0 installed"),
        "{places:?}"
    );
}

/// Item 1: the reset on `loaded` fails while a kept A runs. A keeps running;
/// when it exits the restart runs the installed run, with no ladder.
#[tokio::test]
async fn a_failed_reset_keeps_a_running_and_its_exit_restarts_on_the_installed_run() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    let started = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &started, false);
    drop(started);
    rig.disk.fail_journal("J-L", 1);
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("A runs");
    core.note_message(id, &crate::protocol::Message::Loaded { ms: 5 });
    assert!(core.rt.disabled.is_some(), "switching is off");
    assert!(core.rt.journal.is_none());
    assert_eq!(core.rt.active, Some(id), "A keeps running");
    assert_eq!(rig.driver.places(), ["1.0.0 kept"]);
    let writes = journal_writes(&rig.disk);
    core.worker_exited(id, "crashed", false).await;
    ticks!(core, rig, 2);
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "1.0.0 installed"],
        "the restart after A's exit runs the installed run"
    );
    assert_eq!(journal_writes(&rig.disk), writes, "nothing was written");
}

/// Item 1: a failed +1 with no installed file that qualifies (it is not this
/// binary's own bytes): degraded, nothing is spawned, and no later rung is.
#[tokio::test]
async fn a_failed_count_with_no_qualifying_installed_file_is_degraded_and_spawns_nothing() {
    let (mut rig, _dirs, _f) = restart_ladder_after_a_failed_start_up_try(false).await;
    rig.disk.fail_journal("J-L", 1);
    let mut core = core_of!(rig);
    ticks!(core, rig, 8);
    assert!(core.rt.disabled.is_some(), "switching is off");
    assert!(core.rt.degraded.is_some(), "degraded");
    assert!(core.rt.run.is_none());
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept"],
        "nothing is spawned and no later rung is tried"
    );
}

/// Item 1, the return ladder: a failed +1 writes no J-R2 and no outcome, and a
/// restart of the supervisor on the same disk recovers the open return.
#[tokio::test]
async fn a_failed_count_on_the_return_ladder_writes_no_j_r2_and_a_restart_recovers_the_return() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, true);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, true);
    drop(first);
    rig.disk.fail_journal("J-L", 1);
    {
        let mut core = core_of!(rig);
        ticks!(core, rig, 3);
        assert!(core.rt.disabled.is_some(), "switching is off");
        assert!(core.rt.returning.is_none(), "the return is abandoned");
    }
    let order = rig.disk.order();
    assert!(order.iter().all(|name| name != "J-R2"), "{order:?}");
    assert!(
        rig.disk.state.lock().unwrap().state_writes.is_empty(),
        "no outcome was written"
    );
    let held = loaded(&rig);
    assert!(
        held.switch.as_ref().is_some_and(|s| s.return_.is_some()),
        "the open return is on the disk"
    );
    assert!(held.outcomes.is_empty());
    rig.rt = None;
    let second = boot(&mut rig).await;
    seat_expecting(&mut rig, &second, true);
    assert_eq!(
        rig.driver.places().last().map(String::as_str),
        Some("1.0.0 kept"),
        "recovery goes on with the open return"
    );
}

/// What a Core's state is once switching is off, for comparing two Cores.
macro_rules! switch_off_state {
    ($core:ident, $rig:ident) => {{
        $core.publish();
        let snapshot = $rig.shared.snapshot();
        [
            format!(
                "disabled {} no-journal {} stale {} no-retry {} no-pending {} keep-all {}",
                $core.rt.disabled.is_some(),
                $core.rt.journal.is_none(),
                $core.rt.journal_stale,
                $core.rt.journal_retry.is_none(),
                $core.rt.pending.is_empty(),
                $core.rt.keep_all,
            ),
            format!(
                "run {:?} no-degraded {} no-return {} no-ladder-off {} active {} no-restart {}",
                $core.rt.run.as_ref().map(|run| (
                    run.binary == $rig.cfg.installed,
                    run.artifact.version.clone(),
                    run.artifact.sha256.clone(),
                )),
                $core.rt.degraded.is_none(),
                $core.rt.returning.is_none(),
                $core.rt.ladder_off.is_none(),
                $core.rt.active.is_some(),
                $core.rt.restart.is_none(),
            ),
            format!(
                "status journal {} stale {} state-write {} pending {} disabled {} degraded {} active {:?}",
                snapshot.journal,
                snapshot.journal_stale,
                snapshot.state_write,
                snapshot.state_write_pending,
                snapshot.switching_disabled.is_some(),
                snapshot.degraded.is_some(),
                snapshot.active_version,
            ),
        ]
    }};
}

/// Item 1: the state after the Core's transition is the state of a Core built
/// from start-up's `switch_off`: the disabled reason, no journal, no stale
/// mark, no retry, nothing pending, every version kept, the same run, and the
/// same status fields.
#[tokio::test]
async fn the_cores_switch_off_leaves_the_state_start_ups_switch_off_leaves() {
    // The Core's: a failed +1 on the restart ladder, then its restart tick.
    let (mut rig, _dirs, _f) = restart_ladder_after_a_failed_start_up_try(true).await;
    rig.disk.fail_journal("J-L", 1);
    let mut core = core_of!(rig);
    ticks!(core, rig, 1);
    let ours = switch_off_state!(core, rig);
    // Start-up's: a journal of a newer format, so recovery switches off.
    let (mut other, _other_dirs) = real_rig().await;
    other.cfg.runner = std::sync::Arc::new(ScriptedRunner(Ok("open-ferry 0.5.0".to_owned())));
    let f = ladder_fixture(&other, false);
    *other.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    std::fs::write(store(&other).path(), br#"{"format":99}"#).unwrap();
    let started = start(
        &other.cfg,
        &other.host,
        &other.driver,
        &mut other.registry,
        &other.clock,
    )
    .await
    .unwrap();
    assert!(started.switching_disabled.is_some());
    seat(&mut other, &started);
    drop(started);
    let reference = core_of!(other);
    let theirs = switch_off_state!(reference, other);
    assert_eq!(ours, theirs);
}

/// Round 8b (b): A fails after J1, the fresh `from` starts, and the return's
/// +1 cannot be written. The switch ends `failed-to-load` with `returned`
/// false; only then does switching go off (no journal held, the reason in the
/// status), and the disabled run is spawned. Nothing reaches the disk after
/// the handler ends, and a supervisor restart on the same disk recovers the
/// open return.
#[tokio::test]
async fn a_failed_return_count_in_a_switch_waits_for_its_end_and_leaves_the_return_open() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    let b = keep(&rig, "2.0.0", b"binary two");
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    let started = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &started, false);
    drop(started);
    rig.disk.add_artifact("1.0.0", &f.a.sha256);
    rig.disk.add_artifact("2.0.0", &b.sha256);
    rig.disk.add_artifact("0.9.0", &f.p.sha256);
    rig.disk.add_pin("pin");
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![
            crate::testkit::Step::Sleep(Duration::from_secs(20)),
            crate::testkit::loaded(),
        ],
    );
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            crate::testkit::Step::Sleep(Duration::from_secs(3)),
            crate::testkit::Step::Exit(false, "killed"),
        ],
    );
    let mut request = crate::testkit::update(false);
    request.sha256 = b.sha256.clone();
    let root = rig.cfg.data.root().to_path_buf();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut core = core_of!(rig);
    let a = core.rt.active.expect("A runs");
    core.note_message(a, &crate::protocol::Message::Loaded { ms: 5 });
    rig.disk.fail_journal("J-L", 99);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit(
            "1.0.0",
            crate::driver::Event::Exited(crate::driver::ExitInfo {
                code: Some(1),
                success: false,
                text: "crashed".to_owned(),
            }),
        );
    };
    tokio::join!(core.handle_switch(request, tx), script);
    let result = crate::testkit::done_result(&crate::testkit::drain(&mut rx));
    assert_eq!(result["result"], "failed-to-load", "{result}");
    assert_eq!(result["phase"], "loading", "{result}");
    assert_eq!(result["returned"], false, "{result}");
    let order = rig.disk.order();
    assert!(order.iter().any(|name| name == "J-L!"), "{order:?}");
    // The transition applied when the handler ended.
    assert!(core.rt.journal.is_none(), "no journal is held");
    assert!(core.rt.returning.is_none(), "the return is abandoned");
    assert!(
        rig.shared
            .snapshot()
            .switching_disabled
            .as_deref()
            .is_some_and(|why| why.contains("ladder record")),
        "the reason is in the status"
    );
    // Nothing is written from here, and the disabled run is the one that runs.
    let seq = loaded_at(&root).seq;
    ticks!(core, rig, 2);
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "2.0.0 kept", "1.0.0 installed"],
        "the disabled run is spawned"
    );
    assert_eq!(rig.disk.order(), order, "a write reached the disk");
    assert_eq!(loaded_at(&root).seq, seq);
    let held = loaded_at(&root);
    assert!(
        held.switch.as_ref().is_some_and(|s| s.return_.is_some()),
        "the open return is on the disk"
    );
    assert!(held.outcomes.is_empty());
    assert!(
        held.ladder.is_none(),
        "the count that was never written is not on the disk: {:?}",
        held.ladder
    );
    rig.rt = None;
    let second = boot(&mut rig).await;
    assert!(
        second.worker.is_some(),
        "recovery starts the return's target"
    );
    assert_eq!(
        rig.driver.places().last().map(String::as_str),
        Some("1.0.0 kept")
    );
    assert!(
        loaded_at(&root).switch.is_none(),
        "start-up finished the open return it recovered"
    );
}

/// Round 8b, rule 1 and 3: a ladder write fails while a switch is open. The
/// provisional record goes back, in memory, to what the disk has, so a write
/// the switch makes before it ends does not persist it; nothing more is
/// written to the ladder; and the switching-off applies when the switch ends.
#[tokio::test]
async fn a_failed_ladder_write_with_a_switch_open_puts_the_record_back_and_waits_for_its_end() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    let started = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &started, false);
    drop(started);
    let root = rig.cfg.data.root().to_path_buf();
    let before = loaded_at(&root).ladder.expect("start-up counted its try");
    rig.disk.fail_journal("J-L", 1);
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("A runs");
    core.rt.workers.get_mut(&id).unwrap().awaiting_loaded = false;
    core.rt.switching = true;
    // The reset of A's loaded, written now (a switch is open) and failing.
    let tracked = &core.rt.workers[&id];
    let (run, spent) = (
        tracked.rung.clone().expect("A's rung"),
        tracked.spent.clone(),
    );
    core.journal_loaded(&run, &spent);
    assert!(core.rt.ladder_off.is_some(), "the switching-off is pending");
    assert!(core.rt.disabled.is_none(), "and not yet applied");
    assert_eq!(
        core.rt.journal.as_ref().unwrap().ladder,
        Some(before.clone()),
        "the record is the disk's again"
    );
    // A write the switch makes meanwhile carries the restored record.
    core.worker_lost(id, "scripted");
    assert_eq!(loaded_at(&root).ladder, Some(before));
    // The switch ends: the transition applies.
    core.rt.switching = false;
    core.apply_ladder_off();
    assert!(core.rt.disabled.is_some());
    assert!(core.rt.journal.is_none());
}

/// Round 8b, rule 3 on the return ladder: a failed J-R1 with a switch open puts
/// the provisional record back, so a write the switch makes before it ends does
/// not carry it; the switching-off waits for the switch's end.
#[tokio::test]
async fn a_failed_return_retarget_with_a_switch_open_puts_the_record_back() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, true);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let started = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &started, true);
    drop(started);
    let root = rig.cfg.data.root().to_path_buf();
    let mut core = core_of!(rig);
    let held = core.rt.returning.clone().expect("a return is open");
    let before = core.rt.journal.as_ref().unwrap().ladder.clone();
    let run = crate::ladder::run_of(core.cfg, &kept_rung(&f.p));
    let ladder = crate::ladder::record_start(
        core.cfg,
        None,
        &crate::ladder::Progress::on(run.clone(), 2),
        &run,
    );
    assert_ne!(
        before,
        Some(ladder.clone()),
        "the write would change the record"
    );
    core.rt.switching = true;
    rig.disk.fail_journal("J-R1", 3);
    assert!(!core.retarget_return(&held, &run, Some(ladder)));
    assert!(core.rt.ladder_off.is_some(), "the switching-off is pending");
    assert!(core.rt.disabled.is_none(), "and not yet applied");
    assert_eq!(
        core.rt.journal.as_ref().unwrap().ladder,
        before,
        "the record is the disk's again"
    );
    core.write_journal("probe")
        .expect("a write the switch makes");
    assert_eq!(loaded_at(&root).ladder, before);
}

// ---- round 9, item 1: a failed retarget puts the whole retarget back ----------

/// A pending install of `a` (the return's own target at entry), held in the
/// Core's journal and written, so the retarget to another rung supersedes it.
fn hold_an_install_of<D: crate::driver::Driver, H: crate::host::Host, S: crate::disk::Disk>(
    core: &mut crate::core::Core<'_, D, H, S>,
    a: &Artifact,
) {
    let journal = core.rt.journal.as_mut().unwrap();
    journal.install_pending = Some(open_ferry_update::journal::InstallPending {
        target: a.clone(),
        source: "versions/1.0.0".to_owned(),
        aside: "aside".to_owned(),
        attempts: 0,
        next_try: Some("2999-01-01T00:00:00Z".to_owned()),
        started: "2026-10-11T00:00:00Z".to_owned(),
        record_id: Some("inst-1".to_owned()),
    });
    core.write_journal("fixture").unwrap();
}

/// No switch open: a return's kept `from` rung fails to spawn three times, the
/// retarget to `previous` supersedes the pending install, and its J-R1 cannot be
/// written. Nothing of the retarget stays: no S2 and no J8 are written, the
/// disk's journal is what it was, and a restart recovers the open return with
/// the install still pending.
#[tokio::test]
async fn a_failed_return_retarget_with_no_switch_open_delivers_nothing_and_leaves_the_disk_alone() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, true);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, true);
    drop(first);
    let root = rig.cfg.data.root().to_path_buf();
    let mut core = core_of!(rig);
    hold_an_install_of(&mut core, &f.a);
    // Two more tries on A (its third was start-up's, the first its count).
    ticks!(core, rig, 2);
    let held = loaded_at(&root);
    assert!(held.install_pending.is_some());
    rig.disk.fail_journal("J-R1", 3);
    let order = rig.disk.order();
    ticks!(core, rig, 1);
    assert!(core.rt.disabled.is_some(), "switching is off");
    let after: Vec<String> = rig.disk.order().split_off(order.len());
    assert!(
        after.iter().all(|name| name != "S2" && name != "J8"),
        "nothing was delivered: {after:?}"
    );
    let now = loaded_at(&root);
    assert_eq!(now, held, "the disk's journal is unchanged");
    assert!(
        now.install_pending.is_some(),
        "the install is still pending"
    );
    assert!(
        now.switch
            .as_ref()
            .and_then(|switch| switch.return_.as_ref())
            .is_some_and(|ret| ret.target == f.a),
        "the open return has its old target"
    );
    drop(core);
    rig.rt = None;
    let before = open_return_of(&held);
    assert_eq!(before.target, f.a, "the open return has its old target");
    assert!(!before.installed, "at its kept location");
    let spent = before.ladder.as_ref().map(|ladder| ladder.attempts);
    assert_eq!(spent, Some(3), "A's three tries are the disk's progress");
    let second = boot(&mut rig).await;
    // A is spent on the disk: recovery goes on to the next rung, `previous`.
    assert_restart_recovers_the_old_return(&second, &rig, &before, &f.p, 1);
}

/// The open return the disk holds: target, location, ladder record, install.
struct Recovered {
    target: Artifact,
    installed: bool,
    ladder: Option<open_ferry_update::journal::LadderRecord>,
    install_pending: bool,
}

fn open_return_of(journal: &Journal) -> Recovered {
    let ret = journal
        .switch
        .as_ref()
        .and_then(|switch| switch.return_.as_ref())
        .expect("an open return");
    Recovered {
        target: ret.target.clone(),
        installed: ret.installed,
        ladder: journal.ladder.clone(),
        install_pending: journal.install_pending.is_some(),
    }
}

/// What a restart on that disk does with the open return: it goes on from the
/// ladder progress the disk had. `run` is the rung it starts (version and
/// location) and `attempts` the count after its own start; the pending install
/// is still there.
fn assert_restart_recovers_the_old_return(
    started: &Started,
    rig: &Rig,
    before: &Recovered,
    run: &Artifact,
    attempts: u32,
) {
    let started_run = started.run.as_ref().expect("recovery starts a run");
    assert_eq!(&started_run.artifact, run, "the rung recovery starts");
    assert!(
        started_run.binary != rig.cfg.installed,
        "at its kept location"
    );
    let journal = started.journal.as_ref().expect("a journal");
    assert!(
        journal.install_pending.is_some(),
        "the install is still pending"
    );
    assert_eq!(
        journal.ladder.as_ref().map(|ladder| ladder.attempts),
        Some(attempts),
        "the ladder goes on from the disk's {:?}",
        before.ladder
    );
    assert!(before.install_pending);
}

/// Through the real handler route: A fails after J1 with its rung spent, the
/// switch's return starts (`fresh_from`): its first decision lands (the return
/// is A's kept copy) and the retarget to `previous` supersedes the pending
/// install, but its three J-R1 tries fail. Nothing of the retarget stays: no
/// S2, no J8; before the deferred switch-off drops it, the in-memory journal is
/// the pre-retarget copy; the disk's journal is what the first decision left;
/// and a restart on that disk recovers the old target (version and location),
/// the ladder progress the disk had, and the install still pending.
#[tokio::test]
async fn a_failed_return_retarget_in_a_switch_delivers_nothing_and_leaves_the_install_pending() {
    let (mut rig, _dirs, f, b) = switch_rig().await;
    let mut request = crate::testkit::update(false);
    request.sha256 = b.sha256.clone();
    let root = rig.cfg.data.root().to_path_buf();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut core = core_of!(rig);
    let a = core.rt.active.expect("A runs");
    hold_an_install_of(&mut core, &f.a);
    // A is on its last try: its exit spends the rung, so the return goes on to
    // `previous`.
    {
        let worker = core.rt.workers.get_mut(&a).unwrap();
        worker.attempts = 2;
        worker.awaiting_loaded = true;
    }
    // The first return decision lands; the retarget's three tries fail.
    rig.disk.fail_journal_after("J-R1", 1, 3);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit("1.0.0", a_crash());
    };
    tokio::join!(core.handle_switch(request, tx), script);
    let result = crate::testkit::done_result(&crate::testkit::drain(&mut rx));
    assert_eq!(result["result"], "failed-to-load", "{result}");
    assert_eq!(result["returned"], false, "{result}");
    let order = rig.disk.order();
    assert_eq!(
        order.iter().filter(|name| *name == "J-R1!").count(),
        3,
        "the retarget's three tries failed: {order:?}"
    );
    assert!(
        order.iter().all(|name| name != "S2" && name != "J8"),
        "nothing of the superseded install was delivered: {order:?}"
    );
    // The disk has what the first decision left, and so did memory.
    let landed = rig
        .disk
        .journal_log()
        .into_iter()
        .rfind(|(name, _)| name == "J-R1")
        .expect("the first decision landed")
        .1;
    assert_eq!(
        rig.disk.on_disk().as_ref(),
        Some(&landed),
        "the disk is what the first decision left"
    );
    assert_eq!(
        core.rt.settle_snapshot.as_ref(),
        Some(&landed),
        "the in-memory journal was the pre-retarget copy"
    );
    let held = loaded_at(&root);
    assert!(held.install_pending.is_some(), "the install is pending");
    assert!(held.outcomes.iter().all(|o| o.record.id != "inst-1"));
    let before = open_return_of(&held);
    assert_eq!(before.target, f.a, "the old target");
    assert!(!before.installed, "at its kept location");
    drop(core);
    rig.rt = None;
    let second = boot(&mut rig).await;
    let held = before.ladder.as_ref().map_or(0, |ladder| ladder.attempts);
    assert_restart_recovers_the_old_return(&second, &rig, &before, &f.a, held + 1);
    assert_eq!(
        rig.driver.places().last().map(String::as_str),
        Some("1.0.0 kept"),
        "recovery runs the old target"
    );
}

// ---- round 9, items 4 and 6: the handler's return, J-R2 and the uncount --------

/// A real-journal rig on A (kept 1.0.0, a rung with `previous` and the
/// installed file behind it), the pieces of a switch to B (2.0.0, held loading)
/// scripted: A's exit, one second in, starts the Core's return inside the
/// handler (`fresh_from`).
async fn switch_rig() -> (Rig, Dirs, Fallback, Artifact) {
    let (mut rig, dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    let b = keep(&rig, "2.0.0", b"binary two");
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    let started = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &started, false);
    drop(started);
    rig.disk.add_artifact("1.0.0", &f.a.sha256);
    rig.disk.add_artifact("2.0.0", &b.sha256);
    rig.disk.add_artifact("0.9.0", &f.p.sha256);
    rig.disk.add_pin("pin");
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![
            crate::testkit::Step::Sleep(Duration::from_secs(20)),
            crate::testkit::loaded(),
        ],
    );
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            crate::testkit::Step::Sleep(Duration::from_secs(3)),
            crate::testkit::Step::Exit(false, "killed"),
        ],
    );
    (rig, dirs, f, b)
}

fn a_crash() -> crate::driver::Event {
    crate::driver::Event::Exited(crate::driver::ExitInfo {
        code: Some(1),
        success: false,
        text: "crashed".to_owned(),
    })
}

/// Item 4: a return that succeeds inside the handler and whose J-R2 cannot be
/// written. J-R2 records a start that was made, so it rolls forward with the
/// journal retry (not a ladder write, no switch-off): the replacement runs,
/// `returned` is true, the switch ends `failed-to-load`, switching stays on and
/// the retry lands J-R2 later. With `crash_first` the supervisor restarts
/// before the retry: the disk has the open return at the rung J-R1 counted, as
/// after a crash just before J-R2.
async fn j_r2_fails_in_the_handler(crash_first: bool) {
    let (mut rig, _dirs, f, b) = switch_rig().await;
    let mut request = crate::testkit::update(false);
    request.sha256 = b.sha256.clone();
    let root = rig.cfg.data.root().to_path_buf();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut core = core_of!(rig);
    rig.disk.fail_journal("J-R2", 1);
    rig.disk.fail_journal("J8", 1);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit("1.0.0", a_crash());
    };
    tokio::join!(core.handle_switch(request, tx), script);
    let result = crate::testkit::done_result(&crate::testkit::drain(&mut rx));
    assert_eq!(result["result"], "failed-to-load", "{result}");
    assert_eq!(result["returned"], true, "{result}");
    assert!(core.rt.disabled.is_none(), "switching stays on");
    assert!(core.rt.ladder_off.is_none());
    assert!(core.rt.journal_stale, "J-R2 is waiting for its retry");
    assert!(core.rt.active.is_some(), "the replacement runs");
    let order = rig.disk.order();
    assert!(order.iter().any(|name| name == "J-R2!"), "{order:?}");
    let held = loaded_at(&root);
    let target = held
        .switch
        .as_ref()
        .and_then(|switch| switch.return_.as_ref())
        .map(|ret| ret.target.clone());
    assert_eq!(
        target,
        Some(f.a.clone()),
        "the open return, at the rung J-R1 counted"
    );
    if crash_first {
        drop(core);
        rig.rt = None;
        let second = boot(&mut rig).await;
        assert!(second.worker.is_some(), "recovery finishes the open return");
        assert_eq!(
            rig.driver.places().last().map(String::as_str),
            Some("1.0.0 kept")
        );
        assert!(loaded_at(&root).switch.is_none());
    } else {
        tokio::time::advance(rig.cfg.timings.status_every * 2).await;
        core.on_tick().await;
        assert!(!core.rt.journal_stale, "the retry landed");
        assert!(rig.disk.order().iter().any(|name| name == "J-retry"));
        assert!(loaded_at(&root).switch.is_none(), "J-R2 is on the disk");
    }
}

#[tokio::test]
async fn a_failed_j_r2_in_the_handler_rolls_forward_and_the_retry_lands_it() {
    j_r2_fails_in_the_handler(false).await;
}

#[tokio::test]
async fn a_failed_j_r2_in_the_handler_leaves_the_open_return_for_a_restart_before_the_retry() {
    j_r2_fails_in_the_handler(true).await;
}

/// How the installed path is a link, for item 5.
#[derive(Clone, Copy, PartialEq)]
enum Linked {
    No,
    /// The fake disk's flag.
    Fake,
    /// A real symlink to the supervisor's own bytes (Unix).
    #[cfg(unix)]
    Symlink,
}

/// Item 4 (test 2), item 5 and item 6: inside the handler the return's kept
/// rung is refused as untrusted and the uncount cannot be written. Switching
/// goes off at the handler's end; the disk keeps the count. Unlinked, the
/// switched-off run (the installed file) starts at once, and not on a new
/// cycle's backoff, though the restart the handler scheduled carried the
/// installed rung as already spent. With a linked installed file the Core is
/// degraded, as start-up is: no run, nothing spawned, now or later.
async fn uncount_fails_in_the_handler(linked: Linked) {
    let (mut rig, _dirs, f, b) = switch_rig().await;
    match linked {
        Linked::No => {}
        Linked::Fake => rig.disk.link_installed(),
        #[cfg(unix)]
        Linked::Symlink => {
            let target = rig.cfg.installed.with_file_name("installed-target");
            std::fs::rename(&rig.cfg.installed, &target).unwrap();
            std::os::unix::fs::symlink(&target, &rig.cfg.installed).unwrap();
        }
    }
    rig.driver.fail_spawn("1.0.0", SpawnFail::UntrustedKept);
    let mut request = crate::testkit::update(false);
    request.sha256 = b.sha256.clone();
    let root = rig.cfg.data.root().to_path_buf();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut core = core_of!(rig);
    let a = core.rt.active.expect("A runs");
    // The installed rung is spent in A's own progress, so the restart that the
    // handler schedules carries it.
    let installed = crate::startup::installed_as_own_by(
        core.cfg,
        Some(&f.i.sha256),
        f.i.size,
        Some(&f.i.sha256),
        &crate::host::Host::own_version(core.host),
    )
    .unwrap();
    core.rt.workers.get_mut(&a).unwrap().spent = vec![installed];
    let before = loaded_at(&root)
        .ladder
        .as_ref()
        .map(|ladder| ladder.attempts);
    // The count lands; its take-back does not.
    rig.disk.fail_journal_after("J-L", 1, 99);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit("1.0.0", a_crash());
    };
    tokio::join!(core.handle_switch(request, tx), script);
    let result = crate::testkit::done_result(&crate::testkit::drain(&mut rx));
    assert_eq!(result["result"], "failed-to-load", "{result}");
    let order = rig.disk.order();
    assert!(order.iter().any(|name| name == "J-L!"), "{order:?}");
    assert!(core.rt.disabled.is_some(), "switching is off");
    assert!(core.rt.journal.is_none());
    assert_eq!(
        loaded_at(&root)
            .ladder
            .as_ref()
            .map(|ladder| ladder.attempts),
        before.map(|attempts| attempts + 1),
        "the disk keeps the count"
    );
    let placed = rig.driver.places();
    core.on_tick().await;
    if linked == Linked::No {
        // The switched-off run starts on the next tick, with no backoff.
        assert_eq!(
            rig.driver.places().last().map(String::as_str),
            Some("1.0.0 installed"),
            "{:?} degraded: {:?}",
            rig.driver.places(),
            core.rt.degraded
        );
    } else {
        assert!(core.rt.run.is_none(), "a linked file is not run");
        assert!(core.rt.degraded.is_some(), "the Core is degraded");
        tokio::time::advance(Duration::from_secs(3600)).await;
        core.on_tick().await;
        assert_eq!(rig.driver.places(), placed, "nothing is spawned later");
    }
    assert_eq!(rig.disk.order(), order, "nothing more reached the disk");
}

#[tokio::test]
async fn a_failed_uncount_in_the_handler_switches_off_at_its_end_and_starts_the_installed_run_at_once()
 {
    uncount_fails_in_the_handler(Linked::No).await;
}

#[tokio::test]
async fn a_switch_off_with_a_linked_installed_file_degrades_the_core_as_start_up_does() {
    uncount_fails_in_the_handler(Linked::Fake).await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_switch_off_with_a_symlinked_installed_file_degrades_the_core_as_start_up_does() {
    uncount_fails_in_the_handler(Linked::Symlink).await;
}

// ---- round 10, item 1: a panic re-entry is a restart ---------------------------

/// A re-entered run, checked as a fresh process on the same disk would be:
/// switching on, the start counted, a worker running.
fn assert_started_like_a_fresh_process(started: &Started, before: Option<u32>) {
    assert_eq!(started.switching_disabled, None, "switching is on");
    assert!(!started.switched_off, "the switch-off is not carried");
    assert!(!started.ladder_off_reason);
    let journal = started.journal.as_ref().expect("the journal is back");
    assert_eq!(
        journal.ladder.as_ref().map(|ladder| ladder.attempts),
        before.map(|attempts| attempts + 1),
        "the start was counted"
    );
    assert!(started.worker.is_some(), "a worker runs");
}

/// A start-up whose count could not be written (switching off, the installed
/// file runs), the journal then repaired on the disk, with an outcome waiting in
/// it for the next recovery, and a panic re-entry.
async fn reentered_after_a_start_up_switch_off() -> (Rig, Dirs, Started, Option<u32>) {
    reentered_after_a_start_up_switch_off_with(false).await
}

/// As above. With `break_state` the state file is unreadable while recovery
/// runs (a real failure of its S3 write: the write reads the file strictly),
/// then put back, so the outcome stays pending in the journal and recovery's
/// own S3 delivery is what failed.
async fn reentered_after_a_start_up_switch_off_with(
    break_state: bool,
) -> (Rig, Dirs, Started, Option<u32>) {
    let (mut rig, dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    let mut started = boot(&mut rig).await;
    let good = std::fs::read(store(&rig).path()).unwrap();
    std::fs::write(store(&rig).path(), b"not a journal").unwrap();
    crate::startup::launch(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &mut started,
    )
    .await;
    assert!(started.switched_off, "start-up switched off");
    assert!(started.ladder_off_reason);
    // The disk is as it was before the write that failed, and an outcome waits.
    std::fs::write(store(&rig).path(), good).unwrap();
    let before = recorded_attempts(&rig);
    let mut journal = loaded(&rig);
    journal.outcomes.push(open_ferry_update::journal::Outcome {
        record: open_ferry_update::state::SwitchRecord {
            id: "sw-waiting".to_owned(),
            from: "1.0.0".to_owned(),
            to: "2.0.0".to_owned(),
            result: Some("failed-to-load".to_owned()),
            ..Default::default()
        },
        mark_failed: None,
    });
    store(&rig).write(&mut journal, "fixture").unwrap();
    let state_path = rig.cfg.data.state_file();
    let state_bytes = std::fs::read(&state_path).ok();
    if break_state {
        std::fs::write(&state_path, b"not a state").unwrap();
    }
    crate::startup::reenter(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
        &mut started,
    )
    .await
    .unwrap();
    if break_state {
        match state_bytes {
            Some(bytes) => std::fs::write(&state_path, bytes).unwrap(),
            None => std::fs::remove_file(&state_path).unwrap(),
        }
    }
    (rig, dirs, started, before)
}

fn state_has(rig: &Rig, id: &str) -> bool {
    open_ferry_update::state::State::load(&rig.cfg.data.state_file())
        .history
        .iter()
        .any(|record| record.id == id)
}

#[tokio::test]
async fn a_panic_re_entry_after_a_start_up_switch_off_starts_like_a_fresh_process() {
    let (rig, _dirs, started, before) = reentered_after_a_start_up_switch_off().await;
    assert_started_like_a_fresh_process(&started, before);
    assert!(
        state_has(&rig, "sw-waiting"),
        "the recovered outcome was delivered"
    );
    assert!(loaded(&rig).outcomes.is_empty());
}

#[tokio::test]
async fn after_a_panic_re_entry_a_failed_s3_is_retried_and_delivered() {
    let (mut rig, _dirs, started, _before) = reentered_after_a_start_up_switch_off().await;
    tokio::time::pause();
    seat(&mut rig, &started);
    assert!(!rig.rt.as_ref().unwrap().switched_off);
    rig.disk.fail_state("S3", 1);
    let outcome = open_ferry_update::journal::Outcome {
        record: open_ferry_update::state::SwitchRecord {
            id: "sw-after".to_owned(),
            from: "1.0.0".to_owned(),
            to: "2.0.0".to_owned(),
            result: Some("failed-to-load".to_owned()),
            ..Default::default()
        },
        mark_failed: None,
    };
    let mut core = core_of!(rig);
    assert!(
        !core.write_state("S3", &outcome.clone(), false),
        "the first write fails"
    );
    ticks!(core, rig, 3);
    drop(core);
    let order = rig.disk.order();
    let failed = order
        .iter()
        .position(|name| name == "S3!")
        .expect("S3 failed once");
    assert!(
        order[failed..].iter().any(|name| name == "S3"),
        "the retry delivered it: {order:?}"
    );
}

/// Round 10b: the path the round-9 defect was in. Recovery's own S3 delivery of
/// a waiting outcome fails, so recovery leaves it pending (`state_pending`, the
/// outcome still in the journal); the Runtime imports it into its queue
/// (`in_journal`), a first tick's S3 fails once more, and the retry delivers it.
#[tokio::test]
async fn a_recovered_pending_outcome_is_delivered_after_a_re_entry_and_one_transient_s3_failure() {
    let (mut rig, _dirs, started, before) = reentered_after_a_start_up_switch_off_with(true).await;
    assert_started_like_a_fresh_process(&started, before);
    assert!(started.state_pending, "recovery left the outcome pending");
    assert!(!state_has(&rig, "sw-waiting"), "the state has not got it");
    assert!(
        loaded(&rig)
            .outcomes
            .iter()
            .any(|outcome| outcome.record.id == "sw-waiting"),
        "the journal still holds it"
    );
    tokio::time::pause();
    seat(&mut rig, &started);
    {
        let runtime = rig.rt.as_ref().unwrap();
        assert!(!runtime.switched_off);
        let queued = runtime
            .pending
            .iter()
            .find(|pending| pending.outcome.record.id == "sw-waiting")
            .expect("the outcome is in the queue");
        assert!(queued.in_journal);
        assert_eq!(queued.name, "S3");
    }
    rig.disk.fail_state("S3", 1);
    let mut core = core_of!(rig);
    ticks!(core, rig, 4);
    drop(core);
    let order = rig.disk.order();
    let failed = order
        .iter()
        .position(|name| name == "S3!")
        .expect("S3 failed once");
    assert!(
        order[failed..].iter().any(|name| name == "S3"),
        "the retry delivered it: {order:?}"
    );
    // The state the Core wrote to (the fake disk's) has the record.
    let record = rig
        .disk
        .record("sw-waiting")
        .expect("the state has the outcome's record");
    assert_eq!(record.result.as_deref(), Some("failed-to-load"));
    assert!(
        loaded(&rig).outcomes.is_empty(),
        "the outcome left the journal"
    );
}

/// The Core's own switch-off lives only in the Runtime, which the panic drops,
/// so this test does not depend on the reset in `reenter` (reverting it does not
/// fail this test); it is kept as the Core-route guard that a re-entry after the
/// Core's switch-off starts like a fresh process.
#[tokio::test]
async fn a_panic_re_entry_after_the_cores_switch_off_starts_like_a_fresh_process() {
    let (mut rig, _dirs) = real_rig().await;
    rig.cfg.runner = std::sync::Arc::new(ScriptedRunner(Ok("open-ferry 0.5.0".to_owned())));
    let f = ladder_fixture(&rig, false);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let mut started = boot(&mut rig).await;
    assert!(started.run_failed);
    rig.driver.heal_spawn("1.0.0");
    tokio::time::pause();
    seat_expecting(&mut rig, &started, false);
    let before = recorded_attempts(&rig);
    {
        rig.disk.fail_journal("J-L", 1);
        let mut core = core_of!(rig);
        ticks!(core, rig, 1);
        assert!(core.rt.disabled.is_some() && core.rt.switched_off);
    }
    rig.rt = None;
    let _ = f;
    crate::startup::reenter(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &rig.clock,
        &mut started,
    )
    .await
    .unwrap();
    assert_started_like_a_fresh_process(&started, before);
    seat(&mut rig, &started);
    let runtime = rig.rt.as_ref().unwrap();
    assert!(!runtime.switched_off && runtime.disabled.is_none());
}

// ---- round 10, item 2: the restart flag's clear is read-only after a switch-off --

/// An older supervisor version set `needs_restart`, and start-up's ladder write
/// failed: the flag check reads and writes nothing (no S4), the runtime's
/// answer is `false` (the flag holds only for the version that set it), and
/// the flag stays set in the file for the next start to clear.
#[tokio::test]
async fn after_a_start_up_switch_off_the_restart_flag_check_writes_no_s4() {
    let (mut rig, _dirs) = real_rig().await;
    let f = ladder_fixture(&rig, false);
    *rig.host.own_sha256.lock().unwrap() = Some(f.i.sha256.clone());
    let mut started = boot(&mut rig).await;
    let state = open_ferry_update::state::State {
        needs_restart: true,
        supervisor_version: Some("0.0.1-older".to_owned()),
        ..Default::default()
    };
    state
        .save_named(&rig.cfg.data.state_file(), "fixture")
        .unwrap();
    let file = std::fs::read(rig.cfg.data.state_file()).unwrap();
    std::fs::write(store(&rig).path(), b"not a journal").unwrap();
    crate::startup::launch(
        &rig.cfg,
        &rig.host,
        &rig.driver,
        &mut rig.registry,
        &mut started,
    )
    .await;
    assert!(started.switched_off, "start-up switched off");
    let needs = crate::supervise::needs_restart_after_recovery(&rig.cfg, &rig.host, &started);
    assert!(!needs, "an older version's flag is not this version's");
    assert_eq!(
        std::fs::read(rig.cfg.data.state_file()).unwrap(),
        file,
        "no S4 was written"
    );
    let state = open_ferry_update::state::State::load_strict(&rig.cfg.data.state_file()).unwrap();
    assert!(state.needs_restart, "the flag is still set in the file");
}

// ---- a clean service stop takes the start count back ---------------------------

/// The J-L writes the fake disk has seen.
fn ladder_writes(disk: &crate::testkit::FakeDisk) -> usize {
    disk.journal_log()
        .iter()
        .filter(|(name, _)| name == "J-L")
        .count()
}

/// Test 1: start-up's worker is held before `loaded`, then a service stop. The
/// count goes back to what it was before the start (1, 2, 1).
#[tokio::test]
async fn a_clean_stop_while_start_ups_worker_loads_takes_its_try_back() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    assert_eq!(recorded_attempts(&rig), Some(1));
    drop(first);
    let second = boot(&mut rig).await;
    assert_eq!(recorded_attempts(&rig), Some(2), "the second start counted");
    tokio::time::pause();
    seat_expecting(&mut rig, &second, false);
    drop(second);
    let mut core = core_of!(rig);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(recorded_attempts(&rig), Some(1), "the stopped try is back");
}

/// Test 2: a restart's worker: one failed start, the restart is counted, then a
/// stop before `loaded`. The earlier failure stays (2, 1), the rest of the
/// record is as it was.
#[tokio::test]
async fn a_clean_stop_while_a_restarts_worker_loads_keeps_the_earlier_failure() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    rig.driver.heal_spawn("1.0.0");
    let mut core = core_of!(rig);
    ticks!(core, rig, 1);
    assert!(core.rt.active.is_some(), "the restart's worker runs");
    let before = core
        .rt
        .journal
        .as_ref()
        .unwrap()
        .ladder
        .clone()
        .expect("a record");
    assert_eq!(before.attempts, 2, "the failure and the restart's count");
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    let after = loaded(&rig).ladder.expect("a record");
    assert_eq!(
        after,
        LadderRecord {
            attempts: 1,
            ..before
        },
        "only the stopped try is taken back"
    );
}

/// Test 3: three runs in a row, each stopped before `loaded`: the fourth start
/// picks the same rung, not `previous`.
#[tokio::test]
async fn three_clean_stops_in_a_row_do_not_spend_the_rung() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    tokio::time::pause();
    for run in 1..=3 {
        let started = boot(&mut rig).await;
        assert!(started.worker.is_some(), "run {run} starts");
        seat_expecting(&mut rig, &started, false);
        drop(started);
        let mut core = core_of!(rig);
        assert_eq!(core.stop().await, crate::core::Exit::Stopped);
        drop(core);
        assert_eq!(recorded_attempts(&rig), Some(0), "run {run} is taken back");
    }
    let fourth = boot(&mut rig).await;
    assert!(fourth.worker.is_some() && !fourth.run_failed);
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "1.0.0 kept", "1.0.0 kept", "1.0.0 kept"],
        "the same rung every time, never previous"
    );
    assert_eq!(recorded_attempts(&rig), Some(1));
}

/// Test 4: after `loaded` a stop writes nothing.
#[tokio::test]
async fn a_stop_after_loaded_writes_nothing() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let held = store(&rig);
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("a worker");
    core.note_message(id, &crate::protocol::Message::Loaded { ms: 5 });
    let before = match held.load().unwrap() {
        JournalLoad::Loaded(journal) => journal,
        other => panic!("no journal: {other:?}"),
    };
    assert_eq!(
        before.ladder.as_ref().map(|l| l.attempts),
        Some(0),
        "loaded reset the count"
    );
    let writes = rig.disk.journal_log().len();
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(
        rig.disk.journal_log().len(),
        writes,
        "no journal write at all"
    );
    assert_eq!(loaded(&rig), before, "the record is unchanged");
}

/// Test 5: the take-back write fails once: the count stays on the disk,
/// switching stays on, one warning is logged.
#[tokio::test]
async fn a_failed_take_back_leaves_the_count_and_keeps_switching_on() {
    let (_guard, lines) = crate::switch_tests::capture_logs();
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    assert_eq!(recorded_attempts(&rig), Some(1));
    rig.disk.fail_journal("J-L", 1);
    let mut core = core_of!(rig);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    assert!(core.rt.ladder_off.is_none(), "switching is not turned off");
    assert!(!core.rt.switched_off && core.rt.disabled.is_none());
    let held = core.rt.journal.as_ref().unwrap().ladder.as_ref().unwrap();
    assert_eq!(held.attempts, 1, "the record in memory is the disk's again");
    drop(core);
    assert_eq!(recorded_attempts(&rig), Some(1), "the count stays");
    let warned = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("could not be taken back"))
        .count();
    assert_eq!(warned, 1, "one warning");
}

/// Test 6: with switching off (`ladder_off` set) the stop writes nothing.
#[tokio::test]
async fn a_stop_with_switching_off_writes_nothing() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    core.rt.ladder_off = Some("switching is off: test".to_owned());
    let writes = rig.disk.journal_log().len();
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(rig.disk.journal_log().len(), writes, "no journal write");
    assert_eq!(recorded_attempts(&rig), Some(1), "the count stays");
}

/// Test 7: the worker's channel was lost before the stop: the count stays.
#[tokio::test]
async fn a_stop_after_the_workers_channel_was_lost_keeps_the_count() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("a worker");
    core.worker_lost(id, "the channel was lost");
    assert!(core.rt.workers[&id].lost);
    let writes = ladder_writes(&rig.disk);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(ladder_writes(&rig.disk), writes, "no J-L write");
    assert_eq!(recorded_attempts(&rig), Some(1), "the count stays");
}

/// Test 8: the take-back changes only `attempts` in the ladder record: every
/// other field, `switch` (the key a return's rung carries) included, is kept.
/// Recovery clears an open return before start-up's worker runs, so the key is
/// set on the Core's record by hand; the write under test is the same one. This
/// does not claim anything about an open return in the journal.
#[tokio::test]
async fn a_clean_stop_keeps_every_ladder_field_but_the_count() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, true);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat(&mut rig, &first);
    drop(first);
    let mut core = core_of!(rig);
    let journal = core.rt.journal.as_mut().expect("a journal");
    let ladder = journal.ladder.as_mut().expect("a record");
    ladder.switch = Some("sw-000000000000aaaa-1".to_owned());
    let ladder = ladder.clone();
    assert_eq!(ladder.attempts, 1);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    let after = loaded(&rig).ladder.expect("a record");
    assert!(after.switch.is_some(), "the key is still on the disk");
    assert_eq!(
        after,
        LadderRecord {
            attempts: 0,
            ..ladder
        }
    );
}

// ---- fix round 1 -----------------------------------------------------------

/// The attempts the Core holds in memory for its ladder record.
macro_rules! held_attempts {
    ($core:ident) => {
        $core
            .rt
            .journal
            .as_ref()
            .and_then(|journal| journal.ladder.as_ref())
            .map(|ladder| ladder.attempts)
    };
}

/// The `kill <version>` lines the fake driver logged.
fn kills(driver: &crate::testkit::FakeDriver, version: &str) -> usize {
    let line = format!("kill {version}");
    driver.log().iter().filter(|l| **l == line).count()
}

/// A worker's exit with failure, as the registry would queue it.
fn crashed(id: u64) -> (u64, crate::driver::Event) {
    (
        id,
        crate::driver::Event::Exited(crate::driver::ExitInfo {
            code: Some(1),
            success: false,
            text: "crashed".to_owned(),
        }),
    )
}

/// R1: an active start-up worker that greets and never sends `loaded` is
/// killed at its deadline (not before), the try stays counted, and the next
/// start follows the ladder.
#[tokio::test]
async fn an_active_start_up_worker_that_never_loads_is_killed_at_the_deadline() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let load = rig.cfg.timings.active_load;
    let mut core = core_of!(rig);
    tokio::time::advance(load - Duration::from_secs(1)).await;
    core.on_tick().await;
    assert_eq!(kills(&rig.driver, "1.0.0"), 0, "not before the deadline");
    tokio::time::advance(Duration::from_secs(2)).await;
    core.on_tick().await;
    // The worker logs the kill when it gets to it; its exit follows.
    let (id, event) = tokio::time::timeout(Duration::from_secs(5), core.registry.next_event())
        .await
        .expect("the worker was killed and exited");
    assert_eq!(kills(&rig.driver, "1.0.0"), 1, "killed at the deadline");
    core.background_event(id, event).await;
    assert_eq!(held_attempts!(core), Some(1), "the try stays counted");
    let started = ticks!(core, rig, 1);
    assert_eq!(started, [1], "the ladder goes on");
    assert_eq!(held_attempts!(core), Some(2), "the next try is counted");
    assert_eq!(rig.driver.places(), ["1.0.0 kept", "1.0.0 kept"]);
}

/// R1: the same for a restart's worker.
#[tokio::test]
async fn an_active_restart_worker_that_never_loads_is_killed_at_the_deadline() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    rig.driver.heal_spawn("1.0.0");
    let load = rig.cfg.timings.active_load;
    let mut core = core_of!(rig);
    assert_eq!(ticks!(core, rig, 1), [1], "the restart's worker runs");
    assert_eq!(held_attempts!(core), Some(2));
    tokio::time::advance(load - Duration::from_secs(1)).await;
    core.on_tick().await;
    assert_eq!(kills(&rig.driver, "1.0.0"), 0, "not before the deadline");
    tokio::time::advance(Duration::from_secs(2)).await;
    core.on_tick().await;
    // The worker logs the kill when it gets to it; its exit follows.
    let (id, event) = tokio::time::timeout(Duration::from_secs(5), core.registry.next_event())
        .await
        .expect("the worker was killed and exited");
    assert_eq!(kills(&rig.driver, "1.0.0"), 1, "killed at the deadline");
    core.background_event(id, event).await;
    assert_eq!(held_attempts!(core), Some(2), "the try stays counted");
    assert_eq!(ticks!(core, rig, 1), [1], "the ladder goes on");
    assert_eq!(held_attempts!(core), Some(3));
}

/// R1: `loaded` just before the deadline clears it: no kill, ever.
#[tokio::test]
async fn loaded_just_before_the_deadline_clears_it() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let load = rig.cfg.timings.active_load;
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("a worker");
    tokio::time::advance(load - Duration::from_secs(1)).await;
    core.note_message(id, &crate::protocol::Message::Loaded { ms: 5 });
    assert!(core.rt.workers[&id].load_deadline.is_none());
    tokio::time::advance(load).await;
    core.on_tick().await;
    assert_eq!(kills(&rig.driver, "1.0.0"), 0, "no kill after `loaded`");
    assert_eq!(held_attempts!(core), Some(0), "loaded reset the count");
}

/// R1: a stop after the deadline has passed, before the timer fired, takes
/// nothing back.
#[tokio::test]
async fn a_stop_after_the_load_deadline_takes_nothing_back() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let load = rig.cfg.timings.active_load;
    let mut core = core_of!(rig);
    tokio::time::advance(load + Duration::from_secs(1)).await;
    let writes = ladder_writes(&rig.disk);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(ladder_writes(&rig.disk), writes, "no J-L write");
    assert_eq!(recorded_attempts(&rig), Some(1), "the count stays");
}

/// R2: a failure queued when Stop wins the select is not refunded.
#[tokio::test]
async fn a_failure_queued_before_the_stop_is_not_refunded() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("a worker");
    core.registry.events().send(crashed(id)).unwrap();
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(recorded_attempts(&rig), Some(1), "the crash counts");
}

/// R2: a worker that fails by itself during the stop budget is not refunded.
#[tokio::test]
async fn a_failure_exit_during_the_stop_is_not_refunded() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.on(
        "1.0.0",
        "stop",
        vec![crate::testkit::Step::Exit(false, "crashed")],
    );
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(recorded_attempts(&rig), Some(1), "the crash counts");
}

/// R2: a `load_failed` during the stop is not refunded, even if the worker
/// then exits with success.
#[tokio::test]
async fn a_load_failed_during_the_stop_is_not_refunded() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.on(
        "1.0.0",
        "stop",
        vec![
            crate::testkit::Step::Msg(crate::protocol::Message::LoadFailed {
                reason: "no".to_owned(),
            }),
            crate::testkit::Step::Exit(true, "stopped"),
        ],
    );
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(recorded_attempts(&rig), Some(1), "the failure counts");
}

/// R2: a worker that ignores `stop` and is killed by the registry at the stop
/// deadline is refunded: the supervisor ended it. (A graceful exit 0 is the
/// refund of tests 1-3.)
#[tokio::test]
async fn a_kill_at_the_stop_deadline_is_refunded() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.on("1.0.0", "stop", Vec::new());
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(kills(&rig.driver, "1.0.0"), 1, "the registry killed it");
    assert_eq!(recorded_attempts(&rig), Some(0), "the stopped try is back");
}

/// The stale guard: a stop while the disk is behind the journal writes no J-L
/// and leaves the journal on disk as it is.
#[tokio::test]
async fn a_stop_while_the_journal_is_stale_writes_nothing() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let before = loaded(&rig);
    let writes = ladder_writes(&rig.disk);
    let mut core = core_of!(rig);
    core.rt.journal_stale = true;
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(ladder_writes(&rig.disk), writes, "no J-L write");
    assert_eq!(loaded(&rig), before, "the journal on disk is unchanged");
}

/// A record with spent rungs, refunded from the third try (3 to 2): `spent`
/// and everything else in the record is kept.
#[tokio::test]
async fn a_take_back_from_the_third_try_keeps_the_spent_rungs() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    rig.driver.fail_spawn(
        "0.9.0",
        SpawnFail::Script(vec![SpawnFail::Start, SpawnFail::Start]),
    );
    let first = boot(&mut rig).await;
    assert!(first.run_failed);
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    // 1.0.0 fails three times (boot and two restarts), then 0.9.0 twice.
    assert_eq!(ticks!(core, rig, 5), [1, 1, 1, 1, 1]);
    assert!(core.rt.active.is_some(), "0.9.0's third try runs");
    let before = core
        .rt
        .journal
        .as_ref()
        .and_then(|journal| journal.ladder.clone())
        .expect("a record");
    assert_eq!(before.attempts, 3, "the third try of its rung");
    assert!(!before.spent.is_empty(), "1.0.0 is spent");
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    let after = loaded(&rig).ladder.expect("a record");
    assert_eq!(
        after,
        LadderRecord {
            attempts: 2,
            ..before
        }
    );
}

// ---- fix round 2 -----------------------------------------------------------

/// R1: an active worker's deadline is `active_load`, not `load`: it is not
/// ended a little after `load`, and is ended at `active_load`.
#[tokio::test]
async fn the_active_deadline_is_active_load_not_load() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let (load, active_load) = (rig.cfg.timings.load, rig.cfg.timings.active_load);
    assert!(active_load > load, "the active wait is the longer one");
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("a worker");
    tokio::time::advance(load + Duration::from_secs(10)).await;
    core.on_tick().await;
    // The state, not the fake worker's log: the log is written only when the
    // worker's task gets to the kill command.
    assert!(!core.rt.workers[&id].load_timed_out, "not at `load`");
    tokio::time::advance(active_load - load).await;
    core.on_tick().await;
    let (_, event) = tokio::time::timeout(Duration::from_secs(5), core.registry.next_event())
        .await
        .expect("the worker was killed and exited");
    assert!(matches!(event, crate::driver::Event::Exited(_)));
    assert_eq!(kills(&rig.driver, "1.0.0"), 1, "killed at `active_load`");
}

/// R1a: the timeout is final. A `loaded` after it, before the exit, does not
/// clear `awaiting_loaded`, reset the count or write; the failed start is
/// charged and the ladder goes on.
#[tokio::test]
async fn a_loaded_after_the_load_timeout_does_not_undo_the_failed_start() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let active_load = rig.cfg.timings.active_load;
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("a worker");
    tokio::time::advance(active_load + Duration::from_secs(1)).await;
    core.on_tick().await;
    assert!(core.rt.workers[&id].load_timed_out);
    let writes = ladder_writes(&rig.disk);
    core.note_message(id, &crate::protocol::Message::Loaded { ms: 5 });
    assert!(core.rt.workers[&id].awaiting_loaded, "still a failed start");
    assert!(!core.rt.workers[&id].wants_peers_gone);
    assert_eq!(ladder_writes(&rig.disk), writes, "no `loaded` write");
    assert_eq!(held_attempts!(core), Some(1), "the count was not reset");
    let (id, event) = tokio::time::timeout(Duration::from_secs(5), core.registry.next_event())
        .await
        .expect("the worker was killed and exited");
    core.background_event(id, event).await;
    assert_eq!(held_attempts!(core), Some(1), "the try stays counted");
    assert_eq!(ticks!(core, rig, 1), [1], "the ladder goes on");
    assert_eq!(held_attempts!(core), Some(2), "the next try is counted");
}

/// R2(b): a worker that died by itself, its exit not yet seen when the
/// registry's kill is made, is not credited to the kill.
#[tokio::test]
async fn a_failure_that_the_kill_finds_already_exited_is_not_refunded() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.on("1.0.0", "stop", Vec::new());
    rig.driver.on(
        "1.0.0",
        "kill",
        vec![crate::testkit::Step::Died(false, "crashed")],
    );
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(kills(&rig.driver, "1.0.0"), 1, "the registry made its kill");
    assert_eq!(recorded_attempts(&rig), Some(1), "the crash counts");
}

/// R2(c): the channel failed after `stop` while the process lived, and the
/// deadline kill ended it: not refunded.
#[tokio::test]
async fn a_channel_failure_after_stop_then_a_kill_is_not_refunded() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver
        .on("1.0.0", "stop", vec![crate::testkit::Step::Lost]);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(kills(&rig.driver, "1.0.0"), 1, "it needed the kill");
    assert_eq!(recorded_attempts(&rig), Some(1), "the count stays");
}

/// R2(c): the worker closes the channel as it leaves and exits 0: refunded.
#[tokio::test]
async fn a_close_after_stop_then_exit_zero_is_refunded() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.on(
        "1.0.0",
        "stop",
        vec![
            crate::testkit::Step::Lost,
            crate::testkit::Step::Exit(true, "stopped"),
        ],
    );
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(kills(&rig.driver, "1.0.0"), 0);
    assert_eq!(recorded_attempts(&rig), Some(0), "the stopped try is back");
}

/// R2(d): a `load_failed` the Core already read, then Stop, then exit 0: not
/// refunded.
#[tokio::test]
async fn a_load_failed_read_before_the_stop_is_not_refunded() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("a worker");
    core.note_message(
        id,
        &crate::protocol::Message::LoadFailed {
            reason: "no".to_owned(),
        },
    );
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(kills(&rig.driver, "1.0.0"), 0);
    assert_eq!(recorded_attempts(&rig), Some(1), "the failure counts");
}

/// R2(d): the same, with a worker that then needs the deadline kill.
#[tokio::test]
async fn a_load_failed_read_before_the_stop_is_not_refunded_after_a_kill_either() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    rig.driver.on("1.0.0", "stop", Vec::new());
    let first = boot(&mut rig).await;
    tokio::time::pause();
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("a worker");
    core.note_message(
        id,
        &crate::protocol::Message::LoadFailed {
            reason: "no".to_owned(),
        },
    );
    assert_eq!(core.stop().await, crate::core::Exit::Stopped);
    drop(core);
    assert_eq!(kills(&rig.driver, "1.0.0"), 1, "the registry killed it");
    assert_eq!(recorded_attempts(&rig), Some(1), "the failure counts");
}

/// R5: start-up's deadline counts from the handshake, not from when the
/// Core is built after start-up's journal writes.
#[tokio::test]
async fn the_start_up_deadline_counts_from_the_handshake() {
    let (mut rig, _dirs) = real_rig().await;
    let _f = ladder_fixture(&rig, false);
    let first = boot(&mut rig).await;
    tokio::time::pause();
    let active_load = rig.cfg.timings.active_load;
    // Start-up's later work took nearly the whole wait.
    tokio::time::advance(active_load - Duration::from_secs(1)).await;
    seat_expecting(&mut rig, &first, false);
    drop(first);
    let mut core = core_of!(rig);
    let id = core.rt.active.expect("a worker");
    core.on_tick().await;
    assert!(!core.rt.workers[&id].load_timed_out, "not yet");
    tokio::time::advance(Duration::from_secs(2)).await;
    core.on_tick().await;
    tokio::time::timeout(Duration::from_secs(5), core.registry.next_event())
        .await
        .expect("the worker was killed and exited");
    assert_eq!(kills(&rig.driver, "1.0.0"), 1, "killed from the handshake");
}
