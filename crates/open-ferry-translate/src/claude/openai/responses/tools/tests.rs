// Ported from CLIProxyAPI internal/translator/claude/openai/responses/claude_openai-responses_tool_names_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use serde_json::json;

const ALPHA: &str =
    "very_long_tool_name_that_exceeds_sixty_four_characters_and_needs_truncation_alpha";
const BETA: &str =
    "very_long_tool_name_that_exceeds_sixty_four_characters_and_needs_truncation_beta";

#[test]
fn names_are_valid_unique_and_reversible() {
    let request = json!({
        "tools": [
            {"type": "function", "name": "normal_tool"},
            {"type": "function", "name": ALPHA},
            {"type": "function", "name": BETA},
            {"type": "custom", "name": "custom.tool.with.dots"},
            {"type": "namespace", "name": "mcp__my_server", "tools": [
                {"type": "function", "name": "get_something"}
            ]}
        ],
        "input": [
            {"type": "function_call", "name": "history_only_tool_name_that_is_also_very_long_exceeding_sixty_four_characters"}
        ]
    });
    let tools = RequestTools::new(&request);

    let mut seen = HashMap::new();
    for (id, claude_name) in &tools.to_claude {
        assert!(
            is_valid_claude_tool_name(claude_name),
            "{claude_name} for {id}"
        );
        if let Some(existing) = seen.insert(claude_name, id) {
            assert_eq!(existing, id, "{claude_name} collided");
        }
        assert_eq!(tools.identity(claude_name), id);
    }
    assert_eq!(tools.to_claude.len(), 6);

    assert_eq!(tools.claude_name("normal_tool"), "normal_tool");
    let qualified = "mcp__my_server__get_something";
    assert_eq!(tools.claude_name(qualified), qualified);
    assert_ne!(tools.claude_name(ALPHA), tools.claude_name(BETA));
}

#[test]
fn declaration_order_does_not_change_names() {
    let prices =
        "mcp__example_apps__acme_inventory_service__acme_inventory_service_get_item_prices";
    let metrics =
        "mcp__example_apps__acme_inventory_service__acme_inventory_service_get_item_metrics";
    let a = RequestTools::new(&json!({"tools": [
        {"type": "function", "name": prices},
        {"type": "function", "name": metrics}
    ]}));
    let b = RequestTools::new(&json!({"tools": [
        {"type": "function", "name": metrics},
        {"type": "function", "name": prices}
    ]}));
    assert_eq!(a.claude_name(prices), b.claude_name(prices));
    assert_eq!(a.claude_name(metrics), b.claude_name(metrics));
    assert_ne!(a.claude_name(prices), a.claude_name(metrics));
}

#[test]
fn a_single_long_name_is_cut_without_a_hash() {
    let long = "a_single_very_long_tool_name_that_exceeds_sixty_four_characters_cleanly";
    let tools = RequestTools::new(&json!({"tools": [{"type": "function", "name": long}]}));
    let claude_name = tools.claude_name(long);
    assert_eq!(claude_name, &long[..64]);
    assert_eq!(tools.identity(&claude_name), long);
}

#[test]
fn colliding_custom_tools_get_hashed_names() {
    let handler = "custom__long_namespace_path_exceeding_sixty_four_characters_long__operation_query_v1_execute_handler";
    let stream = "custom__long_namespace_path_exceeding_sixty_four_characters_long__operation_query_v1_execute_stream";
    let tools = RequestTools::new(&json!({"tools": [
        {"type": "custom", "name": handler},
        {"type": "custom", "name": stream}
    ]}));
    let (a, b) = (tools.claude_name(handler), tools.claude_name(stream));
    assert_ne!(a, b);
    assert!(a.len() <= 64 && b.len() <= 64);
    assert_eq!(tools.identity(&a), handler);
    assert_eq!(tools.identity(&b), stream);
    // Both sanitize alike, so both are hashed: 53 bytes, `_` and 10 hex digits.
    assert_eq!(a.len(), 64);
    assert!(a.starts_with(&handler[..53]));
    assert_eq!(tools.winner(handler).unwrap().kind, "custom");
}

#[test]
fn declared_tools_precede_history() {
    let tools = RequestTools::new(&json!({
        "tools": [{"type": "function", "name": "tool_x"}],
        "input": [{"type": "function_call", "name": "tool.x"}]
    }));
    assert_eq!(tools.claude_name("tool_x"), "tool_x");
    let history = tools.claude_name("tool.x");
    assert!(history.starts_with("tool_x_"), "{history}");
    assert_eq!(history.len(), "tool_x_".len() + 10);
}

#[test]
fn hashes_match_upstream() {
    // Expected values from Go's crypto/sha256 over "tool.x", and over
    // "tool.x", a NUL byte and "1".
    let tools = RequestTools::new(&json!({
        "tools": [{"type": "function", "name": "tool_x"}],
        "input": [{"type": "function_call", "name": "tool.x"}]
    }));
    assert_eq!(tools.claude_name("tool.x"), "tool_x_9f78b25061");

    // When that name is taken too, the seed gets a counter.
    let tools = RequestTools::new(&json!({
        "tools": [
            {"type": "function", "name": "tool_x"},
            {"type": "function", "name": "tool_x_9f78b25061"}
        ],
        "input": [{"type": "function_call", "name": "tool.x"}]
    }));
    assert_eq!(tools.claude_name("tool.x"), "tool_x_4b4de69d89");
}

#[test]
fn other_tool_types_keep_their_names() {
    let tools = RequestTools::new(&json!({"tools": [
        {"type": "web_search"},
        {"type": "web_search", "name": "search", "external_web_access": false},
        {"type": "file_search", "name": "files"},
        {"type": "mcp", "name": "server.tool"}
    ]}));
    let names: Vec<_> = tools
        .winning()
        .map(|d| (d.name.as_str(), d.kind.as_str()))
        .collect();
    assert_eq!(
        names,
        [("web_search", "web_search"), ("server.tool", "mcp")]
    );
    assert_eq!(tools.claude_name("server.tool"), "server.tool");
    assert_eq!(tools.identity("server.tool"), "server.tool");
}

#[test]
fn namespace_children_split_back() {
    let tools = RequestTools::new(&json!({"tools": [
        {"type": "namespace", "name": " ns ", "tools": [
            {"type": "function", "name": "lookup"},
            {"type": "custom", "function": {"name": "edit"}},
            {"type": "web_search", "name": "skipped"}
        ]},
        {"type": "function", "name": "direct"}
    ]}));
    assert_eq!(
        tools.split("ns__lookup"),
        ("lookup".to_owned(), "ns".to_owned())
    );
    assert_eq!(
        tools.split(" ns__edit "),
        ("edit".to_owned(), "ns".to_owned())
    );
    assert_eq!(tools.split("direct"), ("direct".to_owned(), String::new()));
    assert_eq!(
        tools.split("unknown"),
        ("unknown".to_owned(), String::new())
    );
    assert_eq!(tools.split(" "), (String::new(), String::new()));
    assert!(tools.winner("ns__skipped").is_none());
}

#[test]
fn name_map_prefers_direct_names() {
    let tools = RequestTools::new(&json!({"tools": [
        {"type": "namespace", "name": "ns", "tools": [
            {"type": "function", "name": "lookup"},
            {"type": "function", "name": "direct"}
        ]},
        {"type": "function", "name": "direct"}
    ]}));
    let accepted: HashSet<String> = ["ns__lookup", "ns__direct", "direct"]
        .into_iter()
        .map(String::from)
        .collect();
    let map = tools.name_map(&accepted);
    // A namespace child is mapped by its own name; `direct` is owned by the
    // direct declaration.
    assert_eq!(map["lookup"], "ns__lookup");
    assert_eq!(map["direct"], "direct");
    assert_eq!(map.len(), 2);
}

#[test]
fn qualify_trims_only_the_child() {
    assert_eq!(qualify("ns", " child "), "ns__child");
    assert_eq!(qualify(" ns ", "child"), " ns __child");
    assert_eq!(qualify("ns__", "child"), "ns__child");
    assert_eq!(qualify("ns", "ns__child"), "ns__child");
    assert_eq!(qualify("ns", "mcp__x"), "mcp__x");
    assert_eq!(qualify("", "child"), "child");
}
