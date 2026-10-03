// Ported from CLIProxyAPI internal/runtime/executor/helps/codex_tool_schema.go
// and HasUnsupportedUnicodePropertyEscape, SchemaMapKeywords and
// SchemaValueKeywords in internal/util/claude_schema.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tool schemas Codex would refuse, made acceptable
//! (`NormalizeCodexToolSchemas`).
//!
//! Two changes, to `function` and `custom` tools, also inside `namespace`
//! tools:
//! - a `pattern` (or a `patternProperties` key) with a `\p{..}`, `\P{..}` or
//!   `\0` escape is removed, as Codex's validator rejects them; the client
//!   still checks its own input;
//! - a property's `oneOf` or `anyOf` of eight or more `const` branches
//!   becomes an `enum`, or is dropped when an `enum` already says the same.
//!
//! Integer types are left alone, as Codex validates its own tools' schemas
//! strictly.
//!
//! Deviations from upstream:
//! - Edits are made on parsed JSON, so the tools are written again rather
//!   than spliced into the original bytes.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::json::str_of;

/// How many branches a union needs before it is turned into an enum.
const COMPLEX_UNION_BRANCHES: usize = 8;

/// Keywords whose values are maps of subschemas (`SchemaMapKeywords`, but
/// for `patternProperties`, which is handled on its own).
const SCHEMA_MAP_KEYWORDS: [&str; 5] = [
    "properties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];

/// Keywords whose values are a subschema or a list of them
/// (`SchemaValueKeywords`).
const SCHEMA_VALUE_KEYWORDS: [&str; 16] = [
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

/// Normalizes the schemas of the body's `tools`. Returns whether anything
/// changed.
pub(crate) fn normalize_tool_schemas(body: &mut Value) -> bool {
    match body.get_mut("tools") {
        Some(Value::Array(tools)) => normalize_tool_list(tools),
        _ => false,
    }
}

fn normalize_tool_list(tools: &mut [Value]) -> bool {
    let mut changed = false;
    for tool in tools {
        changed |= normalize_tool(tool);
    }
    changed
}

fn normalize_tool(tool: &mut Value) -> bool {
    let tool_type = str_of(tool.get("type"));
    if tool_type == "namespace" {
        return match tool.get_mut("tools") {
            Some(Value::Array(tools)) => normalize_tool_list(tools),
            _ => false,
        };
    }
    if tool_type != "function" && tool_type != "custom" {
        return false;
    }
    let name = str_of(tool.get("name"));
    let changed = match tool.get_mut("parameters") {
        Some(params @ Value::Object(_)) => normalize_parameters(params),
        _ => false,
    };
    if changed {
        tracing::debug!("codex: normalized schema for tool {name} to avoid upstream abort");
    }
    changed
}

fn normalize_parameters(params: &mut Value) -> bool {
    let mut changed = false;
    if strip_incompatible_patterns(params) {
        // Upstream re-encodes the schema from a Go map, which sorts its keys.
        sort_keys(params);
        changed = true;
    }
    if let Some(Value::Object(properties)) = params.get_mut("properties") {
        for property in properties.values_mut() {
            changed |= normalize_property(property);
        }
    }
    changed
}

/// Whether a regular expression has an escape strict validators reject:
/// `\p{`, `\P{` or `\0` (`HasUnsupportedUnicodePropertyEscape`).
fn has_unsupported_escape(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes.get(i) != Some(&b'\\') {
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            None => break,
            Some(b'p' | b'P') if bytes.get(i + 2) == Some(&b'{') => return true,
            Some(b'0') => return true,
            Some(_) => i += 2,
        }
    }
    false
}

/// Removes rejected patterns from the schema and the subschemas under its
/// schema keywords, leaving data such as `default` or `enum` alone
/// (`stripIncompatiblePatterns`). Returns whether anything was removed.
fn strip_incompatible_patterns(value: &mut Value) -> bool {
    let mut changed = false;
    match value {
        Value::Object(schema) => {
            if matches!(schema.get("pattern"), Some(Value::String(pattern)) if has_unsupported_escape(pattern))
            {
                schema.shift_remove("pattern");
                changed = true;
            }
            if let Some(Value::Object(pattern_properties)) = schema.get_mut("patternProperties") {
                pattern_properties.retain(|key, subschema| {
                    if has_unsupported_escape(key) {
                        changed = true;
                        return false;
                    }
                    changed |= strip_incompatible_patterns(subschema);
                    true
                });
            }
            for key in SCHEMA_MAP_KEYWORDS {
                if let Some(Value::Object(subschemas)) = schema.get_mut(key) {
                    for subschema in subschemas.values_mut() {
                        changed |= strip_incompatible_patterns(subschema);
                    }
                }
            }
            for key in SCHEMA_VALUE_KEYWORDS {
                match schema.get_mut(key) {
                    Some(subschema @ Value::Object(_)) => {
                        changed |= strip_incompatible_patterns(subschema);
                    }
                    Some(Value::Array(subschemas)) => {
                        for subschema in subschemas {
                            changed |= strip_incompatible_patterns(subschema);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                changed |= strip_incompatible_patterns(item);
            }
        }
        _ => {}
    }
    changed
}

/// Sorts every object's keys, as Go writes a map.
fn sort_keys(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<(String, Value)> = std::mem::take(object).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, child) in &mut entries {
                sort_keys(child);
            }
            *object = entries.into_iter().collect::<Map<String, Value>>();
        }
        Value::Array(items) => items.iter_mut().for_each(sort_keys),
        _ => {}
    }
}

/// Turns a property's large union of distinct `const` branches into an
/// `enum` (`normalizeCodexPropertySchema`).
fn normalize_property(property: &mut Value) -> bool {
    let Value::Object(object) = property else {
        return false;
    };
    let union_name = match (object.contains_key("oneOf"), object.contains_key("anyOf")) {
        (true, false) => "oneOf",
        (false, true) => "anyOf",
        // Both together are a compound constraint, kept as it is.
        _ => return false,
    };
    let Some(Value::Array(branches)) = object.get(union_name) else {
        return false;
    };
    if branches.len() < COMPLEX_UNION_BRANCHES {
        return false;
    }
    let mut values = Vec::with_capacity(branches.len());
    let mut keys = Vec::with_capacity(branches.len());
    let mut seen = HashSet::with_capacity(branches.len());
    for branch in branches {
        let Some((key, value)) = pure_const_branch(branch) else {
            return false;
        };
        // A repeated value breaks oneOf's exclusivity; keep the schema.
        if !seen.insert(key.clone()) {
            return false;
        }
        keys.push(key);
        values.push(value.clone());
    }
    if values.is_empty() {
        return false;
    }
    if let Some(Value::Array(existing)) = object.get("enum") {
        let Some(existing_keys) = existing
            .iter()
            .map(canonical_key)
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        if !equal_canonical_sets(&existing_keys, &keys) {
            return false;
        }
        object.shift_remove(union_name);
        return true;
    }
    object.insert("enum".to_owned(), Value::Array(values));
    object.shift_remove(union_name);
    true
}

/// A branch's `const` and its canonical key, when the branch has nothing
/// but `const`, `description` and `title` (`isPureConstBranch`).
fn pure_const_branch(branch: &Value) -> Option<(String, &Value)> {
    let object = branch.as_object()?;
    let value = object.get("const")?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "const" | "description" | "title"))
    {
        return None;
    }
    Some((canonical_key(value)?, value))
}

/// A key equal for JSON values that are equal: numbers compare as rationals
/// and types stay apart (`canonicalJSONValueKey`).
fn canonical_key(value: &Value) -> Option<String> {
    Some(match value {
        Value::String(text) => format!("s:{text}"),
        Value::Number(number) => format!("n:{}", number_key(&number.to_string())),
        Value::Bool(true) => "b:true".to_owned(),
        Value::Bool(false) => "b:false".to_owned(),
        Value::Null => "null".to_owned(),
        _ => return None,
    })
}

/// A JSON number literal as significant digits and a power of ten, equal
/// for literals of the same value, as upstream's `big.Rat` keys are.
fn number_key(raw: &str) -> String {
    let raw = raw.trim();
    let (negative, rest) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let (mantissa, exponent) = rest.split_once(['e', 'E']).unwrap_or((rest, "0"));
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let all_digits = |text: &str| text.bytes().all(|b| b.is_ascii_digit());
    let Ok(exponent) = exponent.parse::<i128>() else {
        return raw.to_owned();
    };
    if (int_part.is_empty() && frac_part.is_empty())
        || !all_digits(int_part)
        || !all_digits(frac_part)
    {
        return raw.to_owned();
    }
    let digits = format!("{int_part}{frac_part}");
    let digits = digits.trim_start_matches('0');
    let significant = digits.trim_end_matches('0');
    if significant.is_empty() {
        return "0".to_owned();
    }
    let len = |text: &str| i128::try_from(text.len()).unwrap_or(i128::MAX);
    let exponent = exponent
        .saturating_sub(len(frac_part))
        .saturating_add(len(digits) - len(significant));
    let sign = if negative { "-" } else { "" };
    format!("{sign}{significant}e{exponent}")
}

/// Whether two lists hold the same keys, without repeats in the first
/// (`equalCanonicalSets`).
fn equal_canonical_sets(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let set_a: HashSet<&String> = a.iter().collect();
    b.iter().all(|key| set_a.contains(key)) && set_a.len() == a.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{exists, get, str_at};
    use serde_json::json;

    fn normalized(body: Value) -> Value {
        let mut body = body;
        normalize_tool_schemas(&mut body);
        body
    }

    fn tool_with(properties: Value) -> Value {
        json!({"model": "gpt-5.5", "tools": [{
            "type": "function",
            "name": "t",
            "parameters": {"type": "object", "properties": properties},
        }]})
    }

    fn consts(values: &[Value]) -> Value {
        Value::Array(values.iter().map(|value| json!({"const": value})).collect())
    }

    fn strings(values: &[&str]) -> Vec<Value> {
        values.iter().map(|value| Value::from(*value)).collect()
    }

    fn enum_len(value: &Value, path: &str) -> usize {
        get(value, path)
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    }

    #[test]
    fn simplifies_complex_one_of_and_keeps_the_rest() {
        let names = [
            "p.list",
            "m.list",
            "s.list",
            "s.create",
            "s.send",
            "s.fork",
            "s.status",
            "s.messages",
            "sch.list",
            "sch.create",
            "sch.run",
            "sch.delete",
            "sch.toggle",
        ];
        let one_of: Vec<Value> = names
            .iter()
            .map(|name| json!({"const": name, "description": format!("Do {name}")}))
            .collect();
        let out = normalized(json!({"model": "gpt-5.5", "tools": [{
            "type": "function",
            "name": "t1",
            "description": "test tool",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": names, "oneOf": one_of, "description": "Action to perform"},
                    "target": {"type": "string", "description": "Target ID"},
                },
                "required": ["action"],
            },
        }]}));
        let tool = &out["tools"][0];
        assert!(!exists(tool, "parameters.properties.action.oneOf"));
        assert_eq!(str_at(tool, "parameters.properties.action.type"), "string");
        assert_eq!(enum_len(tool, "parameters.properties.action.enum"), 13);
        assert_eq!(str_at(tool, "parameters.properties.target.type"), "string");
        assert_eq!(str_at(tool, "parameters.required.0"), "action");
        assert_eq!(str_at(tool, "name"), "t1");
        assert_eq!(tool["strict"], true);
    }

    #[test]
    fn dotted_and_colon_property_names_stay_whole() {
        let digits = strings(&["1", "2", "3", "4", "5", "6", "7", "8"]);
        for name in ["my.action", ":action"] {
            let out = normalized(tool_with(json!({
                name: {"type": "string", "enum": digits, "oneOf": consts(&digits)},
            })));
            let property = &out["tools"][0]["parameters"]["properties"][name];
            assert!(property.is_object(), "{out}");
            assert!(property.get("oneOf").is_none());
            assert_eq!(property["type"], "string");
            assert_eq!(
                out["tools"][0]["parameters"]["properties"]
                    .as_object()
                    .map(Map::len),
                Some(1)
            );
        }
    }

    #[test]
    fn numeric_duplicates_are_left_alone() {
        let values = [
            json!(1),
            serde_json::from_str("1.0").unwrap(),
            json!(2),
            json!(3),
            json!(4),
            json!(5),
            json!(6),
            json!(7),
        ];
        let out = normalized(tool_with(
            json!({"val": {"type": "number", "oneOf": consts(&values)}}),
        ));
        assert!(exists(&out, "tools.0.parameters.properties.val.oneOf"));
    }

    #[test]
    fn large_integers_keep_their_digits() {
        let large: Value = serde_json::from_str("9007199254740993").unwrap();
        let values = [
            large,
            json!(1),
            json!(2),
            json!(3),
            json!(4),
            json!(5),
            json!(6),
            json!(7),
        ];
        let out = normalized(tool_with(
            json!({"id": {"type": "integer", "oneOf": consts(&values)}}),
        ));
        assert!(!exists(&out, "tools.0.parameters.properties.id.oneOf"));
        assert_eq!(
            get(&out, "tools.0.parameters.properties.id.enum.0")
                .map(Value::to_string)
                .as_deref(),
            Some("9007199254740993")
        );
    }

    #[test]
    fn string_duplicates_are_left_alone() {
        // Upstream writes the second "a" as a JSON escape; parsed, the two are equal.
        let values = strings(&["a", "a", "c", "d", "e", "f", "g", "h"]);
        let out = normalized(tool_with(
            json!({"val": {"type": "string", "oneOf": consts(&values)}}),
        ));
        assert!(exists(&out, "tools.0.parameters.properties.val.oneOf"));
    }

    #[test]
    fn types_stay_apart_when_comparing_with_an_enum() {
        let numbers: Vec<Value> = (1..=8).map(Value::from).collect();
        let out = normalized(tool_with(json!({"val": {
            "enum": strings(&["1", "2", "3", "4", "5", "6", "7", "8"]),
            "oneOf": consts(&numbers),
        }})));
        assert!(exists(&out, "tools.0.parameters.properties.val.oneOf"));
    }

    #[test]
    fn both_one_of_and_any_of_are_left_alone() {
        let first = strings(&["1", "2", "3", "4", "5", "6", "7", "8"]);
        let second = strings(&["5", "6", "7", "8", "9", "10", "11", "12"]);
        let out = normalized(tool_with(
            json!({"val": {"oneOf": consts(&first), "anyOf": consts(&second)}}),
        ));
        assert!(exists(&out, "tools.0.parameters.properties.val.oneOf"));
        assert!(exists(&out, "tools.0.parameters.properties.val.anyOf"));
    }

    #[test]
    fn migrates_const_branches_to_an_enum() {
        let values = strings(&["m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "m9", "m10"]);
        let out = normalized(tool_with(
            json!({"mode": {"type": "string", "oneOf": consts(&values)}}),
        ));
        assert!(!exists(&out, "tools.0.parameters.properties.mode.oneOf"));
        assert_eq!(
            enum_len(&out, "tools.0.parameters.properties.mode.enum"),
            10
        );
        // anyOf works the same way.
        let out = normalized(tool_with(json!({"mode": {"anyOf": consts(&values)}})));
        assert_eq!(
            out["tools"][0]["parameters"]["properties"]["mode"],
            json!({"enum": values})
        );
    }

    #[test]
    fn a_different_enum_is_left_alone() {
        let values = strings(&["1", "2", "3", "4", "5", "6", "7", "8"]);
        let out = normalized(tool_with(json!({"status": {
            "type": "string",
            "enum": strings(&["1", "2", "3", "4", "5", "6", "7", "8", "extra"]),
            "oneOf": consts(&values),
        }})));
        assert!(exists(&out, "tools.0.parameters.properties.status.oneOf"));
    }

    #[test]
    fn non_const_unions_are_left_alone() {
        let out = normalized(tool_with(json!({"data": {"oneOf": [
            {"type": "string", "pattern": "^[a-z]+$"},
            {"type": "number", "minimum": 0},
            {"type": "boolean"},
            {"type": "null"},
            {"type": "array"},
            {"type": "object"},
            {"type": "integer"},
            {"type": "string", "pattern": "^[0-9]+$"},
        ]}})));
        assert!(exists(&out, "tools.0.parameters.properties.data.oneOf"));
    }

    #[test]
    fn simple_tools_are_left_alone() {
        let mut body = json!({"model": "gpt-5.5", "tools": [{
            "type": "function",
            "name": "lookup",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false,
            },
        }]});
        let before = body.clone();
        assert!(!normalize_tool_schemas(&mut body));
        assert_eq!(body, before);
    }

    #[test]
    fn namespace_tools_are_normalized() {
        let values = strings(&["1", "2", "3", "4", "5", "6", "7", "8"]);
        let out = normalized(json!({"model": "gpt-5.5", "tools": [{
            "type": "namespace",
            "name": "mcp",
            "tools": [{
                "type": "function",
                "name": "complex_tool",
                "parameters": {"type": "object", "properties": {"action": {"type": "string", "oneOf": consts(&values)}}},
            }],
        }]}));
        assert!(!exists(
            &out,
            "tools.0.tools.0.parameters.properties.action.oneOf"
        ));
        assert_eq!(
            enum_len(&out, "tools.0.tools.0.parameters.properties.action.enum"),
            8
        );
    }

    #[test]
    fn strips_unicode_property_escape_patterns() {
        let input = json!({"model": "gpt-5.6", "tools": [{
            "type": "function",
            "name": "Artifact",
            "parameters": {
                "type": "object",
                "properties": {
                    "field": {
                        "type": "string",
                        "description": "field to edit",
                        "pattern": r#"^(?!__.*__$)[^\p{Cc}\p{Cf}\p{Zl}\p{Zp}"\\./[\]]{1,200}$"#,
                    },
                    "asset_id": {"type": "string", "pattern": "^[0-9a-f]{32}$"},
                },
                "required": ["field"],
            },
        }]});
        let out = normalized(input);
        let params = &out["tools"][0]["parameters"];
        assert!(!exists(params, "properties.field.pattern"));
        assert_eq!(str_at(params, "properties.field.type"), "string");
        assert_eq!(
            str_at(params, "properties.asset_id.pattern"),
            "^[0-9a-f]{32}$"
        );
        // Keys come sorted, as upstream re-encodes the schema from a map.
        let keys: Vec<&String> = params.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["properties", "required", "type"]);
        assert_eq!(normalized(out.clone()), out);
    }

    #[test]
    fn strips_octal_nul_pattern_escapes() {
        let out = normalized(json!({"model": "gpt-5.6", "tools": [{
            "type": "function",
            "name": "Artifact",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_paths": {
                        "type": "array",
                        "minItems": 1,
                        "items": {"type": "string", "minLength": 1, "maxLength": 1024, "pattern": r"^[^\0]*$"},
                    },
                    "asset_id": {"type": "string", "pattern": "^[0-9a-f]{32}$"},
                    "hex_nul": {"type": "string", "pattern": r"^[^\x00]*$"},
                },
                "required": ["file_paths"],
            },
        }]}));
        let params = &out["tools"][0]["parameters"];
        assert!(!exists(params, "properties.file_paths.items.pattern"));
        assert_eq!(str_at(params, "properties.file_paths.items.type"), "string");
        assert_eq!(str_at(params, "properties.file_paths.items.minLength"), "1");
        assert_eq!(
            str_at(params, "properties.file_paths.items.maxLength"),
            "1024"
        );
        assert_eq!(str_at(params, "properties.file_paths.minItems"), "1");
        assert_eq!(
            str_at(params, "properties.asset_id.pattern"),
            "^[0-9a-f]{32}$"
        );
        assert_eq!(str_at(params, "properties.hex_nul.pattern"), r"^[^\x00]*$");
        assert_eq!(normalized(out.clone()), out);
    }

    #[test]
    fn keeps_pattern_keys_in_data() {
        let out = normalized(tool_with(json!({
            "regex_config": {
                "type": "object",
                "default": {"pattern": r"\p{L}+"},
                "enum": [{"pattern": r"\p{N}+"}],
            },
            "real_schema": {"type": "string", "pattern": r"\p{L}+"},
        })));
        let params = &out["tools"][0]["parameters"];
        assert!(!exists(params, "properties.real_schema.pattern"));
        assert_eq!(
            str_at(params, "properties.regex_config.default.pattern"),
            r"\p{L}+"
        );
        assert_eq!(
            str_at(params, "properties.regex_config.enum.0.pattern"),
            r"\p{N}+"
        );
    }

    #[test]
    fn covers_every_schema_keyword_location() {
        let out = normalized(json!({"model": "gpt-5.6", "tools": [{
            "type": "function",
            "name": "deep_tool",
            "parameters": {
                "type": "object",
                "$defs": {"custom_type": {"type": "string", "pattern": r"\p{L}+"}},
                "additionalProperties": {"type": "string", "pattern": r"\p{N}+"},
                "patternProperties": {"^s_": {"type": "string", "pattern": r"\p{M}+"}},
                "if": {"properties": {"flag": {"type": "string", "pattern": r"\p{P}+"}}},
                "then": {"properties": {"val": {"type": "string", "pattern": r"\p{S}+"}}},
                "else": {"properties": {"other": {"type": "string", "pattern": r"\p{Z}+"}}},
                "allOf": [{"pattern": r"\P{L}"}],
            },
        }]}));
        let params = &out["tools"][0]["parameters"];
        for path in [
            "$defs.custom_type.pattern",
            "additionalProperties.pattern",
            "patternProperties.^s_.pattern",
            "if.properties.flag.pattern",
            "then.properties.val.pattern",
            "else.properties.other.pattern",
            "allOf.0.pattern",
        ] {
            assert!(!exists(params, path), "{path}");
        }
        assert_eq!(str_at(params, "$defs.custom_type.type"), "string");
    }

    #[test]
    fn malformed_or_empty_parameters() {
        for body in [
            json!({"model": "gpt-5.6", "tools": [{"type": "function", "name": "t", "parameters": null}]}),
            json!({"model": "gpt-5.6", "tools": [{"type": "function", "name": "t", "parameters": "not_an_object"}]}),
            json!({"model": "gpt-5.6", "tools": [{"type": "function", "name": "t", "parameters": {"type": "object"}}]}),
            json!({"model": "gpt-5.6", "tools": []}),
            json!({"model": "gpt-5.6"}),
            json!({"tools": [null, 123, {"type": "namespace", "tools": "x"}]}),
            Value::Null,
        ] {
            let mut value = body.clone();
            assert!(!normalize_tool_schemas(&mut value));
            assert_eq!(value, body);
        }
    }

    #[test]
    fn json_unicode_escapes_dont_hide_patterns() {
        // Upstream's fast path looks for `\u` in the raw text; parsed, the
        // escapes are plain characters, so this only checks they're caught.
        let backslash = '\\';
        let raw = format!(
            r#"{{"tools":[{{"type":"function","name":"e","parameters":{{"type":"object","properties":{{
                "p1":{{"type":"string","pattern":"{b}u005c{b}u0070{{L}}+"}},
                "p2":{{"type":"string","pattern":"{b}u005cp{{Cc}}"}},
                "p3":{{"type":"string","pattern":"{b}u005c{b}u0050{{N}}+"}},
                "valid":{{"type":"string","pattern":"^[0-9a-f]{{32}}$"}}}}}}}}]}}"#,
            b = backslash
        );
        let out = normalized(serde_json::from_str(&raw).unwrap());
        let params = &out["tools"][0]["parameters"];
        for name in ["p1", "p2", "p3"] {
            assert!(
                !exists(params, &format!("properties.{name}.pattern")),
                "{name}"
            );
        }
        assert_eq!(str_at(params, "properties.valid.pattern"), "^[0-9a-f]{32}$");
    }

    #[test]
    fn pattern_properties_keys() {
        let out = normalized(json!({"tools": [{
            "type": "function",
            "name": "pattern_props_tool",
            "parameters": {
                "type": "object",
                "patternProperties": {
                    r"^\p{L}+$": {"type": "string"},
                    r"^\\p{L}+$": {"type": "string"},
                    "^[a-z]+$": {"type": "number"},
                },
            },
        }]}));
        let keys: Vec<&String> = out["tools"][0]["parameters"]["patternProperties"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        // An escaped backslash before `p` is a literal one, which is fine.
        assert_eq!(keys, [r"^[a-z]+$", r"^\\p{L}+$"]);
    }

    #[test]
    fn integer_types_are_left_alone() {
        let out = normalized(json!({"tools": [{
            "type": "function",
            "name": "exec_command",
            "parameters": {"type": "object", "properties": {"yield_time_ms": {"type": "number"}}},
        }]}));
        assert_eq!(
            str_at(&out, "tools.0.parameters.properties.yield_time_ms.type"),
            "number"
        );
    }

    #[test]
    fn escapes_and_number_keys() {
        assert!(has_unsupported_escape(r"[^\p{Cc}]"));
        assert!(has_unsupported_escape(r"\P{L}+"));
        assert!(has_unsupported_escape(r"a\0"));
        assert!(!has_unsupported_escape(r"^\\p{Cc}$"));
        assert!(!has_unsupported_escape(r"\p"));
        assert!(!has_unsupported_escape("trailing\\"));
        assert_eq!(number_key("1"), number_key("1.0"));
        assert_eq!(number_key("1.50"), number_key("15e-1"));
        assert_eq!(number_key("100"), number_key("1E2"));
        assert_eq!(number_key("0"), number_key("-0.000"));
        assert_ne!(number_key("1"), number_key("-1"));
        assert_ne!(
            number_key("9007199254740993"),
            number_key("9007199254740992")
        );
    }
}
