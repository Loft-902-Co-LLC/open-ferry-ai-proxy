// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/gemini_openai-responses_web_search_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests web search for a Responses client on Gemini: the `googleSearch`
//! gate in the request translator, grounding metadata merging and citation
//! offsets, and the `web_search_call` items, `url_citation` annotations and
//! event order of the stream and whole-response translators.
//!
//! Upstream's `registerTestWebSearchModel` registers a made-up model in the
//! global registry; here a model's web search support comes only from the
//! embedded catalog. A model registered as searching is replaced by the
//! catalog's `gemini-3.7-flash-high` (antigravity section,
//! `native_capabilities.web_search: true` and `supports_web_search: true`),
//! in the model argument and in the request's `model`. The model registered
//! as not searching is replaced by the catalog's `gpt-oss-120b-medium`
//! (antigravity section, no web search fields).
//!
//! Dropped or changed tests:
//! - convert_openai_responses_request_to_gemini_web_search_capability_gate,
//!   convert_openai_responses_request_to_gemini_web_search_allowed_domains,
//!   convert_openai_responses_request_to_gemini_web_search_tool_choice_none_suppresses,
//!   convert_openai_responses_request_to_gemini_web_search_only_tool_choice_required_no_functions,
//!   convert_openai_responses_request_to_gemini_web_search_preview20250311 and every stream
//!   test that calls `registerTestWebSearchModel`: changed, the catalog models above stand in
//!   for the registered ones, since models can't be registered here.
//! - convert_gemini_response_to_openai_responses_stream_signature_boundary_continuation_text_chunks:
//!   changed, the registration is dropped and the upstream model ID kept; its request declares
//!   no tools, so whether the model searches changes nothing.
//! - convert_gemini_response_to_openai_responses_stream_model_alias_uses_effective_request:
//!   changed, the registration is dropped; `gemini-3.7-flash-high`, the model upstream
//!   registers, is in the catalog with web search. It also sends a trailing `[DONE]`, as
//!   upstream's copy of this test does since v8.0.11: the stream translator finishes only on
//!   a chunk with usage or `[DONE]`, and this test's last chunk has no usage.
//! - model_supports_web_search_static_veto_takes_precedence: changed. Upstream registers a
//!   model whose dynamic flag says it searches and whose static capability says it doesn't,
//!   and expects the static `false` to win. Models can't be registered here and the catalog
//!   has no model with `native_capabilities.web_search: false`, so the veto itself can't be
//!   built. The test keeps upstream's assertion (its made-up model doesn't search) and checks
//!   instead that every catalog model with an explicit native capability gets exactly that
//!   answer, whatever `supports_web_search` says.
//! - Each `t.Run` subtest is a block in its upstream test, so the first failing subtest stops
//!   the ones after it.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::super::test_support::convert_openai_responses_request_to_gemini;
use super::super::test_support::{GEMINI_SIGNATURE, events_by_type, sse_events};
use super::super::{
    GeminiToOpenAIResponsesStream, convert_gemini_response_to_openai_responses_non_stream,
};
use super::*;
use crate::json::bool_of;

/// A catalog model with web search, for upstream's models registered with
/// `registerTestWebSearchModel(..., true)`.
const SEARCH_MODEL: &str = "gemini-3.7-flash-high";

/// A catalog model without web search, for upstream's model registered with
/// `registerTestWebSearchModel(..., false)`.
const NO_SEARCH_MODEL: &str = "gpt-oss-120b-medium";

type Events = Vec<(String, Value)>;

/// Upstream's `ConvertGeminiResponseToOpenAIResponses`, called once for each
/// chunk with one `param`: the SSE text of each call. Upstream's tests don't
/// call `FinalizeToolInput`, so neither does this.
fn translate_chunks(
    model: &str,
    original: &Value,
    request: &Value,
    chunks: &[&str],
) -> Vec<String> {
    let mut stream = GeminiToOpenAIResponsesStream::new(model, original, request);
    chunks
        .iter()
        .map(|chunk| stream.translate_line(chunk.as_bytes()))
        .collect()
}

/// [`translate_chunks`] with each call's events.
fn stream_with(model: &str, original: &Value, request: &Value, chunks: &[&str]) -> Vec<Events> {
    translate_chunks(model, original, request, chunks)
        .iter()
        .map(|out| sse_events(out))
        .collect()
}

/// [`stream_with`] with the same request as original and translated request,
/// as upstream's `req, req`.
fn stream(model: &str, request: &Value, chunks: &[&str]) -> Vec<Events> {
    stream_with(model, request, request, chunks)
}

/// The event names, in order.
fn names(events: &Events) -> Vec<&str> {
    events.iter().map(|(name, _)| name.as_str()).collect()
}

/// The data of the last `response.completed` event, or `Null`, as upstream's
/// loops that keep the last one they see.
fn completed(events: &Events) -> Value {
    events
        .iter()
        .rev()
        .find(|(name, _)| name == "response.completed")
        .map_or(Value::Null, |(_, data)| data.clone())
}

/// `byType[name]`: the data of each event called `name`.
fn by<'m>(by_type: &'m HashMap<String, Vec<Value>>, name: &str) -> &'m [Value] {
    by_type.get(name).map_or(&[], Vec::as_slice)
}

/// gjson `Get(path).Array()`.
fn list<'v>(value: &'v Value, path: &str) -> &'v [Value] {
    array_of(at(value, path))
}

/// gjson `Get(path).String()`.
fn text(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// gjson `String()`.
fn string(value: &Value) -> String {
    str_of(Some(value)).into_owned()
}

/// gjson `Get(path).Int()`.
fn int(value: &Value, path: &str) -> i64 {
    at(value, path).map_or(0, int_of)
}

/// An annotation's `start_index` and `end_index`.
fn span(annotation: &Value) -> (i64, i64) {
    (int(annotation, "start_index"), int(annotation, "end_index"))
}

/// The items of `response.output` in a `response.completed` event.
fn output(completed: &Value) -> &[Value] {
    list(completed, "response.output")
}

/// The items of type `kind`, in order.
fn of_type<'v>(items: &'v [Value], kind: &str) -> Vec<&'v Value> {
    items
        .iter()
        .filter(|item| text(item, "type") == kind)
        .collect()
}

/// gjson `#(type=kind)`: the first item of type `kind`.
fn first_of_type<'v>(items: &'v [Value], kind: &str) -> Option<&'v Value> {
    items.iter().find(|item| text(item, "type") == kind)
}

/// The last item of type `kind`, as upstream's loops that keep the last one.
fn last_of_type<'v>(items: &'v [Value], kind: &str) -> Option<&'v Value> {
    items.iter().rev().find(|item| text(item, "type") == kind)
}

/// `findWebSearchCallDone`.
fn find_web_search_call_done(payloads: &[Value]) -> Option<&Value> {
    payloads
        .iter()
        .find(|payload| text(payload, "item.type") == "web_search_call")
}

/// `findMessageOutputItemDone`.
fn find_message_output_item_done(payloads: &[Value]) -> Option<&Value> {
    payloads
        .iter()
        .find(|payload| text(payload, "item.type") == "message")
}

/// Whether a translated request has a `googleSearch` tool.
fn has_google_search(out: &Value) -> bool {
    list(out, "tools")
        .iter()
        .any(|tool| tool.get("googleSearch").is_some())
}

/// The whole-response translator with no requests, as upstream's
/// `ConvertGeminiResponseToOpenAIResponsesNonStream(ctx, model, nil, nil, raw, nil)`.
fn non_stream(raw: &str) -> Value {
    convert_gemini_response_to_openai_responses_non_stream(
        &Value::Null,
        &Value::Null,
        raw.as_bytes(),
    )
    .expect("a response")
}

#[test]
fn convert_openai_responses_request_to_gemini_web_search_capability_gate() {
    let req = json!({
        "model": "dummy",
        "input": "test query",
        "tools": [{"type": "web_search"}]
    });

    // Capable model should receive googleSearch tool
    let capable_out = convert_openai_responses_request_to_gemini(SEARCH_MODEL, &req, false);
    assert!(
        has_google_search(&capable_out),
        "expected googleSearch tool for capable model, got: {capable_out}"
    );

    // Incapable model must NOT receive googleSearch tool
    let incapable_out = convert_openai_responses_request_to_gemini(NO_SEARCH_MODEL, &req, false);
    assert!(
        !has_google_search(&incapable_out),
        "incapable model should not receive googleSearch tool, got: {incapable_out}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_web_search_allowed_domains() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "search query",
        "tools": [{
            "type": "web_search",
            "filters": {
                "allowed_domains": ["go.dev", "github.com"]
            }
        }]
    });

    let out = convert_openai_responses_request_to_gemini(SEARCH_MODEL, &req, false);
    let domains = list(&out, "tools.0.googleSearch.includedDomains");
    assert_eq!(
        domains.len(),
        2,
        "expected 2 includedDomains, got {}: {out}",
        domains.len()
    );
    assert!(
        string(&domains[0]) == "go.dev" && string(&domains[1]) == "github.com",
        "unexpected domains: {domains:?}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_web_search_tool_choice_none_suppresses() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "search query",
        "tools": [{"type": "web_search"}],
        "tool_choice": "none"
    });

    let out = convert_openai_responses_request_to_gemini(SEARCH_MODEL, &req, false);
    assert!(
        !has_google_search(&out),
        "tool_choice: none should suppress googleSearch, got: {out}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_stream_grounding_metadata() {
    let gemini_resp = r#"{
        "responseId": "resp_test123",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{
                    "text": "Go 1.27 is the latest release."
                }]
            },
            "groundingMetadata": {
                "webSearchQueries": ["latest Go release"],
                "groundingChunks": [
                    {"web": {"uri": "https://go.dev/dl/", "title": "Download Go"}}
                ],
                "groundingSupports": [{
                    "groundingChunkIndices": [0],
                    "segment": {
                        "startIndex": 0,
                        "endIndex": 7,
                        "text": "Go 1.27"
                    }
                }]
            }
        }],
        "usageMetadata": {
            "promptTokenCount": 15,
            "candidatesTokenCount": 8,
            "totalTokenCount": 23
        }
    }"#;

    let parsed = non_stream(gemini_resp);

    // Check output array has 2 items: web_search_call and message
    let outputs = list(&parsed, "output");
    assert_eq!(
        outputs.len(),
        2,
        "expected 2 output items, got {}: {parsed}",
        outputs.len()
    );

    // First item: web_search_call
    let ws_call = &outputs[0];
    assert_eq!(
        text(ws_call, "type"),
        "web_search_call",
        "expected first output item type web_search_call"
    );
    assert_eq!(
        text(ws_call, "action.type"),
        "search",
        "expected action.type search"
    );
    assert_eq!(
        text(ws_call, "action.query"),
        "latest Go release",
        "expected action.query 'latest Go release'"
    );
    let sources = list(ws_call, "action.sources");
    assert!(
        sources.len() == 1 && text(&sources[0], "url") == "https://go.dev/dl/",
        "unexpected sources: {:?}",
        at(ws_call, "action.sources")
    );

    // Second item: message with url_citation annotation
    let msg = &outputs[1];
    assert_eq!(
        text(msg, "type"),
        "message",
        "expected second output item type message"
    );
    let citations = list(msg, "content.0.annotations");
    assert_eq!(
        citations.len(),
        1,
        "expected 1 citation, got {}: {msg}",
        citations.len()
    );
    let citation = &citations[0];
    assert_eq!(
        text(citation, "type"),
        "url_citation",
        "expected citation type url_citation"
    );
    assert_eq!(
        text(citation, "url"),
        "https://go.dev/dl/",
        "expected citation url 'https://go.dev/dl/'"
    );
    assert_eq!(
        text(citation, "title"),
        "Download Go",
        "expected citation title 'Download Go'"
    );
    assert_eq!(
        span(citation),
        (0, 7),
        "expected start_index=0, end_index=7"
    );

    // Tool usage check
    assert_eq!(
        int(&parsed, "tool_usage.web_search.num_requests"),
        1,
        "expected tool_usage.web_search.num_requests = 1"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_web_search() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "search query",
        "tools": [{"type": "web_search"}]
    });

    let chunk1 = r#"data: {
        "responseId": "stream_resp_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Found information."}]
            },
            "groundingMetadata": {
                "webSearchQueries": ["search query"],
                "groundingChunks": [{"web": {"uri": "https://example.com", "title": "Example"}}],
                "groundingSupports": [{
                    "groundingChunkIndices": [0],
                    "segment": {"startIndex": 0, "endIndex": 5, "text": "Found"}
                }]
            }
        }]
    }"#;

    let chunk2 = r#"data: {
        "candidates": [{"finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2]).concat();
    let event_types = names(&all_events);

    // Verify event progression
    let expected_events = [
        "response.created",
        "response.in_progress",
        "response.output_item.added", // web_search_call
        "response.web_search_call.searching",
        "response.web_search_call.completed",
        "response.output_item.done",  // web_search_call done
        "response.output_item.added", // message
        "response.content_part.added",
        "response.output_text.delta",
        "response.output_text.done",
        "response.content_part.done",
        "response.output_item.done", // message done
        "response.completed",
    ];

    for expected in expected_events {
        assert!(
            event_types.contains(&expected),
            "missing expected event {expected:?} in event sequence: {event_types:?}\nFull events:\n{all_events:?}"
        );
    }
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_no_grounding_does_not_emit_web_search_call() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "Calculate 2+2",
        "tools": [{"type": "web_search"}]
    });

    let chunk1 = r#"data: {
        "responseId": "stream_noground_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "2+2=4"}]
            }
        }]
    }"#;

    let chunk2 = r#"data: {
        "candidates": [{"finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 5, "totalTokenCount": 10}
    }"#;

    for ev_str in translate_chunks(SEARCH_MODEL, &req, &req, &[chunk1, chunk2]) {
        assert!(
            !ev_str.contains("web_search_call"),
            "did not expect web_search_call when no grounding occurred, got events: {ev_str}"
        );
        assert!(
            !ev_str.contains(r#""tool_usage""#),
            "did not expect tool_usage when no grounding occurred, got events: {ev_str}"
        );
    }
}

#[test]
fn build_responses_url_citations_rune_offset_conversion() {
    let full_text = "Go语言的最新版本是Go 1.27。";
    // "Go语言的最新版本是" has 10 runes, 26 UTF-8 bytes.
    // "Go 1.27" has 7 runes, 7 UTF-8 bytes.
    // Start byte = 26 (rune 10). End byte = 26 + 7 = 33 (rune 17).
    let gm = json!({
        "groundingChunks": [{"web": {"uri": "https://go.dev", "title": "Go"}}],
        "groundingSupports": [
            {
                "groundingChunkIndices": [0],
                "segment": {"startIndex": 26, "endIndex": 33}
            },
            {
                "groundingChunkIndices": [0],
                "segment": {"startIndex": 33, "endIndex": 20}
            }
        ]
    });

    let citations = build_url_citations(Some(&gm), Some(full_text));
    assert_eq!(
        citations.len(),
        1,
        "expected exactly 1 citation (inverted one dropped), got {}",
        citations.len()
    );

    let cite = &citations[0];
    assert_eq!(int(cite, "start_index"), 10, "start_index, want 10 runes");
    assert_eq!(int(cite, "end_index"), 17, "end_index, want 17 runes");
}

#[test]
fn convert_openai_responses_request_to_gemini_web_search_only_tool_choice_required_no_functions() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "search query",
        "tools": [{"type": "web_search"}],
        "tool_choice": "required"
    });

    let parsed = convert_openai_responses_request_to_gemini(SEARCH_MODEL, &req, false);

    // googleSearch should be present
    assert!(
        at(&parsed, "tools.0.googleSearch").is_some(),
        "expected googleSearch in tools, got: {parsed}"
    );
    // functionCallingConfig should NOT be present since functionDeclarations are empty
    assert!(
        at(&parsed, "toolConfig.functionCallingConfig").is_none(),
        "functionCallingConfig should not be set when no function declarations exist, got: {parsed}"
    );
}

#[test]
fn has_valid_web_grounding() {
    // The test has the function's name, so the function is called through
    // `super`.
    use super::has_valid_web_grounding as is_valid;

    let empty = json!({});
    assert!(
        !is_valid(Some(&empty)),
        "empty metadata should not be valid web grounding"
    );

    let no_web = json!({"groundingChunks": [{"other": "something"}]});
    assert!(
        !is_valid(Some(&no_web)),
        "metadata without web uri should not be valid web grounding"
    );

    let supports_only = json!({"groundingSupports": [{"groundingChunkIndices": [0]}]});
    assert!(
        !is_valid(Some(&supports_only)),
        "metadata with only groundingSupports should not be valid web grounding"
    );

    let rag_retrieval = json!({
        "groundingChunks": [{"retrievedContext": {"uri": "rag-doc-1", "title": "Internal Doc"}}],
        "groundingSupports": [{"groundingChunkIndices": [0], "segment": {"startIndex": 0, "endIndex": 10}}]
    });
    assert!(
        !is_valid(Some(&rag_retrieval)),
        "metadata with only retrievedContext chunks and supports should not be valid web grounding"
    );

    let empty_web_uri = json!({
        "groundingChunks": [{"web": {"uri": ""}}],
        "groundingSupports": [{"groundingChunkIndices": [0]}]
    });
    assert!(
        !is_valid(Some(&empty_web_uri)),
        "metadata with empty web uri should not be valid web grounding"
    );

    let with_query = json!({"webSearchQueries": ["query"]});
    assert!(
        is_valid(Some(&with_query)),
        "metadata with webSearchQueries should be valid web grounding"
    );

    let with_chunk = json!({"groundingChunks": [{"web": {"uri": "https://example.com"}}]});
    assert!(
        is_valid(Some(&with_chunk)),
        "metadata with web chunk should be valid web grounding"
    );

    let rag_with_web_query = json!({
        "groundingChunks": [{"retrievedContext": {"uri": "rag-doc-1"}}],
        "webSearchQueries": ["actual query"]
    });
    assert!(
        is_valid(Some(&rag_with_web_query)),
        "metadata with retrievedContext and valid webSearchQueries should be valid web grounding"
    );
}

#[test]
fn model_supports_web_search_static_veto_takes_precedence() {
    // Upstream's model, which it registers and vetoes, isn't in the catalog.
    let model_id = "gemini-veto-test-model";
    assert!(
        !model_supports_web_search(model_id),
        "expected ModelSupportsWebSearch to be false due to explicit veto, got true"
    );

    // An explicit native capability decides, whatever supports_web_search
    // says.
    for info in ModelCatalog::embedded().models() {
        let want = info.native_web_search.unwrap_or(info.supports_web_search);
        assert_eq!(
            model_supports_web_search(&info.id),
            want,
            "model {}: native_capabilities.web_search {:?}, supports_web_search {}",
            info.id,
            info.native_web_search,
            info.supports_web_search
        );
    }
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_late_grounding_metadata_and_cjk_offsets() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "Go最新版本是多少？",
        "tools": [{"type": "web_search"}]
    });

    // Chunk 1: Chinese text only, NO groundingMetadata
    let chunk1 = r#"data: {
        "responseId": "stream_late_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Go语言的最新版本是"}]
            }
        }]
    }"#;

    // Chunk 2: Remaining text + groundingMetadata + finishReason STOP
    // "Go语言的最新版本是" has 10 runes and 26 bytes.
    // "Go 1.27" has 7 runes (bytes: 26 to 33).
    // Total: "Go语言的最新版本是Go 1.27。" = 18 runes, 36 bytes.
    // Segment at byte [26, 33) points to "Go 1.27" -> runes [10, 17)
    let chunk2 = r#"data: {
        "responseId": "stream_late_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Go 1.27。"}]
            },
            "groundingMetadata": {
                "webSearchQueries": ["Go release"],
                "groundingChunks": [
                    {"web": {"uri": "https://go.dev/doc/devel/release", "title": "Go Releases"}}
                ],
                "groundingSupports": [
                    {
                        "groundingChunkIndices": [0],
                        "segment": {"startIndex": 26, "endIndex": 33}
                    }
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 20, "totalTokenCount": 30}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2]).concat();

    let mut event_types = Vec::new();
    let mut completed_json = Value::Null;
    let mut part_done_json = None;
    let mut item_done_json = Value::Null;
    for (current_type, parsed) in &all_events {
        event_types.push(current_type.as_str());
        if current_type == "response.completed" {
            completed_json = parsed.clone();
        }
        if current_type == "response.content_part.done"
            && at(parsed, "part.annotations.0").is_some()
        {
            part_done_json = Some(parsed.clone());
        }
        if current_type == "response.output_item.done" && text(parsed, "item.type") == "message" {
            item_done_json = parsed.clone();
        }
    }

    // 1. Verify event types sequence
    // web_search_call events must precede message deltas and message done
    let mut ws_added_idx = None;
    let mut ws_done_idx = None;
    let mut msg_added_idx = None;
    let mut msg_done_idx = None;
    let mut completed_idx = None;
    for (i, typ) in event_types.iter().enumerate() {
        match *typ {
            "response.output_item.added" => {
                if ws_added_idx.is_none() {
                    ws_added_idx = Some(i);
                } else if msg_added_idx.is_none() {
                    msg_added_idx = Some(i);
                }
            }
            "response.output_item.done" => {
                if ws_done_idx.is_none() {
                    ws_done_idx = Some(i);
                } else if msg_done_idx.is_none() {
                    msg_done_idx = Some(i);
                }
            }
            "response.completed" => completed_idx = Some(i),
            _ => {}
        }
    }

    let (Some(_), Some(ws_done_idx), Some(msg_added_idx), Some(msg_done_idx)) =
        (ws_added_idx, ws_done_idx, msg_added_idx, msg_done_idx)
    else {
        panic!(
            "expected both web_search_call and message output items, got events: {event_types:?}"
        );
    };

    // web_search_call should be added and completed BEFORE message is added
    assert!(
        ws_done_idx < msg_added_idx,
        "expected web_search_call to complete (idx={ws_done_idx}) before message added (idx={msg_added_idx}), events={event_types:?}"
    );
    assert!(
        completed_idx.is_some_and(|completed_idx| msg_done_idx < completed_idx),
        "expected message done (idx={msg_done_idx}) before completed (idx={completed_idx:?})"
    );

    // 2. Verify Unicode rune offset conversion on the citations
    let Some(part_done_json) = part_done_json else {
        panic!("expected content_part.done with annotations, got none. Events: {event_types:?}");
    };
    assert_eq!(
        int(&part_done_json, "part.annotations.0.start_index"),
        10,
        "annotation start_index, want 10"
    );
    assert_eq!(
        int(&part_done_json, "part.annotations.0.end_index"),
        17,
        "annotation end_index, want 17"
    );

    // 3. Verify message output_item.done also has the exact rune offsets
    assert_eq!(
        int(&item_done_json, "item.content.0.annotations.0.start_index"),
        10,
        "item.content annotations start_index != 10"
    );

    // 4. Verify response.completed.output has [web_search_call, message] in order
    let outputs = output(&completed_json);
    assert!(
        outputs.len() >= 2,
        "expected at least 2 output items in completed, got {}",
        outputs.len()
    );
    assert_eq!(
        text(&outputs[0], "type"),
        "web_search_call",
        "output[0].type"
    );
    assert_eq!(text(&outputs[1], "type"), "message", "output[1].type");

    // 5. Verify ID prefix stripping on web_search_call
    let ws_id = text(&outputs[0], "id");
    assert!(
        ws_id.starts_with("ws_") && !ws_id.starts_with("ws_resp_"),
        "expected web_search_call id format ws_<id> without resp_ prefix, got {ws_id:?}"
    );

    // 6. Verify tool_usage
    assert_eq!(
        int(
            &completed_json,
            "response.tool_usage.web_search.num_requests"
        ),
        1,
        "tool_usage num_requests, want 1"
    );
}

#[test]
fn allows_responses_web_search_tool_choice_allowed_tools() {
    // 1. allowed_tools containing web_search should permit search
    let with_search = json!({
        "tool_choice": {
            "type": "allowed_tools",
            "mode": "auto",
            "tools": [{"type": "function", "name": "lookup"}, {"type": "web_search"}]
        }
    });
    assert!(
        allows_web_search_tool_choice(&with_search),
        "expected allowed_tools containing web_search to allow search"
    );

    // 2. allowed_tools without web_search should NOT permit search
    let without_search = json!({
        "tool_choice": {
            "type": "allowed_tools",
            "mode": "auto",
            "tools": [{"type": "function", "name": "lookup"}]
        }
    });
    assert!(
        !allows_web_search_tool_choice(&without_search),
        "expected allowed_tools without web_search to disallow search"
    );

    // 3. allowed_tools containing the versioned preview alias should permit search
    let with_preview = json!({
        "tool_choice": {
            "type": "allowed_tools",
            "mode": "auto",
            "tools": [{"type": "web_search_preview_2025_03_11"}]
        }
    });
    assert!(
        allows_web_search_tool_choice(&with_preview),
        "expected allowed_tools containing web_search_preview_2025_03_11 to allow search"
    );
}

#[test]
fn responses_web_search_tool_type_aliases() {
    let aliases = [
        "web_search",
        "web_search_2025_08_26",
        "web_search_preview",
        "web_search_preview_2025_03_11",
    ];
    for tool_type in aliases {
        let decl = json!({"tools": [{"type": tool_type}]});
        assert!(
            has_web_search_tool(&decl),
            "HasResponsesWebSearchTool({tool_type:?}) = false, want true"
        );
        let choice = json!({"tool_choice": {"type": tool_type}});
        assert!(
            allows_web_search_tool_choice(&choice),
            "AllowsResponsesWebSearchToolChoice({tool_type:?}) = false, want true"
        );
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_web_search_preview20250311() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "search query",
        "tools": [{"type": "web_search_preview_2025_03_11"}],
        "tool_choice": {"type": "web_search_preview_2025_03_11"}
    });

    let out = convert_openai_responses_request_to_gemini(SEARCH_MODEL, &req, false);
    assert!(
        at(&out, "tools.0.googleSearch").is_some(),
        "expected googleSearch tool for web_search_preview_2025_03_11 declaration and tool_choice, got: {out}"
    );
}

#[test]
fn extract_responses_web_search_query_multiple_text_parts() {
    // 1. Array of input_text parts
    let flat_input = json!({
        "input": [
            {"type": "input_text", "text": "Who is"},
            {"type": "input_text", "text": "the current Go release lead?"}
        ]
    });
    let got = extract_web_search_query(&flat_input);
    let want = "Who is\nthe current Go release lead?";
    assert_eq!(got, want, "flat input query");

    // 2. Message with multiple content parts
    let nested_input = json!({
        "input": [
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Part A"},
                    {"type": "input_text", "text": "Part B"}
                ]
            }
        ]
    });
    let got_nested = extract_web_search_query(&nested_input);
    let want_nested = "Part A\nPart B";
    assert_eq!(got_nested, want_nested, "nested content query");
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_model_alias_uses_effective_request() {
    // Upstream model is capable of web search; it is in the catalog with it.
    let resolved_model = "gemini-3.7-flash-high";

    // Original request uses a custom unregistered client alias
    let client_alias = "my-unregistered-alias";
    let original_req = json!({
        "model": client_alias,
        "input": "Search latest news",
        "tools": [{"type": "web_search"}]
    });

    // Effective translated upstream request has googleSearch
    let effective_req = json!({
        "model": resolved_model,
        "contents": [{"role": "user", "parts": [{"text": "Search latest news"}]}],
        "tools": [{"googleSearch": {}}]
    });

    let chunk1 = r#"data: {
        "responseId": "stream_alias_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Breaking news:"}]
            }
        }]
    }"#;

    let chunk2 = r#"data: {
        "responseId": "stream_alias_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": " Go 1.27 released."}]
            },
            "groundingMetadata": {
                "webSearchQueries": ["latest news"],
                "groundingChunks": [{"web": {"uri": "https://example.com", "title": "News"}}]
            },
            "finishReason": "STOP"
        }]
    }"#;

    let all_events = stream_with(
        resolved_model,
        &original_req,
        &effective_req,
        &[chunk1, chunk2, "[DONE]"],
    )
    .concat();
    let completed_json = completed(&all_events);

    let outputs = output(&completed_json);
    assert!(
        outputs.len() >= 2,
        "expected 2 output items in completed, got {}",
        outputs.len()
    );
    // web_search_call MUST precede message even when an alias was used
    assert_eq!(
        text(&outputs[0], "type"),
        "web_search_call",
        "output[0].type"
    );
    assert_eq!(text(&outputs[1], "type"), "message", "output[1].type");
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_interleaved_text_and_function_call_preserves_trailing_text()
 {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "search and calc",
        "tools": [{"type": "web_search"}, {"type": "function", "function": {"name": "calculate"}}]
    });

    // Upstream returns text A -> functionCall -> text B -> STOP without grounding metadata
    let chunk1 = r#"data: {
        "responseId": "stream_interleaved_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Text A"}]
            }
        }]
    }"#;

    let chunk2 = r#"data: {
        "responseId": "stream_interleaved_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "calculate", "args": {"expr": "1+1"}}}]
            }
        }]
    }"#;

    let chunk3 = r#"data: {
        "responseId": "stream_interleaved_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Text B"}]
            }
        }]
    }"#;

    let chunk4 = r#"data: {
        "responseId": "stream_interleaved_1",
        "candidates": [{
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3, chunk4]).concat();

    let text_b_deltas: Vec<String> = all_events
        .iter()
        .filter(|(name, _)| name == "response.output_text.delta")
        .map(|(_, data)| text(data, "delta"))
        .filter(|delta| delta.contains("Text B"))
        .collect();
    let completed_json = completed(&all_events);

    assert!(
        !text_b_deltas.is_empty(),
        "expected output_text.delta event containing 'Text B', but got none"
    );

    let outputs = output(&completed_json);
    assert_eq!(
        outputs.len(),
        3,
        "expected 3 output items (message, function_call, message), got {}: {completed_json}",
        outputs.len()
    );

    assert!(
        text(&outputs[0], "type") == "message" && text(&outputs[0], "content.0.text") == "Text A",
        "unexpected outputs[0]: {}",
        outputs[0]
    );
    assert!(
        text(&outputs[1], "type") == "function_call" && text(&outputs[1], "name") == "calculate",
        "unexpected outputs[1]: {}",
        outputs[1]
    );
    assert!(
        text(&outputs[2], "type") == "message" && text(&outputs[2], "content.0.text") == "Text B",
        "unexpected outputs[2]: {}",
        outputs[2]
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_output_index_matches_completed_order_with_late_grounding()
 {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test query",
        "tools": [{"type": "web_search"}, {"type": "function", "function": {"name": "query_db"}}]
    });

    // Text A is flushed early because function call arrives
    let chunk1 = r#"data: {
        "responseId": "stream_late_order_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Searching database..."}]
            }
        }]
    }"#;

    let chunk2 = r#"data: {
        "responseId": "stream_late_order_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "query_db", "args": {}}}]
            }
        }]
    }"#;

    let chunk3 = r#"data: {
        "responseId": "stream_late_order_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Results found."}]
            }
        }]
    }"#;

    // Late grounding metadata arrives with web_search queries
    let chunk4 = r#"data: {
        "responseId": "stream_late_order_1",
        "candidates": [{
            "groundingMetadata": {
                "webSearchQueries": ["test query"],
                "groundingChunks": [{"web": {"uri": "https://example.com/db", "title": "DB"}}]
            }
        }]
    }"#;

    let chunk5 = r#"data: {
        "responseId": "stream_late_order_1",
        "candidates": [{
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let all_events = stream(
        SEARCH_MODEL,
        &req,
        &[chunk1, chunk2, chunk3, chunk4, chunk5],
    )
    .concat();

    // (output_index, item type, item id) of each output_item.added.
    let added_items: Vec<(i64, String, String)> = all_events
        .iter()
        .filter(|(name, _)| name == "response.output_item.added")
        .map(|(_, data)| {
            (
                int(data, "output_index"),
                text(data, "item.type"),
                text(data, "item.id"),
            )
        })
        .collect();
    let completed_json = completed(&all_events);

    let outputs = output(&completed_json);
    assert_eq!(
        outputs.len(),
        added_items.len(),
        "mismatch: emitted {} added items, but response.completed has {} outputs",
        added_items.len(),
        outputs.len()
    );

    for (i, ((output_index, item_type, item_id), out_item)) in
        added_items.iter().zip(outputs).enumerate()
    {
        let out_type = text(out_item, "type");
        let out_id = text(out_item, "id");
        assert_eq!(
            &out_type, item_type,
            "output[{i}] type mismatch: emitted {item_type:?} at index {output_index}, got {out_type:?} in completed"
        );
        assert_eq!(
            &out_id, item_id,
            "output[{i}] ID mismatch: emitted {item_id:?} at index {output_index}, got {out_id:?} in completed"
        );
    }
}

#[test]
fn convert_gemini_response_to_openai_responses_citations_respect_part_index_and_message() {
    // multi-part within single message
    {
        let gemini_resp = r#"{
            "responseId": "resp_multipart_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "Intro "},
                        {"text": "Fact"}
                    ]
                },
                "groundingMetadata": {
                    "webSearchQueries": ["fact query"],
                    "groundingChunks": [{"web": {"uri": "https://example.com/fact", "title": "Fact Source"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {
                            "partIndex": 1,
                            "startIndex": 0,
                            "endIndex": 4,
                            "text": "Fact"
                        }
                    }]
                }
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 5, "totalTokenCount": 10}
        }"#;

        let parsed = non_stream(gemini_resp);

        let Some(msg) = first_of_type(list(&parsed, "response.output"), "message")
            .or_else(|| first_of_type(list(&parsed, "output"), "message"))
        else {
            panic!("expected message output item, got: {parsed}");
        };

        let citations = list(msg, "content.0.annotations");
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}: {msg}",
            citations.len()
        );

        // In "Intro Fact": "Intro " is 6 runes [0, 6). "Fact" is 4 runes [6, 10).
        assert_eq!(
            span(&citations[0]),
            (6, 10),
            "expected start_index=6, end_index=10 referencing 'Fact'"
        );
    }

    // multi-message separated by function calls
    {
        let gemini_resp = r#"{
            "responseId": "resp_multimsg_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "First intro "},
                        {"functionCall": {"name": "search", "args": {}}},
                        {"text": "Second fact"}
                    ]
                },
                "groundingMetadata": {
                    "webSearchQueries": ["second query"],
                    "groundingChunks": [{"web": {"uri": "https://example.com/second", "title": "Second Source"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {
                            "partIndex": 2,
                            "startIndex": 7,
                            "endIndex": 11,
                            "text": "fact"
                        }
                    }]
                }
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let parsed = non_stream(gemini_resp);

        // outputs: [web_search_call, message_0, function_call, message_1]
        let messages = of_type(list(&parsed, "output"), "message");
        // Upstream keeps the first message and the last one after it.
        let [first_msg, .., second_msg] = messages[..] else {
            panic!("expected two messages, got: {parsed}");
        };

        // First message must NOT have citations meant for second message
        assert!(
            list(first_msg, "content.0.annotations").is_empty(),
            "first message should have 0 annotations, got: {:?}",
            at(first_msg, "content.0.annotations")
        );

        // Second message must have the citation for "fact" [7, 11)
        let second_citations = list(second_msg, "content.0.annotations");
        assert_eq!(
            second_citations.len(),
            1,
            "second message should have 1 annotation, got {}: {second_msg}",
            second_citations.len()
        );
        assert_eq!(
            span(&second_citations[0]),
            (7, 11),
            "expected second message start=7 end=11"
        );
    }

    // non-ASCII CJK text multi-part
    {
        let gemini_resp = r#"{
            "responseId": "resp_cjk_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "Go语言的最新版本是"},
                        {"text": "Go 1.27。"}
                    ]
                },
                "groundingMetadata": {
                    "webSearchQueries": ["Go最新版本"],
                    "groundingChunks": [{"web": {"uri": "https://go.dev", "title": "Go"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {
                            "partIndex": 1,
                            "startIndex": 0,
                            "endIndex": 7,
                            "text": "Go 1.27"
                        }
                    }]
                }
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let parsed = non_stream(gemini_resp);

        let msg = first_of_type(list(&parsed, "output"), "message").unwrap_or(&Value::Null);
        let citations = list(msg, "content.0.annotations");
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}: {msg}",
            citations.len()
        );
        // "Go语言的最新版本是" has 10 runes.
        // "Go 1.27" has 7 runes.
        // So in merged message "Go语言的最新版本是Go 1.27。", start=10, end=17
        assert_eq!(span(&citations[0]), (10, 17), "expected start=10 end=17");
    }

    // citation span across multiple messages
    {
        let gemini_resp = r#"{
            "responseId": "resp_multimsg_span_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "Hello ", "partIndex": 0},
                        {"functionCall": {"name": "search", "args": {}}, "partIndex": 1},
                        {"text": "world", "partIndex": 0}
                    ]
                },
                "groundingMetadata": {
                    "webSearchQueries": ["span query"],
                    "groundingChunks": [{"web": {"uri": "https://example.com/span", "title": "Span Source"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {
                            "partIndex": 0,
                            "startIndex": 0,
                            "endIndex": 11,
                            "text": "Hello world"
                        }
                    }]
                }
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let parsed = non_stream(gemini_resp);

        let messages = of_type(list(&parsed, "output"), "message");
        // Upstream keeps the first message and the last one after it.
        let [first_msg, .., second_msg] = messages[..] else {
            panic!("expected two messages, got: {parsed}");
        };

        let first_citations = list(first_msg, "content.0.annotations");
        assert_eq!(
            first_citations.len(),
            1,
            "expected first message to have 1 citation, got {}: {first_msg}",
            first_citations.len()
        );
        assert_eq!(
            span(&first_citations[0]),
            (0, 6),
            "expected first message citation [0, 6)"
        );
        assert_eq!(
            text(&first_citations[0], "url"),
            "https://example.com/span",
            "expected url https://example.com/span"
        );

        let second_citations = list(second_msg, "content.0.annotations");
        assert_eq!(
            second_citations.len(),
            1,
            "expected second message to have 1 citation, got {}: {second_msg}",
            second_citations.len()
        );
        assert_eq!(
            span(&second_citations[0]),
            (0, 5),
            "expected second message citation [0, 5)"
        );
        assert_eq!(
            text(&second_citations[0], "url"),
            "https://example.com/span",
            "expected url https://example.com/span"
        );
    }
}

/// One subtest of `TestBuildResponsesURLCitationsForMessages_MultiMessageSpan`:
/// a support over `[start, end)` of part 0, whose text is `first` in message
/// 0 then `second` in message 1, should give `want0` in message 0 and
/// `want1` in message 1.
fn check_multi_message_span(
    url: &str,
    title: &str,
    (start, end): (i64, i64),
    (first, second): (&str, &str),
    want0: (i64, i64),
    want1: (i64, i64),
) {
    let gm = json!({
        "groundingChunks": [
            {"web": {"uri": url, "title": title}}
        ],
        "groundingSupports": [
            {
                "segment": {"partIndex": 0, "startIndex": start, "endIndex": end},
                "groundingChunkIndices": [0]
            }
        ]
    });

    let mappings = [
        PartMapping {
            part_index: 0,
            message_index: 0,
            start_rune: 0,
            text: first.to_owned(),
        },
        PartMapping {
            part_index: 0,
            message_index: 1,
            start_rune: 0,
            text: second.to_owned(),
        },
    ];

    let res = build_url_citations_for_messages(
        Some(&gm),
        &mappings,
        &[first.to_owned(), second.to_owned()],
    )
    .unwrap_or_default();
    assert_eq!(
        res.len(),
        2,
        "expected citations in 2 messages, got {}",
        res.len()
    );

    let c0 = res.get(&0).map_or(&[][..], Vec::as_slice);
    assert_eq!(
        c0.len(),
        1,
        "expected 1 citation in message 0, got {}",
        c0.len()
    );
    assert_eq!(span(&c0[0]), want0, "in message 0, got: {}", c0[0]);

    let c1 = res.get(&1).map_or(&[][..], Vec::as_slice);
    assert_eq!(
        c1.len(),
        1,
        "expected 1 citation in message 1, got {}",
        c1.len()
    );
    assert_eq!(span(&c1[0]), want1, "in message 1, got: {}", c1[0]);
}

#[test]
fn build_responses_url_citations_for_messages_multi_message_span() {
    // citation span covering two messages
    check_multi_message_span(
        "https://example.com/span",
        "Span Title",
        (0, 11),
        ("Hello ", "world"),
        (0, 6),
        (0, 5),
    );

    // citation partial overlap across two messages
    check_multi_message_span(
        "https://example.com/span",
        "Span Title",
        (2, 9),
        ("Hello ", "world"),
        (2, 6),
        (0, 3),
    );

    // citation span with non-ASCII CJK runes across two messages
    check_multi_message_span(
        "https://example.com/cjk",
        "CJK Title",
        (0, 13),
        ("你好 ", "世界"),
        (0, 3),
        (0, 2),
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_disables_buffering_when_search_disabled() {
    // tool_choice none disables buffering
    {
        let req = json!({
            "model": SEARCH_MODEL,
            "input": "test query",
            "tools": [{"type": "web_search"}],
            "tool_choice": "none"
        });

        let chunk1 = r#"data: {
            "responseId": "stream_nobuffer_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Immediate stream text"}]
                }
            }]
        }"#;

        let events = stream(SEARCH_MODEL, &req, &[chunk1]).concat();
        assert!(
            names(&events).contains(&"response.output_text.delta"),
            "expected immediate output_text.delta event on chunk 1 when tool_choice is none, got events: {events:?}"
        );
    }

    // upstream request without googleSearch disables buffering
    {
        let original_req = json!({
            "model": SEARCH_MODEL,
            "input": "test query",
            "tools": [{"type": "web_search"}, {"type": "function", "function": {"name": "func1"}}]
        });

        // Effective upstream request had search stripped (e.g. Antigravity mixed tool fallback)
        let upstream_req = json!({
            "model": SEARCH_MODEL,
            "contents": [{"role": "user", "parts": [{"text": "test query"}]}],
            "tools": [{"functionDeclarations": [{"name": "func1"}]}]
        });

        let chunk1 = r#"data: {
            "responseId": "stream_nobuffer_2",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Immediate stream text"}]
                }
            }]
        }"#;

        let events = stream_with(SEARCH_MODEL, &original_req, &upstream_req, &[chunk1]).concat();
        assert!(
            names(&events).contains(&"response.output_text.delta"),
            "expected immediate output_text.delta event on chunk 1 when upstream lacks googleSearch, got events: {events:?}"
        );
    }
}

/// gjson `response.output.#(type=message)`: the first message in a
/// `response.completed` event's outputs, if any.
fn completed_message(completed: &Value) -> Option<&Value> {
    first_of_type(output(completed), "message")
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_multi_chunk_citations_and_explicit_part_index()
 {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test query",
        "tools": [{"type": "web_search"}]
    });

    // citation spanning across multiple streamed chunks
    {
        let chunk1 = r#"data: {
            "responseId": "stream_mc_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello "}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_mc_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "world"}]
                }
            }]
        }"#;

        // Grounding metadata referencing segment [0, 11) for full "Hello world"
        let chunk3 = r#"data: {
            "responseId": "stream_mc_1",
            "candidates": [{
                "groundingMetadata": {
                    "webSearchQueries": ["hello query"],
                    "groundingChunks": [{"web": {"uri": "https://example.com/hello", "title": "Hello"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {
                            "partIndex": 0,
                            "startIndex": 0,
                            "endIndex": 11,
                            "text": "Hello world"
                        }
                    }]
                }
            }]
        }"#;

        let chunk4 = r#"data: {
            "responseId": "stream_mc_1",
            "candidates": [{
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3, chunk4]).concat();
        let completed_json = completed(&all_events);

        let Some(msg) = completed_message(&completed_json) else {
            panic!("expected message output item, got: {completed_json}");
        };
        assert_eq!(
            text(msg, "content.0.text"),
            "Hello world",
            "expected full text 'Hello world'"
        );
        let citations = list(msg, "content.0.annotations");
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}: {msg}",
            citations.len()
        );
        assert_eq!(
            span(&citations[0]),
            (0, 11),
            "expected start_index=0, end_index=11 spanning across chunks"
        );
    }

    // citation spanning across multiple streamed chunks without partIndex
    {
        let chunk1 = r#"data: {
            "responseId": "stream_mc_1b",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello "}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_mc_1b",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "world"}]
                }
            }]
        }"#;

        // Grounding metadata referencing segment [0, 11) for full "Hello world" without partIndex
        let chunk3 = r#"data: {
            "responseId": "stream_mc_1b",
            "candidates": [{
                "groundingMetadata": {
                    "webSearchQueries": ["hello query"],
                    "groundingChunks": [{"web": {"uri": "https://example.com/hello", "title": "Hello"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {
                            "startIndex": 0,
                            "endIndex": 11,
                            "text": "Hello world"
                        }
                    }]
                }
            }]
        }"#;

        let chunk4 = r#"data: {
            "responseId": "stream_mc_1b",
            "candidates": [{
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3, chunk4]).concat();
        let completed_json = completed(&all_events);

        let Some(msg) = completed_message(&completed_json) else {
            panic!("expected message output item, got: {completed_json}");
        };
        let citations = list(msg, "content.0.annotations");
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}: {msg}",
            citations.len()
        );
        assert_eq!(
            span(&citations[0]),
            (0, 11),
            "expected start_index=0, end_index=11 spanning across chunks without partIndex"
        );
    }

    // citation referencing text in subsequent chunk
    {
        let chunk1 = r#"data: {
            "responseId": "stream_mc_2",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello "}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_mc_2",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "world"}]
                }
            }]
        }"#;

        // Grounding metadata referencing segment [6, 11) for "world" arriving in chunk 2
        let chunk3 = r#"data: {
            "responseId": "stream_mc_2",
            "candidates": [{
                "groundingMetadata": {
                    "webSearchQueries": ["world query"],
                    "groundingChunks": [{"web": {"uri": "https://example.com/world", "title": "World"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {
                            "partIndex": 0,
                            "startIndex": 6,
                            "endIndex": 11,
                            "text": "world"
                        }
                    }]
                }
            }]
        }"#;

        let chunk4 = r#"data: {
            "responseId": "stream_mc_2",
            "candidates": [{
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3, chunk4]).concat();
        let completed_json = completed(&all_events);

        let msg = completed_message(&completed_json).unwrap_or(&Value::Null);
        let citations = list(msg, "content.0.annotations");
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}: {msg}",
            citations.len()
        );
        assert_eq!(
            span(&citations[0]),
            (6, 11),
            "expected start_index=6, end_index=11 for 'world'"
        );
    }

    // streaming with explicit partIndex across parts
    {
        let chunk1 = r#"data: {
            "responseId": "stream_mc_3",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"partIndex": 0, "text": "Intro "}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_mc_3",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"partIndex": 1, "text": "Fact"}]
                }
            }]
        }"#;

        let chunk3 = r#"data: {
            "responseId": "stream_mc_3",
            "candidates": [{
                "groundingMetadata": {
                    "webSearchQueries": ["fact query"],
                    "groundingChunks": [{"web": {"uri": "https://example.com/fact", "title": "Fact"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {
                            "partIndex": 1,
                            "startIndex": 0,
                            "endIndex": 4,
                            "text": "Fact"
                        }
                    }]
                }
            }]
        }"#;

        let chunk4 = r#"data: {
            "responseId": "stream_mc_3",
            "candidates": [{
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3, chunk4]).concat();
        let completed_json = completed(&all_events);

        let msg = completed_message(&completed_json).unwrap_or(&Value::Null);
        let citations = list(msg, "content.0.annotations");
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}: {msg}",
            citations.len()
        );
        // "Intro " is 6 runes [0, 6). "Fact" starts at rune 6, end at 10.
        assert_eq!(
            span(&citations[0]),
            (6, 10),
            "expected start_index=6, end_index=10 for 'Fact'"
        );
    }

    // streaming with explicit partIndex across multiple chunks of same part
    {
        let chunk1 = r#"data: {
            "responseId": "stream_mc_4",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"partIndex": 0, "text": "Intro "}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_mc_4",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"partIndex": 0, "text": "continuation "}]
                }
            }]
        }"#;

        let chunk3 = r#"data: {
            "responseId": "stream_mc_4",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"partIndex": 1, "text": "Fact"}]
                }
            }]
        }"#;

        let chunk4 = r#"data: {
            "responseId": "stream_mc_4",
            "candidates": [{
                "groundingMetadata": {
                    "webSearchQueries": ["all queries"],
                    "groundingChunks": [
                        {"web": {"uri": "https://example.com/intro", "title": "Intro"}},
                        {"web": {"uri": "https://example.com/fact", "title": "Fact"}}
                    ],
                    "groundingSupports": [
                        {
                            "groundingChunkIndices": [0],
                            "segment": {
                                "partIndex": 0,
                                "startIndex": 0,
                                "endIndex": 19,
                                "text": "Intro continuation "
                            }
                        },
                        {
                            "groundingChunkIndices": [1],
                            "segment": {
                                "partIndex": 1,
                                "startIndex": 0,
                                "endIndex": 4,
                                "text": "Fact"
                            }
                        }
                    ]
                }
            }]
        }"#;

        let chunk5 = r#"data: {
            "responseId": "stream_mc_4",
            "candidates": [{
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let all_events = stream(
            SEARCH_MODEL,
            &req,
            &[chunk1, chunk2, chunk3, chunk4, chunk5],
        )
        .concat();
        let completed_json = completed(&all_events);

        let msg = completed_message(&completed_json).unwrap_or(&Value::Null);
        let citations = list(msg, "content.0.annotations");
        assert_eq!(
            citations.len(),
            2,
            "expected 2 citations, got {}: {msg}",
            citations.len()
        );
        // First citation: "Intro continuation " [0, 19)
        assert_eq!(
            span(&citations[0]),
            (0, 19),
            "expected citation 0 at [0, 19)"
        );
        // Second citation: "Fact" [19, 23)
        assert_eq!(
            span(&citations[1]),
            (19, 23),
            "expected citation 1 at [19, 23)"
        );
    }
}

/// Each `response.output_item.added` event's output index and item type.
fn added_items(events: &Events) -> Vec<(i64, String)> {
    events
        .iter()
        .filter(|(name, _)| name == "response.output_item.added")
        .map(|(_, data)| (int(data, "output_index"), text(data, "item.type")))
        .collect()
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_interleaved_text_and_thought() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test query",
        "tools": [{"type": "web_search"}]
    });

    // without grounding metadata preserves text -> thought -> text order
    {
        // Text A is sent first
        let chunk1 = r#"data: {
            "responseId": "stream_it_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Text A"}]
                }
            }]
        }"#;

        // Thought R arrives
        let chunk2 = r#"data: {
            "responseId": "stream_it_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"thought": true, "text": "Reasoning R"}]
                }
            }]
        }"#;

        // Text B arrives
        let chunk3 = r#"data: {
            "responseId": "stream_it_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Text B"}]
                }
            }]
        }"#;

        let chunk4 = r#"data: {
            "responseId": "stream_it_1",
            "candidates": [{
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3, chunk4]).concat();
        let added = added_items(&all_events);
        let completed_json = completed(&all_events);

        assert_eq!(
            added.len(),
            3,
            "expected 3 added items (message, reasoning, message), got {}: {added:?}",
            added.len()
        );
        assert_eq!(
            added[0],
            (0, "message".to_owned()),
            "expected added[0] to be message at index 0"
        );
        assert_eq!(
            added[1],
            (1, "reasoning".to_owned()),
            "expected added[1] to be reasoning at index 1"
        );
        assert_eq!(
            added[2],
            (2, "message".to_owned()),
            "expected added[2] to be message at index 2"
        );

        let outputs = output(&completed_json);
        assert_eq!(
            outputs.len(),
            3,
            "expected 3 completed outputs, got {}: {completed_json}",
            outputs.len()
        );
        assert!(
            text(&outputs[0], "type") == "message"
                && text(&outputs[0], "content.0.text") == "Text A",
            "unexpected outputs[0]: {}",
            outputs[0]
        );
        assert_eq!(
            text(&outputs[1], "type"),
            "reasoning",
            "unexpected outputs[1]: {}",
            outputs[1]
        );
        assert!(
            text(&outputs[2], "type") == "message"
                && text(&outputs[2], "content.0.text") == "Text B",
            "unexpected outputs[2]: {}",
            outputs[2]
        );
    }

    // with grounding metadata preserves text -> thought -> text order and attaches citations
    {
        let chunk1 = r#"data: {
            "responseId": "stream_it_2",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Text A"}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_it_2",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"thought": true, "text": "Reasoning R"}]
                }
            }]
        }"#;

        // Grounding metadata arrives with Text B
        let chunk3 = r#"data: {
            "responseId": "stream_it_2",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Text B"}]
                },
                "groundingMetadata": {
                    "webSearchQueries": ["search B"],
                    "groundingChunks": [
                        {"web": {"uri": "https://example.com/a", "title": "A Source"}},
                        {"web": {"uri": "https://example.com/b", "title": "B Source"}}
                    ],
                    "groundingSupports": [
                        {
                            "groundingChunkIndices": [0],
                            "segment": {
                                "partIndex": 0,
                                "startIndex": 0,
                                "endIndex": 6,
                                "text": "Text A"
                            }
                        },
                        {
                            "groundingChunkIndices": [1],
                            "segment": {
                                "partIndex": 2,
                                "startIndex": 0,
                                "endIndex": 6,
                                "text": "Text B"
                            }
                        }
                    ]
                }
            }]
        }"#;

        let chunk4 = r#"data: {
            "responseId": "stream_it_2",
            "candidates": [{
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3, chunk4]).concat();
        let added = added_items(&all_events);
        let completed_json = completed(&all_events);

        // Items: Message A (0), Reasoning R (1), web_search_call (2), Message B (3)
        assert_eq!(
            added.len(),
            4,
            "expected 4 added items (message A, reasoning, web_search_call, message B), got {}: {added:?}",
            added.len()
        );
        assert_eq!(
            added[0],
            (0, "message".to_owned()),
            "expected added[0] to be message at index 0"
        );
        assert_eq!(
            added[1],
            (1, "reasoning".to_owned()),
            "expected added[1] to be reasoning at index 1"
        );
        assert_eq!(
            added[2],
            (2, "web_search_call".to_owned()),
            "expected added[2] to be web_search_call at index 2"
        );
        assert_eq!(
            added[3],
            (3, "message".to_owned()),
            "expected added[3] to be message at index 3"
        );

        let outputs = output(&completed_json);
        assert_eq!(
            outputs.len(),
            4,
            "expected 4 completed outputs, got {}: {completed_json}",
            outputs.len()
        );
        assert_eq!(
            text(&outputs[0], "content.0.text"),
            "Text A",
            "unexpected outputs[0]: {}",
            outputs[0]
        );
        assert_eq!(
            text(&outputs[3], "content.0.text"),
            "Text B",
            "unexpected outputs[3]: {}",
            outputs[3]
        );

        // Verify Message A has citation for "Text A"
        let citations_a = list(&outputs[0], "content.0.annotations");
        assert_eq!(
            citations_a.len(),
            1,
            "expected 1 citation on Message A, got {}: {}",
            citations_a.len(),
            outputs[0]
        );
        assert_eq!(
            text(&citations_a[0], "url"),
            "https://example.com/a",
            "expected citation url 'https://example.com/a'"
        );

        // Verify Message B has citation for "Text B"
        let citations_b = list(&outputs[3], "content.0.annotations");
        assert_eq!(
            citations_b.len(),
            1,
            "expected 1 citation on Message B, got {}: {}",
            citations_b.len(),
            outputs[3]
        );
        assert_eq!(
            text(&citations_b[0], "url"),
            "https://example.com/b",
            "expected citation url 'https://example.com/b'"
        );
    }
}

/// The shared checks of the two
/// `TestConvertGeminiResponseToOpenAIResponsesStream_ConsecutiveBufferedTextChunksAfterClosure`
/// subtests: the last output is the "Hello world" message with citations
/// over `[0, 11)` and `[6, 11)`.
fn assert_last_message_hello_world_citations(completed_json: &Value) {
    let outputs = output(completed_json);
    assert!(
        outputs.len() >= 4,
        "expected at least 4 output items, got {}: {completed_json}",
        outputs.len()
    );
    let last_msg = &outputs[outputs.len() - 1];
    assert_eq!(
        text(last_msg, "type"),
        "message",
        "expected last output item to be message, got: {last_msg}"
    );
    assert_eq!(
        text(last_msg, "content.0.text"),
        "Hello world",
        "expected last message text 'Hello world'"
    );

    let citations = list(last_msg, "content.0.annotations");
    assert_eq!(
        citations.len(),
        2,
        "expected 2 citations on last message, got {}: {last_msg}",
        citations.len()
    );
    assert_eq!(
        span(&citations[0]),
        (0, 11),
        "expected citation 0 span [0, 11)"
    );
    assert_eq!(
        span(&citations[1]),
        (6, 11),
        "expected citation 1 span [6, 11)"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_consecutive_buffered_text_chunks_after_closure()
 {
    // function call followed by multiple consecutive buffered text chunks and late-arriving grounding metadata
    {
        let req = json!({
            "model": SEARCH_MODEL,
            "input": "test query",
            "tools": [{"type": "web_search"}, {"type": "function", "name": "test_tool"}]
        });

        let chunk1 = r#"data: {
            "responseId": "stream_cbc_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "text A"}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_cbc_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": "test_tool", "args": {}}}]
                }
            }]
        }"#;

        let chunk3 = r#"data: {
            "responseId": "stream_cbc_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello "}]
                }
            }]
        }"#;

        let chunk4 = r#"data: {
            "responseId": "stream_cbc_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "world"}]
                }
            }]
        }"#;

        let chunk5 = r#"data: {
            "responseId": "stream_cbc_1",
            "candidates": [{
                "groundingMetadata": {
                    "webSearchQueries": ["hello query"],
                    "groundingChunks": [
                        {"web": {"uri": "https://example.com/hello", "title": "Hello"}},
                        {"web": {"uri": "https://example.com/world", "title": "World"}}
                    ],
                    "groundingSupports": [
                        {
                            "groundingChunkIndices": [0],
                            "segment": {
                                "partIndex": 2,
                                "startIndex": 0,
                                "endIndex": 11,
                                "text": "Hello world"
                            }
                        },
                        {
                            "groundingChunkIndices": [1],
                            "segment": {
                                "partIndex": 2,
                                "startIndex": 6,
                                "endIndex": 11,
                                "text": "world"
                            }
                        }
                    ]
                }
            }]
        }"#;

        let chunk6 = r#"data: {
            "responseId": "stream_cbc_1",
            "candidates": [{
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let all_events = stream(
            SEARCH_MODEL,
            &req,
            &[chunk1, chunk2, chunk3, chunk4, chunk5, chunk6],
        )
        .concat();

        // Expect: message 0 (text A), function_call 1, web_search_call 2, message 3 (Hello world)
        assert_last_message_hello_world_citations(&completed(&all_events));
    }

    // thought followed by multiple consecutive buffered text chunks and late-arriving grounding metadata
    {
        let req = json!({
            "model": SEARCH_MODEL,
            "input": "test query",
            "tools": [{"type": "web_search"}]
        });

        let chunk1 = r#"data: {
            "responseId": "stream_cbc_2",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "text A"}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_cbc_2",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"thought": true, "text": "Reasoning R"}]
                }
            }]
        }"#;

        let chunk3 = r#"data: {
            "responseId": "stream_cbc_2",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello "}]
                }
            }]
        }"#;

        let chunk4 = r#"data: {
            "responseId": "stream_cbc_2",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "world"}]
                }
            }]
        }"#;

        let chunk5 = r#"data: {
            "responseId": "stream_cbc_2",
            "candidates": [{
                "groundingMetadata": {
                    "webSearchQueries": ["hello query"],
                    "groundingChunks": [
                        {"web": {"uri": "https://example.com/hello", "title": "Hello"}},
                        {"web": {"uri": "https://example.com/world", "title": "World"}}
                    ],
                    "groundingSupports": [
                        {
                            "groundingChunkIndices": [0],
                            "segment": {
                                "partIndex": 2,
                                "startIndex": 0,
                                "endIndex": 11,
                                "text": "Hello world"
                            }
                        },
                        {
                            "groundingChunkIndices": [1],
                            "segment": {
                                "partIndex": 2,
                                "startIndex": 6,
                                "endIndex": 11,
                                "text": "world"
                            }
                        }
                    ]
                }
            }]
        }"#;

        let chunk6 = r#"data: {
            "responseId": "stream_cbc_2",
            "candidates": [{
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let all_events = stream(
            SEARCH_MODEL,
            &req,
            &[chunk1, chunk2, chunk3, chunk4, chunk5, chunk6],
        )
        .concat();

        assert_last_message_hello_world_citations(&completed(&all_events));
    }
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_consecutive_function_calls_advance_part_index()
 {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test query",
        "tools": [
            {"type": "web_search"},
            {"type": "function", "function": {"name": "func_a", "parameters": {"type": "object"}}},
            {"type": "function", "function": {"name": "func_b", "parameters": {"type": "object"}}}
        ]
    });

    let chunk1 = r#"data: {
        "responseId": "stream_fc_adv_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Initial text."}],
                "role": "model"
            }
        }]
    }"#;

    let chunk2 = r#"data: {
        "responseId": "stream_fc_adv_1",
        "candidates": [{
            "content": {
                "parts": [{"functionCall": {"name": "func_a", "args": {}}}],
                "role": "model"
            }
        }]
    }"#;

    let chunk3 = r#"data: {
        "responseId": "stream_fc_adv_1",
        "candidates": [{
            "content": {
                "parts": [{"functionCall": {"name": "func_b", "args": {}}}],
                "role": "model"
            }
        }]
    }"#;

    let chunk4 = r#"data: {
        "responseId": "stream_fc_adv_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Trailing citation text."}],
                "role": "model"
            }
        }]
    }"#;

    let chunk5 = r#"data: {
        "responseId": "stream_fc_adv_1",
        "candidates": [{
            "groundingMetadata": {
                "webSearchQueries": ["test query"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/trailing", "title": "Trailing Source"}}
                ],
                "groundingSupports": [
                    {
                        "segment": {
                            "startIndex": 0,
                            "endIndex": 8,
                            "text": "Trailing",
                            "partIndex": 3
                        },
                        "groundingChunkIndices": [0]
                    }
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 20, "totalTokenCount": 25}
    }"#;

    let all_events = stream(
        SEARCH_MODEL,
        &req,
        &[chunk1, chunk2, chunk3, chunk4, chunk5],
    )
    .concat();
    let completed_json = completed(&all_events);

    let outputs = output(&completed_json);
    assert!(
        !outputs.is_empty(),
        "expected outputs in completed response, got none: {completed_json}"
    );

    let Some(trailing_msg) = outputs.iter().find(|out| {
        text(out, "type") == "message" && text(out, "content.0.text") == "Trailing citation text."
    }) else {
        panic!("trailing message not found in outputs: {completed_json}");
    };

    let citations = list(trailing_msg, "content.0.annotations");
    assert_eq!(
        citations.len(),
        1,
        "expected 1 citation on trailing message with partIndex=3, got {}: {trailing_msg}",
        citations.len()
    );
    assert_eq!(
        text(&citations[0], "url"),
        "https://example.com/trailing",
        "expected citation url 'https://example.com/trailing'"
    );
    assert_eq!(span(&citations[0]), (0, 8), "expected citation span [0, 8)");
}

/// The values of a JSON array of numbers.
fn ints(value: &Value, path: &str) -> Vec<i64> {
    list(value, path).iter().map(int_of).collect()
}

/// The `web.uri` of each grounding chunk.
fn chunk_uris(metadata: &Value) -> Vec<String> {
    list(metadata, "groundingChunks")
        .iter()
        .map(|chunk| text(chunk, "web.uri"))
        .collect()
}

#[test]
fn merge_grounding_metadata() {
    use super::merge_grounding_metadata as merge;

    // merging disjoint chunks and supports with index remapping
    {
        let gm1 = json!({
            "webSearchQueries": ["query1"],
            "groundingChunks": [
                {"web": {"uri": "https://example.com/1", "title": "Title 1"}}
            ],
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [0]
                }
            ]
        });
        let gm2 = json!({
            "webSearchQueries": ["query2"],
            "groundingChunks": [
                {"web": {"uri": "https://example.com/2", "title": "Title 2"}}
            ],
            "groundingSupports": [
                {
                    "segment": {"startIndex": 6, "endIndex": 10, "partIndex": 0},
                    "groundingChunkIndices": [1]
                }
            ]
        });

        let merged = merge(Some(&gm1), Some(&gm2)).unwrap_or_default();

        let queries: Vec<String> = list(&merged, "webSearchQueries")
            .iter()
            .map(string)
            .collect();
        assert_eq!(
            queries,
            ["query1", "query2"],
            "expected 2 queries [query1, query2], got: {:?}",
            at(&merged, "webSearchQueries")
        );

        let chunks = list(&merged, "groundingChunks");
        assert_eq!(
            chunks.len(),
            2,
            "expected 2 chunks, got {}: {merged}",
            chunks.len()
        );
        assert_eq!(
            chunk_uris(&merged),
            ["https://example.com/1", "https://example.com/2"],
            "unexpected chunks: {:?}",
            at(&merged, "groundingChunks")
        );

        let supports = list(&merged, "groundingSupports");
        assert_eq!(
            supports.len(),
            2,
            "expected 2 supports, got {}: {merged}",
            supports.len()
        );
        assert_eq!(
            int(&supports[0], "groundingChunkIndices.0"),
            0,
            "expected support 0 index 0"
        );
        assert_eq!(
            int(&supports[1], "groundingChunkIndices.0"),
            1,
            "expected support 1 remapped index 1"
        );
    }

    // incremental supports without chunks referencing previously seen chunk
    {
        let gm1 = json!({
            "webSearchQueries": ["query1"],
            "groundingChunks": [
                {"web": {"uri": "https://example.com/1", "title": "Title 1"}}
            ]
        });
        let gm2 = json!({
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [0]
                }
            ]
        });

        let merged = merge(Some(&gm1), Some(&gm2)).unwrap_or_default();
        assert_eq!(
            chunk_uris(&merged),
            ["https://example.com/1"],
            "expected 1 chunk retained, got: {:?}",
            at(&merged, "groundingChunks")
        );

        let supports = list(&merged, "groundingSupports");
        assert!(
            supports.len() == 1 && int(&supports[0], "groundingChunkIndices.0") == 0,
            "expected 1 support pointing to chunk 0, got: {:?}",
            at(&merged, "groundingSupports")
        );
    }

    // overlapping chunks deduplication and remapping
    {
        let gm1 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/1", "title": ""}}
            ]
        });
        let gm2 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/1", "title": "Title 1"}},
                {"web": {"uri": "https://example.com/2", "title": "Title 2"}}
            ],
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [0, 2]
                }
            ]
        });

        let merged = merge(Some(&gm1), Some(&gm2)).unwrap_or_default();
        let chunks = list(&merged, "groundingChunks");
        assert_eq!(
            chunks.len(),
            2,
            "expected 2 chunks, got {}: {merged}",
            chunks.len()
        );
        assert_eq!(
            text(&chunks[0], "web.title"),
            "Title 1",
            "expected upgraded title 'Title 1'"
        );
        let supports = list(&merged, "groundingSupports");
        assert_eq!(supports.len(), 1, "expected 1 support, got: {merged}");
        assert_eq!(
            ints(&supports[0], "groundingChunkIndices"),
            [0, 1],
            "expected indices [0, 1], got: {:?}",
            at(&supports[0], "groundingChunkIndices")
        );
    }

    // deduplication of identical supports
    {
        let gm1 = json!({
            "groundingChunks": [{"web": {"uri": "https://example.com/1"}}],
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [0]
                }
            ]
        });
        let gm2 = json!({
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [0]
                },
                {
                    "segment": {"startIndex": 6, "endIndex": 10, "partIndex": 0},
                    "groundingChunkIndices": [0]
                }
            ]
        });

        let merged = merge(Some(&gm1), Some(&gm2)).unwrap_or_default();
        let supports = list(&merged, "groundingSupports");
        assert_eq!(
            supports.len(),
            2,
            "expected 2 unique supports, got {}: {merged}",
            supports.len()
        );
    }

    // incremental grounding queries then supports then chunks preserves indices
    {
        let gm1 = json!({
            "webSearchQueries": ["query1"]
        });
        let gm2 = json!({
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [0]
                }
            ]
        });
        let gm3 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/1", "title": "Title 1"}}
            ]
        });

        let merged1 = merge(Some(&gm1), Some(&gm2)).unwrap_or_default();
        let supports1 = list(&merged1, "groundingSupports");
        assert!(
            supports1.len() == 1 && int(&supports1[0], "groundingChunkIndices.0") == 0,
            "expected 1 support with index 0 preserved, got: {:?}",
            at(&merged1, "groundingSupports")
        );

        let merged2 = merge(Some(&merged1), Some(&gm3)).unwrap_or_default();
        assert_eq!(
            chunk_uris(&merged2),
            ["https://example.com/1"],
            "expected 1 chunk, got: {:?}",
            at(&merged2, "groundingChunks")
        );
        let supports2 = list(&merged2, "groundingSupports");
        assert!(
            supports2.len() == 1 && int(&supports2[0], "groundingChunkIndices.0") == 0,
            "expected 1 support with index 0, got: {:?}",
            at(&merged2, "groundingSupports")
        );

        let citations = build_url_citations(Some(&merged2), Some("Hello world"));
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}",
            citations.len()
        );
        assert_eq!(
            text(&citations[0], "url"),
            "https://example.com/1",
            "expected url https://example.com/1, got: {}",
            citations[0]
        );
    }

    // supports arrive first with index 1 then identical duplicate URL chunks arrive and deduplicate
    {
        let gm1 = json!({
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 11, "partIndex": 0},
                    "groundingChunkIndices": [1]
                }
            ]
        });
        let gm2 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/dup", "title": "Dup 1"}},
                {"web": {"uri": "https://example.com/dup", "title": "Dup 2"}}
            ]
        });

        let merged = merge(Some(&gm1), Some(&gm2)).unwrap_or_default();
        let chunks = list(&merged, "groundingChunks");
        assert_eq!(
            chunks.len(),
            1,
            "expected 1 deduplicated chunk, got {}: {:?}",
            chunks.len(),
            at(&merged, "groundingChunks")
        );
        assert_eq!(
            text(&chunks[0], "web.uri"),
            "https://example.com/dup",
            "expected uri https://example.com/dup"
        );

        let supports = list(&merged, "groundingSupports");
        assert_eq!(
            supports.len(),
            1,
            "expected 1 support, got {}: {:?}",
            supports.len(),
            at(&merged, "groundingSupports")
        );
        assert_eq!(
            ints(&supports[0], "groundingChunkIndices"),
            [0],
            "expected remapped index [0], got: {:?}",
            at(&supports[0], "groundingChunkIndices")
        );

        let citations = build_url_citations(Some(&merged), Some("Hello world"));
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}",
            citations.len()
        );
        assert_eq!(
            text(&citations[0], "url"),
            "https://example.com/dup",
            "expected citation url https://example.com/dup, got {}",
            citations[0]
        );
    }

    // queries in frame 1 then duplicate chunks [A, A, B] in frame 2 then supports [2] in frame 3
    {
        // Frame 1: queries only
        let gm1 = json!({
            "webSearchQueries": ["test query"]
        });

        // Frame 2: duplicate chunks [A, A, B] (indices 0, 1, 2)
        let gm2 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/A", "title": "Title A1"}},
                {"web": {"uri": "https://example.com/A", "title": "Title A2"}},
                {"web": {"uri": "https://example.com/B", "title": "Title B"}}
            ]
        });

        let merged1 = merge(Some(&gm1), Some(&gm2)).unwrap_or_default();
        let chunks1 = list(&merged1, "groundingChunks");
        assert_eq!(
            chunks1.len(),
            2,
            "expected 2 deduplicated chunks [A, B], got {}: {:?}",
            chunks1.len(),
            at(&merged1, "groundingChunks")
        );
        assert_eq!(
            chunk_uris(&merged1),
            ["https://example.com/A", "https://example.com/B"],
            "unexpected chunks order: {:?}",
            at(&merged1, "groundingChunks")
        );

        // Frame 3: supports only with index [2], pointing to B in Gemini stream
        let gm3 = json!({
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [2]
                },
                {
                    "segment": {"startIndex": 6, "endIndex": 11, "partIndex": 0},
                    "groundingChunkIndices": [1]
                }
            ]
        });

        let merged2 = merge(Some(&merged1), Some(&gm3)).unwrap_or_default();
        let supports = list(&merged2, "groundingSupports");
        assert_eq!(
            supports.len(),
            2,
            "expected 2 supports, got {}: {:?}",
            supports.len(),
            at(&merged2, "groundingSupports")
        );

        // Support 0 had index [2], should be remapped to index 1 (chunk B)
        assert_eq!(
            ints(&supports[0], "groundingChunkIndices"),
            [1],
            "expected support 0 remapped to chunk index 1 (B), got: {:?}",
            at(&supports[0], "groundingChunkIndices")
        );

        // Support 1 had index [1], should be remapped to index 0 (chunk A)
        assert_eq!(
            ints(&supports[1], "groundingChunkIndices"),
            [0],
            "expected support 1 remapped to chunk index 0 (A), got: {:?}",
            at(&supports[1], "groundingChunkIndices")
        );

        let citations = build_url_citations(Some(&merged2), Some("Alpha Beta."));
        assert_eq!(
            citations.len(),
            2,
            "expected 2 citations, got {}",
            citations.len()
        );
        assert_eq!(
            text(&citations[0], "url"),
            "https://example.com/B",
            "expected citation 0 url https://example.com/B, got {}",
            citations[0]
        );
        assert_eq!(
            text(&citations[1], "url"),
            "https://example.com/A",
            "expected citation 1 url https://example.com/A, got {}",
            citations[1]
        );
    }

    // existing pending support resolves to cumulative raw index rather than subsequent frame local chunk index
    {
        // Frame 1: groundingChunks = [A], support with groundingChunkIndices = [1]
        let gm1 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/A", "title": "Chunk A"}}
            ],
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [1]
                }
            ]
        });

        // Frame 2: groundingChunks = [B, C]
        let gm2 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/B", "title": "Chunk B"}},
                {"web": {"uri": "https://example.com/C", "title": "Chunk C"}}
            ]
        });

        let merged1 = merge(None, Some(&gm1));
        let merged2 = merge(merged1.as_ref(), Some(&gm2)).unwrap_or_default();

        let chunks = list(&merged2, "groundingChunks");
        assert_eq!(
            chunks.len(),
            3,
            "expected 3 chunks [A, B, C], got {}: {:?}",
            chunks.len(),
            at(&merged2, "groundingChunks")
        );
        assert_eq!(
            chunk_uris(&merged2),
            [
                "https://example.com/A",
                "https://example.com/B",
                "https://example.com/C"
            ],
            "unexpected chunks: {:?}",
            at(&merged2, "groundingChunks")
        );

        let supports = list(&merged2, "groundingSupports");
        assert_eq!(
            supports.len(),
            1,
            "expected 1 support, got {}: {:?}",
            supports.len(),
            at(&merged2, "groundingSupports")
        );

        // Frame 1's existing support had cumulative index [1], must resolve to Chunk B (merged index 1), not Chunk C (merged index 2)
        assert_eq!(
            ints(&supports[0], "groundingChunkIndices"),
            [1],
            "expected existing support 0 to resolve to chunk index 1 (B), got {:?}",
            at(&supports[0], "groundingChunkIndices")
        );

        let citations = build_url_citations(Some(&merged2), Some("Alpha."));
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}",
            citations.len()
        );
        assert_eq!(
            text(&citations[0], "url"),
            "https://example.com/B",
            "expected citation url https://example.com/B, got {}",
            citations[0]
        );
    }

    // existing pending support and new frame support with same numeric index resolve correctly
    {
        // Frame 1: groundingChunks = [A], support with groundingChunkIndices = [1]
        let gm1 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/A", "title": "Chunk A"}}
            ],
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [1]
                }
            ]
        });

        // Frame 2: groundingChunks = [B, C], and new support with cumulative index [1] (pointing to B)
        let gm2 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/B", "title": "Chunk B"}},
                {"web": {"uri": "https://example.com/C", "title": "Chunk C"}}
            ],
            "groundingSupports": [
                {
                    "segment": {"startIndex": 6, "endIndex": 11, "partIndex": 0},
                    "groundingChunkIndices": [1]
                }
            ]
        });

        let merged1 = merge(None, Some(&gm1));
        let merged2 = merge(merged1.as_ref(), Some(&gm2)).unwrap_or_default();

        let supports = list(&merged2, "groundingSupports");
        assert_eq!(
            supports.len(),
            2,
            "expected 2 supports, got {}: {:?}",
            supports.len(),
            at(&merged2, "groundingSupports")
        );

        // Support 0 (existing from frame 1): cumulative raw index 1 -> Chunk B (index 1)
        assert_eq!(
            ints(&supports[0], "groundingChunkIndices"),
            [1],
            "expected existing support 0 to resolve to chunk index 1 (B), got {:?}",
            at(&supports[0], "groundingChunkIndices")
        );

        // Support 1 (new from frame 2): cumulative index 1 -> Chunk B (index 1)
        assert_eq!(
            ints(&supports[1], "groundingChunkIndices"),
            [1],
            "expected new support 1 to resolve to chunk index 1 (B), got {:?}",
            at(&supports[1], "groundingChunkIndices")
        );

        let citations = build_url_citations(Some(&merged2), Some("Alpha Beta."));
        assert_eq!(
            citations.len(),
            2,
            "expected 2 citations, got {}",
            citations.len()
        );
        assert_eq!(
            text(&citations[0], "url"),
            "https://example.com/B",
            "expected citation 0 url https://example.com/B, got {}",
            citations[0]
        );
        assert_eq!(
            text(&citations[1], "url"),
            "https://example.com/B",
            "expected citation 1 url https://example.com/B, got {}",
            citations[1]
        );
    }

    // deduplicated frame 1 chunks resolve frame 2 cumulative support index correctly
    {
        // Frame 1: groundingChunks = [A, A] (deduplicated to [A])
        let gm1 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/A", "title": "Chunk A"}},
                {"web": {"uri": "https://example.com/A", "title": "Chunk A"}}
            ]
        });

        // Frame 2: groundingChunks = [B], with new support referencing groundingChunkIndices = [1] (pointing to A)
        let gm2 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/B", "title": "Chunk B"}}
            ],
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [1]
                }
            ]
        });

        let merged1 = merge(None, Some(&gm1));
        let merged2 = merge(merged1.as_ref(), Some(&gm2)).unwrap_or_default();

        let chunks = list(&merged2, "groundingChunks");
        assert_eq!(
            chunks.len(),
            2,
            "expected 2 chunks [A, B], got {}: {:?}",
            chunks.len(),
            at(&merged2, "groundingChunks")
        );
        assert_eq!(
            chunk_uris(&merged2),
            ["https://example.com/A", "https://example.com/B"],
            "unexpected chunks: {:?}",
            at(&merged2, "groundingChunks")
        );

        let supports = list(&merged2, "groundingSupports");
        assert_eq!(
            supports.len(),
            1,
            "expected 1 support, got {}: {:?}",
            supports.len(),
            at(&merged2, "groundingSupports")
        );

        // Support referenced index [1], which in candidate stream was raw chunk 1 (Chunk A).
        // Must resolve to merged index 0 (Chunk A), NOT merged index 1 (Chunk B).
        assert_eq!(
            ints(&supports[0], "groundingChunkIndices"),
            [0],
            "expected support to resolve to chunk index 0 (A), got {:?}",
            at(&supports[0], "groundingChunkIndices")
        );

        let citations = build_url_citations(Some(&merged2), Some("Alpha."));
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}",
            citations.len()
        );
        assert_eq!(
            text(&citations[0], "url"),
            "https://example.com/A",
            "expected citation url https://example.com/A, got {}",
            citations[0]
        );
    }

    // deduplicated frame 1 chunks [A, A] resolve frame 2 chunks [B, C] cumulative support [1] to A not C
    {
        // Frame 1: groundingChunks = [A, A] (deduplicated to [A], prevRawCount = 2, cumulativeRemap has {0: 0, 1: 0})
        let gm1 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/A", "title": "Chunk A"}},
                {"web": {"uri": "https://example.com/A", "title": "Chunk A"}}
            ]
        });

        // Frame 2: groundingChunks = [B, C], and support with groundingChunkIndices = [1] (referencing cumulative raw chunk 1 = A)
        let gm2 = json!({
            "groundingChunks": [
                {"web": {"uri": "https://example.com/B", "title": "Chunk B"}},
                {"web": {"uri": "https://example.com/C", "title": "Chunk C"}}
            ],
            "groundingSupports": [
                {
                    "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                    "groundingChunkIndices": [1]
                }
            ]
        });

        let merged1 = merge(None, Some(&gm1));
        let merged2 = merge(merged1.as_ref(), Some(&gm2)).unwrap_or_default();

        let chunks = list(&merged2, "groundingChunks");
        assert_eq!(
            chunks.len(),
            3,
            "expected 3 chunks [A, B, C], got {}: {:?}",
            chunks.len(),
            at(&merged2, "groundingChunks")
        );
        assert_eq!(
            chunk_uris(&merged2),
            [
                "https://example.com/A",
                "https://example.com/B",
                "https://example.com/C"
            ],
            "unexpected chunks: {:?}",
            at(&merged2, "groundingChunks")
        );

        let supports = list(&merged2, "groundingSupports");
        assert_eq!(
            supports.len(),
            1,
            "expected 1 support, got {}: {:?}",
            supports.len(),
            at(&merged2, "groundingSupports")
        );

        // Support referenced cumulative index [1], which in candidate stream was raw chunk 1 (Chunk A).
        // Must resolve to merged index 0 (Chunk A), NOT merged index 2 (Chunk C).
        assert_eq!(
            ints(&supports[0], "groundingChunkIndices"),
            [0],
            "expected support to resolve to chunk index 0 (A), got {:?}",
            at(&supports[0], "groundingChunkIndices")
        );

        let citations = build_url_citations(Some(&merged2), Some("Alpha."));
        assert_eq!(
            citations.len(),
            1,
            "expected 1 citation, got {}",
            citations.len()
        );
        assert_eq!(
            text(&citations[0], "url"),
            "https://example.com/A",
            "expected citation url https://example.com/A, got {}",
            citations[0]
        );
    }
}

#[test]
fn merge_citation_annotations() {
    use super::merge_citation_annotations as merge;

    let c1 = json!({"type":"url_citation","url":"https://example.com/1","title":"","start_index":0,"end_index":5});
    let c2 = json!({"type":"url_citation","url":"https://example.com/1","title":"Title 1","start_index":0,"end_index":5});
    let c3 = json!({"type":"url_citation","url":"https://example.com/2","title":"Title 2","start_index":6,"end_index":10});

    let merged = merge(Vec::new(), vec![c1.clone()]);
    assert_eq!(merged.len(), 1, "expected 1 citation, got {}", merged.len());

    let merged = merge(vec![c1], vec![c2, c3]);
    assert_eq!(
        merged.len(),
        2,
        "expected 2 citations, got {}",
        merged.len()
    );
    assert_eq!(
        text(&merged[0], "title"),
        "Title 1",
        "expected upgraded title 'Title 1'"
    );
    assert_eq!(
        text(&merged[1], "url"),
        "https://example.com/2",
        "expected url 'https://example.com/2'"
    );
}

/// The checks `MultipleGroundingUpdates` and
/// `IncrementalGroundingQueriesSupportsChunks` share: the web_search_call
/// `output_item.done` event carries the same sources, by URL and in order,
/// as the completed web_search_call item.
fn assert_done_sources_match(by_type: &HashMap<String, Vec<Value>>, sources: &[Value]) {
    let Some(ws_done) = find_web_search_call_done(by(by_type, "response.output_item.done")) else {
        panic!("expected web_search_call output_item.done");
    };
    let done_sources = list(ws_done, "item.action.sources");
    assert_eq!(
        done_sources.len(),
        sources.len(),
        "output_item.done sources={}, response.completed sources={}; want matching full sources",
        done_sources.len(),
        sources.len()
    );
    for (i, (done_source, source)) in done_sources.iter().zip(sources).enumerate() {
        assert_eq!(
            text(done_source, "url"),
            text(source, "url"),
            "output_item.done source[{i}] differs from completed source[{i}]"
        );
    }
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_multiple_grounding_updates() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test search",
        "tools": [{"type": "web_search"}]
    });

    let chunk1 = r#"data: {
        "responseId": "stream_multi_gm_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Alpha "}],
                "role": "model"
            },
            "groundingMetadata": {
                "webSearchQueries": ["query alpha"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/alpha", "title": "Alpha"}}
                ],
                "groundingSupports": [
                    {
                        "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                        "groundingChunkIndices": [0]
                    }
                ]
            }
        }]
    }"#;

    let chunk2 = r#"data: {
        "responseId": "stream_multi_gm_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Beta."}],
                "role": "model"
            },
            "groundingMetadata": {
                "webSearchQueries": ["query beta"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/beta", "title": "Beta"}}
                ],
                "groundingSupports": [
                    {
                        "segment": {"startIndex": 6, "endIndex": 10, "partIndex": 0},
                        "groundingChunkIndices": [1]
                    }
                ]
            }
        }]
    }"#;

    let chunk3 = r#"data: {
        "responseId": "stream_multi_gm_1",
        "candidates": [{
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3]);

    let (_, e1_by_type) = events_by_type(&events[0]);
    let e1_completed = by(&e1_by_type, "response.web_search_call.completed");
    assert!(
        e1_completed.is_empty(),
        "did not expect web_search_call.completed on first incremental grounding frame, events={e1_completed:?}"
    );
    if let Some(done) = find_web_search_call_done(by(&e1_by_type, "response.output_item.done")) {
        panic!(
            "did not expect web_search_call output_item.done on first incremental grounding frame, got: {done}"
        );
    }

    let all_events = events.concat();
    let (_, by_type) = events_by_type(&all_events);
    let Some(completed_json) = by(&by_type, "response.completed").first() else {
        panic!("expected response.completed event");
    };

    let outputs = output(completed_json);
    let msg_item = last_of_type(outputs, "message");
    let ws_item = last_of_type(outputs, "web_search_call");

    let Some(msg_item) = msg_item else {
        panic!("expected message output item, got: {completed_json}");
    };
    assert_eq!(
        text(msg_item, "content.0.text"),
        "Alpha Beta.",
        "expected message text 'Alpha Beta.'"
    );

    let citations = list(msg_item, "content.0.annotations");
    assert_eq!(
        citations.len(),
        2,
        "expected 2 citations from merged grounding metadata, got {}: {msg_item}",
        citations.len()
    );
    assert!(
        text(&citations[0], "url") == "https://example.com/alpha" && span(&citations[0]) == (0, 5),
        "unexpected citation 0: {}",
        citations[0]
    );
    assert!(
        text(&citations[1], "url") == "https://example.com/beta" && span(&citations[1]) == (6, 10),
        "unexpected citation 1: {}",
        citations[1]
    );

    let Some(ws_item) = ws_item else {
        panic!("expected web_search_call item, got: {completed_json}");
    };
    let sources = list(ws_item, "action.sources");
    assert_eq!(
        sources.len(),
        2,
        "expected 2 sources in web_search_call, got {}: {ws_item}",
        sources.len()
    );

    assert_done_sources_match(&by_type, sources);
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_incremental_supports_without_chunks() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test search",
        "tools": [{"type": "web_search"}]
    });

    let chunk1 = r#"data: {
        "responseId": "stream_incr_sup_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Hello world."}],
                "role": "model"
            },
            "groundingMetadata": {
                "webSearchQueries": ["query hw"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/hw", "title": "Hello World"}}
                ]
            }
        }]
    }"#;

    let chunk2 = r#"data: {
        "responseId": "stream_incr_sup_1",
        "candidates": [{
            "groundingMetadata": {
                "groundingSupports": [
                    {
                        "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                        "groundingChunkIndices": [0]
                    }
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2]).concat();
    let completed_json = completed(&all_events);

    let Some(msg_item) = first_of_type(output(&completed_json), "message") else {
        panic!("expected message output item, got: {completed_json}");
    };

    let citations = list(msg_item, "content.0.annotations");
    assert_eq!(
        citations.len(),
        1,
        "expected 1 citation attached from incremental supports, got {}: {msg_item}",
        citations.len()
    );
    assert!(
        text(&citations[0], "url") == "https://example.com/hw" && span(&citations[0]) == (0, 5),
        "unexpected citation: {}",
        citations[0]
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_incremental_grounding_queries_supports_chunks()
 {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test search",
        "tools": [{"type": "web_search"}]
    });

    // Frame 1: queries only
    let chunk1 = r#"data: {
        "responseId": "stream_incr_qsc_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Hello world."}],
                "role": "model"
            },
            "groundingMetadata": {
                "webSearchQueries": ["query hw"]
            }
        }]
    }"#;

    // Frame 2: supports only (chunks not yet present)
    let chunk2 = r#"data: {
        "responseId": "stream_incr_qsc_1",
        "candidates": [{
            "groundingMetadata": {
                "groundingSupports": [
                    {
                        "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                        "groundingChunkIndices": [0]
                    }
                ]
            }
        }]
    }"#;

    // Frame 3: chunks arrive
    let chunk3 = r#"data: {
        "responseId": "stream_incr_qsc_1",
        "candidates": [{
            "groundingMetadata": {
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/hw", "title": "Hello World"}}
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3]);

    let (_, e1_by_type) = events_by_type(&events[0]);
    let Some(e1_added) = by(&e1_by_type, "response.output_item.added").first() else {
        panic!("expected web_search_call output_item.added when queries first arrive");
    };
    assert_eq!(
        text(e1_added, "item.status"),
        "in_progress",
        "expected in_progress web_search_call on queries-only frame, got: {e1_added}"
    );
    assert!(
        by(&e1_by_type, "response.web_search_call.completed").is_empty(),
        "did not expect web_search_call.completed on queries-only frame"
    );
    if let Some(done) = find_web_search_call_done(by(&e1_by_type, "response.output_item.done")) {
        panic!(
            "did not expect web_search_call output_item.done on queries-only frame, got: {done}"
        );
    }

    let all_events = events.concat();
    let (_, by_type) = events_by_type(&all_events);
    let Some(completed_json) = by(&by_type, "response.completed").first() else {
        panic!("expected response.completed event");
    };

    let outputs = output(completed_json);
    let msg_item = last_of_type(outputs, "message");
    let ws_item = last_of_type(outputs, "web_search_call");
    let Some(msg_item) = msg_item else {
        panic!("expected message output item, got: {completed_json}");
    };

    let citations = list(msg_item, "content.0.annotations");
    assert_eq!(
        citations.len(),
        1,
        "expected 1 citation from queries -> supports -> chunks sequence, got {}: {msg_item}",
        citations.len()
    );
    assert!(
        text(&citations[0], "url") == "https://example.com/hw" && span(&citations[0]) == (0, 5),
        "unexpected citation: {}",
        citations[0]
    );

    let Some(ws_item) = ws_item else {
        panic!("expected web_search_call item, got: {completed_json}");
    };
    let sources = list(ws_item, "action.sources");
    assert!(
        sources.len() == 1 && text(&sources[0], "url") == "https://example.com/hw",
        "expected source in web_search_call, got: {ws_item}"
    );

    assert_done_sources_match(&by_type, sources);
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_late_sources_after_interleaved_finalization()
{
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test search",
        "tools": [{"type": "web_search"}, {"type": "function", "name": "lookup"}]
    });

    // Frame 1: queries and an initial source arrive with text.
    let chunk1 = r#"data: {
        "responseId": "stream_late_src_fc_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Alpha "}],
                "role": "model"
            },
            "groundingMetadata": {
                "webSearchQueries": ["query alpha"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/alpha", "title": "Alpha"}}
                ]
            }
        }]
    }"#;

    // Frame 2: a function call forces early web_search_call finalization.
    let chunk2 = r#"data: {
        "responseId": "stream_late_src_fc_1",
        "candidates": [{
            "content": {
                "parts": [{"functionCall": {"name": "lookup", "args": {}}}],
                "role": "model"
            }
        }]
    }"#;

    // Frame 3: additional sources and queries arrive after the search item was emitted.
    let chunk3 = r#"data: {
        "responseId": "stream_late_src_fc_1",
        "candidates": [{
            "groundingMetadata": {
                "webSearchQueries": ["query beta"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/beta", "title": "Beta"}}
                ]
            }
        }]
    }"#;

    let chunk4 = r#"data: {
        "responseId": "stream_late_src_fc_1",
        "candidates": [{
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3, chunk4]);

    let (_, e1_by_type) = events_by_type(&events[0]);
    let e1_completed = by(&e1_by_type, "response.web_search_call.completed");
    assert!(
        e1_completed.is_empty(),
        "did not expect web_search_call.completed on queries/sources frame, events={e1_completed:?}"
    );

    let (_, e2_by_type) = events_by_type(&events[1]);
    assert!(
        !by(&e2_by_type, "response.web_search_call.completed").is_empty(),
        "expected function call to finalize web_search_call"
    );
    let Some(early_done) = find_web_search_call_done(by(&e2_by_type, "response.output_item.done"))
    else {
        panic!("expected web_search_call output_item.done when function call arrives");
    };
    let early_sources = list(early_done, "item.action.sources");
    assert!(
        early_sources.len() == 1 && text(&early_sources[0], "url") == "https://example.com/alpha",
        "expected early-finalized search to contain only the first source, got: {early_done}"
    );

    let all_events = events.concat();
    let (_, by_type) = events_by_type(&all_events);
    let Some(completed_json) = by(&by_type, "response.completed").first() else {
        panic!("expected response.completed event");
    };

    let Some(ws_item) = first_of_type(output(completed_json), "web_search_call") else {
        panic!("expected web_search_call item, got: {completed_json}");
    };

    let sources = list(ws_item, "action.sources");
    assert_eq!(
        sources.len(),
        2,
        "expected 2 sources in completed web_search_call after late grounding, got {}: {ws_item}",
        sources.len()
    );
    let got_urls: Vec<String> = sources.iter().map(|src| text(src, "url")).collect();
    assert!(
        got_urls
            .iter()
            .any(|url| url == "https://example.com/alpha")
            && got_urls.iter().any(|url| url == "https://example.com/beta"),
        "expected both alpha and beta sources in completed output, got: {ws_item}"
    );

    let got_queries: Vec<String> = list(ws_item, "action.queries").iter().map(string).collect();
    assert!(
        got_queries.iter().any(|query| query == "query alpha")
            && got_queries.iter().any(|query| query == "query beta"),
        "expected both alpha and beta queries in completed output, got: {ws_item}"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_citations_span_across_messages() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test query",
        "tools": [{"type": "web_search"}]
    });

    // Chunk 1: message 0 part
    let chunk1 = r#"data: {
        "responseId": "stream_span_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Hello ", "partIndex": 0}]
            }
        }]
    }"#;

    // Chunk 2: intervening function call forces message 0 to close
    let chunk2 = r#"data: {
        "responseId": "stream_span_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"functionCall": {"name": "search", "args": {}}, "partIndex": 1}]
            }
        }]
    }"#;

    // Chunk 3: message 1 continues part 0
    let chunk3 = r#"data: {
        "responseId": "stream_span_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "world", "partIndex": 0}]
            }
        }]
    }"#;

    // Chunk 4: grounding metadata covering partIndex 0 [0, 11)
    let chunk4 = r#"data: {
        "responseId": "stream_span_1",
        "candidates": [{
            "groundingMetadata": {
                "webSearchQueries": ["span query"],
                "groundingChunks": [{"web": {"uri": "https://example.com/span", "title": "Span"}}],
                "groundingSupports": [{
                    "groundingChunkIndices": [0],
                    "segment": {
                        "partIndex": 0,
                        "startIndex": 0,
                        "endIndex": 11,
                        "text": "Hello world"
                    }
                }]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3, chunk4]).concat();
    let completed_json = completed(&all_events);

    let messages = of_type(output(&completed_json), "message");
    assert_eq!(
        messages.len(),
        2,
        "expected 2 messages, got {}: {completed_json}",
        messages.len()
    );

    let c0 = list(messages[0], "content.0.annotations");
    assert_eq!(
        c0.len(),
        1,
        "expected 1 citation in message 0, got {}: {}",
        c0.len(),
        messages[0]
    );
    assert_eq!(span(&c0[0]), (0, 6), "expected [0, 6) in message 0");
    assert_eq!(
        text(&c0[0], "url"),
        "https://example.com/span",
        "expected url https://example.com/span"
    );

    let c1 = list(messages[1], "content.0.annotations");
    assert_eq!(
        c1.len(),
        1,
        "expected 1 citation in message 1, got {}: {}",
        c1.len(),
        messages[1]
    );
    assert_eq!(span(&c1[0]), (0, 5), "expected [0, 5) in message 1");
    assert_eq!(
        text(&c1[0], "url"),
        "https://example.com/span",
        "expected url https://example.com/span"
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_supports_arrive_first_then_duplicate_chunks_deduplicate()
 {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test dup search",
        "tools": [{"type": "web_search"}]
    });

    // Frame 1: text content
    let chunk1 = r#"data: {
        "responseId": "stream_dup_dedup_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Hello world."}],
                "role": "model"
            }
        }]
    }"#;

    // Frame 2: supports arrive referencing index [1] before chunks
    let chunk2 = r#"data: {
        "responseId": "stream_dup_dedup_1",
        "candidates": [{
            "groundingMetadata": {
                "groundingSupports": [
                    {
                        "segment": {"startIndex": 0, "endIndex": 11, "partIndex": 0},
                        "groundingChunkIndices": [1]
                    }
                ]
            }
        }]
    }"#;

    // Frame 3: duplicate URL chunks arrive and get deduplicated to index 0
    let chunk3 = r#"data: {
        "responseId": "stream_dup_dedup_1",
        "candidates": [{
            "groundingMetadata": {
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/dup", "title": "Dup 1"}},
                    {"web": {"uri": "https://example.com/dup", "title": "Dup 2"}}
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3]).concat();
    let completed_json = completed(&all_events);

    let Some(msg_item) = first_of_type(output(&completed_json), "message") else {
        panic!("expected message output item, got: {completed_json}");
    };

    let citations = list(msg_item, "content.0.annotations");
    assert_eq!(
        citations.len(),
        1,
        "expected 1 citation from remapped support, got {}: {msg_item}",
        citations.len()
    );
    assert!(
        text(&citations[0], "url") == "https://example.com/dup" && span(&citations[0]) == (0, 11),
        "unexpected citation: {}",
        citations[0]
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_signature_boundary_continuation_text_chunks()
{
    // No tools are declared, so web search support of the model does not
    // matter; the upstream model ID is kept.
    let model_id = "gemini-sig-boundary-continuation";

    let req = json!({
        "model": "gemini-sig-boundary-continuation",
        "input": "test query"
    });

    // Frame 1: signed text chunk A (partIndex=0)
    let chunk1 = r#"data: {
        "responseId": "stream_sbc_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "Hello ", "thoughtSignature": "SIGNATURE", "partIndex": 0}]
            }
        }]
    }"#
    .replace("SIGNATURE", GEMINI_SIGNATURE);

    // Frame 2: unsigned text chunk B (partIndex=1). Pending signature logic finalizes message 0.
    let chunk2 = r#"data: {
        "responseId": "stream_sbc_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "beautiful ", "partIndex": 1}]
            }
        }]
    }"#;

    // Frame 3: continuation text chunk C (without explicit partIndex)
    let chunk3 = r#"data: {
        "responseId": "stream_sbc_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "world"}]
            }
        }]
    }"#;

    // Frame 4: grounding metadata referencing partIndex 1 across both B and C
    let chunk4 = r#"data: {
        "responseId": "stream_sbc_1",
        "candidates": [{
            "groundingMetadata": {
                "webSearchQueries": ["world query"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/world", "title": "World"}}
                ],
                "groundingSupports": [
                    {
                        "groundingChunkIndices": [0],
                        "segment": {
                            "partIndex": 1,
                            "startIndex": 0,
                            "endIndex": 15,
                            "text": "beautiful world"
                        }
                    }
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let all_events = stream(model_id, &req, &[chunk1.as_str(), chunk2, chunk3, chunk4]).concat();
    let completed_json = completed(&all_events);

    let messages = of_type(output(&completed_json), "message");
    assert_eq!(
        messages.len(),
        2,
        "expected 2 messages, got {}: {completed_json}",
        messages.len()
    );

    assert_eq!(
        text(messages[0], "content.0.text"),
        "Hello ",
        "expected message 0 text 'Hello '"
    );
    assert_eq!(
        text(messages[1], "content.0.text"),
        "beautiful world",
        "expected message 1 text 'beautiful world'"
    );

    let citations = list(messages[1], "content.0.annotations");
    assert_eq!(
        citations.len(),
        1,
        "expected 1 citation in message 1, got {}: {}",
        citations.len(),
        messages[1]
    );
    assert_eq!(
        text(&citations[0], "url"),
        "https://example.com/world",
        "expected url https://example.com/world"
    );
    assert_eq!(span(&citations[0]), (0, 15), "expected span [0, 15)");
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_frame2_duplicate_chunks_then_frame3_supports()
{
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test dup search separate frames",
        "tools": [{"type": "web_search"}]
    });

    // Frame 1: text content + webSearchQueries
    let chunk1 = r#"data: {
        "responseId": "stream_f2_f3_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Alpha Beta."}],
                "role": "model"
            },
            "groundingMetadata": {
                "webSearchQueries": ["test query"]
            }
        }]
    }"#;

    // Frame 2: duplicate chunks [A, A, B] arrive without supports
    let chunk2 = r#"data: {
        "responseId": "stream_f2_f3_1",
        "candidates": [{
            "groundingMetadata": {
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/A", "title": "Title A1"}},
                    {"web": {"uri": "https://example.com/A", "title": "Title A2"}},
                    {"web": {"uri": "https://example.com/B", "title": "Title B"}}
                ]
            }
        }]
    }"#;

    // Frame 3: supports arrive with index [2] (referencing B) without chunks
    let chunk3 = r#"data: {
        "responseId": "stream_f2_f3_1",
        "candidates": [{
            "groundingMetadata": {
                "groundingSupports": [
                    {
                        "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                        "groundingChunkIndices": [2]
                    }
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3]).concat();
    let completed_json = completed(&all_events);

    let Some(msg_item) = first_of_type(output(&completed_json), "message") else {
        panic!("expected message output item, got: {completed_json}");
    };

    let citations = list(msg_item, "content.0.annotations");
    assert_eq!(
        citations.len(),
        1,
        "expected 1 citation from remapped support, got {}: {msg_item}",
        citations.len()
    );
    assert_eq!(
        text(&citations[0], "url"),
        "https://example.com/B",
        "expected citation url https://example.com/B"
    );
    assert_eq!(
        span(&citations[0]),
        (0, 5),
        "unexpected citation range: {}",
        citations[0]
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_pending_support_resolves_to_cumulative_raw_chunk()
 {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test pending support cumulative resolution",
        "tools": [{"type": "web_search"}]
    });

    // Frame 1: text + groundingChunks=[A] + support pointing to upcoming chunk [1]
    let chunk1 = r#"data: {
        "responseId": "stream_pend_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Alpha Beta."}],
                "role": "model"
            },
            "groundingMetadata": {
                "webSearchQueries": ["test query"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/A", "title": "Chunk A"}}
                ],
                "groundingSupports": [
                    {
                        "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                        "groundingChunkIndices": [1]
                    }
                ]
            }
        }]
    }"#;

    // Frame 2: subsequent frame brings chunks [B, C]
    let chunk2 = r#"data: {
        "responseId": "stream_pend_1",
        "candidates": [{
            "groundingMetadata": {
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/B", "title": "Chunk B"}},
                    {"web": {"uri": "https://example.com/C", "title": "Chunk C"}}
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2]).concat();
    let completed_json = completed(&all_events);

    let Some(msg_item) = first_of_type(output(&completed_json), "message") else {
        panic!("expected message output item, got: {completed_json}");
    };

    let citations = list(msg_item, "content.0.annotations");
    assert_eq!(
        citations.len(),
        1,
        "expected 1 citation from remapped support, got {}: {msg_item}",
        citations.len()
    );
    // Must resolve to Chunk B (raw chunk 1 in cumulative stream), NOT Chunk C (local chunk 1 in frame 2)
    assert_eq!(
        text(&citations[0], "url"),
        "https://example.com/B",
        "expected citation url https://example.com/B"
    );
    assert_eq!(
        span(&citations[0]),
        (0, 5),
        "unexpected citation range: {}",
        citations[0]
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_deduplicated_chunks_resolve_frame2_cumulative_support()
 {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test dedup cumulative resolution",
        "tools": [{"type": "web_search"}]
    });

    // Frame 1: text + groundingChunks=[A, A] (deduplicated to [A])
    let chunk1 = r#"data: {
        "responseId": "stream_dedup_f2_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Alpha."}],
                "role": "model"
            },
            "groundingMetadata": {
                "webSearchQueries": ["test query"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/A", "title": "Chunk A"}},
                    {"web": {"uri": "https://example.com/A", "title": "Chunk A"}}
                ]
            }
        }]
    }"#;

    // Frame 2: subsequent frame brings chunks [B] and new support referencing raw chunk [1]
    let chunk2 = r#"data: {
        "responseId": "stream_dedup_f2_1",
        "candidates": [{
            "groundingMetadata": {
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/B", "title": "Chunk B"}}
                ],
                "groundingSupports": [
                    {
                        "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                        "groundingChunkIndices": [1]
                    }
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2]).concat();
    let completed_json = completed(&all_events);

    let Some(msg_item) = first_of_type(output(&completed_json), "message") else {
        panic!("expected message output item, got: {completed_json}");
    };

    let citations = list(msg_item, "content.0.annotations");
    assert_eq!(
        citations.len(),
        1,
        "expected 1 citation, got {}: {msg_item}",
        citations.len()
    );
    // Must resolve to Chunk A (raw chunk 1 in cumulative stream), NOT Chunk B (local chunk 0 in frame 2)
    assert_eq!(
        text(&citations[0], "url"),
        "https://example.com/A",
        "expected citation url https://example.com/A"
    );
    assert_eq!(
        span(&citations[0]),
        (0, 5),
        "unexpected citation range: {}",
        citations[0]
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_deduplicated_frame1_chunks_frame2_chunks_and_support_resolves_to_frame1()
 {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test dedup cumulative resolution with multiple frame 2 chunks",
        "tools": [{"type": "web_search"}]
    });

    // Frame 1: text + groundingChunks=[A, A] (deduplicated to [A], raw count 2, cumulativeRemap {0: 0, 1: 0})
    let chunk1 = r#"data: {
        "responseId": "stream_dedup_f1_f2_1",
        "candidates": [{
            "content": {
                "parts": [{"text": "Alpha."}],
                "role": "model"
            },
            "groundingMetadata": {
                "webSearchQueries": ["test query"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/A", "title": "Chunk A"}},
                    {"web": {"uri": "https://example.com/A", "title": "Chunk A"}}
                ]
            }
        }]
    }"#;

    // Frame 2: subsequent frame brings chunks [B, C] and support referencing cumulative raw chunk [1]
    let chunk2 = r#"data: {
        "responseId": "stream_dedup_f1_f2_1",
        "candidates": [{
            "groundingMetadata": {
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/B", "title": "Chunk B"}},
                    {"web": {"uri": "https://example.com/C", "title": "Chunk C"}}
                ],
                "groundingSupports": [
                    {
                        "segment": {"startIndex": 0, "endIndex": 5, "partIndex": 0},
                        "groundingChunkIndices": [1]
                    }
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2]).concat();
    let completed_json = completed(&all_events);

    let Some(msg_item) = first_of_type(output(&completed_json), "message") else {
        panic!("expected message output item, got: {completed_json}");
    };

    let citations = list(msg_item, "content.0.annotations");
    assert_eq!(
        citations.len(),
        1,
        "expected 1 citation, got {}: {msg_item}",
        citations.len()
    );
    // Must resolve to Chunk A (raw chunk 1 in cumulative stream), NOT Chunk C (which would happen if 1 hit Frame 2's local index)
    assert_eq!(
        text(&citations[0], "url"),
        "https://example.com/A",
        "expected citation url https://example.com/A"
    );
    assert_eq!(
        span(&citations[0]),
        (0, 5),
        "unexpected citation range: {}",
        citations[0]
    );
}

#[test]
fn convert_gemini_response_to_openai_responses_non_web_grounding_no_web_search_call_or_usage() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "Summarize company policy",
        "tools": [{"type": "web_search"}]
    });

    // stream
    {
        let chunk1 = r#"data: {
            "responseId": "stream_rag_test_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "According to the internal policy document, vacation is 20 days."}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_rag_test_1",
            "candidates": [{
                "groundingMetadata": {
                    "groundingChunks": [
                        {
                            "retrievedContext": {
                                "uri": "policy-doc-42",
                                "title": "Employee Handbook"
                            }
                        }
                    ],
                    "groundingSupports": [
                        {
                            "groundingChunkIndices": [0],
                            "segment": {
                                "partIndex": 0,
                                "startIndex": 0,
                                "endIndex": 40,
                                "text": "According to the internal policy document"
                            }
                        }
                    ]
                }
            }]
        }"#;

        let chunk3 = r#"data: {
            "responseId": "stream_rag_test_1",
            "candidates": [{
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 16, "totalTokenCount": 24}
        }"#;

        let outputs = translate_chunks(SEARCH_MODEL, &req, &req, &[chunk1, chunk2, chunk3]);

        let mut completed_json = None;
        for out in &outputs {
            for frame in out.split("\n\n").filter(|frame| !frame.trim().is_empty()) {
                assert!(
                    !frame.contains("web_search_call"),
                    "did not expect web_search_call for non-web grounding, got event: {frame}"
                );
            }
            let events = sse_events(out);
            if let Some((_, data)) = events
                .iter()
                .rev()
                .find(|(name, _)| name == "response.completed")
            {
                completed_json = Some(data.clone());
            }
        }

        let Some(completed_json) = completed_json else {
            panic!("expected response.completed event");
        };

        assert!(
            at(&completed_json, "response.tool_usage.web_search").is_none(),
            "did not expect tool_usage.web_search for non-web grounding: {completed_json}"
        );

        assert!(
            first_of_type(output(&completed_json), "web_search_call").is_none(),
            "expected no web_search_call in response.output for non-web grounding: {completed_json}"
        );
    }

    // non_stream
    {
        let resp_json = r#"{
            "responseId": "nonstream_rag_test_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "According to the internal policy document, vacation is 20 days."}]
                },
                "groundingMetadata": {
                    "groundingChunks": [
                        {
                            "retrievedContext": {
                                "uri": "policy-doc-42",
                                "title": "Employee Handbook"
                            }
                        }
                    ],
                    "groundingSupports": [
                        {
                            "groundingChunkIndices": [0],
                            "segment": {
                                "partIndex": 0,
                                "startIndex": 0,
                                "endIndex": 40,
                                "text": "According to the internal policy document"
                            }
                        }
                    ]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 16, "totalTokenCount": 24}
        }"#;

        let out_parsed = convert_gemini_response_to_openai_responses_non_stream(
            &req,
            &req,
            resp_json.as_bytes(),
        )
        .unwrap_or_default();

        assert!(
            at(&out_parsed, "tool_usage.web_search").is_none(),
            "did not expect tool_usage.web_search in non-stream response for non-web grounding: {out_parsed}"
        );

        assert!(
            first_of_type(list(&out_parsed, "output"), "web_search_call").is_none(),
            "expected no web_search_call in non-stream output for non-web grounding: {out_parsed}"
        );
    }
}

/// Each `response.output_text.annotation.added` event's data, and the last
/// `response.completed` event's data.
fn annotation_events(events: &Events) -> (Vec<&Value>, Value) {
    let added = events
        .iter()
        .filter(|(name, _)| name == "response.output_text.annotation.added")
        .map(|(_, data)| data)
        .collect();
    (added, completed(events))
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_late_citations_annotation_added() {
    // message closed early by function call followed by late grounding metadata
    {
        let req = json!({
            "model": SEARCH_MODEL,
            "input": "What is the weather in Paris?",
            "tools": [{"type": "web_search"}, {"type": "function", "function": {"name": "get_weather"}}]
        });

        let chunk1 = r#"data: {
            "responseId": "stream_lc_fc_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Paris is sunny today."}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_lc_fc_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}]
                }
            }]
        }"#;

        let chunk3 = r#"data: {
            "responseId": "stream_lc_fc_1",
            "candidates": [{
                "groundingMetadata": {
                    "webSearchQueries": ["Paris weather today"],
                    "groundingChunks": [
                        {"web": {"uri": "https://weather.example.com/paris", "title": "Paris Weather"}}
                    ],
                    "groundingSupports": [
                        {
                            "groundingChunkIndices": [0],
                            "segment": {
                                "partIndex": 0,
                                "startIndex": 0,
                                "endIndex": 5,
                                "text": "Paris"
                            }
                        }
                    ]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 15, "totalTokenCount": 25}
        }"#;

        let events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3]);

        // Verify chunk 2 closed message 0 without citations
        let found_msg_done_without_citations = events[1].iter().any(|(name, data)| {
            name == "response.content_part.done" && list(data, "part.annotations").is_empty()
        });
        assert!(
            found_msg_done_without_citations,
            "expected message 0 to be closed without citations on chunk 2"
        );

        let all_events = events.concat();
        let event_sequence = names(&all_events);
        let (annotation_added_events, completed_json) = annotation_events(&all_events);

        assert_eq!(
            annotation_added_events.len(),
            1,
            "expected 1 response.output_text.annotation.added event, got {}. All events: {event_sequence:?}",
            annotation_added_events.len()
        );

        let ann_ev = annotation_added_events[0];
        assert_eq!(
            text(ann_ev, "type"),
            "response.output_text.annotation.added",
            "unexpected event type"
        );
        assert_eq!(int(ann_ev, "output_index"), 0, "expected output_index 0");
        assert_eq!(int(ann_ev, "content_index"), 0, "expected content_index 0");
        assert_eq!(
            int(ann_ev, "annotation_index"),
            0,
            "expected annotation_index 0"
        );
        assert!(
            !text(ann_ev, "response_id").is_empty(),
            "expected non-empty response_id"
        );

        let ann = at(ann_ev, "annotation").unwrap_or(&Value::Null);
        assert_eq!(
            text(ann, "type"),
            "url_citation",
            "expected annotation type url_citation"
        );
        assert_eq!(
            text(ann, "url"),
            "https://weather.example.com/paris",
            "expected url https://weather.example.com/paris"
        );
        assert_eq!(
            text(ann, "title"),
            "Paris Weather",
            "expected title Paris Weather"
        );
        assert_eq!(span(ann), (0, 5), "expected span [0, 5)");

        // Verify event sequence: response.output_text.annotation.added must be emitted before response.completed
        let ann_idx = event_sequence
            .iter()
            .position(|name| *name == "response.output_text.annotation.added");
        let completed_idx = event_sequence
            .iter()
            .position(|name| *name == "response.completed");
        assert!(
            matches!((ann_idx, completed_idx), (Some(ann_idx), Some(completed_idx)) if ann_idx < completed_idx),
            "expected response.output_text.annotation.added (idx={ann_idx:?}) before response.completed (idx={completed_idx:?}), events={event_sequence:?}"
        );

        // Verify that response.completed.response.output[0] annotations match the emitted annotation
        let msg_output = at(&completed_json, "response.output.0").unwrap_or(&Value::Null);
        assert_eq!(
            text(msg_output, "type"),
            "message",
            "expected output 0 to be message: {completed_json}"
        );
        assert!(
            !text(ann_ev, "item_id").is_empty(),
            "expected non-empty item_id in response.output_text.annotation.added"
        );
        assert_eq!(
            text(ann_ev, "item_id"),
            text(msg_output, "id"),
            "expected item_id to be the message id"
        );
        let msg_citations = list(msg_output, "content.0.annotations");
        assert_eq!(
            msg_citations.len(),
            1,
            "expected 1 citation in completed message 0, got {}: {msg_output}",
            msg_citations.len()
        );
        assert_eq!(
            text(&msg_citations[0], "url"),
            text(ann, "url"),
            "citation url mismatch: completed vs emitted"
        );
        assert_eq!(
            span(&msg_citations[0]),
            span(ann),
            "citation range mismatch"
        );
    }

    // message closed early by thought boundary followed by multiple late citations
    {
        let req = json!({
            "model": SEARCH_MODEL,
            "input": "Provide details on Alpha and Beta",
            "tools": [{"type": "web_search"}]
        });

        let chunk1 = r#"data: {
            "responseId": "stream_lc_tb_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Alpha is active. Beta is ready."}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_lc_tb_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"thought": true, "text": "Analyzing downstream steps..."}]
                }
            }]
        }"#;

        let chunk3 = r#"data: {
            "responseId": "stream_lc_tb_1",
            "candidates": [{
                "groundingMetadata": {
                    "webSearchQueries": ["Alpha and Beta status"],
                    "groundingChunks": [
                        {"web": {"uri": "https://example.com/alpha", "title": "Alpha Doc"}},
                        {"web": {"uri": "https://example.com/beta", "title": "Beta Doc"}}
                    ],
                    "groundingSupports": [
                        {
                            "groundingChunkIndices": [0],
                            "segment": {
                                "partIndex": 0,
                                "startIndex": 0,
                                "endIndex": 5,
                                "text": "Alpha"
                            }
                        },
                        {
                            "groundingChunkIndices": [1],
                            "segment": {
                                "partIndex": 0,
                                "startIndex": 17,
                                "endIndex": 21,
                                "text": "Beta"
                            }
                        }
                    ]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 20, "totalTokenCount": 30}
        }"#;

        let all_events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3]).concat();
        let event_sequence = names(&all_events);
        let (annotation_added_events, completed_json) = annotation_events(&all_events);

        assert_eq!(
            annotation_added_events.len(),
            2,
            "expected 2 response.output_text.annotation.added events, got {}. All events: {event_sequence:?}",
            annotation_added_events.len()
        );

        // Verify event 0
        let ev0 = annotation_added_events[0];
        assert_eq!(
            int(ev0, "annotation_index"),
            0,
            "expected annotation_index 0 for first event"
        );
        assert_eq!(int(ev0, "output_index"), 0, "expected output_index 0");
        assert_eq!(
            text(ev0, "annotation.url"),
            "https://example.com/alpha",
            "expected url https://example.com/alpha"
        );

        // Verify event 1
        let ev1 = annotation_added_events[1];
        assert_eq!(
            int(ev1, "annotation_index"),
            1,
            "expected annotation_index 1 for second event"
        );
        assert_eq!(int(ev1, "output_index"), 0, "expected output_index 0");
        assert_eq!(
            text(ev1, "annotation.url"),
            "https://example.com/beta",
            "expected url https://example.com/beta"
        );

        // Verify output in completed JSON matches the two annotations
        let Some(msg_output) = first_of_type(output(&completed_json), "message") else {
            panic!("expected message output in completed: {completed_json}");
        };
        let msg_id = text(msg_output, "id");
        assert!(
            !text(ev0, "item_id").is_empty() && text(ev0, "item_id") == msg_id,
            "expected item_id {msg_id:?} for first event, got {:?}",
            text(ev0, "item_id")
        );
        assert!(
            !text(ev1, "item_id").is_empty() && text(ev1, "item_id") == msg_id,
            "expected item_id {msg_id:?} for second event, got {:?}",
            text(ev1, "item_id")
        );
        let citations = list(msg_output, "content.0.annotations");
        assert_eq!(
            citations.len(),
            2,
            "expected 2 citations in completed message, got {}: {msg_output}",
            citations.len()
        );
        assert!(
            text(&citations[0], "url") == "https://example.com/alpha"
                && text(&citations[1], "url") == "https://example.com/beta",
            "citations array does not match emitted events: {msg_output}"
        );
    }
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_web_search_buffering_signature_boundary() {
    let req = json!({
        "model": SEARCH_MODEL,
        "input": "test search query",
        "tools": [{"type": "web_search"}]
    });

    // Frame 1: signed text chunk A
    let chunk1 = r#"data: {
        "responseId": "stream_sb_buf_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "signed chunk A", "thoughtSignature": "SIGNATURE"}]
            }
        }]
    }"#
    .replace("SIGNATURE", GEMINI_SIGNATURE);

    // Frame 2: unsigned text chunk B
    let chunk2 = r#"data: {
        "responseId": "stream_sb_buf_1",
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"text": "unsigned chunk B"}]
            }
        }]
    }"#;

    // Frame 3: grounding metadata
    let chunk3 = r#"data: {
        "responseId": "stream_sb_buf_1",
        "candidates": [{
            "groundingMetadata": {
                "webSearchQueries": ["test search query"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/info", "title": "Example Info"}}
                ],
                "groundingSupports": [
                    {
                        "groundingChunkIndices": [0],
                        "segment": {
                            "startIndex": 0,
                            "endIndex": 16,
                            "text": "unsigned chunk B"
                        }
                    }
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 20, "totalTokenCount": 30}
    }"#;

    let all_events = stream(SEARCH_MODEL, &req, &[chunk1.as_str(), chunk2, chunk3]).concat();
    let completed_json = completed(&all_events);

    let messages = of_type(output(&completed_json), "message");
    assert_eq!(
        messages.len(),
        2,
        "expected 2 messages, got {}: {completed_json}",
        messages.len()
    );

    assert_eq!(
        text(messages[0], "content.0.text"),
        "signed chunk A",
        "expected message 0 text 'signed chunk A'"
    );
    assert_eq!(
        text(messages[1], "content.0.text"),
        "unsigned chunk B",
        "expected message 1 text 'unsigned chunk B'"
    );

    // Verify that chunk A message has the thought signature, and chunk B message does not.
    // Translate the completed response outputs back to Gemini request to verify signature binding.
    let replay_req = json!({
        "model": SEARCH_MODEL,
        "input": at(&completed_json, "response.output").cloned().unwrap_or(Value::Null)
    });
    let translated = convert_openai_responses_request_to_gemini(SEARCH_MODEL, &replay_req, false);

    let visible_parts: Vec<&Value> = list(&translated, "contents.0.parts")
        .iter()
        .filter(|part| !part.get("thought").is_some_and(bool_of) && !text(part, "text").is_empty())
        .collect();

    assert_eq!(
        visible_parts.len(),
        2,
        "expected 2 visible parts on replay, got {}: {translated}",
        visible_parts.len()
    );
    assert!(
        text(visible_parts[0], "text") == "signed chunk A"
            && text(visible_parts[0], "thoughtSignature") == GEMINI_SIGNATURE,
        "expected chunk A to have thought signature {GEMINI_SIGNATURE:?}, got text={:?} sig={:?}",
        text(visible_parts[0], "text"),
        text(visible_parts[0], "thoughtSignature")
    );
    assert!(
        text(visible_parts[1], "text") == "unsigned chunk B"
            && text(visible_parts[1], "thoughtSignature").is_empty(),
        "expected chunk B to have no thought signature, got text={:?} sig={:?}",
        text(visible_parts[1], "text"),
        text(visible_parts[1], "thoughtSignature")
    );
}

/// `assertAnnotationMatches` from
/// `TestConvertGeminiResponseToOpenAIResponsesStream_CitationAnnotationAddedTimings`:
/// one `response.output_text.annotation.added` event with the given
/// citation, for the message, emitted before `response.completed` and
/// before the message's `output_item.done`, or after it when `late`.
fn assert_annotation_matches(
    events: &Events,
    want_url: &str,
    want_title: &str,
    (want_start, want_end): (i64, i64),
    late: bool,
) {
    let (event_types, by_type) = events_by_type(events);
    let ann_events = by(&by_type, "response.output_text.annotation.added");
    assert_eq!(
        ann_events.len(),
        1,
        "expected 1 response.output_text.annotation.added, got {}. events={event_types:?}",
        ann_events.len()
    );
    let ann_ev = &ann_events[0];
    assert_eq!(
        text(ann_ev, "type"),
        "response.output_text.annotation.added",
        "unexpected event type"
    );
    assert!(
        !text(ann_ev, "response_id").is_empty(),
        "expected non-empty response_id"
    );
    assert!(
        !text(ann_ev, "item_id").is_empty(),
        "expected non-empty item_id"
    );
    assert_eq!(int(ann_ev, "content_index"), 0, "expected content_index 0");
    assert_eq!(
        int(ann_ev, "annotation_index"),
        0,
        "expected annotation_index 0"
    );
    let ann = at(ann_ev, "annotation").unwrap_or(&Value::Null);
    assert_eq!(
        text(ann, "type"),
        "url_citation",
        "expected annotation type url_citation"
    );
    assert_eq!(text(ann, "url"), want_url, "expected url {want_url:?}");
    assert_eq!(
        text(ann, "title"),
        want_title,
        "expected title {want_title:?}"
    );
    assert_eq!(
        span(ann),
        (want_start, want_end),
        "expected span [{want_start}, {want_end})"
    );

    let Some(msg_done) = find_message_output_item_done(by(&by_type, "response.output_item.done"))
    else {
        panic!("expected message output_item.done");
    };
    assert_eq!(
        text(ann_ev, "item_id"),
        text(msg_done, "item.id"),
        "annotation item_id != message id"
    );
    assert_eq!(
        int(ann_ev, "output_index"),
        int(msg_done, "output_index"),
        "annotation output_index != message output_index"
    );
    if !late {
        let msg_cites = list(msg_done, "item.content.0.annotations");
        assert!(
            msg_cites.len() == 1 && text(&msg_cites[0], "url") == want_url,
            "message output_item.done annotations mismatch: {msg_done}"
        );
    }

    let Some(completed_json) = by(&by_type, "response.completed").first() else {
        panic!("expected response.completed");
    };
    let Some(completed_msg) = first_of_type(output(completed_json), "message") else {
        panic!("expected message in response.completed: {completed_json}");
    };
    let completed_cites = list(completed_msg, "content.0.annotations");
    assert!(
        completed_cites.len() == 1 && text(&completed_cites[0], "url") == want_url,
        "completed message annotations mismatch: {completed_msg}"
    );

    let ann_idx = event_types
        .iter()
        .position(|name| name == "response.output_text.annotation.added");
    let completed_idx = event_types
        .iter()
        .position(|name| name == "response.completed");
    let msg_done_event_idx = events.iter().position(|(name, data)| {
        name == "response.output_item.done" && text(data, "item.type") == "message"
    });
    let Some(ann_idx) = ann_idx.filter(|&ann_idx| completed_idx.is_some_and(|idx| ann_idx < idx))
    else {
        panic!(
            "expected annotation.added (idx={ann_idx:?}) before response.completed (idx={completed_idx:?}), events={event_types:?}"
        );
    };
    if late {
        assert!(
            msg_done_event_idx.is_some_and(|idx| ann_idx > idx),
            "expected late annotation.added (idx={ann_idx}) after message output_item.done (idx={msg_done_event_idx:?}), events={event_types:?}"
        );
    } else {
        assert!(
            msg_done_event_idx.is_some_and(|idx| ann_idx < idx),
            "expected annotation.added (idx={ann_idx}) before message output_item.done (idx={msg_done_event_idx:?}), events={event_types:?}"
        );
    }
}

#[test]
fn convert_gemini_response_to_openai_responses_stream_citation_annotation_added_timings() {
    // early metadata before message closes
    {
        let req = json!({
            "model": SEARCH_MODEL,
            "input": "search query",
            "tools": [{"type": "web_search"}]
        });

        let chunk1 = r#"data: {
            "responseId": "stream_ann_early_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello world."}]
                },
                "groundingMetadata": {
                    "webSearchQueries": ["hello query"],
                    "groundingChunks": [{"web": {"uri": "https://example.com/hello", "title": "Hello"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {"partIndex": 0, "startIndex": 0, "endIndex": 5, "text": "Hello"}
                    }]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_ann_early_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": " More."}]
                }
            }]
        }"#;

        let chunk3 = r#"data: {
            "responseId": "stream_ann_early_1",
            "candidates": [{"finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15}
        }"#;

        let events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3]).concat();
        assert_annotation_matches(&events, "https://example.com/hello", "Hello", (0, 5), false);
    }

    // same-frame metadata with message close
    {
        let req = json!({
            "model": SEARCH_MODEL,
            "input": "search query",
            "tools": [{"type": "web_search"}]
        });

        let chunk1 = r#"data: {
            "responseId": "stream_ann_same_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello world."}]
                },
                "groundingMetadata": {
                    "webSearchQueries": ["hello query"],
                    "groundingChunks": [{"web": {"uri": "https://example.com/hello", "title": "Hello"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {"partIndex": 0, "startIndex": 0, "endIndex": 5, "text": "Hello"}
                    }]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 8, "totalTokenCount": 13}
        }"#;

        let events = stream(SEARCH_MODEL, &req, &[chunk1]).concat();
        assert_annotation_matches(&events, "https://example.com/hello", "Hello", (0, 5), false);
    }

    // late metadata after message closes
    {
        let req = json!({
            "model": SEARCH_MODEL,
            "input": "What is the weather in Paris?",
            "tools": [{"type": "web_search"}, {"type": "function", "function": {"name": "get_weather"}}]
        });

        let chunk1 = r#"data: {
            "responseId": "stream_ann_late_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Paris is sunny today."}]
                }
            }]
        }"#;

        let chunk2 = r#"data: {
            "responseId": "stream_ann_late_1",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}]
                }
            }]
        }"#;

        let chunk3 = r#"data: {
            "responseId": "stream_ann_late_1",
            "candidates": [{
                "groundingMetadata": {
                    "webSearchQueries": ["Paris weather today"],
                    "groundingChunks": [{"web": {"uri": "https://weather.example.com/paris", "title": "Paris Weather"}}],
                    "groundingSupports": [{
                        "groundingChunkIndices": [0],
                        "segment": {"partIndex": 0, "startIndex": 0, "endIndex": 5, "text": "Paris"}
                    }]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 15, "totalTokenCount": 25}
        }"#;

        let events = stream(SEARCH_MODEL, &req, &[chunk1, chunk2, chunk3]).concat();
        assert_annotation_matches(
            &events,
            "https://weather.example.com/paris",
            "Paris Weather",
            (0, 5),
            true,
        );
    }
}
