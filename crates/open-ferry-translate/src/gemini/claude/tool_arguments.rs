// Ported from CLIProxyAPI internal/util/translator.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Restores Claude tool names and removes schema-normalization placeholders.
//!
//! Antigravity's post-translation schema sanitization is not simulated: Gemini
//! does not run that pass. Arguments are represented as JSON values, as in the
//! rest of this translator, so their text is serialized compactly. Cleaning
//! stops after 128 argument levels to bound recursive walks of recursive schemas;
//! deeper data is preserved.

use crate::common::gemini::sanitize_gemini_function_name;
use crate::common::tool_names::canonical_tool_name;
use crate::json::{path, str_of};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn tool_name(tool: &Value) -> String {
    let name = str_of(tool.get("name"));
    if name.trim().is_empty() {
        str_of(path(tool, "function.name")).trim().to_owned()
    } else {
        name.trim().to_owned()
    }
}

pub(super) fn client_name(request: &Value, name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let names: BTreeSet<_> = request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(tool_name)
        .filter(|n| !n.is_empty() && canonical_tool_name(n) == canonical_tool_name(name))
        .collect();
    if names.contains(name) || names.len() != 1 {
        name.to_owned()
    } else {
        names.into_iter().next().unwrap()
    }
}

// None means unknown or ambiguous; Some(None) means a declared tool with no schema.
fn original_schema<'a>(request: &'a Value, name: &str) -> Option<Option<&'a Value>> {
    if name.is_empty() {
        return None;
    }
    let tools = request.get("tools")?.as_array()?;
    let exact: Vec<_> = tools.iter().filter(|t| tool_name(t) == name).collect();
    let candidates: Vec<_> = if exact.is_empty() {
        let names: BTreeSet<_> = tools
            .iter()
            .map(tool_name)
            .filter(|n| canonical_tool_name(n) == canonical_tool_name(name))
            .collect();
        if names.len() != 1 {
            return None;
        }
        tools
            .iter()
            .filter(|t| names.contains(&tool_name(t)))
            .collect()
    } else {
        exact
    };
    let schemas: Vec<_> = candidates
        .iter()
        .map(|t| {
            t.get("input_schema")
                .or_else(|| t.get("parameters"))
                .or_else(|| path(t, "function.parameters"))
        })
        .collect();
    let first = *schemas.first()?;
    schemas.iter().all(|s| *s == first).then_some(first)
}

#[derive(Default, Clone)]
struct Info<'a> {
    properties: BTreeMap<String, Vec<&'a Value>>,
    required: BTreeSet<String>,
    allows: bool,
    forbids: bool,
    pattern: bool,
    complex: bool,
    unsafe_complex: bool,
    object: bool,
    branches: Vec<Info<'a>>,
}
impl<'a> Info<'a> {
    fn merge(&mut self, other: Self) {
        for (key, values) in other.properties {
            for value in values {
                unique(self.properties.entry(key.clone()).or_default(), value);
            }
        }
        self.required.extend(other.required);
        self.branches.extend(other.branches);
        self.allows |= other.allows;
        self.forbids |= other.forbids;
        self.pattern |= other.pattern;
        self.complex |= other.complex;
        self.unsafe_complex |= other.unsafe_complex;
        self.object |= other.object;
    }
    fn dynamic(&self) -> bool {
        self.pattern || (self.allows && !self.forbids) || self.branches.iter().any(Self::dynamic)
    }
    fn matches(&self, current: &Value) -> bool {
        self.required.iter().all(|k| current.get(k).is_some())
    }
    fn branch_accepts(&self, key: &str, current: &Value) -> bool {
        self.pattern
            || (!self.forbids && (self.matches(current) || self.allows))
            || self.parent_accepts(key, current)
    }
    fn parent_accepts(&self, key: &str, current: &Value) -> bool {
        self.branches
            .iter()
            .any(|b| !b.properties.contains_key(key) && b.branch_accepts(key, current))
    }
    fn parameterless(&self) -> bool {
        !self.complex
            && self.object
            && self.required.is_empty()
            && self.properties.is_empty()
            && !self.dynamic()
    }
}
fn unique<'a>(values: &mut Vec<&'a Value>, value: &'a Value) {
    if !values.contains(&value) {
        values.push(value);
    }
}
fn array<'a>(schema: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    schema
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}
fn branches(schema: &Value) -> Option<Vec<&Value>> {
    let mut out = Vec::new();
    for key in ["anyOf", "oneOf"] {
        if let Some(value) = schema.get(key) {
            for branch in value.as_array()? {
                if branch.get("type") == Some(&json!("null"))
                    || branch.get("type") == Some(&json!(["null"]))
                {
                    continue;
                }
                unique(&mut out, branch);
            }
        }
    }
    Some(out)
}
fn local_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in reference.strip_prefix("#/")?.split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        current = if let Some(map) = current.as_object() {
            map.get(&segment)?
        } else {
            // Upstream uses strconv.Atoi, allowing +0, -0 and leading zeros.
            let index = usize::try_from(segment.parse::<i64>().ok()?).ok()?;
            current.as_array()?.get(index)?
        };
    }
    Some(current)
}

fn analyze<'a>(schema: &'a Value, root: &'a Value) -> Info<'a> {
    analyze_depth(schema, root, 0, &mut BTreeSet::new())
}
fn analyze_depth<'a>(
    schema: &'a Value,
    root: &'a Value,
    depth: usize,
    visited: &mut BTreeSet<String>,
) -> Info<'a> {
    let mut info = Info::default();
    let Some(map) = schema.as_object().filter(|_| depth <= 16) else {
        info.complex = true;
        return info;
    };
    info.object = schema.get("type") == Some(&json!("object"))
        || array(schema, "type").any(|v| v == "object");
    const KNOWN: &str = "type description title properties required items additionalProperties patternProperties anyOf oneOf allOf $ref $defs definitions nullable minLength maxLength minimum maximum exclusiveMinimum exclusiveMaximum multipleOf pattern format default examples minItems maxItems maxProperties $schema $id id $comment deprecated readOnly writeOnly example contentEncoding contentMediaType";
    if map
        .keys()
        .any(|k| !k.starts_with("x-") && !KNOWN.split_whitespace().any(|known| known == k))
    {
        info.complex = true;
        info.unsafe_complex = true;
        return info;
    }
    let has_union = schema.get("anyOf").is_some() || schema.get("oneOf").is_some();
    if has_union {
        let Some(branches) = branches(schema).filter(|b| !b.is_empty()) else {
            info.complex = true;
            return info;
        };
        let multiple = branches.len() > 1;
        for branch in branches {
            let branch_info = analyze_depth(branch, root, depth + 1, visited);
            if multiple {
                info.branches.push(branch_info.clone());
            }
            info.merge(branch_info);
        }
        info.complex |= multiple;
    }
    if let Some(reference) = schema.get("$ref") {
        let reference = str_of(Some(reference)).into_owned();
        if !reference.starts_with("#/") || !visited.insert(reference.clone()) {
            info.complex = true;
            return info;
        }
        if let Some(target) = local_ref(root, &reference) {
            info.merge(analyze_depth(target, root, depth + 1, visited));
        } else {
            info.complex = true;
        }
        visited.remove(&reference);
    }
    if let Some(all) = schema.get("allOf") {
        if let Some(all) = all.as_array() {
            for branch in all {
                info.merge(analyze_depth(branch, root, depth + 1, visited));
            }
        } else {
            info.complex = true;
        }
    }
    info.pattern |= schema
        .get("patternProperties")
        .and_then(Value::as_object)
        .is_some_and(|p| !p.is_empty());
    info.allows |= info.pattern
        || schema
            .get("additionalProperties")
            .is_some_and(|v| v != &Value::Bool(false));
    info.forbids |= schema.get("additionalProperties") == Some(&Value::Bool(false));
    if let Some(properties) = schema.get("properties") {
        if let Some(properties) = properties.as_object() {
            info.object = true;
            for (key, value) in properties {
                unique(info.properties.entry(key.clone()).or_default(), value);
            }
        } else {
            info.complex = true;
        }
    }
    info.required
        .extend(array(schema, "required").map(|v| str_of(Some(v)).into_owned()));
    info
}

pub(super) fn parameterless(request: &Value, name: &str) -> bool {
    match original_schema(request, name) {
        Some(None) => true,
        Some(Some(s)) => analyze(s, s).parameterless(),
        None => false,
    }
}

// Derive upstream's request-wide disambiguated name when matching a sent schema.
fn sent_tool_name(original: &Value, target: &str) -> Option<String> {
    use sha2::{Digest as _, Sha256};
    let names: BTreeSet<_> = original
        .get("tools")?
        .as_array()?
        .iter()
        .map(tool_name)
        .filter(|name| !name.is_empty())
        .collect();
    let mut counts = BTreeMap::<String, usize>::new();
    for name in &names {
        *counts
            .entry(sanitize_gemini_function_name(name))
            .or_default() += 1;
    }
    let mut used = BTreeSet::new();
    for name in names {
        let base = sanitize_gemini_function_name(&name);
        let mut mapped = base.clone();
        if counts[&base] > 1 || used.contains(&base) {
            for attempt in 0u64.. {
                let digest = Sha256::digest(format!("{name}\0{attempt}").as_bytes());
                let suffix: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
                mapped = format!("{}_{}", &base[..base.len().min(51)], suffix);
                if !used.contains(&mapped) {
                    break;
                }
            }
        }
        if name == target {
            return Some(mapped);
        }
        used.insert(mapped);
    }
    None
}

fn translated_schemas<'a>(sent: &'a Value, original: &Value, name: &str) -> Vec<&'a Value> {
    let sent_name = sent_tool_name(original, name);
    let sanitized = sanitize_gemini_function_name(name);
    let mut exact = Vec::new();
    let mut canonical = BTreeMap::<String, Vec<&Value>>::new();
    for tools in [sent.get("tools"), path(sent, "request.tools")]
        .into_iter()
        .flatten()
    {
        for group in tools.as_array().into_iter().flatten() {
            for decl in array(group, "functionDeclarations") {
                let declared = str_of(decl.get("name")).trim().to_owned();
                let Some(schema) = decl
                    .get("parametersJsonSchema")
                    .or_else(|| decl.get("parameters"))
                    .filter(|s| s.is_object())
                else {
                    continue;
                };
                if declared == name || sent_name.as_deref() == Some(&declared) {
                    exact.push(schema);
                }
                if canonical_tool_name(&declared) == canonical_tool_name(name)
                    || declared == sanitized
                    || sanitize_gemini_function_name(&declared) == sanitized
                {
                    canonical.entry(declared).or_default().push(schema);
                }
            }
        }
    }
    if !exact.is_empty() {
        exact
    } else if canonical.len() == 1 {
        canonical.into_values().next().unwrap()
    } else {
        Vec::new()
    }
}

fn items<'a>(
    schema: &'a Value,
    root: &'a Value,
    depth: usize,
    visited: &mut BTreeSet<String>,
) -> Option<Vec<&'a Value>> {
    if depth > 16 || !schema.is_object() {
        return None;
    }
    let mut result = Vec::new();
    if let Some(reference) = schema.get("$ref") {
        let reference = str_of(Some(reference)).into_owned();
        if !reference.starts_with("#/") || !visited.insert(reference.clone()) {
            return None;
        }
        for item in items(local_ref(root, &reference)?, root, depth + 1, visited)? {
            unique(&mut result, item);
        }
        visited.remove(&reference);
    }
    if schema.get("anyOf").is_some() || schema.get("oneOf").is_some() {
        let branches = branches(schema)?;
        if branches.len() != 1 {
            return None;
        }
        for item in items(branches[0], root, depth + 1, visited)? {
            unique(&mut result, item);
        }
    }
    if let Some(all) = schema.get("allOf") {
        for branch in all.as_array()? {
            for item in items(branch, root, depth + 1, visited)? {
                unique(&mut result, item);
            }
        }
    }
    if let Some(item) = schema.get("items") {
        unique(&mut result, item);
    }
    Some(result)
}

fn candidate_accepts(infos: &[Info<'_>], key: Option<&str>, current: &Value) -> bool {
    infos.len() > 1
        && infos.iter().any(|info| {
            if info.complex {
                return false;
            }
            match key {
                Some(key) => {
                    !info.properties.contains_key(key)
                        && (info.pattern || (!info.forbids && info.matches(current)))
                }
                None => !info.forbids && info.matches(current),
            }
        })
}

fn injected(schema: &Value, key: &str, top: bool) -> bool {
    let info = analyze(schema, schema);
    if let Some(properties) = info.properties.get(key) {
        if array(schema, "required").any(|v| v == key) {
            return true;
        }
        if key == "reason"
            && properties.iter().any(|s| {
                s.get("description").and_then(Value::as_str)
                    == Some("Brief explanation of why you are calling this tool")
            })
        {
            return true;
        }
        if key == "_"
            && !top
            && properties
                .iter()
                .any(|s| s.get("type").and_then(Value::as_str) == Some("boolean"))
        {
            return true;
        }
    }
    ["anyOf", "oneOf"].iter().any(|k| {
        array(schema, k).any(|branch| {
            analyze(branch, branch).properties.contains_key(key)
                && array(branch, "required").any(|v| v == key)
        })
    })
}

fn clean_value(
    value: &mut Value,
    originals: &[&Value],
    translated: &[&Value],
    root: &Value,
    top: bool,
    depth: usize,
) {
    if depth > 128 || originals.is_empty() {
        return;
    }
    let infos: Vec<_> = originals.iter().map(|s| analyze(s, root)).collect();
    let mut info = Info::default();
    for item in &infos {
        info.merge(item.clone());
    }
    if info.unsafe_complex {
        return;
    }
    if let Some(values) = value.as_array_mut() {
        let mut original_items = Vec::new();
        let mut translated_items = Vec::new();
        for schema in originals {
            let Some(resolved) = items(schema, root, 0, &mut BTreeSet::new()) else {
                return;
            };
            if resolved.is_empty() {
                unique(&mut original_items, schema);
            }
            for item in resolved {
                unique(&mut original_items, item);
            }
        }
        for schema in translated {
            let Some(resolved) = items(schema, schema, 0, &mut BTreeSet::new()) else {
                return;
            };
            for item in resolved {
                unique(&mut translated_items, item);
            }
        }
        if !translated.is_empty() && translated_items.is_empty() {
            return;
        }
        for value in values {
            clean_value(
                value,
                &original_items,
                &translated_items,
                root,
                false,
                depth + 1,
            );
        }
        return;
    }
    if !value.is_object() || (!translated.is_empty() && originals.iter().any(|s| !s.is_object())) {
        return;
    }
    if translated.is_empty() {
        if info.complex || !info.object {
            return;
        }
    } else if info.complex
        && !originals
            .iter()
            .any(|s| s.get("anyOf").is_some() || s.get("oneOf").is_some())
    {
        return;
    }
    for (key, children) in &info.properties {
        if info.parent_accepts(key, value) || candidate_accepts(&infos, Some(key), value) {
            continue;
        }
        let mut translated_children = Vec::new();
        for schema in translated {
            if let Some(children) = analyze(schema, schema).properties.get(key) {
                for child in children {
                    unique(&mut translated_children, child);
                }
            }
        }
        if !translated.is_empty() && translated_children.is_empty() {
            continue;
        }
        if let Some(child) = value.get_mut(key) {
            clean_value(
                child,
                children,
                &translated_children,
                root,
                false,
                depth + 1,
            );
        }
    }
    let dynamic = info.dynamic() || infos.iter().any(Info::dynamic);
    for key in ["reason", "_"] {
        if info.properties.contains_key(key)
            || info.required.contains(key)
            || dynamic
            || info.parent_accepts(key, value)
            || candidate_accepts(&infos, None, value)
        {
            continue;
        }
        if translated.is_empty() || translated.iter().any(|schema| injected(schema, key, top)) {
            value.as_object_mut().unwrap().shift_remove(key);
        }
    }
    if !info.complex
        && infos.iter().all(|i| i.object)
        && info.required.is_empty()
        && info.properties.is_empty()
        && !dynamic
        && !candidate_accepts(&infos, None, value)
    {
        *value = json!({});
    }
}

pub(super) fn clean_arguments(args: &Value, name: &str, original: &Value, sent: &Value) -> Value {
    if !args.is_object() {
        return args.clone();
    }
    let Some(schema) = original_schema(original, name) else {
        return args.clone();
    };
    let Some(schema) = schema else {
        return json!({});
    };
    let mut args = args.clone();
    clean_value(
        &mut args,
        &[schema],
        &translated_schemas(sent, original, name),
        schema,
        true,
        0,
    );
    args
}

#[cfg(test)]
mod tests;
