// Ported from CLIProxyAPI internal/api/handlers/management/quota.go
// (ResetQuota) and api_tools.go (authByIndex) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v0/management/reset-quota` (also
//! `/v8/management/routing/cooldown/reset`): clears one credential's quota
//! and cooldowns and puts its models back in rotation.
//!
//! The body names the credential by its `auth_index`. The answer lists the
//! models cleared, or the credential's registered models when none had
//! state.
//!
//! Deviations from upstream:
//! - When two credentials share an index, the first by ID is reset;
//!   upstream takes whichever its map yields first.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use axum::routing::post;
use http::StatusCode;
use open_ferry_core::auth::Auth;
use open_ferry_core::manager::Manager;
use serde::de::MapAccess;

use crate::Route;
use crate::auth_files::{auth_index, run_blocking};
use crate::bind::{self, GoStruct, set_string};
use crate::json::{self, Json};
use crate::state::ManagementState;

/// The body of a reset.
#[derive(Default)]
struct ResetQuotaRequest {
    auth_index: String,
}

impl GoStruct for ResetQuotaRequest {
    const FIELDS: &'static [&'static str] = &["auth_index"];

    fn set<'de, A: MapAccess<'de>>(&mut self, _: usize, map: &mut A) -> Result<(), A::Error> {
        set_string(&mut self.auth_index, map)
    }
}

/// The credential with index `index`, after trimming (upstream's
/// `authByIndex`).
pub(crate) fn auth_by_index(manager: &Manager, index: &str) -> Option<Arc<Auth>> {
    let index = index.trim();
    if index.is_empty() {
        return None;
    }
    manager
        .list()
        .into_iter()
        .find(|auth| auth_index(auth) == index)
}

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key("/v0/management/reset-quota", post(reset)),
        Route::key("/v8/management/routing/cooldown/reset", post(reset)),
    ]
}

/// `POST /v0/management/reset-quota` (upstream's `ResetQuota`).
pub(crate) async fn reset(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match bind::read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(request) = bind::decode::<ResetQuotaRequest>(&body) else {
        return json::error(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let index = request.auth_index.trim();
    if index.is_empty() {
        return json::error(StatusCode::BAD_REQUEST, "auth_index is required");
    }
    let manager = state.manager().clone();
    let Some(auth) = auth_by_index(&manager, index) else {
        return json::error(StatusCode::NOT_FOUND, "auth not found");
    };
    // The reset saves the credential to its file.
    let id = auth.id.clone();
    let result = run_blocking(move || manager.reset_quota(&id)).await;
    match result {
        Err(error) => json::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to reset quota: {error}"),
        ),
        Ok(None) => json::error(StatusCode::NOT_FOUND, "auth not found"),
        Ok(Some(reset)) => {
            let models = reset.models.into_iter().map(Json::Str).collect();
            json::response(
                StatusCode::OK,
                &Json::map([
                    ("status", Json::Str("ok".into())),
                    ("auth_index", Json::Str(auth_index(&reset.auth))),
                    ("models", Json::Array(models)),
                ]),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go's decode of these bodies into upstream's reset request: `None`
    /// where it fails, else its `auth_index` (go1.27.1 building for go
    /// 1.26.0, without `GOEXPERIMENT=jsonv2`).
    #[test]
    fn bodies_decode_as_go_decodes_them() {
        let cases: &[(&[u8], Option<&str>)] = &[
            (b"", None),
            (b" ", None),
            (b"null", Some("")),
            (b"{}", Some("")),
            (b"[]", None),
            (b"1", None),
            (b"\"x\"", None),
            (b"{\"method\":\"GET\"}", Some("")),
            (b"{\"method\":1}", Some("")),
            (b"{\"METHOD\":\"a\"}", Some("")),
            (b"{\"Method\":\"a\",\"method\":\"b\"}", Some("")),
            (b"{\"method\":\"b\",\"Method\":\"a\"}", Some("")),
            (b"{\"method\":null}", Some("")),
            (b"{\"auth_index\":null}", Some("")),
            (b"{\"auth_index\":\"x\"}", Some("x")),
            (b"{\"authindex\":\"x\"}", Some("")),
            (b"{\"AUTHINDEX\":\"y\"}", Some("")),
            (
                b"{\"auth_index\":\"a\",\"authIndex\":\"b\",\"AuthIndex\":\"c\"}",
                Some("a"),
            ),
            (
                b"{\"header\":{\"a\":\"1\"},\"header\":{\"b\":\"2\"}}",
                Some(""),
            ),
            (b"{\"header\":null}", Some("")),
            (b"{\"header\":{\"a\":1}}", Some("")),
            (b"{\"header\":[]}", Some("")),
            (b"{\"header\":{\"a\":null}}", Some("")),
            (b"{\"data\":\"x\",\"data\":null}", Some("")),
            (b"{\"url\":\"\x5cu0041\"}", Some("")),
            (b"{\"x\":1}{", Some("")),
            (b"{\"method\":\"a\"} trailing", Some("")),
            (b"{\"method\":\"a\"", None),
            (b"{\"method\":\"\xff\"}", Some("")),
            (b"{\"auth_\xc4\xb1ndex\":\"i\"}", Some("")),
            (b"{\"auth_\xc4\xb0ndex\":\"j\"}", Some("")),
            (b"{\"AUTH_INDEX\":\" idx \"}", Some(" idx ")),
            (b"{\"auth_index\":1}", None),
            (b"{\"auth_index\":\"a\",\"auth_index\":null}", Some("a")),
            (b"{\"AuthIndex\":\"p\",\"authindex\":\"q\"}", Some("")),
            (b"{\"x\":1e400}", Some("")),
            (b"{\"method\":\"GET\",\"x\":[1,{\"y\":null}]}", Some("")),
            (b"\xef\xbb\xbf{}", None),
            (b"{\"method\":\"a\"}\n\n", Some("")),
            (b" {\"method\":\"a\"} x", Some("")),
            (b"{} {", Some("")),
            (b"nul", None),
            (b"null x", Some("")),
            (b"{\"method\":\"a\",}", None),
            (b"{\"method\":\"a\" \"url\":\"b\"}", None),
            (b"{\"header\":{\"a\":\"1\",\"A\":\"2\"}}", Some("")),
            (b"{\"header\":{\"\":\"x\"}}", Some("")),
            (b"{\"method\":\"a\",\"method\":\"b\"}", Some("")),
            (b"{\"method\":true}", Some("")),
            (
                b"{\"header\":{\"a\":\"1\"},\"header\":null,\"header\":{\"c\":\"3\"}}",
                Some(""),
            ),
            (b"{\"header\":\"x\"}", Some("")),
            (b"{\"data\":{\"a\":1}}", Some("")),
            (b"{\"Data\":\"D\",\"DATA\":\"E\"}", Some("")),
            (
                b"{\"proxy_url\":\"p\",\"PROXY_URL\":\"q\",\"proxyurl\":\"r\"}",
                Some(""),
            ),
            (b"{\"auth_index\":\"s\",\"AUTH_INDEX\":\"t\"}", Some("t")),
            (b"{\"authIndex\":null,\"AUTHindex\":\"u\"}", Some("")),
            (b"{\"m\xc4\xb0thod\":\"v\"}", Some("")),
            (b"{\"URL\":\"w\"}", Some("")),
            (b"{\"ur\xc4\xb1\":\"x\"}", Some("")),
            (b"{\"method\":\"\x5cu00e9\"}", Some("")),
            (b"{\"method\":\"a\x5cu0000b\"}", Some("")),
            (b"{\"method\":-0}", Some("")),
            (b"[{}]", None),
            (b"{\"auth_index\":[\"a\"]}", None),
            (b"{\"header\":{\"a\":\"1\",\"a\":\"2\"}}", Some("")),
        ];
        for &(body, want) in cases {
            let got = crate::bind::decode::<ResetQuotaRequest>(body).map(|r| r.auth_index);
            assert_eq!(got.as_deref(), want, "{}", String::from_utf8_lossy(body));
        }
    }
}
