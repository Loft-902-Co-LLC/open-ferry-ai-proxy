//! Not upstream's: open-ferry's cap on long quota rests
//! (`routing.quota.check-after`, see `quota_check`), which CLIProxyAPI
//! doesn't have, so nothing here is ported and there is no parity suite for
//! it.
//!
//! The manager runs against a fake executor whose answers each test sets:
//! a quota answer whose reset is a fixed time, a success, or another
//! failure. Time moves with the test clock, or with paused Tokio time where
//! a call has to stay in flight. The tests of `save-cooldown-status` use a
//! store in memory, and a file store in a temp dir.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use chrono::{TimeZone, Utc};
use futures_util::StreamExt;

use super::cooldown_state_store::RecordingStore;
use super::support::*;
use crate::auth::{QuotaState, Timestamp};
use crate::exec::{Dispatcher, ExecError};
use crate::manager::cooldown::add;
use crate::manager::cooldown_store::{
    FileStore, QuotaCheckRecord, Record, StateStore, flush, install_store, restore_now,
    set_debounce, snapshot,
};
use crate::manager::{Clock, Manager, QuotaCheck, Settings};

const MODEL: &str = "m";
const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(86_400);

/// What the fake provider answers.
#[derive(Clone, Debug)]
enum Answer {
    /// A quota answer saying the quota resets at this time; credential-wide
    /// when the flag is set.
    Quota(Timestamp, bool),
    Ok,
    /// A stream of three chunks.
    Chunks,
    Fail(u16),
}

struct Rig {
    h: Harness,
    executor: Arc<FakeExecutor>,
    answer: Arc<Mutex<Answer>>,
}

impl Rig {
    /// One Claude credential `a` serving `models`, with the cap at `cap`.
    fn new(cap: Duration, models: &[&str]) -> Self {
        let h = Harness::new(Settings {
            quota_check_after: cap,
            ..Settings::default()
        });
        let answer = Arc::new(Mutex::new(Answer::Ok));
        let clock: Clock = h.clock.clock();
        let shared = answer.clone();
        let executor = FakeExecutor::with("claude", move |_: &Call| {
            match shared.lock().expect("answer").clone() {
                Answer::Quota(reset, credential) => {
                    let mut err = ExecError::upstream(429, "rate limited");
                    err.retry_after = Some((reset - clock()).to_std().unwrap_or_default());
                    err.credential_scoped = credential;
                    Reply::Err(err)
                }
                Answer::Ok => Reply::ok("ok"),
                Answer::Chunks => Reply::chunks(
                    ["one", "two", "three"]
                        .into_iter()
                        .map(|chunk| Ok(Bytes::from(chunk)))
                        .collect(),
                ),
                Answer::Fail(status) => Reply::status(status, "upstream failed"),
            }
        });
        h.executor(&executor);
        h.add(auth("a", "claude"), models);
        Self {
            h,
            executor,
            answer,
        }
    }

    fn answer(&self, answer: Answer) {
        *self.answer.lock().expect("answer") = answer;
    }

    /// Calls `model`; true if it succeeded.
    async fn call(&self, model: &str) -> bool {
        call(&self.h.manager, model).await
    }

    /// How many calls reached the provider.
    fn reached(&self) -> usize {
        self.executor.calls().len()
    }

    fn checks(&self) -> Vec<QuotaCheck> {
        self.h.manager.quota_checks("a")
    }

    /// The model's cooldown end.
    fn retry_at(&self, model: &str) -> Option<Timestamp> {
        self.h
            .get("a")
            .model_states
            .get(model)
            .and_then(|state| state.next_retry_after)
    }

    fn at(&self, d: Duration) -> Timestamp {
        add(self.h.now(), d)
    }
}

async fn call(manager: &Manager, model: &str) -> bool {
    manager
        .execute(&providers(&["claude"]), request(model), options())
        .await
        .is_ok()
}

fn check(model_key: &str, next: Timestamp, reset: Timestamp, wait: Duration) -> QuotaCheck {
    QuotaCheck {
        model_key: model_key.to_owned(),
        next_check_at: next,
        provider_reset_at: reset,
        wait,
        checking: false,
    }
}

// Not upstream's: a quota rest longer than the cap lasts the cap, and then
// the next call goes through; the management view says when.
#[tokio::test(start_paused = true)]
async fn a_rest_longer_than_the_cap_lasts_the_cap() {
    let rig = Rig::new(HOUR, &[MODEL]);
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    let check_at = rig.at(HOUR);
    assert_eq!(rig.checks(), [check(MODEL, check_at, reset, HOUR)]);
    assert_eq!(
        rig.retry_at(MODEL),
        Some(check_at),
        "the cooldown ends then"
    );

    rig.h.clock.advance(59 * MINUTE);
    assert!(!rig.call(MODEL).await);
    assert_eq!(rig.reached(), 1, "still resting");

    rig.h.clock.advance(MINUTE);
    rig.answer(Answer::Ok);
    assert!(rig.call(MODEL).await, "the check");
    assert_eq!(rig.reached(), 2);
}

// Not upstream's: a rest within the cap, and every rest with the cap off,
// lasts until the provider's reset, as upstream's does.
#[tokio::test(start_paused = true)]
async fn rests_within_the_cap_or_with_it_off_are_left_alone() {
    let rig = Rig::new(HOUR, &[MODEL]);
    let reset = rig.at(30 * MINUTE);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    assert_eq!(rig.checks(), []);
    assert_eq!(rig.retry_at(MODEL), Some(reset));

    let rig = Rig::new(Duration::ZERO, &[MODEL]);
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    assert_eq!(rig.checks(), []);
    assert_eq!(rig.retry_at(MODEL), Some(reset));
}

// Not upstream's: each quota answer to a check doubles the wait, and a
// wait that would reach the provider's reset rests until it; after the
// reset, a new quota answer starts again at the cap.
#[tokio::test(start_paused = true)]
async fn each_quota_answer_doubles_the_wait_up_to_the_reset() {
    let rig = Rig::new(HOUR, &[MODEL]);
    let reset = rig.at(10 * HOUR);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    let mut reached = 1;
    for wait in [HOUR, 2 * HOUR, 4 * HOUR] {
        let check_at = rig.at(wait);
        assert_eq!(rig.checks(), [check(MODEL, check_at, reset, wait)]);
        rig.h.clock.advance(wait - MINUTE);
        assert!(!rig.call(MODEL).await);
        assert_eq!(rig.reached(), reached, "resting for {wait:?}");
        rig.h.clock.advance(MINUTE);
        assert!(!rig.call(MODEL).await, "the check");
        reached += 1;
        assert_eq!(rig.reached(), reached);
    }
    // Seven hours in, eight would pass the reset three hours away.
    assert_eq!(rig.checks(), []);
    assert_eq!(rig.retry_at(MODEL), Some(reset));
    rig.h.clock.advance(3 * HOUR - MINUTE);
    assert!(!rig.call(MODEL).await);
    assert_eq!(rig.reached(), reached, "resting until the reset");

    rig.h.clock.advance(MINUTE);
    let next_reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(next_reset, false));
    assert!(!rig.call(MODEL).await);
    assert_eq!(rig.reached(), reached + 1);
    assert_eq!(
        rig.checks(),
        [check(MODEL, rig.at(HOUR), next_reset, HOUR)],
        "the cap again, not doubled"
    );
}

// Not upstream's: a check that succeeds ends the rest.
#[tokio::test(start_paused = true)]
async fn a_successful_check_ends_the_rest() {
    let rig = Rig::new(HOUR, &[MODEL]);
    rig.answer(Answer::Quota(rig.at(5 * DAY), false));
    assert!(!rig.call(MODEL).await);
    rig.h.clock.advance(HOUR);
    rig.answer(Answer::Ok);
    assert!(rig.call(MODEL).await);
    assert_eq!(rig.checks(), []);
    assert!(rig.call(MODEL).await);
    assert!(rig.call(MODEL).await);
    assert_eq!(rig.reached(), 4);
    let auth = rig.h.get("a");
    let state = &auth.model_states[MODEL];
    assert!(!state.unavailable && !state.quota.exceeded, "{state:?}");
}

// Not upstream's: while a check is in flight, every other call finds the
// credential resting; one reaches the provider.
#[tokio::test(start_paused = true)]
async fn one_check_at_a_time() {
    for succeeds in [false, true] {
        let rig = Rig::new(HOUR, &[MODEL]);
        let reset = rig.at(5 * DAY);
        rig.answer(Answer::Quota(reset, false));
        assert!(!rig.call(MODEL).await);
        rig.h.clock.advance(HOUR);
        let check_at = rig.h.now();
        if succeeds {
            rig.answer(Answer::Ok);
        }
        rig.executor.set_delay(Duration::from_secs(10));
        let calls: Vec<_> = (0..8)
            .map(|_| {
                let manager = rig.h.manager.clone();
                tokio::spawn(async move { call(&manager, MODEL).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(rig.reached(), 2, "one check in flight");
        assert_eq!(
            rig.checks(),
            [QuotaCheck {
                checking: true,
                ..check(MODEL, check_at, reset, HOUR)
            }]
        );
        let mut ok = 0;
        for task in calls {
            ok += usize::from(task.await.expect("join"));
        }
        assert_eq!(rig.reached(), 2, "succeeds: {succeeds}");
        assert_eq!(ok, usize::from(succeeds));
        if succeeds {
            assert_eq!(rig.checks(), []);
        } else {
            let check_at = rig.at(2 * HOUR);
            assert_eq!(rig.checks(), [check(MODEL, check_at, reset, 2 * HOUR)]);
        }
    }
}

// Not upstream's: a check answered with another failure lets the next call
// check, once that failure's own cooldown is over.
#[tokio::test(start_paused = true)]
async fn another_failure_lets_the_next_call_check() {
    let rig = Rig::new(HOUR, &[MODEL]);
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    rig.h.clock.advance(HOUR);
    let check_at = rig.h.now();
    rig.answer(Answer::Fail(500));
    assert!(!rig.call(MODEL).await);
    assert_eq!(rig.reached(), 2);
    assert_eq!(rig.checks(), [check(MODEL, check_at, reset, HOUR)]);

    rig.h.clock.advance(MINUTE);
    rig.answer(Answer::Ok);
    assert!(rig.call(MODEL).await);
    assert_eq!(rig.reached(), 3);
    assert_eq!(rig.checks(), []);
}

// Not upstream's: a stream holds its check until it ends; a client that
// leaves first lets the next call check.
#[tokio::test(start_paused = true)]
async fn a_stream_holds_its_check_until_it_ends() {
    let rig = Rig::new(HOUR, &[MODEL]);
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    rig.h.clock.advance(HOUR);
    rig.answer(Answer::Chunks);
    let names = providers(&["claude"]);

    let stream = rig
        .h
        .manager
        .execute_stream(&names, request(MODEL), options())
        .await
        .expect("the check's stream");
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(rig.checks()[0].checking);
    assert!(!rig.call(MODEL).await, "the stream is still checking");
    assert_eq!(rig.reached(), 2);

    drop(stream);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!rig.checks()[0].checking, "the client left");

    let stream = rig
        .h
        .manager
        .execute_stream(&names, request(MODEL), options())
        .await
        .expect("the next check's stream");
    let chunks: Vec<_> = stream.chunks.collect().await;
    assert_eq!(chunks.len(), 3);
    assert_eq!(rig.reached(), 3);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(rig.checks(), [], "read to the end");
}

// Not upstream's: a credential-wide quota rests the whole credential, and
// one check of any model settles it.
#[tokio::test(start_paused = true)]
async fn a_credential_wide_rest_is_checked_once() {
    let rig = Rig::new(HOUR, &["m1", "m2"]);
    let reset = rig.at(7 * DAY);
    rig.answer(Answer::Quota(reset, true));
    assert!(!rig.call("m1").await);
    let check_at = rig.at(HOUR);
    assert_eq!(rig.checks(), [check("", check_at, reset, HOUR)]);
    assert!(!rig.call("m2").await);
    assert_eq!(rig.reached(), 1, "every model rests");

    rig.h.clock.advance(HOUR);
    rig.executor.set_delay(Duration::from_secs(10));
    let manager = rig.h.manager.clone();
    let first = tokio::spawn(async move { call(&manager, "m2").await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!rig.call("m1").await, "the other model waits for the check");
    assert!(!first.await.expect("join"));
    assert_eq!(rig.reached(), 2);
    let check_at = rig.at(2 * HOUR);
    assert_eq!(rig.checks(), [check("", check_at, reset, 2 * HOUR)]);

    rig.executor.set_delay(Duration::ZERO);
    rig.h.clock.advance(2 * HOUR);
    rig.answer(Answer::Ok);
    assert!(rig.call("m1").await);
    assert_eq!(rig.checks(), []);
    assert!(rig.call("m2").await);
}

// Not upstream's: resetting the quota forgets the rest and its doubling.
#[tokio::test(start_paused = true)]
async fn reset_quota_forgets_the_rest() {
    let rig = Rig::new(HOUR, &[MODEL]);
    rig.answer(Answer::Quota(rig.at(5 * DAY), false));
    assert!(!rig.call(MODEL).await);
    rig.h.clock.advance(HOUR);
    assert!(!rig.call(MODEL).await);
    assert_eq!(rig.checks()[0].wait, 2 * HOUR);

    rig.h
        .manager
        .reset_quota("a")
        .expect("reset")
        .expect("the credential");
    assert_eq!(rig.checks(), []);
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await, "goes through at once");
    assert_eq!(rig.reached(), 3);
    assert_eq!(rig.checks(), [check(MODEL, rig.at(HOUR), reset, HOUR)]);
}

/// Installs `store` on `rig`'s manager and restores from it, as a start
/// with `save-cooldown-status` on does. It saves only when flushed.
fn attach(rig: &Rig, store: &Arc<RecordingStore>) {
    set_debounce(&rig.h.manager, DAY);
    install_store(&rig.h.manager, store.clone());
    restore_now(&rig.h.manager);
}

/// A manager like `rig`'s started `later` after `rig`'s now, from what
/// `rig`'s store saved.
fn restart(rig: &Rig, saved: Vec<Record>, later: Duration) -> Rig {
    let restarted = Rig::new(HOUR, &[MODEL]);
    let since = (rig.h.now() - restarted.h.now())
        .to_std()
        .expect("the restart is later");
    restarted.h.clock.advance(since + later);
    attach(&restarted, &RecordingStore::with_load(saved));
    restarted
}

/// The saved cooldown of `MODEL`.
fn saved_model(saved: &[Record]) -> Record {
    saved
        .iter()
        .find(|record| record.model == MODEL)
        .cloned()
        .expect("the model's cooldown")
}

// Not upstream's: `save-cooldown-status` saves a capped rest with its
// cooldown, and a restart keeps the doubling.
#[tokio::test(start_paused = true)]
async fn a_restart_keeps_the_doubling() {
    let store = RecordingStore::with_load(Vec::new());
    let rig = Rig::new(HOUR, &[MODEL]);
    attach(&rig, &store);
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    rig.h.clock.advance(HOUR);
    assert!(!rig.call(MODEL).await);
    let check_at = rig.at(2 * HOUR);
    flush(&rig.h.manager);
    let saved = store.saved();
    let model = saved_model(&saved);
    assert_eq!(model.next_retry_after, Some(check_at), "the check time");
    assert_eq!(
        model.quota_check,
        Some(QuotaCheckRecord {
            wait: 2 * HOUR,
            check_at,
            reset_at: reset,
        })
    );
    assert!(
        saved
            .iter()
            .all(|record| record.model == MODEL || record.quota_check.is_none()),
        "the credential's own cooldown has no rest of its own: {saved:?}"
    );

    let restarted = restart(&rig, saved, HOUR);
    assert_eq!(
        restarted.checks(),
        [check(MODEL, check_at, reset, 2 * HOUR)]
    );
    assert_eq!(restarted.retry_at(MODEL), Some(check_at));
    restarted.answer(Answer::Quota(reset, false));
    assert!(!restarted.call(MODEL).await);
    assert_eq!(restarted.reached(), 0, "still resting");
    restarted.h.clock.advance(HOUR);
    assert!(!restarted.call(MODEL).await, "the check");
    assert_eq!(restarted.reached(), 1);
    assert_eq!(
        restarted.checks(),
        [check(MODEL, restarted.at(4 * HOUR), reset, 4 * HOUR)]
    );
}

// Not upstream's: a check in flight when the cooldowns are saved is due at
// once after a restart, not held until the provider's reset.
#[tokio::test(start_paused = true)]
async fn a_check_in_flight_at_the_save_is_due_after_a_restart() {
    let store = RecordingStore::with_load(Vec::new());
    let rig = Rig::new(HOUR, &[MODEL]);
    attach(&rig, &store);
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    rig.h.clock.advance(HOUR);
    let check_at = rig.h.now();
    rig.executor.set_delay(Duration::from_secs(10));
    let manager = rig.h.manager.clone();
    let in_flight = tokio::spawn(async move { call(&manager, MODEL).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(rig.checks()[0].checking);
    flush(&rig.h.manager);
    let saved = store.saved();
    let model = saved_model(&saved);
    assert_eq!(model.next_retry_after, Some(reset), "held for the check");
    assert_eq!(model.quota_check.map(|c| c.check_at), Some(check_at));

    let restarted = restart(&rig, saved, Duration::ZERO);
    assert_eq!(restarted.checks(), [check(MODEL, check_at, reset, HOUR)]);
    assert_eq!(restarted.retry_at(MODEL), Some(check_at));
    restarted.answer(Answer::Ok);
    assert!(restarted.call(MODEL).await, "due at once");
    assert_eq!(restarted.checks(), []);
    assert!(!in_flight.await.expect("join"));
}

// Not upstream's: with the cap off, a saved rest isn't put back, only its
// cooldown, as upstream restores it; and nothing of a rest is saved.
#[tokio::test(start_paused = true)]
async fn with_the_cap_off_no_rest_is_restored_or_saved() {
    let store = RecordingStore::with_load(Vec::new());
    let rig = Rig::new(HOUR, &[MODEL]);
    attach(&rig, &store);
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    flush(&rig.h.manager);
    let saved = store.saved();
    assert!(saved_model(&saved).quota_check.is_some());

    let off = Rig::new(Duration::ZERO, &[MODEL]);
    attach(&off, &RecordingStore::with_load(saved));
    assert_eq!(off.checks(), []);
    assert_eq!(off.retry_at(MODEL), Some(rig.at(HOUR)));

    rig.h.manager.set_settings(Settings::default());
    let records = snapshot(&rig.h.manager, rig.h.now());
    assert!(!records.is_empty());
    assert!(records.iter().all(|record| record.quota_check.is_none()));
}

// Not upstream's: a file keeps a capped rest after upstream's fields, under
// a key of open-ferry's own, and reads it back; a key that isn't a rest is
// ignored and the cooldown read.
#[test]
fn a_file_keeps_the_rest_under_its_own_key() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = FileStore::new(dir.path().to_path_buf());
    let t0 = Utc
        .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("time");
    let check_at = add(t0, 2 * HOUR);
    let reset = add(t0, 4 * DAY);
    let record = Record {
        provider: "claude".into(),
        auth_id: "a".into(),
        model: MODEL.into(),
        status: "cooling".into(),
        next_retry_after: Some(check_at),
        reason: "quota".into(),
        quota: QuotaState {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(check_at),
            ..QuotaState::default()
        },
        updated_at: Some(t0),
        quota_check: Some(QuotaCheckRecord {
            wait: 2 * HOUR,
            check_at,
            reset_at: reset,
        }),
        ..Record::default()
    };
    store.save(std::slice::from_ref(&record), t0).expect("save");
    let path = dir.path().join("a.cds");
    let written = std::fs::read_to_string(&path).expect("read");
    let tail = r#"      "updated_at": "2026-06-01T00:00:00Z",
      "open_ferry_quota_check": {
        "wait_seconds": 7200,
        "check_at": "2026-06-01T02:00:00Z",
        "reset_at": "2026-06-05T00:00:00Z"
      }
    }
  ]
}
"#;
    assert!(written.ends_with(tail), "{written}");
    assert_eq!(store.load().expect("load"), std::slice::from_ref(&record));

    let start = written.find(r#""open_ferry_quota_check": "#).expect("key");
    let end = written.len() - "\n    }\n  ]\n}\n".len();
    for bad in [
        r#""soon""#,
        "null",
        r#"{"wait_seconds":0,"check_at":"2026-06-01T02:00:00Z","reset_at":"2026-06-05T00:00:00Z"}"#,
        r#"{"wait_seconds":7200,"check_at":"soon","reset_at":"2026-06-05T00:00:00Z"}"#,
        r#"{"wait_seconds":7200,"check_at":"2026-06-01T02:00:00Z"}"#,
    ] {
        let file = format!(
            "{}\"open_ferry_quota_check\": {bad}{}",
            &written[..start],
            &written[end..]
        );
        std::fs::write(&path, &file).expect("write");
        let loaded = store.load().expect(bad);
        assert_eq!(
            loaded,
            [Record {
                quota_check: None,
                ..record.clone()
            }],
            "{file}"
        );
    }
}
