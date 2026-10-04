//! Hand-written cases for the Claude and Interactions translators' suites:
//! the input of upstream's tests (each named after its test), then the
//! input of our own tests, then corners.

use serde_json::json;

use crate::cases::Case;

fn request(name: &str, model: &str, body: &str, stream: bool) -> Case {
    Case::new(name, model, body).with_options(json!({ "stream": stream }))
}

fn response(name: &str, model: &str, events: &[&str]) -> Case {
    let events = events.iter().map(|event| (*event).to_owned()).collect();
    Case {
        model: model.to_owned(),
        ..Case::response(name, "", events)
    }
}

/// Claude Messages requests, for the translators to Interactions (both
/// modes).
pub fn claude_requests() -> Vec<Case> {
    let mut cases = vec![
        request(
            "maps-messages-tools-and-stream",
            "gemini-3.1-flash-lite",
            r#"{"model":"gemini-3.1-flash-lite","stream":true,"max_tokens":1024,"tools":[{"name":"get_weather","description":"Weather","input_schema":{"type":"object","properties":{"location":{"type":"string"}},"required":["location"]}}],"messages":[{"role":"user","content":[{"type":"text","text":"今天北京的天气怎么样？"}]}]}"#,
            true,
        ),
        request(
            "maps-tool-use-and-result",
            "gemini-3.1-flash-lite",
            r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"location":"北京"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"晴"}]}]}"#,
            false,
        ),
        request(
            "infers-tool-names-for-out-of-order-results",
            "gemini-3.1-flash-lite",
            r#"{
                "model": "gemini-3.1-flash-lite",
                "messages": [
                    {
                        "role": "assistant",
                        "content": [
                            {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "x"}},
                            {"type": "tool_use", "id": "toolu_2", "name": "weather", "input": {"city": "bj"}}
                        ]
                    },
                    {
                        "role": "user",
                        "content": [
                            {"type": "tool_result", "tool_use_id": "toolu_2", "content": "sunny"},
                            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "found"}
                        ]
                    }
                ]
            }"#,
            false,
        ),
        request(
            "propagates-is-error",
            "gemini-3.1-flash-lite",
            r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_err","content":"command failed","is_error":true}]}]}"#,
            false,
        ),
        request(
            "preserves-tool-adjacency-with-intervening-system-message",
            "gemini-3.1-flash-lite",
            r#"{
                "model": "gemini-3.1-flash-lite",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "Execute tools"}]},
                    {
                        "role": "assistant",
                        "content": [
                            {"type": "tool_use", "id": "call_1", "name": "tool_one", "input": {"a": 1}},
                            {"type": "tool_use", "id": "call_2", "name": "tool_two", "input": {"b": 2}}
                        ]
                    },
                    {"role": "system", "content": "Context reminder between tool_use and tool_result"},
                    {
                        "role": "user",
                        "content": [
                            {"type": "tool_result", "tool_use_id": "call_2", "content": "result 2"},
                            {"type": "tool_result", "tool_use_id": "call_1", "content": "result 1"},
                            {"type": "text", "text": "Now summarize"}
                        ]
                    }
                ]
            }"#,
            false,
        ),
        request(
            "preserves-images-in-tool-result",
            "devin/swe-2",
            r#"{
                "model": "devin/swe-2",
                "messages": [
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "tool_image_1", "name": "screenshot", "input": {}}]},
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": "tool_image_1",
                                "content": [
                                    {"type": "text", "text": "Captured desktop"},
                                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="}}
                                ]
                            }
                        ]
                    }
                ]
            }"#,
            false,
        ),
        request(
            "preserves-business-objects-in-tool-result-array",
            "devin/swe-2",
            r#"{
                "model": "devin/swe-2",
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": "tool_call_1",
                                "content": [
                                    {"text": "failed", "exit_code": 1, "retryable": true},
                                    {"type": "text", "text": "failed with code", "exit_code": 2}
                                ]
                            }
                        ]
                    }
                ]
            }"#,
            false,
        ),
        request(
            "with-compat-preserves-empty-thinking",
            "deepseek-v4",
            r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":""}]}]}"#,
            false,
        ),
        // Our settings_and_held_back_reminder.
        request(
            "settings-and-held-back-reminder",
            " ",
            r#"{"model":"claude-x","max_tokens":10,"temperature":0.5,"top_p":1,"stop_sequences":["x"],
                "thinking":{"type":"enabled","budget_tokens":2048},"output_config":{"effort":" HIGH "},
                "tool_choice":{"type":"tool","name":" lookup "},"system":[{"type":"text","text":"a"},"b",{"text":""}],
                "messages":[
                    {"role":"assistant","content":[{"type":"text","text":"t"},{"type":"tool_use","id":"c1","name":"lookup","input":{"q":1}}]},
                    {"role":"system","content":"note"},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":[{"type":"text","text":"r","cache_control":{}}]}]}
                ],
                "tools":[{"name":" lookup ","description":5,"input_schema":{"type":"object"}},{"name":" "}]}"#,
            false,
        ),
    ];
    let corners = [
        (
            "stream-option-without-field",
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            true,
        ),
        (
            "stream-field-false-option-true",
            r#"{"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
            true,
        ),
        (
            "stream-field-not-bool",
            r#"{"stream":"yes","messages":[{"role":"user","content":"hi"}]}"#,
            false,
        ),
        (
            "thinking-kinds",
            r#"{"thinking":{"type":" Adaptive "},"output_config":{"effort":5},"messages":[
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"plan","signature":"sig"},
                    {"type":"thinking","thinking":"","signature":"only-sig"},
                    {"type":"thinking","thinking":" "},
                    {"type":"redacted_thinking","data":"EmwKAhgB"},
                    {"type":"text","text":""},
                    {"type":"text","text":"answer"}
                ]}]}"#,
            false,
        ),
        (
            "thinking-disabled",
            r#"{"thinking":{"type":"disabled","budget_tokens":1024},"messages":[]}"#,
            false,
        ),
        (
            "thinking-enabled-without-budget",
            r#"{"thinking":{"type":"ENABLED"},"output_config":{"effort":"MAXİMUM"},"messages":[]}"#,
            false,
        ),
        (
            "thinking-budget-raw",
            r#"{"thinking":{"type":"enabled","budget_tokens": 1024.50 },"messages":[]}"#,
            false,
        ),
        (
            "tool-choices",
            r#"{"tool_choice":{"type":"any"},"messages":[{"role":"user","content":"x"}]}"#,
            false,
        ),
        (
            "tool-choice-none",
            r#"{"tool_choice":{"type":"none"},"messages":[]}"#,
            false,
        ),
        (
            "tool-choice-auto",
            r#"{"tool_choice":{"type":"AUTO"},"messages":[]}"#,
            false,
        ),
        (
            "tool-choice-tool-without-name",
            r#"{"tool_choice":{"type":"tool","name":"  "},"messages":[]}"#,
            false,
        ),
        (
            "tool-choice-string",
            r#"{"tool_choice":"any","messages":[]}"#,
            false,
        ),
        (
            "system-string-and-odd",
            r#"{"system":"  be brief  ","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
            false,
        ),
        (
            "system-object",
            r#"{"system":{"text":"x"},"messages":[]}"#,
            false,
        ),
        (
            "media-parts",
            r#"{"messages":[{"role":"user","content":[
                {"type":"image","source":{"type":"url","url":"https://example.com/a.png"}},
                {"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"/9j/"}},
                {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0="}},
                {"type":"document","source":{"type":"text","media_type":"text/plain","data":"plain"}},
                {"type":"image"},
                {"type":"unknown","text":"skipped"},
                "bare string",
                {"text":"no type"}
            ]}]}"#,
            false,
        ),
        (
            "tool-results-of-every-shape",
            r#"{"messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"a","name":"f","input":"{\"x\":1}"},
                    {"type":"tool_use","id":"b","name":"g"},
                    {"type":"tool_use","id":"c","name":"h","input":null},
                    {"type":"tool_use","name":"no_id","input":{}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"c","content":{"nested":{"deep":[1,2]}}},
                    {"type":"tool_result","tool_use_id":"b"},
                    {"type":"tool_result","tool_use_id":"a","content":[{"type":"text","text":"one"},{"type":"text","text":"two"}],"is_error":false},
                    {"type":"tool_result","tool_use_id":"unknown","content":5},
                    {"type":"tool_result","content":"no id"}
                ]}
            ]}"#,
            false,
        ),
        (
            "roles-and-string-content",
            r#"{"messages":[
                {"role":"user","content":"first"},
                {"role":"user","content":"second"},
                {"role":"assistant","content":"reply"},
                {"role":"ASSISTANT","content":"caps"},
                {"role":"tool","content":"odd role"},
                {"content":"no role"},
                {"role":"user","content":null},
                {"role":"user","content":[]},
                {"role":"system","content":[{"type":"text","text":"array system"}]},
                {"role":"system","content":""}
            ]}"#,
            false,
        ),
        (
            "system-message-at-end",
            r#"{"messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"t","name":"f","input":{}}]},
                {"role":"system","content":"held"},
                {"role":"system","content":"also held"}
            ]}"#,
            false,
        ),
        (
            "tools-of-every-shape",
            r#"{"tools":[
                {"name":"plain","input_schema":{"type":"object","properties":{"a":{"type":"integer","minimum":1.50}}}},
                {"name":"no_schema","description":"d"},
                {"type":"web_search_20250305","name":"web_search"},
                {"name":"","input_schema":{}},
                {"description":"nameless"},
                "string tool",
                {"name":"cached","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}
            ],"messages":[]}"#,
            false,
        ),
        (
            "tools-not-array",
            r#"{"tools":{"name":"x"},"messages":"not array"}"#,
            false,
        ),
        ("empty-object", "{}", false),
        ("array-body", "[1,2]", false),
        ("string-body", r#""text""#, true),
        ("null-body", "null", false),
        (
            "escaped-text",
            "{\"messages\":[{\"role\":\"user\",\"content\":\"caf\\u00e9 \\/ \\ud83d\\ude80 <b>&</b>\"}]}",
            false,
        ),
        (
            "generation-settings-raw",
            r#"{"max_tokens":1.50,"temperature":"0.5","top_p":null,"top_k":40,"stop_sequences":"END","metadata":{"user_id":"u"},"messages":[]}"#,
            false,
        ),
    ];
    for (name, body, stream) in corners {
        cases.push(request(name, "gemini-2.5-pro", body, stream));
    }
    cases
}

/// Interactions requests, for the translator to Claude.
pub fn interactions_requests() -> Vec<Case> {
    let mut cases = vec![
        request(
            "with-tool-messages-direct",
            "claude-test",
            r#"{"model":"claude-test","system_instruction":"be brief","input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]},{"type":"function_call","name":"lookup","call_id":"toolu_1","arguments":{"q":"x"}},{"type":"function_result","name":"lookup","call_id":"toolu_1","result":{"ok":true}}]}"#,
            false,
        ),
        request(
            "propagates-is-error",
            "claude-test",
            r#"{"model":"claude-test","input":[{"type":"function_result","name":"lookup","call_id":"toolu_err","result":"command failed","is_error":true}]}"#,
            false,
        ),
        request(
            "groups-consecutive-role-turns",
            "claude-test",
            r#"{
                "input":[
                    {"type":"thought","content":[{"type":"thinking","thinking":"reason"}]},
                    {"type":"model_output","content":[{"type":"text","text":"answer"}]},
                    {"type":"function_call","name":"first","call_id":"call_1","arguments":{}},
                    {"type":"function_call","name":"second","call_id":"call_2","arguments":{}},
                    {"type":"function_result","call_id":"call_1","result":{"value":"one"}},
                    {"type":"function_result","call_id":"call_2","result":{"value":"two"}}
                ]
            }"#,
            false,
        ),
        request(
            "does-not-merge-across-role-changes",
            "claude-test",
            r#"{
                "input":[
                    {"type":"model_output","content":"first assistant"},
                    {"type":"user_input","content":"user reply"},
                    {"type":"model_output","content":"second assistant"}
                ]
            }"#,
            false,
        ),
        request(
            "string-input-direct",
            "claude-test",
            r#"{"model":"claude-test","input":"hello"}"#,
            false,
        ),
        request(
            "maps-generation-config-tools-and-stream-direct",
            "claude-test",
            r#"{"model":"claude-test","stream":true,"input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]}],"tools":[{"type":"function","name":"lookup","description":"Lookup data","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}],"generation_config":{"max_output_tokens":99,"top_p":0.7,"stop_sequences":["END"],"tool_choice":{"type":"function","name":"lookup"},"thinking_level":"high"}}"#,
            false,
        ),
        request(
            "accepts-image-content",
            "claude-test",
            r#"{"model":"claude-test","input":[{"type":"user_input","content":[{"type":"image","mime_type":"image/png","data":"aGVsbG8="}]}]}"#,
            false,
        ),
        request(
            "preserves-non-image-media-content",
            "claude-test",
            r#"{"model":"claude-test","input":[{"type":"thought","content":[{"type":"audio","mime_type":"audio/wav","data":"UklGRg=="},{"type":"video","mime_type":"video/mp4","data":"AAAAIGZ0eXA="},{"type":"document","mime_type":"application/pdf","data":"JVBERi0="}]}]}"#,
            false,
        ),
        // Our settings_turns_and_declarations.
        request(
            "settings-turns-and-declarations",
            "claude-x",
            r#"{"systemInstruction":{"parts":[{"text":"a"},{"text":""},"b"]},
                "generationConfig":{"maxOutputTokens":5,"topP":0.5,"stopSequences":["s"],"thinkingLevel":" NONE ","toolChoice":"required"},
                "reasoning":{"effort":"turbo"},
                "tool_choice":{"type":"tool","function":{"name":"my.tool"}},
                "input":[
                    {"role":"model","steps":[{"type":"thought","content":[{"type":"thinking","thinking":"hm"}]},{"role":"user","content":"x"}]},
                    {"role":"model","parts":[{"text":"p"}]},
                    {"type":"function_call","name":"my.tool","arguments":[1]},
                    {"type":"function_result","name":"my.tool","output":{"ok":true}},
                    {"type":"user_input","content":[{"type":"image","source":{"media_type":"image/gif","data":"R0"}},{"type":"thinking","thinking":"dropped"},{"type":"audio"}]}
                ],
                "tools":[
                    {"functionDeclarations":[{"name":"my.tool","parameters_json_schema":{"type":"object","properties":{"q":{"type":"string"}}}},{"description":"nameless"}]},
                    {"function":{"name":"f2","description":"d"}}
                ]}"#,
            false,
        ),
    ];
    let corners = [
        (
            "thinking-levels",
            r#"{"generation_config":{"thinking_level":"minimal"},"input":"x"}"#,
            false,
        ),
        (
            "thinking-level-low",
            r#"{"generation_config":{"thinking_level":"LOW","thinking_config":{"thinking_budget":2048}},"input":"x"}"#,
            false,
        ),
        (
            "thinking-level-medium",
            r#"{"generation_config":{"thinking_level":" medium "},"input":"x"}"#,
            false,
        ),
        (
            "thinking-level-auto",
            r#"{"generation_config":{"thinking_level":"auto"},"input":"x"}"#,
            false,
        ),
        (
            "thinking-level-unknown",
            r#"{"generation_config":{"thinking_level":"ΑΣ"},"input":"x"}"#,
            false,
        ),
        (
            "thinking-budget-only",
            r#"{"generation_config":{"thinking_config":{"thinking_budget":1024.50,"include_thoughts":true}},"input":"x"}"#,
            false,
        ),
        (
            "thinking-config-camel",
            r#"{"generationConfig":{"thinkingConfig":{"thinkingBudget":0,"thinkingLevel":"high"}},"input":"x"}"#,
            false,
        ),
        (
            "reasoning-effort-and-level",
            r#"{"reasoning":{"effort":" HIGH ","summary":"auto"},"generation_config":{"thinking_level":"low"},"input":"x"}"#,
            false,
        ),
        (
            "reasoning-not-object",
            r#"{"reasoning":"high","input":"x"}"#,
            false,
        ),
        (
            "tool-choice-strings",
            r#"{"generation_config":{"tool_choice":"auto"},"input":"x"}"#,
            false,
        ),
        (
            "tool-choice-none",
            r#"{"tool_choice":"none","input":"x"}"#,
            false,
        ),
        (
            "tool-choice-any",
            r#"{"tool_choice":" ANY ","input":"x"}"#,
            false,
        ),
        (
            "tool-choice-validated",
            r#"{"tool_choice":"validated","input":"x"}"#,
            false,
        ),
        (
            "tool-choice-allowed-tools",
            r#"{"tool_choice":{"allowed_tools":{"mode":"validated","tools":["lookup"]}},"input":"x"}"#,
            false,
        ),
        (
            "tool-choice-function-object",
            r#"{"tool_choice":{"type":"function","function":{"name":"a-b.c"}},"input":"x"}"#,
            false,
        ),
        (
            "tool-choice-object-without-name",
            r#"{"tool_choice":{"type":"function"},"input":"x"}"#,
            false,
        ),
        (
            "system-instruction-shapes",
            r#"{"system_instruction":{"text":"a"},"systemInstruction":"b","input":"x"}"#,
            false,
        ),
        (
            "system-instruction-parts-only",
            r#"{"systemInstruction":{"parts":[{"text":" "},{"text":"keep"},{"inline_data":{}}]},"input":"x"}"#,
            false,
        ),
        (
            "system-instruction-odd",
            r#"{"system_instruction":5,"input":"x"}"#,
            false,
        ),
        (
            "input-object",
            r#"{"input":{"type":"text","text":"one part"}}"#,
            false,
        ),
        ("input-odd", r#"{"input":5}"#, true),
        ("input-null", r#"{"input":null,"stream":true}"#, false),
        (
            "call-arguments-shapes",
            r#"{"input":[
                {"type":"function_call","name":"f","call_id":"c1","arguments":"{\"a\": 1}"},
                {"type":"function_call","name":"g","id":"c2","arguments":"{\"broken\":"},
                {"type":"function_call","name":"h","arguments":null},
                {"type":"function_call","name":"i","call_id":"c4","args":{"x":true}},
                {"type":"function_call","name":"","call_id":"c5"},
                {"type":"function_call","call_id":"c6","arguments":{"nested":{"a":[1, 2.50]}}}
            ]}"#,
            false,
        ),
        (
            "result-shapes",
            r#"{"input":[
                {"type":"function_result","call_id":"c1","result":"text result"},
                {"type":"function_result","call_id":"c2","result":[{"type":"text","text":"a"},{"type":"image","mime_type":"image/png","data":"aGk="}]},
                {"type":"function_result","call_id":"c3","result":{"temperature": 21.50, "note":"x"}},
                {"type":"function_result","call_id":"c4","result":null},
                {"type":"function_result","call_id":"c5","result":5},
                {"type":"function_result","call_id":"c6"},
                {"type":"function_result","name":"named.only","result":"x","is_error":"true"},
                {"type":"function_result","id":"c8","output":"from output","is_error":false}
            ]}"#,
            false,
        ),
        (
            "content-shapes",
            r#"{"input":[
                {"type":"user_input","content":[
                    {"type":"text","text":""},
                    {"type":"text","text":5},
                    {"type":"image","uri":"https://example.com/a.png","mime_type":"image/png"},
                    {"type":"image","mime_type":"","data":"aGk="},
                    {"type":"document","uri":"https://example.com/a.pdf"},
                    {"type":"audio","mime_type":"audio/wav","data":"UklGRg=="},
                    {"type":"unknown","text":"x"},
                    {"type":"","text":"no type"},
                    "string part"
                ]},
                {"type":"user_input","content":"plain"},
                {"type":"user_input","content":null},
                {"type":"model_output","content":[{"type":"text","text":"out"}],"text":"ignored"}
            ]}"#,
            false,
        ),
        (
            "thought-shapes",
            r#"{"input":[
                {"type":"thought","signature":"sig","summary":[{"type":"text","text":"sum"}]},
                {"type":"thought","thought_signature":"s2","text":"t"},
                {"type":"thought","thoughtSignature":"s3"},
                {"type":"thought","content":"plain thought"},
                {"type":"thought","content":[{"type":"text","text":"a"},{"type":"thinking","thinking":"b"}]}
            ]}"#,
            false,
        ),
        (
            "turns-of-every-role",
            r#"{"input":[
                {"role":"user","content":[{"type":"text","text":"u"}]},
                {"role":"model","content":"m"},
                {"role":"assistant","parts":[{"text":"a"},{"thought":true,"text":"t"}]},
                {"role":"system","content":"s"},
                {"role":"function","parts":[{"text":"f"}]},
                {"role":"","steps":[{"type":"user_input","content":"e"}]},
                {"role":"USER","steps":"not array"},
                {"content":"no role"},
                {"type":"unknown_step","content":"x"},
                5,
                "string step"
            ]}"#,
            false,
        ),
        (
            "tools-of-every-shape",
            r#"{"tools":[
                {"type":"function","name":"a","parameters":{"type":"object","properties":{"x":{"type":"number","minimum":0.50}}}},
                {"type":"function","name":"b","description":"no params"},
                {"type":"function_declarations","function_declarations":[{"name":"c","parameters":{"type":"object"}}]},
                {"functionDeclarations":[{"name":"d.e","description":"camel"},{"name":""}]},
                {"type":"google_search"},
                {"google_search":{}},
                {"type":"url_context"},
                {"type":"code_execution"},
                {"type":"function"},
                "odd tool",
                {"type":"function","name":"f","parameters_json_schema":{"type":"object","properties":{}},"input_schema":{"type":"string"}}
            ],"input":"x"}"#,
            false,
        ),
        (
            "tools-not-array",
            r#"{"tools":{"type":"function","name":"a"},"input":"x"}"#,
            false,
        ),
        (
            "generation-settings-raw",
            r#"{"generation_config":{"max_output_tokens":"10","temperature":1.50,"top_p":1,"top_k":40,"stop_sequences":"END","seed":7,"response_modalities":["TEXT"]},"response_format":{"type":"json_object"},"input":"x"}"#,
            false,
        ),
        (
            "generation-settings-both-cases",
            r#"{"generation_config":{"max_output_tokens":10},"generationConfig":{"maxOutputTokens":20,"temperature":0},"input":"x"}"#,
            false,
        ),
        (
            "agent-and-extras",
            r#"{"agent":"deep-research-pro-preview-12-2025","previous_interaction_id":"interaction_1","environment_id":"env_1","service_tier":"flex","input":"x"}"#,
            false,
        ),
        ("empty-object", "{}", false),
        ("array-body", "[1,2]", false),
        ("string-body", r#""text""#, true),
        (
            "escaped-text",
            "{\"input\":\"caf\\u00e9 \\/ \\ud83d\\ude80 <b>&</b>\"}",
            false,
        ),
    ];
    for (name, body, stream) in corners {
        cases.push(request(name, "claude-sonnet-4-5", body, stream));
    }
    cases
}

/// Interactions event streams, for the stream translator to Claude.
pub fn interactions_streams() -> Vec<Case> {
    vec![
        response(
            "response-to-claude-stream",
            "gemini-3.1-flash-lite",
            &[
                "event: interaction.created\ndata: {\"interaction\":{\"id\":\"interaction_1\",\"model\":\"gemini-3.1-flash-lite\"},\"event_type\":\"interaction.created\"}",
                "event: step.start\ndata: {\"index\":0,\"step\":{\"type\":\"model_output\"},\"event_type\":\"step.start\"}",
                "event: step.delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"北京今天晴\"},\"event_type\":\"step.delta\"}",
                "event: step.stop\ndata: {\"index\":0,\"event_type\":\"step.stop\"}",
                "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"interaction_1\",\"model\":\"gemini-3.1-flash-lite\",\"usage\":{\"total_input_tokens\":3,\"total_output_tokens\":4}},\"event_type\":\"interaction.completed\"}",
                "event: done\ndata: [DONE]",
            ],
        ),
        response(
            "stream-tool-call",
            "gemini-3.1-flash-lite",
            &[
                r#"data: {"interaction":{"id":"interaction_1","model":"gemini-3.1-flash-lite"},"event_type":"interaction.created"}"#,
                r#"data: {"index":0,"step":{"type":"function_call","id":"toolu_1","signature":"sig_1","name":"get_weather","arguments":{}},"event_type":"step.start"}"#,
                r#"data: {"index":0,"delta":{"type":"arguments_delta","arguments":"{\"location\":\"北京\"}"},"event_type":"step.delta"}"#,
                r#"data: {"index":0,"event_type":"step.stop"}"#,
                r#"data: {"interaction":{"usage":{"total_input_tokens":1,"total_output_tokens":2}},"event_type":"interaction.completed"}"#,
            ],
        ),
        response(
            "stream-finish-metadata-usage",
            "claude-test",
            &[
                r#"data: {"event_type":"finish","metadata":{"total_usage":{"total_input_tokens":2,"total_output_tokens":6,"total_tokens":8}}}"#,
            ],
        ),
        response(
            "incomplete-max-tokens",
            "devin/swe-2",
            &[
                r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"devin/swe-2"}}"#,
                r#"data: {"event_type":"step.start","index":0,"step":{"type":"model_output"}}"#,
                r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"text","text":"cut short"}}"#,
                r#"data: {"event_type":"step.stop","index":0}"#,
                r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"incomplete","finish_reason":"length"}}"#,
                "data: [DONE]",
            ],
        ),
        response(
            "preserves-cache-read-usage-streaming-cache-hit",
            "devin/swe-2",
            &[
                r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"devin/swe-2"}}"#,
                r#"data: {"event_type":"step.start","index":0,"step":{"type":"model_output"}}"#,
                r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"text","text":"hello"}}"#,
                r#"data: {"event_type":"step.stop","index":0}"#,
                r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","model":"devin/swe-2","status":"completed","usage":{"total_input_tokens":10411,"total_output_tokens":76,"total_cached_tokens":10340,"total_tokens":10487}}}"#,
                "data: [DONE]",
            ],
        ),
        response(
            "response-failed-top-level",
            "devin/kimi-k3",
            &[
                r#"data: {"event_type":"response.failed","error":{"message":"devin upstream error (permission_denied): Unable to process request due to an MCP configuration issue.","code":"403"}}"#,
            ],
        ),
        response(
            "response-failed-interaction-failed-nested",
            "devin/kimi-k3",
            &[
                r#"data: {"event_type":"interaction.failed","interaction":{"error":{"message":"service unavailable","type":"server_error"}}}"#,
            ],
        ),
        response(
            "response-failed-fallback-defaults",
            "devin/kimi-k3",
            &[r#"data: {"event_type":"response.failed"}"#],
        ),
        // Our stream_frames.
        response(
            "stream-frames",
            "m",
            &[
                r#"{"event_type":"step.start","index":0,"step":{"type":"thought"}}"#,
                r#"{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","content":{"text":"hm"}}}"#,
                r#"{"event_type":"step.delta","index":0,"delta":{"type":"thought_signature","signature":"s"}}"#,
                r#"{"event_type":"step.delta","index":3,"delta":{"type":"arguments_delta","arguments":""},"step":{"name":"f"}}"#,
                "[DONE]",
                "[DONE]",
            ],
        ),
        response(
            "steps-of-every-kind",
            "gemini-2.5-pro",
            &[
                r#"{"event_type":"interaction.created","interaction":{"id":"interaction_9","model":" "}}"#,
                r#"{"event_type":"interaction.status_update","status":"in_progress"}"#,
                r#"{"event_type":"step.start","index":0,"step":{"type":"thought","signature":"start-sig"}}"#,
                r#"{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","content":[{"type":"text","text":"a"},{"text":"b"}]}}"#,
                r#"{"event_type":"step.delta","index":0,"delta":{"type":"text","text":"text in a thought"}}"#,
                r#"{"event_type":"step.stop","index":0}"#,
                r#"{"event_type":"step.start","index":1,"step":{"type":"model_output","content":[{"type":"text","text":"initial"}]}}"#,
                r#"{"event_type":"step.delta","index":1,"delta":{"type":"text","text":""}}"#,
                r#"{"event_type":"step.delta","index":1,"delta":{"type":"text","text":"more"}}"#,
                r#"{"event_type":"step.start","index":2,"step":{"type":"function_call","call_id":"c2","name":"lookup","arguments":{"q":"x"}}}"#,
                r#"{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":{"q": "object"}}}"#,
                r#"{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"{\"q\":"}}"#,
                r#"{"event_type":"step.delta","index":2,"delta":{"type":"thought_signature","signature":"call-sig"}}"#,
                r#"{"event_type":"step.stop","index":2}"#,
                r#"{"event_type":"step.start","index":3,"step":{"type":"function_call","name":"no_id"}}"#,
                r#"{"event_type":"step.stop","index":3}"#,
                r#"{"event_type":"step.start","index":4,"step":{"type":"unknown"}}"#,
                r#"{"event_type":"step.delta","index":4,"delta":{"type":"unknown","text":"x"}}"#,
                r#"{"event_type":"step.stop","index":9}"#,
                r#"{"event_type":"interaction.completed","interaction":{"status":"requires_action","finish_reason":"tool_calls","usage":{"total_input_tokens":"12","total_output_tokens":3.50,"total_thought_tokens":2,"total_cached_tokens":20}}}"#,
                r#"{"event_type":"finish","metadata":{"total_usage":{"total_input_tokens":1}}}"#,
                r#"{"event_type":"done"}"#,
                "[DONE]",
            ],
        ),
        response(
            "delta-before-any-start",
            "gemini-2.5-flash",
            &[
                r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"text","text":"early"}}"#,
                r#"data: {"event_type":"step.delta","index":1,"delta":{"type":"thought_summary","content":{"type":"text","text":"late thought"}}}"#,
                r#"data: {"event_type":"step.delta","index":1,"delta":{"type":"thought_signature","signature":"sig"}}"#,
                r#"data: {"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"{}"}}"#,
                r#"data: {"event_type":"interaction.completed","interaction":{"status":"completed","finish_reason":"max_tokens"}}"#,
            ],
        ),
        response(
            "errors-after-output",
            "gemini-2.5-flash",
            &[
                r#"{"event_type":"step.start","index":0,"step":{"type":"model_output"}}"#,
                r#"{"event_type":"step.delta","index":0,"delta":{"type":"text","text":"partial"}}"#,
                r#"{"event_type":"error","error":{"message":"quota","code":429}}"#,
                r#"{"event_type":"interaction.failed","error":{"message":"again","code":"RESOURCE_EXHAUSTED"}}"#,
                r#"{"event_type":"interaction.completed","interaction":{"status":"completed"}}"#,
            ],
        ),
        response(
            "lines-that-give-nothing",
            "",
            &[
                "",
                " ",
                "event: ping",
                ": keep-alive",
                "data:",
                "data: {not json",
                "not json at all",
                r#"{"no_event_type":true}"#,
                r#"data:{"event_type":"interaction.created","interaction":{"id":"x"}}"#,
                "data:  {\"event_type\":\"interaction.completed\",\"interaction\":{\"status\":\"incomplete\"}} \r",
                "data: [DONE]",
            ],
        ),
        response(
            "completed-twice",
            "gemini-2.5-flash",
            &[
                r#"{"event_type":"interaction.completed","interaction":{"status":"completed","usage":{"total_input_tokens":5}}}"#,
                r#"{"event_type":"interaction.completed","interaction":{"status":"incomplete"}}"#,
                r#"{"event_type":"step.start","index":0,"step":{"type":"model_output"}}"#,
            ],
        ),
        response(
            "escaped-and-unicode",
            "gemini-2.5-flash",
            &[
                "{\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"caf\\u00e9 \\/ \\ud83d\\ude80 <b>&</b> \\u2028\"}}",
            ],
        ),
    ]
}

/// Whole interactions, for the non-streaming translator to Claude.
pub fn interactions_finals() -> Vec<Case> {
    let mut cases = vec![
        response(
            "response-to-claude-non-stream",
            "gemini-3.1-flash-lite",
            &[
                r#"{"id":"interaction_1","model":"gemini-3.1-flash-lite","steps":[{"type":"model_output","content":[{"type":"text","text":"ok"}]},{"type":"function_call","call_id":"toolu_1","signature":"sig_1","name":"lookup","arguments":{"q":"x"}}],"usage":{"total_input_tokens":3,"total_output_tokens":4}}"#,
            ],
        ),
        response(
            "incomplete-max-tokens",
            "devin/swe-2",
            &[
                r#"{"id":"interaction_1","model":"devin/swe-2","status":"incomplete","finish_reason":"length","steps":[{"type":"model_output","content":[{"type":"text","text":"cut short"}]}],"usage":{"total_input_tokens":3,"total_output_tokens":4}}"#,
            ],
        ),
    ];
    // TestConvertInteractionsResponseToClaude_PreservesCacheReadUsage's
    // non-streaming subtests.
    let usages = [
        (
            "non-streaming-cache-hit",
            r#"{"total_input_tokens":96724,"total_output_tokens":269,"total_cached_tokens":30784,"total_tokens":96993}"#,
        ),
        (
            "zero-cache-tokens",
            r#"{"total_input_tokens":100,"total_output_tokens":50,"total_cached_tokens":0,"total_tokens":150}"#,
        ),
        (
            "explicit-uncached-and-cache-creation",
            r#"{"input_tokens":71,"total_input_tokens":10411,"total_output_tokens":76,"cache_read_input_tokens":10340,"cache_creation_input_tokens":25,"total_tokens":10487}"#,
        ),
        (
            "explicit-uncached-with-cache-write-in-total",
            r#"{"input_tokens":71,"total_input_tokens":10436,"total_output_tokens":76,"cache_read_input_tokens":10340,"cache_creation_input_tokens":25,"total_tokens":10512}"#,
        ),
        (
            "full-cache-hit",
            r#"{"total_input_tokens":500,"total_output_tokens":50,"total_cached_tokens":500,"total_tokens":550}"#,
        ),
        (
            "cached-tokens-exceeds-input",
            r#"{"total_input_tokens":50,"total_output_tokens":50,"total_cached_tokens":100,"total_tokens":150}"#,
        ),
        (
            "explicit-uncached-without-total-input-tokens",
            r#"{"input_tokens":71,"output_tokens":76,"cache_read_input_tokens":10340}"#,
        ),
        (
            "explicit-uncached-without-total-input-tokens-greater",
            r#"{"input_tokens":100,"output_tokens":30,"cache_read_input_tokens":80}"#,
        ),
        (
            "explicit-uncached-without-total-input-tokens-equal",
            r#"{"input_tokens":80,"output_tokens":30,"cache_read_input_tokens":80}"#,
        ),
        (
            "explicit-uncached-zero-with-total",
            r#"{"input_tokens":0,"total_input_tokens":500,"output_tokens":30,"cache_read_input_tokens":500}"#,
        ),
        (
            "explicit-uncached-equal-to-total-input",
            r#"{"input_tokens":100,"total_input_tokens":100,"output_tokens":30,"cache_read_input_tokens":20}"#,
        ),
        (
            "explicit-uncached-zero-with-partial-cache",
            r#"{"input_tokens":0,"total_input_tokens":500,"output_tokens":30,"cache_read_input_tokens":200}"#,
        ),
        (
            "inclusive-total-without-input-tokens-large-scale",
            r#"{"total_input_tokens":10436,"output_tokens":76,"cache_read_input_tokens":10340,"cache_creation_input_tokens":25}"#,
        ),
        (
            "prompt-tokens-fallback",
            r#"{"prompt_tokens":100,"output_tokens":20,"cached_tokens":30}"#,
        ),
        // Not upstream's: counts of other types, and the usage nested.
        (
            "odd-counts",
            r#"{"total_input_tokens":"40","total_output_tokens":2.50,"total_cached_tokens":true,"cache_creation_input_tokens":null}"#,
        ),
        (
            "negative-counts",
            r#"{"total_input_tokens":-5,"total_output_tokens":-1,"total_cached_tokens":-3}"#,
        ),
    ];
    for (name, usage) in usages {
        let body = format!(
            r#"{{"id":"i","model":"devin/swe-2","status":"completed","steps":[{{"type":"model_output","content":[{{"type":"text","text":"response text"}}]}}],"usage":{usage}}}"#
        );
        cases.push(response(name, "devin/swe-2", &[&body]));
    }
    // Our non_stream_nested_interaction, then corners.
    let bodies = [
        (
            "non-stream-nested-interaction",
            r#"{"id":"outer","interaction":{"model":" ","steps":{
                "a":{"type":"thought","content":"plain","thought_signature":"sig"},
                "b":{"type":"thought","content":[{"text":" "},{"content":{"text":"part"}}]},
                "c":{"type":"function_call","name":"f","args":{"x":1},"arguments":null},
                "d":"skipped"}}}"#,
        ),
        (
            "nested-with-usage-outside",
            r#"{"interaction":{"id":"inner","status":"incomplete","steps":[{"type":"model_output","content":"string content"}]},"usage":{"total_input_tokens":7},"steps":[{"type":"model_output","content":[{"type":"text","text":"outer"}]}]}"#,
        ),
        (
            "steps-of-every-kind",
            r#"{"id":"interaction_2","status":"requires_action","finish_reason":"tool_calls","steps":[
                {"type":"thought","signature":"s","summary":[{"type":"text","text":"summary"}],"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]},
                {"type":"function_call","id":"c1","name":"f","arguments":{"a": [1, 2.50]},"thought_signature":"call-sig"},
                {"type":"function_call","call_id":"c2","name":"g","arguments":"{\"a\":1}"},
                {"type":"function_call","name":"h","arguments":[1]},
                {"type":"model_output","content":[{"type":"text","text":""},{"type":"image","data":"aGk="},{"type":"text","text":"x"}]},
                {"type":"unknown","content":[{"text":"from unknown"}]},
                {"content":[{"type":"text","text":"no type"}]}
            ],"usage":{"total_input_tokens":1,"total_output_tokens":2,"total_thought_tokens":3}}"#,
        ),
        ("error-body", r#"{"error":{"message":"quota","code":429}}"#),
        (
            "finish-reason-max-tokens",
            r#"{"finish_reason":"max_tokens","steps":[]}"#,
        ),
        ("steps-not-array", r#"{"id":"x","steps":"none"}"#),
        ("empty-object", "{}"),
        ("array-body", "[1,2]"),
        ("not-json", "not json"),
        ("empty-body", ""),
        (
            "escaped-and-unicode",
            "{\"id\":\"i\",\"steps\":[{\"type\":\"model_output\",\"content\":[{\"type\":\"text\",\"text\":\"caf\\u00e9 \\/ \\ud83d\\ude80 <b>&</b>\"}]}]}",
        ),
    ];
    for (name, body) in bodies {
        cases.push(response(name, "gemini-2.5-pro", &[body]));
    }
    cases.push(response("no-events", "gemini-2.5-pro", &[]));
    cases
}

/// Claude event streams, for the stream translator to Interactions.
pub fn claude_streams() -> Vec<Case> {
    vec![
        response(
            "stream-merges-usage-and-status",
            "claude-test",
            &[
                r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":3,"output_tokens":0}}}"#,
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
                r#"data: {"type":"message_delta","usage":{"output_tokens":2}}"#,
            ],
        ),
        response(
            "response-to-interactions-stream",
            "claude-test",
            &[
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            ],
        ),
        // Our stream_events.
        response(
            "stream-events",
            "m",
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"f","input":{}}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\""}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
                r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
                r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"signature_delta","signature":"s"}}"#,
                r#"data: {"type":"message_delta","usage":{"output_tokens":2}}"#,
                r#"data: {"type":"message_stop"}"#,
                "event: ping",
                "[DONE]",
                "data: [DONE]",
            ],
        ),
        response(
            "blocks-of-every-kind",
            "claude-opus-4-6",
            &[
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"model\":\"claude-opus-4-6\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":4,\"cache_creation_input_tokens\":1}}}",
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCkYIBxgCKkA="}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"EmwKAhgB"}}"#,
                r#"data: {"type":"content_block_stop","index":1}"#,
                r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_2","name":"get_weather","input":{"city":"Paris"}}}"#,
                r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":""}}"#,
                r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"days\": 2}"}}"#,
                r#"data: {"type":"ping"}"#,
                r#"data: {"type":"content_block_stop","index":2}"#,
                r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}"#,
                r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"x\"}"}}"#,
                r#"data: {"type":"content_block_stop","index":3}"#,
                r#"data: {"type":"content_block_start","index":4,"content_block":{"type":"text","text":"start text"}}"#,
                r#"data: {"type":"content_block_delta","index":4,"delta":{"type":"citations_delta","citation":{}}}"#,
                r#"data: {"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"done"}}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7,"thinking_tokens":3}}"#,
                r#"data: {"type":"message_stop"}"#,
                r#"data: {"type":"message_stop"}"#,
            ],
        ),
        response(
            "interleaved-indexes",
            "claude-sonnet-4-5-20250929",
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","name":"no_id","input":{}}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"back to 0"}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
                r#"data: {"type":"content_block_delta","index":"1","delta":{"type":"input_json_delta","partial_json":"x"}}"#,
                r#"data: {"type":"content_block_delta","index":1.5,"delta":{"type":"text_delta","text":"float index"}}"#,
                r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"no index"}}"#,
                r#"data: {"type":"content_block_delta","index":-1,"delta":{"type":"unknown_delta"}}"#,
                r#"data: {"type":"content_block_stop","index":1}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
            ],
        ),
        response(
            "error-event",
            "claude-opus-4-6",
            &[
                r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                r#"data: {"type":"message_stop"}"#,
            ],
        ),
        response("error-alone", "", &[r#"data: {"type":"error"}"#]),
        response(
            "message-start-twice",
            "claude-opus-4-6",
            &[
                r#"data: {"type":"message_start","message":{"id":"msg_a","model":"first"}}"#,
                r#"data: {"type":"message_start","message":{"id":"msg_b","model":"","usage":{"output_tokens":"5"}}}"#,
                r#"data: {"type":"message_start","message":"msg"}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":1.50}}"#,
                r#"data: {"type":"message_delta","usage":{"output_tokens":9}}"#,
                "data: [DONE]",
                "data: [DONE]",
            ],
        ),
        response(
            "lines-that-give-nothing",
            "claude-opus-4-6",
            &[
                "",
                ": keep-alive",
                "data:",
                "data: {not json",
                r#"{"type":"message_start","message":{"id":"bare"}}"#,
                r#"data:{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                "data:  {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"spaced\"}} \r",
                r#" data: {"type":"content_block_stop","index":0}"#,
                "event: message_stop\ndata: {\"type\":\"message_stop\"}",
                "[DONE]",
            ],
        ),
        response(
            "escaped-and-unicode",
            "claude-opus-4-6",
            &[
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"caf\\u00e9 \\/ \\ud83d\\ude80 <b>&</b> \\u2028\"}}",
            ],
        ),
    ]
}

/// Claude messages and SSE bodies, for the non-streaming translator to
/// Interactions.
pub fn claude_finals() -> Vec<Case> {
    let bodies = [
        (
            "response-to-interactions-non-stream",
            r#"{"id":"msg_1","model":"claude-test","content":[{"type":"thinking","thinking":"reasoning"},{"type":"text","text":"ok"},{"type":"tool_use","id":"toolu_1","name":"lookup","input":{"q":"x"}}],"usage":{"input_tokens":3,"output_tokens":2,"cache_read_input_tokens":1,"cache_creation_input_tokens":4,"thinking_tokens":5}}"#.to_owned(),
        ),
        (
            "sse-to-interactions-non-stream",
            [
                r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":3,"output_tokens":0}}}"#,
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
                r#"data: {"type":"message_delta","usage":{"output_tokens":2}}"#,
            ]
            .join("\n"),
        ),
        // Our sse_non_stream_collects_blocks.
        (
            "sse-non-stream-collects-blocks",
            [
                r#"data: {"type":"message_start","message":{"id":"msg_9","model":"claude-y","usage":{"input_tokens":4,"cache_read_input_tokens":2}}}"#,
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"a"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"b"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t","name":"f","input":{}}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" {\"x\":"}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"[1]} "}}"#,
                r#"data: {"type":"content_block_stop","index":1}"#,
                r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","name":"g","input":{"k":"v"}}}"#,
                r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"x"}}"#,
                r#"data: {"type":"content_block_stop","index":2}"#,
                r#"data: {"type":"content_block_stop","index":7}"#,
                r#"data: {"type":"message_delta","usage":{"output_tokens":3,"thinking_tokens":1}}"#,
                "data: [DONE]",
            ]
            .join("\n"),
        ),
        (
            "message-of-every-block",
            r#"{
                "id": "msg_3",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-4-6",
                "content": [
                    {"type": "text", "text": ""},
                    {"type": "thinking", "thinking": "", "signature": "sig"},
                    {"type": "redacted_thinking", "data": "EmwKAhgB"},
                    {"type": "tool_use", "id": "toolu_a", "name": "f", "input": { "a" : [1, 2.50] }},
                    {"type": "tool_use", "name": "g", "input": "{\"a\":1}"},
                    {"type": "tool_use", "id": "", "name": "h", "input": null},
                    {"type": "tool_use", "id": "toolu_i", "name": "i"},
                    {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}},
                    {"type": "image"},
                    "text",
                    null
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": "7", "output_tokens": 1.50, "cache_read_input_tokens": 0, "thinking_tokens": -1}
            }"#
            .to_owned(),
        ),
        (
            "message-without-id-or-model",
            r#"{"content":[],"usage":{}}"#.to_owned(),
        ),
        (
            "content-not-array",
            r#"{"id":"m","content":"text","usage":{"output_tokens":1}}"#.to_owned(),
        ),
        (
            "content-null",
            r#"{"id":"m","content":null}"#.to_owned(),
        ),
        (
            "sse-with-crlf-and-events",
            [
                "event: message_start",
                r#"data: {"type":"message_start","message":{"id":"","model":"","usage":{"input_tokens":"3"}}}"#,
                "",
                "event: content_block_start",
                r#"data:{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"f","input":{"a":1}}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"b\":2}"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":"start"}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" more"}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"mixed"}}"#,
                r#"data: {"type":"content_block_stop","index":1}"#,
                r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"never started"}}"#,
                r#"data: {"type":"content_block_stop","index":2}"#,
                r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"text","text":"never stopped"}}"#,
                r#"data: {"type":"message_delta","usage":{"output_tokens":4,"cache_creation_input_tokens":2}}"#,
                r#"data: {"type":"error","error":{"message":"bad"}}"#,
                ": keep-alive",
                "data: {not json",
            ]
            .join("\r\n"),
        ),
        ("empty-object", "{}".to_owned()),
        ("array-body", "[1,2]".to_owned()),
        ("not-json", "not json".to_owned()),
        ("empty-body", String::new()),
        (
            "escaped-and-unicode",
            "{\"id\":\"m\",\"content\":[{\"type\":\"text\",\"text\":\"caf\\u00e9 \\/ \\ud83d\\ude80 <b>&</b>\"}]}"
                .to_owned(),
        ),
    ];
    let mut cases: Vec<Case> = bodies
        .iter()
        .map(|(name, body)| response(name, "claude-test", &[body.as_str()]))
        .collect();
    cases.push(response("no-events", "claude-test", &[]));
    cases
}
