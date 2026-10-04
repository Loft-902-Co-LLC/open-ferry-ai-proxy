// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/interactions_openai_responses_request.go
// (responsesContentPartToInteractions, responsesImagePartToInteractions,
// responsesFunctionCallToInteractions, interactionsContentPartToResponses,
// interactionsFunctionCallToResponsesWithIdentity, interactionsContentTexts,
// interactionsMediaDataURL, mediaFormat, parseDataURL, setJSONValue,
// jsonStringValue, firstExisting, firstNonEmpty) and
// interactions_openai_responses_response.go (interactionsToolIdentityMap,
// responseModel, firstUsageInt, interactionsThoughtSignature,
// interactionsReasoningEncryptedContent) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What both directions share: reading values as gjson does, and turning
//! single content parts and calls from one format into the other.
//!
//! The helpers from upstream's request file are copies, kept private here
//! so this half of the port doesn't depend on the request half. The two can
//! share one copy once both have landed.
//!
//! Deviations from upstream:
//! - Upstream's `forAntigravity` branches are not ported: a name is never
//!   mapped for Antigravity, whatever the model is called.
//! - A non-string value read as text is written as compact JSON, where
//!   upstream uses its JSON text, and so is a non-string value kept as JSON
//!   text, such as a call's `arguments` object.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::{Value, json};

use crate::apply_patch::is_custom_tool;
use crate::go;
use crate::json::{int_of, path, set_path, str_of};
use crate::responses_tools::{
    ToolIdentity, collect_tool_winners, qualify_namespace_tool_name,
    unwrap_responses_custom_tool_input,
};
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

/// `firstNonEmpty`: the first value that isn't only white space, as it is.
pub(super) fn first_non_empty<const N: usize>(values: [&str; N]) -> &str {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default()
}

/// `firstExisting`: the first value there is.
pub(super) fn first_existing<const N: usize>(values: [Option<&Value>; N]) -> Option<&Value> {
    values.into_iter().flatten().next()
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

/// `jsonStringValue`: a string as it is, another value as JSON text, or
/// `fallback` if there is none.
pub(super) fn json_string_value(value: Option<&Value>, fallback: &str) -> String {
    match value {
        None => fallback.to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
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

/// `interactionsContentTexts`: the texts in a step's content: the content
/// itself if it is a string, else each part's `text` or `content.text`.
pub(super) fn content_texts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                let (own, nested) = (text(Some(part), "text"), text(Some(part), "content.text"));
                let text = first_non_empty([&own, &nested]);
                (!text.is_empty()).then(|| text.to_owned())
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `interactionsContentPartToResponses`: an Interactions content part as a
/// Responses one, for a message from `role`.
pub(super) fn content_part_to_responses(part: &Value, role: &str) -> Option<Value> {
    let mut part_type = text(Some(part), "type");
    if part_type.is_empty() && part.get("text").is_some() {
        part_type = Cow::Borrowed("text");
    }
    let assistant = role == "assistant";
    match part_type.as_ref() {
        "text" => {
            let mut out = json!({ "type": "", "text": "" });
            set(
                &mut out,
                "type",
                if assistant {
                    "output_text"
                } else {
                    "input_text"
                },
            );
            set(&mut out, "text", text(Some(part), "text"));
            Some(out)
        }
        "image" => {
            let mut out = json!({ "type": "" });
            set(
                &mut out,
                "type",
                if assistant {
                    "output_image"
                } else {
                    "input_image"
                },
            );
            let url = media_data_url(part);
            if !url.is_empty() {
                set(&mut out, "image_url", url);
            }
            Some(out)
        }
        "audio" => {
            let mut out = json!({ "type": "output_text", "text": "" });
            let mime_type = text(Some(part), "mime_type");
            let format = media_format(&mime_type);
            set(
                &mut out,
                "text",
                format!("Audio content: inline data (Format: {format})"),
            );
            Some(out)
        }
        "video" | "document" => {
            let mut out = json!({ "type": "" });
            set(
                &mut out,
                "type",
                if assistant {
                    "output_file"
                } else {
                    "input_file"
                },
            );
            let url = media_data_url(part);
            if !url.is_empty() {
                set(&mut out, "file_data", url);
            }
            let filename = text(Some(part), "filename");
            if !filename.is_empty() {
                set(&mut out, "filename", filename);
            }
            Some(out)
        }
        _ => None,
    }
}

/// `interactionsMediaDataURL`: a media part's URL, or its data as a data
/// URL.
fn media_data_url(part: &Value) -> String {
    let urls = ["image_url", "file_data", "url"].map(|at| text(Some(part), at));
    let url = first_non_empty([&urls[0], &urls[1], &urls[2]]);
    if !url.is_empty() {
        return url.to_owned();
    }
    let data = text(Some(part), "data");
    if data.is_empty() {
        return String::new();
    }
    let mut mime_type = text(Some(part), "mime_type");
    if mime_type.is_empty() {
        mime_type = Cow::Borrowed("application/octet-stream");
    }
    format!("data:{mime_type};base64,{data}")
}

/// `mediaFormat`: the part of a MIME type after its `/`.
fn media_format(mime_type: &str) -> &str {
    if mime_type.is_empty() {
        return "unknown";
    }
    match mime_type.split_once('/') {
        Some((_, format)) if !format.is_empty() => format,
        _ => mime_type,
    }
}

/// `interactionsFunctionCallToResponsesWithIdentity`: an Interactions call
/// as a Responses function or custom tool call, named as the client
/// declared the tool.
pub(super) fn function_call_to_responses(
    item: &Value,
    identities: &HashMap<String, ToolIdentity>,
) -> Value {
    let raw_name = text(Some(item), "name");
    let (name, namespace, custom) = match identities.get(raw_name.as_ref()) {
        Some(identity) => (
            identity.name.as_str(),
            identity.namespace.as_str(),
            identity.custom,
        ),
        None => (raw_name.as_ref(), "", false),
    };
    let (own, call) = (text(Some(item), "call_id"), text(Some(item), "id"));
    let call_id = first_non_empty([&own, &call]);
    let arguments = json_string_value(item.get("arguments"), "{}");
    let mut out = if custom {
        json!({ "type": "custom_tool_call", "call_id": "", "name": "", "input": "" })
    } else {
        json!({ "type": "function_call", "call_id": "", "name": "", "arguments": "{}" })
    };
    if !call_id.is_empty() {
        set(&mut out, "call_id", call_id);
    }
    if !namespace.is_empty() {
        set(&mut out, "namespace", namespace);
    }
    set(&mut out, "name", name);
    if custom {
        set(
            &mut out,
            "input",
            unwrap_responses_custom_tool_input(&arguments),
        );
    } else {
        set(&mut out, "arguments", arguments);
    }
    out
}

/// `responsesContentPartToInteractions`: a Responses content part as an
/// Interactions one, if it is text or an image.
pub(super) fn content_part_to_interactions(part: &Value) -> Option<Value> {
    match text(Some(part), "type").as_ref() {
        "input_text" | "output_text" | "text" => {
            let mut out = json!({ "type": "text", "text": "" });
            set(&mut out, "text", text(Some(part), "text"));
            Some(out)
        }
        "input_image" | "output_image" => Some(image_part_to_interactions(part)),
        _ => {
            let text = part.get("text")?;
            let mut out = json!({ "type": "text", "text": "" });
            set(&mut out, "text", str_of(Some(text)));
            Some(out)
        }
    }
}

/// `responsesImagePartToInteractions`
fn image_part_to_interactions(part: &Value) -> Value {
    let mut out = json!({ "type": "image" });
    let (image_url, url) = (text(Some(part), "image_url"), text(Some(part), "url"));
    let image_url = first_non_empty([&image_url, &url]);
    if let Some((mime_type, data)) = parse_data_url(image_url) {
        set(&mut out, "mime_type", mime_type);
        set(&mut out, "data", data);
        return out;
    }
    let data = text(Some(part), "data");
    if !data.is_empty() {
        set(&mut out, "data", data);
        let mime_type = text(Some(part), "mime_type");
        if !mime_type.is_empty() {
            set(&mut out, "mime_type", mime_type);
        }
        return out;
    }
    if !image_url.is_empty() {
        set(&mut out, "image_url", image_url);
    }
    out
}

/// `parseDataURL`: a data URL's MIME type and data.
fn parse_data_url(value: &str) -> Option<(&str, &str)> {
    let (header, data) = value.strip_prefix("data:")?.split_once(',')?;
    let mime_type = header
        .split_once(';')
        .map_or(header, |(mime_type, _)| mime_type);
    let mime_type = if mime_type.is_empty() {
        "application/octet-stream"
    } else {
        mime_type
    };
    Some((mime_type, data))
}

/// `responsesFunctionCallToInteractions`: a Responses function call as an
/// Interactions call step, with its namespace in its name.
pub(super) fn function_call_to_interactions(item: &Value) -> Value {
    let mut out = json!({ "type": "function_call", "name": "", "arguments": {} });
    let mut name = text(Some(item), "name").into_owned();
    let namespace = text(Some(item), "namespace");
    if !namespace.is_empty() && !name.is_empty() {
        name = qualify_namespace_tool_name(&namespace, &name);
    }
    set(&mut out, "name", name);
    let (own, id) = (text(Some(item), "call_id"), text(Some(item), "id"));
    let call_id = first_non_empty([&own, &id]);
    if !call_id.is_empty() {
        set(&mut out, "call_id", call_id);
    }
    set_json_value(&mut out, "arguments", item.get("arguments"), json!({}));
    out
}

/// `setJSONValue`: sets `at` to `value`, or to the JSON a string holds, or
/// to `default` if there is no value. A string holding JSON that serde_json
/// can't read is kept as a string.
fn set_json_value(out: &mut Value, at: &str, value: Option<&Value>, default: Value) {
    let new = match value {
        None => default,
        Some(Value::String(text)) if go::gjson_valid(text.as_bytes()) => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()))
        }
        Some(other) => other.clone(),
    };
    set_path(out, at, new);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: firstNonEmpty keeps the value as it was.
    #[test]
    fn first_non_empty_skips_white_space() {
        assert_eq!(first_non_empty([" ", "\t", " a ", "b"]), " a ");
        assert_eq!(first_non_empty([" ", ""]), "");
    }

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

    // Not upstream's: data URLs split as parseDataURL splits them.
    #[test]
    fn data_urls_split() {
        assert_eq!(
            parse_data_url("data:image/png;base64,AAA"),
            Some(("image/png", "AAA"))
        );
        assert_eq!(
            parse_data_url("data:;base64,AAA"),
            Some(("application/octet-stream", "AAA"))
        );
        assert_eq!(parse_data_url("data:image/png"), None);
        assert_eq!(parse_data_url("https://x"), None);
    }
}
