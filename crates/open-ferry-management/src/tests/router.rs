//! The router as a whole: unported paths, access control over the network
//! and config reloads, the model list, and how the route registry applies
//! each route's access. No upstream test covers these; the expected
//! answers follow upstream's code.

use std::sync::Arc;

use axum::extract::{Multipart, Path};
use axum::response::IntoResponse as _;
use axum::routing::{get, post};
use http::{Method, StatusCode, header};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::models::ModelInfo;

use super::{Answer, Api, KEY, LOCAL, keyed, keyed_config, request_from};
use crate::{Route, router_from};

const LIST: &str = "/v0/management/auth-files";
const REMOTE: &str = "203.0.113.7:4000";

/// `GET` the credential list from `peer` with `key` as a bearer token.
async fn list_from(api: &Api, peer: &str, key: &str) -> Answer {
    let mut request = request_from(peer, Method::GET, LIST, "");
    let value = format!("Bearer {key}").parse().unwrap();
    request.headers_mut().insert(header::AUTHORIZATION, value);
    api.send(request).await
}

fn error(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

#[tokio::test]
async fn unported_paths_answer_an_empty_404() {
    let api = Api::new();
    for (method, path) in [
        (Method::PUT, "/v0/management/usage-statistics-enabled"),
        (Method::GET, "/v0/management/quota/providers"),
        (Method::GET, "/v0/management"),
        (Method::GET, "/v0/management/"),
        (Method::GET, "/v8/management"),
        (Method::PUT, "/v0/management/config.yaml"),
        (Method::GET, "/v8/management/plugins"),
        (Method::GET, "/v0/management/xai-auth-url"),
        (Method::GET, "/v0/management/api-call"),
        (Method::PUT, "/v8/management/credentials"),
        (Method::GET, "/v8/management/routing/cooldown/reset"),
        (Method::HEAD, "/v0/management/auth-files"),
        (Method::HEAD, "/v8/management/credentials/models"),
        (Method::GET, "/v0/management/auth-files/"),
    ] {
        for request in [
            keyed(method.clone(), path, ""),
            request_from(REMOTE, method.clone(), path, ""),
        ] {
            let answer = api.send(request).await;
            assert_eq!(
                (answer.status, answer.body.as_str()),
                (StatusCode::NOT_FOUND, ""),
                "{method} {path}"
            );
            assert_eq!(answer.header("x-cpa-version"), None, "{method} {path}");
        }
    }
}

#[tokio::test]
async fn availability_follows_config_reloads() {
    let api = Api::with(Default::default(), None);
    let answer = api.get(LIST).await;
    answer.assert(StatusCode::NOT_FOUND, "");
    assert_eq!(answer.header("x-cpa-version"), None);

    api.state.set_config(Arc::new(keyed_config()));
    let answer = api.get(LIST).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.header("x-cpa-version").is_some());

    let mut changed = keyed_config();
    changed.remote_management.secret_key = "another-secret".into();
    api.state.set_config(Arc::new(changed));
    let answer = api.get(LIST).await;
    answer.assert(StatusCode::UNAUTHORIZED, &error("invalid management key"));

    api.state.set_config(Arc::new(Default::default()));
    api.get(LIST).await.assert(StatusCode::NOT_FOUND, "");
}

#[tokio::test]
async fn management_password_enables_the_api_and_remote_access() {
    let api = Api::with(Default::default(), Some(" env-pass "));
    let answer = list_from(&api, REMOTE, "env-pass").await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    list_from(&api, LOCAL, "test-secret")
        .await
        .assert(StatusCode::UNAUTHORIZED, &error("invalid management key"));

    // Both keys work when both are set; remote access needs only the
    // password to be set.
    let api = Api::with(keyed_config(), Some("env-pass"));
    for key in [KEY, "env-pass"] {
        let answer = list_from(&api, REMOTE, key).await;
        assert_eq!(answer.status, StatusCode::OK, "{key}: {}", answer.body);
    }

    // A password of white space counts as unset.
    let api = Api::with(Default::default(), Some(" \t"));
    api.get(LIST).await.assert(StatusCode::NOT_FOUND, "");
}

#[tokio::test]
async fn remote_clients_need_allow_remote() {
    let api = Api::new();
    for peer in [REMOTE, "127.0.0.2:1", "[::2]:1", "[::ffff:127.0.0.2]:1"] {
        list_from(&api, peer, KEY)
            .await
            .assert(StatusCode::FORBIDDEN, &error("remote management disabled"));
    }
    // Go writes a mapped IPv4 address as IPv4, so a dual-stack listener's
    // 127.0.0.1 is local too.
    for peer in [LOCAL, "[::1]:1", "[::ffff:127.0.0.1]:1"] {
        let answer = list_from(&api, peer, KEY).await;
        assert_eq!(answer.status, StatusCode::OK, "{peer}: {}", answer.body);
    }

    let mut config = keyed_config();
    config.remote_management.allow_remote = true;
    let api = Api::with(config, None);
    let answer = list_from(&api, REMOTE, KEY).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
}

#[tokio::test]
async fn keys_are_read_from_each_header() {
    let api = Api::new();
    api.send(request_from(LOCAL, Method::GET, LIST, ""))
        .await
        .assert(StatusCode::UNAUTHORIZED, &error("missing management key"));

    for (name, value) in [
        ("authorization", KEY.to_owned()),
        ("authorization", format!("bearer {KEY}")),
        ("x-management-key", KEY.to_owned()),
    ] {
        let mut request = request_from(LOCAL, Method::GET, LIST, "");
        request.headers_mut().insert(name, value.parse().unwrap());
        let answer = api.send(request).await;
        assert_eq!(answer.status, StatusCode::OK, "{name}: {value}");
    }

    // A wrong Authorization wins over a right X-Management-Key.
    let mut request = request_from(LOCAL, Method::GET, LIST, "");
    let headers = request.headers_mut();
    headers.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
    headers.insert("x-management-key", KEY.parse().unwrap());
    api.send(request)
        .await
        .assert(StatusCode::UNAUTHORIZED, &error("invalid management key"));
}

#[tokio::test]
async fn bcrypt_secret_keys_are_accepted() {
    let mut config = keyed_config();
    config.remote_management.secret_key = bcrypt::hash(KEY, 4).unwrap();
    let api = Api::with(config, None);
    let answer = list_from(&api, LOCAL, KEY).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    list_from(&api, LOCAL, "wrong")
        .await
        .assert(StatusCode::UNAUTHORIZED, &error("invalid management key"));
    // The hash itself isn't the key.
    let hash = api.state.config().remote_management.secret_key.clone();
    list_from(&api, "127.0.0.1:2", &hash)
        .await
        .assert(StatusCode::UNAUTHORIZED, &error("invalid management key"));
}

#[tokio::test]
async fn trusted_proxies_decide_who_is_local() {
    let forwarded = |peer: &str, header: &'static str, value: &str| {
        let mut request = keyed(Method::GET, LIST, "");
        let peer: std::net::SocketAddr = peer.parse().unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
        request.headers_mut().insert(header, value.parse().unwrap());
        request
    };

    // Without trusted proxies, forwarding headers are ignored both ways.
    let api = Api::new();
    for header in ["x-forwarded-for", "x-real-ip"] {
        api.send(forwarded(REMOTE, header, "127.0.0.1"))
            .await
            .assert(StatusCode::FORBIDDEN, &error("remote management disabled"));
        let answer = api.send(forwarded(LOCAL, header, "203.0.113.9")).await;
        assert_eq!(answer.status, StatusCode::OK, "{header}");
    }

    let mut config = keyed_config();
    config.trusted_proxies = vec!["10.0.0.0/8".into()];
    let api = Api::with(config, None);
    // A trusted proxy speaks for its client, local or not.
    let answer = api
        .send(forwarded("10.1.2.3:5000", "x-forwarded-for", "127.0.0.1"))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let answer = api
        .send(forwarded("10.1.2.3:5000", "x-real-ip", "::1"))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    api.send(forwarded("10.1.2.3:5000", "x-forwarded-for", "203.0.113.9"))
        .await
        .assert(StatusCode::FORBIDDEN, &error("remote management disabled"));
    // The list is read from the end: a client can't hide behind an entry
    // it wrote itself.
    api.send(forwarded(
        "10.1.2.3:5000",
        "x-forwarded-for",
        "127.0.0.1, 203.0.113.9",
    ))
    .await
    .assert(StatusCode::FORBIDDEN, &error("remote management disabled"));
    // An untrusted peer can't claim to be local.
    for header in ["x-forwarded-for", "x-real-ip"] {
        api.send(forwarded(REMOTE, header, "127.0.0.1"))
            .await
            .assert(StatusCode::FORBIDDEN, &error("remote management disabled"));
    }
}

#[tokio::test]
async fn bans_are_per_address_and_a_success_clears_the_count() {
    let api = Api::new();
    for _ in 0..4 {
        list_from(&api, LOCAL, "wrong")
            .await
            .assert(StatusCode::UNAUTHORIZED, &error("invalid management key"));
    }
    assert_eq!(list_from(&api, LOCAL, KEY).await.status, StatusCode::OK);
    for _ in 0..4 {
        list_from(&api, LOCAL, "wrong")
            .await
            .assert(StatusCode::UNAUTHORIZED, &error("invalid management key"));
    }
    assert_eq!(list_from(&api, LOCAL, KEY).await.status, StatusCode::OK);

    for _ in 0..5 {
        list_from(&api, "[::1]:1", "wrong").await;
    }
    let answer = list_from(&api, "[::1]:1", KEY).await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert!(
        answer.body.starts_with(
            r#"{"error":"IP banned due to too many failed attempts. Try again in 30m0s"#
        ) || answer.body.starts_with(
            r#"{"error":"IP banned due to too many failed attempts. Try again in 29m59s"#
        ),
        "{}",
        answer.body
    );
    assert!(answer.header("x-cpa-version").is_some());
    assert_eq!(list_from(&api, LOCAL, KEY).await.status, StatusCode::OK);
}

#[tokio::test]
async fn errors_never_echo_the_key() {
    let api = Api::new();
    for key in ["wrong-but-secret", KEY] {
        let answer = list_from(&api, REMOTE, key).await;
        assert!(!answer.body.contains(key), "{}", answer.body);
    }
    let answer = list_from(&api, LOCAL, "wrong-but-secret").await;
    assert!(!answer.body.contains("wrong-but-secret"), "{}", answer.body);
}

#[tokio::test]
async fn auth_file_models_lists_a_credentials_models() {
    let api = Api::new();
    let mut auth = Auth {
        id: "models-auth".into(),
        file_name: "models.json".into(),
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.attributes.insert("runtime_only".into(), "true".into());
    api.register(auth);
    api.registry.register_client(
        "models-auth",
        "codex",
        &[
            ModelInfo {
                id: "gpt-5".into(),
                owned_by: "openai".into(),
                display_name: "GPT 5".into(),
                model_type: "openai".into(),
                ..ModelInfo::default()
            },
            ModelInfo {
                id: "bare".into(),
                ..ModelInfo::default()
            },
        ],
    );

    let want = r#"{"models":[{"display_name":"GPT 5","id":"gpt-5","owned_by":"openai","type":"openai"},{"id":"bare"}]}"#;
    for path in [
        "/v0/management/auth-files/models?name=models.json",
        "/v0/management/auth-files/models?name=models-auth",
        "/v8/management/credentials/models?name=models.json",
    ] {
        api.get(path).await.assert(StatusCode::OK, want);
    }
    // An unknown name is taken as a credential ID.
    api.get("/v0/management/auth-files/models?name=unknown")
        .await
        .assert(StatusCode::OK, r#"{"models":[]}"#);
    for path in [
        "/v0/management/auth-files/models",
        "/v0/management/auth-files/models?name=",
    ] {
        api.get(path)
            .await
            .assert(StatusCode::BAD_REQUEST, &error("name is required"));
    }
}

/// A handler that answers `ok`.
async fn ok() -> &'static str {
    "ok"
}

/// The API with `config`, serving only the test routes of the registry
/// tests.
fn registry_api(config: Config) -> Api {
    let mut api = Api::with(config, None);
    let tree = |Path(path): Path<String>| async move { path };
    let routes = vec![
        Route::key("/v0/management/test-key", get(ok)),
        Route::availability("/v0/management/test-availability", get(ok)),
        Route::open("/v8/management/test-open", get(ok)),
        Route::open("/test/callback", get(ok)),
        Route::key("/v0/management/test-merged", get(ok)),
        Route::key("/v0/management/test-merged", post(ok)),
        Route::availability("/v8/management/test-mixed", get(ok)),
        Route::key("/v8/management/test-mixed", post(ok)),
        Route::key("/v8/management/test-tree/{*path}", get(tree)),
    ];
    api.router = router_from(api.state.clone(), routes);
    api
}

/// Checks `answer` is the empty 404, with no version headers.
fn assert_unported(answer: &Answer, what: &str) {
    assert_eq!(
        (answer.status, answer.body.as_str()),
        (StatusCode::NOT_FOUND, ""),
        "{what}"
    );
    assert_eq!(answer.header("x-cpa-version"), None, "{what}");
}

// Not upstream's: each access gets its checks, whatever the order routes
// on one path come in.
#[tokio::test]
async fn routes_get_the_checks_of_their_access() {
    let api = registry_api(keyed_config());
    let unkeyed = |method: Method, path: &str| request_from(LOCAL, method, path, "");

    // Key: the availability check, then the key.
    let answer = api
        .send(unkeyed(Method::GET, "/v0/management/test-key"))
        .await;
    answer.assert(StatusCode::UNAUTHORIZED, &error("missing management key"));
    let answer = api.get("/v0/management/test-key").await;
    answer.assert(StatusCode::OK, "ok");
    assert!(answer.header("x-cpa-version").is_some());

    // Availability and Open: anyone, from anywhere, without version headers.
    for path in [
        "/v0/management/test-availability",
        "/v8/management/test-open",
        "/test/callback",
    ] {
        for request in [
            unkeyed(Method::GET, path),
            request_from(REMOTE, Method::GET, path, ""),
        ] {
            let answer = api.send(request).await;
            answer.assert(StatusCode::OK, "ok");
            assert_eq!(answer.header("x-cpa-version"), None, "{path}");
        }
    }

    // Two methods on one path, and two accesses on one path.
    let answer = api.post("/v0/management/test-merged", "").await;
    answer.assert(StatusCode::OK, "ok");
    api.get("/v0/management/test-merged")
        .await
        .assert(StatusCode::OK, "ok");
    let answer = api
        .send(unkeyed(Method::GET, "/v8/management/test-mixed"))
        .await;
    answer.assert(StatusCode::OK, "ok");
    let answer = api
        .send(unkeyed(Method::POST, "/v8/management/test-mixed"))
        .await;
    answer.assert(StatusCode::UNAUTHORIZED, &error("missing management key"));
    api.post("/v8/management/test-mixed", "")
        .await
        .assert(StatusCode::OK, "ok");

    // A catch-all route beside the prefix's.
    api.get("/v8/management/test-tree/a/b")
        .await
        .assert(StatusCode::OK, "a/b");
    assert_unported(
        &api.get("/v8/management/test-treeless").await,
        "beside a catch-all",
    );

    // Other methods: the empty 404 under the prefixes, unchecked, and the
    // server's 404 elsewhere.
    for (method, path) in [
        (Method::POST, "/v0/management/test-key"),
        (Method::HEAD, "/v0/management/test-key"),
        (Method::DELETE, "/v0/management/test-availability"),
        (Method::HEAD, "/v8/management/test-open"),
        (Method::PUT, "/v0/management/test-merged"),
        (Method::HEAD, "/v8/management/test-mixed"),
        (Method::PATCH, "/v8/management/test-mixed"),
    ] {
        for request in [
            keyed(method.clone(), path, ""),
            request_from(REMOTE, method.clone(), path, ""),
        ] {
            assert_unported(&api.send(request).await, &format!("{method} {path}"));
        }
    }
    for method in [Method::POST, Method::HEAD] {
        let answer = api.send(unkeyed(method.clone(), "/test/callback")).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method}");
        if method != Method::HEAD {
            assert_eq!(answer.body, "404 page not found");
        }
        assert_eq!(answer.header("content-type"), Some("text/plain"));
    }
}

// Not upstream's: without a management key, only Open routes answer.
#[tokio::test]
async fn only_open_routes_answer_without_a_management_key() {
    let api = registry_api(Config::default());
    for path in [
        "/v0/management/test-key",
        "/v0/management/test-availability",
        "/v8/management/test-mixed",
    ] {
        assert_unported(&api.get(path).await, path);
    }
    for path in ["/v8/management/test-open", "/test/callback"] {
        api.get(path).await.assert(StatusCode::OK, "ok");
    }
}

// Not upstream's: the test helpers' multipart bodies reach a handler
// through axum's multipart extractor.
#[tokio::test]
async fn multipart_bodies_reach_handlers() {
    async fn fields(mut form: Multipart) -> String {
        let mut out = String::new();
        while let Some(field) = form.next_field().await.unwrap() {
            let name = field.name().unwrap_or_default().to_owned();
            let file_name = field.file_name().unwrap_or("-").to_owned();
            let text = field.text().await.unwrap();
            out.push_str(&format!("{name} {file_name} {text}\n"));
        }
        out
    }
    let mut api = Api::new();
    let routes = vec![Route::key("/v0/management/test-upload", post(fields))];
    api.router = router_from(api.state.clone(), routes);
    let form = super::Multipart::new().text("note", "hello").file(
        "file",
        "codex-a.json",
        br#"{"type":"codex"}"#,
    );
    let answer = api
        .send(form.request(Method::POST, "/v0/management/test-upload"))
        .await;
    answer.assert(
        StatusCode::OK,
        "note - hello\nfile codex-a.json {\"type\":\"codex\"}\n",
    );
}

// Not upstream's: the builders set what the service sets on a clone of the
// state, and the clones share the rest.
#[tokio::test]
async fn state_builders_set_what_the_service_sets() {
    let plain = Api::new().state;
    assert!(plain.store().is_none());
    assert!(plain.sync().is_none());
    assert_eq!(plain.config_path(), None);
    let unavailable = plain.credential_store().err().unwrap().into_response();
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        plain.latest_release_url(),
        crate::latest_version::LATEST_RELEASE_URL
    );

    let auth_dir = super::AuthDir::new();
    let built = Api::over(&auth_dir).state;
    assert!(built.credential_store().is_ok());
    assert_eq!(built.config_path(), Some(auth_dir.config_path().as_path()));
    let built = built.with_latest_release_url("http://127.0.0.1:1/latest");
    assert_eq!(built.latest_release_url(), "http://127.0.0.1:1/latest");

    let clone = built.clone().with_config_path("elsewhere.yaml".into());
    assert_eq!(
        clone.config_path(),
        Some(std::path::Path::new("elsewhere.yaml"))
    );
    assert_eq!(built.config_path(), Some(auth_dir.config_path().as_path()));
    let _held = clone.credential_lock().lock().await;
    assert!(built.credential_lock().try_lock().is_err());
    assert!(std::ptr::eq(clone.oauth_sessions(), built.oauth_sessions()));
}
