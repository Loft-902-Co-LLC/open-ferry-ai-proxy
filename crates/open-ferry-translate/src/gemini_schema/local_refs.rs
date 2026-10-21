// Ported from CLIProxyAPI internal/util/gemini_schema.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Local reference expansion for cleaners that discard definitions. Each schema
//! has upstream's 256 KiB expansion allowance. Reference chains additionally stop
//! at 64 links to bound the Rust stack; their remaining references become hints.

use super::{NAME_MAP_KEYWORDS, is_literal_value_keyword, merge_hint, ref_name};
use serde_json::{Map, Value};
use std::collections::HashSet;

const COPY_BYTES: usize = 256 * 1024;
const MAX_REFERENCE_DEPTH: usize = 64;
const MAX_DESCRIPTION_BYTES: usize = 512;

pub(crate) fn inline_for_clean(schema: &Value) -> Value {
    // The projection cleans the same single-key carrier as upstream. References
    // within it are relative to the schema, rather than the carrier.
    if let Some(map) = schema.as_object()
        && map.len() == 1
        && let Some(inner @ Value::Object(_)) = map.get("schema")
    {
        return Value::Object(Map::from_iter([("schema".into(), inline_for_clean(inner))]));
    }
    Resolver {
        root: schema,
        remaining: COPY_BYTES,
        active: HashSet::new(),
    }
    .root()
}

struct Resolver<'a> {
    root: &'a Value,
    remaining: usize,
    active: HashSet<String>,
}

impl Resolver<'_> {
    fn root(&mut self) -> Value {
        let Some(map) = self.root.as_object() else {
            return self.resolve(self.root, 0);
        };
        // Resolve body siblings first, so unused definitions never spend budget
        // ahead of the schema the caller actually sends.
        let mut out = self.entries(map, 0);
        if let Some(reference) = map.get("$ref").and_then(Value::as_str)
            && let Some(target) = local_ref(self.root, reference)
        {
            out.shift_remove("$ref");
            let cost = expansion_size(target, false);
            if let Some(remaining) = self.remaining.checked_sub(cost) {
                self.remaining = remaining;
                self.active.insert(reference.to_owned());
                let resolved = self.resolve(target, 1);
                self.active.remove(reference);
                if let Value::Object(mut target) = resolved {
                    target.extend(out);
                    return Value::Object(target);
                }
            }
            return Value::Object(self.fallback(&out, target, reference, 0));
        }
        Value::Object(out)
    }

    fn resolve(&mut self, value: &Value, depth: usize) -> Value {
        match value {
            Value::Array(values) => {
                Value::Array(values.iter().map(|v| self.resolve(v, depth)).collect())
            }
            Value::Object(map) => {
                if let Some(reference) = map.get("$ref").and_then(Value::as_str)
                    && let Some(target) = local_ref(self.root, reference)
                {
                    let cost = expansion_size(target, false);
                    if depth >= MAX_REFERENCE_DEPTH
                        || self.active.contains(reference)
                        || cost > self.remaining
                    {
                        return Value::Object(self.fallback(map, target, reference, depth));
                    }
                    self.remaining -= cost;
                    self.active.insert(reference.to_owned());
                    let resolved = self.resolve(target, depth + 1);
                    self.active.remove(reference);
                    if let Value::Object(mut out) = resolved {
                        let mut siblings = self.entries(map, depth);
                        siblings.shift_remove("$ref");
                        out.extend(siblings);
                        return Value::Object(out);
                    }
                }
                Value::Object(self.entries(map, depth))
            }
            _ => value.clone(),
        }
    }

    fn entries(&mut self, map: &Map<String, Value>, depth: usize) -> Map<String, Value> {
        // Budget allocation follows Go's sorted traversal, regardless of the
        // client's field order, which we retain in the output.
        let mut keys: Vec<_> = map.keys().collect();
        keys.sort();
        let mut out = Map::new();
        for key in keys {
            if key != "$defs" && key != "definitions" {
                out.insert(key.clone(), self.entry(key, &map[key], depth));
            }
        }
        map.keys()
            .filter_map(|key| out.shift_remove(key).map(|value| (key.clone(), value)))
            .collect()
    }

    fn entry(&mut self, key: &str, value: &Value, depth: usize) -> Value {
        if is_literal_value_keyword(key) {
            return value.clone();
        }
        if NAME_MAP_KEYWORDS.contains(&key)
            && let Value::Object(map) = value
        {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for name in keys {
                out.insert(name.clone(), self.resolve(&map[name], depth));
            }
            return Value::Object(
                map.keys()
                    .filter_map(|key| out.shift_remove(key).map(|value| (key.clone(), value)))
                    .collect(),
            );
        }
        self.resolve(value, depth)
    }

    fn fallback(
        &mut self,
        map: &Map<String, Value>,
        target: &Value,
        reference: &str,
        depth: usize,
    ) -> Map<String, Value> {
        let mut out = Map::new();
        if let Value::Object(target) = target {
            for key in ["type", "nullable", "description"] {
                if let Some(value) = target.get(key) {
                    if key == "description"
                        && let Some(description) = value.as_str()
                    {
                        if description.len() > MAX_DESCRIPTION_BYTES {
                            continue;
                        }
                        if !map.contains_key("description") {
                            let cost = encoded_string_size(description);
                            let Some(remaining) = self.remaining.checked_sub(cost) else {
                                continue;
                            };
                            self.remaining = remaining;
                        }
                    }
                    out.insert(key.into(), value.clone());
                }
            }
        }
        // Sibling schemas still resolve even when this reference cannot expand.
        // Active references stay marked while their fallback siblings are read.
        let inserted = self.active.insert(reference.to_owned());
        let mut siblings = self.entries(map, depth);
        siblings.shift_remove("$ref");
        out.extend(siblings);
        if inserted {
            self.active.remove(reference);
        }
        let hint = format!("See: {}", ref_name(reference));
        let description = out.get("description").and_then(Value::as_str).unwrap_or("");
        out.insert(
            "description".into(),
            Value::from(merge_hint(description, &hint)),
        );
        out
    }
}

fn local_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    reference
        .strip_prefix("#/")?
        .split('/')
        .try_fold(root, |value, key| {
            let key = key.replace("~1", "/").replace("~0", "~");
            match value {
                Value::Object(map) => map.get(&key),
                Value::Array(values) => values.get(usize::try_from(key.parse::<i64>().ok()?).ok()?),
                _ => None,
            }
        })
}

// Conservative encoding/json sizes, including HTML and U+2028/U+2029 escaping.
// Like upstream, containers and booleans may overestimate by a byte.
fn encoded_string_size(value: &str) -> usize {
    2 + value
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' | '"' | '\\' => 2,
            '\0'..='\u{1f}' | '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => 6,
            _ => c.len_utf8(),
        })
        .sum::<usize>()
}

fn expansion_size(value: &Value, name_map: bool) -> usize {
    let size = match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(n) => n.as_str().len(),
        Value::String(s) => encoded_string_size(s),
        Value::Array(values) => values.iter().fold(2usize, |size, value| {
            size.saturating_add(expansion_size(value, false) + 1)
                .min(COPY_BYTES + 1)
        }),
        Value::Object(map) => map.iter().fold(2usize, |size, (key, value)| {
            if !name_map && matches!(key.as_str(), "$defs" | "definitions") {
                return size;
            }
            let child = if !name_map && is_literal_value_keyword(key) {
                literal_size(value)
            } else {
                expansion_size(
                    value,
                    !name_map && NAME_MAP_KEYWORDS.contains(&key.as_str()),
                )
            };
            size.saturating_add(encoded_string_size(key) + child + 2)
                .min(COPY_BYTES + 1)
        }),
    };
    size.min(COPY_BYTES + 1)
}

fn literal_size(value: &Value) -> usize {
    match value {
        Value::Array(values) => values.iter().fold(2usize, |n, v| {
            n.saturating_add(literal_size(v) + 1).min(COPY_BYTES + 1)
        }),
        Value::Object(map) => map.iter().fold(2usize, |n, (key, value)| {
            n.saturating_add(encoded_string_size(key) + literal_size(value) + 2)
                .min(COPY_BYTES + 1)
        }),
        _ => expansion_size(value, false),
    }
}

#[cfg(test)]
mod tests;
