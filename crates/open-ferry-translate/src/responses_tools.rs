// Ported from CLIProxyAPI internal/util/responses_tools.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which declaration a tool name refers to in a Responses-style request, and
//! the Gemini function declarations built from them.
//!
//! Tools can be declared at the top level, in `additional_tools` input items,
//! and inside namespaces, whose children are named `<namespace>__<child>`. When
//! a name is declared more than once, one declaration wins.
//!
//! Gemini function names allow fewer characters than Responses tool names, so
//! [`build_gemini_function_declarations`] gives each winning tool a sanitized
//! name, made unique with a hash suffix where two would collide, and returns
//! maps both ways: from the client's names to Gemini's, and from Gemini's back
//! to the tool's [`ToolIdentity`].
//!
//! Deviations from upstream: none.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::apply_patch;
use crate::common::gemini::{GEMINI_FUNCTION_NAME_LIMIT, sanitize_gemini_function_name};
use crate::gemini_schema::clean_json_schema_for_gemini_json_schema;
use crate::go;
use crate::json::lenient::{self, Found};
use crate::json::{object, path, str_of};

/// `QualifyResponsesNamespaceToolName`: a namespace child's full name.
pub(crate) fn qualify_namespace_tool_name(namespace: &str, child: &str) -> String {
    let child = child.trim();
    let namespace = namespace.trim();
    if child.is_empty() || namespace.is_empty() || child.starts_with("mcp__") {
        return child.to_owned();
    }
    if child == namespace
        || child
            .strip_prefix(namespace)
            .is_some_and(|rest| rest.starts_with("__"))
    {
        return child.to_owned();
    }
    if namespace.ends_with("__") {
        return format!("{namespace}{child}");
    }
    format!("{namespace}__{child}")
}

/// One tool declaration, as `ResponsesToolDescriptor`.
#[derive(Clone, Debug)]
pub(crate) struct ToolDescriptor<'v> {
    pub tool: &'v Value,
    /// The name without its namespace, trimmed.
    pub local_name: String,
    /// The namespace's trimmed name, or empty for a tool declared directly.
    pub namespace: String,
    /// Whether it is a custom tool rather than a function.
    pub custom: bool,
    /// 0 for top-level tools, 1 for `additional_tools`.
    pub source_priority: u8,
    /// False for a namespace child.
    pub direct: bool,
    /// The order the declaration was found in.
    pub order: usize,
}

impl ToolDescriptor<'_> {
    /// `responsesToolDescriptorPrecedes`.
    fn precedes(&self, other: &Self) -> bool {
        if self.source_priority != other.source_priority {
            return self.source_priority < other.source_priority;
        }
        if self.direct != other.direct {
            return self.direct;
        }
        self.order < other.order
    }
}

/// `CollectResponsesToolDescriptors`: every function and custom tool
/// declared, with its qualified name, in the order they're found.
fn collect_tool_descriptors<'v>(root: &'v Value) -> Vec<(String, ToolDescriptor<'v>)> {
    let mut descriptors = Vec::new();
    let mut add =
        |name: String, local_name: String, namespace: Option<&str>, tool: &'v Value, priority| {
            if name.is_empty() {
                return;
            }
            let order = descriptors.len();
            descriptors.push((
                name,
                ToolDescriptor {
                    tool,
                    local_name,
                    namespace: namespace.unwrap_or_default().to_owned(),
                    custom: str_of(tool.get("type")).trim() == "custom",
                    source_priority: priority,
                    direct: namespace.is_none(),
                    order,
                },
            ));
        };
    for (tools, priority) in tool_sources(root) {
        for tool in tools {
            match str_of(tool.get("type")).trim() {
                "" | "function" | "custom" => {
                    let name = tool_name(tool);
                    add(name.clone(), name, None, tool, priority);
                }
                "namespace" => {
                    let namespace = str_of(tool.get("name"));
                    let namespace = namespace.trim();
                    let children = match (tool.get("tools"), tool.get("children")) {
                        (Some(Value::Array(children)), _) | (_, Some(Value::Array(children))) => {
                            children
                        }
                        _ => continue,
                    };
                    for child in children {
                        let child_name = tool_name(child);
                        if child_name.is_empty() {
                            continue;
                        }
                        if matches!(str_of(child.get("type")).trim(), "" | "function" | "custom") {
                            let name = qualify_namespace_tool_name(namespace, &child_name);
                            add(name, child_name, Some(namespace), child, priority);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    descriptors
}

/// `CollectResponsesToolWinners`: the winning declaration of each tool name.
/// Top-level tools beat `additional_tools`, then direct declarations beat
/// namespace children, then earlier beats later.
pub(crate) fn collect_tool_winners(root: &Value) -> HashMap<String, ToolDescriptor<'_>> {
    let descriptors = collect_tool_descriptors(root);
    let winners: HashSet<usize> = winning_indexes(&descriptors).into_values().collect();
    descriptors
        .into_iter()
        .enumerate()
        .filter(|(index, _)| winners.contains(index))
        .map(|(_, descriptor)| descriptor)
        .collect()
}

/// The index in `descriptors` of each name's winning declaration.
fn winning_indexes<'d>(descriptors: &'d [(String, ToolDescriptor<'_>)]) -> HashMap<&'d str, usize> {
    let mut winners = HashMap::<&str, usize>::new();
    for (index, (name, descriptor)) in descriptors.iter().enumerate() {
        match winners.get(name.as_str()) {
            Some(&current) if !descriptor.precedes(&descriptors[current].1) => {}
            _ => {
                winners.insert(name, index);
            }
        }
    }
    winners
}

/// The arrays tools are declared in, with their priority.
fn tool_sources(root: &Value) -> Vec<(&Vec<Value>, u8)> {
    let mut sources = Vec::new();
    if let Some(Value::Array(tools)) = root.get("tools") {
        sources.push((tools, 0));
    }
    if let Some(Value::Array(input)) = root.get("input") {
        for item in input {
            if str_of(item.get("type")) == "additional_tools"
                && let Some(Value::Array(tools)) = item.get("tools")
            {
                sources.push((tools, 1));
            }
        }
    }
    sources
}

/// `responsesToolName`: `name`, or else `function.name`, trimmed.
fn tool_name(tool: &Value) -> String {
    let name = str_of(tool.get("name"));
    if !name.trim().is_empty() {
        return name.trim().to_owned();
    }
    let name = tool
        .get("function")
        .and_then(|function| function.get("name"));
    str_of(name).trim().to_owned()
}

/// `responsesToolDescription`: `description`, or else `function.description`.
fn tool_description(tool: &Value) -> String {
    let description = str_of(tool.get("description"));
    if !description.is_empty() {
        return description.into_owned();
    }
    str_of(path(tool, "function.description")).into_owned()
}

/// `responsesToolParameters`: the first of the places a schema can be given.
fn tool_parameters(tool: &Value) -> Option<&Value> {
    [
        "parameters",
        "parametersJsonSchema",
        "input_schema",
        "function.parameters",
        "function.parametersJsonSchema",
    ]
    .into_iter()
    .find_map(|at| path(tool, at))
}

/// `ResponsesToolIdentity`: the tool a Gemini function name stands for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolIdentity {
    /// The tool's name without its namespace.
    pub name: String,
    /// The namespace it was declared in, or empty.
    pub namespace: String,
    /// Whether it is a custom tool rather than a function.
    pub custom: bool,
    /// Whether it is the custom `apply_patch` tool. This comes from the
    /// winning declaration, never from the name Gemini sends back.
    pub apply_patch: bool,
}

/// What [`build_gemini_function_declarations`] returns.
#[derive(Debug, Default)]
pub(crate) struct GeminiDeclarations {
    /// One function declaration per winning tool, in declaration order.
    pub declarations: Vec<Value>,
    /// From each qualified tool name, and each unambiguous local name, to the
    /// Gemini name.
    pub forward: HashMap<String, String>,
    /// From each Gemini name, and each qualified name that differs from it,
    /// to the tool's identity.
    pub reverse: HashMap<String, ToolIdentity>,
}

/// `BuildGeminiFunctionDeclarations`: the request's function and custom
/// tools as Gemini function declarations, with the name maps.
pub(crate) fn build_gemini_function_declarations(root: &Value) -> GeminiDeclarations {
    let descriptors = collect_tool_descriptors(root);
    let winners = winning_indexes(&descriptors);
    let winning: Vec<&(String, ToolDescriptor<'_>)> = descriptors
        .iter()
        .enumerate()
        .filter(|(index, (name, _))| winners.get(name.as_str()) == Some(index))
        .map(|(_, descriptor)| descriptor)
        .collect();
    let mut out = GeminiDeclarations::default();
    if winning.is_empty() {
        return out;
    }

    let names: Vec<&str> = winning.iter().map(|(name, _)| name.as_str()).collect();
    let sanitized = sanitize_tool_names(&names);
    for (name, descriptor) in winning {
        let gemini_name = match sanitized.get(name.as_str()) {
            Some(mapped) if !mapped.is_empty() => mapped.clone(),
            _ => sanitize_gemini_function_name(name),
        };
        out.forward.insert(name.clone(), gemini_name.clone());
        if !descriptor.local_name.is_empty() && descriptor.local_name != *name {
            out.forward
                .entry(descriptor.local_name.clone())
                .or_insert_with(|| gemini_name.clone());
        }

        let is_apply_patch = apply_patch::is_custom_tool(descriptor.tool);
        let identity = ToolIdentity {
            name: descriptor.local_name.clone(),
            namespace: descriptor.namespace.clone(),
            custom: descriptor.custom,
            apply_patch: is_apply_patch,
        };
        out.reverse.insert(gemini_name.clone(), identity.clone());
        if *name != gemini_name {
            out.reverse.insert(name.clone(), identity);
        }

        let mut description = tool_description(descriptor.tool);
        let parameters = if is_apply_patch {
            description = apply_patch::description(descriptor.tool);
            apply_patch::parameters()
        } else if descriptor.custom {
            serde_json::json!({
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"],
            })
        } else {
            tool_parameters(descriptor.tool).map_or_else(
                || Value::Object(Default::default()),
                clean_json_schema_for_gemini_json_schema,
            )
        };
        out.declarations.push(object([
            ("name", Value::String(gemini_name)),
            ("description", Value::String(description)),
            ("parametersJsonSchema", parameters),
        ]));
    }
    out
}

/// `sanitizeResponsesToolNames`: a Gemini name for each tool name. Names
/// that sanitize alike, or to a name already taken, get a hash suffix.
fn sanitize_tool_names(names: &[&str]) -> HashMap<String, String> {
    let mut seen = HashSet::with_capacity(names.len());
    let mut unique = Vec::new();
    let mut base_counts = HashMap::<String, usize>::new();
    for &name in names {
        if name.is_empty() || !seen.insert(name) {
            continue;
        }
        unique.push(name);
        *base_counts
            .entry(sanitize_gemini_function_name(name))
            .or_default() += 1;
    }
    unique.sort_unstable();

    let mut out = HashMap::with_capacity(unique.len());
    let mut used = HashSet::with_capacity(unique.len());
    for name in unique {
        let base = sanitize_gemini_function_name(name);
        let mapped = if base_counts[&base] > 1 || used.contains(&base) {
            disambiguate_sanitized_name(&base, name, &used)
        } else {
            base
        };
        used.insert(mapped.clone());
        out.insert(name.to_owned(), mapped);
    }
    out
}

/// `disambiguateResponsesSanitizedName`: `base` with a suffix from the hash
/// of the original name and an attempt number, cut so the whole fits in 64
/// bytes, trying attempts until one is free.
fn disambiguate_sanitized_name(base: &str, original: &str, used: &HashSet<String>) -> String {
    for attempt in 0u64.. {
        let digest = Sha256::digest(format!("{original}\0{attempt}").as_bytes());
        let mut suffix = String::from("_");
        for byte in &digest[..6] {
            let _ = write!(suffix, "{byte:02x}");
        }
        // A sanitized name is ASCII, so any byte is a boundary.
        let prefix = &base[..base.len().min(GEMINI_FUNCTION_NAME_LIMIT - suffix.len())];
        let candidate = format!("{prefix}{suffix}");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("some attempt gives an unused name")
}

/// `ResponsesToolReverseIdentityMap`: the reverse map
/// [`build_gemini_function_declarations`] gives for a Responses request, or
/// for the `request` it is wrapped in.
pub(crate) fn responses_tool_reverse_identity_map(raw: &Value) -> HashMap<String, ToolIdentity> {
    let mut root = raw;
    if let Some(request) = raw.get("request")
        && ["model", "input", "tools"]
            .iter()
            .any(|key| request.get(*key).is_some())
    {
        root = request;
    }
    build_gemini_function_declarations(root).reverse
}

/// `MapResponsesToolName`: the Gemini name for a tool name, from `forward`
/// if it is there, else sanitized.
pub(crate) fn map_responses_tool_name(forward: &HashMap<String, String>, name: &str) -> String {
    match forward.get(name) {
        Some(mapped) if !mapped.is_empty() => mapped.clone(),
        _ => sanitize_gemini_function_name(name),
    }
}

/// `ConvertResponsesToolChoiceToGemini`: a Responses `tool_choice` as a
/// Gemini `functionCallingConfig`, or `None` if it names no mode.
pub(crate) fn convert_responses_tool_choice_to_gemini(
    tool_choice: Option<&Value>,
    forward: &HashMap<String, String>,
) -> Option<Value> {
    let mode_of = |choice: &str| match choice {
        "none" => Some("NONE"),
        "auto" => Some("AUTO"),
        "required" | "any" => Some("ANY"),
        _ => None,
    };
    let mut allowed = Vec::new();
    let mode = match tool_choice? {
        Value::String(choice) => mode_of(&go::to_lower(choice.trim()))?,
        choice @ Value::Object(_) => {
            let kind = go::to_lower(str_of(choice.get("type")).trim());
            match kind.as_str() {
                "function" | "custom" | "tool" | "" => {
                    let first = |paths: [&str; 3]| {
                        paths
                            .into_iter()
                            .map(|at| str_of(path(choice, at)).trim().to_owned())
                            .find(|value| !value.is_empty())
                            .unwrap_or_default()
                    };
                    let mut name = first(["name", "function.name", "custom.name"]);
                    let namespace = first(["namespace", "function.namespace", "custom.namespace"]);
                    if !namespace.is_empty() {
                        name = qualify_namespace_tool_name(&namespace, &name);
                    }
                    if !name.is_empty() {
                        allowed.push(Value::String(map_responses_tool_name(forward, &name)));
                    }
                    "ANY"
                }
                other => mode_of(other)?,
            }
        }
        _ => return None,
    };
    let mut config = object([("mode", Value::String(mode.to_owned()))]);
    if !allowed.is_empty() {
        config["allowedFunctionNames"] = Value::Array(allowed);
    }
    Some(config)
}

/// `UnwrapResponsesCustomToolInput`: a custom tool's raw input from the
/// arguments Gemini sent, which may be `{"input": ...}`, a JSON string, or
/// plain text. A non-string `input` comes back as written.
pub(crate) fn unwrap_responses_custom_tool_input(arguments: &str) -> String {
    let arguments = arguments.trim();
    if arguments.is_empty() || arguments == "{}" {
        return String::new();
    }
    if go::gjson_valid(arguments.as_bytes()) {
        // The text is valid JSON, so an object starts at its first `{`.
        if arguments.starts_with('{') {
            match lenient::get(arguments, "input") {
                Some(Found::String(input)) => return input,
                Some(Found::Number(input) | Found::Literal(input) | Found::Json(input)) => {
                    return input.to_owned();
                }
                None => {}
            }
        }
        if arguments.starts_with('"') {
            return gjson_string(arguments);
        }
    }
    arguments.to_owned()
}

/// gjson `String()` of a valid JSON string, quotes included.
pub(crate) fn gjson_string(quoted: &str) -> String {
    match lenient::get(&format!("{{\"v\":{quoted}}}"), "v") {
        Some(Found::String(text)) => text,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn namespace_children_are_qualified() {
        assert_eq!(qualify_namespace_tool_name(" ns ", " child "), "ns__child");
        assert_eq!(qualify_namespace_tool_name("ns__", "child"), "ns__child");
        assert_eq!(qualify_namespace_tool_name("ns", "ns__child"), "ns__child");
        assert_eq!(qualify_namespace_tool_name("ns", "ns"), "ns");
        assert_eq!(qualify_namespace_tool_name("ns", "nsx"), "ns__nsx");
        assert_eq!(qualify_namespace_tool_name("ns", "mcp__x"), "mcp__x");
        assert_eq!(qualify_namespace_tool_name("", "child"), "child");
        assert_eq!(qualify_namespace_tool_name("ns", " "), "");
    }

    #[test]
    fn descriptors_cover_priorities_and_namespaces() {
        let root = json!({
            "tools": [{"type": "function", "name": "top_fn"}],
            "input": [{
                "type": "additional_tools",
                "tools": [
                    {
                        "type": "namespace",
                        "name": "ns1",
                        "tools": [
                            {"type": "function", "name": "child_fn"},
                            {"type": "custom", "name": "child_custom"},
                            {"type": "web_search", "name": "skipped"}
                        ]
                    },
                    {"type": "custom", "name": "direct_custom"}
                ]
            }]
        });
        let names: Vec<String> = collect_tool_descriptors(&root)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "top_fn",
                "ns1__child_fn",
                "ns1__child_custom",
                "direct_custom"
            ]
        );
    }

    #[test]
    fn top_level_beats_additional_tools() {
        let root = json!({
            "tools": [{"type": "function", "name": "shared_fn", "description": "top level"}],
            "input": [{
                "type": "additional_tools",
                "tools": [{"type": "function", "name": "shared_fn", "description": "additional"}]
            }]
        });
        let winners = collect_tool_winners(&root);
        let winner = &winners["shared_fn"];
        assert_eq!(winner.source_priority, 0);
        assert_eq!(winner.tool["description"], "top level");
    }

    #[test]
    fn direct_beats_namespace_child() {
        let root = json!({
            "tools": [
                {"type": "namespace", "name": "n", "tools": [{"type": "function", "name": "x"}]},
                {"type": "custom", "name": "n__x", "description": "direct"}
            ]
        });
        let winners = collect_tool_winners(&root);
        let winner = &winners["n__x"];
        assert!(winner.direct);
        assert_eq!(winner.tool["type"], "custom");
    }

    #[test]
    fn sanitized_names_skip_repeats_and_clashes() {
        let sanitized = sanitize_tool_names(&["a.b", "a-b", "a.b", "", "c"]);
        assert_eq!(sanitized.len(), 3);
        assert_eq!(sanitized["c"], "c");
        assert_ne!(sanitized["a.b"], sanitized["a-b"]);
    }

    #[test]
    fn many_tool_names_are_sanitized() {
        // Thousands of tools take one pass, not one per earlier name.
        let names: Vec<String> = (0..20_000).map(|index| format!("tool_{index}")).collect();
        let mut refs: Vec<&str> = names.iter().map(String::as_str).collect();
        refs.extend(names.iter().map(String::as_str));
        let sanitized = sanitize_tool_names(&refs);
        assert_eq!(sanitized.len(), names.len());
        assert_eq!(sanitized["tool_19999"], "tool_19999");
    }
}
