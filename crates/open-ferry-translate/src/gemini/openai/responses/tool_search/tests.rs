// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/gemini_openai-responses_tool_search_test.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::super::test_support::{
    convert_openai_responses_request_to_gemini as convert, sse_events,
};
use super::super::{
    GeminiToOpenAIResponsesStream, convert_gemini_response_to_openai_responses_non_stream,
};
use super::*;
use crate::responses_tools::build_gemini_function_declarations;

const MODEL: &str = "gemini-3.8-flash-high";
const FIXTURE: &str = r#"{
	"model": "gemini-3.8-flash-high",
	"stream": true,
	"tools": [
		{"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}},
		{"type": "tool_search", "execution": "client", "description": "Search available deferred tools. Multi-agent tools: Spawn and manage sub-agents.", "parameters": {"type": "object", "properties": {"query": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["query"], "additionalProperties": false}},
		{"type": "function", "name": "spawn_agent", "defer_loading": true, "parameters": {"type": "object", "properties": {"prompt": {"type": "string"}}}}
	],
	"input": [
		{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Find the multi-agent tools before answering."}]},
		{"type": "tool_search_call", "id": "tsc_search_1", "execution": "client", "call_id": "search_1", "status": "completed", "arguments": {"query": "fixture tools", "limit": 1}},
		{"type": "tool_search_output", "id": "tso_search_1", "execution": "client", "call_id": "search_1", "status": "completed", "tools": [
			{"type": "namespace", "name": "fixture_tools", "description": "Minimal discovery fixture, not the actual Codex agent schema", "tools": [
				{"type": "function", "name": "ping", "description": "Return a test reply", "defer_loading": true, "parameters": {"type": "object", "properties": {}, "additionalProperties": false}}
			]}
		]},
		{"type": "function_call", "call_id": "call_ping", "name": "ping", "namespace": "fixture_tools", "arguments": "{}"},
		{"type": "function_call_output", "call_id": "call_ping", "output": "pong"}
	]
}"#;
fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}
fn response(name: &str, args: Value) -> Value {
    json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":name,"args":args}}]},"finishReason":"STOP"}],"responseId":"resp_search"})
}
fn stream(request: &Value, response: &Value) -> Vec<(String, Value)> {
    let mut st = GeminiToOpenAIResponsesStream::new(MODEL, request, &Value::Null);
    let mut out = st.translate_line(response.to_string().as_bytes());
    out.push_str(&st.translate_line(b"data: [DONE]"));
    sse_events(&out)
}
fn done_items(events: &[(String, Value)]) -> Vec<&Value> {
    events
        .iter()
        .filter(|(event, _)| event == "response.output_item.done")
        .map(|(_, v)| &v["item"])
        .collect()
}

// Ported from TestConvertOpenAIResponsesRequestToGemini_ToolSearchRoundTrip.
#[test]
fn tool_search_round_trip() {
    let out = convert(MODEL, &fixture(), true);
    let decls = out["tools"][0]["functionDeclarations"].as_array().unwrap();
    assert_eq!(
        decls
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["exec_command", "tool_search", "fixture_tools__ping"]
    );
    assert_eq!(
        decls[1]["description"],
        "Search available deferred tools. Multi-agent tools: Spawn and manage sub-agents."
    );
    assert_eq!(decls[1]["parametersJsonSchema"]["required"][0], "query");
    assert_eq!(decls[2]["description"], "Return a test reply");
    let contents = out["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 5);
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(
        contents[1]["parts"][0]["functionCall"],
        json!({"name":"tool_search","id":"search_1","args":{"query":"fixture tools","limit":1}})
    );
    assert_eq!(contents[2]["role"], "user");
    let result = &contents[2]["parts"][0]["functionResponse"];
    assert_eq!(result["name"], "tool_search");
    assert_eq!(result["id"], "search_1");
    assert_eq!(
        serde_json::from_str::<Value>(result["response"]["result"].as_str().unwrap()).unwrap(),
        json!({"status":"completed","tools":["fixture_tools__ping"]})
    );
    assert_eq!(
        contents[3]["parts"][0]["functionCall"]["name"],
        "fixture_tools__ping"
    );
    assert_eq!(
        contents[4]["parts"][0]["functionResponse"]["name"],
        "fixture_tools__ping"
    );
}

// Ported from TestConvertGeminiResponseToOpenAIResponses_ToolSearchCallStream.
#[test]
fn tool_search_call_stream() {
    let events = stream(
        &fixture(),
        &response("tool_search", json!({"query":"fixture tools","limit":2})),
    );
    assert!(
        !events
            .iter()
            .any(|(e, _)| e.starts_with("response.function_call_arguments."))
    );
    let added = &events
        .iter()
        .find(|(e, _)| e == "response.output_item.added")
        .unwrap()
        .1["item"];
    let done = done_items(&events)[0];
    let completed = &events
        .iter()
        .find(|(e, _)| e == "response.completed")
        .unwrap()
        .1["response"]["output"][0];
    for (item, status) in [
        (added, "in_progress"),
        (done, "completed"),
        (completed, "completed"),
    ] {
        assert_eq!(item["type"], "tool_search_call");
        assert_eq!(item["execution"], "client");
        assert_eq!(item["status"], status);
        assert!(item.get("name").is_none());
        assert_eq!(
            item["arguments"],
            json!({"query":"fixture tools","limit":2})
        );
        assert!(!item["call_id"].as_str().unwrap().is_empty());
        assert_eq!(item["call_id"], added["call_id"]);
        assert_eq!(
            item["id"],
            format!("tsc_{}", item["call_id"].as_str().unwrap())
        );
    }
}

// Ported from TestConvertGeminiResponseToOpenAIResponsesNonStream_ToolSearchCall.
#[test]
fn tool_search_call_non_stream() {
    let out = convert_gemini_response_to_openai_responses_non_stream(
        &fixture(),
        &Value::Null,
        response("tool_search", json!({"query":"fixture tools"}))
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    let item = &out["output"][0];
    assert_eq!(item["type"], "tool_search_call");
    assert_eq!(item["execution"], "client");
    assert_eq!(item["status"], "completed");
    assert_eq!(item["arguments"], json!({"query":"fixture tools"}));
    assert!(item.get("name").is_none());
    let call_id = item["call_id"].as_str().unwrap();
    assert!(!call_id.is_empty());
    assert_eq!(item["id"], format!("tsc_{call_id}"));
}

// Ported from 90c447d3's TestOpenAIResponsesToolSearchCallItemID.
#[test]
fn tool_search_call_item_id_uses_tsc_prefix() {
    let item = call_item("call_fixture", "completed", r#"{"query":"fixture"}"#);
    assert_eq!(item["id"], "tsc_call_fixture");
    assert_eq!(item["call_id"], "call_fixture");
}

#[test]
fn partial_args_tool_search_identity_is_stable_and_round_trips_by_call_id() {
    let mut request = fixture();
    let mut translator = GeminiToOpenAIResponsesStream::new(MODEL, &request, &Value::Null);
    let chunks = [
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"tool_search","id":"call_fixture","willContinue":true,"partialArgs":[{"jsonPath":"$.query","stringValue":"fixture ","willContinue":true}]}}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.query","stringValue":"tools"}]}}]},"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":1}}),
    ];
    let first = sse_events(&translator.translate_line(chunks[0].to_string().as_bytes()));
    assert!(done_items(&first).is_empty());
    let mut events = first;
    events.extend(sse_events(
        &translator.translate_line(chunks[1].to_string().as_bytes()),
    ));
    assert!(
        !events
            .iter()
            .any(|(kind, _)| kind.starts_with("response.function_call_arguments."))
    );
    let added = &events
        .iter()
        .find(|(kind, _)| kind == "response.output_item.added")
        .unwrap()
        .1["item"];
    let done = done_items(&events)[0];
    let completed = &events
        .iter()
        .find(|(kind, _)| kind == "response.completed")
        .unwrap()
        .1["response"]["output"][0];
    for item in [added, done, completed] {
        assert_eq!(item["id"], "tsc_call_fixture");
        assert_eq!(item["call_id"], "call_fixture");
    }
    assert_eq!(done["arguments"], json!({"query":"fixture tools"}));
    assert_eq!(completed, done);
    request["input"] = json!([
        {"type":"message","role":"user","content":"find tools"},
        done,
        {"type":"tool_search_output","id":"tso_fixture","call_id":"call_fixture","execution":"client","status":"completed","tools":[]}
    ]);
    let translated = convert(MODEL, &request, true);
    assert_eq!(
        translated["contents"][1]["parts"][0]["functionCall"]["id"],
        "call_fixture"
    );
    assert_eq!(
        translated["contents"][2]["parts"][0]["functionResponse"]["id"],
        "call_fixture"
    );
}

// Ported from TestConvertOpenAIResponsesRequestToGemini_ToolSearchCallWithStringArgumentsAndEmptyOutput.
#[test]
fn string_arguments_and_empty_output() {
    let req = json!({"tools":[{"type":"tool_search","execution":"client"}],"input":[
        {"type":"message","role":"user","content":"hi"},
        {"type":"tool_search_call","execution":"client","call_id":"search_2","arguments":"{\"query\":\"nothing\"}"},
        {"type":"tool_search_output","execution":"client","call_id":"search_2","status":"completed","tools":[]}]});
    let out = convert(MODEL, &req, false);
    assert_eq!(out["contents"].as_array().unwrap().len(), 3);
    assert_eq!(
        out["contents"][1]["parts"][0]["functionCall"]["args"]["query"],
        "nothing"
    );
    let result: Value = serde_json::from_str(
        out["contents"][2]["parts"][0]["functionResponse"]["response"]["result"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result, json!({"status":"completed","tools":[]}));
}

// Ported from TestConvertGeminiResponseToOpenAIResponses_DiscoveredToolCallRestoresNamespace.
#[test]
fn discovered_tool_restores_namespace() {
    let events = stream(&fixture(), &response("fixture_tools__ping", json!({})));
    let item = done_items(&events)[0];
    assert_eq!(item["type"], "function_call");
    assert_eq!(item["name"], "ping");
    assert_eq!(item["namespace"], "fixture_tools");
}

// Ported from TestConvertGeminiResponseToOpenAIResponses_UserToolSearchFunctionStaysFunctionCall.
#[test]
fn user_tool_search_stays_function_call() {
    let req = json!({"tools":[{"type":"function","name":"tool_search","parameters":{"type":"object","properties":{"query":{"type":"string"}}}}]});
    let events = stream(&req, &response("tool_search", json!({"query":"x"})));
    assert_eq!(done_items(&events)[0]["type"], "function_call");
}

// Ported from TestConvertGeminiResponses_SameNamedUserFunctionAndClientToolSearchCoexist.
#[test]
fn same_named_user_function_and_client_search_coexist() {
    for search_first in [false, true] {
        let mut tools = vec![
            json!({"type":"function","name":"tool_search","parameters":{"type":"object","properties":{"x":{"type":"string"}},"required":["x"]}}),
            json!({"type":"tool_search","execution":"client"}),
        ];
        if search_first {
            tools.reverse();
        }
        tools
            .push(json!({"type":"function","name":"hidden_until_discovered","defer_loading":true}));
        let mut req = json!({"tools":tools,"input":[{"type":"message","role":"user","content":"hi"},
            {"type":"tool_search_call","execution":"client","call_id":"search_1","status":"completed","arguments":{"query":"ping"}},
            {"type":"tool_search_output","execution":"client","call_id":"search_1","status":"completed","tools":[]},
            {"type":"function_call","call_id":"user_call_1","name":"tool_search","arguments":"{\"x\":\"val\"}"},
            {"type":"function_call_output","call_id":"user_call_1","output":"user_ok"}]});
        let out = convert(MODEL, &req, true);
        assert_eq!(
            out["tools"][0]["functionDeclarations"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let shim = map_client_search_name(&build_gemini_function_declarations(&req).forward);
        assert_ne!(shim, "tool_search");
        let c = &out["contents"];
        assert_eq!(c.as_array().unwrap().len(), 5);
        assert_eq!(c[1]["parts"][0]["functionCall"]["name"], shim);
        assert_eq!(c[2]["parts"][0]["functionResponse"]["name"], shim);
        assert_eq!(c[3]["parts"][0]["functionCall"]["name"], "tool_search");
        assert_eq!(c[4]["parts"][0]["functionResponse"]["name"], "tool_search");
        let resp = json!({"candidates":[{"content":{"role":"model","parts":[
            {"functionCall":{"id":"c_shim","name":shim,"args":{"query":"agents"}}},
            {"functionCall":{"id":"c_user","name":"tool_search","args":{"x":"abc"}}}]},"finishReason":"STOP"}]});
        let events = stream(&req, &resp);
        let done = done_items(&events);
        assert_eq!(done.len(), 2);
        assert_eq!(done[0]["type"], "tool_search_call");
        assert_eq!(done[0]["call_id"], "c_shim");
        assert_eq!(done[0]["arguments"]["query"], "agents");
        assert_eq!(done[1]["type"], "function_call");
        assert_eq!(done[1]["name"], "tool_search");
        req["tool_choice"] = json!({"type":"tool_search"});
        let out = convert(MODEL, &req, true);
        assert_eq!(
            out["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
            json!([shim])
        );
    }
}

// Ported from TestConvertOpenAIResponsesRequestToGemini_HistoryOnlyWithNamespacedToolSearchChild.
#[test]
fn history_only_namespaced_tool_search_child() {
    let req = json!({"tools":[],"input":[
        {"type":"tool_search_call","execution":"client","call_id":"search_1","arguments":{"query":"fixture tools","limit":1}},
        {"type":"tool_search_output","execution":"client","call_id":"search_1","tools":[{"type":"namespace","name":"fixture_tools","tools":[{"type":"function","name":"tool_search","parameters":{"type":"object","properties":{}}}]}]}]});
    let out = convert(MODEL, &req, true);
    assert_eq!(
        out["tools"][0]["functionDeclarations"][0]["name"],
        "fixture_tools__tool_search"
    );
    assert_eq!(out["contents"].as_array().unwrap().len(), 2);
    assert_eq!(
        out["contents"][0]["parts"][0]["functionCall"]["name"],
        "tool_search"
    );
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["name"],
        "tool_search"
    );
}

// Not upstream's: cover helper validation, hosted history, failure summaries and mapped-name deduplication.
#[test]
fn invalid_arguments_and_discovery_summaries() {
    for args in ["[]", "null", "broken", "\"text\""] {
        assert_eq!(call_item("c", "completed", args)["status"], "incomplete");
        assert_eq!(call_item("c", "completed", args)["arguments"], json!({}));
        assert_eq!(call_item("c", "in_progress", args)["status"], "in_progress");
    }
    let items = json!([
        {"type":"tool_search_call","execution":"server"},
        {"type":"tool_search_output","status":"failed","error":{"message":"failed"},"tools":[
            {"type":"function","name":"a.b"},{"type":"function","function":{"name":"a.b"}},
            {"type":"namespace","name":"ns","children":[{"name":"child"}]}]}
    ]);
    let normalized = normalize_items(
        items.as_array().unwrap(),
        &HashMap::from([("a.b".into(), "a_b".into())]),
    );
    assert_eq!(normalized.len(), 1);
    let summary: Value = serde_json::from_str(normalized[0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(
        summary,
        json!({"status":"failed","error":{"message":"failed"},"tools":["a_b","ns__child"]})
    );
}

// Not upstream's: check upstream call IDs and truncated item statuses in both response modes.
#[test]
fn tool_search_upstream_id_and_incomplete_status() {
    for (reason, args, status) in [
        ("MAX_TOKENS", json!({"query":"x"}), "incomplete"),
        ("STOP", json!([]), "incomplete"),
    ] {
        let mut resp = response("tool_search", args);
        resp["candidates"][0]["content"]["parts"][0]["functionCall"]["id"] = "upstream_call".into();
        resp["candidates"][0]["finishReason"] = reason.into();
        let events = stream(&fixture(), &resp);
        let done = done_items(&events)[0];
        assert_eq!(done["call_id"], "upstream_call");
        assert_eq!(done["status"], status);
        let out = convert_gemini_response_to_openai_responses_non_stream(
            &fixture(),
            &Value::Null,
            resp.to_string().as_bytes(),
        )
        .unwrap();
        assert_eq!(out["output"][0]["call_id"], "upstream_call");
        assert_eq!(out["output"][0]["status"], status);
    }
}

// Not upstream's: missing args have different terminal statuses in the two upstream paths.
#[test]
fn missing_search_arguments_follow_response_mode() {
    let req = json!({"tools":[{"type":"tool_search","execution":"client"}]});
    let resp = json!({"candidates":[{"content":{"role":"model","parts":[
        {"functionCall":{"id":"s1","name":"tool_search"}}]},"finishReason":"STOP"}]});
    let out = convert_gemini_response_to_openai_responses_non_stream(
        &req,
        &Value::Null,
        resp.to_string().as_bytes(),
    )
    .unwrap();
    assert_eq!(
        out["output"][0],
        json!({"id":"tsc_s1","type":"tool_search_call",
        "status":"incomplete","execution":"client","call_id":"s1","arguments":{}})
    );
    let events = stream(&req, &resp);
    let expected = json!({"id":"tsc_s1","type":"tool_search_call",
        "status":"completed","execution":"client","call_id":"s1","arguments":{}});
    assert_eq!(*done_items(&events)[0], expected);
    let completed = events
        .iter()
        .find(|(e, _)| e == "response.completed")
        .unwrap();
    assert_eq!(completed.1["response"]["output"][0], expected);
}

// Not upstream's: gjson walks object-valued children in summaries but not declarations.
#[test]
fn object_namespace_children_are_summarized_but_not_declared() {
    let req = json!({"input":[{"type":"tool_search_output","tools":[
        {"type":"namespace","name":"ns","children":{"a":{"name":"child"}}}]}]});
    let declarations = build_gemini_function_declarations(&req);
    assert!(declarations.declarations.is_empty());
    let normalized = normalize_items(req["input"].as_array().unwrap(), &declarations.forward);
    let summary: Value = serde_json::from_str(normalized[0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(summary, json!({"status":"completed","tools":["ns__child"]}));
}

// Not upstream's: ordinary image requests must avoid a full copy during search normalization.
#[test]
fn ordinary_input_is_borrowed_during_search_normalization() {
    let items = json!([{"type":"message","role":"user","content":[
        {"type":"input_image","image_url":format!("data:image/png;base64,{}", "A".repeat(65536))}]}]);
    let input = items.as_array().unwrap();
    let normalized = normalize_items(input, &HashMap::new());
    assert!(matches!(normalized, Cow::Borrowed(_)));
    assert!(std::ptr::eq(normalized.as_ref(), input.as_slice()));
    assert_eq!(normalized.as_ref(), input);
}

#[test]
fn tool_arguments_preserve_uppercase_exponent() {
    assert_eq!(
        super::call_item("c", "completed", r#"{"n":1E+2}"#)["arguments"].to_string(),
        r#"{"n":1E+2}"#
    );
}
