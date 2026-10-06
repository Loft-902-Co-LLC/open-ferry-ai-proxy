// Ported from CLIProxyAPI sdk/cliproxy/auth/credential_policy.go
// (credentialPolicyAllows), sdk/cliproxy/auth/conductor_selection.go
// (authSelectionEligibility, pickNextLegacy,
// SelectAuthWithCredentialPolicy), sdk/cliproxy/auth/conductor_execution.go
// (isFreeCodexAuth) and sdk/cliproxy/auth/conductor_models.go
// (ResolveExecutionModel, executionModelCandidates) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Credential policies and the free-plan rule, which narrow the credentials
//! a call may pick, the pick of one provider's credentials that honours a
//! policy, and the upstream model a credential is sent.
//!
//! The only policy is `codex_alpha_search_v1`, for Codex Alpha Search: a
//! Codex ChatGPT sign-in, or a Codex API key with `alpha-search: true` (its
//! `codex_alpha_search` attribute).
//!
//! A call whose metadata disallows free credentials (`disallow_free_auth`,
//! which the images endpoints set for Codex) never picks a Codex credential
//! whose `plan_type` attribute is `free`, in either pick or when deciding
//! on another retry round.
//!
//! Deviations from upstream:
//! - The free-plan rule reads a typed metadata flag, where upstream also
//!   takes the strings and bytes `strconv.ParseBool` reads as true.
//! - A policy is a type, not a name, so there is no invalid policy and no
//!   `invalid_credential_policy` error.
//! - The pick is the built-in strategies' only: custom selectors, the plugin
//!   scheduler, session affinity, required auth kinds, Home dispatch and the
//!   selection log aren't ported.
//! - [`Manager::resolve_execution_model`] has no Home dispatcher model to
//!   prefer.

use std::sync::Arc;

use super::Manager;
use super::credential::attribute;
use super::execute::next_model_pool_offset;
use super::models::Resolver;
use super::select::{
    Picked, Selection, SelectorState, canonical_scheduling_provider, executor_key_from_auth,
    lookup_executor,
};
use super::text::{equal_fold, parse_suffix};
use crate::auth::classification::ATTRIBUTE_CODEX_ALPHA_SEARCH;
use crate::auth::{Auth, AuthKind};
use crate::exec::{ErrorKind, ExecError, Options};

/// Which credentials a call may pick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialPolicy {
    /// Codex Alpha Search (`codex_alpha_search_v1`): Codex OAuth credentials,
    /// and Codex API keys that opted in.
    CodexAlphaSearchV1,
}

impl CredentialPolicy {
    /// Whether the policy lets a call pick `auth` (`credentialPolicyAllows`).
    pub(crate) fn allows(self, auth: &Auth) -> bool {
        match self {
            Self::CodexAlphaSearchV1 => {
                if !equal_fold(auth.provider.trim(), "codex") {
                    return false;
                }
                match auth.auth_kind() {
                    Some(AuthKind::OAuth) => true,
                    Some(AuthKind::ApiKey) => {
                        equal_fold(&attribute(auth, ATTRIBUTE_CODEX_ALPHA_SEARCH), "true")
                    }
                    None => false,
                }
            }
        }
    }
}

/// What narrows the credentials a call may pick, besides a credential
/// policy (upstream's `authSelectionEligibility` without its required kind
/// and policy: required kinds aren't ported, and a policy has its own pick).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Eligibility {
    /// Leave out Codex credentials on the free plan.
    pub(crate) disallow_free_auth: bool,
}

impl Eligibility {
    /// The eligibility a call's options ask for
    /// (`authSelectionEligibilityForRequest`).
    pub(crate) fn for_request(options: &Options) -> Self {
        Self {
            disallow_free_auth: options.metadata.disallow_free_auth,
        }
    }

    /// Whether the call may pick `auth` (`allows`).
    pub(crate) fn allows(self, auth: &Auth) -> bool {
        !self.disallow_free_auth || !is_free_codex_auth(auth)
    }
}

/// Whether `auth` is a Codex credential on the free plan
/// (`isFreeCodexAuth`).
pub(crate) fn is_free_codex_auth(auth: &Auth) -> bool {
    equal_fold(auth.provider.trim(), "codex") && equal_fold(&attribute(auth, "plan_type"), "free")
}

impl Selection<'_> {
    /// Picks the next credential of `provider` for `model` that `policy`
    /// allows (upstream's `pickNextLegacy` with the policy as its
    /// eligibility filter).
    pub(super) fn pick_next_with_policy(
        &self,
        state: &mut SelectorState,
        provider: &str,
        model: &str,
        policy: CredentialPolicy,
    ) -> Result<Picked, ExecError> {
        let Some(executor) = lookup_executor(self.executors, provider) else {
            return Err(ExecError::new(
                ErrorKind::ExecutorNotFound,
                "executor not registered",
            ));
        };
        let mut model_key = model.trim();
        if !model_key.is_empty() {
            let base = parse_suffix(model_key).0;
            if !base.is_empty() {
                model_key = base.trim();
            }
        }
        let target = canonical_scheduling_provider(provider);
        let candidates: Vec<&Arc<Auth>> = self
            .auths
            .values()
            .map(|entry| &entry.auth)
            .filter(|auth| {
                !auth.disabled
                    && canonical_scheduling_provider(&executor_key_from_auth(auth)) == target
                    && policy.allows(auth)
                    && (model_key.is_empty() || self.auth_supports_route_model(auth, model))
            })
            .collect();
        if candidates.is_empty() {
            return Err(ExecError::auth_not_found());
        }
        let available = self.available_auths_for_route_model(&candidates, provider, model)?;
        let selected = self.pick_legacy(state, &available, provider, model)?;
        Ok(Picked {
            auth: Arc::clone(selected),
            executor,
            provider: provider.to_owned(),
        })
    }
}

impl Manager {
    /// Picks a credential of `provider` for `model` that `policy` allows,
    /// without calling it or recording anything about it (upstream's
    /// `SelectAuthWithCredentialPolicy`).
    pub(crate) fn select_auth_with_credential_policy(
        &self,
        provider: &str,
        model: &str,
        policy: CredentialPolicy,
    ) -> Result<Picked, ExecError> {
        let now = self.now();
        let mut guard = self.lock();
        let state = &mut *guard;
        let settings = state.settings.clone();
        let oauth = state.oauth.clone();
        let selection = Selection {
            auths: &state.auths,
            executors: &state.executors,
            models: self.models(),
            resolver: Resolver {
                settings: &settings,
                oauth: &oauth,
            },
            strategy: settings.routing_strategy,
            now,
        };
        let picked =
            selection.pick_next_with_policy(&mut state.selector, provider, model, policy)?;
        if !policy.allows(&picked.auth) {
            return Err(ExecError::new(
                ErrorKind::AuthNotFound,
                "selector returned no eligible auth",
            ));
        }
        Ok(picked)
    }

    /// The upstream model a call for `route_model` sends with `auth`: the
    /// route model without the credential's prefix, through its aliases,
    /// and the next of its model pool (upstream's `ResolveExecutionModel`).
    /// It is the trimmed route model when nothing resolves.
    pub fn resolve_execution_model(&self, auth: &Auth, route_model: &str) -> String {
        let route_model = route_model.trim();
        let mut guard = self.lock();
        let state = &mut *guard;
        let settings = state.settings.clone();
        let oauth = state.oauth.clone();
        let resolver = Resolver {
            settings: &settings,
            oauth: &oauth,
        };
        let offsets = &mut state.pool_offsets;
        let candidates = resolver.execution_model_candidates(auth, route_model, |key, size| {
            next_model_pool_offset(offsets, key, size)
        });
        match candidates.first().map(|model| model.trim()) {
            Some(model) if !model.is_empty() => model.to_owned(),
            _ => route_model.to_owned(),
        }
    }
}
