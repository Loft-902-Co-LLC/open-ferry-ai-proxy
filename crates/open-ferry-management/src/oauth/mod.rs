// To be ported from CLIProxyAPI internal/api/handlers/management/
// oauth_sessions.go, oauth_callback.go, auth_files_oauth_callback.go and
// auth_files_provider_oauth.go (RequestAnthropicToken, RequestCodexToken,
// CancelAuthSession, GetAuthStatus), auth_files_v8.go (StartOAuthV8), and
// the OAuth callback pages of internal/api/server_routes.go (v8.0.10,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OAuth logins started from the management API, for Claude and Codex
//! accounts, with the official OAuth flows only.
//!
//! Not ported yet: `GET /v0/management/anthropic-auth-url`,
//! `codex-auth-url` and `get-auth-status`, `DELETE oauth-session`, and
//! `GET` and `POST oauth-callback`, which needs no key (also under
//! `/v8/management/oauth`); and the main server's `GET /anthropic/callback`
//! and `/codex/callback` pages, which need nothing.

use crate::Route;

/// The OAuth login sessions in progress (upstream's `oauthSessionStore`):
/// none yet.
#[derive(Debug, Default)]
pub(crate) struct Sessions {}

/// The routes this module serves: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
