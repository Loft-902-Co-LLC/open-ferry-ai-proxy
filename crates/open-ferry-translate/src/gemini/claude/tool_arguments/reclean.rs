// Ported from CLIProxyAPI internal/util/gemini_schema.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Structural projection of the wrapped re-clean used by the argument cleaner.
//!
//! Only the decision to declare and require a placeholder is exported. Description
//! hints carry no constraint used by that decision; their presence is retained
//! where they can change a property map. No Antigravity request cleaner is exposed.
//! The 64-level depth limit bounds recursion; a separate 100,000-node copy budget
//! bounds the projected schema. Either limit preserves arguments when the
//! projection cannot finish. Inlining first preflights the source and copies
//! against its own 8 MiB/100,000-node allowance. It also uses upstream's 256 KiB
//! allowance, cycle hints, and inner-schema reference root.
//!
//! Deliberate projection deviations from upstream's raw-path quirks:
//! - Scalar `required` beside `allOf` is retained here; upstream's array-only
//!   `getStrings` ignores it. The underscore decision can be false here, true in Go.
//! - Scalar `required` in `anyOf`/`oneOf` required-only branches likewise differs
//!   from upstream's array-only materialization (false here, true in Go).
//! - Required property names containing `|` or a backslash use literal lookup here;
//!   upstream's incompletely escaped gjson paths can discard their required entry.
//!   For a closed object this can preserve an underscore that upstream deletes.
//! - A property literally named `type` holding `["null"]`, with `required` naming
//!   `nullable`, can require underscore here while Go's type-array path walk does not.
//!
//! The scalar-required and literal-type shapes never come from our Gemini cleaner.

use super::{analyze, array, gjson_array, str_of};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

const NAME_MAPS: [&str; 6] = [
    "properties",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "$defs",
    "definitions",
];
const REPAIR_CONTAINERS: &str = "properties patternProperties additionalProperties items prefixItems $defs definitions dependentSchemas dependentRequired dependencies if then else not contains propertyNames unevaluatedProperties unevaluatedItems contentSchema additionalItems default const example examples discriminator xml externalDocs enumDescriptions enumTitles";
const REMOVED: &str = "minLength maxLength exclusiveMinimum exclusiveMaximum pattern minItems maxItems uniqueItems contains format default examples minimum maximum multipleOf $schema $defs definitions const $ref $id id additionalProperties $anchor $vocabulary $dynamicRef $dynamicAnchor propertyNames patternProperties dependentRequired dependentSchemas dependencies if then else $comment enumDescriptions enumTitles prefill deprecated encrypted discriminator readOnly writeOnly additionalItems unevaluatedProperties unevaluatedItems contentSchema contentMediaType contentEncoding not";

fn string_required(schema: &Value) -> Vec<String> {
    array(schema, "required")
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
fn promote(schema: &mut Value, names: Vec<String>) {
    if names.is_empty() {
        return;
    }
    let mut required = string_required(schema);
    required.extend(names);
    let mut seen = BTreeSet::new();
    required.retain(|name| !name.is_empty() && seen.insert(name.clone()));
    schema["required"] = json!(required);
}
fn non_object_type(value: Option<&Value>) -> bool {
    match value {
        Some(Value::String(kind)) => !kind.is_empty() && !kind.eq_ignore_ascii_case("object"),
        Some(Value::Array(kinds)) => {
            !kinds.is_empty()
                && !kinds
                    .iter()
                    .any(|v| v.as_str().is_some_and(|v| v.eq_ignore_ascii_case("object")))
        }
        _ => false,
    }
}
fn repair_properties(
    mut props: Map<String, Value>,
    depth: usize,
) -> Option<(Map<String, Value>, Vec<String>)> {
    let mut promoted = Vec::new();
    for (name, schema) in &mut props {
        if schema == &Value::Bool(true) {
            *schema = json!({});
        } else if let Some(map) = schema.as_object_mut() {
            if let Some(Value::Bool(required)) = map.get("required") {
                if *required {
                    promoted.push(name.clone());
                }
                map.shift_remove("required");
            }
            *schema = repair(std::mem::take(schema), depth + 1)?;
        }
    }
    promoted.sort();
    Some((props, promoted))
}
fn repair(mut node: Value, depth: usize) -> Option<Value> {
    if depth > 64 {
        return None;
    }
    if !node.is_object() {
        return Some(node);
    }
    if !non_object_type(node.get("type")) {
        let names: Vec<_> = node
            .as_object()?
            .iter()
            .filter(|(key, value)| {
                value.is_object()
                    && !key.starts_with("x-")
                    && !REPAIR_CONTAINERS
                        .split_whitespace()
                        .any(|k| k == key.as_str())
            })
            .map(|(key, _)| key.clone())
            .collect();
        let bare: Map<_, _> = names
            .into_iter()
            .map(|name| {
                let value = node
                    .as_object_mut()
                    .expect("object")
                    .shift_remove(&name)
                    .expect("collected name");
                Some((name, value))
            })
            .collect::<Option<_>>()?;
        if !bare.is_empty() {
            let (bare, promoted) = repair_properties(bare, depth)?;
            if let Some(props) = node.get_mut("properties").and_then(Value::as_object_mut) {
                props.extend(bare);
            } else {
                node["properties"] = Value::Object(bare);
                if node.get("type").is_none() {
                    node["type"] = json!("object");
                }
            }
            promote(&mut node, promoted);
        }
    }
    if let Some(Value::Object(props)) = node.get_mut("properties") {
        let (repaired, promoted) = repair_properties(std::mem::take(props), depth)?;
        *props = repaired;
        if node.get("type").is_none() {
            node["type"] = json!("object");
        }
        promote(&mut node, promoted);
    }
    let is_array = node
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("array"))
        || array(&node, "type")
            .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("array")));
    if !node.get("$ref").is_none_or(Value::is_null) {
        // Resolve the referenced type and items before synthesizing either.
    } else if is_array && node.get("items").is_none() {
        node["items"] = json!({"type":"string"});
    } else if node.get("items").is_some()
        && (node.get("type").is_none_or(Value::is_null) || node.get("type") == Some(&json!("")))
    {
        node["type"] = json!("array");
    }
    for key in [
        "items",
        "additionalProperties",
        "if",
        "then",
        "else",
        "not",
        "contains",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
        "additionalItems",
    ] {
        if let Some(value) = node.get_mut(key) {
            if value.is_object() {
                *value = repair(std::mem::take(value), depth + 1)?;
            } else if value == &Value::Bool(true) && key != "additionalProperties" {
                *value = json!({});
            } else if let Value::Array(values) = value
                && key == "items"
            {
                *value = repair_list(std::mem::take(values), depth)?;
            }
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(values)) = node.get_mut(key) {
            let repaired = repair_list(std::mem::take(values), depth)?;
            node[key] = repaired;
        }
    }
    for key in [
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(Value::Object(values)) = node.get_mut(key) {
            let values = std::mem::take(values);
            let out = if key == "patternProperties" {
                repair_properties(values, depth)?.0
            } else {
                values
                    .into_iter()
                    .map(|(key, value)| {
                        Some((
                            key,
                            if value == Value::Bool(true) {
                                json!({})
                            } else {
                                repair(value, depth + 1)?
                            },
                        ))
                    })
                    .collect::<Option<Map<_, _>>>()?
            };
            node[key] = Value::Object(out);
        }
    }
    Some(node)
}
fn repair_list(values: Vec<Value>, depth: usize) -> Option<Value> {
    Some(Value::Array(
        values
            .into_iter()
            .map(|value| {
                if value == Value::Bool(true) {
                    Some(json!({}))
                } else {
                    repair(value, depth + 1)
                }
            })
            .collect::<Option<Vec<_>>>()?,
    ))
}
// Cycle fallbacks copy siblings without resolving their references; count those
// copies too, rather than hiding an arbitrarily large subtree in Value::clone.
fn copy_bounded(value: &Value, depth: usize, remaining: &mut usize) -> Option<Value> {
    if depth > 64 {
        return None;
    }
    *remaining = remaining.checked_sub(1)?;
    Some(match value {
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|v| copy_bounded(v, depth + 1, remaining))
                .collect::<Option<_>>()?,
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(k, v)| Some((k.clone(), copy_bounded(v, depth + 1, remaining)?)))
                .collect::<Option<_>>()?,
        ),
        _ => value.clone(),
    })
}
// The name-map flag follows upstream's trailing keyword parity, protecting names
// such as properties.properties while visiting all the same JSON containers.
fn walk(value: &mut Value, name_map: bool, phase: fn(&mut Value, bool)) {
    if let Some(map) = value.as_object_mut() {
        for (key, child) in map {
            if name_map || !crate::gemini_schema::is_literal_value_keyword(key) {
                walk(child, NAME_MAPS.contains(&key.as_str()) && !name_map, phase);
            }
        }
    } else if let Some(values) = value.as_array_mut() {
        for child in values {
            walk(child, false, phase);
        }
    }
    phase(value, name_map);
}
fn reference_hint(value: &mut Value, name_map: bool) {
    if !name_map
        && let Some(map) = value.as_object_mut()
        && map.shift_remove("$ref").is_some()
    {
        map.insert("description".into(), json!("reference hint"));
    }
}
fn hint_presence(value: &mut Value, _: bool) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    let enumeration = map
        .get("enum")
        .cloned()
        .or_else(|| map.get("const").map(|v| json!([v])));
    if enumeration
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty() && a.len() <= 10)
    {
        map.insert("description".into(), json!("enum hint"));
    }
    map.shift_remove("enum");
}
fn merge_missing(destination: &mut Value, incoming: &Value) {
    if let (Some(destination), Some(incoming)) = (destination.as_object_mut(), incoming.as_object())
    {
        for (key, value) in incoming {
            if let Some(existing) = destination.get_mut(key) {
                merge_missing(existing, value);
            } else {
                destination.insert(key.clone(), value.clone());
            }
        }
    }
}
fn conditionals(node: &mut Value, name_map: bool) {
    if name_map || !node.is_object() {
        return;
    }
    for key in ["then", "else"] {
        if let Some(props) = node
            .get(key)
            .and_then(|v| v.get("properties"))
            .and_then(Value::as_object)
            .cloned()
        {
            for (key, value) in props {
                if node.get("properties").is_none() {
                    node["properties"] = json!({});
                }
                if let Some(props) = node.get_mut("properties").and_then(Value::as_object_mut) {
                    props.entry(key).or_insert(value);
                }
            }
        }
    }
}
fn all_of(node: &mut Value, _: bool) {
    let Some(branches) = node.get("allOf").and_then(Value::as_array).cloned() else {
        return;
    };
    for branch in branches {
        if let Some(map) = branch.as_object() {
            for (key, incoming) in map {
                if key == "required" {
                    if let Some(required) = incoming.as_array() {
                        let mut names: Vec<String> = gjson_array(node.get("required"))
                            .map(|v| str_of(Some(v)).into_owned())
                            .collect();
                        for name in required {
                            let name = str_of(Some(name)).into_owned();
                            let mut found = false;
                            for existing in &names {
                                if existing == &name {
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                names.push(name);
                            }
                        }
                        node["required"] = json!(names);
                    }
                } else if !["if", "then", "else", "allOf"].contains(&key.as_str()) {
                    if let Some(existing) = node.get_mut(key) {
                        merge_missing(existing, incoming);
                    } else {
                        node[key] = incoming.clone();
                    }
                }
            }
        }
    }
    node.as_object_mut().unwrap().shift_remove("allOf");
}
fn null_only(branch: &Value) -> bool {
    branch.get("properties").is_none()
        && branch.get("items").is_none()
        && (branch.get("type") == Some(&json!("null"))
            || branch
                .get("type")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty() && v.iter().all(|v| v == "null")))
}
fn unions(node: &mut Value, name_map: bool) {
    if name_map || !node.is_object() {
        return;
    }
    for key in ["oneOf", "anyOf"] {
        let Some(branches) = node
            .get(key)
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .cloned()
        else {
            continue;
        };
        if key == "oneOf" && node.get("anyOf").is_some_and(Value::is_array) {
            node.as_object_mut().unwrap().shift_remove(key);
            continue;
        }
        let mut non_null = Vec::new();
        let mut has_null = false;
        for branch in branches {
            if null_only(&branch) {
                has_null = true;
                continue;
            }
            let mut branch = if branch == Value::Bool(true) {
                json!({})
            } else {
                branch
            };
            if !branch.is_object() {
                continue;
            }
            if branch.get("required").is_some()
                && [
                    "type",
                    "properties",
                    "items",
                    "enum",
                    "anyOf",
                    "oneOf",
                    "allOf",
                ]
                .iter()
                .all(|k| branch.get(*k).is_none())
            {
                let names: Vec<_> = gjson_array(branch.get("required"))
                    .map(|v| str_of(Some(v)).into_owned())
                    .collect();
                let Some(props) = node
                    .get("properties")
                    .and_then(Value::as_object)
                    .filter(|p| !names.is_empty() && names.iter().all(|n| p.contains_key(n)))
                else {
                    continue;
                };
                let selected: Map<_, _> = names
                    .iter()
                    .map(|n| (n.clone(), props[n].clone()))
                    .collect();
                branch["type"] = json!("object");
                branch["properties"] = Value::Object(selected);
            }
            non_null.push(branch);
        }
        if non_null.len() >= 2 {
            node["anyOf"] = Value::Array(non_null);
            if key == "oneOf" {
                node.as_object_mut().unwrap().shift_remove("oneOf");
            }
            continue;
        }
        if node.get("properties").is_some_and(Value::is_object) {
            for branch in non_null {
                if let Some(props) = branch.get("properties").and_then(Value::as_object) {
                    for (name, value) in props {
                        let target = node["properties"].as_object_mut().unwrap();
                        if let Some(existing) = target.get_mut(name) {
                            merge_missing(existing, value);
                        } else {
                            target.insert(name.clone(), value.clone());
                        }
                    }
                }
            }
            if str_of(node.get("type")).is_empty() {
                node["type"] = json!("object");
            }
            node.as_object_mut().unwrap().shift_remove(key);
        } else if let Some(branch) = non_null.into_iter().next() {
            *node = branch;
        } else if has_null {
            *node = json!({"type":"null"});
        } else {
            node.as_object_mut().unwrap().shift_remove(key);
        }
    }
}
fn type_arrays(node: &mut Value, _: bool) {
    let Some(kinds) = node
        .get("type")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    else {
        return;
    };
    let non_null: Vec<_> = kinds
        .iter()
        .map(|v| str_of(Some(v)).into_owned())
        .filter(|s| !s.is_empty() && s != "null")
        .collect();
    let kind = if node.get("items").is_some() && non_null.iter().any(|s| s == "array") {
        "array"
    } else if node.get("properties").is_some() && non_null.iter().any(|s| s == "object") {
        "object"
    } else {
        non_null.first().map(String::as_str).unwrap_or("string")
    };
    let kind = kind.to_owned();
    if kind != "array" {
        node.as_object_mut().unwrap().shift_remove("items");
    }
    node["type"] = json!(kind);
}
fn objects(node: &mut Value, name_map: bool) {
    if !name_map
        && node.get("properties").is_some_and(Value::is_object)
        && !str_of(node.get("type")).eq_ignore_ascii_case("object")
    {
        node["type"] = json!("object");
    }
}
fn cleanup(node: &mut Value, name_map: bool) {
    if !name_map && let Some(map) = node.as_object_mut() {
        map.retain(|key, _| {
            !key.starts_with("x-") && !REMOVED.split_whitespace().any(|k| k == key)
        });
    }
}
fn required(node: &mut Value, _: bool) {
    let Some(names) = node.get("required").and_then(Value::as_array) else {
        return;
    };
    let filtered: Vec<_> = names
        .iter()
        .map(|v| str_of(Some(v)).into_owned())
        .filter(|name| {
            node.get("properties")
                .and_then(Value::as_object)
                .is_some_and(|p| p.contains_key(name))
        })
        .collect();
    if filtered.len() == names.len() && node.get("properties").is_some_and(Value::is_object) {
        return;
    }
    if filtered.is_empty() {
        node.as_object_mut().unwrap().shift_remove("required");
    } else {
        node["required"] = json!(filtered);
    }
}
fn placeholders(node: &mut Value, _: bool) {
    if node.get("type") != Some(&json!("object"))
        || node.get("anyOf").is_some_and(Value::is_array)
        || node.get("oneOf").is_some_and(Value::is_array)
    {
        return;
    }
    match node.get("properties") {
        None => {
            node["properties"] = json!({"reason":{"type":"string"}});
            node["required"] = json!(["reason"]);
        }
        Some(Value::Object(props)) if props.is_empty() => {
            node["properties"] = json!({"reason":{"type":"string"}});
            node["required"] = json!(["reason"]);
        }
        Some(Value::Object(_))
            if !node
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty()) =>
        {
            node["properties"]
                .as_object_mut()
                .unwrap()
                .entry("_")
                .or_insert(json!({"type":"boolean"}));
            node["required"] = json!(["_"]);
        }
        _ => {}
    }
}

pub(super) fn requires(schema: &Value, placeholder: &str) -> bool {
    let Some(schema) = repair(schema.clone(), 0) else {
        return false;
    };
    let wrapped = Value::Object(Map::from_iter([("schema".into(), schema)]));
    let Some(inlined) = crate::gemini_schema::local_refs::inline_for_projection(&wrapped) else {
        return false;
    };
    drop(wrapped);
    let Some(mut projected) = copy_bounded(&inlined, 0, &mut 100_000) else {
        return false;
    };
    drop(inlined);
    walk(&mut projected, false, reference_hint);
    walk(&mut projected, false, hint_presence);
    walk(&mut projected, false, conditionals);
    walk(&mut projected, false, all_of);
    walk(&mut projected, false, unions);
    walk(&mut projected, false, type_arrays);
    walk(&mut projected, false, cleanup);
    walk(&mut projected, false, required);
    walk(&mut projected, false, objects);
    walk(&mut projected, false, placeholders);
    let Some(schema) = projected.get("schema") else {
        return false;
    };
    analyze(schema, schema).properties.contains_key(placeholder)
        && gjson_array(schema.get("required")).any(|v| str_of(Some(v)) == placeholder)
}

#[cfg(test)]
mod depth_tests {
    use super::*;
    use crate::gemini_schema::local_refs::tests::{NESTED_CHAINS, nested_reference_chain};

    #[test]
    fn nested_refs_repair_gate_and_direct_projection_fit_two_mib_stack() {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(|| {
                // This shape passes repair's gate and necessarily reaches the
                // inliner inside requires, unlike the intentionally deep cases.
                let shallow = nested_reference_chain(4, 4);
                let repaired = repair(shallow.clone(), 0).expect("under repair's depth gate");
                let wrapped = Value::Object(Map::from_iter([("schema".into(), repaired)]));
                let out = crate::gemini_schema::local_refs::inline_for_projection(&wrapped)
                    .expect("under the inliner's depth and copy limits");
                assert!(copy_bounded(&out, 0, &mut 100_000).is_some());
                assert!(!requires(&shallow, "_"));
                assert!(!requires(&shallow, "reason"));
                for (links, levels) in NESTED_CHAINS {
                    let schema = nested_reference_chain(links, levels);
                    assert!(!requires(&schema, "_"));
                    assert!(!requires(&schema, "reason"));
                    if let Some(out) =
                        crate::gemini_schema::local_refs::inline_for_projection(&schema)
                    {
                        assert!(copy_bounded(&out, 0, &mut 100_000).is_some());
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

#[cfg(test)]
mod round3_tests {
    use super::*;
    #[test]
    fn round3_truncated_projection_preserves_arguments() {
        let mut literal = json!("x");
        for _ in 0..65 {
            literal = json!([literal]);
        }
        let schema =
            json!({"type":"object","properties":{"value":{"type":"string","enum":literal}}});
        assert!(!requires(&schema, "reason"));
    }
}
