// Ported from CLIProxyAPI sdk/cliproxy/auth/selector.go
// (SessionAffinitySelector, NewSessionAffinitySelectorWithConfig, Pick,
// OnResult, InvalidateAuth and highestPriorityAuths) and
// sdk/cliproxy/service_config.go (newRoutingSelector) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Session affinity (`routing.session-affinity`): a conversation stays on
//! the credential that served it, so its prompt cache stays warm.
//!
//! A call's session (see [`Session`]) is bound to the credential its first
//! call picked, for its provider scope and model. Later calls of the session
//! take that credential while it is ready, whatever its priority; when it
//! isn't, the routing strategy picks again among the highest priority ready
//! credentials and the session is bound to the new one. A failure the
//! credential is to blame for unbinds it; a success keeps the binding alive
//! for another TTL. A call with no session is picked as without affinity.
//!
//! A session may fall back to another (its parent, or the conversation of a
//! prompt cache key): when the session itself isn't bound, it takes the
//! credential its fallback is bound to, and the two are bound together. A
//! subagent's session takes its parent's credential only with
//! `routing.session-affinity-subagents` (the default), and is bound alone;
//! so is a fork.
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

use std::sync::Arc;
use std::time::Duration;

use super::classify::should_skip_credential_cooldown;
use super::cooldown::CallResult;
use super::credential::priority;
use super::settings::{RoutingState, Settings};
use super::text::canonical_model_key;
use crate::auth::{Auth, Timestamp};
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
        }
    }

    /// The affinity `settings` turn on, if they do.
    pub(crate) fn for_settings(settings: &Settings) -> Option<Self> {
        let state = RoutingState::of(settings);
        state
            .session_affinity
            .then(|| Self::new(state.session_affinity_ttl, state.session_affinity_subagents))
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
        available: &[&'c Arc<Auth>],
        now: Timestamp,
        mut fallback: F,
    ) -> Result<&'c Arc<Auth>, ExecError>
    where
        F: FnMut(&[&'c Arc<Auth>]) -> Result<&'c Arc<Auth>, ExecError>,
    {
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

        if let Some(id) = self.cache.get_and_refresh(&cache_key, now) {
            let auth = match find(&id) {
                Some(auth) => auth,
                // The bound credential isn't available: pick again, for an
                // even spread.
                None => fallback(&tier)?,
            };
            self.bind(&cache_key, alias, &auth.id, now);
            return Ok(auth);
        }
        if let Some(fallback_key) = &fallback_key
            && let Some(id) = self.cache.get(fallback_key, now)
            && let Some(auth) = find(&id)
            && (!is_subagent || self.subagents)
        {
            self.bind(&cache_key, alias, &auth.id, now);
            return Ok(auth);
        }
        let auth = fallback(&tier)?;
        self.bind(&cache_key, alias, &auth.id, now);
        Ok(auth)
    }

    /// Binds `cache_key`, with `alias` when the two are one session, to
    /// credential `auth` (upstream's `bind` in `Pick`).
    fn bind(&mut self, cache_key: &str, alias: Option<&str>, auth: &str, now: Timestamp) {
        match alias {
            Some(alias) => self.cache.set_aliases(auth, &[cache_key, alias], now),
            None => self.cache.set(cache_key, auth, now),
        }
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
