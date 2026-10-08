//! Not upstream's: the dashboard on each listener while
//! `management.separate-address` is set. On the proxy's listener
//! ([`Listener::Closed`]) every path answers exactly as it does while the
//! control panel is disabled and no management key is set; on the
//! management address's own ([`Listener::Separate`]) everything is served
//! as usual, with the same access rules.

use http::{Method, StatusCode};
use serde_json::json;
use tower::ServiceExt as _;

use super::{Dash, KEY, LOCAL, REMOTE, keyed, keyed_config, request};
use crate::Listener;

/// The requests compared, with and without the key: every route of the
/// app and the API, a method none serves, and paths that aren't routes.
fn requests() -> Vec<(Method, &'static str)> {
    let mut requests = Vec::new();
    for path in [
        "/management.html",
        "/management.html?safe-mode=configure",
        "/dashboard",
        "/dashboard/",
        "/dashboard/index.html",
        "/dashboard/assets/index-3f2a9c.js",
        "/dashboard/usage",
        "/open-ferry",
        "/open-ferry/",
        "/open-ferry/api/v1/nothing",
        "/open-ferry/api/v1/usage/summary",
        "/open-ferry/api/v1/usage/series",
        "/open-ferry/api/v1/usage/requests",
        "/open-ferry/api/v1/usage/ledger",
        "/open-ferry/api/v1/usage/prices",
        "/open-ferry/api/v1/request-logs",
        "/open-ferry/api/v1/request-logs/main.log",
        "/open-ferry/api/v1/request-logs/main.log/download",
        "/open-ferry/api/v1/client-setup",
        "/open-ferry/api/v1/claude-cli/entries",
        "/open-ferry/api/v1/claude-cli/auth-status",
    ] {
        for method in [Method::GET, Method::HEAD, Method::POST] {
            requests.push((method, path));
        }
    }
    requests.push((Method::PATCH, "/open-ferry/api/v1/usage/ledger"));
    requests.push((Method::DELETE, "/open-ferry/api/v1/usage/records"));
    requests.push((Method::PUT, "/open-ferry/api/v1/usage/prices"));
    requests.push((Method::DELETE, "/open-ferry/api/v1/usage/prices"));
    requests
}

#[tokio::test]
async fn the_proxys_listener_answers_as_with_management_disabled() {
    // A key is set, and allow-remote, so nothing but the listener refuses.
    let mut config = keyed_config();
    config.remote_management.allow_remote = true;
    let closed = Dash::on(config, Listener::Closed);
    let mut disabled = keyed_config();
    disabled.remote_management.secret_key.clear();
    disabled.remote_management.disable_control_panel = true;
    let disabled = Dash::with_config(disabled);

    for (method, path) in requests() {
        for peer in [LOCAL, REMOTE] {
            for with_key in [false, true] {
                let make = || {
                    let mut request = request(peer, method.clone(), path, "{}");
                    if with_key {
                        let value = format!("Bearer {KEY}").parse().unwrap();
                        request
                            .headers_mut()
                            .insert(http::header::AUTHORIZATION, value);
                    }
                    request
                };
                let want = disabled.send(make()).await;
                let got = closed.send(make()).await;
                let what = format!("{method} {path} from {peer}, key {with_key}");
                assert_eq!(got.status, want.status, "{what}: {}", got.body);
                assert_eq!(got.body, want.body, "{what}");
                assert_eq!(got.headers, want.headers, "{what}");
            }
        }
    }

    // Wrong keys there aren't checked, so they don't count toward a ban on
    // the management listener, which shares the record.
    for _ in 0..10 {
        let mut request = request(REMOTE, Method::GET, "/open-ferry/api/v1/usage/summary", "");
        let value = "Bearer wrong".parse().unwrap();
        request
            .headers_mut()
            .insert(http::header::AUTHORIZATION, value);
        closed.send(request).await;
    }
    let mut state = closed.state.clone();
    state.listener = Listener::Separate;
    let mut request = request(REMOTE, Method::GET, "/open-ferry/api/v1/usage/ledger", "");
    let value = format!("Bearer {KEY}").parse().unwrap();
    request
        .headers_mut()
        .insert(http::header::AUTHORIZATION, value);
    let response = crate::router_from(state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_management_listener_serves_everything_with_the_same_rules() {
    let dash = Dash::on(keyed_config(), Listener::Separate);
    let app = dash
        .send(request(LOCAL, Method::GET, "/dashboard/", ""))
        .await;
    assert_eq!(app.status, StatusCode::OK, "{}", app.body);
    let redirect = dash
        .send(request(LOCAL, Method::GET, "/management.html", ""))
        .await;
    assert_eq!(redirect.status, StatusCode::FOUND);

    dash.send(keyed(Method::GET, "/open-ferry/api/v1/usage/ledger", ""))
        .await
        .json(StatusCode::OK);
    dash.send(request(
        LOCAL,
        Method::GET,
        "/open-ferry/api/v1/usage/ledger",
        "",
    ))
    .await
    .error(StatusCode::UNAUTHORIZED, "missing_management_key");

    // allow-remote still decides for a client that isn't local.
    let remote = dash
        .send(request(REMOTE, Method::GET, "/dashboard/", ""))
        .await;
    assert_eq!(remote.status, StatusCode::FORBIDDEN);
    let mut request = request(REMOTE, Method::GET, "/open-ferry/api/v1/usage/ledger", "");
    let value = format!("Bearer {KEY}").parse().unwrap();
    request
        .headers_mut()
        .insert(http::header::AUTHORIZATION, value);
    dash.send(request)
        .await
        .error(StatusCode::FORBIDDEN, "remote_management_disabled");

    // And a key must be set.
    let mut config = keyed_config();
    config.remote_management.secret_key.clear();
    let dash = Dash::on(config, Listener::Separate);
    dash.send(keyed(Method::GET, "/open-ferry/api/v1/usage/ledger", ""))
        .await
        .error(StatusCode::NOT_FOUND, "management_disabled");
}

#[tokio::test]
async fn the_client_setup_says_where_it_is_served() {
    let mut config = keyed_config();
    config.port = 8317;
    config.remote_management.base_url = "http://127.0.0.1:8318".to_owned();
    let shared = Dash::with_config(config.clone());
    let setup = shared
        .get("/open-ferry/api/v1/client-setup")
        .await
        .json(StatusCode::OK);
    assert_eq!(setup["separate_management"], false);
    assert_eq!(setup["base_urls"].as_array().map(Vec::len), Some(4));

    let separate = Dash::on(config, Listener::Separate);
    let setup = separate
        .get("/open-ferry/api/v1/client-setup")
        .await
        .json(StatusCode::OK);
    assert_eq!(setup["separate_management"], true);
    assert_eq!(
        setup["base_urls"],
        json!([
            {"url": "http://127.0.0.1:8317", "source": "listen"},
            {"url": "http://[::1]:8317", "source": "listen"},
            {"url": "http://localhost:8317", "source": "listen"},
        ])
    );
}
