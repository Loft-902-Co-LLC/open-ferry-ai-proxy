//! Historical tool declarations and collision precedence (upstream 0c61b12c).

use super::*;
use crate::xai::response::NamespaceRestorer;

fn namespace(name: &str, start: usize, end: usize) -> Value {
    json!({"type": "namespace", "name": name, "tools": (start..end)
        .map(|i| json!({"type": "function", "name": format!("tool_{i}"),
            "parameters": {"type": "object"}})).collect::<Vec<_>>()})
}

fn clock(description: &str) -> Value {
    json!({"type": "namespace", "name": "clock", "tools": [
        {"type": "function", "name": "sleep", "description": description,
         "parameters": {"type": "object"}}
    ]})
}

fn normalized(mut body: Value, fold: bool) -> Value {
    normalize_tools(&mut body, fold);
    promote_additional_tools(&mut body);
    body
}

#[test]
fn promoted_tools_keep_first_declaration_and_namespace_mapping() {
    let flat = json!({"type": "function", "name": "clock__sleep", "description": "flat top"});
    for (top, history, namespaced) in [
        (flat.clone(), clock("history"), false),
        (clock("top"), flat, true),
    ] {
        let body = json!({"tools": [top], "input": [
            {"type": "additional_tools", "tools": [history]},
            {"role": "user", "content": "hi"}
        ]});
        let refs = collect_namespace_refs(&body, false);
        assert_eq!(refs.contains_key("clock__sleep"), namespaced);
        let out = normalized(body, false);
        assert_eq!(out["tools"].as_array().unwrap().len(), 1);
        assert_eq!(
            out["tools"][0]["description"],
            if namespaced { "top" } else { "flat top" }
        );
        assert_eq!(out["input"].as_array().unwrap().len(), 1);
    }
    let out = normalized(
        json!({"tools": [
            {"type": "function", "name": "same", "description": "first"},
            {"type": "custom", "name": "same"},
            {"type": "web_search"}, {"type": "web_search"}
        ]}),
        false,
    );
    assert_eq!(out["tools"].as_array().unwrap().len(), 3);
    assert_eq!(out["tools"][0]["description"], "first");
}

#[test]
fn folding_counts_unique_names_across_history_and_merges_namespace_children() {
    let app = namespace("mcp__app", 0, 120);
    let body = json!({"tools": [app.clone()], "input": [
        {"type": "additional_tools", "tools": [app.clone()]}
    ]});
    assert!(!should_fold(&body, false));
    assert_eq!(
        normalized(body, false)["tools"].as_array().unwrap().len(),
        120
    );

    let mut history = clock("history sleep");
    history["tools"].as_array_mut().unwrap().push(json!({
        "type": "function", "name": "alarm", "description": "history alarm"
    }));
    history["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "tool_search", "name": "search"}));
    let body = json!({"tools": [clock("top sleep"), app], "input": [
        {"type": "additional_tools", "tools": [history, namespace("mcp__extra", 120, 210)]}
    ]});
    assert!(should_fold(&body, false));
    let refs = collect_namespace_refs(&body, true);
    assert_eq!(
        refs["clock"].children,
        HashSet::from(["sleep".into(), "alarm".into()])
    );
    let out = normalized(body, true);
    assert_eq!(out["tools"].as_array().unwrap().len(), 3);
    let description = out["tools"][0]["description"].as_str().unwrap();
    assert!(description.contains("top sleep"));
    assert!(!description.contains("history sleep"));
    assert!(description.contains("history alarm"));
    assert_eq!(
        out["tools"][0]["parameters"]["properties"]["name"]["enum"],
        json!(["sleep", "alarm"])
    );
}

#[test]
fn unique_count_respects_exact_threshold_and_ignores_tool_search_children() {
    for (count, injected, fold) in [(200, false, false), (200, true, true), (201, false, true)] {
        let mut ns = namespace("ns", 0, count);
        ns["tools"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "tool_search"}));
        let body = json!({"tools": [ns.clone()], "input": [
            {"type": "additional_tools", "tools": [ns]}
        ]});
        assert_eq!(should_fold(&body, injected), fold);
    }
}

#[test]
fn folded_dispatcher_children_win_over_qualified_dispatcher_name_collisions() {
    let body = json!({"tools": [clock("top"), namespace("app", 0, 201)], "input": [
        {"type": "additional_tools", "tools": [
            {"type": "namespace", "name": "clock__sleep", "tools": [
                {"type": "function", "name": "alarm"}
            ]}
        ]}
    ]});
    let refs = collect_namespace_refs(&body, true);
    assert!(refs["clock__sleep"].is_dispatcher);
    for (dispatcher, child, arguments) in [
        ("clock", "sleep", json!({"duration_ms": 5})),
        ("clock__sleep", "alarm", json!({"ring": true})),
    ] {
        let event = json!({"type": "response.output_item.done", "item": {
            "type": "function_call", "name": dispatcher,
            "arguments": json!({"name": child, "arguments": arguments}).to_string()
        }});
        let out = NamespaceRestorer::new(refs.clone()).restore(event.to_string().into_bytes());
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(out["item"]["namespace"], dispatcher);
        assert_eq!(out["item"]["name"], child);
        assert_eq!(out["item"]["arguments"], arguments.to_string());
    }
    let invalid = json!({"type": "response.output_item.done", "item": {
        "type": "function_call", "name": "clock",
        "arguments": r#"{"name":"unknown","arguments":{}}"#
    }});
    let out = NamespaceRestorer::new(refs).restore(invalid.to_string().into_bytes());
    let out: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(out["item"]["name"], "clock");
    assert_eq!(out["item"]["arguments"], invalid["item"]["arguments"]);
}

#[test]
fn folded_flat_calls_are_not_restored_as_namespace_children_in_either_order() {
    let flat = json!({"type": "function", "name": "clock__sleep"});
    for tools in [vec![flat.clone(), clock("top")], vec![clock("top"), flat]] {
        let body = json!({"tools": tools, "input": [
            {"type": "additional_tools", "tools": [namespace("extra", 0, 201)]}
        ]});
        let refs = collect_namespace_refs(&body, true);
        assert!(!refs.contains_key("clock__sleep"));
        let event = br#"{"type":"response.output_item.done","item":{"type":"function_call","name":"clock__sleep","arguments":"{\"raw\":true}"}}"#;
        assert_eq!(NamespaceRestorer::new(refs).restore(event.to_vec()), event);
    }
}
