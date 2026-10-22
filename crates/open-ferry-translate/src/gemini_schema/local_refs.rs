// Ported from CLIProxyAPI internal/util/gemini_schema.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Local reference expansion for cleaners that discard definitions. Each schema
//! has upstream's 256 KiB expansion allowance. Reference chains additionally stop
//! at 64 links. Containers and reference hops share a 128-level output bound;
//! the argument projection uses 64 levels. Over-depth targets become hints.

use super::{
    CopyCost, NAME_MAP_KEYWORDS, SchemaCopyBudget, is_literal_value_keyword, merge_hint, ref_name,
};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

const COPY_BYTES: usize = 256 * 1024;
const MAX_REFERENCE_DEPTH: usize = 64;
pub(crate) const MAX_OUTPUT_DEPTH: usize = 128;
const MAX_DESCRIPTION_BYTES: usize = 512;

pub(crate) fn inline_for_clean(schema: &Value, budget: &mut SchemaCopyBudget) -> Value {
    inline_with_depth(schema, MAX_OUTPUT_DEPTH, budget).0
}

pub(crate) fn inline_for_projection(schema: &Value) -> Option<Value> {
    // The projection has its own total allowance, including the source clone.
    // Preflight before the shared inliner allocates, not after expansion.
    let nodes = node_count(schema, 100_000, false);
    if nodes > 100_000 {
        return None;
    }
    let bytes = super::copy_bytes(schema, super::SCHEMA_COPY_BYTES_PER_REQUEST)?;
    let mut budget = SchemaCopyBudget {
        remaining: super::SCHEMA_COPY_BYTES_PER_REQUEST - bytes,
        nodes: 100_000 - nodes,
        path_work: usize::MAX,
    };
    let (value, truncated) = inline_with_depth(schema, 64, &mut budget);
    (!truncated).then_some(value)
}

fn inline_with_depth(
    schema: &Value,
    max_depth: usize,
    budget: &mut SchemaCopyBudget,
) -> (Value, bool) {
    if structural_depth(schema) > max_depth {
        return (Value::Object(depth_hint(schema)), true);
    }
    // Upstream checks raw JSON for the exact token. We receive parsed Values:
    // escaped spellings of $ref are indistinguishable here and also inline.
    // Without a parsed token, later passes still see definition containers.
    if !contains_ref_token(schema) {
        return (schema.clone(), false);
    }
    // The projection cleans the same single-key carrier as upstream. References
    // within it are relative to the schema, rather than the carrier.
    if let Some(map) = schema.as_object()
        && map.len() == 1
        && let Some(inner @ Value::Object(_)) = map.get("schema")
    {
        let (value, truncated) = inline_with_depth(inner, max_depth - 1, budget);
        return (
            Value::Object(Map::from_iter([("schema".into(), value)])),
            truncated,
        );
    }
    let mut resolver = Resolver {
        root: schema,
        remaining: COPY_BYTES,
        max_depth,
        depth_exhausted: false,
        budget,
        budget_exhausted: false,
        active: HashSet::new(),
        targets: HashMap::new(),
        lookups: HashMap::new(),
        #[cfg(test)]
        target_size_scans: 0,
    };
    let value = resolver.root();
    (value, resolver.depth_exhausted || resolver.budget_exhausted)
}

// Counts only material copied by cleaning, or the complete source preflight
// when `for_clean` is false. This is iterative and stops before unbounded work.
fn node_count(value: &Value, limit: usize, for_clean: bool) -> usize {
    let mut count = 0usize;
    let mut stack = vec![(value, false, false)];
    while let Some((value, name_map, literal)) = stack.pop() {
        count += 1;
        if count > limit {
            return count;
        }
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if for_clean
                        && !literal
                        && !name_map
                        && matches!(key.as_str(), "$defs" | "definitions")
                    {
                        continue;
                    }
                    stack.push((
                        value,
                        !literal && !name_map && NAME_MAP_KEYWORDS.contains(&key.as_str()),
                        literal || (!name_map && is_literal_value_keyword(key)),
                    ));
                }
            }
            Value::Array(values) => {
                stack.extend(values.iter().map(|value| (value, false, literal)))
            }
            _ => {}
        }
    }
    count
}

// Iterative preflight also covers literal subtrees, which are cloned verbatim.
// Definition containers discarded by cleaning do not contribute to output depth.
fn structural_depth(value: &Value) -> usize {
    container_depth(value, true)
}

fn container_depth(value: &Value, discard_definitions: bool) -> usize {
    let mut maximum = 0;
    let mut stack = vec![(value, 0, false, false)];
    while let Some((value, depth, name_map, literal)) = stack.pop() {
        match value {
            Value::Object(map) => {
                maximum = maximum.max(depth + 1);
                if maximum > MAX_OUTPUT_DEPTH {
                    return maximum;
                }
                for (key, value) in map {
                    if discard_definitions
                        && !literal
                        && !name_map
                        && matches!(key.as_str(), "$defs" | "definitions")
                    {
                        continue;
                    }
                    stack.push((
                        value,
                        depth + 1,
                        !literal && !name_map && NAME_MAP_KEYWORDS.contains(&key.as_str()),
                        literal || (!name_map && is_literal_value_keyword(key)),
                    ));
                }
            }
            Value::Array(values) => {
                maximum = maximum.max(depth + 1);
                if maximum > MAX_OUTPUT_DEPTH {
                    return maximum;
                }
                stack.extend(
                    values
                        .iter()
                        .map(|value| (value, depth + 1, false, literal)),
                );
            }
            _ => {}
        }
    }
    maximum
}

pub(super) fn fits_at_depth(value: &Value, ancestors: usize) -> bool {
    // Path writes may carry literal data too, so count even definition-shaped
    // fields here; the inliner's target preflight can discard schema definitions.
    ancestors <= MAX_OUTPUT_DEPTH
        && ancestors.saturating_add(container_depth(value, false)) <= MAX_OUTPUT_DEPTH
}

fn depth_hint(value: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for key in ["type", "nullable"] {
        if let Some(value) = value
            .get(key)
            .filter(|value| !value.is_object() && !value.is_array())
        {
            out.insert(key.into(), value.clone());
        }
    }
    out.insert("description".into(), Value::from("Schema nesting limit"));
    out
}

fn contains_ref_token(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key == "$ref" || contains_ref_token(value)),
        Value::Array(values) => values.iter().any(contains_ref_token),
        Value::String(value) => value == "$ref",
        _ => false,
    }
}

struct Resolver<'a, 'b> {
    root: &'a Value,
    remaining: usize,
    max_depth: usize,
    depth_exhausted: bool,
    budget: &'b mut SchemaCopyBudget,
    budget_exhausted: bool,
    active: HashSet<String>,
    targets: HashMap<*const Value, (usize, usize, usize)>,
    lookups: HashMap<String, Option<&'a Value>>,
    #[cfg(test)]
    target_size_scans: usize,
}

enum FallbackSiblings<'a> {
    Unresolved(&'a Map<String, Value>),
    Resolved(Map<String, Value>),
}

impl<'a> Resolver<'a, '_> {
    fn spend_inline(&mut self, bytes: usize, nodes: usize) -> bool {
        let fits = self
            .budget
            .spend(CopyCost {
                bytes,
                nodes,
                path_work: 0,
            })
            .is_some();
        self.budget_exhausted |= !fits;
        fits
    }
    // Targets and their stored size are independent of the active reference
    // stack and remaining budget. Expanded results are not: caching those would
    // change cycle fallback and per-reference charging.
    fn target(&mut self, reference: &str) -> Option<(&'a Value, usize, usize, usize)> {
        let target = *self
            .lookups
            .entry(reference.to_owned())
            .or_insert_with(|| local_ref(self.root, reference));
        let target = target?;
        let identity = std::ptr::from_ref(target);
        if let Some(&(bytes, depth, nodes)) = self.targets.get(&identity) {
            return Some((target, bytes, depth, nodes));
        }
        #[cfg(test)]
        {
            self.target_size_scans += 1;
        }
        let bytes = expansion_size(target, false);
        // An unaffordable target is never copied: do not scan the rest of it
        // again for its depth or node count after the size walk stopped.
        let (depth, nodes) = if bytes <= COPY_BYTES {
            (structural_depth(target), node_count(target, 100_000, true))
        } else {
            (0, 0)
        };
        self.targets.insert(identity, (bytes, depth, nodes));
        Some((target, bytes, depth, nodes))
    }

    fn root(&mut self) -> Value {
        let Some(map) = self.root.as_object() else {
            return self.resolve(self.root, 0);
        };
        // Resolve body siblings first, so unused definitions never spend budget
        // ahead of the schema the caller actually sends.
        let mut out = self.entries(map, 1);
        if let Some(reference) = map.get("$ref").and_then(Value::as_str)
            && let Some((target, cost, target_depth, nodes)) = self.target(reference)
        {
            out.shift_remove("$ref");
            self.depth_exhausted |= 1 + target_depth > self.max_depth;
            if let Some(remaining) = self
                .remaining
                .checked_sub(cost)
                .filter(|_| target_depth < self.max_depth)
                .filter(|_| self.spend_inline(cost, nodes))
            {
                self.remaining = remaining;
                self.active.insert(reference.to_owned());
                let resolved = self.resolve(target, 1);
                self.active.remove(reference);
                if let Value::Object(mut target) = resolved {
                    target.extend(out);
                    return Value::Object(target);
                }
            }
            return Value::Object(self.fallback(
                FallbackSiblings::Resolved(out),
                target,
                reference,
                0,
            ));
        }
        Value::Object(out)
    }

    fn resolve(&mut self, value: &Value, depth: usize) -> Value {
        if depth >= self.max_depth && (value.is_object() || value.is_array()) {
            self.depth_exhausted = true;
            return Value::Null;
        }
        match value {
            Value::Array(values) => {
                Value::Array(values.iter().map(|v| self.resolve(v, depth + 1)).collect())
            }
            Value::Object(map) => {
                if let Some(reference) = map.get("$ref").and_then(Value::as_str)
                    && let Some((target, cost, target_depth, nodes)) = self.target(reference)
                {
                    self.depth_exhausted |= depth + 1 + target_depth > self.max_depth;
                    if depth + 1 + target_depth > self.max_depth
                        || self.active.len() >= MAX_REFERENCE_DEPTH
                        || self.active.contains(reference)
                        || cost > self.remaining
                        || !self.spend_inline(cost, nodes)
                    {
                        return Value::Object(self.fallback(
                            FallbackSiblings::Unresolved(map),
                            target,
                            reference,
                            depth,
                        ));
                    }
                    self.remaining -= cost;
                    self.active.insert(reference.to_owned());
                    let resolved = self.resolve(target, depth + 1);
                    self.active.remove(reference);
                    if let Value::Object(mut out) = resolved {
                        let mut siblings = self.entries(map, depth + 1);
                        siblings.shift_remove("$ref");
                        out.extend(siblings);
                        return Value::Object(out);
                    }
                }
                Value::Object(self.entries(map, depth + 1))
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
                out.insert(name.clone(), self.resolve(&map[name], depth + 1));
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
        siblings: FallbackSiblings<'_>,
        target: &Value,
        reference: &str,
        depth: usize,
    ) -> Map<String, Value> {
        let map = match &siblings {
            FallbackSiblings::Unresolved(map) => *map,
            FallbackSiblings::Resolved(map) => map,
        };
        let mut out = Map::new();
        if let Value::Object(target) = target {
            for key in ["type", "nullable", "description"] {
                if let Some(value) = target.get(key) {
                    if map.contains_key(key) {
                        continue;
                    }
                    if depth + 1 + structural_depth(value) > self.max_depth {
                        self.depth_exhausted = true;
                        continue;
                    }
                    if key == "description"
                        && let Some(description) = value.as_str()
                    {
                        if description.len() > MAX_DESCRIPTION_BYTES {
                            continue;
                        }
                        if encoded_string_size(description) > self.remaining {
                            continue;
                        }
                    }
                    let Some(bytes) = super::copy_bytes(value, self.budget.remaining) else {
                        self.budget_exhausted = true;
                        continue;
                    };
                    let nodes = node_count(value, self.budget.nodes, false);
                    if !self.spend_inline(bytes, nodes) {
                        continue;
                    }
                    if key == "description"
                        && let Some(description) = value.as_str()
                    {
                        self.remaining -= encoded_string_size(description);
                    }
                    out.insert(key.into(), value.clone());
                }
            }
        }
        let mut siblings = match siblings {
            // Root siblings have already been resolved and charged. Transfer
            // their allocations rather than resolving and cloning them again.
            FallbackSiblings::Resolved(siblings) => siblings,
            FallbackSiblings::Unresolved(map) => {
                // Keep active references marked while fallback siblings resolve.
                let inserted = self.active.insert(reference.to_owned());
                let siblings = self.entries(map, depth + 1);
                if inserted {
                    self.active.remove(reference);
                }
                siblings
            }
        };
        siblings.shift_remove("$ref");
        out.extend(siblings);
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
    let mut size = 2usize;
    for c in value.chars() {
        size += match c {
            '\n' | '\r' | '\t' | '"' | '\\' => 2,
            '\0'..='\u{1f}' | '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => 6,
            _ => c.len_utf8(),
        };
        if size > COPY_BYTES {
            return COPY_BYTES + 1;
        }
    }
    size
}

#[cfg(test)]
thread_local! { static SIZE_NODES_VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn expansion_size(value: &Value, name_map: bool) -> usize {
    #[cfg(test)]
    SIZE_NODES_VISITED.with(|n| n.set(n.get() + 1));
    let size = match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(n) => n.as_str().len(),
        Value::String(s) => encoded_string_size(s),
        Value::Array(values) => {
            let mut size = 2usize;
            for value in values {
                size = size.saturating_add(expansion_size(value, false) + 1);
                if size > COPY_BYTES {
                    return COPY_BYTES + 1;
                }
            }
            size
        }
        Value::Object(map) => {
            let mut size = 2usize;
            for (key, value) in map {
                if !name_map && matches!(key.as_str(), "$defs" | "definitions") {
                    continue;
                }
                let child = if !name_map && is_literal_value_keyword(key) {
                    literal_size(value)
                } else {
                    expansion_size(
                        value,
                        !name_map && NAME_MAP_KEYWORDS.contains(&key.as_str()),
                    )
                };
                size = size.saturating_add(encoded_string_size(key) + child + 2);
                if size > COPY_BYTES {
                    return COPY_BYTES + 1;
                }
            }
            size
        }
    };
    size.min(COPY_BYTES + 1)
}

fn literal_size(value: &Value) -> usize {
    #[cfg(test)]
    SIZE_NODES_VISITED.with(|n| n.set(n.get() + 1));
    match value {
        Value::Array(values) => {
            let mut size = 2usize;
            for value in values {
                size = size.saturating_add(literal_size(value) + 1);
                if size > COPY_BYTES {
                    return COPY_BYTES + 1;
                }
            }
            size
        }
        Value::Object(map) => {
            let mut size = 2usize;
            for (key, value) in map {
                size = size.saturating_add(encoded_string_size(key) + literal_size(value) + 2);
                if size > COPY_BYTES {
                    return COPY_BYTES + 1;
                }
            }
            size
        }
        _ => expansion_size(value, false),
    }
}

#[cfg(test)]
pub(crate) mod tests;
