//! Response restoring and filtering, ported from upstream's
//! `xai_executor_test.go`.

use serde_json::{Value, json};

use super::*;
use crate::xai::tools::{NamespaceRef, collect_client_declared_tool_keys, has_native_x_search};

fn parse(text: &[u8]) -> Value {
    serde_json::from_slice(text).expect("valid JSON")
}

/// `restoreXAINamespaceToolCalls`: a fresh restorer's work on one event.
fn restore(data: &str, refs: &NamespaceRefs) -> Value {
    parse(&NamespaceRestorer::new(refs.clone()).restore(data.as_bytes().to_vec()))
}

fn dispatcher(namespace: &str) -> NamespaceRefs {
    NamespaceRefs::from([(
        namespace.to_owned(),
        NamespaceRef {
            namespace: namespace.to_owned(),
            name: String::new(),
            is_dispatcher: true,
        },
    )])
}

fn key(namespace: &str, name: &str, tool_type: &str) -> ClientToolKey {
    ClientToolKey {
        namespace: namespace.to_owned(),
        name: name.to_owned(),
        tool_type: tool_type.to_owned(),
    }
}

fn is_internal(item: &str, client_tools: &HashSet<ClientToolKey>) -> bool {
    is_internal_x_search_call(Some(&parse(item.as_bytes())), client_tools)
}

// TestXAIInternalXSearchResponseFilterRequiresNativeTool.
#[test]
fn x_search_filter_requires_native_tool() {
    assert!(
        !has_native_x_search(&json!({"tools": [{"type": "web_search"}]})),
        "web_search must not enable internal X search filtering"
    );
    assert!(has_native_x_search(
        &json!({"tools": [{"type": "x_search"}]})
    ));

    let event = br#"{"type":"response.output_item.done","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","name":"x_keyword_search"}}"#.to_vec();
    assert_eq!(
        XSearchFilter::new(false, HashSet::new()).apply(event.clone()),
        Some(event.clone()),
        "a disabled filter changes nothing"
    );
    assert_eq!(XSearchFilter::new(true, HashSet::new()).apply(event), None);
}

// TestXAIIsInternalXSearchCallPreservesClientDeclaredTools.
#[test]
fn internal_x_search_call_preserves_client_declared_tools() {
    let client_tools = collect_client_declared_tool_keys(&json!({"tools": [
        {"type": "x_search"},
        {"type": "function", "name": "x_keyword_search", "parameters": {"type": "object"}},
        {"type": "custom", "name": "x_keyword_search"},
        {"type": "namespace", "name": "acme", "tools": [
            {"type": "function", "name": "x_keyword_search", "parameters": {"type": "object"}},
            {"type": "custom", "name": "x_keyword_search"}
        ]}
    ]}));
    // A custom tool goes as a function, so both are keyed as functions.
    assert!(client_tools.contains(&key("", "x_keyword_search", "function")));
    assert!(!client_tools.contains(&key("", "x_keyword_search", "custom")));
    assert!(client_tools.contains(&key("acme", "x_keyword_search", "function")));
    assert!(!client_tools.contains(&key("acme", "x_keyword_search", "custom")));

    // Names the client didn't declare are X search's.
    assert!(is_internal(
        r#"{"type":"custom_tool_call","name":"x_user_search"}"#,
        &client_tools
    ));
    assert!(is_internal(
        r#"{"type":"function_call","name":"x_semantic_search"}"#,
        &client_tools
    ));

    // A function call of a declared name is the client's.
    assert!(!is_internal(
        r#"{"type":"function_call","name":"x_keyword_search","call_id":"call_plain"}"#,
        &client_tools
    ));
    // An xs_call ID is X search's, whatever the client declared.
    let internal_same_name =
        r#"{"type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search"}"#;
    assert!(is_internal(internal_same_name, &client_tools));

    // A function declaration doesn't cover a custom call of the name.
    let function_only = collect_client_declared_tool_keys(&json!({"tools": [
        {"type": "function", "name": "x_keyword_search", "parameters": {"type": "object"}}
    ]}));
    let plain_internal_custom =
        r#"{"type":"custom_tool_call","name":"x_keyword_search","call_id":"call_other"}"#;
    assert!(is_internal(plain_internal_custom, &function_only));

    // Nor does a custom declaration, which goes as a function.
    let custom_only = collect_client_declared_tool_keys(
        &json!({"tools": [{"type": "custom", "name": "x_keyword_search"}]}),
    );
    assert!(custom_only.contains(&key("", "x_keyword_search", "function")));
    assert!(!is_internal(
        r#"{"type":"function_call","name":"x_keyword_search","call_id":"call_custom_fn"}"#,
        &custom_only
    ));
    assert!(is_internal(plain_internal_custom, &custom_only));
    assert!(is_internal(internal_same_name, &custom_only));

    // A namespaced call is a restored client tool's, declared or not.
    let namespaced = r#"{"type":"function_call","name":"x_keyword_search","namespace":"acme"}"#;
    assert!(!is_internal(namespaced, &client_tools));
    assert!(!is_internal(namespaced, &HashSet::new()));
}

// TestXAIInternalXSearchResponseFilterPreservesClientToolsInCompletedOutput.
#[test]
fn x_search_filter_preserves_client_tools_in_completed_output() {
    let client_tools = HashSet::from([
        key("", "x_keyword_search", "function"),
        key("acme", "x_keyword_search", "function"),
    ]);
    let mut filter = XSearchFilter::new(true, client_tools);
    let event = br#"{
        "type":"response.completed",
        "response":{
            "output":[
                {"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search","input":"{}"},
                {"id":"fc_plain","type":"function_call","call_id":"call_plain","name":"x_keyword_search","arguments":"{}"},
                {"id":"fc_ns","type":"function_call","call_id":"call_ns","name":"x_keyword_search","namespace":"acme","arguments":"{}"},
                {"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}
            ]
        }
    }"#;
    let got = parse(&filter.apply(event.to_vec()).expect("completed event kept"));
    let output = got["response"]["output"].as_array().expect("output");
    assert_eq!(output.len(), 3, "{got}");
    assert!(output.iter().all(|item| item["type"] != "custom_tool_call"));
    assert_eq!(output[0]["name"], "x_keyword_search");
    assert_eq!(output[0]["type"], "function_call");
    assert_eq!(output[1]["namespace"], "acme");
}

// Not upstream's: the events about a hidden call go with it, and the output
// indexes after it close up.
#[test]
fn x_search_filter_drops_events_of_hidden_calls() {
    let mut filter = XSearchFilter::new(true, HashSet::new());
    let mut apply = |event: &str| {
        filter
            .apply(event.as_bytes().to_vec())
            .map(|data| String::from_utf8(data).expect("UTF-8"))
    };
    assert_eq!(
        apply(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search"}}"#
        ),
        None
    );
    for event in [
        r#"{"type":"response.custom_tool_call_input.delta","output_index":0,"delta":"{"}"#,
        r#"{"type":"response.custom_tool_call_input.done","item_id":"ctc_1","input":"{}"}"#,
        r#"{"type":"x","call_id":" xs_call-1 "}"#,
    ] {
        assert_eq!(apply(event), None, "{event}");
    }
    let message = r#"{"type":"response.output_item.added","output_index":1,"item":{"id":"msg_1","type":"message"}}"#;
    assert_eq!(
        apply(message).as_deref(),
        Some(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message"}}"#
        )
    );
    // An event without an output index, or before the hidden call's, is
    // left as it came.
    let delta = r#"{"type":"response.output_text.delta", "delta":"hi"}"#;
    assert_eq!(apply(delta).as_deref(), Some(delta));
    // Invalid JSON is passed on.
    assert_eq!(apply("{").as_deref(), Some("{"));
}

// TestRestoreXAINamespaceToolCalls_DispatcherVariants.
#[test]
fn restores_dispatcher_variants() {
    let refs = dispatcher("mcp__app_0");

    // The child's arguments flattened into the dispatcher's.
    let restored = restore(
        r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"mcp__app_0","call_id":"c1","arguments":"{\"name\":\"tool_a\",\"arg1\":\"val1\"}"}}"#,
        &refs,
    );
    assert_eq!(restored["item"]["name"], "tool_a");
    assert_eq!(restored["item"]["namespace"], "mcp__app_0");
    assert_eq!(restored["item"]["arguments"], r#"{"arg1":"val1"}"#);

    // The child's arguments as a string.
    let restored = restore(
        r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"mcp__app_0","call_id":"c2","arguments":"{\"name\":\"tool_b\",\"arguments\":\"{\\\"k\\\":\\\"v\\\"}\"}"}}"#,
        &refs,
    );
    assert_eq!(restored["item"]["name"], "tool_b");
    assert_eq!(restored["item"]["arguments"], r#"{"k":"v"}"#);

    // No arguments.
    let restored = restore(
        r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"mcp__app_0","call_id":"c3","arguments":"{\"name\":\"tool_c\"}"}}"#,
        &refs,
    );
    assert_eq!(restored["item"]["name"], "tool_c");
    assert_eq!(restored["item"]["arguments"], "{}");
}

// TestRestoreXAINamespaceToolCalls_FunctionCallArgumentsDone.
#[test]
fn restores_function_call_arguments_done() {
    let mut restorer = NamespaceRestorer::new(dispatcher("mcp__app_0"));
    let mut restore = |event: &str| parse(&restorer.restore(event.as_bytes().to_vec()));

    let added = restore(
        r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"item_1","type":"function_call","name":"mcp__app_0"}}"#,
    );
    assert_eq!(added["item"]["namespace"], "mcp__app_0");

    let arguments = restore(
        r#"{"type":"response.function_call_arguments.done","item_id":"item_1","output_index":0,"arguments":"{\"name\":\"tool_x\",\"arguments\":{\"count\":42}}"}"#,
    );
    assert_eq!(arguments["arguments"], r#"{"count":42}"#);

    let done = restore(
        r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"item_1","type":"function_call","name":"mcp__app_0","arguments":"{\"name\":\"tool_x\",\"arguments\":{\"count\":42}}"}}"#,
    );
    assert_eq!(done["item"]["name"], "tool_x");
    assert_eq!(done["item"]["namespace"], "mcp__app_0");
}

// TestRestoreXAINamespaceToolCalls_FoldModePreservesNonDispatcherArgumentsDone.
#[test]
fn fold_mode_preserves_non_dispatcher_arguments_done() {
    let mut restorer = NamespaceRestorer::new(dispatcher("mcp__app_0"));
    let added = br#"{"type":"response.output_item.added","output_index":0,"item":{"id":"non_disp_1","type":"function_call","name":"web_search"}}"#;
    assert_eq!(restorer.restore(added.to_vec()), added);

    let done = br#"{"type":"response.function_call_arguments.done","item_id":"non_disp_1","output_index":0,"arguments":"{\"name\":\"golang\",\"query\":\"test\"}"}"#;
    assert_eq!(restorer.restore(done.to_vec()), done);
}

// TestRestoreXAINamespaceToolCalls_FlattenModePreservesNameInArgumentsDone.
#[test]
fn flatten_mode_preserves_name_in_arguments_done() {
    let refs = NamespaceRefs::from([(
        "mcp__github__create_repo".to_owned(),
        NamespaceRef {
            namespace: "mcp__github".to_owned(),
            name: "create_repo".to_owned(),
            is_dispatcher: false,
        },
    )]);
    let event = r#"{"type":"response.function_call_arguments.done","item_id":"item_1","output_index":0,"arguments":"{\"name\":\"my-awesome-repo\",\"private\":true}"}"#;
    assert_eq!(
        restore(event, &refs)["arguments"],
        r#"{"name":"my-awesome-repo","private":true}"#
    );
}

// TestRestoreXAINamespaceToolCalls_OutputItemAddedInDispatcherMode.
#[test]
fn output_item_added_in_dispatcher_mode() {
    let restored = restore(
        r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","name":"mcp__app_0","status":"in_progress"}}"#,
        &dispatcher("mcp__app_0"),
    );
    assert_eq!(restored["item"]["namespace"], "mcp__app_0");
    assert_eq!(
        restored["item"]["name"], "mcp__app_0",
        "the child isn't known yet"
    );
}

// TestRestoreXAINamespaceToolCalls.
#[test]
fn restores_namespace_tool_calls() {
    let refs = tools::collect_namespace_refs(
        &json!({"tools": [{"type": "namespace", "name": "mcp__exa", "tools": [
            {"type": "function", "name": "web_search_exa", "parameters": {"type": "object"}}
        ]}]}),
        false,
    );
    let restored = restore(
        r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"mcp__exa__web_search_exa","call_id":"call_1","arguments":"{}"}}"#,
        &refs,
    );
    assert_eq!(restored["item"]["name"], "web_search_exa");
    assert_eq!(restored["item"]["namespace"], "mcp__exa");

    let restored = restore(
        r#"{"type":"response.completed","response":{"output":[{"type":"function_call","name":"mcp__exa__web_search_exa","call_id":"call_1","arguments":"{}"}]}}"#,
        &refs,
    );
    assert_eq!(restored["response"]["output"][0]["name"], "web_search_exa");
    assert_eq!(restored["response"]["output"][0]["namespace"], "mcp__exa");
}

// TestRestoreXAINamespaceToolCallsPreservesMalformedPayload.
#[test]
fn restore_preserves_malformed_payload() {
    let data = br#"{"item":{"type":"function_call","name":"mcp__exa__web_search_exa""#;
    let refs = NamespaceRefs::from([(
        "mcp__exa__web_search_exa".to_owned(),
        NamespaceRef {
            namespace: "mcp__exa".to_owned(),
            name: "web_search_exa".to_owned(),
            is_dispatcher: false,
        },
    )]);
    assert_eq!(
        NamespaceRestorer::new(refs).restore(data.to_vec()),
        data.to_vec()
    );
}

// TestNormalizeXAITools_WhenFlattenedCountExceedsLimit_FoldsNamespaces, the
// restore half: a folded namespace's dispatcher call becomes its child's.
#[test]
fn restores_folded_dispatcher_call() {
    let tools: Vec<Value> = (0..47)
        .map(|namespace| {
            json!({"type": "namespace", "name": format!("mcp__app_{namespace}"), "tools":
                (0..10).map(|tool| json!({"type": "function", "name": format!("tool_{tool}"),
                    "parameters": {"type": "object", "properties": {"p": {"type": "string"}}}}))
                    .collect::<Vec<_>>()})
        })
        .collect();
    let body = json!({ "tools": tools });
    let fold = tools::should_fold(&body, false);
    assert!(fold);
    let refs = tools::collect_namespace_refs(&body, fold);
    let restored = restore(
        r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"mcp__app_0","call_id":"call_1","arguments":"{\"name\":\"tool_3\",\"arguments\":{\"p\":\"val\"}}"}}"#,
        &refs,
    );
    assert_eq!(restored["item"]["name"], "tool_3");
    assert_eq!(restored["item"]["namespace"], "mcp__app_0");
    assert_eq!(restored["item"]["arguments"], r#"{"p":"val"}"#);
}

// Not upstream's: a dispatcher's arguments that don't name a child (not
// JSON, a name that isn't a string, or another dispatcher) leave the call
// with only its namespace; with no namespace, a name needs arguments or a
// dispatcher's.
#[test]
fn unwrap_dispatcher_arguments_refusals() {
    let mut refs = dispatcher("mcp__app_0");
    refs.insert(
        "mcp__app_0__inner".to_owned(),
        NamespaceRef {
            namespace: "mcp__app_0__inner".to_owned(),
            name: String::new(),
            is_dispatcher: true,
        },
    );
    for arguments in [
        "not json",
        r#"{"name":1}"#,
        r#"{"name":"  "}"#,
        r#"{"name":"inner"}"#,
    ] {
        assert_eq!(
            unwrap_dispatcher_arguments(arguments, "mcp__app_0", &refs),
            None,
            "{arguments}"
        );
    }
    assert_eq!(
        unwrap_dispatcher_arguments(r#"{"name":"tool","q":1}"#, "", &refs),
        None
    );
    assert_eq!(
        unwrap_dispatcher_arguments(r#"{"name":"mcp__app_0","q":1}"#, "", &refs),
        Some(("mcp__app_0".to_owned(), r#"{"q":1}"#.to_owned()))
    );
    assert_eq!(
        unwrap_dispatcher_arguments(r#"{"name":" tool ","arguments":""}"#, "", &refs),
        Some(("tool".to_owned(), "{}".to_owned()))
    );
    assert_eq!(
        unwrap_dispatcher_arguments(r#"{"name":"tool","arguments":null}"#, "mcp__app_0", &refs),
        Some(("tool".to_owned(), "null".to_owned()))
    );

    let restored = restore(
        r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"mcp__app_0","arguments":"oops"}}"#,
        &refs,
    );
    assert_eq!(restored["item"]["name"], "mcp__app_0");
    assert_eq!(restored["item"]["namespace"], "mcp__app_0");
    assert_eq!(restored["item"]["arguments"], "oops");
}

// TestXAIExecutorAliasesClientWebSearchWithExistingAliasCollision, the
// restore half: only the alias in use is restored.
#[test]
fn restores_only_the_web_search_alias_in_use() {
    let alias = "clientfn_web_search_1";
    let original = br#"{"type":"response.output_item.done","item":{"type":"function_call","name":"clientfn_web_search","call_id":"call_1"}}"#;
    assert_eq!(
        restore_client_web_search_name(original.to_vec(), alias),
        original
    );
    let aliased = br#"{"type":"response.output_item.done","item":{"type":"function_call","name":"clientfn_web_search_1","call_id":"call_2"}}"#;
    assert_eq!(
        parse(&restore_client_web_search_name(aliased.to_vec(), alias))["item"]["name"],
        "web_search"
    );
}

// Not upstream's, after TestXAIExecutorRestoresAliasedWebSearchInStreamAndExecute
// and TestXAIExecutorDoesNotRestoreNamespacedClientfnWebSearch (which run
// the executor): every place the alias can be is restored, and a
// namespaced call keeps its name.
#[test]
fn restores_web_search_alias_everywhere_but_namespaces() {
    let alias = "clientfn_web_search";
    let restored = |event: &str| {
        String::from_utf8(restore_client_web_search_name(
            event.as_bytes().to_vec(),
            alias,
        ))
        .expect("UTF-8")
    };
    assert_eq!(
        restored(
            r#"{"item":{"name":" clientfn_web_search ","function":{"name":"clientfn_web_search"}}}"#
        ),
        r#"{"item":{"name":"web_search","function":{"name":"web_search"}}}"#
    );
    assert_eq!(
        restored(
            r#"{"response":{"output":[{"name":"clientfn_web_search","namespace":"acme"},{"function":{"name":"clientfn_web_search"}}]}}"#
        ),
        r#"{"response":{"output":[{"name":"clientfn_web_search","namespace":"acme"},{"function":{"name":"web_search"}}]}}"#
    );
    assert_eq!(
        restored(r#"{"output":[{"name":"clientfn_web_search"}],"name":"clientfn_web_search"}"#),
        r#"{"output":[{"name":"web_search"}],"name":"web_search"}"#
    );
    // The top level's function.name isn't one upstream restores.
    let top_function = r#"{"function":{"name":"clientfn_web_search"}}"#;
    assert_eq!(restored(top_function), top_function);
    let namespaced = r#"{"item":{"name":"clientfn_web_search","namespace":"acme"}}"#;
    assert_eq!(restored(namespaced), namespaced);
    assert_eq!(
        restore_client_web_search_name(b"{".to_vec(), alias),
        b"{".to_vec()
    );
    let untouched = r#"{"item":{"name":"clientfn_web_search"}}"#;
    assert_eq!(
        String::from_utf8(restore_client_web_search_name(
            untouched.as_bytes().to_vec(),
            ""
        ))
        .expect("UTF-8"),
        untouched
    );
}

// TestXAIPatchCompletedOutput_EnsuresUsageDetails.
#[test]
fn patch_completed_output_ensures_usage_details() {
    let event = br#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14}}}"#;
    let got = parse(&patch_completed_output(
        event.to_vec(),
        &OutputItems::default(),
    ));
    let usage = &got["response"]["usage"];
    assert_eq!(usage["output_tokens_details"]["reasoning_tokens"], 0);
    assert_eq!(usage["input_tokens_details"]["cached_tokens"], 0);
}

// Not upstream's: an empty output is filled in from the kept items, in
// output index order and then the rest; an output that came is kept.
#[test]
fn patch_completed_output_fills_in_only_missing_output() {
    let mut items = OutputItems::default();
    for event in [
        r#"{"type":"response.output_item.done","output_index":1,"item":{"id":"b"}}"#,
        r#"{"type":"response.output_item.done","item":{"id":"c"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"a"}}"#,
    ] {
        items.collect(&parse(event.as_bytes()));
    }
    let completed = br#"{"type":"response.completed","response":{"output":[],"usage":{"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}"#;
    let got = parse(&patch_completed_output(completed.to_vec(), &items));
    assert_eq!(
        got["response"]["output"],
        json!([{"id": "a"}, {"id": "b"}, {"id": "c"}])
    );

    let came = br#"{"type":"response.completed","response":{"output":[{"type":"message","id":" "}],"usage":{"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}"#;
    assert_eq!(
        patch_completed_output(came.to_vec(), &items),
        came.to_vec(),
        "xAI's output keeps its IDs as they came"
    );
}
