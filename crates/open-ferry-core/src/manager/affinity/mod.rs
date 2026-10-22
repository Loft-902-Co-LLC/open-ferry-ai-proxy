// Ported from CLIProxyAPI sdk/cliproxy/auth/selector.go
// (SessionAffinitySelector, NewSessionAffinitySelectorWithConfig, Pick,
// OnResult, InvalidateAuth and highestPriorityAuths) and
// sdk/cliproxy/service_config.go (newRoutingSelector) (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Session affinity (`routing.session-affinity`): a conversation stays on
//! the credential that served it, so its prompt cache stays warm.
//!
//! A call's session (see [`Session`]) is bound to the credential its first
//! call picked, for its provider scope and model. Later calls of the session
//! take that credential while it is ready, whatever its priority; when it
//! isn't, the routing strategy picks again among the highest priority ready
//! credentials and the session is bound to the new one. With
//! `routing.session-affinity-rebind-on: quota`, transient failures keep the
//! original binding while this call fails over; quota, auth, model-support
//! and permanent unavailability move it. A success keeps the binding alive
//! for another TTL. A call with no session is picked as without affinity.
//!
//! A session may fall back to another (its parent, or the conversation of a
//! prompt cache key): when the session itself isn't bound, it takes the
//! credential its fallback is bound to, and the two are bound together. A
//! subagent's session takes its parent's credential only with
//! `routing.session-affinity-subagents` (the default), and is bound alone;
//! so is a fork. Ordinary subagents inherit within their own provider/model
//! scope. Guardian reviews look up the parent's newest live selection among
//! real providers available to the review, across model scopes, then inherit
//! only if that selected credential is eligible; they do not search older
//! selections after an ineligible result, or refresh the parent.
//!
//! Deviations from upstream:
//! - The bindings are the cache's (see `cache`): hashed keys, a sweep on use
//!   instead of a ticker, and a cap of 65536 keys.
//! - The LCP matcher isn't ported (see `keys`), nor the session tree store.
//! - Nothing is written into the call's metadata, and nothing is logged:
//!   upstream logs each binding with the start of the session ID.
//! - `LookupAffinity` and `Stop` aren't ported: only the plugin host calls
//!   the one, and the cache has no goroutine for the other.

mod cache;
mod keys;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use super::classify::{
    is_cloudflare_challenge_result_error, is_invalid_grant_result_error,
    is_model_support_result_error, should_skip_credential_cooldown,
};
use super::cooldown::CallResult;
use super::credential::priority;
use super::select::{canonical_scheduling_provider, executor_key_from_auth};
use super::settings::{RoutingState, Settings};
use super::text::canonical_model_key;
use crate::auth::{Auth, AuthError, Timestamp};
use crate::exec::ExecError;

use cache::SessionCache;
pub(crate) use keys::Session;
use keys::is_subagent_session;

#[cfg(test)]
mod tests;

/// The bindings, and whether subagents may take their parent's credential
/// (upstream's `SessionAffinitySelector` without its fallback selector,
/// which is the manager's strategy).
#[derive(Debug)]
pub(crate) struct Affinity {
    cache: SessionCache,
    subagents: bool,
    rebind_on_quota_only: bool,
}

impl Affinity {
    /// Affinity whose bindings live for `ttl`, or an hour for none
    /// (upstream's `NewSessionAffinitySelectorWithConfig`).
    pub(crate) fn new(ttl: Duration, subagents: bool) -> Self {
        let ttl = if ttl.is_zero() {
            Duration::from_secs(60 * 60)
        } else {
            ttl
        };
        Self {
            cache: SessionCache::new(ttl),
            subagents,
            rebind_on_quota_only: false,
        }
    }

    /// Affinity with a smaller cache for selector-level eviction tests.
    #[cfg(test)]
    pub(crate) fn with_capacity(ttl: Duration, subagents: bool, max_entries: usize) -> Self {
        let ttl = if ttl.is_zero() {
            Duration::from_secs(60 * 60)
        } else {
            ttl
        };
        Self {
            cache: SessionCache::with_capacity(ttl, max_entries),
            subagents,
            rebind_on_quota_only: false,
        }
    }

    /// Whether transient failovers preserve the binding.
    pub(super) fn rebind_on_quota_only(&self) -> bool {
        self.rebind_on_quota_only
    }

    /// The affinity `settings` turn on, if they do.
    pub(crate) fn for_settings(settings: &Settings) -> Option<Self> {
        let state = RoutingState::of(settings);
        state.session_affinity.then(|| {
            let mut affinity =
                Self::new(state.session_affinity_ttl, state.session_affinity_subagents);
            affinity.rebind_on_quota_only = state.session_affinity_rebind_on == "quota";
            affinity
        })
    }

    /// Picks among the `available` credentials (every priority, by ID) for a
    /// call of `session` in `scope` (the provider or `mixed`) for `model`:
    /// the bound credential when it is available, else `fallback`'s pick
    /// among the highest priority ones, which is then bound (upstream's
    /// `SessionAffinitySelector.Pick`).
    pub(crate) fn pick<'c, F>(
        &mut self,
        scope: &str,
        model: &str,
        session: Option<&Session>,
        candidates: (&[&'c Arc<Auth>], &HashSet<String>),
        now: Timestamp,
        mut fallback: F,
    ) -> Result<&'c Arc<Auth>, ExecError>
    where
        F: FnMut(&[&'c Arc<Auth>]) -> Result<&'c Arc<Auth>, ExecError>,
    {
        let (available, transient_auth_ids) = candidates;
        let tier = highest_priority_auths(available);
        let Some(session) = session else {
            return fallback(&tier);
        };
        let model_key = canonical_model_key(model);
        let cache_key = format!("{scope}::{}::{model_key}", session.primary);
        let is_fork = session.is_fork;
        let is_subagent = !is_fork && is_subagent_session(&session.primary, &session.fallback);
        let fallback_key = (!session.fallback.is_empty() && session.fallback != session.primary)
            .then(|| format!("{scope}::{}::{model_key}", session.fallback));
        let alias = fallback_key.as_deref().filter(|_| !is_subagent && !is_fork);
        let find = |id: &str| available.iter().copied().find(|auth| auth.id == id);
        // Only a real pick changes selection order. Preserved bindings can
        // gain aliases during failover without becoming a new selection.
        let bind = |cache: &mut SessionCache, id: &str, selected: Option<&Auth>| {
            let mut keys = vec![cache_key.as_str()];
            keys.extend(alias);
            if let Some(auth) = selected {
                let provider = scheduling_provider(auth, scope);
                cache.set_session_aliases(id, &provider, &session.primary, &keys, now);
            } else {
                cache.set_aliases(id, &keys, now);
            }
        };

        if let Some(id) = self.cache.get_and_refresh(&cache_key, now) {
            if let Some(auth) = find(&id) {
                bind(&mut self.cache, &auth.id, Some(auth));
                return Ok(auth);
            }
            let auth = fallback(&tier)?;
            if self.rebind_on_quota_only && transient_auth_ids.contains(&id) {
                bind(&mut self.cache, &id, None);
            } else {
                bind(&mut self.cache, &auth.id, Some(auth));
            }
            return Ok(auth);
        }
        if is_subagent && self.subagents && session.is_guardian {
            let scope_provider = canonical_scheduling_provider(scope);
            let providers = available
                .iter()
                .map(|auth| scheduling_provider(auth, scope))
                .filter(|provider| scope_provider == "mixed" || *provider == scope_provider)
                .collect();
            if let Some(id) = self
                .cache
                .latest_session_auth(&session.fallback, &providers, now)
                && let Some(auth) = find(&id)
            {
                bind(&mut self.cache, &auth.id, Some(auth));
                return Ok(auth);
            }
        }
        if let Some(fallback_key) = &fallback_key
            && let Some(id) = self.cache.get(fallback_key, now)
        {
            if let Some(auth) = find(&id)
                && (!is_subagent || self.subagents)
            {
                bind(&mut self.cache, &auth.id, Some(auth));
                return Ok(auth);
            }
            if !is_subagent
                && !is_fork
                && self.rebind_on_quota_only
                && transient_auth_ids.contains(&id)
            {
                let auth = fallback(&tier)?;
                bind(&mut self.cache, &id, None);
                return Ok(auth);
            }
        }
        let auth = fallback(&tier)?;
        bind(&mut self.cache, &auth.id, Some(auth));
        Ok(auth)
    }

    /// Records a call's outcome for `session` in `scope`: a success refreshes
    /// its bindings to the credential, a failure the credential is to blame
    /// for drops them (upstream's `SessionAffinitySelector.OnResult`).
    pub(crate) fn on_result(
        &mut self,
        session: &Session,
        result: &CallResult,
        scope: &str,
        now: Timestamp,
    ) {
        if result.auth_id.is_empty() {
            return;
        }
        if should_skip_credential_cooldown(result.error.as_ref()) {
            // A request-scoped or caller-attributed failure says nothing
            // about the credential, so the bindings stay.
            return;
        }
        if !result.success
            && self.rebind_on_quota_only
            && !should_rebind_on_error(result.error.as_ref())
        {
            return;
        }
        let model = if result.route_model.is_empty() {
            &result.model
        } else {
            &result.route_model
        };
        let model_key = canonical_model_key(model);
        let cache_key = format!("{scope}::{}::{model_key}", session.primary);
        let fallback_key = (!session.fallback.is_empty()
            && session.fallback != session.primary
            && !is_subagent_session(&session.primary, &session.fallback))
        .then(|| format!("{scope}::{}::{model_key}", session.fallback));
        let keys = std::iter::once(cache_key.as_str()).chain(fallback_key.as_deref());
        for key in keys {
            if result.success {
                self.cache.touch(key, &result.auth_id, now);
            } else {
                self.cache.compare_and_delete(key, &result.auth_id, now);
            }
        }
    }

    /// Drops every binding to credential `id`, which is gone (upstream's
    /// `InvalidateAuth`).
    pub(crate) fn invalidate_auth(&mut self, id: &str) {
        self.cache.invalidate_auth(id);
    }

    /// The credential cache key `key` is bound to, without refreshing it.
    #[cfg(test)]
    pub(crate) fn bound(&mut self, key: &str, now: Timestamp) -> Option<String> {
        self.cache.get(key, now)
    }
}

/// The selected credential's real scheduling provider, including named
/// compatibility channels, or the route's scope for an unnamed provider.
fn scheduling_provider(auth: &Auth, scope: &str) -> String {
    let provider = canonical_scheduling_provider(&executor_key_from_auth(auth));
    if provider.is_empty() {
        canonical_scheduling_provider(scope)
    } else {
        provider
    }
}

/// Whether an error unbinds a quota-only session (upstream's
/// `shouldRebindSessionAffinityOnError`).
pub(super) fn should_rebind_on_error(error: Option<&AuthError>) -> bool {
    let Some(error) = error else {
        return false;
    };
    !should_skip_credential_cooldown(Some(error))
        && (is_model_support_result_error(error)
            || is_cloudflare_challenge_result_error(error)
            || is_invalid_grant_result_error(error)
            || matches!(error.http_status, 401 | 402 | 403 | 404 | 429))
}

/// `auths` narrowed to their highest priority, in their order (upstream's
/// `highestPriorityAuths`).
pub(crate) fn highest_priority_auths<'c>(auths: &[&'c Arc<Auth>]) -> Vec<&'c Arc<Auth>> {
    let Some(best) = auths.iter().map(|auth| priority(auth)).max() else {
        return Vec::new();
    };
    auths
        .iter()
        .copied()
        .filter(|auth| priority(auth) == best)
        .collect()
}
