// Ported from CLIProxyAPI internal/translator/openai/openai/responses/openai_openai-responses_tools.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The tools a Responses request declares, as Chat Completions functions.
//!
//! Tools can be declared at the top level, in `additional_tools` input items,
//! and inside namespaces, whose children are named `<namespace>__<child>`.
//! Strict Chat Completions upstreams limit function names to 64 bytes, so a
//! longer name keeps its last 64, where the child's own name is. Declarations
//! that only collide once cut get `_1`-style suffixes. A custom (freeform) tool
//! becomes a function with one string argument, `input`, and the client's
//! local shell a function named so that it is none of the other tools' names
//! (see [`super::shell_tool`]).
//!
//! The lookups by name, including those that map a call's name back for the
//! response translator, are in [`super::tool_index`].
//!
//! Deviations from upstream:
//! - Names cut to 64 bytes start at a character boundary (see [`cap`]).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::shell_tool;
use crate::apply_patch;
use crate::json::lenient::{self, Found};
use crate::json::{go_value, object, path, str_of};

/// `responsesChatToolNameLimit`: the longest function name strict Chat
/// Completions upstreams accept, in bytes.
const NAME_LIMIT: usize = 64;

/// One function, custom tool or local shell declaration and the Chat
/// Completions name it gets (`responsesToolDeclaration`).
pub(super) struct Declaration<'a> {
    pub(super) tool: &'a Value,
    pub(super) chat_name: String,
    /// The tool's own name, trimmed.
    pub(super) local_name: String,
    /// The namespace it was declared in, trimmed, or `""`.
    pub(super) namespace: String,
    pub(super) custom: bool,
    /// Whether it is the client's local shell.
    pub(super) shell: bool,
}

/// `walkResponsesToolDeclarations`: the request's function and custom tools
/// and local shell, top-level `tools` first, then those in `additional_tools`
/// input items, with namespace children in place of their namespace.
pub(super) fn declarations(request: &Value) -> Vec<Declaration<'_>> {
    let mut declarations = Vec::new();
    scan(request.get("tools"), &mut declarations);
    if let Some(Value::Array(input)) = request.get("input") {
        for item in input {
            if str_of(item.get("type")) == "additional_tools" {
                scan(item.get("tools"), &mut declarations);
            }
        }
    }
    name_shell(&mut declarations);
    disambiguate(&mut declarations);
    declarations
}

/// Names the local shell's declarations with the first shell function name
/// that no other declaration has as its local, Chat Completions or uncut
/// qualified name.
fn name_shell(declarations: &mut [Declaration<'_>]) {
    if !declarations.iter().any(|declaration| declaration.shell) {
        return;
    }
    let reserved: HashSet<String> = declarations
        .iter()
        .filter(|declaration| !declaration.shell)
        .flat_map(|declaration| {
            [
                declaration.local_name.clone(),
                declaration.chat_name.clone(),
                raw_qualified_name(&declaration.namespace, &declaration.local_name),
            ]
        })
        .collect();
    let name = shell_tool::unreserved_name(&reserved);
    for declaration in declarations
        .iter_mut()
        .filter(|declaration| declaration.shell)
    {
        declaration.local_name.clone_from(&name);
        declaration.chat_name.clone_from(&name);
    }
}

fn scan<'a>(tools: Option<&'a Value>, declarations: &mut Vec<Declaration<'a>>) {
    let Some(Value::Array(tools)) = tools else {
        return;
    };
    for tool in tools {
        if str_of(tool.get("type")).trim() != "namespace" {
            emit(tool, "", declarations);
            continue;
        }
        if let Some(Value::Array(children)) = tool.get("tools") {
            let namespace = str_of(tool.get("name")).trim().to_owned();
            for child in children {
                emit(child, &namespace, declarations);
            }
        }
    }
}

fn emit<'a>(tool: &'a Value, namespace: &str, declarations: &mut Vec<Declaration<'a>>) {
    let (custom, shell) = match str_of(tool.get("type")).trim() {
        "" | "function" => (false, false),
        "custom" => (true, false),
        "shell" if shell_tool::is_local_shell(tool, namespace) => (false, true),
        _ => return,
    };
    let local_name = if shell {
        shell_tool::SHELL_NAME.to_owned()
    } else {
        tool_name(tool)
    };
    if local_name.is_empty() {
        return;
    }
    declarations.push(Declaration {
        tool,
        chat_name: qualify(namespace, &local_name),
        local_name,
        namespace: namespace.to_owned(),
        custom,
        shell,
    });
}

/// `disambiguateResponsesChatToolNames`: renames declarations whose names
/// were cut to 64 bytes when the cut name is taken. A declaration's identity
/// is its uncut qualified name; declarations with the same identity are one
/// tool, delivered twice, and share a name.
///
/// Names that fit are claimed first, and so are local names that fit, since a
/// call that leaves out its namespace names the tool by its local name. A
/// local name that more than one identity declares can't be resolved, so no
/// cut name may become it either.
fn disambiguate(declarations: &mut [Declaration<'_>]) {
    // Each name claimed, with the identity that owns it.
    let mut claimed: HashMap<String, String> = HashMap::new();
    let mut claim = |candidate: &str, identity: &str| match claimed.get(candidate) {
        Some(owner) => owner == identity,
        None => {
            claimed.insert(candidate.to_owned(), identity.to_owned());
            true
        }
    };

    let mut long = Vec::new();
    let mut identities = Vec::with_capacity(declarations.len());
    // Each local name with the one identity that declares it, or `""` once a
    // second identity does.
    let mut local_owners: HashMap<&str, String> = HashMap::new();
    for (i, declaration) in declarations.iter().enumerate() {
        let identity = raw_qualified_name(&declaration.namespace, &declaration.local_name);
        if identity.len() > NAME_LIMIT {
            long.push(i);
        } else {
            claim(&identity, &identity);
        }
        let local = declaration.local_name.as_str();
        if !local.is_empty() && local != identity && local.len() <= NAME_LIMIT {
            match local_owners.get_mut(local) {
                None => {
                    local_owners.insert(local, identity.clone());
                }
                Some(owner) if !owner.is_empty() && *owner != identity => owner.clear(),
                Some(_) => {}
            }
        }
        identities.push(identity);
    }

    // Claiming a local name keeps it from every cut name; an ambiguous one is
    // claimed by no identity, so no declaration gets it.
    let mut ambiguous = HashSet::new();
    for (local, owner) in &local_owners {
        claim(local, owner);
        if owner.is_empty() {
            ambiguous.insert(*local);
        }
    }
    let mut renames = Vec::new();
    for i in long {
        let identity = &identities[i];
        let name = &declarations[i].chat_name;
        if !ambiguous.contains(name.as_str()) && claim(name, identity) {
            continue;
        }
        for suffix in 1_u64.. {
            let candidate = cap(&format!("{name}_{suffix}"));
            if ambiguous.contains(candidate.as_str()) {
                continue;
            }
            if claim(&candidate, identity) {
                renames.push((i, candidate));
                break;
            }
        }
    }
    for (i, name) in renames {
        declarations[i].chat_name = name;
    }
}

/// `convertResponsesFunctionToolToOpenAIChat`, or for a custom tool
/// `convertResponsesCustomToolToOpenAIChat`: the Chat Completions function
/// for a declaration, named `chat_name`. Its keys are sorted, as Go writes the
/// map upstream reads it into, except within the client's `parameters`.
pub(super) fn chat_tool(tool: &Value, chat_name: &str, custom: bool) -> Option<Value> {
    let name = match chat_name.trim() {
        "" => tool_name(tool),
        name => name.to_owned(),
    };
    if name.is_empty() {
        return None;
    }
    let mut description = tool_description(tool);
    let parameters = if !custom {
        tool_parameters(tool).cloned().unwrap_or_else(|| json!({}))
    } else if apply_patch::is_custom_tool(tool) {
        description = apply_patch::description(tool);
        go_value(&apply_patch::parameters())
    } else {
        json!({
            "properties": {"input": {"type": "string"}},
            "required": ["input"],
            "type": "object"
        })
    };
    Some(object([
        (
            "function",
            object([
                ("description", description.into()),
                ("name", name.into()),
                ("parameters", parameters),
            ]),
        ),
        ("type", "function".into()),
    ]))
}

/// `responsesToolName`: `name`, or else `function.name`, trimmed.
fn tool_name(tool: &Value) -> String {
    let name = str_of(tool.get("name"));
    if !name.trim().is_empty() {
        return name.trim().to_owned();
    }
    str_of(path(tool, "function.name")).trim().to_owned()
}

/// `responsesToolDescription`: `description`, or else
/// `function.description`.
fn tool_description(tool: &Value) -> String {
    let description = str_of(tool.get("description"));
    if !description.is_empty() {
        return description.into_owned();
    }
    str_of(path(tool, "function.description")).into_owned()
}

/// `responsesToolParameters`: the first schema declared.
fn tool_parameters(tool: &Value) -> Option<&Value> {
    [
        "parameters",
        "parametersJsonSchema",
        "input_schema",
        "function.parameters",
        "function.parametersJsonSchema",
    ]
    .into_iter()
    .find_map(|key| path(tool, key))
}

/// `responsesToolOutputText`: a custom tool's output as text. Text parts are
/// joined; any other value is its JSON.
pub(super) fn tool_output_text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part {
                Value::String(text) => Cow::Borrowed(text.as_str()),
                part => str_of(part.get("text")),
            })
            .collect(),
        other => other.to_string(),
    }
}

/// `unwrapCustomToolInput`: the freeform input in the arguments
/// `{"input": "..."}` of a call to a custom tool, read as gjson reads it,
/// even from malformed JSON. An `input` that isn't a string is kept as
/// written. Arguments gjson finds no `input` in are returned as they are.
pub(super) fn unwrap_custom_tool_input(arguments: &str) -> String {
    match lenient::get(arguments, "input") {
        Some(Found::String(input)) => input,
        Some(Found::Number(input) | Found::Literal(input) | Found::Json(input)) => input.to_owned(),
        None => arguments.to_owned(),
    }
}

/// `qualifyResponsesNamespaceToolName`: a namespace child's full name, cut to
/// the name limit.
pub(super) fn qualify(namespace: &str, child: &str) -> String {
    cap(&raw_qualified_name(namespace, child))
}

/// `rawResponsesNamespaceQualifiedName`: a namespace child's full name. The
/// namespace is taken as given; the child's name is trimmed.
pub(super) fn raw_qualified_name(namespace: &str, child: &str) -> String {
    let child = child.trim();
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

/// `capResponsesChatToolName`: a name cut to its last 64 bytes, without the
/// `_` and `-` the cut leaves at its start.
///
/// Upstream cuts bytes, and can leave part of a character at the start, which
/// its JSON encoder writes as U+FFFD; it then has no separators to trim. We
/// start at the next whole character instead and trim nothing either.
pub(super) fn cap(name: &str) -> String {
    if name.len() <= NAME_LIMIT {
        return name.to_owned();
    }
    let mut start = name.len() - NAME_LIMIT;
    if !name.is_char_boundary(start) {
        while !name.is_char_boundary(start) {
            start += 1;
        }
        return name[start..].to_owned();
    }
    let cut = &name[start..];
    match cut.trim_start_matches(['_', '-']) {
        "" => cut.to_owned(),
        trimmed => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_keeps_the_tail() {
        let name = format!("mcp__{}__tool", "x".repeat(70));
        let capped = cap(&name);
        assert_eq!(capped.len(), NAME_LIMIT);
        assert!(capped.ends_with("__tool"));
        assert_eq!(cap("short"), "short");
    }

    #[test]
    fn cap_trims_separators_left_at_the_start() {
        let name = format!("{}__{}", "a".repeat(10), "b".repeat(62));
        assert_eq!(cap(&name), "b".repeat(62));
        let name = format!("{}{}", "a".repeat(10), "_".repeat(64));
        assert_eq!(cap(&name), "_".repeat(64));
    }

    #[test]
    fn cap_starts_at_a_whole_character() {
        // 63 bytes of ASCII after a 2-byte character: the cut lands inside it.
        let name = format!("{}é_{}", "a".repeat(10), "b".repeat(62));
        assert_eq!(cap(&name), format!("_{}", "b".repeat(62)));
    }

    #[test]
    fn raw_qualified_names() {
        assert_eq!(raw_qualified_name("", "tool"), "tool");
        assert_eq!(raw_qualified_name("ns", " tool "), "ns__tool");
        assert_eq!(raw_qualified_name("ns__", "tool"), "ns__tool");
        assert_eq!(raw_qualified_name("ns", "ns__tool"), "ns__tool");
        assert_eq!(raw_qualified_name("ns", "ns"), "ns");
        assert_eq!(raw_qualified_name("ns", "mcp__x__tool"), "mcp__x__tool");
        assert_eq!(raw_qualified_name(" ns", "tool"), " ns__tool");
    }
}
