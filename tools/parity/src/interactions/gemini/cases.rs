//! The Gemini and Interactions suites' hand-written cases: the inputs of
//! upstream's tests and of the port's own, then corners upstream's tests
//! don't reach. Generation configs go through both key conversions with the
//! inputs of the port's key conversion tests, all checked with Go, but for
//! keys sjson reads as a query and digits padding an array by more than
//! 65,535 nulls, which the port leaves out on purpose.

use serde_json::json;

use crate::cases::Case;

/// The model upstream's tests translate for.
const MODEL: &str = "gemini-3.5-flash";

/// Interactions requests for a Gemini upstream, from upstream's tests and
/// the port's.
const INTERACTIONS_REQUESTS: &[(&str, &str)] = &[
    (
        "string-input",
        r#"{"model":"gemini-3.5-flash","input":"hello"}"#,
    ),
    (
        "system-and-generation-config",
        r#"{"model":"gemini-3.5-flash","system_instruction":{"text":"be brief"},"generation_config":{"max_output_tokens":32,"top_p":0.8},"input":"hi"}"#,
    ),
    (
        "string-system-instruction",
        r#"{"model":"gemini-3.5-flash","system_instruction":"be brief","input":"hi"}"#,
    ),
    (
        "turn-input",
        r#"{"model":"gemini-3.5-flash","input":{"role":"user","steps":[{"type":"user_input","content":[{"text":"hi"}]}]}}"#,
    ),
    (
        "turn-array-input",
        r#"{"model":"gemini-3.5-flash","input":[{"role":"user","steps":[{"type":"user_input","content":[{"text":"hi"}]}]},{"role":"assistant","steps":[{"type":"model_output","content":[{"text":"ok"}]}]}]}"#,
    ),
    (
        "preserves-expressible-top-level-fields",
        r#"{"model":"gemini-3.5-flash","tool_choice":{"type":"function","function":{"name":"lookup"}},"response_modalities":["text","image"],"service_tier":"priority","input":"hi"}"#,
    ),
    (
        "content-input",
        r#"{"model":"gemini-3.5-flash","input":{"role":"user","parts":[{"text":"hi"}]}}"#,
    ),
    (
        "content-array-input",
        r#"{"model":"gemini-3.5-flash","input":[{"role":"user","parts":[{"text":"hi"}]},{"role":"assistant","parts":[{"text":"ok"}]}]}"#,
    ),
    (
        "image-content",
        r#"{"model":"gemini-3.5-flash","input":[{"type":"user_input","content":[{"type":"image","mime_type":"image/png","data":"aGVsbG8="}]}]}"#,
    ),
    (
        "model-output-typed-content",
        r#"{"model":"gemini-3.5-flash","input":[{"type":"model_output","content":[{"type":"image","mime_type":"image/png","data":"aGVsbG8="},{"type":"document","mime_type":"application/pdf","file_uri":"gs://bucket/doc.pdf"}]}]}"#,
    ),
    (
        "thought-typed-content",
        r#"{"model":"gemini-3.5-flash","input":[{"type":"thought","content":[{"type":"text","text":"thinking"},{"type":"audio","mime_type":"audio/wav","data":"UklGRg=="}]}]}"#,
    ),
    (
        "generation-config-all-fields",
        r#"{"model":"gemini-3.5-flash","generation_config":{"max_output_tokens":32,"response_schema":{"type":"object"},"seed":42,"thinking_config":{"thinking_budget":1024,"include_thoughts":true},"context_window_compression":{"trigger_tokens":1000}},"input":"hi"}"#,
    ),
    (
        "generation-config-protocol-fields",
        r#"{"model":"gemini-3.5-flash","generation_config":{"tool_choice":"auto","thinking_level":"high","thinking_summaries":"auto"},"stream":true,"input":"hi"}"#,
    ),
    (
        "turn-with-model-role",
        r#"{"model":"gemini-3.5-flash","input":{"role":"model","steps":[{"type":"user_input","content":[{"text":"hi"}]},{"type":"model_output","content":[{"text":"ok"}]}]}}"#,
    ),
    (
        "generation-config-preserves-large-integers",
        r#"{"model":"gemini-3.5-flash","generation_config":{"max_output_tokens":32,"large_identity":9223372036854775807},"input":"hi"}"#,
    ),
    (
        "function-call-preserves-call-id",
        r#"{"model":"gemini-3.5-flash","input":[{"type":"function_call","name":"lookup","call_id":"call_1","arguments":{"q":"x"}}]}"#,
    ),
    (
        "function-result-preserves-call-id",
        r#"{"model":"gemini-3.5-flash","input":[{"type":"function_result","name":"lookup","call_id":"call_1","result":{"ok":true}}]}"#,
    ),
    (
        "builtin-tools-options-and-aliases",
        r#"{
            "model":"gemini-3.5-flash",
            "input":"test options",
            "tools":[
                {"type":"url_context","url_context":{"max_urls":3}},
                {"code_execution":{"environment":"sandbox"}},
                {"type":"web_search","google_search":{"mode":"search"}}
            ]
        }"#,
    ),
    (
        "builtin-tools-native-composite",
        r#"{
            "model":"gemini-3.5-flash",
            "input":"native composite tools",
            "tools":[{"googleSearch":{},"urlContext":{}}]
        }"#,
    ),
    (
        "builtin-tools-unrecognized-retained",
        r#"{
            "model":"gemini-3.5-flash",
            "input":"unrecognized tool",
            "tools":[{"type":"file_search","file_search":{"max_results":5}}]
        }"#,
    ),
    (
        "function-response-json-ref",
        r##"{
            "model": "gemini-3.5-flash",
            "input": [
                {
                    "type": "function_result",
                    "name": "lookup",
                    "call_id": "call_1",
                    "result": {
                        "schema": {
                            "$ref": "#/components/schemas/ErrorModel"
                        }
                    }
                }
            ]
        }"##,
    ),
    (
        "parallel-tool-calls-history",
        r#"{
            "model": "gemini-3.5-flash",
            "input": [
                {"type": "user_input", "content": [{"type": "text", "text": "run tools"}]},
                {"type": "thought", "signature": "sig_turn1"},
                {"type": "function_call", "name": "f1", "call_id": "c1", "arguments": {"a": 1}},
                {"type": "function_call", "name": "f2", "call_id": "c2", "arguments": {"b": 2}},
                {"type": "function_call", "name": "f3", "call_id": "c3", "arguments": {"c": 3}},
                {"type": "function_result", "name": "f1", "call_id": "c1", "result": {"r": 1}},
                {"type": "function_result", "name": "f2", "call_id": "c2", "result": {"r": 2}},
                {"type": "function_result", "name": "f3", "call_id": "c3", "result": {"r": 3}}
            ]
        }"#,
    ),
    (
        "thought-summary-and-signature",
        r#"{
            "model": "gemini-3.5-flash",
            "input": [
                {"type": "thought", "summary": [{"type": "text", "text": "my thinking"}], "signature": "sig_thought"},
                {"type": "model_output", "content": [{"type": "text", "text": "my answer"}]}
            ]
        }"#,
    ),
    (
        "interleaved-model-turn-steps",
        r#"{
            "model": "gemini-3.5-flash",
            "input": [
                {"type": "function_call", "name": "f1", "call_id": "c1", "arguments": {"a": 1}, "signature": "sig1"},
                {"type": "model_output", "content": [{"type": "text", "text": "explanation"}]},
                {"type": "function_call", "name": "f2", "call_id": "c2", "arguments": {"b": 2}},
                {"type": "function_result", "name": "f1", "call_id": "c1", "result": {"r": 1}},
                {"type": "function_result", "name": "f2", "call_id": "c2", "result": {"r": 2}}
            ]
        }"#,
    ),
    (
        "multiple-signatures-preserved",
        r#"{
            "model": "gemini-3.5-flash",
            "input": [
                {"type": "thought", "summary": [{"type": "text", "text": "thought 1"}], "signature": "sig_thought_1"},
                {"type": "thought", "signature": "sig_fc_1"},
                {"type": "function_call", "name": "f1", "call_id": "c1", "arguments": {}}
            ]
        }"#,
    ),
    (
        "explicit-signature-clears-pending",
        r#"{
            "model": "gemini-3.5-flash",
            "input": [
                {"type": "thought", "signature": "sig_shared"},
                {"type": "function_call", "name": "f1", "call_id": "c1", "arguments": {}, "signature": "sig_shared"},
                {"type": "function_call", "name": "f2", "call_id": "c2", "arguments": {}}
            ]
        }"#,
    ),
    (
        "normalizes-openai-file-data-url",
        r#"{"model":"gemini-3.5-flash","input":[{"type":"user_input","content":[{"type":"file","file":{"filename":"test.pdf","file_data":"data:application/pdf;base64,JVBERi0xLjQK"}}]}]}"#,
    ),
    (
        "whole-request",
        r#"{"model":"m","system_instruction":"be brief","generation_config":{"temperature":0.2,"max_output_tokens":64,"thinking_level":"low","thinking_summaries":"auto","stop_sequences":["x"]},"response_modalities":["text"],"tools":[{"type":"function","name":"lookup","description":"d","parameters":{"type":"object"}},{"type":"google_search"}],"tool_choice":"any","service_tier":"flex","input":[{"type":"user_input","content":[{"type":"text","text":"hi"},{"type":"image","uri":"gs://b/i.png","mime_type":"image/png"}]},{"type":"thought","summary":[{"type":"text","text":"hmm"}],"signature":"sig_a"},{"type":"function_call","name":"lookup","call_id":"c1","arguments":{"q":"x"}},{"type":"function_result","name":"lookup","call_id":"c1","result":"plain text"},{"type":"model_output","content":[{"type":"text","text":"done"}]}],"stream":true}"#,
    ),
];

/// Gemini requests for an Interactions upstream, from upstream's tests and
/// the port's.
const GEMINI_REQUESTS: &[(&str, &str)] = &[
    (
        "gemini-system-instruction-becomes-a-string",
        r#"{"model":"gemini-3.5-flash","systemInstruction":{"parts":[{"text":"be brief"},{"text":"answer directly"}]},"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    ),
    (
        "gemini-function-call",
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"q":"x"}}}]},{"role":"user","parts":[{"functionResponse":{"name":"lookup","response":{"ok":true}}}]}]}"#,
    ),
    (
        "gemini-text-content-type",
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    ),
    (
        "gemini-multimodal",
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"audio/wav","data":"aGVsbG8="}}]}]}"#,
    ),
    (
        "gemini-thought",
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"model","parts":[{"text":"thinking","thought":true}]}]}"#,
    ),
    (
        "gemini-function-call-preserves-id",
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","id":"call_1","args":{"q":"x"}}}]},{"role":"user","parts":[{"functionResponse":{"name":"lookup","id":"call_1","response":{"ok":true}}}]}]}"#,
    ),
    (
        "gemini-function-call-preserves-call-id",
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","call_id":"call_request_1","args":{"q":"x"}}}]},{"role":"user","parts":[{"functionResponse":{"name":"lookup","call_id":"call_request_1","response":{"ok":true}}}]}]}"#,
    ),
    (
        "gemini-generation-config",
        r#"{"model":"gemini-3.5-flash","generationConfig":{"maxOutputTokens":32,"topP":0.8,"thinkingConfig":{"thinkingBudget":1024}},"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    ),
    (
        "builtin-tools-reverse",
        r#"{
            "model":"gemini-3.5-flash",
            "contents":[{"role":"user","parts":[{"text":"hello"}]}],
            "tools":[{"urlContext":{}},{"codeExecution":{}},{"googleSearch":{}}]
        }"#,
    ),
    (
        "builtin-tools-reverse-options",
        r#"{
            "model":"gemini-3.5-flash",
            "contents":[{"role":"user","parts":[{"text":"hello"}]}],
            "tools":[{"googleSearch":{"mode":"search"}},{"codeExecution":{"sandbox":true}}]
        }"#,
    ),
    (
        "builtin-tools-native-composite-2",
        r#"{
            "model":"gemini-3.5-flash",
            "contents":[{"role":"user","parts":[{"text":"hello"}]}],
            "tools":[{"googleSearch":{},"urlContext":{}}]
        }"#,
    ),
    (
        "gemini-signed-function-call-preserved",
        r#"{
            "model": "gemini-3.5-flash",
            "contents": [
                {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"name": "lookup", "id": "call_1", "args": {"q": "x"}}, "thoughtSignature": "sig_fc_pres"}
                    ]
                }
            ]
        }"#,
    ),
    (
        "whole-gemini-request",
        r#"{"model":"m","systemInstruction":{"parts":[{"text":"a"},{"text":"b"}]},"generationConfig":{"temperature":0.5,"maxOutputTokens":10,"thinkingConfig":{"thinkingLevel":"HIGH","includeThoughts":true},"responseMimeType":"application/json"},"tools":[{"functionDeclarations":[{"name":"f","description":"d","parameters":{"type":"object"}}]},{"googleSearch":{}}],"contents":[{"role":"user","parts":[{"text":"hi"},{"inlineData":{"mimeType":"image/png","data":"AAA="}},{"fileData":{"mimeType":"application/pdf","fileUri":"gs://x"}}]},{"role":"model","parts":[{"text":"think","thought":true},{"functionCall":{"name":"f","args":{"a":1},"id":"c1"},"thoughtSignature":"s1"}]},{"role":"user","parts":[{"functionResponse":{"name":"f","id":"c1","response":{"ok":true}}}]},{"role":"model","parts":[{"text":"done"},{"text":"","thoughtSignature":"s2"}]}]}"#,
    ),
];

/// Gemini responses for an Interactions client, from upstream's tests and
/// the port's.
const GEMINI_RESPONSES: &[(&str, &str)] = &[
    (
        "response-to-request-round-trip",
        r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"name": "lookup", "id": "call_1", "args": {"q": "x"}}, "thoughtSignature": "sig_rt_fc"},
                        {"functionCall": {"name": "search", "id": "call_2", "args": {"q": "y"}}}
                    ]
                }
            }]
        }"#,
    ),
    (
        "trailing-signature-round-trip",
        r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "answer"},
                        {"text": "", "thoughtSignature": "sig_trailing_rt"}
                    ]
                }
            }]
        }"#,
    ),
    (
        "non-stream",
        r#"{"responseId":"resp_1","candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}"#,
    ),
    (
        "non-stream-snake-case-usage",
        r#"{"responseId":"resp_snake","candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usage_metadata":{"prompt_token_count":11,"candidates_token_count":22,"total_token_count":33,"thoughts_token_count":44,"cached_content_token_count":55}}"#,
    ),
    (
        "non-stream-function-call",
        r#"{"responseId":"resp_1","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"q":"x"}}}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3,"cachedContentTokenCount":4}}"#,
    ),
    (
        "non-stream-function-call-preserves-call-id",
        r#"{"responseId":"resp_1","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"lookup","call_id":"call_response_1","args":{"q":"x"}}}]}}]}"#,
    ),
    (
        "non-stream-image",
        r#"{"responseId":"resp_1","candidates":[{"content":{"role":"model","parts":[{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]}}]}"#,
    ),
    (
        "non-stream-thought-signature",
        r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"name": "lookup", "id": "call_1", "args": {"q": "x"}}, "thoughtSignature": "sig_fc"},
                        {"functionCall": {"name": "search", "id": "call_2", "args": {"q": "y"}}}
                    ]
                }
            }]
        }"#,
    ),
    (
        "whole-gemini-response",
        r#"{"responseId":"r1","candidates":[{"content":{"role":"model","parts":[{"text":"t","thought":true},{"text":"a"},{"inlineData":{"mimeType":"image/png","data":"AA=="}},{"functionCall":{"name":"f","args":{"x":1},"id":"c1"},"thoughtSignature":"s"},{"text":"","thoughtSignature":"s2"}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}"#,
    ),
];

/// Interactions responses for a Gemini client, from upstream's tests and
/// the port's.
const INTERACTIONS_RESPONSES: &[(&str, &str)] = &[
    (
        "interactions-non-stream-function-call",
        r#"{"id":"i1","model":"gemini-3.1-flash-lite","steps":[{"type":"function_call","call_id":"call_1","signature":"sig_1","name":"get_weather","arguments":{"location":"北京"}}],"usage":{"total_input_tokens":2,"total_output_tokens":3,"total_tokens":5}}"#,
    ),
    (
        "interactions-function-call-args-preserve-ref",
        r##"{
            "id": "i1",
            "model": "gemini-3.1-flash-lite",
            "steps": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "validate_schema",
                    "arguments": {
                        "schema": {
                            "$ref": "#/components/schemas/ErrorModel"
                        }
                    }
                }
            ]
        }"##,
    ),
    (
        "interactions-function-result-with-ref",
        r##"{
            "id": "i1",
            "model": "gemini-3.1-flash-lite",
            "steps": [
                {
                    "type": "function_result",
                    "call_id": "call_1",
                    "name": "get_schema",
                    "result": {
                        "schema": {
                            "$ref": "#/components/schemas/ErrorModel"
                        }
                    }
                }
            ]
        }"##,
    ),
    (
        "whole-interactions-response",
        r#"{"id":"i9","model":"gemini-y","steps":[{"type":"thought","content":[{"type":"text","text":"t"}]},{"type":"model_output","content":[{"type":"text","text":"a"},{"type":"image","mime_type":"image/png","data":"AA=="}]},{"type":"function_call","id":"c1","name":"f","arguments":"{\"x\":1}","signature":"s"},{"type":"function_result","call_id":"c1","name":"f","result":"not json"}],"usage":{"input_tokens":1,"output_tokens":2,"reasoning_tokens":3,"cached_tokens":4}}"#,
    ),
];

/// Generation configs, from the port's tests of its key conversions.
const CONFIGS: &[&str] = &[
    r#"{"max_output_tokens":5,"thinking_config":{"thinking_budget":1.50,"include_thoughts":true},"stop_sequences":["a","b"],"empty":{},"earr":[],"n":null}"#,
    r#"[1,2,3]"#,
    r#"[{"a_b":1},{"c_d":2}]"#,
    r#"5"#,
    r#""str""#,
    r#"null"#,
    r#"{"":{"x_y":1}}"#,
    r#"{"":1,"z":2}"#,
    r#"{"a.b":1,"a":{"c":2}}"#,
    r#"{"a\b":1}"#,
    r#"{":0":1,":x":2}"#,
    r#"{"x":[[1,2],[3]]}"#,
    r#"{"m_x":1,"mX":2}"#,
    r#"{"m_x":{"a":1},"mX":{"b":2}}"#,
    r#"{"m_x":[1],"mX":[2]}"#,
    r#"{"m_x":{"a":1},"mX":5}"#,
    r#"{"m_x":5,"mX":{"a":1}}"#,
    r#"{"a_é":1,"b_ébc":2,"É_x":3,"aÉb":4,"ABC":5}"#,
    r#"{"a__b_":1,"_c":2,"d_1":3}"#,
    r#"{"0":1,"1":2}"#,
    r#"{"a":{"0":1}}"#,
    r#"{"a":{"5":1}}"#,
    r#"{"a":{"-1":1,"-1x":2}}"#,
    r#"{"a_":{"b":1}}"#,
    r#"{"a":{"":{"b":1}}}"#,
    r#"{"a":{"":5}}"#,
    r#"{"a":[{"":1}]}"#,
    r#"{"a":[{}, {"b":1}, [], 5]}"#,
    r#"{"a\\b":1}"#,
    r#"{"a\\\\.b":1}"#,
    r#"{"a\\":1}"#,
    r#"{":":1,"x:y":2}"#,
    r#"{"a":{":":1}}"#,
    r#"{"a":{":-1":1}}"#,
    r#"{"a":[1],"a_":{":-1":2}}"#,
    r#"{"a":[1],"a_":{"-1":2}}"#,
    r#"{"a":[1],"a_":{"3":2}}"#,
    r#"{"a":[1],"a_":{"0":2}}"#,
    r#"{"a":[1],"a_":{"x":2}}"#,
    r#"{"a":[1],"a_":{"":2}}"#,
    r#"{"a":{"x":1},"a_":[2]}"#,
    r#"{"a":{"0":1},"a_":[2]}"#,
    r#"{"":[1],"x":2}"#,
    r#"{"a":"x","a_":[2]}"#,
    r#"{"a.":1}"#,
    r#"{".a":1}"#,
    r#"{"a..b":1}"#,
    r#"{"a":{"b.":{"c":1}}}"#,
    r#"{"a":"é"}"#,
    r#"{"a":1.0e10,"b":-0,"c":123456789012345678901234567890}"#,
    r#"{"a b":1,"a\tb":2}"#,
];

/// More Interactions requests for a Gemini upstream: corners of each field
/// the translator reads.
const MORE_INTERACTIONS_REQUESTS: &[(&str, &str)] = &[
    (
        "builtin-url-context",
        r#"{"model":"gemini-3.5-flash","input":"read url","tools":[{"type":"url_context"}]}"#,
    ),
    (
        "builtin-code-execution",
        r#"{"model":"gemini-3.5-flash","input":"execute code","tools":[{"type":"code_execution"}]}"#,
    ),
    (
        "builtin-google-search",
        r#"{"model":"gemini-3.5-flash","input":"search web","tools":[{"type":"google_search"}]}"#,
    ),
    (
        "round-trip-calls",
        r#"{"model":"gemini-3.5-flash","input":[{"type":"user_input","content":[{"type":"text","text":"hello"}]},{"type":"thought","signature":"sig_rt_fc"},{"type":"function_call","name":"lookup","arguments":{"q":"x"},"call_id":"call_1"},{"type":"function_call","name":"search","arguments":{"q":"y"},"call_id":"call_2"},{"type":"function_result","name":"lookup","call_id":"call_1","result":{"ok":true}},{"type":"function_result","name":"search","call_id":"call_2","result":{"found":true}}]}"#,
    ),
    (
        "round-trip-trailing-signature",
        r#"{"model":"gemini-3.5-flash","input":[{"type":"user_input","content":[{"type":"text","text":"hello"}]},{"type":"model_output","content":[{"text":"answer"}]},{"type":"thought","signature":"sig_trailing_rt"},{"type":"user_input","content":[{"type":"text","text":"next"}]}]}"#,
    ),
    ("no-model", r#"{"input":"hi"}"#),
    (
        "camel-generation-config",
        r#"{"model":"m","input":"hi","generationConfig":{"maxOutputTokens":5,"thinkingLevel":"LOW","thinkingBudget":0,"includeThoughts":false,"toolChoice":"auto","thinkingSummaries":" None "}}"#,
    ),
    (
        "both-generation-configs",
        r#"{"model":"m","input":"hi","generation_config":{"top_k":3,"thinking_summaries":"detailed"},"generationConfig":{"topK":4}}"#,
    ),
    (
        "thinking-summaries-not-a-string",
        r#"{"model":"m","input":"hi","generation_config":{"thinking_summaries":{"a":1},"thinking_config":{"include_thoughts":true}}}"#,
    ),
    (
        "tool-choice-none",
        r#"{"model":"m","input":"hi","tool_choice":"none"}"#,
    ),
    (
        "tool-choice-padded-auto",
        r#"{"model":"m","input":"hi","tool_choice":" AUTO "}"#,
    ),
    (
        "tool-choice-required",
        r#"{"model":"m","input":"hi","tool_choice":"required"}"#,
    ),
    (
        "tool-choice-validated",
        r#"{"model":"m","input":"hi","tool_choice":"validated"}"#,
    ),
    (
        "tool-choice-function-padded-name",
        r#"{"model":"m","input":"hi","tool_choice":{"type":" Function ","function":{"name":" lookup "}}}"#,
    ),
    (
        "tool-choice-function-no-name",
        r#"{"model":"m","input":"hi","tool_choice":{"type":"function"}}"#,
    ),
    (
        "tool-choice-tool",
        r#"{"model":"m","input":"hi","tool_choice":{"type":"tool","name":"search"}}"#,
    ),
    (
        "tool-choice-object-modes",
        r#"{"model":"m","input":"hi","tool_choice":{"type":"any"}}"#,
    ),
    (
        "tool-choice-unknown-object",
        r#"{"model":"m","input":"hi","tool_choice":{"type":"allowed_tools","tools":["a"]}}"#,
    ),
    (
        "tool-choice-number",
        r#"{"model":"m","input":"hi","tool_choice":5}"#,
    ),
    (
        "tool-choice-in-generation-config",
        r#"{"model":"m","input":"hi","generation_config":{"tool_choice":{"type":"function","function":{"name":"f"}}}}"#,
    ),
    (
        "tool-choice-in-camel-generation-config",
        r#"{"model":"m","input":"hi","generationConfig":{"toolChoice":"any"}}"#,
    ),
    (
        "tool-choice-name-object",
        r#"{"model":"m","input":"hi","tool_choice":{"type":"tool","name":{"a":1}}}"#,
    ),
    (
        "response-modalities",
        r#"{"model":"m","input":"hi","response_modalities":["text","IMAGE"," audio ","video",1,null]}"#,
    ),
    (
        "response-modalities-camel",
        r#"{"model":"m","input":"hi","responseModalities":["Text"],"generation_config":{"response_modalities":["AUDIO"]}}"#,
    ),
    (
        "response-modalities-none-known",
        r#"{"model":"m","input":"hi","response_modalities":["video"]}"#,
    ),
    (
        "response-modalities-not-a-list",
        r#"{"model":"m","input":"hi","response_modalities":"text"}"#,
    ),
    (
        "service-tier-not-a-string",
        r#"{"model":"m","input":"hi","service_tier":5}"#,
    ),
    (
        "service-tier",
        r#"{"model":"m","input":"hi","service_tier":" Flex "}"#,
    ),
    (
        "tools-not-a-list",
        r#"{"model":"m","input":"hi","tools":{"googleSearch":{}}}"#,
    ),
    (
        "tools-with-declarations-pass-through",
        r#"{"model":"m","input":"hi","tools":[{"type":"url_context"},{"functionDeclarations":[{"name":"f"}]}]}"#,
    ),
    (
        "tools-snake-declarations",
        r#"{"model":"m","input":"hi","tools":[{"function_declarations":[{"name":"f","parameters":{"type":"object"}}]},{"type":"function","function_declarations":{"name":"g"}}]}"#,
    ),
    (
        "tools-functions",
        r#"{"model":"m","input":"hi","tools":[{"type":"function","name":"f","description":"d","parameters":{"type":"object","properties":{"q":{"type":"string"}}}},{"name":{"a":1},"description":{"b":[1, 2]}},{"name":"g","parameters":null}]}"#,
    ),
    (
        "tools-generic",
        r#"{"model":"m","input":"hi","tools":[{"z":1,"a":{"y":0.1,"b":1.50,"c":1e3,"d":-0}},{"type":"","url_context":{},"web_search":{"x":1}},{"type":"other","google_search":{}},"text",5,null,[1]]}"#,
    ),
    (
        "tools-generic-huge-number",
        r#"{"model":"m","input":"hi","tools":[{"type":"x","n":1e400},{"type":"y","n":123456789012345678901234567890}]}"#,
    ),
    (
        "tools-built-in-config-not-an-object",
        r#"{"model":"m","input":"hi","tools":[{"type":"url_context","url_context":5,"urlContext":{"a":1}},{"type":"code_execution","codeExecution":[]},{"type":"web_search"}]}"#,
    ),
    (
        "tools-html",
        r#"{"model":"m","input":"hi","tools":[{"type":"x","note":"<b>&</b>"}]}"#,
    ),
    (
        "system-instruction-text-object",
        r#"{"model":"m","input":"hi","system_instruction":{"text":{"b": [1, 2]}}}"#,
    ),
    (
        "system-instruction-parts-and-text",
        r#"{"model":"m","input":"hi","system_instruction":{"parts":[{"text":"a"}],"text":"b"}}"#,
    ),
    (
        "system-instruction-number",
        r#"{"model":"m","input":"hi","system_instruction":5}"#,
    ),
    (
        "system-instruction-camel",
        r#"{"model":"m","input":"hi","systemInstruction":"ignored"}"#,
    ),
    (
        "chat-image-urls",
        r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}},{"type":"image_url","image_url":{"url":"https://example.com/a.png"}},{"type":"image_url","image_url":{"url":"data:image/png,raw"}},{"type":"image_url","image_url":"data:image/png;base64,AA=="}]}]}"#,
    ),
    (
        "chat-input-audio",
        r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"input_audio","input_audio":{"format":"wav","data":"AA=="}},{"type":"input_audio","input_audio":{"format":" FLAC ","data":"AQ=="}},{"type":"input_audio","input_audio":{"format":"opus","data":"Ag=="}},{"type":"input_audio","input_audio":{"format":"pcm16","data":"Aw=="}},{"type":"input_audio","input_audio":{"format":"mp3","data":"BA=="}},{"type":"input_audio","input_audio":{"data":"BQ=="}},{"type":"input_audio","input_audio":{"format":"wav"}}]}]}"#,
    ),
    (
        "chat-files",
        r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"file","file":{"filename":"a.pdf","file_data":"data:application/pdf;base64,JVBERi0="}},{"type":"file","file":{"filename":"notes.txt","file_data":"aGVsbG8="}},{"type":"file","file":{"filename":"image.PNG","file_data":"data:text/plain,hello"}},{"type":"file","file":{"file_data":"JVBERi0="}},{"type":"file","file":{"filename":"x.unknownext","file_data":"AA=="}},{"type":"file","file":{"filename":"a.pdf"}}]}]}"#,
    ),
    (
        "media-parts",
        r#"{"model":"m","input":[{"type":"user_input","content":[{"type":" IMAGE ","mimeType":"image/png","data":"AA=="},{"type":"document","mime_type":"application/pdf","file_uri":"gs://bucket/a.pdf"},{"type":"video","fileUri":"https://example.com/v.mp4","mimeType":"video/mp4"},{"type":"audio","url":"data:audio/wav;base64,UklGRg=="},{"type":"image","url":"https://example.com/a.png"},{"type":"image","mime_type":"","data":"AA=="},{"type":"image","mime_type":"image/png","data":""},{"type":"image","mime_type":"image/png","data":"","file_uri":"gs://x"},{"type":"image","inline_data":{"mime_type":"image/png","data":"AA=="}},{"inlineData":{"mimeType":"image/gif","data":"R0lG"}},{"type":"unknown"}]}]}"#,
    ),
    (
        "media-mime-type-object",
        r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"image","mime_type":{"a":1},"data":{"b":2}}]}]}"#,
    ),
    (
        "native-parts",
        r#"{"model":"m","input":[{"type":"user_input","role":" Assistant ","parts":[{"text":"a"},{"inlineData":{"mimeType":"image/png","data":"AA=="}},{"fileData":{"mime_type":"application/pdf","file_uri":"gs://a"}},{"inline_data":{"mime_type":"image/png","data":"AQ=="}},{"file_data":{"mimeType":"text/plain","fileUri":"gs://b"}},{"functionCall":{"name":"f","args":{"q":1}}},{"functionResponse":{"name":"f","response":{"r":2}}},{"inlineData":{"mimeType":"","data":"AA=="}},{"other":1}]},{"role":"user","parts":[{"other":1}]},{"type":"custom","role":"model","parts":[{"text":"b"}]},{"parts":[{"text":"c"}],"role":"function"}]}"#,
    ),
    (
        "native-parts-not-a-list",
        r#"{"model":"m","input":[{"type":"user_input","parts":{"text":"a"}}]}"#,
    ),
    (
        "turns-and-roles",
        r#"{"model":"m","input":[{"role":"model","steps":[{"type":"model_output","content":"hi"},"plain text"]},{"role":"assistant","steps":[{"type":"user_input","content":"x"}]},{"role":"other","steps":[{"steps":[{"type":"model_output","text":"nested"}],"role":"user"}]},{"role":"user","steps":"not a list"}]}"#,
    ),
    (
        "single-turn-model",
        r#"{"model":"m","input":{"role":"assistant","steps":[{"type":"function_call","name":"f","id":"c1"},"text"]}}"#,
    ),
    (
        "single-step",
        r#"{"model":"m","input":{"type":"function_result","call_id":"c1","name":"f","result":"done"}}"#,
    ),
    ("input-number", r#"{"model":"m","input":5}"#),
    (
        "input-strings",
        r#"{"model":"m","input":["a",{"type":"model_output","content":"b"},"c"]}"#,
    ),
    (
        "function-results",
        r##"{"model":"m","input":[{"type":"function_call","name":"f","call_id":"c1","arguments":"{\"a\":1}"},{"type":"function_call","name":"g","id":"c2","arguments":[1]},{"type":"function_call","name":"h"},{"type":"function_result","call_id":"c1","name":"f","result":"{\"$ref\":\"#/a\"}"},{"type":"function_result","id":"c2","name":"g","result":{"$ref":"#/components/b","x":1}},{"type":"function_result","call_id":"c3","name":"h","result":[1,{"a":2}]},{"type":"function_result","call_id":"c4","result":null},{"type":"function_result","call_id":"c5","name":"k","result":5,"is_error":true},{"type":"function_result","call_id":"c6","name":"k","result":"plain"},{"type":"function_result","call_id":"c7","name":"k"},{"type":"user_input","content":"after"},{"type":"function_result","call_id":"c8","name":"k","result":{"ok":true}}]}"##,
    ),
    (
        "function-call-names-and-ids",
        r#"{"model":"m","input":[{"type":"function_call","name":{"a":1},"call_id":{"b":2},"id":"ignored","arguments":{}},{"type":"function_call","name":7,"id":true}]}"#,
    ),
    (
        "signatures",
        r#"{"model":"m","input":[{"type":"thought","signature":"A"},{"type":"thought","thought_signature":"B","summary":[{"type":"text","text":"sum"}]},{"type":"model_output","content":"out"},{"type":"thought","thoughtSignature":" C "},{"type":"function_call","name":"f","signature":"D"},{"type":"thought","signature":"E"},{"type":"function_call","name":"g"},{"type":"thought","signature":"F","text":"t"},{"type":"thought","signature":"F"},{"type":"function_call","name":"h","thought_signature":"F"},{"type":"thought","signature":"G"},{"type":"function_result","call_id":"x","name":"h","result":"r"},{"type":"thought","signature":"H"}]}"#,
    ),
    (
        "signature-objects",
        r#"{"model":"m","input":[{"type":"thought","signature":{"s": 1}},{"type":"function_call","name":"f","signature":"  ","thoughtSignature":{"t":2}}]}"#,
    ),
    (
        "step-types",
        r#"{"model":"m","input":[{"type":"","content":"empty type"},{"type":"other","content":[{"type":"text","text":"x"}]},{"type":"other","text":"y"},{"type":"other","text":{"z":1}},{"type":"other"},{"type":"model_output","text":"alias"},{"type":"model_output"},{"type":"thought"},{"type":"user_input","content":{"type":"text","text":"one part"}},{"type":"user_input","content":5}]}"#,
    ),
    (
        "text-parts",
        r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"text","text":{"a":[1, 2]}},{"type":"text","text":5},{"type":"text","text":null},{"type":"text"},{"text":"no type"}]}]}"#,
    ),
];

/// More Gemini requests for an Interactions upstream.
const MORE_GEMINI_REQUESTS: &[(&str, &str)] = &[
    (
        "builtin-tools-snake",
        r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"tools":[{"url_context":{}},{"code_execution":{"a":1}},{"google_search":{"x":[]},"googleSearch":{"y":1}},{"urlContext":5}]}"#,
    ),
    (
        "function-tools",
        r#"{"contents":[],"tools":[{"name":"f","description":{"a":1},"parametersJsonSchema":{"type":"object"}},{"function_declarations":[{"name":"g","parameters":{"type":"object"},"parametersJsonSchema":{"x":1}},{"description":"no name"}]},{"functionDeclarations":{"a":{"name":"h"}}},{"functionDeclarations":[{"name":7,"description":"d"}],"googleSearch":{}}]}"#,
    ),
    (
        "tools-not-a-list",
        r#"{"contents":[],"tools":{"googleSearch":{}}}"#,
    ),
    (
        "tools-none-known",
        r#"{"contents":[],"tools":[{"other":1}]}"#,
    ),
    (
        "system-instruction-string",
        r#"{"systemInstruction":"be brief","contents":[]}"#,
    ),
    (
        "system-instruction-text",
        r#"{"system_instruction":{"text":"be brief","parts":[{"text":"ignored"}]},"contents":[]}"#,
    ),
    (
        "system-instruction-parts",
        r#"{"systemInstruction":{"text":5,"parts":[{"text":"a"},{"text":""},{"text":{"b": [1, 2]}},{"inlineData":{}},{"text":7}]},"contents":[]}"#,
    ),
    (
        "system-instruction-empty",
        r#"{"systemInstruction":{"parts":[{"text":""}]},"contents":[]}"#,
    ),
    (
        "thinking-config",
        r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":1024,"includeThoughts":true,"thinkingLevel":" HIGH "}}}"#,
    ),
    (
        "thinking-config-snake",
        r#"{"contents":[],"generationConfig":{"thinking_config":{"thinking_level":"low","include_thoughts":false,"thinking_budget":"x"}}}"#,
    ),
    (
        "thinking-summaries-kept",
        r#"{"contents":[],"generationConfig":{"thinkingSummaries":"detailed","thinkingConfig":{"includeThoughts":true}}}"#,
    ),
    (
        "thinking-level-object",
        r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingLevel":{"A": [1, 2]},"includeThoughts":"yes"}}}"#,
    ),
    (
        "generation-config-snake-key",
        r#"{"contents":[],"generation_config":{"maxOutputTokens":5}}"#,
    ),
    (
        "parts",
        r#"{"contents":[{"role":"user","parts":[{"text":"a","thought":true},{"text":"","thoughtSignature":"s1"},{"text":""},{"text":{"b":1}},{"text":5},{"inlineData":{"mime_type":"image/png","data":"AA=="}},{"inline_data":{"mime_type":"audio/wav","data":"AQ=="}},{"inlineData":{"mimeType":"video/mp4","mime_type":"x","data":"Ag=="}},{"inlineData":{"mimeType":"application/pdf"}},{"fileData":{"mimeType":"text/plain","fileUri":"gs://a"}},{"executableCode":{"code":"x"}}]},{"role":"model","parts":[{"text":"t","thought":"true"},{"text":"u","thought":1},{"text":"v","thought":true,"thoughtSignature":"s2"},{"text":"w","thought_signature":"s3"},{"text":"x","extra_content":{"google":{"thought_signature":"s4"}}},{"thoughtSignature":"s5"},{"inlineData":{"mimeType":"image/png","data":"AA=="},"thoughtSignature":"s6"},{"inline_data":{"mime_type":"image/jpeg","data":"AQ=="}}]}]}"#,
    ),
    (
        "calls-and-responses",
        r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"f","args":{"a":1},"id":"c1"},"thoughtSignature":"s1"},{"functionCall":{"name":"g","call_id":"c2"}},{"functionCall":{"name":{"n":1},"id":{"i":2},"args":"text"}},{"functionCall":{}}]},{"role":"user","parts":[{"functionResponse":{"name":"f","id":"c1","response":{"ok":true}}},{"functionResponse":{"name":"g","call_id":"c2","response":"plain"}},{"functionResponse":{"name":"h"}},{"functionResponse":{"name":{"x":1},"id":7,"response":[1]},"thoughtSignature":"s2"}]}]}"#,
    ),
    (
        "contents-shapes",
        r#"{"contents":{"a":{"role":"user","parts":[{"text":"one"}]},"b":{"role":"model","parts":{"x":{"text":"two"}}}},"generationConfig":{"temperature":0.5}}"#,
    ),
    (
        "roles",
        r#"{"contents":[{"role":"function","parts":[{"text":"f"}]},{"role":"","parts":[{"text":"e","thought":true}]},{"role":" model","parts":[{"text":"m","thought":true}]},{"parts":[{"text":"none"}]},{"role":"model"}]}"#,
    ),
    (
        "no-contents",
        r#"{"generationConfig":{"maxOutputTokens":5}}"#,
    ),
    ("not-an-object", r#"[1,2]"#),
];

/// Gemini streams for an Interactions client: upstream's tests' chunks, the
/// port's whole stream, then corners.
const GEMINI_STREAMS: &[(&str, &[&str])] = &[
    (
        "stream-function-call-call-id",
        &[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"lookup","call_id":"call_stream_1","args":{"q":"x"}}}]}}]}"#,
        ],
    ),
    (
        "stream-function-call-thought-signature",
        &[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"thinking","thought":true}]}}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"I will call the tool."}]}}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"thoughtSignature":"sig-call","functionCall":{"name":"lookup","id":"call_1","args":{"q":"x"}}}]}}]}"#,
        ],
    ),
    (
        "stream-step-lifecycle",
        &[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"thinking","thought":true}]}}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"answer"}]}}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"lookup","id":"call_1","args":{"q":"x"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":7,"thoughtsTokenCount":2}}"#,
        ],
    ),
    (
        "stream-snake-case-usage",
        &[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usage_metadata":{"prompt_token_count":11,"candidates_token_count":22,"total_token_count":33,"thoughts_token_count":44,"cached_content_token_count":55}}"#,
        ],
    ),
    (
        "stream-emits-terminal-once",
        &[
            r#"{"candidates":[{"finishReason":"STOP"}]}"#,
            r#"{"candidates":[],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}"#,
            "[DONE]",
        ],
    ),
    (
        "stream-does-not-complete-on-non-terminal-usage",
        &[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"thinking"}]}}],"usageMetadata":{"promptTokenCount":124,"totalTokenCount":124}}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"好的，我将为您调用天气查询工具。"}]}}],"usageMetadata":{"promptTokenCount":124,"candidatesTokenCount":17,"totalTokenCount":452,"thoughtsTokenCount":311}}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"location":"北京"},"id":"nriii75p"}}]}}],"usageMetadata":{"promptTokenCount":124,"candidatesTokenCount":33,"totalTokenCount":468,"thoughtsTokenCount":311}}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":124,"candidatesTokenCount":33,"totalTokenCount":468,"thoughtsTokenCount":311}}"#,
        ],
    ),
    (
        "stream-ignores-traffic-only-usage-metadata",
        &[
            r#"{"candidates":[{"content":{"role":"model","parts":[]}}],"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"}}"#,
        ],
    ),
    (
        "stream-completes-on-done-without-usage",
        &[r#"{"candidates":[{"finishReason":"STOP"}]}"#, "[DONE]"],
    ),
    (
        "stream-trailing-thought-signature",
        &[
            r#"{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"sig_trailing"}]}}]}"#,
        ],
    ),
    (
        "whole-gemini-stream",
        &[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"plan","thought":true}]}}]}"#,
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"ok","thoughtSignature":"s1"}]}}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"f","args":{"a":1}}},{"functionResponse":{"name":"f","response":{"r":2}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":9,"thoughtsTokenCount":2,"cachedContentTokenCount":1}}"#,
            "[DONE]",
        ],
    ),
    (
        "two-calls-without-ids-in-one-chunk",
        &[
            r#"{"responseId":"r1","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"a","args":{"x":1}}},{"functionCall":{"name":"b"}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1}}"#,
        ],
    ),
    (
        "parts-of-every-kind",
        &[
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"a"},{"text":"b","thoughtSignature":"s1"},{"text":"c","thought":true,"thought_signature":"s2"},{"text":"","thoughtSignature":"s3"},{"inlineData":{"mimeType":"image/png","data":"AA=="}},{"inlineData":{"mimeType":"image/png","data":"AA=="},"thoughtSignature":"s4"},{"executableCode":{"code":"x"}},{"text":{"o":[1, 2]}},{"text":5}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":{"n":1},"id":{"i":2},"args":"text"}},{"functionCall":{"name":"f","call_id":"c2","args":[1,2]},"thoughtSignature":"s5"},{"functionCall":{}},{"functionResponse":{"name":"f","id":"c3","response":"plain"}},{"functionResponse":{"name":{"r":1}}},{"text":"after","thought":"true"}]}}]}"#,
        ],
    ),
    (
        "finish-then-usage-then-more",
        &[
            r#"{"candidates":[{"content":{"parts":[{"text":"a"}]},"finishReason":"MAX_TOKENS"}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"b"}]}}]}"#,
            r#"{"usage_metadata":{"total_token_count":5,"cached_content_token_count":0,"cachedContentTokenCount":2}}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"late"}]},"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":9}}"#,
            "[DONE]",
            "[DONE]",
            r#"{"candidates":[{"content":{"parts":[{"text":"after done"}]}}]}"#,
        ],
    ),
    (
        "usage-before-finish",
        &[
            r#"{"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2}}"#,
            r#"{"candidates":[{"finishReason":""}],"usageMetadata":{"prompt_token_count":"3","candidates_token_count":1e30}}"#,
        ],
    ),
    (
        "odd-chunks",
        &[
            "",
            "   ",
            "not json",
            " [DONE] \n",
            r#"{"candidates":{"0":{"content":{"parts":[{"text":"object candidates"}]}}}}"#,
        ],
    ),
    (
        "data-lines",
        &[
            r#"data: {"candidates":[{"content":{"parts":[{"text":"a"}]},"finishReason":"STOP"}]}"#,
            "data: [DONE]",
        ],
    ),
    (
        "two-candidates",
        &[
            r#"{"candidates":[{"content":{"parts":[{"text":"first"}]}},{"content":{"parts":[{"text":"second"}]},"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":1}}"#,
        ],
    ),
    (
        "trailing-text-after-chunk",
        &[r#" {"candidates":[{"content":{"parts":[{"text":"a"}]}}]} trailing"#],
    ),
];

/// More Gemini responses for an Interactions client.
const MORE_GEMINI_RESPONSES: &[(&str, &str)] = &[
    (
        "no-response-id",
        r#"{"candidates":[{"content":{"parts":[{"text":"a"}]}}]}"#,
    ),
    ("not-json", "not json"),
    ("empty", ""),
    (
        "no-candidates",
        r#"{"responseId":"r1","candidates":[],"usageMetadata":{"trafficType":"ON_DEMAND"}}"#,
    ),
    (
        "usage-mixed",
        r#"{"responseId":"r1","usage_metadata":{"prompt_token_count":1},"usageMetadata":{"candidatesTokenCount":2,"thoughts_token_count":3,"cached_content_token_count":0}}"#,
    ),
    (
        "usage-thoughts-only",
        r#"{"responseId":"r1","usageMetadata":{"thoughtsTokenCount":"7"}}"#,
    ),
    (
        "parts-of-every-kind",
        r#"{"responseId":{"id":[1, 2]},"candidates":[{"content":{"role":"model","parts":[{"text":"a","thought":true,"thoughtSignature":"s1"},{"text":"","thoughtSignature":"s2"},{"text":""},{"text":{"o":1}},{"thoughtSignature":"s3"},{"inline_data":{"mime_type":"audio/wav","data":"AA=="}},{"inlineData":{"mime_type":"video/mp4","data":"AQ=="},"thoughtSignature":"s4"},{"inlineData":{"mimeType":"application/pdf","data":"Ag=="}},{"inlineData":{"mimeType":"text/plain"}},{"functionCall":{"name":"f","call_id":"c1","args":"text"},"thought_signature":"s5"},{"functionCall":{"name":{"n":1}}},{"functionResponse":{"name":"f","id":"c1","response":{"ok":true}}},{"functionResponse":{"call_id":"c2"}},{"text":"x","extra_content":{"google":{"thought_signature":" s6 "}}},{"executableCode":{}}]}}]}"#,
    ),
    (
        "candidates-object",
        r#"{"candidates":{"0":{"content":{"parts":[{"text":"a"}]}}},"responseId":"r2"}"#,
    ),
];

/// Interactions streams for a Gemini client: upstream's tests' events, the
/// port's whole stream, then corners.
const INTERACTIONS_STREAMS: &[(&str, &str, &[&str])] = &[
    (
        "interactions-stream-function-call",
        "gemini-3.1-flash-lite",
        &[
            r#"data: {"interaction":{"id":"i1","model":"gemini-3.1-flash-lite"},"event_type":"interaction.created"}"#,
            r#"data: {"index":0,"step":{"type":"function_call","id":"call_1","signature":"sig_1","name":"get_weather","arguments":{}},"event_type":"step.start"}"#,
            r#"data: {"index":0,"delta":{"type":"arguments_delta","arguments":"{\"location\":\"北京\"}"},"event_type":"step.delta"}"#,
            r#"data: {"interaction":{"id":"i1","status":"requires_action","usage":{"total_input_tokens":2,"total_output_tokens":3,"total_tokens":5,"total_thought_tokens":1,"total_cached_tokens":4},"service_tier":"standard","model":"gemini-3.1-flash-lite"},"event_type":"interaction.completed"}"#,
            "event: done\ndata: [DONE]",
        ],
    ),
    (
        "interactions-stream-finish-metadata-usage",
        "gemini-test",
        &[
            r#"data: {"event_type":"finish","metadata":{"total_usage":{"total_input_tokens":2,"total_output_tokens":6,"total_thought_tokens":3,"total_cached_tokens":1,"total_tokens":11}}}"#,
        ],
    ),
    (
        "interactions-response-failed-permission",
        "devin/kimi-k3",
        &[
            r#"data: {"event_type":"response.failed","error":{"message":"devin upstream error: permission_denied","code":"403"}}"#,
        ],
    ),
    (
        "interactions-response-failed-quota",
        "devin/kimi-k3",
        &[
            r#"data: {"event_type":"interaction.failed","error":{"message":"quota exceeded","code":"resource_exhausted"}}"#,
        ],
    ),
    (
        "interactions-response-failed-unavailable",
        "devin/kimi-k3",
        &[
            r#"data: {"event_type":"response.failed","interaction":{"error":{"message":"service unavailable","code":"503"}}}"#,
        ],
    ),
    (
        "interactions-response-failed-bare",
        "devin/kimi-k3",
        &[r#"data: {"event_type":"response.failed"}"#],
    ),
    (
        "whole-interactions-stream",
        "gemini-x",
        &[
            r#"data: {"interaction":{"id":"i1","model":"gemini-y"},"event_type":"interaction.created"}"#,
            r#"{"index":0,"step":{"type":"thought"},"event_type":"step.start"}"#,
            r#"{"index":0,"delta":{"type":"thought_summary","content":{"type":"text","text":"hmm"}},"event_type":"step.delta"}"#,
            r#"{"index":0,"delta":{"type":"thought_signature","signature":"sig"},"event_type":"step.delta"}"#,
            "event: step.start\ndata: {\"index\":1,\"step\":{\"type\":\"function_call\",\"id\":\"c1\",\"name\":\"f\"},\"event_type\":\"step.start\"}",
            r#"{"index":1,"delta":{"type":"arguments_delta","arguments":"{\"a\": [1, 2]}"},"event_type":"step.delta"}"#,
            r#"{"index":2,"delta":{"type":"text","text":"done"},"event_type":"step.delta"}"#,
            r#"{"index":2,"delta":{"type":"text","text":""},"event_type":"step.delta"}"#,
            r#"{"interaction":{"id":"i1","status":"completed","service_tier":"flex","usage":{"total_input_tokens":5,"total_output_tokens":6}},"event_type":"interaction.completed"}"#,
            "data: [DONE]",
        ],
    ),
    (
        "error-codes",
        "gemini-x",
        &[
            r#"{"event_type":"response.failed","error":{"message":"a","code":"400"}}"#,
            r#"{"event_type":"response.failed","error":{"message":"b","code":" UNAUTHENTICATED "}}"#,
            r#"{"event_type":"response.failed","error":{"message":"c","code":"not_found"}}"#,
            r#"{"event_type":"response.failed","error":{"message":"d","code":"rate_limit_exceeded"}}"#,
            r#"{"event_type":"response.failed","error":{"message":"e","code":"canceled"}}"#,
            r#"{"event_type":"response.failed","error":{"message":"f","code":"deadline_exceeded"}}"#,
            r#"{"event_type":"response.failed","error":{"message":"g","code":"internal"}}"#,
            r#"{"event_type":"response.failed","error":{"message":"h","code":502}}"#,
            r#"{"event_type":"response.failed","error":{"message":"i","code":"418"}}"#,
            r#"{"event_type":"response.failed","error":{"message":"j","code":"600"}}"#,
            r#"{"event_type":"response.failed","error":{"message":"k","code":"+503"}}"#,
            r#"{"event_type":"interaction.failed","code":"401","error":{"message":"l"}}"#,
            r#"{"event_type":"interaction.failed","error":{"message":"m","status":"UNAVAILABLE"}}"#,
            r#"{"event_type":"interaction.failed","error":{"message":{"n":[1, 2]},"code":"  "}}"#,
            r#"{"event_type":"interaction.failed","error":"text"}"#,
        ],
    ),
    (
        "deltas",
        "gemini-x",
        &[
            r#"{"event_type":"interaction.created","interaction":{"id":{"o":1},"model":"  "}}"#,
            r#"{"event_type":"step.delta","index":5,"delta":{"type":"arguments_delta","arguments":"{}"},"step":{"name":"from root"}}"#,
            r#"{"event_type":"step.start","index":1,"step":{"type":"function_call","call_id":"c1","id":"ignored","name":{"n":1},"thoughtSignature":"s1"}}"#,
            r#"{"event_type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":" not json "}}"#,
            r#"{"event_type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":"  [1, {\"a\": 2}]  "}}"#,
            r#"{"event_type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":""}}"#,
            r#"{"event_type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":{"a":1}}}"#,
            r#"{"event_type":"step.delta","index":1,"delta":{"type":"thought_signature","thought_signature":"s2"}}"#,
            r#"{"event_type":"step.delta","index":1,"delta":{"type":"arguments_delta","arguments":"{\"b\":1}"}}"#,
            r#"{"event_type":"step.delta","index":"2","delta":{"type":"thought_signature","thoughtSignature":" s3 "}}"#,
            r#"{"event_type":"step.delta","index":2,"delta":{"type":"thought_signature","signature":""}}"#,
            r#"{"event_type":"step.delta","delta":{"type":"text","content":{"text":"from content"}}}"#,
            r#"{"event_type":"step.delta","delta":{"type":"text","text":"  ","content":{"text":"fallback"}}}"#,
            r#"{"event_type":"step.delta","delta":{"type":"thought_summary","text":"from text"}}"#,
            r#"{"event_type":"step.delta","delta":{"type":"thought_summary","content":{"text":{"t":1}}}}"#,
            r#"{"event_type":"step.delta","delta":{"type":"image","data":"AA=="}}"#,
            r#"{"event_type":"interaction.status_update","status":"in_progress"}"#,
            r#"{"event_type":"interaction.completed","interaction":{"id":"later","model":"m2","service_tier":{"t":1},"usage":{"input_tokens":1,"total_input_tokens":2,"output_tokens":3,"reasoning_tokens":4,"cached_tokens":5}}}"#,
        ],
    ),
    (
        "sse-frames",
        "",
        &[
            "event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\n data: \"interaction\":{\"id\":\"i2\"}}\n\n",
            ": comment\ndata: {\"event_type\":\"step.delta\",\"delta\":{\"type\":\"text\",\"text\":\"x\"}}",
            "event: step.delta\n",
            "data:[DONE]",
            "",
            "  \n",
            "[DONE]",
            r#"{"event_type":"finish","metadata":{"total_usage":{"output_tokens":9223372036854775807,"input_tokens":1}}}"#,
            r#"{"event_type":"interaction.completed"}"#,
        ],
    ),
    (
        "completed-without-created",
        "gemini-x",
        &[
            r#"{"event_type":"step.delta","delta":{"type":"text","text":"a"}}"#,
            r#"{"event_type":"interaction.completed","interaction":{"id":"i3","model":"gemini-z","usage":{}}}"#,
        ],
    ),
];

/// More Interactions responses for a Gemini client.
const MORE_INTERACTIONS_RESPONSES: &[(&str, &str, &str)] = &[
    (
        "interaction-wrapper",
        "gemini-x",
        r#"{"interaction":{"id":"","model":"  ","service_tier":"flex","steps":[{"type":"model_output","content":"wrapped"}]},"id":"root-id","model":"root-model","steps":[{"type":"model_output","content":"root"}],"usage":{"total_tokens":3}}"#,
    ),
    (
        "no-id",
        "gemini-x",
        r#"{"steps":[{"type":"model_output","content":[{"type":"text","text":"a"}]}],"service_tier":"priority"}"#,
    ),
    ("no-model", "", r#"{"id":"i1","steps":[]}"#),
    (
        "calls",
        "gemini-x",
        r#"{"id":"i1","steps":[{"type":"function_call","name":"f","arguments":"not json"},{"type":"function_call","name":"g","args":" {\"a\":1} ","call_id":"c2","id":"ignored"},{"type":"function_call","name":{"n":1},"id":{"i":2},"thoughtSignature":{"s":3}},{"type":"function_call","name":"h","arguments":[1],"signature":"  ","thought_signature":"s4"},{"type":"function_call","name":"k","arguments":""}]}"#,
    ),
    (
        "results",
        "gemini-x",
        r##"{"id":"i1","steps":[{"type":"function_result","name":"f","call_id":"c1","result":"[1,2]"},{"type":"function_result","name":"f","id":"c2","result":"   "},{"type":"function_result","name":"f","result":"plain"},{"type":"function_result","name":"f","response":{"r":1}},{"type":"function_result","name":"f","result":5},{"type":"function_result","name":"f","result":null},{"type":"function_result","name":"f","result":{"$ref":"#/x"}},{"type":"function_result","name":"f","result":"{\"$ref\":\"#/y\"}"},{"type":"function_result","name":{"n":1},"id":{"i":2}}]}"##,
    ),
    (
        "contents",
        "gemini-x",
        r#"{"id":"i1","steps":[{"type":"thought","content":"thinking"},{"type":"thought","content":{"type":"text","text":"one"}},{"type":"model_output","content":{"type":"image","mime_type":"image/png","data":"AA=="}},{"type":"other","content":[{"type":"text","text":"x"},{"type":"audio","mime_type":"audio/wav","data":"AQ=="}]},{"type":"model_output"},{"content":"no type"}]}"#,
    ),
    (
        "steps-object",
        "gemini-x",
        r#"{"id":"i1","steps":{"a":{"type":"model_output","content":"a"}}}"#,
    ),
    ("not-json", "gemini-x", "not json"),
    ("empty", "gemini-x", ""),
    (
        "error-only",
        "gemini-x",
        r#"{"error":{"message":"bad","code":400}}"#,
    ),
    (
        "usage-names",
        "gemini-x",
        r#"{"id":"i1","steps":[],"usage":{"total_input_tokens":"4","output_tokens":9007199254740993,"total_thought_tokens":2,"total_cached_tokens":1}}"#,
    ),
    // Go's int64 of an out-of-range float depends on the CPU, and the port
    // saturates: alone the counts compare alike, but not summed, so the
    // total is given.
    (
        "usage-out-of-range",
        "gemini-x",
        r#"{"id":"i1","steps":[],"usage":{"input_tokens":123456789012345678901234567890,"output_tokens":1e30,"total_tokens":-1e30,"reasoning_tokens":1e300}}"#,
    ),
];

/// Interactions requests, events and bodies passed through.
const PASSTHROUGH_REQUESTS: &[(&str, &str)] = &[
    (
        "request",
        r#"{"model":"gemini-x","input":"hi","stream":true,"n":1.50}"#,
    ),
    ("array", r#"[1,2]"#),
    ("string", r#""text""#),
    ("number", "5"),
    ("null", "null"),
    (
        "pretty",
        "{\n  \"model\": \"m\",\n  \"input\": [ { \"type\": \"user_input\", \"content\": \"caf\\u00e9\" } ]\n}",
    ),
];

const PASSTHROUGH_STREAMS: &[(&str, &[&str])] = &[
    (
        "events",
        &[
            r#"{"event_type":"interaction.created","interaction":{"id":"i1"}}"#,
            "",
            " ",
            "data: {}",
            "event: x\ndata: y",
            "[DONE]",
            "\n",
        ],
    ),
    ("none", &[]),
];

const PASSTHROUGH_RESPONSES: &[(&str, &[&str])] = &[
    ("not-json", &["not json"]),
    ("empty", &[""]),
    ("json", &[r#"{"id":1}"#]),
    ("padded", &["  {\"a\":1}  \n"]),
    ("no-body", &[]),
];

/// Upstream's request for a request case, with the stream flag every other
/// case asks for.
fn requests(tables: &[&[(&str, &str)]], model: &str) -> Vec<Case> {
    tables
        .iter()
        .flat_map(|table| table.iter())
        .enumerate()
        .map(|(index, &(name, body))| {
            Case::new(name, model, body).with_options(json!({ "stream": index % 2 == 1 }))
        })
        .collect()
}

/// Interactions requests for a Gemini upstream.
pub fn interactions_requests() -> Vec<Case> {
    let mut cases = requests(&[INTERACTIONS_REQUESTS, MORE_INTERACTIONS_REQUESTS], MODEL);
    for (index, config) in CONFIGS.iter().enumerate() {
        let body = format!(r#"{{"model":"m","input":"hi","generation_config":{config}}}"#);
        cases.push(Case::new(format!("config-{index}"), MODEL, body));
    }
    for (index, data) in [
        "ab\u{7}cd",
        "ab\u{7f}cd",
        "ab\u{e0001}cd",
        "ab\u{1f600}cd",
        "ab\u{85}cd",
        "ab\tcd",
    ]
    .into_iter()
    .enumerate()
    {
        let body = json!({
            "model": "m",
            "input": [{ "type": "user_input", "content": [
                { "type": "image", "mime_type": "image/png", "data": data },
                { "type": "image", "mime_type": data, "file_uri": data },
            ] }],
        });
        cases.push(Case::new(
            format!("quoted-data-{index}"),
            MODEL,
            body.to_string(),
        ));
    }
    cases.push(Case::new(
        "model-without-a-replacement",
        "",
        r#"{"model":"client","input":"hi"}"#,
    ));
    cases
}

/// Gemini requests for an Interactions upstream.
pub fn gemini_requests() -> Vec<Case> {
    let mut cases = requests(&[GEMINI_REQUESTS, MORE_GEMINI_REQUESTS], MODEL);
    for (index, config) in CONFIGS.iter().enumerate() {
        let body = format!(r#"{{"contents":[],"generationConfig":{config}}}"#);
        cases.push(Case::new(format!("config-{index}"), MODEL, body));
    }
    cases.push(
        Case::new("no-model", "", r#"{"contents":[]}"#).with_options(json!({ "stream": true })),
    );
    cases
}

/// A response case for `model`, answering an empty request.
fn response(name: &str, model: &str, events: &[&str]) -> Case {
    let events = events.iter().map(|event| (*event).to_owned()).collect();
    Case {
        model: model.to_owned(),
        ..Case::response(name, "{}", events)
    }
}

/// Gemini streams for an Interactions client.
pub fn gemini_streams() -> Vec<Case> {
    let mut cases: Vec<Case> = GEMINI_STREAMS
        .iter()
        .map(|&(name, chunks)| response(name, MODEL, chunks))
        .collect();
    cases.push(response(
        "no-model",
        "",
        &[r#"{"candidates":[]}"#, "[DONE]"],
    ));
    cases.push(response("no-chunks", MODEL, &[]));
    cases
}

/// Gemini responses for an Interactions client.
pub fn gemini_responses() -> Vec<Case> {
    let mut cases: Vec<Case> = GEMINI_RESPONSES
        .iter()
        .chain(MORE_GEMINI_RESPONSES)
        .map(|&(name, body)| response(name, MODEL, &[body]))
        .collect();
    cases.push(response("no-body", MODEL, &[]));
    cases.push(response("no-model", "", &[r#"{"responseId":"r1"}"#]));
    cases
}

/// Interactions streams for a Gemini client.
pub fn interactions_streams() -> Vec<Case> {
    INTERACTIONS_STREAMS
        .iter()
        .map(|&(name, model, events)| response(name, model, events))
        .collect()
}

/// Interactions responses for a Gemini client.
pub fn interactions_responses() -> Vec<Case> {
    let upstream = INTERACTIONS_RESPONSES
        .iter()
        .map(|&(name, body)| response(name, "gemini-3.1-flash-lite", &[body]));
    let more = MORE_INTERACTIONS_RESPONSES
        .iter()
        .map(|&(name, model, body)| response(name, model, &[body]));
    upstream.chain(more).collect()
}

/// Interactions requests passed through.
pub fn passthrough_requests() -> Vec<Case> {
    requests(&[PASSTHROUGH_REQUESTS], "gemini-x")
}

/// Interactions streams passed through.
pub fn passthrough_streams() -> Vec<Case> {
    PASSTHROUGH_STREAMS
        .iter()
        .map(|&(name, events)| response(name, "gemini-x", events))
        .collect()
}

/// Interactions responses passed through.
pub fn passthrough_responses() -> Vec<Case> {
    PASSTHROUGH_RESPONSES
        .iter()
        .map(|&(name, events)| response(name, "gemini-x", events))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::Value;

    use super::*;

    #[test]
    fn names_are_unique_and_requests_are_json() {
        for cases in [
            interactions_requests(),
            gemini_requests(),
            passthrough_requests(),
            gemini_streams(),
            gemini_responses(),
            interactions_streams(),
            interactions_responses(),
            passthrough_streams(),
            passthrough_responses(),
        ] {
            let mut names = HashSet::new();
            for case in &cases {
                assert!(names.insert(case.name.clone()), "{}", case.name);
                assert!(
                    serde_json::from_str::<Value>(&case.request).is_ok(),
                    "{}",
                    case.name
                );
            }
        }
    }
}
