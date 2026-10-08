//! Config values as the commands read and change them: the config file as
//! a JSON tree in the v8 layout, paths into it, and the changes between
//! two trees.
//!
//! A path is a setting's keys, dotted (`routing.strategy`) or, when a key
//! holds a dot, with slashes (`oauth/model-alias/gpt-5.1`). It must name a
//! setting or section a v8 config may hold, or a key under a setting whose
//! keys are the user's (such as `headers`); a list is set whole.

use std::path::Path;

use open_ferry_core::config::v8_edit::{KnownKind, KnownPath, is_known_v8_path, known_v8_paths};
use open_ferry_core::config::{AnyValue, V8Document};
use serde::Serialize;
use serde_json::{Map, Value};

use super::Failure;

/// The JSON of a value yaml.v3 decoded: a time as its text, a number that
/// isn't finite and a mapping with keys that aren't all strings as `null`.
pub(crate) fn any_to_json(value: &AnyValue) -> Value {
    match value {
        AnyValue::Null | AnyValue::AnyMap => Value::Null,
        AnyValue::Bool(value) => Value::Bool(*value),
        AnyValue::Int(value) => Value::from(*value),
        AnyValue::Uint(value) => Value::from(*value),
        AnyValue::Float(value) => {
            serde_json::Number::from_f64(*value).map_or(Value::Null, Value::Number)
        }
        AnyValue::Str(value) => Value::String(value.clone()),
        AnyValue::Time(text, _) => text.clone().map_or(Value::Null, Value::String),
        AnyValue::Seq(items) => Value::Array(items.iter().map(any_to_json).collect()),
        AnyValue::Map(entries) => Value::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), any_to_json(value)))
                .collect(),
        ),
    }
}

/// The config `data` holds, in the v8 layout, as JSON.
pub(crate) fn tree_of(data: &[u8]) -> Result<Value, Failure> {
    let document = V8Document::migrate(data).map_err(|error| {
        Failure::new(
            "invalid_config",
            format!("the config doesn't load: {error}"),
        )
    })?;
    document_tree(&document)
}

/// `document` as JSON.
pub(crate) fn document_tree(document: &V8Document) -> Result<Value, Failure> {
    match document.value(&[]) {
        Some(Ok(value)) => Ok(any_to_json(&value)),
        Some(Err(error)) => Err(Failure::new(
            "invalid_config",
            format!("the config doesn't load: {error}"),
        )),
        None => Ok(Value::Object(Map::new())),
    }
}

/// The config file at `path`, in the v8 layout, as JSON.
pub(crate) fn read_tree(path: &Path) -> Result<Value, Failure> {
    let data = std::fs::read(path).map_err(|error| {
        Failure::new(
            "not_found",
            format!("can't read {}: {error}", path.display()),
        )
    })?;
    tree_of(&data)
}

/// The value YAML or JSON `text` holds.
pub(crate) fn parse_value(text: &str) -> Result<Value, Failure> {
    AnyValue::parse_yaml(text)
        .map(|value| any_to_json(&value))
        .map_err(|error| Failure::usage(format!("the value isn't YAML or JSON: {error}")))
}

/// `text` as a path: split on `/` when it has one, else on `.`.
pub(crate) fn split_path(text: &str) -> Vec<String> {
    let text = text.trim();
    let (text, separator) = if text.contains('/') {
        (text.trim_matches('/'), '/')
    } else {
        (text.trim_matches('.'), '.')
    };
    if text.is_empty() {
        return Vec::new();
    }
    text.split(separator).map(str::to_owned).collect()
}

/// `parts`, dotted.
pub(crate) fn dotted(parts: &[String]) -> String {
    parts.join(".")
}

/// Checks that `parts` names a setting: a usage failure naming the nearest
/// known one when it doesn't, or the list it is under.
pub(crate) fn check_path(parts: &[String]) -> Result<(), Failure> {
    if parts.is_empty() {
        return Err(Failure::usage("name a setting, such as routing.strategy"));
    }
    if parts.iter().any(String::is_empty) {
        return Err(Failure::new(
            "unknown_path",
            format!("{} has an empty key", parts.join("/")),
        ));
    }
    let known = known_v8_paths();
    let borrowed: Vec<&str> = parts.iter().map(String::as_str).collect();
    if is_known_v8_path(&known, &borrowed) {
        return Ok(());
    }
    let path = dotted(parts);
    for end in 1..parts.len() {
        let prefix = dotted(parts.get(..end).unwrap_or_default());
        if known
            .iter()
            .any(|entry| entry.kind == KnownKind::List && entry.path == prefix)
        {
            return Err(Failure::new(
                "unknown_path",
                format!("{path} is inside the list {prefix}, and lists are set whole"),
            )
            .hint(format!(
                "read the list with `open-ferry config get {prefix}`, then set it whole with `open-ferry config set {prefix} --from-file <file>`"
            )));
        }
    }
    let failure = Failure::new("unknown_path", format!("unknown setting: {path}"));
    Err(match nearest(&known, parts) {
        Some(near) => failure.hint(format!("did you mean {near}?")),
        None => failure,
    })
}

/// The known path nearest `parts`, by edit distance, among those with the
/// same last key if any has it.
pub(crate) fn nearest(known: &[KnownPath], parts: &[String]) -> Option<String> {
    let path = dotted(parts);
    let last = parts.last()?;
    let same_last: Vec<&KnownPath> = known
        .iter()
        .filter(|entry| entry.path.rsplit('.').next() == Some(last.as_str()))
        .collect();
    let candidates: Vec<&KnownPath> = if same_last.is_empty() {
        known.iter().collect()
    } else {
        same_last
    };
    candidates
        .into_iter()
        .filter(|entry| entry.path != "config-version")
        .min_by_key(|entry| (distance(&entry.path, &path), entry.path.len()))
        .map(|entry| entry.path.clone())
}

/// The Levenshtein distance between `a` and `b`, by character.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = Vec::with_capacity(b.len() + 1);
        current.push(i + 1);
        for (j, cb) in b.iter().enumerate() {
            let substitute = previous.get(j).copied().unwrap_or(0) + usize::from(ca != *cb);
            let delete = previous.get(j + 1).copied().unwrap_or(0) + 1;
            let insert = current.get(j).copied().unwrap_or(0) + 1;
            current.push(substitute.min(delete).min(insert));
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(0)
}

/// The value at `parts` in `root`.
pub(crate) fn get<'a>(root: &'a Value, parts: &[String]) -> Option<&'a Value> {
    parts.iter().try_fold(root, |value, part| value.get(part))
}

/// One setting that differs between two configs.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Change {
    /// Its keys, dotted.
    pub(crate) path: String,
    /// Its value before, when it was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) old: Option<Value>,
    /// Its value after, when it is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) new: Option<Value>,
    /// Its keys.
    #[serde(skip)]
    pub(crate) parts: Vec<String>,
}

/// The settings that differ between `before` and `after`: mappings are
/// compared key by key, anything else whole.
pub(crate) fn diff(before: &Value, after: &Value) -> Vec<Change> {
    let mut changes = Vec::new();
    walk(&mut Vec::new(), Some(before), Some(after), &mut changes);
    changes
}

fn walk(
    path: &mut Vec<String>,
    before: Option<&Value>,
    after: Option<&Value>,
    out: &mut Vec<Change>,
) {
    let before_map = before.and_then(Value::as_object);
    let after_map = after.and_then(Value::as_object);
    let maps = match (before_map, after_map) {
        (Some(_), Some(_)) => true,
        (Some(_), None) => after.is_none(),
        (None, Some(_)) => before.is_none(),
        (None, None) => false,
    };
    if maps {
        let mut keys: Vec<&String> = before_map
            .map(|map| map.keys().collect())
            .unwrap_or_default();
        for key in after_map.into_iter().flat_map(Map::keys) {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        for key in keys {
            path.push(key.clone());
            walk(
                path,
                before_map.and_then(|map| map.get(key)),
                after_map.and_then(|map| map.get(key)),
                out,
            );
            path.pop();
        }
        return;
    }
    if before != after {
        out.push(Change {
            path: dotted(path),
            old: before.cloned(),
            new: after.cloned(),
            parts: path.clone(),
        });
    }
}

/// A change, as text: `path: old -> new`, with `(not set)` for a side
/// that has none.
pub(crate) fn change_line(change: &Value) -> String {
    let path = change
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let side = |name: &str| {
        change
            .get(name)
            .map_or_else(|| "(not set)".to_owned(), show)
    };
    format!("  {path}: {} -> {}\n", side("old"), side("new"))
}

/// `value`, compact, as text.
pub(crate) fn show(value: &Value) -> String {
    value.to_string()
}

/// `value` as YAML in block style, for people: a key plain when it can
/// be, else quoted, and every other scalar as JSON, which YAML reads.
pub(crate) fn to_yaml(value: &Value) -> String {
    let mut out = String::new();
    match value {
        Value::Object(map) if !map.is_empty() => yaml_map(map, 0, &mut out),
        Value::Array(items) if !items.is_empty() => yaml_seq(items, 0, &mut out),
        other => {
            out.push_str(&yaml_scalar(other));
            out.push('\n');
        }
    }
    out
}

/// `key` as a YAML key: plain when it reads back as the same string.
fn yaml_key(key: &str) -> String {
    let plain = key.starts_with(|c: char| c.is_ascii_alphanumeric())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/'))
        && key.parse::<f64>().is_err()
        && !matches!(
            key.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "y" | "n" | "inf" | "nan"
        );
    if plain {
        key.to_owned()
    } else {
        Value::String(key.to_owned()).to_string()
    }
}

/// A value on one line: JSON, which YAML reads.
fn yaml_scalar(value: &Value) -> String {
    match value {
        Value::Object(map) if map.is_empty() => "{}".to_owned(),
        Value::Array(items) if items.is_empty() => "[]".to_owned(),
        other => other.to_string(),
    }
}

fn yaml_map(map: &Map<String, Value>, indent: usize, out: &mut String) {
    for (key, value) in map {
        out.push_str(&" ".repeat(indent));
        out.push_str(&yaml_key(key));
        out.push(':');
        yaml_child(value, indent, out);
    }
}

/// What follows `key:` at `indent`.
fn yaml_child(value: &Value, indent: usize, out: &mut String) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            out.push('\n');
            yaml_map(map, indent + 2, out);
        }
        Value::Array(items) if !items.is_empty() => {
            out.push('\n');
            yaml_seq(items, indent + 2, out);
        }
        other => {
            out.push(' ');
            out.push_str(&yaml_scalar(other));
            out.push('\n');
        }
    }
}

fn yaml_seq(items: &[Value], indent: usize, out: &mut String) {
    for item in items {
        out.push_str(&" ".repeat(indent));
        out.push('-');
        match item {
            Value::Object(map) if !map.is_empty() => {
                for (number, (key, value)) in map.iter().enumerate() {
                    if number == 0 {
                        out.push(' ');
                    } else {
                        out.push_str(&" ".repeat(indent + 2));
                    }
                    out.push_str(&yaml_key(key));
                    out.push(':');
                    yaml_child(value, indent + 2, out);
                }
            }
            Value::Array(inner) if !inner.is_empty() => {
                out.push('\n');
                yaml_seq(inner, indent + 2, out);
            }
            other => {
                out.push(' ');
                out.push_str(&yaml_scalar(other));
                out.push('\n');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parts(path: &str) -> Vec<String> {
        split_path(path)
    }

    // Not upstream's: paths split on slashes when they have one, else dots.
    #[test]
    fn splits_paths() {
        assert_eq!(parts("routing.strategy"), ["routing", "strategy"]);
        assert_eq!(
            parts("oauth/model-alias/gpt-5.1"),
            ["oauth", "model-alias", "gpt-5.1"]
        );
        assert_eq!(parts("/server/port/"), ["server", "port"]);
        assert!(parts(" ").is_empty());
    }

    // Not upstream's: an unknown setting names the nearest known one; one
    // inside a list says lists are set whole.
    #[test]
    fn checks_paths() {
        assert!(check_path(&parts("routing.strategy")).is_ok());
        assert!(check_path(&parts("routing")).is_ok());
        let failure = check_path(&parts("routing.stratgy")).unwrap_err();
        assert_eq!((failure.error, failure.code), ("unknown_path", 2));
        assert_eq!(failure.hint.unwrap(), "did you mean routing.strategy?");
        let failure = check_path(&parts("server.prot")).unwrap_err();
        assert_eq!(failure.hint.unwrap(), "did you mean server.port?");
        let failure = check_path(&parts("routng.strategy")).unwrap_err();
        assert_eq!(failure.hint.unwrap(), "did you mean routing.strategy?");
        let failure = check_path(&parts("api-keys.codex.0")).unwrap_err();
        assert!(failure.message.contains("lists are set whole"));
        assert_eq!(check_path(&[]).unwrap_err().code, 2);
    }

    // Not upstream's: mappings are compared key by key, and a mapping on
    // one side only is expanded.
    #[test]
    fn diffs_settings() {
        let before = json!({"server": {"port": 1, "host": "a"}, "list": [1, 2]});
        let after =
            json!({"server": {"port": 2, "host": "a"}, "list": [1], "routing": {"strategy": "x"}});
        let changes: Vec<(String, Option<Value>, Option<Value>)> = diff(&before, &after)
            .into_iter()
            .map(|change| (change.path, change.old, change.new))
            .collect();
        assert_eq!(
            changes,
            vec![
                ("server.port".to_owned(), Some(json!(1)), Some(json!(2))),
                ("list".to_owned(), Some(json!([1, 2])), Some(json!([1]))),
                ("routing.strategy".to_owned(), None, Some(json!("x"))),
            ]
        );
        assert!(diff(&before, &before).is_empty());
        assert_eq!(
            change_line(&json!({"path": "a.b", "new": 1})),
            "  a.b: (not set) -> 1\n"
        );
    }

    // Not upstream's: YAML and JSON values both read.
    #[test]
    fn parses_values() {
        assert_eq!(parse_value("fill-first").unwrap(), json!("fill-first"));
        assert_eq!(parse_value("8317").unwrap(), json!(8317));
        assert_eq!(parse_value("[a, b]").unwrap(), json!(["a", "b"]));
        assert_eq!(parse_value(r#"{"a": true}"#).unwrap(), json!({"a": true}));
        assert_eq!(parse_value("").unwrap(), Value::Null);
        assert_eq!(parse_value("[").unwrap_err().code, 2);
    }

    // Not upstream's: the YAML `config show` prints reads back as the same
    // settings.
    #[test]
    fn writes_yaml() {
        let tree = json!({
            "server": {"host": "127.0.0.1", "port": 8317},
            "access": {"api-keys": ["...abcd", "x: y"]},
            "list": [{"name": "a", "models": [{"name": "m", "alias": "n"}]}, "plain"],
            "headers": {"X-Key": "v", "true": 1, "8": "eight", "a b": null},
            "empty": {},
            "none": [],
            "nested": [[1, 2], []],
            "text": "line\nbreak \"quoted\"",
        });
        let text = to_yaml(&tree);
        assert!(
            text.contains("server:\n  host: \"127.0.0.1\"\n  port: 8317\n"),
            "{text}"
        );
        assert!(text.contains("\"true\": 1"), "{text}");
        assert_eq!(parse_value(&text).unwrap(), tree);
        assert_eq!(to_yaml(&json!({})), "{}\n");
        assert_eq!(to_yaml(&json!("x")), "\"x\"\n");
    }
}
