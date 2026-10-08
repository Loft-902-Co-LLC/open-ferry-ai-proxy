// Ported from CLIProxyAPI internal/api/handlers/management/auth_files.go
// (ListAuthFiles, parseAuthFilesPagination, bounds, authFilesListResponse,
// authFileListName, compareAuthFileListOrder, isAuthFileListable,
// matchesAuthFileLookup, GetAuthFileModels, buildAuthFileEntryLocked,
// isPersistentAuthFailure, isModelStateBlocked,
// reconcileAuthFileCooldownState, quotaObservationPayloadForProvider,
// quotaObservationPayload, modelQuotaObservationPayload, authWeightValue, authWebsocketsValue, authProjectID,
// extractCodexIDTokenClaims, authEmail, isRuntimeOnlyAuth) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `GET /v0/management/auth-files` (also `/v8/management/credentials`):
//! the credentials, each with its state, counts, cooldowns and what is
//! known of its account; and `GET /v0/management/auth-files/models` (also
//! `/v8/management/credentials/models`): the models one credential serves.
//!
//! The list can be narrowed to one `name` (a credential's ID or file name)
//! or `auth_index`. Given `page` or `page_size`, it is paged, sorted by
//! name, and leaves out credentials whose file is gone; otherwise it is
//! sorted by name, case aside. A credential that is neither from a file
//! nor runtime-only isn't listed, nor is a disabled runtime-only one, nor
//! one whose file is gone after it was disabled or removed.
//!
//! The `status`, `unavailable` and `next_retry_after` shown are worked out
//! from the cooldowns at the time of the request, so a cooldown that has
//! run out shows as over even before the manager next looks at the
//! credential.
//!
//! An API-key credential's `account` is the key itself, as upstream shows
//! it to the management client.
//!
//! `quota` is what the provider's last response with quota headers said of
//! the credential: its `signals` by header name and when it came,
//! `observed_at`; `model_quotas` has the same for each model whose last
//! response carried any. Only Claude's and Codex's are shown. Neither holds
//! the cooldown fields, so neither can be taken for the scheduler's state.
//!
//! [`credential_entry`], which upstream doesn't have, gives one
//! credential's entry whatever its source, for the dashboard API to show
//! the config's `claude-cli` entries, which the list hides.
//!
//! `quota_checks`, also not upstream's, lists the quota rests open-ferry's
//! `routing.quota.check-after` capped: for each, its `scope` (`credential`
//! or `model`, with its `model_key`), its `state` (`resting` until
//! `next_check_at`, then `due`, and `checking` while the one call let
//! through is in flight), `next_check_at`, `provider_reset_at` and the
//! current `wait_seconds`. An entry without any has no `quota_checks`.
//!
//! Deviations from upstream:
//! - The plugin host isn't ported: `supports_quota` and `quota_provider`
//!   come only from a `quota_probe` in the metadata.
//! - Quota observations are shown only for Claude and Codex; upstream's
//!   include Devin's, which isn't ported.
//! - Without a credential manager upstream lists the auth directory from
//!   disk; this port always has a manager.
//! - Times are written in UTC; upstream writes some (file times, times read
//!   from files) in the server's time zone.
//! - One clock reading serves the whole listing; upstream reads the clock
//!   again for each credential's state and recent requests.
//! - Without paging, credentials whose names differ only in case keep the
//!   manager's order (by ID); upstream's sort isn't stable.
//! - An entry has open-ferry's `quota_checks` while the cap on quota rests
//!   holds one of its rests; upstream's entries have no such field.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::response::Response;
use axum::routing::get;
use chrono::Utc;
use http::StatusCode;
use open_ferry_core::auth::weight::{parse_weight_str, parse_weight_value};
use open_ferry_core::auth::{Auth, ModelState, QuotaState, Status, Timestamp};
use open_ferry_core::manager::{
    CooldownView, Manager, QuotaCheck, cooldown_snapshot_for_auth, has_unauthorized_auth_failure,
    provider_supports_quota_observation,
};
use open_ferry_providers::codex::jwt::parse_jwt_token;
use open_ferry_translate::go::{to_lower, trim_space};
use serde_json::Value;

use crate::Route;
use crate::go::{atoi, equal_fold, is_zero, parse_bool};
use crate::json::{self, Json};
use crate::query::Query;
use crate::state::ManagementState;

/// The page size when only `page` is given.
const DEFAULT_PAGE_SIZE: i64 = 50;

/// A `gin.H`.
type Entry = BTreeMap<String, Json>;

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key("/v0/management/auth-files", get(list)),
        Route::key("/v8/management/credentials", get(list)),
        Route::key("/v0/management/auth-files/models", get(models)),
        Route::key("/v8/management/credentials/models", get(models)),
    ]
}

/// `GET /v0/management/auth-files` (upstream's `ListAuthFiles`).
pub(crate) async fn list(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
) -> Response {
    let query = Query::parse(raw.as_deref());
    let pagination = match Pagination::parse(&query) {
        Ok(pagination) => pagination,
        Err(message) => return json::error(StatusCode::BAD_REQUEST, message),
    };
    let filter = Filter {
        name: trim_space(query.value("name")).to_vec(),
        auth_index: trim_space(query.value("auth_index")).to_vec(),
    };
    let manager = state.manager().clone();
    // Listing reads each credential's file metadata.
    let body = run_blocking(move || list_body(&manager, &filter, pagination, Utc::now())).await;
    json::response(StatusCode::OK, &body)
}

/// Runs `f` on the blocking pool, passing a panic on.
pub(crate) async fn run_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(error) => panic!("blocking task failed: {error}"),
        },
    }
}

/// `page` and `page_size` (upstream's `authFilesPagination`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pagination {
    enabled: bool,
    page: i64,
    page_size: i64,
}

impl Pagination {
    /// Reads `page` and `page_size`; paging is on when either is given
    /// (upstream's `parseAuthFilesPagination`).
    fn parse(query: &Query) -> Result<Self, &'static str> {
        let page = query.get("page");
        let page_size = query.get("page_size");
        if page.is_none() && page_size.is_none() {
            return Ok(Self {
                enabled: false,
                page: 0,
                page_size: 0,
            });
        }
        let mut pagination = Self {
            enabled: true,
            page: 1,
            page_size: DEFAULT_PAGE_SIZE,
        };
        if let Some(raw) = page {
            pagination.page = positive(raw).ok_or("page must be a positive integer")?;
        }
        if let Some(raw) = page_size {
            pagination.page_size = positive(raw).ok_or("page_size must be a positive integer")?;
        }
        Ok(pagination)
    }

    /// The slice of `total` entries the page covers.
    fn bounds(self, total: usize) -> (usize, usize) {
        let count = i64::try_from(total).unwrap_or(i64::MAX);
        if !self.enabled || count <= 0 {
            return (0, total);
        }
        if self.page > 1 && self.page - 1 > count / self.page_size {
            return (total, total);
        }
        let start = (self.page - 1) * self.page_size;
        if start >= count {
            return (total, total);
        }
        let start_index = usize::try_from(start).unwrap_or(total);
        let remaining = count - start;
        if self.page_size >= remaining {
            return (start_index, total);
        }
        let end = usize::try_from(start + self.page_size).unwrap_or(total);
        (start_index, end)
    }
}

/// A positive integer, after trimming, as Go's `Atoi` reads it.
fn positive(raw: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(trim_space(raw)).ok()?;
    atoi(text).filter(|n| *n > 0)
}

/// The `name` and `auth_index` filters, trimmed; empty matches all.
struct Filter {
    name: Vec<u8>,
    auth_index: Vec<u8>,
}

impl Filter {
    /// upstream's `matchesAuthFileLookup`.
    fn matches(&self, auth: &Auth) -> bool {
        if !self.name.is_empty()
            && auth.id.trim().as_bytes() != self.name
            && auth.file_name.trim().as_bytes() != self.name
        {
            return false;
        }
        self.auth_index.is_empty() || auth_index(auth).as_bytes() == self.auth_index
    }
}

/// The listing's body.
fn list_body(manager: &Manager, filter: &Filter, pagination: Pagination, now: Timestamp) -> Json {
    let auths = manager.list();
    if pagination.enabled {
        let mut matching: Vec<_> = auths
            .iter()
            .filter(|auth| filter.matches(auth) && is_listable(auth))
            .collect();
        matching.sort_by(|a, b| compare_list_order(a, b));
        let total = matching.len();
        let (start, end) = pagination.bounds(total);
        let files = matching[start..end]
            .iter()
            .filter_map(|auth| listed_entry(manager, auth, now))
            .collect();
        let has_more = end < total;
        return Json::map([
            ("observed_at", Json::Time(now)),
            ("files", Json::Array(files)),
            ("total", Json::Int(int(total))),
            ("page", Json::Int(pagination.page)),
            ("page_size", Json::Int(pagination.page_size)),
            ("has_more", Json::Bool(has_more)),
        ]);
    }
    let mut files: Vec<(String, Json)> = auths
        .iter()
        .filter(|auth| filter.matches(auth))
        .filter_map(|auth| {
            let entry = listed_entry(manager, auth, now)?;
            Some((to_lower(&entry_name(auth)), entry))
        })
        .collect();
    files.sort_by(|(a, _), (b, _)| a.cmp(b));
    Json::map([
        ("observed_at", Json::Time(now)),
        (
            "files",
            Json::Array(files.into_iter().map(|(_, entry)| entry).collect()),
        ),
    ])
}

/// A credential's entry with its cooldowns, or `None` when it is hidden.
fn listed_entry(manager: &Manager, auth: &Auth, now: Timestamp) -> Option<Json> {
    Some(with_cooldowns(manager, build_entry(auth, now)?, auth, now))
}

/// The credential with ID `id`, and the entry the listing would show for
/// it at `now`, cooldowns included; `None` when the manager holds none.
///
/// Not upstream's: the listing hides a credential made from the config,
/// and the dashboard API shows a `claude-cli` entry's credential with
/// this. It shows a credential whatever its source, but not one whose
/// file is gone after it was disabled or removed. It reads the metadata of
/// the credential's file, if it has one.
pub fn credential_entry(
    state: &ManagementState,
    id: &str,
    now: Timestamp,
) -> Option<(Arc<Auth>, Value)> {
    let manager = state.manager();
    let auth = manager.get(id)?;
    let entry = with_cooldowns(manager, entry_fields(&auth, now)?, &auth, now);
    let value = serde_json::from_str(&entry.encode()).ok()?;
    Some((auth, value))
}

/// `entry` with the credential's cooldowns at `now`, and its capped quota
/// rests if it has any.
fn with_cooldowns(manager: &Manager, mut entry: Entry, auth: &Auth, now: Timestamp) -> Json {
    let cooldowns = cooldown_snapshot_for_auth(auth, now)
        .iter()
        .map(cooldown_json)
        .collect();
    entry.insert("cooldowns".into(), Json::Array(cooldowns));
    let checks = manager.quota_checks(&auth.id);
    if !checks.is_empty() {
        let checks = checks
            .iter()
            .map(|check| quota_check_json(check, now))
            .collect();
        entry.insert("quota_checks".into(), Json::Array(checks));
    }
    Json::Map(entry)
}

/// A capped quota rest at `now` (open-ferry's `quota_checks`).
pub(crate) fn quota_check_json(check: &QuotaCheck, now: Timestamp) -> Json {
    let mut fields = Vec::new();
    if check.model_key.is_empty() {
        fields.push(("scope", Json::Str("credential".into())));
    } else {
        fields.push(("scope", Json::Str("model".into())));
        fields.push(("model_key", Json::Str(check.model_key.clone())));
    }
    let state = if check.checking {
        "checking"
    } else if check.next_check_at <= now {
        "due"
    } else {
        "resting"
    };
    fields.push(("state", Json::Str(state.into())));
    fields.push(("next_check_at", Json::Time(check.next_check_at)));
    fields.push(("provider_reset_at", Json::Time(check.provider_reset_at)));
    let wait = i64::try_from(check.wait.as_secs()).unwrap_or(i64::MAX);
    fields.push(("wait_seconds", Json::Int(wait)));
    Json::Struct(fields)
}

/// A cooldown as upstream's `CooldownView` struct writes it.
fn cooldown_json(view: &CooldownView) -> Json {
    let mut fields = vec![("scope", Json::Str(view.scope.to_owned()))];
    if !view.model_key.is_empty() {
        fields.push(("model_key", Json::Str(view.model_key.clone())));
    }
    fields.push(("reason", Json::Str(view.reason.to_owned())));
    fields.push(("retry_at", Json::Time(view.retry_at)));
    fields.push(("remaining_seconds", Json::Int(view.remaining_seconds)));
    if let Some(level) = view.backoff_level {
        fields.push(("backoff_level", Json::Int(i64::from(level))));
    }
    if view.http_status != 0 {
        fields.push(("http_status", Json::Int(i64::from(view.http_status))));
    }
    Json::Struct(fields)
}

/// The name a paged list sorts by: the file name, else the ID, trimmed
/// (upstream's `authFileListName`).
fn list_name(auth: &Auth) -> &str {
    match auth.file_name.trim() {
        "" => auth.id.trim(),
        name => name,
    }
}

/// upstream's `compareAuthFileListOrder`: by name case aside, then name,
/// ID and index.
fn compare_list_order(left: &Auth, right: &Auth) -> Ordering {
    let (left_name, right_name) = (list_name(left), list_name(right));
    to_lower(left_name)
        .cmp(&to_lower(right_name))
        .then_with(|| left_name.cmp(right_name))
        .then_with(|| left.id.trim().cmp(right.id.trim()))
        .then_with(|| left.index.trim().cmp(right.index.trim()))
}

/// The name an entry shows: the file name, trimmed, else the ID.
fn entry_name(auth: &Auth) -> String {
    match auth.file_name.trim() {
        "" => auth.id.clone(),
        name => name.to_owned(),
    }
}

/// The credential's index, trimmed, derived if it has none yet (upstream's
/// `EnsureIndex`, which the manager has already called for every
/// credential it holds).
pub(crate) fn auth_index(auth: &Auth) -> String {
    let index = auth.index.trim();
    if !index.is_empty() {
        return index.to_owned();
    }
    let mut auth = auth.clone();
    auth.ensure_index().to_owned()
}

/// Whether the credential is runtime-only: never saved to a file
/// (upstream's `isRuntimeOnlyAuth`).
fn is_runtime_only(auth: &Auth) -> bool {
    equal_fold(attribute(auth, "runtime_only").trim(), "true")
}

fn attribute<'a>(auth: &'a Auth, key: &str) -> &'a str {
    auth.attribute(key).unwrap_or_default()
}

fn is_disabled(auth: &Auth) -> bool {
    auth.disabled || auth.status == Status::Disabled
}

/// Whether a credential whose file is gone should be hidden: it was
/// disabled, or removed through the management API.
fn removed(auth: &Auth) -> bool {
    is_disabled(auth) || equal_fold(auth.status_message.trim(), "removed via management api")
}

/// Whether a paged list includes the credential (upstream's
/// `isAuthFileListable`).
fn is_listable(auth: &Auth) -> bool {
    let runtime_only = is_runtime_only(auth);
    if runtime_only && is_disabled(auth) {
        return false;
    }
    let path = attribute(auth, "path").trim();
    if path.is_empty() {
        return runtime_only;
    }
    let missing = std::fs::metadata(path).is_err_and(|e| e.kind() == io::ErrorKind::NotFound);
    !(missing && !runtime_only && removed(auth))
}

/// A credential's entry, or `None` when it is hidden (upstream's
/// `buildAuthFileEntryLocked`).
fn build_entry(auth: &Auth, now: Timestamp) -> Option<Entry> {
    let runtime_only = is_runtime_only(auth);
    if runtime_only && is_disabled(auth) {
        return None;
    }
    if attribute(auth, "path").trim().is_empty() && !runtime_only {
        return None;
    }
    entry_fields(auth, now)
}

/// A credential's entry, whatever its source, or `None` when its file is
/// gone after it was disabled or removed (the rest of upstream's
/// `buildAuthFileEntryLocked`).
fn entry_fields(auth: &Auth, now: Timestamp) -> Option<Entry> {
    let index = auth_index(auth);
    let runtime_only = is_runtime_only(auth);
    let path = attribute(auth, "path").trim();
    let cooldown = reconcile_cooldown_state(auth, now);
    let provider = auth.provider.trim();
    let mut entry = Entry::new();
    let mut set = |key: &str, value: Json| {
        entry.insert(key.to_owned(), value);
    };
    set("id", Json::Str(auth.id.clone()));
    set("auth_index", Json::Str(index));
    set("name", Json::Str(entry_name(auth)));
    set("type", Json::Str(provider.to_owned()));
    set("provider", Json::Str(provider.to_owned()));
    set("label", Json::Str(auth.label.clone()));
    set("status", Json::Str(cooldown.status.as_str().to_owned()));
    set("status_message", Json::Str(cooldown.status_message));
    set("disabled", Json::Bool(auth.disabled));
    set("unavailable", Json::Bool(cooldown.unavailable));
    set("runtime_only", Json::Bool(runtime_only));
    set("source", Json::Str("memory".into()));
    set("size", Json::Int(0));
    set("success", Json::Int(auth.success));
    set("failed", Json::Int(auth.failed));
    let recent = auth
        .recent_requests_snapshot(now)
        .into_iter()
        .map(|bucket| {
            Json::Struct(vec![
                ("time", Json::Str(bucket.time)),
                ("success", Json::Int(bucket.success)),
                ("failed", Json::Int(bucket.failed)),
            ])
        })
        .collect();
    set("recent_requests", Json::Array(recent));
    set("quota", quota_observation(&auth.provider, &auth.quota));
    let model_quotas = model_quota_observations(&auth.provider, &auth.model_states);
    if !model_quotas.is_empty() {
        set("model_quotas", Json::Map(model_quotas));
    }
    if let Some(probe) = auth.metadata.get("quota_probe").filter(|v| !v.is_null()) {
        set("supports_quota", Json::Bool(true));
        set("quota_probe", Json::Any(probe.clone()));
    }
    let email = email(auth);
    if !email.is_empty() {
        set("email", Json::Str(email.to_owned()));
    }
    let project_id = project_id(auth);
    if !project_id.is_empty() {
        set("project_id", Json::Str(project_id.to_owned()));
    }
    if let Some((account_type, account)) = auth.account_info() {
        set("account_type", Json::Str(account_type.to_owned()));
        if !account.is_empty() {
            set("account", Json::Str(account.to_owned()));
        }
    }
    if !is_zero(auth.created_at) {
        set("created_at", time(auth.created_at));
    }
    if !is_zero(auth.updated_at) {
        set("modtime", time(auth.updated_at));
        set("updated_at", time(auth.updated_at));
    }
    if !is_zero(auth.last_refreshed_at) {
        set("last_refresh", time(auth.last_refreshed_at));
    }
    if !is_zero(cooldown.next_retry) {
        set("next_retry_after", time(cooldown.next_retry));
    }
    if !path.is_empty() {
        set("path", Json::Str(path.to_owned()));
        set("source", Json::Str("file".into()));
        match std::fs::metadata(path) {
            Ok(info) => {
                set(
                    "size",
                    Json::Int(i64::try_from(info.len()).unwrap_or(i64::MAX)),
                );
                if let Ok(modified) = info.modified() {
                    set("modtime", Json::Time(Timestamp::from(modified)));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Hide credentials removed from disk but still in memory.
                if !runtime_only && removed(auth) {
                    return None;
                }
                set("source", Json::Str("memory".into()));
            }
            Err(error) => tracing::warn!("failed to stat auth file {path}: {error}"),
        }
    }
    if let Some(claims) = codex_id_token_claims(auth) {
        set("id_token", Json::Map(claims));
    }
    if let Some(priority) = priority(auth) {
        set("priority", Json::Int(priority));
    }
    if let Some(note) = note(auth) {
        set("note", Json::Str(note.to_owned()));
    }
    if let Some(weight) = weight(auth) {
        set("weight", Json::Int(weight));
    }
    if let Some(websockets) = websockets(auth) {
        set("websockets", Json::Bool(websockets));
    }
    if let Some(retry) = auth.request_retry_override() {
        set("request_retry", Json::Int(retry));
    }
    Some(entry)
}

fn time(time: Option<Timestamp>) -> Json {
    time.map_or(Json::Null, Json::Time)
}

/// A quota snapshot as the listing shows it: its `signals`, always, and
/// `observed_at` when there is one; a provider that isn't observed shows
/// an empty one (upstream's `quotaObservationPayloadForProvider` and
/// `quotaObservationPayload`).
pub(crate) fn quota_observation(provider: &str, quota: &QuotaState) -> Json {
    let mut observed = BTreeMap::new();
    if !provider_supports_quota_observation(provider) {
        observed.insert("signals".to_owned(), Json::Map(BTreeMap::new()));
        return Json::Map(observed);
    }
    if !is_zero(quota.observed_at) {
        observed.insert("observed_at".to_owned(), time(quota.observed_at));
    }
    let signals = quota
        .signals
        .iter()
        .map(|(name, value)| (name.clone(), Json::Str(value.clone())))
        .collect();
    observed.insert("signals".to_owned(), Json::Map(signals));
    Json::Map(observed)
}

/// The snapshot of each model that has one, by model (upstream's
/// `modelQuotaObservationPayload`); none for a provider that isn't
/// observed.
pub(crate) fn model_quota_observations(
    provider: &str,
    states: &BTreeMap<String, ModelState>,
) -> BTreeMap<String, Json> {
    if !provider_supports_quota_observation(provider) {
        return BTreeMap::new();
    }
    states
        .iter()
        .filter(|(_, state)| !is_zero(state.quota.observed_at) || !state.quota.signals.is_empty())
        .map(|(model, state)| (model.clone(), quota_observation(provider, &state.quota)))
        .collect()
}

fn int(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The account's email: the metadata's `email` when it is a string, even
/// an empty one, else the `email` or `account_email` attribute (upstream's
/// `authEmail`).
fn email(auth: &Auth) -> &str {
    if let Some(email) = auth.metadata_str("email") {
        return email.trim();
    }
    ["email", "account_email"]
        .into_iter()
        .map(|key| attribute(auth, key).trim())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

/// upstream's `authProjectID`.
fn project_id(auth: &Auth) -> &str {
    match auth.metadata_str("project_id").map(str::trim) {
        Some(id) if !id.is_empty() => id,
        _ => attribute(auth, "project_id").trim(),
    }
}

/// The plan and subscription claims of a Codex credential's ID token
/// (upstream's `extractCodexIDTokenClaims`).
fn codex_id_token_claims(auth: &Auth) -> Option<BTreeMap<String, Json>> {
    if !equal_fold(auth.provider.trim(), "codex") {
        return None;
    }
    let token = auth.metadata_str("id_token")?.trim();
    if token.is_empty() {
        return None;
    }
    let claims = parse_jwt_token(token).ok()?;
    let info = &claims.codex_auth_info;
    let mut result = BTreeMap::new();
    let account_id = info.chatgpt_account_id.trim();
    if !account_id.is_empty() {
        result.insert(
            "chatgpt_account_id".into(),
            Json::Str(account_id.to_owned()),
        );
    }
    let plan = info.chatgpt_plan_type.trim();
    if !plan.is_empty() {
        result.insert("plan_type".into(), Json::Str(plan.to_owned()));
    }
    for (key, value) in [
        (
            "chatgpt_subscription_active_start",
            &info.chatgpt_subscription_active_start,
        ),
        (
            "chatgpt_subscription_active_until",
            &info.chatgpt_subscription_active_until,
        ),
    ] {
        if !value.is_null() {
            result.insert(key.into(), Json::Any(value.clone()));
        }
    }
    (!result.is_empty()).then_some(result)
}

/// The `priority` attribute, else the metadata's: a number, truncated, or
/// a string holding an integer.
fn priority(auth: &Auth) -> Option<i64> {
    let raw = attribute(auth, "priority").trim();
    if !raw.is_empty() {
        return atoi(raw);
    }
    match auth.metadata.get("priority")? {
        Value::Number(number) => number.as_f64().map(float_to_int),
        Value::String(text) => atoi(text.trim()),
        _ => None,
    }
}

/// Go's `int(f)` on amd64: truncated toward zero, and the smallest integer
/// when out of range.
fn float_to_int(f: f64) -> i64 {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if (-LIMIT..LIMIT).contains(&f) {
        f as i64
    } else {
        i64::MIN
    }
}

/// The `note` attribute, else the metadata's, trimmed.
fn note(auth: &Auth) -> Option<&str> {
    let note = attribute(auth, "note").trim();
    if !note.is_empty() {
        return Some(note);
    }
    auth.metadata_str("note")
        .map(str::trim)
        .filter(|note| !note.is_empty())
}

/// upstream's `authWeightValue`.
fn weight(auth: &Auth) -> Option<i64> {
    let raw = attribute(auth, "weight").trim();
    if !raw.is_empty() {
        return parse_weight_str(raw).ok();
    }
    let value = auth.metadata.get("weight").filter(|v| !v.is_null())?;
    parse_weight_value(value).ok()
}

/// upstream's `authWebsocketsValue`: an attribute that doesn't parse falls
/// through to the metadata.
fn websockets(auth: &Auth) -> Option<bool> {
    let raw = attribute(auth, "websockets").trim();
    if !raw.is_empty()
        && let Some(value) = parse_bool(raw)
    {
        return Some(value);
    }
    match auth.metadata.get("websockets")? {
        Value::Bool(value) => Some(*value),
        Value::String(text) => parse_bool(text.trim()),
        _ => None,
    }
}

/// `GET /v0/management/auth-files/models` (upstream's
/// `GetAuthFileModels`).
pub(crate) async fn models(
    State(state): State<ManagementState>,
    RawQuery(raw): RawQuery,
) -> Response {
    let query = Query::parse(raw.as_deref());
    let name = query.value("name");
    if name.is_empty() {
        return json::error(StatusCode::BAD_REQUEST, "name is required");
    }
    // A name that isn't UTF-8 names no credential, and no client.
    let Ok(name) = std::str::from_utf8(name) else {
        return json::response(
            StatusCode::OK,
            &Json::map([("models", Json::Array(Vec::new()))]),
        );
    };
    let auth_id = state
        .manager()
        .list()
        .into_iter()
        .find(|auth| auth.file_name == name || auth.id == name)
        .map(|auth| auth.id.clone())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| name.to_owned());
    let models = state
        .registry()
        .models_for_client(&auth_id)
        .into_iter()
        .map(|model| {
            let mut entry = Entry::new();
            entry.insert("id".into(), Json::Str(model.id));
            for (key, value) in [
                ("display_name", model.display_name),
                ("type", model.model_type),
                ("owned_by", model.owned_by),
            ] {
                if !value.is_empty() {
                    entry.insert(key.into(), Json::Str(value));
                }
            }
            Json::Map(entry)
        })
        .collect();
    json::response(
        StatusCode::OK,
        &Json::map([("models", Json::Array(models))]),
    )
}

/// A credential's state as the list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CooldownState {
    pub(crate) unavailable: bool,
    pub(crate) status: Status,
    pub(crate) status_message: String,
    pub(crate) next_retry: Option<Timestamp>,
}

/// Whether the credential can't serve calls whatever its cooldowns say: a
/// 401 with no refresh pending, an expired access token, or a status of
/// `token expired` (upstream's `isPersistentAuthFailure`).
fn is_persistent_auth_failure(auth: &Auth, now: Timestamp) -> bool {
    has_unauthorized_auth_failure(auth)
        || auth
            .access_token_expiration_time()
            .is_some_and(|exp| !is_zero(Some(exp)) && exp <= now)
        || equal_fold(auth.status_message.trim(), "token expired")
}

/// Whether a time is set and after `now`.
fn after(time: Option<Timestamp>, now: Timestamp) -> bool {
    time.is_some_and(|time| time > now)
}

/// Whether a model state keeps the model from being scheduled at `now`
/// (upstream's `isModelStateBlocked`).
fn is_model_state_blocked(state: &ModelState, now: Timestamp) -> bool {
    if state.status == Status::Disabled {
        return true;
    }
    if !state.unavailable && !state.quota.exceeded {
        return false;
    }
    let has_recovery_time = !is_zero(state.next_retry_after)
        || (!is_zero(state.quota.next_recover_at) && state.quota.exceeded);
    if after(state.next_retry_after, now) {
        return true;
    }
    if state.quota.exceeded && after(state.quota.next_recover_at, now) {
        return true;
    }
    !has_recovery_time
}

/// The credential's state with its cooldowns weighed at `now` (upstream's
/// `reconcileAuthFileCooldownState`).
pub(crate) fn reconcile_cooldown_state(auth: &Auth, now: Timestamp) -> CooldownState {
    let mut state = CooldownState {
        unavailable: auth.unavailable,
        status: auth.status,
        status_message: auth.status_message.clone(),
        next_retry: auth.next_retry_after.filter(|t| !is_zero(Some(*t))),
    };
    let clear_past = |state: &mut CooldownState| {
        if state.next_retry.is_some_and(|t| t <= now) {
            state.next_retry = None;
        }
    };

    if is_disabled(auth) {
        state.status = Status::Disabled;
        return state;
    }
    // An authentication or token failure never shows as active.
    if is_persistent_auth_failure(auth, now) {
        clear_past(&mut state);
        state.unavailable = true;
        state.status = Status::Error;
        return state;
    }

    // A credential-wide cooldown counts only while the credential is
    // marked unavailable or over quota.
    let mut active_credential_cooldown = false;
    if auth.unavailable || auth.quota.exceeded {
        if after(auth.next_retry_after, now) {
            active_credential_cooldown = true;
        }
        if auth.quota.exceeded
            && auth.quota.reason == "credential_quota"
            && after(auth.quota.next_recover_at, now)
        {
            active_credential_cooldown = true;
            let recover_at = auth.quota.next_recover_at;
            if state.next_retry.is_none() || recover_at > state.next_retry {
                state.next_retry = recover_at;
            }
        }
    }

    let mut schedulable_models = false;
    let mut all_schedulable_blocked = true;
    let mut active_model_cooldown = false;
    let mut any_model_cooldown = false;
    for model in auth.model_states.values() {
        if model.status == Status::Disabled {
            continue;
        }
        schedulable_models = true;
        if !is_zero(model.next_retry_after)
            || (model.quota.exceeded && !is_zero(model.quota.next_recover_at))
        {
            any_model_cooldown = true;
        }
        if after(model.next_retry_after, now)
            || (model.quota.exceeded && after(model.quota.next_recover_at, now))
        {
            active_model_cooldown = true;
        }
        if !is_model_state_blocked(model, now) {
            all_schedulable_blocked = false;
        }
    }

    let had_cooldown = !is_zero(auth.next_retry_after)
        || (auth.quota.exceeded && !is_zero(auth.quota.next_recover_at))
        || any_model_cooldown;

    if active_credential_cooldown
        || (schedulable_models && all_schedulable_blocked && auth.unavailable)
    {
        clear_past(&mut state);
        state.unavailable = true;
        state.status = Status::Error;
        return state;
    }

    if !auth.unavailable {
        if state.status == Status::Error && schedulable_models && !all_schedulable_blocked {
            state.status = Status::Active;
            state.status_message.clear();
        }
        state.unavailable = false;
        state.next_retry = None;
        return state;
    }

    if had_cooldown && !active_model_cooldown {
        return CooldownState {
            unavailable: false,
            status: Status::Active,
            status_message: String::new(),
            next_retry: None,
        };
    }

    if had_cooldown && schedulable_models && !all_schedulable_blocked {
        if state.status == Status::Error {
            state.status = Status::Active;
            state.status_message.clear();
        }
        state.unavailable = false;
        state.next_retry = None;
        return state;
    }

    clear_past(&mut state);
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pagination(raw: &str) -> Result<Pagination, &'static str> {
        Pagination::parse(&Query::parse(Some(raw)))
    }

    #[test]
    fn pagination_reads_as_upstream() {
        assert!(!pagination("").unwrap().enabled);
        assert_eq!(
            pagination("page=2").unwrap(),
            Pagination {
                enabled: true,
                page: 2,
                page_size: 50
            }
        );
        assert_eq!(pagination("page_size=+7").unwrap().page_size, 7);
        assert_eq!(pagination("page=%201%20").unwrap().page, 1);
        assert_eq!(pagination("page="), Err("page must be a positive integer"));
        assert_eq!(pagination("page=0"), Err("page must be a positive integer"));
        assert_eq!(
            pagination("page=1&page_size=x"),
            Err("page_size must be a positive integer")
        );
    }

    #[test]
    fn bounds_match_upstream() {
        let page = |page, page_size| Pagination {
            enabled: true,
            page,
            page_size,
        };
        assert_eq!(page(1, 2).bounds(5), (0, 2));
        assert_eq!(page(3, 2).bounds(5), (4, 5));
        assert_eq!(page(4, 2).bounds(5), (5, 5));
        assert_eq!(page(2, 5).bounds(5), (5, 5));
        assert_eq!(page(1, 10).bounds(5), (0, 5));
        assert_eq!(page(i64::MAX, i64::MAX).bounds(5), (5, 5));
        assert_eq!(page(1, 2).bounds(0), (0, 0));
    }

    #[test]
    fn priorities_truncate_as_go_does() {
        assert_eq!(float_to_int(2.9), 2);
        assert_eq!(float_to_int(-2.9), -2);
        assert_eq!(float_to_int(1e19), i64::MIN);
        assert_eq!(float_to_int(-1e19), i64::MIN);
    }
}
