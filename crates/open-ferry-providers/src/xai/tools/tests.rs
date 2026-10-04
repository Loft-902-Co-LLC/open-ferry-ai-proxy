//! The tools' reshaping, ported from upstream's `xai_executor_test.go`.
//!
//! Upstream's `normalizeXAITools`, `normalizeXAINamespaceToolChoice` and
//! `normalizeXAIInputNamespaceToolCalls` decide themselves whether to fold
//! (`xaiShouldFoldNamespaceTools(body, false)`); [`normalize`],
//! [`choice_normalized`] and [`input_normalized`] do the same.

use serde_json::{Value, json};

use super::*;
use crate::json::{exists, get};

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("valid JSON")
}

/// `normalizeXAITools`.
fn normalize(body: &str) -> Value {
    let mut body = parse(body);
    let fold = should_fold(&body, false);
    normalize_tools(&mut body, fold);
    body
}

/// `normalizeXAINamespaceToolChoice`.
fn choice_normalized(mut body: Value) -> Value {
    let fold = should_fold(&body, false);
    normalize_namespace_tool_choice(&mut body, fold);
    body
}

/// `normalizeXAIInputNamespaceToolCalls`.
fn input_normalized(mut body: Value) -> Value {
    let fold = should_fold(&body, false);
    normalize_input_namespace_tool_calls(&mut body, fold);
    body
}

/// `count` namespaces `mcp__app_<i>`, each with `children` functions
/// `tool_<j>` made by `child`.
fn namespaces(count: usize, children: usize, child: impl Fn(usize) -> String) -> String {
    (0..count)
        .map(|index| {
            let tools: Vec<String> = (0..children).map(&child).collect();
            format!(
                r#"{{"type":"namespace","name":"mcp__app_{index}","tools":[{}]}}"#,
                tools.join(",")
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// A child function with an object schema.
fn plain_child(index: usize) -> String {
    format!(r#"{{"type":"function","name":"tool_{index}","parameters":{{"type":"object"}}}}"#)
}

fn tools_of(body: &Value) -> &Vec<Value> {
    body["tools"].as_array().expect("tools")
}

fn str_at<'v>(body: &'v Value, path: &str) -> &'v str {
    get(body, path).and_then(Value::as_str).unwrap_or_default()
}

// TestEnsureXAINativeXSearchTool.
#[test]
fn ensure_native_x_search_tool() {
    // Missing tools array: a top-level x_search tool is added.
    let mut body = json!({"model": "grok-4.5", "input": "hi"});
    ensure_native_x_search(&mut body);
    assert_eq!(body["tools"], json!([{"type": "x_search"}]));

    // Existing tools without x_search: appended once.
    let mut body = parse(
        r#"{"tools":[{"type":"web_search"},{"type":"function","name":"lookup","parameters":{"type":"object"}}]}"#,
    );
    ensure_native_x_search(&mut body);
    assert_eq!(tools_of(&body).len(), 3);
    assert_eq!(body["tools"][2]["type"], "x_search");

    // Already present: no duplicate.
    let mut body = parse(
        r#"{"tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}},{"type":"x_search"}]}"#,
    );
    let before = body.clone();
    ensure_native_x_search(&mut body);
    assert_eq!(body, before);

    // allowed_tools without x_search: appended once so Grok may pick it.
    let mut body = parse(
        r#"{"tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
            "tool_choice":{"type":"allowed_tools","tools":[{"type":"function","name":"lookup"}]}}"#,
    );
    ensure_native_x_search(&mut body);
    assert_eq!(str_at(&body, "tools.1.type"), "x_search");
    assert_eq!(str_at(&body, "tool_choice.tools.1.type"), "x_search");

    // allowed_tools already lists x_search: no duplicate.
    let mut body = parse(
        r#"{"tools":[{"type":"web_search"},{"type":"x_search"}],
            "tool_choice":{"type":"allowed_tools","tools":[{"type":"web_search"},{"type":"x_search"}]}}"#,
    );
    let before = body.clone();
    ensure_native_x_search(&mut body);
    assert_eq!(body, before);
}

// Not upstream's: an allowed_tools choice without a tools list gets one, and
// X search in an additional_tools item counts as present.
#[test]
fn ensure_native_x_search_edges() {
    let mut body = json!({"tools": [], "tool_choice": {"type": "allowed_tools", "mode": "auto"}});
    ensure_native_x_search(&mut body);
    assert_eq!(
        body,
        json!({"tools": [{"type": "x_search"}], "tool_choice": {"type": "allowed_tools", "mode": "auto", "tools": [{"type": "x_search"}]}})
    );

    let mut body =
        json!({"input": [{"type": "additional_tools", "tools": [{"type": "x_search"}]}]});
    assert!(has_native_x_search(&body));
    ensure_native_x_search(&mut body);
    assert!(body.get("tools").is_none());
}

// TestPruneXAIOrphanedToolChoice.
#[test]
fn prune_orphaned_tool_choice_drops_missing_tools() {
    // A forced choice of a removed tool is dropped.
    let mut body = parse(
        r#"{"tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
            "tool_choice":{"type":"image_generation"}}"#,
    );
    prune_orphaned_tool_choice(&mut body);
    assert!(!exists(&body, "tool_choice"));

    // allowed_tools keeps only what is still there.
    let mut body = parse(
        r#"{"tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}},{"type":"web_search"}],
            "tool_choice":{"type":"allowed_tools","tools":[
                {"type":"function","name":"lookup"},
                {"type":"image_generation"},
                {"type":"web_search"}
            ]}}"#,
    );
    prune_orphaned_tool_choice(&mut body);
    assert_eq!(
        body["tool_choice"]["tools"],
        json!([{"type": "function", "name": "lookup"}, {"type": "web_search"}])
    );

    // With nothing left, the choice goes.
    let mut body = parse(
        r#"{"tools":[],"tool_choice":{"type":"allowed_tools","tools":[{"type":"image_generation"}]}}"#,
    );
    prune_orphaned_tool_choice(&mut body);
    assert!(!exists(&body, "tool_choice"));

    // String choices name no tool.
    let mut body = parse(r#"{"tools":[{"type":"web_search"}],"tool_choice":"auto"}"#);
    let before = body.clone();
    prune_orphaned_tool_choice(&mut body);
    assert_eq!(body, before);
}

// TestXAISupportsNativeImageGeneration.
#[test]
fn supports_native_image_generation_from_grok_4_6() {
    let cases = [
        ("", false),
        ("grok-4.5", false),
        ("grok-4.3", false),
        ("grok-4", false),
        ("grok-4.20-0309-reasoning", false),
        ("grok-4.20-multi-agent-0309", false),
        ("grok-build-0.1", false),
        ("grok-composer-2.5-fast", false),
        ("grok-3-mini", false),
        ("gpt-5.6", false),
        ("grok-4.6", true),
        ("grok-4.6(high)", true),
        ("xai/grok-4.6", true),
        ("grok-4.7", true),
        ("grok-5", true),
        ("grok-5.0", true),
    ];
    for (model, want) in cases {
        assert_eq!(supports_native_image_generation(model), want, "{model:?}");
    }
}

// TestNormalizeXAITools_ImageGenerationByModel.
#[test]
fn image_generation_is_kept_by_model() {
    let cases = [
        // A missing model still strips.
        (
            r#"{"tools":[{"type":"image_generation"},{"type":"web_search"}]}"#,
            false,
        ),
        (
            r#"{"model":"grok-4.5","tools":[{"type":"image_generation"},{"type":"web_search"}]}"#,
            false,
        ),
        // grok-4.20 strips despite the larger minor.
        (
            r#"{"model":"grok-4.20-0309-reasoning","tools":[{"type":"image_generation"},{"type":"web_search"}]}"#,
            false,
        ),
        (
            r#"{"model":"grok-4.6","tools":[{"type":"image_generation","action":"generate"},{"type":"web_search"}]}"#,
            true,
        ),
    ];
    for (body, keep) in cases {
        let out = normalize(body);
        let tools = tools_of(&out);
        assert!(
            tools.iter().any(|tool| tool["type"] == "web_search"),
            "{out}"
        );
        let image = tools.iter().find(|tool| tool["type"] == "image_generation");
        assert_eq!(image.is_some(), keep, "{out}");
        if let Some(image) = image {
            assert_eq!(image["action"], "generate");
        }
    }
}

// TestNormalizeXAITools_SimplifiesCodexAppAutomationUpdateSchema.
#[test]
fn simplifies_codex_app_automation_update_schema() {
    let parameters = format!(
        r##"{{"type":"object","oneOf":[{{"$ref":"#/$defs/__schema0"}},{{"$ref":"#/$defs/__schema3"}}],"$defs":{{"__schema0":{{"type":"object","properties":{{"mode":{{"type":"string"}}}}}},"__schema3":{{"oneOf":[{{"type":"object"}}]}}}},"x":"{}"}}"##,
        "y".repeat(1600)
    );
    let out = normalize(&format!(
        r#"{{"model":"grok-4.5","tools":[{{"type":"namespace","name":"mcp__codex_app","tools":[{{"type":"function","name":"automation_update","description":"sched","strict":true,"parameters":{parameters}}}]}},{{"type":"function","name":"exec_command","parameters":{{"type":"object","properties":{{"cmd":{{"type":"string"}}}}}}}}]}}"#
    ));
    let tools = tools_of(&out);
    let auto = tools
        .iter()
        .find(|tool| tool["name"] == "mcp__codex_app__automation_update")
        .expect("automation_update kept");
    assert_eq!(auto["parameters"], schema::safe_function_parameters());
    assert_eq!(auto["strict"], false);
    let exec = tools
        .iter()
        .find(|tool| tool["name"] == "exec_command")
        .expect("exec_command kept");
    assert_eq!(exec["parameters"]["properties"]["cmd"]["type"], "string");
}

// TestNormalizeXAITools_SimplifiesFlattenedAndInvalidRootSchemas.
#[test]
fn simplifies_flattened_and_invalid_root_schemas() {
    let out = normalize(
        r#"{"tools":[
            {"type":"function","name":"codex_app__automation_update","strict":true,"parameters":{"oneOf":[{"type":"object","properties":{"action":{"type":"string"}},"required":["action"]},{"type":"null"}]}},
            {"type":"function","name":"nullable_lookup","strict":true,"parameters":{"anyOf":[{"type":"object","properties":{"query":{"type":"string"}}},{"type":["object","null"]}]}},
            {"type":"custom","name":"nullable_custom","strict":true,"parameters":{"oneOf":[{"type":"object"},{"type":"null"}]}},
            {"type":"function","name":"mixed_nullable","strict":true,"parameters":{"type":"object","oneOf":[{"required":["query"]},{"type":"null"}],"properties":{"query":{"type":"string"}}}},
            {"type":"function","name":"array_root_union","strict":true,"parameters":{"type":["object"],"anyOf":[{"required":["query"]},{"required":["id"]}],"properties":{"query":{"type":"string"},"id":{"type":"integer"}}}},
            {"type":"function","name":"echo_tool","strict":true,"parameters":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"],"additionalProperties":false}}
        ]}"#,
    );
    let tools = tools_of(&out);
    assert_eq!(tools.len(), 6, "{out}");
    let simplified = [
        "codex_app__automation_update",
        "nullable_lookup",
        "nullable_custom",
        "mixed_nullable",
        "array_root_union",
    ];
    for (tool, name) in tools.iter().zip(simplified) {
        assert_eq!(tool["name"], name);
        assert_eq!(tool["type"], "function", "{name}");
        assert_eq!(
            tool["parameters"],
            schema::safe_function_parameters(),
            "{name}"
        );
        assert_eq!(tool["strict"], false, "{name}");
    }
    let echo = &tools[5];
    assert_eq!(
        echo["parameters"]["properties"]["message"]["type"],
        "string"
    );
    assert_eq!(echo["strict"], true);
    assert_eq!(echo["parameters"]["additionalProperties"], false);
}

// TestNormalizeXAITools_InlinesLocalRefs.
#[test]
fn inlines_local_refs() {
    let out = normalize(
        r##"{"tools":[
            {"type":"function","name":"query_user","strict":true,"parameters":{
                "type":"object",
                "properties":{"user":{"$ref":"#/$defs/User"}},
                "required":["user"],
                "$defs":{"User":{"type":"object","properties":{"name":{"type":"string"},"age":{"type":"integer"}},"required":["name"]}}
            }},
            {"type":"function","name":"render_shape","strict":true,"parameters":{
                "type":"object",
                "oneOf":[{"$ref":"#/$defs/Circle"},{"$ref":"#/$defs/Square"}],
                "$defs":{
                    "Circle":{"type":"object","properties":{"radius":{"type":"number"}},"required":["radius"]},
                    "Square":{"type":"object","properties":{"side":{"type":"number"}},"required":["side"]}
                }
            }}
        ]}"##,
    );
    let tools = tools_of(&out);
    assert_eq!(tools.len(), 2);
    let user = &tools[0]["parameters"];
    assert_eq!(
        user["properties"]["user"]["properties"]["name"]["type"],
        "string"
    );
    assert_eq!(
        user["properties"]["user"]["properties"]["age"]["type"],
        "integer"
    );
    assert!(user.get("$defs").is_none(), "{user}");
    let shape = &tools[1]["parameters"];
    assert_eq!(shape["oneOf"].as_array().map(Vec::len), Some(2));
    assert_eq!(shape["oneOf"][0]["properties"]["radius"]["type"], "number");
    assert_eq!(shape["oneOf"][1]["properties"]["side"]["type"], "number");
    assert!(shape.get("$defs").is_none(), "{shape}");
}

// TestNormalizeXAITools_AddsObjectTypeToRootUnionBranches.
#[test]
fn adds_object_type_to_root_union_branches() {
    let out = normalize(
        r#"{"tools":[
            {"type":"function","name":"crop_around_point","strict":true,"parameters":{
                "type":"object","additionalProperties":false,"required":["imagePath","point"],
                "oneOf":[{"required":["radius"],"not":{"required":["size"]}},{"required":["size"],"not":{"required":["radius"]}}],
                "properties":{"imagePath":{"type":"string"},"point":{"type":"array"},"radius":{"type":"number"},"size":{"type":"object"},
                    "nested":{"oneOf":[{"required":["value"]},{}]}}
            }},
            {"type":"function","name":"lookup","strict":true,"parameters":{
                "type":"object","anyOf":[{"required":["query"]},{"required":["id"]}],
                "properties":{"query":{"type":"string"},"id":{"type":"integer"}}
            }},
            {"type":"custom","name":"custom_lookup","strict":true,"parameters":{
                "type":"object","oneOf":[{"required":["query"]},{"required":["id"]}],
                "properties":{"query":{"type":"string"},"id":{"type":"integer"}}
            }}
        ]}"#,
    );
    for (index, union) in [(0, "oneOf"), (1, "anyOf"), (2, "oneOf")] {
        let tool = &out["tools"][index];
        let branches = tool["parameters"][union].as_array().expect("branches");
        assert_eq!(branches.len(), 2);
        for branch in branches {
            assert_eq!(branch["type"], "object", "{out}");
        }
        assert_eq!(tool["strict"], true, "{out}");
    }
    let crop = &out["tools"][0]["parameters"];
    assert_eq!(crop["additionalProperties"], false);
    assert_eq!(crop["required"].as_array().map(Vec::len), Some(2));
    assert!(exists(crop, "oneOf.0.not.required") && exists(crop, "oneOf.1.not.required"));
    assert!(
        !exists(crop, "properties.nested.oneOf.0.type"),
        "a nested union is left alone"
    );
    assert_eq!(out["tools"][2]["type"], "function");
}

// TestNormalizeXAITools_QualifiesSameNamedNamespaceTools.
#[test]
fn qualifies_same_named_namespace_tools() {
    let out = normalize(
        r#"{"tools":[
            {"type":"namespace","name":"mcp__exa","tools":[{"type":"function","name":"search","parameters":{"type":"object"}}]},
            {"type":"namespace","name":"mcp__docs","tools":[{"type":"function","name":"search","parameters":{"type":"object"}}]}
        ]}"#,
    );
    let names: Vec<&Value> = tools_of(&out).iter().map(|tool| &tool["name"]).collect();
    assert_eq!(
        names,
        [&json!("mcp__exa__search"), &json!("mcp__docs__search")]
    );
}

// TestNormalizeXAITools_WhenFlattenedCountExceedsLimit_FoldsNamespaces, the
// request half; restoring the dispatcher's call is in the response's tests.
#[test]
fn folds_namespaces_when_flattened_count_exceeds_limit() {
    // 47 namespaces of 10 tools: 470, over the 200 xAI takes.
    let list = namespaces(47, 10, |index| {
        format!(
            r#"{{"type":"function","name":"tool_{index}","description":"child tool {index}","parameters":{{"type":"object","properties":{{"p":{{"type":"string"}}}}}}}}"#
        )
    });
    let body = parse(&format!(r#"{{"tools":[{list}]}}"#));
    let refs = collect_namespace_refs(&body, should_fold(&body, false));
    assert!(refs["mcp__app_0"].is_dispatcher);
    assert_eq!(refs["mcp__app_0__tool_3"].name, "tool_3");

    let out = normalize(&body.to_string());
    let tools = tools_of(&out);
    assert_eq!(tools.len(), 47);
    assert_eq!(tools[0]["name"], "mcp__app_0");
    let description = tools[0]["description"].as_str().expect("description");
    assert!(description.contains("Parameters:"), "{description}");
    assert!(
        description.contains(r#""properties":{"p":{"type":"string"}}"#),
        "{description}"
    );

    // The next turn's call of a child goes to the dispatcher.
    let turn = parse(&format!(
        r#"{{"tools":[{list}],"input":[{{"type":"function_call","name":"tool_3","namespace":"mcp__app_0","call_id":"call_1","arguments":"{{\"p\":\"val\"}}"}}]}}"#
    ));
    let turn = input_normalized(turn);
    assert_eq!(turn["input"][0]["name"], "mcp__app_0");
    assert!(!exists(&turn, "input.0.namespace"));
    assert_eq!(
        turn["input"][0]["arguments"],
        r#"{"arguments":{"p":"val"},"name":"tool_3"}"#
    );
}

// Not upstream's: a dispatcher's description lists each child as upstream
// writes it, and its parameters are written with sorted keys.
#[test]
fn dispatcher_lists_children() {
    let namespace = parse(
        r##"{"type":"namespace","name":"mcp__docs","description":"Docs.","tools":[
            {"type":"function","name":"read","description":"Reads.","parameters":{"type":"object","properties":{"id":{"$ref":"#/$defs/Id"}},"$defs":{"Id":{"type":"string"}}}},
            {"type":"function","name":"list","description":"Lists.","parameters":{}},
            {"type":"function","name":"find","input_schema":{"type":"object","properties":{"q":{"type":"string"}}}},
            {"type":"function","name":"ping","parameters":{"type":"object","properties":{}}},
            {"type":"function","name":"  "}
        ]}"##,
    );
    let tool = dispatcher(&namespace).expect("dispatcher");
    assert_eq!(
        tool["description"],
        "Docs.\n\nAvailable tools in this namespace:\n\
         - read: Reads.\n  Parameters: {\"properties\":{\"id\":{\"type\":\"string\"}},\"type\":\"object\"}\n\
         - list: Lists.\n\
         - find\n  Parameters: {\"type\":\"object\",\"properties\":{\"q\":{\"type\":\"string\"}}}\n\
         - ping"
    );
    assert_eq!(
        tool.to_string(),
        format!(
            r#"{{"description":{},"name":"mcp__docs","parameters":{{"properties":{{"arguments":{{"additionalProperties":true,"description":"Arguments object matching the parameter schema of the selected child tool","type":"object"}},"name":{{"description":"Child tool name to execute in namespace mcp__docs","enum":["read","list","find","ping"],"type":"string"}}}},"required":["name"],"type":"object"}},"type":"function"}}"#,
            tool["description"]
        )
    );

    let empty = dispatcher(&json!({"type": "namespace", "name": "bare"})).expect("dispatcher");
    assert_eq!(empty["description"], "Tools in namespace bare.");
    assert!(!exists(&empty, "parameters.properties.name.enum"));
    let undescribed =
        dispatcher(&json!({"name": "ns", "tools": [{"name": "a"}]})).expect("dispatcher");
    assert_eq!(
        undescribed["description"],
        "Tools in namespace ns.\n\nAvailable tools in this namespace:\n- a"
    );
    assert_eq!(dispatcher(&json!({"type": "namespace", "name": " "})), None);
}

// TestNormalizeXAINamespaceToolChoice_WhenFolding.
#[test]
fn namespace_tool_choice_when_folding() {
    let list = namespaces(25, 10, plain_child);
    let out = normalize(&format!(
        r#"{{"tools":[{list}],"tool_choice":{{"type":"function","name":"tool_3","namespace":"mcp__app_0"}}}}"#
    ));
    let out = choice_normalized(out);
    assert_eq!(out["tool_choice"]["name"], "mcp__app_0");
    assert!(!exists(&out, "tool_choice.namespace"));
}

// TestNormalizeXAIInputNamespaceToolCalls_PreservesLargeInts.
#[test]
fn input_namespace_tool_calls_preserve_large_ints() {
    let list = namespaces(25, 10, plain_child);
    let body = parse(&format!(
        r#"{{"tools":[{list}],"input":[{{"type":"function_call","name":"tool_1","namespace":"mcp__app_0","call_id":"c1","arguments":"{{\"large_id\":9223372036854775807}}"}}]}}"#
    ));
    let out = input_normalized(body);
    let arguments = out["input"][0]["arguments"].as_str().expect("arguments");
    assert!(arguments.contains("9223372036854775807"), "{arguments}");
}

// Not upstream's: arguments that aren't JSON go to the dispatcher as a
// string, and empty ones not at all.
#[test]
fn dispatcher_arguments_keep_text_that_isnt_json() {
    assert_eq!(
        dispatcher_arguments("t".into(), "not json"),
        r#"{"arguments":"not json","name":"t"}"#
    );
    assert_eq!(dispatcher_arguments("t".into(), ""), r#"{"name":"t"}"#);
}

// TestClampXAIToolsLimit_PreservesDispatchersOverRegularTools.
#[test]
fn clamp_keeps_dispatchers_over_regular_tools() {
    let mut refs = NamespaceRefs::new();
    let mut tools = Vec::new();
    for index in 0..47 {
        let name = format!("mcp__app_{index}");
        refs.insert(
            name.clone(),
            NamespaceRef {
                namespace: name.clone(),
                name: String::new(),
                is_dispatcher: true,
            },
        );
        tools.push(json!({"type": "function", "name": name, "parameters": {"type": "object"}}));
    }
    for index in 0..180 {
        tools.push(json!({"type": "function", "name": format!("plain_fn_{index}"), "parameters": {"type": "object"}}));
    }
    // Upstream lists the dispatchers first; here they come last, so the
    // clamp has to move them.
    tools.rotate_left(47);
    let mut body = json!({"tools": tools});
    clamp_tools(&mut body, MAX_TOOLS, &refs);
    let tools = tools_of(&body);
    assert_eq!(tools.len(), 200);
    for index in 0..47 {
        let name = format!("mcp__app_{index}");
        assert!(
            tools.iter().any(|tool| tool["name"] == name.as_str()),
            "{name} dropped"
        );
    }
}

// TestPromoteXAIAdditionalTools.
#[test]
fn promotes_additional_tools() {
    let mut out = normalize(
        r#"{"tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
            "input":[
                {"type":"additional_tools","role":"developer","tools":[{"type":"namespace","name":"mcp__exa","tools":[{"type":"function","name":"search","parameters":{"type":"object"}}]}]},
                {"role":"user","content":"hello"},
                {"type":"additional_tools","role":"developer","tools":[{"type":"custom","name":"custom_lookup"}]}
            ]}"#,
    );
    promote_additional_tools(&mut out);
    assert_eq!(out["input"], json!([{"role": "user", "content": "hello"}]));
    let tools = tools_of(&out);
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0]["name"], "lookup");
    assert_eq!(tools[1]["name"], "mcp__exa__search");
    assert_eq!(tools[2]["name"], "custom_lookup");
    assert_eq!(tools[2]["type"], "function");
    assert!(tools[2].get("parameters").is_some());
}

// TestNormalizeXAINamespaceToolChoice.
#[test]
fn namespace_tool_choice() {
    let out = normalize(
        r#"{"tools":[{"type":"namespace","name":"mcp__exa","tools":[{"type":"function","name":"search","parameters":{"type":"object"}}]}],
            "tool_choice":{"type":"function","name":"search","namespace":"mcp__exa"}}"#,
    );
    let out = choice_normalized(out);
    assert_eq!(out["tools"][0]["name"], "mcp__exa__search");
    assert_eq!(out["tool_choice"]["name"], "mcp__exa__search");
    assert!(!exists(&out, "tool_choice.namespace"));
}

// TestNormalizeXAINamespaceToolChoiceAllowedTools.
#[test]
fn namespace_tool_choice_allowed_tools() {
    let out = choice_normalized(parse(
        r#"{"tool_choice":{"type":"allowed_tools","tools":[
            {"type":"function","name":"search","namespace":"mcp__exa"},
            {"type":"function","name":"collaboration__send_message","namespace":"collaboration"},
            {"type":"function","name":"lookup"},
            {"type":"web_search","namespace":"ignored"}
        ]}}"#,
    ));
    assert_eq!(
        out["tool_choice"]["tools"],
        json!([
            {"type": "function", "name": "mcp__exa__search"},
            {"type": "function", "name": "collaboration__send_message"},
            {"type": "function", "name": "lookup"},
            {"type": "web_search", "namespace": "ignored"}
        ])
    );
}

// TestNormalizeXAINamespaceToolChoice_PreservesOtherChoices. Upstream's
// malformed-payload case is dropped: the body here is always parsed JSON.
#[test]
fn namespace_tool_choice_leaves_other_choices() {
    for text in [
        r#"{"tool_choice":"auto"}"#,
        r#"{"tool_choice":{"type":"function","name":"search"}}"#,
        r#"{"tool_choice":{"type":"web_search","name":"search","namespace":"mcp__exa"}}"#,
    ] {
        assert_eq!(choice_normalized(parse(text)), parse(text), "{text}");
    }
}

// TestQualifyXAINamespaceToolNamePreservesQualifiedNames.
#[test]
fn qualify_keeps_qualified_names() {
    let cases = [
        ("mcp__exa", "search", "mcp__exa__search"),
        ("mcp__exa", "mcp__exa__search", "mcp__exa__search"),
        (
            "collaboration",
            "collaboration__send_message",
            "collaboration__send_message",
        ),
        (
            "collaboration__",
            "send_message",
            "collaboration__send_message",
        ),
        ("exa", "example_tool", "exa__example_tool"),
    ];
    for (namespace, tool, want) in cases {
        assert_eq!(qualify(namespace, tool), want, "{namespace} {tool}");
    }
}

// TestNormalizeXAITools_PreservesUnrelatedSchemas.
#[test]
fn leaves_unrelated_schemas() {
    let strict_cron = r#"{"type":"object","properties":{"cron":{"type":"string"}},"required":["cron"],"additionalProperties":false}"#;
    let cases = [
        // A top-level automation_update.
        format!(
            r#"{{"tools":[{{"type":"function","name":"automation_update","strict":true,"parameters":{strict_cron}}}]}}"#
        ),
        // automation_update in another namespace.
        format!(
            r#"{{"tools":[{{"type":"namespace","name":"calendar","tools":[{{"type":"function","name":"automation_update","strict":true,"parameters":{strict_cron}}}]}}]}}"#
        ),
        // A custom automation_update in codex_app.
        format!(
            r#"{{"tools":[{{"type":"namespace","name":"codex_app","tools":[{{"type":"custom","name":"automation_update","strict":true,"parameters":{strict_cron}}}]}}]}}"#
        ),
    ];
    for body in &cases {
        let out = normalize(body);
        let tool = &out["tools"][0];
        assert_eq!(tool["strict"], true, "{out}");
        assert_eq!(
            tool["parameters"]["properties"]["cron"]["type"], "string",
            "{out}"
        );
        assert_eq!(tool["parameters"]["additionalProperties"], false, "{out}");
    }

    // A large schema on another codex_app function.
    let large = format!(
        r#"{{"oneOf":[{{"type":"object","properties":{{"mode":{{"type":"string"}}}}}}],"$defs":{{"a":{{"type":"string"}}}},"x":"{}"}}"#,
        "y".repeat(1600)
    );
    let out = normalize(&format!(
        r#"{{"tools":[{{"type":"namespace","name":"codex_app","tools":[{{"type":"function","name":"exec_command","strict":true,"parameters":{large}}}]}}]}}"#
    ));
    let tool = &out["tools"][0];
    assert_eq!(tool["strict"], true);
    assert!(
        exists(tool, "parameters.oneOf") && exists(tool, "parameters.$defs"),
        "{out}"
    );
}

// TestNormalizeXAIInputNamespaceToolCalls.
#[test]
fn input_namespace_tool_calls() {
    let out = input_normalized(parse(
        r#"{"input":[
            {"type":"function_call","name":"web_search_exa","namespace":"mcp__exa","call_id":"call_1","arguments":"{}"},
            {"type":"function_call","name":"plain_tool","call_id":"call_2","arguments":"{}"}
        ]}"#,
    ));
    assert_eq!(out["input"][0]["name"], "mcp__exa__web_search_exa");
    assert!(!exists(&out, "input.0.namespace"));
    assert_eq!(out["input"][1]["name"], "plain_tool");
}

// Not upstream's: a call of a namespace the request declares folded (a
// function named as the namespace) goes to that dispatcher even when this
// request isn't folding.
#[test]
fn input_namespace_tool_calls_follow_declared_dispatchers() {
    let out = input_normalized(parse(
        r#"{"tools":[{"type":"function","name":"mcp__exa"}],
            "input":[{"type":"function_call","name":"search","namespace":"mcp__exa","call_id":"c","arguments":"{\"q\":1}"}]}"#,
    ));
    assert_eq!(out["input"][0]["name"], "mcp__exa");
    assert_eq!(
        out["input"][0]["arguments"],
        r#"{"arguments":{"q":1},"name":"search"}"#
    );
}

// Not upstream's (upstream tests this through Execute,
// TestXAIExecutorExecuteNormalizesCustomToolCallHistory, ported in the
// request's tests): each form of a custom call's input and output.
#[test]
fn custom_tool_call_arguments_and_outputs() {
    assert_eq!(custom_tool_call_arguments(None), "{}");
    assert_eq!(
        custom_tool_call_arguments(Some(&json!(" {\"a\":1} "))),
        r#"{"a":1}"#
    );
    assert_eq!(
        custom_tool_call_arguments(Some(&json!("[1]"))),
        r#"{"input":"[1]"}"#
    );
    assert_eq!(
        custom_tool_call_arguments(Some(&json!("a <b>"))),
        format!(r#"{{"input":"a {bs}u003cb{bs}u003e"}}"#, bs = '\\')
    );
    assert_eq!(
        custom_tool_call_arguments(Some(&json!({"a": 1}))),
        r#"{"a":1}"#
    );
    assert_eq!(
        custom_tool_call_arguments(Some(&json!(7))),
        r#"{"input":7}"#
    );
    assert_eq!(
        custom_tool_call_arguments(Some(&Value::Null)),
        r#"{"input":null}"#
    );
    assert_eq!(custom_tool_call_output(None), "");
    assert_eq!(custom_tool_call_output(Some(&json!("done"))), "done");
    assert_eq!(custom_tool_call_output(Some(&json!([1]))), "[1]");
}

// TestNormalizeXAIToolChoiceForTools_DropsWhenToolsEmpty.
#[test]
fn tool_choice_for_tools_drops_when_tools_empty() {
    let mut body = parse(
        r#"{"model":"grok-4","tools":[],"tool_choice":"auto","parallel_tool_calls":true,"input":"hi"}"#,
    );
    normalize_tool_choice_for_tools(&mut body);
    assert_eq!(body, json!({"model": "grok-4", "input": "hi"}));
}

// TestNormalizeXAIToolChoiceForTools_DropsWhenToolsMissing.
#[test]
fn tool_choice_for_tools_drops_when_tools_missing() {
    let mut body = parse(r#"{"model":"grok-4","tool_choice":"auto","input":"hi"}"#);
    normalize_tool_choice_for_tools(&mut body);
    assert!(!exists(&body, "tool_choice"));
}

// TestNormalizeXAIToolChoiceForTools_DropsOrphanedParallelToolCalls.
#[test]
fn tool_choice_for_tools_drops_orphaned_parallel_tool_calls() {
    let mut body = parse(r#"{"model":"grok-4","parallel_tool_calls":true,"input":"hi"}"#);
    normalize_tool_choice_for_tools(&mut body);
    assert!(!exists(&body, "parallel_tool_calls"));
}

// TestNormalizeXAIToolChoiceForTools_KeepsWhenToolsPresent.
#[test]
fn tool_choice_for_tools_keeps_when_tools_present() {
    let text = r#"{"model":"grok-4","tools":[{"type":"function","name":"Bash"}],"tool_choice":"auto","input":"hi"}"#;
    let mut body = parse(text);
    normalize_tool_choice_for_tools(&mut body);
    assert_eq!(body, parse(text));
}

// TestNormalizeXAIToolChoiceForTools_KeepsWhenAdditionalToolsPresent.
#[test]
fn tool_choice_for_tools_keeps_when_additional_tools_present() {
    let text = r#"{"model":"grok-4","input":[{"type":"additional_tools","tools":[{"type":"function","name":"Bash"}]}],"tool_choice":"auto","parallel_tool_calls":true}"#;
    let mut body = parse(text);
    normalize_tool_choice_for_tools(&mut body);
    assert_eq!(body, parse(text));
}

// TestNormalizeXAIToolChoiceForTools_NoOpWhenBothAbsent.
#[test]
fn tool_choice_for_tools_no_op_when_both_absent() {
    let mut body = parse(r#"{"model":"grok-4","input":"hi"}"#);
    normalize_tool_choice_for_tools(&mut body);
    assert_eq!(body, json!({"model": "grok-4", "input": "hi"}));
}

// Not upstream's: a namespace function without a usable name leaves every
// tool list as it was, as upstream's failed normalization does.
#[test]
fn unnamed_namespace_function_changes_nothing() {
    let text = r#"{"tools":[{"type":"custom","name":"a"},{"type":"namespace","name":"ns","tools":[{"type":"function","name":" "}]}]}"#;
    assert_eq!(normalize(text), parse(text));
}

// Not upstream's: client declared tools are keyed as the client knows them,
// with custom tools as functions.
#[test]
fn client_declared_tool_keys() {
    let body = parse(
        r#"{"tools":[
            {"type":"function","name":"lookup"},
            {"type":"custom","name":" apply_patch "},
            {"type":"web_search"},
            {"type":"namespace","name":"mcp__exa","tools":[{"type":"custom","name":"search"},{"type":"x_search","name":"x"}]}
        ],
        "input":[{"type":"additional_tools","tools":[{"type":"function","name":"extra"}]}]}"#,
    );
    let key = |namespace: &str, name: &str| ClientToolKey {
        namespace: namespace.into(),
        name: name.into(),
        tool_type: FUNCTION.into(),
    };
    let want: HashSet<ClientToolKey> = [
        key("", "lookup"),
        key("", "apply_patch"),
        key("mcp__exa", "search"),
        key("", "extra"),
    ]
    .into_iter()
    .collect();
    assert_eq!(collect_client_declared_tool_keys(&body), want);
}
