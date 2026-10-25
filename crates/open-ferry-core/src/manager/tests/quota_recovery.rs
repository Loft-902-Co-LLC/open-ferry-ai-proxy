//! Not upstream's: a confirmed upstream usage reset ends a stale quota
//! cooldown (issue #8, see `quota_recovery`).
//!
//! The manager runs against a fake executor, and a usage reading is handed to
//! it as the management handler would hand one over; nothing here contacts a
//! provider. The tests cover what is seen through dispatch, and the
//! interplay with the capped rests and the saved cooldown state.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// A model's own limit, capped to its next check, comes out from under a
/// credential-wide cooldown with the provider's reset when the credential
/// recovers: its capped time, passed by then, doesn't read as an ended
/// limit.
#[tokio::test(start_paused = true)]
async fn a_capped_model_layer_survives_the_credentials_recovery() {
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

    let weekly_only = reading(h.now(), &[(UsageWindow::SevenDayOpus, 100.0)]);
    assert!(
        recover(&h, &weekly_only),
        "the credential's window is healthy"
    );
    assert!(!blocked(&h, SONNET), "the wrap is gone");
    assert!(
        blocked(&h, OPUS),
        "opus is still at its limit: {:?}",
        h.get(ID).model_states.get(OPUS)
    );
    sonnet_ok.store(true, Ordering::SeqCst);
    assert!(call(&h, SONNET).await.is_ok());
    let reached = executor.models(Kind::Execute);
    assert!(call(&h, OPUS).await.is_err());
    assert_eq!(executor.models(Kind::Execute), reached, "opus stays held");
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
/// model and holds the whole credential longer: the model's recovery must
/// not take the unproven hold with it.
#[tokio::test(start_paused = true)]
async fn an_independent_no_model_429_stays_when_a_model_recovers() {
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
    assert!(recover(&h, &reading(h.now(), &healthy)));
    assert!(!blocked(&h, OPUS), "the model's own quota is over");
    let current = h.get(ID);
    assert!(
        current.quota.exceeded,
        "the independent hold was removed without a claim: {:?}",
        current.quota
    );
    assert_eq!(current.quota.next_recover_at, until);
}

/// The same, with the cooldown storage turned off, the recovery, and turned
/// on again: the credential's independent record is not taken for a derived
/// one.
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
    assert!(recover(
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

/// The 503 comes after the quota 429 and keeps the 7-day deadline: its own
/// contribution is its own 10 minutes, not the merged one.
#[tokio::test(start_paused = true)]
async fn a_foreign_hold_counts_its_own_deadline_not_the_merged_one() {
    let h = Harness::new(Settings::default());
    h.add(auth(ID, "claude"), &[OPUS]);
    h.manager.mark_result(&failure(OPUS, opus_headers(), false));
    h.clock.advance(Duration::from_secs(10));
    h.manager
        .mark_result(&other_failure(OPUS, 503, Duration::from_secs(600)));
    h.clock.advance(Duration::from_secs(10));
    h.manager.mark_result(&failure(OPUS, opus_headers(), false));
    let healthy = [(UsageWindow::SevenDayOpus, 2.0)];

    h.clock.advance(Duration::from_secs(5 * 60));
    assert!(!recover(&h, &reading(h.now(), &healthy)));
    h.clock.advance(Duration::from_secs(6 * 60));
    assert!(
        recover(&h, &reading(h.now(), &healthy)),
        "the 503 ended 11 minutes in; a 7-day merged deadline blocks the claim"
    );
}
