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
//! (`client.codex.optimize-multi-agent-v2`), and a compatibility model's
//! `agent_message` items are rewritten.
//!
//! [`before_translation`] is what the other executors do to a request before
//! they translate it (`TranslateRequestWithCodexMultiAgentV2ForExecutor`): a
//! Codex client's own tools get integer parameter types, and a Responses
//! request has its orphan delegation outputs and, unless it stays in a
//! Responses format, its `agent_message` items rewritten.
//!
//! Deviations from upstream:
//! - Whether a model is a compatibility model is read from the config for
//!   every call, token counts included. Upstream first reads the model the
//!   credential manager resolved for the call, and for token counts reads
//!   only that; the manager doesn't resolve models that way here, and what
//!   it would resolve for a `codex-api-key` model is the config's flag. The
//!   Home service's credential options aren't ported.
//! - The other executors take the path upstream takes for models that
//!   aren't compatibility models, as they don't resolve the flag.
//! - Upstream normalizes the integer types of a Codex client's tools again
//!   after translation, as it applies the payload config, which isn't
//!   ported. Translation copies the types the pass before it set, so that
//!   pass gives the same result.
//! - The translator plugin hooks aren't ported, so a compatibility
//!   translation isn't passed to them.
//! - Upstream v8.0.10 keeps a v8 document's
//!   `oauth.providers.codex.orphan-delegation-compatibility` from API key
//!   credentials. The config here, as v8.0.11's, applies it to every
//!   credential, so API keys get orphan delegation compatibility too.

use http::HeaderMap;
use http::header::{self, HeaderValue};
use open_ferry_core::auth::{Auth, AuthSource};
use open_ferry_core::config::{CodexKey, Config};
use open_ferry_core::exec::{Format, Options, Request};
use open_ferry_translate::codex::claude::convert_claude_request_to_codex_with_compat;
use open_ferry_translate::codex_client::{
    header_value, multi_agent_v2, orphan_delegation, tool_integers,
};
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
/// models (`resolveCodexModelIsCompat`): by the models of the credential's
/// `codex-api-key` entry, matching the model with or without its thinking
/// suffix by name or alias, regardless of case.
pub(crate) fn is_compat(context: Context<'_>, request: &Request) -> bool {
    let (Some(config), Some(auth)) = (context.config, context.auth) else {
        return false;
    };
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
    if *source == Format::CLAUDE && *to == Format::CODEX && is_compat(context, request) {
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
    let optimized = multi_agent_v2::optimize(body, &user_agent, enabled);
    if is_compat(context, request) {
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
