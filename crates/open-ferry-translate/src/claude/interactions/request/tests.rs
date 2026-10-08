// Ported from CLIProxyAPI internal/translator/claude/interactions/interactions_claude_test.go
// (the request tests) (v8.0.20, MIT), and interactions_claude_user_turn_test.go and
// interactions_claude_instruction_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// All tests are ported. Upstream's user turn tests go through the registry;
// ours call the translator and check that the registry gives the same
// refusal. The tests marked "Not upstream's" are new; their expected output
// comes from upstream.

use serde_json::{Value, json};

use super::*;

/// [`super::convert_interactions_request_to_claude`], for requests it
/// doesn't refuse.
fn convert_interactions_request_to_claude(model: &str, request: &Value, stream: bool) -> Value {
    let (body, err) = super::convert_interactions_request_to_claude(model, request, stream);
    assert_eq!(err, None, "{body}");
    body
}

fn translate(model: &str, input: &str, stream: bool) -> Value {
    convert_interactions_request_to_claude(model, &serde_json::from_str(input).unwrap(), stream)
}

// TestConvertInteractionsRequestToClaudeWithToolMessagesDirect
#[test]
fn with_tool_messages_direct() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","system_instruction":"be brief","input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]},{"type":"function_call","name":"lookup","call_id":"toolu_1","arguments":{"q":"x"}},{"type":"function_result","name":"lookup","call_id":"toolu_1","result":{"ok":true}}]}"#,
        false,
    );
    assert_eq!(out["system"], "be brief", "{out}");
    assert_eq!(out["messages"][0]["content"][0]["text"], "hi", "{out}");
    assert_eq!(
        out["messages"][1]["content"][0]["type"], "tool_use",
        "{out}"
    );
    assert_eq!(
        out["messages"][2]["content"][0]["type"], "tool_result",
        "{out}"
    );
    assert_eq!(
        out["messages"][2]["content"][0]["tool_use_id"], "toolu_1",
        "{out}"
    );
}

// TestConvertInteractionsRequestToClaudePropagatesIsError
#[test]
fn propagates_is_error() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","input":[{"type":"function_result","name":"lookup","call_id":"toolu_err","result":"command failed","is_error":true}]}"#,
        false,
    );
    assert_eq!(
        out["messages"][0]["content"][0]["type"], "tool_result",
        "{out}"
    );
    assert_eq!(out["messages"][0]["content"][0]["is_error"], true, "{out}");
}

// TestConvertInteractionsRequestToClaudeGroupsConsecutiveRoleTurns
#[test]
fn groups_consecutive_role_turns() {
    let out = translate(
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
    );
    let messages = out["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{out}");
    let types: Vec<&str> = messages[0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["thinking", "text", "tool_use", "tool_use"], "{out}");
    let ids: Vec<&str> = messages[1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["tool_use_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["call_1", "call_2"], "{out}");
}

// TestConvertInteractionsRequestToClaudeDoesNotMergeAcrossRoleChanges
#[test]
fn does_not_merge_across_role_changes() {
    let out = translate(
        "claude-test",
        r#"{
            "input":[
                {"type":"model_output","content":"first assistant"},
                {"type":"user_input","content":"user reply"},
                {"type":"model_output","content":"second assistant"}
            ]
        }"#,
        false,
    );
    let roles: Vec<&str> = out["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["assistant", "user", "assistant"], "{out}");
}

// TestConvertInteractionsRequestToClaudeStringInputDirect
#[test]
fn string_input_direct() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","input":"hello"}"#,
        false,
    );
    assert_eq!(out["messages"][0]["role"], "user", "{out}");
    assert_eq!(out["messages"][0]["content"][0]["text"], "hello", "{out}");
}

// TestConvertInteractionsRequestToClaudeMapsGenerationConfigToolsAndStreamDirect
#[test]
fn maps_generation_config_tools_and_stream_direct() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","stream":true,"input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]}],"tools":[{"type":"function","name":"lookup","description":"Lookup data","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}],"generation_config":{"max_output_tokens":99,"top_p":0.7,"stop_sequences":["END"],"tool_choice":{"type":"function","name":"lookup"},"thinking_level":"high"}}"#,
        false,
    );
    assert_eq!(out["stream"], true, "{out}");
    assert_eq!(out["max_tokens"], 99, "{out}");
    assert_eq!(
        out["tools"][0]["input_schema"]["properties"]["q"]["type"], "string",
        "{out}"
    );
    assert_eq!(out["tool_choice"]["name"], "lookup", "{out}");
    assert!(
        out["thinking"]["type"]
            .as_str()
            .is_some_and(|kind| !kind.is_empty()),
        "{out}"
    );
}

// TestConvertInteractionsRequestToClaudeAcceptsImageContent
#[test]
fn accepts_image_content() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","input":[{"type":"user_input","content":[{"type":"image","mime_type":"image/png","data":"aGVsbG8="}]}]}"#,
        false,
    );
    let block = &out["messages"][0]["content"][0];
    assert_eq!(block["type"], "image", "{out}");
    assert_eq!(block["source"]["media_type"], "image/png", "{out}");
    assert_eq!(block["source"]["data"], "aGVsbG8=", "{out}");
}

// TestConvertInteractionsRequestToClaudePreservesNonImageMediaContent
#[test]
fn preserves_non_image_media_content() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","input":[{"type":"thought","content":[{"type":"audio","mime_type":"audio/wav","data":"UklGRg=="},{"type":"video","mime_type":"video/mp4","data":"AAAAIGZ0eXA="},{"type":"document","mime_type":"application/pdf","data":"JVBERi0="}]}]}"#,
        false,
    );
    let message = &out["messages"][0];
    assert_eq!(message["role"], "assistant", "{out}");
    let content = message["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text", "{out}");
    assert_eq!(content[1]["type"], "text", "{out}");
    assert_eq!(content[2]["type"], "document", "{out}");
    assert!(
        !content.iter().any(|block| block["type"] == "image"),
        "{out}"
    );
}

// Not upstream's: the whole output for camel-case settings overridden by
// `reasoning` and a top-level tool choice, Gemini-style turns, a call whose
// ID is made from its name, and declarations nested in a tool.
#[test]
fn settings_turns_and_declarations() {
    let out = translate(
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
    );
    assert_eq!(
        out,
        json!({
            "model": "claude-x",
            "max_tokens": 5,
            "messages": [
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "hm"}]},
                {"role": "user", "content": [{"type": "text", "text": "x"}, {"type": "text", "text": "p"}]},
                {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "toolu_my_tool", "name": "my_tool", "input": {}}],
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_my_tool", "content": "{\"ok\":true}"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/gif", "data": "R0"}},
                    ],
                },
            ],
            "system": "a\nb",
            "top_p": 0.5,
            "stop_sequences": ["s"],
            "thinking": {"type": "adaptive"},
            "tool_choice": {"type": "tool", "name": "my_tool"},
            "output_config": {"effort": "turbo"},
            "tools": [
                {
                    "name": "my_tool",
                    "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}},
                },
                {"name": "f2", "input_schema": {"type": "object", "properties": {}}, "description": "d"},
            ],
        })
    );
}

// Not upstream's: a call's arguments and a result, read with each number as
// written, go on with that text, as upstream copies them (checked with Go).
#[test]
fn numbers_keep_their_text() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let request = format!(
        r#"{{"input":[{{"type":"function_call","id":"a","name":"f","arguments":{spelled}}},{{"type":"function_result","call_id":"a","result":{spelled}}}]}}"#
    );
    let request = crate::json::exact::from_str(&request).unwrap();
    let out = convert_interactions_request_to_claude("m", &request, false);
    assert_eq!(
        out["messages"][0]["content"][0]["input"].to_string(),
        spelled
    );
    assert_eq!(out["messages"][1]["content"][0]["content"], spelled);
}

const INTERACTIONS_HISTORY: &str = r#"{"type":"user_input","content":[{"type":"text","text":"a"}]},{"type":"model_output","content":[{"type":"text","text":"b"}]}"#;
const PDF_BY_URI: &str =
    r#"{"type":"document","mime_type":"application/pdf","uri":"https://example.test/a.pdf"}"#;
const VIDEO_BY_URI: &str =
    r#"{"type":"video","mime_type":"video/mp4","uri":"https://example.test/a.mp4"}"#;
const INLINE_AUDIO: &str = r#"{"type":"audio","mime_type":"audio/wav","data":"UklGRg=="}"#;
const INLINE_VIDEO: &str = r#"{"type":"video","mime_type":"video/mp4","data":"AAAAIGZ0eXA="}"#;

/// Developer and system steps in the shapes an Interactions client can
/// send. Each one carries the text "note".
const CLAUDE_INSTRUCTION_STEPS: [(&str, &str); 7] = [
    (
        "developer user_input",
        r#"{"type":"user_input","role":"developer","content":[{"type":"text","text":"note"}]}"#,
    ),
    (
        "system user_input",
        r#"{"type":"user_input","role":"system","content":[{"type":"text","text":"note"}]}"#,
    ),
    (
        "developer string content",
        r#"{"type":"user_input","role":"developer","content":"note"}"#,
    ),
    (
        "bare system object",
        r#"{"role":"system","content":"note"}"#,
    ),
    ("bare system text", r#"{"type":"system","text":"note"}"#),
    (
        "developer native parts",
        r#"{"role":"developer","parts":[{"text":"note"}]}"#,
    ),
    (
        "developer wrapper",
        r#"{"role":"developer","steps":[{"type":"user_input","content":[{"type":"text","text":"note"}]}]}"#,
    ),
];

/// A user step whose only part Claude can't carry.
const CLAUDE_EMPTIED_STEP: &str = r#"{"type":"user_input","content":[{"type":"video","mime_type":"video/mp4","uri":"https://example.test/a.mp4"}]}"#;

/// `ConvertInteractionsRequestToClaude` for model `m`, with its refusal,
/// checking that the registration carries the same refusal.
fn envelope(input: &str) -> (Value, Option<UnsupportedPartError>) {
    let request: Value = serde_json::from_str(input).expect("test request is valid JSON");
    let (body, err) = super::convert_interactions_request_to_claude("m", &request, false);
    let registered = crate::registry::Registry::global().translate_request_checked(
        &"interactions".into(),
        &"claude".into(),
        "m",
        request,
        false,
    );
    assert_eq!(registered.err(), err, "the registration's refusal");
    (body, err)
}

/// Two history turns followed by a final user step holding `final_content`.
fn final_turn(final_content: &str) -> (Value, Option<UnsupportedPartError>) {
    envelope(&format!(
        r#"{{"model":"m","input":[{INTERACTIONS_HISTORY},{{"type":"user_input","content":[{final_content}]}}]}}"#
    ))
}

#[track_caller]
fn require_media_refusal((body, err): (Value, Option<UnsupportedPartError>), want: &str) {
    let err = err.unwrap_or_else(|| panic!("no refusal, want {want}; body = {body}"));
    assert_eq!(err.part_type, want, "{body}");
    assert_eq!(err.status_code(), 400);
    assert_eq!(err.to_string(), format!("unsupported content part: {want}"));
    assert!(body.is_object(), "{body}");
}

#[track_caller]
fn require_sent((body, err): (Value, Option<UnsupportedPartError>)) -> Value {
    assert_eq!(err, None, "{body}");
    body
}

// TestInteractionsToClaudeURIOnlyDocumentBecomesAURLSource
#[test]
fn uri_only_document_becomes_a_url_source() {
    for field in ["uri", "file_uri", "fileUri", "url"] {
        let body = require_sent(final_turn(&format!(
            r#"{{"type":"document","mime_type":"application/pdf","{field}":"https://example.test/a.pdf"}}"#
        )));
        assert_eq!(
            body["messages"].as_array().map(Vec::len),
            Some(3),
            "{field}: {body}"
        );
        assert_eq!(
            body["messages"][2]["content"][0],
            json!({"type": "document", "source": {"type": "url", "url": "https://example.test/a.pdf"}}),
            "{field}: {body}"
        );
    }
}

// TestInteractionsToClaudeURIOnlyImageBecomesAURLSource
#[test]
fn uri_only_image_becomes_a_url_source() {
    let body = require_sent(final_turn(
        r#"{"type":"image","mime_type":"image/png","uri":"https://example.test/a.png"}"#,
    ));
    assert_eq!(
        body["messages"][2]["content"][0],
        json!({"type": "image", "source": {"type": "url", "url": "https://example.test/a.png"}}),
        "{body}"
    );
}

// TestInteractionsToClaudeInlineDocumentStaysBase64
#[test]
fn inline_document_stays_base64() {
    let body = require_sent(final_turn(
        r#"{"type":"document","mime_type":"application/pdf","data":"JVBERi0xLjQK"}"#,
    ));
    let source = &body["messages"][2]["content"][0]["source"];
    assert_eq!(source["type"], "base64", "{body}");
    assert_eq!(source["media_type"], "application/pdf", "{body}");
    assert_eq!(source["data"], "JVBERi0xLjQK", "{body}");
}

// TestInteractionsToClaudeUnrepresentableMediaOnlyTurnIsRefused
#[test]
fn unrepresentable_media_only_turn_is_refused() {
    let cases = [
        ("video by uri", VIDEO_BY_URI, "video"),
        (
            "audio by uri",
            r#"{"type":"audio","mime_type":"audio/wav","uri":"https://example.test/a.wav"}"#,
            "audio",
        ),
        (
            "document by gs uri",
            r#"{"type":"document","mime_type":"application/pdf","uri":"gs://bucket/a.pdf"}"#,
            "document",
        ),
        (
            "document by files api uri",
            r#"{"type":"document","mime_type":"application/pdf","uri":"files/abc123"}"#,
            "document",
        ),
        (
            "document with nothing",
            r#"{"type":"document","mime_type":"application/pdf"}"#,
            "document",
        ),
        ("file with nothing", r#"{"type":"file"}"#, "document"),
        (
            "image by gs uri",
            r#"{"type":"image","mime_type":"image/png","uri":"gs://bucket/a.png"}"#,
            "image",
        ),
    ];
    for (name, part, want) in cases {
        let (body, err) = final_turn(part);
        assert_eq!(
            err.as_ref().map(|err| err.part_type.as_str()),
            Some(want),
            "{name}: {body}"
        );
        require_media_refusal((body, err), want);
    }
}

// TestInteractionsToClaudeRefusalIsNotHiddenBySystemInstructionOrHistory
#[test]
fn refusal_is_not_hidden_by_system_instruction_or_history() {
    require_media_refusal(
        envelope(&format!(
            r#"{{"model":"m","system_instruction":"be brief","input":[{INTERACTIONS_HISTORY},{{"type":"user_input","content":[{VIDEO_BY_URI}]}}]}}"#
        )),
        "video",
    );
}

// TestInteractionsToClaudeRefusesAnEmptiedTurnBeforeALaterTextTurn
#[test]
fn refuses_an_emptied_turn_before_a_later_text_turn() {
    require_media_refusal(
        envelope(&format!(
            r#"{{"model":"m","input":[{{"type":"user_input","content":[{VIDEO_BY_URI}]}},{{"type":"model_output","content":[{{"type":"text","text":"ok"}}]}},{{"type":"user_input","content":[{{"type":"text","text":"next"}}]}}]}}"#
        )),
        "video",
    );
}

// TestInteractionsToClaudeEmptyTextDoesNotHideAnUnrepresentableMedia
#[test]
fn empty_text_does_not_hide_an_unrepresentable_media() {
    require_media_refusal(
        final_turn(&format!(r#"{{"type":"text","text":""}},{VIDEO_BY_URI}"#)),
        "video",
    );
}

// TestInteractionsToClaudeUnrepresentableMediaBesideTextStillSucceeds
#[test]
fn unrepresentable_media_beside_text_still_succeeds() {
    let body = require_sent(final_turn(&format!(
        r#"{{"type":"text","text":"keep me"}},{VIDEO_BY_URI}"#
    )));
    assert_eq!(
        body["messages"][2]["content"],
        json!([{"type": "text", "text": "keep me"}]),
        "{body}"
    );
}

// TestInteractionsToClaudeFlatContentInputKeepsTextBesideMedia
#[test]
fn flat_content_input_keeps_text_beside_media() {
    let body = require_sent(envelope(&format!(
        r#"{{"model":"m","input":[{{"type":"text","text":"Describe this"}},{VIDEO_BY_URI}]}}"#
    )));
    assert_eq!(
        body["messages"][0]["content"][0]["text"], "Describe this",
        "{body}"
    );
}

// TestInteractionsToClaudeFlatContentInputConvertsAndRefusesBareMedia
#[test]
fn flat_content_input_converts_and_refuses_bare_media() {
    let body = require_sent(envelope(&format!(
        r#"{{"model":"m","input":[{INTERACTIONS_HISTORY},{PDF_BY_URI}]}}"#
    )));
    assert_eq!(
        body["messages"][2]["content"][0]["source"]["url"], "https://example.test/a.pdf",
        "{body}"
    );
    require_media_refusal(
        envelope(&format!(
            r#"{{"model":"m","input":[{INTERACTIONS_HISTORY},{VIDEO_BY_URI}]}}"#
        )),
        "video",
    );
}

// TestInteractionsToClaudeNeighbouringUserStepsShareOneTurn
#[test]
fn neighbouring_user_steps_share_one_turn() {
    let cases = [
        (
            "text step after the media step",
            format!(
                r#"{{"model":"m","input":[{INTERACTIONS_HISTORY},{{"type":"user_input","content":[{VIDEO_BY_URI}]}},{{"type":"user_input","content":[{{"type":"text","text":"keep me"}}]}}]}}"#
            ),
        ),
        (
            "text step before the media step",
            format!(
                r#"{{"model":"m","input":[{INTERACTIONS_HISTORY},{{"type":"user_input","content":[{{"type":"text","text":"keep me"}}]}},{{"type":"user_input","content":[{VIDEO_BY_URI}]}}]}}"#
            ),
        ),
        (
            "tool result beside the media step",
            format!(
                r#"{{"model":"m","input":[{{"type":"user_input","content":[{{"type":"text","text":"a"}}]}},{{"type":"function_call","name":"lookup","call_id":"toolu_1","arguments":{{}}}},{{"type":"function_result","name":"lookup","call_id":"toolu_1","result":{{"ok":true}}}},{{"type":"user_input","content":[{VIDEO_BY_URI}]}}]}}"#
            ),
        ),
    ];
    for (name, input) in cases {
        let (body, err) = envelope(&input);
        assert_eq!(err, None, "{name}: {body}");
    }
}

// TestInteractionsToClaudeAssistantStepEndsTheUserTurn
#[test]
fn assistant_step_ends_the_user_turn() {
    // A text step in the same turn keeps it.
    require_sent(envelope(&format!(
        r#"{{"model":"m","input":[{{"type":"user_input","content":[{{"type":"text","text":"a"}}]}},{{"type":"user_input","content":[{VIDEO_BY_URI}]}},{{"type":"model_output","content":[{{"type":"text","text":"b"}}]}},{{"type":"user_input","content":[{{"type":"text","text":"c"}}]}}]}}"#
    )));
    require_media_refusal(
        envelope(&format!(
            r#"{{"model":"m","input":[{{"type":"user_input","content":[{VIDEO_BY_URI}]}},{{"type":"model_output","content":[{{"type":"text","text":"b"}}]}},{{"type":"user_input","content":[{{"type":"text","text":"c"}}]}}]}}"#
        )),
        "video",
    );
}

// TestInteractionsToClaudeExportedWrapperKeepsAJSONBody
#[test]
fn exported_wrapper_keeps_a_json_body() {
    let (body, err) = envelope(&format!(
        r#"{{"model":"m","input":[{{"type":"user_input","content":[{VIDEO_BY_URI}]}}]}}"#
    ));
    assert!(body.is_object(), "{body}");
    assert!(err.is_some());
}

// TestInteractionsToClaudeInlineAudioOrVideoOnlyUserTurnIsRefused
#[test]
fn inline_audio_or_video_only_user_turn_is_refused() {
    let cases = [
        ("audio", INLINE_AUDIO.to_owned(), "audio"),
        ("video", INLINE_VIDEO.to_owned(), "video"),
        (
            "empty text does not hide audio",
            format!(r#"{{"type":"text","text":""}},{INLINE_AUDIO}"#),
            "audio",
        ),
        (
            "whitespace text does not hide video",
            format!(r#"{{"type":"text","text":" \n"}},{INLINE_VIDEO}"#),
            "video",
        ),
    ];
    for (name, content, want) in cases {
        let (body, err) = final_turn(&content);
        assert!(
            !body.to_string().contains("omitted"),
            "{name}: a placeholder was made for a user attachment: {body}"
        );
        require_media_refusal((body, err), want);
    }
}

// TestInteractionsToClaudeInlineAudioBesideTextSendsTheTextWithoutAPlaceholder
#[test]
fn inline_audio_beside_text_sends_the_text_without_a_placeholder() {
    let body = require_sent(final_turn(&format!(
        r#"{{"type":"text","text":"keep me"}},{INLINE_AUDIO},{INLINE_VIDEO}"#
    )));
    assert_eq!(
        body["messages"][2]["content"],
        json!([{"type": "text", "text": "keep me"}]),
        "{body}"
    );
}

// TestInteractionsToClaudeAssistantMediaKeepsThePlaceholder
#[test]
fn assistant_media_keeps_the_placeholder() {
    let body = require_sent(envelope(&format!(
        r#"{{"model":"m","input":[{{"type":"user_input","content":[{{"type":"text","text":"a"}}]}},{{"type":"model_output","content":[{INLINE_AUDIO}]}},{{"type":"user_input","content":[{{"type":"text","text":"c"}}]}}]}}"#
    )));
    assert_eq!(
        body["messages"][1]["content"][0]["text"], "[audio content omitted]",
        "{body}"
    );
}

// TestInteractionsToClaudeInlineAndURLDocumentsStillConvert
#[test]
fn inline_and_url_documents_still_convert() {
    let inline = require_sent(final_turn(
        r#"{"type":"document","mime_type":"application/pdf","data":"JVBERi0xLjQK"}"#,
    ));
    assert_eq!(
        inline["messages"][2]["content"][0]["source"]["type"], "base64",
        "{inline}"
    );
    let by_url = require_sent(final_turn(PDF_BY_URI));
    assert_eq!(
        by_url["messages"][2]["content"][0]["source"]["url"], "https://example.test/a.pdf",
        "{by_url}"
    );
}

// TestInteractionsToClaudeDeveloperOrSystemStepDoesNotHideAnEmptiedUserTurn
#[test]
fn developer_or_system_step_does_not_hide_an_emptied_user_turn() {
    for (name, note) in CLAUDE_INSTRUCTION_STEPS {
        let after = format!(
            r#"{{"model":"m","input":[{INTERACTIONS_HISTORY},{CLAUDE_EMPTIED_STEP},{note}]}}"#
        );
        let (body, err) = envelope(&after);
        assert!(err.is_some(), "after {name}: {body}");
        require_media_refusal((body, err), "video");
        let before = format!(
            r#"{{"model":"m","input":[{INTERACTIONS_HISTORY},{note},{CLAUDE_EMPTIED_STEP}]}}"#
        );
        let (body, err) = envelope(&before);
        assert!(err.is_some(), "before {name}: {body}");
        require_media_refusal((body, err), "video");
    }
}

// TestInteractionsToClaudeDeveloperOrSystemStepStillSendsItsText
#[test]
fn developer_or_system_step_still_sends_its_text() {
    for (name, note) in CLAUDE_INSTRUCTION_STEPS {
        let (body, err) = envelope(&format!(
            r#"{{"model":"m","input":[{INTERACTIONS_HISTORY},{{"type":"user_input","content":[{{"type":"text","text":"keep me"}}]}},{note}]}}"#
        ));
        assert_eq!(err, None, "{name}: {body}");
        let text = body.to_string();
        assert!(
            text.contains("note") && text.contains("keep me"),
            "{name}: text was lost: {body}"
        );
    }
}

// TestInteractionsToClaudeRealUserTextBesideAnUnrepresentableAttachmentSurvivesAnInstructionStep
#[test]
fn real_user_text_beside_an_unrepresentable_attachment_survives_an_instruction_step() {
    for (name, note) in CLAUDE_INSTRUCTION_STEPS {
        let (body, err) = envelope(&format!(
            r#"{{"model":"m","input":[{INTERACTIONS_HISTORY},{note},{{"type":"user_input","content":[{{"type":"text","text":"keep me"}},{VIDEO_BY_URI}]}}]}}"#
        ));
        assert_eq!(err, None, "{name}: {body}");
        assert!(
            body.to_string().contains("keep me"),
            "{name}: text was lost: {body}"
        );
    }
}

// Not upstream's: a URL is trimmed and must be http(s) with a host, the
// first good one of `uri`, `file_uri`, `fileUri` and `url` is used, bytes
// win over a URL, and a tool result's media keeps its note or URL.
#[test]
fn media_urls_in_detail() {
    let body = require_sent(final_turn(
        r#"{"type":"image","mime_type":"image/png","uri":"files/x","url":" https://example.test/b.png "}"#,
    ));
    assert_eq!(
        body["messages"][2]["content"][0]["source"],
        json!({"type": "url", "url": "https://example.test/b.png"}),
        "{body}"
    );
    let body = require_sent(final_turn(
        r#"{"type":"image","mime_type":"image/png","data":"QQ==","uri":"https://example.test/a.png"}"#,
    ));
    assert_eq!(
        body["messages"][2]["content"][0]["source"]["type"], "base64",
        "{body}"
    );
    require_media_refusal(
        final_turn(r#"{"type":"image","mime_type":"image/png","uri":"https://"}"#),
        "image",
    );

    let body = require_sent(envelope(&format!(
        r#"{{"input":[{{"type":"function_call","name":"f","call_id":"t","arguments":{{}}}},{{"type":"function_result","call_id":"t","result":[{INLINE_AUDIO},{PDF_BY_URI}]}}]}}"#
    )));
    assert_eq!(
        body["messages"][1]["content"][0]["content"],
        json!([
            {"type": "text", "text": "[audio content omitted]"},
            {"type": "document", "source": {"type": "url", "url": "https://example.test/a.pdf"}},
        ]),
        "{body}"
    );
}

// Not upstream's: a dropped part with bytes but no media type is reported by
// its own type, thinking in a user turn is dropped without a refusal, and
// an assistant's unsendable media and a turn of only blank text aren't
// refused.
#[test]
fn dropped_parts_in_detail() {
    require_media_refusal(final_turn(r#"{"type":"blob","data":"QQ=="}"#), "blob");
    require_media_refusal(
        final_turn(r#"{"type":"input_audio","file_data":"QQ=="}"#),
        "input_audio",
    );
    require_sent(final_turn(r#"{"type":"thinking","thinking":"hm"}"#));
    require_sent(final_turn(r#"{"type":"text","text":"  "}"#));
    require_sent(envelope(&format!(
        r#"{{"input":[{{"type":"user_input","content":"q"}},{{"type":"model_output","content":[{VIDEO_BY_URI}]}}]}}"#
    )));
}
