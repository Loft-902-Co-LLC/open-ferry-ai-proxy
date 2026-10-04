// Ported from CLIProxyAPI sdk/cliproxy/auth/cooldown_state.go
// (CooldownStateRecord, cooldownStateFile, readCooldownStateFile and the
// marshaling in writeCooldownStateGroup) (v8.0.10, MIT).
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
//! `updated_at`, with upstream's field names, order and `omitempty`s.
//! Reading takes the fields as Go's decoder does: a key matches its field
//! whatever its case, unknown keys are ignored, `null` leaves a field as it
//! is, and a value of the wrong type fails the whole file.
//!
//! Deviations from upstream:
//! - Times are written in UTC; upstream writes the offset of the time it
//!   was given, often the local one.
//! - A record with a time Go can't write (a year before 0 or after 9999)
//!   is left out of the file; upstream's save fails.
//! - A quota's `observed_at` is always the zero time and it has no
//!   `signals`: the port doesn't keep them. Reading, `signals` is ignored,
//!   and an `http_status` or `backoff_level` outside the port's range reads
//!   as zero.

use serde_json::{Map, Value};

use open_ferry_translate::go::{equal_fold, json_string};

use crate::auth::{AuthError, QuotaState, Timestamp, parse_go_rfc3339};
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
}

/// `time`, or `None` for Go's zero time.
pub(crate) fn nonzero(time: Option<Timestamp>) -> Option<Timestamp> {
    if is_zero(time) { None } else { time }
}

/// Whether Go can write every time in `record` (years 0 to 9999).
pub(crate) fn writable(record: &Record) -> bool {
    [
        record.next_retry_after,
        record.updated_at,
        record.quota.next_recover_at,
    ]
    .into_iter()
    .flatten()
    .all(|time| (0..=9999).contains(&chrono::Datelike::year(&time)))
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
    Node::Object(fields)
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
    // A struct, so `omitempty` keeps it.
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
            _ => {}
        }
    }
    Ok(record)
}

fn decode_quota(quota: &mut QuotaState, map: &Map<String, Value>) -> Result<(), String> {
    const FIELDS: &[&str] = &[
        "exceeded",
        "reason",
        "next_recover_at",
        "backoff_level",
        "observed_at",
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
            Some("observed_at") => {
                timestamp(value, key)?;
            }
            _ => {}
        }
    }
    Ok(())
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
