//! Not upstream's: the fixture's own endpoint. On Unix a supervisor that is given no
//! location binds the per-user socket, which a real supervisor of the same user may hold
//! (the A3 review's High). The fixture's supervisor binds `<fixture>/e/supervisor.sock`
//! instead, the fixture connects only there, and its own `supervisor.json` must name that
//! location and the child it started before an answer is trusted.
#![cfg(feature = "blue-green-test")]

mod under_test;
use open_ferry_supervisor::endpoint::{Identity, Location, ServiceKind};
use under_test::*;

#[test]
fn the_fixture_binds_and_asks_its_own_endpoint_and_the_record_names_its_child() {
    let fx = Fx::new();
    let sup = fx.start(&Tm::default());
    let status = fx
        .status()
        .expect("the supervisor answers on the fixture's endpoint");
    assert_eq!(status["state"], "idle", "{status}");

    // The record is the fixture's own: the child's pid and the location the client uses.
    let text = std::fs::read_to_string(fx.data().join("supervisor.json")).expect("record");
    let record: serde_json::Value = serde_json::from_str(&text).expect("record parses");
    let used = client_location(fx.root()).expect("the client's location");
    assert_eq!(record["pid"].as_u64(), Some(u64::from(sup.pid())), "{text}");
    assert_eq!(
        record["endpoint"].as_str(),
        Some(used.to_record().as_str()),
        "{text}"
    );
    fx.check_the_supervisor_is_ours();

    // The client's location is not the default one. On Windows the default is already
    // per data directory (Location::resolve, src/endpoint/windows.rs:103), so the two
    // are the same name there and the check is the record above.
    let identity = Identity::current(ServiceKind::User).expect("identity");
    let default = Location::resolve(&identity, &fx.data());
    #[cfg(unix)]
    {
        let root = std::fs::canonicalize(fx.root()).expect("canonical fixture root");
        let own = Location::new(&root, "e").expect("location");
        assert!(
            own.socket_path().as_os_str().len() <= 103,
            "SET-UP: canonical socket path exceeds 103 bytes: {}",
            own.socket_path().display()
        );
        assert_eq!(used, own);
        assert!(
            std::path::Path::new(&own.to_record()).exists(),
            "the injected socket"
        );
        if let Ok(default) = default {
            assert_ne!(
                used.to_record(),
                default.to_record(),
                "the fixture used the default"
            );
            assert_ne!(
                record["endpoint"].as_str(),
                Some(default.to_record().as_str()),
                "{text}"
            );
        }
    }
    #[cfg(windows)]
    assert_eq!(default.expect("default").to_record(), used.to_record());
}

/// A record naming a live process that is not the child is refused, naming both.
#[test]
#[should_panic(expected = "another supervisor may be answering")]
fn a_record_naming_another_live_process_is_refused() {
    let fx = Fx::new();
    let _sup = fx.start(&Tm::default());
    let text = std::fs::read_to_string(fx.data().join("supervisor.json")).unwrap();
    let mut record: serde_json::Value = serde_json::from_str(&text).unwrap();
    // The test process itself: alive, and not the child.
    record["pid"] = std::process::id().into();
    std::fs::write(fx.data().join("supervisor.json"), record.to_string()).unwrap();
    fx.check_the_supervisor_is_ours();
}

/// A record naming another endpoint is refused.
#[test]
#[should_panic(expected = "supervisor.json names the endpoint")]
fn a_record_naming_another_endpoint_is_refused() {
    let fx = Fx::new();
    let _sup = fx.start(&Tm::default());
    let text = std::fs::read_to_string(fx.data().join("supervisor.json")).unwrap();
    let mut record: serde_json::Value = serde_json::from_str(&text).unwrap();
    record["endpoint"] = "/run/user/1000/open-ferry/supervisor.sock".into();
    std::fs::write(fx.data().join("supervisor.json"), record.to_string()).unwrap();
    fx.check_the_supervisor_is_ours();
}

/// The record of a supervisor that answers, rewritten with `edit`; the answer must not be
/// trusted while the record is not the child's own.
fn with_the_record(edit: impl FnOnce(&Fx, &mut serde_json::Value)) -> (Fx, Sup) {
    let fx = Fx::new();
    let sup = fx.start(&Tm::default());
    // The supervisor does answer, and the record is its own.
    assert!(fx.status().is_some(), "{}", fx.dump());
    let path = fx.data().join("supervisor.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    edit(&fx, &mut record);
    std::fs::write(&path, record.to_string()).unwrap();
    (fx, sup)
}

/// A pid that belonged to a process that has exited.
fn a_dead_pid() -> u32 {
    #[cfg(windows)]
    let mut command = std::process::Command::new("cmd");
    #[cfg(windows)]
    command.args(["/c", "exit"]);
    #[cfg(unix)]
    let mut command = std::process::Command::new("true");
    let mut child = command.spawn().expect("a short process");
    let pid = child.id();
    child.wait().unwrap();
    assert!(!alive(pid));
    pid
}

#[track_caller]
fn assert_refused(fx: &Fx, what: &str) {
    assert!(
        matches!(fx.identity(), Err(Refusal::Wait(_))),
        "{what}: {:?}",
        fx.identity()
    );
    // The supervisor is there and would answer; the fixture does not ask it.
    assert!(fx.status().is_none(), "{what}: the reply was trusted");
}

#[test]
fn a_missing_record_is_not_trusted() {
    let (fx, _sup) = with_the_record(|_, _| {});
    std::fs::remove_file(fx.data().join("supervisor.json")).unwrap();
    assert_refused(&fx, "no record");
}

#[test]
fn a_malformed_record_is_not_trusted() {
    let (fx, _sup) = with_the_record(|_, _| {});
    std::fs::write(fx.data().join("supervisor.json"), "{not json").unwrap();
    assert_refused(&fx, "a malformed record");
    std::fs::write(fx.data().join("supervisor.json"), "{}").unwrap();
    assert_refused(&fx, "a record with no endpoint");
}

#[test]
fn a_record_naming_a_dead_pid_is_not_trusted() {
    let (fx, _sup) = with_the_record(|_, record| record["pid"] = a_dead_pid().into());
    assert_refused(&fx, "a dead pid");
}

/// A record with no usable pid is refused outright, not waited for.
#[test]
#[should_panic(expected = "names no usable pid")]
fn a_record_with_no_pid_is_refused() {
    let (fx, _sup) = with_the_record(|_, record| {
        record.as_object_mut().unwrap().remove("pid");
    });
    let _ = fx.status();
}

#[test]
#[should_panic(expected = "names no usable pid")]
fn a_record_with_a_pid_that_is_not_a_number_is_refused() {
    let (fx, _sup) = with_the_record(|_, record| record["pid"] = "12".into());
    let _ = fx.status();
}

/// The child's own pid plus 2^32 truncates to the child's pid as a `u32`; as the
/// pid it names it is out of range, so the record is refused and the ask panics.
#[test]
#[should_panic(expected = "which is out of range")]
fn a_record_naming_the_childs_pid_plus_two_to_the_32_is_refused() {
    let (fx, _sup) = with_the_record(|_, record| {
        let child = record["pid"].as_u64().expect("the child's pid");
        record["pid"] = (child + (1u64 << 32)).into();
    });
    let _ = fx.status();
}

/// The same record naming a live pid that is not the child: the ask itself panics.
#[test]
#[should_panic(expected = "another supervisor may be answering")]
fn a_status_with_a_record_naming_another_live_process_panics() {
    let (fx, _sup) = with_the_record(|_, record| record["pid"] = std::process::id().into());
    let _ = fx.status();
}
