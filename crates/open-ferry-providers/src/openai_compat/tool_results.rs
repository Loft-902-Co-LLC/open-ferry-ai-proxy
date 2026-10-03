// Ported from CLIProxyAPI
// internal/runtime/executor/helps/openai_compat_tool_results.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tool results as text, for a model that takes no images.
//!
//! A model whose `input-modalities` list `text` and not `image` gets every
//! `tool` message's content as one string: text parts joined by blank
//! lines, and each image replaced by a marker. Images a Claude client relays
//! in a user message after the tool results (behind a notice the Claude
//! translator writes) are dropped too, the tool result before them marked,
//! and the user message dropped if nothing else was in it.
//!
//! The model is looked up in the provider's `models` as for
//! [`super::max_tokens`]; when only aliases match, every model under the
//! alias must take no images.
//!
//! Deviations from upstream:
//! - JSON that goes into a flattened tool result as it is (an unknown part,
//!   a number) is written by `serde_json`, compact, where upstream copies the
//!   client's bytes.

use open_ferry_core::config::{OpenAiCompatibility, OpenAiCompatibilityModel};
use serde_json::Value;

use super::max_tokens::normalize_model_name;
use crate::json::{eq_fold, exists, lower_trim, set, str_at};

/// What replaces an image in a tool result.
pub(crate) const IMAGE_OMITTED_TEXT: &str = "[image omitted: unsupported by upstream]";
/// The text part that starts a user message of images relayed from tool
/// results.
pub(crate) const RELAY_NOTICE: &str = "Images returned by the preceding tool call(s):";
/// A tool result whose images were relayed in the next user message.
pub(crate) const IMAGE_PLACEHOLDER: &str =
    "[Tool returned image content; the images follow in the next user message.]";

/// Whether the model takes text and no images, so its tool results go as
/// text (`ShouldNormalizeOpenAIToolResultsForModel`).
pub(crate) fn should_normalize_tool_results(
    compat: Option<&OpenAiCompatibility>,
    upstream_model: &str,
    requested_model: &str,
) -> bool {
    let Some(compat) = compat else {
        return false;
    };
    if let Some(normalize) = model_excludes_images(&compat.models, upstream_model) {
        return normalize;
    }
    model_excludes_images(&compat.models, requested_model).unwrap_or(false)
}

/// Whether the model named `model` takes no images, if a model by that name
/// or alias is configured (`openAICompatibilityModelExcludesImages`).
fn model_excludes_images(models: &[OpenAiCompatibilityModel], model: &str) -> Option<bool> {
    let model = normalize_model_name(model);
    if model.is_empty() {
        return None;
    }
    if let Some(entry) = models
        .iter()
        .find(|entry| eq_fold(model, normalize_model_name(&entry.name)))
    {
        return Some(modalities_exclude_images(&entry.input_modalities));
    }
    let mut matched = false;
    let mut excludes_images = true;
    for entry in models
        .iter()
        .filter(|entry| eq_fold(model, normalize_model_name(&entry.alias)))
    {
        matched = true;
        excludes_images &= modalities_exclude_images(&entry.input_modalities);
    }
    matched.then_some(excludes_images)
}

/// Whether the modalities name text and not images
/// (`inputModalitiesExcludeImages`).
fn modalities_exclude_images(modalities: &[String]) -> bool {
    let mut has_text = false;
    for modality in modalities {
        match lower_trim(modality).as_str() {
            "image" => return false,
            "text" => has_text = true,
            _ => {}
        }
    }
    has_text
}

/// Turns tool results into text and drops relayed images
/// (`NormalizeOpenAIToolResultsTextOnly`).
pub(crate) fn normalize_tool_results_text_only(body: &mut Value) {
    let Some(Value::Array(messages)) = body.get_mut("messages") else {
        return;
    };
    if messages.is_empty() {
        return;
    }
    let mut normalized: Vec<Value> = Vec::with_capacity(messages.len());
    // Whether a tool result of this turn said its images follow, so it
    // already has the marker.
    let mut replaced_placeholder = false;
    for mut message in std::mem::take(messages) {
        match str_at(&message, "role").as_str() {
            "tool" => {
                let content = match message.get("content") {
                    Some(Value::String(text)) if text == IMAGE_PLACEHOLDER => {
                        replaced_placeholder = true;
                        Some(IMAGE_OMITTED_TEXT.to_owned())
                    }
                    None | Some(Value::String(_)) => None,
                    Some(content) => Some(flatten(content)),
                };
                if let Some(content) = content {
                    set(&mut message, "content", Value::String(content));
                }
                normalized.push(message);
            }
            "user" => {
                if let Some(Value::Array(parts)) = message.get("content") {
                    let mut remaining = Vec::new();
                    let mut has_relay_notice = false;
                    let mut has_images = false;
                    for part in parts {
                        if part.is_object() {
                            if str_at(part, "type") == "text"
                                && str_at(part, "text") == RELAY_NOTICE
                            {
                                has_relay_notice = true;
                                continue;
                            }
                            if is_image_part(part) {
                                has_images = true;
                                continue;
                            }
                        }
                        remaining.push(part.clone());
                    }
                    if has_relay_notice && has_images {
                        if !replaced_placeholder {
                            mark_last_tool_result(&mut normalized);
                        }
                        replaced_placeholder = false;
                        if remaining.is_empty() {
                            // Only relayed images: the message goes.
                            continue;
                        }
                        set(&mut message, "content", Value::Array(remaining));
                    }
                }
                normalized.push(message);
            }
            _ => {
                replaced_placeholder = false;
                normalized.push(message);
            }
        }
    }
    *messages = normalized;
}

/// Adds the image marker to the last message if it is a tool result
/// without one.
fn mark_last_tool_result(messages: &mut [Value]) {
    let Some(last) = messages.last_mut() else {
        return;
    };
    if str_at(last, "role") != "tool" {
        return;
    }
    let content = str_at(last, "content");
    if content.contains(IMAGE_OMITTED_TEXT) {
        return;
    }
    let content = if content.is_empty() {
        IMAGE_OMITTED_TEXT.to_owned()
    } else {
        format!("{content}\n\n{IMAGE_OMITTED_TEXT}")
    };
    set(last, "content", Value::String(content));
}

/// A tool result's content as text (`flattenOpenAIToolResultContent`).
fn flatten(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(part_text).collect::<Vec<_>>().join("\n\n"),
        Value::Object(object) => {
            if is_image_part(content) {
                IMAGE_OMITTED_TEXT.to_owned()
            } else if let Some(Value::String(text)) = object.get("text") {
                text.clone()
            } else {
                content.to_string()
            }
        }
        other => other.to_string(),
    }
}

/// One part of a tool result's content as text
/// (`openAIToolResultPartText`).
fn part_text(item: &Value) -> String {
    match item {
        Value::String(text) => text.clone(),
        Value::Object(object) => {
            if is_image_part(item) {
                IMAGE_OMITTED_TEXT.to_owned()
            } else if let Some(Value::String(text)) = object.get("text") {
                text.clone()
            } else {
                item.to_string()
            }
        }
        other => other.to_string(),
    }
}

/// Whether a content part is an image (`isOpenAIImageToolResultPart`).
fn is_image_part(item: &Value) -> bool {
    if !item.is_object() {
        return false;
    }
    matches!(
        lower_trim(&str_at(item, "type")).as_str(),
        "image" | "image_url" | "input_image"
    ) || exists(item, "image_url")
        || exists(item, "input_image")
}

#[cfg(test)]
mod tests {
    // Ports internal/runtime/executor/helps/openai_compat_tool_results_test.go.
    // The relayed-image cases also check the whole body.
    use super::*;
    use serde_json::json;

    fn normalized(input: &str) -> Value {
        let mut body: Value = serde_json::from_str(input).unwrap();
        normalize_tool_results_text_only(&mut body);
        body
    }

    fn content(body: &Value, index: usize) -> String {
        str_at(body, &format!("messages.{index}.content"))
    }

    const ASSISTANT_CALL: &str = r#"{"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"inspect_image","arguments":"{}"}}]}"#;

    #[test]
    fn relayed_images_synthetic_relay_message_is_dropped_and_tool_is_marked() {
        let got = normalized(&format!(
            r#"{{"messages":[{ASSISTANT_CALL},
            {{"role":"tool","tool_call_id":"call_1","content":"image inspected"}},
            {{"role":"user","content":[
                {{"type":"text","text":"Images returned by the preceding tool call(s):"}},
                {{"type":"image_url","image_url":{{"url":"data:image/png;base64,AA=="}}}}
            ]}}]}}"#
        ));
        assert!(!got.to_string().contains("image_url"), "{got}");
        assert_eq!(got["messages"].as_array().unwrap().len(), 2, "{got}");
        assert_eq!(
            content(&got, 1),
            format!("image inspected\n\n{IMAGE_OMITTED_TEXT}")
        );
    }

    #[test]
    fn relayed_images_placeholder_tool_content_is_replaced_with_omitted_marker() {
        let got = normalized(&format!(
            r#"{{"messages":[{ASSISTANT_CALL},
            {{"role":"tool","tool_call_id":"call_1","content":"{IMAGE_PLACEHOLDER}"}},
            {{"role":"user","content":[
                {{"type":"text","text":"{RELAY_NOTICE}"}},
                {{"type":"image_url","image_url":{{"url":"https://example.com/img.png"}}}}
            ]}}]}}"#
        ));
        assert!(!got.to_string().contains("image_url"), "{got}");
        assert_eq!(got["messages"].as_array().unwrap().len(), 2, "{got}");
        assert_eq!(content(&got, 1), IMAGE_OMITTED_TEXT);
    }

    #[test]
    fn relayed_images_merged_user_message_retains_user_prompt() {
        let got = normalized(&format!(
            r#"{{"messages":[{ASSISTANT_CALL},
            {{"role":"tool","tool_call_id":"call_1","content":"image inspected"}},
            {{"role":"user","content":[
                {{"type":"text","text":"{RELAY_NOTICE}"}},
                {{"type":"image_url","image_url":{{"url":"data:image/png;base64,AA=="}}}},
                {{"type":"text","text":"What color is the car?"}}
            ]}}]}}"#
        ));
        assert!(!got.to_string().contains("image_url"), "{got}");
        assert_eq!(got["messages"].as_array().unwrap().len(), 3, "{got}");
        assert_eq!(
            content(&got, 1),
            format!("image inspected\n\n{IMAGE_OMITTED_TEXT}")
        );
        let user = got["messages"][2]["content"].to_string();
        assert!(!user.contains(RELAY_NOTICE), "{user}");
        assert_eq!(user, r#"[{"type":"text","text":"What color is the car?"}]"#);
    }

    #[test]
    fn relayed_images_multiple_preceding_tools_where_one_is_placeholder() {
        let got = normalized(&format!(
            r#"{{"messages":[
            {{"role":"assistant","content":"","tool_calls":[
                {{"id":"call_1","type":"function","function":{{"name":"tool1","arguments":"{{}}"}}}},
                {{"id":"call_2","type":"function","function":{{"name":"tool2","arguments":"{{}}"}}}}
            ]}},
            {{"role":"tool","tool_call_id":"call_1","content":"text result only"}},
            {{"role":"tool","tool_call_id":"call_2","content":"{IMAGE_PLACEHOLDER}"}},
            {{"role":"user","content":[
                {{"type":"text","text":"{RELAY_NOTICE}"}},
                {{"type":"image_url","image_url":{{"url":"https://example.com/tool2.png"}}}}
            ]}}]}}"#
        ));
        assert!(!got.to_string().contains("image_url"), "{got}");
        assert_eq!(got["messages"].as_array().unwrap().len(), 3, "{got}");
        assert_eq!(content(&got, 1), "text result only");
        assert_eq!(content(&got, 2), IMAGE_OMITTED_TEXT);
    }

    #[test]
    fn relayed_images_multiple_preceding_tools_where_image_tool_precedes_text_tool() {
        let got = normalized(&format!(
            r#"{{"messages":[
            {{"role":"assistant","content":"","tool_calls":[
                {{"id":"call_1","type":"function","function":{{"name":"tool1","arguments":"{{}}"}}}},
                {{"id":"call_2","type":"function","function":{{"name":"tool2","arguments":"{{}}"}}}}
            ]}},
            {{"role":"tool","tool_call_id":"call_1","content":"{IMAGE_PLACEHOLDER}"}},
            {{"role":"tool","tool_call_id":"call_2","content":"text result only"}},
            {{"role":"user","content":[
                {{"type":"text","text":"{RELAY_NOTICE}"}},
                {{"type":"image_url","image_url":{{"url":"https://example.com/tool1.png"}}}}
            ]}}]}}"#
        ));
        assert!(!got.to_string().contains("image_url"), "{got}");
        assert_eq!(got["messages"].as_array().unwrap().len(), 3, "{got}");
        assert_eq!(content(&got, 1), IMAGE_OMITTED_TEXT);
        assert_eq!(content(&got, 2), "text result only");
    }

    #[test]
    fn text_only() {
        let got = normalized(
            r#"{"messages":[
            {"role":"assistant","content":[{"type":"text","text":"before"}]},
            {"role":"tool","tool_call_id":"call_1","content":[
                {"type":"text","text":"image inspected"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}
            ]},
            {"role":"tool","tool_call_id":"call_2","content":"already text"},
            {"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/user.png"}}]}
        ]}"#,
        );
        assert_eq!(
            got["messages"][1]["content"],
            json!(format!("image inspected\n\n{IMAGE_OMITTED_TEXT}"))
        );
        assert_eq!(content(&got, 2), "already text");
        assert!(got["messages"][0]["content"].is_array());
        assert!(got["messages"][3]["content"].is_array());
    }

    #[test]
    fn image_and_unknown_content() {
        let cases = [
            (
                "image-only array",
                r#"{"messages":[{"role":"tool","content":[{"type":"image_url","image_url":{"url":"https://example.com/image.png"}}]}]}"#,
                IMAGE_OMITTED_TEXT,
            ),
            (
                "image object",
                r#"{"messages":[{"role":"tool","content":{"type":"image","source":{"type":"base64","data":"AA=="}}}]}"#,
                IMAGE_OMITTED_TEXT,
            ),
            (
                "unknown object",
                r#"{"messages":[{"role":"tool","content":[{"type":"custom","value":1}]}]}"#,
                r#"{"type":"custom","value":1}"#,
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(content(&normalized(input), 0), want, "{name}");
        }
    }

    #[test]
    fn other_content_shapes() {
        // Go flattens every content that isn't a string: null and numbers
        // become their JSON text, an object its text, nested arrays and
        // scalars in an array their JSON.
        let got = normalized(
            r#"{"messages":[
            {"role":"tool","content":null},
            {"role":"tool","content":1.50},
            {"role":"tool","content":{"text":"t","x":1}},
            {"role":"tool","content":["a",{"text":"b"},[1, 2],null,{"input_image":{}}]},
            {"role":"tool"}
        ]}"#,
        );
        assert_eq!(content(&got, 0), "null");
        assert_eq!(content(&got, 1), "1.50");
        assert_eq!(content(&got, 2), "t");
        assert_eq!(
            content(&got, 3),
            format!("a\n\nb\n\n[1,2]\n\nnull\n\n{IMAGE_OMITTED_TEXT}")
        );
        assert!(!exists(&got, "messages.4.content"));
    }

    #[test]
    fn relay_without_images_or_notice_is_kept() {
        let input = format!(
            r#"{{"messages":[
            {{"role":"tool","content":"r"}},
            {{"role":"user","content":[{{"type":"text","text":"{RELAY_NOTICE}"}}]}},
            {{"role":"user","content":[{{"type":"TEXT","text":"{RELAY_NOTICE}"}},{{"type":"input_image"}}]}}
        ]}}"#
        );
        let got = normalized(&input);
        assert_eq!(got, serde_json::from_str::<Value>(&input).unwrap());
    }

    #[test]
    fn marker_goes_on_a_tool_result_without_content_once() {
        let relay = format!(
            r#"{{"role":"user","content":[{{"type":"text","text":"{RELAY_NOTICE}"}},{{"type":"image"}}]}}"#
        );
        let got = normalized(&format!(
            r#"{{"messages":[{{"role":"tool"}},{relay},{{"role":"tool","content":"x {IMAGE_OMITTED_TEXT}"}},{relay}]}}"#
        ));
        assert_eq!(got["messages"].as_array().unwrap().len(), 2, "{got}");
        assert_eq!(content(&got, 0), IMAGE_OMITTED_TEXT);
        assert_eq!(content(&got, 1), format!("x {IMAGE_OMITTED_TEXT}"));
    }

    #[test]
    fn a_plain_user_message_keeps_the_turn() {
        // The placeholder's mark lasts through a user message that isn't a
        // relay, and ends at any other role.
        let relay = format!(
            r#"{{"role":"user","content":[{{"type":"text","text":"{RELAY_NOTICE}"}},{{"type":"image"}}]}}"#
        );
        let got = normalized(&format!(
            r#"{{"messages":[{{"role":"tool","content":"{IMAGE_PLACEHOLDER}"}},{{"role":"user","content":"hi"}},{{"role":"tool","content":"t"}},{relay}]}}"#
        ));
        assert_eq!(content(&got, 2), "t", "{got}");
        let got = normalized(&format!(
            r#"{{"messages":[{{"role":"tool","content":"{IMAGE_PLACEHOLDER}"}},{{"role":"system","content":"s"}},{{"role":"tool","content":"t"}},{relay}]}}"#
        ));
        assert_eq!(
            content(&got, 2),
            format!("t\n\n{IMAGE_OMITTED_TEXT}"),
            "{got}"
        );
    }

    #[test]
    fn bodies_without_messages_are_left() {
        for input in [
            r#"{"messages":[]}"#,
            r#"{"messages":{}}"#,
            r#"{"input":[]}"#,
            "[]",
        ] {
            let want: Value = serde_json::from_str(input).unwrap();
            assert_eq!(normalized(input), want, "{input}");
        }
    }

    #[test]
    fn should_normalize_tool_results_for_model() {
        let entry = |name: &str, alias: &str, modalities: &[&str]| OpenAiCompatibilityModel {
            name: name.into(),
            alias: alias.into(),
            input_modalities: modalities.iter().map(|m| (*m).to_owned()).collect(),
            ..OpenAiCompatibilityModel::default()
        };
        let compat = OpenAiCompatibility {
            models: vec![
                entry("upstream-text", "alias-text", &["text"]),
                entry(
                    "upstream-multimodal",
                    "alias-multimodal",
                    &["text", "image"],
                ),
                entry("upstream-unspecified", "alias-unspecified", &[]),
                entry("upstream-uppercase", "alias-uppercase", &["TEXT"]),
                entry("pool-text", "shared-alias", &["text"]),
                entry("pool-image", "shared-alias", &["text", "image"]),
            ],
            ..OpenAiCompatibility::default()
        };
        let cases = [
            ("upstream text", "upstream-text", "", true),
            ("upstream suffix", "upstream-text(high)", "", true),
            ("requested alias", "unknown", "alias-text", true),
            ("multimodal", "upstream-multimodal", "", false),
            ("unspecified", "upstream-unspecified", "", false),
            ("case insensitive modality", "upstream-uppercase", "", true),
            ("mixed alias pool", "unknown", "shared-alias", false),
            ("unknown", "unknown", "missing", false),
        ];
        for (name, upstream, requested, want) in cases {
            assert_eq!(
                should_normalize_tool_results(Some(&compat), upstream, requested),
                want,
                "{name}"
            );
        }
        assert!(!should_normalize_tool_results(
            None,
            "upstream-text",
            "alias-text"
        ));
    }
}
