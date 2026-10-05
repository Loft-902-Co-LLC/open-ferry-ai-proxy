// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_request.go
// (xaiSupportsNativeImageGeneration, ensureXAINativeXSearchTool, the client
// web_search alias, normalizeXAIForcedHostedToolChoice,
// pruneXAIOrphanedToolChoice, xaiShouldFoldNamespaceTools,
// buildXAINamespaceDispatcherTool, normalizeXAITools, clampXAIToolsLimit,
// promoteXAIAdditionalTools, normalizeXAIToolChoiceForTools,
// normalizeXAINamespaceToolChoice, qualifyXAINamespaceToolName,
// collectXAINamespaceToolRefs, normalizeXAIInputCustomToolCalls) and
// xai_executor_response.go (xaiRequestHasNativeXSearch,
// collectXAIClientDeclaredToolKeys, normalizeXAIInputNamespaceToolCalls)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The tools of a request, reshaped for Grok.
//!
//! xAI takes no `namespace` tools, `tool_search`, `additional_tools` input
//! items, custom tools or `namespace` on a call, and at most 200 tools. So:
//! - a namespace's tools are flattened into functions named
//!   `<namespace>__<tool>`, or, when the tools would number more than 200,
//!   each namespace is folded into one dispatcher function that takes the
//!   child's name and arguments ([`should_fold`]);
//! - a custom tool becomes a function, and its parameters are made
//!   acceptable (see [`super::schema`]);
//! - `tool_search` is dropped, and so is `image_generation` before Grok 4.6;
//! - `additional_tools` declarations move to the top-level `tools`;
//! - the tool choice follows: namespaced names are qualified, choices of
//!   dropped tools are pruned, a choice of a hosted tool becomes
//!   `"required"` with that tool alone, and with no tools left there is no
//!   choice;
//! - a client function named `web_search` is renamed
//!   (`clientfn_web_search`), so Grok doesn't take it for its own search;
//! - past custom and namespaced calls in the input are rewritten to match.
//!
//! With `xai.inject-x-search` on, Grok's own X search tool is added.
//!
//! Deviations from upstream:
//! - Edits are made on parsed JSON, so a changed list is written again
//!   rather than spliced into the original bytes; keys keep their order.
//! - A folded namespace lists each child's parameters written compactly
//!   from the parsed schema; upstream copies the client's text, whitespace
//!   included. A folded call's arguments are likewise parsed and written
//!   compactly, and neither has Go's `\u003c`-style escapes of `<`, `>`
//!   and `&`.
//! - A custom call's object `input` is written compactly as parsed.
//! - A function whose parameters' references are too large or too deep to
//!   inline gets the permissive object schema, and a folded namespace lists
//!   that schema for such a child (see [`super::schema`]); upstream inlines
//!   them all.

use std::collections::{HashMap, HashSet};

use open_ferry_translate::go;
use open_ferry_translate::json::exact;
use serde_json::{Map, Value, json};

use super::schema::{self, Inlined};
use crate::codex::request::base_model;
use crate::json::{self, str_of};

/// The most tools xAI takes (`xaiMaxTools`).
pub(crate) const MAX_TOOLS: usize = 200;

/// A function tool's type.
pub(crate) const FUNCTION: &str = "function";

/// A custom tool's type.
pub(crate) const CUSTOM: &str = "custom";

const NAMESPACE: &str = "namespace";
const TOOL_SEARCH: &str = "tool_search";
const ADDITIONAL_TOOLS: &str = "additional_tools";

/// The hosted image tool (`xaiImageGenerationToolType`).
pub(crate) const IMAGE_GENERATION: &str = "image_generation";

/// The hosted web search tool (`xaiWebSearchToolType`).
pub(crate) const WEB_SEARCH: &str = "web_search";

/// Grok's own X search tool (`xaiXSearchToolType`).
pub(crate) const X_SEARCH: &str = "x_search";

/// What a client function named `web_search` is renamed to
/// (`xaiClientWebSearchAlias`), with `_1`, `_2`, … if that is taken.
const CLIENT_WEB_SEARCH_ALIAS: &str = "clientfn_web_search";

/// The first Grok version that takes the hosted `image_generation` tool
/// (`xaiGrokImageGenerationMinVersion`).
const IMAGE_GENERATION_MIN_VERSION: (i64, i64) = (4, 6);

/// What a flattened or folded tool name stands for (`xaiNamespaceToolRef`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamespaceRef {
    /// The namespace.
    pub(crate) namespace: String,
    /// The child's name, or empty for a dispatcher.
    pub(crate) name: String,
    /// Whether the name is a folded namespace's dispatcher.
    pub(crate) is_dispatcher: bool,
}

/// Flattened and folded tool names and what they stand for.
pub(crate) type NamespaceRefs = HashMap<String, NamespaceRef>;

/// A tool the client declared, as the client knows it, with the type it is
/// sent as (`xaiClientToolKey`): a custom tool goes as a function.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ClientToolKey {
    /// The tool's namespace, or empty.
    pub(crate) namespace: String,
    /// The tool's own name.
    pub(crate) name: String,
    /// `function`, for a function or custom tool.
    pub(crate) tool_type: String,
}

/// gjson `String()` of `value`'s `key`.
fn text(value: &Value, key: &str) -> String {
    str_of(value.get(key))
}

/// [`text`], trimmed.
fn trimmed(value: &Value, key: &str) -> String {
    text(value, key).trim().to_owned()
}

/// `value`'s `key`, if it is an array.
fn array<'v>(value: &'v Value, key: &str) -> Option<&'v Vec<Value>> {
    value.get(key).and_then(Value::as_array)
}

/// gjson `Array()`: an array's items; nothing for a missing value or
/// `null`; any other value as the only item.
fn elements(value: Option<&Value>) -> &[Value] {
    match value {
        None | Some(Value::Null) => &[],
        Some(Value::Array(items)) => items,
        Some(other) => std::slice::from_ref(other),
    }
}

/// The body's `input` items, if `input` is an array.
fn input_items(body: &Value) -> &[Value] {
    array(body, "input").map_or(&[][..], Vec::as_slice)
}

/// The `tools` of each `additional_tools` input item.
fn additional_tool_lists(body: &Value) -> impl Iterator<Item = Option<&Value>> {
    input_items(body)
        .iter()
        .filter(|item| text(item, "type") == ADDITIONAL_TOOLS)
        .map(|item| item.get("tools"))
}

/// The body's `tools`, then each `additional_tools` item's.
fn tool_lists(body: &Value) -> impl Iterator<Item = Option<&Value>> {
    std::iter::once(body.get("tools")).chain(additional_tool_lists(body))
}

/// `xaiSupportsNativeImageGeneration`: whether the model, a Grok from 4.6
/// on, takes the hosted `image_generation` tool. `grok-4.20` is an older
/// line, despite its number.
pub(crate) fn supports_native_image_generation(model: &str) -> bool {
    let name = go::to_lower(base_model(model).trim());
    let name = name.rsplit('/').next().unwrap_or_default();
    let Some(rest) = name.strip_prefix("grok-") else {
        return false;
    };
    if rest == "4.20" || rest.starts_with("4.20-") {
        return false;
    }
    grok_version(rest)
        .is_some_and(|(major, minor)| (major, minor.max(0)) >= IMAGE_GENERATION_MIN_VERSION)
}

/// `xaiParseGrokVersionPrefix`: the major and minor version a model name
/// starts with; a missing minor is `-1`.
fn grok_version(rest: &str) -> Option<(i64, i64)> {
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let major = rest.get(..digits)?.parse::<i64>().ok()?;
    let Some(minor) = rest.get(digits..).and_then(|after| after.strip_prefix('.')) else {
        return Some((major, -1));
    };
    let digits = minor.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Some((major, -1));
    }
    Some((major, minor.get(..digits)?.parse::<i64>().ok()?))
}

/// `xaiRequestHasNativeXSearch`: whether the tools, or an
/// `additional_tools` item's, include Grok's X search.
pub(crate) fn has_native_x_search(body: &Value) -> bool {
    let has = |tools: Option<&Value>| {
        tools.and_then(Value::as_array).is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("type").and_then(Value::as_str) == Some(X_SEARCH))
        })
    };
    has(body.get("tools"))
        || input_items(body)
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some(ADDITIONAL_TOOLS))
            .any(|item| has(item.get("tools")))
}

/// Grok's X search tool (`xaiXSearchToolJSON`).
fn x_search_tool() -> Value {
    json!({"type": X_SEARCH})
}

/// `ensureXAINativeXSearchTool`: adds Grok's X search to the tools unless it
/// is there, and to an `allowed_tools` choice unless it lists it.
pub(crate) fn ensure_native_x_search(body: &mut Value) {
    if !has_native_x_search(body) {
        match body.get_mut("tools") {
            Some(Value::Array(tools)) => tools.push(x_search_tool()),
            _ => {
                json::set(body, "tools", Value::Array(vec![x_search_tool()]));
            }
        }
    }
    let Some(choice @ Value::Object(_)) = body.get_mut("tool_choice") else {
        return;
    };
    if text(choice, "type") != "allowed_tools" {
        return;
    }
    match choice.get_mut("tools") {
        Some(Value::Array(allowed)) => {
            if !allowed.iter().any(|tool| trimmed(tool, "type") == X_SEARCH) {
                allowed.push(x_search_tool());
            }
        }
        _ => {
            json::set(choice, "tools", Value::Array(vec![x_search_tool()]));
        }
    }
}

/// Whether `tool` is a client function or custom tool named `name`.
fn is_client_function_named(tool: &Value, name: &str) -> bool {
    let tool_type = trimmed(tool, "type");
    (tool_type == FUNCTION || tool_type == CUSTOM) && trimmed(tool, "name") == name
}

/// `xaiHasClientWebSearchFunction`: whether the client declared a function
/// or custom tool named `web_search` that isn't a folded namespace's
/// dispatcher.
pub(crate) fn has_client_web_search_function(body: &Value, refs: &NamespaceRefs) -> bool {
    !refs.contains_key(WEB_SEARCH)
        && array(body, "tools").is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| is_client_function_named(tool, WEB_SEARCH))
        })
}

/// `xaiBodyHasToolNamed`: whether a tool, a namespace's tool or an input
/// item has the name.
fn body_has_tool_named(body: &Value, name: &str) -> bool {
    let named = |value: &Value| trimmed(value, "name") == name;
    let in_tools = array(body, "tools").is_some_and(|tools| {
        tools.iter().any(|tool| {
            named(tool) || array(tool, "tools").is_some_and(|children| children.iter().any(named))
        })
    });
    in_tools || input_items(body).iter().any(named)
}

/// `xaiResolveClientWebSearchAlias`: the first of `clientfn_web_search`,
/// `clientfn_web_search_1`, … that nothing in the body is named.
pub(crate) fn resolve_client_web_search_alias(body: &Value) -> String {
    if !body_has_tool_named(body, CLIENT_WEB_SEARCH_ALIAS) {
        return CLIENT_WEB_SEARCH_ALIAS.to_owned();
    }
    (1usize..)
        .map(|index| format!("{CLIENT_WEB_SEARCH_ALIAS}_{index}"))
        .find(|alias| !body_has_tool_named(body, alias))
        .unwrap_or_default()
}

/// `aliasXAIClientWebSearchInput`: renames past calls of the client's
/// `web_search`, and their outputs, outside any namespace.
pub(crate) fn alias_client_web_search_input(body: &mut Value, alias: &str, refs: &NamespaceRefs) {
    if alias.is_empty() || refs.contains_key(WEB_SEARCH) {
        return;
    }
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return;
    };
    for item in input {
        let item_type = trimmed(item, "type");
        if matches!(
            item_type.as_str(),
            "function_call" | "custom_tool_call" | "function_call_output"
        ) && trimmed(item, "name") == WEB_SEARCH
            && trimmed(item, "namespace").is_empty()
        {
            json::set(item, "name", Value::from(alias));
        }
    }
}

/// `aliasXAIClientWebSearchFunction`: renames the client's `web_search`
/// function in the tools, the tool choice and past calls, unless a folded
/// namespace's dispatcher has the name.
pub(crate) fn alias_client_web_search_function(
    body: &mut Value,
    alias: &str,
    refs: &NamespaceRefs,
) {
    if alias.is_empty() {
        return;
    }
    let dispatcher = refs.contains_key(WEB_SEARCH);
    if let Some(Value::Array(tools)) = body.get_mut("tools") {
        for tool in tools {
            if is_client_function_named(tool, WEB_SEARCH) && !dispatcher {
                json::set(tool, "name", Value::from(alias));
            }
        }
    }
    if let Some(choice @ Value::Object(_)) = body.get_mut("tool_choice") {
        if str_of(json::get(choice, "function.name")).trim() == WEB_SEARCH
            && str_of(json::get(choice, "function.namespace"))
                .trim()
                .is_empty()
            && !dispatcher
        {
            json::set(choice, "function.name", Value::from(alias));
        }
        let choice_type = trimmed(choice, "type");
        if trimmed(choice, "name") == WEB_SEARCH
            && trimmed(choice, "namespace").is_empty()
            && (choice_type == FUNCTION || choice_type == "tool")
            && !dispatcher
        {
            json::set(choice, "name", Value::from(alias));
        }
        if let Some(Value::Array(allowed)) = choice.get_mut("tools") {
            for tool in allowed {
                if !trimmed(tool, "namespace").is_empty() || dispatcher {
                    continue;
                }
                // Upstream renames a `function` or `tool` entry, or any other
                // that isn't the hosted search.
                if trimmed(tool, "name") == WEB_SEARCH && trimmed(tool, "type") != WEB_SEARCH {
                    json::set(tool, "name", Value::from(alias));
                }
            }
        }
    }
    alias_client_web_search_input(body, alias, refs);
}

/// `normalizeXAIForcedHostedToolChoice`: a choice of the hosted
/// `tool_type` (`web_search` or `image_generation`) becomes `"required"`
/// with that tool alone in the tools. An `allowed_tools` choice listing
/// only it becomes its mode (`"auto"`, else `"required"`), likewise; one
/// listing others too loses it.
pub(crate) fn normalize_forced_hosted_tool_choice(body: &mut Value, tool_type: &str) {
    let Some(choice @ Value::Object(_)) = body.get("tool_choice") else {
        return;
    };
    let choice_type = trimmed(choice, "type");
    if choice_type == tool_type {
        keep_only_hosted_tools(body, tool_type);
        json::set(body, "tool_choice", Value::from("required"));
        return;
    }
    if choice_type != "allowed_tools" {
        return;
    }
    let Some(allowed) = array(choice, "tools") else {
        return;
    };
    let filtered: Vec<Value> = allowed
        .iter()
        .filter(|tool| trimmed(tool, "type") != tool_type)
        .cloned()
        .collect();
    if filtered.len() == allowed.len() {
        return;
    }
    if filtered.is_empty() {
        let mode = if trimmed(choice, "mode") == "auto" {
            "auto"
        } else {
            "required"
        };
        keep_only_hosted_tools(body, tool_type);
        json::set(body, "tool_choice", Value::from(mode));
        return;
    }
    json::set(body, "tool_choice.tools", Value::Array(filtered));
}

/// `xaiKeepOnlyHostedTools`: drops every tool but those of `tool_type`,
/// unless there are none of those, or nothing else.
fn keep_only_hosted_tools(body: &mut Value, tool_type: &str) {
    let Some(tools) = array(body, "tools") else {
        return;
    };
    let kept: Vec<Value> = tools
        .iter()
        .filter(|tool| trimmed(tool, "type") == tool_type)
        .cloned()
        .collect();
    if kept.is_empty() || kept.len() == tools.len() {
        return;
    }
    json::set(body, "tools", Value::Array(kept));
}

/// `xaiToolChoiceRequiresHostedToolOnly`: whether the choice is
/// `"required"` or `"auto"` and every tool, of at least one, is of
/// `tool_type`.
fn requires_hosted_tool_only(body: &Value, tool_type: &str) -> bool {
    if !matches!(
        body.get("tool_choice").and_then(Value::as_str),
        Some("required" | "auto")
    ) {
        return false;
    }
    array(body, "tools").is_some_and(|tools| {
        !tools.is_empty() && tools.iter().all(|tool| trimmed(tool, "type") == tool_type)
    })
}

/// `xaiToolChoiceRequiresHostedToolOnlyAny`: [`requires_hosted_tool_only`]
/// for the image or the web search tool.
pub(crate) fn requires_hosted_tool_only_any(body: &Value) -> bool {
    requires_hosted_tool_only(body, IMAGE_GENERATION) || requires_hosted_tool_only(body, WEB_SEARCH)
}

/// A tool as a choice names it (`xaiToolChoiceKey`): its type, and a
/// function's or custom tool's name.
type ChoiceKey = (String, String);

/// The key a tool or choice entry is chosen by, if it has one.
fn choice_key(tool: &Value) -> Option<ChoiceKey> {
    let tool_type = trimmed(tool, "type");
    if tool_type.is_empty() {
        return None;
    }
    let name = if tool_type == FUNCTION || tool_type == CUSTOM {
        let name = trimmed(tool, "name");
        if name.is_empty() {
            return None;
        }
        name
    } else {
        String::new()
    };
    Some((tool_type, name))
}

/// `collectXAIAvailableToolChoiceKeys`.
fn available_choice_keys(body: &Value) -> HashSet<ChoiceKey> {
    tool_lists(body)
        .filter_map(|tools| tools.and_then(Value::as_array))
        .flatten()
        .filter_map(choice_key)
        .collect()
}

/// `pruneXAIOrphanedToolChoice`: drops a choice of a tool that isn't there
/// any more, and such entries of an `allowed_tools` choice (the whole
/// choice if none is left).
pub(crate) fn prune_orphaned_tool_choice(body: &mut Value) {
    let Some(choice @ Value::Object(_)) = body.get("tool_choice") else {
        return;
    };
    let available = available_choice_keys(body);
    let matches = |tool: &Value| choice_key(tool).is_some_and(|key| available.contains(&key));
    match trimmed(choice, "type").as_str() {
        "allowed_tools" => {
            let Some(allowed) = array(choice, "tools") else {
                json::delete(body, "tool_choice");
                return;
            };
            let filtered: Vec<Value> = allowed
                .iter()
                .filter(|tool| matches(tool))
                .cloned()
                .collect();
            if filtered.len() == allowed.len() {
                return;
            }
            if filtered.is_empty() {
                json::delete(body, "tool_choice");
            } else {
                json::set(body, "tool_choice.tools", Value::Array(filtered));
            }
        }
        "" => {}
        _ => {
            if !matches(choice) {
                json::delete(body, "tool_choice");
            }
        }
    }
}

/// `xaiCountFlattenedTools`: how many tools a list makes once namespaces
/// are flattened and `tool_search` is dropped.
fn count_flattened(tools: Option<&Value>) -> usize {
    let Some(Value::Array(tools)) = tools else {
        return 0;
    };
    tools
        .iter()
        .map(|tool| match text(tool, "type").as_str() {
            NAMESPACE => array(tool, "tools").map_or(1, Vec::len),
            TOOL_SEARCH => 0,
            _ => 1,
        })
        .sum()
}

/// `xaiShouldFoldNamespaceTools`: whether flattening the namespaces would
/// make more than [`MAX_TOOLS`], counting the X search tool if it is to be
/// added (`will_inject_x_search`).
pub(crate) fn should_fold(body: &Value, will_inject_x_search: bool) -> bool {
    let injected =
        will_inject_x_search && !has_native_x_search(body) && !requires_hosted_tool_only_any(body);
    let total: usize = tool_lists(body).map(count_flattened).sum::<usize>() + usize::from(injected);
    total > MAX_TOOLS
}

/// `buildXAINamespaceDispatcherTool`: a namespace folded into one function,
/// named as the namespace, whose description lists each child with its
/// parameters and which takes the child's `name` and `arguments`.
fn dispatcher(tool: &Value) -> Option<Value> {
    let namespace = trimmed(tool, "name");
    if namespace.is_empty() {
        return None;
    }
    let description = trimmed(tool, "description");
    let mut names = Vec::new();
    let mut entries = Vec::new();
    for child in array(tool, "tools").map_or(&[][..], Vec::as_slice) {
        let name = trimmed(child, "name");
        if name.is_empty() {
            continue;
        }
        let child_description = trimmed(child, "description");
        let parameters = child
            .get("parameters")
            .or_else(|| child.get("input_schema"))
            .map(catalogue_parameters)
            .unwrap_or_default();
        entries.push(
            match (child_description.is_empty(), parameters.is_empty()) {
                (false, false) => {
                    format!("- {name}: {child_description}\n  Parameters: {parameters}")
                }
                (false, true) => format!("- {name}: {child_description}"),
                (true, false) => format!("- {name}\n  Parameters: {parameters}"),
                (true, true) => format!("- {name}"),
            },
        );
        names.push(Value::String(name));
    }
    let full_description = if !entries.is_empty() {
        let catalogue = format!("Available tools in this namespace:\n{}", entries.join("\n"));
        if description.is_empty() {
            format!("Tools in namespace {namespace}.\n\n{catalogue}")
        } else {
            format!("{description}\n\n{catalogue}")
        }
    } else if description.is_empty() {
        format!("Tools in namespace {namespace}.")
    } else {
        description
    };
    // Upstream marshals Go maps, so every object's keys are sorted.
    let mut name_property = Map::new();
    name_property.insert(
        "description".to_owned(),
        Value::from(format!(
            "Child tool name to execute in namespace {namespace}"
        )),
    );
    if !names.is_empty() {
        name_property.insert("enum".to_owned(), Value::Array(names));
    }
    name_property.insert("type".to_owned(), Value::from("string"));
    Some(json!({
        "description": full_description,
        "name": namespace,
        "parameters": {
            "properties": {
                "arguments": {
                    "additionalProperties": true,
                    "description": "Arguments object matching the parameter schema of the selected child tool",
                    "type": "object",
                },
                "name": name_property,
            },
            "required": ["name"],
            "type": "object",
        },
        "type": FUNCTION,
    }))
}

/// A child's parameters as a dispatcher's description lists them: local
/// references inlined and the definitions dropped; nothing for an empty
/// object schema. Those too large to inline are listed as the permissive
/// object schema (see [`schema::safe_function_parameters`]).
fn catalogue_parameters(parameters: &Value) -> String {
    let raw = parameters.to_string();
    if raw == "{}" || raw == r#"{"type":"object","properties":{}}"# {
        return String::new();
    }
    let mut cleaned = match schema::inline_local_refs(parameters) {
        Inlined::Schema(inlined) => inlined,
        Inlined::Unchanged => parameters.clone(),
        Inlined::TooLarge => return schema::safe_function_parameters().to_string(),
    };
    if let Value::Object(object) = &mut cleaned {
        object.shift_remove("$defs");
        object.shift_remove("definitions");
    }
    cleaned.to_string()
}

/// `normalizeXAIToolsWithFold`: reshapes the tools and each
/// `additional_tools` item's, flattening namespaces or, if `fold`, folding
/// them. Changes nothing if a namespace's function has no name.
pub(crate) fn normalize_tools(body: &mut Value, fold: bool) {
    let keep_image_generation = supports_native_image_generation(&text(body, "model"));
    let tools = match body.get("tools") {
        Some(Value::Array(tools)) => match normalize_tool_array(tools, keep_image_generation, fold)
        {
            Some(tools) => Some(tools),
            None => return,
        },
        _ => None,
    };
    let mut additional = Vec::new();
    for (index, item) in input_items(body).iter().enumerate() {
        if text(item, "type") != ADDITIONAL_TOOLS {
            continue;
        }
        if let Some(Value::Array(tools)) = item.get("tools") {
            match normalize_tool_array(tools, keep_image_generation, fold) {
                Some(tools) => additional.push((index, tools)),
                None => return,
            }
        }
    }
    if let Some(tools) = tools {
        json::set(body, "tools", Value::Array(tools));
    }
    for (index, tools) in additional {
        json::set(body, &format!("input.{index}.tools"), Value::Array(tools));
    }
}

/// `normalizeXAIToolArray`; `None` if a tool can't be reshaped.
fn normalize_tool_array(
    tools: &[Value],
    keep_image_generation: bool,
    fold: bool,
) -> Option<Vec<Value>> {
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        if text(tool, "type") != NAMESPACE {
            out.extend(normalize_tool(tool, "", keep_image_generation)?);
            continue;
        }
        if fold {
            out.extend(dispatcher(tool));
            continue;
        }
        let namespace = text(tool, "name");
        for child in array(tool, "tools").map_or(&[][..], Vec::as_slice) {
            out.extend(normalize_tool(child, &namespace, keep_image_generation)?);
        }
    }
    Some(out)
}

/// `normalizeXAITool`: a tool as xAI takes it, in `namespace` (`""` for
/// none). `Some(None)` drops it; `None` means it can't be reshaped (a
/// namespace's function without a name).
fn normalize_tool(
    tool: &Value,
    namespace: &str,
    keep_image_generation: bool,
) -> Option<Option<Value>> {
    let tool_type = text(tool, "type");
    if tool_type == TOOL_SEARCH || (tool_type == IMAGE_GENERATION && !keep_image_generation) {
        return Some(None);
    }
    let mut out = tool.clone();
    let is_custom = tool_type == CUSTOM;
    let callable = tool_type == FUNCTION || is_custom;
    // Parameters with references too large to inline are simplified.
    let mut too_large = false;
    if callable {
        match out.get("parameters").map(schema::inline_local_refs) {
            Some(Inlined::Schema(inlined)) => {
                json::set(&mut out, "parameters", inlined);
                if let Some(Value::Object(parameters)) = out.get_mut("parameters") {
                    parameters.shift_remove("$defs");
                    parameters.shift_remove("definitions");
                }
            }
            Some(Inlined::TooLarge) => too_large = true,
            Some(Inlined::Unchanged) | None => {}
        }
        if schema::type_root_union_branches(&mut out) {
            tracing::debug!(
                "xai: added object types to root union branches for tool {namespace}.{}",
                text(tool, "name")
            );
        }
    }
    // Read before the custom tool becomes a function and gets parameters.
    let has_parameters = out.get("parameters").is_some();
    let simplify = callable && (too_large || schema::needs_simplification(&out, namespace));
    if is_custom {
        json::set(&mut out, "type", Value::from(FUNCTION));
    }
    if tool_type == WEB_SEARCH && tool.get("external_web_access").is_some() {
        json::delete(&mut out, "external_web_access");
    }
    if callable && !has_parameters {
        json::set(
            &mut out,
            "parameters",
            json!({"type": "object", "properties": {}}),
        );
    }
    if simplify {
        json::set(&mut out, "parameters", schema::safe_function_parameters());
        if json::bool_of(tool.get("strict")) {
            json::set(&mut out, "strict", Value::Bool(false));
        }
        tracing::debug!(
            "xai: simplified parameters for tool {namespace}.{} to avoid upstream schema rejection or hang",
            text(tool, "name")
        );
    }
    if callable && !namespace.trim().is_empty() {
        let qualified = qualify(namespace, &text(tool, "name"));
        if qualified.is_empty() {
            return None;
        }
        json::set(&mut out, "name", Value::String(qualified));
    }
    Some(Some(out))
}

/// `qualifyXAINamespaceToolName`: `<namespace>__<tool>`, unless the tool's
/// name already starts so or with `mcp__`, or either is empty.
pub(crate) fn qualify(namespace: &str, tool: &str) -> String {
    let namespace = namespace.trim();
    let tool = tool.trim();
    if namespace.is_empty() || tool.is_empty() || tool.starts_with("mcp__") {
        return tool.to_owned();
    }
    let prefix = if namespace.ends_with("__") {
        namespace.to_owned()
    } else {
        format!("{namespace}__")
    };
    if tool.starts_with(&prefix) {
        tool.to_owned()
    } else {
        format!("{prefix}{tool}")
    }
}

/// `xaiHasFunctionToolNamed`: whether a function in the tools, or in an
/// `additional_tools` item, has the name.
fn has_function_tool_named(body: &Value, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let named = |tool: &Value| text(tool, "type") == FUNCTION && text(tool, "name") == name;
    array(body, "tools").is_some_and(|tools| tools.iter().any(named))
        || additional_tool_lists(body).any(|tools| elements(tools).iter().any(named))
}

/// `clampXAIToolsLimit`: keeps at most `max` tools, dispatchers first, then
/// fixes the choice up.
pub(crate) fn clamp_tools(body: &mut Value, max: usize, refs: &NamespaceRefs) {
    let Some(tools) = array(body, "tools") else {
        return;
    };
    if tools.len() <= max {
        return;
    }
    let is_dispatcher = |tool: &&Value| {
        refs.get(&trimmed(tool, "name"))
            .is_some_and(|reference| reference.is_dispatcher)
    };
    let (dispatchers, regular): (Vec<&Value>, Vec<&Value>) = tools.iter().partition(is_dispatcher);
    let capped: Vec<Value> = dispatchers
        .into_iter()
        .chain(regular)
        .take(max)
        .cloned()
        .collect();
    json::set(body, "tools", Value::Array(capped));
    prune_orphaned_tool_choice(body);
    normalize_tool_choice_for_tools(body);
}

/// `promoteXAIAdditionalTools`: moves the tools of `additional_tools` input
/// items, which xAI doesn't take, to the end of the top-level tools.
pub(crate) fn promote_additional_tools(body: &mut Value) {
    if !input_items(body)
        .iter()
        .any(|item| text(item, "type") == ADDITIONAL_TOOLS)
    {
        return;
    }
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return;
    };
    let mut promoted = Vec::new();
    input.retain(|item| {
        if text(item, "type") != ADDITIONAL_TOOLS {
            return true;
        }
        promoted.extend(elements(item.get("tools")).iter().cloned());
        false
    });
    if promoted.is_empty() {
        return;
    }
    let mut tools = match body.get_mut("tools") {
        Some(Value::Array(tools)) => std::mem::take(tools),
        _ => Vec::new(),
    };
    tools.extend(promoted);
    json::set(body, "tools", Value::Array(tools));
}

/// `normalizeXAIToolChoiceForTools`: with no tools, here or in an
/// `additional_tools` item, drops `tools`, `tool_choice` and
/// `parallel_tool_calls`, which xAI refuses without tools.
pub(crate) fn normalize_tool_choice_for_tools(body: &mut Value) {
    let non_empty = |tools: Option<&Value>| {
        tools
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
    };
    if non_empty(body.get("tools")) || additional_tool_lists(body).any(non_empty) {
        return;
    }
    for field in ["tools", "tool_choice", "parallel_tool_calls"] {
        json::delete(body, field);
    }
}

/// `normalizeXAINamespaceToolChoiceWithFold`: a function choice, or
/// `allowed_tools` entry, with a `namespace` names the tool as it is sent:
/// the dispatcher if the namespace is folded (or `fold` and neither is
/// declared), else the qualified name; xAI takes no `namespace` there.
pub(crate) fn normalize_namespace_tool_choice(body: &mut Value, fold: bool) {
    let mut paths = vec!["tool_choice".to_owned()];
    if let Some(Value::Array(allowed)) = json::get(body, "tool_choice.tools") {
        paths.extend((0..allowed.len()).map(|index| format!("tool_choice.tools.{index}")));
    }
    for path in paths {
        let Some(choice @ Value::Object(_)) = json::get(body, &path) else {
            continue;
        };
        if text(choice, "type") != FUNCTION {
            continue;
        }
        let namespace = trimmed(choice, "namespace");
        if namespace.is_empty() {
            continue;
        }
        let qualified = qualify(&namespace, &trimmed(choice, "name"));
        let target = if has_function_tool_named(body, &namespace) {
            namespace
        } else if has_function_tool_named(body, &qualified) || !fold {
            qualified
        } else {
            namespace
        };
        if target.is_empty() {
            continue;
        }
        json::set(body, &format!("{path}.name"), Value::String(target));
        json::delete(body, &format!("{path}.namespace"));
    }
}

/// `collectXAINamespaceToolRefsWithFold`: each namespace's flattened child
/// names, and its dispatcher if `fold`.
pub(crate) fn collect_namespace_refs(body: &Value, fold: bool) -> NamespaceRefs {
    let mut refs = NamespaceRefs::new();
    for tools in tool_lists(body).filter_map(|tools| tools.and_then(Value::as_array)) {
        for tool in tools {
            if text(tool, "type") != NAMESPACE {
                continue;
            }
            let namespace = trimmed(tool, "name");
            if namespace.is_empty() {
                continue;
            }
            if fold {
                refs.insert(
                    namespace.clone(),
                    NamespaceRef {
                        namespace: namespace.clone(),
                        name: String::new(),
                        is_dispatcher: true,
                    },
                );
            }
            for child in elements(tool.get("tools")) {
                let name = trimmed(child, "name");
                let qualified = qualify(&namespace, &name);
                if qualified.is_empty() {
                    continue;
                }
                refs.insert(
                    qualified,
                    NamespaceRef {
                        namespace: namespace.clone(),
                        name,
                        is_dispatcher: false,
                    },
                );
            }
        }
    }
    refs
}

/// `collectXAIClientDeclaredToolKeys`: the function and custom tools the
/// client declared, with their namespaces, before namespaces are
/// flattened.
pub(crate) fn collect_client_declared_tool_keys(body: &Value) -> HashSet<ClientToolKey> {
    let mut keys = HashSet::new();
    let mut add = |namespace: &str, tool: &Value| {
        let tool_type = trimmed(tool, "type");
        let name = trimmed(tool, "name");
        if (tool_type == FUNCTION || tool_type == CUSTOM) && !name.is_empty() {
            keys.insert(ClientToolKey {
                namespace: namespace.to_owned(),
                name,
                tool_type: FUNCTION.to_owned(),
            });
        }
    };
    for tools in tool_lists(body).filter_map(|tools| tools.and_then(Value::as_array)) {
        for tool in tools {
            if trimmed(tool, "type") != NAMESPACE {
                add("", tool);
                continue;
            }
            let namespace = trimmed(tool, "name");
            if namespace.is_empty() {
                continue;
            }
            for child in elements(tool.get("tools")) {
                add(&namespace, child);
            }
        }
    }
    keys
}

/// `normalizeXAIInputCustomToolCalls`: past custom tool calls and their
/// outputs become function calls and outputs, which is how xAI saw them;
/// one without a call ID (or a call without a name) is dropped.
pub(crate) fn normalize_input_custom_tool_calls(body: &mut Value) {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return;
    };
    let items = std::mem::take(input);
    for item in items {
        match text(&item, "type").as_str() {
            "custom_tool_call" => {
                let call_id = trimmed(&item, "call_id");
                let name = trimmed(&item, "name");
                if call_id.is_empty() || name.is_empty() {
                    continue;
                }
                input.push(json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": custom_tool_call_arguments(item.get("input")),
                }));
            }
            "custom_tool_call_output" => {
                let call_id = trimmed(&item, "call_id");
                if call_id.is_empty() {
                    continue;
                }
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": custom_tool_call_output(item.get("output")),
                }));
            }
            _ => input.push(item),
        }
    }
}

/// `xaiCustomToolCallArguments`: a custom call's `input` as function
/// arguments: a string holding a JSON object as that object, any other
/// string or value as `{"input": …}`.
fn custom_tool_call_arguments(input: Option<&Value>) -> String {
    match input {
        None => "{}".to_owned(),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.starts_with('{') && go::gjson_valid(trimmed.as_bytes()) {
                trimmed.to_owned()
            } else {
                format!("{{\"input\":{}}}", go::json_string(text))
            }
        }
        Some(object @ Value::Object(_)) => object.to_string(),
        Some(other) => format!("{{\"input\":{other}}}"),
    }
}

/// `xaiCustomToolCallOutput`: a string as it is, any other value as JSON.
fn custom_tool_call_output(output: Option<&Value>) -> String {
    match output {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// `normalizeXAIInputNamespaceToolCallsWithFold`: a past function call with
/// a `namespace` names the tool as it is sent. A folded namespace's call
/// goes to its dispatcher, the child's name and arguments in its
/// arguments.
pub(crate) fn normalize_input_namespace_tool_calls(body: &mut Value, fold: bool) {
    let mut edits = Vec::new();
    for (index, item) in input_items(body).iter().enumerate() {
        if text(item, "type") != "function_call" {
            continue;
        }
        let namespace = trimmed(item, "namespace");
        if namespace.is_empty() {
            continue;
        }
        let name = trimmed(item, "name");
        let qualified = qualify(&namespace, &name);
        let folded = if has_function_tool_named(body, &namespace) {
            true
        } else if has_function_tool_named(body, &qualified) {
            false
        } else {
            fold
        };
        if folded {
            edits.push((
                index,
                namespace,
                Some(dispatcher_arguments(name, &text(item, "arguments"))),
            ));
        } else if !qualified.is_empty() {
            edits.push((index, qualified, None));
        }
    }
    for (index, name, arguments) in edits {
        json::set(body, &format!("input.{index}.name"), Value::String(name));
        if let Some(arguments) = arguments {
            json::set(
                body,
                &format!("input.{index}.arguments"),
                Value::String(arguments),
            );
        }
        json::delete(body, &format!("input.{index}.namespace"));
    }
}

/// A dispatcher call's arguments: the child's `name` and, if any, its
/// `arguments`, as JSON if they are, else as a string. Keys are sorted, as
/// upstream marshals a Go map; the arguments' numbers keep their text, as
/// upstream's `json.RawMessage` does.
fn dispatcher_arguments(name: String, arguments: &str) -> String {
    let mut out = Map::new();
    if !arguments.is_empty() {
        let value = go::gjson_valid(arguments.as_bytes())
            .then(|| exact::from_str(arguments).ok())
            .flatten()
            .unwrap_or_else(|| Value::from(arguments));
        out.insert("arguments".to_owned(), value);
    }
    out.insert("name".to_owned(), Value::String(name));
    Value::Object(out).to_string()
}

#[cfg(test)]
mod tests;
