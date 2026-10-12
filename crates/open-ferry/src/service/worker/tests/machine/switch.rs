//! Not upstream's: the worker's switch messages (L9), one at a time, on the
//! scripted supervisor of the parent module. What needs the test seams'
//! barriers and faults is in `seams`, built with the `blue-green-test`
//! feature; what doesn't fails its steps some other way.

use open_ferry_core::gate::GateState;
use open_ferry_management::{Ban, Handover};
use tokio::sync::{mpsc, watch};

use super::*;
use crate::service::worker::switch as real;
use crate::service::worker::{Launch, ProcessLease, activate};

const STATE: &str = "0123456789abcdef0123456789abcdef";
const MANAGEMENT_CONFIG: &str = "/v0/management/config";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}

/// A hand over's state, named `id`, with one ban (a documentation address).
fn state(id: &str) -> Handover {
    Handover {
        id: id.to_owned(),
        at: now_ms(),
        bans: vec![Ban {
            ip: "192.0.2.7".into(),
            count: 3,
            blocked_for_ms: 60_000,
            idle_ms: 1_000,
        }],
    }
}

/// Puts `handover` where a take over reads it.
fn put_state(data: &std::path::Path, handover: &Handover) {
    std::fs::create_dir_all(data).unwrap();
    real::write_handover(data, handover).unwrap();
}

/// Whether `answer` is the management API's "updating" refusal.
fn is_updating(answer: &str) -> bool {
    answer.starts_with("HTTP/1.1 503") && answer.contains("updating, try again")
}

/// The answer to a `GET`, or `None` when none comes soon: a socket nobody
/// accepts on takes the connection and never answers.
async fn try_get(address: SocketAddr, path: &str) -> Option<String> {
    tokio::time::timeout(Duration::from_millis(300), async {
        let mut stream = TcpStream::connect(address).await.ok()?;
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
            )
            .await
            .ok()?;
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.ok()?;
        Some(answer)
    })
    .await
    .ok()
    .flatten()
}

/// Waits until `address` answers `/healthz`: accepting again.
async fn wait_serving(address: SocketAddr) {
    for _ in 0..200 {
        if try_get(address, "/healthz")
            .await
            .is_some_and(|answer| answer.starts_with("HTTP/1.1 200"))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{address} never answered /healthz");
}

/// What `logged` holds, at any level, that contains `needle`.
fn logged_count(logged: &Logged, needle: &str) -> usize {
    logged
        .at_least(Level::TRACE)
        .iter()
        .filter(|line| line.contains(needle))
        .count()
}

/// Waits until the worker has logged `needle`: how a test sees a step that
/// sends no message, such as a failed hand over.
async fn wait_logged(logged: &Logged, needle: &str) {
    for _ in 0..500 {
        if logged_count(logged, needle) > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the worker never logged {needle:?}");
}

impl Fake {
    /// Everything the worker sends until it closes the channel.
    async fn until_closed(&mut self) -> Vec<Message> {
        let mut all = Vec::new();
        while let Some(frame) = tokio::time::timeout(WAIT, self.channel.receive::<Message>())
            .await
            .expect("the exited worker kept the channel open")
            .unwrap()
        {
            all.push(frame.message);
        }
        all
    }

    /// Everything the worker has sent before it answers a message of a type
    /// it doesn't know: it answers in order, so this is what it sent for
    /// the messages before.
    async fn sync(&mut self) -> Vec<Message> {
        self.send_raw(serde_json::json!({"type": "sync_point"}))
            .await;
        let mut seen = Vec::new();
        loop {
            match self.recv().await {
                Message::Unsupported { message_type } if message_type == "sync_point" => {
                    return seen;
                }
                other => seen.push(other),
            }
        }
    }
}

/// A setup with a management listener of its own.
fn managed(role: Role) -> Setup {
    let mut setup = Setup::new(role);
    setup.management = true;
    setup
}

async fn active_rig(setup: Setup) -> Rig {
    let mut rig = setup.start();
    rig.fake().recv_active_start().await;
    rig
}

async fn standby_rig(setup: Setup) -> Rig {
    let mut rig = setup.start();
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    assert_eq!(rig.fake().recv_status().await.2, "standby");
    rig
}

/// What a take over that succeeds sends: `taking-over`, `ready`, `ready`.
async fn expect_ready(rig: &mut Rig) {
    assert_eq!(rig.fake().recv_status().await.2, "taking-over");
    let next = rig.fake().recv_other().await;
    assert!(matches!(next, Message::Ready), "{next:?}");
    assert_eq!(rig.fake().recv_status().await.2, "ready");
}

/// A take over with a valid `handover.json`, up to `ready`.
async fn take_over(rig: &mut Rig) {
    put_state(&rig.data, &state(STATE));
    rig.fake().send(Message::TakeOver).await;
    expect_ready(rig).await;
}

/// A hand over that succeeds: the id of the state it wrote.
async fn hand_over(rig: &mut Rig) -> String {
    rig.fake()
        .send(Message::HandOver { budget_ms: 5_000 })
        .await;
    match rig.fake().recv_other().await {
        Message::HandedOver { state_id, .. } => state_id,
        other => panic!("expected handed_over, got {other:?}"),
    }
}

/// A `peers_gone`, and the wait for the worker to have dealt with it.
async fn peers_gone(rig: &mut Rig) {
    rig.fake().send(Message::PeersGone).await;
    let seen = rig.fake().sync().await;
    assert!(seen.is_empty(), "peers_gone was answered: {seen:?}");
}

async fn stop(rig: &mut Rig) -> ExitCode {
    rig.fake().send(Message::Stop { budget_ms: 1_000 }).await;
    rig.exit().await
}

fn is_hex_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

// L9.3: the state is written whole, the lease goes, and `handed_over` names
// the state.
#[tokio::test]
async fn a_hand_over_writes_the_state_and_gives_the_lease_up() {
    let mut rig = active_rig(Setup::new(Role::Active)).await;
    assert!(rig.rotating(), "an active worker rotates its log");
    assert!(!rig.lease_is_free());
    let id = hand_over(&mut rig).await;
    assert!(is_hex_id(&id), "{id}");
    assert_eq!(rig.fake().recv_status().await.2, "handed-over");
    let written = std::fs::read(rig.path("handover.json")).unwrap();
    assert_eq!(Handover::from_json(&written).unwrap().id, id);
    let stray: Vec<String> = std::fs::read_dir(&rig.data)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(stray.is_empty(), "the write left {stray:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(rig.path("handover.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(rig.lease_is_free(), "a handed over worker holds no lease");
    assert!(!rig.rotating(), "rotation stops with the background work");
    assert!(!rig.cleaning());
    assert!(succeeded(stop(&mut rig).await));
}

// L9.4: a step that fails leaves the worker waiting for `resume`, serving
// as before, with the lease, and management still answering 503; `resume`
// ends the 503 before it says `resumed`. A hand over also makes the next
// `peers_gone` count again.
#[tokio::test]
async fn a_failed_hand_over_waits_for_resume_and_resume_restores_the_worker() {
    let (_guard, logged) = capture_here();
    let mut rig = active_rig(managed(Role::Active)).await;
    let management = rig.management.unwrap();
    // The write can't rename onto a directory that isn't empty.
    std::fs::create_dir_all(rig.path("handover.json").join("in-the-way")).unwrap();

    peers_gone(&mut rig).await;
    assert!(rig.cleaning());
    assert_eq!(logged_count(&logged, "no other worker is running"), 1);

    rig.fake()
        .send(Message::HandOver { budget_ms: 5_000 })
        .await;
    wait_logged(&logged, "the hand over failed").await;
    wait_serving(rig.main).await;
    let seen = rig.fake().sync().await;
    assert!(
        seen.iter()
            .all(|message| matches!(message, Message::Status { .. })),
        "a failed hand over sent {seen:?}"
    );
    assert_eq!(rig.fake().ping().await, "handing-over");
    assert!(!rig.lease_is_free(), "the failed hand over kept the lease");
    assert!(!rig.cleaning(), "a hand over turns cleaning off, first");
    assert!(
        rig.rotating(),
        "it failed before the background work stopped"
    );
    let answer = get(management, MANAGEMENT_CONFIG).await;
    assert!(is_updating(&answer), "{answer}");

    rig.fake().send(Message::Resume).await;
    let next = rig.fake().recv_other().await;
    assert!(matches!(next, Message::Resumed), "{next:?}");
    let answer = get(management, MANAGEMENT_CONFIG).await;
    assert!(!is_updating(&answer), "the 503 outlived resumed: {answer}");
    assert_eq!(rig.fake().recv_status().await.2, "active");
    assert!(rig.rotating() && !rig.cleaning() && !rig.lease_is_free());

    // The hand over reset `peers_gone`: the next one counts.
    peers_gone(&mut rig).await;
    assert!(rig.cleaning());
    assert_eq!(logged_count(&logged, "no other worker is running"), 2);

    assert!(succeeded(stop(&mut rig).await));
    let rest = rig.fake().until_closed().await;
    assert!(
        !rest
            .iter()
            .any(|message| matches!(message, Message::Loaded { .. })),
        "a resume sent a second loaded: {rest:?}"
    );
}

// L9.5, after `handed_over`: the lease again, within the budget, a reload,
// and the 503 ends last.
#[tokio::test]
async fn a_resume_after_handed_over_takes_the_lease_back() {
    let mut rig = active_rig(managed(Role::Active)).await;
    let management = rig.management.unwrap();
    hand_over(&mut rig).await;
    assert!(rig.lease_is_free());
    rig.fake().send(Message::Resume).await;
    let next = rig.fake().recv_other().await;
    assert!(matches!(next, Message::Resumed), "{next:?}");
    assert!(!rig.lease_is_free(), "resume retook the lease");
    let answer = get(management, MANAGEMENT_CONFIG).await;
    assert!(!is_updating(&answer), "the 503 outlived resumed: {answer}");
    let answer = get(rig.main, "/healthz").await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert_eq!(rig.fake().recv_status().await.2, "active");
    assert!(
        rig.rotating(),
        "resume reopened the log and turned rotation on"
    );
    assert!(!rig.cleaning(), "cleaning waits for peers_gone");
    assert!(succeeded(stop(&mut rig).await));
    let rest = rig.fake().until_closed().await;
    assert!(
        !rest
            .iter()
            .any(|message| matches!(message, Message::Loaded { .. })),
        "{rest:?}"
    );
}

// L9.5: the lease is tried for `resume_ms`, and a worker that can't have it
// stops with failure, with no `resumed`.
#[tokio::test]
async fn a_resume_that_cannot_retake_the_lease_stops_the_worker() {
    let mut setup = Setup::new(Role::Active);
    setup.resume_ms = 300;
    setup.stop_ms = 500;
    let mut rig = active_rig(setup).await;
    hand_over(&mut rig).await;
    let held = Lease::try_take(&rig.data).unwrap().unwrap();
    rig.fake().send(Message::Resume).await;
    assert_eq!(rig.exit().await, ExitCode::FAILURE);
    let rest = rig.fake().until_closed().await;
    assert!(
        !rest
            .iter()
            .any(|message| matches!(message, Message::Resumed)),
        "{rest:?}"
    );
    drop(held);
}

// L9.5: an active or watching worker with no hand over says `resumed` and
// changes nothing; messages that are not for its state do nothing.
#[tokio::test]
async fn a_resume_with_no_hand_over_changes_nothing() {
    let mut rig = active_rig(managed(Role::Active)).await;
    rig.fake().send(Message::Resume).await;
    let next = rig.fake().recv_other().await;
    assert!(matches!(next, Message::Resumed), "{next:?}");
    // `go` and `take_over` are not for an active worker.
    rig.fake().send(Message::Go).await;
    rig.fake().send(Message::TakeOver).await;
    let seen = rig.fake().sync().await;
    assert!(seen.is_empty(), "{seen:?}");
    assert_eq!(rig.fake().ping().await, "active");
    assert!(rig.rotating() && !rig.cleaning() && !rig.lease_is_free());
    assert!(!is_updating(
        &get(rig.management.unwrap(), MANAGEMENT_CONFIG).await
    ));
    assert!(succeeded(stop(&mut rig).await));
}

#[tokio::test]
async fn a_watching_worker_answers_resume_and_stays_watching() {
    let mut rig = managed_standby().await;
    take_over(&mut rig).await;
    rig.fake().send(Message::Go).await;
    assert_eq!(rig.fake().recv_status().await.2, "watching");
    rig.fake().send(Message::Resume).await;
    let next = rig.fake().recv_other().await;
    assert!(matches!(next, Message::Resumed), "{next:?}");
    assert_eq!(rig.fake().ping().await, "watching");
    assert!(rig.rotating());
    assert!(succeeded(stop(&mut rig).await));
}

async fn managed_standby() -> Rig {
    standby_rig(managed(Role::Standby)).await
}

// L9.5: a worker that began in taking-over or ready never resumes, and a
// standby has nothing to switch.
#[tokio::test]
async fn switch_messages_a_standby_does_not_take_are_ignored() {
    let mut rig = managed_standby().await;
    for message in [
        Message::Resume,
        Message::HandOver { budget_ms: 1_000 },
        Message::Go,
        Message::PeersGone,
        Message::Release,
    ] {
        rig.fake().send(message).await;
    }
    let seen = rig.fake().sync().await;
    assert!(seen.is_empty(), "{seen:?}");
    assert_eq!(rig.fake().ping().await, "standby");
    assert!(!rig.cleaning() && rig.lease_is_free());

    take_over(&mut rig).await;
    rig.fake().send(Message::Resume).await;
    let seen = rig.fake().sync().await;
    assert!(seen.is_empty(), "a ready worker answered resume: {seen:?}");
    assert_eq!(rig.fake().ping().await, "ready");
    assert!(succeeded(stop(&mut rig).await));
}

// L9.1, L9.2, L9.15: a take over serves at `ready`; `go` starts the
// background work and never log cleaning; `peers_gone` turns cleaning on
// once, in watching only.
#[tokio::test]
async fn a_take_over_serves_at_ready_and_go_starts_the_background() {
    let (_guard, logged) = capture_here();
    let mut rig = managed_standby().await;
    let management = rig.management.unwrap();
    take_over(&mut rig).await;
    assert!(!rig.lease_is_free(), "the take over holds the lease");
    assert!(
        !rig.path("handover.json").exists(),
        "the take over deletes the state it read"
    );
    let answer = get(rig.main, "/healthz").await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let answer = get(management, MANAGEMENT_CONFIG).await;
    assert!(!is_updating(&answer), "{answer}");
    assert!(!rig.rotating(), "rotation waits for go");

    // Not in ready.
    peers_gone(&mut rig).await;
    assert!(!rig.cleaning());
    assert_eq!(logged_count(&logged, "no other worker is running"), 0);

    rig.fake().send(Message::Go).await;
    assert_eq!(rig.fake().recv_status().await.2, "watching");
    assert!(rig.rotating(), "go turns rotation on");
    assert!(!rig.cleaning(), "go never turns cleaning on");
    // A second `go` is ignored.
    rig.fake().send(Message::Go).await;
    let seen = rig.fake().sync().await;
    assert!(seen.is_empty(), "{seen:?}");

    peers_gone(&mut rig).await;
    assert!(rig.cleaning());
    peers_gone(&mut rig).await;
    assert_eq!(logged_count(&logged, "no other worker is running"), 1);

    rig.fake().send(Message::Release).await;
    assert_eq!(rig.fake().recv_status().await.2, "draining");
    let next = rig.fake().recv_other().await;
    assert!(matches!(next, Message::Drained), "{next:?}");
    assert!(succeeded(rig.exit().await));
    let rest = rig.fake().until_closed().await;
    assert!(
        !rest
            .iter()
            .any(|message| matches!(message, Message::Loaded { .. })),
        "a take over sent a second loaded: {rest:?}"
    );
}

// L9.1: one try for the lease; a worker that fails is a standby, and can be
// asked again.
#[tokio::test]
async fn a_take_over_that_cannot_take_the_lease_fails_and_stays_a_standby() {
    let mut rig = standby_rig(Setup::new(Role::Standby)).await;
    put_state(&rig.data, &state(STATE));
    let held = Lease::try_take(&rig.data).unwrap().unwrap();
    rig.fake().send(Message::TakeOver).await;
    let Message::TakeFailed { reason } = rig.fake().recv_other().await else {
        panic!("expected take_failed");
    };
    assert!(reason.starts_with("lease: "), "{reason}");
    assert_eq!(rig.fake().recv_status().await.2, "standby");
    assert!(
        rig.path("handover.json").exists(),
        "a take over that never got the lease read the state"
    );
    drop(held);
    rig.fake().send(Message::TakeOver).await;
    expect_ready(&mut rig).await;
    assert!(succeeded(stop(&mut rig).await));
}

// L9.1: a state that is missing, doesn't parse or has no valid id fails the
// take over, with the lease kept and the file left; the worker then answers
// only `ping`, `status` and `stop`, and stops with failure.
#[tokio::test]
async fn a_take_over_with_a_bad_state_fails_with_the_lease_kept() {
    // Missing.
    let mut rig = standby_rig(Setup::new(Role::Standby)).await;
    rig.fake().send(Message::TakeOver).await;
    let Message::TakeFailed { reason } = rig.fake().recv_other().await else {
        panic!("expected take_failed");
    };
    assert!(reason.starts_with("handover: "), "{reason}");
    assert!(!rig.lease_is_free(), "a failed take over keeps the lease");
    assert_eq!(rig.fake().ping().await, "taking-over");
    rig.fake()
        .send(Message::HandOver { budget_ms: 1_000 })
        .await;
    rig.fake().send(Message::Go).await;
    let seen = rig.fake().sync().await;
    assert!(
        seen.iter()
            .all(|message| matches!(message, Message::Status { .. })),
        "a failed take over answered {seen:?}"
    );
    assert_eq!(stop(&mut rig).await, ExitCode::FAILURE);

    // Not JSON: left where it is.
    let mut rig = standby_rig(Setup::new(Role::Standby)).await;
    std::fs::create_dir_all(&rig.data).unwrap();
    std::fs::write(rig.path("handover.json"), "not json").unwrap();
    rig.fake().send(Message::TakeOver).await;
    let Message::TakeFailed { reason } = rig.fake().recv_other().await else {
        panic!("expected take_failed");
    };
    assert!(reason.starts_with("handover: "), "{reason}");
    assert!(rig.path("handover.json").exists());
    assert_eq!(stop(&mut rig).await, ExitCode::FAILURE);

    // An id that isn't 32 hexadecimal digits.
    let mut rig = standby_rig(Setup::new(Role::Standby)).await;
    put_state(&rig.data, &state("short"));
    rig.fake().send(Message::TakeOver).await;
    let Message::TakeFailed { reason } = rig.fake().recv_other().await else {
        panic!("expected take_failed");
    };
    assert!(reason.contains("no valid id"), "{reason}");
    assert!(rig.path("handover.json").exists());
    assert_eq!(stop(&mut rig).await, ExitCode::FAILURE);
}

// L9.1: the reload comes before the state is read, and a config that no
// longer loads fails the take over, with the state left.
#[tokio::test]
async fn a_take_over_with_a_config_that_no_longer_loads_fails() {
    let mut rig = standby_rig(Setup::new(Role::Standby)).await;
    put_state(&rig.data, &state(STATE));
    std::fs::write(rig._dir.path().join("config.yaml"), "port: [unclosed\n").unwrap();
    rig.fake().send(Message::TakeOver).await;
    let Message::TakeFailed { reason } = rig.fake().recv_other().await else {
        panic!("expected take_failed");
    };
    assert!(reason.starts_with("reload: "), "{reason}");
    assert!(rig.path("handover.json").exists());
    assert!(!rig.lease_is_free());
    assert_eq!(stop(&mut rig).await, ExitCode::FAILURE);
}

// L9.16: a worker that took over and is `ready` hands over like any other,
// and is never resumable.
#[tokio::test]
async fn a_ready_worker_can_hand_over_and_never_resumes() {
    let mut rig = managed_standby().await;
    take_over(&mut rig).await;
    let id = hand_over(&mut rig).await;
    assert!(is_hex_id(&id), "{id}");
    assert_ne!(id, STATE, "a hand over makes a state of its own");
    assert_eq!(rig.fake().recv_status().await.2, "handed-over");
    assert_eq!(
        Handover::from_json(&std::fs::read(rig.path("handover.json")).unwrap())
            .unwrap()
            .id,
        id
    );
    assert!(rig.lease_is_free());
    rig.fake().send(Message::Resume).await;
    let seen = rig.fake().sync().await;
    assert!(seen.is_empty(), "a ready worker resumed: {seen:?}");
    assert!(rig.lease_is_free(), "an ignored resume took the lease");
    rig.fake().send(Message::Release).await;
    assert_eq!(rig.fake().recv_status().await.2, "draining");
    let next = rig.fake().recv_other().await;
    assert!(matches!(next, Message::Drained), "{next:?}");
    assert!(succeeded(rig.exit().await));
}

// L9.15: `peers_gone` counts in active and watching only, and a hand over
// that is resumed makes the next one count again (also in the failed hand
// over test). Here: ignored while loading's neighbours, counted once.
#[tokio::test]
async fn peers_gone_in_an_active_worker_counts_once() {
    let (_guard, logged) = capture_here();
    let mut rig = active_rig(Setup::new(Role::Active)).await;
    assert!(
        !rig.cleaning(),
        "a supervised worker starts with cleaning off"
    );
    peers_gone(&mut rig).await;
    peers_gone(&mut rig).await;
    assert!(rig.cleaning());
    assert_eq!(logged_count(&logged, "no other worker is running"), 1);
    assert!(succeeded(stop(&mut rig).await));
}

// The state a hand over writes is the state a take over reads: the file
// round-trips, and the reader deletes it.
#[test]
fn the_handover_file_round_trips_and_the_reader_deletes_it() {
    let dir = tempfile::tempdir().unwrap();
    let written = state(STATE);
    real::write_handover(dir.path(), &written).unwrap();
    assert!(dir.path().join(real::HANDOVER_FILE).is_file());
    assert_eq!(real::read_handover(dir.path()).unwrap(), written);
    assert!(!dir.path().join(real::HANDOVER_FILE).exists());
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "the write left a temporary file"
    );
    let error = real::read_handover(dir.path()).unwrap_err();
    assert!(error.starts_with("handover: "), "{error}");
}

// The bans a hand over wrote come back with the time that has passed taken
// off: `restore_attempts`' elapsed durations, through the take over.
#[tokio::test]
async fn a_take_over_restores_the_bans_less_the_time_since_the_hand_over() {
    let (dir, standby) = super::prepared::prepared(true);
    let data = dir.path().join("data");
    let mut handover = state(STATE);
    handover.at = now_ms() - 10_000;
    put_state(&data, &handover);
    let management = standby.handles.management.clone();
    let (_cancel, cancelled) = watch::channel(false);
    let taken = real::take_over(real::TakeOver {
        standby,
        lease: lease_slot(Some(ProcessLease(Some(
            Lease::try_take(&data).unwrap().unwrap(),
        )))),
        cancel: cancelled,
        config_path: dir.path().join("config.yaml"),
        timing: Timing::default(),
    })
    .await;
    let real::Taken::Ready(active) = taken else {
        panic!("the take over did not end serving");
    };
    assert!(!data.join(real::HANDOVER_FILE).exists());
    let bans = management.freeze_attempts("after").bans;
    assert_eq!(bans.len(), 1, "{bans:?}");
    assert_eq!((bans[0].ip.as_str(), bans[0].count), ("192.0.2.7", 3));
    assert!(
        (30_000..=50_000).contains(&bans[0].blocked_for_ms),
        "the ban kept {} ms of 60000, after 10 s",
        bans[0].blocked_for_ms
    );
    active
        .stop(tokio::time::Instant::now() + Duration::from_secs(2))
        .await;
}

// T1, in process: a refresh the hand over is waiting for completes after a
// resume that kept the lease, and its rotated token and the credential's
// registration epoch are kept, with no reload. The refresh is held by a
// provider the test controls, not the core's once-per-process barrier.
mod held_refresh {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use open_ferry_core::auth::Auth;
    use open_ferry_core::exec::{ErrorKind, ExecError, Options, Request, Response, StreamResponse};
    use open_ferry_core::executor::ProviderExecutor;
    use tokio::sync::Notify;

    use super::*;

    type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

    /// A Codex executor whose refresh waits to be released, and rotates the
    /// credential's tokens.
    #[derive(Default)]
    struct Held {
        arrived: Notify,
        release: Notify,
        calls: AtomicUsize,
    }

    impl ProviderExecutor for Held {
        fn id(&self) -> &str {
            "codex"
        }

        fn execute(
            &self,
            _: Arc<Auth>,
            _: Request,
            _: Options,
        ) -> BoxFuture<'_, Result<Response, ExecError>> {
            Box::pin(async { Err(ExecError::new(ErrorKind::Upstream, "not served")) })
        }

        fn execute_stream(
            &self,
            _: Arc<Auth>,
            _: Request,
            _: Options,
        ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
            Box::pin(async { Err(ExecError::new(ErrorKind::Upstream, "not served")) })
        }

        fn count_tokens(
            &self,
            _: Arc<Auth>,
            _: Request,
            _: Options,
        ) -> BoxFuture<'_, Result<Response, ExecError>> {
            Box::pin(async { Err(ExecError::new(ErrorKind::Upstream, "not served")) })
        }

        fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.arrived.notify_one();
                self.release.notified().await;
                let mut auth = Auth::clone(&auth);
                auth.metadata
                    .insert("access_token".into(), "access-1".into());
                auth.metadata
                    .insert("refresh_token".into(), "refresh-1".into());
                Ok(auth)
            })
        }
    }

    fn saved(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_resume_that_keeps_the_lease_keeps_a_refresh_the_hand_over_waited_for() {
        let (dir, prepared) = super::super::prepared::prepared(false);
        let file = dir.path().join("auth").join("codex-a.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            r#"{"type":"codex","email":"a@example.com","access_token":"access-0","refresh_token":"refresh-0"}"#,
        )
        .unwrap();
        let held = Arc::new(Held::default());
        prepared.service.manager.register_executor(held.clone());
        let lease = lease_slot(Some(ProcessLease(Some(
            Lease::try_take(&dir.path().join("data")).unwrap().unwrap(),
        ))));
        let active = activate(
            prepared,
            &dir.path().join("config.yaml"),
            lease.clone(),
            &Timing::default(),
            Launch::First,
        )
        .await
        .unwrap();
        let handles = active.handles.clone();
        let id = handles.manager.list()[0].id.clone();
        // The watcher's first scan sends the file it read (access-0) as an
        // added file, and a file credential is never stale to it: applied
        // after the refresh published, it would put access-0 back. The
        // activation registered the credential at generation 1, and the scan's
        // update makes it 2, so wait for that before the refresh starts.
        let mut scanned = false;
        for _ in 0..500 {
            if handles.manager.get(&id).unwrap().generation >= 2 {
                scanned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(scanned, "the watcher's first scan was never applied");
        let epoch = handles.manager.get(&id).unwrap().registration_epoch;

        // A refresh is in flight, holding the gate open for a hand over.
        let refresh = tokio::spawn({
            let manager = handles.manager.clone();
            let id = id.clone();
            async move { manager.force_refresh(&id).await }
        });
        tokio::time::timeout(WAIT, held.arrived.notified())
            .await
            .expect("the refresh never reached the provider");

        // The hand over starts closing the gate and waits for the refresh.
        let cleaned = real::turn_cleaning_off(&handles);
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let hand = tokio::spawn(real::hand_over(real::HandOver {
            handles: handles.clone(),
            cleaned: cleaned.clone(),
            cancel: cancelled,
        }));
        for _ in 0..500 {
            if handles.gate.state() == GateState::Quiescing {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(handles.gate.state(), GateState::Quiescing);
        // The worker cancels it and `resume` joins it.
        cancel.send(true).unwrap();

        let (commands, mut reloads) = mpsc::unbounded_channel();
        tokio::time::timeout(
            WAIT,
            real::resume(real::Resume {
                handles: handles.clone(),
                lease: lease.clone(),
                commands,
                retake_within: Duration::from_secs(1),
                timing: Timing::default(),
                hand: Some(hand),
                cleaned,
            }),
        )
        .await
        .expect("a resume that kept the lease waited for a reload")
        .unwrap();
        assert!(
            reloads.try_recv().is_err(),
            "a resume that kept the lease asked for a reload"
        );
        assert_eq!(handles.gate.state(), GateState::Open);

        held.release.notify_one();
        tokio::time::timeout(WAIT, refresh)
            .await
            .expect("the refresh never finished")
            .unwrap()
            .expect("the refresh was refused");
        assert_eq!(held.calls.load(Ordering::SeqCst), 1);
        let auth = handles.manager.get(&id).unwrap();
        assert_eq!(auth.registration_epoch, epoch);
        assert_eq!(auth.metadata["access_token"], "access-1");
        assert_eq!(auth.metadata["refresh_token"], "refresh-1");
        let on_disk = saved(&file);
        assert_eq!(on_disk["access_token"], "access-1");
        assert_eq!(on_disk["refresh_token"], "refresh-1");

        active
            .stop(tokio::time::Instant::now() + Duration::from_secs(2))
            .await;
    }
}

/// A test's hold on one blocking step of the hand over or the take over, and
/// the means to see it arrive and to let it go. Dropping it lets it go.
struct Held {
    arrival: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
}

impl Held {
    /// Waits for the worker to be inside the step.
    async fn arrived(&self) {
        for _ in 0..500 {
            if self.arrival.try_recv().is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the worker never reached the held step");
    }

    fn release(&self) {
        let _ = self.release.send(());
    }
}

/// `setup`, holding the blocking step `step`. (`Hooks` has private fields
/// under the test feature, so no struct update here.)
#[allow(clippy::field_reassign_with_default)]
fn holding(mut setup: Setup, step: &'static str) -> (Setup, Held) {
    let (hold, arrival, release) = crate::service::worker::hooks::StepHold::new(step);
    let mut hooks = Hooks::default();
    hooks.step_hold = Some(hold);
    setup.timing.hooks = Some(hooks);
    (setup, Held { arrival, release })
}

/// Asserts the worker sends nothing but statuses for a while: a bounded
/// look at what must not happen, not an ordering.
async fn quiet(rig: &mut Rig) {
    let heard = tokio::time::timeout(Duration::from_millis(300), rig.fake().recv_other()).await;
    assert!(heard.is_err(), "the worker sent {heard:?}");
}

// The blocking steps of the switch that a cancel can't stop, held by the
// tests' own `StepHold`: they need no seam files.
mod stepped {
    use super::*;

    // A hand over arriving in a take over's step that cannot be cut short
    // is recorded and answered later: the loop goes on answering the
    // supervisor, and the hand over follows the step (L9.16).
    #[tokio::test]
    async fn a_hand_over_during_a_blocked_take_over_leaves_the_worker_answering() {
        let (setup, held) = holding(Setup::new(Role::Standby), "read_handover");
        let mut rig = standby_rig(setup).await;
        put_state(&rig.data, &state(STATE));
        rig.fake().send(Message::TakeOver).await;
        held.arrived().await;

        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        // The take over is still in its step, and the worker answers.
        assert_eq!(rig.fake().ping().await, "taking-over");
        let seen = rig.fake().sync().await;
        assert!(
            seen.iter()
                .all(|message| matches!(message, Message::Status { .. })),
            "{seen:?}"
        );
        // A second hand over is no news.
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        held.release();
        let id = match rig.fake().recv_other().await {
            Message::HandedOver { state_id, .. } => state_id,
            other => panic!("expected handed_over, got {other:?}"),
        };
        assert!(is_hex_id(&id), "{id}");
        assert!(rig.lease_is_free(), "the hand over gave the lease up");
        let seen = rig.fake().sync().await;
        assert!(
            seen.iter()
                .all(|message| matches!(message, Message::Status { .. })),
            "a worker that never served said {seen:?}"
        );
        assert!(succeeded(stop(&mut rig).await));
    }

    // L9.5: a resume waits for the mutating step the hand over has begun, and
    // does not run beside it; afterwards the worker is as it was.
    #[tokio::test]
    async fn a_resume_waits_for_the_blocking_step_the_hand_over_began() {
        let (setup, held) = holding(managed(Role::Active), "stop_background");
        let mut rig = active_rig(setup).await;
        let management = rig.management.unwrap();
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        held.arrived().await;
        assert!(rig.path("handover.json").exists());
        assert!(
            rig.rotating(),
            "the step was held before it stopped rotation"
        );

        rig.fake().send(Message::Resume).await;
        // The loop answers, and the resume has not ended.
        let seen = rig.fake().sync().await;
        assert!(
            !seen
                .iter()
                .any(|message| matches!(message, Message::Resumed)),
            "{seen:?}"
        );
        quiet(&mut rig).await;
        held.release();

        let next = rig.fake().recv_other().await;
        assert!(matches!(next, Message::Resumed), "{next:?}");
        assert_eq!(rig.fake().recv_status().await.2, "active");
        assert!(
            rig.rotating(),
            "the step stopped the background work after the resume had started it"
        );
        assert!(!rig.cleaning() && !rig.lease_is_free());
        let answer = get(management, MANAGEMENT_CONFIG).await;
        assert!(!is_updating(&answer), "{answer}");
        let answer = get(rig.main, "/healthz").await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert_eq!(rig.fake().ping().await, "active");
        assert!(succeeded(stop(&mut rig).await));
    }

    // A take over that fails on its own while a stop waits for it is a
    // failure, as it is for a worker the stop finds after its `take_failed`:
    // the worker exits with failure. (A take over the stop cancelled is not.)
    #[tokio::test]
    async fn a_stop_during_a_take_over_that_then_fails_exits_with_failure() {
        let (mut setup, held) = holding(Setup::new(Role::Standby), "read_handover");
        let took = Arc::new(tokio::sync::Notify::new());
        setup.timing.stop_took_the_take_over = Some(Arc::clone(&took));
        let mut rig = standby_rig(setup).await;
        std::fs::create_dir_all(&rig.data).unwrap();
        std::fs::write(rig.path("handover.json"), "not json").unwrap();
        rig.fake().send(Message::TakeOver).await;
        held.arrived().await;
        rig.fake().send(Message::Stop { budget_ms: 5_000 }).await;
        // The stop owns the shutdown, and waits for the take over's task,
        // before the held read is let go to fail.
        tokio::time::timeout(WAIT, took.notified()).await.unwrap();
        held.release();
        assert_eq!(rig.exit().await, ExitCode::FAILURE);
    }

    // A hand over cancelled during activation, between the watcher and the
    // gate, hands over without ever serving: no `ready`, nothing answered
    // on the main listener.
    #[tokio::test]
    async fn a_cancel_during_activation_hands_over_with_the_gate_never_opened() {
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        let (arrived, arrival) = tokio::sync::oneshot::channel();
        let arrived = Mutex::new(Some(arrived));
        let (mut setup, held) = holding(Setup::new(Role::Standby), "write_handover");
        setup.timing.wedge_watcher = Some(Arc::new(move |_, _| {
            if let Some(arrived) = arrived.lock().unwrap().take() {
                let _ = arrived.send(());
            }
            let _ = blocked.lock().unwrap().recv();
        }));
        let mut rig = standby_rig(setup).await;
        put_state(&rig.data, &state(STATE));
        rig.fake().send(Message::TakeOver).await;
        tokio::time::timeout(WAIT, arrival).await.unwrap().unwrap();

        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        assert_eq!(rig.fake().ping().await, "taking-over");
        release.send(()).unwrap();
        // The hand over is on its way to writing the state: the worker never
        // served, and nothing on the main listener answers.
        held.arrived().await;
        assert!(
            try_get(rig.main, "/healthz").await.is_none(),
            "the gate opened and accept resumed before the cancel"
        );
        held.release();
        let id = match rig.fake().recv_other().await {
            Message::HandedOver { state_id, .. } => state_id,
            other => panic!("expected handed_over, got {other:?}"),
        };
        assert!(is_hex_id(&id), "{id}");
        assert!(rig.lease_is_free());
        assert!(succeeded(stop(&mut rig).await));
    }
}

// The hand over's own steps, without a worker around it.
mod quiescing {
    use super::*;

    // A hand over that finds the gate closing, as a take over that failed
    // leaves it, settles it first: it goes on only once the operation that
    // holds the gate is done, and the lease is not its to give up.
    #[tokio::test]
    async fn a_hand_over_that_finds_the_gate_closing_waits_for_its_operations() {
        let (dir, prepared) = super::super::prepared::prepared(false);
        let data = dir.path().join("data");
        let lease = lease_slot(Some(ProcessLease(Some(
            Lease::try_take(&data).unwrap().unwrap(),
        ))));
        let active = activate(
            prepared,
            &dir.path().join("config.yaml"),
            lease.clone(),
            &Timing::default(),
            Launch::First,
        )
        .await
        .unwrap();
        let handles = active.handles.clone();
        let operation = handles.gate.begin_operation().unwrap();
        handles.gate.quiesce(handles.gate.epoch()).unwrap();
        assert_eq!(handles.gate.state(), GateState::Quiescing);

        let cleaned = real::turn_cleaning_off(&handles);
        let (_cancel, cancelled) = watch::channel(false);
        let mut hand = tokio::spawn(real::hand_over(real::HandOver {
            handles: handles.clone(),
            cleaned,
            cancel: cancelled,
        }));
        let early = tokio::time::timeout(Duration::from_millis(300), &mut hand).await;
        assert!(
            early.is_err(),
            "the hand over went on while an operation held the gate: {early:?}"
        );
        assert!(!data.join("handover.json").exists());
        assert_eq!(handles.gate.state(), GateState::Quiescing);

        drop(operation);
        let id = tokio::time::timeout(WAIT, hand)
            .await
            .expect("the hand over never settled the gate")
            .unwrap()
            .expect("the hand over failed");
        assert!(is_hex_id(&id), "{id}");
        assert_eq!(handles.gate.state(), GateState::Closed);
        assert!(data.join("handover.json").exists());
        assert!(
            real::lease_held(&lease),
            "the continuation gave the lease up"
        );
        active
            .stop(tokio::time::Instant::now() + Duration::from_secs(2))
            .await;
    }
}

// The hand over's end and the resume's steps, without a worker around them:
// the test is the service loop, and answers the resume's commands itself.
mod ordering {
    use futures_util::FutureExt;

    use super::*;

    type Resuming = std::pin::Pin<Box<dyn Future<Output = Result<(), String>>>>;

    /// A service the hand over has handed over, the lease still in its slot.
    struct Handed {
        _dir: tempfile::TempDir,
        active: crate::service::worker::Active,
        lease: real::LeaseSlot,
        data: std::path::PathBuf,
        cleaned: watch::Receiver<bool>,
    }

    async fn handed() -> Handed {
        let (dir, prepared) = super::super::prepared::prepared(false);
        let data = dir.path().join("data");
        let lease = lease_slot(Some(ProcessLease(Some(
            Lease::try_take(&data).unwrap().unwrap(),
        ))));
        let active = activate(
            prepared,
            &dir.path().join("config.yaml"),
            lease.clone(),
            &Timing::default(),
            Launch::First,
        )
        .await
        .unwrap();
        let handles = active.handles.clone();
        assert_eq!(handles.gate.state(), GateState::Open);
        let cleaned = real::turn_cleaning_off(&handles);
        let (_cancel, cancelled) = watch::channel(false);
        let id = real::hand_over(real::HandOver {
            handles: handles.clone(),
            cleaned: cleaned.clone(),
            cancel: cancelled,
        })
        .await
        .expect("the hand over failed");
        assert!(is_hex_id(&id), "{id}");
        Handed {
            _dir: dir,
            active,
            lease,
            data,
            cleaned,
        }
    }

    impl Handed {
        /// A resume after the driver gave the lease up, with a command
        /// receiver of the test's own.
        fn resume(&self) -> (Resuming, mpsc::UnboundedReceiver<real::Command>) {
            let (commands, received) = mpsc::unbounded_channel();
            let job = real::Resume {
                handles: self.active.handles.clone(),
                lease: self.lease.clone(),
                commands,
                retake_within: WAIT,
                timing: Timing::default(),
                hand: None,
                cleaned: self.cleaned.clone(),
            };
            (Box::pin(real::resume(job)), received)
        }

        async fn stop(self) {
            self.active
                .stop(tokio::time::Instant::now() + Duration::from_secs(2))
                .await;
        }
    }

    /// Drives `resume` until it asks for `ApplyDeferred`, answering the
    /// reload: the ack of the apply is returned.
    async fn until_apply(
        resume: &mut Resuming,
        received: &mut mpsc::UnboundedReceiver<real::Command>,
    ) -> tokio::sync::oneshot::Sender<Result<(), String>> {
        loop {
            tokio::select! {
                result = &mut *resume => panic!("the resume ended before it asked to apply the config: {result:?}"),
                command = received.recv() => match command.expect("the resume dropped its commands") {
                    real::Command::Reload(done) => done.send(()).unwrap(),
                    real::Command::ApplyDeferred(done) => return done,
                },
            }
        }
    }

    // L9.3: a hand over that ends well leaves the lease to the driver, which
    // gives it up when it takes the end of the hand over (L9.17). The
    // driver's release is `a_hand_over_writes_the_state_and_gives_the_lease_up`.
    #[tokio::test]
    async fn a_hand_over_that_ends_well_leaves_the_lease_in_its_slot() {
        let handed = handed().await;
        assert!(
            real::lease_held(&handed.lease),
            "the continuation gave the lease up"
        );
        assert!(handed.data.join("handover.json").exists());
        handed.stop().await;
    }

    // L9.5: `resumed` waits for the service loop to have applied the config.
    // The loop's own wait for the gate applies a deferred config too, so only
    // a resume that holds back until the ack is shown by a loop that answers
    // late: the resume stays pending, accept paused and the 503 kept.
    #[tokio::test]
    async fn a_resume_waits_for_the_ack_of_the_apply_before_it_resumes() {
        let handed = handed().await;
        let handles = handed.active.handles.clone();
        assert!(handles.management.is_updating());
        assert!(handles.control.accepting_is_paused());
        real::release_lease(&handed.lease);
        let (mut resume, mut received) = handed.resume();

        let ack = until_apply(&mut resume, &mut received).await;
        // The resume asked, and is parked on the answer.
        assert!(resume.as_mut().now_or_never().is_none());
        assert_eq!(handles.gate.state(), GateState::Open);
        let early = tokio::time::timeout(Duration::from_millis(500), &mut resume).await;
        assert!(
            early.is_err(),
            "the resume went on without the ack: {early:?}"
        );
        assert!(handles.control.accepting_is_paused());
        assert!(handles.management.is_updating());

        ack.send(Ok(())).unwrap();
        tokio::time::timeout(WAIT, resume)
            .await
            .expect("the resume never finished")
            .expect("the resume failed");
        assert!(!handles.control.accepting_is_paused());
        assert!(!handles.management.is_updating());
        handed.stop().await;
    }

    // L9.5: an `Err` for the apply, as when the config names an auth
    // directory that couldn't be taken into use, ends the resume with that
    // reason, and nothing it would have done after has been done.
    #[tokio::test]
    async fn a_resume_whose_apply_fails_resumes_nothing() {
        let handed = handed().await;
        let handles = handed.active.handles.clone();
        real::release_lease(&handed.lease);
        let (mut resume, mut received) = handed.resume();

        let ack = until_apply(&mut resume, &mut received).await;
        ack.send(Err("no such directory".to_owned())).unwrap();
        let result = tokio::time::timeout(WAIT, resume)
            .await
            .expect("the resume never finished");
        let reason = result.expect_err("the resume ignored the failed apply");
        assert!(reason.contains("no such directory"), "{reason}");
        assert!(handles.control.accepting_is_paused());
        assert!(handles.management.is_updating());
        handed.stop().await;
    }
}

// The cross-process test seams of L7, on the machine: the barriers, the
// faults, `events.log` and the marker header. A worker built with the
// feature reads them from the data directory it was welcomed with.
#[cfg(feature = "blue-green-test")]
mod seams {
    use super::*;

    const GATE_CLOSING: &str = "handover.gate_closing";
    const REFRESH: &str = "refresh.before_save";
    const GATE_CLOSED: &str = "handover.gate_closed";
    const LEASE_RELEASE: &str = "lease.release";
    const BEFORE_RELOAD: &str = "take_over.before_reload";

    /// `setup`, with its seams marked `marker` and `faults` written.
    fn marked(
        mut setup: Setup,
        marker: &'static str,
        faults: &[(&'static str, &'static str)],
    ) -> Setup {
        setup.marker = Some(marker);
        setup.faults = faults.to_vec();
        setup
    }

    /// The barrier files of one worker. What it holds is released when it
    /// is dropped, however the test ends: a runtime waits for the blocking
    /// poll of a barrier that is held.
    struct Barriers {
        dir: PathBuf,
        held: Vec<&'static str>,
    }

    impl Barriers {
        fn of(rig: &Rig) -> Self {
            Self {
                dir: rig.data.join(".blue-green-test"),
                held: Vec::new(),
            }
        }

        fn touch(&self, file: String) {
            std::fs::write(self.dir.join(file), "").unwrap();
        }

        fn hold(&mut self, name: &'static str) {
            self.touch(format!("{name}.hold"));
            self.held.push(name);
        }

        fn release(&self, name: &str) {
            self.touch(format!("{name}.release"));
        }

        fn fail(&self, name: &str) {
            self.touch(format!("{name}.fail"));
        }

        fn has(&self, file: &str) -> bool {
            self.dir.join(file).exists()
        }

        /// Waits for the worker to be at `name`.
        async fn arrived(&self, name: &str) {
            for _ in 0..500 {
                if self.has(&format!("{name}.arrived")) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("the worker never arrived at {name}");
        }
    }

    impl Drop for Barriers {
        fn drop(&mut self) {
            for name in &self.held {
                let _ = std::fs::write(self.dir.join(format!("{name}.release")), "");
            }
        }
    }

    /// The lines of `events.log`: an event and whether the lease was held.
    fn events(rig: &Rig) -> Vec<(String, bool)> {
        std::fs::read_to_string(rig.path(".blue-green-test").join("events.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let line: serde_json::Value = serde_json::from_str(line).unwrap();
                (
                    line["event"].as_str().unwrap().to_owned(),
                    line["lease"].as_bool().unwrap(),
                )
            })
            .collect()
    }

    /// The events among `wanted`, in the order they were logged.
    fn events_of(rig: &Rig, wanted: &[&str]) -> Vec<(String, bool)> {
        events(rig)
            .into_iter()
            .filter(|(name, _)| wanted.contains(&name.as_str()))
            .collect()
    }

    fn event(name: &str, lease: bool) -> (String, bool) {
        (name.to_owned(), lease)
    }

    async fn wait_event(rig: &Rig, name: &str) {
        for _ in 0..500 {
            if events(rig).iter().any(|(seen, _)| seen == name) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the worker never logged the event {name}");
    }

    // L9.3: the gate closes in two steps, then the state is written, then
    // the lease goes, each step after its barrier, and management answers
    // 503 from the first.
    #[tokio::test]
    async fn a_hand_over_goes_through_its_barriers_in_order() {
        let mut rig = active_rig(marked(managed(Role::Active), "a", &[])).await;
        let management = rig.management.unwrap();
        let mut barriers = Barriers::of(&rig);
        for name in [GATE_CLOSING, GATE_CLOSED, LEASE_RELEASE] {
            barriers.hold(name);
        }
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;

        barriers.arrived(GATE_CLOSING).await;
        let answer = get(management, MANAGEMENT_CONFIG).await;
        assert!(is_updating(&answer), "{answer}");
        assert!(!rig.path("handover.json").exists());
        barriers.release(GATE_CLOSING);

        barriers.arrived(GATE_CLOSED).await;
        assert!(
            !rig.path("handover.json").exists(),
            "the state was written before the gate closed"
        );
        assert!(
            !barriers.has(&format!("{REFRESH}.arrived")),
            "the hand over waited for a refresh with no fault to say so"
        );
        barriers.release(GATE_CLOSED);

        barriers.arrived(LEASE_RELEASE).await;
        assert!(rig.path("handover.json").exists());
        assert!(!rig.lease_is_free(), "the lease went before its barrier");
        barriers.release(LEASE_RELEASE);

        let id = match rig.fake().recv_other().await {
            Message::HandedOver { state_id, .. } => state_id,
            other => panic!("expected handed_over, got {other:?}"),
        };
        assert!(is_hex_id(&id));
        assert!(rig.lease_is_free());
        rig.fake().sync().await;
        assert_eq!(
            events_of(
                &rig,
                &[
                    GATE_CLOSING,
                    GATE_CLOSED,
                    "handover.written",
                    LEASE_RELEASE,
                    "handed_over"
                ]
            ),
            vec![
                event(GATE_CLOSING, true),
                event(GATE_CLOSED, true),
                event("handover.written", true),
                event(LEASE_RELEASE, true),
                event("handed_over", false),
            ]
        );
        let log = std::fs::read_to_string(rig.path(".blue-green-test").join("events.log")).unwrap();
        let first: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(first["marker"], "a");
        assert_eq!(first["pid"], std::process::id());
        assert!(succeeded(stop(&mut rig).await));
    }

    // L9.5: a resume never waits for a barrier the hand over holds, and with
    // the lease still held it keeps it.
    #[tokio::test]
    async fn a_resume_does_not_wait_for_a_held_barrier() {
        let mut rig = active_rig(marked(managed(Role::Active), "a", &[])).await;
        let management = rig.management.unwrap();
        let mut barriers = Barriers::of(&rig);
        barriers.hold(LEASE_RELEASE);
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        barriers.arrived(LEASE_RELEASE).await;
        assert!(rig.path("handover.json").exists());

        rig.fake().send(Message::Resume).await;
        let next = rig.fake().recv_other().await;
        assert!(matches!(next, Message::Resumed), "{next:?}");
        assert!(!rig.lease_is_free(), "the lease was kept");
        let answer = get(management, MANAGEMENT_CONFIG).await;
        assert!(!is_updating(&answer), "{answer}");
        assert!(rig.rotating());
        rig.fake().sync().await;
        assert_eq!(
            events_of(&rig, &["handover.written", "handed_over", "resumed"]),
            vec![event("handover.written", true), event("resumed", true)]
        );
        barriers.release(LEASE_RELEASE);
        assert!(succeeded(stop(&mut rig).await));
    }

    // L7: `hold-refresh` makes the hand over wait at `refresh.before_save`,
    // between the gate's two steps; a fault for another marker does not.
    #[tokio::test]
    async fn hold_refresh_holds_the_hand_over_before_the_gate_closes() {
        let mut rig = active_rig(marked(
            Setup::new(Role::Active),
            "a",
            &[("hold-refresh", "on")],
        ))
        .await;
        let mut barriers = Barriers::of(&rig);
        barriers.hold(REFRESH);
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        barriers.arrived(REFRESH).await;
        let logged = events(&rig);
        assert!(logged.iter().any(|(name, _)| name == GATE_CLOSING));
        assert!(!logged.iter().any(|(name, _)| name == GATE_CLOSED));
        assert!(!rig.path("handover.json").exists());
        barriers.release(REFRESH);
        let next = rig.fake().recv_other().await;
        assert!(matches!(next, Message::HandedOver { .. }), "{next:?}");
        assert!(succeeded(stop(&mut rig).await));

        let mut rig = active_rig(marked(
            Setup::new(Role::Active),
            "a",
            &[("hold-refresh", "on@b")],
        ))
        .await;
        let mut barriers = Barriers::of(&rig);
        barriers.hold(REFRESH);
        hand_over(&mut rig).await;
        assert!(
            !barriers.has(&format!("{REFRESH}.arrived")),
            "a fault for marker b held marker a"
        );
        barriers.release(REFRESH);
        assert!(succeeded(stop(&mut rig).await));
    }

    // L7: `ignore-handover` drops the message: no 503, no state, no answer.
    #[tokio::test]
    async fn ignore_handover_ignores_the_message() {
        let mut rig = active_rig(marked(
            managed(Role::Active),
            "a",
            &[("ignore-handover", "on")],
        ))
        .await;
        rig.fake()
            .send(Message::HandOver { budget_ms: 1_000 })
            .await;
        let seen = rig.fake().sync().await;
        assert!(seen.is_empty(), "{seen:?}");
        assert_eq!(rig.fake().ping().await, "active");
        assert!(!rig.path("handover.json").exists());
        assert!(!rig.lease_is_free());
        assert!(!is_updating(
            &get(rig.management.unwrap(), MANAGEMENT_CONFIG).await
        ));
        assert!(succeeded(stop(&mut rig).await));
    }

    /// A hand over whose `barrier` fails: it stops there, says nothing, and
    /// waits for `resume`, which puts the worker back.
    async fn fails_at(
        barrier: &'static str,
        faults: &[(&'static str, &'static str)],
        written: bool,
    ) {
        let mut rig = active_rig(marked(managed(Role::Active), "a", faults)).await;
        let management = rig.management.unwrap();
        let barriers = Barriers::of(&rig);
        barriers.fail(barrier);
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        wait_event(&rig, "handover_failed").await;
        wait_serving(rig.main).await;
        assert!(
            barriers.has(&format!("{barrier}.failed")),
            "{barrier} left no receipt"
        );
        let seen = rig.fake().sync().await;
        assert!(
            seen.iter()
                .all(|message| matches!(message, Message::Status { .. })),
            "{barrier}: a failed hand over sent {seen:?}"
        );
        let mut expected = Vec::new();
        if written {
            expected.push(event("handover.written", true));
        }
        expected.push(event("handover_failed", true));
        assert_eq!(
            events_of(
                &rig,
                &["handover.written", "handed_over", "handover_failed"]
            ),
            expected,
            "{barrier}"
        );
        assert_eq!(rig.fake().ping().await, "handing-over");
        assert!(!rig.lease_is_free(), "{barrier}: the lease was given up");
        // Past the write the background work had stopped; before it, not.
        assert_eq!(rig.rotating(), !written, "{barrier}");
        let answer = get(management, MANAGEMENT_CONFIG).await;
        assert!(is_updating(&answer), "{barrier}: {answer}");

        rig.fake().send(Message::Resume).await;
        let next = rig.fake().recv_other().await;
        assert!(matches!(next, Message::Resumed), "{barrier}: {next:?}");
        let answer = get(management, MANAGEMENT_CONFIG).await;
        assert!(!is_updating(&answer), "{barrier}: {answer}");
        assert_eq!(rig.fake().recv_status().await.2, "active");
        assert!(rig.rotating() && !rig.cleaning() && !rig.lease_is_free());
        rig.fake().sync().await;
        assert_eq!(
            events(&rig).last().cloned(),
            Some(event("resumed", true)),
            "{barrier}"
        );
        assert!(succeeded(stop(&mut rig).await));
    }

    // L9.4: a failure at any step of the hand over, each one in turn.
    #[tokio::test]
    async fn a_hand_over_that_fails_at_a_step_waits_for_resume() {
        fails_at(GATE_CLOSING, &[], false).await;
        fails_at(REFRESH, &[("hold-refresh", "on")], false).await;
        fails_at(GATE_CLOSED, &[], false).await;
        fails_at(LEASE_RELEASE, &[], true).await;
    }

    // L7: `fail-before-ready` fails the take over after the state was read,
    // with the lease kept.
    #[tokio::test]
    async fn fail_before_ready_fails_the_take_over_with_the_lease_kept() {
        let mut rig = standby_rig(marked(
            Setup::new(Role::Standby),
            "b",
            &[("fail-before-ready", "on")],
        ))
        .await;
        put_state(&rig.data, &state(STATE));
        rig.fake().send(Message::TakeOver).await;
        let Message::TakeFailed { reason } = rig.fake().recv_other().await else {
            panic!("expected take_failed");
        };
        assert_eq!(reason, "fail-before-ready");
        assert!(
            !rig.path("handover.json").exists(),
            "the state was read first"
        );
        assert!(!rig.lease_is_free());
        assert_eq!(rig.fake().ping().await, "taking-over");
        assert_eq!(stop(&mut rig).await, ExitCode::FAILURE);
    }

    // L7: `slow-ready` delays `ready` by its milliseconds.
    #[tokio::test]
    async fn slow_ready_delays_ready() {
        let mut rig = standby_rig(marked(
            Setup::new(Role::Standby),
            "b",
            &[("slow-ready", "400")],
        ))
        .await;
        let began = Instant::now();
        take_over(&mut rig).await;
        assert!(
            began.elapsed() >= Duration::from_millis(350),
            "ready came after {:?}",
            began.elapsed()
        );
        assert!(succeeded(stop(&mut rig).await));
    }

    // L7: a failed `take_over.before_reload` fails the take over before the
    // reload and before the state is read.
    #[tokio::test]
    async fn a_failed_barrier_before_the_reload_fails_the_take_over() {
        let mut rig = standby_rig(marked(Setup::new(Role::Standby), "b", &[])).await;
        put_state(&rig.data, &state(STATE));
        Barriers::of(&rig).fail(BEFORE_RELOAD);
        rig.fake().send(Message::TakeOver).await;
        let Message::TakeFailed { reason } = rig.fake().recv_other().await else {
            panic!("expected take_failed");
        };
        assert!(reason.contains("a test barrier failed"), "{reason}");
        assert!(rig.path("handover.json").exists());
        assert!(!rig.lease_is_free());
        assert_eq!(stop(&mut rig).await, ExitCode::FAILURE);
    }

    // L7: a fault with `@marker` is for the worker so marked, no other.
    #[tokio::test]
    async fn a_fault_for_another_marker_does_not_apply() {
        let mut rig = standby_rig(marked(
            Setup::new(Role::Standby),
            "b",
            &[("fail-before-ready", "on@a")],
        ))
        .await;
        take_over(&mut rig).await;
        assert!(succeeded(stop(&mut rig).await));
    }

    // L9.16: a hand over cancels a take over that has not reached its last
    // step and answers `handed_over`, not `take_failed`; the worker never
    // served, so `release` finishes it.
    #[tokio::test]
    async fn a_hand_over_cancels_a_take_over_and_hands_over() {
        let mut rig = standby_rig(marked(Setup::new(Role::Standby), "b", &[])).await;
        put_state(&rig.data, &state(STATE));
        let mut barriers = Barriers::of(&rig);
        barriers.hold(BEFORE_RELOAD);
        rig.fake().send(Message::TakeOver).await;
        barriers.arrived(BEFORE_RELOAD).await;

        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        let id = match rig.fake().recv_other().await {
            Message::HandedOver { state_id, .. } => state_id,
            other => panic!("expected handed_over, got {other:?}"),
        };
        assert!(is_hex_id(&id), "{id}");
        assert!(rig.lease_is_free(), "the hand over gave the lease up");
        assert_eq!(
            Handover::from_json(&std::fs::read(rig.path("handover.json")).unwrap())
                .unwrap()
                .id,
            id,
            "the worker handed over the state the other worker left"
        );
        rig.fake().send(Message::Resume).await;
        rig.fake().send(Message::Release).await;
        let next = rig.fake().recv_other().await;
        assert!(matches!(next, Message::Drained), "{next:?}");
        assert!(succeeded(rig.exit().await));
        barriers.release(BEFORE_RELOAD);
    }

    // L7: `fail-load` fails a load, standby or active, with its name.
    #[tokio::test]
    async fn fail_load_fails_the_load_of_the_worker_it_names() {
        for role in [Role::Standby, Role::Active] {
            let mut rig = marked(Setup::new(role), "a", &[("fail-load", "on")]).start();
            let Message::LoadFailed { reason } = rig.fake().recv().await else {
                panic!("expected load_failed");
            };
            assert_eq!(reason, "fail-load");
            assert_eq!(rig.exit().await, ExitCode::FAILURE);
        }
        let mut rig = standby_rig(marked(
            Setup::new(Role::Standby),
            "a",
            &[("fail-load", "on@b")],
        ))
        .await;
        assert!(succeeded(stop(&mut rig).await));
    }

    /// The points `exit-after` would have exited at, in a scenario that
    /// reaches `point` and the ones before it.
    async fn exits_at(point: &str, fault: &'static str) -> Vec<&'static str> {
        let active = point == "handed_over";
        let setup = marked(
            Setup::new(if active { Role::Active } else { Role::Standby }),
            "a",
            &[("exit-after", fault)],
        );
        let mut rig = if active {
            active_rig(setup).await
        } else {
            standby_rig(setup).await
        };
        match point {
            "handed_over" => {
                hand_over(&mut rig).await;
            }
            "ready" => take_over(&mut rig).await,
            "go" => {
                take_over(&mut rig).await;
                rig.fake().send(Message::Go).await;
            }
            _ => {}
        }
        // The worker is done with what it was sent when it answers this.
        rig.fake().sync().await;
        let exits = rig.hooks.exits();
        let _ = stop(&mut rig).await;
        exits
    }

    // L7: `exit-after` exits at the point it names, and at no other.
    #[tokio::test]
    async fn exit_after_exits_at_each_point_it_names() {
        for point in ["welcome", "loaded", "handed_over", "ready", "go"] {
            let exits = exits_at(point, point).await;
            assert_eq!(exits, vec![point], "{point}");
        }
        assert!(exits_at("ready", "ready@b").await.is_empty());
        assert_eq!(exits_at("ready", "ready@a").await, vec!["ready"]);
        assert_eq!(exits_at("go", "ready").await, vec!["ready"]);
    }

    // L7, L9.10: `drop-channel` drops the channel, stops accepting, keeps
    // the lease and stays alive until it is killed.
    async fn dropped(rig: &mut Rig) {
        let seen = rig.fake().until_closed().await;
        assert!(
            seen.iter()
                .all(|message| matches!(message, Message::Status { .. })),
            "{seen:?}"
        );
        assert!(!rig.lease_is_free(), "the lease was given up");
        assert!(
            try_get(rig.main, "/healthz").await.is_none(),
            "the worker kept accepting"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !rig.worker.as_ref().unwrap().is_finished(),
            "the worker exited instead of staying until killed"
        );
        rig.worker.take().unwrap().abort();
    }

    #[tokio::test]
    async fn drop_channel_at_taking_over_keeps_the_lease() {
        let mut rig = standby_rig(marked(
            Setup::new(Role::Standby),
            "b",
            &[("drop-channel", "taking-over")],
        ))
        .await;
        put_state(&rig.data, &state(STATE));
        rig.fake().send(Message::TakeOver).await;
        dropped(&mut rig).await;
        assert!(
            !rig.path("handover.json").exists(),
            "the channel was dropped after the state was read"
        );
    }

    #[tokio::test]
    async fn drop_channel_at_watching_keeps_the_lease() {
        let mut rig = standby_rig(marked(
            Setup::new(Role::Standby),
            "b",
            &[("drop-channel", "watching")],
        ))
        .await;
        take_over(&mut rig).await;
        rig.fake().send(Message::Go).await;
        dropped(&mut rig).await;
        assert!(rig.rotating(), "go started the background work first");
    }

    #[tokio::test]
    async fn drop_channel_at_handing_over_keeps_the_lease() {
        let mut rig = active_rig(marked(
            Setup::new(Role::Active),
            "a",
            &[("drop-channel", "handing-over")],
        ))
        .await;
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        dropped(&mut rig).await;
        assert!(
            !rig.path("handover.json").exists(),
            "the hand over ran after the channel was dropped"
        );
    }

    // L7: every HTTP response of both listeners, the management API's 503
    // too, names the worker; a worker without seams names nothing.
    #[tokio::test]
    async fn every_response_carries_the_marker_header() {
        let mut rig = active_rig(marked(managed(Role::Active), "a", &[])).await;
        let management = rig.management.unwrap();
        let header = "x-open-ferry-test-marker: a";
        let answer = get(rig.main, "/healthz").await.to_lowercase();
        assert!(answer.contains(header), "{answer}");
        let answer = get(management, "/healthz").await.to_lowercase();
        assert!(answer.contains(header), "{answer}");
        std::fs::create_dir_all(rig.path("handover.json").join("in-the-way")).unwrap();
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        wait_event(&rig, "handover_failed").await;
        wait_serving(rig.main).await;
        let answer = get(management, MANAGEMENT_CONFIG).await;
        assert!(is_updating(&answer), "{answer}");
        assert!(answer.to_lowercase().contains(header), "{answer}");
        assert!(succeeded(stop(&mut rig).await));

        let mut rig = active_rig(Setup::new(Role::Active)).await;
        let answer = get(rig.main, "/healthz").await.to_lowercase();
        assert!(!answer.contains("x-open-ferry-test-marker"), "{answer}");
        assert!(succeeded(stop(&mut rig).await));
    }

    // L9.12: the events of a standby that takes over, goes and is released.
    #[tokio::test]
    async fn the_event_log_follows_a_take_over_and_a_release() {
        let mut rig = standby_rig(marked(Setup::new(Role::Standby), "b", &[])).await;
        take_over(&mut rig).await;
        rig.fake().send(Message::Go).await;
        assert_eq!(rig.fake().recv_status().await.2, "watching");
        peers_gone(&mut rig).await;
        rig.fake().send(Message::Release).await;
        let next = rig.fake().recv_other().await;
        assert!(matches!(next, Message::Drained), "{next:?}");
        assert!(succeeded(rig.exit().await));
        assert_eq!(
            events_of(
                &rig,
                &[
                    "loaded",
                    "standby",
                    "ready",
                    "go",
                    "peers_gone",
                    "draining",
                    "drained"
                ]
            ),
            vec![
                event("loaded", false),
                event("standby", false),
                event("ready", true),
                event("go", true),
                event("peers_gone", true),
                event("draining", true),
                event("drained", true),
            ]
        );
    }

    // L9.16: a take over that fails or is dropped is not cancelled by a hand
    // over that waits for it, and never hands over: the worker keeps the
    // lease, says `take_failed`, and says no `handed_over`.
    #[tokio::test]
    async fn a_take_over_that_fails_with_a_hand_over_waiting_never_hands_over() {
        let (setup, held) = holding(
            marked(
                Setup::new(Role::Standby),
                "b",
                &[("fail-before-ready", "on")],
            ),
            "read_handover",
        );
        let mut rig = standby_rig(setup).await;
        put_state(&rig.data, &state(STATE));
        rig.fake().send(Message::TakeOver).await;
        held.arrived().await;
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        // The hand over is recorded before the take over goes on to fail.
        let seen = rig.fake().sync().await;
        assert!(
            seen.iter()
                .all(|message| matches!(message, Message::Status { .. })),
            "{seen:?}"
        );
        held.release();

        let Message::TakeFailed { reason } = rig.fake().recv_other().await else {
            panic!("expected take_failed");
        };
        assert_eq!(reason, "fail-before-ready");
        let seen = rig.fake().sync().await;
        assert!(
            !seen
                .iter()
                .any(|message| matches!(message, Message::HandedOver { .. })),
            "{seen:?}"
        );
        assert!(!rig.lease_is_free(), "a failed take over gave the lease up");
        assert!(
            !events(&rig).iter().any(|(name, _)| name == "handed_over"),
            "{:?}",
            events(&rig)
        );
        assert!(!rig.path("handover.json").exists());
        assert_eq!(rig.fake().ping().await, "taking-over");
        assert_eq!(stop(&mut rig).await, ExitCode::FAILURE);
    }

    // L9.5: a resume never waits for the observing steps of the hand over,
    // such as the barrier the gate closes behind: it ends the hand over and
    // answers at once, with the gate open again.
    #[tokio::test]
    async fn a_resume_while_the_hand_over_waits_at_a_barrier_resumes_promptly() {
        let mut rig = active_rig(marked(managed(Role::Active), "a", &[])).await;
        let management = rig.management.unwrap();
        let mut barriers = Barriers::of(&rig);
        barriers.hold(GATE_CLOSED);
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        barriers.arrived(GATE_CLOSED).await;
        assert!(!rig.path("handover.json").exists());

        rig.fake().send(Message::Resume).await;
        let next = rig.fake().recv_other().await;
        assert!(matches!(next, Message::Resumed), "{next:?}");
        assert!(!rig.lease_is_free(), "the lease was kept");
        let answer = get(management, MANAGEMENT_CONFIG).await;
        assert!(!is_updating(&answer), "{answer}");
        let answer = get(rig.main, "/healthz").await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(rig.rotating());
        assert!(
            !rig.path("handover.json").exists(),
            "the cancelled hand over went on to write its state"
        );
        barriers.release(GATE_CLOSED);
        assert!(succeeded(stop(&mut rig).await));
    }

    // L9.17: only the driver gives the lease up, when it takes the end of the
    // hand over; the continuation never does (see
    // `ordering::a_hand_over_that_ends_well_leaves_the_lease_in_its_slot`). A
    // `stop` while the hand over waits at the lease barrier ends the worker
    // without a `handed_over`. The barrier stays held, so the stop is the only
    // thing the worker can have taken: the hand over can't have ended. The
    // race of a barrier that goes just as the stop is selected can't be
    // set up from outside the worker, which has no signal for the choice.
    #[tokio::test]
    async fn a_stop_during_the_lease_barrier_keeps_the_lease_until_the_worker_exits() {
        let mut rig = active_rig(marked(Setup::new(Role::Active), "a", &[])).await;
        let mut barriers = Barriers::of(&rig);
        barriers.hold(LEASE_RELEASE);
        rig.fake()
            .send(Message::HandOver { budget_ms: 5_000 })
            .await;
        barriers.arrived(LEASE_RELEASE).await;
        rig.fake().send(Message::Stop { budget_ms: 3_000 }).await;

        assert!(succeeded(rig.exit().await));
        let rest = rig.fake().until_closed().await;
        assert!(
            !rest
                .iter()
                .any(|message| matches!(message, Message::HandedOver { .. })),
            "{rest:?}"
        );
        assert!(!events(&rig).iter().any(|(name, _)| name == "handed_over"));
    }

    /// The answer to a `GET` that carries `key` as a bearer token.
    async fn get_bearer(address: SocketAddr, path: &str, key: &str) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut answer = String::new();
        tokio::time::timeout(WAIT, stream.read_to_string(&mut answer))
            .await
            .expect("no answer in time")
            .unwrap();
        answer
    }

    /// A codex credential named `name` in `dir`.
    fn credential(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(name),
            r#"{"type":"codex","email":"a@example.com","access_token":"access-0","refresh_token":"refresh-0"}"#,
        )
        .unwrap();
    }

    // L9.5: a resume that took the lease back has applied the whole config
    // when it says `resumed`: the new API keys, and the credentials of a new
    // auth directory, which a closed gate had deferred.
    #[tokio::test]
    async fn a_resume_that_retakes_the_lease_applies_the_whole_config_before_resumed() {
        // (That `resumed` waits for the loop's ack is shown by the switch-level
        // `ordering::a_resume_waits_for_the_ack_of_the_apply_before_it_resumes`.)
        let mut setup = marked(Setup::new(Role::Active), "a", &[]);
        setup.config =
            "api-keys:\n  - old-key\nremote-management:\n  secret-key: pw\n  allow-remote: true\n"
                .to_owned();
        let mut rig = active_rig(setup).await;
        let root = rig._dir.path().to_owned();
        credential(&root.join("auth"), "codex-old.json");
        let mut listed = String::new();
        for _ in 0..500 {
            listed = get_bearer(rig.main, "/v0/management/auth-files", "pw").await;
            if listed.contains("codex-old.json") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(listed.contains("codex-old.json"), "{listed}");
        let answer = get_bearer(rig.main, "/v1/models", "old-key").await;
        assert!(!answer.starts_with("HTTP/1.1 401"), "{answer}");

        hand_over(&mut rig).await;
        assert!(rig.lease_is_free());
        credential(&root.join("auth-new"), "codex-new.json");
        std::fs::write(
            root.join("config.yaml"),
            format!(
                "host: 127.0.0.1\nport: {}\nauth-dir: '{}'\napi-keys:\n  - new-key\nremote-management:\n  secret-key: pw\n  allow-remote: true\n",
                rig.main.port(),
                root.join("auth-new").display()
            ),
        )
        .unwrap();

        rig.fake().send(Message::Resume).await;
        let next = rig.fake().recv_other().await;
        assert!(matches!(next, Message::Resumed), "{next:?}");
        let answer = get_bearer(rig.main, "/v1/models", "old-key").await;
        assert!(answer.starts_with("HTTP/1.1 401"), "{answer}");
        let answer = get_bearer(rig.main, "/v1/models", "new-key").await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        let listed = get_bearer(rig.main, "/v0/management/auth-files", "pw").await;
        assert!(listed.contains("codex-new.json"), "{listed}");
        assert!(!listed.contains("codex-old.json"), "{listed}");
        assert!(succeeded(stop(&mut rig).await));
    }

    // L9.5: a config whose auth directory can't be taken into use is not
    // applied, and a resume that retook the lease then does not say
    // `resumed`: the worker ends with failure, as a failed resume does.
    #[tokio::test]
    async fn a_resume_whose_config_names_an_unusable_auth_dir_does_not_say_resumed() {
        let mut rig = active_rig(marked(Setup::new(Role::Active), "a", &[])).await;
        let root = rig._dir.path().to_owned();
        hand_over(&mut rig).await;
        assert!(rig.lease_is_free());
        let file = root.join("not-a-directory");
        std::fs::write(&file, "a file").unwrap();
        std::fs::write(
            root.join("config.yaml"),
            format!(
                "host: 127.0.0.1\nport: {}\nauth-dir: '{}'\n",
                rig.main.port(),
                file.display()
            ),
        )
        .unwrap();

        rig.fake().send(Message::Resume).await;
        let rest = rig.fake().until_closed().await;
        assert!(
            !rest
                .iter()
                .any(|message| matches!(message, Message::Resumed)),
            "{rest:?}"
        );
        assert_eq!(rig.exit().await, ExitCode::FAILURE);
        assert!(!events(&rig).iter().any(|(name, _)| name == "resumed"));
    }

    // L9.5: a credential directory the resume can't list (here replaced by a
    // regular file while the worker was handed over, the config unchanged) is
    // not acknowledged: no `resumed`, the worker ends with failure, and what it
    // still answers while it stops is not a served request. Unix only: on
    // Windows the watcher's handle keeps the directory from being moved or
    // removed, so the replacement can't be made.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_resume_whose_auth_dir_became_a_file_does_not_say_resumed() {
        let mut rig = active_rig(marked(managed(Role::Active), "a", &[])).await;
        let management = rig.management.unwrap();
        let root = rig._dir.path().to_owned();
        hand_over(&mut rig).await;
        assert!(rig.lease_is_free());
        std::fs::rename(root.join("auth"), root.join("auth-aside")).unwrap();
        std::fs::write(root.join("auth"), "a file").unwrap();

        rig.fake().send(Message::Resume).await;
        // Whatever either listener still answers while the worker stops is
        // the closed gate's, not a served request.
        for (address, path) in [(management, MANAGEMENT_CONFIG), (rig.main, "/healthz")] {
            let probe = async {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut stream = tokio::net::TcpStream::connect(address).await.ok()?;
                let request =
                    format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
                stream.write_all(request.as_bytes()).await.ok()?;
                let mut answer = String::new();
                stream.read_to_string(&mut answer).await.ok()?;
                Some(answer)
            };
            if let Ok(Some(answer)) = tokio::time::timeout(Duration::from_secs(2), probe).await {
                assert!(answer.starts_with("HTTP/1.1 503"), "{path}: {answer}");
            }
        }
        let rest = rig.fake().until_closed().await;
        assert!(
            !rest
                .iter()
                .any(|message| matches!(message, Message::Resumed)),
            "{rest:?}"
        );
        assert_eq!(rig.exit().await, ExitCode::FAILURE);
        assert!(!events(&rig).iter().any(|(name, _)| name == "resumed"));
    }

    // L9.5: a credential file deleted while the worker was handed over is not
    // listed once `resumed` is said: the watcher's removal was held back by
    // the closed gate.
    #[tokio::test]
    async fn a_resume_that_retakes_the_lease_forgets_a_credential_deleted_meanwhile() {
        let mut setup = marked(Setup::new(Role::Active), "a", &[]);
        setup.config = "remote-management:\n  secret-key: pw\n  allow-remote: true\n".to_owned();
        let mut rig = active_rig(setup).await;
        let root = rig._dir.path().to_owned();
        credential(&root.join("auth"), "codex-gone.json");
        credential(&root.join("auth"), "codex-kept.json");
        let mut listed = String::new();
        for _ in 0..500 {
            listed = get_bearer(rig.main, "/v0/management/auth-files", "pw").await;
            if listed.contains("codex-gone.json") && listed.contains("codex-kept.json") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(listed.contains("codex-gone.json"), "{listed}");

        hand_over(&mut rig).await;
        std::fs::remove_file(root.join("auth").join("codex-gone.json")).unwrap();
        rig.fake().send(Message::Resume).await;
        let next = rig.fake().recv_other().await;
        assert!(matches!(next, Message::Resumed), "{next:?}");
        let listed = get_bearer(rig.main, "/v0/management/auth-files", "pw").await;
        assert!(listed.contains("codex-kept.json"), "{listed}");
        assert!(!listed.contains("codex-gone.json"), "{listed}");
        assert!(succeeded(stop(&mut rig).await));
    }
}
