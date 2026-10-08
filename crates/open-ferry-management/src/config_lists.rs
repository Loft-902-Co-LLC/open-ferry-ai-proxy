// Ported from CLIProxyAPI internal/api/handlers/management/config_lists.go
// (putStringList, patchStringList, deleteFromStringList, PutAPIKeys,
// PatchAPIKeys, DeleteAPIKeys, PutOAuthExcludedModels,
// PatchOAuthExcludedModels, DeleteOAuthExcludedModels, PutOAuthModelAlias,
// PatchOAuthModelAlias, DeleteOAuthModelAlias, PutOAuthRequestScopedErrors,
// PatchOAuthRequestScopedErrors, DeleteOAuthRequestScopedErrors) and
// internal/api/server_management.go (their routes) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Changing the client API keys and the OAuth channels' lists. Each change
//! saves the config and answers `{"status":"ok"}`; see
//! [`crate::config_write`].
//!
//! `/v0/management/api-keys`:
//! - `PUT` with a list of keys, or `{"items":[...]}` with at least one,
//!   replaces the keys.
//! - `PATCH` with `{"index":i,"value":"k"}` replaces key `i`; else with
//!   `{"old":"a","new":"b"}` replaces the first key `a` with `b`, or adds
//!   `b` when there is none. Anything else answers 400 `missing fields`.
//! - `DELETE ?index=i` removes key `i`; else `?value=k` removes every key
//!   that is `k` once trimmed. Neither answers 400 `missing index or
//!   value`.
//!
//! `/v0/management/oauth-excluded-models`, `oauth-model-alias` and
//! `oauth-request-scoped-errors`, maps from a channel to a list:
//! - `PUT` with the map, or `{"items":{...}}`, replaces it, cleaned up as
//!   loading cleans it up.
//! - `PATCH` with `{"provider":"c","models":[...]}` (excluded models), or
//!   `{"channel":"c","aliases":[...]}` or `{"channel":"c","rules":[...]}`
//!   (`provider` naming the channel when `channel` is missing), sets the
//!   channel's list; a list that is empty once cleaned up removes the
//!   channel, or answers 404 when there is none.
//! - `DELETE ?provider=c` (or `?channel=c`) removes the channel, or
//!   answers 404.
//!
//! Channels are trimmed and in lower case. A body that doesn't read as
//! the request answers 400 `invalid body`.
//!
//! Deviations from upstream: those of [`crate::config_write`],
//! [`crate::config_sanitize`] and [`crate::go_json`].

use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::response::Response;
use axum::routing::put;
use open_ferry_core::config::{OAuthModelAlias, RequestScopedErrorRule};
use open_ferry_translate::go::{to_lower, trim_space};
use serde::de::DeserializeOwned;

use crate::Route;
use crate::config_sanitize::{
    normalize_excluded_models, normalize_oauth_excluded_models, sanitized_oauth_model_alias,
    sanitized_oauth_request_scoped_errors,
};
use crate::config_write::{self, bad_request, not_found};
use crate::go::lossy;
use crate::go_json;
use crate::query::Query;
use crate::state::ManagementState;

/// What a body that doesn't read as the request answers.
pub(crate) const INVALID_BODY: &str = "invalid body";

/// The module's routes.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key(
            "/v0/management/api-keys",
            put(put_api_keys)
                .patch(patch_api_keys)
                .delete(delete_api_keys),
        ),
        Route::key(
            "/v0/management/oauth-excluded-models",
            put(put_excluded_models)
                .patch(patch_excluded_models)
                .delete(delete_excluded_models),
        ),
        Route::key(
            "/v0/management/oauth-model-alias",
            put(put_model_alias)
                .patch(patch_model_alias)
                .delete(delete_model_alias),
        ),
        Route::key(
            "/v0/management/oauth-request-scoped-errors",
            put(put_scoped_errors)
                .patch(patch_scoped_errors)
                .delete(delete_scoped_errors),
        ),
    ]
}

/// A list `PUT`'s body: the whole body as the list, else the `items` of an
/// object, which must hold at least one.
pub(crate) fn list_body<T: DeserializeOwned>(body: &[u8]) -> Option<Vec<T>> {
    let value = go_json::whole(body)?;
    if let Some(list) = go_json::decode(&value) {
        return Some(list);
    }
    let [items] = go_json::fields(&value, ["items"])?;
    let items: Vec<T> = go_json::plain(items).ok()?;
    (!items.is_empty()).then_some(items)
}

/// A map `PUT`'s body: the whole body as the map, else the `items` of an
/// object.
fn map_body<T: DeserializeOwned>(body: &[u8]) -> Option<BTreeMap<String, T>> {
    let value = go_json::whole(body)?;
    if let Some(map) = go_json::decode(&value) {
        return Some(map);
    }
    let [items] = go_json::fields(&value, ["items"])?;
    go_json::plain(items).ok()
}

/// A query value trimmed and in lower case, as the channels are kept.
fn channel_query(query: &Query, name: &str) -> String {
    to_lower(&lossy(trim_space(query.value(name))))
}

/// `PUT /v0/management/api-keys` (upstream's `putStringList`).
async fn put_api_keys(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match config_write::request_body(&state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(keys) = list_body::<String>(&body) else {
        return bad_request(INVALID_BODY);
    };
    config_write::update(&state, false, move |config| {
        config.api_keys = keys;
        Ok(())
    })
    .await
}

/// `PATCH /v0/management/api-keys`'s body.
struct ListPatch {
    old: Option<String>,
    new: Option<String>,
    index: Option<i64>,
    value: Option<String>,
}

impl ListPatch {
    fn read(body: &[u8]) -> Option<Self> {
        let request = go_json::first(body)?;
        let [old, new, index, value] = go_json::fields(&request, ["old", "new", "index", "value"])?;
        Some(Self {
            old: go_json::pointer(old).ok()?,
            new: go_json::pointer(new).ok()?,
            index: go_json::pointer(index).ok()?,
            value: go_json::pointer(value).ok()?,
        })
    }
}

/// `PATCH /v0/management/api-keys` (upstream's `patchStringList`).
async fn patch_api_keys(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match config_write::request_body(&state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(patch) = ListPatch::read(&body) else {
        return bad_request(INVALID_BODY);
    };
    config_write::update(&state, false, move |config| {
        let keys = &mut config.api_keys;
        if let (Some(index), Some(value)) = (patch.index, patch.value)
            && let Some(key) = usize::try_from(index).ok().and_then(|i| keys.get_mut(i))
        {
            *key = value;
            return Ok(());
        }
        if let (Some(old), Some(new)) = (patch.old, patch.new) {
            match keys.iter_mut().find(|key| **key == old) {
                Some(key) => *key = new,
                None => keys.push(new),
            }
            return Ok(());
        }
        Err(bad_request("missing fields"))
    })
    .await
}

/// `DELETE /v0/management/api-keys` (upstream's `deleteFromStringList`).
async fn delete_api_keys(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
) -> Response {
    let query = Query::parse(raw.as_deref());
    let index = query.value("index");
    let index = if index.is_empty() {
        None
    } else {
        go_json::sscanf_int(index)
    };
    let value = trim_space(query.value("value")).to_vec();
    config_write::update(&state, false, move |config| {
        let keys = &mut config.api_keys;
        if let Some(index) = index.and_then(|index| usize::try_from(index).ok())
            && index < keys.len()
        {
            keys.remove(index);
            return Ok(());
        }
        if !value.is_empty() {
            keys.retain(|key| key.trim().as_bytes() != value);
            return Ok(());
        }
        Err(bad_request("missing index or value"))
    })
    .await
}

/// `PUT /v0/management/oauth-excluded-models` (upstream's
/// `PutOAuthExcludedModels`).
async fn put_excluded_models(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match config_write::request_body(&state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(entries) = map_body::<Vec<String>>(&body) else {
        return bad_request(INVALID_BODY);
    };
    config_write::update(&state, false, move |config| {
        config.oauth_excluded_models = normalize_oauth_excluded_models(&entries);
        Ok(())
    })
    .await
}

/// `PATCH /v0/management/oauth-excluded-models` (upstream's
/// `PatchOAuthExcludedModels`).
async fn patch_excluded_models(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match config_write::request_body(&state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let read = || -> Option<(String, Vec<String>)> {
        let request = go_json::first(&body)?;
        let [provider, models] = go_json::fields(&request, ["provider", "models"])?;
        let models = go_json::plain(models).ok()?;
        Some((go_json::pointer(provider).ok()??, models))
    };
    let Some((provider, models)) = read() else {
        return bad_request(INVALID_BODY);
    };
    let provider = to_lower(provider.trim());
    if provider.is_empty() {
        return bad_request("invalid provider");
    }
    let models = normalize_excluded_models(&models);
    config_write::update(&state, false, move |config| {
        set_channel(
            &mut config.oauth_excluded_models,
            provider,
            models,
            "provider not found",
        )
    })
    .await
}

/// `DELETE /v0/management/oauth-excluded-models` (upstream's
/// `DeleteOAuthExcludedModels`).
async fn delete_excluded_models(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
) -> Response {
    if let Err(response) = config_write::writer(&state) {
        return response;
    }
    let provider = channel_query(&Query::parse(raw.as_deref()), "provider");
    if provider.is_empty() {
        return bad_request("missing provider");
    }
    config_write::update(&state, false, move |config| {
        remove_channel(
            &mut config.oauth_excluded_models,
            &provider,
            "provider not found",
        )
    })
    .await
}

/// `PUT /v0/management/oauth-model-alias` (upstream's
/// `PutOAuthModelAlias`).
async fn put_model_alias(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match config_write::request_body(&state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(entries) = map_body::<Vec<OAuthModelAlias>>(&body) else {
        return bad_request(INVALID_BODY);
    };
    config_write::update(&state, false, move |config| {
        config.oauth_model_alias = sanitized_oauth_model_alias(&entries);
        Ok(())
    })
    .await
}

/// `PATCH /v0/management/oauth-model-alias` (upstream's
/// `PatchOAuthModelAlias`).
async fn patch_model_alias(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match config_write::request_body(&state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some((channel, aliases)) = channel_patch::<OAuthModelAlias>(&body, "aliases") else {
        return bad_request(INVALID_BODY);
    };
    if channel.is_empty() {
        return bad_request("invalid channel");
    }
    let mut aliases = sanitized_oauth_model_alias(&BTreeMap::from([(channel.clone(), aliases)]));
    let aliases = aliases.remove(&channel).unwrap_or_default();
    config_write::update(&state, false, move |config| {
        set_channel(
            &mut config.oauth_model_alias,
            channel,
            aliases,
            "channel not found",
        )
    })
    .await
}

/// `DELETE /v0/management/oauth-model-alias` (upstream's
/// `DeleteOAuthModelAlias`).
async fn delete_model_alias(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
) -> Response {
    if let Err(response) = config_write::writer(&state) {
        return response;
    }
    let Some(channel) = channel_of_query(raw.as_deref()) else {
        return bad_request("missing channel");
    };
    config_write::update(&state, false, move |config| {
        remove_channel(&mut config.oauth_model_alias, &channel, "channel not found")
    })
    .await
}

/// `PUT /v0/management/oauth-request-scoped-errors` (upstream's
/// `PutOAuthRequestScopedErrors`).
async fn put_scoped_errors(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match config_write::request_body(&state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(entries) = map_body::<Vec<RequestScopedErrorRule>>(&body) else {
        return bad_request(INVALID_BODY);
    };
    config_write::update(&state, false, move |config| {
        config.oauth_request_scoped_errors = sanitized_oauth_request_scoped_errors(&entries);
        Ok(())
    })
    .await
}

/// `PATCH /v0/management/oauth-request-scoped-errors` (upstream's
/// `PatchOAuthRequestScopedErrors`).
async fn patch_scoped_errors(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match config_write::request_body(&state, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some((channel, rules)) = channel_patch::<RequestScopedErrorRule>(&body, "rules") else {
        return bad_request(INVALID_BODY);
    };
    if channel.is_empty() {
        return bad_request("invalid channel");
    }
    let mut rules =
        sanitized_oauth_request_scoped_errors(&BTreeMap::from([(channel.clone(), rules)]));
    let rules = rules.remove(&channel).unwrap_or_default();
    config_write::update(&state, false, move |config| {
        set_channel(
            &mut config.oauth_request_scoped_errors,
            channel,
            rules,
            "channel not found",
        )
    })
    .await
}

/// `DELETE /v0/management/oauth-request-scoped-errors` (upstream's
/// `DeleteOAuthRequestScopedErrors`).
async fn delete_scoped_errors(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
) -> Response {
    if let Err(response) = config_write::writer(&state) {
        return response;
    }
    let Some(channel) = channel_of_query(raw.as_deref()) else {
        return bad_request("missing channel");
    };
    config_write::update(&state, false, move |config| {
        remove_channel(
            &mut config.oauth_request_scoped_errors,
            &channel,
            "channel not found",
        )
    })
    .await
}

/// A channel `PATCH`'s body, `{"provider","channel",<list>}`: the channel
/// (`channel`, else `provider`) trimmed and in lower case, and the list.
fn channel_patch<T: DeserializeOwned>(body: &[u8], list: &str) -> Option<(String, Vec<T>)> {
    let request = go_json::first(body)?;
    let [provider, channel, items] = go_json::fields(&request, ["provider", "channel", list])?;
    let provider: Option<String> = go_json::pointer(provider).ok()?;
    let channel: Option<String> = go_json::pointer(channel).ok()?;
    let items = go_json::plain(items).ok()?;
    let channel = channel.or(provider).unwrap_or_default();
    Some((to_lower(channel.trim()), items))
}

/// The `channel` query, else the `provider` query, trimmed and in lower
/// case; `None` when both are empty.
fn channel_of_query(raw: Option<&str>) -> Option<String> {
    let query = Query::parse(raw);
    let channel = channel_query(&query, "channel");
    let channel = if channel.is_empty() {
        channel_query(&query, "provider")
    } else {
        channel
    };
    (!channel.is_empty()).then_some(channel)
}

/// Sets `channel`'s list to `items`, or removes the channel when `items`
/// is empty, answering 404 `missing` when there is none to remove.
fn set_channel<T>(
    map: &mut BTreeMap<String, Vec<T>>,
    channel: String,
    items: Vec<T>,
    missing: &str,
) -> Result<(), Response> {
    if items.is_empty() {
        return remove_channel(map, &channel, missing);
    }
    map.insert(channel, items);
    Ok(())
}

/// Removes `channel`, answering 404 `missing` when there is none.
fn remove_channel<T>(
    map: &mut BTreeMap<String, Vec<T>>,
    channel: &str,
    missing: &str,
) -> Result<(), Response> {
    match map.remove(channel) {
        Some(_) => Ok(()),
        None => Err(not_found(missing)),
    }
}
