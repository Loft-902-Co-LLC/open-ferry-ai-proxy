// Ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_status_sync_test.go, auth_files_patch_fields_test.go and
// auth_files_refresh_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The routes of `crate::credential_state`: turning credentials on and off,
//! changing their fields and refreshing them.
//!
//! Upstream calls its handlers on a bare gin context with a hook of its
//! own; here each test drives the management router with the key, over an
//! [`AuthDir`] whose [`FakeSync`](super::FakeSync) stands in for the hook
//! unless the test needs a sync of its own. Upstream's in-memory stores are
//! the auth directory's file store, so a change is also checked in the
//! credential's file.
//!
//! Deviations from upstream:
//! - `TestPatchPluginVirtualSourceStatusInvokesPostAuthPersistHook` and
//!   `TestPatchPluginVirtualSourceStatusHookErrorReturnsError` are dropped,
//!   and so is `TestSetSourceAuthFileDisabledNormalizesLegacyMetadata`,
//!   which tests the helper that turns a plugin source file on and off:
//!   there is no plugin host.
//! - `TestRefreshAuthFiles_PreservesPathInList_Issue6119` is dropped: the
//!   fix it checks is in the plugin host's refresh, which keeps the
//!   attributes a plugin leaves out of its answer.
//! - config_apikey_disable_test.go tests helpers, and is ported in
//!   `crate::config_sanitize`; `status_toggles_config_api_key` and
//!   `status_config_api_key_failures` turn a config API key off and on
//!   through the route, checking the config saved.
//! - `TestPatchAuthFileStatusHookErrorReturns500` stops the sync and expects
//!   503 with the stopped service's error, where upstream's hook returns its
//!   own error and the handler answers 500.
//! - `TestSyncAuthFilePriorityAttributeTracksFileSource`,
//!   `TestNormalizeAuthFilePatchFieldsCanonicalizesLegacyRoots` and
//!   `TestAuthFileRequestRetryFromJSON` call helpers upstream; here they go
//!   through the field route, or the list for the last. The core's field
//!   names leave `fingerprint-profile` as sent, so the field change keeps
//!   it, and the manager renames it to `fingerprint_profile` when it saves
//!   the credential.
//! - `TestPatchAuthFileFields_RejectsInvalidWeights` also checks the
//!   messages, and `TestRefreshAuthFiles_AllAndSpecific` the bodies. Its
//!   chunked body is a body sent without a `Content-Length`, which the
//!   handler reads alike.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use http::{Method, StatusCode};
use open_ferry_core::auth::synthesizer::StableIdGenerator;
use open_ferry_core::auth::{
    Auth, AuthError, AuthStore as _, ModelState, QuotaState, Status, Timestamp,
};
use open_ferry_core::config::{AuthFile, CodexKey, Config};
use open_ferry_core::exec::{ErrorKind, ExecError, Options, Request, Response, StreamResponse};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::manager::Manager;
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_providers::codex::CodexExecutor;
use open_ferry_providers::codex::oauth::Endpoints;
use serde_json::{Map, Value, json};
use tokio::sync::Notify;

use super::{
    Answer, Api, AuthDir, SyncCall, Upstream, auth, file_auth, http_response, keyed, object,
};
use crate::credential_state::auth_json;
use crate::json::Json;
use crate::{CredentialSync, SyncFuture};

const STATUS: &str = "/v0/management/auth-files/status";
const FIELDS: &str = "/v0/management/auth-files/fields";
const REFRESH: &str = "/v0/management/auth-files/refresh";

/// `PATCH path` with `body`, with the key.
async fn patch(api: &Api, path: &str, body: &str) -> Answer {
    api.send(keyed(Method::PATCH, path, body)).await
}

/// A Codex credential from file `name` in `dir`, written with `contents`,
/// which are also its metadata, as the watcher would register it.
fn codex_file(dir: &AuthDir, name: &str, contents: &str) -> Auth {
    let mut auth = file_auth(&dir.path(), name, name, contents);
    auth.metadata = serde_json::from_str(contents).unwrap();
    auth
}

/// The credentials the sync was handed, in order.
fn upserts(api: &Api) -> Vec<Auth> {
    api.sync
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            SyncCall::Upsert(auth) => Some(*auth),
            _ => None,
        })
        .collect()
}

/// The registered credential `id`.
fn current(api: &Api, id: &str) -> Arc<Auth> {
    api.manager
        .get(id)
        .unwrap_or_else(|| panic!("{id} isn't registered"))
}

/// A sync call that is done at once.
fn done<'a>() -> SyncFuture<'a> {
    Box::pin(std::future::ready(Ok(())))
}

// TestPatchAuthFileStatusInvokesPostAuthPersistHook
#[tokio::test]
async fn status_invokes_post_auth_persist_hook() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "codex-test.json";
    api.register(codex_file(
        &dir,
        name,
        r#"{"type":"codex","disabled":false}"#,
    ));

    let body = r#"{"name":"codex-test.json","disabled":true}"#;
    patch(&api, STATUS, body)
        .await
        .assert(StatusCode::OK, r#"{"disabled":true,"status":"ok"}"#);
    let calls = upserts(&api);
    assert_eq!(calls.len(), 1);
    assert!(calls[0].disabled);
    assert_eq!(calls[0].status, Status::Disabled);
    assert_eq!(calls[0].status_message, "disabled via management API");
    assert_eq!(dir.read_json(name)["disabled"], json!(true));

    let body = r#"{"name":"codex-test.json","disabled":false}"#;
    patch(&api, STATUS, body)
        .await
        .assert(StatusCode::OK, r#"{"disabled":false,"status":"ok"}"#);
    let calls = upserts(&api);
    assert_eq!(calls.len(), 2);
    assert!(!calls[1].disabled);
    assert_eq!(calls[1].status, Status::Active);
    assert_eq!(calls[1].status_message, "");
    assert_eq!(dir.read_json(name)["disabled"], json!(false));
}

/// A sync whose first call waits until it is released.
#[derive(Default)]
struct BlockingSync {
    calls: AtomicUsize,
    started: Notify,
    release: Notify,
}

impl CredentialSync for BlockingSync {
    fn upsert(&self, _: Auth) -> SyncFuture<'_> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok(())
        })
    }

    fn file_written(&self, _: AuthFile) -> SyncFuture<'_> {
        done()
    }

    fn file_removed(&self, _: std::path::PathBuf) -> SyncFuture<'_> {
        done()
    }
}

// TestPatchAuthFileStatusDoesNotHoldLockAcrossPersistHook
#[tokio::test]
async fn status_does_not_hold_lock_across_persist_hook() {
    let dir = AuthDir::new();
    let sync = Arc::new(BlockingSync::default());
    let api = Api::build(
        dir.config(),
        None,
        Some(Arc::clone(&dir.store) as _),
        |state, _| {
            state
                .with_store(Arc::clone(&dir.store))
                .with_sync(Arc::clone(&sync) as _)
        },
    );
    for name in ["codex-status-a.json", "codex-status-b.json"] {
        api.register(codex_file(
            &dir,
            name,
            r#"{"type":"codex","disabled":false}"#,
        ));
    }

    let first = patch(
        &api,
        STATUS,
        r#"{"name":"codex-status-a.json","disabled":true}"#,
    );
    tokio::pin!(first);
    tokio::select! {
        () = sync.started.notified() => {}
        answer = &mut first => panic!("first change finished before its sync: {answer:?}"),
    }

    let second = patch(
        &api,
        STATUS,
        r#"{"name":"codex-status-b.json","disabled":true}"#,
    );
    let second = tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .expect("second change blocked by the lock held across the sync");
    second.assert(StatusCode::OK, r#"{"disabled":true,"status":"ok"}"#);

    sync.release.notify_one();
    let first = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("first change didn't finish once its sync was released");
    first.assert(StatusCode::OK, r#"{"disabled":true,"status":"ok"}"#);
}

/// A sync that applies a credential as the service does, registering its
/// one model, or unregistering it while the credential is off.
#[derive(Default)]
struct ModelSync {
    target: OnceLock<(Manager, Arc<ModelRegistry>)>,
}

/// The model [`ModelSync`] registers.
fn astra() -> ModelInfo {
    ModelInfo {
        id: "gpt-6-astra".into(),
        display_name: "GPT-6 Astra".into(),
        ..ModelInfo::default()
    }
}

impl CredentialSync for ModelSync {
    fn upsert(&self, auth: Auth) -> SyncFuture<'_> {
        let (manager, registry) = self.target.get().expect("sync not wired");
        if auth.disabled {
            registry.unregister_client(&auth.id);
        } else {
            registry.register_client(&auth.id, &auth.provider, &[astra()]);
        }
        manager.update_unsaved(auth).unwrap();
        done()
    }

    fn file_written(&self, _: AuthFile) -> SyncFuture<'_> {
        done()
    }

    fn file_removed(&self, _: std::path::PathBuf) -> SyncFuture<'_> {
        done()
    }
}

// TestPatchAuthFileStatusRestoresModelsViaSyncHook
#[tokio::test]
async fn status_restores_models_via_sync_hook() {
    let dir = AuthDir::new();
    let sync = Arc::new(ModelSync::default());
    let api = Api::build(
        dir.config(),
        None,
        Some(Arc::clone(&dir.store) as _),
        |state, _| {
            state
                .with_store(Arc::clone(&dir.store))
                .with_sync(Arc::clone(&sync) as _)
        },
    );
    assert!(
        sync.target
            .set((api.manager.clone(), Arc::clone(&api.registry)))
            .is_ok()
    );
    let name = "codex-models.json";
    api.register(codex_file(
        &dir,
        name,
        r#"{"type":"codex","disabled":false}"#,
    ));
    api.registry.register_client(name, "codex", &[astra()]);

    let models = async || {
        let answer = api
            .get("/v0/management/auth-files/models?name=codex-models.json")
            .await;
        answer.expect(StatusCode::OK)["models"]
            .as_array()
            .map_or(0, Vec::len)
    };
    assert_eq!(models().await, 1);

    let body = r#"{"name":"codex-models.json","disabled":true}"#;
    patch(&api, STATUS, body).await.expect(StatusCode::OK);
    assert_eq!(models().await, 0);

    let body = r#"{"name":"codex-models.json","disabled":false}"#;
    patch(&api, STATUS, body).await.expect(StatusCode::OK);
    assert_eq!(models().await, 1);
}

// TestPatchAuthFileStatusHookErrorReturns500
#[tokio::test]
async fn status_hook_error_returns_503() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "codex-hook-err.json";
    api.register(codex_file(
        &dir,
        name,
        r#"{"type":"codex","disabled":false}"#,
    ));
    api.sync.stop();

    let body = r#"{"name":"codex-hook-err.json","disabled":true}"#;
    patch(&api, STATUS, body).await.assert(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"failed to synchronize auth runtime: credential sync unavailable: the service has stopped"}"#,
    );
    // As upstream, the change was saved before the hook failed.
    assert!(current(&api, name).disabled);
    assert_eq!(dir.read_json(name)["disabled"], json!(true));
}

// TestSyncAuthFilePriorityAttributeTracksFileSource
#[tokio::test]
async fn fields_priority_attribute_tracks_file_source() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "priority.json";
    let mut record = codex_file(&dir, name, r#"{"type":"codex"}"#);
    record
        .attributes
        .insert("source_backend".into(), "file".into());
    api.register(record);

    let body = r#"{"name":"priority.json","priority":1}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    let updated = current(&api, name);
    assert_eq!(updated.attribute("file_priority"), Some("true"));
    assert_eq!(updated.attribute("priority"), Some("1"));

    let body = r#"{"name":"priority.json","priority":null}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    let updated = current(&api, name);
    assert_eq!(updated.attribute("file_priority"), None);
    assert_eq!(updated.attribute("priority"), None);
}

/// A Claude credential from file `name` in `dir`, with extra `headers`.
fn claude_with_headers(dir: &AuthDir, name: &str, headers: &[(&str, &str)]) -> Auth {
    let header_map: Map<String, Value> = headers
        .iter()
        .map(|(key, value)| ((*key).to_owned(), json!(value)))
        .collect();
    let metadata = json!({"type": "claude", "headers": header_map});
    let mut record = file_auth(&dir.path(), name, name, &metadata.to_string());
    record.provider = "claude".into();
    record.metadata = object(&metadata).clone();
    for (key, value) in headers {
        record
            .attributes
            .insert(format!("header:{key}"), (*value).into());
    }
    record
}

// TestPatchAuthFileFields_MergeHeadersAndDeleteEmptyValues
#[tokio::test]
async fn fields_merge_headers_and_delete_empty_values() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "test.json";
    api.register(claude_with_headers(
        &dir,
        name,
        &[("X-Old", "old"), ("X-Remove", "gone")],
    ));

    let body = r#"{"name":"test.json","prefix":"p1","proxy_url":"http://proxy.local","headers":{"X-Old":"new","X-New":"v","X-Remove":"  ","X-Nope":""}}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);

    let updated = current(&api, name);
    assert_eq!(updated.prefix, "p1");
    assert_eq!(updated.proxy_url, "http://proxy.local");
    assert_eq!(updated.metadata["prefix"], json!("p1"));
    assert_eq!(updated.metadata["proxy_url"], json!("http://proxy.local"));
    assert_eq!(
        updated.metadata["headers"],
        json!({"X-New": "v", "X-Old": "new"})
    );
    assert_eq!(updated.attribute("header:X-Old"), Some("new"));
    assert_eq!(updated.attribute("header:X-New"), Some("v"));
    assert_eq!(updated.attribute("header:X-Remove"), None);
    assert_eq!(updated.attribute("header:X-Nope"), None);
}

// TestPatchAuthFileFields_HeadersEmptyMapIsNoop
#[tokio::test]
async fn fields_headers_empty_map_is_noop() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "noop.json";
    api.register(claude_with_headers(&dir, name, &[("X-Kee", "1")]));

    let body = r#"{"name":"noop.json","note":"hello","headers":{}}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);

    let updated = current(&api, name);
    assert_eq!(updated.attribute("header:X-Kee"), Some("1"));
    assert_eq!(updated.metadata["headers"], json!({"X-Kee": "1"}));
    assert_eq!(updated.attribute("note"), Some("hello"));
}

// TestPatchAuthFileFields_WebsocketsFalseIsUpdate
#[tokio::test]
async fn fields_websockets_false_is_update() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "codex.json";
    let mut record = codex_file(&dir, name, r#"{"type":"codex","websockets":true}"#);
    record.attributes.insert("websockets".into(), "true".into());
    api.register(record);

    let body = r#"{"name":"codex.json","websockets":false}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);

    let updated = current(&api, name);
    assert_eq!(updated.attribute("websockets"), Some("false"));
    assert_eq!(updated.metadata["websockets"], json!(false));
}

// TestPatchAuthFileFields_ArbitraryFieldsPersistToFile
#[tokio::test]
async fn fields_arbitrary_fields_persist_to_file() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "generic.json";
    api.register(codex_file(&dir, name, r#"{"type":"codex"}"#));

    let body = r#"{"name":"generic.json","abc":true,"nested.cde":true,"fgh":{"ijk":true}}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);

    let data = dir.read_json(name);
    assert_eq!(data["abc"], json!(true));
    assert_eq!(data["nested"], json!({"cde": true}));
    assert_eq!(data["fgh"], json!({"ijk": true}));
}

// TestPatchAuthFileFields_WeightPersistsAndSyncsRuntime
#[tokio::test]
async fn fields_weight_persists_and_syncs_runtime() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "weighted.json";
    api.register(codex_file(&dir, name, r#"{"type":"codex"}"#));

    let body = r#"{"name":"weighted.json","weight":7}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    assert_eq!(current(&api, name).attribute("weight"), Some("7"));
    assert_eq!(dir.read_json(name)["weight"], json!(7));

    let body = r#"{"name":"weighted.json","weight":null}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    assert_eq!(current(&api, name).attribute("weight"), None);
    assert!(object(&dir.read_json(name)).get("weight").is_none());
}

// TestPatchAuthFileFields_RejectsInvalidWeights
#[tokio::test]
async fn fields_rejects_invalid_weights() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    api.register(codex_file(&dir, "auth.json", r#"{"type":"codex"}"#));

    for (weight, message) in [
        (
            "1.5",
            r#"weight must be an integer: strconv.ParseInt: parsing "1.5": invalid syntax"#,
        ),
        ("1000001", "weight must not exceed 1000000"),
        (
            "9223372036854775808",
            r#"weight must be an integer: strconv.ParseInt: parsing "9223372036854775808": value out of range"#,
        ),
        (r#""7""#, "weight must be an integer"),
    ] {
        let body = format!(r#"{{"name":"auth.json","weight":{weight}}}"#);
        let answer = patch(&api, FIELDS, &body).await;
        assert_eq!(
            answer.expect(StatusCode::BAD_REQUEST)["error"],
            json!(message),
            "{weight}"
        );
    }
    assert!(upserts(&api).is_empty());
}

/// The `request_retry` the list shows for its one credential.
async fn listed_request_retry(api: &Api) -> Option<Value> {
    let files = api.files("").await;
    assert_eq!(files.len(), 1, "{files:?}");
    files[0].get("request_retry").cloned()
}

// TestPatchAuthFileFields_RequestRetryRoundTrip
#[tokio::test]
async fn fields_request_retry_round_trip() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "request-retry.json";
    api.register(codex_file(&dir, name, r#"{"type":"codex"}"#));
    let ok = async |body: &str| {
        patch(&api, FIELDS, body)
            .await
            .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    };

    ok(r#"{"name":"request-retry.json","request-retry":2}"#).await;
    let updated = current(&api, name);
    assert_eq!(updated.request_retry_override(), Some(2));
    assert!(!updated.metadata.contains_key("request-retry"));
    let persisted = dir.read_json(name);
    assert_eq!(persisted["request_retry"], json!(2));
    assert!(object(&persisted).get("request-retry").is_none());
    assert_eq!(listed_request_retry(&api).await, Some(json!(2)));

    ok(r#"{"name":"request-retry.json","request_retry":0}"#).await;
    assert_eq!(listed_request_retry(&api).await, Some(json!(0)));

    ok(r#"{"name":"request-retry.json","request-retry":-1}"#).await;
    assert_eq!(listed_request_retry(&api).await, None);

    ok(r#"{"name":"request-retry.json","request_retry":2}"#).await;
    ok(r#"{"name":"request-retry.json","request-retry":2,"request_retry":3}"#).await;
    assert_eq!(listed_request_retry(&api).await, Some(json!(3)));

    ok(r#"{"name":"request-retry.json","request_retry":2}"#).await;
    for (body, message) in [
        (
            r#"{"name":"request-retry.json","request-retry":"2"}"#,
            "request_retry must be an integer or null",
        ),
        (
            r#"{"name":"request-retry.json","request-retry":1.5}"#,
            "request_retry must be an integer or null",
        ),
        (
            r#"{"name":"request-retry.json","request_retry.child":2}"#,
            "request_retry does not support nested fields",
        ),
        (
            r#"{"name":"request-retry.json","request_retry .child":2}"#,
            "request_retry does not support nested fields",
        ),
        (
            r#"{"name":"request-retry.json","request-retry .child":2}"#,
            "request_retry does not support nested fields",
        ),
    ] {
        let answer = patch(&api, FIELDS, body).await;
        assert_eq!(
            answer.expect(StatusCode::BAD_REQUEST)["error"],
            json!(message),
            "{body}"
        );
        assert_eq!(listed_request_retry(&api).await, Some(json!(2)), "{body}");
    }

    ok(r#"{"name":"request-retry.json","request_retry":null}"#).await;
    assert_eq!(listed_request_retry(&api).await, None);
}

// TestAuthFileRequestRetryFromJSON
#[tokio::test]
async fn request_retry_from_json() {
    for (raw, want) in [
        (r#"{"request_retry":2}"#, Some(json!(2))),
        (r#"{"request-retry":2}"#, Some(json!(2))),
        (r#"{"request_retry":-1}"#, None),
        (r#"{"request_retry":"2"}"#, Some(json!(2))),
    ] {
        let api = Api::new();
        let mut record = auth("retry.json", &[("runtime_only", "true")]);
        record.metadata = serde_json::from_str(raw).unwrap();
        api.register(record);
        assert_eq!(listed_request_retry(&api).await, want, "{raw}");
    }
}

// TestNormalizeAuthFilePatchFieldsCanonicalizesLegacyRoots
#[tokio::test]
async fn fields_canonicalize_legacy_roots() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "legacy.json";
    api.register(codex_file(&dir, name, r#"{"type":"codex"}"#));

    let body = r#"{"name":"legacy.json","request-retry":2," disable-cooling ":true,"fingerprint-profile.value":"x","provider-specific":"preserved"}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    for metadata in [
        current(&api, name).metadata.clone(),
        object(&dir.read_json(name)).clone(),
    ] {
        assert_eq!(metadata["request_retry"], json!(2));
        assert_eq!(metadata["disable_cooling"], json!(true));
        assert_eq!(metadata["fingerprint_profile"], json!({"value": "x"}));
        assert_eq!(metadata["provider-specific"], json!("preserved"));
        for legacy in [
            "request-retry",
            "disable-cooling",
            " disable-cooling ",
            "fingerprint-profile",
        ] {
            assert!(!metadata.contains_key(legacy), "{legacy}");
        }
    }

    let body = r#"{"name":"legacy.json","request-retry":2,"request_retry":3}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    assert_eq!(current(&api, name).metadata["request_retry"], json!(3));

    let body =
        r#"{"name":"legacy.json","disable_cooling.value":true,"disable_cooling . value":false}"#;
    patch(&api, FIELDS, body).await.assert(
        StatusCode::BAD_REQUEST,
        r#"{"error":"auth file fields \"disable_cooling.value\" and \"disable_cooling . value\" refer to the same field"}"#,
    );
}

/// Upstream's Codex ID token with plan `team`.
const TEAM_ID_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ICJ1c2VyQGV4YW1wbGUuY29tIiwgImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6IHsiY2hhdGdwdF9wbGFuX3R5cGUiOiAidGVhbSIsICJjaGF0Z3B0X2FjY291bnRfaWQiOiAiYWNjLTEyMyJ9fQ.sig";

// TestPatchAuthFileFields_SyncsPlanTypeAndInvokesHook
#[tokio::test]
async fn fields_sync_plan_type_and_invoke_hook() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "codex-plan-patch.json";
    let mut record = codex_file(&dir, name, r#"{"type":"codex"}"#);
    record.attributes.insert("plan_type".into(), "pro".into());
    api.register(record);

    let body = json!({"name": name, "id_token": TEAM_ID_TOKEN}).to_string();
    patch(&api, FIELDS, &body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);

    assert_eq!(current(&api, name).attribute("plan_type"), Some("team"));
    let calls = upserts(&api);
    assert_eq!(calls.len(), 1, "the hook wasn't called once");
    assert_eq!(calls[0].attribute("plan_type"), Some("team"));
}

// TestPatchAuthFileFields_ClearsPlanTypeWhenRemoved
#[tokio::test]
async fn fields_clear_plan_type_when_removed() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "codex-clear-plan.json";
    let mut record = codex_file(&dir, name, r#"{"type":"codex","plan_type":"free"}"#);
    record.attributes.insert("plan_type".into(), "free".into());
    api.register(record);

    let body = r#"{"name":"codex-clear-plan.json","plan_type":null}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    assert_eq!(current(&api, name).attribute("plan_type"), None);
}

// TestPatchAuthFileFields_IDTokenMissingPlanTypeDefaultsToFree
#[tokio::test]
async fn fields_id_token_missing_plan_type_defaults_to_free() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "codex-test-patch-idtoken.json";
    api.register(codex_file(&dir, name, r#"{"type":"codex"}"#));

    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let claims = json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": {"chatgpt_account_id": "acc-12345"},
    });
    let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
    let id_token = format!("{header}.{claims}.");

    let body = json!({"name": name, "id_token": id_token}).to_string();
    patch(&api, FIELDS, &body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    assert_eq!(current(&api, name).attribute("plan_type"), Some("free"));
}

type ExecFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ExecError>> + Send + 'a>>;

/// An executor that counts its refreshes and gives each credential new
/// tokens, except `failing.json`, whose refresh fails with `boom`.
struct RefreshRecordExecutor {
    provider: &'static str,
    refreshes: AtomicUsize,
}

impl RefreshRecordExecutor {
    fn new(provider: &'static str) -> Arc<Self> {
        Arc::new(Self {
            provider,
            refreshes: AtomicUsize::new(0),
        })
    }

    fn refreshes(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }
}

/// The answer to a call the tests never make.
fn unused<'a, T: Send + 'a>() -> ExecFuture<'a, T> {
    Box::pin(std::future::ready(Err(ExecError::upstream(
        501, "not used",
    ))))
}

impl ProviderExecutor for RefreshRecordExecutor {
    fn id(&self) -> &str {
        self.provider
    }

    fn execute(&self, _: Arc<Auth>, _: Request, _: Options) -> ExecFuture<'_, Response> {
        unused()
    }

    fn execute_stream(
        &self,
        _: Arc<Auth>,
        _: Request,
        _: Options,
    ) -> ExecFuture<'_, StreamResponse> {
        unused()
    }

    fn count_tokens(&self, _: Arc<Auth>, _: Request, _: Options) -> ExecFuture<'_, Response> {
        unused()
    }

    fn refresh(&self, auth: Arc<Auth>) -> ExecFuture<'_, Auth> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        if auth.id == "failing.json" {
            return Box::pin(std::future::ready(Err(ExecError::new(
                ErrorKind::Upstream,
                "boom",
            ))));
        }
        let mut auth = Auth::clone(&auth);
        let metadata = &mut auth.metadata;
        metadata.insert("access_token".into(), json!("refreshed-token"));
        metadata.insert("refresh_token".into(), json!("refresh-token"));
        metadata.insert("expires_in".into(), json!(3600));
        let expired = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        metadata.insert("expired".into(), json!(expired));
        Box::pin(std::future::ready(Ok(auth)))
    }
}

/// An active credential of `provider` with tokens.
fn refreshable(id: &str, provider: &str, refresh_token: &str, access_token: &str) -> Auth {
    let mut record = auth(id, &[]);
    record.provider = provider.into();
    record.status = Status::Active;
    let metadata = json!({
        "type": provider,
        "refresh_token": refresh_token,
        "access_token": access_token,
    });
    record.metadata = object(&metadata).clone();
    record
}

// TestRefreshAuthFiles_AllAndSpecific
#[tokio::test]
async fn refresh_all_and_specific() {
    let api = Api::new();
    let executor = RefreshRecordExecutor::new("antigravity");
    api.manager.register_executor(Arc::clone(&executor) as _);
    api.register(refreshable(
        "antigravity-1.json",
        "antigravity",
        "ref-1",
        "old-1",
    ));
    let mut second = refreshable("antigravity-2.json", "antigravity", "ref-2", "old-2");
    second.status = Status::Error;
    second.unavailable = true;
    second.last_error = Some(AuthError {
        message: "unauthorized".into(),
        ..AuthError::default()
    });
    api.register(second);

    // 1. Refresh all.
    api.post(&format!("{REFRESH}?all=true"), "").await.assert(
        StatusCode::OK,
        r#"{"ok":true,"results":[{"id":"antigravity-1.json","success":true},{"id":"antigravity-2.json","success":true}]}"#,
    );
    assert!(executor.refreshes() >= 2, "{}", executor.refreshes());

    // 2. The credential in error has recovered.
    assert_ne!(current(&api, "antigravity-2.json").status, Status::Error);

    // 3. One credential by name.
    let before = executor.refreshes();
    let answer = api
        .post(&format!("{REFRESH}?name=antigravity-1.json"), "")
        .await;
    let body = answer.expect(StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["auth"]["id"], json!("antigravity-1.json"));
    assert_eq!(
        body["auth"]["metadata"]["access_token"],
        json!("refreshed-token")
    );
    assert_eq!(executor.refreshes(), before + 1);

    // 4. A credential that doesn't exist.
    api.post(&format!("{REFRESH}?name=nonexistent.json"), "")
        .await
        .assert(StatusCode::NOT_FOUND, r#"{"error":"auth file not found"}"#);

    // 5. A JSON body without a Content-Length, as a chunked one comes.
    let request = keyed(Method::POST, REFRESH, r#"{"name":"antigravity-1.json"}"#);
    assert!(
        request
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .is_none()
    );
    api.send(request).await.expect(StatusCode::OK);
    assert_eq!(executor.refreshes(), before + 2);

    // 6. A malformed body.
    api.post(REFRESH, "{invalid").await.assert(
        StatusCode::BAD_REQUEST,
        r#"{"error":"invalid request body"}"#,
    );
}

/// A config API key's credential: the first Codex key of [`config_key`].
fn config_key_auth(api: &Api) -> String {
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
    id
}

/// The config of `dir`, with a Codex key excluding `gpt-5`.
fn config_key(dir: &AuthDir) -> Config {
    let mut config = dir.config();
    config.codex_api_key = vec![CodexKey {
        api_key: "sk-test".into(),
        base_url: "https://example.com/v1".into(),
        excluded_models: vec!["gpt-5".into()],
        ..CodexKey::default()
    }];
    config
}

// Not upstream's: turning a credential from a config API key off adds `*`
// to the key's `excluded-models`, and on removes it, saved as any config
// change is (in the v8 layout from the v8 route); the credential itself is
// left for the reload to change, and the service isn't synced.
#[tokio::test]
async fn status_toggles_config_api_key() {
    let dir = AuthDir::new();
    let api = Api::over_with(&dir, config_key(&dir), None).with_writer();
    let id = config_key_auth(&api);
    let before = current(&api, &id);

    for (path, migrate_v8) in [(STATUS, false), ("/v8/management/credentials/status", true)] {
        for (disabled, excluded) in [(true, &["gpt-5", "*"][..]), (false, &["gpt-5"][..])] {
            let body = format!(r#"{{"name":"{id}","disabled":{disabled}}}"#);
            let want = format!(
                r#"{{"disabled":{disabled},"excluded_pattern":"*","status":"ok","via":"config:excluded-models"}}"#
            );
            patch(&api, path, &body).await.assert(StatusCode::OK, &want);
            assert_eq!(api.saved().codex_api_key[0].excluded_models, excluded);
            let saves = api.writer.saved();
            assert_eq!(saves.last().map(|(_, migrate)| *migrate), Some(migrate_v8));
        }
    }
    assert_eq!(api.writer.saved().len(), 4);
    assert!(api.writer.lock_held().iter().all(|&held| held));
    assert_eq!(api.reload.count(), 4);

    let after = current(&api, &id);
    assert!(!after.disabled);
    assert_eq!(after.status, before.status);
    assert_eq!(after.updated_at, before.updated_at);
    assert!(api.sync.calls().is_empty());
    assert!(!dir.config_path().exists());
}

// Not upstream's: a config API key the config no longer has, a save the
// writer refuses, and no writer.
#[tokio::test]
async fn status_config_api_key_failures() {
    let dir = AuthDir::new();
    let api = Api::over_with(&dir, config_key(&dir), None).with_writer();
    let id = config_key_auth(&api);
    api.register(auth(
        "codex:apikey:gone",
        &[("api_key", "sk-gone"), ("source", "config:codex[abc]")],
    ));
    patch(
        &api,
        STATUS,
        r#"{"name":"codex:apikey:gone","disabled":true}"#,
    )
    .await
    .assert(
        StatusCode::NOT_FOUND,
        r#"{"error":"config api key entry not found"}"#,
    );
    assert!(api.writer.written().is_empty());

    api.writer.fail("disk full");
    let body = format!(r#"{{"name":"{id}","disabled":true}}"#);
    patch(&api, STATUS, &body).await.assert(
        StatusCode::INTERNAL_SERVER_ERROR,
        r#"{"error":"failed to save config: disk full"}"#,
    );
    assert_eq!(api.writer.saved().len(), 1);
    assert!(*api.state.config() == config_key(&dir));
    assert_eq!(api.reload.count(), 0);

    let api = Api::over_with(&dir, config_key(&dir), None);
    let id = config_key_auth(&api);
    let body = format!(r#"{{"name":"{id}","disabled":true}}"#);
    patch(&api, STATUS, &body).await.assert(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"config writer unavailable"}"#,
    );
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: the v8 route, `auth_index` and the validation answers.
#[tokio::test]
async fn status_v8_route_and_validation() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "codex-v8.json";
    let index = api.register(codex_file(&dir, name, r#"{"type":"codex"}"#));

    let body = json!({"name": format!(" {name} "), "auth_index": index, "disabled": true});
    patch(&api, "/v8/management/credentials/status", &body.to_string())
        .await
        .assert(StatusCode::OK, r#"{"disabled":true,"status":"ok"}"#);
    assert!(current(&api, name).disabled);

    for (body, status, error) in [
        ("", StatusCode::BAD_REQUEST, "invalid request body"),
        ("{", StatusCode::BAD_REQUEST, "invalid request body"),
        ("[]", StatusCode::BAD_REQUEST, "invalid request body"),
        (
            r#"{"name":"codex-v8.json","disabled":"true"}"#,
            StatusCode::BAD_REQUEST,
            "invalid request body",
        ),
        ("null", StatusCode::BAD_REQUEST, "name is required"),
        (
            r#"{"name":"  ","disabled":true}"#,
            StatusCode::BAD_REQUEST,
            "name is required",
        ),
        (
            r#"{"name":"codex-v8.json"}"#,
            StatusCode::BAD_REQUEST,
            "disabled is required",
        ),
        (
            r#"{"name":"codex-v8.json","disabled":null}"#,
            StatusCode::BAD_REQUEST,
            "disabled is required",
        ),
        (
            r#"{"name":"missing.json","disabled":true}"#,
            StatusCode::NOT_FOUND,
            "auth file not found",
        ),
        (
            r#"{"name":"codex-v8.json","auth_index":"other","disabled":false}"#,
            StatusCode::NOT_FOUND,
            "auth file not found",
        ),
    ] {
        let answer = patch(&api, STATUS, body).await;
        assert_eq!(answer.expect(status)["error"], json!(error), "{body}");
    }
    assert!(current(&api, name).disabled);
    assert_eq!(upserts(&api).len(), 1);
}

// Not upstream's: without a credential store, status and field changes
// answer 503; a refresh needs none.
#[tokio::test]
async fn changes_without_store_answer_503() {
    let api = Api::new();
    api.register(auth("plain", &[]));
    for (path, body) in [
        (STATUS, r#"{"name":"plain","disabled":true}"#),
        (FIELDS, r#"{"name":"plain","note":"x"}"#),
        (
            "/v8/management/credentials/fields",
            r#"{"name":"plain","note":"x"}"#,
        ),
    ] {
        patch(&api, path, body).await.assert(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":"credential store unavailable"}"#,
        );
    }
    assert!(!current(&api, "plain").disabled);
    assert_eq!(current(&api, "plain").attribute("note"), None);
    api.post(REFRESH, "").await.assert(
        StatusCode::BAD_REQUEST,
        r#"{"error":"name or all=true is required"}"#,
    );
}

// Not upstream's: the settings read from patched fields, and the v8 route.
#[tokio::test]
async fn fields_sync_settings_from_strings() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "settings.json";
    api.register(codex_file(&dir, name, r#"{"type":"codex"}"#));

    let body = r#"{"name":"settings.json","disabled":"true","priority":" 5 ","note":"  hi ","websockets":"1","prefix":" team ","proxy_url":" http://p ","plan_type":" plus "}"#;
    patch(&api, "/v8/management/credentials/fields", body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    let updated = current(&api, name);
    assert!(updated.disabled);
    assert_eq!(updated.status, Status::Disabled);
    assert_eq!(updated.status_message, "disabled via management API");
    assert_eq!(updated.attribute("priority"), Some("5"));
    assert_eq!(updated.attribute("file_priority"), None);
    assert_eq!(updated.attribute("note"), Some("hi"));
    assert_eq!(updated.attribute("websockets"), Some("true"));
    assert_eq!(updated.attribute("plan_type"), Some("plus"));
    assert_eq!(updated.prefix, "team");
    assert_eq!(updated.proxy_url, "http://p");
    assert_eq!(dir.read_json(name)["note"], json!("  hi "));

    let body = r#"{"name":"settings.json","disabled":false,"priority":"x","note":"","websockets":2,"prefix":1}"#;
    patch(&api, FIELDS, body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    let updated = current(&api, name);
    assert!(!updated.disabled);
    assert_eq!(updated.status, Status::Active);
    assert_eq!(updated.status_message, "");
    assert_eq!(updated.attribute("priority"), None);
    assert_eq!(updated.attribute("note"), None);
    assert_eq!(updated.attribute("websockets"), None);
    // A prefix that isn't a string leaves the setting as it was.
    assert_eq!(updated.prefix, "team");
}

// Not upstream's: the field change's error answers.
#[tokio::test]
async fn fields_errors() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    api.register(codex_file(&dir, "errors.json", r#"{"type":"codex"}"#));

    for (body, status, error) in [
        ("", StatusCode::BAD_REQUEST, "invalid request body"),
        ("[]", StatusCode::BAD_REQUEST, "invalid request body"),
        (
            r#"{"name":"errors.json""#,
            StatusCode::BAD_REQUEST,
            "invalid request body",
        ),
        ("null", StatusCode::BAD_REQUEST, "name is required"),
        ("{}", StatusCode::BAD_REQUEST, "name is required"),
        (
            r#"{"name":1,"a":1}"#,
            StatusCode::BAD_REQUEST,
            "name is required",
        ),
        (
            r#"{"name":" ","a":1}"#,
            StatusCode::BAD_REQUEST,
            "name is required",
        ),
        (
            r#"{"name":"missing.json","a":1}"#,
            StatusCode::NOT_FOUND,
            "auth file not found",
        ),
        (
            r#"{"name":"errors.json"}"#,
            StatusCode::BAD_REQUEST,
            "no fields to update",
        ),
        (
            r#"{"name":"errors.json","":1}"#,
            StatusCode::BAD_REQUEST,
            "field name is required",
        ),
        (
            r#"{"name":"errors.json","a..b":1}"#,
            StatusCode::BAD_REQUEST,
            "invalid field path: a..b",
        ),
        (
            r#"{"name":"errors.json","weight.x":1}"#,
            StatusCode::BAD_REQUEST,
            "weight does not support nested fields",
        ),
        (
            r#"{"name":"errors.json","disable-cooling":true," disable-cooling":false}"#,
            StatusCode::BAD_REQUEST,
            r#"auth file fields "disable-cooling" and " disable-cooling" refer to the same field"#,
        ),
    ] {
        let answer = patch(&api, FIELDS, body).await;
        assert_eq!(answer.expect(status)["error"], json!(error), "{body}");
    }
    assert!(upserts(&api).is_empty());

    api.sync.stop();
    patch(&api, FIELDS, r#"{"name":"errors.json","note":"x"}"#)
        .await
        .assert(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":"post-auth persist hook failed: credential sync unavailable: the service has stopped"}"#,
        );
}

// Not upstream's: a field can't nest the credential's file deeper than the
// store reads back, 127, however many parts its dotted name has. Upstream
// builds whatever the name asks for, and a name of 5000 parts overflowed
// the stack here.
#[tokio::test]
async fn fields_refuse_nesting_the_file_too_deep() {
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let name = "deep.json";
    api.register(codex_file(&dir, name, r#"{"type":"codex"}"#));
    let before = dir.read_json(name);
    let dotted = |root: &str, parts: usize| vec![root; parts].join(".");
    let nested = |depth: usize| format!("{}true{}", "[".repeat(depth), "]".repeat(depth));

    for (key, value) in [
        (dotted("a", 5000), nested(0)),
        (dotted("a", 128), nested(0)),
        (dotted("a", 100), nested(28)),
        (dotted("a", 1), nested(127)),
    ] {
        let body = format!(r#"{{"name":"deep.json","{key}":{value}}}"#);
        patch(&api, FIELDS, &body).await.assert(
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid request body"}"#,
        );
    }
    assert!(upserts(&api).is_empty());
    assert_eq!(dir.read_json(name), before);

    // Right at the limit, the file is saved and the store reads it back.
    let body = format!(
        r#"{{"name":"deep.json","{}":{},"{}":{}}}"#,
        dotted("b", 127),
        nested(0),
        dotted("c", 100),
        nested(27),
    );
    patch(&api, FIELDS, &body)
        .await
        .assert(StatusCode::OK, r#"{"status":"ok"}"#);
    let data = dir.read_json(name);
    let pointer = |root: &str, parts: usize| format!("/{}", vec![root; parts].join("/"));
    assert_eq!(data.pointer(&pointer("b", 127)), Some(&json!(true)));
    let mut value = data.pointer(&pointer("c", 100)).unwrap();
    for _ in 0..27 {
        value = &value[0];
    }
    assert_eq!(value, &json!(true));
    let listed = dir.store.list().unwrap();
    assert!(listed.iter().any(|auth| auth.id == name), "{listed:?}");
}

// Not upstream's: a Codex refresh against a token endpoint on 127.0.0.1;
// the answer holds the new tokens, as upstream's does.
#[tokio::test]
async fn refresh_codex_answers_with_tokens() {
    let tokens = json!({
        "access_token": "new-access",
        "refresh_token": "new-refresh",
        "id_token": TEAM_ID_TOKEN,
        "token_type": "Bearer",
        "expires_in": 3600,
    })
    .to_string();
    let upstream = Upstream::answering(http_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        tokens.as_bytes(),
    ))
    .await;
    let api = Api::new();
    let executor =
        CodexExecutor::new("direct").with_oauth_endpoints(Endpoints::with_base(&upstream.url));
    api.manager.register_executor(Arc::new(executor));
    api.register(refreshable(
        "codex-user.json",
        "codex",
        "old-refresh",
        "old-access",
    ));

    let answer = api
        .post(
            "/v8/management/credentials/refresh",
            r#"{"name":"codex-user.json"}"#,
        )
        .await;
    let body = answer.expect(StatusCode::OK);
    let refreshed = &body["auth"];
    assert_eq!(body["ok"], json!(true));
    assert_eq!(refreshed["id"], json!("codex-user.json"));
    assert_eq!(refreshed["provider"], json!("codex"));
    assert_eq!(refreshed["status"], json!("active"));
    assert_eq!(refreshed["metadata"]["access_token"], json!("new-access"));
    assert_eq!(refreshed["metadata"]["refresh_token"], json!("new-refresh"));
    assert_eq!(refreshed["metadata"]["id_token"], json!(TEAM_ID_TOKEN));
    assert_eq!(refreshed["attributes"]["plan_type"], json!("team"));
    assert_eq!(
        current(&api, "codex-user.json").metadata_str("access_token"),
        Some("new-access")
    );

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].starts_with("POST /oauth/token "),
        "{}",
        requests[0]
    );
    assert!(requests[0].contains("refresh_token=old-refresh"));
}

// Not upstream's: the outcome of each credential in a refresh of all, a
// failed refresh, and the other ways to name a credential.
#[tokio::test]
async fn refresh_results_and_names() {
    let api = Api::new();
    api.post(&format!("{REFRESH}?all=true"), "")
        .await
        .assert(StatusCode::OK, r#"{"ok":true,"results":[]}"#);

    let executor = RefreshRecordExecutor::new("antigravity");
    api.manager.register_executor(Arc::clone(&executor) as _);
    api.register(refreshable("a.json", "antigravity", "ref-a", "old-a"));
    let mut disabled = refreshable("disabled.json", "antigravity", "ref-d", "old-d");
    disabled.disabled = true;
    disabled.status = Status::Disabled;
    api.register(disabled);
    api.register(refreshable("failing.json", "antigravity", "ref-f", "old-f"));
    let mut by_file = refreshable("id-x", "antigravity", "ref-x", "old-x");
    by_file.file_name = "x.json".into();
    let index = api.register(by_file);

    api.post(REFRESH, r#"{"all":true}"#).await.assert(
        StatusCode::OK,
        r#"{"ok":true,"results":[{"id":"a.json","success":true},{"id":"failing.json","success":false,"error":"boom"},{"id":"id-x","success":true}]}"#,
    );
    assert_eq!(executor.refreshes(), 3);

    api.post(REFRESH, r#"{"name":"failing.json"}"#)
        .await
        .assert(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#);

    for (path, body) in [
        (REFRESH.to_owned(), r#"{"name":" x.json "}"#.to_owned()),
        (
            format!("{REFRESH}?name=x.json&auth_index={index}"),
            "  \n".to_owned(),
        ),
        (
            "/v8/management/credentials/refresh".to_owned(),
            json!({"name": "id-x", "auth_index": index, "all": null}).to_string(),
        ),
    ] {
        let answer = api.post(&path, &body).await;
        assert_eq!(
            answer.expect(StatusCode::OK)["auth"]["id"],
            json!("id-x"),
            "{path} {body}"
        );
    }

    for (path, body, status, error) in [
        (
            REFRESH.to_owned(),
            "null",
            StatusCode::BAD_REQUEST,
            "name or all=true is required",
        ),
        (
            REFRESH.to_owned(),
            "[]",
            StatusCode::BAD_REQUEST,
            "invalid request body",
        ),
        (
            format!("{REFRESH}?all=1"),
            "",
            StatusCode::BAD_REQUEST,
            "name or all=true is required",
        ),
        (
            format!("{REFRESH}?name=x.json&auth_index=other"),
            "",
            StatusCode::NOT_FOUND,
            "auth file not found",
        ),
    ] {
        let answer = api.post(&path, body).await;
        assert_eq!(
            answer.expect(status)["error"],
            json!(error),
            "{path} {body}"
        );
    }
}

/// The time `text` names.
fn at(text: &str) -> Timestamp {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

/// `text` with each `~u` made the start of a JSON `\u` escape.
fn escaped(text: &str) -> String {
    text.replace("~u", "\\u")
}

// Not upstream's: a credential written as upstream writes its `Auth` in
// `gin.H{"auth": auth, "ok": true}`, checked against the bytes Go wrote for
// the same credential.
#[test]
fn auth_json_matches_go() {
    let mut full = auth(
        "codex-user.json",
        &[
            ("path", "/tmp/codex-user.json"),
            ("plan_type", "pro"),
            ("header:X-A", "<b>&"),
        ],
    );
    full.registration_epoch = 3;
    full.credential_version = 2;
    full.generation = 7;
    full.prefix = "team-a".into();
    full.label = "user@example.com".into();
    full.status = Status::Error;
    full.status_message = "unauthorized".into();
    full.unavailable = true;
    full.proxy_url = "http://proxy.local:8080".into();
    full.metadata = serde_json::from_str(
        r#"{"type":"codex","access_token":"at<1>","expires_in":3600,"big":1e21,"nested":{"z":1.5,"a":[true,null,"x&y"]},"email":"user@example.com"}"#,
    )
    .unwrap();
    full.quota = QuotaState {
        exceeded: true,
        reason: "quota".into(),
        next_recover_at: Some(at("2026-01-02T03:04:05.5Z")),
        backoff_level: 2,
        ..QuotaState::default()
    };
    full.last_error = Some(AuthError {
        message: "unauthorized".into(),
        http_status: 401,
        ..AuthError::default()
    });
    full.created_at = Some(at("2026-01-01T00:00:00Z"));
    full.updated_at = Some(at("2026-01-01T00:00:00.123456789Z"));
    full.next_retry_after = Some(at("2026-01-03T00:00:00Z"));
    full.model_states.insert(
        "o3".into(),
        ModelState {
            status: Status::Active,
            ..ModelState::default()
        },
    );
    full.model_states.insert(
        "gpt-5".into(),
        ModelState {
            status: Status::Error,
            status_message: "rate limited".into(),
            unavailable: true,
            next_retry_after: Some(at("2026-01-02T00:00:00Z")),
            last_error: Some(AuthError {
                code: "rate_limit".into(),
                message: "slow down".into(),
                retryable: true,
                http_status: 429,
            }),
            quota: QuotaState {
                exceeded: true,
                reason: "rate".into(),
                next_recover_at: Some(at("2026-01-02T00:00:00Z")),
                backoff_level: 1,
                observed_at: Some(at("2026-01-01T11:59:00.25Z")),
                signals: [("X-Codex-Plan-Type", "<pro>"), ("Retry-After", "30")]
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect(),
            },
            updated_at: Some(at("2026-01-01T12:00:00Z")),
        },
    );
    let bare = Auth {
        id: "bare".into(),
        status: Status::Active,
        ..Auth::default()
    };

    let answer =
        |auth: &Auth| Json::map([("auth", auth_json(auth)), ("ok", Json::Bool(true))]).encode();
    assert_eq!(
        answer(&full),
        escaped(concat!(
            r#"{"auth":{"id":"codex-user.json","registration_epoch":3,"credential_version":2,"generation":7,"#,
            r#""provider":"codex","prefix":"team-a","label":"user@example.com","#,
            r#""status":"error","status_message":"unauthorized","disabled":false,"unavailable":true,"#,
            r#""proxy_url":"http://proxy.local:8080","#,
            r#""attributes":{"header:X-A":"~u003cb~u003e~u0026","path":"/tmp/codex-user.json","plan_type":"pro"},"#,
            r#""metadata":{"access_token":"at~u003c1~u003e","big":1e+21,"email":"user@example.com","expires_in":3600,"#,
            r#""nested":{"a":[true,null,"x~u0026y"],"z":1.5},"type":"codex"},"#,
            r#""quota":{"exceeded":true,"reason":"quota","next_recover_at":"2026-01-02T03:04:05.5Z","backoff_level":2,"#,
            r#""observed_at":"0001-01-01T00:00:00Z"},"#,
            r#""last_error":{"message":"unauthorized","retryable":false,"http_status":401},"#,
            r#""created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00.123456789Z","#,
            r#""last_refreshed_at":"0001-01-01T00:00:00Z","next_refresh_after":"0001-01-01T00:00:00Z","#,
            r#""next_retry_after":"2026-01-03T00:00:00Z","#,
            r#""model_states":{"gpt-5":{"status":"error","status_message":"rate limited","unavailable":true,"#,
            r#""next_retry_after":"2026-01-02T00:00:00Z","#,
            r#""last_error":{"code":"rate_limit","message":"slow down","retryable":true,"http_status":429},"#,
            r#""quota":{"exceeded":true,"reason":"rate","next_recover_at":"2026-01-02T00:00:00Z","backoff_level":1,"#,
            r#""observed_at":"2026-01-01T11:59:00.25Z","#,
            r#""signals":{"Retry-After":"30","X-Codex-Plan-Type":"~u003cpro~u003e"}},"#,
            r#""updated_at":"2026-01-01T12:00:00Z"},"#,
            r#""o3":{"status":"active","unavailable":false,"next_retry_after":"0001-01-01T00:00:00Z","#,
            r#""quota":{"exceeded":false,"next_recover_at":"0001-01-01T00:00:00Z","observed_at":"0001-01-01T00:00:00Z"},"#,
            r#""updated_at":"0001-01-01T00:00:00Z"}}},"ok":true}"#,
        ))
    );
    assert_eq!(
        answer(&bare),
        concat!(
            r#"{"auth":{"id":"bare","provider":"","status":"active","disabled":false,"unavailable":false,"#,
            r#""quota":{"exceeded":false,"next_recover_at":"0001-01-01T00:00:00Z","observed_at":"0001-01-01T00:00:00Z"},"#,
            r#""created_at":"0001-01-01T00:00:00Z","updated_at":"0001-01-01T00:00:00Z","#,
            r#""last_refreshed_at":"0001-01-01T00:00:00Z","next_refresh_after":"0001-01-01T00:00:00Z","#,
            r#""next_retry_after":"0001-01-01T00:00:00Z"},"ok":true}"#,
        )
    );
}
