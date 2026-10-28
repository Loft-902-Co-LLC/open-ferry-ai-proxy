//! Not upstream's: open-ferry's own update routes, `GET /update` and
//! `POST /update/check`. The updater behind them trusts no key, keeps its
//! data in a temporary directory, and downloads through a fake that only
//! counts, so no test makes a request.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use http::{Method, StatusCode};
use open_ferry_core::config::Config;
use open_ferry_update::background::MakeFetch;
use open_ferry_update::fetch::BoxFuture;
use open_ferry_update::install::RealSystem;
use open_ferry_update::{DataDir, Fetch, FetchError, ReleaseKeys, UpdateService, Updater};
use url::Url;

use super::{Dash, LOCAL, keyed_config, request};

const STATUS: &str = "/open-ferry/api/v1/update";
const CHECK: &str = "/open-ferry/api/v1/update/check";

/// A downloader that counts its calls and fails each.
#[derive(Default)]
struct Counting {
    calls: AtomicUsize,
}

impl Counting {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Fetch for Counting {
    fn get<'a>(
        &'a self,
        _url: &'a Url,
        _limit: u64,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(FetchError::Network("the tests make no request".to_owned())) })
    }
}

/// Update checks following `config`, downloading with `fetch` and keeping
/// their data in `data`; their loop isn't started.
fn service(config: &Config, fetch: &Arc<Counting>, data: &Path) -> UpdateService {
    let as_fetch: Arc<dyn Fetch> = Arc::clone(fetch) as Arc<dyn Fetch>;
    let updater = Updater {
        base: Url::parse("http://127.0.0.1:9/").unwrap(),
        keys: ReleaseKeys::none(),
        system: Arc::new(RealSystem { write_probe: false }),
        ..Updater::for_this_binary(Arc::clone(&as_fetch), DataDir::at(data)).unwrap()
    };
    let make_fetch: MakeFetch = Arc::new(move |_proxy: &str| Ok(Arc::clone(&as_fetch)));
    UpdateService::new(updater, make_fetch, config)
}

/// [`keyed_config`] with `self-update.mode` set to `mode`.
fn config_in(mode: &str) -> Config {
    let mut config = keyed_config();
    config.self_update.mode = mode.to_owned();
    config
}

#[tokio::test]
async fn a_server_without_update_checks_says_so() {
    let dash = Dash::new();

    let message = dash
        .get(STATUS)
        .await
        .error(StatusCode::SERVICE_UNAVAILABLE, "updates_unavailable");
    assert_eq!(message, "this server doesn't check for updates");
    dash.call(Method::POST, CHECK, "")
        .await
        .error(StatusCode::SERVICE_UNAVAILABLE, "updates_unavailable");
}

#[tokio::test]
async fn the_status_says_the_mode_and_what_set_it() {
    let data = tempfile::tempdir().unwrap();
    let fetch = Arc::new(Counting::default());
    let config = keyed_config();
    let dash = Dash::with_updates(config.clone(), service(&config, &fetch, data.path()));

    let body = dash.get(STATUS).await.json(StatusCode::OK);

    assert_eq!(body["channel"], "stable");
    assert_eq!(body["release_source"], serde_json::Value::Null);
    assert_eq!(body["mode"], "auto");
    assert_eq!(body["mode_source"], "default");
    assert_eq!(body["updates"], "on");
    assert_eq!(body["check_every_seconds"], 6 * 60 * 60);
    assert_eq!(body["running_version"], open_ferry_update::CURRENT_VERSION);
    assert_eq!(
        body["installed_version"],
        open_ferry_update::CURRENT_VERSION
    );
    assert_eq!(body["target"], open_ferry_update::TARGET);
    assert_eq!(body["trusts_release_key"], false);
    assert_eq!(body["checking"], false);
    assert!(body["latest_version"].is_null(), "{body}");
    assert!(body["last_check"].is_null(), "{body}");
    assert_eq!(fetch.calls(), 0);
}

#[tokio::test]
async fn the_status_follows_the_config_when_updates_are_off() {
    let data = tempfile::tempdir().unwrap();
    let fetch = Arc::new(Counting::default());
    let config = config_in("off");
    let dash = Dash::with_updates(config.clone(), service(&config, &fetch, data.path()));

    let body = dash.get(STATUS).await.json(StatusCode::OK);

    assert_eq!(body["mode"], "off");
    assert_eq!(body["mode_source"], "config");
    assert_eq!(body["updates"], "off");
    assert!(body["next_check"].is_null(), "{body}");
}

#[tokio::test]
async fn a_check_while_updates_are_off_is_refused_and_makes_no_request() {
    let data = tempfile::tempdir().unwrap();
    let fetch = Arc::new(Counting::default());
    let config = config_in("off");
    let dash = Dash::with_updates(config.clone(), service(&config, &fetch, data.path()));

    let message = dash
        .call(Method::POST, CHECK, "")
        .await
        .error(StatusCode::CONFLICT, "updates_off");

    assert!(message.contains("open-ferry update -check"), "{message}");
    assert_eq!(fetch.calls(), 0);
    assert!(
        !DataDir::at(data.path()).state_file().exists(),
        "a check while off wrote the state"
    );
}

#[tokio::test]
async fn a_check_now_runs_in_the_background_and_the_status_shows_how_it_went() {
    let data = tempfile::tempdir().unwrap();
    let fetch = Arc::new(Counting::default());
    let config = config_in("notify");
    let updates = service(&config, &fetch, data.path());
    let dash = Dash::with_updates(config.clone(), updates.clone());

    let body = dash
        .call(Method::POST, CHECK, "")
        .await
        .json(StatusCode::ACCEPTED);
    assert_eq!(body, serde_json::json!({"check": "started"}));
    // Waits for the check started, then runs one more.
    updates.check_once().await;

    let body = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(body["mode"], "notify");
    assert_eq!(body["updates"], "notify-only");
    assert_eq!(body["last_result"], "error");
    let error = body["last_error"].as_str().unwrap();
    assert!(error.contains("trusts no release key"), "{error}");
    assert!(body["last_check"].is_string(), "{body}");
    // With no trusted key nothing is downloaded.
    assert_eq!(fetch.calls(), 0);
}

#[tokio::test]
async fn the_update_routes_need_the_management_key() {
    let data = tempfile::tempdir().unwrap();
    let fetch = Arc::new(Counting::default());
    let config = keyed_config();
    let dash = Dash::with_updates(config.clone(), service(&config, &fetch, data.path()));

    dash.send(request(LOCAL, Method::GET, STATUS, ""))
        .await
        .error(StatusCode::UNAUTHORIZED, "missing_management_key");
    dash.send(request(LOCAL, Method::POST, CHECK, ""))
        .await
        .error(StatusCode::UNAUTHORIZED, "missing_management_key");
    assert_eq!(fetch.calls(), 0);
}

struct VersionRunner;
impl open_ferry_update::runner::Runner for VersionRunner {
    fn version<'a>(
        &'a self,
        binary: &'a Path,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let version = binary
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap();
            Ok(format!("open-ferry {version}"))
        })
    }
}

struct Progress;
impl open_ferry_update::SwitchProgress for Progress {
    fn next(
        &mut self,
        _deadline: std::time::Instant,
    ) -> BoxFuture<'_, Result<open_ferry_update::SwitchEvent, open_ferry_update::SwitchError>> {
        panic!("the route must answer at acceptance without waiting for progress")
    }
}

type StatusGate = (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);

struct Switcher {
    delay: Duration,
    status_gate: std::sync::Mutex<Option<StatusGate>>,
    error: std::sync::Mutex<Option<open_ferry_update::SwitchError>>,
    calls: AtomicUsize,
    snapshot: std::sync::Mutex<serde_json::Value>,
    disabled: std::sync::atomic::AtomicBool,
}

impl Switcher {
    fn new(delay: Duration, refusal: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            delay,
            status_gate: std::sync::Mutex::new(None),
            error: std::sync::Mutex::new(
                refusal.map(|reason| open_ferry_update::SwitchError::Refused(reason.into())),
            ),
            calls: AtomicUsize::new(0),
            disabled: std::sync::atomic::AtomicBool::new(false),
            snapshot: std::sync::Mutex::new(serde_json::json!({
                "state": "idle", "active_version": "0.1.0", "switch": null, "switching_disabled": null,
            })),
        })
    }

    async fn accept(
        &self,
    ) -> Result<open_ferry_update::SwitchHandle, open_ferry_update::SwitchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        if let Some(error) = self.error.lock().unwrap().take() {
            return Err(error);
        }
        Ok(open_ferry_update::SwitchHandle::new(
            "switch-1".into(),
            Box::new(Progress),
        ))
    }
}

impl open_ferry_update::Switcher for Switcher {
    fn status(
        &self,
        _deadline: std::time::Instant,
    ) -> BoxFuture<'_, Result<serde_json::Value, open_ferry_update::SwitchError>> {
        Box::pin(async move {
            if self.disabled.load(Ordering::SeqCst) {
                return Err(open_ferry_update::SwitchError::Disabled(
                    "private policy path".into(),
                ));
            }
            let snapshot = self.snapshot.lock().unwrap().clone();
            let gate = self.status_gate.lock().unwrap().take();
            if let Some((started, release)) = gate {
                started.notify_one();
                release.notified().await;
            }
            if snapshot.is_null() {
                Err(open_ferry_update::SwitchError::Unreachable(
                    "fake endpoint".into(),
                ))
            } else {
                Ok(snapshot)
            }
        })
    }
    fn update<'a>(
        &'a self,
        _pin: &'a open_ferry_update::pins::Pin,
        auto: bool,
        _deadline: std::time::Instant,
    ) -> BoxFuture<'a, Result<open_ferry_update::SwitchHandle, open_ferry_update::SwitchError>>
    {
        assert!(
            !auto,
            "dashboard installs are manual, even while updates are off"
        );
        Box::pin(self.accept())
    }
    fn rollback<'a>(
        &'a self,
        target: &'a open_ferry_update::journal::Artifact,
        _deadline: std::time::Instant,
    ) -> BoxFuture<'a, Result<open_ferry_update::SwitchHandle, open_ferry_update::SwitchError>>
    {
        assert_eq!(target.version, "0.0.9");
        Box::pin(self.accept())
    }
    fn cancel(
        &self,
        _deadline: std::time::Instant,
    ) -> BoxFuture<'_, Result<open_ferry_update::CancelOutcome, open_ferry_update::SwitchError>>
    {
        panic!("no cancel route was requested")
    }
}

fn supervised(fake: Arc<Switcher>) -> (Dash, tempfile::TempDir) {
    use open_ferry_update::journal::{Artifact, Journal};
    let scratch = tempfile::tempdir().unwrap();
    let data = DataDir::at(scratch.path());
    let bytes = b"fake release bytes";
    let sha256 = open_ferry_update::release::sha256_hex(bytes);
    for version in ["0.0.9", "0.1.0", "0.2.0"] {
        std::fs::create_dir_all(data.version_dir(version)).unwrap();
        let name = if cfg!(windows) {
            "open-ferry.exe"
        } else {
            "open-ferry"
        };
        std::fs::write(data.binary(version, name), bytes).unwrap();
    }
    let artifact = |version: &str| Artifact {
        version: version.into(),
        sha256: sha256.clone(),
        size: bytes.len() as u64,
    };
    let mut journal = Journal::first_start(artifact("0.1.0"), None);
    journal.previous = Some(artifact("0.0.9"));
    std::fs::write(
        scratch
            .path()
            .join(open_ferry_update::journal::JOURNAL_FILE),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    open_ferry_update::State {
        staged: Some("0.2.0".into()),
        staged_sha256: Some(sha256),
        latest: Some("0.2.0".into()),
        ..Default::default()
    }
    .save(&data.state_file())
    .unwrap();
    let config = config_in("off");
    let fetch: Arc<dyn Fetch> = Arc::new(Counting::default());
    let updater = Updater {
        base: Url::parse("http://127.0.0.1:9/").unwrap(),
        keys: ReleaseKeys::none(),
        runner: Arc::new(VersionRunner),
        ..Updater::for_this_binary(fetch.clone(), data).unwrap()
    };
    let updates = UpdateService::new(updater, Arc::new(move |_| Ok(fetch.clone())), &config)
        .with_switcher(fake);
    (Dash::with_updates(config, updates), scratch)
}

const INSTALL: &str = "/open-ferry/api/v1/update/install";
const ROLLBACK: &str = "/open-ferry/api/v1/update/rollback";

fn switch_body(path: &str) -> &str {
    if path == ROLLBACK {
        r#"{"target":"0.0.9"}"#
    } else {
        ""
    }
}

#[tokio::test(start_paused = true)]
async fn disabled_status_and_post_agree_without_exposing_policy_details() {
    let fake = Switcher::new(Duration::ZERO, None);
    fake.disabled.store(true, Ordering::SeqCst);
    *fake.error.lock().unwrap() = Some(open_ferry_update::SwitchError::Disabled(
        "private policy path".into(),
    ));
    let (dash, _scratch) = supervised(fake);
    for _ in 0..2 {
        let body = dash.get(STATUS).await.json(StatusCode::OK);
        assert_eq!(
            body["supervisor"]["switching_disabled"],
            "switching is disabled"
        );
        assert!(body["supervisor"]["error"].is_null());
        assert_eq!(body["supervisor"]["stale"], false);
        assert_eq!(body["supervisor"]["failed_reads"], 0);
    }
    let reason = dash
        .call(Method::POST, INSTALL, "")
        .await
        .error(StatusCode::CONFLICT, "switching_disabled");
    assert_eq!(reason, "switching is disabled");
}

#[tokio::test]
async fn policy_disabled_reads_keep_the_previous_snapshot() {
    let fake = Switcher::new(Duration::ZERO, None);
    let (dash, _scratch) = supervised(fake.clone());
    let before = dash.get(STATUS).await.json(StatusCode::OK);
    *fake.snapshot.lock().unwrap() = serde_json::Value::Null;
    assert_eq!(
        dash.get(STATUS).await.json(StatusCode::OK)["supervisor"]["failed_reads"],
        1
    );
    fake.disabled.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        let body = dash.get(STATUS).await.json(StatusCode::OK);
        for field in ["phase", "running_version", "switch", "watch_until"] {
            assert_eq!(body["supervisor"][field], before["supervisor"][field]);
        }
        assert!(body["supervisor"]["error"].is_null());
        assert_eq!(body["supervisor"]["stale"], false);
        assert_eq!(body["supervisor"]["failed_reads"], 0);
        assert_eq!(
            body["supervisor"]["switching_disabled"],
            "switching is disabled"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn rollback_does_not_send_when_the_confirmed_target_changes_or_disappears() {
    use open_ferry_update::journal::{JOURNAL_FILE, Journal};
    for removed in [false, true] {
        let fake = Switcher::new(Duration::ZERO, None);
        let (dash, scratch) = supervised(fake.clone());
        assert_eq!(
            dash.get(STATUS).await.json(StatusCode::OK)["supervisor"]["rollback_target"],
            "0.0.9"
        );
        let path = scratch.path().join(JOURNAL_FILE);
        let mut journal: Journal = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if removed {
            journal.previous = None;
        } else {
            journal.previous.as_mut().unwrap().version = "0.1.0".into();
        }
        std::fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
        let reason = dash
            .call(Method::POST, ROLLBACK, switch_body(ROLLBACK))
            .await
            .error(StatusCode::CONFLICT, "rollback_target_changed");
        assert_eq!(
            reason,
            "the rollback target changed; read status and confirm again"
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }
}

// Local preparation succeeds; these errors come back from the switcher after sending.
#[tokio::test(start_paused = true)]
async fn typed_supervisor_refusals_reach_the_http_api_after_preparation() {
    use open_ferry_update::{SwitchError, UpdateError};
    for (error, path, code, reason) in [
        (
            SwitchError::Disabled("switching is disabled; see the supervisor log".into()),
            INSTALL,
            "switching_disabled",
            "switching is disabled",
        ),
        (
            SwitchError::Update(UpdateError::NoPrevious),
            ROLLBACK,
            "rollback_target_changed",
            "the rollback target changed; read status and confirm again",
        ),
        (
            SwitchError::Update(UpdateError::RollbackChanged {
                expected: "0.0.9".into(),
                actual: None,
            }),
            ROLLBACK,
            "rollback_target_changed",
            "the rollback target changed; read status and confirm again",
        ),
    ] {
        let fake = Switcher::new(Duration::ZERO, None);
        *fake.error.lock().unwrap() = Some(error);
        let (dash, _scratch) = supervised(fake.clone());
        assert_eq!(
            dash.call(Method::POST, path, switch_body(path))
                .await
                .error(StatusCode::CONFLICT, code),
            reason
        );
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "local checks passed and the supervisor was asked"
        );
        let body = dash.get(STATUS).await.json(StatusCode::OK);
        assert_eq!(body["switch_request"]["state"], "failed");
        assert_eq!(body["switch_request"]["pending"], false);
        assert_eq!(body["switch_request"]["reason"], reason);
    }
}

#[tokio::test]
async fn rollback_requires_the_confirmed_target() {
    let fake = Switcher::new(Duration::ZERO, None);
    let (dash, _scratch) = supervised(fake.clone());
    for (body, code) in [
        ("", "invalid_json"),
        ("{", "invalid_json"),
        ("{}", "invalid_request"),
        (r#"{"target":""}"#, "invalid_request"),
        (r#"{"target":" "}"#, "invalid_request"),
        (r#"{"target":"0.0.9","extra":true}"#, "invalid_request"),
    ] {
        dash.call(Method::POST, ROLLBACK, body)
            .await
            .error(StatusCode::BAD_REQUEST, code);
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn not_sent_transport_and_preparation_errors_do_not_stay_pending() {
    for error in [
        open_ferry_update::SwitchError::Unreachable(
            "pre-send timeout or zero accepted bytes".into(),
        ),
        open_ferry_update::UpdateError::Io {
            what: "prepare the rollback",
            error: "preparation JoinError".into(),
        }
        .into(),
    ] {
        let fake = Switcher::new(Duration::ZERO, None);
        *fake.error.lock().unwrap() = Some(error);
        let (dash, _scratch) = supervised(fake);
        dash.call(Method::POST, INSTALL, "")
            .await
            .error(StatusCode::CONFLICT, "switch_failed");
        let body = dash.get(STATUS).await.json(StatusCode::OK);
        assert_eq!(body["switch_request"]["state"], "failed");
        assert_eq!(body["switch_request"]["pending"], false);
    }
}

#[tokio::test]
async fn unsupervised_switches_explain_how_to_update_and_status_has_no_supervisor() {
    let data = tempfile::tempdir().unwrap();
    let config = keyed_config();
    let fetch = Arc::new(Counting::default());
    let dash = Dash::with_updates(config.clone(), service(&config, &fetch, data.path()));
    assert!(dash.get(STATUS).await.json(StatusCode::OK)["supervisor"].is_null());
    for path in [INSTALL, ROLLBACK] {
        let reason = dash
            .call(Method::POST, path, switch_body(path))
            .await
            .error(StatusCode::CONFLICT, "not_supervised");
        assert!(reason.contains("open-ferry update"), "{reason}");
        assert!(reason.contains("restart"), "{reason}");
    }
    assert_eq!(fetch.calls(), 0);
}

// These acceptance cases freeze async time while the artifact files are read.
// Slow preparation has its own clock-driven cases below.
#[tokio::test(start_paused = true)]
async fn accepted_switches_answer_with_the_id_without_following_progress() {
    for path in [INSTALL, ROLLBACK] {
        let fake = Switcher::new(Duration::ZERO, None);
        let (dash, _scratch) = supervised(fake.clone());
        let reply = dash
            .call(Method::POST, path, switch_body(path))
            .await
            .json(StatusCode::ACCEPTED);
        assert_eq!(reply["accepted"], true);
        assert_eq!(reply["id"], "switch-1");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let status = dash.get(STATUS).await.json(StatusCode::OK);
        assert_eq!(status["supervisor"]["form"], "supervised");
        assert_eq!(status["supervisor"]["phase"], "idle");
        assert_eq!(status["supervisor"]["running_version"], "0.1.0");
        assert_eq!(status["supervisor"]["rollback_target"], "0.0.9");
        assert_eq!(status["switch_request"]["state"], "accepted");
        assert_eq!(status["switch_request"]["id"], "switch-1");
        assert_eq!(reply["request_id"], status["switch_request"]["request_id"]);
    }
}

#[tokio::test(start_paused = true)]
async fn a_refused_switch_keeps_the_supervisors_reason() {
    let fake = Switcher::new(Duration::ZERO, Some("sign-ins are pending"));
    let (dash, _scratch) = supervised(fake);
    let reason = dash
        .call(Method::POST, INSTALL, "")
        .await
        .error(StatusCode::CONFLICT, "switch_refused");
    assert!(reason.contains("sign-ins are pending"), "{reason}");
    let body = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(body["switch_request"]["state"], "refused");
    assert!(
        body["switch_request"]["reason"]
            .as_str()
            .unwrap()
            .contains("sign-ins are pending")
    );
}

#[tokio::test(start_paused = true)]
async fn slow_preparation_outlives_the_response_and_a_duplicate_is_refused() {
    for path in [INSTALL, ROLLBACK] {
        for refusal in [None, Some("a switch is already running")] {
            let fake = Switcher::new(Duration::from_secs(5), refusal);
            let (dash, _scratch) = supervised(fake.clone());
            let start = tokio::time::Instant::now();
            let reply = dash
                .call(Method::POST, path, switch_body(path))
                .await
                .json(StatusCode::ACCEPTED);
            assert_eq!(reply["preparing"], true);
            assert!(start.elapsed() < Duration::from_secs(1));
            dash.call(Method::POST, path, switch_body(path))
                .await
                .error(StatusCode::CONFLICT, "already_preparing");
            let preparing = dash.get(STATUS).await.json(StatusCode::OK);
            assert_eq!(preparing["switch_request"]["state"], "preparing");
            let request_id = preparing["switch_request"]["request_id"].as_str().unwrap();
            assert_eq!(request_id.len(), 32);
            assert_eq!(reply["request_id"], request_id);
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
            let body = dash.get(STATUS).await.json(StatusCode::OK);
            assert_eq!(
                body["switch_request"]["state"],
                if refusal.is_some() {
                    "refused"
                } else {
                    "accepted"
                }
            );
            assert_eq!(body["switch_request"]["request_id"], request_id);
            assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn an_ambiguous_request_is_never_resent_and_requires_a_status_read_before_another_request() {
    for error in [
        open_ferry_update::SwitchError::TimedOut,
        open_ferry_update::SwitchError::Stopped,
        open_ferry_update::SwitchError::Protocol("bad reply".into()),
    ] {
        let fake = Switcher::new(Duration::ZERO, None);
        *fake.error.lock().unwrap() = Some(error);
        let (dash, _scratch) = supervised(fake.clone());
        let reply = dash
            .call(Method::POST, INSTALL, "")
            .await
            .json(StatusCode::ACCEPTED);
        assert_eq!(reply["uncertain"], true);
        dash.call(Method::POST, INSTALL, "")
            .await
            .error(StatusCode::CONFLICT, "switch_uncertain");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let body = dash.get(STATUS).await.json(StatusCode::OK);
        assert_eq!(body["switch_request"]["state"], "unknown");
        assert_eq!(reply["request_id"], body["switch_request"]["request_id"]);
        assert_eq!(body["switch_request"]["pending"], false);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        dash.call(Method::POST, INSTALL, "")
            .await
            .json(StatusCode::ACCEPTED);
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            2,
            "only an explicit later request retries"
        );
    }
}

#[tokio::test]
async fn switch_routes_have_the_same_auth_and_unavailable_errors() {
    let dash = Dash::new();
    for path in [INSTALL, ROLLBACK] {
        dash.send(request(LOCAL, Method::POST, path, ""))
            .await
            .error(StatusCode::UNAUTHORIZED, "missing_management_key");
        dash.call(Method::POST, path, switch_body(path))
            .await
            .error(StatusCode::SERVICE_UNAVAILABLE, "updates_unavailable");
    }
}

#[tokio::test(start_paused = true)]
async fn dropping_the_http_request_does_not_drop_its_preparation() {
    let fake = Switcher::new(Duration::from_secs(5), None);
    let (dash, _scratch) = supervised(fake.clone());
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            dash.call(Method::POST, INSTALL, "")
        )
        .await
        .is_err()
    );
    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    let status = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(status["switch_request"]["state"], "accepted");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn status_marks_a_cached_supervisor_stale_and_keeps_switching_disabled() {
    let fake = Switcher::new(Duration::ZERO, None);
    let (dash, _scratch) = supervised(fake.clone());
    *fake.snapshot.lock().unwrap() = serde_json::json!({
        "state": "switching", "active_version": "0.1.0",
        "switch": { "id": "switch-1", "phase": "loading", "status": "waiting-for-signins", "watch_until": "2026-10-18T16:10:00Z" },
        "switching_disabled": "a switch is already active",
    });
    let body = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(body["supervisor"]["phase"], "waiting-for-signins");
    assert_eq!(body["supervisor"]["watch_until"], "2026-10-18T16:10:00Z");
    assert_eq!(body["supervisor"]["stale"], false);
    assert_eq!(body["supervisor"]["failed_reads"], 0);
    *fake.snapshot.lock().unwrap() = serde_json::Value::Null;
    let body = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(body["supervisor"]["form"], "supervised");
    assert_eq!(body["supervisor"]["phase"], "waiting-for-signins");
    assert_eq!(body["supervisor"]["rollback_target"], "0.0.9");
    assert!(body["supervisor"]["error"].is_string());
    assert_eq!(body["supervisor"]["stale"], true);
    assert_eq!(body["supervisor"]["failed_reads"], 1);
    assert!(body["supervisor"]["snapshot_age_seconds"].is_number());
    assert_eq!(
        body["supervisor"]["switching_disabled"],
        "switching is disabled"
    );
    for count in 2..=5 {
        let body = dash.get(STATUS).await.json(StatusCode::OK);
        assert_eq!(body["supervisor"]["failed_reads"], count);
    }
}

#[tokio::test]
async fn snapshot_disabled_reasons_are_fixed_and_do_not_expose_paths() {
    let private_path = r"private\state\conflicting-directory";
    let fake = Switcher::new(Duration::ZERO, None);
    let (dash, _scratch) = supervised(fake.clone());
    for reason in [Some(private_path), None] {
        fake.snapshot.lock().unwrap()["switching_disabled"] = serde_json::json!(reason);
        let body = dash.get(STATUS).await.json(StatusCode::OK);
        assert_eq!(
            body["supervisor"]["switching_disabled"],
            serde_json::json!(reason.map(|_| "switching is disabled"))
        );
        assert!(!body.to_string().contains("private"));
    }
}

#[tokio::test]
async fn status_prefers_a_pending_journal_outcome_to_older_state_history() {
    use open_ferry_update::journal::{JOURNAL_FILE, Journal, Outcome};
    use open_ferry_update::state::SwitchRecord;
    let fake = Switcher::new(Duration::ZERO, None);
    let (dash, scratch) = supervised(fake);
    let path = scratch.path().join(JOURNAL_FILE);
    let mut journal: Journal = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut record = SwitchRecord::started(
        "previous-switch",
        "rollback",
        "0.2.0",
        "0.1.0",
        "2026-10-18T16:00:00Z",
    );
    record.result = Some("rolled-back".into());
    record.why = Some("the operator requested it".into());
    let data = DataDir::at(scratch.path());
    let mut state = open_ferry_update::State::load(&data.state_file());
    let mut older =
        SwitchRecord::started("older", "update", "0.0.9", "0.1.0", "2026-10-17T16:00:00Z");
    older.result = Some("switched".into());
    state.upsert_record(older);
    state.save(&data.state_file()).unwrap();
    let state_before = std::fs::read(data.state_file()).unwrap();
    journal.outcomes.push(Outcome {
        record,
        mark_failed: None,
    });
    std::fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
    let body = dash.get(STATUS).await.json(StatusCode::OK);
    let last = &body["supervisor"]["last_switch"];
    assert_eq!(last["result"], "rolled-back");
    assert_eq!(last["to"], "0.1.0");
    assert_eq!(last["reason"], "the operator requested it");
    assert!(last["auto"].is_null());
    assert_eq!(last["how"], "rollback");
    assert_eq!(last["from"], "0.2.0");
    assert_eq!(
        std::fs::read(data.state_file()).unwrap(),
        state_before,
        "GET did not flush the pending outcome into state"
    );
}

#[tokio::test(start_paused = true)]
async fn a_busy_update_lock_returns_conflict_without_calling_the_supervisor() {
    let fake = Switcher::new(Duration::ZERO, None);
    let (dash, scratch) = supervised(fake.clone());
    let data = DataDir::at(scratch.path());
    let _lock = data.try_lock().unwrap().unwrap();
    let reason = dash
        .call(Method::POST, INSTALL, "")
        .await
        .error(StatusCode::CONFLICT, "update_busy");
    assert_eq!(reason, "busy, try again shortly");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn defensive_ended_mapping_reports_failure_rather_than_refusal() {
    for delay in [Duration::ZERO, Duration::from_secs(5)] {
        let fake = Switcher::new(delay, None);
        *fake.error.lock().unwrap() = Some(open_ferry_update::SwitchError::Ended(
            "the new worker exited".into(),
        ));
        let (dash, _scratch) = supervised(fake.clone());
        let body = dash
            .call(Method::POST, INSTALL, "")
            .await
            .json(StatusCode::ACCEPTED);
        if delay.is_zero() {
            assert_eq!(body["failed"], true);
            assert_eq!(body["reason"], "the new worker exited");
        } else {
            assert_eq!(body["preparing"], true);
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
        }
        let status = dash.get(STATUS).await.json(StatusCode::OK);
        assert_eq!(status["switch_request"]["state"], "failed");
        assert_eq!(body["request_id"], status["switch_request"]["request_id"]);
        assert_eq!(status["switch_request"]["reason"], "the new worker exited");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn unreadable_optional_history_keeps_live_status_and_returns_a_fixed_note() {
    let (dash, scratch) = supervised(Switcher::new(Duration::ZERO, None));
    std::fs::write(
        DataDir::at(scratch.path()).state_file(),
        b"invalid private state data",
    )
    .unwrap();
    let body = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(
        body["supervisor"]["notes"],
        serde_json::json!(["the update state could not be read; its history is unavailable"])
    );
    assert!(body["supervisor"]["error"].is_null());
    assert_eq!(body["supervisor"]["stale"], false);
    assert_eq!(body["supervisor"]["phase"], "idle");
    assert_eq!(body["supervisor"]["rollback_target"], "0.0.9");
    assert!(body["supervisor"]["switching_disabled"].is_null());
    assert!(
        !body
            .to_string()
            .contains(&scratch.path().to_string_lossy().into_owned())
    );
}

#[tokio::test(start_paused = true)]
async fn a_status_read_started_before_acceptance_does_not_clear_pending() {
    for delay in [Duration::ZERO, Duration::from_millis(1500)] {
        let fake = Switcher::new(delay, None);
        let (dash, _scratch) = supervised(fake.clone());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *fake.status_gate.lock().unwrap() = Some((started.clone(), release.clone()));
        let read = dash.get(STATUS);
        tokio::pin!(read);
        tokio::select! {
            _ = &mut read => panic!("the old snapshot must stay gated"),
            () = started.notified() => {}
        }
        let reply = dash
            .call(Method::POST, INSTALL, "")
            .await
            .json(StatusCode::ACCEPTED);
        if delay.is_zero() {
            assert_eq!(reply["accepted"], true);
        } else {
            assert_eq!(reply["preparing"], true);
        }
        // A 202 can still be preparing. Wait for the task to record acceptance
        // without reading status, which would itself clear pending.
        dash.state
            .update_requests
            .wait_for_preparation_for_test()
            .await;
        release.notify_one();
        let stale_read = read.await.json(StatusCode::OK);
        assert_eq!(stale_read["switch_request"]["state"], "accepted");
        assert_eq!(stale_read["switch_request"]["pending"], true);
        assert_eq!(
            reply["request_id"],
            stale_read["switch_request"]["request_id"]
        );
        let fresh_read = dash.get(STATUS).await.json(StatusCode::OK);
        assert_eq!(fresh_read["switch_request"]["state"], "accepted");
        assert_eq!(fresh_read["switch_request"]["pending"], false);
    }
}

#[tokio::test]
async fn a_live_supervisor_resets_stale_reads_and_the_next_failure_starts_at_one() {
    let fake = Switcher::new(Duration::ZERO, None);
    *fake.snapshot.lock().unwrap() = serde_json::json!({"state": "switching", "switch": {"status": "loading"}, "switching_disabled": null});
    let (dash, _scratch) = supervised(fake.clone());
    let fresh = dash.get(STATUS).await.json(StatusCode::OK);
    *fake.snapshot.lock().unwrap() = serde_json::Value::Null;
    for count in 1..=3 {
        let stale = dash.get(STATUS).await.json(StatusCode::OK);
        let supervisor = &stale["supervisor"];
        assert_eq!(supervisor["phase"], "loading");
        assert_eq!(supervisor["stale"], true);
        assert_eq!(supervisor["failed_reads"], count);
        let age = supervisor["snapshot_age_seconds"].as_f64().unwrap();
        assert!(
            (0.0..30.0).contains(&age),
            "cached snapshot is within the age bound"
        );
    }
    *fake.snapshot.lock().unwrap() =
        serde_json::json!({"state": "idle", "switch": null, "switching_disabled": null});
    let recovered = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(recovered["supervisor"]["stale"], false);
    assert_eq!(recovered["supervisor"]["failed_reads"], 0);
    assert_eq!(
        recovered["supervisor"]["error"],
        fresh["supervisor"]["error"]
    );
    assert_eq!(recovered["supervisor"]["phase"], "idle");
    *fake.snapshot.lock().unwrap() = serde_json::Value::Null;
    let stale = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(stale["supervisor"]["failed_reads"], 1);
}

#[tokio::test]
async fn get_recovers_a_preparation_task_that_ended_without_a_result() {
    let (dash, _scratch) = supervised(Switcher::new(Duration::ZERO, None));
    let request_id = dash
        .state
        .update_requests
        .ended_preparation_for_test("install")
        .await;
    let body = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(body["switch_request"]["state"], "failed");
    assert_eq!(body["switch_request"]["request_id"], request_id);
    assert_eq!(body["switch_request"]["pending"], false);
    assert_eq!(
        body["switch_request"]["reason"],
        "preparing the switch stopped"
    );
}

#[tokio::test(start_paused = true)]
async fn post_recovers_a_dead_preparation_before_admitting_a_new_request() {
    for (path, action) in [(INSTALL, "install"), (ROLLBACK, "rollback")] {
        let fake = Switcher::new(Duration::ZERO, None);
        let (dash, _scratch) = supervised(fake.clone());
        dash.state
            .update_requests
            .ended_preparation_for_test(action)
            .await;
        let reply = dash
            .call(Method::POST, path, switch_body(path))
            .await
            .json(StatusCode::ACCEPTED);
        assert_eq!(reply["accepted"], true);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let body = dash.get(STATUS).await.json(StatusCode::OK);
        assert_eq!(body["switch_request"]["state"], "accepted");
        assert!(body["switch_request"]["reason"].is_null());
    }
}

#[tokio::test(start_paused = true)]
async fn request_ids_differ_between_requests_and_workers() {
    let mut ids = std::collections::HashSet::new();
    for _ in 0..2 {
        let (dash, _scratch) = supervised(Switcher::new(Duration::ZERO, None));
        for _ in 0..2 {
            dash.call(Method::POST, INSTALL, "")
                .await
                .json(StatusCode::ACCEPTED);
            let status = dash.get(STATUS).await.json(StatusCode::OK);
            assert!(
                ids.insert(
                    status["switch_request"]["request_id"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                )
            );
        }
    }
}

#[tokio::test]
async fn the_status_exposes_the_explicit_preview_channel() {
    let data = tempfile::tempdir().unwrap();
    let fetch = Arc::new(Counting::default());
    let mut config = keyed_config();
    config.self_update.channel = "preview".into();
    let dash = Dash::with_updates(config.clone(), service(&config, &fetch, data.path()));
    let body = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(body["channel"], "preview");
    assert_eq!(body["release_source"], serde_json::Value::Null);
}

#[tokio::test]
async fn channel_errors_are_visible_only_for_the_current_channel() {
    let scratch = tempfile::tempdir().unwrap();
    let data = DataDir::at(scratch.path());
    open_ferry_update::State {
        channel: Some("preview".into()),
        latest: Some("0.3.0-rc.1".into()),
        staged: Some("0.3.0-rc.1".into()),
        release_source: Some("preview".into()),
        channel_error: Some("preview source: HTTP 503".into()),
        ..Default::default()
    }
    .save(&data.state_file())
    .unwrap();
    let fetch = Arc::new(Counting::default());
    let mut config = config_in("off");
    config.self_update.channel = "preview".into();
    let updates = service(&config, &fetch, scratch.path());
    let dash = Dash::with_updates(config, updates.clone());
    let body = dash.get(STATUS).await.json(StatusCode::OK);
    assert_eq!(body["channel_error"], "preview source: HTTP 503");
    assert_eq!(body["staged_version"], "0.3.0-rc.1");
    updates.set_config(&config_in("off"));
    let body = dash.get(STATUS).await.json(StatusCode::OK);
    for field in [
        "latest_version",
        "staged_version",
        "release_source",
        "channel_error",
    ] {
        assert!(body[field].is_null(), "{body}");
    }
    assert_eq!(body["channel"], "stable");
}

#[tokio::test]
async fn install_now_does_not_reuse_an_old_preview_stage() {
    let fake = Switcher::new(Duration::ZERO, None);
    let (dash, scratch) = supervised(fake.clone());
    let data = DataDir::at(scratch.path());
    let mut state = open_ferry_update::State::load(&data.state_file());
    state.channel = Some("preview".into());
    state.save(&data.state_file()).unwrap();
    dash.call(Method::POST, INSTALL, "")
        .await
        .error(StatusCode::CONFLICT, "switch_failed");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

struct PolicyGateRunner {
    arrived: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl open_ferry_update::runner::Runner for PolicyGateRunner {
    fn version<'a>(
        &'a self,
        binary: &'a Path,
        _: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.arrived.notify_one();
            self.release.notified().await;
            std::fs::read_to_string(binary).map_err(|e| e.to_string())
        })
    }
}

#[tokio::test]
async fn install_now_rejects_a_channel_change_during_preparation() {
    let mut f = open_ferry_update::test_support::Fixture::new().await;
    f.release("0.2.0");
    let arrived = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    f.updater.runner = Arc::new(PolicyGateRunner {
        arrived: arrived.clone(),
        release: release.clone(),
    });
    let mut config = config_in("off");
    config.self_update.channel = "preview".into();
    let fake = Switcher::new(Duration::ZERO, None);
    let updates = UpdateService::new(
        f.updater,
        open_ferry_update::background::http_fetch(),
        &config,
    )
    .with_switcher(fake.clone());
    let dash = Dash::with_updates(config, updates.clone());
    let (response, ()) = tokio::join!(dash.call(Method::POST, INSTALL, ""), async {
        arrived.notified().await;
        updates.set_config(&config_in("off"));
        release.notify_one();
    });
    response.error(StatusCode::CONFLICT, "update_channel_changed");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}
