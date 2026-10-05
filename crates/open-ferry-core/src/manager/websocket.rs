// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_responses_websocket_session.go
// (responsesWebsocketAvailableAuthsForModel, responsesWebsocketUsesUpstreamWebsocketPassthrough,
// websocketUpstreamSupportsCompactionReplayForModel, responsesWebsocketPinnedAuthMatchesModel,
// responsesWebsocketAuthAvailableForModel) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The manager's answer to [`Dispatcher::websocket_support`]: which of its
//! credentials may serve a model, and whether the Responses WebSocket may
//! hand a session's requests to their upstream WebSocket as they are.
//!
//! Upstream's handler reads the auth manager and the global model registry
//! itself; here the handler asks the [`Dispatcher`], and the manager reads
//! its credentials, executors and [`ClientModels`].
//!
//! Deviations from upstream:
//! - The handler resolves the model and its providers and passes them in;
//!   upstream resolves them here.
//! - The Home runtime's pinned-model check isn't ported: a credential serves
//!   a model when the registry says so.
//!
//! [`Dispatcher::websocket_support`]: crate::exec::Dispatcher::websocket_support
//! [`Dispatcher`]: crate::exec::Dispatcher

use super::credential::websockets_enabled;
use super::select::{ClientModels, lookup_executor};
use super::text::{go_lower, parse_suffix};
use super::{Manager, State};
use crate::auth::{Auth, Status, Timestamp};
use crate::exec::{ProviderId, WebsocketAuth, WebsocketSupport};

impl Manager {
    /// What the Responses WebSocket may rely on for `model` among
    /// `providers`, and what it needs to know about `auth_id`.
    pub(crate) fn websocket_support_for(
        &self,
        providers: &[ProviderId],
        model: &str,
        auth_id: Option<&str>,
    ) -> WebsocketSupport {
        let now = self.now();
        let models = self.models();
        let state = self.lock();
        let available = available_auths(&state, models, providers, model, now);
        let auth = auth_id.and_then(|id| {
            let auth = &state.auths.get(id)?.auth;
            let provider = go_lower(auth.provider.trim());
            let serves_model = in_providers(providers, &provider)
                && available_for_model(auth, model, now)
                && models.client_supports_model(&auth.id, model);
            Some(WebsocketAuth {
                provider,
                serves_model,
                websockets: websockets_enabled(auth),
            })
        });
        WebsocketSupport {
            upstream_passthrough: uses_upstream_passthrough(&state, &available, model),
            compaction_replay: supports_compaction_replay(&available),
            auth,
        }
    }
}

/// Whether `provider` (lower case) is one of `providers`.
fn in_providers(providers: &[ProviderId], provider: &str) -> bool {
    providers
        .iter()
        .any(|candidate| go_lower(candidate.trim()) == provider)
}

/// The credentials that may serve `model` now
/// (`responsesWebsocketAvailableAuthsForModel`).
fn available_auths<'a>(
    state: &'a State,
    models: &dyn ClientModels,
    providers: &[ProviderId],
    model: &str,
    now: Timestamp,
) -> Vec<&'a Auth> {
    if providers.is_empty() {
        return Vec::new();
    }
    state
        .auths
        .values()
        .map(|entry| &*entry.auth)
        .filter(|auth| {
            // responsesWebsocketAuthMatchesModel
            in_providers(providers, &go_lower(auth.provider.trim()))
                && (model.is_empty() || models.client_supports_model(&auth.id, model))
                && available_for_model(auth, model, now)
        })
        .collect()
}

/// Whether the credentials are all of one provider, `codex` or `xai`,
/// whose executor is registered, and all have websockets on
/// (`responsesWebsocketUsesUpstreamWebsocketPassthrough`).
fn uses_upstream_passthrough(state: &State, auths: &[&Auth], model: &str) -> bool {
    if model.trim().is_empty() || auths.is_empty() {
        return false;
    }
    let mut provider = String::new();
    for auth in auths {
        let auth_provider = go_lower(auth.provider.trim());
        if auth_provider != "codex" && auth_provider != "xai" {
            return false;
        }
        if provider.is_empty() {
            if lookup_executor(&state.executors, &auth_provider).is_none() {
                return false;
            }
            provider = auth_provider;
        } else if auth_provider != provider {
            return false;
        }
        if !websockets_enabled(auth) {
            return false;
        }
    }
    !provider.is_empty()
}

/// Whether there are credentials and all are `codex`
/// (`websocketUpstreamSupportsCompactionReplayForModel`).
fn supports_compaction_replay(auths: &[&Auth]) -> bool {
    !auths.is_empty()
        && auths
            .iter()
            .all(|auth| auth.provider.trim().eq_ignore_ascii_case("codex"))
}

/// Whether `auth` is neither disabled nor cooling down for `model`
/// (`responsesWebsocketAuthAvailableForModel`).
fn available_for_model(auth: &Auth, model: &str, now: Timestamp) -> bool {
    if auth.disabled || auth.status == Status::Disabled {
        return false;
    }
    if !model.is_empty() && !auth.model_states.is_empty() {
        let mut state = auth.model_states.get(model);
        if state.is_none() {
            let base = parse_suffix(model).0.trim();
            if !base.is_empty() && base != model {
                state = auth.model_states.get(base);
            }
        }
        if let Some(state) = state {
            if state.status == Status::Disabled {
                return false;
            }
            return !cooling(state.unavailable, state.next_retry_after, now);
        }
    }
    !cooling(auth.unavailable, auth.next_retry_after, now)
}

/// Whether something unavailable waits for a retry time still to come.
fn cooling(unavailable: bool, next_retry_after: Option<Timestamp>, now: Timestamp) -> bool {
    unavailable && next_retry_after.is_some_and(|at| at > now)
}
