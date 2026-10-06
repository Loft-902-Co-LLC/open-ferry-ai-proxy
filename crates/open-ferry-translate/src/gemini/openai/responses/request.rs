// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/gemini_openai-responses_request.go
// (ConvertOpenAIResponsesRequestToGemini, geminiContent,
// coalesceAdjacentOpenAIResponsesModelContents, geminiSystemInstruction,
// stripTrailingOpenAIResponsesModelPrefill, shouldStripTrailingOpenAIResponsesModelPrefill,
// openAIResponsesAssistantVisibleText, isOpenAIResponsesToolCall,
// isOpenAIResponsesToolOutput, pairOpenAIResponsesReasoningWithFunctionCalls,
// reorderOpenAIResponsesDetachedReasoning, buildOpenAIResponsesFunctionCallPart,
// geminiResponsesInlineDataPart, geminiResponsesFileDataPart, firstNonEmpty, isDataURL,
// isRemoteURL, isGenericMIME, firstNonGenericFormat, parseOpenAIResponsesDataURL,
// isResponsesContentPartType, openAIResponsesAudioMimeType, openAIResponsesVideoMimeType,
// openAIResponsesAudioFromBlock, openAIResponsesVideoFromBlock, normalizeFormatToMIME,
// openAIResponsesFileFromBlock, openAIResponsesMediaFromBlock, openAIResponsesPartFromBlock,
// openAIResponsesImageMimeType, openAIResponsesImageFromBlock,
// parseOpenAIResponsesArrayOutput, extractOpenAIResponsesCallID,
// buildOpenAIResponsesSynthesizedFunctionResponsePart, responsesHasMatchingOutput,
// responsesHasSubsequentTurn, buildOpenAIResponsesStandaloneToolOutputTextParts,
// buildOpenAIResponsesFunctionResponseParts, collectOpenAIResponsesFunctionCallOutputs,
// orderOpenAIResponsesFunctionCallOutputs, buildOpenAIResponsesFunctionCallModelContent,
// buildOpenAIResponsesEmptyReasoningFunctionCallModelContent,
// buildOpenAIResponsesReasoningFunctionCallModelContent,
// buildOpenAIResponsesReasoningModelContent, openAIResponsesGeminiThoughtSignature,
// applyOpenAIResponsesTextFormatToGemini), and the parts of Go's net/url
// (Parse) and path/filepath (Base, Ext) it relies on (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! An OpenAI Responses request as a Gemini `generateContent` request.
//!
//! `instructions` and the system and developer messages before the
//! conversation become the system instruction; later ones become
//! `<system-reminder>` user turns, held back while tool calls wait for their
//! outputs. Messages become turns, their parts split by role; text, images,
//! audio, video and files become Gemini parts. Function and custom tool
//! calls become `functionCall` parts, under the names the tool declarations
//! give them; their outputs become `functionResponse` parts, in the order of
//! the calls, with an "interrupted" response made up for a call whose output
//! never comes. An output that answers no call becomes user text.
//!
//! Reasoning items carry Gemini thought signatures. A signature that came
//! from Gemini (or a carrier, see `signature_carrier`) is put back on the
//! part it belongs to: the thought, the text or the call after it, or the
//! one before it if the carrier says so. For a Gemini model, or when the
//! input holds a carrier, the parts are laid out as Gemini sent them;
//! otherwise each reasoning item is a thought part of its own.
//!
//! Deviations from upstream:
//! - A function call's `arguments` that start like a JSON object or array
//!   but aren't valid JSON go into `args.arguments` as text. Upstream copies
//!   the text into the request as it is, which makes the request invalid
//!   JSON.
//! - A value read as text that isn't a string, such as a text part's `text`,
//!   the `instructions` or a tool output that answers no call and is neither
//!   a string nor a list, is written as compact JSON, where upstream copies
//!   the client's JSON text, spacing and all.
//! - Upstream drops a remote file's name from the URL when Go's `url.Parse`
//!   rejects the URL. That check is ported for what can reach it, a URL
//!   starting `http://`, `https://` or `gs://`, except that an IPv6 host in
//!   brackets is checked with Rust's parser, which differs from Go's on a
//!   few malformed addresses.
//! - A file name's extension is found as Go's `filepath.Ext` finds it on
//!   Unix, where only `/` separates path elements.
//! - `isTrailingOpenAIResponsesAssistantPrefill` isn't ported: nothing calls
//!   it.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::net::Ipv6Addr;

use serde_json::{Map, Value};

use super::array_of;
use super::signature_carrier::{
    self, ANY, FUNCTION, NEXT, PREVIOUS, SIGNATURE_FIELD, SUMMARY_FIELD, TEXT, direction,
    is_detached_carrier, summary_text, target,
};
use super::trailing_signature::restore_text_signatures;
use super::web_search::{
    allows_web_search_tool_choice, extract_allowed_domains, has_web_search_tool,
    model_supports_web_search,
};
use crate::common::claude::system_reminder_text;
use crate::common::file_data::normalize_openai_file_data;
use crate::common::gemini::sanitize_gemini_function_name;
use crate::common::gemini::{
    merge_adjacent_gemini_user_contents, set_gemini_function_response_raw,
    set_gemini_function_response_result,
};
use crate::common::mime_types::mime_type;
use crate::common::responses::{extract_responses_call_id, normalize_responses_tool_call_outputs};
use crate::gemini::common::attach_default_safety_settings;
use crate::go;
use crate::json::{bool_of, float_of, int_of, object, path, set_path, str_of};
use crate::responses_tools::{
    build_gemini_function_declarations, convert_responses_tool_choice_to_gemini,
    map_responses_tool_name, qualify_namespace_tool_name,
};
use crate::signature::{
    BlockKind, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, Provider,
    compatible_signature_for_provider_block, gemini_replay_signature_or_bypass,
    sanitize_gemini_request_thought_signatures,
};

/// `geminiResponsesThoughtSignature`: the signature Gemini takes in place of
/// a real one.
const BYPASS: &str = GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR;

/// Where a function response's result goes.
const RESULT_PATH: &str = "functionResponse.response.result";

/// `ConvertOpenAIResponsesRequestToGemini`.
pub fn convert_openai_responses_request_to_gemini(
    model: &str,
    body: &Value,
    _stream: bool,
) -> Value {
    let root = body;
    let mut native = Provider::from_model_name(model) == Provider::Gemini;
    let mut out = Value::Object(Map::new());
    out["contents"] = Value::Array(Vec::new());

    let declarations = build_gemini_function_declarations(root);
    let mut tool_blocks = Vec::new();
    if has_web_search_tool(root)
        && model_supports_web_search(model)
        && allows_web_search_tool_choice(root)
    {
        let mut search = Map::new();
        let domains = extract_allowed_domains(root);
        if !domains.is_empty() {
            search.insert(
                "includedDomains".to_owned(),
                domains.into_iter().map(Value::String).collect(),
            );
        }
        tool_blocks.push(object([("googleSearch", Value::Object(search))]));
    }
    if !declarations.declarations.is_empty() {
        tool_blocks.push(object([(
            "functionDeclarations",
            Value::Array(declarations.declarations.clone()),
        )]));
    }
    if !tool_blocks.is_empty() {
        out["tools"] = Value::Array(tool_blocks);
    }
    if !declarations.declarations.is_empty()
        && let Some(choice) = root.get("tool_choice")
        && let Some(config) =
            convert_responses_tool_choice_to_gemini(Some(choice), &declarations.forward)
    {
        set_path(&mut out, "toolConfig.functionCallingConfig", config);
    }

    let mut system_parts = Vec::new();
    if let Some(instructions) = root.get("instructions") {
        system_parts.push(text_part(str_of(Some(instructions))));
    }

    match root.get("input") {
        Some(Value::Array(input)) => {
            let restored = restore_text_signatures(model, input);
            let (items, has_carrier) = signature_carrier::normalize(&restored);
            if has_carrier {
                native = true;
            }
            let items: Vec<Value> = normalize_responses_tool_call_outputs(&items)
                .into_iter()
                .map(Cow::into_owned)
                .collect();
            let items = pair_reasoning_with_function_calls(items);
            let contents = convert_items(items, native, &declarations.forward, &mut system_parts);
            out["contents"] = Value::Array(contents);
        }
        Some(Value::String(input)) => {
            out["contents"] =
                Value::Array(vec![gemini_content("user", vec![text_part(input.clone())])]);
        }
        _ => {}
    }
    if !system_parts.is_empty() {
        out["systemInstruction"] = object([("parts", Value::Array(system_parts))]);
    }

    if let Some(max_output_tokens) = root.get("max_output_tokens") {
        out["generationConfig"] = object([("maxOutputTokens", int_of(max_output_tokens).into())]);
    }
    if let Some(temperature) = root.get("temperature").and_then(float_of) {
        set_path(&mut out, "generationConfig.temperature", temperature);
    }
    if let Some(top_p) = root.get("top_p").and_then(float_of) {
        set_path(&mut out, "generationConfig.topP", top_p);
    }
    if let Some(Value::Array(sequences)) = root.get("stop_sequences") {
        // Go writes a nil slice, from an empty array, as `null`.
        let sequences = if sequences.is_empty() {
            Value::Null
        } else {
            sequences
                .iter()
                .map(|sequence| Value::String(str_of(Some(sequence)).into_owned()))
                .collect()
        };
        set_path(&mut out, "generationConfig.stopSequences", sequences);
    }
    apply_text_format(&mut out, root);

    if let Some(effort) = path(root, "reasoning.effort") {
        let effort = go::to_lower(str_of(Some(effort)).trim());
        if effort == "auto" {
            set_path(
                &mut out,
                "generationConfig.thinkingConfig.thinkingBudget",
                (-1).into(),
            );
        } else if !effort.is_empty() {
            set_path(
                &mut out,
                "generationConfig.thinkingConfig.thinkingLevel",
                Value::String(effort),
            );
        }
    }

    attach_default_safety_settings(&mut out, "safetySettings");
    if native {
        sanitize_gemini_request_thought_signatures(&mut out, "contents");
    }
    strip_trailing_model_prefill(&mut out);
    out
}

/// The input items as Gemini contents. System and developer messages before
/// the conversation go into `system_parts`.
fn convert_items(
    items: Vec<Value>,
    native: bool,
    forward: &HashMap<String, String>,
    system_parts: &mut Vec<Value>,
) -> Vec<Value> {
    let mut names_by_call_id: HashMap<String, String> = HashMap::new();
    for item in &items {
        if is_tool_call(item) {
            names_by_call_id
                .entry(extract_responses_call_id(item))
                .or_insert_with(|| map_responses_tool_name(forward, &qualified_tool_name(item)));
        }
    }

    let normalized = if native {
        reorder_detached_reasoning(items)
    } else {
        items
    };
    let n = normalized.len();
    let mut consumed = vec![false; n];
    let mut has_encountered_conversation = false;
    let mut pending_developer_parts: Vec<Value> = Vec::new();
    let mut pending_call_ids: Vec<String> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < n {
        'item: {
            if consumed[i] {
                break 'item;
            }
            let item = &normalized[i];
            let raw_type = str_of(item.get("type"));
            let mut role = str_of(item.get("role")).into_owned();
            let mut kind: &str = &raw_type;
            if kind.is_empty() && !role.is_empty() {
                kind = "message";
            } else if is_content_part_type(kind) && role.is_empty() {
                kind = "message";
                role = "user".to_owned();
            }

            match kind {
                "message" => {
                    if equal_fold(&role, "system") || equal_fold(&role, "developer") {
                        if !has_encountered_conversation {
                            pending_call_ids.clear();
                            match item.get("content") {
                                Some(Value::Array(parts)) => {
                                    for part in parts {
                                        system_parts.push(text_part(str_of(part.get("text"))));
                                    }
                                }
                                Some(Value::String(text)) => {
                                    system_parts.push(text_part(text.clone()));
                                }
                                _ => {}
                            }
                            break 'item;
                        }

                        let mut developer_parts = Vec::new();
                        match item.get("content") {
                            Some(Value::Array(parts)) => {
                                let texts: Vec<String> = parts
                                    .iter()
                                    .filter_map(|part| {
                                        let mut text = str_of(part.get("text")).into_owned();
                                        if text.is_empty()
                                            && let Value::String(whole) = part
                                        {
                                            text = whole.clone();
                                        }
                                        (!text.is_empty()).then_some(text)
                                    })
                                    .collect();
                                if !texts.is_empty() {
                                    let joined = texts.join("\n");
                                    if !joined.trim().is_empty() {
                                        developer_parts
                                            .push(text_part(system_reminder_text(&joined)));
                                    }
                                }
                            }
                            Some(Value::String(text)) if !text.trim().is_empty() => {
                                developer_parts.push(text_part(system_reminder_text(text)));
                            }
                            _ => {}
                        }
                        if !developer_parts.is_empty() {
                            if pending_call_ids.is_empty() {
                                contents.push(gemini_content("user", developer_parts));
                            } else {
                                pending_developer_parts.extend(developer_parts);
                            }
                        }
                        break 'item;
                    }

                    has_encountered_conversation = true;
                    if assistant_visible_text(item).is_none() {
                        if !pending_call_ids.is_empty() {
                            let any_has_future_output = pending_call_ids
                                .iter()
                                .any(|call_id| has_matching_output(&normalized[i..], call_id));
                            if !any_has_future_output {
                                let parts: Vec<Value> = pending_call_ids
                                    .iter()
                                    .map(|call_id| {
                                        synthesized_function_response_part(
                                            call_id,
                                            &names_by_call_id,
                                        )
                                    })
                                    .collect();
                                contents.push(gemini_content("user", parts));
                                pending_call_ids.clear();
                            }
                        }
                        if !pending_developer_parts.is_empty() {
                            contents.push(gemini_content(
                                "user",
                                std::mem::take(&mut pending_developer_parts),
                            ));
                        }
                    }

                    // A Responses message can hold model output, `output_text`,
                    // even under the user role; such parts become model turns.
                    let content = item.get("content");
                    let mut parts_to_process: Vec<&Value> = Vec::new();
                    if let Some(Value::Array(parts)) = content {
                        parts_to_process.extend(parts);
                    } else if is_content_part_type(&raw_type) {
                        parts_to_process.push(item);
                        while i + 1 < n {
                            let next = &normalized[i + 1];
                            if str_of(next.get("role")).is_empty()
                                && is_content_part_type(&str_of(next.get("type")))
                            {
                                parts_to_process.push(next);
                                i += 1;
                            } else {
                                break;
                            }
                        }
                    }

                    if !parts_to_process.is_empty() {
                        let mut current_role = String::new();
                        let mut current_parts: Vec<Value> = Vec::new();
                        let flush =
                            |contents: &mut Vec<Value>,
                             current_role: &str,
                             current_parts: &mut Vec<Value>| {
                                let parts = std::mem::take(current_parts);
                                if !current_role.is_empty() && !parts.is_empty() {
                                    contents.push(gemini_content(current_role, parts));
                                }
                            };
                        for part in parts_to_process {
                            let content_type = str_of(part.get("type"));
                            let content_type = if content_type.is_empty() {
                                Cow::Borrowed("input_text")
                            } else {
                                content_type
                            };
                            let mut effective_role = effective_role(&role);
                            if content_type == "output_text" {
                                effective_role = "model".to_owned();
                            }
                            if !current_role.is_empty() && effective_role != current_role {
                                flush(&mut contents, &current_role, &mut current_parts);
                                current_role.clear();
                            }
                            if current_role.is_empty() {
                                current_role = effective_role;
                            }
                            let gemini_part = match &*content_type {
                                "input_text" | "output_text" | "text" => {
                                    part.get("text").map(|text| text_part(str_of(Some(text))))
                                }
                                _ => part_from_block(part),
                            };
                            current_parts.extend(gemini_part);
                        }
                        flush(&mut contents, &current_role, &mut current_parts);
                    } else if let Some(Value::String(text)) = content {
                        contents.push(gemini_content(
                            &effective_role(&role),
                            vec![text_part(text.clone())],
                        ));
                    }
                }

                "function_call" | "custom_tool_call" => {
                    has_encountered_conversation = true;
                    let raw_signature = str_of(item.get(SIGNATURE_FIELD));
                    let raw_signature = raw_signature.trim();
                    let signature = if raw_signature.is_empty() {
                        BYPASS.to_owned()
                    } else {
                        gemini_replay_signature_or_bypass(
                            raw_signature,
                            BlockKind::GeminiFunctionCall,
                        )
                    };
                    let thought = str_of(item.get(SUMMARY_FIELD));
                    contents.push(if !thought.is_empty() {
                        reasoning_function_call_content(&thought, item, &signature, forward)
                    } else if !native && !raw_signature.is_empty() {
                        empty_reasoning_function_call_content(item, &signature, forward)
                    } else {
                        gemini_content("model", vec![function_call_part(item, &signature, forward)])
                    });
                    let call_id = extract_responses_call_id(item);
                    if !call_id.is_empty() {
                        pending_call_ids.push(call_id);
                    }
                }

                "function_call_output" | "custom_tool_call_output" => {
                    has_encountered_conversation = true;
                    let mut end = i + 1;
                    while end < n && is_tool_output(&normalized[end]) {
                        end += 1;
                    }
                    let ordered =
                        order_function_call_outputs(&normalized[i..end], &pending_call_ids);
                    consumed[i..end].fill(true);
                    let has_subsequent = has_subsequent_turn(&normalized[end..]);

                    let mut output_by_call_id: HashMap<String, &Value> = HashMap::new();
                    let mut extra_outputs = Vec::new();
                    for &output in &ordered {
                        let id = extract_responses_call_id(output);
                        if id.is_empty() {
                            extra_outputs.push(output);
                        } else {
                            output_by_call_id.insert(id, output);
                        }
                    }
                    let any_matched = pending_call_ids
                        .iter()
                        .any(|id| output_by_call_id.contains_key(id));

                    let mut response_parts = Vec::new();
                    let mut still_pending = Vec::new();
                    for pending_id in &pending_call_ids {
                        if let Some(output) = output_by_call_id.remove(pending_id) {
                            response_parts.push(function_response_part(output, &names_by_call_id));
                        } else if (has_subsequent || any_matched)
                            && !has_matching_output(&normalized[end..], pending_id)
                        {
                            response_parts.push(synthesized_function_response_part(
                                pending_id,
                                &names_by_call_id,
                            ));
                        } else {
                            still_pending.push(pending_id.clone());
                        }
                    }

                    // An output that answers no call, such as a Codex
                    // send_message_to_thread card, must not become an
                    // unpaired function response; it becomes user text.
                    let mut standalone = Vec::new();
                    let mut push_standalone = |output: &Value| {
                        let parts = standalone_tool_output_text_parts(output);
                        if !parts.is_empty() {
                            standalone.push(gemini_content("user", parts));
                        }
                    };
                    for &output in &ordered {
                        if output_by_call_id
                            .remove(&extract_responses_call_id(output))
                            .is_some()
                        {
                            push_standalone(output);
                        }
                    }
                    for output in extra_outputs {
                        push_standalone(output);
                    }

                    pending_call_ids = still_pending;
                    if !response_parts.is_empty() {
                        contents.push(gemini_content("user", response_parts));
                    }
                    contents.extend(standalone);
                    if pending_call_ids.is_empty() && !pending_developer_parts.is_empty() {
                        contents.push(gemini_content(
                            "user",
                            std::mem::take(&mut pending_developer_parts),
                        ));
                    }
                }

                "reasoning" => {
                    has_encountered_conversation = true;
                    let thought = summary_text(item).into_owned();
                    let mut raw_signature = str_of(item.get("encrypted_content")).into_owned();
                    let carrier_direction = direction(item).into_owned();
                    let carrier_target = target(item).into_owned();
                    if raw_signature.trim().is_empty() && i + 1 < n {
                        let next = &normalized[i + 1];
                        let next_signature = str_of(next.get("encrypted_content"));
                        if str_of(next.get("type")) == "reasoning"
                            && str_of(next.get("id")).contains("_detached_after_")
                            && summary_text(next).trim().is_empty()
                            && !next_signature.trim().is_empty()
                        {
                            raw_signature = next_signature.into_owned();
                            i += 1;
                        }
                    }
                    let signature = if raw_signature.trim().is_empty() {
                        String::new()
                    } else {
                        compatible_signature_for_provider_block(
                            Provider::Gemini,
                            &raw_signature,
                            BlockKind::GeminiModelPart,
                        )
                        .unwrap_or_default()
                    };

                    let mut visible_text = String::new();
                    if native && i + 1 < n {
                        let next = &normalized[i + 1];
                        let binds_next = carrier_direction.is_empty() || carrier_direction == NEXT;
                        let can_bind_text = binds_next
                            && (carrier_target.is_empty()
                                || carrier_target == TEXT
                                || carrier_target == ANY);
                        let can_bind_function = binds_next
                            && (carrier_target.is_empty()
                                || carrier_target == FUNCTION
                                || carrier_target == ANY);
                        if let Some(visible) =
                            assistant_visible_text(next).filter(|_| can_bind_text)
                        {
                            visible_text = visible;
                            i += 1;
                        } else if is_tool_call(next)
                            && can_bind_function
                            && str_of(next.get(SIGNATURE_FIELD)).trim().is_empty()
                        {
                            let signature = if signature.is_empty() {
                                BYPASS
                            } else {
                                &signature
                            };
                            contents.push(reasoning_function_call_content(
                                &thought, next, signature, forward,
                            ));
                            let call_id = extract_responses_call_id(next);
                            if !call_id.is_empty() {
                                pending_call_ids.push(call_id);
                            }
                            i += 1;
                            break 'item;
                        }
                    }
                    contents.extend(reasoning_model_content(
                        &thought,
                        &visible_text,
                        &signature,
                        native,
                    ));
                }

                _ => {}
            }
        }
        i += 1;
    }
    if !pending_developer_parts.is_empty() {
        contents.push(gemini_content("user", pending_developer_parts));
    }
    merge_adjacent_gemini_user_contents(coalesce_model_contents(contents))
}

/// The Gemini role of a message part, before its content type is looked at.
fn effective_role(role: &str) -> String {
    if role.is_empty() {
        return "user".to_owned();
    }
    match go::to_lower(role) {
        lower if lower == "assistant" || lower == "model" => "model".to_owned(),
        lower => lower,
    }
}

/// A tool call's name, qualified by its namespace.
fn qualified_tool_name(item: &Value) -> String {
    let name = str_of(item.get("name"));
    let namespace = str_of(item.get("namespace"));
    if namespace.is_empty() {
        name.into_owned()
    } else {
        qualify_namespace_tool_name(&namespace, &name)
    }
}

/// `{"text": ...}`.
fn text_part(text: impl Into<String>) -> Value {
    object([("text", Value::String(text.into()))])
}

/// `geminiContent`.
fn gemini_content(role: &str, parts: Vec<Value>) -> Value {
    object([("role", role.into()), ("parts", Value::Array(parts))])
}

/// Go's `strings.EqualFold` against an ASCII `word`. Two non-ASCII
/// characters fold to ASCII letters: the long s to `s` and the Kelvin sign
/// to `k`.
pub(super) fn equal_fold(text: &str, word: &str) -> bool {
    let mut chars = text.chars();
    let folds = word.chars().all(|w| {
        chars.next().is_some_and(|c| {
            c.eq_ignore_ascii_case(&w)
                || match w.to_ascii_lowercase() {
                    's' => c == '\u{17f}',
                    'k' => c == '\u{212a}',
                    _ => false,
                }
        })
    });
    folds && chars.next().is_none()
}

/// `coalesceAdjacentOpenAIResponsesModelContents`: each model turn's parts
/// joined onto a model turn just before it.
fn coalesce_model_contents(contents: Vec<Value>) -> Vec<Value> {
    let is_model = |content: &Value| equal_fold(str_of(content.get("role")).trim(), "model");
    let mut coalesced: Vec<Value> = Vec::with_capacity(contents.len());
    for content in contents {
        let Some(last) = coalesced
            .last_mut()
            .filter(|last| is_model(&content) && is_model(last))
        else {
            coalesced.push(content);
            continue;
        };
        let Some(Value::Array(extra)) = content.get("parts") else {
            coalesced.push(content);
            continue;
        };
        if !extra.is_empty() {
            let mut parts = array_of(last.get("parts")).to_vec();
            parts.extend(extra.iter().cloned());
            last["parts"] = Value::Array(parts);
        }
    }
    coalesced
}

/// `stripTrailingOpenAIResponsesModelPrefill`: drops a last model turn that
/// holds only plain text: an assistant prefill Gemini won't continue.
fn strip_trailing_model_prefill(out: &mut Value) {
    let Some(Value::Array(contents)) = out.get_mut("contents") else {
        return;
    };
    let strip = contents.last().is_some_and(|last| {
        str_of(last.get("role")) == "model"
            && match last.get("parts") {
                Some(Value::Array(parts)) => !parts.iter().any(|part| {
                    part.get("thought").is_some_and(bool_of)
                        || part.get("functionCall").is_some()
                        || !str_of(part.get("thoughtSignature")).trim().is_empty()
                }),
                _ => false,
            }
    });
    if strip {
        contents.pop();
    }
}

/// `openAIResponsesAssistantVisibleText`: the text a message shows as model
/// output, if it is one: an assistant or model message's string content, or
/// the `output_text` parts of any message.
pub(super) fn assistant_visible_text(item: &Value) -> Option<String> {
    let kind = str_of(item.get("type"));
    let role = str_of(item.get("role"));
    if !(kind == "message" || (kind.is_empty() && !role.is_empty())) {
        return None;
    }
    match item.get("content")? {
        Value::String(content) => {
            matches!(go::to_lower(role.trim()).as_str(), "assistant" | "model")
                .then(|| content.clone())
        }
        Value::Array(parts) => {
            let texts: Vec<Cow<'_, str>> = parts
                .iter()
                .filter(|part| str_of(part.get("type")) == "output_text")
                .map(|part| str_of(part.get("text")))
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

/// `isOpenAIResponsesToolCall`.
fn is_tool_call(item: &Value) -> bool {
    matches!(
        &*str_of(item.get("type")),
        "function_call" | "custom_tool_call"
    )
}

/// `isOpenAIResponsesToolOutput`.
fn is_tool_output(item: &Value) -> bool {
    matches!(
        &*str_of(item.get("type")),
        "function_call_output" | "custom_tool_call_output"
    )
}

/// Sets an internal field on an item.
fn set_field(item: &mut Value, field: &str, value: &str) {
    if let Value::Object(fields) = item {
        fields.insert(field.to_owned(), Value::String(value.to_owned()));
    }
}

/// `pairOpenAIResponsesReasoningWithFunctionCalls`: moves each signature
/// that belongs to a tool call onto the call, as `_cpa_reasoning_signature`
/// (and a reasoning summary as `_cpa_reasoning_summary`), dropping the
/// reasoning item that carried it.
///
/// In a run of calls and carriers followed by outputs, a run that starts
/// with a call takes a carrier after a call as that call's (when the call
/// has an output in the run); otherwise a reasoning item goes with the call
/// after it.
fn pair_reasoning_with_function_calls(mut items: Vec<Value>) -> Vec<Value> {
    let n = items.len();
    let mut post_call_signature: HashMap<usize, String> = HashMap::new();
    let mut post_call_carrier = HashSet::new();
    let mut consumed_post_call_carrier = HashSet::new();
    let in_group = |item: &Value| is_tool_call(item) || is_detached_carrier(item);
    let mut group_start = 0;
    while group_start < n {
        if !in_group(&items[group_start]) {
            group_start += 1;
            continue;
        }
        let mut group_end = group_start;
        let mut has_function_call = false;
        while group_end < n && in_group(&items[group_end]) {
            has_function_call |= is_tool_call(&items[group_end]);
            group_end += 1;
        }
        if !has_function_call || group_end >= n || !is_tool_output(&items[group_end]) {
            group_start = group_end;
            continue;
        }
        let mut output_end = group_end;
        while output_end < n && is_tool_output(&items[output_end]) {
            output_end += 1;
        }
        // A run starting with a carrier binds carriers to the call after
        // them; one starting with a call, to the call before them. This keeps
        // both carrier,call,carrier,call and call,carrier,call,carrier.
        if is_tool_call(&items[group_start]) {
            for call_index in group_start..group_end {
                let item = &items[call_index];
                if !is_tool_call(item)
                    || !str_of(item.get(SIGNATURE_FIELD)).trim().is_empty()
                    || call_index + 1 >= group_end
                    || !is_detached_carrier(&items[call_index + 1])
                {
                    continue;
                }
                let carrier = &items[call_index + 1];
                let carrier_direction = direction(carrier);
                let carrier_target = target(carrier);
                if !carrier_direction.is_empty()
                    && (carrier_direction != PREVIOUS
                        || (carrier_target != FUNCTION && carrier_target != ANY))
                {
                    continue;
                }
                let mut carrier_end = call_index + 1;
                while carrier_end < group_end && is_detached_carrier(&items[carrier_end]) {
                    post_call_carrier.insert(carrier_end);
                    carrier_end += 1;
                }
                let call_id = extract_responses_call_id(item);
                if call_id.is_empty() {
                    continue;
                }
                if items[group_end..output_end]
                    .iter()
                    .any(|output| extract_responses_call_id(output) == call_id)
                {
                    post_call_signature.insert(
                        call_index,
                        str_of(carrier.get("encrypted_content")).trim().to_owned(),
                    );
                    consumed_post_call_carrier.insert(call_index + 1);
                }
            }
        }
        group_start = output_end;
    }

    let mut paired = Vec::with_capacity(n);
    let mut index = 0;
    while index < n {
        if let Some(signature) = post_call_signature.get(&index).filter(|s| !s.is_empty()) {
            let mut call = std::mem::take(&mut items[index]);
            set_field(&mut call, SIGNATURE_FIELD, signature);
            paired.push(call);
            index += 1;
            continue;
        }
        if consumed_post_call_carrier.contains(&index) {
            index += 1;
            continue;
        }
        let item = &items[index];
        let carrier_direction = direction(item);
        let carrier_target = target(item);
        let can_bind_following_call = carrier_direction.is_empty()
            || (carrier_direction == NEXT && (carrier_target == FUNCTION || carrier_target == ANY));
        if str_of(item.get("type")) == "reasoning"
            && !post_call_carrier.contains(&index)
            && can_bind_following_call
            && !str_of(item.get("id")).contains("_detached_after_")
            && index + 1 < n
            && is_tool_call(&items[index + 1])
        {
            let raw_signature = str_of(item.get("encrypted_content")).trim().to_owned();
            if !raw_signature.is_empty() {
                let summary = summary_text(item).into_owned();
                let mut call = std::mem::take(&mut items[index + 1]);
                set_field(&mut call, SIGNATURE_FIELD, &raw_signature);
                if !summary.is_empty() {
                    set_field(&mut call, SUMMARY_FIELD, &summary);
                }
                paired.push(call);
                index += 2;
                continue;
            }
        }
        paired.push(std::mem::take(&mut items[index]));
        index += 1;
    }
    paired
}

/// `reorderOpenAIResponsesDetachedReasoning`: moves a carrier that follows
/// the item it belongs to ahead of it, so it precedes it as Gemini laid
/// them out.
fn reorder_detached_reasoning(items: Vec<Value>) -> Vec<Value> {
    let mut reordered: Vec<Value> = Vec::with_capacity(items.len());
    let mut iter = items.into_iter().peekable();
    while let Some(mut item) = iter.next() {
        let marked_detached = str_of(item.get("id")).contains("_detached_after_");
        if is_detached_carrier(&item) && !reordered.is_empty() {
            let last = reordered.len() - 1;
            let previous = &reordered[last];
            let mut previous_type = str_of(previous.get("type")).into_owned();
            if previous_type.is_empty() && !str_of(previous.get("role")).is_empty() {
                previous_type = "message".to_owned();
            }
            let mut is_assistant_message =
                previous_type == "message" && assistant_visible_text(previous).is_some();
            let previous_is_unsigned_call = (previous_type == "function_call"
                || previous_type == "custom_tool_call")
                && str_of(previous.get(SIGNATURE_FIELD)).trim().is_empty();
            let prior = last.checked_sub(1).map(|prior| &reordered[prior]);

            let carrier_direction = direction(&item).into_owned();
            if !carrier_direction.is_empty() {
                let carrier_target = target(&item).into_owned();
                let (already_paired_text, already_paired_function) = match prior {
                    Some(prior) => {
                        let prior_direction = direction(prior);
                        let prior_target = target(prior);
                        let binds_following = is_detached_carrier(prior)
                            && (prior_direction == NEXT || prior_direction == PREVIOUS);
                        (
                            binds_following && (prior_target == TEXT || prior_target == ANY),
                            binds_following && (prior_target == FUNCTION || prior_target == ANY),
                        )
                    }
                    None => (false, false),
                };
                let bind_previous_message = carrier_direction == PREVIOUS
                    && (carrier_target == TEXT || carrier_target == ANY)
                    && is_assistant_message
                    && !already_paired_text;
                let bind_previous_function = carrier_direction == PREVIOUS
                    && (carrier_target == FUNCTION || carrier_target == ANY)
                    && previous_is_unsigned_call
                    && !already_paired_function;
                if bind_previous_message || bind_previous_function {
                    set_field(&mut item, signature_carrier::DIRECTION_FIELD, NEXT);
                    let previous = std::mem::replace(&mut reordered[last], item);
                    reordered.push(previous);
                } else {
                    reordered.push(item);
                }
                continue;
            }

            if is_assistant_message
                && !marked_detached
                && let Some(next) = iter.peek()
            {
                is_assistant_message = assistant_visible_text(next).is_none();
            }
            let already_paired = prior.is_some_and(|prior| {
                is_detached_carrier(prior) && str_of(prior.get("id")).contains("_detached_after_")
            });
            if !already_paired
                && (is_assistant_message || (marked_detached && previous_is_unsigned_call))
            {
                let previous = std::mem::replace(&mut reordered[last], item);
                reordered.push(previous);
                continue;
            }
        }
        reordered.push(item);
    }
    reordered
}

/// What a function call's `arguments` text reads as, as gjson's `Parse`
/// reads it.
enum Arguments {
    /// A JSON object or array.
    Container(Value),
    /// Starts like an object or array but isn't valid JSON.
    Invalid,
    /// Anything else.
    Other,
}

fn parse_arguments(arguments: &str) -> Arguments {
    let start = arguments.bytes().position(|b| b > b' ');
    match start.map(|start| (start, arguments.as_bytes()[start])) {
        Some((start, b'{' | b'[')) => match serde_json::from_str(&arguments[start..]) {
            Ok(value) => Arguments::Container(value),
            Err(_) => Arguments::Invalid,
        },
        _ => Arguments::Other,
    }
}

/// `buildOpenAIResponsesFunctionCallPart`.
fn function_call_part(item: &Value, signature: &str, forward: &HashMap<String, String>) -> Value {
    let name = map_responses_tool_name(forward, &qualified_tool_name(item));
    let mut args = Value::Object(Map::new());
    if str_of(item.get("type")) == "custom_tool_call" {
        args["input"] = match item.get("input") {
            Some(Value::String(input)) => Value::String(input.clone()),
            Some(input) => input.clone(),
            None => Value::String(String::new()),
        };
    } else {
        let arguments = str_of(item.get("arguments"));
        if !arguments.is_empty() {
            match parse_arguments(&arguments) {
                Arguments::Container(value) => args = value,
                Arguments::Invalid | Arguments::Other => {
                    args["arguments"] = Value::String(arguments.into_owned());
                }
            }
        }
    }
    object([
        (
            "functionCall",
            object([
                ("name", Value::String(name)),
                ("args", args),
                ("id", Value::String(extract_responses_call_id(item))),
            ]),
        ),
        ("thoughtSignature", signature.into()),
    ])
}

/// `geminiResponsesInlineDataPart`.
fn inline_data_part(media: Media) -> Value {
    object([(
        "inline_data",
        object([
            ("mime_type", Value::String(media.mime_type)),
            ("data", Value::String(media.data)),
        ]),
    )])
}

/// `geminiResponsesFileDataPart`.
fn file_data_part(mime_type: String, file_uri: &str) -> Value {
    object([(
        "file_data",
        object([
            ("mime_type", Value::String(mime_type)),
            ("file_uri", file_uri.into()),
        ]),
    )])
}

/// A media part's MIME type and data.
#[derive(Debug)]
struct Media {
    mime_type: String,
    data: String,
}

fn media(mime_type: impl Into<String>, data: impl Into<String>) -> Option<Media> {
    Some(Media {
        mime_type: mime_type.into(),
        data: data.into(),
    })
}

/// gjson `Get(path).String()` on a block, for a dotted path of keys.
fn field(block: &Value, at: &str) -> String {
    str_of(path(block, at)).into_owned()
}

/// `firstNonEmpty` over fields of a block.
fn first_field(block: &Value, paths: &[&str]) -> String {
    paths
        .iter()
        .map(|at| field(block, at))
        .find(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

/// `firstNonGenericFormat` over fields of a block.
fn first_format(block: &Value, paths: &[&str]) -> String {
    paths
        .iter()
        .map(|at| field(block, at).trim().to_owned())
        .find(|value| !value.is_empty() && !is_generic_mime(value))
        .unwrap_or_default()
}

/// `isDataURL`.
fn is_data_url(raw: &str) -> bool {
    go::to_lower(raw.trim()).starts_with("data:")
}

/// `isRemoteURL`.
fn is_remote_url(url: &str) -> bool {
    let lower = go::to_lower(url.trim());
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("gs://")
}

/// `isGenericMIME`.
fn is_generic_mime(mime_type: &str) -> bool {
    matches!(
        go::to_lower(mime_type.trim()).as_str(),
        "" | "application/octet-stream" | "binary/octet-stream"
    )
}

/// `parseOpenAIResponsesDataURL`: the MIME type and base64 payload of a
/// `data:` URL marked `base64` whose payload decodes.
fn parse_data_url(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim();
    if trimmed.len() < 5 || !trimmed.as_bytes()[..5].eq_ignore_ascii_case(b"data:") {
        return None;
    }
    // The first five bytes are ASCII, so this is a character boundary.
    let (metadata, payload) = trimmed[5..].split_once(',')?;
    let payload = payload.trim();
    if payload.is_empty() {
        return None;
    }
    let mut fields = metadata.split(';');
    let mime_type = fields.next().unwrap_or_default().trim();
    if !fields.any(|field| equal_fold(field.trim(), "base64")) {
        return None;
    }
    if go::base64::STD.decode(payload).is_err() && go::base64::RAW_STD.decode(payload).is_err() {
        return None;
    }
    Some((mime_type.to_owned(), payload.to_owned()))
}

/// `isResponsesContentPartType`.
fn is_content_part_type(kind: &str) -> bool {
    matches!(
        go::to_lower(kind.trim()).as_str(),
        "input_text"
            | "output_text"
            | "text"
            | "input_image"
            | "image_url"
            | "image"
            | "input_audio"
            | "audio"
            | "input_video"
            | "video_url"
            | "video"
            | "input_file"
            | "file"
    )
}

/// Go's `filepath.Ext` without its dot, on Unix: what follows the last dot
/// of the last path element.
fn extension(filename: &str) -> &str {
    let name = filename.rsplit('/').next().unwrap_or_default();
    name.rsplit_once('.').map_or("", |(_, extension)| extension)
}

/// `openAIResponsesAudioMimeType`.
fn audio_mime_type(format: &str) -> String {
    let format = format.trim();
    if is_generic_mime(format) {
        return "audio/wav".to_owned();
    }
    if format.contains('/') {
        return format.to_owned();
    }
    let lower = go::to_lower(format);
    match lower.as_str() {
        "wav" => "audio/wav",
        "mp3" | "mpeg" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "webm" => "audio/webm",
        "pcm16" | "pcm" => "audio/pcm",
        "g711_ulaw" | "g711_alaw" => "audio/basic",
        "opus" => "audio/opus",
        "m4a" => "audio/mp4",
        "wma" => "audio/x-ms-wma",
        other => mime_type(other)
            .filter(|mapped| mapped.starts_with("audio/"))
            .unwrap_or("audio/wav"),
    }
    .to_owned()
}

/// `openAIResponsesVideoMimeType`.
fn video_mime_type(format: &str) -> String {
    let format = format.trim();
    if is_generic_mime(format) {
        return "video/mp4".to_owned();
    }
    if format.contains('/') {
        return format.to_owned();
    }
    let lower = go::to_lower(format);
    match lower.as_str() {
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" | "quicktime" => "video/quicktime",
        "avi" | "x-msvideo" => "video/x-msvideo",
        "mpeg" => "video/mpeg",
        "ogg" => "video/ogg",
        "mkv" | "x-matroska" => "video/x-matroska",
        "flv" | "x-flv" => "video/x-flv",
        "3gpp" => "video/3gpp",
        other => mime_type(other)
            .filter(|mapped| mapped.starts_with("video/"))
            .unwrap_or("video/mp4"),
    }
    .to_owned()
}

/// The MIME type of decoded `data:` URL media: its own unless generic, else
/// from the format, else from the file name, else `fallback`.
fn data_url_media(
    url: &str,
    format: &str,
    filename: &str,
    mime_of: fn(&str) -> String,
    fallback: &str,
) -> Option<Media> {
    let (mut mime, data) = parse_data_url(url)?;
    if is_generic_mime(&mime) {
        mime = if !format.is_empty() && !is_generic_mime(format) {
            mime_of(format)
        } else if !filename.is_empty() {
            mime_of(extension(filename))
        } else {
            fallback.to_owned()
        };
    }
    media(mime, data)
}

/// The MIME type of media given as bare data: from the format, or from the
/// file name when the format is generic.
fn bare_media_mime(format: &str, filename: &str, mime_of: fn(&str) -> String) -> String {
    if is_generic_mime(format) && !filename.is_empty() {
        mime_of(extension(filename))
    } else {
        mime_of(format)
    }
}

/// `openAIResponsesAudioFromBlock`.
fn audio_from_block(block: &Value) -> Option<Media> {
    let kind = go::to_lower(field(block, "type").trim());
    if kind != "input_audio" && kind != "audio" {
        return None;
    }
    let filename = first_field(block, &["filename", "file.filename"]);
    let audio_key = if block.get("input_audio").is_some() {
        "input_audio"
    } else {
        "audio"
    };
    let mut format = first_format(
        block,
        &[
            &format!("{audio_key}.format"),
            &format!("{audio_key}.mime_type"),
            "format",
            "mime_type",
        ],
    );

    let mut data = field(block, &format!("{audio_key}.data"));
    if data.is_empty() {
        data = field(block, "data");
    }
    if data.is_empty() {
        let url = first_field(block, &["audio_url.url", "audio_url", "url"]);
        if !url.is_empty() {
            if is_data_url(&url) {
                return data_url_media(&url, &format, &filename, audio_mime_type, "audio/wav");
            } else if !is_remote_url(&url) {
                return media(bare_media_mime(&format, &filename, audio_mime_type), url);
            }
        }
    }
    if data.is_empty() && field(block, "source.type") == "base64" {
        data = field(block, "source.data");
        if format.is_empty() {
            format = field(block, "source.media_type");
        }
    }
    if data.is_empty() {
        return None;
    }
    if is_data_url(&data) {
        return data_url_media(&data, &format, &filename, audio_mime_type, "audio/wav");
    }
    media(bare_media_mime(&format, &filename, audio_mime_type), data)
}

/// `openAIResponsesVideoFromBlock`.
fn video_from_block(block: &Value) -> Option<Media> {
    let kind = go::to_lower(field(block, "type").trim());
    if kind != "input_video" && kind != "video_url" && kind != "video" {
        return None;
    }
    let filename = first_field(block, &["filename", "file.filename"]);
    let video_key = if block.get("input_video").is_some() {
        "input_video"
    } else {
        "video"
    };
    let mut format = first_format(
        block,
        &[
            &format!("{video_key}.format"),
            &format!("{video_key}.mime_type"),
            "format",
            "mime_type",
        ],
    );

    let url = first_field(block, &["video_url.url", "video_url", "url"]);
    if !url.is_empty() {
        if is_data_url(&url) {
            return data_url_media(&url, &format, &filename, video_mime_type, "video/mp4");
        } else if !is_remote_url(&url) {
            return media(bare_media_mime(&format, &filename, video_mime_type), url);
        }
    }
    let mut data = field(block, &format!("{video_key}.data"));
    if data.is_empty() {
        data = field(block, "data");
    }
    if data.is_empty() && field(block, "source.type") == "base64" {
        data = field(block, "source.data");
        if format.is_empty() {
            format = field(block, "source.media_type");
        }
    }
    if data.is_empty() {
        return None;
    }
    if is_data_url(&data) {
        return data_url_media(&data, &format, &filename, video_mime_type, "video/mp4");
    }
    media(bare_media_mime(&format, &filename, video_mime_type), data)
}

/// `normalizeFormatToMIME`: a format or extension as a MIME type, or `""`.
fn normalize_format_to_mime(format: &str) -> String {
    let format = format.trim();
    if format.is_empty() || is_generic_mime(format) {
        return String::new();
    }
    if format.contains('/') {
        return format.to_owned();
    }
    let lower = go::to_lower(format);
    match lower.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "pdf" => "application/pdf",
        other => mime_type(other).unwrap_or_default(),
    }
    .to_owned()
}

/// `openAIResponsesFileFromBlock`.
fn file_from_block(block: &Value) -> Option<Media> {
    let kind = go::to_lower(field(block, "type").trim());
    if kind != "input_file" && kind != "file" {
        return None;
    }
    let filename = first_field(block, &["filename", "file.filename"]);
    let mut file_data = first_field(block, &["file_data", "file.file_data", "data"]);
    if file_data.is_empty() {
        let url = first_field(block, &["file_url.url", "file_url", "file.file_url", "url"]);
        if is_data_url(&url) {
            file_data = url;
        }
    }

    let mut fallback = first_format(
        block,
        &["mime_type", "file.mime_type", "format", "file.format"],
    );
    if !fallback.is_empty() {
        fallback = normalize_format_to_mime(&fallback);
    }
    if is_generic_mime(&fallback) && !filename.is_empty() {
        let extension = go::to_lower(extension(&filename));
        if !extension.is_empty() {
            fallback = normalize_format_to_mime(&extension);
        }
    }

    if is_data_url(&file_data) {
        let (mut mime, data) = parse_data_url(&file_data)?;
        if is_generic_mime(&mime) && !fallback.is_empty() {
            mime = fallback;
        }
        if is_generic_mime(&mime) && !filename.is_empty() {
            let extension = go::to_lower(extension(&filename));
            if !extension.is_empty() {
                let normalized = normalize_format_to_mime(&extension);
                if !normalized.is_empty() {
                    mime = normalized;
                }
            }
        }
        if is_generic_mime(&mime) {
            mime = "application/octet-stream".to_owned();
        }
        return media(mime, data);
    }

    normalize_openai_file_data(&filename, &fallback, &file_data)
        .and_then(|file| media(file.mime_type, file.data))
}

/// `openAIResponsesMediaFromBlock`.
fn media_from_block(block: &Value) -> Option<Media> {
    image_from_block(block)
        .or_else(|| audio_from_block(block))
        .or_else(|| video_from_block(block))
        .or_else(|| file_from_block(block))
}

/// `openAIResponsesPartFromBlock`: a remote URL as a `file_data` part, or
/// inline media as an `inline_data` part.
fn part_from_block(block: &Value) -> Option<Value> {
    let kind = go::to_lower(field(block, "type").trim());
    let url = first_field(
        block,
        &[
            "video_url.url",
            "video_url",
            "audio_url.url",
            "audio_url",
            "image_url.url",
            "image_url",
            "file_url.url",
            "file_url",
            "file.file_url",
            "url",
        ],
    );
    if is_remote_url(&url) {
        let mut filename = first_field(block, &["filename", "file.filename"]);
        if filename.is_empty() {
            filename = url_path_base(&url).unwrap_or_default();
        }
        let format = first_format(
            block,
            &[
                "format",
                "mime_type",
                "input_video.format",
                "input_video.mime_type",
                "video.format",
                "video.mime_type",
                "input_audio.format",
                "input_audio.mime_type",
                "audio.format",
                "audio.mime_type",
                "input_image.format",
                "input_image.mime_type",
                "image.format",
                "image.mime_type",
                "file.format",
                "file.mime_type",
            ],
        );
        let extension = go::to_lower(extension(&filename));
        let mime = match kind.as_str() {
            "input_video" | "video_url" | "video" => {
                let mut mime = String::new();
                if !format.is_empty() && !is_generic_mime(&format) {
                    mime = video_mime_type(&format);
                } else if !extension.is_empty() {
                    mime = video_mime_type(&extension);
                }
                if is_generic_mime(&mime) {
                    mime = "video/mp4".to_owned();
                }
                mime
            }
            "input_audio" | "audio" => {
                let mut mime = String::new();
                if !format.is_empty() && !is_generic_mime(&format) {
                    mime = audio_mime_type(&format);
                } else if !extension.is_empty() {
                    mime = audio_mime_type(&extension);
                }
                if is_generic_mime(&mime) {
                    mime = "audio/wav".to_owned();
                }
                mime
            }
            "input_image" | "image_url" | "image" => image_mime_type(&format, &filename),
            _ => {
                let mut mime = String::new();
                if !format.is_empty() {
                    mime = normalize_format_to_mime(&format);
                }
                if is_generic_mime(&mime) && !extension.is_empty() {
                    mime = normalize_format_to_mime(&extension);
                }
                if is_generic_mime(&mime) {
                    mime = "application/octet-stream".to_owned();
                }
                mime
            }
        };
        return Some(file_data_part(mime, &url));
    }
    media_from_block(block).map(inline_data_part)
}

/// `openAIResponsesImageMimeType`.
fn image_mime_type(format: &str, filename: &str) -> String {
    let format = format.trim();
    if !format.is_empty() && !is_generic_mime(format) {
        if format.contains('/') {
            return format.to_owned();
        }
        let lower = go::to_lower(format);
        if lower == "jpg" || lower == "jpeg" {
            return "image/jpeg".to_owned();
        }
        if let Some(mapped) = mime_type(&lower) {
            return mapped.to_owned();
        }
        return format!("image/{format}");
    }
    if !filename.is_empty() {
        let extension = go::to_lower(extension(filename));
        if extension == "jpg" || extension == "jpeg" {
            return "image/jpeg".to_owned();
        }
        if !extension.is_empty()
            && let Some(mapped) = mime_type(&extension)
        {
            return mapped.to_owned();
        }
    }
    "image/png".to_owned()
}

/// `openAIResponsesImageFromBlock`.
fn image_from_block(block: &Value) -> Option<Media> {
    let kind = go::to_lower(field(block, "type").trim());
    if !matches!(kind.as_str(), "input_image" | "image_url" | "image") {
        return None;
    }
    let mut format = first_format(
        block,
        &[
            "format",
            "mime_type",
            "input_image.format",
            "input_image.mime_type",
            "image.format",
            "image.mime_type",
        ],
    );
    let filename = first_field(block, &["filename", "file.filename"]);

    let url = first_field(block, &["image_url.url", "image_url", "url"]);
    if !url.is_empty() {
        if is_data_url(&url) {
            let (mut mime, data) = parse_data_url(&url)?;
            if is_generic_mime(&mime) {
                mime = image_mime_type(&format, &filename);
            }
            return media(mime, data);
        } else if !is_remote_url(&url) {
            return media(image_mime_type(&format, &filename), url);
        }
    }

    let mut data = String::new();
    if field(block, "source.type") == "base64" {
        data = field(block, "source.data");
        if format.is_empty() {
            format = field(block, "source.media_type");
        }
    }
    if data.is_empty() && block.get("data").is_some() {
        data = field(block, "data");
    }
    if data.is_empty() {
        return None;
    }
    if is_data_url(&data) {
        let (mut mime, data) = parse_data_url(&data)?;
        if is_generic_mime(&mime) {
            mime = image_mime_type(&format, &filename);
        }
        return media(mime, data);
    }
    media(image_mime_type(&format, &filename), data)
}

/// An entry of a tool output array that isn't media.
struct OutputBlock {
    text: String,
    is_text: bool,
    raw: String,
}

/// `parseOpenAIResponsesArrayOutput`: a tool output array as a result, as
/// text or (when `.1`) JSON, and its media.
fn parse_array_output(output: &Value) -> (String, bool, Vec<Media>) {
    let mut images = Vec::new();
    let mut entries = Vec::new();
    let mut has_content_block = false;
    let mut has_non_text_block = false;
    for block in array_of(Some(output)) {
        if let Some(found) = media_from_block(block) {
            has_content_block = true;
            images.push(found);
            continue;
        }
        let kind = str_of(block.get("type"));
        if kind == "input_text" || kind == "output_text" || kind == "text" {
            has_content_block = true;
            entries.push(OutputBlock {
                text: str_of(block.get("text")).into_owned(),
                is_text: true,
                raw: block.to_string(),
            });
        } else if let Value::String(text) = block {
            entries.push(OutputBlock {
                text: text.clone(),
                is_text: true,
                raw: block.to_string(),
            });
        } else {
            has_non_text_block = true;
            entries.push(OutputBlock {
                text: block.to_string(),
                is_text: false,
                raw: block.to_string(),
            });
        }
    }

    if !has_content_block {
        return (output.to_string(), true, Vec::new());
    }
    match entries.as_slice() {
        [] => (String::new(), false, images),
        [entry] if entry.is_text => (entry.text.clone(), false, images),
        [entry] => (entry.raw.clone(), true, images),
        _ if !has_non_text_block => {
            let texts: Vec<&str> = entries.iter().map(|entry| entry.text.as_str()).collect();
            (texts.join("\n"), false, images)
        }
        _ => {
            let raws: Vec<&str> = entries.iter().map(|entry| entry.raw.as_str()).collect();
            (format!("[{}]", raws.join(",")), true, images)
        }
    }
}

/// `buildOpenAIResponsesSynthesizedFunctionResponsePart`: the response for
/// a call whose output never came.
fn synthesized_function_response_part(
    call_id: &str,
    names_by_call_id: &HashMap<String, String>,
) -> Value {
    let name = names_by_call_id
        .get(call_id)
        .filter(|name| !name.is_empty())
        .map_or("unknown", String::as_str);
    let mut response = object([
        ("name", Value::String(sanitize_gemini_function_name(name))),
        (
            "response",
            object([("result", "call interrupted, no output".into())]),
        ),
    ]);
    if !call_id.is_empty() {
        response["id"] = call_id.into();
    }
    object([("functionResponse", response)])
}

/// `responsesHasMatchingOutput`.
fn has_matching_output(items: &[Value], call_id: &str) -> bool {
    !call_id.is_empty()
        && items
            .iter()
            .any(|item| is_tool_output(item) && extract_responses_call_id(item) == call_id)
}

/// `responsesHasSubsequentTurn`: whether a message or a tool call follows.
fn has_subsequent_turn(items: &[Value]) -> bool {
    items.iter().any(|item| {
        let kind = str_of(item.get("type"));
        kind == "message"
            || (kind.is_empty() && !str_of(item.get("role")).is_empty())
            || kind == "function_call"
            || kind == "custom_tool_call"
    })
}

/// `buildOpenAIResponsesStandaloneToolOutputTextParts`.
fn standalone_tool_output_text_parts(item: &Value) -> Vec<Value> {
    match item.get("output") {
        None => Vec::new(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| str_of(part.get("text")))
            .filter(|text| !text.trim().is_empty())
            .map(text_part)
            .collect(),
        Some(output) => {
            let text = str_of(Some(output));
            if text.trim().is_empty() {
                Vec::new()
            } else {
                vec![text_part(text)]
            }
        }
    }
}

/// `buildOpenAIResponsesFunctionResponseParts`: a tool call output as a
/// `functionResponse` part, with any media it returned as its parts.
fn function_response_part(item: &Value, names_by_call_id: &HashMap<String, String>) -> Value {
    let call_id = extract_responses_call_id(item);
    let name = match names_by_call_id.get(&call_id) {
        Some(name) => name.clone(),
        None => {
            let name = str_of(item.get("name"));
            match name.trim() {
                "" => "unknown".to_owned(),
                name => name.to_owned(),
            }
        }
    };
    let mut part = object([(
        "functionResponse",
        object([
            ("name", Value::String(sanitize_gemini_function_name(&name))),
            ("response", Value::Object(Map::new())),
            ("id", Value::String(call_id)),
        ]),
    )]);

    let mut images = Vec::new();
    match item.get("output") {
        Some(Value::String(output)) => {
            if output.is_empty() || output == "null" {
                return part;
            }
            // Kept as text rather than parsed: Gemini can reject a result
            // parsed from JSON text, such as a file read by a tool.
            set_path(&mut part, RESULT_PATH, Value::String(output.clone()));
            return part;
        }
        Some(output @ Value::Array(_)) => {
            let (result, is_raw, found) = parse_array_output(output);
            images = found;
            if is_raw {
                set_gemini_function_response_raw(&mut part, RESULT_PATH, &result);
            } else {
                set_path(&mut part, RESULT_PATH, Value::String(result));
            }
        }
        Some(output @ Value::Object(_)) => match media_from_block(output) {
            Some(found) => {
                images.push(found);
                set_path(&mut part, RESULT_PATH, Value::String(String::new()));
            }
            None => {
                set_gemini_function_response_result(&mut part, RESULT_PATH, Some(output.clone()))
            }
        },
        None | Some(Value::Null) => {}
        Some(output) => {
            set_path(
                &mut part,
                RESULT_PATH,
                Value::String(str_of(Some(output)).into_owned()),
            );
        }
    }

    if !images.is_empty()
        && let Some(Value::Object(response)) = part.get_mut("functionResponse")
    {
        let parts = response
            .entry("parts")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(parts) = parts {
            for image in images {
                parts.push(object([(
                    "inlineData",
                    object([
                        ("mimeType", Value::String(image.mime_type)),
                        ("data", Value::String(image.data)),
                    ]),
                )]));
            }
        }
    }
    part
}

/// `orderOpenAIResponsesFunctionCallOutputs`: the outputs in the order of
/// the calls waiting for them, then the rest in their order.
fn order_function_call_outputs<'v>(
    outputs: &'v [Value],
    pending_call_ids: &[String],
) -> Vec<&'v Value> {
    let mut ordered = Vec::with_capacity(outputs.len());
    let mut used = vec![false; outputs.len()];
    for pending_id in pending_call_ids {
        if let Some(found) = outputs.iter().enumerate().position(|(index, output)| {
            !used[index] && extract_responses_call_id(output) == *pending_id
        }) {
            used[found] = true;
            ordered.push(&outputs[found]);
        }
    }
    for (index, output) in outputs.iter().enumerate() {
        if !used[index] {
            ordered.push(output);
        }
    }
    ordered
}

/// `buildOpenAIResponsesEmptyReasoningFunctionCallModelContent`: a call
/// after an empty thought holding its signature.
fn empty_reasoning_function_call_content(
    item: &Value,
    signature: &str,
    forward: &HashMap<String, String>,
) -> Value {
    let thought = object([
        ("text", "".into()),
        ("thought", true.into()),
        ("thoughtSignature", signature.into()),
    ]);
    gemini_content(
        "model",
        vec![thought, function_call_part(item, signature, forward)],
    )
}

/// `buildOpenAIResponsesReasoningFunctionCallModelContent`: a call after the
/// thought that led to it.
fn reasoning_function_call_content(
    thought: &str,
    item: &Value,
    signature: &str,
    forward: &HashMap<String, String>,
) -> Value {
    let mut parts = Vec::with_capacity(2);
    if !thought.is_empty() {
        parts.push(object([("text", thought.into()), ("thought", true.into())]));
    }
    parts.push(function_call_part(item, signature, forward));
    gemini_content("model", parts)
}

/// `buildOpenAIResponsesReasoningModelContent`: a reasoning item, and the
/// visible text it is bound to, as a model turn.
fn reasoning_model_content(
    thought: &str,
    visible: &str,
    signature: &str,
    native: bool,
) -> Option<Value> {
    let has_real_signature = !signature.is_empty() && signature != BYPASS;
    let mut parts = Vec::new();
    if native {
        if thought.is_empty() && visible.is_empty() {
            if !has_real_signature {
                return None;
            }
            parts.push(object([
                ("text", "".into()),
                ("thoughtSignature", signature.into()),
            ]));
            return Some(gemini_content("model", parts));
        }
        if !thought.is_empty() {
            let mut part = object([("text", thought.into()), ("thought", true.into())]);
            if visible.is_empty() && has_real_signature {
                part["thoughtSignature"] = signature.into();
            }
            parts.push(part);
        }
        if !visible.is_empty() {
            let mut part = text_part(visible);
            if has_real_signature {
                part["thoughtSignature"] = signature.into();
            }
            parts.push(part);
        }
        return Some(gemini_content("model", parts));
    }
    let mut part = object([("text", thought.into()), ("thought", true.into())]);
    if has_real_signature {
        part["thoughtSignature"] = signature.into();
    }
    parts.push(part);
    Some(gemini_content("model", parts))
}

/// `applyOpenAIResponsesTextFormatToGemini`: `text.format` as Gemini's JSON
/// output settings.
fn apply_text_format(out: &mut Value, root: &Value) {
    let Some(format) = path(root, "text.format") else {
        return;
    };
    match go::to_lower(str_of(format.get("type")).trim()).as_str() {
        "json_object" => {
            set_path(
                out,
                "generationConfig.responseMimeType",
                "application/json".into(),
            );
        }
        "json_schema" => {
            set_path(
                out,
                "generationConfig.responseMimeType",
                "application/json".into(),
            );
            if let Some(schema) = format
                .get("schema")
                .or_else(|| path(format, "json_schema.schema"))
            {
                set_path(out, "generationConfig.responseJsonSchema", schema.clone());
            }
        }
        _ => {}
    }
}

/// `filepath.Base(u.Path)` of `raw` parsed with Go's `url.Parse`, or `None`
/// if it doesn't parse. `raw` starts with `http://`, `https://` or `gs://`,
/// in any case, and is trimmed.
fn url_path_base(raw: &str) -> Option<String> {
    let (url, fragment) = raw.split_once('#').unwrap_or((raw, ""));
    if url.bytes().any(|b| b < b' ' || b == 0x7f) {
        return None;
    }
    let (scheme, rest) = url.split_once(':')?;
    let scheme = go::to_lower(scheme);
    let rest = if rest.ends_with('?') && rest.matches('?').count() == 1 {
        &rest[..rest.len() - 1]
    } else {
        rest.split_once('?').map_or(rest, |(rest, _)| rest)
    };
    let after = rest.strip_prefix("//")?;
    let (authority, escaped_path) = match after.find('/') {
        Some(slash) => after.split_at(slash),
        None => (after, ""),
    };
    parse_authority(&scheme, authority)?;
    let url_path = unescape(escaped_path, Escape::Other)?;
    if !fragment.is_empty() {
        unescape(fragment, Escape::Other)?;
    }
    Some(path_base(&String::from_utf8_lossy(&url_path)))
}

/// What part of a URL [`unescape`] reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    Host,
    Zone,
    Other,
}

/// Go's `url.unescape`: `text` with each `%XX` decoded, or `None` if an
/// escape is malformed, or a host holds what it can't.
fn unescape(text: &str, mode: Escape) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return None;
            }
            let high = hex_value(bytes[i + 1]);
            let value = (high << 4) | hex_value(bytes[i + 2]);
            let is_percent = &bytes[i..i + 3] == b"%25";
            // A host can escape only non-ASCII bytes, or a percent sign.
            if mode == Escape::Host && high < 8 && !is_percent {
                return None;
            }
            if mode == Escape::Zone && !is_percent && value != b' ' && host_must_escape(value) {
                return None;
            }
            out.push(value);
            i += 3;
        } else {
            if mode != Escape::Other && bytes[i] < 0x80 && host_must_escape(bytes[i]) {
                return None;
            }
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

fn hex_value(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        _ => digit - b'A' + 10,
    }
}

/// Go's `shouldEscape(c, encodeHost)`.
fn host_must_escape(c: u8) -> bool {
    !(c.is_ascii_alphanumeric() || b"!$&'()*+,;=:[]<>\"-_.~".contains(&c))
}

/// Go's `parseAuthority`: `Some` if the authority parses.
fn parse_authority(scheme: &str, authority: &str) -> Option<()> {
    let (userinfo, host) = match authority.rfind('@') {
        Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
        None => (None, authority),
    };
    parse_host(scheme, host)?;
    if let Some(userinfo) = userinfo {
        let valid = userinfo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._:~!$&'()*+,;=%@".contains(c));
        if !valid {
            return None;
        }
        unescape(userinfo, Escape::Other)?;
    }
    Some(())
}

/// Go's `parseHost`: `Some` if `host[:port]` parses.
fn parse_host(scheme: &str, host: &str) -> Option<()> {
    if let Some(open) = host.rfind('[') {
        if open > 0 {
            return None;
        }
        let close = host.rfind(']')?;
        if !valid_optional_port(&host[close + 1..]) {
            return None;
        }
        let hostname = &host[1..close];
        let unescaped = match hostname.find("%25") {
            Some(zone) => {
                let mut address = unescape(&hostname[..zone], Escape::Host)?;
                address.extend(unescape(&hostname[zone..], Escape::Zone)?);
                address
            }
            None => unescape(hostname, Escape::Host)?,
        };
        let text = String::from_utf8(unescaped).ok()?;
        let (address, zone) = text
            .split_once('%')
            .map_or((text.as_str(), None), |(a, z)| (a, Some(z)));
        if zone == Some("") || address.parse::<Ipv6Addr>().is_err() {
            return None;
        }
        return Some(());
    }
    if let Some(first) = host.find(':') {
        let last = host.rfind(':').unwrap_or(first);
        // Go 1.26 allows only one colon in an http or https host.
        let colon = if last != first && scheme != "http" && scheme != "https" {
            last
        } else {
            first
        };
        if !valid_optional_port(&host[colon..]) {
            return None;
        }
    }
    unescape(host, Escape::Host).map(|_| ())
}

/// Go's `validOptionalPort`: empty, or a colon and digits.
fn valid_optional_port(port: &str) -> bool {
    port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Go's `filepath.Base` on Unix.
fn path_base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    trimmed.rsplit('/').next().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests;
