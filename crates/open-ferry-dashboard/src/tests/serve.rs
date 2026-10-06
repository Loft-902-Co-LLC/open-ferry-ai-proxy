//! Serving the app at `/dashboard/`, and `/management.html`.

use http::{Method, StatusCode};

use super::{APP, Answer, Dash, LOCAL, REMOTE, keyed_config, request};
use crate::CONTENT_SECURITY_POLICY;
use crate::assets::Assets;

/// The answer to `GET path` from `peer`, without a key.
async fn get_from(dash: &Dash, peer: &str, path: &str) -> Answer {
    dash.send(request(peer, Method::GET, path, "")).await
}

/// Checks that `answer` carries the dashboard's security headers.
fn assert_secured(answer: &Answer) {
    assert_eq!(
        answer.header("content-security-policy"),
        Some(CONTENT_SECURITY_POLICY)
    );
    assert_eq!(answer.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(answer.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(answer.header("x-frame-options"), Some("DENY"));
}

/// Not upstream's: the app's files are served with their types; those
/// under `assets/` are cached for good and every other one is checked
/// each time.
#[tokio::test]
async fn the_apps_files_are_served() {
    let dash = Dash::new();
    for (path, file, content_type, cache) in [
        (
            "/dashboard/",
            "index.html",
            "text/html; charset=utf-8",
            "no-cache",
        ),
        (
            "/dashboard/index.html",
            "index.html",
            "text/html; charset=utf-8",
            "no-cache",
        ),
        (
            "/dashboard/assets/index-3f2a9c.js",
            "assets/index-3f2a9c.js",
            "text/javascript; charset=utf-8",
            "public, max-age=31536000, immutable",
        ),
        (
            "/dashboard/assets/index-77b1e0.css",
            "assets/index-77b1e0.css",
            "text/css; charset=utf-8",
            "public, max-age=31536000, immutable",
        ),
        (
            "/dashboard/favicon.svg",
            "favicon.svg",
            "image/svg+xml",
            "no-cache",
        ),
        (
            "/dashboard/third-party-licenses.txt",
            "third-party-licenses.txt",
            "text/plain; charset=utf-8",
            "no-cache",
        ),
    ] {
        let answer = get_from(&dash, LOCAL, path).await;
        assert_eq!(answer.status, StatusCode::OK, "{path}");
        let bytes = APP
            .iter()
            .find(|(name, _)| *name == file)
            .map(|(_, bytes)| *bytes)
            .unwrap();
        assert_eq!(answer.body.as_bytes(), bytes, "{path}");
        assert_eq!(answer.header("content-type"), Some(content_type), "{path}");
        assert_eq!(answer.header("cache-control"), Some(cache), "{path}");
        assert_secured(&answer);
    }

    let head = dash
        .send(request(LOCAL, Method::HEAD, "/dashboard/", ""))
        .await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.body, "");
    assert_secured(&head);
}

/// Not upstream's: a path below `/dashboard/` that isn't a file answers
/// `index.html`, for the app's own routing, except below `assets/`.
#[tokio::test]
async fn other_paths_answer_the_app_but_assets_dont() {
    let dash = Dash::new();
    for path in [
        "/dashboard/usage",
        "/dashboard/logs/v1-chat-2026-10-05T115802-1234abcd.log",
        "/dashboard/setup?safe-mode=configure",
        "/dashboard/assets",
    ] {
        let answer = get_from(&dash, LOCAL, path).await;
        assert_eq!(answer.status, StatusCode::OK, "{path}");
        assert_eq!(answer.body, "<!doctype html><title>app</title>", "{path}");
        assert_eq!(answer.header("cache-control"), Some("no-cache"), "{path}");
    }
    for path in ["/dashboard/assets/index-000000.js", "/dashboard/assets/"] {
        let answer = get_from(&dash, LOCAL, path).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(answer.body, "404 page not found", "{path}");
        assert_secured(&answer);
    }
}

/// Not upstream's: `/dashboard` sends the browser to `/dashboard/`, with
/// its query.
#[tokio::test]
async fn the_bare_path_redirects() {
    let dash = Dash::new();
    let answer = get_from(&dash, LOCAL, "/dashboard").await;
    assert_eq!(answer.status, StatusCode::FOUND);
    assert_eq!(answer.header("location"), Some("/dashboard/"));
    let answer = get_from(&dash, LOCAL, "/dashboard?tab=usage").await;
    assert_eq!(answer.header("location"), Some("/dashboard/?tab=usage"));
    assert_secured(&answer);
}

/// Ported from upstream's `TestExampleAPIKeySafeModeShowsWarningAndKeepsManagement`
/// ("management button query opens control panel"), in
/// internal/api/server_test.go: `/management.html?safe-mode=configure`
/// opens the control panel, which is the dashboard here, reached by a
/// redirect that keeps the query.
#[tokio::test]
async fn management_html_opens_the_dashboard() {
    let mut config = keyed_config();
    config.api_keys = vec!["your-api-key-1".to_owned()];
    let dash = Dash::with_config(config);

    let answer = get_from(&dash, LOCAL, "/management.html?safe-mode=configure").await;
    assert_eq!(answer.status, StatusCode::FOUND);
    assert_eq!(
        answer.header("location"),
        Some("/dashboard/?safe-mode=configure")
    );
    assert_eq!(answer.header("cache-control"), Some("no-cache"));
    assert_secured(&answer);

    let answer = get_from(&dash, LOCAL, "/management.html").await;
    assert_eq!(answer.status, StatusCode::FOUND);
    assert_eq!(answer.header("location"), Some("/dashboard/"));
}

/// Not upstream's: `/management.html` takes only `GET`, as upstream's gin
/// route does outside safe mode; `HEAD` and other methods are the server's
/// 404.
#[tokio::test]
async fn management_html_takes_only_get() {
    let dash = Dash::new();
    for method in [Method::HEAD, Method::POST, Method::DELETE] {
        let answer = dash
            .send(request(LOCAL, method.clone(), "/management.html", ""))
            .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method}");
        assert_secured(&answer);
    }
    let answer = dash
        .send(request(LOCAL, Method::POST, "/dashboard/", ""))
        .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.body, "404 page not found");
}

/// Not upstream's: while `remote-management.disable-control-panel` is set,
/// the app and `/management.html` answer an empty 404, as upstream's
/// `serveManagementControlPanel` does; the dashboard API still works.
#[tokio::test]
async fn a_disabled_control_panel_hides_the_app() {
    let mut config = keyed_config();
    config.remote_management.disable_control_panel = true;
    let dash = Dash::with_config(config);
    for path in [
        "/management.html",
        "/management.html?safe-mode=configure",
        "/dashboard",
        "/dashboard/",
        "/dashboard/assets/index-3f2a9c.js",
        "/dashboard/usage",
    ] {
        let answer = get_from(&dash, LOCAL, path).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(answer.body, "", "{path}");
        assert_secured(&answer);
    }
    dash.get("/open-ferry/api/v1/usage/ledger")
        .await
        .json(StatusCode::OK);
}

/// Not upstream's: a client the management API would refuse for its
/// address gets the same refusal from the app; one it allows gets the app.
#[tokio::test]
async fn the_app_refuses_whom_the_management_api_refuses() {
    let dash = Dash::new();
    for path in ["/management.html", "/dashboard", "/dashboard/"] {
        let answer = get_from(&dash, REMOTE, path).await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(answer.body, r#"{"error":"remote management disabled"}"#);
        assert_secured(&answer);
    }

    let mut config = keyed_config();
    config.remote_management.allow_remote = true;
    let dash = Dash::with_config(config);
    let answer = get_from(&dash, REMOTE, "/dashboard/").await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, "<!doctype html><title>app</title>");
}

/// Not upstream's: an address banned for failed attempts at the API is
/// refused the app too. The app asks for no key, and counts none it is
/// sent.
#[tokio::test]
async fn a_banned_address_is_refused_the_app() {
    let dash = Dash::new();
    let wrong = |path: &str| {
        let mut wrong = request(LOCAL, Method::GET, path, "");
        wrong
            .headers_mut()
            .insert("x-management-key", "wrong".parse().unwrap());
        wrong
    };
    for _ in 0..10 {
        let answer = dash.send(wrong("/dashboard/")).await;
        assert_eq!(answer.status, StatusCode::OK);
    }
    for _ in 0..5 {
        dash.send(wrong("/open-ferry/api/v1/usage/ledger"))
            .await
            .error(StatusCode::UNAUTHORIZED, "invalid_management_key");
    }

    let answer = get_from(&dash, LOCAL, "/dashboard/").await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    let message = body["error"].as_str().unwrap();
    assert!(
        message.starts_with("IP banned due to too many failed attempts. Try again in"),
        "{message}"
    );
    assert_secured(&answer);
    dash.get("/open-ferry/api/v1/usage/ledger")
        .await
        .error(StatusCode::FORBIDDEN, "ip_banned");
}

/// Not upstream's: the app is served while no management key is set, so
/// it can say how to set one; its API calls answer `management_disabled`.
#[tokio::test]
async fn the_app_is_served_without_a_key() {
    let dash = Dash::with_config(open_ferry_core::config::Config::default());
    let answer = get_from(&dash, LOCAL, "/dashboard/").await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, "<!doctype html><title>app</title>");
    let answer = get_from(&dash, LOCAL, "/management.html").await;
    assert_eq!(answer.status, StatusCode::FOUND);
    dash.get("/open-ferry/api/v1/usage/ledger")
        .await
        .error(StatusCode::NOT_FOUND, "management_disabled");
}

/// Not upstream's: a binary built without the app answers every path
/// below `/dashboard/` with a page saying so and how to build it, which
/// has no inline script or style.
#[tokio::test]
async fn without_an_app_a_page_says_how_to_build_it() {
    let dash = Dash::build(keyed_config(), Assets::missing(), true);
    for path in [
        "/dashboard/",
        "/dashboard/usage",
        "/dashboard/assets/index-3f2a9c.js",
    ] {
        let answer = get_from(&dash, LOCAL, path).await;
        assert_eq!(answer.status, StatusCode::OK, "{path}");
        assert_eq!(
            answer.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(answer.header("cache-control"), Some("no-cache"));
        assert!(
            answer
                .body
                .contains("The dashboard isn't built into this binary"),
            "{}",
            answer.body
        );
        for step in ["npm ci", "npm run build", "Node.js 22.22.2"] {
            assert!(answer.body.contains(step), "{step}");
        }
        for inline in ["<script", "<style", "style="] {
            assert!(!answer.body.contains(inline), "{inline}");
        }
        assert_secured(&answer);
    }
    dash.get("/open-ferry/api/v1/usage/ledger")
        .await
        .json(StatusCode::OK);
}
