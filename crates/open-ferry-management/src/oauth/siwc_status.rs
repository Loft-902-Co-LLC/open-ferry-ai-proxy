// Ported from CLIProxyAPI internal/api/handlers/management/oauth_siwc.go
// (GetSIWCStatus, v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Read-only local SIWC status. The manager is always present in this port;
//! upstream's manager-less disk fallback is deliberately omitted.

use axum::extract::{RawQuery, State};
use axum::response::Response;
use http::StatusCode;
use open_ferry_core::auth::codex_refresh::is_codex_auth;
use open_ferry_translate::go::trim_space;

use crate::json::{self, Json};
use crate::query::Query;
use crate::state::ManagementState;

pub(super) async fn status(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
) -> Response {
    let query = Query::parse(raw.as_deref());
    let selector = |first, alias| {
        let value = trim_space(query.value(first));
        if value.is_empty() {
            trim_space(query.value(alias))
        } else {
            value
        }
    };
    let name = selector("name", "id");
    let index = selector("auth_index", "authIndex");
    if name.is_empty() && index.is_empty() {
        return json::error(StatusCode::BAD_REQUEST, "name or auth_index is required");
    }
    let target = std::str::from_utf8(name)
        .ok()
        .zip(std::str::from_utf8(index).ok())
        .and_then(|(name, index)| {
            if name.is_empty() {
                crate::quota::auth_by_index(state.manager(), index)
            } else {
                crate::credential_state::lookup(state.manager(), name, index)
            }
        });
    let Some(auth) = target else {
        return json::error(StatusCode::NOT_FOUND, "auth file not found");
    };
    if !is_codex_auth(&auth) {
        return json::error(
            StatusCode::BAD_REQUEST,
            "SIWC status requires a Codex credential",
        );
    }
    json::response(
        StatusCode::OK,
        &Json::map([(
            "status",
            Json::Str(auth.siwc_status(chrono::Utc::now()).into()),
        )]),
    )
}
