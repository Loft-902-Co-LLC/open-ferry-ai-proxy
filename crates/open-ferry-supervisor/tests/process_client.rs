//! Endpoint clients against the real supervisor and fake worker harness.
#![cfg(feature = "blue-green-test")]

mod under_test;
use under_test::*;

use open_ferry_supervisor::client::{self, EndpointSwitcher, SwitchRequest};
use open_ferry_supervisor::endpoint::{Identity, ServiceKind};
use open_ferry_supervisor::protocol::How;
use open_ferry_update::{CancelOutcome, SwitchError, SwitchEvent, Switcher};
use std::time::{Duration, Instant};

fn deadline() -> Instant {
    Instant::now() + LONG
}

#[test]
fn client_follows_a_real_switch_and_the_adapter_rolls_back() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let _sup = fx.start(&Tm {
        watch: 30_000,
        ..Tm::default()
    });
    fx.check_the_supervisor_is_ours();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let identity = Identity::current(ServiceKind::User).unwrap();
    let endpoint = EndpointSwitcher::new(fx.data(), identity.clone());
    let pin = fx.pin(B);
    runtime.block_on(async {
        assert_eq!(
            client::status(&fx.data(), &identity, deadline())
                .await
                .unwrap()
                .active_version
                .as_deref(),
            Some(A)
        );
        assert_eq!(
            endpoint.cancel(deadline()).await.unwrap(),
            CancelOutcome::Nothing
        );
        let mut handle = client::switch(
            &fx.data(),
            &identity,
            SwitchRequest {
                to: B.into(),
                sha256: fx.sha(B),
                pin,
                how: How::Update,
                auto: false,
            },
            deadline(),
        )
        .await
        .unwrap();
        let mut phases = Vec::new();
        while let Some(event) = handle.next(deadline()).await.unwrap() {
            match event {
                SwitchEvent::Progress { phase } => phases.push(phase),
                SwitchEvent::Done { result } => assert_eq!(result["result"], "switched"),
            }
        }
        assert!(phases.iter().any(|phase| phase == "loading"));
        assert_eq!(
            endpoint.status(deadline()).await.unwrap()["active_version"],
            B
        );
        assert_eq!(
            endpoint.cancel(deadline()).await.unwrap(),
            CancelOutcome::TooLate
        );
        let journal: open_ferry_update::journal::Journal =
            serde_json::from_value(fx.journal().unwrap()).unwrap();
        let target = journal.switch.unwrap().from;
        let mut rollback = endpoint.rollback(&target, deadline()).await.unwrap();
        while let Some(event) = rollback.next(deadline()).await.unwrap() {
            if let SwitchEvent::Done { result } = event {
                assert_eq!(result["result"], "rolled-back");
            }
        }
        assert_eq!(
            endpoint.status(deadline()).await.unwrap()["active_version"],
            A
        );
    });
}

#[test]
fn dropping_the_client_handle_does_not_cancel_the_switch_and_bad_pins_are_refused() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("hold-load", "@B");
    fx.hold("load.before_loaded");
    let _sup = fx.start(&Tm::default());
    fx.check_the_supervisor_is_ours();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let identity = Identity::current(ServiceKind::User).unwrap();
    let request = |pin| SwitchRequest {
        to: B.into(),
        sha256: fx.sha(B),
        pin,
        how: How::Update,
        auto: false,
    };
    runtime.block_on(async {
        assert!(matches!(
            client::switch(&fx.data(), &identity, request("missing".into()), deadline()).await,
            Err(SwitchError::Refused(_))
        ));
        let handle = client::switch(&fx.data(), &identity, request(fx.pin(B)), deadline())
            .await
            .unwrap();
        drop(handle);
    });
    fx.wait_arrived("load.before_loaded");
    fx.release("load.before_loaded");
    fx.wait_settled();
    runtime.block_on(async {
        let status = client::status(
            &fx.data(),
            &identity,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(status.active_version.as_deref(), Some(B));
    });
}

#[test]
fn the_client_can_cancel_during_loading() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("hold-load", "@B");
    fx.hold("load.before_loaded");
    let _sup = fx.start(&Tm::default());
    fx.check_the_supervisor_is_ours();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let identity = Identity::current(ServiceKind::User).unwrap();
    let mut handle = runtime
        .block_on(client::switch(
            &fx.data(),
            &identity,
            SwitchRequest {
                to: B.into(),
                sha256: fx.sha(B),
                pin: fx.pin(B),
                how: How::Update,
                auto: false,
            },
            deadline(),
        ))
        .unwrap();
    fx.wait_arrived("load.before_loaded");
    runtime.block_on(async {
        assert_eq!(
            client::cancel(&fx.data(), &identity, deadline())
                .await
                .unwrap(),
            CancelOutcome::Cancelled
        );
        while let Some(event) = handle.next(deadline()).await.unwrap() {
            if let SwitchEvent::Done { result } = event {
                assert_eq!(result["result"], "cancelled");
            }
        }
    });
    fx.release("load.before_loaded");
    fx.wait_settled();
    assert!(fx.state().failed.is_empty());
    assert_eq!(fx.status().unwrap()["active_version"], A);
}
