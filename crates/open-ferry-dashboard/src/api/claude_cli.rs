//! `GET /claude-cli/auth-status`: whether a `claude-cli` entry's Claude
//! Code is signed in, by running its `auth status`.
//!
//! Claude Code's answer also names the account and its organization; only
//! whether it is signed in and how are passed on, and nothing of it is
//! logged.

use axum::extract::State;
use axum::response::Response;
use http::{StatusCode, Uri};
use open_ferry_providers::claude_cli::{Entry, StatusError, auth_status};

use super::{ApiError, Query, ok};
use crate::DashboardState;

/// `GET /claude-cli/auth-status?name=<entry>`.
pub(super) async fn auth_status_route(
    State(state): State<DashboardState>,
    uri: Uri,
) -> Result<Response, ApiError> {
    let query = Query::of(&uri)?;
    let name = query
        .non_empty("name")?
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ApiError::invalid("name is required: the claude-cli entry's name"))?;
    let config = state.management.config();
    let entry = config
        .claude_cli
        .iter()
        .find(|entry| entry.name.trim().eq_ignore_ascii_case(name))
        .map(Entry::from_config)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no claude-cli entry is named {name:?}"),
            )
        })?;
    match auth_status(&entry, &state.claude_cli_root).await {
        Ok(status) => Ok(ok(&status)),
        Err(error @ StatusError::Timeout) => Err(ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "claude_cli_timeout",
            format!("claude-cli {}: {error}", entry.name),
        )),
        Err(error) => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "claude_cli_failed",
            format!("claude-cli {}: {error}", entry.name),
        )),
    }
}
