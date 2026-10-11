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
        let own = Location::new(fx.root(), "e").expect("location");
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
