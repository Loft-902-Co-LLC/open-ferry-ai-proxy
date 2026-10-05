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
//!   ends in a `See: <name>` description. Parameters whose copies would
//!   outgrow the schema by more than [`COPY_ALLOWANCE`], or whose walk would
//!   go deeper than [`MAX_DEPTH`], aren't inlined, and get the permissive
//!   object schema below.
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
//! - Inlining stops once its copies would outgrow the schema by more than
//!   [`COPY_ALLOWANCE`] units, a unit being a value or a byte of a key or
//!   string; the function then gets the permissive object schema, as for
//!   parameters xAI rejects, and isn't strict. Upstream copies every
//!   reference, and since each use gets its own copy, a definition used
//!   twice by one used twice by another doubles with each level: seventeen
//!   levels turn 1.5 KB into 11.6 MB.
//! - Inlining also stops once the walk would go more than [`MAX_DEPTH`]
//!   levels deep, each object, array and reference followed counting one,
//!   with the same outcome. The walk recurses, and a chain of references
//!   costs nothing to copy, so a long one (100,000 links, about 3 MB) would
//!   overflow the thread's stack and abort the process; Go's stacks grow,
//!   so upstream inlines it. The bound also keeps the inlined schema within
//!   [`MAX_DEPTH`] levels of nesting (a cycle's hint, which copies the
//!   referring schema's other keywords as they are, counts with its own
//!   nesting), which leaves the request carrying it a few levels down
//!   (three under the body's `tools`, a few more under an `additional_tools`
//!   item or a namespace) well inside the 128 levels a JSON parser reads by
//!   default, and keeps every recursive step after it (sorting the keys,
//!   writing, dropping and checking the schema) shallow.

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

/// How much more inlining references may copy than the schema holds, in
/// [`size`]'s units; see the module docs.
pub(crate) const COPY_ALLOWANCE: usize = 1 << 18;

/// How deep the walk that inlines references may go, each object, array
/// and reference followed counting one level; the inlined schema nests no
/// deeper. Half the 128 levels `serde_json` parses, so the request around
/// the schema has room, and far beyond any real schema; see the module
/// docs.
pub(crate) const MAX_DEPTH: usize = 64;

/// What [`inline_local_refs`] made of a schema.
#[derive(Debug, PartialEq)]
pub(crate) enum Inlined {
    /// Left alone: there is no `"$ref"` in it, or it comes out the same.
    Unchanged,
    /// With its references inlined.
    Schema(Value),
    /// Not inlined, as its copies would outgrow it by more than
    /// [`COPY_ALLOWANCE`], or the walk would go deeper than [`MAX_DEPTH`].
    TooLarge,
}

/// `InlineLocalRefs`: `schema` with each local `$ref` (`#/…`) replaced by a
/// copy of what it points to, the referring schema's other keywords taking
/// precedence. A reference inside its own target becomes the target's
/// `type`, `nullable` and `description` with a `See: <name>` hint.
///
/// [`Inlined::Unchanged`] when upstream leaves the text alone: there is no
/// `"$ref"` in it, or the schema comes out the same. Like Go's encoder, the
/// result has its keys sorted.
pub(crate) fn inline_local_refs(schema: &Value) -> Inlined {
    let Ok(text) = serde_json::to_string(schema) else {
        return Inlined::Unchanged;
    };
    if !text.contains("\"$ref\"") {
        return Inlined::Unchanged;
    }
    let mut inliner = Inliner {
        root: schema,
        active: HashSet::new(),
        budget: size(schema, usize::MAX).saturating_add(COPY_ALLOWANCE),
    };
    let Some(mut resolved) = inliner.resolve(schema, 0) else {
        return Inlined::TooLarge;
    };
    sort_keys(&mut resolved);
    match serde_json::to_string(&resolved) {
        Ok(out) if out != text => Inlined::Schema(resolved),
        _ => Inlined::Unchanged,
    }
}

/// The walk of `resolveLocalRefs` over a schema, and what it may still
/// copy.
struct Inliner<'v> {
    /// The schema references point into.
    root: &'v Value,
    /// The references being inlined, each inside the one before.
    active: HashSet<String>,
    /// What may still be copied, in [`size`]'s units.
    budget: usize,
}

impl Inliner<'_> {
    /// Takes `cost` from the budget; `None` once it runs out.
    fn charge(&mut self, cost: usize) -> Option<()> {
        self.budget = self.budget.checked_sub(cost)?;
        Some(())
    }

    /// `resolveLocalRefs`: `value`, `depth` levels down the walk, with its
    /// references inlined, or `None` once the copies outgrow the budget or
    /// the walk would go deeper than [`MAX_DEPTH`]. Each value is paid for
    /// before it is copied.
    fn resolve(&mut self, value: &Value, depth: usize) -> Option<Value> {
        match value {
            Value::Array(items) => {
                let depth = enter(depth)?;
                self.charge(1)?;
                let items = items
                    .iter()
                    .map(|item| self.resolve(item, depth))
                    .collect::<Option<Vec<_>>>()?;
                Some(Value::Array(items))
            }
            Value::Object(node) => {
                if let Some(Value::String(reference)) = node.get("$ref")
                    && reference.starts_with("#/")
                    && let Some(target) = pointer(self.root, reference)
                {
                    if self.active.contains(reference) {
                        let fallback = cyclic_fallback(node, target, reference);
                        // The fallback is copied as it is, so it must fit
                        // in what is left of the depth too.
                        if depth.saturating_add(nesting(&fallback)) > MAX_DEPTH {
                            return None;
                        }
                        self.charge(size(&fallback, self.budget))?;
                        return Some(fallback);
                    }
                    let followed = enter(depth)?;
                    self.active.insert(reference.clone());
                    let resolved = self.resolve(target, followed);
                    self.active.remove(reference);
                    if let Value::Object(mut out) = resolved? {
                        // The other keywords join the target's object, one
                        // level below the reference followed, which the
                        // target's walk has already entered.
                        let inside = followed.saturating_add(1);
                        for (key, item) in node {
                            if key != "$ref" {
                                self.charge(key.len())?;
                                let item = self.resolve(item, inside)?;
                                out.insert(key.clone(), item);
                            }
                        }
                        return Some(Value::Object(out));
                    }
                }
                let depth = enter(depth)?;
                self.charge(1)?;
                let mut out = Map::new();
                for (key, item) in node {
                    self.charge(key.len())?;
                    let item = self.resolve(item, depth)?;
                    out.insert(key.clone(), item);
                }
                Some(Value::Object(out))
            }
            other => {
                self.charge(size(other, usize::MAX))?;
                Some(other.clone())
            }
        }
    }
}

/// The depth one level further down the walk than `depth`; `None` past
/// [`MAX_DEPTH`].
fn enter(depth: usize) -> Option<usize> {
    let next = depth.saturating_add(1);
    (next <= MAX_DEPTH).then_some(next)
}

/// How many levels of objects and arrays `value` nests, itself included;
/// none for a value that is neither.
fn nesting(value: &Value) -> usize {
    let mut deepest = 0;
    let mut stack = vec![(value, 1)];
    while let Some((value, level)) = stack.pop() {
        match value {
            Value::Object(fields) => stack.extend(fields.values().map(|child| (child, level + 1))),
            Value::Array(items) => stack.extend(items.iter().map(|child| (child, level + 1))),
            _ => continue,
        }
        deepest = deepest.max(level);
    }
    deepest
}

/// The size of `value`: one for each value in it, itself included, and
/// one for each byte of its keys and strings. Counting stops once past
/// `limit`.
fn size(value: &Value, limit: usize) -> usize {
    let mut total: usize = 0;
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        total = total.saturating_add(1);
        match value {
            Value::Object(fields) => {
                for (key, item) in fields {
                    total = total.saturating_add(key.len());
                    stack.push(item);
                }
            }
            Value::Array(items) => stack.extend(items),
            Value::String(text) => total = total.saturating_add(text.len()),
            _ => {}
        }
        if total > limit {
            break;
        }
    }
    total
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
