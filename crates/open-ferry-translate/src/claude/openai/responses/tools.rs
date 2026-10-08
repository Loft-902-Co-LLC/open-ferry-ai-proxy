// Ported from CLIProxyAPI internal/translator/claude/openai/responses/claude_openai-responses_tool_names.go
// and the tool declaration code in claude_openai-responses_request.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The tools a Responses request declares, and the names Claude knows them
//! by.
//!
//! Tools can be declared at the top level, in `additional_tools` input items,
//! and inside namespaces, whose children are named `<namespace>__<child>`.
//! When a name is declared more than once, one declaration wins: top-level
//! tools beat `additional_tools`, then direct declarations beat namespace
//! children, then earlier beats later.
//!
//! Claude tool names must match `^[a-zA-Z0-9_-]{1,64}$`. A name that already
//! does keeps it; any other gets its sanitized form, or, when two would
//! sanitize alike or the sanitized name is taken, the sanitized form cut to 53
//! bytes plus `_` and 10 hex digits of the name's SHA-256. The request and
//! response translators build the same names from the same request, so a
//! Claude name can be turned back into the client's.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::common::claude::sanitize_function_name;
use crate::json::{bool_of, path, str_of};

/// One tool declaration (`responsesToolDescriptor`).
#[derive(Clone, Debug)]
pub(super) struct Descriptor {
    /// The full name: a namespace child's is qualified.
    pub(super) name: String,
    /// A namespace child's own name; empty for a direct declaration.
    pub(super) child_name: String,
    /// A namespace child's namespace, trimmed.
    pub(super) namespace: String,
    /// `function`, `custom`, `web_search`, or another type as declared.
    pub(super) kind: String,
    pub(super) tool: Value,
    /// 0 for top-level tools, 1 for `additional_tools`.
    pub(super) source_priority: u8,
    /// False for a namespace child.
    pub(super) direct: bool,
    /// The order the declaration was found in.
    pub(super) order: usize,
}

impl Descriptor {
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

/// A request's tool declarations, which of them win, and their Claude names.
pub(super) struct RequestTools {
    descriptors: Vec<Descriptor>,
    /// Each name's winning declaration, as an index into `descriptors`.
    winners: HashMap<String, usize>,
    to_claude: HashMap<String, String>,
    from_claude: HashMap<String, String>,
}

impl RequestTools {
    pub(super) fn new(request: &Value) -> Self {
        let descriptors = descriptors(request);
        let mut winners = HashMap::<String, usize>::new();
        for (index, descriptor) in descriptors.iter().enumerate() {
            match winners.get(&descriptor.name) {
                Some(&current) if !descriptor.precedes(&descriptors[current]) => {}
                _ => {
                    winners.insert(descriptor.name.clone(), index);
                }
            }
        }
        let mut tools = Self {
            descriptors,
            winners,
            to_claude: HashMap::new(),
            from_claude: HashMap::new(),
        };
        tools.build_names(request);
        tools
    }

    /// `buildClaudeToolNamesWithWinners`: names other tools, such as web
    /// search, keep as they are; then declared function and custom tools; then
    /// the tools of earlier calls in the conversation.
    fn build_names(&mut self, request: &Value) {
        let mut taken = HashSet::new();
        let mut declared = Vec::new();
        let winning: Vec<(String, bool)> = self
            .winning()
            .map(|d| (d.name.clone(), matches!(&*d.kind, "function" | "custom")))
            .collect();
        for (name, is_function) in winning {
            if is_function {
                declared.push(name);
            } else {
                self.assign(&name, &name, &mut taken);
            }
        }
        self.allocate(&declared, &mut taken);
        self.allocate(&history_tool_identities(request), &mut taken);
    }

    fn assign(&mut self, identity: &str, name: &str, taken: &mut HashSet<String>) {
        self.to_claude.insert(identity.to_owned(), name.to_owned());
        self.from_claude
            .insert(name.to_owned(), identity.to_owned());
        taken.insert(name.to_owned());
    }

    fn allocate(&mut self, identities: &[String], taken: &mut HashSet<String>) {
        let mut changed = Vec::new();
        let mut seen = HashSet::new();
        for id in identities {
            if self.to_claude.contains_key(id) || id.is_empty() || !seen.insert(id) {
                continue;
            }
            if is_valid_claude_tool_name(id) && !taken.contains(id) {
                self.assign(id, id, taken);
                continue;
            }
            changed.push(id);
        }
        let mut count = HashMap::<String, usize>::new();
        for id in &changed {
            *count.entry(sanitize_function_name(id)).or_default() += 1;
        }
        let mut hashed = Vec::new();
        for id in changed {
            let base = sanitize_function_name(id);
            if count[&base] == 1 && !taken.contains(&base) {
                self.assign(id, &base, taken);
            } else {
                hashed.push(id);
            }
        }
        hashed.sort();
        for id in hashed {
            let mut base = sanitize_function_name(id);
            // Sanitized names are ASCII, so any byte is a boundary.
            base.truncate(53);
            for n in 0.. {
                let mut seed = id.clone();
                if n > 0 {
                    seed.push_str(&format!("\x00{n}"));
                }
                let sum = Sha256::digest(seed.as_bytes());
                let hex: String = sum[..5].iter().map(|b| format!("{b:02x}")).collect();
                let name = format!("{base}_{hex}");
                if !taken.contains(&name) {
                    self.assign(id, &name, taken);
                    break;
                }
            }
        }
    }

    /// The winning declarations, in the order they were found.
    pub(super) fn winning(&self) -> impl Iterator<Item = &Descriptor> {
        self.descriptors
            .iter()
            .enumerate()
            .filter(|(index, d)| self.winners.get(&d.name) == Some(index))
            .map(|(_, d)| d)
    }

    /// The winning declaration of `name`.
    pub(super) fn winner(&self, name: &str) -> Option<&Descriptor> {
        self.winners
            .get(name)
            .map(|&index| &self.descriptors[index])
    }

    /// The one declared name, if exactly one is declared.
    pub(super) fn only_name(&self) -> Option<&str> {
        if self.winners.len() != 1 {
            return None;
        }
        self.winners.keys().next().map(String::as_str)
    }

    /// The Claude name for a qualified Responses name. One the request doesn't
    /// mention is just sanitized.
    pub(super) fn claude_name(&self, identity: &str) -> String {
        match self.to_claude.get(identity) {
            Some(name) => name.clone(),
            None => sanitize_function_name(identity),
        }
    }

    /// The qualified Responses name for a Claude name. Unknown names are
    /// returned as they are.
    pub(super) fn identity<'a>(&'a self, claude_name: &'a str) -> &'a str {
        self.from_claude
            .get(claude_name)
            .map_or(claude_name, String::as_str)
    }

    /// `responsesToolNameMap`: maps each accepted name, and each namespace
    /// child's own name that no direct declaration owns, to its qualified
    /// name.
    pub(super) fn name_map(&self, accepted: &HashSet<String>) -> HashMap<String, String> {
        let mut map = HashMap::new();
        // Direct names win over namespace children's, whatever the order.
        for d in self.winning() {
            if d.direct && accepted.contains(&d.name) {
                map.insert(d.name.clone(), d.name.clone());
            }
        }
        for d in self.winning() {
            if d.direct || d.child_name.is_empty() || !accepted.contains(&d.name) {
                continue;
            }
            map.entry(d.child_name.clone())
                .or_insert_with(|| d.name.clone());
        }
        map
    }

    /// `responsesCustomToolNames`: the declared custom tools, by qualified and
    /// by Claude name.
    #[cfg(test)]
    pub(super) fn custom_names(&self) -> HashSet<String> {
        self.winning()
            .filter(|d| d.kind == "custom")
            .flat_map(|d| [d.name.clone(), self.claude_name(&d.name)])
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// `splitResponsesQualifiedFunctionCallFromRequest`: the name and
    /// namespace to report for a call Claude made by `claude_name`.
    pub(super) fn split(&self, claude_name: &str) -> (String, String) {
        let claude_name = claude_name.trim();
        if claude_name.is_empty() {
            return (String::new(), String::new());
        }
        let identity = self.identity(claude_name);
        match self.winner(identity) {
            Some(d) if !d.direct => (d.child_name.clone(), d.namespace.clone()),
            _ => (identity.to_owned(), String::new()),
        }
    }
}

/// `claudeToolNamePattern`: `^[a-zA-Z0-9_-]{1,64}$`.
fn is_valid_claude_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `responsesToolDescriptors`: every declaration with a name, in order.
fn descriptors(request: &Value) -> Vec<Descriptor> {
    let mut descriptors: Vec<Descriptor> = Vec::new();
    let mut add = |tool: &Value,
                   name: String,
                   child_name: String,
                   namespace: &str,
                   kind: &str,
                   source_priority: u8,
                   direct: bool| {
        if name.is_empty() {
            return;
        }
        let order = descriptors.len();
        descriptors.push(Descriptor {
            name,
            child_name,
            namespace: namespace.to_owned(),
            kind: kind.to_owned(),
            tool: tool.clone(),
            source_priority,
            direct,
            order,
        });
    };
    for (tools, priority) in tool_sources(request) {
        for tool in tools {
            let kind = str_of(tool.get("type"));
            let kind = kind.trim();
            match kind {
                "" | "function" | "custom" => {
                    let kind = if kind == "custom" {
                        "custom"
                    } else {
                        "function"
                    };
                    add(
                        tool,
                        tool_name(tool),
                        String::new(),
                        "",
                        kind,
                        priority,
                        true,
                    );
                }
                "namespace" => {
                    let namespace = str_of(tool.get("name"));
                    let namespace = namespace.trim();
                    let Some(Value::Array(children)) = tool.get("tools") else {
                        continue;
                    };
                    for child in children {
                        let child_name = tool_name(child);
                        if child_name.is_empty() {
                            continue;
                        }
                        let name = qualify(namespace, &child_name);
                        let kind = match str_of(child.get("type")).trim() {
                            "" | "function" => "function",
                            "custom" => "custom",
                            _ => continue,
                        };
                        add(child, name, child_name, namespace, kind, priority, false);
                    }
                }
                "web_search" => {
                    if !allows_external_web_access(tool) {
                        continue;
                    }
                    let name = str_of(tool.get("name"));
                    let name = match name.trim() {
                        "" => "web_search",
                        name => name,
                    };
                    add(
                        tool,
                        name.to_owned(),
                        String::new(),
                        "",
                        kind,
                        priority,
                        true,
                    );
                }
                _ => {
                    if is_unsupported_builtin_tool_type(kind) {
                        continue;
                    }
                    let name = str_of(tool.get("name")).trim().to_owned();
                    add(tool, name, String::new(), "", kind, priority, true);
                }
            }
        }
    }
    descriptors
}

/// The arrays tools are declared in, with their priority.
fn tool_sources(request: &Value) -> Vec<(&Vec<Value>, u8)> {
    let mut sources = Vec::new();
    if let Some(Value::Array(tools)) = request.get("tools") {
        sources.push((tools, 0));
    }
    if let Some(Value::Array(input)) = request.get("input") {
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

/// `responsesHistoryToolIdentities`: the qualified names of the conversation's
/// `function_call` and `custom_tool_call` items, in order.
fn history_tool_identities(request: &Value) -> Vec<String> {
    let Some(Value::Array(input)) = request.get("input") else {
        return Vec::new();
    };
    input
        .iter()
        .filter(|item| {
            matches!(
                &*str_of(item.get("type")),
                "function_call" | "custom_tool_call"
            )
        })
        .map(|item| {
            let name = str_of(item.get("name"));
            let namespace = str_of(item.get("namespace"));
            match namespace.trim() {
                "" => name.into_owned(),
                namespace => qualify(namespace, &name),
            }
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// `qualifyResponsesNamespaceToolName`: a namespace child's full name. Unlike
/// [`crate::responses_tools::qualify_namespace_tool_name`], the namespace is
/// taken as given; callers that want it trimmed trim it.
pub(super) fn qualify(namespace: &str, child: &str) -> String {
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

/// `responsesToolName`: `name`, or else `function.name`, trimmed.
pub(super) fn tool_name(tool: &Value) -> String {
    let name = str_of(tool.get("name"));
    if !name.trim().is_empty() {
        return name.trim().to_owned();
    }
    str_of(path(tool, "function.name")).trim().to_owned()
}

/// `responsesToolDescription`: `description`, or else
/// `function.description`.
pub(super) fn tool_description(tool: &Value) -> String {
    let description = str_of(tool.get("description"));
    if !description.is_empty() {
        return description.into_owned();
    }
    str_of(path(tool, "function.description")).into_owned()
}

/// `responsesToolParameters`: the first schema declared.
pub(super) fn tool_parameters(tool: &Value) -> Option<&Value> {
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

/// A web search tool is left out when it says it can't reach the web.
pub(super) fn allows_external_web_access(tool: &Value) -> bool {
    tool.get("external_web_access").is_none_or(bool_of)
}

/// `isUnsupportedOpenAIBuiltinToolType`: OpenAI's own tools, which Claude
/// can't run.
pub(super) fn is_unsupported_builtin_tool_type(kind: &str) -> bool {
    matches!(
        kind,
        "image_generation" | "file_search" | "code_interpreter" | "computer_use_preview"
    )
}

#[cfg(test)]
mod tests;
