// Ported from CLIProxyAPI internal/util/gemini_schema.go (InlineLocalRefs,
// resolveLocalRefs, resolveJSONPointer, cyclicRefFallback, refName,
// mergeHint) and internal/runtime/executor/xai_executor_response.go
// (normalizeXAIObjectRootUnionBranchTypes, xaiSchemaTypeIsObjectOnly,
// isXAICodexAppAutomationUpdate, xaiFunctionParametersNeedSimplification)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Function parameter schemas made acceptable to xAI, which wants a
//! function's parameters to be an object schema and nothing else.
//!
//! - Local `$ref`s are inlined ([`inline_local_refs`]), each use getting its
//!   own copy with the referring schema's other keywords on top; a cycle
//!   ends in a `See: <name>` description.
//! - A root `anyOf` or `oneOf` branch with no `type` gets `"type":"object"`
//!   when the root is an object schema ([`type_root_union_branches`]).
//! - Parameters xAI would still reject, or that make it hang, are replaced
//!   by a permissive object schema ([`needs_simplification`],
//!   [`safe_function_parameters`]): a root union with a branch that isn't
//!   only an object, and the Codex app's `automation_update`.
//!
//! Deviations from upstream:
//! - Upstream inlines on the schema's text and keeps the result only if the
//!   text changed; here the schema is compared written compactly, so
//!   whitespace alone never counts as a change.
//! - An inlined schema is written without Go's `\u003c`-style escapes of
//!   `<`, `>` and `&`.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use crate::codex::tool_schema::sort_keys;
use crate::json::str_of;

/// The schema simplified parameters get (`xaiSafeFunctionParameters`):
/// any object, so the tool stays callable.
pub(crate) fn safe_function_parameters() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": true})
}

/// The Codex app's namespace (`xaiCodexAppNamespaceName`), whose
/// `automation_update` schema makes xAI hang.
const CODEX_APP_NAMESPACE: &str = "codex_app";

/// The Codex app tool xAI hangs on (`xaiAutomationUpdateToolName`).
const AUTOMATION_UPDATE: &str = "automation_update";

/// `InlineLocalRefs`: `schema` with each local `$ref` (`#/…`) replaced by a
/// copy of what it points to, the referring schema's other keywords taking
/// precedence. A reference inside its own target becomes the target's
/// `type`, `nullable` and `description` with a `See: <name>` hint.
///
/// `None` when upstream leaves the text alone: there is no `"$ref"` in it,
/// or the schema comes out the same. Like Go's encoder, the result has its
/// keys sorted.
pub(crate) fn inline_local_refs(schema: &Value) -> Option<Value> {
    let text = serde_json::to_string(schema).ok()?;
    if !text.contains("\"$ref\"") {
        return None;
    }
    let mut resolved = resolve(schema, schema, &mut HashSet::new());
    sort_keys(&mut resolved);
    let out = serde_json::to_string(&resolved).ok()?;
    (out != text).then_some(resolved)
}

/// `resolveLocalRefs`.
fn resolve(root: &Value, value: &Value, active: &mut HashSet<String>) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve(root, item, active))
                .collect(),
        ),
        Value::Object(node) => {
            if let Some(Value::String(reference)) = node.get("$ref")
                && reference.starts_with("#/")
                && let Some(target) = pointer(root, reference)
            {
                if active.contains(reference) {
                    return cyclic_fallback(node, target, reference);
                }
                active.insert(reference.clone());
                let resolved = resolve(root, target, active);
                active.remove(reference);
                if let Value::Object(mut out) = resolved {
                    for (key, item) in node {
                        if key != "$ref" {
                            out.insert(key.clone(), resolve(root, item, active));
                        }
                    }
                    return Value::Object(out);
                }
            }
            Value::Object(
                node.iter()
                    .map(|(key, item)| (key.clone(), resolve(root, item, active)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// `resolveJSONPointer`: what a `#/…` reference points to in `root`.
fn pointer<'v>(root: &'v Value, reference: &str) -> Option<&'v Value> {
    let path = reference.strip_prefix("#/").unwrap_or(reference);
    path.split('/').try_fold(root, |current, raw| {
        let part = raw.replace("~1", "/").replace("~0", "~");
        match current {
            Value::Object(object) => object.get(&part),
            Value::Array(items) => part
                .parse::<i64>()
                .ok()
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| items.get(index)),
            _ => None,
        }
    })
}

/// `cyclicRefFallback`: what a reference inside its own target becomes.
fn cyclic_fallback(node: &Map<String, Value>, target: &Value, reference: &str) -> Value {
    let mut out = Map::new();
    if let Value::Object(target) = target {
        for key in ["type", "nullable", "description"] {
            if let Some(value) = target.get(key) {
                out.insert(key.to_owned(), value.clone());
            }
        }
    }
    for (key, value) in node {
        if key != "$ref" {
            out.insert(key.clone(), value.clone());
        }
    }
    let hint = format!("See: {}", ref_name(reference));
    let description = match out.get("description") {
        Some(Value::String(description)) if !description.is_empty() => {
            merge_hint(description, &hint)
        }
        _ => hint,
    };
    out.insert("description".to_owned(), Value::String(description));
    Value::Object(out)
}

/// `refName`: the last part of a reference, with JSON Pointer escapes
/// decoded. (The schema cleaner in `open-ferry-translate` has the same
/// private helper.)
fn ref_name(reference: &str) -> String {
    match reference.rfind('/') {
        Some(index) if index + 1 < reference.len() => reference
            .get(index + 1..)
            .unwrap_or_default()
            .replace("~1", "/")
            .replace("~0", "~"),
        _ => reference.to_owned(),
    }
}

/// `mergeHint`: `hint` added to a description in parentheses, unless the
/// description already has it.
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

/// `normalizeXAIObjectRootUnionBranchTypes`: when a tool's parameters are an
/// object schema, gives each root `anyOf` and `oneOf` branch that is an
/// object with no `type` or `$ref` the type `object`. Returns whether any
/// branch changed.
pub(crate) fn type_root_union_branches(tool: &mut Value) -> bool {
    let Some(Value::Object(parameters)) = tool.get_mut("parameters") else {
        return false;
    };
    if parameters.get("type").and_then(Value::as_str) != Some("object") {
        return false;
    }
    let mut changed = false;
    for union in ["anyOf", "oneOf"] {
        let Some(Value::Array(branches)) = parameters.get_mut(union) else {
            continue;
        };
        for branch in branches {
            if let Value::Object(branch) = branch
                && !branch.contains_key("type")
                && !branch.contains_key("$ref")
            {
                branch.insert("type".to_owned(), Value::from("object"));
                changed = true;
            }
        }
    }
    changed
}

/// `xaiSchemaTypeIsObjectOnly`: `"object"`, or a non-empty list of only
/// `"object"`, in any case and with spaces around.
fn type_is_object_only(schema_type: Option<&Value>) -> bool {
    let is_object = |value: &Value| {
        value
            .as_str()
            .is_some_and(|text| crate::json::eq_fold(text.trim(), "object"))
    };
    match schema_type {
        Some(Value::String(_)) => schema_type.is_some_and(is_object),
        Some(Value::Array(types)) => !types.is_empty() && types.iter().all(is_object),
        _ => false,
    }
}

/// `isXAICodexAppAutomationUpdate`: the Codex app's `automation_update`,
/// in its namespace or flattened into one name, with or without `mcp__`.
fn is_codex_app_automation_update(tool_name: &str, namespace: &str) -> bool {
    let namespace = namespace.trim();
    let namespace = namespace.strip_prefix("mcp__").unwrap_or(namespace);
    let tool = tool_name.trim();
    let tool = tool.strip_prefix("mcp__").unwrap_or(tool);
    let eq = crate::json::eq_fold;
    if eq(tool, AUTOMATION_UPDATE)
        && (eq(namespace, CODEX_APP_NAMESPACE) || eq(namespace, "codex_apps"))
    {
        return true;
    }
    eq(tool, &format!("{CODEX_APP_NAMESPACE}__{AUTOMATION_UPDATE}"))
        || eq(tool, &format!("codex_apps__{AUTOMATION_UPDATE}"))
}

/// `xaiFunctionParametersNeedSimplification`: whether a function tool, or a
/// custom tool sent as one, in `namespace` (`""` for none) has parameters
/// xAI can't take: a root `anyOf` or `oneOf` branch that is a `$ref` or not
/// only an object, or (for a function) it is the Codex app's
/// `automation_update`.
pub(crate) fn needs_simplification(tool: &Value, namespace: &str) -> bool {
    let tool_type = str_of(tool.get("type"));
    let tool_type = tool_type.trim();
    let is_function = crate::json::eq_fold(tool_type, "function");
    if !is_function && !crate::json::eq_fold(tool_type, "custom") {
        return false;
    }
    if is_function && is_codex_app_automation_update(&str_of(tool.get("name")), namespace) {
        return true;
    }
    let parameters = tool.get("parameters");
    ["anyOf", "oneOf"].into_iter().any(|union| {
        match parameters.and_then(|parameters| parameters.get(union)) {
            Some(Value::Array(branches)) => branches.iter().any(|branch| {
                branch.get("$ref").is_some() || !type_is_object_only(branch.get("type"))
            }),
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests;
