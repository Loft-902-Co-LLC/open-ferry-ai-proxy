// Ported from CLIProxyAPI internal/util/translator.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Removes schema-normalization placeholders from Claude tool arguments.
//!
//! Closed objects use the upstream re-clean decision on every route, without
//! running an Antigravity request cleaner. Arguments use compact JSON values.
//! Schema equality retains key order but cannot distinguish whitespace or string
//! escape spellings discarded when requests are parsed. Cleaning
//! stops after 128 argument levels to bound recursive walks of recursive schemas;
//! deeper data is preserved.

use crate::common::gemini::sanitize_gemini_function_name;
use crate::common::tool_names::canonical_tool_name;
use crate::json::{path, str_of};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

mod budget;
mod call_analysis;
mod reclean;
mod schema_index;

use call_analysis::CallAnalysis;

fn tool_name(tool: &Value) -> String {
    if !budget::value(tool.get("name").unwrap_or(&Value::Null))
        || !budget::value(path(tool, "function.name").unwrap_or(&Value::Null))
    {
        return String::new();
    }
    let name = str_of(tool.get("name"));
    if name.trim().is_empty() {
        str_of(path(tool, "function.name")).trim().to_owned()
    } else {
        name.trim().to_owned()
    }
}

// The old resolver remains only as an oracle for the cached name policy.
#[cfg(test)]
fn legacy_client_name(request: &Value, name: &str) -> String {
    // Upstream's name map has no entry for an empty canonical name, so a
    // name such as `_` is never restored.
    if canonical_tool_name(name).is_empty() {
        return name.to_owned();
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
    let tools = request.get("tools")?;
    if !budget::value(tools) {
        return None;
    }
    let tools = tools.as_array()?;
    let exact: Vec<_> = tools.iter().filter(|t| tool_name(t) == name).collect();
    let candidates: Vec<_> = if exact.is_empty() {
        let names: BTreeSet<_> = tools
            .iter()
            .map(tool_name)
            .filter(|n| !n.is_empty() && canonical_tool_name(n) == canonical_tool_name(name))
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
    schemas
        .iter()
        .all(|s| s.map(budget::text) == first.map(budget::text))
        .then_some(first)
}

type Info<'a> = SchemaInfo<Schemas<'a>>;

#[derive(Default)]
struct SchemaInfo<S> {
    properties: BTreeMap<String, S>,
    required: BTreeSet<String>,
    allows: bool,
    forbids: bool,
    pattern: bool,
    complex: bool,
    unsafe_complex: bool,
    object: bool,
    branches: Vec<SchemaInfo<S>>,
}
impl Clone for Info<'_> {
    fn clone(&self) -> Self {
        if !self.charge_copy() {
            return Self::default();
        }
        Self {
            properties: self.properties.clone(),
            required: self.required.clone(),
            allows: self.allows,
            forbids: self.forbids,
            pattern: self.pattern,
            complex: self.complex,
            unsafe_complex: self.unsafe_complex,
            object: self.object,
            branches: self.branches.clone(),
        }
    }
}

impl<'a> Info<'a> {
    fn charge_copy(&self) -> bool {
        if !budget::enabled() {
            return true;
        }
        for (key, schemas) in &self.properties {
            if !budget::charge(key.len().saturating_add(32).saturating_mul(16)) {
                return false;
            }
            for text in &schemas.serialized {
                if !budget::charge(text.len().saturating_add(32)) {
                    return false;
                }
            }
        }
        self.required
            .iter()
            .all(|key| budget::charge(key.len().saturating_add(32).saturating_mul(16)))
            && budget::charge(self.branches.len().saturating_mul(32))
    }

    fn merge(&mut self, other: Self) {
        for (key, values) in other.properties {
            let entry = self.properties.entry(key).or_default();
            for value in values.values {
                unique(entry, value);
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
}

impl<S> SchemaInfo<S> {
    fn dynamic(&self) -> bool {
        budget::charge(1)
            && (self.pattern
                || (self.allows && !self.forbids)
                || self.branches.iter().any(Self::dynamic))
    }
    fn matches(&self, current: &Value) -> bool {
        self.required
            .iter()
            .all(|k| budget::charge(k.len().saturating_add(1)) && current.get(k).is_some())
    }
    fn branch_accepts(&self, key: &str, current: &Value) -> bool {
        self.pattern
            || (!self.forbids && (self.matches(current) || self.allows))
            || self.parent_accepts(key, current)
    }
    fn parent_accepts(&self, key: &str, current: &Value) -> bool {
        if !budget::charge(key.len().saturating_add(1).saturating_mul(16)) {
            return false;
        }
        self.branches
            .iter()
            .any(|b| !b.properties.contains_key(key) && b.branch_accepts(key, current))
    }
    fn parameterless(&self) -> bool {
        !self.complex
            && self.forbids
            && self.object
            && self.required.is_empty()
            && self.properties.is_empty()
            && !self.dynamic()
    }
}
// Keep first-seen order and cache the text compared by appendUniqueSchema.
#[derive(Default, Clone)]
struct Schemas<'a> {
    values: Vec<&'a Value>,
    serialized: HashSet<String>,
}
impl<'a> std::ops::Deref for Schemas<'a> {
    type Target = [&'a Value];
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}
impl<'a> IntoIterator for Schemas<'a> {
    type Item = &'a Value;
    type IntoIter = std::vec::IntoIter<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.values.into_iter()
    }
}
impl<'a, 'b> IntoIterator for &'b Schemas<'a> {
    type Item = &'b &'a Value;
    type IntoIter = std::slice::Iter<'b, &'a Value>;
    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}
fn unique<'a>(values: &mut Schemas<'a>, value: &'a Value) {
    if !budget::value(value) {
        return;
    }
    if values.serialized.insert(value.to_string()) {
        values.values.push(value);
    }
}
fn array<'a>(schema: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    schema
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}
// gjson's Array returns one element for a non-null scalar or object.
fn gjson_array(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    value
        .filter(|value| !value.is_null())
        .into_iter()
        .flat_map(|value| {
            value
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_else(|| std::slice::from_ref(value))
        })
}

fn branches(schema: &Value) -> Option<Vec<&Value>> {
    let mut out = Schemas::default();
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
    Some(out.values)
}
fn local_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    if !budget::charge(reference.len().saturating_add(1).saturating_mul(16)) {
        return None;
    }
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

#[derive(Clone, Default)]
struct AnalysisReferences {
    followed: BTreeSet<String>,
    cut: BTreeSet<String>,
}

impl AnalysisReferences {
    fn charge_copy(&self) -> bool {
        !budget::enabled()
            || self.followed.iter().chain(&self.cut).all(|reference| {
                budget::charge(reference.len().saturating_add(32).saturating_mul(16))
            })
    }
}

struct CachedAnalysis<T> {
    info: T,
    references: AnalysisReferences,
}

#[derive(Default)]
struct AnalysisMemo<T> {
    // Depth is bounded by 16, so each target has at most 17 cached results.
    targets: HashMap<*const Value, [Option<CachedAnalysis<T>>; 17]>,
    #[cfg(test)]
    target_analyses: usize,
    #[cfg(test)]
    cache_hits: usize,
}

trait CopyWork {
    fn charge_copy(&self) -> bool;
}
impl CopyWork for Info<'_> {
    fn charge_copy(&self) -> bool {
        Info::charge_copy(self)
    }
}
impl CopyWork for bool {
    fn charge_copy(&self) -> bool {
        budget::charge(1)
    }
}
impl CopyWork for Option<Vec<&Value>> {
    fn charge_copy(&self) -> bool {
        budget::charge(
            self.as_ref()
                .map_or(1, |values| values.len().saturating_mul(32)),
        )
    }
}

impl<T: Clone + Default + CopyWork> AnalysisMemo<T> {
    fn resolve_with(
        &mut self,
        target: &Value,
        depth: usize,
        visited: &mut BTreeSet<String>,
        references: &mut AnalysisReferences,
        walk: impl FnOnce(&mut Self, &mut BTreeSet<String>, &mut AnalysisReferences) -> T,
        cacheable: fn(&T) -> bool,
    ) -> T {
        if !budget::value(target) {
            return T::default();
        }
        if depth > 16 {
            return walk(self, visited, references);
        }
        let identity = std::ptr::from_ref(target);
        if let Some(cached) = self
            .targets
            .get(&identity)
            .and_then(|depths| depths[depth].as_ref())
            .filter(|cached| {
                cached.references.charge_copy() && cached.references.followed.is_disjoint(visited)
            })
        {
            #[cfg(test)]
            {
                self.cache_hits += 1;
            }
            if !cached.info.charge_copy() || !cached.references.charge_copy() {
                return T::default();
            }
            references
                .followed
                .extend(cached.references.followed.iter().cloned());
            references.cut.extend(cached.references.cut.iter().cloned());
            return cached.info.clone();
        }
        #[cfg(test)]
        {
            self.target_analyses += 1;
        }
        let mut own = AnalysisReferences::default();
        let info = walk(self, visited, &mut own);
        // A cut inactive at entry was pushed by this walk and repeats whenever
        // its followed refs are inactive. Cuts already active at entry depend
        // on the caller, so those results stay uncached. Cacheable walks restore
        // visited; items failures, which can leave refs active, are not cached.
        if !own.charge_copy() {
            return T::default();
        }
        if cacheable(&info) && own.cut.is_disjoint(visited) {
            if !info.charge_copy() || !own.charge_copy() {
                return T::default();
            }
            self.targets
                .entry(identity)
                .or_insert_with(|| std::array::from_fn(|_| None))[depth] = Some(CachedAnalysis {
                info: info.clone(),
                references: own.clone(),
            });
        }
        references.followed.extend(own.followed);
        references.cut.extend(own.cut);
        info
    }
}

impl<'a> AnalysisMemo<Info<'a>> {
    fn resolve(
        &mut self,
        target: &'a Value,
        root: &'a Value,
        depth: usize,
        visited: &mut BTreeSet<String>,
        references: &mut AnalysisReferences,
    ) -> Info<'a> {
        self.resolve_with(
            target,
            depth,
            visited,
            references,
            |memo, visited, own| analyze_depth_memo(target, root, depth, visited, memo, own),
            |_| true,
        )
    }
}

fn analyze<'a>(schema: &'a Value, root: &'a Value) -> Info<'a> {
    #[cfg(test)]
    if analysis_memo_tests::legacy_enabled() {
        return analysis_memo_tests::legacy_analyze(schema, root);
    }
    analyze_depth(schema, root, 0, &mut BTreeSet::new())
}
fn analyze_depth<'a>(
    schema: &'a Value,
    root: &'a Value,
    depth: usize,
    visited: &mut BTreeSet<String>,
) -> Info<'a> {
    analyze_depth_memo(
        schema,
        root,
        depth,
        visited,
        &mut AnalysisMemo::default(),
        &mut AnalysisReferences::default(),
    )
}
fn analyze_depth_memo<'a>(
    schema: &'a Value,
    root: &'a Value,
    depth: usize,
    visited: &mut BTreeSet<String>,
    memo: &mut AnalysisMemo<Info<'a>>,
    references: &mut AnalysisReferences,
) -> Info<'a> {
    if !budget::value(schema) {
        return Info::default();
    }
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
    let mut multi_branch_union = false;
    if has_union {
        let Some(branches) = branches(schema).filter(|b| !b.is_empty()) else {
            info.complex = true;
            return info;
        };
        let multiple = branches.len() > 1;
        multi_branch_union = multiple;
        for branch in branches {
            let branch_info =
                analyze_depth_memo(branch, root, depth + 1, visited, memo, references);
            if multiple {
                info.branches.push(branch_info.clone());
            }
            info.merge(branch_info);
        }
        info.complex |= multiple;
    }
    if let Some(reference) = schema.get("$ref") {
        let reference = str_of(Some(reference)).into_owned();
        let target = (!visited.contains(&reference))
            .then(|| local_ref(root, &reference))
            .flatten();
        if let Some(target) = target {
            references.followed.insert(reference.clone());
            visited.insert(reference.clone());
            info.merge(memo.resolve(target, root, depth + 1, visited, references));
            visited.remove(&reference);
        } else {
            if visited.contains(&reference) {
                references.cut.insert(reference);
            }
            if !multi_branch_union {
                info.complex = true;
                return info;
            }
        }
    }
    if let Some(all) = schema.get("allOf") {
        if let Some(all) = all.as_array() {
            for branch in all {
                info.merge(analyze_depth_memo(
                    branch,
                    root,
                    depth + 1,
                    visited,
                    memo,
                    references,
                ));
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
        .extend(gjson_array(schema.get("required")).map(|v| str_of(Some(v)).into_owned()));
    info
}

pub(super) fn parameterless(request: &Value, name: &str) -> bool {
    match original_schema(request, name) {
        Some(None) => true,
        Some(Some(s)) => analyze(s, s).parameterless(),
        None => false,
    }
}

// `functionNamesFromRequest`: collect raw names, preferring function.name.
fn collect_function_names(tool: &Value, names: &mut Vec<String>) {
    if !budget::value(tool) {
        return;
    }
    if let Some(tools) = tool.get("tools").and_then(Value::as_array) {
        for tool in tools {
            collect_function_names(tool, names);
        }
        return;
    }
    let mut has_declarations = false;
    for key in ["functionDeclarations", "function_declarations"] {
        if let Some(declarations) = tool.get(key).and_then(Value::as_array) {
            has_declarations = true;
            for declaration in declarations {
                let name = str_of(declaration.get("name"));
                if !name.is_empty() {
                    names.push(name.into_owned());
                }
            }
        }
    }
    if has_declarations {
        return;
    }
    let function_name = str_of(path(tool, "function.name"));
    let name = if function_name.is_empty() {
        str_of(tool.get("name"))
    } else {
        function_name
    };
    if !name.is_empty() {
        names.push(name.into_owned());
    }
}

fn sent_tool_name(original: &Value, target: &str) -> Option<String> {
    let mut names = Vec::new();
    for tool in original.get("tools")?.as_array()? {
        collect_function_names(tool, &mut names);
    }
    let names: Vec<_> = names.iter().map(String::as_str).collect();
    crate::responses_tools::sanitize_tool_names_metered(&names, budget::charge)?.remove(target)
}

fn translated_schemas<'a>(sent: &'a Value, original: &Value, name: &str) -> Vec<&'a Value> {
    if !budget::value(sent.get("tools").unwrap_or(&Value::Null))
        || !budget::value(path(sent, "request.tools").unwrap_or(&Value::Null))
    {
        return Vec::new();
    }
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
                if declared.is_empty() {
                    continue;
                }
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
    #[cfg(test)]
    if analysis_memo_tests::legacy_enabled() {
        return walk_memo_tests::legacy_items(schema, root, depth, visited);
    }
    items_memo(
        schema,
        root,
        depth,
        visited,
        &mut AnalysisMemo::default(),
        &mut AnalysisReferences::default(),
    )
}

fn items_memo<'a>(
    schema: &'a Value,
    root: &'a Value,
    depth: usize,
    visited: &mut BTreeSet<String>,
    memo: &mut AnalysisMemo<Option<Vec<&'a Value>>>,
    references: &mut AnalysisReferences,
) -> Option<Vec<&'a Value>> {
    if !budget::value(schema) {
        return None;
    }
    if depth > 16 || !schema.is_object() {
        return None;
    }
    let mut result = Schemas::default();
    if let Some(reference) = schema.get("$ref") {
        let reference = str_of(Some(reference)).into_owned();
        if !reference.starts_with("#/") {
            return None;
        }
        if !visited.insert(reference.clone()) {
            references.cut.insert(reference);
            return None;
        }
        let target = local_ref(root, &reference)?;
        references.followed.insert(reference.clone());
        for item in memo.resolve_with(
            target,
            depth + 1,
            visited,
            references,
            |memo, visited, own| items_memo(target, root, depth + 1, visited, memo, own),
            Option::is_some,
        )? {
            unique(&mut result, item);
        }
        visited.remove(&reference);
    }
    if schema.get("anyOf").is_some() || schema.get("oneOf").is_some() {
        let branches = branches(schema)?;
        if branches.len() != 1 {
            return None;
        }
        for item in items_memo(branches[0], root, depth + 1, visited, memo, references)? {
            unique(&mut result, item);
        }
    }
    if let Some(all) = schema.get("allOf") {
        for branch in all.as_array()? {
            for item in items_memo(branch, root, depth + 1, visited, memo, references)? {
                unique(&mut result, item);
            }
        }
    }
    if let Some(item) = schema.get("items") {
        unique(&mut result, item);
    }
    Some(result.values)
}

fn candidate_accepts<S>(infos: &[Arc<SchemaInfo<S>>], key: Option<&str>, current: &Value) -> bool {
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

fn injected(schema: &Value, key: &str, top: bool, analyses: &mut CallAnalysis) -> bool {
    let analysis = analyses.resolve(&[schema], schema);
    let info = &analysis.merged;
    if let Some(properties) = info.properties.get(key) {
        if gjson_array(schema.get("required")).any(|v| v == key) {
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
            analyses
                .resolve(&[branch], branch)
                .merged
                .properties
                .contains_key(key)
                && gjson_array(branch.get("required")).any(|v| v == key)
        })
    })
}

// `claudeOriginalDeclaresPropertyDeep`: a branch that could not be analyzed
// may still declare this field; never delete that client's argument.
fn declares_deep(
    schema: &Value,
    root: &Value,
    key: &str,
    depth: usize,
    visited: &mut BTreeSet<String>,
) -> bool {
    #[cfg(test)]
    if analysis_memo_tests::legacy_enabled() {
        return walk_memo_tests::legacy_declares_deep(schema, root, key, depth, visited);
    }
    // The root and property name stay fixed for this memo's lifetime.
    declares_deep_memo(
        schema,
        root,
        key,
        depth,
        visited,
        &mut AnalysisMemo::default(),
        &mut AnalysisReferences::default(),
    )
}

fn declares_deep_memo(
    schema: &Value,
    root: &Value,
    key: &str,
    depth: usize,
    visited: &mut BTreeSet<String>,
    memo: &mut AnalysisMemo<bool>,
    references: &mut AnalysisReferences,
) -> bool {
    if !budget::value(schema) {
        return false;
    }
    if depth > 16 || !schema.is_object() {
        return false;
    }
    if let Some(reference) = schema.get("$ref") {
        let reference = str_of(Some(reference)).into_owned();
        if visited.insert(reference.clone()) {
            let declared = local_ref(root, &reference).is_some_and(|target| {
                references.followed.insert(reference.clone());
                memo.resolve_with(
                    target,
                    depth + 1,
                    visited,
                    references,
                    |memo, visited, own| {
                        declares_deep_memo(target, root, key, depth + 1, visited, memo, own)
                    },
                    |_| true,
                )
            });
            visited.remove(&reference);
            if declared {
                return true;
            }
        } else {
            references.cut.insert(reference);
        }
    }
    if schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|props| props.contains_key(key))
        || gjson_array(schema.get("required")).any(|required| str_of(Some(required)) == key)
    {
        return true;
    }
    ["allOf", "anyOf", "oneOf"].iter().any(|keyword| {
        gjson_array(schema.get(*keyword)).any(|branch| {
            declares_deep_memo(branch, root, key, depth + 1, visited, memo, references)
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
    analyses: &mut CallAnalysis,
) {
    if !budget::node(value) || depth > 128 || originals.is_empty() {
        return;
    }
    let analysis = analyses.resolve(originals, root);
    let infos = &analysis.infos;
    let info = &analysis.merged;
    if info.unsafe_complex {
        return;
    }
    if let Some(values) = value.as_array_mut() {
        let Some(original_items) = analyses.items(originals, Some(root)) else {
            return;
        };
        let Some(translated_items) = analyses.items(translated, None) else {
            return;
        };
        let original_items: Vec<_> = original_items.iter().collect();
        let translated_items: Vec<_> = translated_items.iter().collect();
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
                analyses,
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
        && !originals.iter().any(|s| {
            s.get("anyOf").is_some_and(Value::is_array)
                || s.get("oneOf").is_some_and(Value::is_array)
        })
    {
        return;
    }
    // A probe contains only its path. Do not copy/check every unrelated schema
    // child on every probe, even when the analyses themselves are cached.
    let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    for key in keys {
        if !budget::charge(key.len().saturating_add(32).saturating_mul(16)) {
            return;
        }
        let Some(children) = info.properties.get(&key) else {
            continue;
        };
        if info.parent_accepts(&key, value) || candidate_accepts(infos, Some(&key), value) {
            continue;
        }
        let translated_children = analyses.translated_children(translated, &key);
        let translated_children: Vec<_> = translated_children.iter().collect();
        if !translated.is_empty() && translated_children.is_empty() {
            continue;
        }
        let children: Vec<_> = children.iter().collect();
        clean_value(
            value.get_mut(&key).unwrap(),
            &children,
            &translated_children,
            root,
            false,
            depth + 1,
            analyses,
        );
    }
    let dynamic = info.dynamic() || infos.iter().any(|info| info.dynamic());
    for key in ["reason", "_"] {
        if value.get(key).is_none()
            || info.properties.contains_key(key)
            || info.required.contains(key)
            || dynamic
            || info.parent_accepts(key, value)
            || candidate_accepts(infos, None, value)
            || (!translated.is_empty()
                && originals
                    .iter()
                    .any(|schema| declares_deep(schema, root, key, 0, &mut BTreeSet::new())))
        {
            continue;
        }
        if translated.is_empty()
            || translated.iter().any(|schema| {
                injected(schema, key, top, analyses)
                    || (top && info.forbids && reclean::requires(schema, key))
            })
        {
            value.as_object_mut().unwrap().shift_remove(key);
        }
    }
    if !info.complex
        && info.forbids
        && infos.iter().all(|i| i.object)
        && info.required.is_empty()
        && info.properties.is_empty()
        && !dynamic
        && !candidate_accepts(infos, None, value)
    {
        *value = json!({});
    }
}

// Owned once per streaming call: probes reuse both schemas and their analyses.
// The unbounded whole-response path uses a fresh cache for its borrowed schemas.
#[derive(Default)]
pub(super) struct CallSchemas {
    original: Option<Option<Box<Value>>>,
    translated: Vec<Value>,
    analyses: Mutex<CallAnalysis>,
    indexed: Mutex<Option<schema_index::SchemaIndex>>,
}

impl CallSchemas {
    pub(super) fn resolve(
        name: &str,
        original: &Value,
        sent: &Value,
        work: &crate::common::vertex_partial_args::WorkBudget,
    ) -> Self {
        budget::with(work, || {
            let mut result = Self {
                original: original_schema(original, name)
                    .map(|schema| schema.map(|schema| Box::new(budget::copy(schema)))),
                analyses: Mutex::default(),
                indexed: Mutex::default(),
                translated: translated_schemas(sent, original, name)
                    .into_iter()
                    .map(budget::copy)
                    .collect(),
            };
            #[cfg(test)]
            let use_index = !crate::common::vertex_partial_args::WorkBudget::test_unbounded();
            #[cfg(not(test))]
            let use_index = true;
            if use_index && let Some(Some(schema)) = result.original.as_ref() {
                result.indexed = Mutex::new(Some(schema_index::SchemaIndex::new(
                    schema,
                    &result.translated,
                )));
            }
            result
        })
    }

    pub(super) fn parameterless(
        &self,
        work: &crate::common::vertex_partial_args::WorkBudget,
    ) -> bool {
        budget::with(work, || {
            if let Some(index) = self.indexed.lock().expect("schema index").as_mut() {
                return index.parameterless();
            }
            match &self.original {
                Some(None) => true,
                Some(Some(schema)) => self
                    .analyses
                    .lock()
                    .expect("call analysis cache")
                    .resolve(&[schema], schema)
                    .merged
                    .parameterless(),
                None => false,
            }
        })
    }

    pub(super) fn probe(
        &self,
        state: usize,
        step: crate::common::vertex_partial_args::ProbeStep<'_>,
        work: &crate::common::vertex_partial_args::WorkBudget,
    ) -> Option<(bool, usize)> {
        #[cfg(test)]
        if crate::common::vertex_partial_args::WorkBudget::test_unbounded() {
            return None;
        }
        budget::with(work, || {
            if let Some(index) = self.indexed.lock().expect("schema index").as_mut() {
                return Some(index.probe(state, step));
            }
            use crate::common::vertex_partial_args::ProbeStep;
            let is_index = matches!(step, ProbeStep::Index);
            let result = match &self.original {
                None => (false, 2),
                Some(None) => match state {
                    1 => (!is_index, 1),
                    2 => (false, 2),
                    _ => {
                        if is_index && !self.translated.is_empty() {
                            (false, 2)
                        } else {
                            (!is_index, 1)
                        }
                    }
                },
                Some(Some(_)) => return None,
            };
            Some(result)
        })
    }

    pub(super) fn clean(
        &self,
        args: &Value,
        work: &crate::common::vertex_partial_args::WorkBudget,
    ) -> Value {
        budget::with(work, || {
            if let Some(index) = self.indexed.lock().expect("schema index").as_mut() {
                return index.clean(args);
            }
            let translated: Vec<_> = self.translated.iter().collect();
            clean_resolved(
                args,
                self.original.as_ref().map(|schema| schema.as_deref()),
                &translated,
                &mut self.analyses.lock().expect("call analysis cache"),
            )
        })
    }
}

pub(super) fn clean_arguments(args: &Value, name: &str, original: &Value, sent: &Value) -> Value {
    let translated = translated_schemas(sent, original, name);
    clean_resolved(
        args,
        original_schema(original, name),
        &translated,
        &mut CallAnalysis::default(),
    )
}

fn clean_resolved(
    args: &Value,
    original: Option<Option<&Value>>,
    translated: &[&Value],
    analyses: &mut CallAnalysis,
) -> Value {
    if !budget::value(args) {
        return Value::Null;
    }
    if !args.is_object() && !translated.is_empty() {
        return budget::copy(args);
    }
    let Some(schema) = original else {
        return budget::copy(args);
    };
    let Some(schema) = schema else {
        return json!({});
    };
    if !args.is_object() {
        return budget::copy(args);
    }
    let mut args = budget::copy(args);
    clean_value(&mut args, &[schema], translated, schema, true, 0, analyses);
    args
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod analysis_memo_tests;

#[cfg(test)]
mod walk_memo_tests;

#[cfg(test)]
mod round6_tests;
