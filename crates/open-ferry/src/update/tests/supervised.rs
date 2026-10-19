//! Commands share confirmation and JSON behavior with a fake supervisor.

use std::collections::VecDeque;

use open_ferry_update::SwitchProgress;
use open_ferry_update::fetch::BoxFuture;
use serde_json::json;

use super::*;

struct Events(VecDeque<Result<SwitchEvent, SwitchError>>);

impl SwitchProgress for Events {
    fn next(&mut self, _: Instant) -> BoxFuture<'_, Result<SwitchEvent, SwitchError>> {
        Box::pin(async move { self.0.pop_front().unwrap_or(Err(SwitchError::Stopped)) })
    }
}

struct SupervisedFake {
    base: Fake,
    view: SwitchView,
    cancel: CancelOutcome,
    events: RefCell<VecDeque<Result<SwitchEvent, SwitchError>>>,
}

impl SupervisedFake {
    fn new(result: &str) -> Self {
        let mut base = Fake::finding("0.2.0", "0.1.0");
        base.plan = Some(RollbackPlan {
            previous: "0.0.9".into(),
            installed: "0.1.0".into(),
            binary: BINARY.into(),
        });
        Self {
            base,
            view: SwitchView {
                supervisor: Some(
                    json!({"state":"watching", "active_version":"0.1.0", "installed_version":"0.1.0", "switch":{"phase":"watching", "watch_until":"2026-10-18T01:10:00Z"}, "switching_disabled":null}),
                ),
                unreachable: None,
                rollback_target: Some("0.0.9".into()),
                last: None,
            },
            cancel: CancelOutcome::Nothing,
            events: RefCell::new(VecDeque::from([
                Ok(SwitchEvent::Progress {
                    phase: "loading".into(),
                }),
                Ok(SwitchEvent::Progress {
                    phase: "handing-over".into(),
                }),
                Ok(SwitchEvent::Done {
                    result: json!({"result":result,"why":"test outcome"}),
                }),
            ])),
        }
    }

    fn changed(&self, how: &str, to: &str) -> Changed {
        let record = SwitchRecord::started("operation", how, "0.1.0", to, "2026-10-18T01:00:00Z");
        Changed::Supervised {
            handle: SwitchHandle::new(
                "operation".into(),
                Box::new(Events(self.events.borrow_mut().drain(..).collect())),
            ),
            record,
        }
    }
}

impl Updates for SupervisedFake {
    type Found = ();
    fn status(&self, settings: &Settings) -> Status {
        self.base.status(settings)
    }
    fn is_supervised(&self) -> bool {
        true
    }
    async fn switch_view(&self) -> Result<Option<SwitchView>, UpdateError> {
        Ok(Some(self.view.clone()))
    }
    async fn latest(&self) -> Result<Offer<()>, UpdateError> {
        self.base.latest().await
    }
    fn rollback_plan(&self) -> Result<RollbackPlan, UpdateError> {
        self.base.rollback_plan()
    }
    async fn update(&self, (): ()) -> Result<Changed, UpdateError> {
        self.base.calls.borrow_mut().push("update");
        Ok(self.changed("update", "0.2.0"))
    }
    async fn rollback(&self) -> Result<Changed, UpdateError> {
        self.base.calls.borrow_mut().push("rollback");
        Ok(self.changed("rollback", "0.0.9"))
    }
    async fn cancel(&self) -> Result<CancelOutcome, UpdateError> {
        self.base.calls.borrow_mut().push("cancel");
        Ok(self.cancel)
    }
}

#[tokio::test]
async fn supervised_update_and_rollback_follow_progress_in_text_and_one_json_object() {
    for (request, result, word) in [
        (Request::Update, "switched", "updated"),
        (Request::Rollback, "rolled-back", "rolled-back"),
    ] {
        for json in [false, true] {
            let fake = SupervisedFake::new(result);
            let ran = go(request, &fake, &settings("off", None), true, json, None).await;
            assert_eq!((ran.code, ran.result), (0, word), "{ran:?}");
            assert!(ran.err.is_empty() && ran.asked.is_empty());
            assert!(!ran.out.contains("Nothing is restarted"));
            if json {
                let value = ran.json();
                assert_eq!(value["action"], request.action());
                assert_eq!(value["operation_id"], "operation");
                assert_eq!(value["progress"], json!(["loading", "handing-over"]));
                assert_eq!(value["supervisor_result"]["result"], result);
                assert_eq!(value["switch"]["blue_green"], true);
                assert!(
                    value["message"]
                        .as_str()
                        .unwrap()
                        .contains("Ctrl-C stops following")
                );
            } else {
                for fragment in [
                    "same port",
                    "loading",
                    "handing-over",
                    "The supervisor watches",
                ] {
                    assert!(ran.out.contains(fragment), "{ran:?}");
                }
            }
        }
    }
}

#[tokio::test]
async fn supervised_confirmation_still_precedes_switching() {
    let fake = SupervisedFake::new("switched");
    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        false,
        true,
        Some(true),
    )
    .await;
    assert_eq!((ran.code, ran.result), (3, "needs-confirmation"));
    assert_eq!(fake.base.calls(), ["latest"]);
}

#[tokio::test]
async fn check_keeps_the_live_view_even_when_the_release_check_fails() {
    let mut fake = SupervisedFake::new("switched");
    fake.base = Fake::failing();
    fake.view.supervisor.as_mut().unwrap()["switching_disabled"] = json!("the journal is stale");
    let ran = go(
        Request::Check,
        &fake,
        &settings("", None),
        false,
        true,
        None,
    )
    .await;
    let value = ran.json();
    assert_eq!(ran.code, 1);
    assert_eq!(value["switch_view"]["rollback_target"], "0.0.9");
    assert_eq!(
        value["switch_view"]["supervisor"]["switch"]["phase"],
        "watching"
    );
    assert_eq!(value["status"]["running_version"], "0.1.0");
    assert_eq!(value["status"]["why_not"], "the journal is stale");
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("Watch deadline: 2026-10-18T01:10:00Z")
    );
}

#[tokio::test]
async fn a_watch_keeps_the_installed_version_distinct_from_the_running_version() {
    let mut fake = SupervisedFake::new("switched");
    fake.base = Fake::finding("0.2.0", "0.2.0");
    fake.view.supervisor.as_mut().unwrap()["active_version"] = json!("0.2.0");
    let ran = go(
        Request::Check,
        &fake,
        &settings("", None),
        false,
        true,
        None,
    )
    .await;
    let value = ran.json();
    assert_eq!((ran.code, ran.result), (0, "up-to-date"));
    assert_eq!(value["installed_version"], "0.1.0");
    assert_eq!(value["status"]["installed_version"], "0.1.0");
    assert_eq!(value["status"]["running_version"], "0.2.0");
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("0.2.0 is running")
    );
}

#[tokio::test]
async fn cancellation_distinguishes_cancelled_too_late_and_nothing_without_prompting() {
    for (cancel, result, code) in [
        (CancelOutcome::Cancelled, "cancelled", 0),
        (CancelOutcome::TooLate, "too-late", 1),
        (CancelOutcome::Nothing, "nothing-switching", 0),
    ] {
        let mut fake = SupervisedFake::new("switched");
        fake.cancel = cancel;
        let ran = go(
            Request::Cancel,
            &fake,
            &settings("off", None),
            false,
            true,
            None,
        )
        .await;
        assert_eq!((ran.code, ran.result), (code, result));
        assert_eq!(ran.json()["action"], "cancel");
        assert!(ran.asked.is_empty());
        assert_eq!(fake.base.calls(), ["cancel"]);
    }
}

#[tokio::test]
async fn failed_done_and_ended_after_acceptance_return_failure_with_the_operation_id() {
    for ended in [false, true] {
        let fake = SupervisedFake::new("failed-to-load");
        if ended {
            *fake.events.borrow_mut() =
                VecDeque::from([Err(SwitchError::Ended("the supervisor is stopping".into()))]);
        }
        let ran = go(
            Request::Update,
            &fake,
            &settings("", None),
            true,
            true,
            None,
        )
        .await;
        assert_eq!((ran.code, ran.result), (1, "error"));
        assert_eq!(ran.json()["operation_id"], "operation");
        if ended {
            assert!(
                ran.json()["error"]
                    .as_str()
                    .unwrap()
                    .contains("ended the switch")
            );
        } else {
            assert_eq!(ran.json()["supervisor_result"]["result"], "failed-to-load");
        }
    }
}

#[tokio::test]
async fn stopping_the_follower_drops_its_handle_without_cancelling_the_switch() {
    let fake = SupervisedFake::new("switched");
    let Changed::Supervised { handle, record } = fake.changed("update", "0.2.0") else {
        unreachable!()
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut console = Console::new(true, &mut out, &mut err);
    let outcome = follow_until(
        &fake,
        handle,
        record,
        Outcome::new("update", &settings("", None)),
        &mut console,
        async { Ok(()) },
    )
    .await;
    assert_eq!((outcome.code, outcome.result), (0, "following-stopped"));
    console.finish(outcome);
    let value: Value = serde_json::from_slice(&out).unwrap();
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("it was not cancelled")
    );
    assert!(fake.base.calls().is_empty());
}
