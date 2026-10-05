// Ported from CLIProxyAPI internal/translator/openai/openai/responses/responses_tool_index.go
// and the index methods in openai_openai-responses_tools.go and shell_tool.go
// (isShell, shellName) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Chat Completions names of a request's tools: for naming the calls and
//! the forced tool that refer to them, and for giving a call the model makes
//! back its Responses name and namespace.
//!
//! Deviations from upstream: none.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;

use serde_json::Value;

use super::shell_tool;
use super::tools::{Declaration, cap, chat_tool, declarations, qualify, raw_qualified_name};
use crate::apply_patch::is_custom_tool;

/// `responsesToolIndex`: one request's tool declarations, by name. It
/// dereferences to the [`ToolNames`] lookups.
pub(super) struct ToolIndex<'a> {
    declarations: Vec<Declaration<'a>>,
    names: ToolNames,
}

/// The lookups of a [`ToolIndex`], which don't borrow the request.
pub(super) struct ToolNames {
    /// The first declaration given each Chat Completions name.
    by_chat: HashMap<String, Winner>,
    /// The first Chat Completions name for each namespace and local name.
    by_identity: HashMap<(String, String), String>,
    /// The first Chat Completions name for each uncut qualified name.
    by_raw: HashMap<String, String>,
    /// The Chat Completions name for each local name, or `""` if tools with
    /// different names share it.
    by_local: HashMap<String, String>,
    /// The Chat Completions names whose first declaration is a custom tool.
    custom: HashSet<String>,
    /// The local shell's Chat Completions name, or `""` if the request
    /// declares no local shell.
    shell_name: String,
}

/// What the lookups keep of the first declaration given a name.
struct Winner {
    local_name: String,
    namespace: String,
    /// Whether it is the `apply_patch` custom tool.
    apply_patch: bool,
    /// Whether it is the client's local shell.
    shell: bool,
}

impl<'a> ToolIndex<'a> {
    /// `newResponsesToolIndex`.
    pub(super) fn new(request: &'a Value) -> Self {
        let declarations = declarations(request);
        let mut names = ToolNames {
            by_chat: HashMap::new(),
            by_identity: HashMap::new(),
            by_raw: HashMap::new(),
            by_local: HashMap::new(),
            custom: HashSet::new(),
            shell_name: String::new(),
        };
        for declaration in &declarations {
            let chat_name = &declaration.chat_name;
            if declaration.shell && names.shell_name.is_empty() {
                names.shell_name.clone_from(chat_name);
            }
            names
                .by_identity
                .entry((
                    declaration.namespace.clone(),
                    declaration.local_name.clone(),
                ))
                .or_insert_with(|| chat_name.clone());
            names
                .by_raw
                .entry(raw_qualified_name(
                    &declaration.namespace,
                    &declaration.local_name,
                ))
                .or_insert_with(|| chat_name.clone());
            let Entry::Vacant(winner) = names.by_chat.entry(chat_name.clone()) else {
                continue;
            };
            winner.insert(Winner {
                local_name: declaration.local_name.clone(),
                namespace: declaration.namespace.clone(),
                apply_patch: declaration.custom && is_custom_tool(declaration.tool),
                shell: declaration.shell,
            });
            match names.by_local.entry(declaration.local_name.clone()) {
                Entry::Occupied(mut owner) => owner.get_mut().clear(),
                Entry::Vacant(owner) => {
                    owner.insert(chat_name.clone());
                }
            }
            if declaration.custom {
                names.custom.insert(chat_name.clone());
            }
        }
        Self {
            declarations,
            names,
        }
    }

    /// `chatTools`: the Chat Completions function for each name, from the
    /// first declaration that has it.
    pub(super) fn chat_tools(&self) -> Vec<Value> {
        let mut seen = HashSet::new();
        let mut tools = Vec::new();
        for declaration in &self.declarations {
            if seen.contains(declaration.chat_name.as_str()) {
                continue;
            }
            let tool = if declaration.shell {
                Some(shell_tool::chat_tool(&declaration.chat_name))
            } else {
                chat_tool(declaration.tool, &declaration.chat_name, declaration.custom)
            };
            if let Some(tool) = tool {
                tools.push(tool);
                seen.insert(declaration.chat_name.as_str());
            }
        }
        tools
    }
}

impl Deref for ToolIndex<'_> {
    type Target = ToolNames;

    fn deref(&self) -> &ToolNames {
        &self.names
    }
}

impl ToolNames {
    /// The lookups for `request`'s tools.
    pub(super) fn new(request: &Value) -> Self {
        ToolIndex::new(request).names
    }

    /// `namespaceName`: the Chat Completions name for a call to `name` in
    /// `namespace`. One the request doesn't declare is qualified and cut as a
    /// declaration would be, and kept off the names declarations have.
    pub(super) fn namespace_name(&self, namespace: &str, name: &str) -> String {
        if let Some(chat_name) = self
            .by_identity
            .get(&(namespace.to_owned(), name.to_owned()))
        {
            return chat_name.clone();
        }
        self.avoid_alias(qualify(namespace, name))
    }

    /// `canonicalName`: the Chat Completions name for a call to `name` without
    /// a namespace. A name given to a declaration is kept; then a declaration's
    /// uncut qualified name is looked up, then the one tool with that local
    /// name. Otherwise the name is cut, and kept off the names declarations
    /// have.
    pub(super) fn canonical_name(&self, name: &str) -> String {
        if self.by_chat.contains_key(name) {
            return name.to_owned();
        }
        if let Some(chat_name) = self.by_raw.get(name) {
            return chat_name.clone();
        }
        if let Some(chat_name) = self.by_local.get(name).filter(|name| !name.is_empty()) {
            return chat_name.clone();
        }
        self.avoid_alias(cap(name))
    }

    /// `avoidAlias`: `candidate`, or if a declaration has it, the first of
    /// `candidate_1`, `candidate_2`, … (cut to the limit) that none has.
    fn avoid_alias(&self, candidate: String) -> String {
        if !self.by_chat.contains_key(&candidate) {
            return candidate;
        }
        (1_u64..)
            .map(|suffix| cap(&format!("{candidate}_{suffix}")))
            .find(|variant| !self.by_chat.contains_key(variant))
            .unwrap_or(candidate)
    }

    /// `applyIdentity`, less the writing: the Responses name and namespace for
    /// the Chat Completions name `name`. A name no declaration has is its own
    /// name, trimmed, with no namespace.
    pub(super) fn identity<'n>(&'n self, name: &'n str) -> (&'n str, &'n str) {
        let name = name.trim();
        match self.by_chat.get(name) {
            Some(winner) => (&winner.local_name, &winner.namespace),
            None => (name, ""),
        }
    }

    /// Whether the first declaration named `name` is a custom tool, whose
    /// calls are custom tool calls.
    pub(super) fn is_custom(&self, name: &str) -> bool {
        self.custom.contains(name)
    }

    /// `isApplyPatch`: whether the first declaration named `name` is the
    /// `apply_patch` custom tool.
    pub(super) fn is_apply_patch(&self, name: &str) -> bool {
        self.by_chat
            .get(name)
            .is_some_and(|winner| winner.apply_patch)
    }

    /// Whether any name's first declaration is the `apply_patch` custom tool.
    pub(super) fn patch_enabled(&self) -> bool {
        self.by_chat.values().any(|winner| winner.apply_patch)
    }

    /// `isShell`: whether the first declaration named `name` is the client's
    /// local shell, whose calls are shell calls.
    pub(super) fn is_shell(&self, name: &str) -> bool {
        self.by_chat.get(name).is_some_and(|winner| winner.shell)
    }

    /// `shellName`: the local shell's Chat Completions name, or `""` if the
    /// request declares no local shell.
    pub(super) fn shell_name(&self) -> &str {
        &self.shell_name
    }

    /// Every name the lookups know a tool by: Chat Completions names, uncut
    /// qualified names and local names.
    pub(super) fn known_names(&self) -> impl Iterator<Item = &str> {
        self.by_chat
            .keys()
            .chain(self.by_raw.keys())
            .chain(self.by_local.keys())
            .map(String::as_str)
    }

    /// The Chat Completions names whose first declaration is a custom tool:
    /// `responsesCustomToolNames`.
    #[cfg(test)]
    pub(super) fn custom_names(&self) -> impl Iterator<Item = &str> {
        self.custom.iter().map(String::as_str)
    }

    /// `singleCustomName`: the name of the request's one custom tool, and
    /// whether it is the request's only tool; `("", false)` unless there is
    /// exactly one custom tool.
    pub(super) fn single_custom_name(&self) -> (&str, bool) {
        match self.custom.iter().next() {
            Some(name) if self.custom.len() == 1 => (name, self.by_chat.len() == 1),
            _ => ("", false),
        }
    }
}
