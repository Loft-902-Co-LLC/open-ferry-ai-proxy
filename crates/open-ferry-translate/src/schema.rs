// Ported from CLIProxyAPI internal/util/claude_schema.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! JSON Schema keyword tables and checks shared by tool-schema cleaners, and
//! making a schema fit for a Claude tool.

use serde_json::{Map, Value, json};

/// Keywords whose value is a map of subschemas.
pub(crate) const MAP_KEYWORDS: [&str; 6] = [
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
];

/// Keywords whose value is a single subschema or a list of subschemas.
pub(crate) const VALUE_KEYWORDS: [&str; 16] = [
    "items",
    "prefixItems",
    "contains",
    "additionalProperties",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "additionalItems",
    "contentSchema",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
];

/// Reports whether a regex uses `\p{..}`/`\P{..}` property escapes or `\0`,
/// which some upstream schema validators reject. Escaped backslashes are skipped,
/// so a literal `\\p{..}` is allowed.
pub(crate) fn has_unsupported_unicode_property_escape(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let Some(&next) = bytes.get(i + 1) else {
            break;
        };
        if matches!(next, b'p' | b'P') && bytes.get(i + 2) == Some(&b'{') {
            return true;
        }
        if next == b'0' {
            return true;
        }
        i += 2;
    }
    false
}

/// Makes a JSON Schema fit Claude's rule that a tool's input schema is an
/// object with no union at the root (`NormalizeClaudeToolInputSchema`).
///
/// The properties of each `anyOf`, `oneOf` and `allOf` branch that can be an
/// object are merged into the root, and an `allOf` branch's `required` names
/// join the root's. A schema that isn't an object becomes an empty object
/// schema. Like upstream, which goes through a Go map, the root's keys and its
/// property names come out sorted.
pub(crate) fn normalize_claude_tool_input_schema(schema: Option<&Value>) -> Value {
    let Some(Value::Object(schema)) = schema else {
        return json!({"type": "object", "properties": {}});
    };
    let mut root = schema.clone();
    let mut properties = match root.get("properties") {
        Some(Value::Object(properties)) => properties.clone(),
        _ => Map::new(),
    };
    for union in ["anyOf", "oneOf", "allOf"] {
        let Some(Value::Array(branches)) = root.shift_remove(union) else {
            continue;
        };
        for branch in &branches {
            let Value::Object(branch) = branch else {
                continue;
            };
            if !can_be_object(branch.get("type")) {
                continue;
            }
            if let Some(Value::Object(branch_properties)) = branch.get("properties") {
                for (name, property) in branch_properties {
                    if !properties.contains_key(name) {
                        properties.insert(name.clone(), property.clone());
                    }
                }
            }
            if union == "allOf" {
                merge_required(&mut root, branch.get("required"));
            }
        }
    }
    root.insert("type".into(), "object".into());
    root.insert("properties".into(), Value::Object(sorted(properties)));
    Value::Object(sorted(root))
}

/// Reports whether a schema with this `type` can describe an object.
fn can_be_object(schema_type: Option<&Value>) -> bool {
    match schema_type {
        None => true,
        Some(Value::String(schema_type)) => schema_type == "object",
        Some(types) => {
            string_list(Some(types)).is_some_and(|types| types.iter().any(|t| t == "object"))
        }
    }
}

/// Adds a branch's `required` names to the root's, skipping names already there.
fn merge_required(root: &mut Map<String, Value>, branch_required: Option<&Value>) {
    let mut required = string_list(root.get("required")).unwrap_or_default();
    let Some(names) = branch_required.and_then(|names| string_list(Some(names))) else {
        return;
    };
    for name in names {
        if !required.contains(&name) {
            required.push(name);
        }
    }
    if !required.is_empty() {
        root.insert("required".into(), required.into());
    }
}

/// A list of strings as Go's `json.Unmarshal` reads it into `[]string`:
/// `null` is an empty list and a `null` item an empty string. Anything else
/// fails.
fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => Some(s.clone()),
                Value::Null => Some(String::new()),
                _ => None,
            })
            .collect(),
        Some(_) => None,
    }
}

fn sorted(map: Map<String, Value>) -> Map<String, Value> {
    let mut fields: Vec<(String, Value)> = map.into_iter().collect();
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    fields.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_property_escapes_but_not_escaped_backslashes() {
        assert!(has_unsupported_unicode_property_escape(r"[^\p{Cc}]"));
        assert!(has_unsupported_unicode_property_escape(r"\P{L}+"));
        assert!(has_unsupported_unicode_property_escape(r"a\0"));
        assert!(!has_unsupported_unicode_property_escape(r"^\\p{Cc}$"));
        assert!(!has_unsupported_unicode_property_escape(r"^[0-9a-f]{32}$"));
        assert!(!has_unsupported_unicode_property_escape(r"\p"));
        assert!(!has_unsupported_unicode_property_escape("trailing\\"));
    }

    fn normalize(schema: Value) -> Value {
        normalize_claude_tool_input_schema(Some(&schema))
    }

    #[test]
    fn non_object_schemas_become_empty_objects() {
        let empty = r#"{"type":"object","properties":{}}"#;
        assert_eq!(normalize_claude_tool_input_schema(None).to_string(), empty);
        assert_eq!(normalize(json!(null)).to_string(), empty);
        assert_eq!(normalize(json!([1])).to_string(), empty);
        assert_eq!(
            normalize(json!({"z": 1, "properties": "x"})).to_string(),
            r#"{"properties":{},"type":"object","z":1}"#
        );
    }

    #[test]
    fn root_unions_merge_into_the_root() {
        let schema = json!({
            "type": "object",
            "properties": {"b": {"type": "string"}},
            "required": ["b"],
            "anyOf": [
                {"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "number"}}},
                {"type": "string", "properties": {"skipped": {}}}
            ],
            "allOf": [
                {"type": ["null", "object"], "properties": {"c": {}}, "required": ["c", "b", null]},
                {"required": "not a list", "properties": {"d": {}}},
                null
            ]
        });
        assert_eq!(
            normalize(schema).to_string(),
            r#"{"properties":{"a":{"type":"integer"},"b":{"type":"string"},"c":{},"d":{}},"required":["b","c",""],"type":"object"}"#
        );
        // A union that isn't a list is dropped.
        assert_eq!(
            normalize(json!({"oneOf": {"properties": {"a": {}}}})).to_string(),
            r#"{"properties":{},"type":"object"}"#
        );
    }

    #[test]
    fn an_invalid_root_required_list_is_replaced() {
        let schema = json!({"required": ["a", 1], "allOf": [{"required": ["b"]}]});
        assert_eq!(
            normalize(schema).to_string(),
            r#"{"properties":{},"required":["b"],"type":"object"}"#
        );
        let schema = json!({"required": ["a", 1], "allOf": [{"required": []}]});
        assert_eq!(normalize(schema)["required"], json!(["a", 1]));
    }
}
