// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_auth.go
// (resolveCodexKeyConfig, resolveCodexModelIsCompat),
// codex_executor_request.go (translateCodexRequestPairWithUpdateIntent), the
// translation of codex_executor_tokens.go,
// helps/codex_multi_agent_v2.go (OptimizeCodexMultiAgentV2RequestForAuth,
// TranslateRequestWithAPIKeyModelCompatibilityForExecutor,
// TranslateRequestWithCodexMultiAgentV2ForExecutor),
// helps/payload_helpers.go (isCodexTargetExecutor),
// internal/client/codex/optimize-multi-agent-v2/optimize_multi_agent_v2.go
// (RewriteCodexOrphanDelegationInputForConfig,
// TranslateRequestEnvelopeWithCodexMultiAgentV2), and
// sdk/cliproxy/auth/api_key_model_capabilities.go (CodexAPIKeyModelIsCompat)
// with conductor_models.go (resolveAPIKeyConfig) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex clients' requests, and compatibility models, around translation.
//!
//! A credential's `codex-api-key` entry can mark a model `is-compat`: a
//! third-party endpoint that speaks Codex's Responses dialect but not its
//! multi-agent extensions or Claude's empty thinking blocks. [`is_compat`]
//! finds that flag; for such a model a Claude request is translated with
//! [`convert_claude_request_to_codex_with_compat`], reasoning items are
//! cleaned for it, and `agent_message` items become user messages without
//! their routing metadata.
//!
//! [`prepare`] runs first among the Codex executor's own rewrites
//! (`OptimizeCodexMultiAgentV2RequestForAuth`): orphan delegation outputs
//! become user messages (`codex.orphan-delegation-compatibility`), an
//! official Codex client's multi-agent v2 request is optimized
//! (`client.codex.optimize-multi-agent-v2`), with the models the executor
//! knows listed for `spawn_agent`
//! ([`open_ferry_core::codex_models::spawn_agent`]), and a compatibility
//! model's `agent_message` items are rewritten.
//!
//! [`before_translation`] is what the other executors do to a request before
//! they translate it (`TranslateRequestWithCodexMultiAgentV2ForExecutor`): a
//! Codex client's own tools get integer parameter types, and a Responses
//! request has its orphan delegation outputs and, unless it stays in a
//! Responses format, its `agent_message` items rewritten.
//! [`crate::payload::apply`] gives the tools integer types again in the
//! translated body of a call or a stream, but not of a token count.
//!
//! Deviations from upstream:
//! - The credential manager doesn't bind the model it resolved to the call,
//!   so [`is_compat`] resolves it again, from the config and the route model
//!   in the call's metadata, as the manager would have bound it. When
//!   nothing resolves, upstream reads the credential's entry, except for
//!   token counts, which then take no model as a compatibility model; here
//!   token counts read the entry too. The Home service's credential options
//!   aren't ported.
//! - The other executors take the path upstream takes for models that
//!   aren't compatibility models, as they don't resolve the flag.
//! - The translator plugin hooks aren't ported, so a compatibility
//!   translation isn't passed to them.
//! - Upstream v8.0.10 keeps a v8 document's
//!   `oauth.providers.codex.orphan-delegation-compatibility` from API key
//!   credentials. The config here, as v8.0.11's, applies it to every
//!   credential, so API keys get orphan delegation compatibility too.
//! - An executor without a model catalog lists no models for `spawn_agent`,
//!   which leaves its description as it is.

use http::HeaderMap;
use http::header::{self, HeaderValue};
use open_ferry_core::auth::{Auth, AuthKind, AuthSource};
use open_ferry_core::codex_models::spawn_agent::spawn_agent_model_list;
use open_ferry_core::config::{CodexKey, Config};
use open_ferry_core::exec::{Format, Options, Request};
use open_ferry_translate::codex::claude::convert_claude_request_to_codex_with_compat;
use open_ferry_translate::codex_client::{
    header_value, multi_agent_v2, orphan_delegation, tool_integers,
};
use open_ferry_translate::go;
use open_ferry_translate::models::ModelCatalog as Catalog;
use open_ferry_translate::registry::Registry;
use open_ferry_translate::thinking::summary;
use serde_json::Value;

use super::request::{Context, Kind, base_model};
use crate::json::eq_fold;

/// The first non-blank value of the client's header `name`, trimmed.
fn header(headers: &HeaderMap, name: &str) -> String {
    header_value(headers.get_all(name).iter().map(HeaderValue::as_bytes))
}

/// Whether the model of `request` is one of the credential's compatibility
/// models (`resolveCodexModelIsCompat`): as the credential manager resolved
/// the model of the call ([`resolved_compat`]), for a call it made, which
/// carries the client's route model as `requested_model`, or, when it picked
/// the credential by another model and restored the client's, the model of
/// `request`; else by the models of the credential's `codex-api-key` entry,
/// matching the model with or without its thinking suffix by name or alias,
/// regardless of case.
pub(crate) fn is_compat(context: Context<'_>, request: &Request, options: &Options) -> bool {
    let (Some(config), Some(auth)) = (context.config, context.auth) else {
        return false;
    };
    let selection = options
        .metadata
        .auth_selection_model
        .as_deref()
        .map_or("", str::trim);
    let route = if selection.is_empty() || selection == request.model.trim() {
        options.metadata.requested_model.trim()
    } else {
        request.model.trim()
    };
    if !route.is_empty()
        && let Some(compat) = resolved_compat(config, auth, route, &request.model)
    {
        return compat;
    }
    let base = base_model(&request.model);
    if let Some(entry) = codex_key_config(config, auth)
        && !entry.models.is_empty()
    {
        let requested = request.model.trim();
        let target = base.trim();
        let named = |name: &str, alias: &str, model: &str| {
            !model.is_empty() && (eq_fold(name, model) || eq_fold(alias, model))
        };
        return entry
            .models
            .iter()
            .find(|model| {
                let (name, alias) = (model.name.trim(), model.alias.trim());
                named(name, alias, target) || named(name, alias, requested)
            })
            .is_some_and(|model| model.is_compat);
    }
    api_key_model_is_compat(config, auth, base)
        || api_key_model_is_compat(config, auth, &request.model)
}

/// The compatibility flag the credential manager binds to a call of a
/// `codex` credential (`attachResolvedAPIKeyModelInfo`, read back by
/// `ResolvedModelInfo`), if it binds one: that of the configured model the
/// client's `route_model` was resolved through
/// (`lookupAPIKeyModelCapability`), else, for an API key whose entry has its
/// key and base URL, that of the configured model named `upstream_model`,
/// or none (`lookupUnlistedCodexAPIKeyModelCapability`). Upstream also binds
/// an OAuth credential's model from the static Codex models, which are
/// never compatibility models, as the config gives for such a credential.
fn resolved_compat(
    config: &Config,
    auth: &Auth,
    route_model: &str,
    upstream_model: &str,
) -> Option<bool> {
    if !eq_fold(auth.provider.trim(), "codex") {
        return None;
    }
    let entry = api_key_config(&config.codex_api_key, auth)?;
    if configured_model_routing(auth)
        && let Some(compat) = route_compat(entry, auth, route_model, upstream_model)
    {
        return Some(compat);
    }
    unlisted_compat(entry, auth, upstream_model)
}

/// Whether the credential's models come from the config: an API key, or a
/// config-made OpenAI-compatible entry (`isConfiguredModelRoutingAuth`).
fn configured_model_routing(auth: &Auth) -> bool {
    auth.auth_kind() == Some(AuthKind::ApiKey)
        || (auth.auth_source_kind() == Some(AuthSource::Config)
            && !attribute(auth, "compat_name").is_empty())
}

/// The compatibility flag of the entry model that `route_model` routes to
/// `upstream_model` through (`lookupAPIKeyModelCapability` over the table
/// `compileAPIKeyModelCapabilitiesForAuth` makes): the models whose alias
/// or name, with or without a thinking suffix, is the route model without
/// the credential's `prefix/`, or failing that its base, listed once for
/// each name; of those, the first named `upstream_model`, else the first
/// whose name has no suffix and is the upstream model without its suffix.
fn route_compat(
    entry: &CodexKey,
    auth: &Auth,
    route_model: &str,
    upstream_model: &str,
) -> Option<bool> {
    let route = route_model.trim();
    let prefix = auth.prefix.trim();
    let route = match route.strip_prefix(prefix) {
        Some(rest) if !prefix.is_empty() => rest.strip_prefix('/').unwrap_or(route),
        _ => route,
    };
    let mut routes: Vec<(&str, bool)> = Vec::new();
    for candidate in lookup_candidates(route) {
        let key = go::to_lower(candidate.trim());
        let start = routes.len();
        for configured in &entry.models {
            let (mut name, mut alias) = (configured.name.trim(), configured.alias.trim());
            if name.is_empty() {
                name = alias;
            }
            if alias.is_empty() {
                alias = name;
            }
            if name.is_empty() {
                continue;
            }
            let routed = [alias, name]
                .into_iter()
                .flat_map(lookup_candidates)
                .any(|known| go::to_lower(known.trim()) == key);
            let listed = routes
                .get(start..)
                .unwrap_or_default()
                .iter()
                .any(|(upstream, _)| eq_fold(upstream, name));
            if routed && !listed {
                routes.push((name, configured.is_compat));
            }
        }
    }
    let selected = upstream_model.trim();
    routes
        .iter()
        .find(|(upstream, _)| eq_fold(upstream, selected))
        .or_else(|| {
            routes
                .iter()
                .find(|(upstream, _)| fallback_matches(upstream, selected))
        })
        .map(|(_, compat)| *compat)
}

/// The compatibility flag of a `codex` API key's model that its entry may
/// not route to by name (`lookupUnlistedCodexAPIKeyModelCapability`): when
/// the credential has the entry's key, and its base URL if the entry has
/// one, that of the first model named `upstream_model`, or with no suffix
/// and named the upstream model without its suffix; else none.
fn unlisted_compat(entry: &CodexKey, auth: &Auth, upstream_model: &str) -> Option<bool> {
    let upstream = upstream_model.trim();
    if auth.auth_kind() != Some(AuthKind::ApiKey) || upstream.is_empty() {
        return None;
    }
    let (key, base) = (attribute(auth, "api_key"), attribute(auth, "base_url"));
    let entry_base = entry.base_url.trim();
    if (key.is_empty() && base.is_empty())
        || !eq_fold(key, entry.api_key.trim())
        || (!entry_base.is_empty() && !eq_fold(base, entry_base))
    {
        return None;
    }
    let configured = entry.models.iter().find(|configured| {
        eq_fold(configured.name.trim(), upstream) || fallback_matches(&configured.name, upstream)
    });
    Some(configured.is_some_and(|configured| configured.is_compat))
}

/// The model and, when it has a thinking suffix, the model without it
/// (`modelAliasLookupCandidates`).
fn lookup_candidates(model: &str) -> Vec<&str> {
    let model = model.trim();
    if model.is_empty() {
        return Vec::new();
    }
    match base_model(model) {
        "" => vec![model],
        base if base == model => vec![model],
        base => vec![model, base],
    }
}

/// Whether a configured model with no thinking suffix is `selected`
/// without its suffix (`configuredUpstreamFallbackMatches`).
fn fallback_matches(configured: &str, selected: &str) -> bool {
    let configured = configured.trim();
    if base_model(configured) != configured {
        return false;
    }
    eq_fold(configured, base_model(selected.trim()).trim())
}

/// The trimmed attribute `key` of `auth`.
fn attribute<'a>(auth: &'a Auth, key: &str) -> &'a str {
    auth.attribute(key).unwrap_or_default().trim()
}

/// The credential's `config_index` attribute, when it is an index.
fn config_index(auth: &Auth) -> Option<usize> {
    let index = attribute(auth, "config_index").parse::<i64>().ok()?;
    usize::try_from(index).ok()
}

/// The `codex-api-key` entry of a credential, as the Codex executor finds it
/// (`resolveCodexKeyConfig`): its `config_index` entry unless its key or
/// base URL differs, else the first with its key and base URL, or with its
/// key and none, or for a credential without a key with its base URL; else
/// the first with its key. Keys and URLs compare trimmed and regardless of
/// case.
fn codex_key_config<'c>(config: &'c Config, auth: &Auth) -> Option<&'c CodexKey> {
    let entries = &config.codex_api_key;
    let (key, base) = (attribute(auth, "api_key"), attribute(auth, "base_url"));
    if let Some(entry) = config_index(auth).and_then(|index| entries.get(index)) {
        let (entry_key, entry_base) = (entry.api_key.trim(), entry.base_url.trim());
        if (key.is_empty() || eq_fold(entry_key, key))
            && (base.is_empty() || eq_fold(entry_base, base))
        {
            return Some(entry);
        }
    }
    for entry in entries {
        let (entry_key, entry_base) = (entry.api_key.trim(), entry.base_url.trim());
        if !key.is_empty() && !base.is_empty() {
            if eq_fold(entry_key, key) && eq_fold(entry_base, base) {
                return Some(entry);
            }
            continue;
        }
        if !key.is_empty()
            && eq_fold(entry_key, key)
            && (entry_base.is_empty() || eq_fold(entry_base, base))
        {
            return Some(entry);
        }
        if key.is_empty() && !base.is_empty() && eq_fold(entry_base, base) {
            return Some(entry);
        }
    }
    if key.is_empty() {
        return None;
    }
    entries
        .iter()
        .find(|entry| eq_fold(entry.api_key.trim(), key))
}

/// The `codex-api-key` entry of a credential, as the credential manager
/// finds it (`resolveAPIKeyConfig`, which this crate can't reach): for a
/// credential from the config file, its `config_index` entry if its key and
/// base URL match; else the first that matches with the credential's prefix
/// and proxy; else the first that matches; else the first with its key.
fn api_key_config<'c>(entries: &'c [CodexKey], auth: &Auth) -> Option<&'c CodexKey> {
    let (key, base) = (attribute(auth, "api_key"), attribute(auth, "base_url"));
    let matches = |entry: &CodexKey| {
        let (entry_key, entry_base) = (entry.api_key.trim(), entry.base_url.trim());
        if !key.is_empty() && !base.is_empty() {
            return eq_fold(entry_key, key) && eq_fold(entry_base, base);
        }
        if !key.is_empty() {
            return eq_fold(entry_key, key) && (entry_base.is_empty() || eq_fold(entry_base, base));
        }
        !base.is_empty() && eq_fold(entry_base, base)
    };
    if auth.auth_source_kind() == Some(AuthSource::Config)
        && let Some(entry) = config_index(auth).and_then(|index| entries.get(index))
        && matches(entry)
    {
        return Some(entry);
    }
    entries
        .iter()
        .find(|entry| {
            matches(entry)
                && eq_fold(entry.prefix.trim(), auth.prefix.trim())
                && eq_fold(entry.proxy_url.trim(), auth.proxy_url.trim())
        })
        .or_else(|| entries.iter().find(|entry| matches(entry)))
        .or_else(|| {
            (!key.is_empty())
                .then(|| {
                    entries
                        .iter()
                        .find(|entry| eq_fold(entry.api_key.trim(), key))
                })
                .flatten()
        })
}

/// Whether `model` is a compatibility model of a `codex` credential's
/// `codex-api-key` entry as the credential manager finds it
/// (`CodexAPIKeyModelIsCompat`): by name or alias, each standing in for the
/// other when empty, with or without the thinking suffix.
fn api_key_model_is_compat(config: &Config, auth: &Auth, model: &str) -> bool {
    if !eq_fold(auth.provider.trim(), "codex") {
        return false;
    }
    let Some(entry) = api_key_config(&config.codex_api_key, auth) else {
        return false;
    };
    let requested = model.trim();
    if entry.models.is_empty() || requested.is_empty() {
        return false;
    }
    let mut base = base_model(requested).trim();
    if base.is_empty() {
        base = requested;
    }
    for configured in &entry.models {
        let (mut name, mut alias) = (configured.name.trim(), configured.alias.trim());
        if name.is_empty() {
            name = alias;
        }
        if alias.is_empty() {
            alias = name;
        }
        if name.is_empty() {
            continue;
        }
        if [name, alias]
            .iter()
            .any(|known| eq_fold(known, requested) || eq_fold(known, base))
        {
            return configured.is_compat;
        }
    }
    false
}

/// Translates the client's payload to `to` for the Codex executor
/// (`translateCodexRequestPairWithUpdateIntent`, and for token counts
/// `TranslateRequestWithAPIKeyModelCompatibilityAndUpdateIntentForExecutor`).
/// A Claude request to a compatibility model goes through the
/// compatibility translator. A token count of a Responses request also has
/// its orphan delegation outputs rewritten first, as upstream's counting
/// path does.
pub(crate) fn translate(
    kind: Kind,
    context: Context<'_>,
    request: &Request,
    options: &Options,
    to: &Format,
    stream: bool,
    mut payload: Value,
) -> Value {
    let base = base_model(&request.model);
    let source = &options.source_format;
    if kind == Kind::CountTokens
        && *source == Format::OPENAI_RESPONSE
        && context
            .config
            .is_some_and(|config| config.codex.orphan_delegation_compatibility)
    {
        rewrite_orphans(&mut payload, &options.headers);
    }
    if *source == Format::CLAUDE && *to == Format::CODEX && is_compat(context, request, options) {
        let summary = summary::extract_translated(&payload, source.as_str(), to.as_str());
        let mut body = convert_claude_request_to_codex_with_compat(base, &payload);
        summary::apply_for_model(&mut body, to.as_str(), base, summary, Catalog::embedded());
        return body;
    }
    Registry::global().translate_request(source, to, base, payload, stream)
}

/// Turns orphan delegation outputs into user messages when the client says
/// it is a spawned sub-agent.
fn rewrite_orphans(body: &mut Value, headers: &HeaderMap) {
    let subagent = header(headers, orphan_delegation::SUBAGENT_HEADER);
    orphan_delegation::rewrite(body, &subagent, true);
}

/// Rewrites a prepared Codex body for a Codex client's multi-agent requests
/// (`OptimizeCodexMultiAgentV2RequestForAuth`): orphan delegation outputs,
/// then the multi-agent v2 optimization, then a compatibility model's
/// `agent_message` items. Returns whether the collaboration namespace was
/// renamed, so that responses must be restored.
///
/// Upstream reads the config through `cfg.ForAPIKey()` for an API key
/// credential. That resets none of the settings read here: this config, as
/// v8.0.11's, shares `codex.orphan-delegation-compatibility` with API keys
/// however a v8 document spells it, and the multi-agent switch is a client
/// setting.
pub(crate) fn prepare(
    context: Context<'_>,
    request: &Request,
    options: &Options,
    body: &mut Value,
) -> bool {
    let Some(config) = context.config else {
        return false;
    };
    if config.codex.orphan_delegation_compatibility {
        rewrite_orphans(body, &options.headers);
    }
    let user_agent = header(&options.headers, header::USER_AGENT.as_str());
    let enabled = config.client.codex.optimize_multi_agent_v2;
    let models = || {
        context
            .models
            .map(spawn_agent_model_list)
            .unwrap_or_default()
    };
    let optimized = multi_agent_v2::optimize(body, &user_agent, enabled, models);
    if is_compat(context, request, options) {
        multi_agent_v2::rewrite_input(body, &user_agent, enabled, true);
    }
    optimized
}

/// Readies the payload of a request that another executor is about to
/// translate to `to` (`TranslateRequestWithCodexMultiAgentV2ForExecutor` for
/// a target that isn't Codex): a Codex client's tools get integer parameter
/// types; and a Responses request has its orphan delegation outputs turned
/// into user messages and, unless `to` is a Responses format, its
/// `agent_message` items into user messages, as the config says.
pub(crate) fn before_translation(
    config: Option<&Config>,
    options: &Options,
    to: &Format,
    payload: &mut Value,
) {
    let user_agent = header(&options.headers, header::USER_AGENT.as_str());
    if tool_integers::normalize(payload, &user_agent) {
        tracing::debug!("codex: normalized target tool number types to integer");
    }
    let Some(config) = config else {
        return;
    };
    if options.source_format != Format::OPENAI_RESPONSE {
        return;
    }
    if config.codex.orphan_delegation_compatibility {
        rewrite_orphans(payload, &options.headers);
    }
    if *to != Format::CODEX && *to != Format::OPENAI_RESPONSE {
        let enabled = config.client.codex.optimize_multi_agent_v2;
        multi_agent_v2::rewrite_input(payload, &user_agent, enabled, false);
    }
}

#[cfg(test)]
mod tests;
