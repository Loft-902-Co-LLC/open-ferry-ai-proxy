// Ported from CLIProxyAPI sdk/cliproxy/auth/cooldown_state.go
// (CooldownStateRecord, cooldownStateFile, readCooldownStateFile and the
// marshaling in writeCooldownStateGroup) (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A saved cooldown, and the JSON of the `.cds` file that holds one
//! credential's.
//!
//! A file is upstream's envelope, written as Go's `json.MarshalIndent`
//! with a two-space indent writes it:
//!
//! ```json
//! {
//!   "version": 1,
//!   "auth_id": "auth-1",
//!   "provider": "xai",
//!   "updated_at": "2026-06-01T00:00:00Z",
//!   "records": [ ... ]
//! }
//! ```
//!
//! and each record has `provider`, `auth_id`, `model`, `status`,
//! `next_retry_after`, `reason`, `quota` and `last_error`, and
//! `updated_at`, with upstream's field names, order and `omitempty`s. A
//! record whose quota rest open-ferry's `routing.quota.check-after` capped
//! ends with that rest's `open_ferry_quota_check`:
//!
//! ```json
//! "open_ferry_quota_check": {
//!   "wait_seconds": 7200,
//!   "check_at": "2026-06-01T03:00:00Z",
//!   "reset_at": "2026-06-05T00:00:00Z"
//! }
//! ```
//!
//! the rest's wait, in whole seconds, its next check and the provider's
//! reset (see `quota_check`). A credential's own record may also end with
//! `open_ferry_recovery_refused_until`, the time (or the string `"unknown"`)
//! until which a merge of two models' cooldowns keeps a usage read from ending
//! any of them (see `quota_recovery`). While the refusal stands and the
//! credential has no cooldown of its own, it is written alone on a record with
//! the zero `next_retry_after`, which upstream's restore skips.
//! Reading takes the fields as Go's decoder does: a key matches its field
//! whatever its case, unknown keys are ignored, `null` leaves a field as it
//! is, and a value of the wrong type fails the whole file.
//!
//! Deviations from upstream:
//! - The independent state's status reads by name, an unknown one as
//!   `Unknown`.
//! - Times are written in UTC; upstream writes the offset of the time it
//!   was given, often the local one.
//! - A record with a time Go can't write (a year before 0 or after 9999)
//!   is left out of the file; upstream's save fails.
//! - Reading, an `http_status` or `backoff_level` outside the port's range
//!   reads as zero.
//! - `open_ferry_quota_check` is open-ferry's own, written only while the
//!   cap is on; upstream's decoder ignores the unknown key. Reading, one
//!   that isn't an object with a positive `wait_seconds` and both times is
//!   ignored rather than failing the file.
//! - `open_ferry_recovery_refused_until` is open-ferry's own too, written only
//!   while a merge's refusal stands; upstream's decoder ignores the unknown
//!   key. Reading, a value that is neither `unknown` nor a time is ignored.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Map, Value};

use open_ferry_translate::go::{equal_fold, json_string};

use crate::auth::{AuthError, ModelState, QuotaState, Status, Timestamp, parse_go_rfc3339};
use crate::manager::credential::is_zero;

/// One saved cooldown: a credential's, or one of its models' (upstream's
/// `CooldownStateRecord`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) provider: String,
    pub(crate) auth_id: String,
    /// The credential's file, which names the record's `.cds` file; it
    /// isn't written (upstream's `json:"-"`).
    pub(crate) auth_file: String,
    /// The model, or empty for the credential's own cooldown.
    pub(crate) model: String,
    pub(crate) status: String,
    pub(crate) next_retry_after: Option<Timestamp>,
    pub(crate) reason: String,
    pub(crate) quota: QuotaState,
    pub(crate) last_error: Option<AuthError>,
    pub(crate) updated_at: Option<Timestamp>,
    /// A model's own cooldown beneath the credential-wide quota cooldown
    /// that its outer fields show, so that a restart doesn't lose which is
    /// which (`independent_state`).
    pub(crate) independent_state: Option<ModelState>,
    /// Whether the credential-wide quota cooldown wraps this model's state,
    /// for one with no cooldown of its own beneath it
    /// (`credential_quota_propagated`).
    pub(crate) credential_quota_propagated: bool,
    /// open-ferry's capped quota rest under this cooldown, if any
    /// (`open_ferry_quota_check`; see `quota_check`).
    pub(crate) quota_check: Option<QuotaCheckRecord>,
    /// Until when a merge of two models' cooldowns keeps a usage read from
    /// ending any of the credential's (`open_ferry_recovery_refused_until`;
    /// see `quota_recovery`). Only a credential's own record (no model)
    /// carries it, and a record that holds nothing else has no
    /// `next_retry_after`, so upstream's restore skips it whole.
    pub(crate) recovery_refused: Option<Refusal>,
}

/// How long a merge's refusal of usage recoveries lasts, as a record keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Until the time.
    Until(Timestamp),
    /// For a layer that blocked with no end of its own.
    Unknown,
}

/// A capped quota rest, as a record keeps it (open-ferry's own; see
/// `quota_check`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QuotaCheckRecord {
    /// The rest's length since the last quota answer.
    pub(crate) wait: Duration,
    /// When one call is next let through to check.
    pub(crate) check_at: Timestamp,
    /// The provider's reset.
    pub(crate) reset_at: Timestamp,
}

/// `time`, or `None` for Go's zero time.
pub(crate) fn nonzero(time: Option<Timestamp>) -> Option<Timestamp> {
    if is_zero(time) { None } else { time }
}

/// Whether Go can write every time in `record` (years 0 to 9999).
pub(crate) fn writable(record: &Record) -> bool {
    let own = record.independent_state.as_ref();
    [
        record.next_retry_after,
        record.updated_at,
        record.quota.next_recover_at,
        own.and_then(|state| state.next_retry_after),
        own.and_then(|state| state.updated_at),
        own.and_then(|state| state.quota.next_recover_at),
        record.quota_check.map(|check| check.check_at),
        record.quota_check.map(|check| check.reset_at),
        // The refusal is not checked: it never costs a record its place
        // (an unrepresentable bound is written as `"unknown"`).
    ]
    .into_iter()
    .flatten()
    .all(representable)
}

/// Whether Go can write `time` (years 0 to 9999, in UTC).
fn representable(time: Timestamp) -> bool {
    (0..=9999).contains(&chrono::Datelike::year(&time))
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// A JSON value to write indented.
enum Node {
    /// Already-encoded JSON.
    Raw(String),
    Object(Vec<(&'static str, Node)>),
    Array(Vec<Node>),
}

fn string(s: &str) -> Node {
    Node::Raw(json_string(s))
}

fn time(time: Option<Timestamp>) -> Node {
    Node::Raw(format!("\"{}\"", go_time(time)))
}

/// A time as Go's `MarshalJSON` writes a UTC time: RFC 3339 with up to
/// nine digits of fraction, trailing zeros dropped. `None` is Go's zero
/// time.
fn go_time(time: Option<Timestamp>) -> String {
    let Some(time) = time else {
        return "0001-01-01T00:00:00Z".to_owned();
    };
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

fn record_node(record: &Record) -> Node {
    let mut fields = Vec::new();
    if !record.provider.is_empty() {
        fields.push(("provider", string(&record.provider)));
    }
    fields.push(("auth_id", string(&record.auth_id)));
    if !record.model.is_empty() {
        fields.push(("model", string(&record.model)));
    }
    if !record.status.is_empty() {
        fields.push(("status", string(&record.status)));
    }
    fields.push(("next_retry_after", time(record.next_retry_after)));
    if !record.reason.is_empty() {
        fields.push(("reason", string(&record.reason)));
    }
    fields.push(("quota", quota_node(&record.quota)));
    if let Some(err) = &record.last_error {
        fields.push(("last_error", error_node(err)));
    }
    fields.push(("updated_at", time(record.updated_at)));
    if let Some(state) = &record.independent_state {
        fields.push(("independent_state", model_state_node(state)));
    }
    if record.credential_quota_propagated {
        fields.push(("credential_quota_propagated", Node::Raw("true".to_owned())));
    }
    if let Some(check) = &record.quota_check {
        fields.push(("open_ferry_quota_check", quota_check_node(check)));
    }
    if let Some(refusal) = record.recovery_refused {
        let node = match refusal {
            Refusal::Until(time) if representable(time) => time_node(time),
            Refusal::Until(_) | Refusal::Unknown => string(UNKNOWN),
        };
        fields.push(("open_ferry_recovery_refused_until", node));
    }
    Node::Object(fields)
}

/// What `open_ferry_recovery_refused_until` holds when the refusal has no end.
const UNKNOWN: &str = "unknown";

fn time_node(at: Timestamp) -> Node {
    time(Some(at))
}

/// A model's own cooldown (upstream's `ModelState` as `json.Marshal` writes
/// it). Like a record's quota, its quota keeps no snapshot.
fn model_state_node(state: &ModelState) -> Node {
    let mut fields = vec![("status", string(state.status.as_str()))];
    if !state.status_message.is_empty() {
        fields.push(("status_message", string(&state.status_message)));
    }
    fields.push(("unavailable", Node::Raw(state.unavailable.to_string())));
    fields.push(("next_retry_after", time(state.next_retry_after)));
    if let Some(err) = &state.last_error {
        fields.push(("last_error", error_node(err)));
    }
    fields.push(("quota", quota_node(&state.quota)));
    fields.push(("updated_at", time(state.updated_at)));
    Node::Object(fields)
}

/// open-ferry's capped quota rest (see `quota_check`).
fn quota_check_node(check: &QuotaCheckRecord) -> Node {
    let wait = check.wait.as_secs().max(1);
    Node::Object(vec![
        ("wait_seconds", Node::Raw(wait.to_string())),
        ("check_at", time(Some(check.check_at))),
        ("reset_at", time(Some(check.reset_at))),
    ])
}

fn quota_node(quota: &QuotaState) -> Node {
    let mut fields = vec![("exceeded", Node::Raw(quota.exceeded.to_string()))];
    if !quota.reason.is_empty() {
        fields.push(("reason", string(&quota.reason)));
    }
    fields.push(("next_recover_at", time(quota.next_recover_at)));
    if quota.backoff_level != 0 {
        fields.push(("backoff_level", Node::Raw(quota.backoff_level.to_string())));
    }
    // A record keeps no quota snapshot (upstream's `cooldownFieldsOf`), so
    // `observed_at` is the zero time, which `omitempty` keeps as a struct,
    // and there are no `signals`.
    fields.push(("observed_at", time(None)));
    Node::Object(fields)
}

fn error_node(err: &AuthError) -> Node {
    let mut fields = Vec::new();
    if !err.code.is_empty() {
        fields.push(("code", string(&err.code)));
    }
    fields.push(("message", string(&err.message)));
    fields.push(("retryable", Node::Raw(err.retryable.to_string())));
    if err.http_status != 0 {
        fields.push(("http_status", Node::Raw(err.http_status.to_string())));
    }
    Node::Object(fields)
}

fn write_node(out: &mut String, node: &Node, depth: usize) {
    match node {
        Node::Raw(raw) => out.push_str(raw),
        Node::Object(fields) if fields.is_empty() => out.push_str("{}"),
        Node::Array(items) if items.is_empty() => out.push_str("[]"),
        Node::Object(fields) => {
            out.push('{');
            for (i, (key, value)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                out.push_str(&json_string(key));
                out.push_str(": ");
                write_node(out, value, depth + 1);
            }
            newline(out, depth);
            out.push('}');
        }
        Node::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                write_node(out, item, depth + 1);
            }
            newline(out, depth);
            out.push(']');
        }
    }
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// The file for one credential's `records`, written at `now`, ending in a
/// newline (upstream's `cooldownStateFile` through `MarshalIndent`). The
/// envelope's credential and provider are the first record's.
pub(crate) fn encode_file(records: &[Record], now: Timestamp) -> String {
    let mut fields = vec![("version", Node::Raw("1".to_owned()))];
    if let Some(first) = records.first() {
        if !first.auth_id.is_empty() {
            fields.push(("auth_id", string(&first.auth_id)));
        }
        if !first.provider.is_empty() {
            fields.push(("provider", string(&first.provider)));
        }
    }
    fields.push(("updated_at", time(Some(now))));
    let records = if records.is_empty() {
        Node::Raw("null".to_owned())
    } else {
        Node::Array(records.iter().map(record_node).collect())
    };
    fields.push(("records", records));
    let mut out = String::new();
    write_node(&mut out, &Node::Object(fields), 0);
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The records in a file's JSON (upstream's `json.Unmarshal` into
/// `cooldownStateFile`). The error says what was wrong.
pub(crate) fn decode_file(data: &[u8]) -> Result<Vec<Record>, String> {
    let value: Value = serde_json::from_slice(data).map_err(|err| err.to_string())?;
    let mut records = Vec::new();
    match value {
        Value::Null => {}
        Value::Object(map) => {
            const FIELDS: &[&str] = &["version", "auth_id", "provider", "updated_at", "records"];
            for (key, value) in &map {
                match field(key, FIELDS) {
                    Some("version") => {
                        int(value, "version")?;
                    }
                    Some("auth_id" | "provider") => {
                        text(value, key)?;
                    }
                    Some("updated_at") => {
                        timestamp(value, key)?;
                    }
                    Some("records") => records = decode_records(value)?,
                    _ => {}
                }
            }
        }
        other => return Err(mismatch(&other, "cooldown state file")),
    }
    Ok(records)
}

fn decode_records(value: &Value) -> Result<Vec<Record>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items.iter().map(decode_record).collect(),
        other => Err(mismatch(other, "records")),
    }
}

fn decode_record(value: &Value) -> Result<Record, String> {
    const FIELDS: &[&str] = &[
        "provider",
        "auth_id",
        "model",
        "status",
        "next_retry_after",
        "reason",
        "quota",
        "last_error",
        "updated_at",
        "independent_state",
        "credential_quota_propagated",
        "open_ferry_quota_check",
        "open_ferry_recovery_refused_until",
    ];
    let mut record = Record::default();
    let map = match value {
        Value::Null => return Ok(record),
        Value::Object(map) => map,
        other => return Err(mismatch(other, "records")),
    };
    for (key, value) in map {
        match field(key, FIELDS) {
            Some("provider") => set_text(&mut record.provider, value, key)?,
            Some("auth_id") => set_text(&mut record.auth_id, value, key)?,
            Some("model") => set_text(&mut record.model, value, key)?,
            Some("status") => set_text(&mut record.status, value, key)?,
            Some("reason") => set_text(&mut record.reason, value, key)?,
            Some("next_retry_after") => set_time(&mut record.next_retry_after, value, key)?,
            Some("updated_at") => set_time(&mut record.updated_at, value, key)?,
            Some("quota") => match value {
                Value::Null => {}
                Value::Object(map) => decode_quota(&mut record.quota, map)?,
                other => return Err(mismatch(other, key)),
            },
            Some("last_error") => match value {
                Value::Null => record.last_error = None,
                Value::Object(map) => {
                    let err = record.last_error.get_or_insert_with(AuthError::default);
                    decode_error(err, map)?;
                }
                other => return Err(mismatch(other, key)),
            },
            Some("independent_state") => match value {
                Value::Null => record.independent_state = None,
                Value::Object(map) => {
                    let state = record
                        .independent_state
                        .get_or_insert_with(ModelState::default);
                    decode_model_state(state, map)?;
                }
                other => return Err(mismatch(other, key)),
            },
            Some("credential_quota_propagated") => {
                set_bool(&mut record.credential_quota_propagated, value, key)?;
            }
            Some("open_ferry_quota_check") => record.quota_check = decode_quota_check(value),
            Some("open_ferry_recovery_refused_until") => {
                record.recovery_refused = decode_refusal(value);
            }
            _ => {}
        }
    }
    Ok(record)
}

/// A model's own cooldown (upstream's `ModelState`). A status it has no
/// name for reads as unknown.
fn decode_model_state(state: &mut ModelState, map: &Map<String, Value>) -> Result<(), String> {
    const FIELDS: &[&str] = &[
        "status",
        "status_message",
        "unavailable",
        "next_retry_after",
        "last_error",
        "quota",
        "updated_at",
    ];
    for (key, value) in map {
        match field(key, FIELDS) {
            Some("status") => {
                if let Some(name) = text(value, key)? {
                    state.status = match name.as_str() {
                        "active" => Status::Active,
                        "pending" => Status::Pending,
                        "refreshing" => Status::Refreshing,
                        "error" => Status::Error,
                        "disabled" => Status::Disabled,
                        _ => Status::Unknown,
                    };
                }
            }
            Some("status_message") => set_text(&mut state.status_message, value, key)?,
            Some("unavailable") => set_bool(&mut state.unavailable, value, key)?,
            Some("next_retry_after") => set_time(&mut state.next_retry_after, value, key)?,
            Some("updated_at") => set_time(&mut state.updated_at, value, key)?,
            Some("quota") => match value {
                Value::Null => {}
                Value::Object(map) => decode_quota(&mut state.quota, map)?,
                other => return Err(mismatch(other, key)),
            },
            Some("last_error") => match value {
                Value::Null => state.last_error = None,
                Value::Object(map) => {
                    let err = state.last_error.get_or_insert_with(AuthError::default);
                    decode_error(err, map)?;
                }
                other => return Err(mismatch(other, key)),
            },
            _ => {}
        }
    }
    Ok(())
}

/// open-ferry's capped quota rest, or `None` for anything but an object
/// with a positive `wait_seconds` and both times (see `quota_check`).
fn decode_quota_check(value: &Value) -> Option<QuotaCheckRecord> {
    const FIELDS: &[&str] = &["wait_seconds", "check_at", "reset_at"];
    let Value::Object(map) = value else {
        return None;
    };
    let (mut wait, mut check_at, mut reset_at) = (None, None, None);
    for (key, value) in map {
        // A time Go could not write back reads as absent, so a rest never
        // costs its record its place.
        let time = || {
            timestamp(value, key)
                .ok()
                .flatten()
                .flatten()
                .filter(|time| representable(*time))
        };
        match field(key, FIELDS) {
            Some("wait_seconds") => {
                wait = value
                    .as_u64()
                    .filter(|seconds| *seconds > 0)
                    .map(Duration::from_secs);
            }
            Some("check_at") => check_at = time(),
            Some("reset_at") => reset_at = time(),
            _ => {}
        }
    }
    Some(QuotaCheckRecord {
        wait: wait?,
        check_at: check_at?,
        reset_at: reset_at?,
    })
}

/// open-ferry's refusal of usage recoveries: the string `unknown`, or an RFC
/// 3339 time. Anything else is ignored, not an error (see `quota_recovery`).
fn decode_refusal(value: &Value) -> Option<Refusal> {
    let Value::String(text) = value else {
        return None;
    };
    if text.trim().eq_ignore_ascii_case(UNKNOWN) {
        return Some(Refusal::Unknown);
    }
    // A bound Go could not write back (after normalising to UTC) reads as an
    // unknown one: that only refuses more, and the record keeps its place.
    parse_go_rfc3339(text.trim()).map(|time| {
        if representable(time) {
            Refusal::Until(time)
        } else {
            Refusal::Unknown
        }
    })
}

fn decode_quota(quota: &mut QuotaState, map: &Map<String, Value>) -> Result<(), String> {
    const FIELDS: &[&str] = &[
        "exceeded",
        "reason",
        "next_recover_at",
        "backoff_level",
        "observed_at",
        "signals",
    ];
    for (key, value) in map {
        match field(key, FIELDS) {
            Some("exceeded") => set_bool(&mut quota.exceeded, value, key)?,
            Some("reason") => set_text(&mut quota.reason, value, key)?,
            Some("next_recover_at") => set_time(&mut quota.next_recover_at, value, key)?,
            Some("backoff_level") => {
                if let Some(level) = int(value, key)? {
                    quota.backoff_level = u32::try_from(level).unwrap_or(0);
                }
            }
            Some("observed_at") => set_time(&mut quota.observed_at, value, key)?,
            Some("signals") => decode_signals(&mut quota.signals, value, key)?,
            _ => {}
        }
    }
    Ok(())
}

/// A quota snapshot's headers, as Go decodes a `map[string]string`: `null`
/// drops them, an object adds its entries to those a key in another case
/// gave (a `null` value as an empty one), and anything else fails.
fn decode_signals(
    signals: &mut BTreeMap<String, String>,
    value: &Value,
    field: &str,
) -> Result<(), String> {
    match value {
        Value::Null => {
            signals.clear();
            Ok(())
        }
        Value::Object(entries) => {
            for (name, value) in entries {
                let value = text(value, field)?.unwrap_or_default();
                signals.insert(name.clone(), value);
            }
            Ok(())
        }
        other => Err(mismatch(other, field)),
    }
}

fn decode_error(err: &mut AuthError, map: &Map<String, Value>) -> Result<(), String> {
    const FIELDS: &[&str] = &["code", "message", "retryable", "http_status"];
    for (key, value) in map {
        match field(key, FIELDS) {
            Some("code") => set_text(&mut err.code, value, key)?,
            Some("message") => set_text(&mut err.message, value, key)?,
            Some("retryable") => set_bool(&mut err.retryable, value, key)?,
            Some("http_status") => {
                if let Some(status) = int(value, key)? {
                    err.http_status = u16::try_from(status).unwrap_or(0);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The field `key` names, as Go's decoder finds it: the exact name, or else
/// one equal under case folding.
fn field(key: &str, fields: &[&'static str]) -> Option<&'static str> {
    fields
        .iter()
        .find(|name| **name == key)
        .or_else(|| fields.iter().find(|name| equal_fold(name, key)))
        .copied()
}

fn mismatch(value: &Value, field: &str) -> String {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    format!("cannot unmarshal {kind} into {field}")
}

fn text(value: &Value, field: &str) -> Result<Option<String>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        other => Err(mismatch(other, field)),
    }
}

fn set_text(slot: &mut String, value: &Value, field: &str) -> Result<(), String> {
    if let Some(s) = text(value, field)? {
        *slot = s;
    }
    Ok(())
}

fn set_bool(slot: &mut bool, value: &Value, field: &str) -> Result<(), String> {
    match value {
        Value::Null => Ok(()),
        Value::Bool(b) => {
            *slot = *b;
            Ok(())
        }
        other => Err(mismatch(other, field)),
    }
}

/// An integer, as Go reads one into an `int`: a number written without a
/// fraction or exponent.
fn int(value: &Value, field: &str) -> Result<Option<i64>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("cannot unmarshal number {n} into {field}")),
        other => Err(mismatch(other, field)),
    }
}

fn timestamp(value: &Value, field: &str) -> Result<Option<Option<Timestamp>>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => parse_go_rfc3339(s)
            .map(|time| Some(nonzero(Some(time))))
            .ok_or_else(|| format!("parsing time {} as RFC 3339", json_string(s))),
        other => Err(mismatch(other, field)),
    }
}

fn set_time(slot: &mut Option<Timestamp>, value: &Value, field: &str) -> Result<(), String> {
    if let Some(time) = timestamp(value, field)? {
        *slot = time;
    }
    Ok(())
}
