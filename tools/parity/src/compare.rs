//! Structural comparison of upstream (Go) and open-ferry (Rust) output.

use std::collections::BTreeSet;

use open_ferry_translate::json::exact;
use serde_json::{Map, Value};

/// A documented way our output may differ from upstream's (see UPSTREAM.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Deviation {
    /// Upstream sorts tool parameter schema keys; we keep the client's order.
    ParametersKeyOrder,
    /// Upstream writes tool parameter schema numbers as Go writes a float64;
    /// we keep the client's text, which is the same number.
    ParametersNumberText,
    /// JSON held in a string, whole or in part, is the same JSON written
    /// compactly by us. Only where the translator does so (see [`JsonForm`]).
    EmbeddedJson,
    /// Upstream cut a name or ID to 64 bytes in the middle of a character; we
    /// cut at a character boundary.
    CharBoundary,
    /// protobuf-go's error prefix has a non-breaking space in some builds; ours
    /// always has a regular one.
    ProtoErrorPrefix,
    /// Go converted a float too large for int64 as amd64 does, to the minimum
    /// int64; we saturate, as arm64 does.
    SaturatedInt,
    /// Go wrote negative zero, a number it read as a float64, as `-0`; we
    /// write `0` (see UPSTREAM.md, "Numbers beyond f64"). Only at the paths
    /// the suite lists (see [`FloatPaths`]), and so only where numbers are
    /// read as written (see [`Numbers`]): `serde_json` reads `-0` as `0`.
    NegativeZero,
    /// Upstream made up a user ID for a client that sent none; we leave it out.
    SyntheticUserId,
    /// A Gemini response's `createTime` is the same instant, which upstream
    /// writes in the local time zone and we in UTC. Only the response's own
    /// field, not one in the data it carries.
    UtcCreateTime,
    /// A Chat Completions call ID derived from a Gemini call's JSON is
    /// derived from the JSON written compactly, where the client's text
    /// isn't. Ours is replaced by the ID upstream derives from the text,
    /// where it is the one we should derive (see `translator.rs`).
    CompactCallIdSource,
}

impl Deviation {
    pub fn describe(self) -> &'static str {
        match self {
            Self::ParametersKeyOrder => "tool parameter key order",
            Self::ParametersNumberText => "tool parameter number text",
            Self::EmbeddedJson => "embedded JSON re-serialized",
            Self::CharBoundary => "cut at a character boundary",
            Self::ProtoErrorPrefix => "protobuf error prefix space",
            Self::SaturatedInt => "out-of-range number saturated",
            Self::NegativeZero => "negative zero written as 0",
            Self::SyntheticUserId => "made-up user ID left out",
            Self::UtcCreateTime => "createTime written in UTC",
            Self::CompactCallIdSource => "call ID derived from compact JSON",
        }
    }
}

/// How a translator writes JSON it read where upstream copies the JSON's
/// text, so that the string differs from upstream's (see UPSTREAM.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonForm {
    /// The string is one JSON value, which we write compactly.
    Whole,
    /// The string holds JSON values within other text. We write each one
    /// compactly; the text around them is the same.
    InText,
    /// The string is JSON from Go's encoder, which escapes `<`, `>`, `&`,
    /// U+2028 and U+2029. We write them as they are.
    GoEscaped,
}

impl JsonForm {
    /// Whether `rust` is `go` with its JSON, read with `numbers`, written in
    /// this form.
    fn matches(self, go: &str, rust: &str, numbers: Numbers) -> bool {
        match self {
            Self::Whole => numbers
                .read(go)
                .is_some_and(|value| compact(&value) == rust),
            Self::InText => {
                let (go, rust) = (parts(go, numbers), parts(rust, numbers));
                go.len() == rust.len()
                    && go.iter().zip(&rust).all(|((go, value), (rust, _))| {
                        go == rust || value.as_ref().is_some_and(|value| compact(value) == *rust)
                    })
            }
            Self::GoEscaped => go_escaped(rust) == go,
        }
    }
}

/// `value` as we write it: compact, with key order and number text kept.
fn compact(value: &Value) -> String {
    value.to_string()
}

/// How a suite's translators keep the numbers in JSON they read, and so how
/// JSON held in a string is read to compare it (see [`JsonForm`]). Numbers
/// outside strings are compared by their text either way, as each suite's
/// `read` gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Numbers {
    /// As `serde_json` reads them, which writes `-0` as `0` and an exponent
    /// with a small `e` and a sign (`1E20` as `1e+20`). The suites other than
    /// the Interactions ones, whose translators keep no more.
    Respelled,
    /// Exactly as written, `-0` and exponents included (see
    /// [`exact`]). The Interactions suites, whose translators keep each
    /// number's text where upstream copies it.
    AsWritten,
}

impl Numbers {
    /// `text` as one JSON value, its numbers read this way.
    ///
    /// # Errors
    ///
    /// `serde_json`'s, where `text` isn't one JSON value.
    pub fn parse(self, text: &str) -> serde_json::Result<Value> {
        match self {
            Self::Respelled => serde_json::from_str(text),
            Self::AsWritten => exact::from_str(text),
        }
    }

    /// [`Self::parse`], or `None` where `text` isn't one JSON value.
    pub fn read(self, text: &str) -> Option<Value> {
        self.parse(text).ok()
    }
}

/// A string where a translator writes JSON in a [`JsonForm`]: its path, with
/// `[*]` for every array index as [`Difference::shape`] writes it, and the form.
/// `**` in a path stands for any run of keys and indices, none included, for
/// strings at any depth of a schema.
pub type JsonAt = (&'static str, JsonForm);

/// Paths, as [`JsonAt`] writes them, of numbers upstream reads as a
/// float64 and writes again, where Go writes negative zero as `-0` and we
/// write `0` (see [`Deviation::NegativeZero`]). Anywhere else, `-0` must stay
/// `-0`. The first path that matches decides; one that starts with `!` marks
/// a part of such a place that isn't one, listed before it.
pub type FloatPaths = &'static [&'static str];

/// Whether `shape`, a path as [`Difference::shape`] writes it, is `path`.
fn path_matches(path: &str, shape: &str) -> bool {
    match path.split_once("**") {
        Some((prefix, suffix)) => {
            shape.len() >= prefix.len() + suffix.len()
                && shape.starts_with(prefix)
                && shape.ends_with(suffix)
        }
        None => path == shape,
    }
}

/// protobuf-go's error prefix as some builds write it.
const NBSP_PROTO_PREFIX: &str = "proto:\u{a0}";

/// What amd64 Go gives for `int64(f)` when `f` is out of range.
const GO_AMD64_OUT_OF_RANGE: &str = "-9223372036854775808";

/// The bytes upstream cuts names and IDs to.
const CUT_LIMIT: usize = 64;

#[derive(Clone, Debug)]
pub struct Difference {
    /// Where the outputs differ, for example `$.input[2].content[0].text`.
    pub path: String,
    pub go: String,
    pub rust: String,
}

impl Difference {
    /// A difference in the whole output, such as a panic on one side.
    pub fn whole(go: String, rust: String) -> Self {
        Self {
            path: "$".into(),
            go,
            rust,
        }
    }

    /// The path with array indices replaced by `*`, to group similar differences.
    pub fn shape(&self) -> String {
        let mut shape = String::with_capacity(self.path.len());
        let mut in_index = false;
        for c in self.path.chars() {
            match c {
                '[' => {
                    in_index = true;
                    shape.push_str("[*]");
                }
                ']' => in_index = false,
                _ if in_index => {}
                c => shape.push(c),
            }
        }
        shape
    }
}

#[derive(Default)]
pub struct Comparison {
    pub deviations: BTreeSet<Deviation>,
    pub differences: Vec<Difference>,
}

/// [`compare_numbers`] for a suite whose numbers are respelled as
/// `serde_json` reads them, as most tests compare.
#[cfg(test)]
pub fn compare(go: &Value, rust: &Value, embedded_json: &[JsonAt]) -> Comparison {
    compare_numbers(go, rust, embedded_json, Numbers::Respelled, &[])
}

/// Compares the outputs. JSON in a string may differ from upstream's only at
/// the paths in `embedded_json`, and only in the form given there, its
/// numbers read as `numbers` says. Go's `-0` may be our `0` only at the
/// paths in `floats`.
pub fn compare_numbers(
    go: &Value,
    rust: &Value,
    embedded_json: &[JsonAt],
    numbers: Numbers,
    floats: FloatPaths,
) -> Comparison {
    let mut walker = Walker {
        path: Vec::new(),
        embedded_json,
        numbers,
        floats,
        out: Comparison::default(),
    };
    walker.walk(go, rust);
    walker.out
}

enum Segment<'a> {
    Key(&'a str),
    Index(usize),
}

/// Whether `go` is a name or ID upstream cut to [`CUT_LIMIT`] bytes in the
/// middle of a character, and `rust` the same one cut at a character
/// boundary. Go's JSON encoder writes each byte of the partial character as
/// U+FFFD, so `go` holds one run of one to three of them, and was
/// [`CUT_LIMIT`] bytes long with the partial character's bytes in their
/// place. Without the run, `go` must be `rust`.
fn cut_in_a_character(go: &str, rust: &str) -> bool {
    const REPLACEMENT: char = '\u{FFFD}';
    let Some(start) = go.find(REPLACEMENT) else {
        return false;
    };
    let run = go[start..]
        .chars()
        .take_while(|&c| c == REPLACEMENT)
        .count();
    let end = start + run * REPLACEMENT.len_utf8();
    let held = go.len() - run * (REPLACEMENT.len_utf8() - 1);
    (1..=3).contains(&run)
        && held == CUT_LIMIT
        && !go[end..].contains(REPLACEMENT)
        && rust.len() == start + (go.len() - end)
        && rust.starts_with(&go[..start])
        && rust.ends_with(&go[end..])
}

struct Walker<'a> {
    path: Vec<Segment<'a>>,
    embedded_json: &'a [JsonAt],
    numbers: Numbers,
    floats: FloatPaths,
    out: Comparison,
}

impl<'a> Walker<'a> {
    fn walk(&mut self, go: &'a Value, rust: &'a Value) {
        match (go, rust) {
            (Value::Object(go), Value::Object(rust)) => self.walk_objects(go, rust),
            (Value::Array(go), Value::Array(rust)) => {
                for (index, (go, rust)) in go.iter().zip(rust).enumerate() {
                    self.path.push(Segment::Index(index));
                    self.walk(go, rust);
                    self.path.pop();
                }
                if go.len() != rust.len() {
                    self.differ(
                        format!("{} items", go.len()),
                        format!("{} items", rust.len()),
                    );
                }
            }
            (Value::String(go), Value::String(rust)) => self.walk_strings(go, rust),
            (go, rust) if identical(go, rust) => {}
            (Value::Number(go), Value::Number(rust))
                if self.in_tool_parameters() && same_float(go, rust) =>
            {
                self.out.deviations.insert(Deviation::ParametersNumberText);
            }
            (Value::Number(go), Value::Number(rust))
                if go.to_string() == GO_AMD64_OUT_OF_RANGE && rust.as_i64() == Some(i64::MAX) =>
            {
                self.out.deviations.insert(Deviation::SaturatedInt);
            }
            (Value::Number(go), Value::Number(rust))
                if go.to_string() == "-0" && rust.to_string() == "0" && self.at_float() =>
            {
                self.out.deviations.insert(Deviation::NegativeZero);
            }
            (go, rust) => self.differ(go.to_string(), rust.to_string()),
        }
    }

    fn walk_objects(&mut self, go: &'a Map<String, Value>, rust: &'a Map<String, Value>) {
        for (key, go_value) in go {
            self.path.push(Segment::Key(key));
            match rust.get(key) {
                Some(rust_value) => self.walk(go_value, rust_value),
                None => self.differ(go_value.to_string(), "(missing)".into()),
            }
            self.path.pop();
        }
        for (key, rust_value) in rust {
            if !go.contains_key(key) {
                self.path.push(Segment::Key(key));
                self.differ("(missing)".into(), rust_value.to_string());
                self.path.pop();
            }
        }

        let same_keys = go.len() == rust.len() && go.keys().all(|key| rust.contains_key(key));
        if same_keys && !go.keys().eq(rust.keys()) {
            if self.in_tool_parameters() {
                self.out.deviations.insert(Deviation::ParametersKeyOrder);
            } else {
                let go_keys: Vec<&String> = go.keys().collect();
                let rust_keys: Vec<&String> = rust.keys().collect();
                self.differ(
                    format!("key order {go_keys:?}"),
                    format!("key order {rust_keys:?}"),
                );
            }
        }
    }

    fn walk_strings(&mut self, go: &str, rust: &str) {
        if go == rust {
            return;
        }
        if cut_in_a_character(go, rust) {
            self.out.deviations.insert(Deviation::CharBoundary);
            return;
        }
        if go.contains(NBSP_PROTO_PREFIX) && go.replace(NBSP_PROTO_PREFIX, "proto: ") == rust {
            self.out.deviations.insert(Deviation::ProtoErrorPrefix);
            return;
        }
        if matches!(
            self.path.as_slice(),
            [Segment::Key("createTime")] | [Segment::Index(_), Segment::Key("createTime")]
        ) && rust.ends_with('Z')
            && rfc3339_seconds(go).is_some_and(|go| rfc3339_seconds(rust) == Some(go))
        {
            self.out.deviations.insert(Deviation::UtcCreateTime);
            return;
        }
        if self
            .json_form()
            .is_some_and(|form| form.matches(go, rust, self.numbers))
        {
            self.out.deviations.insert(Deviation::EmbeddedJson);
            return;
        }
        self.differ(Value::from(go).to_string(), Value::from(rust).to_string());
    }

    /// How the translator writes JSON in the string at the walk's path, if it
    /// is one where we write JSON compactly.
    fn json_form(&self) -> Option<JsonForm> {
        let shape = self.path_text(false);
        self.embedded_json
            .iter()
            .find(|(path, _)| path_matches(path, &shape))
            .map(|&(_, form)| form)
    }

    /// Whether the walk is at a number upstream writes as a float64 (see
    /// [`FloatPaths`]).
    fn at_float(&self) -> bool {
        let shape = self.path_text(false);
        self.floats
            .iter()
            .find_map(|path| match path.strip_prefix('!') {
                Some(path) => path_matches(path, &shape).then_some(false),
                None => path_matches(path, &shape).then_some(true),
            })
            == Some(true)
    }

    /// Reports whether the walk is inside `tools[i].parameters`, or a Chat
    /// Completions request's `tools[i].function.parameters`.
    fn in_tool_parameters(&self) -> bool {
        matches!(
            self.path.as_slice(),
            [
                Segment::Key("tools"),
                Segment::Index(_),
                Segment::Key("parameters"),
                ..
            ] | [
                Segment::Key("tools"),
                Segment::Index(_),
                Segment::Key("function"),
                Segment::Key("parameters"),
                ..
            ]
        )
    }

    fn differ(&mut self, go: String, rust: String) {
        let path = self.path_text(true);
        self.out.differences.push(Difference { path, go, rust });
    }

    /// The walk's path, such as `$.input[2].text`, or with every index written
    /// `[*]` if `indices` is false.
    fn path_text(&self, indices: bool) -> String {
        let mut path = String::from("$");
        for segment in &self.path {
            match segment {
                Segment::Key(key) => {
                    path.push('.');
                    path.push_str(key);
                }
                Segment::Index(index) if indices => path.push_str(&format!("[{index}]")),
                Segment::Index(_) => path.push_str("[*]"),
            }
        }
        path
    }
}

/// The Unix time in seconds of an RFC 3339 time as Go's `time.RFC3339Nano`
/// layout writes a whole second, `[-]YYYY-MM-DDTHH:MM:SS` then `Z` or a
/// `+HH:MM` or `-HH:MM` offset, with a year of four digits or more. A year
/// too large for the result gives `None`.
pub fn rfc3339_seconds(text: &str) -> Option<i128> {
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (date, rest) = text.split_once('T')?;
    let (year, month_day) = date.split_once('-')?;
    let (month, day) = month_day.split_once('-')?;
    let (time, offset) = match rest.strip_suffix('Z') {
        Some(time) => (time, 0),
        None => {
            let at = rest.len().checked_sub(6)?;
            let (time, offset) = rest.split_at_checked(at)?;
            let sign = match offset.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (hours, minutes) = offset[1..].split_once(':')?;
            (time, sign * (number(hours)? * 3600 + number(minutes)? * 60))
        }
    };
    let mut clock = time.split(':');
    let (hour, minute, second) = (clock.next()?, clock.next()?, clock.next()?);
    if clock.next().is_some()
        || year.len() < 4
        || [month, day, hour, minute, second]
            .iter()
            .any(|part| part.len() != 2)
    {
        return None;
    }
    let year = if negative {
        -number(year)?
    } else {
        number(year)?
    };
    let days = days_from_civil(year, number(month)?, number(day)?)?;
    let clock = number(hour)? * 3600 + number(minute)? * 60 + number(second)? - offset;
    days.checked_mul(86_400)?.checked_add(clock)
}

fn number(digits: &str) -> Option<i128> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Days from 1970-01-01 to a proleptic Gregorian date, by Howard Hinnant's
/// `days_from_civil`. Month and day are two digits.
fn days_from_civil(year: i128, month: i128, day: i128) -> Option<i128> {
    let year = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)?.checked_add(day_of_era - 719_468)
}

/// Splits `text` into the JSON objects and arrays embedded in it, each with
/// its value, and the runs of other text between them.
pub fn json_parts(text: &str) -> Vec<(&str, Option<Value>)> {
    parts(text, Numbers::Respelled)
}

/// [`json_parts`], with each value's numbers read as `numbers` says.
fn parts(text: &str, numbers: Numbers) -> Vec<(&str, Option<Value>)> {
    let mut parts = Vec::new();
    let (mut plain, mut at) = (0, 0);
    while let Some(offset) = text[at..].find(['{', '[']) {
        let start = at + offset;
        let mut values = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        let Some(Ok(value)) = values.next() else {
            at = start + 1;
            continue;
        };
        if plain < start {
            parts.push((&text[plain..start], None));
        }
        let end = start + values.byte_offset();
        let value = match numbers {
            Numbers::Respelled => Some(value),
            Numbers::AsWritten => numbers.read(&text[start..end]),
        };
        parts.push((&text[start..end], value));
        (plain, at) = (end, end);
    }
    if plain < text.len() {
        parts.push((&text[plain..], None));
    }
    parts
}

/// `json` as Go's encoder writes it, which escapes `<`, `>`, `&`, U+2028 and
/// U+2029. In compact JSON they can only be in strings.
fn go_escaped(json: &str) -> String {
    let mut escaped = String::with_capacity(json.len());
    for c in json.chars() {
        match c {
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => {
                escaped.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => escaped.push(c),
        }
    }
    escaped
}

/// Whether two numbers, written differently, are the same finite float64.
fn same_float(a: &serde_json::Number, b: &serde_json::Number) -> bool {
    a.as_f64()
        .zip(b.as_f64())
        .is_some_and(|(a, b)| a.is_finite() && a == b)
}

/// Exact equality: object key order and number text count, unlike `Value`'s `==`.
fn identical(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|((ka, va), (kb, vb))| ka == kb && identical(va, vb))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| identical(a, b))
        }
        (Value::Number(a), Value::Number(b)) => a.to_string() == b.to_string(),
        (a, b) => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::cases::Case;
    use crate::translator::Translator;

    /// Where the outputs below hold JSON, as a translator lists it.
    const JSON_AT: &[JsonAt] = &[
        ("$.arguments", JsonForm::Whole),
        ("$.text", JsonForm::InText),
        ("$.partial_json", JsonForm::GoEscaped),
    ];

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn key_order_is_a_deviation_only_inside_tool_parameters() {
        let go = parse(r#"{"tools":[{"parameters":{"a":1,"b":2}}],"x":{"a":1,"b":2}}"#);
        let rust = parse(r#"{"tools":[{"parameters":{"b":2,"a":1}}],"x":{"b":2,"a":1}}"#);
        let cmp = compare(&go, &rust, JSON_AT);
        assert_eq!(
            cmp.deviations,
            BTreeSet::from([Deviation::ParametersKeyOrder])
        );
        assert_eq!(cmp.differences.len(), 1);
        assert_eq!(cmp.differences[0].path, "$.x");

        // A Chat Completions function's parameters too.
        let go = parse(r#"{"tools":[{"function":{"parameters":{"a":1,"b":2}}}]}"#);
        let rust = parse(r#"{"tools":[{"function":{"parameters":{"b":2,"a":1}}}]}"#);
        let cmp = compare(&go, &rust, JSON_AT);
        assert!(cmp.differences.is_empty());
        assert_eq!(
            cmp.deviations,
            BTreeSet::from([Deviation::ParametersKeyOrder])
        );
    }

    #[test]
    fn number_text_is_a_deviation_only_inside_tool_parameters() {
        let go = parse(r#"{"tools":[{"function":{"parameters":{"max":100,"min":0.5}}}]}"#);
        let rust = parse(r#"{"tools":[{"function":{"parameters":{"max":1e2,"min":0.50}}}]}"#);
        let cmp = compare(&go, &rust, JSON_AT);
        assert!(cmp.differences.is_empty());
        assert_eq!(
            cmp.deviations,
            BTreeSet::from([Deviation::ParametersNumberText])
        );

        // A different number is still a difference.
        let rust = parse(r#"{"tools":[{"function":{"parameters":{"max":101,"min":0.5}}}]}"#);
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
        // And so is other number text.
        let go = parse(r#"{"x":{"n":100}}"#);
        let rust = parse(r#"{"x":{"n":1e2}}"#);
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
    }

    #[test]
    fn strings_holding_the_same_json_are_equivalent() {
        let go = json!({ "arguments": "{ \"a\": \"caf\\u00e9\" }" });
        let rust = json!({ "arguments": "{\"a\":\"café\"}" });
        let cmp = compare(&go, &rust, JSON_AT);
        assert!(cmp.differences.is_empty());
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::EmbeddedJson]));

        let go = json!({ "arguments": r#""<\/a>""# });
        let rust = json!({ "arguments": r#""</a>""# });
        assert!(compare(&go, &rust, JSON_AT).differences.is_empty());
    }

    #[test]
    fn json_inside_text_is_compared_by_value() {
        let go = json!({ "text": "<r>\n{\n  \"a\": [1.50, \"x\"]\n} and [ 2 ]\n</r> {" });
        let rust = json!({ "text": "<r>\n{\"a\":[1.50,\"x\"]} and [2]\n</r> {" });
        let cmp = compare(&go, &rust, JSON_AT);
        assert!(cmp.differences.is_empty());
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::EmbeddedJson]));

        // Key order and number text inside the JSON still count.
        let go = json!({ "text": "x {\"a\":1,\"b\":2.0}" });
        let rust = json!({ "text": "x {\"b\":2.0,\"a\":1}" });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
        let rust = json!({ "text": "x {\"a\":1,\"b\":2}" });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
    }

    #[test]
    fn json_in_other_strings_is_compared_as_text() {
        // Text from the provider is compared exactly, even when it holds JSON.
        let delta = |text: &str| {
            json!([{ "event": "content_block_delta", "data": {
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": text }
            } }])
        };
        let go = delta("prefix { \"a\": 1 }");
        let rust = delta("prefix {\"a\":1}");
        let cmp = compare(
            &go,
            &rust,
            Translator::Stream.embedded_json(&Case::new("", "", "")),
        );
        assert_eq!(cmp.differences.len(), 1);
        assert_eq!(cmp.differences[0].path, "$[0].data.delta.text");
        assert!(cmp.deviations.is_empty());

        let go = json!({ "other": "{ \"a\": 1 }", "text": "x" });
        let rust = json!({ "other": "{\"a\":1}", "text": "x" });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
    }

    #[test]
    fn number_text_in_listed_json_counts_where_kept() {
        // Upstream copies `-0`, `1E20` and `1e5` as written. Read as
        // `serde_json` reads them, compact JSON respells them; where the
        // translators keep them, it must not.
        let go = json!({ "arguments": "{ \"x\": -0, \"y\": 1E20, \"z\": [1e5] }" });
        let respelled = json!({ "arguments": r#"{"x":0,"y":1e+20,"z":[1e+5]}"# });
        let kept = json!({ "arguments": r#"{"x":-0,"y":1E20,"z":[1e5]}"# });
        let respelled_cmp = compare(&go, &respelled, JSON_AT);
        assert!(respelled_cmp.differences.is_empty());
        assert_eq!(compare(&go, &kept, JSON_AT).differences.len(), 1);

        let as_written = |rust| compare_numbers(&go, rust, JSON_AT, Numbers::AsWritten, &[]);
        let cmp = as_written(&kept);
        assert!(cmp.differences.is_empty(), "{:?}", cmp.differences);
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::EmbeddedJson]));
        assert_eq!(as_written(&respelled).differences.len(), 1);

        // Within text too.
        let go = json!({ "text": "x { \"n\": -0 } [ 1E2 ] y" });
        let kept = json!({ "text": r#"x {"n":-0} [1E2] y"# });
        let respelled = json!({ "text": r#"x {"n":0} [1e+2] y"# });
        assert!(
            compare_numbers(&go, &kept, JSON_AT, Numbers::AsWritten, &[])
                .differences
                .is_empty()
        );
        assert_eq!(
            compare_numbers(&go, &respelled, JSON_AT, Numbers::AsWritten, &[])
                .differences
                .len(),
            1
        );
        assert!(compare(&go, &respelled, JSON_AT).differences.is_empty());
    }

    #[test]
    fn listed_json_must_be_in_its_form() {
        let go = json!({ "arguments": "{\n  \"a\": [1.50]\n}" });
        let rust = json!({ "arguments": "{\"a\":[1.50]}" });
        let cmp = compare(&go, &rust, JSON_AT);
        assert!(cmp.differences.is_empty());
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::EmbeddedJson]));

        // The same JSON written some other way is a difference.
        let rust = json!({ "arguments": "{\"a\": [1.50]}" });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
        let go = json!({ "arguments": "{\"a\":[1.50]}" });
        let rust = json!({ "arguments": "{\n  \"a\": [1.50]\n}" });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
        // So is JSON within text where the whole string is JSON.
        let go = json!({ "arguments": "x { \"a\": 1 }" });
        let rust = json!({ "arguments": "x {\"a\":1}" });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
        // And within text, a change to the text around the JSON.
        let go = json!({ "text": "<r>\n{ \"a\": 1 }\n</r>" });
        let rust = json!({ "text": "<r>{\"a\":1}</r>" });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
    }

    #[test]
    fn go_escapes_are_equivalent_where_listed() {
        let go = json!({ "partial_json": format!(r#"{{"query":"a {b}u003c b {b}u0026 c"}}"#, b = '\\') });
        let rust = json!({ "partial_json": r#"{"query":"a < b & c"}"# });
        let cmp = compare(&go, &rust, JSON_AT);
        assert!(cmp.differences.is_empty());
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::EmbeddedJson]));

        // Function call arguments are passed through, never re-serialized.
        let go = json!({ "partial_json": r#"{"query": "a"}"# });
        let rust = json!({ "partial_json": r#"{"query":"a"}"# });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
    }

    #[test]
    fn a_split_character_is_equivalent_to_cutting_before_it() {
        // Cut at the end: 61 bytes, two bytes of a three-byte character, and
        // a suffix added after the cut.
        let head = "a".repeat(60);
        let go = json!({ "name": format!("{head}\u{FFFD}\u{FFFD}_1") });
        let rust = json!({ "name": format!("{head}_1") });
        let cmp = compare(&go, &rust, JSON_AT);
        assert!(cmp.differences.is_empty(), "{:?}", cmp.differences);
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::CharBoundary]));

        // Cut at the start: the last byte of a character, then 63 bytes.
        let tail = "b".repeat(63);
        let go = json!({ "name": format!("\u{FFFD}{tail}") });
        let rust = json!({ "name": tail });
        let cmp = compare(&go, &rust, JSON_AT);
        assert!(cmp.differences.is_empty(), "{:?}", cmp.differences);
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::CharBoundary]));
    }

    #[test]
    fn replacement_characters_elsewhere_still_differ() {
        // Text that isn't a 64-byte cut: losing its U+FFFD is a difference.
        let go = json!({ "delta": { "text": "a\u{FFFD}b" } });
        let rust = json!({ "delta": { "text": "ab" } });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);

        // A cut name that lost more than the partial character.
        let head = "a".repeat(62);
        let go = json!({ "name": format!("{head}\u{FFFD}\u{FFFD}") });
        let rust = json!({ "name": "a".repeat(61) });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);

        // Two runs, one of them not from the cut.
        let rest = "a".repeat(60);
        let go = json!({ "name": format!("\u{FFFD}{rest}\u{FFFD}") });
        let rust = json!({ "name": format!("{rest}\u{FFFD}") });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
    }

    #[test]
    fn protobuf_error_prefix_space_is_equivalent() {
        let go = json!({ "error": "malformed protobuf tag: proto:\u{a0}unexpected EOF" });
        let rust = json!({ "error": "malformed protobuf tag: proto: unexpected EOF" });
        let cmp = compare(&go, &rust, JSON_AT);
        assert!(cmp.differences.is_empty());
        assert_eq!(
            cmp.deviations,
            BTreeSet::from([Deviation::ProtoErrorPrefix])
        );

        let rust = json!({ "error": "malformed protobuf tag: unexpected EOF" });
        assert_eq!(compare(&go, &rust, JSON_AT).differences.len(), 1);
    }

    #[test]
    fn a_saturated_number_is_equivalent_to_amd64_overflow() {
        let go = parse(r#"{"n":-9223372036854775808}"#);
        let cmp = compare(&go, &json!({ "n": i64::MAX }), JSON_AT);
        assert!(cmp.differences.is_empty());
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::SaturatedInt]));

        assert_eq!(
            compare(&go, &json!({ "n": 0 }), JSON_AT).differences.len(),
            1
        );
    }

    #[test]
    fn negative_zero_from_go_may_be_zero_only_where_upstream_reads_a_float() {
        // Not upstream's: where upstream writes a float64, Go writes `-0`
        // and we `0`; where it copies the text, `-0` must stay.
        const FLOATS: FloatPaths = &["$.t", "!$.m.raw**", "$.m.**"];
        let go = exact::from_str(r#"{"t":-0,"n":-0,"m":{"a":[-0],"raw":{"b":-0}}}"#).unwrap();
        let rust = json!({ "t": 0, "n": 0, "m": { "a": [0], "raw": { "b": 0 } } });
        let cmp = compare_numbers(&go, &rust, JSON_AT, Numbers::AsWritten, FLOATS);
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::NegativeZero]));
        let paths: Vec<String> = cmp.differences.into_iter().map(|d| d.path).collect();
        assert_eq!(paths, ["$.n", "$.m.raw.b"]);

        // Not the other way, nor for other spellings.
        let go = exact::from_str(r#"{"t":0,"m":{"b":-0.0,"c":1E2}}"#).unwrap();
        let rust = exact::from_str(r#"{"t":-0,"m":{"b":0,"c":1e+2}}"#).unwrap();
        let cmp = compare_numbers(&go, &rust, JSON_AT, Numbers::AsWritten, FLOATS);
        assert_eq!(cmp.differences.len(), 3);
        assert!(cmp.deviations.is_empty());
    }

    #[test]
    fn only_a_response_create_time_may_be_in_utc() {
        let local = "2023-11-14T16:13:20-06:00";
        let utc = "2023-11-14T22:13:20Z";
        let response = |time: &str| json!({ "createTime": time });
        let cmp = compare(&response(local), &response(utc), JSON_AT);
        assert!(cmp.differences.is_empty());
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::UtcCreateTime]));
        let chunks = |time: &str| json!([{}, { "createTime": time }]);
        assert!(
            compare(&chunks(local), &chunks(utc), JSON_AT)
                .differences
                .is_empty()
        );

        // The same time in a function call's arguments must be kept as it is.
        let call = |time: &str| {
            json!({ "candidates": [{ "content": { "parts": [
                { "functionCall": { "name": "f", "args": { "createTime": time } } }
            ] } }] })
        };
        assert_eq!(
            compare(&call(local), &call(utc), JSON_AT).differences.len(),
            1
        );
        // And another instant is a difference.
        let later = "2023-11-14T22:13:21Z";
        assert_eq!(
            compare(&response(local), &response(later), JSON_AT)
                .differences
                .len(),
            1
        );
    }

    #[test]
    fn extreme_times_are_read_without_overflow() {
        assert_eq!(rfc3339_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_seconds("1969-12-31T18:00:00-06:00"), Some(0));
        assert_eq!(
            rfc3339_seconds("292277026596-12-04T15:30:07Z"),
            Some(i128::from(i64::MAX))
        );
        assert_eq!(
            rfc3339_seconds("-292277022657-01-27T08:29:52Z"),
            Some(i128::from(i64::MIN))
        );
        assert_eq!(
            rfc3339_seconds("292277026596-12-04T09:30:08-06:00"),
            rfc3339_seconds("292277026596-12-04T15:30:08Z")
        );
        let huge = format!("{}-01-01T00:00:00Z", "9".repeat(38));
        assert_eq!(rfc3339_seconds(&huge), None);
        let huge = format!("-{}-01-01T00:00:00Z", "9".repeat(38));
        assert_eq!(rfc3339_seconds(&huge), None);
    }

    #[test]
    fn number_text_and_missing_keys_are_differences() {
        let go = parse(r#"{"n":1.50,"only_go":true,"list":[1,2]}"#);
        let rust = parse(r#"{"n":1.5,"list":[1]}"#);
        let paths: Vec<String> = compare(&go, &rust, JSON_AT)
            .differences
            .into_iter()
            .map(|d| d.path)
            .collect();
        assert_eq!(paths, ["$.n", "$.only_go", "$.list"]);
    }

    #[test]
    fn shape_hides_indices() {
        let difference = Difference {
            path: "$.input[12].content[0].text".into(),
            go: String::new(),
            rust: String::new(),
        };
        assert_eq!(difference.shape(), "$.input[*].content[*].text");
    }

    #[test]
    fn a_double_star_matches_any_run_of_segments() {
        let path = "$.tools[*].schema**.description";
        assert!(path_matches(path, "$.tools[*].schema.description"));
        assert!(path_matches(
            path,
            "$.tools[*].schema.properties.a.items[*].description"
        ));
        assert!(!path_matches(path, "$.tools[*].description"));
        assert!(!path_matches(path, "$.tools[*].schema.title"));
        assert!(path_matches("$.a[*].b", "$.a[*].b"));
        assert!(!path_matches("$.a[*].b", "$.a[*].b.c"));
    }
}
