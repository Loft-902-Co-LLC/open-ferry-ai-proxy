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
