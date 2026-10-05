// Ported from CLIProxyAPI internal/translator/openai/openai/responses/shell_tool_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

// All 5 tests are ported. Subtests run in a loop. Where upstream passes no
// translated request, these pass `Null`. Tests of our own, marked as such,
// were checked against upstream with a Go probe.

use serde_json::{Value, json};

use super::super::{
    OpenAIToOpenAIResponsesStream,
    convert_openai_chat_completions_response_to_openai_responses_non_stream,
    convert_openai_responses_request_to_openai_chat_completions,
};
use super::INVALID_ACTION;
use crate::json::{exact, str_of};

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("valid JSON")
}

/// Looks up a dotted path such as `output.0.type`, like a plain gjson path.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Object(map) => map.get(key),
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        _ => None,
    })
}

/// The value at `path` as gjson's `String()` gives it.
fn text_at(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// The length of the array at `path`, as gjson's `#` gives it.
fn count_at(value: &Value, path: &str) -> usize {
    at(value, path)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

fn chat(request: &str) -> Value {
    convert_openai_responses_request_to_openai_chat_completions("test", &parse(request), false)
}

/// `shellRequest`.
const SHELL_REQUEST: &str = r#"{"tools":[{"type":"shell","environment":{"type":"local"}},{"type":"function","name":"__cpa_local_shell","parameters":{}},{"type":"custom","name":"__cpa_local_shell_1"},{"type":"namespace","name":"ns","tools":[{"type":"function","name":"lookup","parameters":{}}]}],"tool_choice":{"type":"shell"}}"#;

/// A request that declares only the local shell.
const LOCAL_SHELL: &str = r#"{"tools":[{"type":"shell","environment":{"type":"local"}}]}"#;

/// `shellUpstreamCall`.
fn upstream_call(name: &str, args: &str) -> Value {
    json!({
        "index": 0,
        "id": "call_shell",
        "type": "function",
        "function": {"name": name, "arguments": args},
    })
}

/// `shellNonStream`.
fn non_stream(request: &str, name: &str, args: &str) -> Value {
    non_stream_finishing(request, name, args, "tool_calls")
}

/// `shellNonStream`, finishing for `finish`.
fn non_stream_finishing(request: &str, name: &str, args: &str, finish: &str) -> Value {
    let upstream = json!({
        "id": "resp_test",
        "choices": [{
            "index": 0,
            "finish_reason": finish,
            "message": {"role": "assistant", "tool_calls": [upstream_call(name, args)]},
        }],
    });
    convert_openai_chat_completions_response_to_openai_responses_non_stream(
        &parse(request),
        &Value::Null,
        upstream.to_string().as_bytes(),
    )
}

/// The events a stream sends for `lines`.
fn stream_lines(request: &str, lines: &[String]) -> Vec<Value> {
    let mut translator = OpenAIToOpenAIResponsesStream::new("test", &parse(request), &Value::Null);
    lines
        .iter()
        .flat_map(|line| {
            translator
                .translate_line(line.as_bytes())
                .split('\n')
                .filter_map(|line| line.strip_prefix("data: ").map(parse))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// `shellStream`: the call's arguments in two halves, the second without the
/// call's ID or name, then a `tool_calls` finish and `[DONE]`.
fn stream(request: &str, name: &str, args: &str) -> Vec<Value> {
    let half = args.len() / 2;
    let mut lines = Vec::new();
    for (i, fragment) in [&args[..half], &args[half..]].into_iter().enumerate() {
        let mut call = upstream_call(name, fragment);
        if i == 1 {
            call.as_object_mut().expect("an object").remove("id");
            call["function"]
                .as_object_mut()
                .expect("an object")
                .remove("name");
        }
        let chunk = json!({
            "id": "resp_test",
            "choices": [{"index": 0, "delta": {"tool_calls": [call]}}],
        });
        lines.push(format!("data: {chunk}"));
    }
    lines.push(
        r#"data: {"id":"resp_test","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#
            .to_owned(),
    );
    lines.push("data: [DONE]".to_owned());
    stream_lines(request, &lines)
}

/// `TestLocalShellDeclarationSurvivesChatTranslation`.
#[test]
fn local_shell_declaration_survives_chat_translation() {
    let got = chat(
        r#"{"tools":[{"type":"shell","environment":{"type":"local"}}],"tool_choice":"auto","input":"Run echo SHELL_OK"}"#,
    );
    assert_eq!(
        count_at(&got, "tools"),
        1,
        "local shell declaration was dropped from chat request: {got}"
    );
    assert_eq!(
        text_at(
            &got,
            "tools.0.function.parameters.properties.commands.items.type"
        ),
        "string",
        "shell action must expose commands as strings: {got}"
    );
}

/// `TestLocalShellRoundTrip`.
#[test]
fn local_shell_round_trip() {
    let request = parse(SHELL_REQUEST);
    let chat_request =
        convert_openai_responses_request_to_openai_chat_completions("test", &request, false);
    let name = text_at(&chat_request, "tools.0.function.name");
    assert!(
        !name.is_empty()
            && name != "__cpa_local_shell"
            && name != "__cpa_local_shell_1"
            && count_at(&chat_request, "tools") == 4,
        "synthetic tool collided with user tools: {chat_request}"
    );
    assert_eq!(
        text_at(&chat_request, "tool_choice.function.name"),
        name,
        "shell tool_choice not mapped: {chat_request}"
    );

    let args =
        r#"{"commands":["echo SHELL_OK","pwd"],"timeout_ms":120000,"max_output_length":4096}"#;
    let non_streamed = non_stream(SHELL_REQUEST, &name, args);
    let item = at(&non_streamed, "output.0").cloned().unwrap_or_default();
    assert!(
        text_at(&item, "type") == "shell_call"
            && text_at(&item, "action.commands.0") == "echo SHELL_OK"
            && text_at(&item, "status") == "completed"
            && item.get("name").is_none()
            && item.get("arguments").is_none(),
        "invalid shell item: {non_streamed}"
    );

    let (mut done, mut completed) = (None, None);
    for event in stream(SHELL_REQUEST, &name, args) {
        match &*text_at(&event, "type") {
            "response.output_item.added" => assert_eq!(
                text_at(&event, "item.type"),
                "shell_call",
                "wrong added item: {event}"
            ),
            "response.output_item.done" => done = at(&event, "item").cloned(),
            "response.completed" => completed = at(&event, "response.output.0").cloned(),
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                panic!("shell leaked function event: {event}")
            }
            _ => {}
        }
    }
    assert!(
        done.is_some() && done.as_ref() == Some(&item) && done == completed,
        "inconsistent final items: nonstream={item} done={done:?} completed={completed:?}"
    );

    let output = r#"{"type":"shell_call_output","call_id":"call_shell","max_output_length":4096,"output":[{"stdout":"SHELL_OK\n","stderr":"warning","outcome":{"type":"exit","exit_code":0}},{"stdout":"partial","stderr":"","outcome":{"type":"timeout"}}]}"#;
    let mut replay = request.clone();
    replay["input"] = json!([
        item,
        {"type": "function_call", "call_id": "call_user", "name": "__cpa_local_shell", "arguments": "{}"},
        parse(output),
        {"type": "function_call_output", "call_id": "call_user", "output": "ok"},
    ]);
    let replayed =
        convert_openai_responses_request_to_openai_chat_completions("test", &replay, false);
    assert!(
        count_at(&replayed, "messages.0.tool_calls") == 2
            && text_at(&replayed, "messages.0.tool_calls.0.function.name") == name
            && text_at(&replayed, "messages.0.tool_calls.0.function.arguments") == args,
        "shell history not grouped/replayed: {replayed}"
    );
    let content = text_at(&replayed, "messages.1.content");
    assert!(
        text_at(&replayed, "messages.1.role") == "tool"
            && serde_json::from_str::<Value>(&content).ok() == Some(parse(output)),
        "shell output envelope lost: {replayed}"
    );

    for (tool, kind) in [
        ("__cpa_local_shell", "function_call"),
        ("__cpa_local_shell_1", "custom_tool_call"),
        ("ns__lookup", "function_call"),
    ] {
        let got = non_stream(SHELL_REQUEST, tool, r#"{"input":"hello"}"#);
        assert_eq!(
            text_at(&got, "output.0.type"),
            kind,
            "user tool misclassified: {got}"
        );
    }
    let unknown = non_stream("{}", &name, args);
    assert_eq!(
        text_at(&unknown, "output.0.type"),
        "function_call",
        "synthetic name recognized without provenance: {unknown}"
    );
}

/// `TestLocalShellInvalidActions`.
#[test]
fn local_shell_invalid_actions() {
    for args in [
        "{",
        "{}",
        r#"{"commands":[]}"#,
        r#"{"commands":[42]}"#,
        r#"{"commands":[" "]}"#,
        r#"{"command":["echo","ok"]}"#,
        r#"{"commands":["pwd"],"timeout_ms":-1}"#,
        r#"{"commands":["pwd"],"timeout_ms":1.5}"#,
        r#"{"commands":["pwd"],"env":{}}"#,
    ] {
        let got = non_stream(LOCAL_SHELL, "__cpa_local_shell", args);
        assert!(
            text_at(&got, "status") == "failed" && text_at(&got, "error.message").contains("shell"),
            "{args}: invalid action accepted: {got}"
        );
        let mut failed = false;
        for event in stream(LOCAL_SHELL, "__cpa_local_shell", args) {
            match &*text_at(&event, "type") {
                "response.failed" => failed = true,
                "response.completed" | "response.output_item.done" => {
                    panic!("{args}: invalid action completed: {event}")
                }
                _ => {}
            }
        }
        assert!(failed, "{args}: invalid action did not fail streaming");
    }
}

/// `TestLocalShellOnlyExplicitLocalEnvironment`.
#[test]
fn local_shell_only_explicit_local_environment() {
    for environment in [
        r#"{"type":"container_auto"}"#,
        r#"{"type":"container_reference","container_id":"cntr_x"}"#,
        "{}",
    ] {
        let got = chat(&format!(
            r#"{{"tools":[{{"type":"shell","environment":{environment}}}]}}"#
        ));
        assert_eq!(
            count_at(&got, "tools"),
            0,
            "hosted shell represented as local: {got}"
        );
    }
    for choice in ["auto", "required", "none"] {
        let got = chat(&format!(
            r#"{{"tools":[{{"type":"shell","environment":{{"type":"local"}}}}],"tool_choice":"{choice}"}}"#
        ));
        assert_eq!(
            text_at(&got, "tool_choice"),
            choice,
            "choice changed: {got}"
        );
    }
}

/// `TestLocalShellHistoryWithoutDeclaration`.
#[test]
fn local_shell_history_without_declaration() {
    for tools in [
        "",
        r#","tools":[]"#,
        r#","tools":[{"type":"function","name":"__cpa_local_shell_1","parameters":{}}]"#,
    ] {
        let request = format!(
            r#"{{"input":[{{"type":"shell_call","call_id":"shell_1","action":{{"commands":["pwd"]}}}},{{"type":"shell_call_output","call_id":"shell_1","output":[{{"stdout":"/workspace","stderr":"","outcome":{{"type":"exit","exit_code":0}}}}]}},{{"type":"function_call","call_id":"user_1","name":"__cpa_local_shell","arguments":"{{}}"}},{{"type":"function_call_output","call_id":"user_1","output":"ok"}}]{tools}}}"#
        );
        let got = chat(&request);
        let calls = at(&got, "messages.0.tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(
            calls.len() == 1
                && text_at(&calls[0], "id") == "shell_1"
                && text_at(&got, "messages.2.tool_calls.0.id") == "user_1",
            "shell history dropped without declaration: {got}"
        );
        let name = text_at(&calls[0], "function.name");
        assert!(
            !(name == "__cpa_local_shell"
                || name == "__cpa_local_shell_1" && !tools.is_empty() && tools != r#","tools":[]"#),
            "history name collided: {got}"
        );
        let content =
            serde_json::from_str::<Value>(&text_at(&got, "messages.1.content")).unwrap_or_default();
        assert_eq!(
            text_at(&content, "output.0.stdout"),
            "/workspace",
            "shell result lost: {got}"
        );
        assert_eq!(
            count_at(&got, "tools"),
            count_at(&parse(&request), "tools"),
            "history re-enabled a tool: {got}"
        );
    }
}

/// Not upstream's: limits that are positive whole numbers however they are
/// written, even past `f64`'s range, or `null`, are valid, and arguments with
/// space around them, and the action keeps the numbers as written; zero, negative zero, strings, a command of nothing but
/// a no-break space, an action in a list and commands that aren't a list
/// aren't, and the failure says why.
#[test]
fn shell_action_limits() {
    for args in [
        r#"{"commands":["pwd"],"timeout_ms":1e3}"#,
        r#"{"commands":["pwd"],"timeout_ms":null}"#,
        r#"{"commands":["pwd"],"max_output_length":1.0}"#,
        r#"{"commands":["pwd"],"timeout_ms":1e400}"#,
        r#" {"commands":["a"]} "#,
    ] {
        let got = non_stream(LOCAL_SHELL, "__cpa_local_shell", args);
        assert_eq!(
            text_at(&got, "output.0.type"),
            "shell_call",
            "{args}: {got}"
        );
        assert_eq!(
            at(&got, "output.0.action"),
            exact::from_str(args).ok().as_ref(),
            "{args}: {got}"
        );
    }
    for args in [
        r#"{"commands":["pwd"],"timeout_ms":0}"#,
        r#"{"commands":["pwd"],"timeout_ms":-0}"#,
        "{\"commands\":[\"\u{a0}\"]}",
        r#"{"commands":["pwd"],"timeout_ms":"5"}"#,
        r#"[{"commands":["pwd"]}]"#,
        r#"{"commands":"pwd"}"#,
    ] {
        let got = non_stream(LOCAL_SHELL, "__cpa_local_shell", args);
        assert_eq!(
            got,
            json!({
                "id": "resp_test",
                "object": "response",
                "status": "failed",
                "error": {
                    "type": "server_error",
                    "code": "invalid_tool_arguments",
                    "message": INVALID_ACTION,
                    "param": null,
                },
            }),
            "{args}"
        );
    }
}

/// Not upstream's: the action keeps the arguments' key order and numbers as
/// written, in a whole response and in a stream's done item and finished
/// response.
#[test]
fn shell_action_keeps_key_order_and_numbers() {
    let args = r#"{"max_output_length":4096.0,"commands":["a"],"timeout_ms":1E3}"#;
    let got = non_stream(LOCAL_SHELL, "__cpa_local_shell", args);
    assert_eq!(
        at(&got, "output.0.action").map(Value::to_string).as_deref(),
        Some(args)
    );

    let call = json!({
        "id": "resp_test",
        "choices": [{"index": 0, "delta": {"tool_calls": [upstream_call("__cpa_local_shell", args)]}}],
    });
    let mut translator =
        OpenAIToOpenAIResponsesStream::new("test", &parse(LOCAL_SHELL), &Value::Null);
    let output: String = [
        format!("data: {call}"),
        r#"data: {"id":"resp_test","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#
            .to_owned(),
        "data: [DONE]".to_owned(),
    ]
    .iter()
    .map(|line| translator.translate_line(line.as_bytes()))
    .collect();
    // Read as text: parsing would respell the numbers.
    let action = format!(r#""action":{args}"#);
    let types: Vec<String> = output
        .lines()
        .filter(|line| line.contains(&action))
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| text_at(&parse(data), "type"))
        .collect();
    assert_eq!(
        types,
        ["response.output_item.done", "response.completed"],
        "{output}"
    );
}

/// Not upstream's: a call finished for `length` is incomplete, in a whole
/// response and in a stream, where its item is done and the response is
/// incomplete.
#[test]
fn incomplete_shell_calls() {
    let item = json!({
        "id": "sh_call_shell",
        "type": "shell_call",
        "status": "incomplete",
        "call_id": "call_shell",
        "action": {"commands": ["a"]},
    });
    let args = r#"{"commands":["a"]}"#;
    let got = non_stream_finishing(LOCAL_SHELL, "__cpa_local_shell", args, "length");
    assert_eq!(text_at(&got, "status"), "incomplete");
    assert_eq!(at(&got, "output"), Some(&json!([item])));

    let lines = [
        r#"data: {"id":"resp_test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_shell","type":"function","function":{"name":"__cpa_local_shell","arguments":"{\"commands\":[\"a\"]}"}}]}}]}"#.to_owned(),
        r#"data: {"id":"resp_test","choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#.to_owned(),
        "data: [DONE]".to_owned(),
    ];
    let events = stream_lines(LOCAL_SHELL, &lines);
    let types: Vec<String> = events.iter().map(|event| text_at(event, "type")).collect();
    assert_eq!(
        types,
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.output_item.done",
            "response.incomplete",
        ]
    );
    assert_eq!(
        at(&events[2], "item"),
        Some(&json!({
            "id": "sh_call_shell",
            "type": "shell_call",
            "status": "in_progress",
            "call_id": "call_shell",
            "action": {"commands": []},
        }))
    );
    assert_eq!(at(&events[3], "item"), Some(&item));
    assert_eq!(at(&events[4], "response.output"), Some(&json!([item])));
}

/// Not upstream's: a stream that ends without a finish reason leaves a call
/// whose arguments aren't JSON open, without failing, and finishes one whose
/// are.
#[test]
fn shell_call_without_finish_reason() {
    let call = |args: &str| {
        let chunk = json!({
            "id": "resp_test",
            "choices": [{"index": 0, "delta": {"tool_calls": [upstream_call("__cpa_local_shell", args)]}}],
        });
        vec![format!("data: {chunk}"), "data: [DONE]".to_owned()]
    };
    let types = |lines: &[String]| -> Vec<String> {
        stream_lines(LOCAL_SHELL, lines)
            .iter()
            .map(|event| text_at(event, "type"))
            .collect()
    };
    assert_eq!(
        types(&call(r#"{"commands""#)),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added"
        ]
    );
    assert_eq!(
        types(&call(r#"{"commands":["a"]}"#)),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.output_item.done",
            "response.completed",
        ]
    );
}

/// Not upstream's: a `shell_call` in another environment is left as it is,
/// which drops it, and a local one without an action is a call with no
/// arguments.
#[test]
fn history_converts_only_local_shell_calls() {
    let got = chat(
        r#"{"input":[{"type":"shell_call","call_id":"c","environment":{"type":"container_auto"},"action":{"commands":["pwd"]}},{"type":"shell_call","call_id":"d","environment":{"type":"local"}},{"type":"shell_call_output","call_id":"d","output":[]}]}"#,
    );
    assert_eq!(
        got,
        parse(
            r#"{"model":"test","messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"","name":"__cpa_local_shell"},"id":"d","type":"function"}]},{"role":"tool","tool_call_id":"d","content":"{\"type\":\"shell_call_output\",\"call_id\":\"d\",\"output\":[]}"}],"stream":false}"#
        )
    );
}

/// Not upstream's: a shell declared in a namespace isn't the local shell, so
/// a `shell` tool choice is passed on as it is; one in `additional_tools` is,
/// and the choice forces it.
#[test]
fn where_the_local_shell_is_declared() {
    let got = chat(
        r#"{"tools":[{"type":"namespace","name":"ns","tools":[{"type":"shell","environment":{"type":"local"}}]},{"type":"function","name":"f"}],"tool_choice":{"type":"shell"}}"#,
    );
    assert_eq!(count_at(&got, "tools"), 1, "{got}");
    assert_eq!(at(&got, "tool_choice"), Some(&json!({"type": "shell"})));

    let got = chat(
        r#"{"input":[{"type":"additional_tools","tools":[{"type":"shell","environment":{"type":"local"}}]},{"type":"message","role":"user","content":"hi"}],"tool_choice":{"type":"shell","x":1}}"#,
    );
    assert_eq!(
        got.to_string(),
        r#"{"model":"test","messages":[{"role":"user","content":"hi"}],"stream":false,"tools":[{"function":{"description":"Request commands to execute in the client-provided local shell environment. Each commands entry is a complete shell command, not an argv element.","name":"__cpa_local_shell","parameters":{"additionalProperties":false,"properties":{"commands":{"items":{"type":"string"},"minItems":1,"type":"array"},"max_output_length":{"minimum":1,"type":"integer"},"timeout_ms":{"minimum":1,"type":"integer"}},"required":["commands"],"type":"object"}},"type":"function"}],"tool_choice":{"type":"function","function":{"name":"__cpa_local_shell"}}}"#
    );
}

/// Not upstream's: the shell's name avoids another tool's local name, when
/// declared and when only replayed, and a replayed one also avoids the names
/// of the calls in the input.
#[test]
fn shell_names_avoid_local_names_and_calls() {
    let got = chat(
        r#"{"tools":[{"type":"namespace","name":"ns","tools":[{"type":"function","name":"__cpa_local_shell"}]},{"type":"shell","environment":{"type":"local"}}]}"#,
    );
    let names: Vec<String> = (0..count_at(&got, "tools"))
        .map(|i| text_at(&got, &format!("tools.{i}.function.name")))
        .collect();
    assert_eq!(names, ["ns____cpa_local_shell", "__cpa_local_shell_1"]);

    let got = chat(
        r#"{"tools":[{"type":"namespace","name":"ns","tools":[{"type":"function","name":"__cpa_local_shell"}]}],"input":[{"type":"function_call","call_id":"u","name":"__cpa_local_shell_1","arguments":"{}"},{"type":"shell_call","call_id":"s","action":{"commands":["pwd"]}}]}"#,
    );
    assert_eq!(
        at(&got, "messages"),
        Some(&parse(
            r#"[{"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"__cpa_local_shell_1"},"id":"u","type":"function"},{"function":{"arguments":"{\"commands\":[\"pwd\"]}","name":"__cpa_local_shell_2"},"id":"s","type":"function"}]}]"#
        ))
    );
}
