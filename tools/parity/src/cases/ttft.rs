//! Hand-written cases for first-token events: the Responses events of
//! upstream's TestIsResponsesTokenEvent_Classification
//! (internal/runtime/executor/helps/responses_ttft_helpers_test.go), and,
//! not upstream's, events of each kind the Responses, Chat Completions,
//! Claude and Gemini classifiers tell apart, with the SSE framings they
//! read.

use serde_json::json;

use super::Case;

/// The hand-written cases for `ttft/token-event`.
pub fn token_events() -> Vec<Case> {
    [
        ("responses", RESPONSES),
        ("responses", RESPONSES_MORE),
        ("chat", CHAT),
        ("claude", CLAUDE),
        ("gemini", GEMINI),
    ]
    .into_iter()
    .flat_map(|(format, events)| {
        events.iter().map(move |(name, payload)| {
            Case::new(format!("{format}-{name}"), "", *payload)
                .with_options(json!({ "format": format }))
        })
    })
    .collect()
}

/// Upstream's TestIsResponsesTokenEvent_Classification.
const RESPONSES: &[(&str, &str)] = &[
    ("empty-payload", ""),
    ("whitespace-only", "   \n\t  "),
    (
        "codex-rate-limits-metadata",
        r#"{"type":"codex.rate_limits","rate_limits":{"plan_type":"pro"}}"#,
    ),
    (
        "codex-response-metadata",
        r#"{"type":"codex.response.metadata","etag":"W/\"123\""}"#,
    ),
    (
        "responsesapi-websocket-timing",
        r#"{"type":"responsesapi.websocket_timing","timing":{"duration_ms":100}}"#,
    ),
    (
        "response-created",
        r#"{"type":"response.created","response":{"id":"resp_123","status":"in_progress"}}"#,
    ),
    (
        "response-in-progress",
        r#"{"type":"response.in_progress","response":{"id":"resp_123","tools":[{"type":"function"}]}}"#,
    ),
    (
        "response-output-item-added-with-encrypted-content-only",
        r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"gAAAAAB..."}}"#,
    ),
    (
        "response-content-part-added",
        r#"{"type":"response.content_part.added","part":{"type":"text","text":""}}"#,
    ),
    (
        "response-reasoning-summary-part-added",
        r#"{"type":"response.reasoning_summary_part.added","part":{"type":"summary_text","text":""}}"#,
    ),
    (
        "response-reasoning-summary-text-delta-empty",
        r#"{"type":"response.reasoning_summary_text.delta","delta":""}"#,
    ),
    (
        "response-reasoning-summary-text-delta-non-empty",
        r#"{"type":"response.reasoning_summary_text.delta","delta":"**Inspecting**"}"#,
    ),
    (
        "response-reasoning-delta-non-empty",
        r#"{"type":"response.reasoning.delta","delta":"Analyzing requirements..."}"#,
    ),
    (
        "response-reasoning-text-delta-non-empty",
        r#"{"type":"response.reasoning_text.delta","delta":"Step 1: Check code"}"#,
    ),
    (
        "response-output-text-delta-non-empty",
        r#"{"type":"response.output_text.delta","delta":"Hello world"}"#,
    ),
    (
        "response-text-delta-non-empty",
        r#"{"type":"response.text.delta","delta":"Direct text chunk"}"#,
    ),
    (
        "response-function-call-arguments-delta-non-empty",
        r#"{"type":"response.function_call_arguments.delta","delta":"{\"query\":\"test\"}"}"#,
    ),
    (
        "response-custom-tool-call-input-delta-non-empty",
        r#"{"type":"response.custom_tool_call_input.delta","delta":"{\"param\":1}"}"#,
    ),
    (
        "response-code-interpreter-call-code-delta-non-empty",
        r#"{"type":"response.code_interpreter_call_code.delta","delta":"import math\n"}"#,
    ),
    (
        "response-mcp-call-arguments-delta-non-empty",
        r#"{"type":"response.mcp_call_arguments.delta","delta":"{\"tool\":\"lookup\"}"}"#,
    ),
    (
        "response-shell-call-command-added-with-non-empty-command",
        r#"{"type":"response.shell_call_command.added","command":"ls -la"}"#,
    ),
    (
        "response-shell-call-command-added-with-empty-command",
        r#"{"type":"response.shell_call_command.added","command":""}"#,
    ),
    (
        "response-shell-call-command-delta-non-empty",
        r#"{"type":"response.shell_call_command.delta","delta":"ls -la\n"}"#,
    ),
    (
        "response-shell-call-output-content-delta-is-tool-execution-output-not-model-token",
        r#"{"type":"response.shell_call_output_content.delta","delta":{"stdout":"output text\n","stderr":""}}"#,
    ),
    (
        "response-shell-call-output-content-done-is-tool-execution-output-not-model-token",
        r#"{"type":"response.shell_call_output_content.done","output":[]}"#,
    ),
    (
        "response-refusal-delta-non-empty",
        r#"{"type":"response.refusal.delta","delta":"I cannot fulfill this request"}"#,
    ),
    (
        "response-audio-transcript-delta-non-empty",
        r#"{"type":"response.audio.transcript.delta","delta":"Spoken text"}"#,
    ),
    (
        "response-audio-delta-non-empty",
        r#"{"type":"response.audio.delta","delta":"UklGRi..."}"#,
    ),
    (
        "response-image-generation-call-partial-image-non-empty",
        r#"{"type":"response.image_generation_call.partial_image","partial_image_b64":"iVBORw0KGgo..."}"#,
    ),
    (
        "response-web-search-call-in-progress",
        r#"{"type":"response.web_search_call.in_progress"}"#,
    ),
    (
        "response-file-search-call-searching",
        r#"{"type":"response.file_search_call.searching"}"#,
    ),
    (
        "response-code-interpreter-call-interpreting",
        r#"{"type":"response.code_interpreter_call.interpreting"}"#,
    ),
    (
        "response-mcp-call-in-progress",
        r#"{"type":"response.mcp_call.in_progress"}"#,
    ),
    (
        "response-output-item-done-function-call-empty-args",
        r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"lookup","arguments":""}}"#,
    ),
    (
        "response-output-item-done-function-call-non-empty-args",
        r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"lookup","arguments":"{\"q\":1}"}}"#,
    ),
    (
        "response-output-item-done-message-empty",
        r#"{"type":"response.output_item.done","item":{"type":"message","content":[]}}"#,
    ),
    (
        "response-output-item-done-message-with-text",
        r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"text","text":"hello"}]}}"#,
    ),
    (
        "response-completed-fallback",
        r#"{"type":"response.completed","response":{"id":"resp_123","status":"completed"}}"#,
    ),
    (
        "response-done-fallback",
        r#"{"type":"response.done","response":{"id":"resp_123"}}"#,
    ),
    (
        "response-incomplete-fallback",
        r#"{"type":"response.incomplete","response":{"id":"resp_123","status":"incomplete"}}"#,
    ),
    (
        "response-failed-fallback",
        r#"{"type":"response.failed","response":{"id":"resp_123","status":"failed"}}"#,
    ),
    (
        "generic-error-fallback",
        r#"{"type":"error","error":{"message":"overloaded","code":"rate_limit_exceeded"}}"#,
    ),
    (
        "sse-data-line-with-output-text-delta",
        r#"data: {"type":"response.output_text.delta","delta":"Hello SSE"}"#,
    ),
    (
        "sse-data-line-with-response-created",
        r#"data: {"type":"response.created","response":{"id":"resp_sse"}}"#,
    ),
];

/// Not upstream's: the Responses events its test leaves out, values that
/// aren't strings, and framings.
const RESPONSES_MORE: &[(&str, &str)] = &[
    (
        "audio-delta-data-only",
        r#"{"type":"response.audio.delta","data":"UklGRi"}"#,
    ),
    (
        "audio-delta-empty",
        r#"{"type":"response.audio.delta","delta":"","data":""}"#,
    ),
    (
        "shell-call-command-done",
        r#"{"type":"response.shell_call_command.done","command":"ls"}"#,
    ),
    (
        "reasoning-summary-text-done",
        r#"{"type":"response.reasoning_summary_text.done","text":"done"}"#,
    ),
    (
        "reasoning-text-done-empty",
        r#"{"type":"response.reasoning_text.done","text":""}"#,
    ),
    (
        "output-text-done",
        r#"{"type":"response.output_text.done","text":"hi"}"#,
    ),
    (
        "refusal-done",
        r#"{"type":"response.refusal.done","refusal":"no"}"#,
    ),
    (
        "refusal-done-empty",
        r#"{"type":"response.refusal.done","refusal":""}"#,
    ),
    (
        "function-call-arguments-done",
        r#"{"type":"response.function_call_arguments.done","arguments":"{}"}"#,
    ),
    (
        "mcp-call-arguments-done-empty",
        r#"{"type":"response.mcp_call_arguments.done","arguments":""}"#,
    ),
    (
        "custom-tool-call-input-done",
        r#"{"type":"response.custom_tool_call_input.done","input":"x"}"#,
    ),
    (
        "code-interpreter-call-code-done",
        r#"{"type":"response.code_interpreter_call_code.done","code":"print(1)"}"#,
    ),
    (
        "reasoning-summary-part-done",
        r#"{"type":"response.reasoning_summary_part.done","part":{"type":"summary_text","text":"s"}}"#,
    ),
    (
        "content-part-done-text",
        r#"{"type":"response.content_part.done","part":{"type":"output_text","text":"t"}}"#,
    ),
    (
        "content-part-done-refusal",
        r#"{"type":"response.content_part.done","part":{"type":"refusal","refusal":"no"}}"#,
    ),
    (
        "content-part-done-empty",
        r#"{"type":"response.content_part.done","part":{"type":"output_text","text":""}}"#,
    ),
    (
        "output-item-done-custom-tool-call",
        r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","input":"patch"}}"#,
    ),
    (
        "output-item-done-message-refusal",
        r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"refusal","refusal":"no"}]}}"#,
    ),
    (
        "output-item-done-message-object",
        r#"{"type":"response.output_item.done","item":{"type":"message","content":{"text":"one"}}}"#,
    ),
    (
        "output-item-done-reasoning",
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","summary":[{"text":"s"}]}}"#,
    ),
    (
        "image-partial-empty",
        r#"{"type":"response.image_generation_call.partial_image","partial_image_b64":""}"#,
    ),
    (
        "delta-number",
        r#"{"type":"response.output_text.delta","delta":0}"#,
    ),
    (
        "delta-null",
        r#"{"type":"response.output_text.delta","delta":null}"#,
    ),
    (
        "delta-false",
        r#"{"type":"response.output_text.delta","delta":false}"#,
    ),
    (
        "delta-object",
        r#"{"type":"response.output_text.delta","delta":{}}"#,
    ),
    ("delta-missing", r#"{"type":"response.output_text.delta"}"#),
    (
        "data-no-space",
        r#"data:{"type":"response.output_text.delta","delta":"x"}"#,
    ),
    (
        "data-two-spaces",
        r#"data:  {"type":"response.output_text.delta","delta":"x"}"#,
    ),
    ("data-only", "data:"),
    ("data-blank", "data:   \t"),
    ("event-line", "event: response.output_text.delta"),
    (
        "event-and-data",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}",
    ),
    ("done", "data: [DONE]"),
    ("type-not-string", r#"{"type":5,"delta":"x"}"#),
    ("type-cased", r#"{"type":"Response.Completed"}"#),
    ("array", r#"[{"type":"response.completed"}]"#),
    ("leading-text", r#"x {"type":"response.completed"}"#),
    (
        "pretty",
        "{\n  \"type\": \"response.output_text.delta\",\n  \"delta\": \"x\"\n}\n",
    ),
];

/// Not upstream's: Chat Completions chunks.
const CHAT: &[(&str, &str)] = &[
    ("empty", ""),
    ("blank", " \r\n"),
    ("done", "[DONE]"),
    ("data-done", "data: [DONE]"),
    ("data-done-no-space", "data:[DONE]"),
    ("data-done-spaced", "data:   [DONE]  "),
    ("data-only", "data:"),
    (
        "role-only",
        r#"data: {"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
    ),
    (
        "content-empty",
        r#"data: {"choices":[{"index":0,"delta":{"content":""}}]}"#,
    ),
    (
        "content",
        r#"data: {"choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
    ),
    (
        "reasoning-content",
        r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#,
    ),
    (
        "reasoning",
        r#"{"choices":[{"delta":{"reasoning":"think"}}]}"#,
    ),
    ("refusal", r#"{"choices":[{"delta":{"refusal":"no"}}]}"#),
    (
        "tool-call-name",
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":""}}]}}]}"#,
    ),
    (
        "tool-call-arguments",
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\""}}]}}]}"#,
    ),
    (
        "tool-call-empty",
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"","arguments":""}}]}}]}"#,
    ),
    (
        "custom-tool-input",
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"type":"custom","custom":{"input":"patch"}}]}}]}"#,
    ),
    (
        "tool-calls-object",
        r#"{"choices":[{"delta":{"tool_calls":{"function":{"name":"x"}}}}]}"#,
    ),
    (
        "message-content",
        r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
    ),
    (
        "message-reasoning-only",
        r#"{"choices":[{"message":{"reasoning":"r"}}]}"#,
    ),
    (
        "message-reasoning-content",
        r#"{"choices":[{"message":{"reasoning_content":"r"}}]}"#,
    ),
    (
        "message-refusal",
        r#"{"choices":[{"message":{"refusal":"no"}}]}"#,
    ),
    (
        "message-tool-call",
        r#"{"choices":[{"message":{"tool_calls":[{"function":{"name":"f","arguments":""}}]}}]}"#,
    ),
    (
        "message-custom-tool-call",
        r#"{"choices":[{"message":{"tool_calls":[{"custom":{"input":"x"}}]}}]}"#,
    ),
    (
        "finish-reason",
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ),
    (
        "finish-reason-empty",
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":""}]}"#,
    ),
    (
        "second-choice",
        r#"{"choices":[{"delta":{}},{"delta":{"content":"b"}}]}"#,
    ),
    (
        "choices-empty",
        r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
    ),
    ("choices-object", r#"{"choices":{"delta":{"content":"x"}}}"#),
    ("choices-null", r#"{"choices":null}"#),
    ("error", r#"{"error":{"message":"overloaded"}}"#),
    ("error-null", r#"{"error":null}"#),
    ("error-string", r#"data: {"error":"bad"}"#),
    ("content-number", r#"{"choices":[{"delta":{"content":0}}]}"#),
    (
        "content-null",
        r#"{"choices":[{"delta":{"content":null}}]}"#,
    ),
    ("event-line", "event: message"),
    ("array", r#"[{"choices":[{"delta":{"content":"x"}}]}]"#),
];

/// Not upstream's: Claude Messages events.
const CLAUDE: &[(&str, &str)] = &[
    ("empty", ""),
    (
        "message-start",
        r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":5,"output_tokens":1}}}"#,
    ),
    ("ping", r#"data: {"type":"ping"}"#),
    (
        "content-block-start-empty-text",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    ),
    (
        "content-block-start-text",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Hi"}}"#,
    ),
    (
        "content-block-start-thinking",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"t"}}"#,
    ),
    (
        "content-block-start-tool-use",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"lookup","input":{}}}"#,
    ),
    (
        "text-delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
    ),
    (
        "text-delta-empty",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#,
    ),
    (
        "thinking-delta",
        r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
    ),
    (
        "input-json-delta",
        r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"a\""}}"#,
    ),
    (
        "input-json-delta-empty",
        r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":""}}"#,
    ),
    (
        "signature-delta",
        r#"{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"EqQB"}}"#,
    ),
    (
        "content-block-stop",
        r#"{"type":"content_block_stop","index":0}"#,
    ),
    (
        "message-delta-stop-reason",
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}"#,
    ),
    (
        "message-delta-no-stop-reason",
        r#"{"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":7}}"#,
    ),
    ("message-stop", r#"data: {"type":"message_stop"}"#),
    (
        "error",
        r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    ),
    (
        "event-and-data",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"x\"}}",
    ),
    (
        "event-crlf-and-data",
        "event: message_stop\r\ndata: {\"type\":\"message_stop\"}",
    ),
    ("event-only", "event: message_stop"),
    ("data-no-space", r#"data:{"type":"message_stop"}"#),
    ("data-only", "data: "),
    (
        "whole-message-text",
        r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"Hi"}],"stop_reason":"end_turn"}"#,
    ),
    (
        "whole-message-thinking",
        r#"{"type":"message","content":[{"type":"thinking","thinking":"t"}]}"#,
    ),
    (
        "whole-message-tool-use",
        r#"{"type":"message","content":[{"type":"tool_use","id":"toolu_1","name":"lookup","input":{}}]}"#,
    ),
    (
        "whole-message-tool-use-unnamed",
        r#"{"type":"message","content":[{"type":"tool_use","name":""}]}"#,
    ),
    ("whole-message-empty", r#"{"type":"message","content":[]}"#),
    ("no-type-content", r#"{"content":[{"text":"x"}]}"#),
    ("content-object", r#"{"content":{"text":"x"}}"#),
    ("type-not-string", r#"{"type":["message_stop"]}"#),
];

/// Not upstream's: Gemini chunks, bare and as Gemini CLI wraps them.
const GEMINI: &[(&str, &str)] = &[
    ("empty", ""),
    (
        "text",
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"Hi"}]}}]}"#,
    ),
    (
        "text-empty",
        r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":""}]}}]}"#,
    ),
    (
        "thought-text",
        r#"{"candidates":[{"content":{"parts":[{"thoughtText":"t"}]}}]}"#,
    ),
    (
        "thought-true",
        r#"{"candidates":[{"content":{"parts":[{"thought":true}]}}]}"#,
    ),
    (
        "thought-string",
        r#"{"candidates":[{"content":{"parts":[{"thought":"t"}]}}]}"#,
    ),
    (
        "thought-true-with-text",
        r#"{"candidates":[{"content":{"parts":[{"thought":true,"text":"t"}]}}]}"#,
    ),
    (
        "function-call",
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup","args":{}}}]}}]}"#,
    ),
    (
        "function-call-unnamed",
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"args":{}}}]}}]}"#,
    ),
    (
        "inline-data",
        r#"{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"iVBOR"}}]}}]}"#,
    ),
    (
        "thought-signature-only",
        r#"{"candidates":[{"content":{"parts":[{"thoughtSignature":"sig"}]}}]}"#,
    ),
    (
        "finish-reason",
        r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}]}"#,
    ),
    (
        "usage-only",
        r#"{"usageMetadata":{"promptTokenCount":3,"totalTokenCount":3}}"#,
    ),
    ("candidates-empty", r#"{"candidates":[]}"#),
    (
        "candidates-object",
        r#"{"candidates":{"content":{"parts":[{"text":"x"}]}}}"#,
    ),
    (
        "parts-object",
        r#"{"candidates":[{"content":{"parts":{"text":"x"}}}]}"#,
    ),
    (
        "cli-text",
        r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"Hi"}]}}]}}"#,
    ),
    ("cli-empty-candidates", r#"{"response":{"candidates":[]}}"#),
    (
        "cli-shadowed",
        r#"{"candidates":[],"response":{"candidates":[{"content":{"parts":[{"text":"x"}]}}]}}"#,
    ),
    ("error", r#"{"error":{"code":429,"message":"quota"}}"#),
    ("error-null", r#"{"error":null}"#),
    ("cli-error", r#"{"response":{"error":{"code":500}}}"#),
    (
        "data-no-space",
        r#"data:{"candidates":[{"finishReason":"STOP"}]}"#,
    ),
    ("data-only", "data:"),
    (
        "array-chunk",
        r#"[{"candidates":[{"content":{"parts":[{"text":"x"}]}}]}]"#,
    ),
    (
        "array-continuation",
        r#",{"candidates":[{"content":{"parts":[{"text":"x"}]}}]}"#,
    ),
    (
        "second-candidate",
        r#"{"candidates":[{"content":{"parts":[]}},{"finishReason":"STOP"}]}"#,
    ),
];
