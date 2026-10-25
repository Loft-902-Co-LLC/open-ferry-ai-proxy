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
