// Ported from CLIProxyAPI internal/util/gemini_schema.go (CleanJSONSchemaForGeminiJSONSchema and the
// passes it runs) and internal/util/translator.go (Walk) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Making a JSON Schema fit for a Gemini function declaration's
//! `parametersJsonSchema`.
//!
//! [`clean_json_schema_for_gemini_json_schema`] is upstream's
//! `CleanJSONSchemaForGeminiJSONSchema`. It keeps standard constraints such as
//! `pattern` and `minLength`, and `additionalProperties` in both its forms. It
//! removes what Gemini rejects:
//! - `$ref` becomes a description hint, and `const` becomes `enum`
//! - enum values become strings
//! - `allOf` is merged into its parent, and `anyOf` and `oneOf` are merged or
//!   flattened to one branch
//! - a type array is flattened to one type
//! - conditionals, metadata keywords and `x-` extensions are dropped
//!
//! It also repairs schemas some MCP servers write: a bare property map gets
//! its `type` and `properties` wrapper, a boolean `required` on a property
//! moves into its parent's `required`, and an array gets `items`.
//!
//! Upstream rewrites the schema's JSON text one gjson path at a time, and
//! several of its passes depend on how those paths resolve:
//! - a key holding a dot is escaped
//! - a key that is the empty string makes a path that resolves somewhere else
//! - a path found before an edit may name something else after it
//!
//! To give the same output, this module builds the same path strings and
//! resolves them as gjson and sjson do, on a parsed [`Value`].
//!
//! Upstream's other cleaners are not ported: `CleanJSONSchemaForAntigravity`
//! with its tool and response variants, and the legacy
//! `CleanJSONSchemaForGemini`. Neither are the passes only they run (local
//! `$ref` inlining, constraint and `not` hints, the empty-schema placeholder).
//! Antigravity is out of scope, and nothing here calls the legacy cleaner.
//!
//! Deviations from upstream:
//! - Upstream doesn't escape some gjson path syntax in keys: `|`, `#`, `@`, or
//!   a leading `[`, `{`, `!` or `:`. We take such a key literally. Upstream's
//!   paths through it resolve as queries or modifiers, or not at all.
//! - Where upstream copies JSON text into a string, we write the JSON
//!   compactly. That happens for an enum value, `type` or `$ref` that is an
//!   object or array, and for such a `required` entry.
//! - Of duplicate keys, upstream's reads see the first, and its repair of a
//!   malformed schema keeps the last. We only ever see the last.
//! - A `const` number too large for a float64 is kept as written in the new
//!   `enum`. Upstream fails to encode it and returns an empty string.
//! - sjson pads an array with nulls up to the index it sets. A key that names
//!   an index past 1024 leaves the schema unchanged there.
//! - Upstream drops nullable properties from each object's `required` in Go's
//!   random map order; we go in document order. The order only matters for a
//!   `required` array that itself holds such a schema.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::json::{go_marshaled, object, str_of};

/// The description upstream gives a placeholder `reason` property.
const PLACEHOLDER_REASON_DESCRIPTION: &str = "Brief explanation of why you are calling this tool";

/// The highest array index we pad to with nulls; see the module docs.
const MAX_PADDED_INDEX: usize = 1024;

/// Keywords Gemini rejects, removed wherever they are a keyword rather than a
/// name. Upstream's list also has `additionalProperties`, which this cleaner
/// keeps.
const UNSUPPORTED_KEYWORDS: [&str; 26] = [
    "$schema",
    "$defs",
    "definitions",
    "const",
    "$ref",
    "$id",
    "id",
    "$anchor",
    "$vocabulary",
    "$dynamicRef",
    "$dynamicAnchor",
    "propertyNames",
    "patternProperties",
    "if",
    "then",
    "else",
    "$comment",
    "enumDescriptions",
    "enumTitles",
    "prefill",
    "deprecated",
    "encrypted",
    "additionalItems",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contentSchema",
];

/// Keywords whose value maps names chosen by the schema's author to
/// subschemas, so a key directly under one is a name, never a keyword.
const NAME_MAP_KEYWORDS: [&str; 5] = [
    "properties",
    "patternProperties",
    "dependentSchemas",
    "$defs",
    "definitions",
];

/// Upstream's `CleanJSONSchemaForGeminiJSONSchema`: `schema` made fit for a
/// Gemini function declaration's `parametersJsonSchema`.
///
/// Pass the schema itself, never a whole request. The passes rewrite keys by
/// name wherever they are, and only keys directly under `properties` and the
/// like are protected as names.
pub(crate) fn clean_json_schema_for_gemini_json_schema(schema: &Value) -> Value {
    let mut doc = normalize_malformed_schema_objects(schema);

    // Convert and add hints.
    convert_refs_to_hints(&mut doc);
    convert_const_to_enum(&mut doc);
    convert_enum_values_to_strings(&mut doc);
    add_enum_hints(&mut doc);

    // Flatten.
    merge_conditionals(&mut doc);
    merge_all_of(&mut doc);
    flatten_any_of_one_of(&mut doc);
    flatten_type_arrays(&mut doc);

    // Clean up.
    remove_unsupported_keywords(&mut doc);
    remove_keywords(&mut doc, &["nullable", "title"]);
    remove_placeholder_fields(&mut doc);
    cleanup_required_fields(&mut doc);
    sanitize_array_items(&mut doc);
    doc
}

/// Ensures a schema node that has `items` is an array: a missing type becomes
/// `array`, and `items` is dropped from a node of another type.
fn sanitize_array_items(doc: &mut Value) {
    let mut paths = find_paths(doc, "items");
    sort_by_depth(&mut paths);
    for path in paths {
        let parent = trim_suffix(&path, ".items");
        if is_property_definition(&parent) {
            continue;
        }
        let type_path = join_path(&parent, "type");
        let kind = str_of(get(doc, &type_path)).into_owned();
        if kind.is_empty() {
            set(doc, &type_path, Value::from("array"));
        } else if !kind.eq_ignore_ascii_case("array") {
            delete(doc, &path);
        }
    }
}

/// Removes every use of `keywords` as a keyword.
fn remove_keywords(doc: &mut Value, keywords: &[&str]) {
    let mut paths_by_field = find_paths_by_fields(doc, keywords);
    let mut delete_paths = Vec::new();
    for &key in keywords {
        for path in paths_by_field.remove(key).unwrap_or_default() {
            if !is_property_definition(&trim_suffix(&path, &format!(".{key}"))) {
                delete_paths.push(path);
            }
        }
    }
    sort_by_depth(&mut delete_paths);
    for path in delete_paths {
        delete(doc, &path);
    }
}

/// Removes the placeholder properties upstream adds for Claude: `_`, and a
/// `reason` that is its object's only property, with their `required` entries.
fn remove_placeholder_fields(doc: &mut Value) {
    let mut paths = find_paths(doc, "_");
    sort_by_depth(&mut paths);
    for path in paths {
        if !path.ends_with(".properties._") {
            continue;
        }
        delete(doc, &path);
        let parent = trim_suffix(&path, ".properties._");
        remove_required_name(doc, &join_path(&parent, "required"), "_");
    }

    let mut paths = find_paths(doc, "reason");
    sort_by_depth(&mut paths);
    for path in paths {
        if !path.ends_with(".properties.reason") {
            continue;
        }
        let parent = trim_suffix(&path, ".properties.reason");
        let Some(Value::Object(properties)) = get(doc, &join_path(&parent, "properties")) else {
            continue;
        };
        if properties.len() != 1
            || str_of(get(doc, &format!("{path}.description"))) != PLACEHOLDER_REASON_DESCRIPTION
        {
            continue;
        }
        delete(doc, &path);
        remove_required_name(doc, &join_path(&parent, "required"), "reason");
    }
}

/// Removes `name` from the `required` array at `path`, removing the array if
/// nothing is left.
fn remove_required_name(doc: &mut Value, path: &str, name: &str) {
    let Some(Value::Array(required)) = get(doc, path) else {
        return;
    };
    let kept: Vec<String> = required
        .iter()
        .map(|item| str_of(Some(item)).into_owned())
        .filter(|item| item != name)
        .collect();
    if kept.is_empty() {
        delete(doc, path);
    } else {
        set(doc, path, strings(kept));
    }
}

/// Upstream's `normalizeMalformedSchemaObjects`: repairs the malformed schemas
/// some MCP servers write (see [`repair_schema_node`]). Root `true` becomes
/// `{}`. A whole request is left alone, and so is a single-key `{"schema":
/// ...}` wrapper, whose inner schema is repaired instead. A repaired schema
/// comes out as Go encodes a map: with every object's keys sorted.
fn normalize_malformed_schema_objects(schema: &Value) -> Value {
    let root = match schema {
        Value::Bool(true) => return Value::Object(Map::new()),
        Value::Object(root) if !is_api_request_document(root) => root,
        _ => return schema.clone(),
    };
    if root.len() == 1 {
        match root.get("schema") {
            Some(Value::Object(inner)) => {
                let (repaired, modified) = repair_schema_node(inner);
                if !modified {
                    return schema.clone();
                }
                return sorted(object([("schema", Value::Object(repaired))]));
            }
            Some(Value::Bool(true)) => return object([("schema", Value::Object(Map::new()))]),
            _ => {}
        }
    }
    let (repaired, modified) = repair_schema_node(root);
    if modified {
        sorted(Value::Object(repaired))
    } else {
        schema.clone()
    }
}

/// `value` with the keys of every object sorted, as Go encodes a map.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut fields: Vec<(String, Value)> = fields.into_iter().collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| (key, sorted(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// Reports whether an object-valued key is a schema keyword or extension
/// rather than a bare property definition.
fn is_known_schema_keyword_or_extension(key: &str) -> bool {
    key.starts_with("x-")
        || matches!(
            key,
            "properties"
                | "patternProperties"
                | "additionalProperties"
                | "items"
                | "prefixItems"
                | "$defs"
                | "definitions"
                | "dependentSchemas"
                | "dependentRequired"
                | "dependencies"
                | "if"
                | "then"
                | "else"
                | "not"
                | "contains"
                | "propertyNames"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "contentSchema"
                | "additionalItems"
                | "default"
                | "const"
                | "example"
                | "examples"
                | "discriminator"
                | "xml"
                | "externalDocs"
                | "enumDescriptions"
                | "enumTitles"
        )
}

/// Reports whether `kind` declares a type other than object: a non-empty
/// string other than `object`, or a non-empty list without `object`. Case is
/// ignored, as Go's `strings.EqualFold` ignores it for these ASCII words.
fn is_non_object_declared_type(kind: Option<&Value>) -> bool {
    match kind {
        Some(Value::String(kind)) => !kind.is_empty() && !kind.eq_ignore_ascii_case("object"),
        Some(Value::Array(kinds)) => {
            !kinds.is_empty()
                && !kinds.iter().any(|kind| {
                    kind.as_str()
                        .is_some_and(|k| k.eq_ignore_ascii_case("object"))
                })
        }
        _ => false,
    }
}

/// Reports whether `kind` declares an array: `array`, or a list holding it.
fn is_array_declared_type(kind: Option<&Value>) -> bool {
    match kind {
        Some(Value::String(kind)) => kind.eq_ignore_ascii_case("array"),
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| {
            kind.as_str()
                .is_some_and(|k| k.eq_ignore_ascii_case("array"))
        }),
        _ => false,
    }
}

/// Reports whether `fields` look like a whole API request rather than a schema.
fn is_api_request_document(fields: &Map<String, Value>) -> bool {
    [
        "tools",
        "contents",
        "messages",
        "functionDeclarations",
        "function_declarations",
    ]
    .iter()
    .any(|key| fields.get(*key).is_some_and(Value::is_array))
        || matches!(fields.get("request"), Some(Value::Object(request)) if is_api_request_document(request))
}

/// Upstream's `repairSchemaNode`: `node` repaired, and whether anything changed.
///
/// Unless the node declares a type other than object, its object-valued keys
/// that aren't keywords are bare property definitions: they move into
/// `properties`, and the node becomes an object if it has no type. Properties
/// are repaired (see [`repair_property_map`]), an array without `items` gets
/// `{"type":"string"}`, a node with `items` and no type becomes an array, and
/// every subschema is repaired in turn, `true` becoming `{}`.
fn repair_schema_node(node: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut modified = false;
    let mut node = node.clone();

    if !is_non_object_declared_type(node.get("type")) {
        let bare: Map<String, Value> = node
            .iter()
            .filter(|(key, value)| value.is_object() && !is_known_schema_keyword_or_extension(key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !bare.is_empty() {
            let (repaired, promoted, _) = repair_property_map(&bare);
            for key in bare.keys() {
                node.shift_remove(key);
            }
            if let Some(Value::Object(existing)) = node.get_mut("properties") {
                existing.extend(repaired);
            } else {
                node.insert("properties".into(), Value::Object(repaired));
                if !node.contains_key("type") {
                    node.insert("type".into(), Value::from("object"));
                }
            }
            if !promoted.is_empty() {
                promote_required(&mut node, &promoted);
            }
            modified = true;
        }
    }

    if let Some(Value::Object(properties)) = node.get("properties") {
        let (repaired, promoted, properties_modified) = repair_property_map(properties);
        if properties_modified {
            node.insert("properties".into(), Value::Object(repaired));
            modified = true;
        }
        if !promoted.is_empty() {
            promote_required(&mut node, &promoted);
            modified = true;
        }
    }

    if is_array_declared_type(node.get("type")) {
        if !node.contains_key("items") {
            node.insert("items".into(), object([("type", Value::from("string"))]));
            modified = true;
        }
    } else if node.contains_key("items") {
        let untyped = match node.get("type") {
            None | Some(Value::Null) => true,
            Some(Value::String(kind)) => kind.is_empty(),
            Some(_) => false,
        };
        if untyped {
            node.insert("type".into(), Value::from("array"));
            modified = true;
        }
    }

    match node.get("items") {
        Some(Value::Object(items)) => {
            let (repaired, items_modified) = repair_schema_node(items);
            if items_modified {
                node.insert("items".into(), Value::Object(repaired));
                modified = true;
            }
        }
        Some(Value::Array(items)) => {
            let (repaired, items_modified) = repair_schema_list(items);
            if items_modified {
                node.insert("items".into(), Value::Array(repaired));
                modified = true;
            }
        }
        Some(Value::Bool(true)) => {
            node.insert("items".into(), Value::Object(Map::new()));
            modified = true;
        }
        _ => {}
    }

    if let Some(Value::Object(additional)) = node.get("additionalProperties") {
        let (repaired, additional_modified) = repair_schema_node(additional);
        if additional_modified {
            node.insert("additionalProperties".into(), Value::Object(repaired));
            modified = true;
        }
    }

    if let Some(Value::Object(patterns)) = node.get("patternProperties") {
        let (repaired, _, patterns_modified) = repair_property_map(patterns);
        if patterns_modified {
            node.insert("patternProperties".into(), Value::Object(repaired));
            modified = true;
        }
    }

    for key in [
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
        match node.get(key) {
            Some(Value::Object(sub)) => {
                let (repaired, sub_modified) = repair_schema_node(sub);
                if sub_modified {
                    node.insert(key.into(), Value::Object(repaired));
                    modified = true;
                }
            }
            Some(Value::Bool(true)) => {
                node.insert(key.into(), Value::Object(Map::new()));
                modified = true;
            }
            _ => {}
        }
    }

    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(list)) = node.get(key) {
            let (repaired, list_modified) = repair_schema_list(list);
            if list_modified {
                node.insert(key.into(), Value::Array(repaired));
                modified = true;
            }
        }
    }

    for key in ["$defs", "definitions", "dependentSchemas", "dependencies"] {
        let Some(Value::Object(definitions)) = node.get(key) else {
            continue;
        };
        let mut repaired = Map::new();
        let mut definitions_modified = false;
        for (name, definition) in definitions {
            let definition = match definition {
                Value::Object(definition) => {
                    let (definition, definition_modified) = repair_schema_node(definition);
                    definitions_modified |= definition_modified;
                    Value::Object(definition)
                }
                Value::Bool(true) => {
                    definitions_modified = true;
                    Value::Object(Map::new())
                }
                other => other.clone(),
            };
            repaired.insert(name.clone(), definition);
        }
        if definitions_modified {
            node.insert(key.into(), Value::Object(repaired));
            modified = true;
        }
    }

    (node, modified)
}

/// Adds `promoted` names to the node's `required`, after its string entries,
/// without duplicates or empty names. Upstream writes `null` if nothing is
/// left.
fn promote_required(node: &mut Map<String, Value>, promoted: &[String]) {
    let mut merged: Vec<String> = Vec::new();
    let existing = match node.get("required") {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    for name in existing
        .into_iter()
        .chain(promoted.iter().map(String::as_str))
    {
        if !name.is_empty() && !merged.iter().any(|seen| seen == name) {
            merged.push(name.to_owned());
        }
    }
    node.insert("required".into(), strings(merged));
}

/// Upstream's `repairSchemaList`: each object in `list` repaired, and `true`
/// made `{}`.
fn repair_schema_list(list: &[Value]) -> (Vec<Value>, bool) {
    let mut modified = false;
    let repaired = list
        .iter()
        .map(|item| match item {
            Value::Object(item) => {
                let (item, item_modified) = repair_schema_node(item);
                modified |= item_modified;
                Value::Object(item)
            }
            Value::Bool(true) => {
                modified = true;
                Value::Object(Map::new())
            }
            other => other.clone(),
        })
        .collect();
    (repaired, modified)
}

/// Upstream's `repairPropertyMap`: each property repaired, `true` made `{}`,
/// and a boolean `required` on a property removed. Returns the names whose
/// `required` was `true`, sorted, for the parent's `required`.
fn repair_property_map(properties: &Map<String, Value>) -> (Map<String, Value>, Vec<String>, bool) {
    let mut out = Map::new();
    let mut promoted = Vec::new();
    let mut modified = false;
    for (name, property) in properties {
        let property = match property {
            Value::Bool(true) => {
                modified = true;
                Value::Object(Map::new())
            }
            Value::Object(property) => {
                let mut property = property.clone();
                if let Some(&Value::Bool(required)) = property.get("required") {
                    property.shift_remove("required");
                    modified = true;
                    if required {
                        promoted.push(name.clone());
                    }
                }
                let (property, property_modified) = repair_schema_node(&property);
                modified |= property_modified;
                Value::Object(property)
            }
            other => other.clone(),
        };
        out.insert(name.clone(), property);
    }
    promoted.sort();
    (out, promoted, modified)
}

/// The last part of a `$ref`, with JSON Pointer escapes decoded.
fn ref_name(reference: &str) -> String {
    match reference.rfind('/') {
        Some(index) if index + 1 < reference.len() => {
            reference[index + 1..].replace("~1", "/").replace("~0", "~")
        }
        _ => reference.to_owned(),
    }
}

/// Replaces each schema holding a `$ref` with an object schema whose
/// description names the definition.
fn convert_refs_to_hints(doc: &mut Value) {
    let mut paths = find_paths(doc, "$ref");
    sort_by_depth(&mut paths);
    for path in paths {
        let name = ref_name(&str_of(get(doc, &path)));
        let parent = trim_suffix(&path, ".$ref");
        let mut hint = format!("See: {name}");
        let existing = str_of(get(doc, &description_path(&parent)));
        if !existing.is_empty() {
            hint = format!("{existing} ({hint})");
        }
        let replacement = object([
            ("type", Value::from("object")),
            ("description", Value::from(hint)),
        ]);
        set_raw_at(doc, &parent, replacement);
    }
}

/// Adds an `enum` holding the value of each `const` that has none.
///
/// For a root `const` upstream's path is `.enum`, which names the `enum` key
/// of an object under the empty key; this mirrors that.
fn convert_const_to_enum(doc: &mut Value) {
    for path in find_paths(doc, "const") {
        let Some(value) = get(doc, &path) else {
            continue;
        };
        let value = Value::Array(vec![go_marshaled(value)]);
        let enum_path = format!("{}.enum", trim_suffix(&path, ".const"));
        if get(doc, &enum_path).is_none() {
            set(doc, &enum_path, value);
        }
    }
}

/// Writes every enum value as a string, and declares each enum's type as
/// `string`. An empty enum becomes `null`, as upstream encodes its nil slice.
fn convert_enum_values_to_strings(doc: &mut Value) {
    for path in find_paths(doc, "enum") {
        let Some(Value::Array(items)) = get(doc, &path) else {
            continue;
        };
        let values: Vec<String> = items
            .iter()
            .map(|item| str_of(Some(item)).into_owned())
            .collect();
        set(doc, &path, strings(values));
        let parent = trim_suffix(&path, ".enum");
        set(doc, &join_path(&parent, "type"), Value::from("string"));
    }
}

/// Adds an `Allowed: ...` hint for each enum of two to ten values.
fn add_enum_hints(doc: &mut Value) {
    for path in find_paths(doc, "enum") {
        let Some(Value::Array(items)) = get(doc, &path) else {
            continue;
        };
        if items.len() <= 1 || items.len() > 10 {
            continue;
        }
        let values: Vec<String> = items
            .iter()
            .map(|item| str_of(Some(item)).into_owned())
            .collect();
        let hint = format!("Allowed: {}", values.join(", "));
        append_hint(doc, &trim_suffix(&path, ".enum"), &hint);
    }
}

/// Copies the properties of each `then` and `else` branch into the parent's
/// `properties`, where it has none of that name.
fn merge_conditionals(doc: &mut Value) {
    let mut paths_by_field = find_paths_by_fields(doc, &["then", "else"]);
    let mut paths = Vec::new();
    for key in ["then", "else"] {
        for path in paths_by_field.remove(key).unwrap_or_default() {
            if !is_property_definition(&trim_suffix(&path, &format!(".{key}"))) {
                paths.push(path);
            }
        }
    }
    sort_by_depth(&mut paths);

    for path in paths {
        let Some(Value::Object(properties)) = get(doc, &join_path(&path, "properties")).cloned()
        else {
            continue;
        };
        let parent = if path.ends_with(".then") {
            trim_suffix(&path, ".then")
        } else if path.ends_with(".else") {
            trim_suffix(&path, ".else")
        } else if path == "then" || path == "else" {
            String::new()
        } else {
            continue;
        };
        for (key, value) in properties {
            let destination = join_path(&parent, &format!("properties.{}", escape_key(&key)));
            if get(doc, &destination).is_none() {
                set(doc, &destination, value);
            }
        }
    }
}

/// Merges each `allOf` branch into its parent: `required` names are added,
/// conditionals are dropped, and other keywords fill what the parent lacks.
/// The `allOf` is then removed.
fn merge_all_of(doc: &mut Value) {
    let mut paths = find_paths(doc, "allOf");
    sort_by_depth(&mut paths);
    for path in paths {
        let Some(Value::Array(branches)) = get(doc, &path).cloned() else {
            continue;
        };
        let parent = trim_suffix(&path, ".allOf");
        for branch in branches {
            let Value::Object(fields) = branch else {
                continue;
            };
            for (field, value) in fields {
                match field.as_str() {
                    "required" => {
                        let Value::Array(names) = value else {
                            continue;
                        };
                        let required_path = join_path(&parent, "required");
                        let mut current = get_strings(doc, &required_path);
                        for name in &names {
                            let name = str_of(Some(name));
                            if !current.iter().any(|seen| *seen == name) {
                                current.push(name.into_owned());
                            }
                        }
                        set(doc, &required_path, strings(current));
                    }
                    // A condition can't be expressed in the upstream schema.
                    "if" | "then" | "else" | "allOf" => {}
                    _ => {
                        let destination = join_path(&parent, &escape_key(&field));
                        merge_missing_schema_at_path(doc, &destination, &value);
                    }
                }
            }
        }
        delete(doc, &path);
    }
}

/// Fills what is missing at `destination` from `incoming`, recursing into
/// objects, without replacing anything already there.
fn merge_missing_schema_at_path(doc: &mut Value, destination: &str, incoming: &Value) {
    let Some(existing) = get(doc, destination) else {
        set(doc, destination, incoming.clone());
        return;
    };
    let (true, Value::Object(incoming)) = (existing.is_object(), incoming) else {
        return;
    };
    for (key, value) in incoming {
        merge_missing_schema_at_path(doc, &join_path(destination, &escape_key(key)), value);
    }
}

/// Resolves each `anyOf` and `oneOf`. Where the parent has `properties`, the
/// branches' properties fill what it lacks and a null branch makes it
/// nullable. Otherwise the parent is replaced by its best branch (see
/// [`select_best`]), with the parent's description and an `Accepts: ...` hint
/// naming the branches' types.
fn flatten_any_of_one_of(doc: &mut Value) {
    for key in ["anyOf", "oneOf"] {
        let mut paths = find_paths(doc, key);
        sort_by_depth(&mut paths);
        for path in paths {
            let Some(Value::Array(items)) = get(doc, &path).cloned() else {
                continue;
            };
            if items.is_empty() {
                continue;
            }
            let parent_path = trim_suffix(&path, &format!(".{key}"));
            let parent = if parent_path.is_empty() {
                Some(&*doc)
            } else {
                get(doc, &parent_path)
            };

            if parent
                .and_then(|parent| child(parent, "properties"))
                .is_some_and(Value::is_object)
            {
                let mut has_null = false;
                for item in &items {
                    if str_of(child(item, "type")) == "null" {
                        has_null = true;
                    }
                    if let Some(Value::Object(properties)) = child(item, "properties") {
                        for (name, property) in properties {
                            let destination = join_path(
                                &parent_path,
                                &format!("properties.{}", escape_key(name)),
                            );
                            merge_missing_schema_at_path(doc, &destination, property);
                        }
                    }
                }
                if has_null {
                    set(doc, &join_path(&parent_path, "nullable"), Value::Bool(true));
                }
                delete(doc, &path);
                continue;
            }

            let parent_description = str_of(get(doc, &description_path(&parent_path))).into_owned();
            let (best, types) = select_best(&items);
            let mut selected = items[best].clone();
            let has_null = items
                .iter()
                .any(|item| str_of(child(item, "type")) == "null");
            if has_null && str_of(child(&items[best], "type")) != "null" {
                set(&mut selected, "nullable", Value::Bool(true));
            }
            if !parent_description.is_empty() {
                merge_description(&mut selected, &parent_description);
            }
            if types.len() > 1 {
                let hint = format!("Accepts: {}", types.join(" | "));
                append_hint(&mut selected, "", &hint);
            }
            set_raw_at(doc, &parent_path, selected);
        }
    }
}

/// The index of the branch upstream keeps, and the types of all branches.
/// An object (or a branch with `properties`) beats an array (or a branch with
/// `items`), which beats any other non-null type; the first of the best wins.
fn select_best(items: &[Value]) -> (usize, Vec<String>) {
    let (mut best, mut best_score) = (0, -1);
    let mut types = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let mut kind = str_of(child(item, "type")).into_owned();
        let score = if kind == "object" || child(item, "properties").is_some() {
            if kind.is_empty() {
                kind = "object".into();
            }
            3
        } else if kind == "array" || child(item, "items").is_some() {
            if kind.is_empty() {
                kind = "array".into();
            }
            2
        } else if !kind.is_empty() && kind != "null" {
            1
        } else {
            0
        };
        if !kind.is_empty() {
            types.push(kind);
        }
        if score > best_score {
            (best, best_score) = (index, score);
        }
    }
    (best, types)
}

/// Flattens each type array to one type: `array` if the node has `items` and
/// allows it, otherwise the first non-null type, or `string`. Other non-null
/// types become an `Accepts: ...` hint. A property allowing null gets a
/// `(nullable)` hint and is dropped from its object's `required`.
fn flatten_type_arrays(doc: &mut Value) {
    let mut paths = find_paths(doc, "type");
    sort_by_depth(&mut paths);

    let mut nullable_fields: Vec<(String, Vec<String>)> = Vec::new();
    for path in paths {
        let Some(Value::Array(kinds)) = get(doc, &path) else {
            continue;
        };
        if kinds.is_empty() {
            continue;
        }
        let mut has_null = false;
        let mut non_null = Vec::new();
        for kind in kinds {
            let kind = str_of(Some(kind));
            if kind == "null" {
                has_null = true;
            } else if !kind.is_empty() {
                non_null.push(kind.into_owned());
            }
        }

        let parent = trim_suffix(&path, ".type");
        let items_path = join_path(&parent, "items");
        let first = match non_null.first() {
            None => "string".to_owned(),
            Some(_) if get(doc, &items_path).is_some() && non_null.iter().any(|k| k == "array") => {
                "array".to_owned()
            }
            Some(first) => first.clone(),
        };
        let is_array = first == "array";
        set(doc, &path, Value::from(first));
        if !is_array && get(doc, &items_path).is_some() {
            delete(doc, &items_path);
        }
        if non_null.len() > 1 {
            append_hint(doc, &parent, &format!("Accepts: {}", non_null.join(" | ")));
        }

        if has_null {
            let parts = segments(&path);
            if parts.len() >= 3 && parts[parts.len() - 3] == "properties" {
                let escaped = parts[parts.len() - 2];
                let field = unescape_segment(escaped);
                let object_path = parts[..parts.len() - 3].join(".");
                match nullable_fields
                    .iter_mut()
                    .find(|(path, _)| *path == object_path)
                {
                    Some((_, fields)) => fields.push(field),
                    None => nullable_fields.push((object_path.clone(), vec![field])),
                }
                let property = join_path(&object_path, &format!("properties.{escaped}"));
                append_hint(doc, &property, "(nullable)");
            }
        }
    }

    for (object_path, fields) in nullable_fields {
        let required_path = join_path(&object_path, "required");
        let Some(Value::Array(required)) = get(doc, &required_path) else {
            continue;
        };
        let kept: Vec<String> = required
            .iter()
            .map(|item| str_of(Some(item)).into_owned())
            .filter(|name| !fields.contains(name))
            .collect();
        if kept.is_empty() {
            delete(doc, &required_path);
        } else {
            set(doc, &required_path, strings(kept));
        }
    }
}

/// Removes the [`UNSUPPORTED_KEYWORDS`] and `x-` extensions.
fn remove_unsupported_keywords(doc: &mut Value) {
    remove_keywords(doc, &UNSUPPORTED_KEYWORDS);
    remove_extension_fields(doc);
}

/// Removes every `x-` key that isn't a name in a name map.
fn remove_extension_fields(doc: &mut Value) {
    let mut paths = Vec::new();
    walk_for_extensions(doc, "", &mut paths);
    for path in paths {
        delete(doc, &path);
    }
}

fn walk_for_extensions(value: &Value, path: &str, paths: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate().rev() {
                walk_for_extensions(item, &join_path(path, &index.to_string()), paths);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                let child_path = join_path(path, &escape_key(key));
                if key.starts_with("x-") && !is_property_definition(path) {
                    paths.push(child_path);
                    continue;
                }
                walk_for_extensions(value, &child_path, paths);
            }
        }
        _ => {}
    }
}

/// Drops `required` names that aren't properties, and `required` itself where
/// there are no properties or no names left.
fn cleanup_required_fields(doc: &mut Value) {
    for path in find_paths(doc, "required") {
        let parent = trim_suffix(&path, ".required");
        let Some(Value::Array(required)) = get(doc, &path) else {
            continue;
        };
        let properties = get(doc, &join_path(&parent, "properties"));
        let Some(properties @ Value::Object(_)) = properties else {
            delete(doc, &path);
            continue;
        };
        let count = required.len();
        let valid: Vec<String> = required
            .iter()
            .map(|item| str_of(Some(item)).into_owned())
            .filter(|name| get(properties, &escape_key(name)).is_some())
            .collect();
        if valid.len() != count {
            if valid.is_empty() {
                delete(doc, &path);
            } else {
                set(doc, &path, strings(valid));
            }
        }
    }
}

/// A list of strings as upstream's sjson writes a Go string slice: `null` when
/// empty, as only a nil slice is.
fn strings(values: Vec<String>) -> Value {
    if values.is_empty() {
        Value::Null
    } else {
        Value::Array(values.into_iter().map(Value::String).collect())
    }
}

/// The strings of the array at `path`, as gjson's `String()` gives them, or
/// none if it isn't an array.
fn get_strings(doc: &Value, path: &str) -> Vec<String> {
    match get(doc, path) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| str_of(Some(item)).into_owned())
            .collect(),
        _ => Vec::new(),
    }
}

/// Upstream's `mergeHint`: `hint` added to a description in parentheses,
/// unless the description already has it.
fn merge_hint(existing: &str, hint: &str) -> String {
    if existing.is_empty() {
        return hint.to_owned();
    }
    if existing == hint
        || existing.starts_with(&format!("{hint} ("))
        || existing.contains(&format!("({hint})"))
    {
        return existing.to_owned();
    }
    format!("{existing} ({hint})")
}

/// Adds `hint` to the description of the schema at `parent` (`""` for the
/// root).
fn append_hint(doc: &mut Value, parent: &str, hint: &str) {
    let path = description_path(parent);
    let merged = merge_hint(&str_of(get(doc, &path)), hint);
    set(doc, &path, Value::from(merged));
}

/// Upstream's `mergeDescriptionRaw`: puts the parent's description on the
/// branch replacing it, with the branch's own after it in parentheses.
fn merge_description(schema: &mut Value, parent_description: &str) {
    let own = str_of(get(schema, "description")).into_owned();
    if own.is_empty() {
        set(schema, "description", Value::from(parent_description));
    } else if own != parent_description {
        set(
            schema,
            "description",
            Value::from(format!("{parent_description} ({own})")),
        );
    }
}

// ---------------------------------------------------------------------------
// gjson and sjson paths, as upstream builds and resolves them.

/// A key as upstream writes it in a path: `.`, `*` and `?` escaped.
fn escape_key(key: &str) -> Cow<'_, str> {
    if !key.contains(['.', '*', '?']) {
        return Cow::Borrowed(key);
    }
    let mut escaped = String::with_capacity(key.len() + 2);
    for c in key.chars() {
        if matches!(c, '.' | '*' | '?') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    Cow::Owned(escaped)
}

/// The keys a path names, as gjson and sjson read it: split at each unescaped
/// dot, with each escaping backslash removed. A backslash at the very end is
/// dropped.
fn keys(path: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut key = String::new();
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => key.extend(chars.next()),
            '.' => keys.push(std::mem::take(&mut key)),
            c => key.push(c),
        }
    }
    keys.push(key);
    keys
}

/// Upstream's `splitGJSONPath`: a path split at each unescaped dot, escapes
/// kept. An empty path has no segments.
fn segments(path: &str) -> Vec<&str> {
    if path.is_empty() {
        return Vec::new();
    }
    let bytes = path.as_bytes();
    let mut segments = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'.' => {
                segments.push(&path[start..i]);
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    segments.push(&path[start..]);
    segments
}

/// Upstream's `unescapeGJSONPathKey`: each backslash removed and the
/// character after it kept. A backslash at the end is kept.
fn unescape_segment(segment: &str) -> String {
    let mut key = String::with_capacity(segment.len());
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        key.push(if c == '\\' {
            chars.next().unwrap_or('\\')
        } else {
            c
        });
    }
    key
}

/// Upstream's `sortByDepth`: deepest paths first, by segment count, keeping
/// the order of paths of the same depth.
fn sort_by_depth(paths: &mut [String]) {
    paths.sort_by_key(|path| Reverse(segments(path).len()));
}

/// Upstream's `trimSuffix`: `path` without `suffix`, or `""` if `path` is the
/// suffix without its leading dot.
fn trim_suffix(path: &str, suffix: &str) -> String {
    if path == suffix.strip_prefix('.').unwrap_or(suffix) {
        return String::new();
    }
    path.strip_suffix(suffix).unwrap_or(path).to_owned()
}

/// `suffix` under `base`, or `suffix` alone if `base` is the root.
fn join_path(base: &str, suffix: &str) -> String {
    if base.is_empty() {
        suffix.to_owned()
    } else {
        format!("{base}.{suffix}")
    }
}

/// The path of the description of the schema at `parent`.
fn description_path(parent: &str) -> String {
    if parent.is_empty() || parent == "@this" {
        "description".to_owned()
    } else {
        format!("{parent}.description")
    }
}

/// Upstream's `isPropertyDefinition`: whether `path` names a map whose keys
/// are names, so a key spelled like a keyword there is kept. Each name-map
/// keyword at the end of the path flips the answer: `properties` is a map,
/// `properties.properties` the schema of a property named `properties`.
fn is_property_definition(path: &str) -> bool {
    let trailing = segments(path)
        .iter()
        .rev()
        .take_while(|segment| NAME_MAP_KEYWORDS.contains(&unescape_segment(segment).as_str()))
        .count();
    trailing % 2 == 1
}

/// Upstream's `Walk`: the path of every key named `field`, each parent before
/// its children, in document order.
fn find_paths(doc: &Value, field: &str) -> Vec<String> {
    let mut paths = Vec::new();
    walk(doc, "", &mut |key, path| {
        if key == field {
            paths.push(path.to_owned());
        }
    });
    paths
}

/// [`find_paths`] for several fields at once.
fn find_paths_by_fields<'f>(doc: &Value, fields: &[&'f str]) -> HashMap<&'f str, Vec<String>> {
    let mut paths: HashMap<&str, Vec<String>> = HashMap::new();
    walk(doc, "", &mut |key, path| {
        if let Some(&field) = fields.iter().find(|field| **field == key) {
            paths.entry(field).or_default().push(path.to_owned());
        }
    });
    paths
}

fn walk(value: &Value, path: &str, visit: &mut dyn FnMut(&str, &str)) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                let child_path = join_path(path, &escape_key(key));
                visit(key, &child_path);
                walk(value, &child_path, visit);
            }
        }
        Value::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                let key = index.to_string();
                let child_path = join_path(path, &key);
                visit(&key, &child_path);
                walk(value, &child_path, visit);
            }
        }
        _ => {}
    }
}

/// gjson `Get`: the value at `path`, if there is one. A key that is an index
/// reads an array's item, and an object's field of that name.
pub(crate) fn get<'v>(doc: &'v Value, path: &str) -> Option<&'v Value> {
    keys(path)
        .iter()
        .try_fold(doc, |value, key| child(value, key))
}

/// The value under one key: an object's field, or an array's item if the key
/// is an index.
fn child<'v>(value: &'v Value, key: &str) -> Option<&'v Value> {
    match value {
        Value::Object(fields) => fields.get(key),
        Value::Array(items) => index(key).and_then(|index| items.get(index)),
        _ => None,
    }
}

/// [`child`] for editing.
fn child_mut<'v>(value: &'v mut Value, key: &str) -> Option<&'v mut Value> {
    match value {
        Value::Object(fields) => fields.get_mut(key),
        Value::Array(items) => index(key).and_then(|index| items.get_mut(index)),
        _ => None,
    }
}

/// gjson's array index: one or more ASCII digits.
fn index(key: &str) -> Option<usize> {
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse().ok()
}

/// sjson's reading of a key as an index when it builds an array: ASCII digits,
/// none at all reading as 0.
fn build_index(key: &str) -> Option<usize> {
    if !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(if key.is_empty() {
        0
    } else {
        key.parse().unwrap_or(usize::MAX)
    })
}

/// sjson `Set` for a gjson path, whose keys are separated by dots and may be
/// escaped with backslashes. A missing key is added last, a missing array
/// index is padded to with nulls (up to [`MAX_PADDED_INDEX`]), and a value in
/// the way that is neither an object nor an array is replaced by one. sjson
/// fails, changing nothing, on an empty path and on a key in an array that
/// isn't an index (`-1` appends).
///
/// The Gemini passthrough uses it too, where upstream builds paths from
/// indices and object keys alike.
pub(crate) fn set(doc: &mut Value, path: &str, new: Value) {
    if !path.is_empty() {
        set_keys(doc, &keys(path), new);
    }
}

fn set_keys(value: &mut Value, keys: &[String], new: Value) {
    let Some((key, rest)) = keys.split_first() else {
        return;
    };
    if let Some(child) = child_mut(value, key) {
        if rest.is_empty() {
            *child = new;
        } else {
            set_keys(child, rest, new);
        }
        return;
    }
    let Some(built) = built(rest, new) else {
        return;
    };
    let at = build_index(key);
    if at.is_some_and(|at| at > MAX_PADDED_INDEX) && !value.is_object() {
        return;
    }
    if !value.is_object() && !value.is_array() {
        *value = match at {
            Some(_) => Value::Array(Vec::new()),
            None => Value::Object(Map::new()),
        };
    }
    match value {
        Value::Object(fields) => {
            fields.insert(key.clone(), built);
        }
        Value::Array(items) => match at {
            Some(at) => {
                if items.len() < at {
                    items.resize(at, Value::Null);
                }
                items.push(built);
            }
            None if key == "-1" => items.push(built),
            None => {}
        },
        _ => unreachable!("made an object or array above"),
    }
}

/// The value sjson builds for the keys after a missing one, or `None` if it
/// would pad an array past [`MAX_PADDED_INDEX`].
fn built(keys: &[String], new: Value) -> Option<Value> {
    let Some((key, rest)) = keys.split_first() else {
        return Some(new);
    };
    let inner = built(rest, new)?;
    Some(match build_index(key) {
        Some(at) if at <= MAX_PADDED_INDEX => {
            let mut items = vec![Value::Null; at];
            items.push(inner);
            Value::Array(items)
        }
        Some(_) => return None,
        None if key == "-1" => Value::Array(vec![inner]),
        None => object([(key.as_str(), inner)]),
    })
}

/// sjson's `SetRaw` as upstream's `setRawAt` calls it: an empty path replaces
/// the whole document.
fn set_raw_at(doc: &mut Value, path: &str, value: Value) {
    if path.is_empty() {
        *doc = value;
    } else {
        set(doc, path, value);
    }
}

/// sjson `Delete`: removes the value at `path`, keeping the order of the rest.
/// In an array, `-1` names the last item. Nothing changes if the path doesn't
/// lead anywhere.
fn delete(doc: &mut Value, path: &str) {
    if !path.is_empty() {
        delete_keys(doc, &keys(path));
    }
}

fn delete_keys(value: &mut Value, keys: &[String]) {
    let Some((key, rest)) = keys.split_first() else {
        return;
    };
    match value {
        Value::Object(fields) => {
            if rest.is_empty() {
                fields.shift_remove(key);
            } else if let Some(child) = fields.get_mut(key) {
                delete_keys(child, rest);
            }
        }
        Value::Array(items) => {
            let at = if key == "-1" {
                items.len().checked_sub(1)
            } else {
                index(key).filter(|&at| at < items.len())
            };
            if let Some(at) = at {
                if rest.is_empty() {
                    items.remove(at);
                } else {
                    delete_keys(&mut items[at], rest);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
