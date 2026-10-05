// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/interactions_openai_responses_response.go
// (interactionsToolIdentityMap, responseModel, firstUsageInt,
// interactionsThoughtSignature, interactionsReasoningEncryptedContent)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What both directions share: reading values as gjson does, and the small
//! helpers of upstream's response file. The helpers it takes from the
//! request file, such as `firstNonEmpty` and the content part and call
//! converters, are the request translator's `pub(super)` ones.
//!
//! Deviations from upstream:
//! - A non-string value read as text is written as compact JSON, where
//!   upstream uses its JSON text.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::Value;

use super::super::request::first_non_empty;
use crate::apply_patch::is_custom_tool;
use crate::json::{int_of, path, set_path, str_of};
use crate::responses_tools::{ToolIdentity, collect_tool_winners};
use crate::signature::is_recognized_reasoning_signature;

/// gjson `Get(at).String()` on `value`, for a dotted path of object keys.
pub(super) fn text<'v>(value: Option<&'v Value>, at: &str) -> Cow<'v, str> {
    str_of(value.and_then(|value| path(value, at)))
}

/// gjson `Get(at)` on `value`, for a dotted path of object keys.
pub(super) fn get<'v>(value: Option<&'v Value>, at: &str) -> Option<&'v Value> {
    value.and_then(|value| path(value, at))
}

/// sjson `Set`, ignoring whether it could.
pub(super) fn set(value: &mut Value, at: &str, new: impl Into<Value>) {
    set_path(value, at, new.into());
}

/// gjson `ForEach`: an array's elements, with their indexes as keys, or an
/// object's members, with their keys. A value of another kind is given
/// once, with no key, and a missing one not at all.
pub(super) fn for_each(value: Option<&Value>) -> Vec<(Option<Value>, &Value)> {
    match value {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, item)| (Some(Value::from(index)), item))
            .collect(),
        Some(Value::Object(fields)) => fields
            .iter()
            .map(|(key, field)| (Some(Value::String(key.clone())), field))
            .collect(),
        Some(other) => vec![(None, other)],
    }
}

/// gjson `Int()` of a [`for_each`] key: 0 where there is none.
pub(super) fn key_int(key: Option<&Value>) -> i64 {
    key.map_or(0, int_of)
}

/// `firstUsageInt`: the first of `paths` that `usage` has, as an integer.
pub(super) fn first_usage_int(usage: &Value, paths: &[&str]) -> Option<i64> {
    paths.iter().find_map(|at| path(usage, at)).map(int_of)
}

/// `responseModel`: `model`, else the model `root` names.
pub(super) fn response_model(model: &str, root: Option<&Value>) -> String {
    let named = ["model", "response.model", "interaction.model"].map(|at| text(root, at));
    first_non_empty([model, &named[0], &named[1], &named[2]]).to_owned()
}

/// `interactionsToolIdentityMap`: what each tool the client declared is
/// called upstream, read from the client's request, or the `request` it is
/// wrapped in. Interactions takes namespaced names as they are, not
/// sanitized as Gemini's are.
pub(super) fn identity_map(request: Option<&Value>) -> HashMap<String, ToolIdentity> {
    let Some(mut root) = request else {
        return HashMap::new();
    };
    if let Some(request) = root.get("request") {
        root = request;
    }
    collect_tool_winners(root)
        .into_iter()
        .map(|(name, descriptor)| {
            let identity = ToolIdentity {
                apply_patch: is_custom_tool(descriptor.tool),
                name: descriptor.local_name,
                namespace: descriptor.namespace,
                custom: descriptor.custom,
            };
            (name, identity)
        })
        .collect()
}

/// Whether `name` is the client's `apply_patch` tool.
pub(super) fn is_patch(identities: &HashMap<String, ToolIdentity>, name: &str) -> bool {
    identities
        .get(name)
        .is_some_and(|identity| identity.apply_patch)
}

/// `interactionsReasoningEncryptedContent`: a signature, trimmed, if it is
/// one this proxy recognizes, else `""`.
pub(super) fn encrypted_content(signature: &str) -> String {
    let candidate = signature.trim();
    if !candidate.is_empty() && is_recognized_reasoning_signature(candidate) {
        candidate.to_owned()
    } else {
        String::new()
    }
}

/// `interactionsThoughtSignature`: a thought step's recognized signature,
/// from its own fields, else the first of its content parts'.
pub(super) fn thought_signature(step: &Value) -> String {
    for at in [
        "encrypted_content",
        "signature",
        "thought_signature",
        "thoughtSignature",
        "extra_content.google.thought_signature",
    ] {
        let signature = encrypted_content(&text(Some(step), at));
        if !signature.is_empty() {
            return signature;
        }
    }
    let Some(Value::Array(parts)) = step.get("content") else {
        return String::new();
    };
    for part in parts {
        let fields = [
            "signature",
            "thought_signature",
            "thoughtSignature",
            "extra_content.google.thought_signature",
        ]
        .map(|at| text(Some(part), at));
        let candidate = first_non_empty([&fields[0], &fields[1], &fields[2], &fields[3]]);
        let signature = encrypted_content(candidate);
        if !signature.is_empty() {
            return signature;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Not upstream's: gjson ForEach on each kind of value.
    #[test]
    fn for_each_gives_keys_as_gjson_does() {
        let array = json!(["a", "b"]);
        let keys: Vec<i64> = for_each(Some(&array))
            .iter()
            .map(|(key, _)| key_int(key.as_ref()))
            .collect();
        assert_eq!(keys, [0, 1]);
        let object = json!({ "7": 1, "x": 2 });
        let keys: Vec<i64> = for_each(Some(&object))
            .iter()
            .map(|(key, _)| key_int(key.as_ref()))
            .collect();
        assert_eq!(keys, [7, 0]);
        let scalar = json!("text");
        assert_eq!(for_each(Some(&scalar)), vec![(None, &scalar)]);
        assert!(for_each(None).is_empty());
    }
}
