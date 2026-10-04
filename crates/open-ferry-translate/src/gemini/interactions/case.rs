// Ported from CLIProxyAPI internal/translator/gemini/interactions/interactions_gemini_common.go
// (convertSnakeCaseKeysToCamelCase, copySnakeCaseValueToCamelCase, joinJSONPath,
// toCamelCase, convertCamelCaseKeysToSnakeCase, copyCamelCaseValueToSnakeCase,
// toSnakeCase) (v8.0.10, MIT), and the parts of sjson v1.2.5's `SetRawBytes`
// they rely on.
// https://github.com/router-for-me/CLIProxyAPI

//! snake_case and camelCase keys for a generation config.
//!
//! Upstream rebuilds the config leaf by leaf: each scalar is written with sjson
//! at a dotted path made of the converted keys, with `-1` for each array item.
//! So the output is what sjson makes of those paths, not a plain renaming. Empty
//! objects and arrays vanish, nested arrays flatten, a key holding a dot nests,
//! digit keys can make arrays, and keys that convert to the same name merge.
//! [`set_raw`] does what sjson does with such a path.
//!
//! Deviations from upstream:
//! - A key holding `|`, `#`, `@`, `*` or `?` is left out. sjson reads it as a
//!   gjson query, which may overwrite a value already written.
//! - A digit key that would pad an array with more than 65,535 nulls, or index
//!   into an array it doesn't reach, is left out. Upstream runs out of memory or
//!   panics.

use serde_json::{Map, Value};

use crate::go;

/// The most nulls [`set_raw`] pads an array with.
const MAX_PADDING: usize = 65_535;

/// `convertSnakeCaseKeysToCamelCase`: `value` with each object key in
/// camelCase. A scalar gives `{}`.
pub(super) fn snake_to_camel(value: &Value) -> Value {
    convert(value, &to_camel_case)
}

/// `convertCamelCaseKeysToSnakeCase`: `value` with each object key in
/// snake_case. A scalar gives `{}`.
pub(super) fn camel_to_snake(value: &Value) -> Value {
    convert(value, &to_snake_case)
}

/// Rebuilds `value` with each key passed through `rename`, as upstream's
/// `copy...ValueTo...` does.
fn convert(value: &Value, rename: &dyn Fn(&str) -> String) -> Value {
    let mut out = Value::Object(Map::new());
    copy(&mut out, "", value, rename);
    out
}

/// Writes each scalar under `node` into `out`, at `path` plus the renamed keys.
fn copy(out: &mut Value, path: &str, node: &Value, rename: &dyn Fn(&str) -> String) {
    match node {
        Value::Object(fields) => {
            for (key, value) in fields {
                let key = rename(key);
                let child = if path.is_empty() {
                    key
                } else {
                    format!("{path}.{key}")
                };
                copy(out, &child, value, rename);
            }
        }
        Value::Array(items) => {
            let child = format!("{path}.-1");
            for value in items {
                copy(out, &child, value, rename);
            }
        }
        leaf => {
            set_raw(out, path, leaf.clone());
        }
    }
}

/// `toCamelCase`: drops each `_` and upper-cases the byte after it.
///
/// Go upper-cases the first byte on its own, so a character of several bytes
/// there becomes as many U+FFFD characters.
fn to_camel_case(s: &str) -> String {
    let mut parts = s.split('_');
    let mut out = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        let mut chars = part.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        if first.is_ascii() {
            out.push(first.to_ascii_uppercase());
        } else {
            out.extend(std::iter::repeat_n('\u{fffd}', first.len_utf8()));
        }
        out.push_str(chars.as_str());
    }
    out
}

/// `toSnakeCase`: a `_` before each ASCII capital but a leading one, then the
/// whole key in lower case.
fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (index, c) in s.char_indices() {
        if index > 0 && c.is_ascii_uppercase() {
            out.push('_');
        }
        out.push(c);
    }
    go::to_lower(&out)
}

/// One key of an sjson path.
struct Segment {
    /// The key, with its escapes removed.
    key: String,
    /// Whether it began with `:`, which makes digits a key, not an index.
    force: bool,
}

impl Segment {
    /// The array index sjson reads in this key: its digits, or 0 for an empty
    /// key. `None` if it isn't digits or is forced to be a key. `Some(None)` if
    /// it is digits but too large to use.
    fn index(&self) -> Option<Option<usize>> {
        if self.force || !self.key.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(self.key.bytes().try_fold(0usize, |n, digit| {
            n.checked_mul(10)?.checked_add(usize::from(digit - b'0'))
        }))
    }

    /// Whether this key appends to an array: an unforced `-1`.
    fn appends(&self) -> bool {
        !self.force && self.key == "-1"
    }
}

/// sjson's `parsePath`, applied to each key: `None` for an empty path or one
/// sjson treats as a gjson query.
fn parse_path(path: &str) -> Option<Vec<Segment>> {
    if path.is_empty() {
        return None;
    }
    let mut segments = Vec::new();
    let mut rest = path;
    loop {
        let (force, body) = match rest.strip_prefix(':') {
            Some(body) => (true, body),
            None => (false, rest),
        };
        let mut key = String::new();
        let mut chars = body.char_indices();
        let mut next = None;
        while let Some((index, c)) = chars.next() {
            match c {
                '.' => {
                    next = body.get(index + 1..);
                    break;
                }
                '|' | '#' | '@' | '*' | '?' => return None,
                '\\' => {
                    if let Some((_, escaped)) = chars.next() {
                        key.push(escaped);
                    }
                }
                c => key.push(c),
            }
        }
        segments.push(Segment { key, force });
        match next {
            Some(more) => rest = more,
            None => return Some(segments),
        }
    }
}

/// sjson `SetRawBytes(out, path, leaf)` for the paths upstream builds here.
/// Where sjson fails, nothing changes.
fn set_raw(out: &mut Value, path: &str, leaf: Value) {
    if let Some(segments) = parse_path(path) {
        set_segments(out, &segments, leaf);
    }
}

/// Sets `leaf` at `segments` under `target`, as sjson's `appendRawPaths` does.
fn set_segments(target: &mut Value, segments: &[Segment], leaf: Value) {
    let Some((first, rest)) = segments.split_first() else {
        return;
    };
    let index = first.index();
    let existing = match target {
        Value::Object(fields) => fields.get_mut(&first.key),
        // An empty key doesn't index an array, though it counts as 0 below.
        Value::Array(items) if !first.key.is_empty() => {
            index.flatten().and_then(|index| items.get_mut(index))
        }
        _ => None,
    };
    if let Some(existing) = existing {
        if rest.is_empty() {
            *existing = leaf;
        } else {
            set_segments(existing, rest, leaf);
        }
        return;
    }
    let Some(built) = build(rest, leaf) else {
        return;
    };
    match target {
        Value::Object(fields) => {
            fields.insert(first.key.clone(), built);
        }
        // sjson pads the array with nulls up to the index, then appends: an
        // empty key, which found nothing, appends.
        Value::Array(items) => match index {
            Some(Some(index)) if index.saturating_sub(items.len()) <= MAX_PADDING => {
                if index > items.len() {
                    items.resize(index, Value::Null);
                }
                items.push(built);
            }
            None if first.appends() => items.push(built),
            _ => {}
        },
        // sjson replaces a scalar with an empty array if the key is an index,
        // or else an empty object.
        scalar => {
            *scalar = match index {
                None => {
                    let mut fields = Map::new();
                    fields.insert(first.key.clone(), built);
                    Value::Object(fields)
                }
                Some(Some(padding)) if padding <= MAX_PADDING => {
                    let mut items = vec![Value::Null; padding];
                    items.push(built);
                    Value::Array(items)
                }
                Some(_) => return,
            };
        }
    }
}

/// sjson's `appendBuild`: the value holding `leaf` under `segments`, made
/// fresh. `None` where it would pad an array with too many nulls.
fn build(segments: &[Segment], leaf: Value) -> Option<Value> {
    let Some((first, rest)) = segments.split_first() else {
        return Some(leaf);
    };
    let built = build(rest, leaf)?;
    match first.index() {
        Some(Some(padding)) if padding <= MAX_PADDING => {
            let mut items = vec![Value::Null; padding];
            items.push(built);
            Some(Value::Array(items))
        }
        Some(_) => None,
        None if first.appends() => Some(Value::Array(vec![built])),
        None => {
            let mut fields = Map::new();
            fields.insert(first.key.clone(), built);
            Some(Value::Object(fields))
        }
    }
}

#[cfg(test)]
mod tests;
