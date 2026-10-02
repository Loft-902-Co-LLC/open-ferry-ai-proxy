//! Structural comparison of upstream (Go) and open-ferry (Rust) output.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// A documented way our output may differ from upstream's (see UPSTREAM.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Deviation {
    /// Upstream sorts tool parameter schema keys; we keep the client's order.
    ParametersKeyOrder,
    /// JSON held in a string, whole or in part, is the same JSON written
    /// compactly by us. Only where the translator does so (see [`JsonForm`]).
    EmbeddedJson,
    /// Upstream cut a string in the middle of a character; we cut before it.
    CharBoundary,
    /// protobuf-go's error prefix has a non-breaking space in some builds; ours
    /// always has a regular one.
    ProtoErrorPrefix,
    /// Go converted a float too large for int64 as amd64 does, to the minimum
    /// int64; we saturate, as arm64 does.
    SaturatedInt,
    /// Upstream made up a user ID for a client that sent none; we leave it out.
    SyntheticUserId,
}

impl Deviation {
    pub fn describe(self) -> &'static str {
        match self {
            Self::ParametersKeyOrder => "tool parameter key order",
            Self::EmbeddedJson => "embedded JSON re-serialized",
            Self::CharBoundary => "cut at a character boundary",
            Self::ProtoErrorPrefix => "protobuf error prefix space",
            Self::SaturatedInt => "out-of-range number saturated",
            Self::SyntheticUserId => "made-up user ID left out",
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
    /// Whether `rust` is `go` with its JSON written in this form.
    fn matches(self, go: &str, rust: &str) -> bool {
        match self {
            Self::Whole => serde_json::from_str(go).is_ok_and(|value| compact(&value) == rust),
            Self::InText => {
                let (go, rust) = (json_parts(go), json_parts(rust));
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

/// A string where a translator writes JSON in a [`JsonForm`]: its path, with
/// `[*]` for every array index as [`Difference::shape`] writes it, and the form.
pub type JsonAt = (&'static str, JsonForm);

/// protobuf-go's error prefix as some builds write it.
const NBSP_PROTO_PREFIX: &str = "proto:\u{a0}";

/// What amd64 Go gives for `int64(f)` when `f` is out of range.
const GO_AMD64_OUT_OF_RANGE: &str = "-9223372036854775808";

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

/// Compares the outputs. JSON in a string may differ from upstream's only at
/// the paths in `embedded_json`, and only in the form given there.
pub fn compare(go: &Value, rust: &Value, embedded_json: &[JsonAt]) -> Comparison {
    let mut walker = Walker {
        path: Vec::new(),
        embedded_json,
        out: Comparison::default(),
    };
    walker.walk(go, rust);
    walker.out
}

enum Segment<'a> {
    Key(&'a str),
    Index(usize),
}

struct Walker<'a> {
    path: Vec<Segment<'a>>,
    embedded_json: &'a [JsonAt],
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
                if go.to_string() == GO_AMD64_OUT_OF_RANGE && rust.as_i64() == Some(i64::MAX) =>
            {
                self.out.deviations.insert(Deviation::SaturatedInt);
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
        // Upstream's byte cut leaves a partial character, which reaches us as
        // U+FFFD. Without it the strings match if we cut at the boundary before.
        if go.contains('\u{FFFD}')
            && !rust.contains('\u{FFFD}')
            && go.replace('\u{FFFD}', "") == rust
        {
            self.out.deviations.insert(Deviation::CharBoundary);
            return;
        }
        if go.contains(NBSP_PROTO_PREFIX) && go.replace(NBSP_PROTO_PREFIX, "proto: ") == rust {
            self.out.deviations.insert(Deviation::ProtoErrorPrefix);
            return;
        }
        if self.json_form().is_some_and(|form| form.matches(go, rust)) {
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
            .find(|(path, _)| *path == shape)
            .map(|&(_, form)| form)
    }

    /// Reports whether the walk is inside `tools[i].parameters`.
    fn in_tool_parameters(&self) -> bool {
        matches!(
            self.path.as_slice(),
            [
                Segment::Key("tools"),
                Segment::Index(_),
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

/// Splits `text` into the JSON objects and arrays embedded in it, each with
/// its value, and the runs of other text between them.
pub fn json_parts(text: &str) -> Vec<(&str, Option<Value>)> {
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
        parts.push((&text[start..end], Some(value)));
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
        let go = json!({ "name": "aé\u{FFFD}_1" });
        let rust = json!({ "name": "aé_1" });
        let cmp = compare(&go, &rust, JSON_AT);
        assert!(cmp.differences.is_empty());
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::CharBoundary]));
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
}
