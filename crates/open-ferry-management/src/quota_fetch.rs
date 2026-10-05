// Ported from CLIProxyAPI internal/api/handlers/management/plugin_quota.go
// (credentialQuotaRequest, resolveAuthIndex, FetchCredentialQuota,
// executeQuotaProbe, filterUsableQuotaSummary, parseNumericFraction,
// mapProbeResponse) (v8.0.15, MIT), with golang.org/x/text currency
// (ParseISO) (v0.40.0, BSD-3-Clause) and Go net/http transport.go
// (Transport.roundTrip, validateHeaders) and httpguts
// (ValidHeaderFieldName, ValidHeaderFieldValue) (go1.26, BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! `POST /v0/management/quota/fetch`: a credential's quota, fetched with
//! the declarative probe its credential file describes.
//!
//! The body names the credential by its `auth_index` (or `authIndex`, or
//! `AuthIndex`); a body that isn't an object gives 400 `invalid request
//! body`, no index 400 `auth_index is required`, and an unknown one 404
//! `auth not found`. A credential whose metadata holds a `quota_probe`
//! object with a `url` is probed:
//!
//! - The probe's `method` (`GET` by default), `url`, `data` and `header`
//!   (else `headers`) map make the request. `$TOKEN$` in the URL, the data
//!   or a header value is replaced by the credential's token, as
//!   [`crate::api_call`] looks it up; a credential with none gives 502
//!   without contacting the upstream. A method that isn't an HTTP token, or
//!   a URL Go can't read, leaves the credential unprobed.
//! - The request is sent as [`crate::api_call`] sends one, through the
//!   proxy it would use, with `open-ferry/<version>` as its user agent.
//! - A 2xx response with a JSON body is read with the probe's `mapping`, if
//!   it has one: `plan`, `tier_name` and `tier_id` are gjson paths into
//!   the body, and each of `groups` names a `display_name`, and buckets
//!   either as a `buckets_path` to a list and the keys to read in each item,
//!   or as a list of `buckets` of paths. A bucket needs a remaining
//!   fraction, or a remaining amount and a total above zero.
//! - Without a mapping the body must be in the answer's own shape (read as
//!   [`crate::quota_types`] describes); a bucket is kept only where the
//!   body's own `remainingFraction` (else `remaining_fraction`) is a number,
//!   or a string holding one.
//! - Either way, a `summary` list (the member of that name, else one of the
//!   name in another case) adds each item with a `key`, a `label` and a
//!   numeric `value`; its `unit`, and its `format` when `number`, or
//!   `currency` with an ISO 4217 `currency`, are kept.
//! - The answer needs a plan, a bucket or a summary item; it is 200, with
//!   the offset of the response's `Date` from this clock in
//!   `serverTimeOffsetMs` unless the body set one. Anything else gives 502
//!   `quota probe failed: ` and the reason.
//!
//! Without a probe the answer is 501 `no quota provider available for
//! credential`.
//!
//! The token is never logged, and neither a reason nor an answer shows it:
//! where it appears, however it is written, it is written `$TOKEN$`.
//!
//! Deviations from upstream:
//! - There is no plugin host: `plugin_id` and `provider` are read and
//!   ignored, and only a credential's declarative probe answers.
//! - The probe never sends a header that says which client is calling
//!   (`User-Agent`, `X-App`, `Originator`, session and similar IDs,
//!   `X-Stainless-*`); each is skipped with a warning naming it, and the
//!   user agent is `open-ferry/<version>`. Upstream sends what the probe
//!   says, and otherwise Go's `Go-http-client/1.1`. The probe's `Host`
//!   header is never sent, as upstream's isn't.
//! - An `Accept: */*` header is sent, as [`crate::api_call`] sends it.
//! - A header value outside ASCII fails the request (`probe request failed:
//!   invalid header field value`); upstream sends it.
//! - The probe's headers are applied in the order of the credential file,
//!   so of two names that differ in case alone the later wins, and invalid
//!   ones are reported in the order of their names; upstream's order is
//!   random.
//! - A request and reading its response have a minute, and a body over
//!   16 MiB fails; upstream waits for as long as the request does, and
//!   reads any size.
//! - Tokens are never refreshed or minted, as in [`crate::api_call`].
//! - A failed request's reason is this port's HTTP client's, without Go's
//!   `Get "<url>": ` before it; a failed read's is `failed to read
//!   response`. The URL is read by the `url` crate, as in
//!   [`crate::api_call`].
//! - The token is written `$TOKEN$` in a reason, a response body it
//!   quotes included, and in the text of a successful answer (a plan, a
//!   name, a window and so on); upstream shows it. It is found in any case
//!   of letter, and as a URL or JSON writes it: each character as itself,
//!   percent-encoded, or with a JSON escape (a quote after a backslash, or
//!   a backslash, `u` and four hex digits).
//! - Paths are read as gjson reads them, but a path with a wildcard, pipe,
//!   query, modifier, literal (`!`), sub-selector or `..` finds nothing.
//! - With several members named `summary` in different cases and none in
//!   lower case, the first by name is read; upstream reads any of them.
//! - A zone in an RFC 850 `Date` never moves the time (see
//!   [`http_date`]).
//! - The body is read as [`crate::bind`] reads it: over 16 MiB gives 413,
//!   and an unpaired surrogate escape fails it.

mod gjson;
mod http_date;

use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use axum::response::{IntoResponse as _, Response};
use axum::routing::post;
use bytes::Bytes;
use chrono::Utc;
use http::StatusCode;
use open_ferry_core::auth::Auth;
use open_ferry_providers::is_identity_header;
use open_ferry_translate::go::{json_valid, parse_float_checked, quote, trim_space};
use serde::de::MapAccess;
use serde_json::{Map, Value};

use crate::Route;
use crate::api_call::{self, CallError, Hop, Received, token_value_for_auth, valid_method};
use crate::bind::{self, GoStruct, Nullable, set_string};
use crate::go::{canonical_header_key, equal_fold, lossy, to_upper};
use crate::go_url;
use crate::json::{self, Json};
use crate::proxy;
use crate::quota::auth_by_index;
use crate::quota_types::{
    QuotaBucket, QuotaFetchResponse, QuotaGroup, QuotaMetric, decode_normalized, top_level,
};
use crate::state::ManagementState;
use gjson::Kind;

/// What a probe's URL, data or header value names the credential's token
/// by.
const TOKEN: &str = "$TOKEN$";

/// Why a normalized body gives no answer.
const NO_MATCH: &str =
    "upstream probe response does not match normalized quota shape or declared mapping";

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    vec![Route::key(
        "/v0/management/quota/fetch",
        post(fetch_credential_quota),
    )]
}

/// The body of a fetch (upstream's `credentialQuotaRequest`).
#[derive(Default)]
struct CredentialQuotaRequest {
    auth_index_snake: Option<String>,
    auth_index_camel: Option<String>,
    auth_index_pascal: Option<String>,
}

impl GoStruct for CredentialQuotaRequest {
    const FIELDS: &'static [&'static str] = &[
        "auth_index",
        "authIndex",
        "AuthIndex",
        "plugin_id",
        "provider",
    ];

    fn set<'de, A: MapAccess<'de>>(&mut self, index: usize, map: &mut A) -> Result<(), A::Error> {
        match index {
            0 => self.auth_index_snake = map.next_value::<Nullable>()?.0,
            1 => self.auth_index_camel = map.next_value::<Nullable>()?.0,
            2 => self.auth_index_pascal = map.next_value::<Nullable>()?.0,
            // `plugin_id` and `provider` choose among plugins, which this
            // port hasn't; they must still be strings.
            _ => set_string(&mut String::new(), map)?,
        }
        Ok(())
    }
}

impl CredentialQuotaRequest {
    /// The first index given that isn't blank, trimmed
    /// (`resolveAuthIndex`).
    fn auth_index(&self) -> &str {
        [
            &self.auth_index_snake,
            &self.auth_index_camel,
            &self.auth_index_pascal,
        ]
        .into_iter()
        .flatten()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
    }
}

/// `POST /v0/management/quota/fetch` (upstream's `FetchCredentialQuota`).
async fn fetch_credential_quota(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match bind::read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(request) = bind::decode::<CredentialQuotaRequest>(&body) else {
        return json::error(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let auth_index = request.auth_index();
    if auth_index.is_empty() {
        return json::error(StatusCode::BAD_REQUEST, "auth_index is required");
    }
    let Some(auth) = auth_by_index(state.manager(), auth_index) else {
        return json::error(StatusCode::NOT_FOUND, "auth not found");
    };

    if let Some(Value::Object(probe)) = auth.metadata.get("quota_probe") {
        match execute_quota_probe(&state, &auth, probe).await {
            Probe::Fetched(answer) => return answer_response(&answer),
            Probe::Failed(reason) => {
                let mut message = b"quota probe failed: ".to_vec();
                message.extend_from_slice(&reason);
                let body = Json::map([("error", Json::Bytes(message))]);
                return json::response(StatusCode::BAD_GATEWAY, &body);
            }
            Probe::NotHandled => {}
        }
    }
    json::error(
        StatusCode::NOT_IMPLEMENTED,
        "no quota provider available for credential",
    )
}

/// The 200 answer. Where Go's encoder fails, on an infinite fraction, gin
/// has set the status and content type and writes no body.
fn answer_response(answer: &QuotaFetchResponse) -> Response {
    match answer.to_json() {
        Some(body) => json::response(StatusCode::OK, &body),
        None => (
            StatusCode::OK,
            [(
                CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            )],
        )
            .into_response(),
    }
}

/// How a probe went.
enum Probe {
    /// The probe can't be made: the credential is answered as unprobed.
    NotHandled,
    Fetched(QuotaFetchResponse),
    /// The reason, which may quote the response's body.
    Failed(Vec<u8>),
}

fn failed(reason: impl Into<Vec<u8>>) -> Probe {
    Probe::Failed(reason.into())
}

/// Probes the credential's quota (`executeQuotaProbe`).
async fn execute_quota_probe(
    state: &ManagementState,
    auth: &Auth,
    probe: &Map<String, Value>,
) -> Probe {
    let text = |key: &str| probe.get(key).and_then(Value::as_str).unwrap_or_default();
    let mut url = text("url").trim().to_owned();
    if url.is_empty() {
        return Probe::NotHandled;
    }
    let mut method = to_upper(text("method").trim());
    if method.is_empty() {
        "GET".clone_into(&mut method);
    }
    let mut data = text("data").to_owned();
    let headers = match probe.get("header") {
        Some(Value::Object(headers)) => Some(headers),
        _ => match probe.get("headers") {
            Some(Value::Object(headers)) => Some(headers),
            _ => None,
        },
    };

    let needs_token = url.contains(TOKEN)
        || data.contains(TOKEN)
        || headers
            .into_iter()
            .flatten()
            .any(|(_, value)| value.as_str().is_some_and(|value| value.contains(TOKEN)));
    let token = if needs_token {
        let token = token_value_for_auth(auth);
        if token.is_empty() {
            return failed("probe authentication token not found for credential");
        }
        url = url.replace(TOKEN, &token);
        data = data.replace(TOKEN, &token);
        Some(token)
    } else {
        None
    };

    // Go's NewRequest: a method that isn't a token, or a URL it can't
    // read, leaves the probe unmade.
    let Some(method) = valid_method(&method) else {
        return Probe::NotHandled;
    };
    let Some(mut target) = go_url::parse(url.as_bytes()) else {
        return Probe::NotHandled;
    };
    // NewRequest drops an empty port (removeEmptyPort).
    let last = |byte| target.host.iter().rposition(|&b| b == byte);
    if last(b':').is_some_and(|colon| Some(colon) > last(b']')) && target.host.ends_with(b":") {
        target.host.pop();
    }

    // Go's Header.Set for each string value.
    let mut fields = BTreeMap::new();
    for (key, value) in headers.into_iter().flatten() {
        let Some(value) = value.as_str() else {
            continue;
        };
        let value = match &token {
            Some(token) => value.replace(TOKEN, token),
            None => value.to_owned(),
        };
        fields.insert(canonical_header_key(key), value);
    }

    let hop = Hop {
        method,
        url,
        go: target,
        host: String::new(),
        headers: fields,
        body: (!data.is_empty()).then(|| Bytes::from(data)),
    };
    let outcome = send_probe(state, auth, probe, hop).await;
    let Some(token) = &token else {
        return outcome;
    };
    // What a failure quotes of the response, and what an answer reads from
    // it, may both hold the token.
    let forms = TokenForms::new(token);
    match outcome {
        Probe::Failed(reason) => Probe::Failed(forms.scrub(&reason)),
        Probe::Fetched(mut answer) => {
            forms.answer(&mut answer);
            Probe::Fetched(answer)
        }
        Probe::NotHandled => Probe::NotHandled,
    }
}

/// Sends the probe and reads its answer from the response.
async fn send_probe(
    state: &ManagementState,
    auth: &Auth,
    probe: &Map<String, Value>,
    mut hop: Hop,
) -> Probe {
    // What Go's transport checks before it sends anything (roundTrip).
    let scheme = hop.go.scheme.as_str();
    if !matches!(scheme, "http" | "https") {
        let reason = format!("unsupported protocol scheme {}", quote(scheme));
        return failed(format!("probe request failed: {reason}"));
    }
    for (name, value) in &hop.headers {
        if !valid_field_name(name) {
            let reason = format!("net/http: invalid header field name {}", quote(name));
            return failed(format!("probe request failed: {reason}"));
        }
        if !valid_field_value(value) {
            let reason = format!("net/http: invalid header field value for {}", quote(name));
            return failed(format!("probe request failed: {reason}"));
        }
    }
    if hop.go.host.is_empty() {
        return failed("probe request failed: http: no Host in request URL");
    }

    // Go never sends a Host from the header map; this port never sends a
    // header that names the client.
    hop.headers.remove("Host");
    hop.headers.retain(|name, _| {
        let identity =
            HeaderName::from_bytes(name.as_bytes()).is_ok_and(|header| is_identity_header(&header));
        if identity {
            tracing::warn!("quota probe: header {name:?} would set the client's identity; skipped");
        }
        !identity
    });

    let route = proxy::api_call_route(&state.config(), Some(auth), "");
    let received = match api_call::exchange(state, &route, hop).await {
        Ok(received) => received,
        Err(CallError::Request(reason)) => {
            return failed(format!("probe request failed: {reason}"));
        }
        Err(CallError::Read) => return failed("read probe response: failed to read response"),
    };
    if !(200..300).contains(&received.status) {
        let mut reason = format!("probe returned status {}: ", received.status).into_bytes();
        reason.extend_from_slice(&received.body);
        return Probe::Failed(reason);
    }
    if !json_valid(&received.body) {
        return failed("upstream probe response is not valid JSON");
    }
    let offset = server_offset(&received);
    let mapping = match probe.get("mapping") {
        Some(Value::Object(mapping)) => Some(mapping),
        _ => None,
    };
    read_answer(&received.body, offset, mapping)
}

/// The answer a probe's JSON body gives, `offset` being the server's clock
/// less this one's, in milliseconds.
fn read_answer(body: &[u8], offset: i64, mapping: Option<&Map<String, Value>>) -> Probe {
    let answer = match mapping {
        Some(mapping) => match map_probe_response(body, mapping) {
            Ok(answer) => answer,
            Err(reason) => return failed(format!("probe response mapping failed: {reason}")),
        },
        None => match normalized(body) {
            Some(answer) => answer,
            None => return failed(NO_MATCH),
        },
    };
    let mut answer = answer;
    if answer.server_time_offset_ms == 0 {
        answer.server_time_offset_ms = offset;
    }
    Probe::Fetched(answer)
}

/// The response's `Date` less this clock, in milliseconds, or 0 without a
/// date Go reads.
fn server_offset(received: &Received) -> i64 {
    let date = received
        .headers
        .get("Date")
        .and_then(|values| values.first())
        .map_or(&b""[..], Vec::as_slice);
    if date.is_empty() {
        return 0;
    }
    let Some(date) = http_date::parse_time(date) else {
        return 0;
    };
    let now = Utc::now();
    // Go's Time.Sub saturates; Duration.Milliseconds truncates.
    let saturated = if date < now { i64::MIN } else { i64::MAX };
    (date - now).num_nanoseconds().unwrap_or(saturated) / 1_000_000
}

/// The credential's token, found in what the upstream sends back however it
/// writes it.
///
/// Each character of the token may be written as itself, in either case of
/// an ASCII letter (a URL's scheme and host are written in lower case);
/// percent-encoded, as `%22` in either case of hex digit, or `+` for a
/// space; or as JSON escapes it, as `\"`, `\\`, `\/`, `\n` and the like, or
/// with a backslash, `u` and four hex digits (a surrogate pair beyond the
/// Basic Plane). Each character is taken as it comes, so a URL that encodes
/// a quote and leaves a backslash, or a JSON string that quotes such a URL,
/// is found as a token written one way throughout is.
struct TokenForms {
    /// For each character of the token, the ways it may be written, in lower
    /// case.
    chars: Vec<Vec<Vec<u8>>>,
    /// The bytes a way to write the token's first character may start with,
    /// in either case.
    first: [bool; 256],
}

impl TokenForms {
    fn new(token: &str) -> Self {
        let chars: Vec<Vec<Vec<u8>>> = token.chars().map(spellings).collect();
        let mut first = [false; 256];
        for byte in chars
            .first()
            .into_iter()
            .flatten()
            .filter_map(|way| way.first())
        {
            for byte in [byte.to_ascii_lowercase(), byte.to_ascii_uppercase()] {
                if let Some(slot) = first.get_mut(usize::from(byte)) {
                    *slot = true;
                }
            }
        }
        Self { chars, first }
    }

    /// `text` with each time the token is written, written `$TOKEN$`.
    fn scrub(&self, text: &[u8]) -> Vec<u8> {
        if self.chars.is_empty() {
            return text.to_vec();
        }
        let mut out = Vec::with_capacity(text.len());
        let mut at = 0;
        while let Some(&byte) = text.get(at) {
            let starts = self.first.get(usize::from(byte)).is_some_and(|&hit| hit);
            match starts.then(|| self.written_at(text, at)).flatten() {
                Some(end) => {
                    out.extend_from_slice(TOKEN.as_bytes());
                    at = end;
                }
                None => {
                    out.push(byte);
                    at += 1;
                }
            }
        }
        out
    }

    /// Where the longest writing of the token that starts at `start` ends.
    fn written_at(&self, text: &[u8], start: usize) -> Option<usize> {
        // Where the characters so far may end: more than one place where
        // one way to write a character begins another's.
        let mut ends = vec![start];
        for ways in &self.chars {
            let mut next = Vec::new();
            for &at in &ends {
                let rest = text.get(at..).unwrap_or_default();
                for way in ways {
                    let found = rest
                        .get(..way.len())
                        .is_some_and(|head| head.eq_ignore_ascii_case(way));
                    if found && !next.contains(&(at + way.len())) {
                        next.push(at + way.len());
                    }
                }
            }
            if next.is_empty() {
                return None;
            }
            ends = next;
        }
        ends.into_iter().max()
    }

    fn field(&self, field: &mut Vec<u8>) {
        *field = self.scrub(field);
    }

    /// The text of a successful answer, scrubbed.
    fn answer(&self, answer: &mut QuotaFetchResponse) {
        if let Some(subscription) = &mut answer.subscription {
            self.field(&mut subscription.plan);
            self.field(&mut subscription.tier_name);
            self.field(&mut subscription.tier_id);
        }
        for metric in &mut answer.summary {
            self.field(&mut metric.key);
            self.field(&mut metric.label);
            self.field(&mut metric.unit);
            self.field(&mut metric.format);
            self.field(&mut metric.currency);
        }
        for group in &mut answer.groups {
            self.field(&mut group.display_name);
            for bucket in &mut group.buckets {
                self.field(&mut bucket.window);
                self.field(&mut bucket.reset_time);
                self.field(&mut bucket.description);
            }
        }
    }
}

/// The ways `c` may be written in a token's quotation, in lower case (see
/// [`TokenForms`]).
fn spellings(c: char) -> Vec<Vec<u8>> {
    let mut ways: Vec<Vec<u8>> = Vec::new();
    let mut add = |way: Vec<u8>| {
        let way = way.to_ascii_lowercase();
        if !ways.contains(&way) {
            ways.push(way);
        }
    };
    let mut buffer = [0; 4];
    let utf8 = c.encode_utf8(&mut buffer).as_bytes();
    add(utf8.to_vec());
    add(utf8
        .iter()
        .flat_map(|byte| format!("%{byte:02x}").into_bytes())
        .collect());
    if c == ' ' {
        add(b"+".to_vec());
    }
    match c {
        '"' | '\\' | '/' => add(format!("\\{c}").into_bytes()),
        '\u{8}' => add(b"\\b".to_vec()),
        '\u{c}' => add(b"\\f".to_vec()),
        '\n' => add(b"\\n".to_vec()),
        '\r' => add(b"\\r".to_vec()),
        '\t' => add(b"\\t".to_vec()),
        _ => {}
    }
    let mut units = [0; 2];
    add(c
        .encode_utf16(&mut units)
        .iter()
        .flat_map(|unit| format!("\\u{unit:04x}").into_bytes())
        .collect());
    ways
}

/// Whether `name` is an HTTP token (`httpguts.ValidHeaderFieldName`).
fn valid_field_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Whether `value` has no control character but tabs
/// (`httpguts.ValidHeaderFieldValue`).
fn valid_field_value(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b == b'\t' || (b >= b' ' && b != 0x7f))
}

/// The answer from a body in the answer's own shape: its buckets kept only
/// where the body gives a number for them, and its summary
/// (`executeQuotaProbe`'s normalized path, the offset aside).
fn normalized(body: &[u8]) -> Option<QuotaFetchResponse> {
    let mut answer = decode_normalized(body)?;
    let has_plan = answer
        .subscription
        .as_ref()
        .is_some_and(|sub| !trim_space(&sub.plan).is_empty());
    let mut groups = Vec::new();
    let raw_groups = gjson::get(body, "groups");
    if raw_groups.is_array() {
        for (index, raw_group) in raw_groups.array().iter().enumerate() {
            let Some(group) = answer.groups.get(index) else {
                break;
            };
            let raw_buckets = raw_group.get("buckets");
            if !raw_buckets.is_array() {
                continue;
            }
            let mut buckets = Vec::new();
            for (index, raw_bucket) in raw_buckets.array().iter().enumerate() {
                let Some(bucket) = group.buckets.get(index) else {
                    break;
                };
                let mut fraction = raw_bucket.get("remainingFraction");
                if !fraction.exists() {
                    fraction = raw_bucket.get("remaining_fraction");
                }
                if let Some(fraction) = parse_numeric_fraction(&fraction) {
                    let mut bucket = bucket.clone();
                    bucket.remaining_fraction = fraction;
                    buckets.push(bucket);
                }
            }
            if !buckets.is_empty() {
                groups.push(QuotaGroup {
                    display_name: group.display_name.clone(),
                    buckets,
                });
            }
        }
    }
    answer.groups = groups;
    answer.summary = filter_usable_quota_summary(body);
    (has_plan || !answer.groups.is_empty() || !answer.summary.is_empty()).then_some(answer)
}

/// A number, or a string holding one, if finite (`parseNumericFraction`).
fn parse_numeric_fraction(value: &gjson::Value<'_>) -> Option<f64> {
    if !value.exists() {
        return None;
    }
    match value.kind {
        Kind::Number => Some(value.num).filter(|num| num.is_finite()),
        Kind::String => {
            let text = value.string();
            let text = trim_space(&text);
            if text.is_empty() {
                return None;
            }
            std::str::from_utf8(text)
                .ok()
                .and_then(parse_float_checked)
                .filter(|num| num.is_finite())
        }
        _ => None,
    }
}

/// The usable items of the body's `summary` list
/// (`filterUsableQuotaSummary`).
fn filter_usable_quota_summary(body: &[u8]) -> Vec<QuotaMetric> {
    let Some(members) = top_level(body) else {
        return Vec::new();
    };
    let raw = members.get("summary").copied().or_else(|| {
        members
            .iter()
            .find(|(key, _)| equal_fold(key, "summary"))
            .map(|(_, value)| *value)
    });
    let summary = gjson::parse(raw.unwrap_or_default());
    if !summary.is_array() {
        return Vec::new();
    }
    let mut usable = Vec::new();
    for item in summary.array() {
        let key_result = item.get("key");
        let label_result = item.get("label");
        let key = trim_space(&key_result.string()).to_vec();
        let label = trim_space(&label_result.string()).to_vec();
        let value = item.get("value");
        if key_result.kind != Kind::String
            || label_result.kind != Kind::String
            || key.is_empty()
            || label.is_empty()
            || value.kind != Kind::Number
            || !value.num.is_finite()
        {
            continue;
        }
        let mut metric = QuotaMetric {
            key,
            label,
            value: value.num,
            ..QuotaMetric::default()
        };
        let unit = item.get("unit");
        if unit.kind == Kind::String {
            metric.unit = trim_space(&unit.string()).to_vec();
        }
        let format = item.get("format");
        if format.kind == Kind::String {
            match trim_space(&format.string()) {
                b"number" => metric.format = b"number".to_vec(),
                b"currency" => {
                    let currency = item.get("currency");
                    if currency.kind == Kind::String {
                        let code = to_upper(&lossy(trim_space(&currency.string())));
                        if ISO_CURRENCIES.binary_search(&code.as_bytes()).is_ok() {
                            metric.format = b"currency".to_vec();
                            metric.currency = code.into_bytes();
                        }
                    }
                }
                _ => {}
            }
        }
        usable.push(metric);
    }
    usable
}

/// The answer the probe's `mapping` reads from the body
/// (`mapProbeResponse`).
fn map_probe_response(
    body: &[u8],
    mapping: &Map<String, Value>,
) -> Result<QuotaFetchResponse, &'static str> {
    let path = |key: &str| {
        mapping
            .get(key)
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
    };
    // What a path names, as text, if it isn't blank.
    let found = |path: &str| {
        let result = gjson::get(body, path);
        let text = result.string();
        (result.exists() && !trim_space(&text).is_empty()).then(|| text.into_owned())
    };
    let mut answer = QuotaFetchResponse::default();
    if let Some(plan) = path("plan").and_then(found) {
        answer.subscription.get_or_insert_default().plan = plan;
    }
    if let Some(tier) = path("tier_name")
        .or_else(|| path("tierName"))
        .and_then(found)
    {
        answer.subscription.get_or_insert_default().tier_name = tier;
    }
    if let Some(tier) = path("tier_id").or_else(|| path("tierId")).and_then(found) {
        answer.subscription.get_or_insert_default().tier_id = tier;
    }

    if let Some(Value::Array(groups)) = mapping.get("groups") {
        for spec in groups.iter().filter_map(Value::as_object) {
            let mut group = QuotaGroup::default();
            let text = |key: &str| spec.get(key).and_then(Value::as_str);
            if let Some(name) = text("display_name").or_else(|| text("displayName")) {
                group.display_name = found(name).unwrap_or_else(|| name.as_bytes().to_vec());
            }
            if let Some(items_path) = text("buckets_path").filter(|path| !path.is_empty()) {
                list_buckets(body, spec, items_path, &mut group.buckets);
            }
            if let Some(Value::Array(buckets)) = spec.get("buckets") {
                for bucket in buckets.iter().filter_map(Value::as_object) {
                    group.buckets.extend(path_bucket(body, bucket));
                }
            }
            if !group.buckets.is_empty() {
                answer.groups.push(group);
            }
        }
    }

    let has_plan = answer
        .subscription
        .as_ref()
        .is_some_and(|sub| !trim_space(&sub.plan).is_empty());
    answer.summary = filter_usable_quota_summary(body);
    if answer.groups.is_empty() && !has_plan && answer.summary.is_empty() {
        return Err("response mapping did not match any valid quota fields in upstream response");
    }
    Ok(answer)
}

/// The buckets of the list `items_path` names, read with the keys `spec`
/// gives.
fn list_buckets(
    body: &[u8],
    spec: &Map<String, Value>,
    items_path: &str,
    buckets: &mut Vec<QuotaBucket>,
) {
    let items = gjson::get(body, items_path);
    let list = items.array();
    if !items.is_array() || list.is_empty() {
        return;
    }
    let key = |name: &str, default: &'static str| {
        spec.get(name)
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .unwrap_or(default)
    };
    let window_key = key("window_key", "window");
    let fraction_key = key("remaining_fraction_key", "remaining_fraction");
    let remaining_key = key("remaining_amount_key", "");
    let total_key = key("total_amount_key", "");
    let reset_key = key("reset_time_key", "reset_time");
    let description_key = key("description_key", "description");
    for item in &list {
        let mut fraction = parse_numeric_fraction(&item.get(fraction_key));
        if fraction.is_none() && !remaining_key.is_empty() && !total_key.is_empty() {
            fraction = ratio(
                parse_numeric_fraction(&item.get(remaining_key)),
                parse_numeric_fraction(&item.get(total_key)),
            );
        }
        let Some(fraction) = fraction else {
            continue;
        };
        buckets.push(QuotaBucket {
            window: item.get(window_key).string().into_owned(),
            remaining_fraction: fraction,
            reset_time: item.get(reset_key).string().into_owned(),
            description: item.get(description_key).string().into_owned(),
        });
    }
}

/// A bucket given as paths into the body, if it has a fraction.
fn path_bucket(body: &[u8], spec: &Map<String, Value>) -> Option<QuotaBucket> {
    let text = |key: &str| spec.get(key).and_then(Value::as_str);
    let path = |key: &str| text(key).filter(|path| !path.is_empty());
    let number = |path: &str| parse_numeric_fraction(&gjson::get(body, path));
    let mut fraction = path("remaining_fraction").and_then(number);
    if fraction.is_none()
        && let (Some(remaining), Some(total)) = (path("remaining_amount"), path("total_amount"))
    {
        fraction = ratio(number(remaining), number(total));
    }
    // What a path names, else the path itself.
    let read = |key: &str| {
        text(key)
            .map(|path| {
                let result = gjson::get(body, path);
                if result.exists() {
                    result.string().into_owned()
                } else {
                    path.as_bytes().to_vec()
                }
            })
            .unwrap_or_default()
    };
    Some(QuotaBucket {
        remaining_fraction: fraction?,
        window: read("window"),
        description: read("description"),
        reset_time: read("reset_time"),
    })
}

/// A remaining amount over a total above zero.
fn ratio(remaining: Option<f64>, total: Option<f64>) -> Option<f64> {
    match (remaining, total) {
        (Some(remaining), Some(total)) if total > 0.0 => Some(remaining / total),
        _ => None,
    }
}

/// The codes golang.org/x/text/currency's `ParseISO` accepts, in order.
const ISO_CURRENCIES: [&[u8]; 300] = [
    b"ADP", b"AED", b"AFA", b"AFN", b"ALK", b"ALL", b"AMD", b"ANG", b"AOA", b"AOK", b"AON", b"AOR",
    b"ARA", b"ARL", b"ARM", b"ARP", b"ARS", b"ATS", b"AUD", b"AWG", b"AZM", b"AZN", b"BAD", b"BAM",
    b"BAN", b"BBD", b"BDT", b"BEC", b"BEF", b"BEL", b"BGL", b"BGM", b"BGN", b"BGO", b"BHD", b"BIF",
    b"BMD", b"BND", b"BOB", b"BOL", b"BOP", b"BOV", b"BRB", b"BRC", b"BRE", b"BRL", b"BRN", b"BRR",
    b"BRZ", b"BSD", b"BTN", b"BUK", b"BWP", b"BYB", b"BYN", b"BYR", b"BZD", b"CAD", b"CDF", b"CHE",
    b"CHF", b"CHW", b"CLE", b"CLF", b"CLP", b"CNH", b"CNX", b"CNY", b"COP", b"COU", b"CRC", b"CSD",
    b"CSK", b"CUC", b"CUP", b"CVE", b"CYP", b"CZK", b"DDM", b"DEM", b"DJF", b"DKK", b"DOP", b"DZD",
    b"ECS", b"ECV", b"EEK", b"EGP", b"ERN", b"ESA", b"ESB", b"ESP", b"ETB", b"EUR", b"FIM", b"FJD",
    b"FKP", b"FRF", b"GBP", b"GEK", b"GEL", b"GHC", b"GHS", b"GIP", b"GMD", b"GNF", b"GNS", b"GQE",
    b"GRD", b"GTQ", b"GWE", b"GWP", b"GYD", b"HKD", b"HNL", b"HRD", b"HRK", b"HTG", b"HUF", b"IDR",
    b"IEP", b"ILP", b"ILR", b"ILS", b"INR", b"IQD", b"IRR", b"ISJ", b"ISK", b"ITL", b"JMD", b"JOD",
    b"JPY", b"KES", b"KGS", b"KHR", b"KMF", b"KPW", b"KRH", b"KRO", b"KRW", b"KWD", b"KYD", b"KZT",
    b"LAK", b"LBP", b"LKR", b"LRD", b"LSL", b"LTL", b"LTT", b"LUC", b"LUF", b"LUL", b"LVL", b"LVR",
    b"LYD", b"MAD", b"MAF", b"MCF", b"MDC", b"MDL", b"MGA", b"MGF", b"MKD", b"MKN", b"MLF", b"MMK",
    b"MNT", b"MOP", b"MRO", b"MTL", b"MTP", b"MUR", b"MVP", b"MVR", b"MWK", b"MXN", b"MXP", b"MXV",
    b"MYR", b"MZE", b"MZM", b"MZN", b"NAD", b"NGN", b"NIC", b"NIO", b"NLG", b"NOK", b"NPR", b"NZD",
    b"OMR", b"PAB", b"PEI", b"PEN", b"PES", b"PGK", b"PHP", b"PKR", b"PLN", b"PLZ", b"PTE", b"PYG",
    b"QAR", b"RHD", b"ROL", b"RON", b"RSD", b"RUB", b"RUR", b"RWF", b"SAR", b"SBD", b"SCR", b"SDD",
    b"SDG", b"SDP", b"SEK", b"SGD", b"SHP", b"SIT", b"SKK", b"SLL", b"SOS", b"SRD", b"SRG", b"SSP",
    b"STD", b"STN", b"SUR", b"SVC", b"SYP", b"SZL", b"THB", b"TJR", b"TJS", b"TMM", b"TMT", b"TND",
    b"TOP", b"TPE", b"TRL", b"TRY", b"TTD", b"TWD", b"TZS", b"UAH", b"UAK", b"UGS", b"UGX", b"USD",
    b"USN", b"USS", b"UYI", b"UYP", b"UYU", b"UZS", b"VEB", b"VEF", b"VND", b"VNN", b"VUV", b"WST",
    b"XAF", b"XAG", b"XAU", b"XBA", b"XBB", b"XBC", b"XBD", b"XCD", b"XDR", b"XEU", b"XFO", b"XFU",
    b"XOF", b"XPD", b"XPF", b"XPT", b"XRE", b"XSU", b"XTS", b"XUA", b"XXX", b"YDD", b"YER", b"YUD",
    b"YUM", b"YUN", b"YUR", b"ZAL", b"ZAR", b"ZMK", b"ZMW", b"ZRN", b"ZRZ", b"ZWD", b"ZWL", b"ZWR",
];

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the ISO list is in order, for the binary search.
    #[test]
    fn iso_codes_are_sorted() {
        assert!(ISO_CURRENCIES.windows(2).all(|pair| pair[0] < pair[1]));
        for code in [&b"USD"[..], b"XXX", b"XTS", b"ADP", b"ZWR"] {
            assert!(ISO_CURRENCIES.binary_search(&code).is_ok());
        }
        for code in [&b"US"[..], b"USDX", b"usd", b"AAA"] {
            assert!(ISO_CURRENCIES.binary_search(&code).is_err());
        }
    }

    fn scrubbed(text: &str, token: &str) -> String {
        String::from_utf8(TokenForms::new(token).scrub(text.as_bytes())).expect("utf-8")
    }

    // Not upstream's: a reason never shows the token, in any case the
    // scheme gives it.
    #[test]
    fn scrub_hides_the_token() {
        assert_eq!(
            scrubbed("x Sec-Ret y sec-ret Sec-Ret SEC-RET", "Sec-Ret"),
            "x $TOKEN$ y $TOKEN$ $TOKEN$ $TOKEN$"
        );
        assert_eq!(scrubbed("aaa", "aa"), "$TOKEN$a");
        assert_eq!(scrubbed("aaa", ""), "aaa");
        assert_eq!(scrubbed("ab", "abc"), "ab");
    }

    // Not upstream's: nor does it show the token as a JSON string or a URL
    // writes it, character by character.
    #[test]
    fn scrub_hides_the_token_as_escaped() {
        let token = "s-\"b\\k /é😀\n";
        // JSON's escape of a UTF-16 unit: a backslash, `u` and four hex
        // digits.
        let u = |hex: &str| format!("\\u{hex}");
        let ascii = format!(
            "s-{}b{}k {}{}{}{}{}",
            u("0022"),
            u("005C"),
            u("002f"),
            u("00e9"),
            u("d83d"),
            u("DE00"),
            u("000a")
        );
        for written in [
            // As is, and as JSON writes it.
            token.to_owned(),
            r#"s-\"b\\k \/é😀\n"#.to_owned(),
            // ASCII only, with surrogates, in either case of hex digit.
            ascii,
            // As a URL writes it, with a space as a plus and as `%20`, in
            // either case.
            "s-%22b%5Ck+%2F%C3%A9%F0%9F%98%80%0A".to_owned(),
            "s-%22b%5ck%20%2f%c3%a9%f0%9f%98%80%0a".to_owned(),
            // Mixed: a quote encoded and a backslash not, in JSON.
            r#"s-%22b\\k+/é%F0%9F%98%80\n"#.to_owned(),
        ] {
            assert_eq!(
                scrubbed(&format!("<{written}> and {written}"), token),
                "<$TOKEN$> and $TOKEN$",
                "{written}"
            );
        }
        // Neither half of it alone.
        assert_eq!(scrubbed(r#"s-\"b"#, token), r#"s-\"b"#);
    }

    // Not upstream's: where a way to write one character starts another's,
    // the token is still found.
    #[test]
    fn scrub_finds_the_token_where_writings_overlap() {
        assert_eq!(scrubbed("a%25b", "a%b"), "$TOKEN$");
        assert_eq!(scrubbed("a%b", "a%b"), "$TOKEN$");
        assert_eq!(
            scrubbed("a%2525b and a%25b", "a%25b"),
            "$TOKEN$ and $TOKEN$"
        );
    }
}
