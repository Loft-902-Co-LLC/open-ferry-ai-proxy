// Ported from CLIProxyAPI internal/translator/codex/interactions/interactions_codex_test.go
// and noop_optimization_test.go (v8.0.20, MIT), and interactions_codex_uri_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// All ten tests of interactions_codex_test.go are ported whole, and all of
// interactions_codex_uri_test.go. Upstream's URI tests go through the
// registry; ours call the translator and check that the registry gives the
// same refusal. Changed:
// TestSetInteractionsCodexRawIfDifferentReusesMatchingValue checks that the
// payload isn't copied when a pass-through field already holds the same
// value; here it checks that such a field passes through unchanged, in its
// place, since a `Map` is edited in place anyway. The tests marked "Not
// upstream's" are ours.

use serde_json::{Value, json};

use super::response::{
    CodexToInteractionsStream, convert_codex_response_to_interactions_non_stream,
};
use crate::registry::UnsupportedPartError;

/// [`super::request::convert_interactions_request_to_codex`], for requests
/// it doesn't refuse.
fn convert_interactions_request_to_codex(model: &str, request: &Value, stream: bool) -> Value {
    let (body, err) = super::request::convert_interactions_request_to_codex(model, request, stream);
    assert_eq!(err, None, "{body}");
    body
}

fn convert(request: &str, stream: bool) -> Value {
    let request: Value = serde_json::from_str(request).expect("test request is valid JSON");
    convert_interactions_request_to_codex("codex-test", &request, stream)
}

/// Looks up a dotted path such as `input.0.type`, like a plain gjson path.
fn at<'v>(value: &'v Value, path: &str) -> &'v Value {
    path.split('.')
        .fold(value, |value, key| match key.parse::<usize>() {
            Ok(index) if value.is_array() => &value[index],
            _ => &value[key],
        })
}

/// The frames of an SSE stream: each event's name and its data, parsed if
/// it is JSON.
fn frames(stream: &str) -> Vec<(String, Value)> {
    stream
        .split_terminator("\n\n")
        .map(|frame| {
            let (event, data) = frame
                .strip_prefix("event: ")
                .and_then(|frame| frame.split_once("\ndata: "))
                .expect("a frame has an event and its data");
            let data = serde_json::from_str(data).unwrap_or_else(|_| Value::from(data));
            (event.to_owned(), data)
        })
        .collect()
}

/// As upstream's helpers name an event: its data's `event_type`, else its
/// `type`, else the frame's event name.
fn event_name((event, data): &(String, Value)) -> String {
    ["event_type", "type"]
        .into_iter()
        .find_map(|key| {
            data.get(key)
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
        })
        .unwrap_or(event)
        .to_owned()
}

/// The data of the first event named `name`.
fn find_event<'f>(frames: &'f [(String, Value)], name: &str) -> &'f Value {
    &frames
        .iter()
        .find(|frame| event_name(frame) == name)
        .unwrap_or_else(|| panic!("no {name} event in {frames:?}"))
        .1
}

// TestConvertInteractionsRequestToCodexWithToolMessagesDirect
#[test]
fn request_with_tool_messages_direct() {
    let out = convert(
        r#"{"model":"codex-test","system_instruction":"be brief","input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]},{"type":"thought","content":[{"type":"text","text":"thinking"}]},{"type":"function_call","name":"lookup","call_id":"call_1","arguments":{"q":"x"}},{"type":"function_result","name":"lookup","call_id":"call_1","result":{"ok":true}}],"tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}]}"#,
        false,
    );
    assert_eq!(at(&out, "instructions"), "be brief", "{out}");
    assert_eq!(at(&out, "input.0.content.0.text"), "hi", "{out}");
    assert_eq!(at(&out, "input.1.type"), "reasoning", "{out}");
    assert_eq!(at(&out, "input.2.type"), "function_call", "{out}");
    assert_eq!(at(&out, "input.2.call_id"), "call_1", "{out}");
    assert_eq!(at(&out, "input.3.type"), "function_call_output", "{out}");
    assert_eq!(at(&out, "tools.0.name"), "lookup", "{out}");
    assert!(
        out.get("contents").is_none() && out.get("systemInstruction").is_none(),
        "{out}"
    );
}

// TestConvertInteractionsRequestToCodexPreservesNonImageMediaContent
#[test]
fn request_preserves_non_image_media_content() {
    let out = convert(
        r#"{"model":"codex-test","input":[{"type":"model_output","content":[{"type":"audio","mime_type":"audio/wav","data":"UklGRg=="},{"type":"video","mime_type":"video/mp4","data":"AAAAIGZ0eXA="},{"type":"document","mime_type":"application/pdf","data":"JVBERi0="}]}]}"#,
        false,
    );
    assert_eq!(at(&out, "input.0.role"), "assistant", "{out}");
    assert_eq!(at(&out, "input.0.content.0.type"), "input_audio", "{out}");
    assert_eq!(at(&out, "input.1.content.0.type"), "input_file", "{out}");
    assert_eq!(at(&out, "input.2.content.0.type"), "input_file", "{out}");
}

// TestConvertInteractionsRequestToCodexPreservesTopLevelThinkingLevel
#[test]
fn request_preserves_top_level_thinking_level() {
    let out = convert(
        r#"{"model":"codex-test","generation_config":{"thinking_level":"high"},"input":"hi"}"#,
        true,
    );
    assert_eq!(at(&out, "reasoning.effort"), "high", "{out}");
    assert_eq!(at(&out, "stream"), true, "{out}");
}

// TestConvertInteractionsRequestToCodexUsesBodyStream
#[test]
fn request_uses_body_stream() {
    let out = convert(
        r#"{"model":"codex-test","stream":true,"input":"hi"}"#,
        false,
    );
    assert_eq!(at(&out, "stream"), true, "{out}");
}

// TestConvertInteractionsRequestToCodexFunctionDeclarations
#[test]
fn request_function_declarations() {
    let out = convert(
        r#"{"model":"codex-test","input":"hi","tools":[{"function_declarations":[{"name":"lookup","description":"Lookup data","parameters":{"type":"object","$schema":"http://json-schema.org/draft-07/schema#","properties":{"q":{"type":"string"}}}}]}]}"#,
        false,
    );
    assert_eq!(at(&out, "tools.0.type"), "function", "{out}");
    assert_eq!(at(&out, "tools.0.name"), "lookup", "{out}");
    assert!(
        at(&out, "tools.0.parameters").get("$schema").is_none(),
        "{out}"
    );
}

// TestConvertCodexResponseToInteractionsIncompleteTerminal
#[test]
fn response_incomplete_terminal() {
    let raw = r#"{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#;
    let event: Value = serde_json::from_str(raw).expect("valid JSON");
    let out = convert_codex_response_to_interactions_non_stream("codex-test", &event);
    assert_eq!(out["status"], "incomplete", "{out}");

    let mut stream = CodexToInteractionsStream::new("codex-test");
    let frames = frames(&stream.translate_line(format!("data: {raw}").as_bytes()));
    let completed = find_event(&frames, "interaction.completed");
    assert_eq!(
        at(completed, "interaction.status"),
        "incomplete",
        "{completed}"
    );
}

// TestConvertCodexResponseToInteractionsNonStream
#[test]
fn response_non_stream() {
    let event = json!({"type":"response.completed","response":{"id":"resp_1","created_at":1_700_000_000,"usage":{"input_tokens":3,"output_tokens":2},"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]},{"type":"reasoning","content":"thinking"},{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"x\"}"}]}});
    let out = convert_codex_response_to_interactions_non_stream("codex-test", &event);
    assert_eq!(at(&out, "steps.0.content.0.text"), "ok", "{out}");
    assert_eq!(at(&out, "steps.1.type"), "thought", "{out}");
    assert_eq!(at(&out, "steps.2.type"), "function_call", "{out}");
    assert_eq!(at(&out, "usage.total_tokens"), 5, "{out}");
}

// TestConvertCodexResponseToInteractionsStream
#[test]
fn response_stream() {
    let mut stream = CodexToInteractionsStream::new("codex-test");
    let frames = frames(
        &stream.translate_line(br#"data: {"type":"response.output_text.delta","delta":"ok"}"#),
    );
    let delta = find_event(&frames, "step.delta");
    assert_eq!(at(delta, "delta.text"), "ok", "{delta}");
}

// TestConvertCodexResponseToInteractionsStreamFunctionCallStartHasCallID
#[test]
fn response_stream_function_call_start_has_call_id() {
    let mut stream = CodexToInteractionsStream::new("codex-test");
    let frames = frames(&stream.translate_line(
        br#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"x\"}"}}"#,
    ));
    let start = find_event(&frames, "step.start");
    assert_eq!(at(start, "step.call_id"), "call_1", "{start}");
}

// TestConvertCodexResponseToInteractionsStreamCompletesAfterSteps
#[test]
fn response_stream_completes_after_steps() {
    let mut stream = CodexToInteractionsStream::new("codex-test");
    let mut out = String::new();
    for chunk in [
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"codex-test"}}"#,
        r#"data: {"type":"response.output_text.delta","delta":"我将调用工具。"}"#,
        r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"weather\"}"},"output_index":1}"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_1","output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#,
    ] {
        out.push_str(&stream.translate_line(chunk.as_bytes()));
    }
    let frames = frames(&out);
    let names: Vec<String> = frames.iter().map(event_name).collect();
    assert_eq!(
        names.join(","),
        "interaction.created,interaction.status_update,step.start,step.delta,step.stop,step.start,step.delta,step.stop,interaction.completed,done"
    );
    let completed = find_event(&frames, "interaction.completed");
    assert_eq!(
        at(completed, "interaction.usage.total_tokens"),
        3,
        "{completed}"
    );
}

// TestCleanedCodexToolParametersPreservesCanonicalSchema, through the
// request translator.
#[test]
fn canonical_tool_parameters_are_unchanged() {
    let schema = r#"{"type":"object","properties":{"value":{"type":"string"}},"additionalProperties":false}"#;
    let out = convert(
        &format!(r#"{{"tools":[{{"name":"lookup","parameters":{schema}}}]}}"#),
        false,
    );
    assert_eq!(at(&out, "tools.0.parameters").to_string(), schema);
}

// TestSetInteractionsCodexRawIfDifferentReusesMatchingValue, changed (see
// the top of the file): a pass-through field that already holds the same
// value keeps it, in its place.
#[test]
fn unchanged_input_is_passed_through() {
    let out = convert(
        r#"{"generation_config":{"tool_choice":"auto"},"tool_choice":"auto","input":[]}"#,
        false,
    );
    assert_eq!(
        out.to_string(),
        r#"{"model":"codex-test","instructions":"","input":[],"tool_choice":"auto"}"#
    );
}

// Not upstream's: the whole request, for the field order and the forms of
// the converted parts.
#[test]
fn request_converts_every_part_kind() {
    let out = convert(
        r#"{
            "systemInstruction": {"parts": [{"text": "a"}, {"text": ""}, {"text": "b"}]},
            "generationConfig": {
                "thinkingConfig": {"thinkingBudget": 1024, "includeThoughts": true},
                "maxOutputTokens": 64, "verbosity": "low", "serviceTier": "flex"
            },
            "input": {"role": "model", "steps": [
                "said",
                {"type": "user_input", "role": "user", "content": [
                    {"type": "image", "url": "https://x/i.png"},
                    {"type": "image", "mime_type": "image/png", "data": "QUJD"},
                    {"type": "image_url", "image_url": {"url": "https://x/j.png"}},
                    {"type": "input_audio", "input_audio": {"data": "QQ==", "format": "wav"}},
                    {"type": "file", "file": {"file_data": "Rg==", "filename": "f.txt"}},
                    {"type": "document", "file_uri": "gs://b/d", "mime_type": "text/csv"},
                    {"type": "inline_data", "inline_data": {"mime_type": "audio/ogg", "data": "T2dn"}},
                    {"type": "blob", "fileData": {"mimeType": "image/jpeg", "fileUri": "gs://b/i"}},
                    {"inline_data": {"mime_type": "audio/ogg", "data": "T2dn"}},
                    {"type": "text"},
                    {"type": "unknown"}
                ]},
                {"type": "function_call_output", "id": " call_2 ", "output": [1, 2]},
                {"type": "reasoning", "text": "hmm", "id": 7}
            ]},
            "service_tier": " Fast ",
            "store": false
        }"#,
        false,
    );
    let want = json!({
        "model": "codex-test",
        "instructions": "a\nb",
        "input": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "said"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_image", "image_url": "https://x/i.png"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_image", "image_url": "data:image/png;base64,QUJD"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_image", "image_url": "https://x/j.png"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_audio", "input_audio": {"data": "QQ==", "format": "wav"}}]},
            {"type": "message", "role": "user", "content": [{"type": "input_file", "file_data": "Rg==", "filename": "f.txt"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_file", "file_url": "gs://b/d", "filename": "document.csv"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_audio", "input_audio": {"data": "T2dn", "format": "opus"}}]},
            {"type": "message", "role": "user", "content": [{"type": "input_image", "image_url": "gs://b/i"}]},
            {"type": "function_call_output", "call_id": "call_2", "output": "[1,2]"},
            {"type": "reasoning", "content": "hmm", "id": "7"}
        ],
        "reasoning": {"effort": "low", "summary": "auto"},
        "max_output_tokens": 64,
        "text": {"verbosity": "low"},
        "service_tier": "priority",
        "store": false
    });
    assert_eq!(out.to_string(), want.to_string());
}

// Not upstream's: tools, their names cut, and the forms of their parameters.
#[test]
fn request_converts_tools() {
    let long = "a".repeat(70);
    let mcp = format!("mcp__server__{}", "b".repeat(60));
    let out = convert(
        &format!(
            r#"{{"tool_choice":"none","tools":[
                {{"functionDeclarations":[
                    {{"name":"{long}","parametersJsonSchema":{{"type":"object","additionalProperties":true,"$schema":"s"}}}},
                    {{"description":"no name"}}
                ]}},
                {{"name":"{mcp}","description":5,"parameters":"text"}},
                {{"name":"listed","parameters":[1]}},
                {{"function_declarations":null,"name":"skipped"}},
                {{"type":"google_search"}}
            ]}}"#
        ),
        false,
    );
    let want = json!([
        {"name": "a".repeat(64), "parameters": {"type": "object", "additionalProperties": false}, "strict": false, "type": "function"},
        {"description": "5", "name": format!("mcp__{}", "b".repeat(59)), "parameters": {"additionalProperties": false}, "strict": false, "type": "function"},
        {"name": "listed", "parameters": [1], "strict": false, "type": "function"}
    ]);
    assert_eq!(out["tools"].to_string(), want.to_string());
    assert_eq!(out["tool_choice"], "none");

    // Not a list, or with nothing to convert: passed through, without a
    // tool choice.
    for tools in [r#"{"a":1}"#, r#"[{"type":"google_search"}]"#] {
        let out = convert(&format!(r#"{{"tools":{tools}}}"#), false);
        assert_eq!(out["tools"].to_string(), tools);
        assert!(out.get("tool_choice").is_none(), "{out}");
    }
}

// Not upstream's: a tool name cut in a character is cut before it.
#[test]
fn request_cuts_tool_names_at_a_character_boundary() {
    let name = format!("{}名前", "a".repeat(62));
    let out = convert(&format!(r#"{{"tools":[{{"name":"{name}"}}]}}"#), false);
    assert_eq!(out["tools"][0]["name"], "a".repeat(62));
}

// Not upstream's: the reasoning settings, and the fall-backs among them.
#[test]
fn request_reasoning_settings() {
    for (config, want) in [
        (
            r#"{"thinking_level":" ","thinkingLevel":"MEDIUM"}"#,
            json!({"effort": "medium"}),
        ),
        (
            r#"{"thinking_budget":-5,"thinkingConfig":{"thinking_budget":0}}"#,
            json!({"effort": "none"}),
        ),
        (
            r#"{"thinking_summaries":"detailed","reasoning":{"summary":" NONE "}}"#,
            json!({"summary": "none"}),
        ),
        (
            r#"{"reasoning":"x","thinking_level":"high","include_thoughts":"yes","thinking_config":{"include_thoughts":false}}"#,
            json!({"effort": "high", "summary": "none"}),
        ),
        (r#"{"reasoning":[1],"thinking_level":"high"}"#, json!([1])),
    ] {
        let out = convert(&format!(r#"{{"generation_config":{config}}}"#), false);
        assert_eq!(out["reasoning"], want, "{config}: {out}");
    }

    // Without a generation config, only a top-level reasoning is copied.
    let out = convert(
        r#"{"reasoning":{"effort":"low"},"thinking_level":"high"}"#,
        false,
    );
    assert_eq!(out["reasoning"], json!({"effort": "low"}));
}

// Not upstream's: inline data is quoted with Go's `%q` and read back with
// gjson, which stops at an escape it doesn't know.
#[test]
fn request_inline_data_stops_at_unknown_escapes() {
    let out = convert(
        r#"{"input":[{"content":[{"type":"inline","inline_data":{"mime_type":"image/png","data":"QU\u0007JD"}},{"type":"inline","inline_data":{"mime_type":"\u000bimage/png","data":"QQ=="}}]}]}"#,
        false,
    );
    assert_eq!(
        out["input"][0]["content"][0]["image_url"],
        "data:image/png;base64,QU"
    );
    // The second's MIME type reads as nothing.
    assert_eq!(out["input"].as_array().map(Vec::len), Some(1), "{out}");
}

// Not upstream's: a stream with every kind of step, closed by `[DONE]`.
#[test]
fn response_stream_steps() {
    let mut stream = CodexToInteractionsStream::new("codex-test");
    let mut out = String::new();
    for chunk in [
        r#"{"type":"response.created","response":{"id":"resp_9","model":"gpt-x","created_at":1700000000}}"#,
        r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","delta":"think"}"#,
        r#"{"type":"response.output_item.added","item":{"type":"function_call","name":"f","call_id":"c1"}}"#,
        r#"{"type":"response.function_call_arguments.delta","delta":"{\"a\""}"#,
        r#"{"type":"response.output_item.done","item":{"type":"image_generation_call","result":"QUJD","output_format":"JPG"}}"#,
        r#"{"type":"response.unknown"}"#,
        "",
        "data: [DONE]",
        "data: [DONE]",
    ] {
        out.push_str(&stream.translate_line(chunk.as_bytes()));
    }
    let frames = frames(&out);
    let data: Vec<&Value> = frames.iter().map(|(_, data)| data).collect();
    let completed = find_event(&frames, "interaction.completed");
    assert_eq!(completed["interaction"]["created"], "2023-11-14T22:13:20Z");
    assert_eq!(completed["interaction"]["model"], "gpt-x");
    assert_eq!(completed["interaction"]["usage"], json!({}));
    let want = [
        json!({"interaction":{"id":"resp_9","status":"in_progress","object":"interaction","model":"gpt-x"},"event_type":"interaction.created"}),
        json!({"interaction_id":"resp_9","status":"in_progress","event_type":"interaction.status_update"}),
        json!({"index":0,"step":{"type":"thought"},"event_type":"step.start"}),
        json!({"index":0,"delta":{"content":{"text":"think","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}),
        json!({"index":0,"event_type":"step.stop"}),
        json!({"index":1,"step":{"type":"function_call","id":"c1","call_id":"c1","name":"f","arguments":{}},"event_type":"step.start"}),
        json!({"index":1,"delta":{"arguments":"{\"a\"","type":"arguments_delta"},"event_type":"step.delta"}),
        json!({"index":1,"event_type":"step.stop"}),
        json!({"index":2,"step":{"type":"model_output"},"event_type":"step.start"}),
        json!({"index":2,"delta":{"content":{"type":"image","mime_type":"image/jpeg","data":"QUJD"},"type":"content"},"event_type":"step.delta"}),
        json!({"index":2,"event_type":"step.stop"}),
    ];
    for (index, want) in want.iter().enumerate() {
        assert_eq!(data[index].to_string(), want.to_string(), "frame {index}");
    }
    assert_eq!(data.len(), want.len() + 2, "{out}");
    assert_eq!(frames.last().map(|(event, _)| event.as_str()), Some("done"));
}

// Not upstream's: the non-streaming response's steps and usage.
#[test]
fn response_non_stream_steps_and_usage() {
    let event = json!({
        "id": "resp_2", "status": "", "model": "",
        "output": [
            {"type": "message", "content": [{"text": 5}, {"content": "a"}, {"text": ""}]},
            {"type": "message", "content": []},
            {"type": "reasoning", "content": [], "summary": "ignored"},
            {"type": "reasoning", "summary": [{"text": "s1"}, {"text": "s2"}]},
            {"type": "tool_call", "id": " t1 ", "name": "f", "arguments": "[1]"},
            {"type": "function_call", "arguments": {"b": 2}},
            {"type": "function_call", "arguments": 7},
            {"type": "image_generation_call", "result": "QQ==", "output_format": "image/webp"}
        ],
        "usage": {"prompt_tokens": 4, "completion_tokens": 6, "output_tokens_details": {"reasoning_tokens": 2}, "cached_tokens": 1}
    });
    let out = convert_codex_response_to_interactions_non_stream("codex-test", &event);
    let want = json!({
        "id": "resp_2", "object": "interaction", "status": "completed", "model": "codex-test",
        "steps": [
            {"type": "model_output", "content": [{"type": "text", "text": "a"}]},
            {"type": "thought", "content": [{"type": "text", "text": "s1\ns2"}]},
            {"type": "function_call", "name": "f", "arguments": {}, "call_id": "t1"},
            {"type": "function_call", "name": "", "arguments": {"b": 2}},
            {"type": "function_call", "name": "", "arguments": {}},
            {"type": "model_output", "content": [{"type": "image", "mime_type": "image/webp", "data": "QQ=="}]}
        ],
        "usage": {"input_tokens": 4, "output_tokens": 6, "total_tokens": 10, "reasoning_tokens": 2, "cached_tokens": 1}
    });
    assert_eq!(out.to_string(), want.to_string());
}

// Not upstream's: a function call's arguments sent as text keep each number
// as written, as upstream copies them, and a response id sent as the number
// -0 stays "-0", as gjson's String() gives it (checked with Go).
#[test]
fn numbers_keep_their_text() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let event = json!({"type":"response.completed","response":{"id":"r1","output":[{"type":"function_call","call_id":"c1","name":"f","arguments":spelled}]}});
    let out = convert_codex_response_to_interactions_non_stream("m", &event);
    assert_eq!(at(&out, "steps.0.arguments").to_string(), spelled, "{out}");

    let mut stream = CodexToInteractionsStream::new("m");
    let out = stream
        .translate_line(br#"data: {"type":"response.created","response":{"id":-0,"model":"m"}}"#);
    assert!(out.contains(r#"{"interaction":{"id":"-0","#), "{out}");
}

// Not upstream's: review 11a's request. A call's arguments object, read with
// each number as written, goes on as that text, as upstream copies it
// (checked with Go).
#[test]
fn request_arguments_keep_their_numbers() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let request = format!(
        r#"{{"input":[{{"type":"function_call","id":"a","name":"f","arguments":{{"x":-0}}}},{{"type":"function_call","id":"b","name":"f","arguments":{spelled}}}]}}"#
    );
    let request = crate::json::exact::from_str(&request).unwrap();
    let out = convert_interactions_request_to_codex("m", &request, false);
    assert_eq!(at(&out, "input.0.arguments"), r#"{"x":-0}"#);
    assert_eq!(at(&out, "input.1.arguments"), spelled);
}

const INTERACTIONS_CODEX_HISTORY: &str = r#"{"type":"user_input","content":[{"type":"text","text":"a"}]},{"type":"model_output","content":[{"type":"text","text":"b"}]}"#;

/// `interactionsToCodexEnvelope`: the translator's body and refusal, after
/// checking that the registration gives the same refusal.
fn envelope(input: &str) -> (Value, Option<UnsupportedPartError>) {
    let request: Value = serde_json::from_str(input).expect("test request is valid JSON");
    let (body, err) = super::request::convert_interactions_request_to_codex("m", &request, false);
    let registered = crate::registry::Registry::global().translate_request_checked(
        &"interactions".into(),
        &"codex".into(),
        "m",
        request,
        false,
    );
    assert_eq!(registered.err(), err, "the registration's refusal");
    (body, err)
}

/// `interactionsFinalTurnToCodex`: two history turns followed by a final
/// user step holding `final_content`.
fn final_turn(final_content: &str) -> (Value, Option<UnsupportedPartError>) {
    envelope(&format!(
        r#"{{"model":"m","input":[{INTERACTIONS_CODEX_HISTORY},{{"type":"user_input","content":[{final_content}]}}]}}"#
    ))
}

/// `requireInteractionsToCodexRefusal`.
#[track_caller]
fn require_refusal((body, err): (Value, Option<UnsupportedPartError>), want: &str) {
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

// TestInteractionsToCodexKeepsTheMediaGeminiToInteractionsWrites
#[test]
fn keeps_the_media_gemini_to_interactions_writes() {
    let body = require_sent(final_turn(
        r#"{"type":"image","uri":"gs://b/a.png","mime_type":"image/png"}"#,
    ));
    assert_eq!(body["input"].as_array().map(Vec::len), Some(3), "{body}");
    let part = at(&body, "input.2.content.0");
    assert_eq!(part["type"], "input_image", "{body}");
    assert_eq!(part["image_url"], "gs://b/a.png", "{body}");
}

// TestInteractionsToCodexReadsURIAsAFileReference
#[test]
fn reads_uri_as_a_file_reference() {
    for field in ["uri", "file_uri", "fileUri", "url"] {
        let body = require_sent(final_turn(&format!(
            r#"{{"type":"image","mime_type":"image/png","{field}":"https://example.test/a.png"}}"#
        )));
        let part = at(&body, "input.2.content.0");
        assert_eq!(part["type"], "input_image", "image {field}: {body}");
        assert_eq!(
            part["image_url"], "https://example.test/a.png",
            "image {field}: {body}"
        );

        let body = require_sent(final_turn(&format!(
            r#"{{"type":"document","mime_type":"application/pdf","{field}":"https://example.test/a.pdf"}}"#
        )));
        let part = at(&body, "input.2.content.0");
        assert_eq!(part["type"], "input_file", "document {field}: {body}");
        assert_eq!(
            part["file_url"], "https://example.test/a.pdf",
            "document {field}: {body}"
        );
    }
}

// TestInteractionsToCodexInlineDataStillConverts
#[test]
fn inline_data_still_converts() {
    let image = require_sent(final_turn(
        r#"{"type":"image","mime_type":"image/png","data":"aGVsbG8="}"#,
    ));
    assert_eq!(
        at(&image, "input.2.content.0.image_url"),
        "data:image/png;base64,aGVsbG8=",
        "{image}"
    );
    let audio = require_sent(final_turn(
        r#"{"type":"audio","mime_type":"audio/wav","data":"UklGRg=="}"#,
    ));
    assert_eq!(
        at(&audio, "input.2.content.0.input_audio.data"),
        "UklGRg==",
        "{audio}"
    );
}

// TestInteractionsToCodexUnrepresentableAttachmentOnlyTurnIsRefused
#[test]
fn unrepresentable_attachment_only_turn_is_refused() {
    let cases = [
        (
            "audio by uri",
            r#"{"type":"audio","mime_type":"audio/wav","uri":"gs://b/a.wav"}"#,
            "audio",
        ),
        ("image without anything", r#"{"type":"image"}"#, "image"),
        (
            "document without anything",
            r#"{"type":"document","mime_type":"application/pdf"}"#,
            "document",
        ),
        (
            "empty text does not hide it",
            r#"{"type":"text","text":""},{"type":"audio","mime_type":"audio/wav"}"#,
            "audio",
        ),
        (
            "whitespace text does not hide it",
            r#"{"type":"text","text":" \n"},{"type":"audio","mime_type":"audio/wav"}"#,
            "audio",
        ),
    ];
    for (name, content, want) in cases {
        let (body, err) = final_turn(content);
        assert!(err.is_some(), "{name}: no refusal; body = {body}");
        require_refusal((body, err), want);
    }
}

// TestInteractionsToCodexRefusesAnEmptiedTurnBeforeALaterTextTurn
#[test]
fn refuses_an_emptied_turn_before_a_later_text_turn() {
    require_refusal(
        envelope(
            r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]},{"type":"model_output","content":[{"type":"text","text":"ok"}]},{"type":"user_input","content":[{"type":"text","text":"next"}]}]}"#,
        ),
        "audio",
    );
}

// TestInteractionsToCodexInstructionStepDoesNotHideAnEmptiedTurn
#[test]
fn instruction_step_does_not_hide_an_emptied_turn() {
    const EMPTIED: &str = r#"{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav","uri":"gs://b/a.wav"}]}"#;
    let cases = [
        (
            "role developer",
            r#"{"type":"user_input","role":"developer","content":[{"type":"text","text":"note"}]}"#,
        ),
        (
            "role system",
            r#"{"type":"user_input","role":"system","content":[{"type":"text","text":"note"}]}"#,
        ),
        (
            "type developer",
            r#"{"type":"developer","content":[{"type":"text","text":"note"}]}"#,
        ),
        (
            "type system",
            r#"{"type":"system","content":[{"type":"text","text":"note"}]}"#,
        ),
        (
            "type system wrapper",
            r#"{"type":"system","steps":[{"type":"user_input","content":[{"type":"text","text":"note"}]}]}"#,
        ),
    ];
    for (name, instruction) in cases {
        let (body, err) = envelope(&format!(
            r#"{{"model":"m","input":[{INTERACTIONS_CODEX_HISTORY},{EMPTIED},{instruction}]}}"#
        ));
        assert!(err.is_some(), "{name}: no refusal; body = {body}");
        require_refusal((body, err), "audio");
    }
}

// TestInteractionsToCodexTypeOnlyInstructionStepIsSentAsDeveloper
#[test]
fn type_only_instruction_step_is_sent_as_developer() {
    for step_type in ["system", "developer"] {
        let body = require_sent(envelope(&format!(
            r#"{{"model":"m","input":[{INTERACTIONS_CODEX_HISTORY},{{"type":"{step_type}","content":[{{"type":"text","text":"note"}}]}}]}}"#
        )));
        let last = at(&body, "input.2");
        assert_eq!(last["role"], "developer", "{step_type}: {body}");
        assert_eq!(at(last, "content.0.text"), "note", "{step_type}: {body}");
    }
}

// TestInteractionsToCodexUnrepresentableAttachmentBesideTextStillSucceeds
#[test]
fn unrepresentable_attachment_beside_text_still_succeeds() {
    let body = require_sent(final_turn(
        r#"{"type":"text","text":"keep me"},{"type":"audio","mime_type":"audio/wav"}"#,
    ));
    assert_eq!(at(&body, "input.2.content.0.text"), "keep me", "{body}");
    assert_eq!(body["input"].as_array().map(Vec::len), Some(3), "{body}");
}

// TestInteractionsToCodexNeighbouringStepKeepsTheTurnAlive
#[test]
fn neighbouring_step_keeps_the_turn_alive() {
    let cases = [
        (
            "text step",
            r#"{"type":"user_input","content":[{"type":"text","text":"keep me"}]},{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]}"#,
        ),
        (
            "tool result",
            r#"{"type":"function_call","name":"f","call_id":"c1","arguments":{}},{"type":"function_result","name":"f","call_id":"c1","result":{"ok":true}},{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]}"#,
        ),
        (
            "string step",
            r#""keep me",{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]}"#,
        ),
        (
            "turn of steps",
            r#"{"role":"user","steps":[{"type":"user_input","content":[{"type":"text","text":"keep me"}]},{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]}]}"#,
        ),
    ];
    for (name, steps) in cases {
        let (body, err) = envelope(&format!(
            r#"{{"model":"m","input":[{INTERACTIONS_CODEX_HISTORY},{steps}]}}"#
        ));
        assert_eq!(err, None, "{name}: {body}");
    }
}

// TestInteractionsToCodexAssistantMediaIsNotAUserTurn
#[test]
fn assistant_media_is_not_a_user_turn() {
    require_sent(envelope(
        r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"text","text":"a"}]},{"type":"model_output","content":[{"type":"audio","mime_type":"audio/wav"}]},{"type":"user_input","content":[{"type":"text","text":"c"}]}]}"#,
    ));
}

// TestInteractionsToCodexExportedWrapperKeepsAJSONBody
#[test]
fn exported_wrapper_keeps_a_json_body() {
    let request: Value = serde_json::from_str(&format!(
        r#"{{"model":"m","input":[{INTERACTIONS_CODEX_HISTORY},{{"type":"user_input","content":[{{"type":"audio","mime_type":"audio/wav"}}]}}]}}"#
    ))
    .expect("test request is valid JSON");
    let (body, err) = super::request::convert_interactions_request_to_codex("m", &request, false);
    assert!(err.is_some(), "{body}");
    assert!(body.is_object(), "{body}");
}

// Not upstream's: a blank URL falls through to the next field, and the URL
// sent is trimmed.
#[test]
fn blank_urls_fall_through_and_urls_are_trimmed() {
    let body = require_sent(final_turn(
        r#"{"type":"image","url":" ","file_uri":" https://example.test/a.png "}"#,
    ));
    assert_eq!(
        at(&body, "input.2.content.0.image_url"),
        "https://example.test/a.png",
        "{body}"
    );
    let body = require_sent(final_turn(
        r#"{"type":"document","mime_type":"application/pdf","file_uri":"","url":" https://example.test/a.pdf"}"#,
    ));
    assert_eq!(
        at(&body, "input.2.content.0.file_url"),
        "https://example.test/a.pdf",
        "{body}"
    );
    require_refusal(final_turn(r#"{"type":"image","url":"  "}"#), "image");
}

// Not upstream's: a function call or a thought closes the emptied turn, a
// developer step with null content does too, blank string steps don't keep
// the turn alive, and a plain string input is never refused.
#[test]
fn user_turns_in_detail() {
    const EMPTIED: &str =
        r#"{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]}"#;
    for closer in [
        r#"{"type":"function_call","name":"f","call_id":"c1","arguments":{}}"#,
        r#"{"type":"thought","text":"hmm"}"#,
        r#"{"type":"developer","content":null}"#,
    ] {
        require_refusal(
            envelope(&format!(
                r#"{{"model":"m","input":[{EMPTIED},{closer},"later text"]}}"#
            )),
            "audio",
        );
    }
    require_refusal(
        envelope(&format!(r#"{{"model":"m","input":[" ",{EMPTIED}]}}"#)),
        "audio",
    );
    require_refusal(
        envelope(&format!(
            r#"{{"model":"m","input":{{"role":"user","steps":[{EMPTIED}]}}}}"#
        )),
        "audio",
    );
    require_sent(envelope(r#"{"model":"m","input":"just text"}"#));
}
