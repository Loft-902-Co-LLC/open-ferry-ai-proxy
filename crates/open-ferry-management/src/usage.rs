// Ported from CLIProxyAPI internal/api/handlers/management/usage.go
// (usageQueueRecord.MarshalJSON, GetUsageQueue, parseUsageQueueCount) and
// api_key_usage.go (mergeRecentRequestBuckets, apiKeyUsageProviderKey,
// GetAPIKeyUsage) (v8.0.20, MIT), with Go's encoding/json (appendCompact)
// (go1.27, BSD-3-Clause) as gin writes a `Marshaler`'s output.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! The usage routes: `GET /v0/management/usage-queue` (also
//! `/v8/management/observability/usage/queue`), which takes the oldest
//! queued usage records, and `GET /v0/management/api-key-usage` (also
//! `/v8/management/observability/usage/api-keys`), which counts the calls
//! of each API key credential.
//!
//! `usage-queue` takes up to `count` records (1 unless given), oldest
//! first, and answers them as a JSON array: a record that is valid JSON as
//! itself, compacted and escaped for HTML as Go writes it, any other as a
//! JSON string. A `count` that isn't a positive integer answers 400 and
//! takes nothing. While the queue is off (see the core's usage module) the
//! array is empty.
//!
//! `api-key-usage` answers, for each credential with an API key, its calls
//! that succeeded and failed and its 20 recent request buckets, grouped by
//! provider (the credential's `compat_name` if set, lowercased) and keyed
//! by `base_url|api_key`; credentials sharing a key are summed. Keys are in
//! clear, as upstream's are.
//!
//! Deviations from upstream:
//! - Upstream's `503 core auth manager unavailable` can't happen: the
//!   state always has a manager.
//! - The Redis protocol listener that also serves the queue isn't ported
//!   yet (P3 WP-F).

use std::collections::BTreeMap;

use axum::extract::{RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use chrono::Utc;
use http::{HeaderValue, StatusCode, header};
use open_ferry_core::auth::{Auth, RecentRequestBucket};
use open_ferry_translate::go::{json_valid, to_lower, trim_space};

use crate::Route;
use crate::go::{atoi, equal_fold};
use crate::json::{self, Json};
use crate::query::Query;
use crate::state::ManagementState;

/// The answer to a `count` that isn't a positive integer.
const BAD_COUNT: &str = "count must be a positive integer";

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key("/v0/management/usage-queue", get(usage_queue)),
        Route::key("/v8/management/observability/usage/queue", get(usage_queue)),
        Route::key("/v0/management/api-key-usage", get(api_key_usage)),
        Route::key(
            "/v8/management/observability/usage/api-keys",
            get(api_key_usage),
        ),
    ]
}

/// `GET usage-queue` (upstream's `GetUsageQueue`).
async fn usage_queue(State(state): State<ManagementState>, RawQuery(raw): RawQuery) -> Response {
    let query = Query::parse(raw.as_deref());
    let Some(count) = parse_count(query.value("count")) else {
        return json::error(StatusCode::BAD_REQUEST, BAD_COUNT);
    };
    let records = state.observability().usage.pop_oldest(count);
    let mut response = (StatusCode::OK, records_body(&records)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// How many records to take: 1 when `value` is blank, `None` when it isn't
/// a positive integer (upstream's `parseUsageQueueCount`).
fn parse_count(value: &[u8]) -> Option<usize> {
    let value = trim_space(value);
    if value.is_empty() {
        return Some(1);
    }
    let count = atoi(std::str::from_utf8(value).ok()?)?;
    if count <= 0 {
        return None;
    }
    Some(usize::try_from(count).unwrap_or(usize::MAX))
}

/// `records` as a JSON array, as gin writes upstream's `usageQueueRecord`s.
fn records_body(records: &[Bytes]) -> Vec<u8> {
    let mut out = Vec::with_capacity(records.iter().map(|r| r.len() + 1).sum::<usize>() + 2);
    out.push(b'[');
    for (i, record) in records.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        if json_valid(record) {
            compact_html(&mut out, record);
        } else {
            let mut text = String::new();
            json::write_string(&mut text, record);
            out.extend_from_slice(text.as_bytes());
        }
    }
    out.push(b']');
    out
}

/// Appends `src`, valid JSON, to `out` without the white space between its
/// tokens and with `<`, `>`, `&`, U+2028 and U+2029 written as `\u`
/// escapes (Go's `appendCompact` with `escape`).
fn compact_html(out: &mut Vec<u8>, src: &[u8]) {
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while let Some(&c) = src.get(i) {
        i += 1;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
        } else if matches!(c, b' ' | b'\t' | b'\r' | b'\n') {
            continue;
        } else if c == b'"' {
            in_string = true;
        }
        match c {
            b'<' => out.extend_from_slice(b"\\u003c"),
            b'>' => out.extend_from_slice(b"\\u003e"),
            b'&' => out.extend_from_slice(b"\\u0026"),
            0xe2 if src.get(i) == Some(&0x80) && src.get(i + 1) == Some(&0xa8) => {
                out.extend_from_slice(b"\\u2028");
                i += 2;
            }
            0xe2 if src.get(i) == Some(&0x80) && src.get(i + 1) == Some(&0xa9) => {
                out.extend_from_slice(b"\\u2029");
                i += 2;
            }
            c => out.push(c),
        }
    }
}

/// `GET api-key-usage` (upstream's `GetAPIKeyUsage`).
async fn api_key_usage(State(state): State<ManagementState>) -> Response {
    let now = Utc::now();
    let mut out: BTreeMap<String, BTreeMap<String, KeyUsage>> = BTreeMap::new();
    for auth in state.manager().list() {
        let Some((kind, api_key)) = auth.account_info() else {
            continue;
        };
        let api_key = api_key.trim();
        if !equal_fold(kind.trim(), "api_key") || api_key.is_empty() {
            continue;
        }
        let base_url = [
            auth.attributes.get("base_url"),
            auth.attributes.get("base-url"),
        ]
        .into_iter()
        .flatten()
        .map(|url| url.trim())
        .find(|url| !url.is_empty())
        .unwrap_or_default();
        let key = format!("{base_url}|{api_key}");
        let recent = auth.recent_requests_snapshot(now);
        let bucket = out.entry(provider_key(&auth)).or_default();
        match bucket.get_mut(&key) {
            Some(existing) => {
                existing.success = existing.success.wrapping_add(auth.success);
                existing.failed = existing.failed.wrapping_add(auth.failed);
                merge_recent_requests(&mut existing.recent, recent);
            }
            None => {
                bucket.insert(
                    key,
                    KeyUsage {
                        success: auth.success,
                        failed: auth.failed,
                        recent,
                    },
                );
            }
        }
    }
    let body = out
        .into_iter()
        .map(|(provider, keys)| {
            let keys = keys
                .into_iter()
                .map(|(key, usage)| (key, usage.json()))
                .collect();
            (provider, Json::Map(keys))
        })
        .collect();
    json::response(StatusCode::OK, &Json::Map(body))
}

/// One API key's calls (upstream's `apiKeyUsageEntry`).
struct KeyUsage {
    success: i64,
    failed: i64,
    recent: Vec<RecentRequestBucket>,
}

impl KeyUsage {
    fn json(self) -> Json {
        let recent = self
            .recent
            .into_iter()
            .map(|bucket| {
                Json::Struct(vec![
                    ("time", Json::Str(bucket.time)),
                    ("success", Json::Int(bucket.success)),
                    ("failed", Json::Int(bucket.failed)),
                ])
            })
            .collect();
        Json::Struct(vec![
            ("success", Json::Int(self.success)),
            ("failed", Json::Int(self.failed)),
            ("recent_requests", Json::Array(recent)),
        ])
    }
}

/// Adds the counts of `src`'s buckets to `dst`'s, as far as both go; an
/// empty `dst` takes `src` (upstream's `mergeRecentRequestBuckets`).
fn merge_recent_requests(dst: &mut Vec<RecentRequestBucket>, src: Vec<RecentRequestBucket>) {
    if dst.is_empty() {
        *dst = src;
        return;
    }
    for (into, from) in dst.iter_mut().zip(src) {
        into.success = into.success.wrapping_add(from.success);
        into.failed = into.failed.wrapping_add(from.failed);
    }
}

/// The provider `auth` is grouped under: its `compat_name` if set, else its
/// provider, lowercased, or `unknown` (upstream's `apiKeyUsageProviderKey`).
fn provider_key(auth: &Auth) -> String {
    let compat_name = auth
        .attributes
        .get("compat_name")
        .map(|name| name.trim())
        .unwrap_or_default();
    let provider = if compat_name.is_empty() {
        to_lower(auth.provider.trim())
    } else {
        to_lower(compat_name)
    };
    if provider.is_empty() {
        "unknown".to_owned()
    } else {
        provider
    }
}
