// Ported from CLIProxyAPI internal/translator/codex/claude/codex_claude_commentary_phase_test.go
// (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;

fn blocks(events: &[Value]) -> Vec<Value> {
    let mut stream = CodexToClaudeStream::new(&json!({"messages":[]}));
    let out: String = events
        .iter()
        .map(|event| stream.translate_line(format!("data: {event}").as_bytes()))
        .collect();
    let mut blocks = Vec::new();
    let mut active: Option<(usize, Value)> = None;
    for line in out.lines().filter_map(|line| line.strip_prefix("data: ")) {
        let event: Value = serde_json::from_str(line).unwrap();
        match event["type"].as_str().unwrap() {
            "content_block_start" => {
                assert!(active.is_none(), "interleaved blocks: {out}");
                let index = event["index"].as_u64().unwrap() as usize;
                assert_eq!(index, blocks.len(), "{out}");
                active = Some((index, event["content_block"].clone()));
            }
            "content_block_delta" => {
                let (index, block) = active.as_mut().expect("delta has an open block");
                assert_eq!(event["index"], *index);
                let delta = &event["delta"];
                let field = match delta["type"].as_str().unwrap() {
                    "text_delta" => "text",
                    "thinking_delta" => "thinking",
                    "signature_delta" => "signature",
                    "input_json_delta" => "partial_json",
                    other => panic!("unexpected delta {other}"),
                };
                let text = format!(
                    "{}{}",
                    block[field].as_str().unwrap_or_default(),
                    delta[field].as_str().unwrap()
                );
                block[field] = text.into();
            }
            "content_block_stop" => {
                let (index, block) = active.take().expect("stop has an open block");
                assert_eq!(event["index"], index);
                blocks.push(block);
            }
            _ => {}
        }
    }
    assert!(active.is_none(), "unclosed block: {out}");
    blocks
}

fn message(phase: &str, text: &str) -> Value {
    json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","phase":phase,"content":[{"type":"output_text","text":text}]}})
}

fn added(phase: &str) -> Value {
    json!({"type":"response.output_item.added","item":{"type":"message","role":"assistant","phase":phase}})
}

fn completed() -> Value {
    json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}})
}

#[test]
fn commentary_streams_as_marked_thinking_before_final_answer() {
    let events = [
        added("commentary"),
        json!({"type":"response.content_part.added","part":{"type":"output_text"}}),
        json!({"type":"response.output_text.delta","delta":"Checking the "}),
        json!({"type":"response.output_text.delta","delta":"gate first."}),
        json!({"type":"response.content_part.done","part":{"type":"output_text","text":"Checking the gate first."}}),
        message("commentary", "Checking the gate first."),
        added("final_answer"),
        json!({"type":"response.content_part.added","part":{"type":"output_text"}}),
        json!({"type":"response.output_text.delta","delta":"The merge was refused."}),
        json!({"type":"response.content_part.done","part":{"type":"output_text","text":"The merge was refused."}}),
        message("final_answer", "The merge was refused."),
        completed(),
    ];
    assert_eq!(
        blocks(&events),
        [
            json!({"type":"thinking","thinking":"Checking the gate first.","signature":"cpa:commentary:v1"}),
            json!({"type":"text","text":"The merge was refused."}),
        ]
    );
}

#[test]
fn commentary_done_only_messages_and_later_fallbacks_are_each_emitted_once() {
    assert_eq!(
        blocks(&[
            message("commentary", "On it."),
            message("final_answer", "Done."),
            completed()
        ]),
        [
            json!({"type":"thinking","thinking":"On it.","signature":"cpa:commentary:v1"}),
            json!({"type":"text","text":"Done."})
        ]
    );
    for second_added in [false, true] {
        let mut events = vec![
            added("commentary"),
            json!({"type":"response.output_text.delta","delta":"Checking."}),
            message("commentary", "Checking."),
        ];
        if second_added {
            events.push(added("final_answer"));
        }
        events.extend([message("final_answer", "All checks passed."), completed()]);
        assert_eq!(
            blocks(&events),
            [
                json!({"type":"thinking","thinking":"Checking.","signature":"cpa:commentary:v1"}),
                json!({"type":"text","text":"All checks passed."})
            ]
        );
    }
}

#[test]
fn commentary_does_not_take_reasoning_signatures_or_leave_reasoning_open() {
    for reasoning_done in [false, true] {
        let mut events = vec![
            json!({"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"fallback"}}),
            json!({"type":"response.reasoning_summary_part.added"}),
            json!({"type":"response.reasoning_summary_text.delta","delta":"Reasoning step"}),
            json!({"type":"response.reasoning_summary_part.done"}),
        ];
        if reasoning_done {
            events.push(json!({"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"final_signature"}}));
        }
        events.extend([
            added("commentary"),
            json!({"type":"response.output_text.delta","delta":"Looking."}),
            message("commentary", "Looking."),
            added("final_answer"),
            json!({"type":"response.output_text.delta","delta":"All green."}),
            message("final_answer", "All green."),
            completed(),
        ]);
        assert_eq!(
            blocks(&events),
            [
                json!({"type":"thinking","thinking":"Reasoning step","signature":if reasoning_done {"final_signature"} else {"fallback"}}),
                json!({"type":"thinking","thinking":"Looking.","signature":"cpa:commentary:v1"}),
                json!({"type":"text","text":"All green."}),
            ]
        );
    }
    assert_eq!(
        blocks(&[
            json!({"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"parked"}}),
            added("commentary"),
            json!({"type":"response.output_text.delta","delta":"Checking."}),
            message("commentary", "Checking."),
            json!({"type":"response.output_item.done","item":{"type":"reasoning","summary":[{"type":"summary_text","text":"Thought."}],"encrypted_content":""}}),
            completed(),
        ]),
        [
            json!({"type":"thinking","thinking":"Checking.","signature":"cpa:commentary:v1"}),
            json!({"type":"thinking","thinking":"Thought.","signature":"parked"})
        ]
    );
}

#[test]
fn commentary_tool_reasoning_and_final_answer_preserve_block_order() {
    assert_eq!(
        blocks(&[
            added("commentary"),
            json!({"type":"response.output_text.delta","delta":"Calling a tool."}),
            json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"exec"}}),
            json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"exec","arguments":"{}"}}),
            json!({"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"sig"}}),
            json!({"type":"response.reasoning_summary_text.delta","delta":"Tool result reasoning."}),
            json!({"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"sig"}}),
            added("final_answer"),
            message("final_answer", "Done."),
            completed(),
        ]),
        [
            json!({"type":"thinking","thinking":"Calling a tool.","signature":"cpa:commentary:v1"}),
            json!({"type":"tool_use","id":"call_1","name":"exec","input":{},"partial_json":"{}"}),
            json!({"type":"thinking","thinking":"Tool result reasoning.","signature":"sig"}),
            json!({"type":"text","text":"Done."}),
        ]
    );
}

#[test]
fn commentary_and_final_messages_keep_their_non_stream_block_types() {
    for final_phase in ["final_answer", ""] {
        for commentary in [
            json!([{"type":"output_text","text":"Checking."}]),
            json!("Checking."),
        ] {
            let event = json!({"type":"response.completed","response":{"output":[
                {"type":"message","phase":"commentary","content":commentary},
                {"type":"reasoning","summary":[{"type":"summary_text","text":"Thought."}],"encrypted_content":"sig"},
                {"type":"message","phase":final_phase,"content":[{"type":"output_text","text":"Done."}]}
            ]}});
            let response =
                convert_codex_response_to_claude_non_stream(&json!({"messages":[]}), &event)
                    .unwrap();
            assert_eq!(
                response["content"],
                json!([
                    {"type":"thinking","thinking":"Checking.","signature":"cpa:commentary:v1"},
                    {"type":"thinking","thinking":"Thought.","signature":"sig"},
                    {"type":"text","text":"Done."}
                ])
            );
        }
    }
}

#[test]
fn commentary_final_answer_and_phaseless_text_remain_text() {
    for phase in ["final_answer", ""] {
        assert_eq!(
            blocks(&[
                added(phase),
                json!({"type":"response.output_text.delta","delta":"Hi."}),
                message(phase, "Hi."),
                completed()
            ]),
            [json!({"type":"text","text":"Hi."})]
        );
    }
}

#[test]
fn commentary_round_trip_preserves_replay_order() {
    let events = [
        message("commentary", "Checking."),
        message("final_answer", "Before tool."),
        json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}}),
        completed(),
    ];
    let stream_content = blocks(&events);
    let native_event = json!({"type":"response.completed","response":{"output":[
        {"type":"message","phase":"commentary","content":[{"type":"output_text","text":"Checking."}]},
        {"type":"message","phase":"final_answer","content":[{"type":"output_text","text":"Before tool."}]},
        {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}
    ]}});
    let native =
        convert_codex_response_to_claude_non_stream(&json!({"messages":[]}), &native_event)
            .unwrap();
    for content in [Value::Array(stream_content), native["content"].clone()] {
        let request = json!({"messages":[{"role":"assistant","content":content},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"result"}]}]});
        for convert in [
            crate::codex::claude::convert_claude_request_to_codex,
            crate::codex::claude::convert_claude_request_to_codex_with_compat,
        ] {
            let (out, error) = convert("gpt-5.4", &request);
            assert_eq!(error, None);
            let items = out["input"].as_array().unwrap();
            assert_eq!(items[0]["phase"], "commentary");
            assert_eq!(items[0]["content"][0]["text"], "Checking.");
            assert_eq!(items[1]["content"][0]["text"], "Before tool.");
            assert_eq!(items[2]["type"], "function_call");
            assert_eq!(items[3]["type"], "function_call_output");
            assert!(!out.to_string().contains("cpa:commentary:v1"));
        }
    }
}

#[test]
fn commentary_marker_is_plain_text_for_other_native_targets() {
    let request = json!({"messages":[{"role":"user","content":"first"},
        {"role":"assistant","content":[
            {"type":"thinking","thinking":"COMMENTARY","signature":" cpa:commentary:v1 "},
            {"type":"thinking","thinking":"","signature":"cpa:commentary:v1"},
            {"type":"text","text":"FINAL"}]},
        {"role":"user","content":"next"}]});
    for compat in [false, true] {
        let chat = if compat {
            crate::openai::claude::convert_claude_request_to_openai_with_compat
        } else {
            crate::openai::claude::convert_claude_request_to_openai
        };
        let (out, error) = chat("test-model", &request, false);
        assert_eq!(error, None);
        assert!(
            out["messages"][1]["content"]
                .to_string()
                .contains("COMMENTARY")
        );
        assert!(out["messages"][1].get("reasoning_content").is_none());
        assert!(!out.to_string().contains("cpa:commentary:v1"));
        let gemini = if compat {
            crate::gemini::claude::convert_claude_request_to_gemini_with_compat
        } else {
            crate::gemini::claude::convert_claude_request_to_gemini
        };
        let (out, error) = gemini(
            "test-model",
            &request,
            false,
            crate::models::ModelCatalog::embedded(),
        );
        assert_eq!(error, None);
        assert_eq!(
            out["contents"][1]["parts"],
            json!([{"text":"COMMENTARY"},{"text":"FINAL"}])
        );
        assert!(!out.to_string().contains("cpa:commentary:v1"));
        let interactions = if compat {
            crate::interactions::claude::convert_claude_request_to_interactions_with_compat
        } else {
            crate::interactions::claude::convert_claude_request_to_interactions
        };
        let (out, error) = interactions("test-model", &request, false);
        assert_eq!(error, None);
        assert!(out.to_string().contains("COMMENTARY"));
        assert!(out.to_string().contains("FINAL"));
        assert!(!out.to_string().contains("cpa:commentary:v1"));
        assert!(!out.to_string().contains("\"type\":\"thought\""));
    }
    for convert in [
        crate::codex::claude::convert_claude_request_to_codex,
        crate::codex::claude::convert_claude_request_to_codex_with_compat,
    ] {
        let (out, error) = convert("gpt-5.4", &request);
        assert_eq!(error, None);
        assert_eq!(out["input"][1]["phase"], "commentary");
        assert!(!out.to_string().contains("cpa:commentary:v1"));
    }
}

fn check_empty_commentary_alignment(target: usize) {
    let plain = json!({"messages":[
        {"role":"assistant","content":[
            {"type":"tool_use","id":"call_a","name":"lookup_a","input":{}},
            {"type":"tool_use","id":"call_b","name":"lookup_b","input":{}}]},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"call_b","content":"b"},
            {"type":"tool_result","tool_use_id":"call_a","content":"a"}]}]});
    for count in [1, 2] {
        let mut marked = plain.clone();
        marked["messages"].as_array_mut().unwrap().insert(1, json!({"role":"assistant","content":
            vec![json!({"type":"thinking","thinking":"","signature":" cpa:commentary:v1 "});count]}));
        for compat in [false, true] {
            let chat = if compat {
                crate::openai::claude::convert_claude_request_to_openai_with_compat
            } else {
                crate::openai::claude::convert_claude_request_to_openai
            };
            if target == 0 {
                assert_eq!(
                    chat("test-model", &marked, false),
                    chat("test-model", &plain, false),
                    "Chat compat={compat}"
                );
            }
            let codex = if compat {
                crate::codex::claude::convert_claude_request_to_codex_with_compat
            } else {
                crate::codex::claude::convert_claude_request_to_codex
            };
            if target == 1 {
                assert_eq!(
                    codex("gpt-5.4", &marked),
                    codex("gpt-5.4", &plain),
                    "Codex compat={compat}"
                );
            }
            let gemini = if compat {
                crate::gemini::claude::convert_claude_request_to_gemini_with_compat
            } else {
                crate::gemini::claude::convert_claude_request_to_gemini
            };
            let models = crate::models::ModelCatalog::embedded();
            if target == 2 {
                assert_eq!(
                    gemini("test-model", &marked, false, models),
                    gemini("test-model", &plain, false, models),
                    "Gemini compat={compat}"
                );
            }
            let interactions = if compat {
                crate::interactions::claude::convert_claude_request_to_interactions_with_compat
            } else {
                crate::interactions::claude::convert_claude_request_to_interactions
            };
            if target == 3 {
                assert_eq!(
                    interactions("test-model", &marked, false),
                    interactions("test-model", &plain, false),
                    "Interactions compat={compat}"
                );
            }
            let mut restored = marked.clone();
            crate::signature::sanitize_claude_messages_for_claude_upstream(
                &mut restored,
                "test-model",
                compat,
            );
            assert_eq!(restored, plain);
        }
    }
}

#[test]
fn r2_empty_commentary_chat_alignment() {
    check_empty_commentary_alignment(0);
}

#[test]
fn r2_empty_commentary_codex_alignment() {
    check_empty_commentary_alignment(1);
}

#[test]
fn r2_empty_commentary_gemini_alignment() {
    check_empty_commentary_alignment(2);
}

#[test]
fn r2_empty_commentary_interactions_alignment() {
    check_empty_commentary_alignment(3);
}
