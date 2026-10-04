// Ported from CLIProxyAPI internal/redisqueue/plugin.go (HandleUsage,
// queuedUsageDetail, requestDetail, tokenStats, failDetail, resolveFail)
// and sdk/cliproxy/session/identity.go (NormalizeToCanonicalUUID's UUID
// case) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A usage record as the usage queue holds it: one JSON object per
//! executor call, with its timing, its tokens, its outcome, the credential
//! and the client.
//!
//! The fields come in upstream's order. Blank names become `unknown`, a
//! blank alias the model, and a blank trace ID the request's ID; the token
//! breakdown is filled in for the provider when the record has none. A
//! call that didn't fail has `fail` `{"status_code":200,"body":""}`; one
//! that failed without a status has 500.
//!
//! Deviations from upstream:
//! - `timestamp` is in UTC; upstream writes the local time with its offset.
//! - `failed` is the call's own outcome. Upstream also counts a call failed
//!   when the status sent to the client was 400 or more by the time the
//!   record is handled, and takes a failure's missing status from it.
//! - `session_id` is a session header the client sent, as sent: a UUID is
//!   written in lower case, anything else as it is. Upstream also derives
//!   one from the request, strips known prefixes and projects anything not
//!   a UUID onto a version 8 UUID; open-ferry never derives an identity
//!   (policy). `parent_session_id`, `node_kind`, `is_fork` and
//!   `is_compaction` are never written, for the same reason.
//! - An `execution_id` made here is a version 7 UUID; upstream's is
//!   version 4.

use std::fmt::Write as _;
use std::time::Duration;

use chrono::{DateTime, Utc};
use open_ferry_translate::go::json_string;

use super::accounting::{
    Detail, InputBreakdown, OutputBreakdown, TOKEN_ACCOUNTING_SCHEMA_VERSION, TokenBreakdown,
    ensure_token_breakdown_for_provider,
};

/// What a usage record says, before the blanks are filled in. It holds the
/// client's key and the credential's account in clear, as upstream's does:
/// it has no `Debug`.
#[derive(Clone, Default)]
pub(crate) struct Record {
    /// The executor call's own ID.
    pub(crate) execution_id: String,
    /// The ID of the request the call was made for.
    pub(crate) request_id: String,
    /// The trace the call belongs to; the request's ID when blank.
    pub(crate) trace_id: String,
    pub(crate) provider: String,
    pub(crate) executor_type: String,
    /// The model sent upstream.
    pub(crate) model: String,
    /// The model as the client named it.
    pub(crate) alias: String,
    /// The account or key the credential belongs to.
    pub(crate) source: String,
    /// The key the client authenticated with.
    pub(crate) api_key: String,
    /// The session header the client sent, or empty.
    pub(crate) session_id: String,
    pub(crate) auth_index: String,
    /// The SHA-256 of the credential's access token, in hex, or empty.
    pub(crate) access_token_sha256: String,
    pub(crate) auth_type: String,
    pub(crate) reasoning_effort: String,
    /// The service tier the client asked for.
    pub(crate) service_tier: String,
    /// The model the answer said it served, or empty.
    pub(crate) response_model: String,
    pub(crate) generate: bool,
    pub(crate) stream: bool,
    /// When the call started; now when unknown.
    pub(crate) requested_at: Option<DateTime<Utc>>,
    pub(crate) latency: Duration,
    pub(crate) ttft: Duration,
    pub(crate) failed: bool,
    /// The failure's status, or 0 for none.
    pub(crate) fail_status: i64,
    /// The failure's message, scrubbed of the call's secrets.
    pub(crate) fail_body: String,
    pub(crate) detail: Detail,
    /// The latest upstream answer's headers: canonical names in order,
    /// credentials masked.
    pub(crate) response_headers: Vec<(String, Vec<String>)>,
    /// The method and route of the request.
    pub(crate) endpoint: String,
    pub(crate) client_ip: String,
    pub(crate) resolved_client_ip: String,
    pub(crate) forwarded_for: String,
    pub(crate) user_agent: String,
}

/// `value` trimmed, or `fallback` when that leaves nothing.
fn or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    match value.trim() {
        "" => fallback,
        trimmed => trimmed,
    }
}

/// A time as Go's `MarshalJSON` writes a UTC time: RFC 3339 with up to
/// nine digits of fraction, trailing zeros dropped.
pub(crate) fn go_time(time: DateTime<Utc>) -> String {
    let mut out = time.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = time.timestamp_subsec_nanos().min(999_999_999);
    if nanos != 0 {
        let fraction = format!("{nanos:09}");
        out.push('.');
        out.push_str(fraction.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

/// `id` as the record writes a session: trimmed, and in lower case when it
/// is a UUID in its usual form (upstream's `NormalizeToCanonicalUUID`,
/// without its derivations).
pub(crate) fn normalize_session_id(id: &str) -> String {
    let id = id.trim();
    if is_canonical_uuid(id) {
        id.to_ascii_lowercase()
    } else {
        id.to_owned()
    }
}

/// Whether `id` is 8-4-4-4-12 hex digits (upstream's
/// `canonicalUUIDPattern`).
fn is_canonical_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Whole milliseconds, as Go's `Duration.Milliseconds` truncates.
fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// A JSON object written field by field.
struct Object {
    out: String,
    empty: bool,
}

impl Object {
    fn new() -> Self {
        Self {
            out: String::from("{"),
            empty: true,
        }
    }

    fn key(&mut self, name: &str) -> &mut String {
        if !self.empty {
            self.out.push(',');
        }
        self.empty = false;
        self.out.push('"');
        self.out.push_str(name);
        self.out.push_str("\":");
        &mut self.out
    }

    fn str(&mut self, name: &str, value: &str) {
        let encoded = json_string(value);
        self.key(name).push_str(&encoded);
    }

    /// A string field, left out when empty (`omitempty`).
    fn str_omitempty(&mut self, name: &str, value: &str) {
        if !value.is_empty() {
            self.str(name, value);
        }
    }

    fn int(&mut self, name: &str, value: i64) {
        let out = self.key(name);
        let _ = write!(out, "{value}");
    }

    fn bool(&mut self, name: &str, value: bool) {
        self.key(name)
            .push_str(if value { "true" } else { "false" });
    }

    fn raw(&mut self, name: &str, value: &str) {
        self.key(name).push_str(value);
    }

    fn finish(mut self) -> String {
        self.out.push('}');
        self.out
    }
}

/// The `token_breakdown` object.
fn breakdown_json(breakdown: &TokenBreakdown) -> String {
    let InputBreakdown {
        total_tokens: input_total,
        uncached_tokens,
        cache_read_tokens,
        cache_write_tokens,
    } = breakdown.input;
    let OutputBreakdown {
        total_tokens: output_total,
        non_reasoning_tokens,
        reasoning_tokens,
    } = breakdown.output;
    let mut input = Object::new();
    input.int("total_tokens", input_total);
    input.int("uncached_tokens", uncached_tokens);
    input.int("cache_read_tokens", cache_read_tokens);
    input.int("cache_write_tokens", cache_write_tokens);
    let mut output = Object::new();
    output.int("total_tokens", output_total);
    output.int("non_reasoning_tokens", non_reasoning_tokens);
    output.int("reasoning_tokens", reasoning_tokens);

    let mut object = Object::new();
    object.int("schema_version", breakdown.schema_version);
    object.str("quality", breakdown.quality.as_str());
    object.int("total_tokens", breakdown.total_tokens);
    object.raw("input", &input.finish());
    object.raw("output", &output.finish());
    object.int("unclassified_tokens", breakdown.unclassified_tokens);
    object.finish()
}

/// The `response_headers` object: each name's values as an array, in the
/// order given.
fn headers_json(headers: &[(String, Vec<String>)]) -> String {
    let mut out = String::from("{");
    for (i, (name, values)) in headers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(name));
        out.push_str(":[");
        for (j, value) in values.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&json_string(value));
        }
        out.push(']');
    }
    out.push('}');
    out
}

impl Record {
    /// The record as the queue holds it (upstream's `HandleUsage`, from
    /// its blanks filled in to its `json.Marshal`).
    pub(crate) fn encode(&self) -> String {
        let model = or(&self.model, "unknown");
        let alias = or(&self.alias, model);
        let provider = or(&self.provider, "unknown");
        let executor_type = or(&self.executor_type, "unknown");
        let auth_type = or(&self.auth_type, "unknown");
        let request_id = self.request_id.trim();
        let trace_id = or(&self.trace_id, request_id);
        let generated;
        let execution_id = match self.execution_id.trim() {
            "" => {
                generated = uuid::Uuid::now_v7().to_string();
                generated.as_str()
            }
            id => id,
        };
        let session_id = normalize_session_id(&self.session_id);
        let detail = ensure_token_breakdown_for_provider(
            self.detail.clone(),
            &self.provider,
            &self.executor_type,
        );
        let (fail_status, fail_body) = if self.failed {
            let status = if self.fail_status <= 0 {
                500
            } else {
                self.fail_status
            };
            (status, self.fail_body.trim())
        } else {
            (200, "")
        };
        let timestamp = self.requested_at.unwrap_or_else(Utc::now);

        let mut tokens = Object::new();
        tokens.int("input_tokens", detail.input_tokens);
        tokens.int("output_tokens", detail.output_tokens);
        tokens.int("reasoning_tokens", detail.reasoning_tokens);
        tokens.int("cached_tokens", detail.cached_tokens);
        tokens.int("cache_read_tokens", detail.cache_read_tokens);
        tokens.bool("cache_read_tokens_present", true);
        tokens.int("cache_creation_tokens", detail.cache_creation_tokens);
        tokens.int("total_tokens", detail.total_tokens);

        let mut fail = Object::new();
        fail.int("status_code", fail_status);
        fail.str("body", fail_body);

        let mut object = Object::new();
        object.str("timestamp", &go_time(timestamp));
        object.int("latency_ms", millis(self.latency));
        object.int("ttft_ms", millis(self.ttft));
        object.str("source", &self.source);
        object.str("auth_index", &self.auth_index);
        object.str_omitempty("access_token_sha256", &self.access_token_sha256);
        object.str("client_ip", &self.client_ip);
        object.str("resolved_client_ip", &self.resolved_client_ip);
        object.str("x_forwarded_for", &self.forwarded_for);
        object.str("user_agent", &self.user_agent);
        object.raw("tokens", &tokens.finish());
        object.bool("failed", self.failed);
        object.bool("generate", self.generate);
        object.bool("stream", self.stream);
        object.raw("fail", &fail.finish());
        if !self.response_headers.is_empty() {
            object.raw("response_headers", &headers_json(&self.response_headers));
        }
        object.int("accounting_version", TOKEN_ACCOUNTING_SCHEMA_VERSION);
        object.raw("token_breakdown", &breakdown_json(&detail.token_breakdown));
        object.str("provider", provider);
        object.str("executor_type", executor_type);
        object.str("model", model);
        object.str("alias", alias);
        object.str("endpoint", self.endpoint.trim());
        object.str("auth_type", auth_type);
        object.str("api_key", self.api_key.trim());
        object.str("request_id", request_id);
        object.str_omitempty("execution_id", execution_id);
        object.str_omitempty("trace_id", trace_id);
        object.str_omitempty("session_id", &session_id);
        object.str("reasoning_effort", self.reasoning_effort.trim());
        object.str("service_tier", self.service_tier.trim());
        object.str_omitempty("response_service_tier", detail.response_service_tier.trim());
        object.str_omitempty("response_model", self.response_model.trim());
        object.finish()
    }
}
