// Ported from CLIProxyAPI sdk/pluginapi/types.go (QuotaFetchResponse,
// QuotaSubscription, QuotaMetric, QuotaGroup, QuotaBucket and their
// UnmarshalJSON) and internal/api/handlers/management/plugin_quota.go
// (the normalized decode in executeQuotaProbe) (v8.0.15, MIT), with Go
// encoding/json decode.go (array, object, literalStore, indirect,
// unquoteBytes, getu4) and fold.go (foldName) (go1.26, BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! The quota answer's types, written as upstream's Go tags write them, and
//! a probe response read into them as upstream's Go decoder reads it.
//!
//! A probe response in the normalized shape is decoded as upstream decodes
//! it: its top-level members, but for any named `summary` in any case,
//! taken in key order (the last of equal keys winning) and decoded into
//! the answer. A member matches a field by its exact name, else by its name
//! in any case; `tier_name`, `tier_id`, `display_name`,
//! `remaining_fraction`, `reset_time` and `server_time_offset_ms` are read
//! too, where the camel-case field is empty. A key met twice decodes into
//! the same field twice, and a list decoded into an earlier, longer one
//! takes up the elements it left off, as Go reuses a slice's spare
//! capacity. Unknown members are skipped; a member of the wrong type fails
//! the decode.
//!
//! Deviations from upstream:
//! - None.

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Number, Value};

use crate::go::{equal_fold, lossy};
use crate::json::Json;

/// A credential's quota, as the management API answers it
/// (`QuotaFetchResponse`).
#[derive(Clone, Default)]
pub(crate) struct QuotaFetchResponse {
    pub(crate) subscription: Option<QuotaSubscription>,
    pub(crate) summary: Vec<QuotaMetric>,
    pub(crate) server_time_offset_ms: i64,
    pub(crate) groups: Vec<QuotaGroup>,
}

/// The account's plan (`QuotaSubscription`). Text read with gjson may hold
/// bytes that aren't UTF-8, as a Go string may.
#[derive(Clone, Default)]
pub(crate) struct QuotaSubscription {
    pub(crate) plan: Vec<u8>,
    pub(crate) tier_name: Vec<u8>,
    pub(crate) tier_id: Vec<u8>,
}

/// A figure for the account, such as a balance (`QuotaMetric`).
#[derive(Clone, Default)]
pub(crate) struct QuotaMetric {
    pub(crate) key: Vec<u8>,
    pub(crate) label: Vec<u8>,
    pub(crate) value: f64,
    pub(crate) unit: Vec<u8>,
    /// `number` or `currency`.
    pub(crate) format: Vec<u8>,
    /// An ISO 4217 code, when the format is `currency`.
    pub(crate) currency: Vec<u8>,
}

/// A group of quota windows (`QuotaGroup`).
#[derive(Clone, Default)]
pub(crate) struct QuotaGroup {
    pub(crate) display_name: Vec<u8>,
    pub(crate) buckets: Vec<QuotaBucket>,
}

/// One quota window or limit (`QuotaBucket`).
#[derive(Clone, Default)]
pub(crate) struct QuotaBucket {
    pub(crate) window: Vec<u8>,
    pub(crate) remaining_fraction: f64,
    pub(crate) reset_time: Vec<u8>,
    pub(crate) description: Vec<u8>,
}

impl QuotaFetchResponse {
    /// The answer as Go writes it, empty fields left out as its
    /// `omitempty` tags leave them out; `None` if it holds a number JSON
    /// can't write (an infinity), where Go's encoder fails.
    pub(crate) fn to_json(&self) -> Option<Json> {
        let mut fields = Vec::new();
        if let Some(subscription) = &self.subscription {
            let mut sub = Vec::new();
            text_field(&mut sub, "plan", &subscription.plan);
            text_field(&mut sub, "tierName", &subscription.tier_name);
            text_field(&mut sub, "tierId", &subscription.tier_id);
            fields.push(("subscription", Json::Struct(sub)));
        }
        if !self.summary.is_empty() {
            let metrics = self
                .summary
                .iter()
                .map(QuotaMetric::to_json)
                .collect::<Option<Vec<_>>>()?;
            fields.push(("summary", Json::Array(metrics)));
        }
        if self.server_time_offset_ms != 0 {
            fields.push(("serverTimeOffsetMs", Json::Int(self.server_time_offset_ms)));
        }
        if !self.groups.is_empty() {
            let groups = self
                .groups
                .iter()
                .map(QuotaGroup::to_json)
                .collect::<Option<Vec<_>>>()?;
            fields.push(("groups", Json::Array(groups)));
        }
        Some(Json::Struct(fields))
    }
}

impl QuotaMetric {
    fn to_json(&self) -> Option<Json> {
        let mut fields = vec![
            ("key", Json::Bytes(self.key.clone())),
            ("label", Json::Bytes(self.label.clone())),
            ("value", float(self.value)?),
        ];
        text_field(&mut fields, "unit", &self.unit);
        text_field(&mut fields, "format", &self.format);
        text_field(&mut fields, "currency", &self.currency);
        Some(Json::Struct(fields))
    }
}

impl QuotaGroup {
    fn to_json(&self) -> Option<Json> {
        let mut fields = Vec::new();
        text_field(&mut fields, "displayName", &self.display_name);
        if !self.buckets.is_empty() {
            let buckets = self
                .buckets
                .iter()
                .map(QuotaBucket::to_json)
                .collect::<Option<Vec<_>>>()?;
            fields.push(("buckets", Json::Array(buckets)));
        }
        Some(Json::Struct(fields))
    }
}

impl QuotaBucket {
    fn to_json(&self) -> Option<Json> {
        let mut fields = Vec::new();
        text_field(&mut fields, "window", &self.window);
        fields.push(("remainingFraction", float(self.remaining_fraction)?));
        text_field(&mut fields, "resetTime", &self.reset_time);
        text_field(&mut fields, "description", &self.description);
        Some(Json::Struct(fields))
    }
}

/// Adds a text field unless it is empty (`omitempty`).
fn text_field(fields: &mut Vec<(&'static str, Json)>, name: &'static str, text: &[u8]) {
    if !text.is_empty() {
        fields.push((name, Json::Bytes(text.to_vec())));
    }
}

/// A `float64` as Go writes it; `None` for an infinity or NaN.
fn float(f: f64) -> Option<Json> {
    Number::from_f64(f).map(|number| Json::Any(Value::Number(number)))
}

/// The members of the top-level object of `body`, as Go decodes it into a
/// `map[string]json.RawMessage`: the last of equal keys winning, in key
/// order. Empty for `null`; `None` for anything but an object. `body` must
/// be valid JSON.
pub(crate) fn top_level(body: &[u8]) -> Option<BTreeMap<String, &[u8]>> {
    let start = skip_ws(body, 0);
    match body.get(start) {
        Some(b'n') => Some(BTreeMap::new()),
        Some(b'{') => {
            let object = span(body, start, value_end(body, start));
            Some(
                members(object)
                    .into_iter()
                    .map(|(key, value)| (unquote(key), value))
                    .collect(),
            )
        }
        _ => None,
    }
}

/// A probe response in the normalized shape, decoded as upstream decodes
/// it, its `summary` aside; `None` where Go's decoder fails. `body` must be
/// valid JSON.
pub(crate) fn decode_normalized(body: &[u8]) -> Option<QuotaFetchResponse> {
    let mut answer = Answer::default();
    let mut alt_offset = 0;
    for (key, raw) in top_level(body)? {
        if equal_fold(&key, "summary") {
            continue;
        }
        match field(
            &[
                "subscription",
                "serverTimeOffsetMs",
                "groups",
                "server_time_offset_ms",
            ],
            &key,
        ) {
            Some(0) => decode_subscription(raw, &mut answer.subscription)?,
            Some(1) => decode_int(raw, &mut answer.offset)?,
            Some(2) => answer.groups.decode(raw, decode_group)?,
            Some(3) => decode_int(raw, &mut alt_offset)?,
            _ => {}
        }
    }
    if answer.offset == 0 && alt_offset != 0 {
        answer.offset = alt_offset;
    }
    Some(QuotaFetchResponse {
        subscription: answer.subscription.map(|sub| QuotaSubscription {
            plan: sub.plan.into_bytes(),
            tier_name: sub.tier_name.into_bytes(),
            tier_id: sub.tier_id.into_bytes(),
        }),
        summary: Vec::new(),
        server_time_offset_ms: answer.offset,
        groups: answer
            .groups
            .items
            .into_iter()
            .map(|group| QuotaGroup {
                display_name: group.display_name.into_bytes(),
                buckets: group
                    .buckets
                    .items
                    .into_iter()
                    .map(|bucket| QuotaBucket {
                        window: bucket.window.into_bytes(),
                        remaining_fraction: bucket.remaining_fraction,
                        reset_time: bucket.reset_time.into_bytes(),
                        description: bucket.description.into_bytes(),
                    })
                    .collect(),
            })
            .collect(),
    })
}

/// The answer while it is decoded.
#[derive(Default)]
struct Answer {
    subscription: Option<Subscription>,
    offset: i64,
    groups: GoSlice<Group>,
}

#[derive(Default)]
struct Subscription {
    plan: String,
    tier_name: String,
    tier_id: String,
}

#[derive(Default)]
struct Group {
    display_name: String,
    buckets: GoSlice<Bucket>,
}

#[derive(Default)]
struct Bucket {
    window: String,
    remaining_fraction: f64,
    reset_time: String,
    description: String,
}

/// A Go slice: its elements, and those past its length that its capacity
/// still holds, which decoding a longer list takes up again.
struct GoSlice<T> {
    items: Vec<T>,
    spare: VecDeque<T>,
}

impl<T> Default for GoSlice<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            spare: VecDeque::new(),
        }
    }
}

impl<T: Default> GoSlice<T> {
    /// Decodes a list into the slice as Go's decoder does: each element
    /// into the one already there, an empty list or `null` leaving none.
    fn decode(&mut self, raw: &[u8], element: fn(&[u8], &mut T) -> Option<()>) -> Option<()> {
        match raw.first() {
            Some(b'n') => {
                self.items.clear();
                self.spare.clear();
            }
            Some(b'[') => {
                let mut count = 0;
                for (i, value) in elements(raw).into_iter().enumerate() {
                    if i >= self.items.len() {
                        let reused = self.spare.pop_front().unwrap_or_default();
                        self.items.push(reused);
                    }
                    element(value, self.items.get_mut(i)?)?;
                    count = i + 1;
                }
                if count == 0 {
                    self.items.clear();
                    self.spare.clear();
                } else if count < self.items.len() {
                    for item in self.items.split_off(count).into_iter().rev() {
                        self.spare.push_front(item);
                    }
                }
            }
            _ => return None,
        }
        Some(())
    }
}

/// Decodes into a `*QuotaSubscription`.
fn decode_subscription(raw: &[u8], out: &mut Option<Subscription>) -> Option<()> {
    match raw.first() {
        Some(b'n') => *out = None,
        Some(b'{') => {
            let sub = out.get_or_insert_default();
            let (mut alt_name, mut alt_id) = (String::new(), String::new());
            for (key, value) in members(raw) {
                let fields = ["plan", "tierName", "tierId", "tier_name", "tier_id"];
                match field(&fields, &unquote(key)) {
                    Some(0) => decode_string(value, &mut sub.plan)?,
                    Some(1) => decode_string(value, &mut sub.tier_name)?,
                    Some(2) => decode_string(value, &mut sub.tier_id)?,
                    Some(3) => decode_string(value, &mut alt_name)?,
                    Some(4) => decode_string(value, &mut alt_id)?,
                    _ => {}
                }
            }
            if sub.tier_name.is_empty() && !alt_name.is_empty() {
                sub.tier_name = alt_name;
            }
            if sub.tier_id.is_empty() && !alt_id.is_empty() {
                sub.tier_id = alt_id;
            }
        }
        _ => return None,
    }
    Some(())
}

/// Decodes into a `QuotaGroup`; `null` leaves it as it is.
fn decode_group(raw: &[u8], group: &mut Group) -> Option<()> {
    match raw.first() {
        Some(b'n') => return Some(()),
        Some(b'{') => {}
        _ => return None,
    }
    let mut alt_name = String::new();
    for (key, value) in members(raw) {
        match field(&["displayName", "buckets", "display_name"], &unquote(key)) {
            Some(0) => decode_string(value, &mut group.display_name)?,
            Some(1) => group.buckets.decode(value, decode_bucket)?,
            Some(2) => decode_string(value, &mut alt_name)?,
            _ => {}
        }
    }
    if group.display_name.is_empty() && !alt_name.is_empty() {
        group.display_name = alt_name;
    }
    Some(())
}

/// Decodes into a `QuotaBucket`; `null` leaves it as it is.
fn decode_bucket(raw: &[u8], bucket: &mut Bucket) -> Option<()> {
    match raw.first() {
        Some(b'n') => return Some(()),
        Some(b'{') => {}
        _ => return None,
    }
    let (mut fraction, mut alt_fraction, mut alt_reset) = (None, None, String::new());
    for (key, value) in members(raw) {
        let fields = [
            "window",
            "resetTime",
            "description",
            "remainingFraction",
            "remaining_fraction",
            "reset_time",
        ];
        match field(&fields, &unquote(key)) {
            Some(0) => decode_string(value, &mut bucket.window)?,
            Some(1) => decode_string(value, &mut bucket.reset_time)?,
            Some(2) => decode_string(value, &mut bucket.description)?,
            Some(3) => decode_float(value, &mut fraction)?,
            Some(4) => decode_float(value, &mut alt_fraction)?,
            Some(5) => decode_string(value, &mut alt_reset)?,
            _ => {}
        }
    }
    if let Some(fraction) = fraction.or(alt_fraction) {
        bucket.remaining_fraction = fraction;
    }
    if bucket.reset_time.is_empty() && !alt_reset.is_empty() {
        bucket.reset_time = alt_reset;
    }
    Some(())
}

/// Decodes into a `string`; `null` leaves it as it is.
fn decode_string(raw: &[u8], out: &mut String) -> Option<()> {
    match raw.first() {
        Some(b'"') => *out = unquote(raw),
        Some(b'n') => {}
        _ => return None,
    }
    Some(())
}

/// Decodes into a `*float64`: `null` clears it; a number too large for an
/// `f64` fails.
fn decode_float(raw: &[u8], out: &mut Option<f64>) -> Option<()> {
    match raw.first() {
        Some(b'n') => *out = None,
        Some(b'-' | b'0'..=b'9') => {
            let text = std::str::from_utf8(raw).ok()?;
            *out = Some(open_ferry_translate::go::parse_float_checked(text)?);
        }
        _ => return None,
    }
    Some(())
}

/// Decodes into an `int64`: `null` leaves it as it is; a fraction, an
/// exponent or a number out of range fails.
fn decode_int(raw: &[u8], out: &mut i64) -> Option<()> {
    match raw.first() {
        Some(b'n') => {}
        Some(b'-' | b'0'..=b'9') => *out = std::str::from_utf8(raw).ok()?.parse().ok()?,
        _ => return None,
    }
    Some(())
}

/// Which of `fields` the key names, as Go's decoder picks: the field of
/// that exact name, else the first it matches in any case.
fn field(fields: &[&str], key: &str) -> Option<usize> {
    fields
        .iter()
        .position(|name| *name == key)
        .or_else(|| fields.iter().position(|name| fold_matches(name, key)))
}

/// Whether `key` matches the ASCII field name `field` as Go's decoder
/// folds case: ASCII letters in any case, `ſ` as `S` and the Kelvin sign
/// as `K`.
fn fold_matches(field: &str, key: &str) -> bool {
    let mut chars = key.chars();
    for expected in field.bytes() {
        let folded = match chars.next() {
            Some('\u{17f}') => b'S',
            Some('\u{212a}') => b'K',
            Some(c) if c.is_ascii() => (c as u8).to_ascii_uppercase(),
            _ => return false,
        };
        if folded != expected.to_ascii_uppercase() {
            return false;
        }
    }
    chars.next().is_none()
}

/// `s[start..end]`, or empty.
fn span(s: &[u8], start: usize, end: usize) -> &[u8] {
    s.get(start..end).unwrap_or_default()
}

/// The index of the first byte from `i` that isn't JSON whitespace.
fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while matches!(s.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// Where the string whose opening quote is at `i` ends.
fn string_end(s: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    loop {
        match s.get(j) {
            Some(b'\\') => j += 2,
            Some(b'"') => return j + 1,
            Some(_) => j += 1,
            None => return s.len(),
        }
    }
}

/// Where the value starting at `i` ends.
fn value_end(s: &[u8], i: usize) -> usize {
    match s.get(i) {
        Some(b'"') => string_end(s, i),
        Some(b'{' | b'[') => {
            let mut depth = 0usize;
            let mut j = i;
            while let Some(&b) = s.get(j) {
                match b {
                    b'"' => {
                        j = string_end(s, j);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return j + 1;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            s.len()
        }
        _ => s
            .iter()
            .skip(i)
            .position(|b| matches!(b, b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r'))
            .map_or(s.len(), |n| i + n),
    }
}

/// An object's members: each key, quoted, and its value.
fn members(object: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut out = Vec::new();
    let mut i = skip_ws(object, 1);
    while object.get(i) == Some(&b'"') {
        let key_end = string_end(object, i);
        let key = span(object, i, key_end);
        // Past the colon.
        let start = skip_ws(object, skip_ws(object, key_end) + 1);
        let end = value_end(object, start);
        out.push((key, span(object, start, end)));
        i = skip_ws(object, end);
        if object.get(i) == Some(&b',') {
            i = skip_ws(object, i + 1);
        }
    }
    out
}

/// An array's elements.
fn elements(array: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = skip_ws(array, 1);
    while i < array.len() && array.get(i) != Some(&b']') {
        let end = value_end(array, i);
        if end <= i {
            break;
        }
        out.push(span(array, i, end));
        i = skip_ws(array, end);
        if array.get(i) == Some(&b',') {
            i = skip_ws(array, i + 1);
        }
    }
    out
}

/// Four hex digits from `i`, as a code point.
fn hex4(s: &[u8], i: usize) -> Option<u32> {
    let digits = std::str::from_utf8(s.get(i..i + 4)?).ok()?;
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// A JSON string's text, as Go's decoder reads it (`unquoteBytes`): a byte
/// that isn't part of a valid character, and an escaped surrogate not in a
/// valid pair, read as U+FFFD. `raw` is the string with its quotes.
pub(crate) fn unquote(raw: &[u8]) -> String {
    let inner = span(raw, 1, raw.len().saturating_sub(1));
    let mut out = String::with_capacity(inner.len());
    let mut run = 0;
    let mut i = 0;
    while let Some(&b) = inner.get(i) {
        if b != b'\\' {
            i += 1;
            continue;
        }
        out.push_str(&lossy(span(inner, run, i)));
        let escape = inner.get(i + 1).copied().unwrap_or(0);
        i += 2;
        match escape {
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let code = hex4(inner, i).unwrap_or(0xfffd);
                i += 4;
                let c = if (0xd800..0xe000).contains(&code) {
                    let low = (inner.get(i) == Some(&b'\\') && inner.get(i + 1) == Some(&b'u'))
                        .then(|| hex4(inner, i + 2))
                        .flatten();
                    match low {
                        Some(low)
                            if (0xd800..0xdc00).contains(&code)
                                && (0xdc00..0xe000).contains(&low) =>
                        {
                            i += 6;
                            char::from_u32(0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00))
                        }
                        _ => None,
                    }
                } else {
                    char::from_u32(code)
                };
                out.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            other => out.push(char::from(other)),
        }
        run = i;
    }
    out.push_str(&lossy(span(inner, run, inner.len())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decoded answer as JSON, or `ERR` where Go's decoder fails.
    fn decoded(body: &str) -> String {
        decode_normalized(body.as_bytes())
            .and_then(|answer| answer.to_json())
            .map_or_else(|| "ERR".to_owned(), |json| json.encode())
    }

    // Not upstream's: the decode as Go 1.26.4's decoder gave it, with
    // upstream's types, for these bodies.
    #[test]
    fn decodes_as_go_does() {
        let cases = [
            (
                r#"{"subscription":{"plan":"P","tier_name":"TN","tier_id":"TI"},"server_time_offset_ms":5,"groups":[{"display_name":"G","buckets":[{"remaining_fraction":0.25,"reset_time":"rt"}]}]}"#,
                r#"{"subscription":{"plan":"P","tierName":"TN","tierId":"TI"},"serverTimeOffsetMs":5,"groups":[{"displayName":"G","buckets":[{"remainingFraction":0.25,"resetTime":"rt"}]}]}"#,
            ),
            // A list decoded into a longer one takes up what it left off.
            (
                r#"{"groups":[{"buckets":[{"remainingFraction":1,"window":"a","description":"d"},{"remainingFraction":2,"window":"b","description":"e"}],"buckets":[{"remainingFraction":3}],"buckets":[{"window":"c"},{"window":"f"}]}]}"#,
                r#"{"groups":[{"buckets":[{"window":"c","remainingFraction":3,"description":"d"},{"window":"f","remainingFraction":2,"description":"e"}]}]}"#,
            ),
            (
                r#"{"GROUPS":[{"displayName":"A"},{"displayName":"B"}],"Groups":[{"displayName":"C"}],"groups":[{"buckets":[{"remainingFraction":1}]},{"buckets":[{"remainingFraction":2}]}]}"#,
                r#"{"groups":[{"displayName":"C","buckets":[{"remainingFraction":1}]},{"displayName":"B","buckets":[{"remainingFraction":2}]}]}"#,
            ),
            // An empty list leaves nothing to take up.
            (
                r#"{"groups":[{"buckets":[{"remainingFraction":1,"window":"a"}]},{"buckets":[{"window":"b"}]}],"groups":[],"groups":[{"displayName":"x"}]}"#,
                r#"{"groups":[{"displayName":"x"}]}"#,
            ),
            (
                r#"{"groups":[{"buckets":[{"remainingFraction":1,"window":"a"},{"window":"b"}]}],"groups":[{"buckets":[{"window":"c"}]}],"groups":[{"buckets":null}]}"#,
                r#"{"groups":[{}]}"#,
            ),
            (
                r#"{"groups":[{"buckets":[{"remainingFraction":null,"remaining_fraction":0.3}]}]}"#,
                r#"{"groups":[{"buckets":[{"remainingFraction":0.3}]}]}"#,
            ),
            (
                r#"{"groups":[{"buckets":[{"remainingFraction":0.4},null]},null]}"#,
                r#"{"groups":[{"buckets":[{"remainingFraction":0.4},{"remainingFraction":0}]},{}]}"#,
            ),
            (
                r#"{"groups":[{"buckets":[{"RESETTIME":"x","reset_time":"y","ReMaInInGfRaCtIoN":0.5}]}]}"#,
                r#"{"groups":[{"buckets":[{"remainingFraction":0.5,"resetTime":"x"}]}]}"#,
            ),
            (
                r#"{"subscription":{"plan":"P"},"Subscription":{"tierId":"T"},"subscription":{"tier_name":"N"}}"#,
                r#"{"subscription":{"tierName":"N","tierId":"T"}}"#,
            ),
            (
                r#"{"subscription":{"plan":"P"},"Subscription":null,"subscription":{"tierId":"T"}}"#,
                r#"{"subscription":{"tierId":"T"}}"#,
            ),
            (
                r#"{"serverTimeOffsetMs":-0,"server_time_offset_ms":-9}"#,
                r#"{"serverTimeOffsetMs":-9}"#,
            ),
            (
                r#"{"serverTimeOffsetMs":7,"server_time_offset_ms":9}"#,
                r#"{"serverTimeOffsetMs":7}"#,
            ),
            (
                r#"{"groups":[{"buckets":[{"remainingFraction":1e-400}]}]}"#,
                r#"{"groups":[{"buckets":[{"remainingFraction":0}]}]}"#,
            ),
            (
                "{\"\u{17f}ubscription\":{\"PLAN\":\"p\",\"\u{212a}ey\":1}}",
                r#"{"subscription":{"plan":"p"}}"#,
            ),
            (
                r#"{"subscription":{"plan":"P"},"Summary":"x","summary":5}"#,
                r#"{"subscription":{"plan":"P"}}"#,
            ),
            (r#"{"subscription":{}}"#, r#"{"subscription":{}}"#),
            ("null", "{}"),
            (
                " {\"subscription\" : { \"plan\" : \"<&>\" } } ",
                r#"{"subscription":{"plan":"\u003c\u0026\u003e"}}"#,
            ),
            // What Go's decoder fails on.
            (r#"{"serverTimeOffsetMs":1.5}"#, "ERR"),
            (r#"{"serverTimeOffsetMs":1e3}"#, "ERR"),
            (r#"{"serverTimeOffsetMs":"5"}"#, "ERR"),
            (r#"{"serverTimeOffsetMs":9223372036854775808}"#, "ERR"),
            (
                r#"{"groups":[{"buckets":[{"remainingFraction":1e400}]}]}"#,
                "ERR",
            ),
            (
                r#"{"groups":[{"buckets":[{"remainingFraction":"0.5"}]}]}"#,
                "ERR",
            ),
            (r#"{"groups":[{"buckets":[{"window":5}]}]}"#, "ERR"),
            (r#"{"groups":[1]}"#, "ERR"),
            (r#"{"groups":{}}"#, "ERR"),
            (r#"{"subscription":"x"}"#, "ERR"),
            (r#"{"subscription":[]}"#, "ERR"),
            ("[1]", "ERR"),
            ("\"x\"", "ERR"),
        ];
        for (body, want) in cases {
            assert_eq!(decoded(body), want, "{body}");
        }
    }

    // Not upstream's: strings read as Go's decoder reads them.
    #[test]
    fn strings_unquote_as_go_does() {
        let bs = '\u{5c}';
        let raw = format!("\"x{bs}ud800{bs}u0041y{bs}udc00{bs}ud800z\"");
        assert_eq!(unquote(raw.as_bytes()), "x\u{fffd}Ay\u{fffd}\u{fffd}z");
        let raw =
            format!("\"{bs}ud83d{bs}ude00 {bs}\"{bs}{bs}{bs}/{bs}b{bs}f{bs}n{bs}r{bs}t{bs}u00e9\"");
        assert_eq!(
            unquote(raw.as_bytes()),
            "\u{1f600} \"\\/\u{8}\u{c}\n\r\t\u{e9}"
        );
        assert_eq!(unquote(b"\"a\xffb\xe2\x82\""), "a\u{fffd}b\u{fffd}\u{fffd}");
        assert_eq!(unquote(b"\"\""), "");
    }

    // Not upstream's: the answer is written as Go writes it.
    #[test]
    fn writes_as_go_does() {
        let answer = QuotaFetchResponse {
            subscription: Some(QuotaSubscription::default()),
            summary: vec![QuotaMetric {
                key: b"k".to_vec(),
                label: b"\xff".to_vec(),
                value: -0.0,
                currency: b"USD".to_vec(),
                ..QuotaMetric::default()
            }],
            server_time_offset_ms: -3,
            groups: vec![QuotaGroup {
                display_name: Vec::new(),
                buckets: vec![
                    QuotaBucket {
                        remaining_fraction: 1e-7,
                        ..QuotaBucket::default()
                    },
                    QuotaBucket {
                        window: b"w".to_vec(),
                        remaining_fraction: 1e21,
                        ..QuotaBucket::default()
                    },
                ],
            }],
        };
        let json = answer.to_json().map(|json| json.encode());
        assert_eq!(
            json.as_deref(),
            Some(
                r#"{"subscription":{},"summary":[{"key":"k","label":"\ufffd","value":-0,"currency":"USD"}],"serverTimeOffsetMs":-3,"groups":[{"buckets":[{"remainingFraction":1e-7},{"window":"w","remainingFraction":1e+21}]}]}"#
            )
        );
        let mut infinite = answer;
        infinite.groups = vec![QuotaGroup {
            display_name: Vec::new(),
            buckets: vec![QuotaBucket {
                remaining_fraction: f64::INFINITY,
                ..QuotaBucket::default()
            }],
        }];
        assert!(infinite.to_json().is_none());
    }

    // Not upstream's: the top-level members, as Go reads them into a map.
    #[test]
    fn top_level_members() {
        let members = top_level(br#" {"b":1,"a":{"x":[1,"]"]},"b":"2"} "#);
        let members: Option<Vec<(String, String)>> = members.map(|map| {
            map.into_iter()
                .map(|(key, value)| (key, String::from_utf8_lossy(value).into_owned()))
                .collect()
        });
        assert_eq!(
            members,
            Some(vec![
                ("a".to_owned(), r#"{"x":[1,"]"]}"#.to_owned()),
                ("b".to_owned(), "\"2\"".to_owned()),
            ])
        );
        assert_eq!(top_level(b"null").map(|map| map.len()), Some(0));
        assert!(top_level(b"[]").is_none());
        assert!(top_level(b"5").is_none());
    }
}
