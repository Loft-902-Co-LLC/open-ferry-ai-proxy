// Ported from CLIProxyAPI internal/translator/claude/openai/responses/ (v8.0.15, MIT):
// - claude_openai-responses_server_tool_test.go → `server_tool`
// - claude_openai-responses_reasoning_order_test.go → `reasoning_order`
// - claude_openai-responses_citations_test.go → `citations`
// - claude_openai-responses_interleaved_search_test.go → `interleaved_search`
// - claude_openai_responses_compat_test.go → `compat`
// - noop_optimization_test.go → `noop_optimization`
// - claude_openai-responses_pause_test.go → `pause`
// - claude_openai_native_response_test.go → `native`
// https://github.com/router-for-me/CLIProxyAPI

//! Tests that exercise both directions of the translator, often as a round
//! trip: a Claude stream becomes Responses output items, and those items come
//! back as the next Claude request.

use serde_json::{Value, json};

use super::test_support::*;
use super::*;
use crate::json::{int_of, str_of};
use crate::models::ModelCatalog;

const MODEL: &str = "claude-test";

/// `ConvertOpenAIResponsesRequestToClaude` for a non-streaming request.
fn convert(model: &str, request: &Value) -> Value {
    let (body, err) =
        convert_openai_responses_request_to_claude(model, request, false, ModelCatalog::embedded());
    assert_eq!(err, None, "refused: {body}");
    body
}

/// `ConvertOpenAIResponsesRequestToClaudeWithCompat` for a non-streaming
/// request.
fn convert_with_compat(model: &str, request: &Value) -> Value {
    let (body, err) = convert_openai_responses_request_to_claude_with_compat(
        model,
        request,
        false,
        ModelCatalog::embedded(),
    );
    assert_eq!(err, None, "refused: {body}");
    body
}

/// Replays Responses output items as the input of the next request.
fn replay(items: impl IntoIterator<Item = Value>) -> Value {
    convert(MODEL, &request_from_items(items))
}

/// Looks up a dotted path such as `messages.0.content`, like a plain gjson
/// path.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Object(map) => map.get(key),
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        _ => None,
    })
}

/// The value at `path` as gjson's `String()` would return it.
fn text_at(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// The value at `path` as gjson's `Array()` would return it: an array's items,
/// nothing for a missing or null value, or else the value alone.
fn array_at<'v>(value: &'v Value, path: &str) -> Vec<&'v Value> {
    match at(value, path) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
    }
}

/// gjson's `path.#`: the length of the array at `path`, or 0.
fn count_at(value: &Value, path: &str) -> usize {
    match at(value, path) {
        Some(Value::Array(items)) => items.len(),
        _ => 0,
    }
}

/// The `type` of each block in the array at `path`.
fn types_at(value: &Value, path: &str) -> Vec<String> {
    array_at(value, path)
        .into_iter()
        .map(|block| text_at(block, "type"))
        .collect()
}

/// A `data:` line carrying `event`.
fn data(event: Value) -> String {
    format!("data: {event}")
}

fn message_start(id: &str) -> String {
    data(json!({
        "type": "message_start",
        "message": {"id": id, "usage": {"input_tokens": 1, "output_tokens": 0}},
    }))
}

fn message_stop() -> String {
    data(json!({"type": "message_stop"}))
}

fn block_start(index: u32, block: Value) -> String {
    data(json!({"type": "content_block_start", "index": index, "content_block": block}))
}

fn block_delta(index: u32, delta: Value) -> String {
    data(json!({"type": "content_block_delta", "index": index, "delta": delta}))
}

fn block_stop(index: u32) -> String {
    data(json!({"type": "content_block_stop", "index": index}))
}

fn text_delta(index: u32, text: &str) -> String {
    block_delta(index, json!({"type": "text_delta", "text": text}))
}

/// An `input_json_delta` carrying all of `input` at once.
fn input_json_delta(index: u32, input: Value) -> String {
    block_delta(
        index,
        json!({"type": "input_json_delta", "partial_json": input.to_string()}),
    )
}

/// The start of a Claude `web_search` server tool block.
fn web_search_use_start(index: u32, id: &str) -> String {
    block_start(
        index,
        json!({"type": "server_tool_use", "id": id, "name": "web_search", "input": {}}),
    )
}

/// A whole signed thinking block.
fn thinking_lines(index: u32, text: &str, signature: &str) -> Vec<String> {
    vec![
        block_start(index, json!({"type": "thinking", "thinking": ""})),
        block_delta(index, json!({"type": "thinking_delta", "thinking": text})),
        block_delta(
            index,
            json!({"type": "signature_delta", "signature": signature}),
        ),
        block_stop(index),
    ]
}

/// `translateClaudeResponsesStreamThroughRegistry`: feeds `lines` to a fresh
/// stream with no requests and returns the events it emits.
fn stream(lines: &[String]) -> Vec<(String, Value)> {
    let mut stream = ClaudeToOpenAIResponsesStream::new(MODEL, &Value::Null, &Value::Null);
    lines
        .iter()
        .flat_map(|line| sse_events(&stream.translate_line(line.as_bytes())))
        .collect()
}

/// The data of the last `response.completed` event, or null if none came.
fn completed(events: &[(String, Value)]) -> Value {
    events
        .iter()
        .rev()
        .find(|(name, _)| name == "response.completed")
        .map_or(Value::Null, |(_, data)| data.clone())
}

/// `ConvertClaudeResponseToOpenAIResponsesNonStream` over `lines`, one per
/// line, with no requests.
fn non_stream(lines: &[String]) -> Value {
    convert_claude_response_to_openai_responses_non_stream(
        &Value::Null,
        &Value::Null,
        lines.join("\n").as_bytes(),
    )
}

// Ports claude_openai-responses_server_tool_test.go.
mod server_tool {
    use super::*;

    /// `claudeWebSearchStreamChunks`: one search with two results.
    pub(super) fn web_search_lines() -> Vec<String> {
        vec![
            message_start("msg_ws"),
            web_search_use_start(0, "srvtoolu_1"),
            input_json_delta(0, json!({"query": "lindorm vector"})),
            block_stop(0),
            block_start(
                1,
                json!({
                    "type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_1",
                    "content": [
                        {"type": "web_search_result", "title": "Lindorm Vector", "url": "https://example.com/a", "encrypted_content": "ENC_A", "page_age": "1 day"},
                        {"type": "web_search_result", "title": "Docs", "url": "https://example.com/b", "encrypted_content": "ENC_B"},
                    ],
                }),
            ),
            block_stop(1),
            message_stop(),
        ]
    }

    // Claude server tool blocks become a Responses web_search_call.

    #[test]
    fn web_search_blocks_become_web_search_call_item() {
        let completed = completed(&stream(&web_search_lines()));
        let items = array_at(&completed, "response.output");
        assert_eq!(items.len(), 1, "output: {completed}");
        let item = items[0];
        assert_eq!(text_at(item, "type"), "web_search_call");
        assert_eq!(text_at(item, "action.query"), "lindorm vector");
        assert_eq!(text_at(item, "status"), "completed");
        assert_eq!(count_at(item, "results"), 2, "item: {item}");
        assert_eq!(text_at(item, "results.0.url"), "https://example.com/a");
        // Anthropic validates encrypted_content on replay, so it must survive
        // the hop.
        assert_eq!(text_at(item, "results.0.encrypted_content"), "ENC_A");
    }

    #[test]
    fn web_search_blocks_become_web_search_call_item_non_stream() {
        let out = non_stream(&web_search_lines());
        let items = array_at(&out, "output");
        assert!(
            items.len() == 1 && text_at(items[0], "type") == "web_search_call",
            "output: {out}"
        );
        assert_eq!(text_at(items[0], "action.query"), "lindorm vector");
        assert_eq!(count_at(items[0], "results"), 2);
    }

    // Responses web_search_call items become Claude server tool blocks.

    #[test]
    fn web_search_call_item_replays_as_claude_server_tool_blocks() {
        let out = replay([json!({
            "type": "web_search_call", "id": "ws_srvtoolu_1", "status": "completed",
            "action": {"type": "search", "query": "lindorm vector"},
            "results": [{"title": "Lindorm Vector", "url": "https://example.com/a", "encrypted_content": "ENC_A"}],
        })]);

        assert_eq!(
            assistant_block_types(&out),
            ["server_tool_use", "web_search_tool_result"]
        );
        let use_block = at(&out, "messages.0.content.0").expect("server_tool_use block");
        let result = at(&out, "messages.0.content.1").expect("web_search_tool_result block");
        // The ws_ prefix is stripped.
        assert_eq!(text_at(use_block, "id"), "srvtoolu_1");
        assert_eq!(text_at(use_block, "name"), "web_search");
        assert_eq!(text_at(use_block, "input.query"), "lindorm vector");
        assert_eq!(text_at(result, "tool_use_id"), "srvtoolu_1");
        assert_eq!(text_at(result, "content.0.url"), "https://example.com/a");
        assert_eq!(text_at(result, "content.0.encrypted_content"), "ENC_A");
    }

    // Anthropic rejects a web_search_result without genuine encrypted_content
    // and a server_tool_use with no result block at all, but accepts an empty
    // result list. A client that drops the field must degrade, not break.
    #[test]
    fn web_search_call_without_encrypted_content_replays_empty_results() {
        let out = replay([json!({
            "type": "web_search_call", "id": "ws_srvtoolu_1", "status": "completed",
            "action": {"type": "search", "query": "q"},
            "results": [{"title": "T", "url": "https://example.com/a"}],
        })]);
        assert_eq!(assistant_block_types(&out).len(), 2, "output: {out}");
        assert_eq!(count_at(&out, "messages.0.content.1.content"), 0);
    }

    #[test]
    fn output_text_annotations_replay_as_claude_citations() {
        let out = replay([json!({
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "Answer.", "annotations": [
                {"type": "web_search_result_location", "url": "https://example.com/a", "title": "A", "cited_text": "Answer", "encrypted_index": "IDX_A"},
            ]}],
        })]);
        let block = at(&out, "messages.0.content.0").expect("text block");
        assert_eq!(text_at(block, "type"), "text");
        assert_eq!(count_at(block, "citations"), 1, "block: {block}");
        assert_eq!(text_at(block, "citations.0.url"), "https://example.com/a");
        // encrypted_index is mandatory on replay, so the annotation must ride
        // through verbatim.
        assert_eq!(text_at(block, "citations.0.encrypted_index"), "IDX_A");
    }

    #[test]
    fn annotations_without_encrypted_index_are_not_replayed_as_citations() {
        let out = replay([json!({
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "Answer.", "annotations": [
                {"type": "url_citation", "url": "https://example.com/a", "title": "A"},
            ]}],
        })]);
        assert!(
            at(&out, "messages.0.content.0.citations").is_none(),
            "citations without encrypted_index must be dropped, not sent: {out}"
        );
    }

    #[test]
    fn refusal_part_replays_as_claude_text() {
        let out = replay([json!({
            "type": "message", "role": "assistant",
            "content": [{"type": "refusal", "refusal": "I cannot help with that."}],
        })]);
        let text = match at(&out, "messages.0.content") {
            Some(Value::String(text)) => text.clone(),
            _ => text_at(&out, "messages.0.content.0.text"),
        };
        assert_eq!(text, "I cannot help with that.", "output: {out}");
    }

    // Every reachable Claude block survives a round trip.
    #[test]
    fn round_trip_preserves_reachable_claude_blocks() {
        let signature = test_signature();
        let lines = [
            vec![message_start("msg_rt")],
            thinking_lines(0, "ponder", &signature),
            vec![
                block_start(1, json!({"type": "text", "text": ""})),
                text_delta(1, "Researching."),
                block_stop(1),
                web_search_use_start(2, "srvtoolu_1"),
                input_json_delta(2, json!({"query": "q"})),
                block_stop(2),
                block_start(
                    3,
                    json!({
                        "type": "web_search_tool_result",
                        "tool_use_id": "srvtoolu_1",
                        "content": [{"type": "web_search_result", "title": "T", "url": "https://example.com/a"}],
                    }),
                ),
                block_stop(3),
            ],
            thinking_lines(4, "more", &signature),
            vec![
                block_start(
                    5,
                    json!({"type": "tool_use", "id": "toolu_1", "name": "exec", "input": {}}),
                ),
                input_json_delta(5, json!({"cmd": "pwd"})),
                block_stop(5),
                message_stop(),
            ],
        ]
        .concat();

        let completed = completed(&stream(&lines));
        let out = replay(array_at(&completed, "response.output").into_iter().cloned());

        assert_eq!(
            assistant_block_types(&out),
            [
                "thinking",
                "text",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "tool_use",
            ],
            "output: {out}"
        );
    }

    // Anthropic constrains server tool ids to ^srvtoolu_[a-zA-Z0-9_]+$.
    // Responses items that never came from Claude (a native OpenAI
    // web_search_call) carry ids that violate it, so the id must be normalised
    // instead of trusted.
    #[test]
    fn web_search_call_id_is_normalised_to_claude_server_tool_pattern() {
        let cases = [
            (
                "claude round trip keeps the original id",
                "ws_srvtoolu_abc123",
                "srvtoolu_abc123",
            ),
            (
                "native OpenAI id gains the required prefix",
                "ws_00112233aabb",
                "srvtoolu_00112233aabb",
            ),
            (
                "characters outside the pattern are replaced",
                "ws_00112233-aabb.cc",
                "srvtoolu_00112233_aabb_cc",
            ),
        ];
        for (name, responses_id, want) in cases {
            let out = replay([json!({
                "type": "web_search_call", "id": responses_id, "status": "completed",
                "action": {"type": "search", "query": "q"},
            })]);
            assert_eq!(
                text_at(&out, "messages.0.content.0.id"),
                want,
                "{name}: server_tool_use id"
            );
            assert_eq!(
                text_at(&out, "messages.0.content.1.tool_use_id"),
                want,
                "{name}: tool_use_id must pair with the use block"
            );
        }
    }

    #[test]
    fn web_search_call_without_id_produces_no_blocks() {
        for id in ["", "ws_"] {
            let out = replay([json!({
                "type": "web_search_call", "id": id, "status": "completed",
                "action": {"type": "search", "query": "q"},
            })]);
            let got = assistant_block_types(&out);
            assert!(
                got.is_empty(),
                "id={id:?} produced {got:?}; Anthropic rejects an unpairable server_tool_use"
            );
        }
    }

    // A turn can end after the search block when the upstream stream is cut
    // short. The item must still be closed so the client does not see a
    // dangling item.
    #[test]
    fn web_search_without_result_block_still_emits_item() {
        let events = stream(&[
            message_start("msg_ws"),
            web_search_use_start(0, "srvtoolu_1"),
            input_json_delta(0, json!({"query": "q"})),
            block_stop(0),
            message_stop(),
        ]);
        let done = events
            .iter()
            .filter(|(name, data)| {
                name == "response.output_item.done"
                    && text_at(data, "item.type") == "web_search_call"
            })
            .count();
        assert_eq!(done, 1, "web_search_call output_item.done count");
        assert!(
            at(&completed(&events), "response.output.0.results").is_none(),
            "no result block arrived, so results must be absent"
        );
    }

    #[test]
    fn unmapped_server_tool_produces_no_item() {
        let completed = completed(&stream(&[
            message_start("msg_ws"),
            block_start(
                0,
                json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "code_execution", "input": {}}),
            ),
            block_stop(0),
            block_start(1, json!({"type": "text", "text": ""})),
            text_delta(1, "done"),
            block_stop(1),
            message_stop(),
        ]));
        assert_eq!(
            count_at(&completed, "response.output"),
            1,
            "want the message only: {completed}"
        );
        assert_eq!(text_at(&completed, "response.output.0.type"), "message");
    }

    #[test]
    fn web_search_result_without_matching_use_is_ignored() {
        let events = stream(&[
            message_start("msg_ws"),
            block_start(
                0,
                json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_missing", "content": []}),
            ),
            block_stop(0),
            message_stop(),
        ]);
        for (_, data) in events
            .iter()
            .filter(|(name, _)| name == "response.completed")
        {
            assert_eq!(
                count_at(data, "response.output"),
                0,
                "orphan result produced output items: {data}"
            );
        }
    }

    // Native OpenAI clients send `queries` alongside `query`, and use an
    // `open_page` action with a url instead of a query. Both reach this
    // translator when a session started on an OpenAI provider is resumed
    // against Claude.
    #[test]
    fn web_search_call_query_accepts_native_openai_action_shapes() {
        let cases = [
            (
                "search with query",
                json!({"type": "search", "query": "go release", "queries": ["go release"]}),
                "go release",
            ),
            (
                "search with only queries",
                json!({"type": "search", "queries": ["go release"]}),
                "go release",
            ),
            (
                "open_page falls back to the url",
                json!({"type": "open_page", "url": "https://go.dev/dl"}),
                "https://go.dev/dl",
            ),
        ];
        for (name, action, want) in cases {
            let out = replay([json!({
                "type": "web_search_call", "id": "ws_srvtoolu_1", "status": "completed",
                "action": action,
            })]);
            assert_eq!(
                text_at(&out, "messages.0.content.0.input.query"),
                want,
                "{name}"
            );
        }
    }

    // An empty result list is distinct from a missing one: Claude reported a
    // search that found nothing, which Anthropic accepts on replay.
    #[test]
    fn web_search_with_empty_results_keeps_empty_list() {
        let completed = completed(&stream(&[
            message_start("msg_ws"),
            web_search_use_start(0, "srvtoolu_1"),
            input_json_delta(0, json!({"query": "q"})),
            block_stop(0),
            block_start(
                1,
                json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": []}),
            ),
            block_stop(1),
            message_stop(),
        ]));
        assert_eq!(
            at(&completed, "response.output.0.results"),
            Some(&json!([])),
            "output: {completed}"
        );
    }

    #[test]
    fn web_search_error_result_survives_round_trip() {
        let completed = completed(&stream(&[
            message_start("msg_ws_err"),
            web_search_use_start(0, "srvtoolu_err"),
            input_json_delta(0, json!({"query": "err_query"})),
            block_stop(0),
            block_start(
                1,
                json!({
                    "type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_err",
                    "content": [{"type": "web_search_tool_result_error", "error_code": "rate_limited"}],
                }),
            ),
            block_stop(1),
            message_stop(),
        ]));
        let items = array_at(&completed, "response.output");
        assert!(
            items.len() == 1 && text_at(items[0], "type") == "web_search_call",
            "want 1 web_search_call: {completed}"
        );
        assert_eq!(
            text_at(items[0], "results.0.type"),
            "web_search_tool_result_error"
        );
        assert_eq!(text_at(items[0], "results.0.error_code"), "rate_limited");

        let replayed = replay([items[0].clone()]);
        assert_eq!(
            text_at(&replayed, "messages.0.content.1.content.0.type"),
            "web_search_tool_result_error"
        );
        assert_eq!(
            text_at(&replayed, "messages.0.content.1.content.0.error_code"),
            "rate_limited"
        );
    }

    #[test]
    fn text_search_text_order_is_preserved_in_streaming_and_replay() {
        let lines = [
            message_start("msg_order"),
            block_start(0, json!({"type": "text", "text": ""})),
            text_delta(0, "Before search."),
            block_stop(0),
            web_search_use_start(1, "srvtoolu_order"),
            input_json_delta(1, json!({"query": "query"})),
            block_stop(1),
            block_start(
                2,
                json!({
                    "type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_order",
                    "content": [{"type": "web_search_result", "title": "T", "url": "https://example.com/order", "encrypted_content": "ENC_ORD"}],
                }),
            ),
            block_stop(2),
            block_delta(
                1,
                json!({"type": "citations_delta", "citation": {
                    "type": "web_search_result_location",
                    "cited_text": "After search.",
                    "url": "https://example.com/order",
                    "title": "T",
                    "encrypted_index": "IDX_ORD",
                }}),
            ),
            block_start(3, json!({"type": "text", "text": ""})),
            text_delta(3, "After search."),
            block_stop(3),
            message_stop(),
        ];

        let completed = completed(&stream(&lines));
        let items = array_at(&completed, "response.output");
        assert_eq!(items.len(), 3, "output: {completed}");
        assert!(
            text_at(items[0], "type") == "message"
                && text_at(items[0], "content.0.text") == "Before search.",
            "item 0 must be message 'Before search.': {}",
            items[0]
        );
        assert_eq!(
            text_at(items[1], "type"),
            "web_search_call",
            "item 1: {}",
            items[1]
        );
        assert!(
            text_at(items[2], "type") == "message"
                && text_at(items[2], "content.0.text") == "After search.",
            "item 2 must be message 'After search.': {}",
            items[2]
        );
        assert_eq!(
            text_at(items[2], "content.0.annotations.0.encrypted_index"),
            "IDX_ORD",
            "item 2: {}",
            items[2]
        );

        // Non-streaming must match streaming.
        let out = non_stream(&lines);
        let non_stream_items = array_at(&out, "output");
        assert_eq!(non_stream_items.len(), 3, "non-stream output: {out}");
        assert_eq!(
            text_at(non_stream_items[0], "content.0.text"),
            "Before search."
        );
        assert_eq!(text_at(non_stream_items[1], "type"), "web_search_call");
        assert_eq!(
            text_at(non_stream_items[2], "content.0.text"),
            "After search."
        );
        assert_eq!(
            text_at(
                non_stream_items[2],
                "content.0.annotations.0.encrypted_index"
            ),
            "IDX_ORD"
        );

        // The replay keeps Claude's original block order.
        let replayed = replay(items.into_iter().cloned());
        assert_eq!(
            assistant_block_types(&replayed),
            ["text", "server_tool_use", "web_search_tool_result", "text"]
        );
    }
}

// Ports claude_openai-responses_reasoning_order_test.go.
mod reasoning_order {
    use super::*;
    use crate::claude::openai::responses::request::REDACTED_THINKING_PREFIX;

    /// `responsesReasoningItem`
    fn reasoning_item(signature: &str, text: &str) -> Value {
        json!({
            "type": "reasoning",
            "encrypted_content": signature,
            "summary": [{"type": "summary_text", "text": text}],
        })
    }

    /// `responsesFunctionCallItem`
    fn function_call_item(call_id: &str, name: &str) -> Value {
        json!({"type": "function_call", "call_id": call_id, "name": name, "arguments": "{}"})
    }

    /// `responsesFunctionCallOutputItem`
    fn function_call_output_item(call_id: &str, output: &str) -> Value {
        json!({"type": "function_call_output", "call_id": call_id, "output": output})
    }

    /// `responsesWebSearchCallItem`
    fn web_search_call_item(id: &str, query: &str) -> Value {
        json!({
            "type": "web_search_call",
            "id": id,
            "status": "completed",
            "action": {"type": "search", "query": query},
        })
    }

    fn user_message(text: &str) -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
    }

    fn assistant_message(text: &str) -> Value {
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
    }

    #[test]
    fn keeps_latest_consecutive_reasoning() {
        let (first_raw, _) = claude_thinking_signature_for_model("claude-opus-5-first");
        let (second_raw, _) = claude_thinking_signature_for_model("claude-opus-5-second");
        let (third_raw, third_signature) =
            claude_thinking_signature_for_model("claude-opus-5-third");

        let out = replay([
            assistant_message("prefix"),
            reasoning_item(&first_raw, "first reasoning"),
            reasoning_item(&second_raw, "second reasoning"),
            reasoning_item(&third_raw, "third reasoning"),
            function_call_item("call_latest", "latest_tool"),
            function_call_output_item("call_latest", "done"),
        ]);

        assert_eq!(
            types_at(&out, "messages.0.content"),
            ["text", "thinking", "tool_use"],
            "output: {out}"
        );
        assert_eq!(text_at(&out, "messages.0.content.0.text"), "prefix");
        assert_eq!(
            text_at(&out, "messages.0.content.1.thinking"),
            "third reasoning",
            "want the latest consecutive reasoning: {out}"
        );
        assert_eq!(
            text_at(&out, "messages.0.content.1.signature"),
            third_signature,
            "want the latest signature: {out}"
        );
    }

    #[test]
    fn tool_calls_separate_reasoning_blocks() {
        let (first_raw, first_signature) =
            claude_thinking_signature_for_model("claude-opus-5-first");
        let (second_raw, second_signature) =
            claude_thinking_signature_for_model("claude-opus-5-second");

        let out = replay([
            reasoning_item(&first_raw, "first reasoning"),
            function_call_item("call_first", "first_tool"),
            reasoning_item(&second_raw, "second reasoning"),
            function_call_item("call_second", "second_tool"),
            function_call_output_item("call_first", "first result"),
            function_call_output_item("call_second", "second result"),
        ]);

        assert_eq!(
            types_at(&out, "messages.0.content"),
            ["thinking", "tool_use", "thinking", "tool_use"],
            "output: {out}"
        );
        assert_eq!(
            text_at(&out, "messages.0.content.0.signature"),
            first_signature
        );
        assert_eq!(
            text_at(&out, "messages.0.content.2.signature"),
            second_signature
        );
        assert_eq!(text_at(&out, "messages.0.content.1.id"), "call_first");
        assert_eq!(text_at(&out, "messages.0.content.3.id"), "call_second");
    }

    #[test]
    fn non_thinking_blocks_separate_reasoning() {
        let (first_raw, first_signature) =
            claude_thinking_signature_for_model("claude-opus-5-first");
        let (second_raw, second_signature) =
            claude_thinking_signature_for_model("claude-opus-5-second");
        const REDACTED_DATA: &str = "opaque-redacted-data";

        let out = replay([
            reasoning_item(&first_raw, "first reasoning"),
            assistant_message("visible separator"),
            reasoning_item(&second_raw, "second reasoning"),
            json!({
                "type": "reasoning",
                "encrypted_content": format!("{REDACTED_THINKING_PREFIX}{REDACTED_DATA}"),
                "summary": [],
            }),
            reasoning_item(&first_raw, "third reasoning"),
            function_call_item("call_separator", "separator_tool"),
            function_call_output_item("call_separator", "done"),
        ]);

        assert_eq!(
            types_at(&out, "messages.0.content"),
            [
                "thinking",
                "text",
                "thinking",
                "redacted_thinking",
                "thinking",
                "tool_use",
            ],
            "output: {out}"
        );
        assert_eq!(
            text_at(&out, "messages.0.content.0.signature"),
            first_signature
        );
        assert_eq!(
            text_at(&out, "messages.0.content.2.signature"),
            second_signature
        );
        assert_eq!(text_at(&out, "messages.0.content.3.data"), REDACTED_DATA);
        assert_eq!(
            text_at(&out, "messages.0.content.4.signature"),
            first_signature
        );
        assert_eq!(text_at(&out, "messages.0.content.5.id"), "call_separator");
    }

    // The four cases below are the subtests of upstream's
    // TestConvertOpenAIResponsesRequestToClaude_WebSearchSeparatesToolUseWithThinking.

    const THOUGHT: &str = "I should search and then run a command.";

    #[test]
    fn web_search_after_function_call_separates_tool_use_with_thinking() {
        let (raw, signature) = claude_thinking_signature_for_model("claude-opus-5-test");
        let out = replay([
            user_message("hi"),
            reasoning_item(&raw, THOUGHT),
            assistant_message("Searching, then running."),
            function_call_item("call_00_abc", "exec_command"),
            web_search_call_item("ws_srvtoolu_12_x", "hello world"),
            function_call_output_item("call_00_abc", "hi"),
        ]);

        assert_eq!(
            types_at(&out, "messages.1.content"),
            [
                "thinking",
                "text",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "tool_use",
            ],
            "output: {out}"
        );
        assert_eq!(text_at(&out, "messages.1.content.0.signature"), signature);
        assert_eq!(text_at(&out, "messages.1.content.4.signature"), signature);
        assert_eq!(
            text_at(&out, "messages.1.content.4.thinking"),
            THOUGHT,
            "the separating thinking block keeps the original text"
        );
        assert_eq!(text_at(&out, "messages.1.content.5.id"), "call_00_abc");
    }

    #[test]
    fn web_search_before_function_call_separates_tool_use_with_thinking() {
        let (raw, signature) = claude_thinking_signature_for_model("claude-opus-5-test");
        let out = replay([
            user_message("hi"),
            reasoning_item(&raw, THOUGHT),
            assistant_message("Searching, then running."),
            web_search_call_item("ws_srvtoolu_12_x", "hello world"),
            function_call_item("call_00_abc", "exec_command"),
            function_call_output_item("call_00_abc", "hi"),
        ]);

        assert_eq!(
            types_at(&out, "messages.1.content"),
            [
                "thinking",
                "text",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "tool_use",
            ],
            "output: {out}"
        );
        assert_eq!(text_at(&out, "messages.1.content.4.signature"), signature);
        assert_eq!(text_at(&out, "messages.1.content.5.id"), "call_00_abc");
    }

    #[test]
    fn web_search_without_thinking_adds_no_thinking_block() {
        let out = replay([
            user_message("hi"),
            assistant_message("Searching, then running."),
            web_search_call_item("ws_srvtoolu_12_x", "hello world"),
            function_call_item("call_00_abc", "exec_command"),
            function_call_output_item("call_00_abc", "hi"),
        ]);

        assert_eq!(
            types_at(&out, "messages.1.content"),
            [
                "text",
                "server_tool_use",
                "web_search_tool_result",
                "tool_use"
            ],
            "output: {out}"
        );
    }

    #[test]
    fn web_search_separation_uses_latest_thinking_block() {
        let (first_raw, _) = claude_thinking_signature_for_model("claude-opus-5-first");
        let (second_raw, second_signature) =
            claude_thinking_signature_for_model("claude-opus-5-second");
        let out = replay([
            user_message("hi"),
            reasoning_item(&first_raw, "first reasoning"),
            assistant_message("thought once"),
            reasoning_item(&second_raw, "second reasoning"),
            web_search_call_item("ws_srvtoolu_12_x", "hello world"),
            function_call_item("call_00_abc", "exec_command"),
            function_call_output_item("call_00_abc", "hi"),
        ]);

        assert_eq!(
            types_at(&out, "messages.1.content"),
            [
                "thinking",
                "text",
                "thinking",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "tool_use",
            ],
            "output: {out}"
        );
        assert_eq!(
            text_at(&out, "messages.1.content.5.signature"),
            second_signature,
            "want the latest signature"
        );
        assert_eq!(
            text_at(&out, "messages.1.content.5.thinking"),
            "second reasoning",
            "want the latest thinking text"
        );
    }
}

// Ports claude_openai-responses_citations_test.go.
mod citations {
    use super::*;

    const WANT_TEXT: &str = "The store reopened in 2024, and Olga was named for Ohlert.";

    /// A search, then three text blocks; the first and last carry citations.
    fn lines() -> Vec<String> {
        let citation = |url: &str, title: &str| {
            json!({"type": "citations_delta", "citation": {
                "type": "web_search_result_location", "url": url, "title": title,
            }})
        };
        vec![
            message_start("msg_citations"),
            web_search_use_start(0, "srv_1"),
            block_stop(0),
            block_start(
                1,
                json!({"type": "web_search_tool_result", "tool_use_id": "srv_1", "content": []}),
            ),
            block_stop(1),
            block_start(2, json!({"type": "text", "text": ""})),
            text_delta(2, "The store reopened in 2024"),
            block_delta(2, citation("https://example.com/store", "Store")),
            block_stop(2),
            block_start(3, json!({"type": "text", "text": ""})),
            text_delta(3, ", and "),
            block_stop(3),
            block_start(4, json!({"type": "text", "text": ""})),
            text_delta(4, "Olga was named for Ohlert."),
            block_delta(4, citation("https://example.com/olga", "Olga")),
            block_stop(4),
            message_stop(),
        ]
    }

    /// Adjacent text blocks share one message part, with both citations.
    fn check_output(items: &[&Value]) {
        assert_eq!(items.len(), 2, "want web search and one message: {items:?}");
        assert_eq!(text_at(items[0], "type"), "web_search_call");
        let message = items[1];
        assert_eq!(text_at(message, "type"), "message");
        assert_eq!(count_at(message, "content"), 1, "message: {message}");
        assert_eq!(text_at(message, "content.0.text"), WANT_TEXT);
        let annotations = array_at(message, "content.0.annotations");
        assert!(
            annotations.len() == 2
                && text_at(annotations[0], "url") == "https://example.com/store"
                && text_at(annotations[1], "url") == "https://example.com/olga",
            "want both citations: {annotations:?}"
        );
    }

    #[test]
    fn adjacent_cited_text_blocks_share_message_when_streaming() {
        let events = stream(&lines());
        let completed = completed(&events);
        check_output(&array_at(&completed, "response.output"));

        let count = |wanted: &str| events.iter().filter(|(name, _)| name == wanted).count();
        for event in [
            "response.content_part.added",
            "response.output_text.done",
            "response.content_part.done",
        ] {
            assert_eq!(count(event), 1, "{event} events");
        }
        assert_eq!(
            count("response.output_item.done"),
            2,
            "response.output_item.done events"
        );
    }

    #[test]
    fn adjacent_cited_text_blocks_share_message_without_streaming() {
        let out = non_stream(&lines());
        check_output(&array_at(&out, "output"));
    }
}

// Ports claude_openai-responses_interleaved_search_test.go.
mod interleaved_search {
    use super::*;

    /// A search block and its one result, at `index` and `index + 1`.
    fn search_lines(index: u32, id: &str) -> Vec<String> {
        vec![
            web_search_use_start(index, id),
            input_json_delta(index, json!({"query": "lindorm"})),
            block_stop(index),
            block_start(
                index + 1,
                json!({
                    "type": "web_search_tool_result",
                    "tool_use_id": id,
                    "content": [{"type": "web_search_result", "title": "T", "url": format!("https://example.com/{id}")}],
                }),
            ),
            block_stop(index + 1),
        ]
    }

    // Regression for a production incident: a Claude turn that interleaved
    // thinking with server-side web searches used to lose the search blocks,
    // leaving two adjacent reasoning items. Replaying those produced two
    // adjacent Claude `thinking` blocks, which Anthropic rejects with
    // "`thinking` ... blocks in the latest assistant message cannot be
    // modified", killing the session mid-turn.
    #[test]
    fn interleaved_thinking_and_search_survive_round_trip() {
        let signature = test_signature();
        let lines = [
            vec![message_start("msg_x")],
            thinking_lines(0, "first", &signature),
            vec![
                block_start(1, json!({"type": "text", "text": ""})),
                text_delta(1, "Researching."),
                block_stop(1),
            ],
            search_lines(2, "srvtoolu_a"),
            thinking_lines(6, "second", &signature),
            search_lines(7, "srvtoolu_b"),
            thinking_lines(11, "third", &signature),
            vec![
                block_start(
                    12,
                    json!({"type": "tool_use", "id": "toolu_1", "name": "exec", "input": {}}),
                ),
                input_json_delta(12, json!({"cmd": "pwd"})),
                block_stop(12),
                message_stop(),
            ],
        ]
        .concat();

        let completed = completed(&stream(&lines));
        let out = replay(array_at(&completed, "response.output").into_iter().cloned());

        assert_eq!(
            assistant_block_types(&out),
            [
                "thinking",
                "text",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "tool_use",
            ],
            "output: {out}"
        );
    }
}

// Ports claude_openai_responses_compat_test.go.
mod compat {
    use super::*;

    #[test]
    fn with_compat_preserves_empty_reasoning() {
        let payload = json!({"input": [{
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "reason"}],
            "encrypted_content": "",
        }]});

        let without_compat = convert("deepseek-v4", &payload);
        assert_eq!(
            count_at(&without_compat, "messages"),
            0,
            "default translation preserved empty reasoning: {without_compat}"
        );

        let with_compat = convert_with_compat("deepseek-v4", &payload);
        assert!(
            text_at(&with_compat, "messages.0.content.0.type") == "thinking"
                && text_at(&with_compat, "messages.0.content.0.signature").is_empty(),
            "compat translation missing unsigned thinking block: {with_compat}"
        );

        let opaque_payload = json!({"input": [{
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "reason"}],
            "encrypted_content": "opaque-deepseek-id",
        }]});
        let opaque_compat = convert_with_compat("deepseek-v4", &opaque_payload);
        let part = at(&opaque_compat, "messages.0.content.0").unwrap_or(&Value::Null);
        assert!(
            text_at(part, "type") == "thinking"
                && text_at(part, "thinking") == "reason"
                && text_at(part, "signature") == "opaque-deepseek-id",
            "compat translation dropped invalid-signature thinking block: {opaque_compat}"
        );
    }
}

// Ports noop_optimization_test.go.
mod noop_optimization {
    use super::*;

    #[test]
    fn non_stream_keeps_zero_usage_defaults() {
        let output = non_stream(&[data(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hello"},
        }))]);

        for path in [
            "usage.input_tokens",
            "usage.input_tokens_details.cached_tokens",
            "usage.output_tokens",
            "usage.total_tokens",
        ] {
            let value = at(&output, path);
            assert!(
                value.is_some_and(|value| int_of(value) == 0),
                "{path} = {value:?}, want zero"
            );
        }
    }
}

// Ports claude_openai-responses_pause_test.go.
mod pause {
    use super::*;

    // TestClaudeResponsesServerToolStopReason: a `pause_turn` stop, in any
    // case and with spaces around it, leaves the response incomplete with
    // `null` details; `max_tokens` gives its reason; the other stops
    // complete it. The search and the usage survive either way.
    #[test]
    fn server_tool_stop_reason() {
        for (reason, status, detail) in [
            ("pause_turn", "incomplete", Some(Value::Null)),
            (" PAUSE_TURN ", "incomplete", Some(Value::Null)),
            (
                "max_tokens",
                "incomplete",
                Some(json!({"reason": "max_output_tokens"})),
            ),
            ("end_turn", "completed", None),
            ("tool_use", "completed", None),
            ("stop_sequence", "completed", None),
        ] {
            let mut lines = server_tool::web_search_lines();
            let stop = lines.pop().unwrap_or_default();
            lines.push(data(json!({
                "type": "message_delta",
                "delta": {"stop_reason": reason},
                "usage": {"output_tokens": 12},
            })));
            lines.push(stop);
            let check = |mode: &str, response: &Value| {
                assert_eq!(text_at(response, "status"), status, "{reason:?} {mode}");
                let got = at(response, "incomplete_details");
                match &detail {
                    Some(want) => assert_eq!(got, Some(want), "{reason:?} {mode}: {response}"),
                    None => assert!(
                        got.is_none_or(Value::is_null),
                        "{reason:?} {mode}: incomplete_details = {got:?}"
                    ),
                }
                assert_eq!(
                    text_at(response, "output.0.action.query"),
                    "lindorm vector",
                    "{reason:?} {mode}"
                );
                assert_eq!(
                    text_at(response, "output.0.results.0.encrypted_content"),
                    "ENC_A",
                    "{reason:?} {mode}"
                );
                assert_eq!(
                    at(response, "usage.output_tokens").map(int_of),
                    Some(12),
                    "{reason:?} {mode}"
                );
            };

            let mut terminals = 0;
            for (event, data) in stream(&lines) {
                if event != "response.completed" && event != "response.incomplete" {
                    continue;
                }
                terminals += 1;
                assert_eq!(event, format!("response.{status}"), "{reason:?} stream");
                check("stream", at(&data, "response").unwrap_or(&Value::Null));
            }
            assert_eq!(terminals, 1, "{reason:?} stream: terminal count");

            check("buffered", &non_stream(&lines));
        }
    }
}

// Ports claude_openai_native_response_test.go.
mod native {
    use super::*;

    // TestConvertClaudeResponseToOpenAIResponsesNonStream_NativeMessagesJSON:
    // a whole Messages response becomes a whole Responses response.
    #[test]
    fn non_stream_native_messages_json() {
        for (name, content, stop_reason, status, tools) in [
            (
                "text",
                r#"[{"type":"text","text":"Hello "},{"type":"text","text":"world!"}]"#,
                "end_turn",
                "completed",
                false,
            ),
            (
                "tools",
                r#"[{"type":"text","text":"Hello world!"},{"type":"tool_use","id":"toolu_weather","name":"get_weather","input":{"city":"Paris","days":2}},{"type":"tool_use","id":"toolu_clock","name":"get_time","input":{}}]"#,
                "tool_use",
                "completed",
                true,
            ),
            (
                "max_tokens",
                r#"[{"type":"text","text":"Hello world!"}]"#,
                "max_tokens",
                "incomplete",
                false,
            ),
        ] {
            let raw = format!(
                r#"{{"id":"msg_native","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":{content},"stop_reason":"{stop_reason}","stop_sequence":null,"usage":{{"input_tokens":13,"cache_read_input_tokens":7,"cache_creation_input_tokens":3,"output_tokens":5}}}}"#
            );
            let request = json!({"model": "claude-sonnet-4-6"});
            let out = convert_claude_response_to_openai_responses_non_stream(
                &request,
                &request,
                raw.as_bytes(),
            );
            for (path, want) in [
                ("id", "msg_native"),
                ("object", "response"),
                ("model", "claude-sonnet-4-6"),
                ("status", status),
                ("output.0.type", "message"),
                ("output.0.role", "assistant"),
                ("output.0.status", status),
                ("output.0.content.0.type", "output_text"),
                ("output.0.content.0.text", "Hello world!"),
            ] {
                assert_eq!(text_at(&out, path), want, "{name}: {path} in {out}");
            }
            for (path, want) in [
                ("usage.input_tokens", 23),
                ("usage.output_tokens", 5),
                ("usage.total_tokens", 28),
                ("usage.input_tokens_details.cached_tokens", 7),
            ] {
                assert_eq!(
                    at(&out, path).map(int_of),
                    Some(want),
                    "{name}: {path} in {out}"
                );
            }
            if status == "incomplete" {
                assert_eq!(
                    text_at(&out, "incomplete_details.reason"),
                    "max_output_tokens",
                    "{name}"
                );
            } else {
                assert_eq!(at(&out, "incomplete_details"), Some(&Value::Null), "{name}");
            }
            if !tools {
                assert_eq!(count_at(&out, "output"), 1, "{name}: {out}");
                continue;
            }
            assert_eq!(count_at(&out, "output"), 3, "{name}: {out}");
            for (path, want) in [
                ("output.1.call_id", "toolu_weather"),
                ("output.1.type", "function_call"),
                ("output.1.name", "get_weather"),
                ("output.1.status", "completed"),
                ("output.2.call_id", "toolu_clock"),
                ("output.2.type", "function_call"),
                ("output.2.name", "get_time"),
                ("output.2.status", "completed"),
            ] {
                assert_eq!(text_at(&out, path), want, "{name}: {path} in {out}");
            }
            let arguments: Value =
                serde_json::from_str(&text_at(&out, "output.1.arguments")).expect("JSON");
            assert_eq!(arguments, json!({"city": "Paris", "days": 2}), "{name}");
            let empty: Value =
                serde_json::from_str(&text_at(&out, "output.2.arguments")).expect("JSON");
            assert_eq!(empty, json!({}), "{name}");
        }
    }

    // TestNativeMessagesJSONCitations: a text block's citations become the
    // annotations of its output text.
    #[test]
    fn citations() {
        let raw = r#"{"id":"msg_citations","type":"message","model":"claude-native","content":[{"type":"text","text":"Answer.","citations":[{"type":"web_search_result_location","url":"https://example.com","title":"Source","cited_text":"Answer.","encrypted_index":"IDX"}]}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}}"#;
        let out = non_stream(&[raw.to_owned()]);
        let annotations = "output.0.content.0.annotations";
        assert_eq!(count_at(&out, annotations), 1, "{out}");
        assert_eq!(
            text_at(&out, &format!("{annotations}.0.url")),
            "https://example.com"
        );
        assert_eq!(
            text_at(&out, &format!("{annotations}.0.encrypted_index")),
            "IDX"
        );
    }
}
