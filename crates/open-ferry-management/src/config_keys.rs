// Ported from CLIProxyAPI internal/api/handlers/management/config_lists.go
// (parseCredentialWeightPatch, rejectInvalidCredentialWeight,
// PutGeminiKeys, PatchGeminiKey, DeleteGeminiKey, PutInteractionsKeys,
// PatchInteractionsKey, DeleteInteractionsKey, PutClaudeKeys,
// PatchClaudeKey, DeleteClaudeKey, PutOpenAICompat, PatchOpenAICompat,
// DeleteOpenAICompat, PutVertexCompatKeys, PatchVertexCompatKey,
// DeleteVertexCompatKey, PutCodexKeys, PatchCodexKey, DeleteCodexKey,
// PutXAIKeys, PatchXAIKey, DeleteXAIKey, PutMetaKeys, PatchMetaKey,
// DeleteMetaKey, applyDisableCoolingPatch) and
// internal/api/server_management.go (their routes) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Changing the providers' API keys: `gemini-api-key`,
//! `interactions-api-key`, `claude-api-key`, `codex-api-key`,
//! `xai-api-key`, `meta-api-key`, `openai-compatibility` and
//! `vertex-api-key` under `/v0/management/`. Each change saves the config
//! and answers `{"status":"ok"}`; see [`crate::config_write`].
//!
//! - `PUT` with a list of entries, or `{"items":[...]}` with at least one,
//!   replaces the list. A weight over 1,000,000 answers 400
//!   `<list>[i].weight: weight must not exceed 1000000`. Codex, xAI and
//!   OpenAI-compatible entries without a base URL are dropped, Meta's get
//!   `https://api.meta.ai/v1`, and a Vertex entry without a key answers 400
//!   `vertex-api-key[i].api-key is required`.
//! - `PATCH` with `{"index":i,"value":{...}}`, or `{"match":"<key>",...}`
//!   (`{"name":"<name>",...}` for OpenAI compatibility), changes the fields
//!   `value` gives of entry `i`, or of the entry with that key. A Gemini
//!   key's `match` must be unique, among the keys with the `base-url`
//!   query's base URL when it is given. No entry answers 404 `item not
//!   found`. `weight` and `disable-cooling` may be `null`, to unset them.
//!   An entry left without a key and base URL (Gemini), without a base URL
//!   (Codex, xAI, OpenAI compatibility) or without a key (Vertex) is
//!   removed.
//! - `DELETE ?api-key=<key>` removes the entry with that key, or with that
//!   key and the `base-url` query's base URL when it is given (every such
//!   entry, but for Gemini keys, where it must be unique); several entries
//!   with the key and no `base-url` answer 400. `?name=<name>` removes the
//!   OpenAI-compatible providers so named. Else `?index=i` removes entry
//!   `i`.
//!
//! The list is then cleaned up as loading cleans it up.
//!
//! Deviations from upstream:
//! - The client impersonation settings, which open-ferry doesn't read, are
//!   skipped wherever a request gives them, whatever their value: a Claude
//!   key's `cloak` and `fingerprint-profile`, and a Codex key's
//!   `disable-codex-cloaking`. Upstream checks them, and a Claude `PUT`
//!   keeps the cloak mode of the key it replaces.
//! - Those of [`crate::config_write`], [`crate::config_sanitize`] and
//!   [`crate::go_json`].

use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::response::Response;
use axum::routing::put;
use open_ferry_core::auth::weight::normalize_weight;
use open_ferry_core::config::{
    ClaudeKey, ClaudeModel, CodexKey, CodexModel, Config, GeminiKey, OpenAiCompatibility,
    OpenAiCompatibilityApiKey, OpenAiCompatibilityModel, RequestScopedErrorRule, VertexCompatKey,
    VertexCompatModel,
};
use open_ferry_translate::go::trim_space;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::Route;
use crate::config_lists::{INVALID_BODY, list_body};
use crate::config_sanitize::{
    META_BASE_URL, normalize_claude_key, normalize_codex_key, normalize_excluded_models,
    normalize_headers, normalize_openai_entry, normalize_vertex_key, sanitize_claude_keys,
    sanitize_codex_keys, sanitize_gemini_keys, sanitize_meta_keys, sanitize_openai_compatibility,
    sanitize_vertex_keys, sanitize_xai_keys,
};
use crate::config_write::{self, bad_request, not_found};
use crate::go_json;
use crate::query::Query;
use crate::state::ManagementState;

/// What a `PATCH` or `DELETE` answers when no entry matches.
const ITEM_NOT_FOUND: &str = "item not found";

/// A list of keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Gemini,
    Interactions,
    Claude,
    Codex,
    Xai,
    Meta,
    OpenAi,
    Vertex,
}

impl Family {
    const ALL: [Self; 8] = [
        Self::Gemini,
        Self::Interactions,
        Self::Claude,
        Self::Codex,
        Self::Xai,
        Self::Meta,
        Self::OpenAi,
        Self::Vertex,
    ];

    /// The list's name: its path under `/v0/management/` and its key in
    /// the config.
    fn name(self) -> &'static str {
        match self {
            Self::Gemini => "gemini-api-key",
            Self::Interactions => "interactions-api-key",
            Self::Claude => "claude-api-key",
            Self::Codex => "codex-api-key",
            Self::Xai => "xai-api-key",
            Self::Meta => "meta-api-key",
            Self::OpenAi => "openai-compatibility",
            Self::Vertex => "vertex-api-key",
        }
    }

    /// The fields a `PATCH`'s `value` may give, as upstream's patch type
    /// for the list names them, less the ones skipped here.
    fn patch_fields(self) -> &'static [&'static str] {
        match self {
            Self::Gemini | Self::Interactions => &[
                "api-key",
                "priority",
                "weight",
                "prefix",
                "base-url",
                "proxy-url",
                "headers",
                "excluded-models",
                "disable-cooling",
                "request-retry",
                "request-scoped-errors",
            ],
            Self::Claude => &[
                "api-key",
                "priority",
                "weight",
                "prefix",
                "base-url",
                "proxy-url",
                "models",
                "headers",
                "excluded-models",
                "rebuild-mid-system-message",
                "disable-cooling",
                "request-retry",
                "request-scoped-errors",
            ],
            Self::Codex => &[
                "api-key",
                "priority",
                "weight",
                "prefix",
                "base-url",
                "proxy-url",
                "alpha-search",
                "models",
                "headers",
                "excluded-models",
                "disable-cooling",
                "request-retry",
                "request-scoped-errors",
            ],
            Self::Xai => &[
                "api-key",
                "priority",
                "weight",
                "prefix",
                "base-url",
                "websockets",
                "proxy-url",
                "models",
                "headers",
                "excluded-models",
                "disable-cooling",
                "request-retry",
                "request-scoped-errors",
            ],
            Self::Meta => &[
                "api-key",
                "priority",
                "weight",
                "prefix",
                "base-url",
                "proxy-url",
                "models",
                "headers",
                "excluded-models",
                "disable-cooling",
                "request-retry",
                "request-scoped-errors",
            ],
            Self::OpenAi => &[
                "name",
                "priority",
                "prefix",
                "disabled",
                "disable-cooling",
                "base-url",
                "api-key-entries",
                "models",
                "headers",
                "support-prompt-cache-key",
                "request-retry",
                "request-scoped-errors",
            ],
            Self::Vertex => &[
                "api-key",
                "priority",
                "weight",
                "prefix",
                "base-url",
                "proxy-url",
                "headers",
                "models",
                "excluded-models",
                "disable-cooling",
                "request-retry",
            ],
        }
    }
}

/// The module's routes.
pub(crate) fn routes() -> Vec<Route> {
    Family::ALL
        .into_iter()
        .map(|family| {
            Route::key(
                format!("/v0/management/{}", family.name()),
                put(move |State(state): State<ManagementState>, body: Body| async move {
                    put_keys(&state, family, body).await
                })
                .patch(
                    move |State(state): State<ManagementState>,
                          RawQuery(raw): RawQuery,
                          body: Body| async move {
                        patch_key(&state, family, raw.as_deref(), body).await
                    },
                )
                .delete(
                    move |State(state): State<ManagementState>, RawQuery(raw): RawQuery| async move {
                        delete_key(&state, family, raw.as_deref()).await
                    },
                ),
            )
        })
        .collect()
}

/// A Gemini or Interactions list.
fn gemini_keys(config: &mut Config, family: Family) -> &mut Vec<GeminiKey> {
    match family {
        Family::Interactions => &mut config.interactions_api_key,
        _ => &mut config.gemini_api_key,
    }
}

/// A Codex, xAI or Meta list.
fn codex_keys(config: &mut Config, family: Family) -> &mut Vec<CodexKey> {
    match family {
        Family::Xai => &mut config.xai_api_key,
        Family::Meta => &mut config.meta_api_key,
        _ => &mut config.codex_api_key,
    }
}

/// Cleans up a Codex, xAI or Meta list.
fn sanitize_codex_family(keys: &mut Vec<CodexKey>, family: Family) {
    match family {
        Family::Xai => sanitize_xai_keys(keys),
        Family::Meta => sanitize_meta_keys(keys),
        _ => sanitize_codex_keys(keys),
    }
}

/// Upstream's `rejectInvalidCredentialWeight`: the 400 answer naming
/// `field` when `weight` is over the limit.
fn check_weight(field: &str, weight: Option<i64>) -> Result<(), Response> {
    match weight.map(normalize_weight) {
        Some(Err(error)) => Err(bad_request(&format!("{field}: {error}"))),
        _ => Ok(()),
    }
}

/// `PUT /v0/management/<list>` (upstream's `Put<Family>Keys`).
async fn put_keys(state: &ManagementState, family: Family, body: Body) -> Response {
    let body = match config_write::request_body(state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let change = match put_change(family, &body) {
        Ok(change) => change,
        Err(response) => return response,
    };
    config_write::update(state, false, change).await
}

/// A change made by the config.
type Change = Box<dyn FnOnce(&mut Config) -> Result<(), Response> + Send>;

/// What a `PUT` with `body` changes, or its 400 answer.
fn put_change(family: Family, body: &[u8]) -> Result<Change, Response> {
    let name = family.name();
    match family {
        Family::Gemini | Family::Interactions => {
            let keys: Vec<GeminiKey> = list_body(body).ok_or_else(|| bad_request(INVALID_BODY))?;
            for (i, key) in keys.iter().enumerate() {
                check_weight(&format!("{name}[{i}].weight"), key.weight)?;
            }
            Ok(Box::new(move |config| {
                let list = gemini_keys(config, family);
                *list = keys;
                sanitize_gemini_keys(list);
                Ok(())
            }))
        }
        Family::Claude => {
            let mut keys: Vec<ClaudeKey> =
                list_body(body).ok_or_else(|| bad_request(INVALID_BODY))?;
            for (i, key) in keys.iter_mut().enumerate() {
                normalize_claude_key(key);
                check_weight(&format!("{name}[{i}].weight"), key.weight)?;
            }
            Ok(Box::new(move |config| {
                config.claude_api_key = keys;
                sanitize_claude_keys(&mut config.claude_api_key);
                Ok(())
            }))
        }
        Family::Codex | Family::Xai | Family::Meta => {
            let keys: Vec<CodexKey> = list_body(body).ok_or_else(|| bad_request(INVALID_BODY))?;
            let mut kept = Vec::with_capacity(keys.len());
            for (i, mut key) in keys.into_iter().enumerate() {
                normalize_codex_key(&mut key);
                if key.base_url.is_empty() {
                    if family != Family::Meta {
                        continue;
                    }
                    key.base_url = META_BASE_URL.to_owned();
                }
                check_weight(&format!("{name}[{i}].weight"), key.weight)?;
                kept.push(key);
            }
            Ok(Box::new(move |config| {
                let list = codex_keys(config, family);
                *list = kept;
                sanitize_codex_family(list, family);
                Ok(())
            }))
        }
        Family::OpenAi => {
            let providers: Vec<OpenAiCompatibility> =
                list_body(body).ok_or_else(|| bad_request(INVALID_BODY))?;
            let mut kept = Vec::with_capacity(providers.len());
            for (i, mut provider) in providers.into_iter().enumerate() {
                normalize_openai_entry(&mut provider);
                if provider.base_url.trim().is_empty() {
                    continue;
                }
                for (j, key) in provider.api_key_entries.iter().enumerate() {
                    check_weight(
                        &format!("{name}[{i}].api-key-entries[{j}].weight"),
                        key.weight,
                    )?;
                }
                kept.push(provider);
            }
            Ok(Box::new(move |config| {
                config.openai_compatibility = kept;
                sanitize_openai_compatibility(&mut config.openai_compatibility);
                Ok(())
            }))
        }
        Family::Vertex => {
            let mut keys: Vec<VertexCompatKey> =
                list_body(body).ok_or_else(|| bad_request(INVALID_BODY))?;
            for (i, key) in keys.iter_mut().enumerate() {
                normalize_vertex_key(key);
                if key.api_key.is_empty() {
                    return Err(bad_request(&format!("{name}[{i}].api-key is required")));
                }
                check_weight(&format!("{name}[{i}].weight"), key.weight)?;
            }
            Ok(Box::new(move |config| {
                config.vertex_api_key = keys;
                sanitize_vertex_keys(&mut config.vertex_api_key);
                Ok(())
            }))
        }
    }
}

/// A `PATCH`'s `value`: each field the list's patch has, `None` when
/// missing or `null`, but `weight` and `disable-cooling` as given, `null`
/// included.
struct Patch<M> {
    api_key: Option<String>,
    name: Option<String>,
    priority: Option<i64>,
    weight: Option<Value>,
    prefix: Option<String>,
    base_url: Option<String>,
    proxy_url: Option<String>,
    disabled: Option<bool>,
    websockets: Option<bool>,
    alpha_search: Option<bool>,
    rebuild_mid_system_message: Option<bool>,
    support_prompt_cache_key: Option<bool>,
    models: Option<Vec<M>>,
    api_key_entries: Option<Vec<OpenAiCompatibilityApiKey>>,
    headers: Option<BTreeMap<String, String>>,
    excluded_models: Option<Vec<String>>,
    disable_cooling: Option<Value>,
    request_retry: Option<i64>,
    request_scoped_errors: Option<Vec<RequestScopedErrorRule>>,
}

impl<M: DeserializeOwned> Patch<M> {
    /// Reads `value`, an object, with the fields `names`; `None` when a
    /// field has the wrong type.
    fn read(value: &Value, names: &[&str]) -> Option<Self> {
        let values = go_json::field_values(value, names)?;
        let get = |name: &str| {
            names
                .iter()
                .position(|known| *known == name)
                .and_then(|index| values.get(index).copied().flatten())
        };
        Some(Self {
            api_key: go_json::pointer(get("api-key")).ok()?,
            name: go_json::pointer(get("name")).ok()?,
            priority: go_json::pointer(get("priority")).ok()?,
            weight: get("weight").cloned(),
            prefix: go_json::pointer(get("prefix")).ok()?,
            base_url: go_json::pointer(get("base-url")).ok()?,
            proxy_url: go_json::pointer(get("proxy-url")).ok()?,
            disabled: go_json::pointer(get("disabled")).ok()?,
            websockets: go_json::pointer(get("websockets")).ok()?,
            alpha_search: go_json::pointer(get("alpha-search")).ok()?,
            rebuild_mid_system_message: go_json::pointer(get("rebuild-mid-system-message")).ok()?,
            support_prompt_cache_key: go_json::pointer(get("support-prompt-cache-key")).ok()?,
            models: go_json::pointer(get("models")).ok()?,
            api_key_entries: go_json::pointer(get("api-key-entries")).ok()?,
            headers: go_json::pointer(get("headers")).ok()?,
            excluded_models: go_json::pointer(get("excluded-models")).ok()?,
            disable_cooling: get("disable-cooling").cloned(),
            request_retry: go_json::pointer(get("request-retry")).ok()?,
            request_scoped_errors: go_json::pointer(get("request-scoped-errors")).ok()?,
        })
    }

    /// Upstream's `parseCredentialWeightPatch`, when the patch gives a
    /// weight: `null` unsets it, else it must be an integer within the
    /// limit.
    fn weight(&self, target: &mut Option<i64>) -> Result<(), Response> {
        let Some(raw) = &self.weight else {
            return Ok(());
        };
        *target = match raw {
            Value::Null => None,
            Value::Number(number) => {
                let weight =
                    go_json::int(number).ok_or_else(|| bad_request("weight must be an integer"))?;
                normalize_weight(weight).map_err(|error| bad_request(&error.to_string()))?;
                Some(weight)
            }
            _ => return Err(bad_request("weight must be an integer")),
        };
        Ok(())
    }

    /// Upstream's `applyDisableCoolingPatch`, when the patch gives
    /// `disable-cooling`: `null` unsets it, else it must be a boolean.
    fn disable_cooling(&self, target: &mut Option<bool>) -> Result<(), Response> {
        *target = match &self.disable_cooling {
            None => return Ok(()),
            Some(Value::Null) => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err(bad_request("disable-cooling must be a boolean or null")),
        };
        Ok(())
    }
}

/// A `PATCH` request: the entry's index or its `match` (`name`), and the
/// fields to change.
struct PatchRequest<M> {
    index: Option<i64>,
    matched: Option<String>,
    value: Patch<M>,
}

impl<M: DeserializeOwned> PatchRequest<M> {
    /// Reads `body` as the `PATCH` of `family`, as upstream's
    /// `ShouldBindJSON` does: `None` for a body that doesn't decode or has
    /// no `value`.
    fn read(body: &[u8], family: Family) -> Option<Self> {
        let request = go_json::first(body)?;
        let selector = match family {
            Family::OpenAi => "name",
            _ => "match",
        };
        let [index, matched, value] = go_json::fields(&request, ["index", selector, "value"])?;
        let value = value.filter(|value| !value.is_null())?;
        Some(Self {
            index: go_json::pointer(index).ok()?,
            matched: go_json::pointer(matched).ok()?,
            value: Patch::read(value, family.patch_fields())?,
        })
    }

    /// The index the request gives, when it is within `len`.
    fn index_within(&self, len: usize) -> Option<usize> {
        self.index
            .and_then(|index| usize::try_from(index).ok())
            .filter(|&index| index < len)
    }
}

/// `PATCH /v0/management/<list>` (upstream's `Patch<Family>Key`).
async fn patch_key(
    state: &ManagementState,
    family: Family,
    raw_query: Option<&str>,
    body: Body,
) -> Response {
    let body = match config_write::request_body(state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let base_query = Query::parse(raw_query)
        .get("base-url")
        .map(|base| trim_space(base).to_vec());
    let change: Option<Change> = match family {
        Family::Gemini | Family::Interactions => {
            PatchRequest::<()>::read(&body, family).map(|request| -> Change {
                Box::new(move |config| {
                    patch_gemini(gemini_keys(config, family), &request, base_query.as_deref())
                })
            })
        }
        Family::Claude => PatchRequest::read(&body, family).map(|request| -> Change {
            Box::new(move |config| patch_claude(&mut config.claude_api_key, request))
        }),
        Family::Codex | Family::Xai | Family::Meta => {
            PatchRequest::read(&body, family).map(|request| -> Change {
                Box::new(move |config| patch_codex(codex_keys(config, family), family, request))
            })
        }
        Family::OpenAi => PatchRequest::read(&body, family).map(|request| -> Change {
            Box::new(move |config| patch_openai(&mut config.openai_compatibility, request))
        }),
        Family::Vertex => PatchRequest::read(&body, family).map(|request| -> Change {
            Box::new(move |config| patch_vertex(&mut config.vertex_api_key, request))
        }),
    };
    match change {
        Some(change) => config_write::update(state, false, change).await,
        None => bad_request(INVALID_BODY),
    }
}

/// Trims a patched string.
fn trimmed(value: &str) -> String {
    value.trim().to_owned()
}

/// Upstream's `PatchGeminiKey` and `PatchInteractionsKey`.
fn patch_gemini(
    keys: &mut Vec<GeminiKey>,
    request: &PatchRequest<()>,
    base_query: Option<&[u8]>,
) -> Result<(), Response> {
    let index = match request.index_within(keys.len()) {
        Some(index) => index,
        None => {
            let matched = request
                .matched
                .as_deref()
                .map(str::trim)
                .unwrap_or_default();
            let mut found = keys.iter().enumerate().filter(|(_, key)| {
                !matched.is_empty()
                    && key.api_key.trim() == matched
                    && base_query.is_none_or(|base| key.base_url.trim().as_bytes() == base)
            });
            let first = found.next().map(|(index, _)| index);
            if found.next().is_some() {
                return Err(bad_request("multiple items match; index is required"));
            }
            first.ok_or_else(|| not_found(ITEM_NOT_FOUND))?
        }
    };
    let Some(mut entry) = keys.get(index).cloned() else {
        return Err(not_found(ITEM_NOT_FOUND));
    };
    let patch = &request.value;
    if let Some(api_key) = &patch.api_key {
        entry.api_key = trimmed(api_key);
    }
    if let Some(priority) = patch.priority {
        entry.priority = priority;
    }
    patch.weight(&mut entry.weight)?;
    if let Some(prefix) = &patch.prefix {
        entry.prefix = trimmed(prefix);
    }
    if let Some(base_url) = &patch.base_url {
        entry.base_url = trimmed(base_url);
    }
    if let Some(proxy_url) = &patch.proxy_url {
        entry.proxy_url = trimmed(proxy_url);
    }
    if let Some(headers) = &patch.headers {
        entry.headers = normalize_headers(headers);
    }
    if let Some(excluded) = &patch.excluded_models {
        entry.excluded_models = normalize_excluded_models(excluded);
    }
    patch.disable_cooling(&mut entry.disable_cooling)?;
    if let Some(retry) = patch.request_retry {
        entry.request_retry = Some(retry);
    }
    if let Some(rules) = &patch.request_scoped_errors {
        entry.request_scoped_errors = rules.clone();
    }
    if entry.api_key.is_empty() && entry.base_url.is_empty() {
        remove_at(keys, index);
    } else {
        replace_at(keys, index, entry);
    }
    sanitize_gemini_keys(keys);
    Ok(())
}

/// The first entry whose key (as stored) is the trimmed `match`, when
/// the request gives no index within the list.
fn first_match<T>(
    keys: &[T],
    request: &PatchRequest<impl DeserializeOwned>,
    key: impl Fn(&T) -> &str,
) -> Result<usize, Response> {
    if let Some(index) = request.index_within(keys.len()) {
        return Ok(index);
    }
    request
        .matched
        .as_deref()
        .map(str::trim)
        .and_then(|matched| keys.iter().position(|entry| key(entry) == matched))
        .ok_or_else(|| not_found(ITEM_NOT_FOUND))
}

/// Upstream's `PatchClaudeKey`.
fn patch_claude(
    keys: &mut [ClaudeKey],
    request: PatchRequest<ClaudeModel>,
) -> Result<(), Response> {
    let index = first_match(keys, &request, |key| &key.api_key)?;
    let Some(mut entry) = keys.get(index).cloned() else {
        return Err(not_found(ITEM_NOT_FOUND));
    };
    let patch = request.value;
    if let Some(api_key) = &patch.api_key {
        entry.api_key = trimmed(api_key);
    }
    if let Some(priority) = patch.priority {
        entry.priority = priority;
    }
    patch.weight(&mut entry.weight)?;
    if let Some(prefix) = &patch.prefix {
        entry.prefix = trimmed(prefix);
    }
    if let Some(base_url) = &patch.base_url {
        entry.base_url = trimmed(base_url);
    }
    if let Some(proxy_url) = &patch.proxy_url {
        entry.proxy_url = trimmed(proxy_url);
    }
    if let Some(models) = &patch.models {
        entry.models = models.clone();
    }
    if let Some(headers) = &patch.headers {
        entry.headers = normalize_headers(headers);
    }
    if let Some(excluded) = &patch.excluded_models {
        entry.excluded_models = normalize_excluded_models(excluded);
    }
    if let Some(rebuild) = patch.rebuild_mid_system_message {
        entry.rebuild_mid_system_message = rebuild;
    }
    patch.disable_cooling(&mut entry.disable_cooling)?;
    if let Some(retry) = patch.request_retry {
        entry.request_retry = Some(retry);
    }
    if let Some(rules) = &patch.request_scoped_errors {
        entry.request_scoped_errors = rules.clone();
    }
    normalize_claude_key(&mut entry);
    if let Some(slot) = keys.get_mut(index) {
        *slot = entry;
    }
    sanitize_claude_keys(keys);
    Ok(())
}

/// Upstream's `PatchCodexKey`, `PatchXAIKey` and `PatchMetaKey`.
fn patch_codex(
    keys: &mut Vec<CodexKey>,
    family: Family,
    request: PatchRequest<CodexModel>,
) -> Result<(), Response> {
    let index = first_match(keys, &request, |key| &key.api_key)?;
    let Some(mut entry) = keys.get(index).cloned() else {
        return Err(not_found(ITEM_NOT_FOUND));
    };
    let patch = request.value;
    if let Some(api_key) = &patch.api_key {
        entry.api_key = trimmed(api_key);
    }
    if let Some(priority) = patch.priority {
        entry.priority = priority;
    }
    patch.weight(&mut entry.weight)?;
    if let Some(prefix) = &patch.prefix {
        entry.prefix = trimmed(prefix);
    }
    if let Some(base_url) = &patch.base_url {
        let base_url = trimmed(base_url);
        entry.base_url = match (base_url.is_empty(), family) {
            (true, Family::Meta) => META_BASE_URL.to_owned(),
            (true, _) => {
                remove_at(keys, index);
                sanitize_codex_family(keys, family);
                return Ok(());
            }
            (false, _) => base_url,
        };
    }
    if let Some(websockets) = patch.websockets {
        entry.websockets = websockets;
    }
    if let Some(proxy_url) = &patch.proxy_url {
        entry.proxy_url = trimmed(proxy_url);
    }
    if let Some(alpha_search) = patch.alpha_search {
        entry.alpha_search = alpha_search;
    }
    if let Some(models) = &patch.models {
        entry.models = models.clone();
    }
    if let Some(headers) = &patch.headers {
        entry.headers = normalize_headers(headers);
    }
    if let Some(excluded) = &patch.excluded_models {
        entry.excluded_models = normalize_excluded_models(excluded);
    }
    patch.disable_cooling(&mut entry.disable_cooling)?;
    if let Some(retry) = patch.request_retry {
        entry.request_retry = Some(retry);
    }
    if let Some(rules) = &patch.request_scoped_errors {
        entry.request_scoped_errors = rules.clone();
    }
    normalize_codex_key(&mut entry);
    replace_at(keys, index, entry);
    sanitize_codex_family(keys, family);
    Ok(())
}

/// Upstream's `PatchOpenAICompat`.
fn patch_openai(
    providers: &mut Vec<OpenAiCompatibility>,
    request: PatchRequest<OpenAiCompatibilityModel>,
) -> Result<(), Response> {
    let index = first_match(providers, &request, |provider| &provider.name)?;
    let Some(mut entry) = providers.get(index).cloned() else {
        return Err(not_found(ITEM_NOT_FOUND));
    };
    let patch = request.value;
    if let Some(name) = &patch.name {
        entry.name = trimmed(name);
    }
    if let Some(priority) = patch.priority {
        entry.priority = priority;
    }
    if let Some(prefix) = &patch.prefix {
        entry.prefix = trimmed(prefix);
    }
    if let Some(disabled) = patch.disabled {
        entry.disabled = disabled;
    }
    patch.disable_cooling(&mut entry.disable_cooling)?;
    if let Some(retry) = patch.request_retry {
        entry.request_retry = Some(retry);
    }
    if let Some(base_url) = &patch.base_url {
        let base_url = trimmed(base_url);
        if base_url.is_empty() {
            remove_at(providers, index);
            sanitize_openai_compatibility(providers);
            return Ok(());
        }
        entry.base_url = base_url;
    }
    if let Some(keys) = &patch.api_key_entries {
        for (i, key) in keys.iter().enumerate() {
            check_weight(&format!("api-key-entries[{i}].weight"), key.weight)?;
        }
        entry.api_key_entries = keys.clone();
    }
    if let Some(models) = &patch.models {
        entry.models = models.clone();
    }
    if let Some(headers) = &patch.headers {
        entry.headers = normalize_headers(headers);
    }
    if let Some(support) = patch.support_prompt_cache_key {
        entry.support_prompt_cache_key = support;
    }
    if let Some(rules) = &patch.request_scoped_errors {
        entry.request_scoped_errors = rules.clone();
    }
    normalize_openai_entry(&mut entry);
    replace_at(providers, index, entry);
    sanitize_openai_compatibility(providers);
    Ok(())
}

/// Upstream's `PatchVertexCompatKey`.
fn patch_vertex(
    keys: &mut Vec<VertexCompatKey>,
    request: PatchRequest<VertexCompatModel>,
) -> Result<(), Response> {
    let index = match request.index_within(keys.len()) {
        Some(index) => index,
        None => request
            .matched
            .as_deref()
            .map(str::trim)
            .filter(|matched| !matched.is_empty())
            .and_then(|matched| keys.iter().position(|key| key.api_key == matched))
            .ok_or_else(|| not_found(ITEM_NOT_FOUND))?,
    };
    let Some(mut entry) = keys.get(index).cloned() else {
        return Err(not_found(ITEM_NOT_FOUND));
    };
    let patch = request.value;
    if let Some(api_key) = &patch.api_key {
        let api_key = trimmed(api_key);
        if api_key.is_empty() {
            remove_at(keys, index);
            sanitize_vertex_keys(keys);
            return Ok(());
        }
        entry.api_key = api_key;
    }
    if let Some(priority) = patch.priority {
        entry.priority = priority;
    }
    patch.weight(&mut entry.weight)?;
    if let Some(prefix) = &patch.prefix {
        entry.prefix = trimmed(prefix);
    }
    if let Some(base_url) = &patch.base_url {
        entry.base_url = trimmed(base_url);
    }
    if let Some(proxy_url) = &patch.proxy_url {
        entry.proxy_url = trimmed(proxy_url);
    }
    if let Some(headers) = &patch.headers {
        entry.headers = normalize_headers(headers);
    }
    if let Some(models) = &patch.models {
        entry.models = models.clone();
    }
    if let Some(excluded) = &patch.excluded_models {
        entry.excluded_models = normalize_excluded_models(excluded);
    }
    patch.disable_cooling(&mut entry.disable_cooling)?;
    if let Some(retry) = patch.request_retry {
        entry.request_retry = Some(retry);
    }
    normalize_vertex_key(&mut entry);
    replace_at(keys, index, entry);
    sanitize_vertex_keys(keys);
    Ok(())
}

/// Removes entry `index`, if there is one.
fn remove_at<T>(list: &mut Vec<T>, index: usize) {
    if index < list.len() {
        list.remove(index);
    }
}

/// Replaces entry `index`, if there is one.
fn replace_at<T>(list: &mut [T], index: usize, entry: T) {
    if let Some(slot) = list.get_mut(index) {
        *slot = entry;
    }
}

/// An entry a `DELETE` finds by its key and base URL.
trait Keyed {
    fn key(&self) -> &str;
    fn base(&self) -> &str;
}

macro_rules! keyed {
    ($($entry:ty),*) => {$(
        impl Keyed for $entry {
            fn key(&self) -> &str {
                &self.api_key
            }

            fn base(&self) -> &str {
                &self.base_url
            }
        }
    )*};
}

keyed!(GeminiKey, ClaudeKey, CodexKey, VertexCompatKey);

/// A `DELETE`'s query.
struct DeleteQuery {
    /// `api-key`, trimmed.
    api_key: Vec<u8>,
    /// `base-url`, trimmed, when given.
    base_url: Option<Vec<u8>>,
    /// `name`, as given.
    name: Vec<u8>,
    /// `index`, when given and read as an integer.
    index: Option<i64>,
}

impl DeleteQuery {
    fn parse(raw: Option<&str>) -> Self {
        let query = Query::parse(raw);
        let index = query.value("index");
        Self {
            api_key: trim_space(query.value("api-key")).to_vec(),
            base_url: query.get("base-url").map(|base| trim_space(base).to_vec()),
            name: query.value("name").to_vec(),
            index: if index.is_empty() {
                None
            } else {
                go_json::sscanf_int(index)
            },
        }
    }

    /// Removes the entry the query names from `keys`. With `strict` (Gemini keys), a key and base URL
    /// must match exactly one entry, and a key alone at least one.
    fn remove<T: Keyed>(&self, keys: &mut Vec<T>, strict: bool) -> Result<(), Response> {
        if !self.api_key.is_empty() {
            let key_matches = |entry: &T| entry.key().trim().as_bytes() == self.api_key;
            if let Some(base) = &self.base_url {
                let matches =
                    |entry: &T| key_matches(entry) && entry.base().trim().as_bytes() == *base;
                if strict {
                    match keys.iter().filter(|entry| matches(entry)).count() {
                        0 => return Err(not_found(ITEM_NOT_FOUND)),
                        1 => {}
                        _ => {
                            return Err(bad_request("multiple items match; index is required"));
                        }
                    }
                }
                keys.retain(|entry| !matches(entry));
                return Ok(());
            }
            match keys.iter().filter(|entry| key_matches(entry)).count() {
                0 if strict => return Err(not_found(ITEM_NOT_FOUND)),
                0 | 1 => {}
                _ => {
                    return Err(bad_request(
                        "multiple items match api-key; base-url is required",
                    ));
                }
            }
            if let Some(index) = keys.iter().position(key_matches) {
                keys.remove(index);
            }
            return Ok(());
        }
        self.remove_index(keys, "missing api-key or index")
    }

    /// Removes entry `index`, or answers 400 `missing`.
    fn remove_index<T>(&self, keys: &mut Vec<T>, missing: &str) -> Result<(), Response> {
        match self
            .index
            .and_then(|index| usize::try_from(index).ok())
            .filter(|&index| index < keys.len())
        {
            Some(index) => {
                keys.remove(index);
                Ok(())
            }
            None => Err(bad_request(missing)),
        }
    }
}

/// `DELETE /v0/management/<list>` (upstream's `Delete<Family>Key`).
async fn delete_key(state: &ManagementState, family: Family, raw_query: Option<&str>) -> Response {
    let query = DeleteQuery::parse(raw_query);
    config_write::update(state, false, move |config| {
        match family {
            Family::Gemini | Family::Interactions => {
                let keys = gemini_keys(config, family);
                query.remove(keys, true)?;
                sanitize_gemini_keys(keys);
            }
            Family::Claude => {
                let keys = &mut config.claude_api_key;
                query.remove(keys, false)?;
                sanitize_claude_keys(keys);
            }
            Family::Codex | Family::Xai | Family::Meta => {
                let keys = codex_keys(config, family);
                query.remove(keys, false)?;
                sanitize_codex_family(keys, family);
            }
            Family::OpenAi => {
                let providers = &mut config.openai_compatibility;
                if query.name.is_empty() {
                    query.remove_index(providers, "missing name or index")?;
                } else {
                    providers.retain(|provider| provider.name.as_bytes() != query.name);
                }
                sanitize_openai_compatibility(providers);
            }
            Family::Vertex => {
                let keys = &mut config.vertex_api_key;
                query.remove(keys, false)?;
                sanitize_vertex_keys(keys);
            }
        }
        Ok(())
    })
    .await
}
