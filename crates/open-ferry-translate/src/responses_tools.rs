// Ported from CLIProxyAPI internal/util/responses_tools.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which declaration a tool name refers to in a Responses-style request.
//!
//! Tools can be declared at the top level, in `additional_tools` input items,
//! and inside namespaces, whose children are named `<namespace>__<child>`. When
//! a name is declared more than once, one declaration wins.
//!
//! Only what the Chat Completions translator needs is ported so far. The rest
//! of upstream's file builds Gemini function declarations.

use std::collections::HashMap;

use serde_json::Value;

use crate::json::str_of;

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

/// One tool declaration, as `ResponsesToolDescriptor` (with only the fields
/// used so far).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolDescriptor<'v> {
    pub tool: &'v Value,
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
fn collect_tool_descriptors(root: &Value) -> Vec<(String, ToolDescriptor<'_>)> {
    let mut descriptors = Vec::new();
    let mut add = |name: String, tool, source_priority, direct| {
        if name.is_empty() {
            return;
        }
        let order = descriptors.len();
        descriptors.push((
            name,
            ToolDescriptor {
                tool,
                source_priority,
                direct,
                order,
            },
        ));
    };
    for (tools, priority) in tool_sources(root) {
        for tool in tools {
            match str_of(tool.get("type")).trim() {
                "" | "function" | "custom" => add(tool_name(tool), tool, priority, true),
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
                            add(name, child, priority, false);
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
    let mut winners = HashMap::<String, ToolDescriptor<'_>>::new();
    for (name, descriptor) in collect_tool_descriptors(root) {
        match winners.get(&name) {
            Some(current) if !descriptor.precedes(current) => {}
            _ => {
                winners.insert(name, descriptor);
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
}
