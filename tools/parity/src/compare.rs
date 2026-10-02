//! Structural comparison of upstream (Go) and open-ferry (Rust) output.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// A documented way our output may differ from upstream's (see UPSTREAM.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Deviation {
    /// Upstream sorts tool parameter schema keys; we keep the client's order.
    ParametersKeyOrder,
    /// JSON held in a string, whole or in part, is the same JSON serialized differently.
    EmbeddedJson,
    /// Upstream cut a string in the middle of a character; we cut before it.
    CharBoundary,
}

impl Deviation {
    pub fn describe(self) -> &'static str {
        match self {
            Self::ParametersKeyOrder => "tool parameter key order",
            Self::EmbeddedJson => "embedded JSON re-serialized",
            Self::CharBoundary => "cut at a character boundary",
        }
    }
}

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

pub fn compare(go: &Value, rust: &Value) -> Comparison {
    let mut walker = Walker {
        path: Vec::new(),
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
        let whole_json_identical = match (
            serde_json::from_str::<Value>(go),
            serde_json::from_str::<Value>(rust),
        ) {
            (Ok(go), Ok(rust)) => identical(&go, &rust),
            _ => false,
        };
        if whole_json_identical || minify_embedded_json(go) == minify_embedded_json(rust) {
            self.out.deviations.insert(Deviation::EmbeddedJson);
            return;
        }
        self.differ(Value::from(go).to_string(), Value::from(rust).to_string());
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
        let mut path = String::from("$");
        for segment in &self.path {
            match segment {
                Segment::Key(key) => {
                    path.push('.');
                    path.push_str(key);
                }
                Segment::Index(index) => path.push_str(&format!("[{index}]")),
            }
        }
        self.out.differences.push(Difference { path, go, rust });
    }
}

/// Rewrites each JSON object or array embedded in `text` in compact form.
/// Key order and number text are kept, so only formatting and escapes change.
fn minify_embedded_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(['{', '[']) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        let skip = match values.next() {
            Some(Ok(value)) => {
                out.push_str(&value.to_string());
                values.byte_offset()
            }
            _ => {
                out.push_str(&rest[..1]);
                1
            }
        };
        rest = &rest[skip..];
    }
    out.push_str(rest);
    out
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

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn key_order_is_a_deviation_only_inside_tool_parameters() {
        let go = parse(r#"{"tools":[{"parameters":{"a":1,"b":2}}],"x":{"a":1,"b":2}}"#);
        let rust = parse(r#"{"tools":[{"parameters":{"b":2,"a":1}}],"x":{"b":2,"a":1}}"#);
        let cmp = compare(&go, &rust);
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
        let cmp = compare(&go, &rust);
        assert!(cmp.differences.is_empty());
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::EmbeddedJson]));

        let go = json!({ "arguments": r#""<\/a>""# });
        let rust = json!({ "arguments": r#""</a>""# });
        assert!(compare(&go, &rust).differences.is_empty());
    }

    #[test]
    fn json_inside_text_is_compared_by_value() {
        let go = json!({ "text": "<r>\n{\n  \"a\": [1.50, \"x\"]\n} and [ 2 ]\n</r> {" });
        let rust = json!({ "text": "<r>\n{\"a\":[1.50,\"x\"]} and [2]\n</r> {" });
        let cmp = compare(&go, &rust);
        assert!(cmp.differences.is_empty());
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::EmbeddedJson]));

        // Key order and number text inside the JSON still count.
        let go = json!({ "text": "x {\"a\":1,\"b\":2.0}" });
        let rust = json!({ "text": "x {\"b\":2.0,\"a\":1}" });
        assert_eq!(compare(&go, &rust).differences.len(), 1);
        let rust = json!({ "text": "x {\"a\":1,\"b\":2}" });
        assert_eq!(compare(&go, &rust).differences.len(), 1);
    }

    #[test]
    fn a_split_character_is_equivalent_to_cutting_before_it() {
        let go = json!({ "name": "aé\u{FFFD}_1" });
        let rust = json!({ "name": "aé_1" });
        let cmp = compare(&go, &rust);
        assert!(cmp.differences.is_empty());
        assert_eq!(cmp.deviations, BTreeSet::from([Deviation::CharBoundary]));
    }

    #[test]
    fn number_text_and_missing_keys_are_differences() {
        let go = parse(r#"{"n":1.50,"only_go":true,"list":[1,2]}"#);
        let rust = parse(r#"{"n":1.5,"list":[1]}"#);
        let paths: Vec<String> = compare(&go, &rust)
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
