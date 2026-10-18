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
    FileStore, QuotaCheckRecord, Record, RestoreHooks, StateStore, Thaw, flush, install_store,
    restore_hooks_are_gone, restore_now, set_debounce, set_restore_hooks, snapshot, thaw,
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

// Not upstream's: a success from a call dispatched before the quota answer
// leaves the cooldown be (upstream's issue 6643), so it leaves the rest
// beside it too: one check at the cap, and the next quota answer doubles.
#[tokio::test(start_paused = true)]
async fn a_stale_success_keeps_the_rest() {
    use crate::auth::AuthError;
    use crate::manager::CallResult;

    let rig = Rig::new(HOUR, &[MODEL]);
    let result = |success: bool, execution_seq: u64, retry_after: Option<Duration>| CallResult {
        auth_id: "a".into(),
        provider: "claude".into(),
        model: MODEL.into(),
        success,
        execution_seq,
        retry_after,
        error: (!success).then(|| AuthError {
            code: "quota_exhausted".into(),
            message: "QUOTA_EXHAUSTED".into(),
            retryable: true,
            http_status: 429,
        }),
        ..CallResult::default()
    };
    // Two calls are dispatched; the second gets the quota answer first.
    let first = rig.h.manager.next_execution_seq();
    let second = rig.h.manager.next_execution_seq();
    let reset = rig.at(72 * HOUR);
    rig.h
        .manager
        .mark_result(&result(false, second, Some(72 * HOUR)));
    let rest = [check(MODEL, rig.at(HOUR), reset, HOUR)];
    assert_eq!(rig.checks(), rest);

    // The first call succeeds: the cooldown stays, and so does its rest.
    rig.h.manager.mark_result(&result(true, first, None));
    assert_eq!(rig.checks(), rest, "the rest after a stale success");
    assert!(rig.retry_at(MODEL).is_some());

    // At the cap's end one call checks, as it would without the success.
    rig.h.clock.advance(HOUR);
    rig.answer(Answer::Quota(reset, false));
    rig.executor.set_delay(Duration::from_secs(10));
    let calls: Vec<_> = (0..6)
        .map(|_| {
            let manager = rig.h.manager.clone();
            tokio::spawn(async move { call(&manager, MODEL).await })
        })
        .collect();
    for task in calls {
        assert!(!task.await.expect("join"));
    }
    assert_eq!(rig.reached(), 1, "one check, not every waiting call");
    // And its quota answer doubles the wait.
    assert_eq!(
        rig.checks(),
        [check(MODEL, rig.at(2 * HOUR), reset, 2 * HOUR)]
    );
}

/// A result of a call dispatched as `execution_seq`: a success, or a quota
/// answer resetting after `retry_after` (credential-wide when flagged).
fn outcome(
    success: bool,
    execution_seq: u64,
    retry_after: Option<Duration>,
    credential_scope: bool,
) -> crate::manager::CallResult {
    use crate::auth::AuthError;
    crate::manager::CallResult {
        auth_id: "a".into(),
        provider: "claude".into(),
        model: MODEL.into(),
        success,
        execution_seq,
        retry_after,
        credential_scope,
        error: (!success).then(|| AuthError {
            code: "quota_exhausted".into(),
            message: "QUOTA_EXHAUSTED".into(),
            retryable: true,
            http_status: 429,
        }),
        ..crate::manager::CallResult::default()
    }
}

// Not upstream's: a model success is no recovery from an active
// credential-wide quota (upstream keeps it), so it leaves the credential's
// rest, its reset and its doubling be, as it leaves the cooldown.
#[tokio::test(start_paused = true)]
async fn a_model_success_keeps_the_credential_rest() {
    let rig = Rig::new(HOUR, &[MODEL]);
    let first = rig.h.manager.next_execution_seq();
    let second = rig.h.manager.next_execution_seq();
    let reset = rig.at(72 * HOUR);
    rig.h
        .manager
        .mark_result(&outcome(false, second, Some(72 * HOUR), true));
    let rests = rig.checks();
    assert_eq!(rests.len(), 1, "{rests:?}");
    assert_eq!(rests[0].wait, HOUR);
    assert_eq!(rests[0].provider_reset_at, reset);

    // The first call, dispatched before the answer, succeeds.
    rig.h
        .manager
        .mark_result(&outcome(true, first, None, false));
    assert_eq!(rig.checks(), rests, "the rest after a model success");

    // One call checks at the cap's end, and its quota answer doubles.
    rig.h.clock.advance(HOUR);
    rig.answer(Answer::Quota(reset, true));
    rig.executor.set_delay(Duration::from_secs(10));
    let calls: Vec<_> = (0..6)
        .map(|_| {
            let manager = rig.h.manager.clone();
            tokio::spawn(async move { call(&manager, MODEL).await })
        })
        .collect();
    for task in calls {
        assert!(!task.await.expect("join"));
    }
    assert_eq!(rig.reached(), 1, "one check, not every waiting call");
    let rests = rig.checks();
    assert_eq!(rests.len(), 1, "{rests:?}");
    assert_eq!(rests[0].wait, 2 * HOUR);
}

// Not upstream's: the cap shortens a model cooldown to the check's time, so
// a success from a call dispatched before the quota answer that arrives
// after that time still meets a live provider reset; it isn't recovery and
// keeps the rest.
#[tokio::test(start_paused = true)]
async fn a_stale_success_after_the_check_is_due_keeps_the_rest() {
    let rig = Rig::new(MINUTE, &[MODEL]);
    let first = rig.h.manager.next_execution_seq();
    let second = rig.h.manager.next_execution_seq();
    let reset = rig.at(72 * HOUR);
    rig.h
        .manager
        .mark_result(&outcome(false, second, Some(72 * HOUR), false));
    let rests = [check(MODEL, rig.at(MINUTE), reset, MINUTE)];
    assert_eq!(rig.checks(), rests);

    rig.h.clock.advance(MINUTE);
    rig.h
        .manager
        .mark_result(&outcome(true, first, None, false));
    assert_eq!(rig.checks(), rests, "the rest after a stale success");
    assert!(
        rig.h.get("a").model_states[MODEL].unavailable,
        "the model is still resting"
    );

    // The next call is the check, and one check only.
    rig.answer(Answer::Ok);
    assert!(rig.call(MODEL).await, "the check");
    assert_eq!(rig.reached(), 1);
    assert_eq!(rig.checks(), []);
}

// Not upstream's: the same stale success while the check is in flight keeps
// the check's claim and lease, so the other calls still find the credential
// resting; the check's own success, dispatched after the answer, ends it.
#[tokio::test(start_paused = true)]
async fn a_stale_success_while_a_check_is_leased_keeps_its_claim() {
    let rig = Rig::new(MINUTE, &[MODEL]);
    let first = rig.h.manager.next_execution_seq();
    let second = rig.h.manager.next_execution_seq();
    let reset = rig.at(72 * HOUR);
    rig.h
        .manager
        .mark_result(&outcome(false, second, Some(72 * HOUR), false));
    rig.h.clock.advance(MINUTE);

    // The check C goes out and stays in flight.
    rig.answer(Answer::Ok);
    rig.executor.set_delay(Duration::from_secs(10));
    let manager = rig.h.manager.clone();
    let checking = tokio::spawn(async move { call(&manager, MODEL).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(rig.reached(), 1);
    assert!(rig.checks()[0].checking);

    // A succeeds meanwhile: no recovery, and C is still the check.
    rig.h
        .manager
        .mark_result(&outcome(true, first, None, false));
    let rests = rig.checks();
    assert_eq!(rests.len(), 1, "{rests:?}");
    assert!(rests[0].checking, "the lease stays: {rests:?}");
    assert_eq!(rests[0].provider_reset_at, reset);
    assert!(!rig.call(MODEL).await, "the other calls still rest");
    assert_eq!(rig.reached(), 1);

    // C's own success, dispatched after the answer, is recovery.
    assert!(checking.await.expect("join"));
    assert_eq!(rig.checks(), []);
    assert!(rig.call(MODEL).await);
}

// Not upstream's: another model's failure, recorded while the credential's
// check runs, doesn't make the check's own success look stale: the
// credential's rest is ordered by the credential's own watermark.
#[tokio::test(start_paused = true)]
async fn another_models_failure_does_not_make_the_check_stale() {
    use crate::auth::AuthError;
    use crate::manager::CallResult;

    let rig = Rig::new(MINUTE, &["m1", "m2"]);
    // D is dispatched on m2, then B on m1; B gets the credential-wide answer.
    let d = rig.h.manager.next_execution_seq();
    let b = rig.h.manager.next_execution_seq();
    let reset = rig.at(72 * HOUR);
    rig.h.manager.mark_result(&CallResult {
        model: "m1".into(),
        ..outcome(false, b, Some(72 * HOUR), true)
    });
    assert_eq!(rig.checks(), [check("", rig.at(MINUTE), reset, MINUTE)]);
    rig.h.clock.advance(MINUTE);

    // The check C goes out on m1 and stays in flight.
    rig.answer(Answer::Ok);
    rig.executor.set_delay(Duration::from_secs(10));
    let manager = rig.h.manager.clone();
    let checking = tokio::spawn(async move { call(&manager, "m1").await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(rig.checks()[0].checking);

    // D fails on m2 meanwhile, with an error of the model's own.
    rig.h.manager.mark_result(&CallResult {
        model: "m2".into(),
        error: Some(AuthError {
            code: "server_error".into(),
            message: "boom".into(),
            retryable: true,
            http_status: 500,
        }),
        ..outcome(false, d, None, false)
    });

    // C's success is recovery: the credential's rest is over.
    assert!(checking.await.expect("join"));
    assert_eq!(rig.checks(), []);
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

// Not upstream's: a credential removed, or registered again, takes its
// capped rests with it, so none is left to hold back the next one's calls.
#[tokio::test(start_paused = true)]
async fn a_removed_or_replaced_credential_forgets_its_rest() {
    let rig = Rig::new(HOUR, &[MODEL]);
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    assert_eq!(rig.checks().len(), 1);
    rig.h.manager.remove("a");
    assert_eq!(rig.checks(), []);

    rig.h.add(auth("a", "claude"), &[MODEL]);
    assert_eq!(rig.checks(), []);
    assert!(!rig.call(MODEL).await);
    assert_eq!(rig.checks().len(), 1);
    rig.h.add(auth("a", "claude"), &[MODEL]);
    assert_eq!(rig.checks(), []);

    // Its calls aren't held back as checks.
    rig.executor.set_delay(Duration::from_secs(10));
    rig.answer(Answer::Ok);
    let manager = rig.h.manager.clone();
    let first = tokio::spawn(async move { call(&manager, MODEL).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(rig.call(MODEL).await, "a second call at once");
    assert!(first.await.expect("join"));
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

// Not upstream's: a restore that makes the file the authority forgets the
// capped rests with the cooldowns, so a stale one can't fire later on a
// model that was cleared.
#[tokio::test(start_paused = true)]
async fn an_authoritative_restore_forgets_the_old_rests() {
    let rig = Rig::new(HOUR, &[MODEL]);
    attach(&rig, &RecordingStore::with_load(Vec::new()));
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    assert_eq!(rig.checks().len(), 1);

    // The file holds nothing: the other process lifted the rest.
    thaw(&rig.h.manager, Thaw::Restore);
    assert_eq!(rig.checks(), [], "the old rest outlived the file");
    assert_eq!(rig.retry_at(MODEL), None);
    rig.h.clock.advance(HOUR + MINUTE);
    rig.answer(Answer::Ok);
    assert!(rig.call(MODEL).await, "the model stays available");
    assert_eq!(rig.checks(), []);
}

// Not upstream's: the rests of the file replace those held, so a snapshot
// with another schedule for the same key installs its own.
#[tokio::test(start_paused = true)]
async fn an_authoritative_restore_installs_the_snapshots_schedule() {
    let store = RecordingStore::with_load(Vec::new());
    let rig = Rig::new(HOUR, &[MODEL]);
    attach(&rig, &store);
    let reset = rig.at(5 * DAY);
    rig.answer(Answer::Quota(reset, false));
    assert!(!rig.call(MODEL).await);
    flush(&rig.h.manager);
    let mut saved = store.saved();
    let sooner = rig.at(30 * MINUTE);
    for record in &mut saved {
        if record.model == MODEL {
            record.next_retry_after = Some(sooner);
            record.quota.next_recover_at = Some(sooner);
            record.quota_check = Some(QuotaCheckRecord {
                wait: 30 * MINUTE,
                check_at: sooner,
                reset_at: reset,
            });
        }
    }

    install_store(&rig.h.manager, RecordingStore::with_load(saved));
    thaw(&rig.h.manager, Thaw::Restore);
    assert_eq!(rig.checks(), [check(MODEL, sooner, reset, 30 * MINUTE)]);
    assert_eq!(rig.retry_at(MODEL), Some(sooner));
}

/// Has a call of `MODEL` start on a thread of its own at the point of an
/// authoritative restore between its clear and its import, and waits a
/// while for it. The call needs the manager's state lock: a restore that
/// lets go of the lock between its two steps lets the call through, and its
/// result is recorded in between; one that holds the lock keeps the call
/// waiting until it is done, and the wait runs out. The thread is not joined
/// there, for it would never finish when the lock is held; the test joins it
/// after the restore.
fn call_between_the_steps(rig: &Rig) -> Arc<Mutex<Vec<std::thread::JoinHandle<bool>>>> {
    let started = Arc::new(Mutex::new(Vec::new()));
    let (manager, list) = (rig.h.manager.clone(), started.clone());
    set_restore_hooks(
        &rig.h.manager,
        RestoreHooks {
            between: Box::new(move || {
                // The restore holds the state lock for both steps.
                assert!(
                    matches!(
                        manager.shared.state.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ),
                    "the state lock was let go between the clear and the import"
                );
                let (done, finished) = std::sync::mpsc::channel();
                let manager = manager.clone();
                let handle = std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime");
                    let reached = runtime.block_on(call(&manager, MODEL));
                    let _ = done.send(());
                    reached
                });
                let _ = finished.recv_timeout(Duration::from_millis(500));
                list.lock().expect("threads").push(handle);
            }),
            applied: Box::new(|_| {}),
            before_enable: Box::new(|| {}),
        },
    );
    started
}

// Not upstream's: an authoritative restore clears and imports as one step for
// request results. A call that gets its result recorded between the two
// finds the quota cleared, reaches the provider and rests the model again on
// the cap's schedule, which the import then keeps over the snapshot's. Here
// the call waits for the restore, finds the model cooling and is held back,
// so the file's rest is the one saved.
#[tokio::test(start_paused = true)]
async fn a_result_cannot_get_between_the_clear_and_the_import_of_a_restore() {
    let rig = Rig::new(HOUR, &[MODEL]);
    let reset = rig.at(5 * DAY);
    let sooner = rig.at(30 * MINUTE);
    rig.answer(Answer::Quota(reset, false));
    let first = RecordingStore::with_load(Vec::new());
    attach(&rig, &first);
    assert!(!rig.call(MODEL).await);
    flush(&rig.h.manager);
    let mut saved = first.saved();
    for record in &mut saved {
        if record.model == MODEL {
            record.next_retry_after = Some(sooner);
            record.quota.next_recover_at = Some(sooner);
            record.quota_check = Some(QuotaCheckRecord {
                wait: 30 * MINUTE,
                check_at: sooner,
                reset_at: reset,
            });
        }
    }
    let second = RecordingStore::with_load(saved);
    install_store(&rig.h.manager, second.clone());
    let threads = call_between_the_steps(&rig);
    let reached = rig.reached();

    thaw(&rig.h.manager, Thaw::Restore);
    for thread in threads.lock().expect("threads").drain(..) {
        thread.join().expect("the call's thread");
    }
    assert!(restore_hooks_are_gone(&rig.h.manager));
    assert_eq!(rig.reached(), reached, "the call got through the restore");
    assert_eq!(rig.checks(), [check(MODEL, sooner, reset, 30 * MINUTE)]);
    let kept = saved_model(&second.saved());
    assert_eq!(kept.next_retry_after, Some(sooner));
    assert_eq!(kept.quota_check.map(|c| c.check_at), Some(sooner));
}

// Not upstream's: the same step for a call that succeeds. Its result would put
// a healthy time on the model, and the import would take the saved cooldown
// for one that result cleared, and the save would erase it.
#[tokio::test(start_paused = true)]
async fn a_success_cannot_get_between_the_clear_and_the_import_of_a_restore() {
    let rig = Rig::new(HOUR, &[MODEL]);
    let reset = rig.at(5 * DAY);
    let sooner = rig.at(30 * MINUTE);
    rig.answer(Answer::Quota(reset, false));
    let first = RecordingStore::with_load(Vec::new());
    attach(&rig, &first);
    assert!(!rig.call(MODEL).await);
    flush(&rig.h.manager);
    let mut saved = first.saved();
    for record in &mut saved {
        if record.model == MODEL {
            record.next_retry_after = Some(sooner);
            record.quota.next_recover_at = Some(sooner);
            record.quota_check = Some(QuotaCheckRecord {
                wait: 30 * MINUTE,
                check_at: sooner,
                reset_at: reset,
            });
        }
    }
    // A result after the record was saved is newer than it.
    rig.h.clock.advance(MINUTE);
    rig.answer(Answer::Ok);
    let second = RecordingStore::with_load(saved);
    install_store(&rig.h.manager, second.clone());
    let threads = call_between_the_steps(&rig);
    let reached = rig.reached();

    thaw(&rig.h.manager, Thaw::Restore);
    for thread in threads.lock().expect("threads").drain(..) {
        thread.join().expect("the call's thread");
    }
    assert!(restore_hooks_are_gone(&rig.h.manager));
    assert_eq!(rig.reached(), reached, "the call got through the restore");
    assert_eq!(rig.retry_at(MODEL), Some(sooner));
    let kept = saved_model(&second.saved());
    assert_eq!(
        kept.next_retry_after,
        Some(sooner),
        "the cooldown was erased"
    );
}

// Not upstream's: saving is off from the install of a store until its restore
// has ended, and a result recorded after the restore let go of the state lock,
// before saving was on, is not noted. An empty snapshot ended the restore
// without a save or a mark, so the file stayed empty until some later change.
#[tokio::test(start_paused = true)]
async fn an_empty_restore_saves_a_cooldown_recorded_before_saving_was_on() {
    let rig = Rig::new(HOUR, &[MODEL]);
    rig.answer(Answer::Quota(rig.at(5 * DAY), false));
    let store = RecordingStore::with_load(Vec::new());
    install_store(&rig.h.manager, store.clone());
    let manager = rig.h.manager.clone();
    set_restore_hooks(
        &rig.h.manager,
        RestoreHooks {
            between: Box::new(|| {}),
            applied: Box::new(|_| {}),
            before_enable: Box::new(move || {
                // The state lock is free, so the call finishes, and its
                // result is recorded with saving still off, before this
                // returns.
                let manager = manager.clone();
                let reached = std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime");
                    runtime.block_on(call(&manager, MODEL))
                })
                .join()
                .expect("the call's thread");
                assert!(!reached, "the call was served");
            }),
        },
    );

    thaw(&rig.h.manager, Thaw::Restore);
    assert!(restore_hooks_are_gone(&rig.h.manager));
    flush(&rig.h.manager);
    let kept = saved_model(&store.saved());
    assert!(
        kept.next_retry_after.is_some(),
        "the cooldown was not saved"
    );
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
