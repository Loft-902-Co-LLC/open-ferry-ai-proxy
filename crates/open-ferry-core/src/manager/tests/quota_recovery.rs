//! Not upstream's: a confirmed upstream usage reset ends a stale quota
//! cooldown (issue #8, see `quota_recovery`).
//!
//! The manager runs against a fake executor, and a usage reading is handed to
//! it as the management handler would hand one over; nothing here contacts a
//! provider. The tests cover what is seen through dispatch, and the
//! interplay with the capped rests and the saved cooldown state.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::TimeDelta;
use http::{HeaderMap, HeaderName, HeaderValue};

use super::cooldown_state_store::{RecordingStore, files, temp_dir};
use super::support::*;
use crate::auth::{Auth, AuthError, Timestamp};
use crate::config::Config;
use crate::exec::{Dispatcher, ExecError, Response};
use crate::manager::cooldown_store::{
    FileStore, StateStore, flush, install_store, reconfigure, restore, restore_now, set_debounce,
    snapshot,
};
use crate::manager::select::is_auth_blocked_for_model;
use crate::manager::{CallResult, Settings, UsageReading, UsageWindow, UsageWindowReading};

const ID: &str = "claude-recovery";
const SONNET: &str = "claude-3-5-sonnet-20241022";
const OPUS: &str = "claude-3-opus-20240229";
const HOUR: Duration = Duration::from_secs(3600);
const SEVEN_DAYS: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const FIVE_DAYS: Duration = Duration::from_secs(5 * 24 * 60 * 60);

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

/// The headers of a 429 that rejects the 7-day window.
fn weekly(now: Timestamp) -> HeaderMap {
    let reset = (now + TimeDelta::days(7)).timestamp().to_string();
    headers(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-7d-status", "rejected"),
        ("anthropic-ratelimit-unified-7d-reset", &reset),
        ("anthropic-ratelimit-unified-7d-utilization", "1.0"),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "seven_day",
        ),
    ])
}

/// The headers of a 429 of one model alone, rejecting its 7-day window.
fn model_headers(claim: &str) -> HeaderMap {
    headers(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-status", "rejected"),
        ("anthropic-ratelimit-unified-representative-claim", claim),
    ])
}

fn opus_headers() -> HeaderMap {
    model_headers("seven_day_opus")
}

fn sonnet_headers() -> HeaderMap {
    model_headers("seven_day_sonnet")
}

fn rejection(response_headers: HeaderMap, credential: bool, retry: Duration) -> ExecError {
    let mut err = ExecError::upstream(429, "rate limit rejected");
    err.headers = response_headers;
    err.retry_after = Some(retry);
    err.credential_scoped = credential;
    err
}

fn failure(model: &str, response_headers: HeaderMap, credential: bool) -> CallResult {
    CallResult {
        auth_id: ID.into(),
        provider: "claude".into(),
        model: model.into(),
        success: false,
        retry_after: Some(SEVEN_DAYS),
        credential_scope: credential,
        response_headers,
        error: Some(AuthError {
            http_status: 429,
            message: "rate limit rejected".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    }
}

/// A usage reading answered `now` with the 5-hour and 7-day windows healthy,
/// plus the `extra` windows at the given percent.
fn reading(now: Timestamp, extra: &[(UsageWindow, f64)]) -> UsageReading {
    let mut windows = vec![(UsageWindow::FiveHour, 4.0), (UsageWindow::SevenDay, 9.0)];
    windows.extend_from_slice(extra);
    UsageReading {
        observed_at: Some(now),
        windows: windows
            .into_iter()
            .map(|(window, percent)| {
                (
                    window,
                    UsageWindowReading {
                        utilization_percent: percent,
                        resets_at: Some(now + TimeDelta::days(1)),
                    },
                )
            })
            .collect(),
    }
}

async fn call(h: &Harness, model: &str) -> Result<Response, ExecError> {
    h.manager
        .execute(&providers(&["claude"]), request(model), options())
        .await
}

fn blocked(h: &Harness, model: &str) -> bool {
    is_auth_blocked_for_model(&h.get(ID), model, h.now()).0
}

/// What applying `reading` against a token taken now ended, in total.
fn recover(h: &Harness, reading: &UsageReading) -> bool {
    let token = h.manager.quota_recovery_token(ID).expect("token");
    !h.manager
        .apply_quota_usage_recovery(&token, reading)
        .expect("apply")
        .is_empty()
}

/// The models of the capped rests of the credential, sorted.
fn rested(h: &Harness) -> Vec<String> {
    let mut models: Vec<String> = h
        .manager
        .quota_checks(ID)
        .into_iter()
        .map(|check| check.model_key)
        .collect();
    models.sort();
    models
}

/// A cooldown ended by a usage read lets the next generation call through
/// to the provider, and a later real 429 blocks the credential again.
#[tokio::test(start_paused = true)]
async fn a_recovered_credential_is_dispatched_again_and_a_new_429_blocks_it_again() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[SONNET]);
    let rejecting = Arc::new(AtomicBool::new(true));
    let flag = rejecting.clone();
    let clock = h.clock.clock();
    let executor = FakeExecutor::with("claude", move |_: &Call| {
        if flag.load(Ordering::SeqCst) {
            Reply::Err(rejection(weekly(clock()), true, SEVEN_DAYS))
        } else {
            Reply::ok("generated")
        }
    });
    h.executor(&executor);

    assert!(call(&h, SONNET).await.is_err());
    settle().await;
    assert!(blocked(&h, SONNET), "the 429 must block the credential");
    assert!(call(&h, SONNET).await.is_err());
    assert_eq!(
        executor.ids(Kind::Execute).len(),
        1,
        "the cooldown must hold the second call back"
    );

    rejecting.store(false, Ordering::SeqCst);
    h.clock.advance(Duration::from_secs(5));
    assert!(recover(&h, &reading(h.now(), &[])));
    let response = call(&h, SONNET)
        .await
        .expect("a call after the recovery must reach the provider");
    assert_eq!(&response.payload[..], b"generated");
    assert_eq!(executor.ids(Kind::Execute).len(), 2);

    rejecting.store(true, Ordering::SeqCst);
    assert!(call(&h, SONNET).await.is_err());
    settle().await;
    assert!(blocked(&h, SONNET), "a new 429 must block it again");
    assert!(call(&h, SONNET).await.is_err());
    assert_eq!(
        executor.ids(Kind::Execute).len(),
        3,
        "the new cooldown must hold the next call back"
    );
}

/// A model's own limit, capped to its next check, stays under a
/// credential-wide cooldown when a read proves the credential: the recovery is
/// refused while any model of the credential rests on a quota check, and
/// changes nothing. The wrap then runs out by itself, and Opus's due check lets
/// exactly one call through.
#[tokio::test(start_paused = true)]
async fn a_capped_model_layer_refuses_the_credentials_recovery() {
    let h = Harness::new(Settings {
        quota_check_after: HOUR,
        ..Settings::default()
    });
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    let sonnet_ok = Arc::new(AtomicBool::new(false));
    let flag = sonnet_ok.clone();
    let clock = h.clock.clock();
    let executor = FakeExecutor::with("claude", move |call: &Call| {
        if call.model == OPUS {
            Reply::Err(rejection(opus_headers(), false, FIVE_DAYS))
        } else if flag.load(Ordering::SeqCst) {
            Reply::ok("generated")
        } else {
            Reply::Err(rejection(weekly(clock()), true, FIVE_DAYS))
        }
    });
    h.executor(&executor);

    assert!(call(&h, OPUS).await.is_err());
    settle().await;
    assert_eq!(rested(&h), [OPUS], "opus rests at the cap");
    h.clock.advance(Duration::from_secs(30 * 60));
    assert!(call(&h, SONNET).await.is_err());
    settle().await;
    assert!(blocked(&h, OPUS) && blocked(&h, SONNET));
    // Opus's capped check time is past; the wrap's is not.
    h.clock.advance(Duration::from_secs(50 * 60));

    let before = shape(&h);
    let rests = rested(&h);
    let weekly_only = reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)]);
    assert!(
        !recover(&h, &weekly_only),
        "a credential recovery went ahead with a model resting on a check"
    );
    assert_eq!(shape(&h), before, "a refused recovery changed something");
    assert!(blocked(&h, SONNET), "the wrap stays");
    assert_eq!(rested(&h), rests, "the rests are as they were");
    assert!(
        rests.iter().any(|scope| scope == OPUS),
        "opus keeps its rest"
    );

    // The wrap's own check time is next: past it, Opus's due check lets one
    // call through, and the wrap is gone by itself.
    h.clock.advance(Duration::from_secs(15 * 60));
    sonnet_ok.store(true, Ordering::SeqCst);
    assert!(call(&h, SONNET).await.is_ok());
    assert_eq!(reached_by_two_calls(&h).await, 1, "one check proceeds");
}

/// The same refusal before opus's check is due, in memory and after a restart:
/// nothing changes, the flushed record equals what memory holds, and the
/// restored manager (which has no claim to recover against) changes nothing
/// either.
#[tokio::test(start_paused = true)]
async fn a_capped_model_rest_refuses_the_credentials_recovery_in_memory_and_after_a_restart() {
    let dir = temp_dir();
    let settings = Settings {
        quota_check_after: HOUR,
        ..Settings::default()
    };
    let h = saving(dir.path(), settings.clone(), &[OPUS, SONNET]);
    let clock = h.clock.clock();
    let executor = FakeExecutor::with("claude", move |call: &Call| {
        if call.model == OPUS {
            Reply::Err(rejection(opus_headers(), false, FIVE_DAYS))
        } else {
            Reply::Err(rejection(weekly(clock()), true, FIVE_DAYS))
        }
    });
    h.executor(&executor);
    assert!(call(&h, OPUS).await.is_err());
    settle().await;
    h.clock.advance(Duration::from_secs(30 * 60));
    assert!(call(&h, SONNET).await.is_err());
    settle().await;
    h.clock.advance(Duration::from_secs(10 * 60));
    assert!(blocked(&h, OPUS) && blocked(&h, SONNET));

    let opus = |h: &Harness| {
        let state = h.get(ID).model_states.get(OPUS).cloned().expect("opus");
        (
            state.next_retry_after,
            state.quota.next_recover_at,
            format!("{:?}", h.manager.quota_checks(ID)),
        )
    };
    let before = shape(&h);
    let live = opus(&h);
    let weekly_only = reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)]);
    assert!(!recover(&h, &weekly_only));
    assert_eq!(shape(&h), before, "a refused recovery changed something");
    assert!(blocked(&h, OPUS) && blocked(&h, SONNET), "the wrap stays");
    assert_eq!(opus(&h), live);

    flush(&h.manager);
    let next = Harness::new(settings);
    next.add(credential(dir.path()), &[OPUS, SONNET]);
    next.clock
        .advance((h.now() - next.now()).to_std().unwrap_or_default());
    let on = config(dir.path(), true);
    reconfigure(&next.manager, None, &on);
    restore(&next.manager, &on);
    // A restore puts the wrap's own deadline back under opus (the cap is not
    // stored), as it did before any recovery existed; the rests are memory's.
    assert_eq!(opus(&next).2, live.2, "the restored rests are not memory's");
    assert!(blocked(&next, OPUS) && blocked(&next, SONNET));
    let restored = shape(&next);
    assert!(!recover(&next, &weekly_only));
    assert_eq!(shape(&next), restored, "a read changed the restored state");

    // Past the wrap's own check time both run alike.
    h.clock.advance(Duration::from_secs(55 * 60));
    next.clock.advance(Duration::from_secs(55 * 60));
    assert_eq!(
        reached_by_two_calls(&h).await,
        reached_by_two_calls(&next).await,
        "memory and a restart disagree on the due check"
    );
}

/// Ending one model's cooldown leaves the capped rest of another, even when
/// its capped time has passed.
#[tokio::test(start_paused = true)]
async fn another_models_capped_rest_stays_when_one_model_recovers() {
    let h = Harness::new(Settings {
        quota_check_after: HOUR,
        ..Settings::default()
    });
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    let executor = FakeExecutor::with("claude", |call: &Call| {
        let headers = if call.model == OPUS {
            opus_headers()
        } else {
            sonnet_headers()
        };
        Reply::Err(rejection(headers, false, FIVE_DAYS))
    });
    h.executor(&executor);

    assert!(call(&h, OPUS).await.is_err());
    assert!(call(&h, SONNET).await.is_err());
    settle().await;
    assert_eq!(rested(&h), [SONNET, OPUS]);
    // Both capped times are past, with the limits still standing.
    h.clock.advance(2 * HOUR);

    let sonnet_only = reading(
        h.now(),
        &[
            (UsageWindow::SevenDayOpus, 100.0),
            (UsageWindow::SevenDaySonnet, 3.0),
        ],
    );
    assert!(recover(&h, &sonnet_only));
    assert_eq!(rested(&h), [OPUS], "opus keeps its rest");
    assert!(h.get(ID).model_states[OPUS].quota.exceeded);
}

/// A cooldown put back from the saved state is not one a claim from before
/// speaks for.
#[tokio::test(start_paused = true)]
async fn a_restored_cooldown_is_not_ended_by_an_earlier_claim() {
    let saved = Harness::new(Settings::default());
    saved.add(auth(ID, "claude"), &[SONNET]);
    saved
        .manager
        .mark_result(&failure(SONNET, sonnet_headers(), false));
    let records = snapshot(&saved.manager, saved.now());
    assert!(!records.is_empty());

    // Control: the same 429 with nothing restored is ended by a read.
    let control = Harness::new(Settings::default());
    control.add(auth(ID, "claude"), &[SONNET]);
    control
        .manager
        .mark_result(&failure(SONNET, sonnet_headers(), false));
    let healthy = [(UsageWindow::SevenDaySonnet, 3.0)];
    assert!(recover(&control, &reading(control.now(), &healthy)));

    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[SONNET]);
    h.manager
        .mark_result(&failure(SONNET, sonnet_headers(), false));
    install_store(&h.manager, RecordingStore::with_load(records));
    restore_now(&h.manager);
    assert!(blocked(&h, SONNET));
    assert!(
        !recover(&h, &reading(h.now(), &healthy)),
        "a restored cooldown ended by a claim from before it"
    );
    assert!(blocked(&h, SONNET));
}

fn config(dir: &Path, on: bool) -> Config {
    Config {
        auth_dir: dir.to_string_lossy().into_owned(),
        save_cooldown_status: on,
        ..Config::default()
    }
}

fn credential(dir: &Path) -> Auth {
    let mut credential = auth(ID, "claude");
    credential.attributes.insert(
        "path".into(),
        dir.join("claude.json").to_string_lossy().into_owned(),
    );
    credential
}

/// Saving the cooldowns off, a recovery, and saving on again must not bring
/// back, from the file kept as it was, the cooldown that was ended: neither
/// the model's own record nor the credential's gathered one.
#[tokio::test(start_paused = true)]
async fn turning_saving_off_and_on_does_not_bring_back_an_ended_cooldown() {
    let dir = temp_dir();
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    h.manager
        .register_unsaved(credential(dir.path()))
        .expect("register");
    let on = config(dir.path(), true);
    let off = config(dir.path(), false);
    reconfigure(&h.manager, None, &on);
    restore(&h.manager, &on);

    h.manager.mark_result(&failure(OPUS, opus_headers(), false));
    flush(&h.manager);
    let saved = FileStore::new(dir.path().to_path_buf())
        .load()
        .expect("load");
    assert!(
        saved
            .iter()
            .any(|record| record.model.trim().is_empty() && record.quota.reason == "quota"),
        "the credential's gathered record is saved: {saved:?}"
    );
    assert!(saved.iter().any(|record| !record.model.trim().is_empty()));

    reconfigure(&h.manager, Some(&on), &off);
    h.clock.advance(Duration::from_secs(5));
    assert!(recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(!blocked(&h, OPUS));
    let current = h.get(ID);
    assert!(!current.unavailable && !current.quota.exceeded);

    // Saving on again reads the file as it was left.
    h.clock.advance(Duration::from_secs(5));
    reconfigure(&h.manager, Some(&off), &on);
    restore(&h.manager, &on);
    assert!(!blocked(&h, OPUS), "the model's record came back");
    let current = h.get(ID);
    assert!(
        !current.unavailable && !current.quota.exceeded,
        "the gathered record came back: {:?}",
        current.quota
    );
    flush(&h.manager);
    assert!(files(dir.path()).is_empty(), "{:?}", files(dir.path()));
}

/// A failure with no recognized quota headers: a 503, or a 429 that names no
/// model (`model` empty), holding the scope for `retry`.
fn other_failure(model: &str, status: u16, retry: Duration) -> CallResult {
    CallResult {
        auth_id: ID.into(),
        provider: "claude".into(),
        model: model.into(),
        success: false,
        retry_after: Some(retry),
        error: Some(AuthError {
            http_status: status,
            message: "not a recognized quota".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    }
}

/// A recognized quota 429 of Opus, then an independent 429 that names no
/// model and holds the whole credential longer: the model's recovery is
/// refused while the unproven hold lives, and changes nothing.
#[tokio::test(start_paused = true)]
async fn an_independent_no_model_429_refuses_a_model_recovery() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager.mark_result(&failure(OPUS, opus_headers(), false));
    h.manager
        .mark_result(&other_failure("", 429, 2 * SEVEN_DAYS));
    let held = h.get(ID);
    assert!(held.quota.exceeded && held.unavailable, "{:?}", held.quota);
    let until = held.quota.next_recover_at;

    h.clock.advance(Duration::from_secs(5));
    let healthy = [(UsageWindow::SevenDayOpus, 2.0)];
    // Was: recovered, and the model came free.
    assert!(!recover(&h, &reading(h.now(), &healthy)));
    assert!(blocked(&h, OPUS), "the model stays in cooldown");
    let current = h.get(ID);
    assert!(
        current.quota.exceeded,
        "the independent hold was removed without a claim: {:?}",
        current.quota
    );
    assert_eq!(current.quota.next_recover_at, until);
}

/// The same, with the cooldown storage turned off, the refused read, and
/// turned on again: the credential's independent record is not taken for a
/// derived one.
#[tokio::test(start_paused = true)]
async fn turning_saving_off_and_on_keeps_an_independent_no_model_hold() {
    let dir = temp_dir();
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    h.manager
        .register_unsaved(credential(dir.path()))
        .expect("register");
    let on = config(dir.path(), true);
    let off = config(dir.path(), false);
    reconfigure(&h.manager, None, &on);
    restore(&h.manager, &on);

    h.manager.mark_result(&failure(OPUS, opus_headers(), false));
    h.manager
        .mark_result(&other_failure("", 429, 2 * SEVEN_DAYS));
    flush(&h.manager);

    reconfigure(&h.manager, Some(&on), &off);
    h.clock.advance(Duration::from_secs(5));
    // Was: recovered.
    assert!(!recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(h.get(ID).quota.exceeded, "recovery took the hold");

    h.clock.advance(Duration::from_secs(5));
    reconfigure(&h.manager, Some(&off), &on);
    restore(&h.manager, &on);
    let current = h.get(ID);
    assert!(
        current.quota.exceeded,
        "the independent hold is gone after the restore: {:?}",
        current.quota
    );
}

/// A 503 left a foreign hold; a manual reset ended it with everything else.
/// A recognized quota 429 after it is the only thing on the model.
#[tokio::test(start_paused = true)]
async fn a_reset_forgets_an_earlier_foreign_hold() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager
        .mark_result(&other_failure(OPUS, 503, Duration::from_secs(600)));
    assert!(blocked(&h, OPUS));
    h.manager.reset_quota(ID).expect("reset");
    assert!(!blocked(&h, OPUS));

    h.manager.mark_result(&failure(OPUS, opus_headers(), false));
    h.clock.advance(Duration::from_secs(5));
    assert!(
        recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "a hold the reset ended still taints the new claim"
    );
    assert!(!blocked(&h, OPUS));
}

/// A 503 hold ended on its own clock stops blocking the claim that came
/// after it, though the model's merged deadline still reads the quota's.
#[tokio::test(start_paused = true)]
async fn an_expired_foreign_hold_stops_blocking_a_claim() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager
        .mark_result(&other_failure(OPUS, 503, Duration::from_secs(600)));
    h.manager.mark_result(&failure(OPUS, opus_headers(), false));
    let healthy = [(UsageWindow::SevenDayOpus, 2.0)];

    h.clock.advance(Duration::from_secs(5 * 60));
    assert!(
        !recover(&h, &reading(h.now(), &healthy)),
        "the 503 hold is still running"
    );
    assert!(blocked(&h, OPUS));

    h.clock.advance(Duration::from_secs(6 * 60));
    assert!(
        recover(&h, &reading(h.now(), &healthy)),
        "the 503 hold ended; only the quota is left"
    );
    assert!(!blocked(&h, OPUS));
}

/// The headers of a 429 that rejects the 5-hour window alone.
fn five_hour_headers() -> HeaderMap {
    headers(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "five_hour",
        ),
    ])
}

/// A recognized quota 429 of `model` holding it for `retry`.
fn quota_429(model: &str, response_headers: HeaderMap, retry: Duration) -> CallResult {
    CallResult {
        retry_after: Some(retry),
        ..failure(model, response_headers, false)
    }
}

/// The 503 comes after the quota 429 and keeps the 7-day deadline: its own
/// contribution is its own 10 minutes, not the merged one. The 429 after it
/// names another window, so the first one's stays owed.
#[tokio::test(start_paused = true)]
async fn a_foreign_hold_counts_its_own_deadline_not_the_merged_one() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager.mark_result(&failure(OPUS, opus_headers(), false));
    h.clock.advance(Duration::from_secs(10));
    h.manager
        .mark_result(&other_failure(OPUS, 503, Duration::from_secs(600)));
    h.clock.advance(Duration::from_secs(10));
    h.manager
        .mark_result(&quota_429(OPUS, five_hour_headers(), HOUR));
    let healthy = [(UsageWindow::SevenDayOpus, 2.0)];

    h.clock.advance(Duration::from_secs(5 * 60));
    assert!(!recover(&h, &reading(h.now(), &healthy)));
    h.clock.advance(Duration::from_secs(6 * 60));
    let opus_full = [(UsageWindow::SevenDayOpus, 100.0)];
    assert!(
        !recover(&h, &reading(h.now(), &opus_full)),
        "the earlier Opus window is still owed"
    );
    assert!(blocked(&h, OPUS));
    assert!(
        recover(&h, &reading(h.now(), &healthy)),
        "the 503 ended 11 minutes in; a 7-day merged deadline blocks the claim"
    );
}

/// A 503 between two recognized 429s of a model must not drop the windows the
/// first one rejected: the second names another window, and a read that
/// shows the first still full must not end the longer cooldown.
#[tokio::test(start_paused = true)]
async fn an_intervening_503_keeps_an_earlier_window_owed() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(10));
    h.manager
        .mark_result(&other_failure(OPUS, 503, Duration::from_secs(600)));
    h.clock.advance(Duration::from_secs(10));
    h.manager
        .mark_result(&quota_429(OPUS, five_hour_headers(), HOUR));

    h.clock.advance(Duration::from_secs(11 * 60));
    let opus_full = [(UsageWindow::SevenDayOpus, 100.0)];
    assert!(
        !recover(&h, &reading(h.now(), &opus_full)),
        "the five-day Opus cooldown was cleared by a read showing Opus full"
    );
    assert!(blocked(&h, OPUS));
    assert!(recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(!blocked(&h, OPUS));
}

/// The same with a 429 no usage read can speak for between them. Its length
/// is hidden behind the longer cooldown, so it stays unknown and fails
/// closed whatever the read shows.
#[tokio::test(start_paused = true)]
async fn an_unrecognized_429_between_keeps_windows_owed_and_fails_closed() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(10));
    h.manager
        .mark_result(&other_failure(OPUS, 429, Duration::from_secs(600)));
    h.clock.advance(Duration::from_secs(10));
    h.manager
        .mark_result(&quota_429(OPUS, five_hour_headers(), HOUR));

    h.clock.advance(Duration::from_secs(11 * 60));
    let healthy = [(UsageWindow::SevenDayOpus, 2.0)];
    assert!(
        !recover(&h, &reading(h.now(), &healthy)),
        "a hold of unknown length was taken for an ended one"
    );
    assert!(blocked(&h, OPUS));
}

/// An unrecognized 429 holding a model for seven days, recorded under a 1 hour
/// check cap, is still a seven-day foreign hold: a recognized 429 after it
/// and a healthy read after the check time must not end the unproven rest.
#[tokio::test(start_paused = true)]
async fn a_capped_foreign_hold_keeps_its_provider_deadline() {
    let h = Harness::new(Settings {
        quota_check_after: HOUR,
        ..Settings::default()
    });
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager.mark_result(&other_failure(OPUS, 429, SEVEN_DAYS));
    h.clock.advance(Duration::from_secs(10));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), SEVEN_DAYS));

    h.clock.advance(Duration::from_secs(65 * 60));
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "the 1 hour check time was taken for the end of a 7 day hold"
    );
    assert!(h.get(ID).model_states[OPUS].quota.exceeded);
}

/// The selection a request that names no model makes of the credential.
fn no_model_blocked(h: &Harness) -> bool {
    is_auth_blocked_for_model(&h.get(ID), "", h.now()).0
}

/// A 429 naming no model holds the credential for 14 days; a recognized 429
/// of Opus comes after it, and a read that would end Opus's is refused. The
/// independent hold keeps a request that names no model off the credential,
/// and the saved cooldowns still carry it into a new manager.
#[tokio::test(start_paused = true)]
async fn an_independent_no_model_hold_still_blocks_selection_and_storage_after_a_refused_model_read()
 {
    let dir = temp_dir();
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    h.manager
        .register_unsaved(credential(dir.path()))
        .expect("register");
    let on = config(dir.path(), true);
    reconfigure(&h.manager, None, &on);
    restore(&h.manager, &on);

    h.manager
        .mark_result(&other_failure("", 429, 2 * SEVEN_DAYS));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    assert!(no_model_blocked(&h));

    h.clock.advance(Duration::from_secs(5));
    // Was: recovered, and Opus came free.
    assert!(!recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(blocked(&h, OPUS), "the model stays in cooldown");
    let current = h.get(ID);
    assert!(
        no_model_blocked(&h),
        "selection picks a credential held for 14 days: unavailable {} next retry {:?} quota {:?}",
        current.unavailable,
        current.next_retry_after,
        current.quota
    );

    flush(&h.manager);
    let saved = FileStore::new(dir.path().to_path_buf())
        .load()
        .expect("load");
    assert!(
        saved.iter().any(|record| record.model.trim().is_empty()),
        "the independent hold is not saved: {saved:?}"
    );

    let next = Harness::new(Settings::default());
    next.manager
        .register_unsaved(credential(dir.path()))
        .expect("register");
    reconfigure(&next.manager, None, &on);
    restore(&next.manager, &on);
    assert!(
        no_model_blocked(&next),
        "the restored credential is selectable: {:?}",
        next.get(ID).quota
    );
}

/// A header edit between the token and the apply leaves the version of the
/// credential alone, yet the read was made with the old header.
#[tokio::test(start_paused = true)]
async fn a_request_shaping_edit_fences_a_read_in_flight() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    let token = h.manager.quota_recovery_token(ID).expect("token");
    h.clock.advance(Duration::from_secs(5));
    let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
    assert!(token.matches(&h.get(ID)));

    h.manager
        .edit(ID, |auth| {
            auth.attributes
                .insert("header:X-Team".into(), "green".into());
            Ok::<_, ()>(true)
        })
        .expect("edit");
    assert!(
        h.manager
            .apply_quota_usage_recovery(&token, &healthy)
            .expect("apply")
            .is_empty(),
        "a read made before a header edit ended a cooldown"
    );
    assert!(blocked(&h, OPUS));
    assert!(!token.matches(&h.get(ID)));
}

/// Claude's executor picks its authentication headers from the credential's
/// `auth_kind` metadata (and which token fields it holds), so an edit of
/// those fences a read in flight; a label, a counter or a refreshed expiry
/// does not.
#[tokio::test]
async fn an_identity_metadata_edit_fences_a_read_in_flight_and_a_label_does_not() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.manager
        .edit(ID, |auth| {
            auth.metadata
                .insert("expires_at".into(), "2029-01-01T00:00:00Z".into());
            Ok::<_, ()>(true)
        })
        .expect("edit");
    let token = h.manager.quota_recovery_token(ID).expect("token");

    for edit in 0..3 {
        h.manager
            .edit(ID, |auth| {
                match edit {
                    0 => auth.label = "renamed".into(),
                    1 => {
                        auth.metadata.insert("request_count".into(), 7.into());
                    }
                    _ => {
                        auth.metadata
                            .insert("expires_at".into(), "2030-01-01T00:00:00Z".into());
                    }
                }
                Ok::<_, ()>(true)
            })
            .expect("edit");
        assert!(
            token.matches(&h.get(ID)),
            "unrelated edit {edit} fenced a read"
        );
    }

    for (key, value) in [("auth_kind", "apikey"), ("access_token", "swapped")] {
        let fresh = {
            let h = Harness::new(Settings::default());
            h.add(auth(ID, "claude"), &[OPUS]);
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h
        };
        let token = fresh.manager.quota_recovery_token(ID).expect("token");
        fresh.clock.advance(Duration::from_secs(5));
        let healthy = reading(fresh.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
        fresh
            .manager
            .edit(ID, |auth| {
                auth.metadata.insert(key.into(), value.into());
                Ok::<_, ()>(true)
            })
            .expect("edit");
        assert!(
            fresh
                .manager
                .apply_quota_usage_recovery(&token, &healthy)
                .expect("apply")
                .is_empty(),
            "a read made before a {key} edit ended a cooldown"
        );
        assert!(blocked(&fresh, OPUS));
        assert!(!token.matches(&fresh.get(ID)), "{key} edit did not fence");
    }
}

/// Another model's outcome between the token and the apply neither fences
/// this model's read nor changes what the token stands for.
#[tokio::test(start_paused = true)]
async fn an_unrelated_models_outcome_leaves_a_read_in_flight_valid() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    let token = h.manager.quota_recovery_token(ID).expect("token");
    h.clock.advance(Duration::from_secs(5));
    h.manager
        .mark_result(&quota_429(SONNET, sonnet_headers(), FIVE_DAYS));
    assert!(token.matches(&h.get(ID)));
    let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
    assert!(
        !h.manager
            .apply_quota_usage_recovery(&token, &healthy)
            .expect("apply")
            .is_empty()
    );
    assert!(!blocked(&h, OPUS));
    assert!(blocked(&h, SONNET));
}

/// A clock that reads true until its call number `stale_on` (counted from the
/// first call once armed), which it and every later one answers 5 minutes on.
fn aging_clock(
    clock: &TestClock,
    armed: &Arc<AtomicBool>,
    calls: &Arc<AtomicUsize>,
    stale_on: usize,
) -> crate::manager::Clock {
    let (clock, armed, calls) = (clock.clone(), armed.clone(), calls.clone());
    Arc::new(move || {
        if !armed.load(Ordering::SeqCst) {
            return clock.now();
        }
        let call = calls.fetch_add(1, Ordering::SeqCst);
        if call >= stale_on {
            clock.now() + TimeDelta::minutes(5)
        } else {
            clock.now()
        }
    })
}

/// A reading that ages past its 120 seconds while the apply waits (for the
/// mutation gate, or for the state lock) recovers nothing. The clock answers
/// fresh up to a given call and stale after, so the apply must get past the
/// checks before it and fail the one it stops at.
#[tokio::test(start_paused = true)]
async fn a_reading_that_ages_while_the_apply_waits_recovers_nothing() {
    for stale_on in [0usize, 1] {
        let test_clock = TestClock::new();
        let armed = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let models = Arc::new(FakeModels::default());
        let h = Harness {
            manager: crate::manager::Manager::with_clock(
                Settings::default(),
                models.clone(),
                None,
                aging_clock(&test_clock, &armed, &calls, stale_on),
            ),
            models,
            store: Arc::new(FakeStore::default()),
            clock: test_clock,
        };
        h.add(auth(ID, "claude"), &[OPUS]);
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
        let token = h.manager.quota_recovery_token(ID).expect("token");
        h.clock.advance(Duration::from_secs(5));
        let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);

        armed.store(true, Ordering::SeqCst);
        let recovery = h
            .manager
            .apply_quota_usage_recovery(&token, &healthy)
            .expect("apply");
        armed.store(false, Ordering::SeqCst);
        assert!(recovery.is_empty(), "stale on call {stale_on} recovered");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            stale_on + 1,
            "the apply stopped before the check under test"
        );
        assert!(blocked(&h, OPUS));
    }
}

const FOURTEEN_DAYS: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// A manager saving cooldowns under `dir`, with the credential registered
/// for `models` and the debounce out of the way.
fn saving(dir: &Path, settings: Settings, models: &[&str]) -> Harness {
    let h = Harness::new(settings);
    set_debounce(&h.manager, Duration::from_secs(3600));
    h.add(credential(dir), models);
    let on = config(dir, true);
    reconfigure(&h.manager, None, &on);
    restore(&h.manager, &on);
    h
}

/// The credential's own saved record (the one naming no model).
fn saved_own(dir: &Path) -> Option<crate::manager::cooldown_store::Record> {
    FileStore::new(dir.to_path_buf())
        .load()
        .expect("load")
        .into_iter()
        .find(|record| record.model.trim().is_empty())
}

/// How many of two calls made while the first is in flight reach the
/// provider: a check that is due lets one through and rests the credential
/// for the rest; with nothing resting it, both go.
async fn reached_by_two_calls(h: &Harness) -> usize {
    let executor = FakeExecutor::with("claude", |_: &Call| Reply::ok("generated"));
    executor.set_delay(Duration::from_secs(10));
    h.executor(&executor);
    let manager = h.manager.clone();
    let first = tokio::spawn(async move {
        manager
            .execute(&providers(&["claude"]), request(OPUS), options())
            .await
            .is_ok()
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = call(h, OPUS).await;
    let reached = executor.ids(Kind::Execute).len();
    first.await.expect("join");
    reached
}

/// An unrecognized no-model 429 holds the credential for 14 days under a one
/// hour check cap; a recognized Opus 429 follows; a read proves Opus, either
/// before the cap's check time or two hours on with no result in between. The
/// read is refused while the 14 day hold lives: the credential's fields, the
/// check rests (the hold's and Opus's), the selection and the saved record
/// stay as they were.
#[tokio::test(start_paused = true)]
async fn a_capped_no_model_hold_is_left_as_it_was_by_a_model_read() {
    for (wait, passed) in [(Duration::from_secs(20 * 60), false), (2 * HOUR, true)] {
        let dir = temp_dir();
        let h = saving(dir.path(), capped_hour(), &[OPUS, SONNET]);
        h.manager
            .mark_result(&other_failure("", 429, FOURTEEN_DAYS));
        let reset = h.now() + TimeDelta::days(14);
        h.clock.advance(Duration::from_secs(10));
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
        assert_eq!(rested(&h), [String::new(), OPUS.to_owned()]);

        h.clock.advance(wait);
        let before = h.get(ID);
        assert!(
            !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
            "passed {passed}: the read recovered under the 14 day hold"
        );
        let current = h.get(ID);
        assert!(
            current.model_states[OPUS].quota.exceeded,
            "passed {passed}: Opus is still held"
        );
        assert_eq!(
            format!("{:?} {:?}", current.quota, current.model_states),
            format!("{:?} {:?}", before.quota, before.model_states),
            "passed {passed}"
        );
        assert_eq!(current.next_retry_after, before.next_retry_after);
        let checks = h.manager.quota_checks(ID);
        assert_eq!(
            checks
                .iter()
                .map(|c| c.model_key.as_str())
                .collect::<Vec<_>>(),
            ["", OPUS],
            "passed {passed}: the check rests: {checks:?}"
        );
        assert_eq!(checks[0].provider_reset_at, reset);
        assert_eq!(checks[0].wait, HOUR);
        assert_eq!(
            no_model_blocked(&h),
            !passed,
            "passed {passed}: selection of the credential (next retry {:?}, quota {:?})",
            current.next_retry_after,
            current.quota
        );
        if passed {
            assert_eq!(
                reached_by_two_calls(&h).await,
                1,
                "a due check lets one call through"
            );
            continue;
        }
        flush(&h.manager);
        let own = saved_own(dir.path()).unwrap_or_else(|| {
            panic!("passed {passed}: the hold is not saved");
        });
        assert!(own.quota.exceeded && own.quota.reason == "quota", "{own:?}");
        assert_eq!(own.quota_check.map(|check| check.reset_at), Some(reset));
    }
}

/// A short no-model 503 hold, then a recognized Opus 429 of 5 days, then a
/// read that proves Opus: refused while the 600 seconds run (the hold is its
/// own 600 seconds, not the model's 5 days), and recovered by a fresh read
/// once they are over.
#[tokio::test(start_paused = true)]
async fn a_short_no_model_hold_is_left_as_it_was_by_a_model_read() {
    let dir = temp_dir();
    let h = saving(dir.path(), Settings::default(), &[OPUS, SONNET]);
    h.manager
        .mark_result(&other_failure("", 503, Duration::from_secs(600)));
    let until = h.now() + TimeDelta::seconds(600);
    h.clock.advance(Duration::from_secs(5));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    let healthy = [(UsageWindow::SevenDayOpus, 2.0)];
    let before = h.get(ID);
    assert!(!recover(&h, &reading(h.now(), &healthy)));

    let current = h.get(ID);
    assert!(blocked(&h, OPUS));
    assert!(no_model_blocked(&h));
    assert_eq!(current.next_retry_after, before.next_retry_after);
    assert_eq!(
        format!("{:?}", current.quota),
        format!("{:?}", before.quota)
    );
    flush(&h.manager);
    let own = saved_own(dir.path()).expect("the credential is saved");
    assert_eq!(own.next_retry_after, before.next_retry_after);

    h.clock.advance(Duration::from_secs(11 * 60));
    assert!(h.now() > until, "the 600 seconds are over");
    assert!(recover(&h, &reading(h.now(), &healthy)));
    assert!(!blocked(&h, OPUS));
    assert!(!no_model_blocked(&h));
    assert!(h.manager.quota_checks(ID).is_empty());
}

/// A model's 1 hour quota hold, then a 2 hour no-model 503, then a read that
/// proves the model: refused while the 2 hours run, and the credential keeps
/// them, in memory and on disk.
#[tokio::test(start_paused = true)]
async fn a_long_no_model_hold_is_left_as_it_was_by_a_model_read() {
    let dir = temp_dir();
    let h = saving(dir.path(), Settings::default(), &[OPUS, SONNET]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), HOUR));
    h.clock.advance(Duration::from_secs(5));
    h.manager.mark_result(&other_failure("", 503, 2 * HOUR));
    let until = h.now() + TimeDelta::hours(2);
    h.clock.advance(Duration::from_secs(5));
    assert!(!recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));

    let current = h.get(ID);
    assert!(blocked(&h, OPUS));
    assert_eq!(
        current.next_retry_after,
        Some(until),
        "the hold changed (quota {:?})",
        current.quota
    );
    flush(&h.manager);
    let own = saved_own(dir.path()).expect("the hold is saved");
    assert_eq!(own.next_retry_after, Some(until));

    h.clock.advance(Duration::from_secs(90 * 60));
    assert!(no_model_blocked(&h), "the 2 hours are not over");
    h.clock.advance(Duration::from_secs(40 * 60));
    assert!(!no_model_blocked(&h));
}

/// A no-model 429 without a deadline hint backs off with its own level. A
/// read is refused while that hold lives and leaves its deadline and level;
/// once it ends, a fresh read recovers the model.
#[tokio::test(start_paused = true)]
async fn a_no_model_429s_backoff_is_left_as_it_was_by_a_model_read() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    let unhinted = CallResult {
        retry_after: None,
        ..other_failure("", 429, Duration::ZERO)
    };
    // 1, 2, 4, 8 and 16 second holds, each after the last ran out.
    for step in 0..5u32 {
        h.manager.mark_result(&unhinted);
        if step < 4 {
            h.clock.advance(Duration::from_secs((1u64 << step) + 1));
        }
    }
    assert_eq!(h.get(ID).quota.backoff_level, 5);
    h.clock.advance(Duration::from_secs(1));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(1));
    let healthy = [(UsageWindow::SevenDayOpus, 2.0)];
    let before = h.get(ID);
    assert!(!recover(&h, &reading(h.now(), &healthy)));

    let current = h.get(ID);
    assert!(blocked(&h, OPUS));
    assert_eq!(
        format!("{:?} {:?}", current.quota, current.next_retry_after),
        format!("{:?} {:?}", before.quota, before.next_retry_after),
        "the credential's fields changed"
    );
    h.clock.advance(Duration::from_secs(60));
    assert!(recover(&h, &reading(h.now(), &healthy)));
    assert!(!blocked(&h, OPUS));
}

/// Known limitation: an Opus claim whose 10 second cooldown ran out is still
/// owed when a later 429 of another window re-blocks the scope, because the
/// claims are pruned against the state after the new outcome. Recovery is
/// refused, the safe direction, until a read shows Opus below its limit too.
#[tokio::test(start_paused = true)]
async fn an_expired_claims_window_stays_owed_through_a_later_429() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), Duration::from_secs(10)));
    h.clock.advance(Duration::from_secs(20));
    h.manager
        .mark_result(&quota_429(OPUS, five_hour_headers(), HOUR));
    h.clock.advance(Duration::from_secs(5));
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)])),
        "the expired Opus window is owed again: this pins the known limitation (fail closed); \
         if it is lifted on purpose, change this test"
    );
    assert!(blocked(&h, OPUS));
    assert!(recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(!blocked(&h, OPUS));
}

// Round 6: a model recovery is refused while the credential's own hold
// (a failure that named no model) lives. It changes nothing then: the
// credential, its models, its quota-check rest and its saved record stay as
// a run without the read leaves them.

const TWENTY_DAYS: Duration = Duration::from_secs(20 * 24 * 60 * 60);

fn capped_hour() -> Settings {
    Settings {
        quota_check_after: HOUR,
        ..Settings::default()
    }
}

#[derive(Clone, Copy, Debug)]
enum Case {
    /// Opus 429 (5 d), a credential-wide Sonnet 429 (1 h), a no-model 503 (2 h).
    WrapLive503,
    /// A capped 14 d no-model 429 whose check comes back as a recognized Opus
    /// 429, which rewrites the credential rest's reset.
    CappedLengthened,
    /// A capped 14 d no-model 429, an Opus 429, then a no-model 503 (2 h).
    CappedShortened,
    /// A 2 h 503 restored from disk (no provenance), an Opus 429, then a
    /// 10 minute no-model 503.
    RestoredThen503,
    /// A capped 14 d no-model 429 and an Opus 429, read 2 h on: the capped
    /// check time is past and no check is leased.
    CappedDue,
    /// The same, read 20 minutes on, before the check time.
    CappedEarly,
    /// A 600 s no-model 503, then an Opus 429 (5 d).
    ShortHold,
    /// An Opus 1 h quota 429, then a 2 h no-model 503.
    LongHold,
}

fn settings_of(case: Case) -> Settings {
    match case {
        Case::CappedLengthened | Case::CappedShortened | Case::CappedDue | Case::CappedEarly => {
            capped_hour()
        }
        _ => Settings::default(),
    }
}

/// The credential in `case`'s state, the clock at the moment of the read.
async fn build(case: Case, dir: &Path) -> Harness {
    if matches!(case, Case::RestoredThen503) {
        let first = saving(dir, Settings::default(), &[OPUS, SONNET]);
        first.manager.mark_result(&other_failure("", 503, 2 * HOUR));
        flush(&first.manager);
    }
    let h = saving(dir, settings_of(case), &[OPUS, SONNET]);
    let five = Duration::from_secs(5);
    match case {
        Case::WrapLive503 => {
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h.clock.advance(five);
            h.manager.mark_result(&CallResult {
                retry_after: Some(HOUR),
                ..failure(SONNET, sonnet_headers(), true)
            });
            h.clock.advance(five);
            h.manager.mark_result(&other_failure("", 503, 2 * HOUR));
            h.clock.advance(five);
        }
        Case::CappedLengthened => {
            h.manager
                .mark_result(&other_failure("", 429, FOURTEEN_DAYS));
            h.clock.advance(HOUR + Duration::from_secs(60));
            let executor = FakeExecutor::with("claude", |_: &Call| {
                Reply::Err(rejection(opus_headers(), false, FIVE_DAYS))
            });
            h.executor(&executor);
            let _ = call(&h, OPUS).await;
            settle().await;
            h.clock.advance(five);
        }
        Case::CappedShortened => {
            h.manager
                .mark_result(&other_failure("", 429, FOURTEEN_DAYS));
            h.clock.advance(Duration::from_secs(10));
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(10));
            h.manager.mark_result(&other_failure("", 503, 2 * HOUR));
            h.clock.advance(five);
        }
        Case::RestoredThen503 => {
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(10));
            h.manager
                .mark_result(&other_failure("", 503, Duration::from_secs(600)));
            h.clock.advance(five);
        }
        Case::CappedDue | Case::CappedEarly => {
            h.manager
                .mark_result(&other_failure("", 429, FOURTEEN_DAYS));
            h.clock.advance(Duration::from_secs(10));
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h.clock.advance(if matches!(case, Case::CappedDue) {
                2 * HOUR
            } else {
                Duration::from_secs(20 * 60)
            });
        }
        Case::ShortHold => {
            h.manager
                .mark_result(&other_failure("", 503, Duration::from_secs(600)));
            h.clock.advance(five);
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h.clock.advance(five);
        }
        Case::LongHold => {
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), HOUR));
            h.clock.advance(five);
            h.manager.mark_result(&other_failure("", 503, 2 * HOUR));
            h.clock.advance(five);
        }
    }
    h
}

/// Everything a refused read must leave alone: the live credential, the
/// selections, the check rests, the flushed record, the state restored from
/// it, and how many of two calls in flight reach the provider, on both.
async fn capture(h: &Harness, dir: &Path, settings: Settings) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut shape = |label: &str, h: &Harness| {
        let current = h.get(ID);
        let _ = writeln!(
            out,
            "{label}: unavailable {} retry {:?} status {:?}\n  quota {:?}\n  models {:?}\n  \
             selection opus {} sonnet {} none {}\n  checks {:?}",
            current.unavailable,
            current.next_retry_after,
            current.status,
            current.quota,
            current.model_states,
            blocked(h, OPUS),
            blocked(h, SONNET),
            no_model_blocked(h),
            h.manager.quota_checks(ID),
        );
    };
    shape("live", h);
    flush(&h.manager);
    let saved = FileStore::new(dir.to_path_buf()).load().expect("load");
    let next = Harness::new(settings);
    next.add(credential(dir), &[OPUS, SONNET]);
    next.clock
        .advance((h.now() - next.now()).to_std().unwrap_or_default());
    let on = config(dir, true);
    reconfigure(&next.manager, None, &on);
    restore(&next.manager, &on);
    shape("restored", &next);
    let live_reached = reached_by_two_calls(h).await;
    let restored_reached = reached_by_two_calls(&next).await;
    format!("{out}saved {saved:?}\nreached live {live_reached} restored {restored_reached}\n")
}

async fn run(case: Case, read: bool) -> (bool, String) {
    let dir = temp_dir();
    let h = build(case, dir.path()).await;
    let recovered = read && recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]));
    (recovered, capture(&h, dir.path(), settings_of(case)).await)
}

async fn refused(case: Case) {
    let (recovered, with_read) = run(case, true).await;
    let (_, control) = run(case, false).await;
    assert!(
        !recovered,
        "{case:?}: a model recovered under the credential's own hold"
    );
    assert_eq!(
        with_read, control,
        "{case:?}: the read changed what a run without it leaves"
    );
}

macro_rules! refusal {
    ($name:ident, $case:expr) => {
        #[tokio::test(start_paused = true)]
        async fn $name() {
            refused($case).await;
        }
    };
}

// Finding 1: a live credential-wide quota cooldown no longer shortens it.
refusal!(
    a_live_wrap_and_a_503_are_not_shortened_by_a_model_read,
    Case::WrapLive503
);
// Finding 2, lengthening: the shared rest's reset rewritten by a check.
refusal!(
    a_rewritten_shared_rest_does_not_lengthen_the_hold_on_a_model_read,
    Case::CappedLengthened
);
// Finding 2, shortening: a later 503 inside a capped 14 d hold.
refusal!(
    a_503_inside_a_capped_hold_is_not_shortened_by_a_model_read,
    Case::CappedShortened
);
// Finding 3: a restored hold with no provenance, then a new failure.
refusal!(
    a_restored_hold_and_a_new_503_are_not_replaced_by_a_model_read,
    Case::RestoredThen503
);
// Finding 6, and round 5's cases a, b and c.
refusal!(
    a_due_capped_rest_is_left_as_it_was_by_a_model_read,
    Case::CappedDue
);
refusal!(
    a_capped_hold_before_its_check_is_left_as_it_was_by_a_model_read,
    Case::CappedEarly
);
refusal!(
    a_short_hold_is_left_as_it_was_by_a_model_read,
    Case::ShortHold
);
refusal!(
    a_long_hold_is_left_as_it_was_by_a_model_read,
    Case::LongHold
);

/// Once the 503 ends, a fresh read recovers the model; the other model's
/// cooldown is untouched, and the refused read before it used nothing up.
#[tokio::test(start_paused = true)]
async fn a_model_recovers_once_the_credentials_503_hold_has_ended() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.manager
        .mark_result(&quota_429(SONNET, sonnet_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    h.manager.mark_result(&other_failure("", 503, 2 * HOUR));
    h.clock.advance(Duration::from_secs(5));
    let healthy = [(UsageWindow::SevenDayOpus, 2.0)];
    assert!(!recover(&h, &reading(h.now(), &healthy)));
    assert!(blocked(&h, OPUS));

    h.clock.advance(2 * HOUR + Duration::from_secs(60));
    assert!(recover(&h, &reading(h.now(), &healthy)));
    assert!(!blocked(&h, OPUS));
    assert!(blocked(&h, SONNET), "the other model's cooldown was taken");
    assert!(h.get(ID).model_states[SONNET].quota.exceeded);
}

/// The same for a capped 14 d no-model hold, ended by its own clock.
#[tokio::test(start_paused = true)]
async fn a_model_recovers_once_a_capped_14_day_hold_has_ended() {
    let h = Harness::new(capped_hour());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager
        .mark_result(&other_failure("", 429, FOURTEEN_DAYS));
    h.clock.advance(Duration::from_secs(10));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), TWENTY_DAYS));
    h.manager
        .mark_result(&quota_429(SONNET, sonnet_headers(), TWENTY_DAYS));
    h.clock.advance(2 * HOUR);
    let healthy = [(UsageWindow::SevenDayOpus, 2.0)];
    assert!(!recover(&h, &reading(h.now(), &healthy)));

    h.clock.advance(FOURTEEN_DAYS);
    assert!(recover(&h, &reading(h.now(), &healthy)));
    assert!(!blocked(&h, OPUS));
    assert!(
        h.get(ID).model_states[SONNET].quota.exceeded,
        "the other model's cooldown was taken"
    );
}

/// A no-model backoff that ran out must not come back: a model 429 and its
/// recovery reset the live backoff, so a later unhinted no-model 429 is a
/// first one (1 s) and a model recovery after it is not held longer.
#[tokio::test(start_paused = true)]
async fn an_expired_no_model_backoff_does_not_come_back_through_a_recovery() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    let unhinted = CallResult {
        retry_after: None,
        ..other_failure("", 429, Duration::ZERO)
    };
    for step in 0..5u32 {
        h.manager.mark_result(&unhinted);
        h.clock.advance(Duration::from_secs((1u64 << step) + 1));
    }
    h.clock.advance(Duration::from_secs(40));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(1));
    let healthy = [(UsageWindow::SevenDayOpus, 2.0)];
    assert!(recover(&h, &reading(h.now(), &healthy)));

    h.manager.mark_result(&unhinted);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(3));
    assert!(recover(&h, &reading(h.now(), &healthy)));
    assert!(!blocked(&h, OPUS));
    assert!(
        !no_model_blocked(&h),
        "the old backoff was laid back on: {:?}",
        h.get(ID).quota
    );
}

// Finding 5: clearing cooldowns without a failure forgets what was noted.

/// A no-model 503 (2 h), `disable_cooling` on and off, a new Opus 429, then a
/// read that proves Opus: the 503 was cleared with everything else and must
/// not be put back.
#[tokio::test(start_paused = true)]
async fn a_settings_toggle_forgets_the_hold_it_cleared() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager.mark_result(&other_failure("", 503, 2 * HOUR));
    h.manager.set_settings(Settings {
        disable_cooling: true,
        ..Settings::default()
    });
    assert!(!no_model_blocked(&h), "cooling off cleared the hold");
    h.manager.set_settings(Settings::default());
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    assert!(recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(!blocked(&h, OPUS));
    assert!(
        !no_model_blocked(&h),
        "the cleared hold came back: {:?} retry {:?}",
        h.get(ID).quota,
        h.get(ID).next_retry_after
    );
}

/// The same when the credential is disabled and enabled again by an edit.
#[tokio::test(start_paused = true)]
async fn disabling_and_enabling_a_credential_forgets_the_hold_it_cleared() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager.mark_result(&other_failure("", 503, 2 * HOUR));
    h.manager
        .edit(ID, |auth| {
            auth.disabled = true;
            Ok::<_, ()>(true)
        })
        .expect("disable");
    h.manager
        .edit(ID, |auth| {
            auth.disabled = false;
            Ok::<_, ()>(true)
        })
        .expect("enable");
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    assert!(recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(
        !no_model_blocked(&h),
        "the cleared hold came back: {:?} retry {:?}",
        h.get(ID).quota,
        h.get(ID).next_retry_after
    );
}

/// A read in flight across the toggle is fenced: the cooldown it was taken
/// for was cleared, and the one that follows is a new one.
#[tokio::test(start_paused = true)]
async fn a_read_in_flight_across_a_settings_toggle_is_fenced() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    let token = h.manager.quota_recovery_token(ID).expect("token");
    h.clock.advance(Duration::from_secs(5));
    let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
    h.manager.set_settings(Settings {
        disable_cooling: true,
        ..Settings::default()
    });
    h.manager.set_settings(Settings::default());
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    assert!(
        h.manager
            .apply_quota_usage_recovery(&token, &healthy)
            .expect("apply")
            .is_empty(),
        "a read from before the toggle ended the new cooldown"
    );
    assert!(blocked(&h, OPUS));
}

/// Claude's executor picks its headers from which identity fields hold
/// anything: adding or removing each one fences a read in flight.
#[tokio::test(start_paused = true)]
async fn each_identity_presence_flag_fences_a_read_in_flight() {
    let keys: [(&str, serde_json::Value); 8] = [
        ("access_token", "x".into()),
        ("refresh_token", "x".into()),
        ("id_token", "x".into()),
        ("email", "a@b.c".into()),
        ("token_type", "Bearer".into()),
        ("expires_at", "2029-01-01T00:00:00Z".into()),
        ("expired", "2029-01-01T00:00:00Z".into()),
        ("token", serde_json::json!({"k": "v"})),
    ];
    for (key, value) in keys {
        let h = Harness::new(Settings::default());
        h.add(auth(ID, "claude"), &[OPUS]);
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
        let token = h.manager.quota_recovery_token(ID).expect("token");
        h.clock.advance(Duration::from_secs(5));
        let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
        h.manager
            .edit(ID, |auth| {
                if auth.metadata.remove(key).is_none() {
                    auth.metadata.insert(key.into(), value.clone());
                }
                Ok::<_, ()>(true)
            })
            .expect("edit");
        assert!(
            h.manager
                .apply_quota_usage_recovery(&token, &healthy)
                .expect("apply")
                .is_empty(),
            "a read made before a {key} presence change ended a cooldown"
        );
        assert!(blocked(&h, OPUS), "{key}");
        assert!(!token.matches(&h.get(ID)), "{key} change did not fence");
    }
}

/// A hold restored from disk has no deadline of its own to go by, so it
/// counts as live for good: a model read stays refused after its time has
/// passed, the safe direction.
#[tokio::test(start_paused = true)]
async fn an_expired_restored_hold_still_refuses_a_model_read() {
    let dir = temp_dir();
    {
        let first = saving(dir.path(), Settings::default(), &[OPUS, SONNET]);
        first.manager.mark_result(&other_failure("", 503, 2 * HOUR));
        flush(&first.manager);
    }
    let h = saving(dir.path(), Settings::default(), &[OPUS, SONNET]);
    h.clock.advance(3 * HOUR);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "an unknown hold was taken for an ended one"
    );
    assert!(blocked(&h, OPUS));
}

/// The credential, its flags and its models as a test compares them before and
/// after a read that must change nothing.
fn shape(h: &Harness) -> String {
    let current = h.get(ID);
    format!(
        "unavailable {} retry {:?} status {:?}\nquota {:?}\nmodels {:?}\nchecks {:?}",
        current.unavailable,
        current.next_retry_after,
        current.status,
        current.quota,
        current.model_states,
        h.manager.quota_checks(ID),
    )
}

const ROUTE: &str = "claude-opus-route";

/// Opus's target is `ROUTE`'s alias target from here on.
fn route_alias() -> Settings {
    Settings {
        oauth_model_alias: std::collections::BTreeMap::from([(
            "claude".to_owned(),
            vec![crate::manager::ModelAlias {
                name: OPUS.into(),
                alias: ROUTE.into(),
                force_mapping: false,
            }],
        )]),
        ..Settings::default()
    }
}

/// A recognised Opus 429, then a 503 on `ROUTE`, which is not yet an alias of
/// Opus: a model hold of 2 hours under a state of its own.
fn before_the_alias() -> Harness {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, ROUTE]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    h.manager.mark_result(&other_failure(ROUTE, 503, 2 * HOUR));
    assert!(
        h.get(ID).model_states.contains_key(ROUTE),
        "the 503 has a state of its own"
    );
    h
}

/// `ROUTE` becomes Opus's alias, and the registry reconciles the credential's
/// model states: the 503's state is merged into Opus's.
fn merge_the_alias(h: &Harness) {
    h.manager.set_settings(route_alias());
    h.models.register(ID, &[ROUTE]);
    h.manager.reconcile_registry_model_states(ID);
    let merged = h.get(ID);
    assert!(
        !merged.model_states.contains_key(ROUTE),
        "the alias was merged: {:?}",
        merged.model_states
    );
    assert!(blocked(h, OPUS));
}

/// A live 503 hold of an alias key is merged into Opus's state by the
/// reconcile. Opus's claim described only its own 429, so a read that proves
/// Opus must not end the merged state: the claim is invalidated and the
/// target's cooldown runs its course.
#[tokio::test(start_paused = true)]
async fn an_alias_hold_merged_into_a_model_is_not_ended_by_that_models_recovery() {
    let h = before_the_alias();
    merge_the_alias(&h);
    h.clock.advance(Duration::from_secs(5));
    let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
    let before = shape(&h);
    assert!(!recover(&h, &healthy), "the merged 503 hold was ended");
    assert_eq!(shape(&h), before);
    assert!(blocked(&h, OPUS));

    // The claim is gone for good, not for the 2 hours: failing closed.
    h.clock.advance(3 * HOUR);
    assert!(!recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(blocked(&h, OPUS));
}

/// A later recognised Opus 429 gives Opus a new claim; the merged 503 hold
/// still refuses it while it lives, and then a fresh read recovers.
#[tokio::test(start_paused = true)]
async fn a_live_alias_hold_merged_into_a_model_still_refuses_a_later_claim() {
    let h = before_the_alias();
    merge_the_alias(&h);
    h.clock.advance(Duration::from_secs(5));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    let before = shape(&h);
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "the merged 503 hold was ended by a later claim"
    );
    assert_eq!(shape(&h), before);

    // Round 12: the merge mark stands until the latest layer it merged (Opus's
    // own five days) has run out, and it refuses a model's recovery as well:
    // the 503 has ended but the read still can't end the merged cooldown.
    h.clock.advance(3 * HOUR);
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "a model recovered under the merge mark"
    );
    assert!(blocked(&h, OPUS));
    h.clock.advance(FIVE_DAYS);
    assert!(!blocked(&h, OPUS), "it runs out by itself");
}

/// A read taken before the reconcile merged the alias is fenced.
#[tokio::test(start_paused = true)]
async fn a_read_in_flight_across_an_alias_merge_is_fenced() {
    let h = before_the_alias();
    let token = h.manager.quota_recovery_token(ID).expect("token");
    h.clock.advance(Duration::from_secs(5));
    let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
    merge_the_alias(&h);
    assert!(
        h.manager
            .apply_quota_usage_recovery(&token, &healthy)
            .expect("apply")
            .is_empty(),
        "a read from before the merge ended the merged state"
    );
    assert!(blocked(&h, OPUS));
}

/// A stop-and-cooldown rule's error: it holds the scope even with transient
/// cooling off, for a minute.
fn forced(model: &str, status: u16) -> CallResult {
    let mut result = other_failure(model, status, Duration::ZERO);
    result.retry_after = None;
    if let Some(error) = result.error.as_mut() {
        error.code = "force_cooldown".into();
    }
    result
}

fn cooling_off() -> Settings {
    Settings {
        transient_error_cooldown_seconds: -1,
        ..Settings::default()
    }
}

/// A recognised Opus 429, then a forced 503 that names no model with
/// transient cooling off: it holds the whole credential for 60 seconds. A read
/// inside the minute is refused and changes nothing; one after it recovers.
#[tokio::test(start_paused = true)]
async fn a_forced_credential_hold_with_cooling_off_refuses_a_model_read_until_it_ends() {
    let h = Harness::new(cooling_off());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    h.manager.mark_result(&forced("", 503));
    assert!(no_model_blocked(&h), "the forced hold is installed");
    h.clock.advance(Duration::from_secs(5));
    let before = shape(&h);
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "the forced hold was dropped with 50 seconds left"
    );
    assert_eq!(shape(&h), before);
    assert!(blocked(&h, OPUS) && no_model_blocked(&h));

    h.clock.advance(Duration::from_secs(61));
    assert!(recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(!blocked(&h, OPUS));
    assert!(!no_model_blocked(&h));
}

/// The same on the model: a forced 503 on Opus, then a recognised Opus 429.
#[tokio::test(start_paused = true)]
async fn a_forced_model_hold_with_cooling_off_refuses_a_model_read_until_it_ends() {
    let h = Harness::new(cooling_off());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager.mark_result(&forced(OPUS, 503));
    h.clock.advance(Duration::from_secs(5));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    let before = shape(&h);
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "the forced model hold was dropped with 50 seconds left"
    );
    assert_eq!(shape(&h), before);
    assert!(blocked(&h, OPUS));

    h.clock.advance(Duration::from_secs(61));
    assert!(recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(!blocked(&h, OPUS));
}

/// A read taken before a forced credential hold was installed, applied while
/// the hold lives, is refused.
#[tokio::test(start_paused = true)]
async fn a_read_taken_before_a_forced_hold_is_refused_while_it_lives() {
    let h = Harness::new(cooling_off());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    let token = h.manager.quota_recovery_token(ID).expect("token");
    h.clock.advance(Duration::from_secs(5));
    h.manager.mark_result(&forced("", 503));
    h.clock.advance(Duration::from_secs(5));
    let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
    assert!(
        h.manager
            .apply_quota_usage_recovery(&token, &healthy)
            .expect("apply")
            .is_empty(),
        "a read from before the forced hold ended it"
    );
    assert!(blocked(&h, OPUS) && no_model_blocked(&h));
}

// Round 8: a credential-wide recovery is refused wherever the state isn't
// simple (a model resting on a quota check under any key, a live hold of a
// model's own, a merge since the claim); a merge keeps the windows it owed; and
// a failure's own hold is the deadline application set.

/// A credential-wide 429 of Sonnet that wraps every model for `retry`.
fn wrap(h: &Harness, retry: Duration) {
    h.manager.mark_result(&CallResult {
        retry_after: Some(retry),
        ..failure(SONNET, weekly(h.now()), true)
    });
}

fn alias_settings(cap: Duration) -> Settings {
    Settings {
        quota_check_after: cap,
        ..route_alias()
    }
}

/// `ROUTE` becomes Opus's alias under `settings`, and the registry reconciles
/// the credential's model states.
fn merge_with(h: &Harness, settings: Settings) {
    h.manager.set_settings(settings);
    h.models.register(ID, &[ROUTE]);
    h.manager.reconcile_registry_model_states(ID);
    let merged = h.get(ID);
    assert!(
        !merged.model_states.contains_key(ROUTE),
        "the alias was merged: {:?}",
        merged.model_states
    );
    assert!(blocked(h, OPUS));
}

/// Builds the same state twice with `setup`, once with a credential read that
/// proves the credential (Opus shown full) `wait` after it and once without,
/// and requires the read to be refused and to change nothing: the live
/// credential, the selections and dispatch counts, the check rests, the flushed
/// record and the state restored from it.
async fn refused_credential_read(
    initial: Settings,
    finally: Settings,
    models: &[&str],
    setup: impl Fn(&Harness),
    wait: Duration,
) {
    refused_read(initial, finally, models, setup, wait, 100.0).await;
}

/// [`refused_credential_read`] with Opus shown at `opus` percent: a read that
/// proves a model's claim as well when it is low.
async fn refused_read(
    initial: Settings,
    finally: Settings,
    models: &[&str],
    setup: impl Fn(&Harness),
    wait: Duration,
    opus: f64,
) {
    let mut seen = Vec::new();
    for read in [true, false] {
        let dir = temp_dir();
        let h = saving(dir.path(), initial.clone(), models);
        setup(&h);
        h.clock.advance(wait);
        if read {
            assert!(
                !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, opus)])),
                "the credential's recovery went ahead"
            );
        }
        seen.push(capture(&h, dir.path(), finally.clone()).await);
    }
    assert_eq!(seen[0], seen[1], "a refused read changed something");
}

/// Finding A: Opus's 5-day quota 429 is capped to a 1 hour check, an
/// independent 503 with a 2 hour hint follows, and a credential-wide 429 wraps
/// both. Lifting both layers' times to the provider's reset and capping them
/// again shortened the 503 by an hour; the credential's recovery is refused
/// while Opus rests on a check.
#[tokio::test(start_paused = true)]
async fn a_credential_recovery_does_not_shorten_an_independent_503_under_a_capped_rest() {
    refused_credential_read(
        capped_hour(),
        capped_hour(),
        &[OPUS, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(5));
            h.manager.mark_result(&other_failure(OPUS, 503, 2 * HOUR));
            h.clock.advance(Duration::from_secs(5));
            wrap(h, 2 * HOUR);
        },
        Duration::from_secs(30 * 60),
    )
    .await;
}

/// Finding B: the capped rest of an alias stays under the alias's key when the
/// alias is merged into Opus. A credential recovery then found no rest under
/// Opus and freed a model still at its limit, with no check to lease.
#[tokio::test(start_paused = true)]
async fn a_credential_recovery_is_refused_while_an_alias_keys_rest_is_orphaned() {
    refused_credential_read(
        capped_hour(),
        alias_settings(HOUR),
        &[OPUS, ROUTE, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(ROUTE, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(5));
            wrap(h, 2 * HOUR);
            h.clock.advance(Duration::from_secs(5));
            merge_with(h, alias_settings(HOUR));
        },
        Duration::from_secs(65 * 60),
    )
    .await;
}

/// Finding C: a merge of two wrapped states keeps one shadow, so the other's
/// hold (a 2 hour 503 of the alias) would be lost by the credential's recovery.
#[tokio::test(start_paused = true)]
async fn a_credential_recovery_is_refused_after_a_merge_of_two_wrapped_holds() {
    refused_credential_read(
        Settings::default(),
        route_alias(),
        &[OPUS, ROUTE, SONNET],
        |h| {
            h.manager.mark_result(&other_failure(ROUTE, 503, 2 * HOUR));
            h.clock.advance(Duration::from_secs(5));
            h.manager.mark_result(&other_failure(OPUS, 503, HOUR));
            h.clock.advance(Duration::from_secs(5));
            wrap(h, 3 * HOUR);
            h.clock.advance(Duration::from_secs(5));
            merge_with(h, route_alias());
        },
        Duration::from_secs(30 * 60),
    )
    .await;
}

/// A merge since the credential's claim is refused on its own, with no rest and
/// no hold of a model's own: the claim no longer speaks for what the merged
/// state holds under the wrap.
#[tokio::test(start_paused = true)]
async fn a_merge_since_the_credentials_claim_refuses_its_recovery() {
    refused_credential_read(
        Settings::default(),
        route_alias(),
        &[OPUS, ROUTE, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(ROUTE, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(5));
            wrap(h, 2 * HOUR);
            h.clock.advance(Duration::from_secs(5));
            merge_with(h, route_alias());
        },
        Duration::from_secs(20 * 60),
    )
    .await;
}

/// A live hold of a model's own under the wrap refuses the credential's
/// recovery: the shadow may not show it. Once it has ended a fresh read
/// recovers the credential, and Opus keeps its own 5-day limit.
#[tokio::test(start_paused = true)]
async fn a_live_model_hold_under_the_wrap_refuses_the_credentials_recovery_until_it_ends() {
    refused_credential_read(
        Settings::default(),
        Settings::default(),
        &[OPUS, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(5));
            h.manager.mark_result(&other_failure(OPUS, 503, HOUR));
            h.clock.advance(Duration::from_secs(5));
            wrap(h, 3 * HOUR);
        },
        Duration::from_secs(20 * 60),
    )
    .await;

    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    h.manager.mark_result(&other_failure(OPUS, 503, HOUR));
    h.clock.advance(Duration::from_secs(5));
    wrap(&h, 3 * HOUR);
    h.clock.advance(Duration::from_secs(70 * 60));
    assert!(
        recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)])),
        "the 503 ended an hour in"
    );
    assert!(!blocked(&h, SONNET), "the wrap is gone");
    assert!(blocked(&h, OPUS), "opus keeps its own 5 day limit");
}

/// With no rest and no live hold of a model's own, the credential's recovery
/// still works, and it equals the wrap's natural expiry: each model's own layer
/// is back as it was under the wrap. (Passes before round 8 as well.)
#[tokio::test(start_paused = true)]
async fn a_plain_credential_recovery_equals_the_wraps_natural_expiry() {
    let build = || {
        let h = Harness::new(Settings::default());
        h.add(auth(ID, "claude"), &[OPUS, SONNET]);
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
        h.clock.advance(Duration::from_secs(5));
        wrap(&h, 2 * HOUR);
        h
    };
    let own = |h: &Harness, layer: bool| {
        let state = h.get(ID).model_states.get(OPUS).cloned().expect("opus");
        let state = if layer {
            *state.credential_quota_shadow.expect("the wrap's shadow")
        } else {
            state
        };
        (
            state.next_retry_after,
            state.quota.next_recover_at,
            state.quota.exceeded,
            state.quota.reason.clone(),
        )
    };

    let recovered = build();
    recovered.clock.advance(Duration::from_secs(30 * 60));
    assert!(recover(
        &recovered,
        &reading(recovered.now(), &[(UsageWindow::SevenDayOpus, 100.0)])
    ));
    let natural = build();
    natural.clock.advance(Duration::from_secs(2 * 60 * 60 + 60));
    assert_eq!(
        own(&recovered, false),
        own(&natural, true),
        "opus's own layer is not what the wrap's expiry leaves"
    );
    for h in [&recovered, &natural] {
        assert!(!blocked(h, SONNET), "the wrap is gone");
        assert!(blocked(h, OPUS), "opus keeps its own limit");
    }
    let state = recovered.get(ID);
    assert_ne!(state.quota.reason, "credential_quota", "the wrap is gone");
    assert!(
        state.model_states[OPUS].credential_quota_shadow.is_none()
            && !state.model_states[OPUS].credential_quota_propagated
    );
}

/// Finding D: an alias's recognised claim names a window; merging it into Opus
/// keeps its cooldown, and the window stays owed when a later recognised Opus
/// 429 names another. A read showing the later window healthy and the earlier
/// one full must not end the merged cooldown.
#[tokio::test(start_paused = true)]
async fn a_merge_keeps_the_aliass_owed_window_through_a_later_claim() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, ROUTE]);
    h.manager
        .mark_result(&quota_429(ROUTE, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    merge_with(&h, route_alias());
    h.clock.advance(Duration::from_secs(5));
    h.manager
        .mark_result(&quota_429(OPUS, five_hour_headers(), HOUR));
    h.clock.advance(Duration::from_secs(5));

    let before = shape(&h);
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)])),
        "the alias's window was forgotten by the merge"
    );
    assert_eq!(shape(&h), before);
    assert!(blocked(&h, OPUS));
    // Round 12: the merge mark refuses the model's recovery as well, so the
    // cooldown runs out by itself rather than being ended by a read.
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "a model recovered under the merge mark"
    );
    assert!(blocked(&h, OPUS));
}

/// Finding E: an HTTP 500 with the code `model_not_found` holds the model for
/// 12 hours (the model-support rule), not the minute of an ordinary transient
/// failure. A later recognised Opus 429 and a read a minute on must not end it.
#[tokio::test(start_paused = true)]
async fn a_model_support_hold_is_counted_at_its_own_deadline() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    let mut unsupported = other_failure(OPUS, 500, Duration::ZERO);
    unsupported.retry_after = None;
    if let Some(error) = unsupported.error.as_mut() {
        error.code = "model_not_found".into();
    }
    h.manager.mark_result(&unsupported);
    h.clock.advance(Duration::from_secs(5));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(65));

    let before = shape(&h);
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "a live 12 hour model-support hold was cleared"
    );
    assert_eq!(shape(&h), before);
    assert!(blocked(&h, OPUS));

    h.clock.advance(Duration::from_secs(12 * 60 * 60));
    assert!(recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(!blocked(&h, OPUS));
}

/// Application sets each failure's own deadline once; the recovery counts
/// exactly that. For a table of status, error code, message, hint and cooling
/// setting, the failure's recorded own hold ends where the deadline it left on
/// a credential with nothing earlier standing ends.
#[tokio::test(start_paused = true)]
async fn a_failures_own_hold_is_the_deadline_application_set() {
    use crate::manager::quota_recovery::scope_deadline;

    let statuses = [
        400u16, 401, 402, 403, 404, 408, 429, 500, 502, 503, 504, 520, 526,
    ];
    let codes = ["", "model_not_found", "force_cooldown", "invalid_grant"];
    let hints = [None, Some(Duration::from_secs(90))];
    let settings = [
        Settings::default(),
        cooling_off(),
        Settings {
            transient_error_cooldown_seconds: 30,
            ..Settings::default()
        },
        Settings {
            disable_cooling: true,
            ..Settings::default()
        },
    ];
    let mut checked = 0;
    for (index, setting) in settings.into_iter().enumerate() {
        let h = Harness::new(setting);
        for scoped in [false, true] {
            for status in statuses {
                for code in codes {
                    for message in ["failed", "cloudflare challenge"] {
                        for hint in hints {
                            for recognised in [false, true] {
                                // The control: the failure alone on a fresh
                                // credential, which says where application
                                // puts its own deadline.
                                let mut control_end = None;
                                for existing in [false, true] {
                                    let id = format!(
                                        "parity-{index}-{scoped}-{status}-{code}-{message}-\
                                         {hint:?}-{recognised}-{existing}"
                                    );
                                    h.add(auth(&id, "claude"), &[OPUS, SONNET]);
                                    let scope = if scoped { OPUS } else { "" };
                                    if existing {
                                        // A recognised five day quota cooldown
                                        // of the scope, with its claim.
                                        let mut prior = if scoped {
                                            quota_429(OPUS, opus_headers(), FIVE_DAYS)
                                        } else {
                                            CallResult {
                                                retry_after: Some(FIVE_DAYS),
                                                ..failure(SONNET, weekly(h.now()), true)
                                            }
                                        };
                                        prior.auth_id = id.clone();
                                        h.manager.mark_result(&prior);
                                        h.clock.advance(Duration::from_secs(1));
                                    }
                                    let now = h.now();
                                    let mut result = other_failure(scope, status, Duration::ZERO);
                                    result.auth_id = id.clone();
                                    result.retry_after = hint;
                                    result.credential_scope = !scoped;
                                    if recognised {
                                        result.response_headers =
                                            if scoped { opus_headers() } else { weekly(now) };
                                    }
                                    if let Some(error) = result.error.as_mut() {
                                        error.code = code.into();
                                        error.message = message.into();
                                    }
                                    h.manager.mark_result(&result);
                                    let deadline = scope_deadline(&h.get(&id), scope);
                                    let live = |at: Timestamp| {
                                        h.manager.lock().quota_recovery.foreign_live(&id, scope, at)
                                    };
                                    // A 429 application took for a quota
                                    // cooldown records a claim; every other
                                    // failure, a 429 it classified as
                                    // something else (model support,
                                    // Cloudflare) included, records a hold.
                                    let classified = status == 429
                                        && ((scoped && code == "model_not_found")
                                            || message == "cloudflare challenge");
                                    let quota = status == 429 && !classified;
                                    let case = format!(
                                        "{status} code {code:?} {message:?} hint {hint:?} \
                                         scoped {scoped} recognised {recognised} existing \
                                         {existing} settings {index}: deadline {deadline:?}"
                                    );
                                    if !existing {
                                        control_end = deadline;
                                    }
                                    if !quota || (!recognised && !existing) {
                                        match control_end {
                                            Some(end) if end > now => {
                                                assert!(live(now), "{case}: not held now");
                                                // The end of a backed-off
                                                // cooldown (Cloudflare) depends
                                                // on the earlier 429, so the
                                                // control's end is exact only
                                                // for a credential with
                                                // nothing earlier.
                                                if !existing {
                                                    assert!(
                                                        live(end - TimeDelta::milliseconds(1)),
                                                        "{case}: held less than application did"
                                                    );
                                                    assert!(
                                                        !live(end),
                                                        "{case}: held longer than application did"
                                                    );
                                                }
                                            }
                                            _ => {
                                                assert!(!live(now), "{case}: held with no cooldown")
                                            }
                                        }
                                    }
                                    checked += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(checked > 4000, "the table ran {checked} cases");
}

// Round 9: a merge stays refused under its wrap whatever the wrap's later 429s
// do; a 429 application classified as something else is a hold, not a quota
// claim; and a credential registered with cooldowns has no provenance for them.

/// A recognised Opus 429 that application takes for something else.
fn classified_429(model: &str, code: &str, message: &str, retry: Duration) -> CallResult {
    let mut result = quota_429(model, opus_headers(), retry);
    if let Some(error) = result.error.as_mut() {
        error.code = code.into();
        error.message = message.into();
    }
    result
}

/// Finding 1: an alias with a 5 day quota cooldown, a target with a newer 1
/// hour one, and a credential-wide 5 day 429 over both. The reconcile keeps
/// only the target's layer under the wrap. A call that was in flight then
/// returns another recognised credential-wide 429, which records a fresh claim
/// after the merge. Opus's window is shown full at 70 minutes: the wrap still
/// stands, so the read must not lift it to the expired hour.
#[tokio::test(start_paused = true)]
async fn a_later_credential_429_does_not_lift_the_refusal_of_a_merge_under_its_wrap() {
    refused_credential_read(
        Settings::default(),
        route_alias(),
        &[OPUS, ROUTE, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(ROUTE, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(5));
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), HOUR));
            h.clock.advance(Duration::from_secs(5));
            wrap(h, FIVE_DAYS);
            h.clock.advance(Duration::from_secs(5));
            merge_with(h, route_alias());
            h.clock.advance(Duration::from_secs(5));
            wrap(h, FIVE_DAYS);
        },
        Duration::from_secs(70 * 60),
    )
    .await;
}

/// The same, with the second credential-wide 429 landing after the first wrap
/// has run out: that is a new wrap, which no merge touched, and the credential
/// may recover. (The mark ends with the wrap it was made under.)
#[tokio::test(start_paused = true)]
async fn a_credential_429_after_the_merged_wrap_ran_out_starts_a_wrap_that_can_recover() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, ROUTE, SONNET]);
    h.manager
        .mark_result(&quota_429(ROUTE, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), HOUR));
    h.clock.advance(Duration::from_secs(5));
    wrap(&h, 2 * HOUR);
    h.clock.advance(Duration::from_secs(5));
    merge_with(&h, route_alias());
    // Every layer the merge could have touched, the alias's 5 days included,
    // runs out before the next credential-wide 429.
    h.clock.advance(FIVE_DAYS + HOUR);
    wrap(&h, 2 * HOUR);
    h.clock.advance(Duration::from_secs(20 * 60));
    assert!(
        recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)])),
        "a new wrap, made after the merged one ran out"
    );
    assert!(!blocked(&h, SONNET), "the new wrap is gone");
}

/// Finding 2: Opus has a recognised 5 day cooldown with a claim. A 429 with
/// recognised headers and the code `model_not_found` and a 12 hour hint is
/// application's 12 hour model-support hold, whatever its headers say. A read
/// showing the windows healthy must not clear it.
#[tokio::test(start_paused = true)]
async fn a_429_taken_for_a_model_support_failure_is_a_hold_not_a_claim() {
    refused_read(
        Settings::default(),
        Settings::default(),
        &[OPUS, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(5));
            h.manager.mark_result(&classified_429(
                OPUS,
                "model_not_found",
                "no such model",
                12 * HOUR,
            ));
            h.clock.advance(Duration::from_secs(60));
        },
        Duration::from_secs(60),
        2.0,
    )
    .await;
}

/// A recognised 429 that is a Cloudflare challenge holds the model for the
/// challenge's own short time. Within it a read is refused, and changes
/// nothing.
#[tokio::test(start_paused = true)]
async fn a_429_taken_for_a_cloudflare_challenge_is_a_hold_not_a_claim() {
    refused_read(
        Settings::default(),
        Settings::default(),
        &[OPUS, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(5));
            h.manager.mark_result(&classified_429(
                OPUS,
                "",
                "cloudflare challenge",
                Duration::from_secs(90),
            ));
        },
        Duration::from_secs(1),
        2.0,
    )
    .await;
}

/// The credential-scope equivalent: a credential-wide wrap with its claim, and
/// a credential-wide 429 with recognised headers that is a Cloudflare
/// challenge.
#[tokio::test(start_paused = true)]
async fn a_credential_wide_429_taken_for_a_cloudflare_challenge_is_a_hold_not_a_claim() {
    refused_credential_read(
        Settings::default(),
        Settings::default(),
        &[OPUS, SONNET],
        |h| {
            wrap(h, 2 * HOUR);
            h.clock.advance(Duration::from_secs(5));
            h.manager.mark_result(&CallResult {
                retry_after: Some(Duration::from_secs(90)),
                error: Some(AuthError {
                    http_status: 429,
                    message: "cloudflare challenge".into(),
                    ..AuthError::default()
                }),
                ..failure(SONNET, weekly(h.now()), true)
            });
        },
        Duration::from_secs(1),
    )
    .await;
}

/// Finding 3: Opus has a 5 day quota cooldown. A clone of the live credential
/// is registered again: its provenance goes, its cooldown stays. A recognised
/// Opus 429 naming only the 5 hour window then leaves the longer deadline
/// standing, and a read showing that window healthy must not clear the 5 days.
#[tokio::test(start_paused = true)]
async fn registering_a_credential_that_carries_a_cooldown_gives_it_no_provenance() {
    refused_read(
        Settings::default(),
        Settings::default(),
        &[OPUS, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(5));
            let live = (*h.get(ID)).clone();
            h.manager.register(live).expect("register");
            h.clock.advance(Duration::from_secs(5));
            h.manager
                .mark_result(&as_of_now(h, quota_429(OPUS, five_hour_headers(), HOUR)));
        },
        Duration::from_secs(60),
        2.0,
    )
    .await;
}

/// `result` as a call made after the latest registration would report it: a
/// result carrying an older credential version is dropped as stale.
fn as_of_now(h: &Harness, result: CallResult) -> CallResult {
    let live = h.get(ID);
    CallResult {
        credential_version: live.credential_version,
        registration_epoch: live.registration_epoch,
        ..result
    }
}

/// A credential registered with no cooldowns is unaffected: the normal
/// recovery works after it.
#[tokio::test(start_paused = true)]
async fn a_clean_registration_still_lets_the_normal_recovery_work() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    let clean = (*h.get(ID)).clone();
    h.manager.register(clean).expect("register");
    h.manager
        .mark_result(&as_of_now(&h, quota_429(OPUS, opus_headers(), FIVE_DAYS)));
    h.clock.advance(Duration::from_secs(60));
    assert!(blocked(&h, OPUS));
    assert!(recover(
        &h,
        &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
    ));
    assert!(!blocked(&h, OPUS));
}

// Round 10: a failure application classified is held under the scope it
// changed; a merge's mark outlives the wrap until every layer has run out; an
// update that stores other cooldowns than the live ones gives them no
// provenance.

/// A success of `model`.
fn success(model: &str) -> CallResult {
    CallResult {
        auth_id: ID.into(),
        provider: "claude".into(),
        model: model.into(),
        success: true,
        ..CallResult::default()
    }
}

/// `result` classified by application, reported with the credential-scope
/// flag the Claude executor sets from the unified rejection headers alone;
/// then a success of the sibling model; then an ordinary recognised Opus 429
/// that asks for `claim`, shorter than the classified hold. A read showing
/// the windows healthy, `step` after that, must not clear the hold.
async fn classified_credential_scoped_then_a_claim(
    result: CallResult,
    step: Duration,
    claim: Duration,
) {
    refused_read(
        Settings::default(),
        Settings::default(),
        &[OPUS, SONNET],
        |h| {
            h.manager.mark_result(&CallResult {
                credential_scope: true,
                ..result.clone()
            });
            let state = h.get(ID).model_states.get(OPUS).cloned().expect("opus");
            assert!(
                state.next_retry_after > Some(h.now() + claim + 4 * step),
                "the classified hold outlasts the claim: {state:?}"
            );
            assert!(!blocked(h, SONNET), "the sibling is untouched");
            h.clock.advance(step);
            h.manager.mark_result(&success(SONNET));
            h.clock.advance(step);
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), claim));
            h.clock.advance(step);
        },
        step,
        2.0,
    )
    .await;
}

/// Finding 1: a model-support failure (an explicit `model_not_found` code)
/// that carries the credential-scope flag is held under its model, not under
/// the credential's `""`, which the sibling's success prunes.
#[tokio::test(start_paused = true)]
async fn a_credential_scoped_model_support_failure_is_held_under_its_model() {
    classified_credential_scoped_then_a_claim(
        classified_429(OPUS, "model_not_found", "no such model", 12 * HOUR),
        Duration::from_secs(5),
        HOUR,
    )
    .await;
}

/// The Cloudflare arm, credential-scoped by the same headers: a hold of its
/// own ten seconds, so the whole sequence runs inside it.
#[tokio::test(start_paused = true)]
async fn a_credential_scoped_cloudflare_failure_is_held_under_its_model() {
    classified_credential_scoped_then_a_claim(
        classified_429(OPUS, "", "cloudflare challenge", 12 * HOUR),
        Duration::from_secs(1),
        Duration::from_secs(3),
    )
    .await;
}

/// The invalid-grant arm: never a 429, but a credential-scoped flag can come
/// with it all the same.
#[tokio::test(start_paused = true)]
async fn a_credential_scoped_invalid_grant_failure_is_held_under_its_model() {
    let mut result = other_failure(OPUS, 400, 12 * HOUR);
    if let Some(error) = result.error.as_mut() {
        error.code = "invalid_grant".into();
        error.message = "invalid_grant".into();
    }
    classified_credential_scoped_then_a_claim(
        result,
        Duration::from_secs(5),
        Duration::from_secs(600),
    )
    .await;
}

/// An alias with a 5 day quota cooldown and a target with a newer 1 hour one
/// are recorded without a cap; a check cap of an hour is turned on; a 5 day
/// credential-wide 429 wraps both; the reconcile merges the alias into the
/// target and sets the mark.
fn capped_merge(h: &Harness) {
    h.manager
        .mark_result(&quota_429(ROUTE, opus_headers(), FIVE_DAYS));
    h.clock.advance(Duration::from_secs(5));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), HOUR));
    h.clock.advance(Duration::from_secs(5));
    h.manager.set_settings(alias_settings(HOUR));
    wrap(h, FIVE_DAYS);
    h.clock.advance(Duration::from_secs(5));
    merge_with(h, alias_settings(HOUR));
    h.clock.advance(Duration::from_secs(5));
}

/// Finding 3: 70 minutes on, the cap's check time has passed but the
/// provider's 5 day reset has not; another credential-wide 429 must not drop
/// the mark.
#[tokio::test(start_paused = true)]
async fn a_capped_check_time_that_has_passed_keeps_the_merge_mark() {
    refused_credential_read(
        Settings::default(),
        alias_settings(HOUR),
        &[OPUS, ROUTE, SONNET],
        |h| {
            capped_merge(h);
            h.clock.advance(Duration::from_secs(70 * 60));
            wrap(h, FIVE_DAYS);
        },
        Duration::from_secs(60),
    )
    .await;
}

/// The same, with the second 429 the result of a check that was really
/// leased and dispatched.
#[tokio::test(start_paused = true)]
async fn a_leased_check_that_returns_a_wrap_keeps_the_merge_mark() {
    let mut seen = Vec::new();
    for read in [true, false] {
        let dir = temp_dir();
        let h = saving(dir.path(), Settings::default(), &[OPUS, ROUTE, SONNET]);
        capped_merge(&h);
        h.clock.advance(Duration::from_secs(70 * 60));
        let reset = weekly(h.now());
        let executor = FakeExecutor::with("claude", move |_: &Call| {
            Reply::Err(rejection(reset.clone(), true, FIVE_DAYS))
        });
        h.executor(&executor);
        // The merge's registration listed the alias alone.
        h.models.register(ID, &[OPUS, ROUTE, SONNET]);
        let outcome = call(&h, OPUS).await.map(|_| ());
        settle().await;
        assert_eq!(
            executor.ids(Kind::Execute).len(),
            1,
            "the due check was leased and dispatched: {outcome:?}; checks {:?}; credential {:?}",
            h.manager.quota_checks(ID),
            h.get(ID).model_states
        );
        h.clock.advance(Duration::from_secs(60));
        if read {
            assert!(
                !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)])),
                "the credential's recovery went ahead"
            );
        }
        seen.push(capture(&h, dir.path(), alias_settings(HOUR)).await);
    }
    assert_eq!(seen[0], seen[1], "a refused read changed something");
}

/// The control: every layer the merge touched, the alias's 5 days included,
/// has run out before the next credential-wide 429, so a new wrap may recover.
#[tokio::test(start_paused = true)]
async fn a_capped_wrap_after_every_layer_ran_out_can_recover() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, ROUTE, SONNET]);
    capped_merge(&h);
    h.clock.advance(FIVE_DAYS + HOUR);
    wrap(&h, 2 * HOUR);
    h.clock.advance(Duration::from_secs(20 * 60));
    assert!(
        recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)])),
        "a new wrap, made after everything the merge touched ran out"
    );
    assert!(!blocked(&h, SONNET), "the new wrap is gone");
    assert!(!blocked(&h, OPUS));
}

/// Finding 4: the same merge without a cap, then `failure` names no model.
/// 70 minutes on, the failure's own hold and the target's hour have run out
/// but the alias's 5 days have not; another credential-wide 429 must not make
/// the credential recoverable.
async fn merged_wrap_then_a_no_model_failure(failure: CallResult) {
    refused_credential_read(
        Settings::default(),
        route_alias(),
        &[OPUS, ROUTE, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(ROUTE, opus_headers(), FIVE_DAYS));
            h.clock.advance(Duration::from_secs(5));
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), HOUR));
            h.clock.advance(Duration::from_secs(5));
            wrap(h, FIVE_DAYS);
            h.clock.advance(Duration::from_secs(5));
            merge_with(h, route_alias());
            h.clock.advance(Duration::from_secs(5));
            h.manager.mark_result(&failure);
            h.clock.advance(Duration::from_secs(70 * 60));
            wrap(h, FIVE_DAYS);
        },
        Duration::from_secs(60),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_no_model_cloudflare_failure_keeps_the_merge_mark() {
    let mut failure = other_failure("", 429, Duration::from_secs(90));
    if let Some(error) = failure.error.as_mut() {
        error.message = "cloudflare challenge".into();
    }
    merged_wrap_then_a_no_model_failure(failure).await;
}

#[tokio::test(start_paused = true)]
async fn a_no_model_invalid_grant_failure_keeps_the_merge_mark() {
    let mut failure = other_failure("", 400, Duration::from_secs(90));
    if let Some(error) = failure.error.as_mut() {
        error.code = "invalid_grant".into();
        error.message = "invalid_grant".into();
    }
    merged_wrap_then_a_no_model_failure(failure).await;
}

/// Finding 2(a): an update whose model states carry two keys of one model
/// under a credential-wide wrap. Normalization keeps the newer key's expired
/// shadow in place of the model's own live hour, so the credential's recovery
/// would lift the wrap onto it.
#[tokio::test(start_paused = true)]
async fn an_update_that_merges_two_keys_of_one_model_under_a_wrap_refuses_the_recovery() {
    let twin_key = format!("{OPUS}(8192)");
    refused_credential_read(
        Settings::default(),
        Settings::default(),
        &[OPUS, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), HOUR));
            h.clock.advance(Duration::from_secs(5));
            wrap(h, FIVE_DAYS);
            h.clock.advance(Duration::from_secs(5));
            let mut incoming = (*h.get(ID)).clone();
            let mut twin = incoming.model_states[OPUS].clone();
            twin.updated_at = Some(h.now() + TimeDelta::seconds(1));
            twin.credential_quota_shadow = Some(Box::new(crate::auth::ModelState::default()));
            incoming.model_states.insert(twin_key.clone(), twin);
            h.manager.update_unsaved(incoming).expect("update");
            let merged = h.get(ID);
            assert!(!merged.model_states.contains_key(&twin_key), "merged");
            assert!(
                merged.model_states[OPUS]
                    .credential_quota_shadow
                    .as_ref()
                    .is_some_and(|shadow| shadow.next_retry_after.is_none()),
                "the newer key's expired shadow was kept"
            );
        },
        Duration::from_secs(20 * 60),
    )
    .await;
}

/// Finding 2(b): a read in flight across an update that publishes a longer
/// Opus cooldown. The credential version is the same, so the token matched.
#[tokio::test(start_paused = true)]
async fn a_read_in_flight_across_an_update_of_the_opus_cooldown_is_fenced() {
    let mut seen = Vec::new();
    for read in [true, false] {
        let dir = temp_dir();
        let h = saving(dir.path(), Settings::default(), &[OPUS, SONNET]);
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), HOUR));
        let token = h.manager.quota_recovery_token(ID).expect("token");
        h.clock.advance(Duration::from_secs(5));
        let end = h.now() + TimeDelta::days(10);
        let mut longer = (*h.get(ID)).clone();
        let state = longer.model_states.get_mut(OPUS).expect("opus state");
        state.next_retry_after = Some(end);
        state.quota.next_recover_at = Some(end);
        h.manager.update_unsaved(longer).expect("update");
        h.clock.advance(Duration::from_secs(55));
        if read {
            let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
            assert!(
                h.manager
                    .apply_quota_usage_recovery(&token, &healthy)
                    .expect("apply")
                    .is_empty(),
                "a read from before the update ended the longer cooldown"
            );
        }
        seen.push(capture(&h, dir.path(), Settings::default()).await);
    }
    assert_eq!(seen[0], seen[1], "a refused read changed something");
}

/// An edit or an update that leaves every cooldown as it is does not touch
/// the normal recovery: a label, an update of the live credential's own copy,
/// and an update with no model states (the live ones are kept).
#[tokio::test(start_paused = true)]
async fn an_edit_or_update_that_leaves_the_cooldowns_alone_still_lets_the_recovery_work() {
    for variant in 0..3 {
        let h = Harness::new(Settings::default());
        h.add(auth(ID, "claude"), &[OPUS, SONNET]);
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
        h.clock.advance(Duration::from_secs(5));
        match variant {
            0 => {
                h.manager
                    .edit(ID, |auth| {
                        auth.label = "renamed".into();
                        Ok::<_, ()>(true)
                    })
                    .expect("edit");
            }
            1 => {
                let mut copy = (*h.get(ID)).clone();
                copy.label = "renamed".into();
                h.manager.update_unsaved(copy).expect("update");
            }
            _ => {
                let mut bare = (*h.get(ID)).clone();
                bare.label = "renamed".into();
                bare.model_states.clear();
                h.manager.update_unsaved(bare).expect("update");
            }
        }
        h.clock.advance(Duration::from_secs(55));
        assert!(blocked(&h, OPUS), "variant {variant}");
        assert!(
            recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
            "variant {variant}: the normal recovery was refused"
        );
        assert!(!blocked(&h, OPUS), "variant {variant}");
    }
}

// Round 11: the mark of a merge under a wrap keeps a bound recorded when the
// merge happened, which later outcomes (a check's answer taking the rest away,
// an unwrapping onto a shorter shadow) can't erase.

/// What a refused read must leave alone, and that the restored state selects
/// and dispatches as the live one does.
fn assert_restart_agrees(captured: &str) {
    let selection: Vec<&str> = captured
        .lines()
        .filter(|line| line.trim_start().starts_with("selection"))
        .collect();
    assert_eq!(selection.len(), 2, "{captured}");
    assert_eq!(selection[0], selection[1], "restored selects differently");
    let reached = captured
        .lines()
        .find(|line| line.starts_with("reached live"))
        .expect("reached line");
    let counts: Vec<&str> = reached.split_whitespace().collect();
    assert_eq!(counts[2], counts[4], "restored dispatches differently");
}

/// The capped merge, then 70 minutes on, a due check is really leased and
/// dispatched and `answer` comes back; the answer is taken for a quota answer
/// by the check, which takes the five day rest away. Twenty seconds later
/// another credential-wide 429 arrives, and a read a minute on shows Opus full.
async fn a_check_answer_erases_the_rest(message: &'static str) {
    let mut seen = Vec::new();
    for read in [true, false] {
        let dir = temp_dir();
        let h = saving(dir.path(), Settings::default(), &[OPUS, ROUTE, SONNET]);
        capped_merge(&h);
        h.clock.advance(Duration::from_secs(70 * 60));
        let mut answer = rejection(weekly(h.now()), true, Duration::from_secs(10));
        answer.message = message.into();
        let executor = FakeExecutor::with("claude", move |_: &Call| Reply::Err(answer.clone()));
        h.executor(&executor);
        h.models.register(ID, &[OPUS, ROUTE, SONNET]);
        let outcome = call(&h, OPUS).await.map(|_| ());
        settle().await;
        assert_eq!(
            executor.ids(Kind::Execute).len(),
            1,
            "the due check was leased and dispatched: {outcome:?}; checks {:?}",
            h.manager.quota_checks(ID)
        );
        h.clock.advance(Duration::from_secs(20));
        wrap(&h, FIVE_DAYS);
        h.clock.advance(Duration::from_secs(60));
        if read {
            assert!(
                !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)])),
                "the credential's recovery went ahead while the alias's five days run"
            );
        }
        seen.push(capture(&h, dir.path(), alias_settings(HOUR)).await);
    }
    assert_eq!(seen[0], seen[1], "a refused read changed something");
    assert_restart_agrees(&seen[0]);
}

#[tokio::test(start_paused = true)]
async fn a_cloudflare_answer_to_a_check_that_erases_the_rest_keeps_the_merge_mark() {
    a_check_answer_erases_the_rest("cloudflare challenge").await;
}

#[tokio::test(start_paused = true)]
async fn a_model_support_answer_to_a_check_that_erases_the_rest_keeps_the_merge_mark() {
    a_check_answer_erases_the_rest(
        "{\"error\":{\"code\":\"model_not_found\",\"message\":\"no such model\"}}",
    )
    .await;
}

/// The reviewer's sequence, with nothing but wraps: an alias with a 120 second
/// Opus cooldown and a target with a newer 10 second one under a 10 second
/// wrap; the merge keeps the alias's 120 seconds in the outer state and the
/// target's 10 in the shadow; a second wrap unwraps onto that shadow and a
/// third finds nothing live. The alias's 120 seconds still run.
#[tokio::test(start_paused = true)]
async fn unwrapping_onto_a_shorter_shadow_keeps_the_merge_mark() {
    let secs = Duration::from_secs;
    refused_credential_read(
        Settings::default(),
        route_alias(),
        &[OPUS, ROUTE, SONNET],
        |h| {
            h.manager
                .mark_result(&quota_429(ROUTE, opus_headers(), secs(120)));
            h.clock.advance(secs(1));
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), secs(10)));
            wrap(h, secs(10));
            h.clock.advance(secs(1));
            merge_with(h, route_alias());
            h.clock.advance(secs(10));
            wrap(h, secs(10));
            h.clock.advance(secs(11));
            wrap(h, secs(10));
        },
        secs(5),
    )
    .await;
}

/// The control: the same, but the read comes once the bound (the alias's five
/// days) has passed and nothing is live, so a new wrap recovers.
#[tokio::test(start_paused = true)]
async fn once_the_bound_has_passed_an_erased_rest_no_longer_refuses() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, ROUTE, SONNET]);
    capped_merge(&h);
    h.clock.advance(Duration::from_secs(70 * 60));
    wrap(&h, 2 * HOUR);
    h.clock.advance(FIVE_DAYS + HOUR);
    wrap(&h, 2 * HOUR);
    h.clock.advance(Duration::from_secs(20 * 60));
    assert!(
        recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)])),
        "everything the merge touched ran out"
    );
    assert!(!blocked(&h, OPUS));
    assert!(!blocked(&h, SONNET));
}

/// A refresh that returns the credential with `changed` applied to its Opus
/// state while a usage read taken before it is in flight; `changed` false is
/// the control.
async fn refresh_around_a_read(changed: bool) {
    let mut seen = Vec::new();
    for read in [true, false] {
        let dir = temp_dir();
        let h = saving(dir.path(), Settings::default(), &[OPUS, SONNET]);
        let executor = FakeExecutor::new("claude");
        let clock = h.clock.clone();
        executor.set_refresh(move |auth: &Auth| {
            let mut refreshed = auth.clone();
            refreshed
                .metadata
                .insert("access_token".into(), serde_json::json!("fresh"));
            if changed {
                let end = clock.now() + TimeDelta::days(10);
                let state = refreshed.model_states.get_mut(OPUS).expect("opus state");
                state.next_retry_after = Some(end);
                state.quota.next_recover_at = Some(end);
            }
            Ok(refreshed)
        });
        h.executor(&executor);
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), HOUR));
        let token = h.manager.quota_recovery_token(ID).expect("token");
        h.clock.advance(Duration::from_secs(5));
        executor.set_refresh_delay(Duration::from_secs(1));
        let manager = h.manager.clone();
        let task = tokio::spawn(async move { manager.force_refresh(ID).await });
        settle().await;
        assert_eq!(executor.refresh_count(), 1, "the refresh started");
        tokio::time::advance(Duration::from_secs(1)).await;
        task.await.expect("task").expect("refreshed");
        h.clock.advance(Duration::from_secs(55));
        let held = h
            .get(ID)
            .model_states
            .get(OPUS)
            .is_some_and(|state| state.next_retry_after > Some(h.now() + TimeDelta::days(9)));
        assert_eq!(held, changed, "the refresh published the longer cooldown");
        if read {
            let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
            // A read taken before the refresh is fenced by it either way.
            let stale = h
                .manager
                .apply_quota_usage_recovery(&token, &healthy)
                .expect("apply");
            assert!(
                stale.is_empty(),
                "a read from before the refresh ended {stale:?}"
            );
            // One taken after it recovers only if the refresh left the
            // cooldowns as they were.
            assert_eq!(
                recover(&h, &healthy),
                !changed,
                "a read from after the refresh {}",
                if changed {
                    "ended the longer cooldown"
                } else {
                    "was refused"
                }
            );
        }
        seen.push(capture(&h, dir.path(), Settings::default()).await);
    }
    if changed {
        assert_eq!(seen[0], seen[1], "a refused read changed something");
        assert_restart_agrees(&seen[0]);
    } else {
        assert!(!blocked_in(&seen[0]), "the control recovered Opus");
    }
}

/// Whether the capture's live selection blocks Opus.
fn blocked_in(captured: &str) -> bool {
    captured
        .lines()
        .find(|line| line.trim_start().starts_with("selection"))
        .is_some_and(|line| line.contains("opus true"))
}

#[tokio::test(start_paused = true)]
async fn a_refresh_that_changes_no_cooldown_leaves_a_read_in_flight_valid() {
    refresh_around_a_read(false).await;
}

#[tokio::test(start_paused = true)]
async fn a_refresh_that_changes_the_opus_cooldown_fences_a_read_in_flight() {
    refresh_around_a_read(true).await;
}

// Round 12: while a credential's merge mark stands every recovery on it is
// refused (a model's as well); the mark survives the callers of `forget` that
// don't end the cooldowns, and a restart; a live hold of the credential isn't
// dropped because aggregation rewrote the live fields; and a refused read
// moves no rest.

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The credential's merge mark: none, or its bound (`None` inside: unknown).
fn mark_of(h: &Harness) -> Option<Option<Timestamp>> {
    h.manager.lock().quota_recovery.mark(ID)
}

/// An alias with a 120 second Opus cooldown, a target with a newer 10 second
/// one, both wrapped for 10 seconds, merged at the 2 second mark.
fn shadow_merge(h: &Harness) {
    h.manager
        .mark_result(&quota_429(ROUTE, opus_headers(), secs(120)));
    h.clock.advance(secs(1));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), secs(10)));
    wrap(h, secs(10));
    h.clock.advance(secs(1));
    merge_with(h, route_alias());
}

/// [`shadow_merge`], then two more 10 second wraps that unwrap onto the
/// shadow and find nothing live: the third ends at 33 seconds, the alias's
/// 120 seconds are the mark's bound.
fn three_wraps(h: &Harness) {
    shadow_merge(h);
    h.clock.advance(secs(10));
    wrap(h, secs(10));
    h.clock.advance(secs(11));
    wrap(h, secs(10));
}

/// Finding 1: a model-support failure and a five-hour 429 of Opus after the
/// merge; the credential's recovery is refused, and so is Opus's own.
#[tokio::test(start_paused = true)]
async fn later_failures_of_a_merged_model_keep_refusing_the_credentials_recovery() {
    for opus in [100.0, 2.0] {
        refused_read(
            Settings::default(),
            route_alias(),
            &[OPUS, ROUTE, SONNET],
            |h| {
                shadow_merge(h);
                h.clock.advance(secs(10));
                h.manager.mark_result(&classified_429(
                    OPUS,
                    "model_not_found",
                    "no such model",
                    secs(10),
                ));
                h.clock.advance(secs(11));
                h.manager
                    .mark_result(&quota_429(OPUS, five_hour_headers(), secs(10)));
            },
            secs(5),
            opus,
        )
        .await;
    }
}

/// Finding 1, model side: with the mark standing a model's recovery is refused
/// too, though its own claim is proven and nothing but the mark stands in the
/// way. The control: the bound has passed.
#[tokio::test(start_paused = true)]
async fn a_merge_mark_refuses_a_models_recovery_until_its_bound_passes() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, ROUTE, SONNET]);
    three_wraps(&h);
    h.clock.advance(secs(11));
    assert!(mark_of(&h).is_some_and(|bound| bound.is_some()));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), secs(100)));
    h.clock.advance(secs(5));
    assert!(
        !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "a model recovered under the credential's mark"
    );
    assert!(blocked(&h, OPUS));
    h.clock.advance(secs(200));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), secs(100)));
    h.clock.advance(secs(5));
    assert!(
        recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        "the mark's bound has passed"
    );
    assert!(!blocked(&h, OPUS));
}

/// A short Sonnet-only cooldown published by an update that changes the
/// cooldowns (the caller that used to take the mark with it).
fn update_with_a_sonnet_cooldown(h: &Harness) {
    let mut updated = Auth::clone(&h.get(ID));
    let end = h.now() + TimeDelta::seconds(5);
    updated.model_states.insert(
        SONNET.into(),
        crate::auth::ModelState {
            status: crate::auth::Status::Error,
            unavailable: true,
            next_retry_after: Some(end),
            ..crate::auth::ModelState::default()
        },
    );
    h.manager.update_unsaved(updated).expect("update");
}

/// Finding 2: an update that stores other cooldowns keeps the mark; a short
/// Sonnet cooldown runs out and a Sonnet success prunes it; a credential-wide
/// 429 and a read that shows Opus full are refused.
#[tokio::test(start_paused = true)]
async fn an_update_of_other_cooldowns_keeps_the_merge_mark_and_its_refusal() {
    refused_credential_read(
        Settings::default(),
        route_alias(),
        &[OPUS, ROUTE, SONNET],
        |h| {
            three_wraps(h);
            h.clock.advance(secs(11));
            h.manager.reconcile_registry_model_states(ID);
            update_with_a_sonnet_cooldown(h);
            assert!(mark_of(h).is_some_and(|bound| bound.is_some()), "kept");
            h.clock.advance(secs(6));
            h.manager.mark_result(&success(SONNET));
            wrap(h, secs(10));
        },
        secs(5),
    )
    .await;
}

/// Finding 2, per caller: what each does to the mark.
#[tokio::test(start_paused = true)]
async fn each_forget_caller_treats_the_merge_mark_as_it_should() {
    let setup = || {
        let h = Harness::new(Settings::default());
        h.add(auth(ID, "claude"), &[OPUS, ROUTE, SONNET]);
        three_wraps(&h);
        h.clock.advance(secs(11));
        h
    };
    // Registering a credential again and updating it keep a finite mark.
    let h = setup();
    let bound = mark_of(&h).expect("mark");
    assert!(bound.is_some());
    h.manager
        .register_unsaved(auth(ID, "claude"))
        .expect("register");
    assert_eq!(mark_of(&h), Some(bound), "register");
    update_with_a_sonnet_cooldown(&h);
    assert_eq!(mark_of(&h), Some(bound), "update");
    // The user's reset keeps a finite mark and ends an unknown one.
    let h = setup();
    h.manager.reset_quota(ID).expect("reset");
    assert_eq!(mark_of(&h), Some(bound), "reset keeps a finite mark");
    h.manager.lock().quota_recovery.restore_mark(ID, None);
    assert_eq!(mark_of(&h), Some(None));
    h.manager.reset_quota(ID).expect("reset");
    assert_eq!(mark_of(&h), None, "reset ends an unknown mark");
    // Removing the credential ends it.
    let h = setup();
    h.manager.remove(ID);
    assert_eq!(mark_of(&h), None, "remove");
}

/// The short-shadow state at 34 seconds on a saving harness: the mark stands,
/// nothing is live.
fn marked_and_idle(dir: &Path) -> Harness {
    let h = saving(dir, Settings::default(), &[OPUS, ROUTE, SONNET]);
    three_wraps(&h);
    h.clock.advance(secs(11));
    assert!(mark_of(&h).is_some_and(|bound| bound.is_some()));
    let current = h.get(ID);
    assert!(
        current.next_retry_after.is_none_or(|end| end <= h.now())
            && !blocked(&h, OPUS)
            && !blocked(&h, SONNET),
        "nothing live: {:?} {:?}",
        current.next_retry_after,
        current.model_states
    );
    h
}

/// A fresh process reading what `h` flushed to `dir`.
fn restarted(h: &Harness, dir: &Path) -> Harness {
    flush(&h.manager);
    let next = Harness::new(Settings::default());
    next.add(credential(dir), &[OPUS, SONNET]);
    next.clock
        .advance((h.now() - next.now()).to_std().unwrap_or_default());
    let on = config(dir, true);
    reconfigure(&next.manager, None, &on);
    restore(&next.manager, &on);
    next
}

/// Another credential-wide weekly 429, then a read that shows Opus full.
fn wrap_then_read(h: &Harness) -> bool {
    wrap(h, secs(10));
    h.clock.advance(secs(5));
    recover(h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)]))
}

/// Finding 3: the mark is written while it stands with no live cooldown, in a
/// field of its own; a restart gives the same refusal.
#[tokio::test(start_paused = true)]
async fn a_restart_keeps_the_merge_mark_and_its_refusal() {
    use crate::manager::cooldown_store::Refusal;
    let dir = temp_dir();
    let h = marked_and_idle(dir.path());
    let bound = mark_of(&h).flatten().expect("finite bound");
    let next = restarted(&h, dir.path());
    let own = saved_own(dir.path()).expect("the credential's record is written");
    assert_eq!(own.recovery_refused, Some(Refusal::Until(bound)));
    assert!(own.next_retry_after.is_none(), "no cooldown in the record");
    assert_eq!(mark_of(&next), Some(Some(bound)), "restored");
    assert!(!next.get(ID).unavailable && !blocked(&next, OPUS));
    assert!(!wrap_then_read(&next), "the restored mark refused nothing");
    assert!(!wrap_then_read(&h));
    assert_restart_agrees(&capture(&h, dir.path(), Settings::default()).await);
}

/// Finding 3 through a thaw that makes the file the authority.
#[tokio::test(start_paused = true)]
async fn a_thaw_that_restores_the_file_brings_the_merge_mark_back() {
    use crate::manager::cooldown_store::{Thaw, freeze, thaw};
    let dir = temp_dir();
    let h = marked_and_idle(dir.path());
    let bound = mark_of(&h).expect("mark");
    freeze(&h.manager).expect("freeze");
    thaw(&h.manager, Thaw::Restore);
    assert_eq!(mark_of(&h), Some(bound), "restored by the thaw");
    assert!(!wrap_then_read(&h), "the restored mark refused nothing");
    assert_restart_agrees(&capture(&h, dir.path(), Settings::default()).await);
}

/// Finding 3: a mark of unknown bound is saved as such and comes back.
#[tokio::test(start_paused = true)]
async fn an_unknown_bound_survives_a_restart() {
    use crate::manager::cooldown_store::Refusal;
    let dir = temp_dir();
    let h = saving(dir.path(), Settings::default(), &[OPUS, ROUTE, SONNET]);
    h.manager.lock().quota_recovery.restore_mark(ID, None);
    h.manager.mark_result(&other_failure(SONNET, 503, secs(5)));
    h.clock.advance(secs(10));
    let next = restarted(&h, dir.path());
    let own = saved_own(dir.path()).expect("the credential's record");
    assert_eq!(own.recovery_refused, Some(Refusal::Unknown));
    assert_eq!(mark_of(&next), Some(None));
    assert!(!wrap_then_read(&next));
    assert!(!wrap_then_read(&h));
}

/// The control: the bound passed before the restart, so nothing is saved and
/// the restored credential recovers.
#[tokio::test(start_paused = true)]
async fn a_bound_that_passed_before_the_restart_is_not_saved() {
    let dir = temp_dir();
    let h = marked_and_idle(dir.path());
    h.clock.advance(secs(200));
    h.manager.mark_result(&other_failure(SONNET, 503, secs(5)));
    h.clock.advance(secs(10));
    let next = restarted(&h, dir.path());
    assert!(
        FileStore::new(dir.path().to_path_buf())
            .load()
            .expect("load")
            .iter()
            .all(|record| record.recovery_refused.is_none()),
        "a passed bound was saved"
    );
    assert_eq!(mark_of(&next), None);
    assert!(wrap_then_read(&next), "nothing stands in the way");
}

/// A file written without the field (upstream's, or an earlier open-ferry's)
/// reads with no mark, and a file with it reads as written, in both forms.
#[tokio::test(start_paused = true)]
async fn the_mark_field_is_optional_in_a_saved_file() {
    use crate::manager::cooldown_store::Refusal;
    let dir = temp_dir();
    let h = saving(dir.path(), Settings::default(), &[OPUS, SONNET]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), HOUR));
    flush(&h.manager);
    let name = files(dir.path())
        .into_iter()
        .find(|name| name.ends_with(".cds"))
        .expect("a cooldown file");
    let text = std::fs::read_to_string(dir.path().join(&name)).expect("read");
    assert!(
        !text.contains("open_ferry_recovery_refused"),
        "a credential without a mark writes no field: {text}"
    );
    let next = restarted(&h, dir.path());
    assert_eq!(mark_of(&next), None);
    assert!(blocked(&next, OPUS), "the cooldown itself came back");

    for (value, expected) in [
        ("\"unknown\"", Some(Refusal::Unknown)),
        ("\"Unknown\"", Some(Refusal::Unknown)),
        ("7", None),
        ("null", None),
    ] {
        let other = temp_dir();
        std::fs::write(
            other.path().join("a.cds"),
            format!(
                r#"{{"Records":[{{"AUTH_ID":"{ID}","open_ferry_recovery_refused_until":{value}}}]}}"#
            ),
        )
        .expect("write");
        let loaded = FileStore::new(other.path().to_path_buf())
            .load()
            .expect("load");
        let read = loaded.first().and_then(|record| record.recovery_refused);
        assert_eq!(read, expected, "{value}");
    }
    let dated = temp_dir();
    std::fs::write(
        dated.path().join("a.cds"),
        format!(
            r#"{{"Records":[{{"AUTH_ID":"{ID}","open_ferry_recovery_refused_until":"2099-01-01T00:00:00Z"}}]}}"#
        ),
    )
    .expect("write");
    let loaded = FileStore::new(dated.path().to_path_buf())
        .load()
        .expect("load");
    assert!(
        matches!(
            loaded.first().and_then(|record| record.recovery_refused),
            Some(Refusal::Until(_))
        ),
        "{loaded:?}"
    );
}

/// Finding 4: a one hour no-model 503, a ten second 503 of Sonnet (the
/// aggregation drops the credential's live flags), then a five day quota 429
/// of Opus; a healthy read must not end the credential's hold. With
/// `sonnet_503` false the flags stay and the hold holds as before.
async fn a_foreign_hold_under_an_aggregation(sonnet_503: bool) {
    refused_read(
        Settings::default(),
        Settings::default(),
        &[OPUS, SONNET],
        |h| {
            h.manager.mark_result(&success(OPUS));
            h.manager.mark_result(&success(SONNET));
            h.clock.advance(secs(1));
            h.manager.mark_result(&other_failure("", 503, HOUR));
            h.clock.advance(secs(5));
            if sonnet_503 {
                h.manager.mark_result(&other_failure(SONNET, 503, secs(10)));
                h.clock.advance(secs(5));
            }
            h.manager
                .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
        },
        secs(5),
        2.0,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn an_aggregation_that_clears_the_credentials_flags_does_not_end_its_hold() {
    a_foreign_hold_under_an_aggregation(true).await;
}

#[tokio::test(start_paused = true)]
async fn a_credential_hold_that_stays_live_still_refuses_a_models_recovery() {
    a_foreign_hold_under_an_aggregation(false).await;
}

/// Finding 5: Opus rests on a one hour check under a five day 429, a two hour
/// credential wrap follows, and the registry drops Opus: the reconcile removes
/// its state and leaves the rest. A refused read moves nothing.
#[tokio::test(start_paused = true)]
async fn a_refused_read_leaves_an_orphaned_rest_alone() {
    for opus in [100.0, 2.0] {
        refused_read(
            capped_hour(),
            capped_hour(),
            &[OPUS, SONNET],
            |h| {
                h.manager
                    .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
                h.clock.advance(secs(5));
                wrap(h, 2 * HOUR);
                h.models.register(ID, &[SONNET]);
                h.manager.reconcile_registry_model_states(ID);
                assert!(!h.get(ID).model_states.contains_key(OPUS));
                assert!(!rested(h).is_empty(), "the rest is orphaned, not gone");
            },
            secs(5),
            opus,
        )
        .await;
    }
}

fn state_until(until: Option<Timestamp>) -> crate::auth::ModelState {
    crate::auth::ModelState {
        status: crate::auth::Status::Error,
        unavailable: true,
        next_retry_after: until,
        ..crate::auth::ModelState::default()
    }
}

fn base() -> Timestamp {
    chrono::DateTime::from_timestamp(1_800_000_000, 0).expect("time")
}

fn after(n: i64) -> Timestamp {
    base() + TimeDelta::seconds(n)
}

#[test]
fn the_merge_bound_counts_every_layer() {
    use crate::manager::quota_recovery::merge_bound;
    let mut credential = auth(ID, "claude");
    assert_eq!(merge_bound(&credential, None, base()), Some(base()));

    // The credential's own deadline.
    credential.unavailable = true;
    credential.next_retry_after = Some(after(30));
    assert_eq!(merge_bound(&credential, None, base()), Some(after(30)));
    credential.unavailable = false;
    credential.next_retry_after = None;

    // Nested shadows count, the latest layer wins.
    let mut outer = state_until(Some(after(10)));
    let mut shadow = state_until(Some(after(100)));
    shadow.credential_quota_shadow = Some(Box::new(state_until(Some(after(250)))));
    outer.credential_quota_shadow = Some(Box::new(shadow));
    credential.model_states.insert("m".into(), outer);
    assert_eq!(merge_bound(&credential, None, base()), Some(after(250)));

    // A key that isn't canonical counts like any other.
    credential.model_states.clear();
    credential
        .model_states
        .insert("  Claude-OPUS-4  ".into(), state_until(Some(after(50))));
    credential
        .model_states
        .insert("claude-opus-4".into(), state_until(Some(after(30))));
    assert_eq!(merge_bound(&credential, None, base()), Some(after(50)));

    // The retry and the quota deadlines of one state differ: the later wins.
    let mut both = state_until(Some(after(10)));
    both.quota.exceeded = true;
    both.quota.next_recover_at = Some(after(60));
    credential.model_states.clear();
    credential.model_states.insert("m".into(), both);
    assert_eq!(merge_bound(&credential, None, base()), Some(after(60)));

    // A rest's reset counts when it is the latest; ended layers count for
    // nothing.
    assert_eq!(
        merge_bound(&credential, Some(after(90)), base()),
        Some(after(90))
    );
    assert_eq!(merge_bound(&credential, None, after(500)), Some(after(500)));
}

#[test]
fn a_layer_that_blocks_with_no_end_makes_the_bound_unknown() {
    use crate::manager::quota_recovery::merge_bound;
    let mut credential = auth(ID, "claude");
    credential
        .model_states
        .insert("m".into(), state_until(Some(after(10))));
    credential
        .model_states
        .insert("n".into(), state_until(None));
    assert_eq!(merge_bound(&credential, Some(after(5)), base()), None);
    // Only under a shadow too.
    credential.model_states.remove("n");
    let mut outer = state_until(Some(after(10)));
    outer.credential_quota_shadow = Some(Box::new(state_until(None)));
    credential.model_states.insert("m".into(), outer);
    assert_eq!(merge_bound(&credential, None, base()), None);
}

#[test]
fn repeated_merges_keep_the_latest_finite_bound_and_unknown_wins() {
    use crate::manager::quota_recovery::Recoveries;
    let mut recoveries = Recoveries::default();
    recoveries.merged(ID, "a", "b", Some(after(100)));
    recoveries.merged(ID, "a", "b", Some(after(50)));
    assert_eq!(recoveries.mark(ID), Some(Some(after(100))), "kept the max");
    recoveries.merged(ID, "a", "b", Some(after(200)));
    assert_eq!(recoveries.mark(ID), Some(Some(after(200))));
    recoveries.merged(ID, "a", "b", None);
    assert_eq!(recoveries.mark(ID), Some(None));
    recoveries.merged(ID, "a", "b", Some(after(900)));
    assert_eq!(recoveries.mark(ID), Some(None), "unknown stays unknown");
}

/// A one hour no-model 503 hold, `between` run (a success of a model), then a
/// recognised five day 429 of Opus; whether a fresh read recovers Opus, and
/// whether the credential's own flags were blocked just before the 429.
fn hold_then_success_then_claim(between: impl Fn(&Harness)) -> (bool, bool) {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager.mark_result(&success(OPUS));
    h.manager.mark_result(&success(SONNET));
    h.clock.advance(secs(1));
    h.manager.mark_result(&other_failure("", 503, HOUR));
    h.clock.advance(secs(5));
    between(&h);
    h.clock.advance(secs(5));
    let flags = h.get(ID).unavailable;
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    h.clock.advance(secs(5));
    (
        recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
        flags,
    )
}

/// A success on any model ends a hold whose scope it leaves unblocked: for the
/// credential's own hold, a sibling's success lets the aggregation clear the
/// credential's flags. The control: a success that leaves the credential
/// blocked (its other model is cooling) ends nothing, and the read is refused.
#[tokio::test(start_paused = true)]
async fn a_sibling_success_that_unblocks_the_credential_ends_its_hold() {
    let (recovered, flags) = hold_then_success_then_claim(|h| {
        h.manager.mark_result(&success(SONNET));
    });
    assert!(
        !flags,
        "the sibling's success cleared the credential's flags"
    );
    assert!(recovered, "the hold ended with the credential unblocked");

    let (recovered, flags) = hold_then_success_then_claim(|h| {
        // A credential-wide quota wrap keeps the credential blocked past a
        // success of Opus.
        wrap(h, HOUR);
        h.manager.mark_result(&success(OPUS));
    });
    assert!(flags, "the credential stayed blocked");
    assert!(
        !recovered,
        "the hold stayed while the credential was blocked"
    );
}

// Round 13: a recovery changes exactly the scopes it recovers; the mark is
// saved whatever the credential's state; a rejected success ends nothing; an
// update keeps a hold only `foreign` shows; a time Go can't write costs no
// record.

/// The check rest of `model`, as the management API shows it.
fn rest_of(h: &Harness, model: &str) -> Vec<crate::manager::QuotaCheck> {
    h.manager
        .quota_checks(ID)
        .into_iter()
        .filter(|check| check.model_key == model)
        .collect()
}

/// Finding 1: Opus rests on a one hour check under a long 429, Sonnet gets its
/// own a half hour later, a reconcile that merges nothing resets Opus's state
/// to clean once its capped time has passed, and a read proves Sonnet healthy
/// with Opus full. Sonnet's recovery leaves Opus's rest, its next check and
/// its doubling be. The control: Opus's reset has passed, so its rest goes.
#[tokio::test(start_paused = true)]
async fn recovering_one_model_leaves_another_models_rest_alone() {
    for (opus_retry, minutes, stays) in [(FIVE_DAYS, 31, true), (secs(70 * 60), 45, false)] {
        let h = Harness::new(capped_hour());
        h.add(auth(ID, "claude"), &[OPUS, SONNET]);
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), opus_retry));
        h.clock.advance(secs(30 * 60));
        h.manager
            .mark_result(&quota_429(SONNET, sonnet_headers(), FIVE_DAYS));
        h.clock.advance(secs(minutes * 60));
        h.manager.reconcile_registry_model_states(ID);
        let opus_before = rest_of(&h, OPUS);
        assert_eq!(opus_before.len(), usize::from(stays), "{opus_before:?}");
        let read = reading(
            h.now(),
            &[
                (UsageWindow::SevenDayOpus, 100.0),
                (UsageWindow::SevenDaySonnet, 3.0),
            ],
        );
        assert!(recover(&h, &read), "Sonnet recovered");
        assert_eq!(rest_of(&h, SONNET), [], "Sonnet's own rest went");
        if stays {
            assert_eq!(rest_of(&h, OPUS), opus_before, "Opus's rest was touched");
            assert_eq!(rested(&h), [OPUS]);
        } else {
            assert_eq!(rested(&h), Vec::<String>::new(), "the ended rest stayed");
        }
    }
}

/// Finding 2: a mark that stands while the credential is disabled, or while
/// cooling is off, is saved and comes back through a restart.
#[tokio::test(start_paused = true)]
async fn a_disabled_or_cooling_off_credentials_mark_is_saved_and_restored() {
    use crate::manager::cooldown_store::Refusal;
    for cooling_off in [false, true] {
        let dir = temp_dir();
        let h = marked_and_idle(dir.path());
        let bound = mark_of(&h).flatten().expect("finite bound");
        if cooling_off {
            h.manager.set_settings(Settings {
                disable_cooling: true,
                ..route_alias()
            });
        } else {
            set_disabled(&h, true);
        }
        assert_eq!(mark_of(&h), Some(Some(bound)), "reset keeps a finite mark");
        let next = restarted(&h, dir.path());
        let own = saved_own(dir.path()).expect("a record for the mark");
        assert_eq!(own.recovery_refused, Some(Refusal::Until(bound)));
        assert!(own.next_retry_after.is_none(), "no cooldown in the record");
        assert_eq!(mark_of(&next), Some(Some(bound)), "restored");
        // Enabled again before the bound passes, both refuse.
        if cooling_off {
            h.manager.set_settings(route_alias());
        } else {
            set_disabled(&h, false);
        }
        assert!(!wrap_then_read(&next), "the restored mark refused nothing");
        assert!(!wrap_then_read(&h), "the live mark refused nothing");
        assert_restart_agrees(&capture(&h, dir.path(), Settings::default()).await);
    }
}

/// [`restarted`] onto a credential that is disabled, or has cooling off.
fn restarted_ineligible(h: &Harness, dir: &Path, cooling_off: bool) -> Harness {
    flush(&h.manager);
    let next = Harness::new(Settings {
        disable_cooling: cooling_off,
        ..Settings::default()
    });
    next.add(credential(dir), &[OPUS, SONNET]);
    if !cooling_off {
        set_disabled(&next, true);
    }
    next.clock
        .advance((h.now() - next.now()).to_std().unwrap_or_default());
    let on = config(dir, true);
    reconfigure(&next.manager, None, &on);
    restore(&next.manager, &on);
    next
}

/// Finding 2, restore side: a saved mark is put back whether or not the
/// credential is eligible for cooldowns when the restore runs, and it makes
/// the credential unavailable for nothing.
#[tokio::test(start_paused = true)]
async fn a_saved_mark_is_restored_onto_a_disabled_or_cooling_off_credential() {
    for cooling_off in [false, true] {
        let dir = temp_dir();
        let h = marked_and_idle(dir.path());
        let bound = mark_of(&h).flatten().expect("finite bound");
        let next = restarted_ineligible(&h, dir.path(), cooling_off);
        assert_eq!(mark_of(&next), Some(Some(bound)), "restored");
        if cooling_off {
            assert!(!blocked(&next, OPUS) && !blocked(&next, SONNET));
        }
        assert!(!next.get(ID).unavailable);
        // Enabled again, the mark still refuses.
        if cooling_off {
            next.manager.set_settings(Settings::default());
        } else {
            set_disabled(&next, false);
        }
        assert!(!wrap_then_read(&next), "the restored mark refused nothing");
    }
}

/// Finding 2 through a thaw that makes the file the authority, while the
/// credential is disabled.
#[tokio::test(start_paused = true)]
async fn a_thaw_restores_the_mark_onto_a_disabled_credential() {
    use crate::manager::cooldown_store::{Thaw, freeze, thaw};
    let dir = temp_dir();
    let h = marked_and_idle(dir.path());
    let bound = mark_of(&h).expect("mark");
    freeze(&h.manager).expect("freeze");
    set_disabled(&h, true);
    thaw(&h.manager, Thaw::Restore);
    assert_eq!(mark_of(&h), Some(bound), "restored by the thaw");
    set_disabled(&h, false);
    assert!(!wrap_then_read(&h), "the restored mark refused nothing");
}

fn set_disabled(h: &Harness, on: bool) {
    h.manager
        .edit(ID, |auth| {
            auth.disabled = on;
            Ok::<_, ()>(true)
        })
        .expect("edit");
}

/// Finding 3: a recognised Opus claim, an unrecognised credential-wide 429 that
/// outlasts it, both deadlines past with nothing in between: a read that
/// proves the claim recovers nothing, and leaves the credential (the same
/// allocation, equal) and the quota checks as they were.
#[tokio::test(start_paused = true)]
async fn a_read_that_recovers_nothing_changes_no_model_state() {
    for settings in [Settings::default(), capped_hour()] {
        let h = Harness::new(settings);
        h.add(auth(ID, "claude"), &[OPUS, SONNET]);
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), secs(100)));
        h.clock.advance(secs(10));
        h.manager.mark_result(&CallResult {
            credential_scope: true,
            ..other_failure(SONNET, 429, secs(200))
        });
        h.clock.advance(secs(300));
        let before = h.get(ID);
        let before_text = format!("{:?}", *before);
        let checks = format!("{:?}", h.manager.lock().quota_checks);
        let read = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
        assert!(!recover(&h, &read), "nothing was recovered");
        let after = h.get(ID);
        assert_eq!(format!("{:?}", *after), before_text, "the credential moved");
        assert!(Arc::ptr_eq(&before, &after), "the credential was copied");
        assert_eq!(format!("{:?}", h.manager.lock().quota_checks), checks);
    }
}

/// Finding 4: the same one hour no-model 503 and ten second Sonnet 503 as the
/// aggregation test, then a Sonnet success that predates the Sonnet cooldown:
/// application rejects it, and it ends no hold. The control has no such
/// success; both refuse the read.
#[tokio::test(start_paused = true)]
async fn a_success_application_rejected_as_stale_ends_no_hold() {
    for stale in [false, true] {
        let h = Harness::new(Settings::default());
        h.add(auth(ID, "claude"), &[OPUS, SONNET]);
        let older = h.manager.next_execution_seq();
        h.manager.mark_result(&success(OPUS));
        h.manager.mark_result(&success(SONNET));
        h.clock.advance(secs(1));
        h.manager.mark_result(&other_failure("", 503, HOUR));
        h.clock.advance(secs(5));
        h.manager.mark_result(&other_failure(SONNET, 503, secs(10)));
        h.clock.advance(secs(3));
        if stale {
            h.manager.mark_result(&CallResult {
                execution_seq: older,
                ..success(SONNET)
            });
            assert!(blocked(&h, SONNET), "the stale success changed Sonnet");
        }
        h.clock.advance(secs(2));
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
        h.clock.advance(secs(5));
        assert!(
            h.manager
                .lock()
                .quota_recovery
                .foreign_live(ID, "", h.now())
        );
        let read = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
        assert!(!recover(&h, &read), "stale {stale}");
        assert!(blocked(&h, OPUS));
    }
}

/// An update of the Sonnet cooldown's deadline, from `h`'s live credential.
fn update_sonnet_deadline(h: &Harness, seconds: i64) {
    let mut updated = Auth::clone(&h.get(ID));
    let end = h.now() + TimeDelta::seconds(seconds);
    let state = updated.model_states.get_mut(SONNET).expect("sonnet state");
    state.next_retry_after = Some(end);
    h.manager.update_unsaved(updated).expect("update");
}

/// Finding 5: the one hour no-model 503 and a ten second Sonnet 503, then an
/// update that changes only Sonnet's deadline: the credential's hold, which
/// only `foreign` still shows, stays. The control: an update that changes no
/// cooldown refuses as well.
#[tokio::test(start_paused = true)]
async fn an_update_of_another_models_cooldown_keeps_a_hold_only_foreign_shows() {
    for changed in [true, false] {
        let h = Harness::new(Settings::default());
        h.add(auth(ID, "claude"), &[OPUS, SONNET]);
        h.manager.mark_result(&success(OPUS));
        h.manager.mark_result(&success(SONNET));
        h.clock.advance(secs(1));
        h.manager.mark_result(&other_failure("", 503, HOUR));
        h.clock.advance(secs(5));
        h.manager.mark_result(&other_failure(SONNET, 503, secs(10)));
        h.clock.advance(secs(1));
        let live = h.get(ID);
        assert!(!live.unavailable, "aggregation hid the credential's hold");
        let live_end = live.model_states[SONNET].next_retry_after;
        if changed {
            update_sonnet_deadline(&h, 20);
            assert_ne!(h.get(ID).model_states[SONNET].next_retry_after, live_end);
        } else {
            let same = Auth::clone(&h.get(ID));
            h.manager.update_unsaved(same).expect("update");
        }
        h.manager
            .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
        h.clock.advance(secs(5));
        let read = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
        assert!(!recover(&h, &read), "changed {changed}");
        assert!(blocked(&h, OPUS));
    }
}

/// Registering the credential again keeps a hold only `foreign` shows; a reset
/// ends it.
#[tokio::test(start_paused = true)]
async fn registering_again_keeps_the_hold_and_a_reset_ends_it() {
    for reset in [false, true] {
        let h = Harness::new(Settings::default());
        h.add(auth(ID, "claude"), &[OPUS, SONNET]);
        h.manager.mark_result(&success(OPUS));
        h.manager.mark_result(&success(SONNET));
        h.clock.advance(secs(1));
        h.manager.mark_result(&other_failure("", 503, HOUR));
        h.clock.advance(secs(5));
        h.manager.mark_result(&other_failure(SONNET, 503, secs(10)));
        h.clock.advance(secs(11));
        if reset {
            h.manager.reset_quota(ID).expect("reset");
        } else {
            h.manager
                .register_unsaved(auth(ID, "claude"))
                .expect("register");
        }
        h.manager
            .mark_result(&as_of_now(&h, quota_429(OPUS, opus_headers(), FIVE_DAYS)));
        h.clock.advance(secs(5));
        let read = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
        assert_eq!(recover(&h, &read), reset, "reset {reset}");
    }
}

/// A saved credential record with a one hour cooldown and `refusals` marks,
/// each in a record of its own; the harness restored from it.
fn restored_with_marks(dir: &Path, refusals: &[&str]) -> Harness {
    let h = Harness::new(Settings::default());
    let end = (h.now() + TimeDelta::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut records = Vec::new();
    for (index, refusal) in refusals.iter().enumerate() {
        let cooldown = if index == 0 {
            format!(r#","next_retry_after":"{end}","status":"cooling","reason":"upstream 503""#)
        } else {
            String::new()
        };
        records.push(format!(
            r#"{{"provider":"claude","auth_id":"{ID}"{cooldown},"open_ferry_recovery_refused_until":{refusal}}}"#
        ));
    }
    std::fs::write(
        dir.join("a.cds"),
        format!(r#"{{"Records":[{}]}}"#, records.join(",")),
    )
    .expect("write");
    h.add(credential(dir), &[OPUS, SONNET]);
    let on = config(dir, true);
    reconfigure(&h.manager, None, &on);
    restore(&h.manager, &on);
    h
}

/// Finding 6: a refusal time that normalises past year 9999 reads as unknown,
/// and the valid one hour cooldown beside it is flushed and survives a restart.
#[tokio::test(start_paused = true)]
async fn an_out_of_range_refusal_time_costs_no_cooldown_record() {
    use crate::manager::cooldown_store::Refusal;
    let dir = temp_dir();
    let h = restored_with_marks(dir.path(), &[r#""9999-12-31T23:59:59-01:00""#]);
    assert!(no_model_blocked(&h), "the cooldown came back");
    assert_eq!(mark_of(&h), Some(None), "read as unknown");
    flush(&h.manager);
    let own = saved_own(dir.path()).expect("the cooldown record survived the flush");
    assert!(own.next_retry_after.is_some());
    assert_eq!(own.recovery_refused, Some(Refusal::Unknown));
    let next = restarted(&h, dir.path());
    assert!(no_model_blocked(&next), "the cooldown survived a restart");
    assert_eq!(mark_of(&next), Some(None));
}

/// Finding 6, decoding: the UTC boundary dates, and what else is read.
#[tokio::test(start_paused = true)]
async fn refusal_times_are_read_by_their_utc_year() {
    use crate::manager::cooldown_store::Refusal;
    for (value, expected) in [
        (
            "\"9999-12-31T23:59:59Z\"",
            Some(Refusal::Until(
                chrono::DateTime::parse_from_rfc3339("9999-12-31T23:59:59Z")
                    .expect("time")
                    .to_utc(),
            )),
        ),
        ("\"9999-12-31T23:59:59-01:00\"", Some(Refusal::Unknown)),
        ("\"0000-01-01T00:00:00+01:00\"", Some(Refusal::Unknown)),
        (
            "\"0000-01-01T00:00:00Z\"",
            Some(Refusal::Until(
                chrono::DateTime::parse_from_rfc3339("0000-01-01T00:00:00Z")
                    .expect("time")
                    .to_utc(),
            )),
        ),
        ("\"not a time\"", None),
    ] {
        let dir = temp_dir();
        std::fs::write(
            dir.path().join("a.cds"),
            format!(
                r#"{{"Records":[{{"auth_id":"{ID}","open_ferry_recovery_refused_until":{value}}}]}}"#
            ),
        )
        .expect("write");
        let loaded = FileStore::new(dir.path().to_path_buf())
            .load()
            .expect("load");
        let read = loaded.first().and_then(|record| record.recovery_refused);
        assert_eq!(read, expected, "{value}");
    }
}

/// Finding 6, restore level: the marks of a mark-only record on an absent
/// credential and on a model's record are ignored; two records for one
/// credential keep the later bound, and unknown wins.
#[tokio::test(start_paused = true)]
async fn restored_marks_follow_their_credential_and_keep_the_latest() {
    let now = Harness::new(Settings::default()).now();
    let later = (now + TimeDelta::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let sooner = (now + TimeDelta::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // Each file is restored by a manager of its own.
    let write = |records: &str| -> Harness {
        let dir = temp_dir();
        std::fs::write(
            dir.path().join("a.cds"),
            format!(r#"{{"Records":[{records}]}}"#),
        )
        .expect("write");
        let h = Harness::new(Settings::default());
        h.add(credential(dir.path()), &[OPUS, SONNET]);
        let on = config(dir.path(), true);
        reconfigure(&h.manager, None, &on);
        restore(&h.manager, &on);
        h
    };
    let record = |id: &str, model: &str, mark: &str| {
        format!(
            r#"{{"provider":"claude","auth_id":"{id}","model":"{model}","open_ferry_recovery_refused_until":{mark}}}"#
        )
    };
    let h = write(&record("ghost", "", "\"unknown\""));
    assert_eq!(
        h.manager.lock().quota_recovery.mark("ghost"),
        None,
        "absent"
    );
    assert_eq!(mark_of(&h), None);
    let h = write(&record(ID, OPUS, "\"unknown\""));
    assert_eq!(mark_of(&h), None, "a model's record carries no mark");
    let h = write(&format!(
        "{},{}",
        record(ID, "", &format!("\"{sooner}\"")),
        record(ID, "", &format!("\"{later}\""))
    ));
    let bound = chrono::DateTime::parse_from_rfc3339(&later)
        .expect("time")
        .to_utc();
    assert_eq!(mark_of(&h), Some(Some(bound)), "the later bound wins");
    let h = write(&format!(
        "{},{}",
        record(ID, "", &format!("\"{later}\"")),
        record(ID, "", "\"unknown\"")
    ));
    assert_eq!(mark_of(&h), Some(None), "unknown wins");
}

/// Item 7: disabling, or turning cooling off for, a credential that is clean
/// but carries an unknown mark ends the mark; a finite one stays.
#[tokio::test(start_paused = true)]
async fn disabling_a_clean_credential_ends_an_unknown_mark() {
    for cooling_off in [false, true] {
        let h = Harness::new(Settings::default());
        h.add(auth(ID, "claude"), &[OPUS, SONNET]);
        h.manager.lock().quota_recovery.restore_mark(ID, None);
        if cooling_off {
            h.manager.set_settings(Settings {
                disable_cooling: true,
                ..Settings::default()
            });
        } else {
            set_disabled(&h, true);
        }
        assert_eq!(mark_of(&h), None, "cooling_off {cooling_off}");
        let finite = Some(h.now() + TimeDelta::hours(1));
        h.manager.lock().quota_recovery.restore_mark(ID, finite);
        h.manager.set_settings(Settings::default());
        set_disabled(&h, false);
        if cooling_off {
            h.manager.set_settings(Settings {
                disable_cooling: true,
                ..Settings::default()
            });
        } else {
            set_disabled(&h, true);
        }
        assert_eq!(mark_of(&h), Some(finite), "a finite mark stays");
    }
}

// Round 14: disabling or turning cooling off ends every hold, and a mixed
// recovery leaves the scopes it doesn't recover alone.

#[derive(Clone, Copy, Debug)]
enum Ender {
    Disable,
    CoolingOff,
    Reset,
    Success,
}

/// Finding 1: a no-model 503 (one hour) and, when `unknown`, a registration of
/// the snapshot that carries it (the hold's end becomes unknown); then a clean
/// registration, which keeps the hold only `foreign` shows. Disabling and
/// enabling the credential, or turning cooling off and on, ends it; so do the
/// explicit reset and an accepted success (the controls).
#[tokio::test(start_paused = true)]
async fn disabling_or_cooling_off_ends_a_hold_only_foreign_shows() {
    hold_ended_by(&[Ender::Disable, Ender::CoolingOff]);
}

/// The controls of the one above: the explicit reset and an accepted success.
#[tokio::test(start_paused = true)]
async fn a_reset_or_an_accepted_success_ends_a_hold_only_foreign_shows() {
    hold_ended_by(&[Ender::Reset, Ender::Success]);
}

fn hold_ended_by(enders: &[Ender]) {
    for unknown in [false, true] {
        for &ender in enders {
            let h = Harness::new(Settings::default());
            h.add(auth(ID, "claude"), &[OPUS, SONNET]);
            h.manager.mark_result(&success(OPUS));
            h.manager.mark_result(&success(SONNET));
            h.clock.advance(secs(1));
            h.manager.mark_result(&other_failure("", 503, HOUR));
            if unknown {
                h.manager
                    .register_unsaved(Auth::clone(&h.get(ID)))
                    .expect("register the snapshot");
            }
            h.manager
                .register_unsaved(auth(ID, "claude"))
                .expect("register clean");
            assert!(!no_model_blocked(&h), "the registration is clean");
            match ender {
                Ender::Disable => {
                    set_disabled(&h, true);
                    set_disabled(&h, false);
                }
                Ender::CoolingOff => {
                    h.manager.set_settings(Settings {
                        disable_cooling: true,
                        ..Settings::default()
                    });
                    h.manager.set_settings(Settings::default());
                }
                Ender::Reset => {
                    h.manager.reset_quota(ID).expect("reset");
                }
                Ender::Success => {
                    h.clock.advance(secs(1));
                    h.manager.mark_result(&as_of_now(&h, success(SONNET)));
                }
            }
            // A finite hold is read inside its hour, an unknown one after it.
            h.clock
                .advance(if unknown { HOUR + secs(5) } else { secs(5) });
            h.manager
                .mark_result(&as_of_now(&h, quota_429(OPUS, opus_headers(), FIVE_DAYS)));
            h.clock.advance(secs(5));
            let read = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
            assert!(
                recover(&h, &read),
                "unknown {unknown} ender {ender:?}: the hold outlived it"
            );
        }
    }
}

/// Finding 2: Opus's wrap has run out with nothing of its own left, Sonnet's
/// own limit still holds, and a read confirms both. Sonnet recovers; Opus's
/// model state is byte-equal to before the read.
#[tokio::test(start_paused = true)]
async fn a_mixed_recovery_leaves_the_scope_it_does_not_recover_alone() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET, ROUTE]);
    h.manager.mark_result(&CallResult {
        credential_scope: true,
        ..other_failure(ROUTE, 429, secs(200))
    });
    h.clock.advance(secs(10));
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), secs(100)));
    h.manager
        .mark_result(&quota_429(SONNET, sonnet_headers(), secs(400)));
    h.clock.advance(secs(300));
    // What a read may touch of a scope it doesn't recover is only what the
    // aggregation refreshes for every state once a deadline has passed
    // (`unavailable`, `next_retry_after`); the wrap and the layer beneath it,
    // the reason and the error stay.
    let kept = |h: &Harness| {
        let state = h.get(ID).model_states[OPUS].clone();
        format!(
            "{:?} {:?} {} {:?} {:?} {:?}",
            state.credential_quota_shadow,
            state.credential_quota_propagated,
            state.quota.reason,
            state.quota.next_recover_at,
            state.last_error,
            state.status
        )
    };
    let opus_before = kept(&h);
    let read = reading(
        h.now(),
        &[
            (UsageWindow::SevenDayOpus, 2.0),
            (UsageWindow::SevenDaySonnet, 3.0),
        ],
    );
    assert!(recover(&h, &read), "Sonnet recovered");
    assert!(!blocked(&h, SONNET));
    assert_eq!(
        kept(&h),
        opus_before,
        "Opus's state was unwrapped by a read that recovered it nothing"
    );
}

/// The third item: a valid cooldown record whose quota check's reset normalises
/// to year 10000 keeps its cooldown; the check reads as absent, and the file
/// written after the restore and the process after it agree.
#[tokio::test(start_paused = true)]
async fn an_out_of_range_check_time_costs_no_cooldown_record() {
    let dir = temp_dir();
    let h = Harness::new(Settings::default());
    let end = (h.now() + TimeDelta::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let soon =
        (h.now() + TimeDelta::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    std::fs::write(
        dir.path().join("a.cds"),
        format!(
            r#"{{"Records":[{{"provider":"claude","auth_id":"{ID}","next_retry_after":"{end}","status":"cooling","reason":"upstream 503","open_ferry_quota_check":{{"wait_seconds":1800,"check_at":"{soon}","reset_at":"9999-12-31T23:59:59-01:00"}}}}]}}"#
        ),
    )
    .expect("write");
    h.add(credential(dir.path()), &[OPUS, SONNET]);
    let on = config(dir.path(), true);
    reconfigure(&h.manager, None, &on);
    restore(&h.manager, &on);
    assert!(no_model_blocked(&h), "the cooldown came back");
    assert!(h.manager.quota_checks(ID).is_empty(), "the check is absent");
    flush(&h.manager);
    let own = saved_own(dir.path()).expect("the cooldown record survived");
    assert!(own.next_retry_after.is_some());
    assert!(own.quota_check.is_none());
    let next = restarted(&h, dir.path());
    assert!(no_model_blocked(&next), "the cooldown survived a restart");
    assert!(next.manager.quota_checks(ID).is_empty());
    assert_eq!(
        format!("{:?}", h.get(ID).next_retry_after),
        format!("{:?}", next.get(ID).next_retry_after),
        "live and restored agree"
    );
}

// Upstream d5816d5e must preserve a recovery that overtakes a file observation.
#[tokio::test(start_paused = true)]
async fn a_pending_file_observation_keeps_the_cooldown_a_usage_read_recovered() {
    use crate::auth::AdoptionRead;
    use crate::manager::AdoptionResult;
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, SONNET]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    let stale = h.get(ID);
    h.clock.advance(Duration::from_secs(60));
    let result = h
        .manager
        .adopt_observed(&stale, None, |_| {
            assert!(recover(
                &h,
                &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])
            ));
            Ok(AdoptionRead::Record(stale.as_ref().clone()))
        })
        .unwrap();
    assert!(matches!(result, AdoptionResult::Published { .. }));
    assert!(!blocked(&h, OPUS));
    assert!(!h.get(ID).model_states[OPUS].quota.exceeded);
}

#[tokio::test(start_paused = true)]
async fn p47_capacity_hold_and_merge_fence_an_inflight_quota_recovery() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS, ROUTE]);
    h.manager
        .mark_result(&quota_429(OPUS, opus_headers(), FIVE_DAYS));
    let token = h.manager.quota_recovery_token(ID).unwrap();
    let error = ExecError::transient_capacity("transient capacity", Some(2 * HOUR));
    let mut capacity = other_failure(ROUTE, 429, 2 * HOUR);
    capacity.error = Some(crate::manager::classify::result_error_from_error(
        crate::manager::classify::ErrView::Exec(&error),
    ));
    h.manager.mark_result(&capacity);
    h.clock.advance(Duration::from_secs(5));
    merge_the_alias(&h);
    let healthy = reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)]);
    assert!(
        h.manager
            .apply_quota_usage_recovery(&token, &healthy)
            .unwrap()
            .is_empty()
    );
    assert!(!recover(&h, &healthy));
    assert!(blocked(&h, OPUS));
}

#[tokio::test(start_paused = true)]
async fn p47_hold_ordering_family_preserves_live_quota_and_cooldowns() {
    const INVARIANT: &str = "live quota and unrelated cooldowns cannot be shortened by stale results, relogin, or refresh/recovery merges";
    let orders = recovery_step_orderings([
        "stale",
        "relogin",
        "refresh",
        "merge",
        "recovery",
        "401",
        "invalid_grant",
    ]);
    let orders: Vec<_> = orders
        .into_iter()
        .filter(|order| {
            order.iter().position(|step| *step == "merge")
                < order.iter().position(|step| *step == "recovery")
        })
        .collect();
    assert_eq!(orders.len(), 420);
    for quota in [false, true] {
        for order in &orders {
            let h = Harness::new(Settings::default());
            let partition_deadline = partition_hold(&h.manager);
            let executor = FakeExecutor::new("claude");
            executor.set_refresh(|_| Err(ExecError::upstream(400, "invalid_grant")));
            executor.set_lead(Some(Duration::from_secs(60)));
            h.manager.register_executor(executor.clone());
            let mut refresh_deadline = None;
            let mut initial = auth_with_metadata(
                ID,
                "claude",
                serde_json::json!({"type":"claude", "access_token":"dummy-before", "refresh_token":"dummy-refresh"}),
            );
            // A refresh can carry explicit older healthy values, not just an
            // empty map (which has an independent compatibility fallback).
            for model in [OPUS, ROUTE] {
                initial.model_states.insert(
                    model.into(),
                    crate::auth::ModelState {
                        status: crate::auth::Status::Active,
                        ..Default::default()
                    },
                );
            }
            initial.status = crate::auth::Status::Active;
            let pre_hold_models = initial.model_states.clone();
            h.add(initial, &[OPUS, ROUTE]);
            let old_seq = h.manager.next_execution_seq();
            let end = h.now() + TimeDelta::from_std(FIVE_DAYS).unwrap();
            let mut held = if quota {
                quota_429(OPUS, opus_headers(), FIVE_DAYS)
            } else {
                other_failure(OPUS, 503, FIVE_DAYS)
            };
            held.execution_seq = h.manager.next_execution_seq();
            h.manager.mark_result(&held);
            let mut alias_hold = other_failure(ROUTE, 503, 2 * HOUR);
            alias_hold.execution_seq = h.manager.next_execution_seq();
            h.manager.mark_result(&alias_hold);
            for step in order {
                match *step {
                    "stale" => {
                        let error = ExecError::transient_capacity(
                            "transient capacity",
                            Some(Duration::ZERO),
                        );
                        let mut stale = other_failure(OPUS, 429, Duration::ZERO);
                        stale.error = Some(crate::manager::classify::result_error_from_error(
                            crate::manager::classify::ErrView::Exec(&error),
                        ));
                        stale.execution_seq = old_seq;
                        let current = h.get(ID);
                        stale.credential_version = current.credential_version;
                        stale.registration_epoch = current.registration_epoch;
                        h.manager.mark_result(&stale);
                    }
                    "401" => {
                        let current = h.get(ID);
                        let mut unauthorized = other_failure("", 401, Duration::ZERO);
                        unauthorized.credential_version = current.credential_version;
                        unauthorized.registration_epoch = current.registration_epoch;
                        h.manager.mark_result(&unauthorized);
                        assert_eq!(h.get(ID).status, crate::auth::Status::Error);
                        refresh_deadline = refresh_deadline.max(Some(h.now()));
                    }
                    "invalid_grant" => {
                        executor.set_refresh(|_| Err(ExecError::upstream(400, "invalid_grant")));
                        assert!(h.manager.force_refresh(ID).await.is_err());
                        assert!(
                            h.get(ID)
                                .last_error
                                .as_ref()
                                .is_some_and(|error| error.message.contains("invalid_grant"))
                        );
                        refresh_deadline = Some(h.now() + TimeDelta::seconds(60));
                    }
                    "relogin" => {
                        let mut fresh = h.get(ID).as_ref().clone();
                        fresh
                            .metadata
                            .insert("access_token".into(), serde_json::json!("dummy-login"));
                        h.manager
                            .adopt_observed(&fresh, None, |_| {
                                Ok(crate::auth::AdoptionRead::Record(fresh.clone()))
                            })
                            .unwrap();
                        refresh_deadline = None;
                        assert_eq!(
                            h.get(ID).status,
                            if quota {
                                crate::auth::Status::Error
                            } else {
                                crate::auth::Status::Active
                            }
                        );
                    }
                    "refresh" => {
                        // Use the current version so a preceding relogin cannot
                        // make this test pass through the older-version fence.
                        let mut base = h.get(ID).as_ref().clone();
                        base.model_states = pre_hold_models.clone();
                        let mut refreshed = base.clone();
                        refreshed
                            .metadata
                            .insert("access_token".into(), serde_json::json!("dummy-refreshed"));
                        refreshed.metadata.insert(
                            "expires_at".into(),
                            serde_json::json!("2000-01-01T00:00:00Z"),
                        );
                        refreshed.next_refresh_after = None;
                        h.manager
                            .update_refreshed(&base, base.registration_epoch, refreshed)
                            .unwrap();
                        executor.set_refresh(|before| {
                            let mut after = before.clone();
                            after.metadata.insert(
                                "access_token".into(),
                                serde_json::json!("dummy-still-due"),
                            );
                            Ok(after)
                        });
                        h.manager.force_refresh(ID).await.unwrap();
                        refresh_deadline = Some(h.now() + TimeDelta::seconds(30));
                        rotate_outer_with_partition_hold(&h.manager);
                    }
                    "merge" => merge_the_alias(&h),
                    "recovery" => {
                        // Recover the original claim: never recreate the hold
                        // whose survival this family is asserting.
                        assert!(
                            !recover(&h, &reading(h.now(), &[(UsageWindow::SevenDayOpus, 2.0)])),
                            "{INVARIANT}: quota={quota}, {order:?}, recovery"
                        );
                    }
                    _ => unreachable!(),
                }
                let live = h.get(ID);
                assert_eq!(
                    live.next_refresh_after, refresh_deadline,
                    "refresh hold: quota={quota}, {order:?}, {step}"
                );
                assert_partition_hold(&h.manager, partition_deadline);
                assert!(
                    blocked(&h, OPUS)
                        && live
                            .model_states
                            .get(OPUS)
                            .and_then(|state| state.next_retry_after)
                            .is_some_and(|next| next >= end),
                    "{INVARIANT}: quota={quota}, {order:?}, after {step}"
                );
                if quota {
                    assert!(
                        live.model_states[OPUS].quota.exceeded,
                        "{INVARIANT}: {order:?}"
                    );
                }
                h.clock.advance(Duration::from_secs(1));
            }
        }
    }
}
