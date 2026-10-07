//! The config's `claude-cli` entries.
//!
//! - `GET /claude-cli/entries`: each entry as the config has it, with the
//!   credential the server made of it, as the management API's
//!   `auth-files` would list it, and its last error. The `auth-files` list
//!   hides credentials made from the config, so this is where the
//!   dashboard finds their state, cooldowns and quota. Nothing of the
//!   entry's command or of its config directory's files is in the answer.
//! - `GET /claude-cli/auth-status`: whether an entry's Claude Code is
//!   signed in, by running its `auth status`. Claude Code's answer also
//!   names the account and its organization; only whether it is signed in
//!   and how are passed on, and nothing of it is logged.

use std::sync::Arc;

use axum::extract::State;
use axum::response::Response;
use chrono::Utc;
use http::{StatusCode, Uri};
use open_ferry_core::auth::synthesizer::StableIdGenerator;
use open_ferry_core::auth::synthesizer::claude_cli::CLAUDE_CLI_PROVIDER;
use open_ferry_core::auth::{Auth, AuthError};
use open_ferry_core::config::ClaudeCli;
use open_ferry_management::{ManagementState, credential_entry};
use open_ferry_providers::claude_cli::{Entry, StatusError, auth_status};
use serde::Serialize;
use serde_json::Value;

use super::{ApiError, Query, ok};
use crate::DashboardState;

/// `GET /claude-cli/entries`.
#[derive(Debug, Serialize)]
struct Entries {
    entries: Vec<EntryState>,
}

/// One entry of the config's `claude-cli` list, and how its credential is
/// doing.
#[derive(Debug, Serialize)]
struct EntryState {
    /// Its name, trimmed.
    name: String,
    /// The prefix of its models' names, trimmed; empty for none.
    prefix: String,
    /// Its `config-dir` as the config has it, trimmed; empty for none.
    config_dir: String,
    disabled: bool,
    /// The credential as `GET /v0/management/auth-files` would list it;
    /// `None` while the entry is disabled, or before the server has made
    /// its credential.
    credential: Option<Value>,
    /// The credential's last failure, until a request succeeds.
    last_error: Option<LastError>,
}

/// A credential's last failure.
#[derive(Debug, Serialize)]
struct LastError {
    /// What went wrong: for an error in the form of Anthropic's error
    /// body, its `error.message`.
    message: String,
    /// The status Claude Code's failure was given; `None` when it had none.
    http_status: Option<u16>,
}

/// `GET /claude-cli/entries`.
pub(super) async fn entries_route(State(state): State<DashboardState>) -> Response {
    ok(&entries(&state.management))
}

/// The config's `claude-cli` entries, in its order, each with its
/// credential.
fn entries(management: &ManagementState) -> Entries {
    let config = management.config();
    let now = Utc::now();
    // A credential's ID is made as the server makes it: from the entry's
    // name and config directory, counting repeats, over the enabled
    // entries in order.
    let mut ids = StableIdGenerator::new();
    let entries = config
        .claude_cli
        .iter()
        .map(|entry| {
            let found = if entry.disabled {
                None
            } else {
                let (id, _) = ids.next(
                    CLAUDE_CLI_PROVIDER,
                    &[entry.name.trim(), entry.config_dir.trim()],
                );
                credential_entry(management, &id, now)
            };
            entry_state(entry, found)
        })
        .collect();
    Entries { entries }
}

/// The answer's entry for `entry`, with the credential found for it.
fn entry_state(entry: &ClaudeCli, found: Option<(Arc<Auth>, Value)>) -> EntryState {
    let (last_error, credential) = match found {
        Some((auth, credential)) => (auth.last_error.as_ref().map(last_error), Some(credential)),
        None => (None, None),
    };
    EntryState {
        name: entry.name.trim().to_owned(),
        prefix: entry.prefix.trim().to_owned(),
        config_dir: entry.config_dir.trim().to_owned(),
        disabled: entry.disabled,
        credential,
        last_error,
    }
}

/// A failure as the answer gives it.
fn last_error(error: &AuthError) -> LastError {
    LastError {
        message: readable_message(&error.message),
        http_status: (error.http_status != 0).then_some(error.http_status),
    }
}

/// `message`, or the `error.message` of an error in the form of
/// Anthropic's error body, which is how the `claude-cli` executor gives
/// Claude Code's failures.
fn readable_message(message: &str) -> String {
    serde_json::from_str::<Value>(message)
        .ok()
        .as_ref()
        .and_then(|body| body.get("error")?.get("message")?.as_str())
        .map_or_else(|| message.to_owned(), str::to_owned)
}

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
