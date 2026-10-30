//! What management does for a switch of versions (v3 12.3, 7.3 and 13):
//! the handlers that write are tasks of the active gate, the config writer
//! takes a permit per write, and the "updating" answer and the ban hand over
//! work as the design says.
//!
//! These not upstream's. The gate itself is tested in open-ferry-core.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use http::{Method, Request, StatusCode};
use open_ferry_core::auth::Auth;
use open_ferry_core::auth::synthesizer::StableIdGenerator;
use open_ferry_core::config::AuthFile;
use open_ferry_core::exec::{
    ExecError, Options, Request as ExecRequest, Response as ExecResponse, StreamResponse,
};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::gate::{ActiveGate, CloseError, GateState};
use open_ferry_core::manager::Settings;
use serde_json::json;
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio::task::JoinHandle;

use super::{
    Answer, Api, AuthDir, FakeSync, LOCAL, Multipart, auth, file_auth, keyed, request_from,
};
use crate::handover::Ban;
use crate::{
    CredentialSync, FileConfigWriter, Handover, SyncFuture, UndoCheck, UndoError, router,
    undo_config,
};

const CODEX: &str = r#"{"type":"codex","email":"user@example.com"}"#;

const OTHER: &str = r#"{"type":"codex","email":"other@example.com"}"#;

const SWITCHING: &str = r#"{"error":"open-ferry is switching versions; try again in a minute"}"#;

const UPDATING: &str = r#"{"error":"updating, try again"}"#;

/// A [`CredentialSync`] that makes each call as a [`FakeSync`] does, then
/// holds it until `release` has a permit: the work of a handler that has
/// been admitted and is not done.
struct HoldSync {
    inner: Arc<FakeSync>,
    /// Told once for each call that reaches the hold.
    reached: Notify,
    release: Semaphore,
}

impl HoldSync {
    fn hold<'a>(&'a self, call: SyncFuture<'a>) -> SyncFuture<'a> {
        Box::pin(async move {
            self.reached.notify_one();
            self.release.acquire().await.expect("open").forget();
            call.await
        })
    }
}

impl CredentialSync for HoldSync {
    fn upsert(&self, auth: open_ferry_core::auth::Auth) -> SyncFuture<'_> {
        self.hold(CredentialSync::upsert(&*self.inner, auth))
    }

    fn file_written(&self, file: AuthFile) -> SyncFuture<'_> {
        self.hold(self.inner.file_written(file))
    }

    fn file_removed(&self, path: std::path::PathBuf) -> SyncFuture<'_> {
        self.hold(self.inner.file_removed(path))
    }
}

/// The API over `dir` with `gate` shared by the store, the manager and the
/// state, and a sync that holds every call until released.
fn gated(dir: &AuthDir, gate: &ActiveGate) -> (Arc<Api>, Arc<HoldSync>) {
    let mut api = Api::over(dir);
    dir.store.set_gate(gate.clone());
    api.manager.set_gate(gate.clone());
    let hold = Arc::new(HoldSync {
        inner: Arc::clone(&api.sync),
        reached: Notify::new(),
        release: Semaphore::new(0),
    });
    let state = api
        .state
        .clone()
        .with_gate(gate.clone())
        .with_sync(Arc::clone(&hold) as _);
    api.router = router(state.clone());
    api.state = state;
    (Arc::new(api), hold)
}

async fn close(gate: &ActiveGate) {
    gate.close(gate.epoch()).await.expect("closes");
    assert_eq!(gate.state(), GateState::Closed);
}

fn names(dir: &AuthDir) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        // Codex credentials keep a permanent lock file, as upstream does.
        .filter(|name| !name.to_ascii_lowercase().ends_with(".json.lock"))
        .collect();
    names.sort();
    names
}

// 12(c): a management write that arrives after the gate has closed is
// refused with 503, and changes nothing.
#[tokio::test]
async fn credential_writes_are_refused_once_the_gate_is_closed() {
    let dir = AuthDir::new();
    dir.write("kept.json", CODEX);
    let gate = ActiveGate::open();
    let (api, hold) = gated(&dir, &gate);
    hold.release.add_permits(100);
    close(&gate).await;

    let upload = api
        .post("/v0/management/auth-files?name=new.json", OTHER)
        .await;
    upload.assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
    let delete = api
        .send(keyed(
            Method::DELETE,
            "/v0/management/auth-files?name=kept.json",
            "",
        ))
        .await;
    delete.assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
    let delete_all = api
        .send(keyed(
            Method::DELETE,
            "/v0/management/auth-files?all=true",
            "",
        ))
        .await;
    delete_all.assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);

    assert_eq!(names(&dir), ["kept.json"]);
    assert!(api.sync.calls().is_empty());

    // The switch did not happen: the gate opens, and the writes work again.
    gate.reopen();
    api.post("/v0/management/auth-files?name=new.json", OTHER)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    assert_eq!(names(&dir), ["kept.json", "new.json"]);
}

// 12(c): a handler that was admitted before the switch began finishes, its
// later writes included, while the gate is quiescing, and the gate closes
// only after it; a request that comes meanwhile is refused.
#[tokio::test]
async fn an_admitted_delete_all_finishes_its_writes_while_the_gate_quiesces() {
    let dir = AuthDir::new();
    dir.write("a.json", CODEX);
    dir.write("b.json", OTHER);
    let gate = ActiveGate::open();
    let (api, hold) = gated(&dir, &gate);

    let running = tokio::spawn({
        let api = Arc::clone(&api);
        async move {
            api.send(keyed(
                Method::DELETE,
                "/v0/management/auth-files?all=true",
                "",
            ))
            .await
        }
    });
    // The first file is removed and the service is being told.
    hold.reached.notified().await;
    assert_eq!(names(&dir), ["b.json"]);

    let epoch = gate.epoch();
    let mut closing = tokio::spawn({
        let gate = gate.clone();
        async move { gate.close(epoch).await }
    });
    while gate.state() == GateState::Open {
        tokio::task::yield_now().await;
    }
    assert_eq!(gate.state(), GateState::Quiescing);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut closing)
            .await
            .is_err(),
        "the gate closed under an admitted handler"
    );
    api.post("/v0/management/auth-files?name=late.json", OTHER)
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);

    hold.release.add_permits(100);
    running
        .await
        .unwrap()
        .assert(StatusCode::OK, r#"{"deleted":2,"status":"ok"}"#);
    closing.await.unwrap().expect("closes");

    // The second file was removed on a blocking thread of the same
    // operation, after the gate was quiescing.
    assert_eq!(names(&dir), Vec::<String>::new());
    assert_eq!(gate.state(), GateState::Closed);
}

// 12(c): a client that goes away does not stop an admitted upload.
#[tokio::test]
async fn an_upload_whose_client_went_away_still_finishes() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let (api, hold) = gated(&dir, &gate);

    let request = tokio::spawn({
        let api = Arc::clone(&api);
        async move {
            api.post("/v0/management/auth-files?name=up.json", CODEX)
                .await
        }
    });
    hold.reached.notified().await;
    request.abort();
    let _ = request.await;

    let epoch = gate.epoch();
    let closing = tokio::spawn({
        let gate = gate.clone();
        async move { gate.close(epoch).await }
    });
    hold.release.add_permits(100);
    closing.await.unwrap().expect("closes after the upload");

    assert_eq!(names(&dir), ["up.json"]);
    assert_eq!(api.sync.calls().len(), 1);
}

// 12(d): the config writer takes a permit per write. Each of the four is
// refused with 503 while the gate is closed, and nothing is written.
#[tokio::test]
async fn config_writes_are_refused_once_the_gate_is_closed() {
    let dir = AuthDir::new();
    let mut api = Api::over_config_file(&dir, "port: 8317\ndebug: false\n");
    let gate = ActiveGate::open();
    let writer = FileConfigWriter::new(dir.config_path()).with_gate(gate.clone());
    let state = api.state.clone().with_config_writer(Arc::new(writer));
    api.router = router(state.clone());
    api.state = state;
    let before = std::fs::read_to_string(dir.config_path()).unwrap();
    close(&gate).await;

    // save_preserving_comments
    api.call(Method::PUT, "/v0/management/debug", r#"{"value":true}"#)
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
    // write_file
    api.call(Method::PUT, "/v0/management/config.yaml", "debug: true\n")
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
    // edit_v8
    api.call(Method::PATCH, "/v8/management/config/debug", "true")
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
    // undo
    let undone = undo_config(&api.state, UndoCheck::default()).await;
    assert_eq!(undone, Err(UndoError::Switching));

    assert_eq!(std::fs::read_to_string(dir.config_path()).unwrap(), before);
    assert!(!api.state.config().debug);

    gate.reopen();
    api.call(Method::PUT, "/v0/management/debug", r#"{"value":true}"#)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    assert!(api.state.config().debug);
}

// 12(e): while the "updating" answer is on, every management route answers
// 503 with `Connection: close`, and a refused request is not a failed
// attempt.
#[tokio::test]
async fn the_updating_answer_is_outermost_and_counts_no_attempt() {
    let api = Api::new();
    api.state.set_updating(true);
    assert!(api.state.is_updating());

    for path in [
        "/v0/management/auth-files",
        "/v8/management/config",
        "/v0/management/no-such-route",
        "/v0/management",
    ] {
        // Without a key, the failed attempts the ban counts.
        for _ in 0..8 {
            let answer = api.send(request_from(LOCAL, Method::GET, path, "")).await;
            answer.assert(StatusCode::SERVICE_UNAVAILABLE, UPDATING);
            assert_eq!(answer.header("connection"), Some("close"));
        }
    }
    // Nor does a request with the key get through.
    api.get("/v0/management/auth-files")
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, UPDATING);

    // Switched off, management works, and the address is not banned.
    api.state.set_updating(false);
    let missing = api
        .send(request_from(
            LOCAL,
            Method::GET,
            "/v0/management/auth-files",
            "",
        ))
        .await;
    assert_eq!(missing.status, StatusCode::UNAUTHORIZED, "{}", missing.body);
    api.get("/v0/management/auth-files")
        .await
        .expect(StatusCode::OK);
}

// 12(e) and (g): failures counted before the snapshot are handed over;
// failures of requests already past the layer, after it, are not recorded;
// a hand over that does not happen leaves the record as it was.
#[tokio::test]
async fn bans_are_frozen_at_the_snapshot_and_restored_in_the_next_worker() {
    let api = Api::new();
    let bad = || request_from(LOCAL, Method::GET, "/v0/management/auth-files", "");
    for _ in 0..5 {
        api.send(bad()).await;
    }
    // Banned now.
    assert_eq!(api.send(bad()).await.status, StatusCode::FORBIDDEN);

    api.state.set_updating(true);
    let handover = api.state.freeze_attempts("switch-1");
    assert_eq!(handover.id, "switch-1");
    assert_eq!(handover.bans.len(), 1);
    assert_eq!(handover.bans[0].ip, "127.0.0.1");
    assert!(handover.bans[0].blocked_for_ms > 29 * 60_000);

    // The file's format.
    let again = Handover::from_json(&handover.to_json()).unwrap();
    assert_eq!(again, handover);

    // A request past the layer when the snapshot was taken: not recorded.
    api.state.set_updating(false);
    let other = || request_from("[::1]:1", Method::GET, "/v0/management/auth-files", "");
    for _ in 0..7 {
        api.send(other()).await;
    }
    assert_eq!(api.send(other()).await.status, StatusCode::UNAUTHORIZED);

    // The next worker takes the ban in, and the one that was not recorded
    // is not there.
    let next = Api::new();
    next.state.restore_attempts(&handover);
    assert_eq!(next.send(bad()).await.status, StatusCode::FORBIDDEN);
    assert_eq!(next.send(other()).await.status, StatusCode::UNAUTHORIZED);

    // The hand over did not happen: A records again.
    api.state.unfreeze_attempts();
    for _ in 0..5 {
        api.send(other()).await;
    }
    assert_eq!(api.send(other()).await.status, StatusCode::FORBIDDEN);
}

// C-R5: a handed-over count at or above the limit is not restored, so the
// next failure counts from 1 and doesn't overflow.
#[tokio::test]
async fn a_failure_count_at_the_limit_is_not_restored() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let handover = Handover {
        id: "switch-2".into(),
        at: now,
        bans: vec![
            Ban {
                ip: "127.0.0.1".into(),
                count: u32::MAX,
                blocked_for_ms: 0,
                idle_ms: 0,
            },
            Ban {
                ip: "::1".into(),
                count: 5,
                blocked_for_ms: 0,
                idle_ms: 0,
            },
        ],
    };
    let api = Api::new();
    api.state.restore_attempts(&handover);
    let bad = || request_from(LOCAL, Method::GET, "/v0/management/auth-files", "");

    // The failures count from 1: four are not a ban, the fifth is.
    for _ in 0..5 {
        assert_eq!(api.send(bad()).await.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(api.send(bad()).await.status, StatusCode::FORBIDDEN);

    // Nor does the count of 5 stand for a ban that wasn't made.
    let other = || request_from("[::1]:1", Method::GET, "/v0/management/auth-files", "");
    assert_eq!(api.send(other()).await.status, StatusCode::UNAUTHORIZED);
}

/// A codex credential saved as file `name` of `dir`, as the watcher would
/// register it.
fn credential(dir: &AuthDir, name: &str, contents: &str) -> Auth {
    let mut auth = file_auth(&dir.path(), name, name, contents);
    auth.metadata = serde_json::from_str(contents).unwrap();
    auth
}

const STATUS: &str = "/v0/management/auth-files/status";
const FIELDS: &str = "/v0/management/auth-files/fields";
const REFRESH: &str = "/v0/management/auth-files/refresh";

/// `PATCH path` with `body`, with the key.
async fn patch(api: &Api, path: &str, body: &str) -> Answer {
    api.send(keyed(Method::PATCH, path, body)).await
}

/// Starts closing `gate` once the request sent before is waiting on the
/// credential lock, and checks that it is held up: the request was admitted.
async fn quiesce_behind(gate: &ActiveGate) -> JoinHandle<Result<(), CloseError>> {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    let epoch = gate.epoch();
    let mut closing = tokio::spawn({
        let gate = gate.clone();
        async move { gate.close(epoch).await }
    });
    while gate.state() == GateState::Open {
        tokio::task::yield_now().await;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut closing)
            .await
            .is_err(),
        "the gate closed: the request wasn't admitted"
    );
    closing
}

// C-R2: a status or field change that arrives once the gate has closed is
// refused with 503, before it changes the credential in memory or its file.
#[tokio::test]
async fn status_and_field_changes_are_refused_once_the_gate_is_closed() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let (api, hold) = gated(&dir, &gate);
    hold.release.add_permits(100);
    api.register(credential(
        &dir,
        "a.json",
        r#"{"type":"codex","disabled":false}"#,
    ));
    let file = std::fs::read(dir.path().join("a.json")).unwrap();
    let before = api.manager.get("a.json").unwrap();
    close(&gate).await;

    for (path, body) in [
        (STATUS, r#"{"name":"a.json","disabled":true}"#),
        (
            "/v8/management/credentials/status",
            r#"{"name":"a.json","disabled":true}"#,
        ),
        (FIELDS, r#"{"name":"a.json","priority":3}"#),
        (
            "/v8/management/credentials/fields",
            r#"{"name":"a.json","priority":3}"#,
        ),
    ] {
        patch(&api, path, body)
            .await
            .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
    }

    let after = api.manager.get("a.json").unwrap();
    assert_eq!(after.disabled, before.disabled);
    assert_eq!(after.metadata, before.metadata);
    assert_eq!(after.generation, before.generation);
    assert_eq!(std::fs::read(dir.path().join("a.json")).unwrap(), file);
    assert!(api.sync.calls().is_empty());

    // The switch did not happen: the change goes through.
    gate.reopen();
    patch(&api, STATUS, r#"{"name":"a.json","disabled":true}"#)
        .await
        .assert(StatusCode::OK, r#"{"disabled":true,"status":"ok"}"#);
    assert_eq!(dir.read_json("a.json")["disabled"], json!(true));
}

// C-R2: a status change admitted while the gate was open saves its file
// while the gate quiesces, and the gate closes only after it.
#[tokio::test]
async fn an_admitted_status_change_saves_while_the_gate_quiesces() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let (api, hold) = gated(&dir, &gate);
    hold.release.add_permits(100);
    api.register(credential(
        &dir,
        "a.json",
        r#"{"type":"codex","disabled":false}"#,
    ));

    let lock = api.state.credential_lock().lock().await;
    let request = tokio::spawn({
        let api = Arc::clone(&api);
        async move { patch(&api, STATUS, r#"{"name":"a.json","disabled":true}"#).await }
    });
    let closing = quiesce_behind(&gate).await;
    assert_eq!(gate.state(), GateState::Quiescing);
    drop(lock);

    request
        .await
        .unwrap()
        .assert(StatusCode::OK, r#"{"disabled":true,"status":"ok"}"#);
    closing.await.unwrap().expect("closes after the change");
    assert_eq!(dir.read_json("a.json")["disabled"], json!(true));
    assert!(api.manager.get("a.json").unwrap().disabled);
}

// C-R2: the same for a field change.
#[tokio::test]
async fn an_admitted_field_change_saves_while_the_gate_quiesces() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let (api, hold) = gated(&dir, &gate);
    hold.release.add_permits(100);
    api.register(credential(&dir, "a.json", r#"{"type":"codex"}"#));

    let lock = api.state.credential_lock().lock().await;
    let request = tokio::spawn({
        let api = Arc::clone(&api);
        async move { patch(&api, FIELDS, r#"{"name":"a.json","note":"kept"}"#).await }
    });
    let closing = quiesce_behind(&gate).await;
    drop(lock);

    request
        .await
        .unwrap()
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    closing.await.unwrap().expect("closes after the change");
    assert_eq!(dir.read_json("a.json")["note"], json!("kept"));
}

// C-R2: a config API key's status change carries its operation into the
// config write and the reload, so the config is saved while the gate
// quiesces.
#[tokio::test]
async fn an_admitted_config_key_change_writes_the_config_while_the_gate_quiesces() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let raw = "codex-api-key:\n  - api-key: sk-test\n    base-url: https://example.com/v1\n    excluded-models:\n      - gpt-5\n";
    let mut api = Api::over_config_file(&dir, raw);
    dir.store.set_gate(gate.clone());
    api.manager.set_gate(gate.clone());
    let writer = FileConfigWriter::new(dir.config_path()).with_gate(gate.clone());
    let state = api
        .state
        .clone()
        .with_gate(gate.clone())
        .with_config_writer(Arc::new(writer));
    api.router = router(state.clone());
    api.state = state;
    let parts = ["sk-test", "https://example.com/v1", "", "", ""];
    let (id, _) = StableIdGenerator::new().next("codex:apikey", &parts);
    api.register(auth(
        &id,
        &[
            ("api_key", "sk-test"),
            ("base_url", "https://example.com/v1"),
            ("source", "config:codex[abc]"),
        ],
    ));
    let api = Arc::new(api);

    let lock = api.state.credential_lock().lock().await;
    let body = format!(r#"{{"name":"{id}","disabled":true}}"#);
    let request = tokio::spawn({
        let api = Arc::clone(&api);
        async move { patch(&api, STATUS, &body).await }
    });
    let closing = quiesce_behind(&gate).await;
    drop(lock);

    let answer = request.await.unwrap();
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    closing.await.unwrap().expect("closes after the change");
    // The config as saved is the one asked for: that key is disabled and
    // the rest of it is as it was.
    let saved = std::fs::read_to_string(dir.config_path()).unwrap();
    let (config, _) = open_ferry_core::config::Config::load_with_sha256(dir.config_path())
        .unwrap_or_else(|error| panic!("{error}: {saved}"));
    let (before, _) = {
        let other = AuthDir::new();
        std::fs::write(other.config_path(), raw).unwrap();
        open_ferry_core::config::Config::load_with_sha256(other.config_path()).unwrap()
    };
    assert_eq!(config.codex_api_key.len(), 1, "{saved}");
    let key = &config.codex_api_key[0];
    let mut excluded = key.excluded_models.clone();
    excluded.sort();
    assert_eq!(excluded, ["*", "gpt-5"], "{saved}");
    assert_eq!(key.api_key, "sk-test");
    assert_eq!(key.base_url, "https://example.com/v1");
    let was = &before.codex_api_key[0];
    assert_eq!(
        was.excluded_models,
        ["gpt-5"],
        "the old key was not disabled"
    );
}

/// A Codex executor whose refresh waits to be released.
struct HeldRefresh {
    refreshes: AtomicUsize,
    /// Told once for each refresh that starts.
    reached: Notify,
    release: Semaphore,
}

impl HeldRefresh {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            refreshes: AtomicUsize::new(0),
            reached: Notify::new(),
            release: Semaphore::new(0),
        })
    }
}

type ExecFuture<'a, T> = BoxFuture<'a, Result<T, ExecError>>;

fn unused<'a, T: Send + 'a>() -> ExecFuture<'a, T> {
    Box::pin(std::future::ready(Err(ExecError::upstream(
        501, "not used",
    ))))
}

impl ProviderExecutor for HeldRefresh {
    fn id(&self) -> &str {
        "codex"
    }

    fn execute(&self, _: Arc<Auth>, _: ExecRequest, _: Options) -> ExecFuture<'_, ExecResponse> {
        unused()
    }

    fn execute_stream(
        &self,
        _: Arc<Auth>,
        _: ExecRequest,
        _: Options,
    ) -> ExecFuture<'_, StreamResponse> {
        unused()
    }

    fn count_tokens(
        &self,
        _: Arc<Auth>,
        _: ExecRequest,
        _: Options,
    ) -> ExecFuture<'_, ExecResponse> {
        unused()
    }

    fn refresh(&self, auth: Arc<Auth>) -> ExecFuture<'_, Auth> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            self.reached.notify_one();
            self.release.acquire().await.expect("open").forget();
            let mut auth = Auth::clone(&auth);
            auth.metadata
                .insert("access_token".into(), json!("refreshed-token"));
            Ok(auth)
        })
    }
}

const REFRESHABLE: &str = r#"{"type":"codex","refresh_token":"r","access_token":"old"}"#;

// C-R4: a forced refresh refused because the gate isn't open answers 503, not
// 500, for one credential, and for all of them before any starts.
#[tokio::test]
async fn forced_refreshes_answer_503_once_the_gate_is_closed() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let (api, _hold) = gated(&dir, &gate);
    let executor = HeldRefresh::new();
    api.manager.register_executor(Arc::clone(&executor) as _);
    api.register(credential(&dir, "a.json", REFRESHABLE));
    api.register(credential(&dir, "b.json", REFRESHABLE));
    close(&gate).await;

    api.post(&format!("{REFRESH}?name=a.json"), "")
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
    api.post(&format!("{REFRESH}?all=true"), "")
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
    assert_eq!(executor.refreshes.load(Ordering::SeqCst), 0);
}

// C-R4: a refresh of all that a switch cuts short (the gate began closing
// after the first refresh started) answers 503 with the results attached.
#[tokio::test]
async fn a_refresh_of_all_cut_short_by_a_switch_answers_503_with_its_results() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let (api, _hold) = gated(&dir, &gate);
    api.manager.set_settings(Settings {
        refresh_workers: 1,
        ..Settings::default()
    });
    let executor = HeldRefresh::new();
    api.manager.register_executor(Arc::clone(&executor) as _);
    api.register(credential(&dir, "a.json", REFRESHABLE));
    api.register(credential(&dir, "b.json", REFRESHABLE));

    let request = tokio::spawn({
        let api = Arc::clone(&api);
        async move { api.post(&format!("{REFRESH}?all=true"), "").await }
    });
    // The first refresh is under way, and holds the switch.
    executor.reached.notified().await;
    let epoch = gate.epoch();
    let closing = tokio::spawn({
        let gate = gate.clone();
        async move { gate.close(epoch).await }
    });
    while gate.state() == GateState::Open {
        tokio::task::yield_now().await;
    }
    executor.release.add_permits(100);

    let answer = request.await.unwrap();
    assert_eq!(
        answer.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        answer.body
    );
    let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(body["error"], json!(SWITCHING_MESSAGE));
    assert_eq!(body["ok"], json!(false));
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "{body}");
    assert_eq!(results[0]["success"], json!(true));
    assert_eq!(results[1]["success"], json!(false));
    assert_eq!(executor.refreshes.load(Ordering::SeqCst), 1);
    closing.await.unwrap().expect("closes");
}

const SWITCHING_MESSAGE: &str = "open-ferry is switching versions; try again in a minute";

/// A request whose body is sent by the test, and what sends it.
fn trickled(request: Request<Body>) -> (Request<Body>, mpsc::Sender<Result<Bytes, Infallible>>) {
    let (sender, receiver) = mpsc::channel(1);
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    let (parts, _) = request.into_parts();
    (
        Request::from_parts(parts, Body::from_stream(stream)),
        sender,
    )
}

// C-L3: a handler reads its body before the gate admits it, so a client slow
// to send it doesn't hold up the switch; once the body is there, after the
// gate has closed, the request is refused with 503 and changes nothing.
#[tokio::test]
async fn a_body_still_arriving_does_not_hold_up_the_switch() {
    let cases = [
        (
            Method::POST,
            "/v0/management/auth-files?name=up.json",
            r#"{"type":"codex","#,
            r#""email":"x@example.com"}"#,
        ),
        (
            Method::DELETE,
            "/v0/management/auth-files",
            r#"["a."#,
            r#"json"]"#,
        ),
        (
            Method::PATCH,
            STATUS,
            r#"{"name":"a.json","#,
            r#""disabled":true}"#,
        ),
        (
            Method::PATCH,
            FIELDS,
            r#"{"name":"a.json","#,
            r#""priority":3}"#,
        ),
    ];
    for (method, path, first, rest) in cases {
        let dir = AuthDir::new();
        let gate = ActiveGate::open();
        let (api, hold) = gated(&dir, &gate);
        hold.release.add_permits(100);
        api.register(credential(
            &dir,
            "a.json",
            r#"{"type":"codex","disabled":false}"#,
        ));
        let file = std::fs::read(dir.path().join("a.json")).unwrap();

        let (request, sender) = trickled(keyed(method.clone(), path, ""));
        let sending = tokio::spawn({
            let api = Arc::clone(&api);
            async move { api.send(request).await }
        });
        sender.send(Ok(Bytes::from(first))).await.unwrap();
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        tokio::time::timeout(Duration::from_secs(2), close(&gate))
            .await
            .unwrap_or_else(|_| panic!("{method} {path}: the switch waited for the body"));

        sender.send(Ok(Bytes::from(rest))).await.unwrap();
        drop(sender);
        sending
            .await
            .unwrap()
            .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
        assert_eq!(names(&dir), ["a.json"], "{method} {path}");
        assert_eq!(std::fs::read(dir.path().join("a.json")).unwrap(), file);
        assert!(api.sync.calls().is_empty());
    }
}

/// A store that saves through `inner`, but holds each save, before it
/// reaches `inner`, until released: the work of a handler that has been
/// admitted and is not done.
struct SlowStore {
    inner: Arc<open_ferry_core::auth::file_store::FileStore>,
    /// Told once for each save that reaches the hold.
    reached: Notify,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl open_ferry_core::auth::AuthStore for SlowStore {
    fn list(&self) -> std::io::Result<Vec<Auth>> {
        self.inner.list()
    }

    fn save(&self, auth: &Auth) -> std::io::Result<String> {
        self.reached.notify_one();
        self.release
            .lock()
            .unwrap()
            .recv()
            .expect("the test releases the save");
        self.inner.save(auth)
    }

    fn delete(&self, id: &str) -> std::io::Result<()> {
        self.inner.delete(id)
    }
}

/// The API over `dir` with `gate` shared by the store, the manager and the
/// state, whose manager saves through a [`SlowStore`]; the sender releases
/// its saves, one for each message.
fn gated_slow_saves(
    dir: &AuthDir,
    gate: &ActiveGate,
) -> (Arc<Api>, Arc<SlowStore>, std::sync::mpsc::Sender<()>) {
    let (release, receive) = std::sync::mpsc::channel();
    let slow = Arc::new(SlowStore {
        inner: Arc::clone(&dir.store),
        reached: Notify::new(),
        release: std::sync::Mutex::new(receive),
    });
    let mut api = Api::build(
        dir.config(),
        None,
        Some(Arc::clone(&slow) as _),
        |state, sync| {
            state
                .with_store(Arc::clone(&dir.store))
                .with_sync(sync)
                .with_config_path(dir.config_path())
        },
    );
    dir.store.set_gate(gate.clone());
    api.manager.set_gate(gate.clone());
    let state = api.state.clone().with_gate(gate.clone());
    api.router = router(state.clone());
    api.state = state;
    (Arc::new(api), slow, release)
}

/// A credential whose quota is exhausted, saved as file `a.json` of `dir`.
fn exhausted(dir: &AuthDir) -> Auth {
    let mut auth = credential(dir, "a.json", r#"{"type":"codex"}"#);
    auth.quota = open_ferry_core::auth::QuotaState {
        exceeded: true,
        reason: "quota".into(),
        next_recover_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        backoff_level: 2,
        ..Default::default()
    };
    auth
}

const RESETS: [&str; 2] = [
    "/v0/management/reset-quota",
    "/v8/management/routing/cooldown/reset",
];

// C2-O3: a quota reset that arrives once the gate has closed is refused with
// 503, before it changes the credential in memory or its file.
#[tokio::test]
async fn quota_resets_are_refused_once_the_gate_is_closed() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let (api, hold) = gated(&dir, &gate);
    hold.release.add_permits(100);
    let index = api.register(exhausted(&dir));
    let file = std::fs::read(dir.path().join("a.json")).unwrap();
    let before = api.manager.get("a.json").unwrap();
    assert!(before.quota.exceeded);
    close(&gate).await;
    let body = json!({ "auth_index": index }).to_string();

    for path in RESETS {
        api.post(path, &body)
            .await
            .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
    }

    let after = api.manager.get("a.json").unwrap();
    assert_eq!(after.quota, before.quota);
    assert_eq!(after.generation, before.generation);
    assert_eq!(std::fs::read(dir.path().join("a.json")).unwrap(), file);

    // The switch did not happen: the reset goes through.
    gate.reopen();
    let answer = api.post(RESETS[0], &body).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(!api.manager.get("a.json").unwrap().quota.exceeded);
}

// C2-O3: a quota reset admitted while the gate was open saves its file while
// the gate quiesces, and the gate closes only after it.
#[tokio::test]
async fn an_admitted_quota_reset_finishes_while_the_gate_quiesces() {
    for path in RESETS {
        let dir = AuthDir::new();
        let gate = ActiveGate::open();
        let (api, slow, release) = gated_slow_saves(&dir, &gate);
        // Registering saves, too.
        release.send(()).unwrap();
        let index = api.register(exhausted(&dir));
        // The registration's save told `reached` too, and the permit is kept
        // for the next wait. Consume it, so the wait below is for the
        // reset's own save.
        slow.reached.notified().await;
        let body = json!({ "auth_index": index }).to_string();

        let request = tokio::spawn({
            let api = Arc::clone(&api);
            let body = body.clone();
            async move { api.post(path, &body).await }
        });
        slow.reached.notified().await;
        let closing = quiesce_behind(&gate).await;
        assert_eq!(gate.state(), GateState::Quiescing);
        // Published before it is saved.
        assert!(!api.manager.get("a.json").unwrap().quota.exceeded);

        // A request that comes meanwhile is refused.
        api.post(path, &body)
            .await
            .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);

        release.send(()).unwrap();
        let answer = request.await.unwrap();
        assert_eq!(answer.status, StatusCode::OK, "{path}: {}", answer.body);
        closing.await.unwrap().expect("closes after the reset");
        assert!(!api.manager.get("a.json").unwrap().quota.exceeded);
    }
}

// C2-O5: what is wrong with a request alone is answered as it is when the
// gate is open (400, or 500 for a file that isn't a credential), not with
// 503, once the gate has closed: a bad field or weight, a malformed service
// account, and a malformed upload. The same
// requests are refused the same way when the gate opens again.
#[tokio::test]
async fn bad_requests_are_answered_whatever_the_gate_is() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let (api, hold) = gated(&dir, &gate);
    hold.release.add_permits(100);
    api.register(credential(&dir, "a.json", r#"{"type":"codex"}"#));
    let file = std::fs::read(dir.path().join("a.json")).unwrap();

    let vertex = |contents: &str| {
        Multipart::new()
            .file("file", "sa.json", contents.as_bytes())
            .request(Method::POST, "/v0/management/vertex/import")
    };
    let v8_vertex = |contents: &str| {
        Multipart::new()
            .file("file", "sa.json", contents.as_bytes())
            .request(Method::POST, "/v8/management/oauth/import?provider=vertex")
    };
    let requests = |api: Arc<Api>| async move {
        let mut answers = Vec::new();
        for path in [FIELDS, "/v8/management/credentials/fields"] {
            for body in [
                r#"{"name":"a.json","weight":"heavy"}"#,
                r#"{"name":"a.json","weight":1.5}"#,
                r#"{"name":"a.json","weight":{"x":1}}"#,
                r#"{"name":"a.json"}"#,
            ] {
                answers.push(patch(&api, path, body).await);
            }
        }
        answers.push(api.send(vertex("not json")).await);
        answers.push(api.send(vertex(r#"{"type":"service_account"}"#)).await);
        answers.push(api.send(v8_vertex("[]")).await);
        answers.push(
            api.post("/v0/management/auth-files?name=bad.json", "not json")
                .await,
        );
        answers.push(
            api.post("/v0/management/auth-files?name=typeless.json", "{}")
                .await,
        );
        answers.push(
            api.post("/v0/management/auth-files?name=big.txt", "{}")
                .await,
        );
        let form = Multipart::new()
            .file("file", "notes.txt", b"{}")
            .request(Method::POST, "/v0/management/auth-files");
        answers.push(api.send(form).await);
        answers
    };

    let open = requests(Arc::clone(&api)).await;
    close(&gate).await;
    let closed = requests(Arc::clone(&api)).await;

    assert_eq!(open.len(), closed.len());
    for (index, (open, closed)) in open.iter().zip(&closed).enumerate() {
        assert_ne!(open.status, StatusCode::SERVICE_UNAVAILABLE, "{index}");
        // Invalid credential-file input remains a client error with the gate closed.
        assert!(open.status.as_u16() >= 400, "{index}: {}", open.body);
        assert_eq!(closed.status, open.status, "{index}: {}", closed.body);
        assert_eq!(closed.body, open.body, "{index}");
    }
    assert_eq!(names(&dir), ["a.json"]);
    assert_eq!(std::fs::read(dir.path().join("a.json")).unwrap(), file);

    // A good request is still refused.
    patch(&api, FIELDS, r#"{"name":"a.json","priority":3}"#)
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, SWITCHING);
}

/// The API over `dir` whose status and fields changes stop after their
/// lookup, until released: it is told on the receiver when one has, and the
/// sender lets it go on.
fn held_edits(
    dir: &AuthDir,
) -> (
    Arc<Api>,
    mpsc::UnboundedReceiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, reached) = mpsc::unbounded_channel();
    let (release, released) = std::sync::mpsc::channel();
    let released = std::sync::Mutex::new(released);
    let mut api = Api::over(dir);
    let state = api.state.clone().with_before_edit(move || {
        entered.send(()).unwrap();
        released
            .lock()
            .unwrap()
            .recv()
            .expect("the test releases the change");
    });
    api.router = router(state.clone());
    api.state = state;
    (Arc::new(api), reached, release)
}

// Ported from auth_files_patch_fields_test.go (v8.0.42): selection must not
// authorize changing a replacement registration, even under the same file ID.
#[tokio::test]
async fn status_and_fields_reject_reregistration_after_lookup() {
    for (path, body) in [
        (STATUS, r#"{"name":"a.json","disabled":true}"#),
        (FIELDS, r#"{"name":"a.json","note":"old-account-edit"}"#),
    ] {
        let dir = AuthDir::new();
        let (api, mut reached, release) = held_edits(&dir);
        api.register(credential(&dir, "a.json", r#"{"type":"codex"}"#));
        let request = tokio::spawn({
            let api = Arc::clone(&api);
            async move { patch(&api, path, body).await }
        });
        reached.recv().await.unwrap();
        let replacement = credential(&dir, "a.json", r#"{"type":"codex","note":"replacement"}"#);
        api.manager.register(replacement).unwrap();
        release.send(()).unwrap();
        let answer = request.await.unwrap();
        assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(answer.body.contains("stale registration epoch"));
        let current = api.manager.get("a.json").unwrap();
        assert!(!current.disabled);
        assert_eq!(current.metadata["note"], "replacement");
        assert!(api.sync.calls().is_empty());
        assert_eq!(dir.read_json("a.json")["note"], "replacement");
    }
}

// C3-2: a status or fields change is made on the credential as it is when
// the change is made. A refresh or an adoption that published and saved new
// tokens after the handler looked the credential up is kept, in memory and in
// the file, and the change is made too.
#[tokio::test]
async fn a_status_or_fields_change_keeps_tokens_saved_after_its_lookup() {
    for (path, body, key, value) in [
        (
            STATUS,
            r#"{"name":"a.json","disabled":true}"#,
            "disabled",
            json!(true),
        ),
        (
            FIELDS,
            r#"{"name":"a.json","priority":3}"#,
            "priority",
            json!(3),
        ),
    ] {
        let dir = AuthDir::new();
        let (api, mut reached, release) = held_edits(&dir);
        api.register(credential(
            &dir,
            "a.json",
            r#"{"type":"codex","access_token":"t0","refresh_token":"r0"}"#,
        ));

        let request = tokio::spawn({
            let api = Arc::clone(&api);
            async move { patch(&api, path, body).await }
        });
        reached.recv().await.expect("the change reached its hold");
        // What a refresh, or an adoption from the file, does meanwhile.
        let mut t1 = Auth::clone(&api.manager.get("a.json").unwrap());
        t1.metadata.insert("access_token".into(), json!("t1"));
        t1.metadata.insert("refresh_token".into(), json!("r1"));
        // A real refresh persists the rotation before it is adopted.
        dir.write("a.json", &serde_json::to_string(&t1.metadata).unwrap());
        api.manager.update(t1).unwrap();
        release.send(()).unwrap();
        let answer = request.await.unwrap();
        assert_eq!(answer.status, StatusCode::OK, "{path}: {}", answer.body);

        let live = api.manager.get("a.json").unwrap();
        assert_eq!(live.metadata["access_token"], json!("t1"), "{path}");
        assert_eq!(live.metadata["refresh_token"], json!("r1"), "{path}");
        assert_eq!(live.metadata[key], value, "{path}");
        let file = dir.read_json("a.json");
        assert_eq!(file["access_token"], json!("t1"), "{path}");
        assert_eq!(file["refresh_token"], json!("r1"), "{path}");
        assert_eq!(file[key], value, "{path}");
    }
}

// C3-3: the fields change answers in upstream's order: a credential that is
// not there is 404, before its fields are checked (400), whether the gate is
// open or closed; one that is there, with a bad weight, is 400, not 503.
#[tokio::test]
async fn a_missing_credential_is_404_before_its_fields_are_checked() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let (api, hold) = gated(&dir, &gate);
    hold.release.add_permits(100);
    api.register(credential(&dir, "a.json", r#"{"type":"codex"}"#));

    for closed in [false, true] {
        if closed {
            close(&gate).await;
        }
        for path in [FIELDS, "/v8/management/credentials/fields"] {
            let missing = patch(&api, path, r#"{"name":"nope.json","weight":"heavy"}"#).await;
            assert_eq!(missing.status, StatusCode::NOT_FOUND, "{path} {closed}");
            assert_eq!(missing.body, r#"{"error":"auth file not found"}"#);
            let present = patch(&api, path, r#"{"name":"a.json","weight":"heavy"}"#).await;
            assert_eq!(present.status, StatusCode::BAD_REQUEST, "{path} {closed}");
            let empty = patch(&api, path, r#"{"name":"a.json"}"#).await;
            assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{path} {closed}");
            assert_eq!(empty.body, r#"{"error":"no fields to update"}"#);
        }
    }
}

// C3-4: a form without a file is a bad request whatever the gate is: 400, not
// 503.
#[tokio::test]
async fn an_upload_without_a_file_is_400_whatever_the_gate_is() {
    let dir = AuthDir::new();
    let gate = ActiveGate::open();
    let (api, hold) = gated(&dir, &gate);
    hold.release.add_permits(100);

    for closed in [false, true] {
        if closed {
            close(&gate).await;
        }
        let form = Multipart::new().request(Method::POST, "/v0/management/auth-files");
        let answer = api.send(form).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{closed}");
        assert_eq!(answer.body, r#"{"error":"no files uploaded"}"#, "{closed}");
    }
    assert_eq!(names(&dir), Vec::<String>::new());
}
