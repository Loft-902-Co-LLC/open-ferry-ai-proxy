//! Hand-written cases for the Chat Completions and Interactions suites: the
//! inputs of upstream's tests (but for the Antigravity ones, whose branches
//! are not ported), the whole inputs of our unit tests, whose expected
//! outputs these cases check against upstream's, and corners the generators
//! are unlikely to build.

use serde_json::json;

use crate::cases::Case;

/// A request case, translated for a stream when `stream` is set.
fn request(name: &str, model: &str, body: &str, stream: bool) -> Case {
    Case::new(name, model, body).with_options(json!({ "stream": stream }))
}

/// A response case for `model`: an event stream, or one body.
fn response(name: &str, model: &str, events: &[&str]) -> Case {
    Case {
        model: model.to_owned(),
        ..Case::response(
            name,
            "{}",
            events.iter().map(|&event| event.to_owned()).collect(),
        )
    }
}

/// Chat Completions requests for the translator to Interactions.
pub fn chat_requests() -> Vec<Case> {
    vec![
        // TestConvertOpenAIRequestToInteractionsMapsMessagesToolsAndStream
        request(
            "maps-messages-tools-and-stream",
            "gemini-3.1-flash-lite",
            r#"{"model":"gemini-3.1-flash-lite","stream":true,"messages":[{"role":"system","content":"be brief"},{"role":"user","content":"今天北京的天气怎么样？"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"weather","parameters":{"type":"object","properties":{"location":{"type":"string"}},"required":["location"]}}}],"tool_choice":"auto","max_completion_tokens":128}"#,
            false,
        ),
        // TestConvertOpenAIRequestToInteractionsMapsToolCallsAndResults
        request(
            "maps-tool-calls-and-results",
            "gemini-3.1-flash-lite",
            r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}]}"#,
            false,
        ),
        // TestConvertOpenAIRequestToInteractionsInfersToolNamesForOutOfOrderResults
        request(
            "infers-tool-names-for-out-of-order-results",
            "gemini-3.1-flash-lite",
            r#"{
                "model": "gemini-3.1-flash-lite",
                "messages": [
                    {"role": "assistant", "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"bj\"}"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_2", "content": "sunny"},
                    {"role": "tool", "tool_call_id": "call_1", "content": "found"}
                ]
            }"#,
            true,
        ),
        // TestConvertOpenAIRequestToInteractions_PreservesEnvironmentIDAndPreviousInteractionID,
        // with a Gemini model.
        request(
            "preserves-environment-id-and-previous-interaction-id",
            "gemini-3.1-flash-lite",
            r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"continue"}],"previous_response_id":"v1_prev123","environment_id":"env_456"}"#,
            false,
        ),
        // TestConvertOpenAIRequestToInteractionsRenamesConflictingAntigravityTools
        // and ...RenamesConflictingToolChoice, for a Gemini model.
        request(
            "keeps-tool-names-and-choice",
            "gemini-3.1-flash-lite",
            r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"read it"}],"tools":[
                {"type":"function","function":{"name":"read_file","description":"r","parameters":{"type":"object"}}},
                {"type":"function","function":{"name":"write_file","description":"w","parameters":{"type":"object"}}},
                {"type":"function","function":{"name":"execute_code","description":"e","parameters":{"type":"object"}}},
                {"type":"function","function":{"name":"web_search","description":"s","parameters":{"type":"object"}}}
            ],"tool_choice":{"type":"function","function":{"name":"read_file"}}}"#,
            false,
        ),
        // TestConvertOpenAIRequestToInteractionsNormalizesFileDataURL
        request(
            "normalizes-file-data-url",
            "gemini-3.5-flash",
            r#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"file","file":{"filename":"test.pdf","file_data":"data:application/pdf;base64,JVBERi0xLjQK"}}]}]}"#,
            false,
        ),
        // TestConvertOpenAIRequestToInteractionsPreservesRawFileDataWithMIMEType
        request(
            "preserves-raw-file-data-with-mime-type",
            "gemini-3.5-flash",
            r#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"document","mime_type":"application/pdf","data":"JVBERi0xLjQK"}]}]}"#,
            false,
        ),
        // The unit tests' whole request.
        request(
            "whole",
            "",
            r#"{"model":"gemini-3-pro","n":2,"stop":["x"],"reasoning_effort":" High ","modalities":["text"],"service_tier":"flex","response_format":{"type":"json_object"},"previous_interaction_id":"p1","environment":{"id":"e1"},"agent_config":{"a":1},"temperature":0.5,"messages":[
                {"role":"developer","content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]},
                {"role":" System ","content":{"text":"three"}},
                {"role":"user","content":[{"type":"input_text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,aGk="}},{"type":"image_url","image_url":"https://x/y.png"},{"type":"input_audio","input_audio":{"data":"UklG","format":"WAV"}},{"type":"audio"},{"type":"input_file","filename":"a.txt","file_data":"aGk="},{"type":"file","file":{"file_url":"https://x/f"}},{"type":"file"},{"type":"refusal"}]},
                {"role":"assistant","reasoning_content":[{"text":"hmm"},{"content":" "},{"content":"ok"}],"content":"sure","tool_calls":[{"id":"c1","function":{"name":"f","arguments":"not json"}},{"type":"custom","function":{"name":"g"}},{"id":"c3","type":"function"}]},
                {"role":"function","id":"c1","content":{"x":1}}
            ],"tools":[{"type":"function","name":"flat","description":7},{"type":"web_search"},{"function":{}}]}"#,
            false,
        ),
        // The `stream` flag fills in only a missing `stream`.
        request(
            "stream-not-bool",
            "m",
            r#"{"stream":"f","messages":"x"}"#,
            true,
        ),
        request("stream-missing", "m", "{}", true),
        // Generation settings under each of their names, and the first of
        // them that is set.
        request(
            "generation-settings",
            "m",
            r#"{"max_output_tokens":5,"max_tokens":6,"presence_penalty":0.5,"frequency_penalty":-1,"stop":"s","response_modalities":["TEXT"],"modalities":["audio"],"tool_choice":"none","messages":[{"role":"user","content":[]},{"role":"user","content":null},{"role":"tool","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]}"#,
            false,
        ),
        // Files typed by their name, by a MIME type, by a `data:` URL, or
        // not at all.
        request(
            "files",
            "m",
            r#"{"messages":[{"role":"user","content":[
                {"type":"file","file":{"filename":"notes.TXT","file_data":"aGk="}},
                {"type":"file","file":{"filename":"x.bin","file_data":"aGk=","mimeType":"text/csv"}},
                {"type":"file","file":{"file_data":"data:text/plain;base64,aGk=","filename":"a.pdf"}},
                {"type":"file","file":{"file_data":"data:;base64,aGk="}},
                {"type":"file","file":{"file_data":"aGk="}},
                {"type":"input_file","file_data":"aGk=","mime_type":"application/pdf","file_url":"https://f"},
                {"type":"image","data":"aGk=","mime_type":"image/gif"},
                {"type":"image","url":"data:image/png;BASE64,aGk="},
                {"type":"image","image_url":"data:image/png;charset=x;base64,aGk="},
                {"type":"audio","data":"aGk=","format":"pcm16"},
                {"type":"input_audio","input_audio":{"data":"aGk=","format":"aac"}}
            ]}]}"#,
            false,
        ),
    ]
}

/// Interactions requests for the translator to Chat Completions.
pub fn interactions_requests() -> Vec<Case> {
    vec![
        // TestConvertInteractionsRequestToOpenAIPreservesExpressibleFields
        request(
            "preserves-expressible-fields",
            "gpt-test",
            r#"{"model":"gpt-test","tool_choice":{"type":"function","function":{"name":"lookup"}},"response_modalities":["text","image"],"service_tier":"priority","input":"hi"}"#,
            false,
        ),
        // TestConvertInteractionsRequestToOpenAIAcceptsImageContent
        request(
            "accepts-image-content",
            "gpt-test",
            r#"{"model":"gpt-test","input":[{"type":"user_input","content":[{"type":"image","mime_type":"image/png","data":"aGVsbG8="}]}]}"#,
            false,
        ),
        // TestConvertInteractionsRequestToOpenAIPreservesNonImageMediaContent
        request(
            "preserves-non-image-media-content",
            "gpt-test",
            r#"{"model":"gpt-test","input":[{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav","data":"UklGRg=="},{"type":"video","mime_type":"video/mp4","data":"AAAAIGZ0eXA="},{"type":"document","mime_type":"application/pdf","data":"JVBERi0="}]}]}"#,
            false,
        ),
        // TestConvertInteractionsRequestToOpenAIWithToolMessagesDirect
        request(
            "with-tool-messages-direct",
            "gpt-test",
            r#"{"model":"gpt-test","input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]},{"type":"function_call","name":"lookup","call_id":"call_1","arguments":{"q":"x"}},{"type":"function_result","name":"lookup","call_id":"call_1","result":{"ok":true}}]}"#,
            true,
        ),
        // The unit tests' whole request.
        request(
            "whole",
            "gpt-x",
            r#"{"model":"m0","stream":"true","system_instruction":{"parts":[{"text":"a"},{"content":{"text":"b"}}]},"input":[
                "plain",
                {"type":"user_input","content":[{"type":"text","text":"x"},{"text":"y"}]},
                {"type":"user_input","content":[{"type":"text","text":"x"},{"type":"image","url":"https://i"},{"type":"audio","data":"QQ==","mime_type":"audio/ogg"},{"type":"video","data":"Vg=="},{"type":"document","mime_type":"image/svg+xml","data":"PHN2Zz4="},{"type":"file","url":"https://f","filename":"f.bin"},{"type":"unknown"}]},
                {"type":"model_output","content":"done"},
                {"type":"thought","content":{"text":"t"}},
                {"type":"function_call","id":"fc","name":"f","arguments":{"a":1}},
                {"type":"function_result","call_id":"fc","output":[1]},
                {"type":"user_input"}
            ],"tools":[{"name":"t1","parameters":{"type":"object"}},{"function_declarations":[{"name":"d1","description":"dd","parametersJsonSchema":{"type":"string"}}]},{"type":"function","function":{"name":"t2","description":null}}],
            "generation_config":{"temperature":0.2,"maxOutputTokens":10,"thinking_config":{"thinking_level":" LOW "},"stop_sequences":"s"},
            "top_p":0.3,"n":3,"response_modalities":["text"],"response_format":{"type":"text"},"service_tier":5,"previous_response_id":"pr","environment_id":"env","agent_config":null,"parallel_tool_calls":false,"seed":7,"user":"u"}"#,
            false,
        ),
        // The first reasoning setting that is a string wins, even a blank one.
        request(
            "blank-reasoning-setting",
            "m",
            r#"{"generationConfig":{"thinkingLevel":7,"thinking_level":" "},"reasoning_effort":"high"}"#,
            false,
        ),
        request(
            "null-generation-config",
            "m",
            r#"{"generation_config":null,"generationConfig":{"temperature":1},"reasoning_effort":"HIGH"}"#,
            true,
        ),
        // Turns with roles, and steps whose content is in another shape.
        request(
            "turns",
            "m",
            r#"{"input":[{"role":"model","content":"a"},{"role":"user","parts":[{"text":"b"}]},{"role":"system","steps":[{"type":"user_input","content":"c"}]},{"type":"model_output","content":[{"type":"image","data":"aGk="}]},{"type":"function_result","name":"f","result":"r","is_error":true},{"type":"function_call","arguments":"{\"a\":"}],"input_text":"ignored"}"#,
            false,
        ),
    ]
}

/// Interactions event streams for the translator to Chat Completions.
pub fn interactions_streams() -> Vec<Case> {
    vec![
        // TestConvertInteractionsResponseToOpenAIStreamToolCall
        response(
            "stream-tool-call",
            "gemini-3.1-flash-lite",
            &[
                r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"gemini-3.1-flash-lite"}}"#,
                r#"data: {"event_type":"step.start","index":0,"step":{"type":"function_call","id":"call_1","name":"get_weather","arguments":{}}}"#,
                r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"arguments_delta","arguments":"{\"location\":\"北京\"}"}}"#,
                r#"data: {"event_type":"step.stop","index":0}"#,
                r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"requires_action","usage":{"total_input_tokens":2,"total_output_tokens":3,"total_tokens":5}}}"#,
            ],
        ),
        // TestConvertInteractionsResponseToOpenAIStreamFinishMetadataUsage
        response(
            "stream-finish-metadata-usage",
            "gpt-test",
            &[
                r#"data: {"event_type":"finish","metadata":{"total_usage":{"total_input_tokens":2,"total_output_tokens":6,"total_thought_tokens":3,"total_cached_tokens":1,"total_tokens":11}}}"#,
            ],
        ),
        // TestConvertInteractionsResponseToOpenAIStream_PreservesEnvironmentID,
        // with a Gemini model.
        response(
            "stream-preserves-environment-id",
            "gemini-3.1-flash-lite",
            &[
                r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"gemini-3.1-flash-lite","environment_id":"env_chat_stream456"}}"#,
            ],
        ),
        // TestConvertInteractionsResponseToOpenAIStreamToolCall_ContiguousZeroBasedIndex
        response(
            "stream-tool-call-contiguous-index",
            "devin/swe-2",
            &[
                r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"devin/swe-2"}}"#,
                r#"data: {"event_type":"step.start","index":0,"step":{"type":"thought"}}"#,
                r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","text":"planning..."}}"#,
                r#"data: {"event_type":"step.stop","index":0}"#,
                r#"data: {"event_type":"step.start","index":1,"step":{"type":"model_output"}}"#,
                r#"data: {"event_type":"step.delta","index":1,"delta":{"type":"text","text":"I will call tool."}}"#,
                r#"data: {"event_type":"step.stop","index":1}"#,
                r#"data: {"event_type":"step.start","index":2,"step":{"type":"function_call","id":"call_first","name":"write_file","arguments":{}}}"#,
                r#"data: {"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"{\"path\":\"a\"}"}}"#,
                r#"data: {"event_type":"step.stop","index":2}"#,
                r#"data: {"event_type":"step.start","index":3,"step":{"type":"function_call","id":"call_second","name":"read_file","arguments":{}}}"#,
                r#"data: {"event_type":"step.delta","index":3,"delta":{"type":"arguments_delta","arguments":"{\"path\":\"b\"}"}}"#,
                r#"data: {"event_type":"step.stop","index":3}"#,
                r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"completed"}}"#,
                "data: [DONE]",
            ],
        ),
        // TestConvertInteractionsResponseToOpenAIStream_IncompleteFinishReasonLength
        response(
            "stream-incomplete-finish-reason-length",
            "devin/swe-2",
            &[
                r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"devin/swe-2"}}"#,
                r#"data: {"event_type":"step.start","index":0,"step":{"type":"function_call","id":"call_1","name":"write_file"}}"#,
                r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"arguments_delta","arguments":"{\"path\":\"a\""}}"#,
                r#"data: {"event_type":"step.stop","index":0}"#,
                r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"incomplete","finish_reason":"length"}}"#,
                "data: [DONE]",
            ],
        ),
        // TestConvertInteractionsResponseToOpenAI_ContentFilterFinishReason
        response(
            "stream-content-filter",
            "devin/swe-2",
            &[
                r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"devin/swe-2"}}"#,
                r#"data: {"event_type":"step.start","index":0,"step":{"type":"model_output"}}"#,
                r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"text","text":"blocked"}}"#,
                r#"data: {"event_type":"step.stop","index":0}"#,
                r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"incomplete","finish_reason":"content_filter"}}"#,
                "data: [DONE]",
            ],
        ),
        // TestConvertInteractionsResponseToOpenAI_ResponseFailed
        response(
            "response-failed-top-level",
            "devin/kimi-k3",
            &[
                r#"data: {"event_type":"response.failed","error":{"message":"devin upstream error (permission_denied): Unable to process request due to an MCP configuration issue.","code":"403"}}"#,
            ],
        ),
        response(
            "interaction-failed-nested",
            "devin/kimi-k3",
            &[
                r#"data: {"event_type":"interaction.failed","interaction":{"error":{"message":"rate limit exceeded","code":"429"}}}"#,
            ],
        ),
        response(
            "response-failed-fallback",
            "devin/kimi-k3",
            &[r#"data: {"event_type":"response.failed"}"#],
        ),
        // The unit tests' whole stream.
        response(
            "whole",
            "m",
            &[
                r#"data: {"event_type":"interaction.created","interaction":{"id":"i9","model":"gm","environment":{"id":"env1"}}}"#,
                "event: step.start\ndata: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"thought\"}}",
                r#"{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","content":{"text":"plan"}}}"#,
                r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","text":"  "}}"#,
                r#"data: {"event_type":"step.delta","index":1,"delta":{"type":"text","text":" "}}"#,
                r#"data: {"event_type":"step.start","index":5,"step":{"type":"function_call","call_id":"cc","id":"ii","name":"f"}}"#,
                r#"data: {"event_type":"step.delta","index":5,"delta":{"type":"arguments_delta","arguments":{"x":1}}}"#,
                r#"data: {"event_type":"step.delta","index":9,"delta":{"type":"arguments_delta"}}"#,
                r#"data: {"event_type":"step.start","index":7,"step":{"type":"function_call"}}"#,
                r#"data: {"event_type":"done"}"#,
                r#"data: {"event_type":"interaction.completed","interaction":{"status":"incomplete","usage":{"input_tokens":4,"total_input_tokens":9,"total_output_tokens":2,"total_cached_tokens":1}},"environment_id":"env2"}"#,
                r#"data: {"event_type":"finish"}"#,
                r#"data: {"event_type":"interaction.failed","error":{"message":"bad","type":"t","code":503}}"#,
                "data: [DONE]",
            ],
        ),
        // No interaction ID: each chunk makes one up.
        response(
            "no-id",
            "",
            &[
                r#"{"event_type":"step.delta","index":0,"delta":{"type":"text","text":"a"}}"#,
                r#"{"event_type":"step.delta","index":0,"delta":{"type":"text","text":"b"}}"#,
                r#"{"event_type":"interaction.completed","interaction":{"model":"late","status":"requires_action"}}"#,
                r#"{"event_type":"interaction.completed"}"#,
            ],
        ),
        // Lines that aren't events.
        response(
            "not-events",
            "m",
            &[
                "",
                "data:",
                "not json",
                "data: [DONE]",
                "event: ping",
                "[1,2]",
            ],
        ),
    ]
}

/// Interactions responses for the translator to Chat Completions.
pub fn interactions_finals() -> Vec<Case> {
    vec![
        // TestConvertInteractionsResponseToOpenAINonStreamToolCall
        response(
            "non-stream-tool-call",
            "gemini-3.1-flash-lite",
            &[
                r#"{"id":"i1","model":"gemini-3.1-flash-lite","steps":[{"type":"function_call","id":"call_1","name":"get_weather","arguments":{"location":"北京"}}],"usage":{"total_input_tokens":2,"total_output_tokens":3,"total_tokens":5}}"#,
            ],
        ),
        // TestConvertInteractionsResponseToOpenAINonStream_PreservesEnvironmentID,
        // with a Gemini model.
        response(
            "non-stream-preserves-environment-id",
            "gemini-3.1-flash-lite",
            &[
                r#"{"id":"i1","model":"gemini-3.1-flash-lite","environment_id":"env_chat123","steps":[{"type":"model_output","content":[{"type":"text","text":"hello"}]}],"usage":{"total_tokens":5}}"#,
            ],
        ),
        // TestConvertInteractionsResponseToOpenAIPreservesNonCollidingAndNonAntigravityNames,
        // its half for a Gemini model.
        response(
            "non-stream-preserves-tool-names",
            "gemini-3.1-flash-lite",
            &[
                r#"{"id":"i2","model":"gemini-3.1-flash-lite","steps":[{"type":"function_call","id":"call_2","name":"external_read_file","arguments":{"path":"/etc/hosts"}}]}"#,
            ],
        ),
        // TestConvertInteractionsResponseToOpenAI_ContentFilterFinishReason
        response(
            "non-stream-content-filter",
            "devin/swe-2",
            &[
                r#"{"id":"i1","model":"devin/swe-2","status":"incomplete","finish_reason":"content_filter","steps":[{"type":"model_output","content":[{"type":"text","text":"blocked"}]}]}"#,
            ],
        ),
        // The unit tests' whole response.
        response(
            "whole",
            "mm",
            &[
                r#"{"interaction":{"id":"","model":"","steps":[{"type":"thought","content":[{"text":"r1"},{"content":{"text":"r2"}}]},{"type":"model_output","content":"a"},{"type":"model_output","content":[{"type":"text","text":"b"},"junk"]},{"type":"function_call","name":"f","arguments":"{}"},{"type":"function_call","id":"x2","arguments":null}],"environment":{"id":"e"},"finish_reason":"max_tokens"},"id":"outer","usage":{"total_tokens":3,"reasoning_tokens":1}}"#,
            ],
        ),
        // Text only, its status giving the finish reason.
        response(
            "text-requires-action",
            "",
            &[
                r#"{"model":"up","status":"requires_action","steps":[{"type":"model_output","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]}"#,
            ],
        ),
        response("empty", "m", &[""]),
        response("not-json", "m", &["data: {\"id\":\"x\"}"]),
        response("no-body", "m", &[]),
    ]
}

/// Chat Completions streams for the translator to Interactions.
pub fn chat_streams() -> Vec<Case> {
    const FINISH: &str = r#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
    vec![
        // TestConvertOpenAIResponseToInteractionsStreamUsageOnlyTerminalChunk
        response(
            "stream-usage-only-terminal-chunk",
            "gpt-test",
            &[
                FINISH,
                r#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","model":"gpt-test","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}"#,
                "data: [DONE]",
            ],
        ),
        // TestConvertOpenAIResponseToInteractionsCompletesOnDoneWithoutUsage
        response(
            "completes-on-done-without-usage",
            "gpt-test",
            &[FINISH, "data: [DONE]"],
        ),
        // TestConvertOpenAIResponseToInteractionsStreamCreatedUsesChunkIdentity
        response(
            "stream-created-uses-chunk-identity",
            "",
            &[
                r#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","model":"gpt-test","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
            ],
        ),
        // TestConvertOpenAIResponseToInteractionsStreamToolCall
        response(
            "stream-tool-call",
            "gpt-test",
            &[
                r#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","model":"gpt-test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},"finish_reason":null}]}"#,
            ],
        ),
        // The unit tests' whole stream.
        response(
            "whole",
            "m",
            &[
                r#"data: {"id":"c1","choices":[{"delta":{"role":"assistant","reasoning_content":"think"}}]}"#,
                r#"{"choices":[{"delta":{"reasoning_content":[{"text":"more"}],"content":"hi"}}]}"#,
                "event: chunk\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\"\"}}]}}]}",
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}},{"index":1,"function":{"name":"g"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens_details":{"reasoning_tokens":2}}}"#,
                "data: [DONE]",
                "data: [DONE]",
            ],
        ),
        // No ID, no model and no `[DONE]`; a finish with usage in the same
        // chunk.
        response(
            "no-id",
            "",
            &[
                r#"data: {"choices":[{"delta":{"content":"a"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"b"},"finish_reason":"length"}],"usage":{"prompt_tokens":"2","total_tokens":3}}"#,
                r#"data: {"choices":[{"delta":{"content":"late"}}]}"#,
            ],
        ),
        // Lines that aren't chunks.
        response(
            "not-chunks",
            "m",
            &["", "data:", "not json", ": keep-alive", "data:  [DONE] "],
        ),
    ]
}

/// Chat Completions responses for the translator to Interactions.
pub fn chat_finals() -> Vec<Case> {
    vec![
        // TestConvertOpenAIResponseToInteractionsNonStreamDirectToolCall
        response(
            "non-stream-direct-tool-call",
            "gpt-test",
            &[
                r#"{"id":"chatcmpl_1","model":"gpt-test","choices":[{"message":{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}"#,
            ],
        ),
        // The unit tests' whole response.
        response(
            "whole",
            "",
            &[
                r#"{"id":"r1","model":"up","choices":{"a":{"message":{"reasoning_content":"why","content":7,"tool_calls":[{"function":{"arguments":{"k":[1]}}}]},"finish_reason":null},"b":{"finish_reason":"stop"}},"usage":{"total_tokens":"9"}}"#,
            ],
        ),
        // Content as parts, reasoning as a list, no ID.
        response(
            "parts",
            "m",
            &[
                r#"{"choices":[{"message":{"content":[{"type":"text","text":"a"},{"text":"b"}],"reasoning_content":[{"text":"r"}],"tool_calls":[{"id":"t","function":{"name":"f","arguments":"{bad"}}]},"finish_reason":"length"}],"usage":{"prompt_tokens":1,"prompt_tokens_details":{"cached_tokens":1}}}"#,
            ],
        ),
        response("empty", "m", &[""]),
        response("not-json", "m", &["data: {\"id\":\"x\"}"]),
        response("no-body", "m", &[]),
    ]
}
