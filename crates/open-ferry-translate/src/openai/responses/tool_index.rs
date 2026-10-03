// Ported from CLIProxyAPI internal/translator/openai/openai/responses/responses_tool_index.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Chat Completions names of a request's tools, for naming the calls and
//! the forced tool that refer to them.
//!
//! Only the lookups the request translator needs are ported; the response
//! translator also maps a Chat Completions name back to its declaration.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::tools::{Declaration, cap, chat_tool, declarations, qualify, raw_qualified_name};

/// `responsesToolIndex`: one request's tool declarations, by name.
pub(super) struct ToolIndex<'a> {
    declarations: Vec<Declaration<'a>>,
    /// The Chat Completions names given to declarations.
    by_chat: HashSet<String>,
    /// The first Chat Completions name for each namespace and local name.
    by_identity: HashMap<(String, String), String>,
    /// The first Chat Completions name for each uncut qualified name.
    by_raw: HashMap<String, String>,
    /// The Chat Completions name for each local name, or `""` if tools with
    /// different names share it.
    by_local: HashMap<String, String>,
}

impl<'a> ToolIndex<'a> {
    /// `newResponsesToolIndex`.
    pub(super) fn new(request: &'a Value) -> Self {
        let mut index = Self {
            declarations: declarations(request),
            by_chat: HashSet::new(),
            by_identity: HashMap::new(),
            by_raw: HashMap::new(),
            by_local: HashMap::new(),
        };
        for declaration in &index.declarations {
            let chat_name = &declaration.chat_name;
            index
                .by_identity
                .entry((
                    declaration.namespace.clone(),
                    declaration.local_name.clone(),
                ))
                .or_insert_with(|| chat_name.clone());
            index
                .by_raw
                .entry(raw_qualified_name(
                    &declaration.namespace,
                    &declaration.local_name,
                ))
                .or_insert_with(|| chat_name.clone());
            if !index.by_chat.insert(chat_name.clone()) {
                continue;
            }
            match index.by_local.entry(declaration.local_name.clone()) {
                Entry::Occupied(mut owner) => owner.get_mut().clear(),
                Entry::Vacant(owner) => {
                    owner.insert(chat_name.clone());
                }
            }
        }
        index
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
        if self.by_chat.contains(name) {
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
        if !self.by_chat.contains(&candidate) {
            return candidate;
        }
        (1_u64..)
            .map(|suffix| cap(&format!("{candidate}_{suffix}")))
            .find(|variant| !self.by_chat.contains(variant))
            .unwrap_or(candidate)
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
            if let Some(tool) =
                chat_tool(declaration.tool, &declaration.chat_name, declaration.custom)
            {
                tools.push(tool);
                seen.insert(declaration.chat_name.as_str());
            }
        }
        tools
    }
}
