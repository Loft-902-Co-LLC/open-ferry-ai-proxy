//! Ported from CLIProxyAPI internal/client/codex/optimize-multi-agent-v2/
//! optimize_multi_agent_v2_test.go (v8.0.20, MIT).
//!
//! Moved: `TestCodexSpawnAgentModelsFromSourcesIncludesModelMetadata` and
//! `TestCodexSpawnAgentModelsCacheInvalidation`, which test how the model
//! list is made, are ported in open-ferry-core's
//! `codex_models/spawn_agent/tests.rs`, and
//! `TestTranslateRequestWithCodexMultiAgentV2Conditions` with the
//! translation hook, in open-ferry-providers' `codex/compat/tests.rs`.
//!
//! Dropped:
//! - `TestDecodeCodexHomeAvailableModels`: the Home service isn't ported.
//! - `TestCodexClientUserAgentPrefersGinRequest`: there is no Gin context;
//!   callers pass the client's own `User-Agent`.
//!
//! Changed:
//! - Tests of `rewriteCodexSpawnAgentDescription`, which upstream gives a
//!   model list, go through [`prepare_tools`] with that list written by
//!   [`format_spawn_agent_models`]. That also drops the `encrypted` marks
//!   of `send_message` and `followup_task`, which upstream's helper leaves.
//! - `TestRewriteCodexSpawnAgentDescriptionEnabledOptimizesTool` passes the
//!   list its registered model would give, as the model registry is in
//!   open-ferry-core.
//! - `TestOptimizeCodexMultiAgentV2RequestSkipsPreparedToolRefresh` has no
//!   prepared marker to set; it checks that an old model list in the
//!   description is kept when there are no models to list, and replaced
//!   when there are. Its description's line breaks are written as escapes,
//!   as raw ones aren't valid JSON.
//! - Tests of upstream's private path helpers (`codexSpawnAgentToolPaths`,
//!   `codexCollaborationMessageToolPaths`) check the rewrite those paths
//!   feed instead.
//!
//! Added: the restored output's encoding (sorted keys, Go's escapes, number
//! text), invalid UTF-8, lone surrogates, repeated keys and the nesting
//! limit, the data restore leaves alone, and key order in rewritten
//! requests; the model list's exact text, where it goes, and when it is
//! made.

use serde_json::Value;

use super::*;

const DESKTOP: &str = "Codex Desktop/0.146.0-alpha.3";
const TUI: &str = "codex-tui/0.154.0";
const CLI: &str = "codex_cli_rs/0.144.1";

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// A gjson-style dotted path, with array indexes.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Array(items) => key.parse::<usize>().ok().and_then(|index| items.get(index)),
        _ => value.get(key),
    })
}

fn text_at(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

fn restored(text: &str) -> Value {
    let out = restore_response(text.as_bytes(), true);
    serde_json::from_slice(&out).unwrap()
}

#[test]
fn is_codex_client_user_agent_cases() {
    for (user_agent, want) in [
        (
            "Codex Desktop/0.146.0-alpha.3 (Mac OS 26.5.2; arm64) unknown (Codex Desktop; 26.721.30844)",
            true,
        ),
        (
            "codex-tui/0.154.0 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.154.0)",
            true,
        ),
        (
            "codex_cli_rs/0.144.1 (Mac OS 26.3.1; arm64) iTerm.app/3.6.9",
            true,
        ),
        ("codex_cli_rs", true),
        (
            "codex_exec/0.153.2 (Mac OS 26.6.2; arm64) unknown (codex_exec; 0.153.2)",
            true,
        ),
        ("  codex-tui/0.154.0  ", true),
        ("curl/8.7.1", false),
        ("proxy Codex Desktop/0.146.0", false),
        ("codex_cli_rs_other", false),
    ] {
        assert_eq!(is_codex_client_user_agent(user_agent), want, "{user_agent}");
    }
}

#[test]
fn spawn_agent_tools_ignore_invalid_containers() {
    let text = r#"{
        "input":[{"type":"message","tools":[{"type":"function","name":"spawn_agent","description":"message"}]}],
        "tools":[
            {"type":"function","name":"wrapper","tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]}]},
            {"type":"custom","name":"spawn_agent","description":"custom"},
            {"type":"namespace","name":"spawn_agent","description":"namespace"}
        ]
    }"#;
    let mut body = json(text);
    assert!(!optimize(&mut body, TUI, true, String::new));
    assert_eq!(body, json(text));
}

#[test]
fn optimize_skips_namespace_conflict() {
    let text = r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]},{"type":"namespace","name":"collaboration-optimize","tools":[]}]}"#;
    let mut body = json(text);
    assert!(!optimize(&mut body, TUI, true, String::new));
    assert_eq!(body, json(text));
    assert!(has_namespace_conflict(&body));
}

#[test]
fn optimize_skips_dot_prefix_conflict() {
    let text = r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]},{"type":"function","name":"collaboration-optimize.tool"}]}"#;
    let mut body = json(text);
    assert!(!optimize(&mut body, TUI, true, String::new));
    assert_eq!(body, json(text));
}

#[test]
fn conflicts_are_found_in_namespaces_and_additional_tools() {
    for text in [
        r#"{"tools":[{"type":"function","name":" collaboration-optimize__tool "}]}"#,
        r#"{"tools":[{"type":" namespace ","name":"other","tools":[{"type":"function","name":"collaboration-optimize"}]}]}"#,
        r#"{"input":[{"type":"additional_tools","tools":[{"type":"function","name":"collaboration-optimize.x"}]}]}"#,
    ] {
        assert!(has_namespace_conflict(&json(text)), "{text}");
    }
    for text in [
        r#"{"tools":[{"type":"function","name":"collaboration_optimize"}]}"#,
        r#"{"tools":[{"type":"function","name":"other","tools":[{"type":"function","name":"collaboration-optimize"}]}]}"#,
        r#"{"input":[{"type":"message","tools":[{"type":"function","name":"collaboration-optimize"}]}]}"#,
    ] {
        assert!(!has_namespace_conflict(&json(text)), "{text}");
    }
}

#[test]
fn optimize_renames_collaboration_namespace_without_models() {
    let mut body = json(
        r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]}]}"#,
    );
    assert!(optimize(&mut body, DESKTOP, true, String::new));
    assert_eq!(
        text_at(&body, "tools.0.name"),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
    // The namespace keeps its key order.
    assert_eq!(
        body.to_string(),
        r#"{"tools":[{"type":"namespace","name":"collaboration-optimize","tools":[{"type":"function","name":"spawn_agent"}]}]}"#
    );
}

#[test]
fn prepare_without_models_still_removes_encrypted() {
    let mut body = json(
        r#"{"tools":[{"type":"function","name":"spawn_agent","description":"unchanged","parameters":{"properties":{"message":{"encrypted":true}}}}]}"#,
    );
    assert!(prepare_tools(&mut body, TUI, true, String::new));
    assert_eq!(text_at(&body, "tools.0.description"), "unchanged");
    assert!(at(&body, "tools.0.parameters.properties.message.encrypted").is_none());
}

// TestRewriteCodexSpawnAgentDescriptionLeavesPayloadWithoutToolUnchanged
#[test]
fn prepare_leaves_payload_without_tool_unchanged() {
    let text = r#"{"tools":[{"type":"function","name":"other","description":"unchanged"}]}"#;
    let mut body = json(text);
    let models = [model("model-a", "Model A.", &[], "")];
    let mut made = false;
    let list = || {
        made = true;
        format_spawn_agent_models(&models)
    };
    assert!(!prepare_tools(&mut body, TUI, true, list));
    assert_eq!(body, json(text));
    // The list isn't made without a `spawn_agent` tool to put it in.
    assert!(!made);
}

/// A model for `spawn_agent` to pick, without service tiers.
fn model(id: &str, description: &str, efforts: &[&str], default: &str) -> SpawnAgentModel {
    SpawnAgentModel {
        id: id.into(),
        description: description.into(),
        reasoning_efforts: efforts.iter().map(|&effort| effort.to_owned()).collect(),
        default_reasoning_effort: default.into(),
        ..SpawnAgentModel::default()
    }
}

// TestRewriteCodexSpawnAgentDescriptionNormalizesModelList
#[test]
fn prepare_normalizes_the_model_list() {
    let mut body = json(
        r#"{
        "input":[{
            "type":"additional_tools",
            "role":"developer",
            "tools":[{
                "type":"namespace",
                "name":"collaboration",
                "tools":[
                    {"type":"function","name":"send_message","description":"unchanged"},
                    {"type":"function","name":"spawn_agent","description":"\n        Available model overrides (optional; inherited parent model is preferred):\n- old duplicate\n- old duplicate\n        Spawns an agent to work on a task.","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
                ]
            }]
        }]
    }"#,
    );
    let models = [
        SpawnAgentModel {
            service_tiers: vec!["priority".into()],
            ..model(
                "model-alpha",
                "Alpha model.",
                &["low", "medium", "high"],
                "medium",
            )
        },
        model("model-beta", "Beta model", &["low", "high"], "low"),
    ];
    assert!(prepare_tools(&mut body, TUI, true, || {
        format_spawn_agent_models(&models)
    }));
    let description = text_at(&body, "input.0.tools.0.tools.1.description");
    let want_alpha = "- `model-alpha`: Alpha model. Reasoning efforts: low, medium (default), high. Service tiers: priority.";
    let want_beta = "- `model-beta`: Beta model. Reasoning efforts: low (default), high.";
    assert!(description.contains(want_alpha), "{description}");
    assert!(description.contains(want_beta), "{description}");
    assert!(!description.contains("old duplicate"), "{description}");
    for id in ["model-alpha", "model-beta"] {
        assert_eq!(description.matches(&format!("`{id}`")).count(), 1, "{id}");
    }
    assert!(
        description.find("`model-beta`").unwrap()
            < description.find(SPAWN_AGENT_DESCRIPTION_MARKER).unwrap()
    );
    // The whole description, the heading's indent kept.
    assert_eq!(
        description,
        format!(
            "\n        {SPAWN_AGENT_MODELS_HEADING}\n{want_alpha}\n{want_beta}\n        Spawns an agent to work on a task."
        )
    );
    assert_eq!(
        text_at(&body, "input.0.tools.0.tools.0.description"),
        "unchanged"
    );
    assert!(
        at(
            &body,
            "input.0.tools.0.tools.1.parameters.properties.message.encrypted"
        )
        .is_none()
    );
}

// TestRewriteCodexSpawnAgentDescriptionTopLevelWithoutMarker
#[test]
fn prepare_appends_the_list_without_the_marker() {
    let mut body = json(
        r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Create a worker."}]}]}"#,
    );
    let models = [model("model-a", "Model A.", &["medium"], "medium")];
    assert!(prepare_tools(&mut body, TUI, true, || {
        format_spawn_agent_models(&models)
    }));
    assert_eq!(
        text_at(&body, "tools.0.tools.0.description"),
        format!(
            "Create a worker.\n\n{SPAWN_AGENT_MODELS_HEADING}\n- `model-a`: Model A. Reasoning efforts: medium (default)."
        )
    );
}

#[test]
fn prepare_makes_the_list_once_and_only_when_needed() {
    let text = r#"{
        "tools":[
            {"type":"function","name":"spawn_agent","description":"Spawns an agent."},
            {"type":"function","name":"spawn_agent","description":7},
            {"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent."}]}
        ]
    }"#;
    let mut body = json(text);
    let mut calls = 0;
    assert!(prepare_tools(&mut body, TUI, true, || {
        calls += 1;
        "- `m`: M.".to_owned()
    }));
    assert_eq!(calls, 1);
    let want = format!("{SPAWN_AGENT_MODELS_HEADING}\n- `m`: M.\nSpawns an agent.");
    assert_eq!(text_at(&body, "tools.0.description"), want);
    assert_eq!(at(&body, "tools.1.description"), Some(&Value::from(7)));
    assert_eq!(text_at(&body, "tools.2.tools.0.description"), want);

    // Preparing again with the same list changes nothing.
    assert!(!prepare_tools(&mut body, TUI, true, || "- `m`: M.".to_owned()));

    // Nor is it made when the optimized namespace's name is taken: the
    // descriptions are kept.
    let text = r#"{"tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}},{"type":"function","name":"collaboration-optimize__x"}]}"#;
    let mut body = json(text);
    assert!(prepare_tools(&mut body, TUI, true, || -> String {
        unreachable!("the list was made")
    }));
    assert_eq!(text_at(&body, "tools.0.description"), "Spawns an agent.");
    assert!(at(&body, "tools.0.parameters.properties.message.encrypted").is_none());

    // Nor while the setting is off, or for another client.
    for (user_agent, enabled) in [(TUI, false), ("curl/8.7.1", true)] {
        let mut body = json(r#"{"tools":[{"type":"function","name":"spawn_agent"}]}"#);
        assert!(!prepare_tools(
            &mut body,
            user_agent,
            enabled,
            || -> String { unreachable!("the list was made") }
        ));
    }
}

// TestRewriteCodexSpawnAgentDescriptionEnabledOptimizesTool
#[test]
fn enabled_optimizes_tool() {
    let mut body = json(
        r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"type":"string","encrypted":true}}}}]}]}"#,
    );
    // The list the registered model gives.
    let models = [model(
        "codex-spawn-agent-test-model",
        "Test agent model.",
        &["low", "medium", "high"],
        "medium",
    )];
    assert!(optimize(&mut body, DESKTOP, true, || {
        format_spawn_agent_models(&models)
    }));
    assert_eq!(
        text_at(&body, "tools.0.name"),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
    let description = text_at(&body, "tools.0.tools.0.description");
    let want = "- `codex-spawn-agent-test-model`: Test agent model. Reasoning efforts: low, medium (default), high.";
    assert!(description.contains(want), "{description}");
    assert!(
        at(
            &body,
            "tools.0.tools.0.parameters.properties.message.encrypted"
        )
        .is_none()
    );
}

#[test]
fn prepare_only_prepares_tool_definitions() {
    let mut body = json(
        r#"{
        "input":[
            {"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"task"}]},
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}},
                    {"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}}
                ]}
            ]}
        ]
    }"#,
    );
    assert!(prepare_tools(&mut body, CLI, true, String::new));
    assert_eq!(
        text_at(&body, "input.0.content.0.type"),
        "encrypted_content"
    );
    assert_eq!(
        text_at(&body, "input.1.tools.0.name"),
        COLLABORATION_NAMESPACE
    );
    for path in ["input.1.tools.0.tools.0", "input.1.tools.0.tools.1"] {
        assert!(
            at(
                &body,
                &format!("{path}.parameters.properties.message.encrypted")
            )
            .is_none(),
            "{path}"
        );
    }
}

#[test]
fn prepare_is_gated_on_the_setting_and_client() {
    let text = r#"{"tools":[{"type":"function","name":"spawn_agent","parameters":{"properties":{"message":{"encrypted":true}}}}]}"#;
    for (user_agent, enabled) in [(TUI, false), ("curl/8.7.1", true), ("", true)] {
        let mut body = json(text);
        assert!(!prepare_tools(&mut body, user_agent, enabled, String::new));
        assert_eq!(body, json(text), "{user_agent} {enabled}");
    }
}

// TestOptimizeCodexMultiAgentV2RequestSkipsPreparedToolRefresh
#[test]
fn optimize_keeps_an_earlier_model_list() {
    let text = r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Available model overrides (optional; inherited parent model is preferred):\n- old-model: Old model.\nSpawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}}]}]}"#;
    let mut body = json(text);
    assert!(optimize(&mut body, CLI, true, String::new));
    assert!(text_at(&body, "tools.0.tools.0.description").contains("old-model"));
    assert!(
        at(
            &body,
            "tools.0.tools.0.parameters.properties.message.encrypted"
        )
        .is_none()
    );

    // With models to list, the old list gives way.
    let mut body = json(text);
    assert!(optimize(&mut body, CLI, true, || "- `new-model`: New.".to_owned()));
    assert_eq!(
        text_at(&body, "tools.0.tools.0.description"),
        format!("{SPAWN_AGENT_MODELS_HEADING}\n- `new-model`: New.\nSpawns an agent.")
    );
}

// TestReplaceCodexSpawnAgentModelsNormalizesSectionsAndPreservesInstructions
#[test]
fn replace_normalizes_sections_and_preserves_instructions() {
    let description = format!(
        "{SPAWN_AGENT_MODELS_HEADING}\n- `old-model`: old\nKeep this multi-agent instruction.\nSpawns an agent.\n{SPAWN_AGENT_MODELS_HEADING}"
    );
    let got = replace_spawn_agent_models(&description, "- `new-model`: New model.");
    assert!(!got.contains("old-model"), "{got}");
    assert_eq!(got.matches(SPAWN_AGENT_MODELS_HEADING).count(), 1, "{got}");
    assert!(got.contains("Keep this multi-agent instruction."), "{got}");
    assert_eq!(
        got,
        format!(
            "Keep this multi-agent instruction.\n{SPAWN_AGENT_MODELS_HEADING}\n- `new-model`: New model.\nSpawns an agent.\n"
        )
    );
}

#[test]
fn replace_places_the_list() {
    let list = "- `m`: M.";
    let section = format!("{SPAWN_AGENT_MODELS_HEADING}\n{list}");
    for (description, want) in [
        // An empty description is only the list.
        (String::new(), section.clone()),
        // One ending in a line break gets no blank line.
        ("Do work.\n".to_owned(), format!("Do work.\n{section}")),
        // The marker may be mid-line; the list goes before its line.
        (
            "Intro.\n  Use it. Spawns an agent.".to_owned(),
            format!("Intro.\n{section}\n  Use it. Spawns an agent."),
        ),
        // The indent of the first indented heading is kept, and a heading
        // with nothing after it goes.
        (
            format!(
                "{SPAWN_AGENT_MODELS_HEADING}\n- a\n\t{SPAWN_AGENT_MODELS_HEADING}\n- b\n  {SPAWN_AGENT_MODELS_HEADING}"
            ),
            format!("\t{section}"),
        ),
        // Lines that only look like list items stay when no heading leads.
        (
            "- keep\nSpawns an agent.".to_owned(),
            format!("- keep\n{section}\nSpawns an agent."),
        ),
    ] {
        assert_eq!(
            replace_spawn_agent_models(&description, list),
            want,
            "{description:?}"
        );
    }
    assert_eq!(replace_spawn_agent_models("unchanged", ""), "unchanged");
}

#[test]
fn format_spawn_agent_models_text() {
    let models = [
        SpawnAgentModel {
            service_tiers: vec!["flex".into(), "priority".into()],
            ..model(
                " spaced\t id ",
                " What\n it  does ",
                &["low", "high"],
                "high",
            )
        },
        model("", "skipped", &["low"], "low"),
        model("tick`ed", "Done!", &[], ""),
        model("bare", "", &[], ""),
        SpawnAgentModel {
            service_tiers: vec!["flex".into()],
            ..model("tiers", "", &[], "")
        },
        model("asks", "Why?", &["max"], "none"),
        model("multibyte", "Fin\u{00e9}", &[], ""),
    ];
    assert_eq!(
        format_spawn_agent_models(&models),
        [
            "- `spaced id`: What it does. Reasoning efforts: low, high (default). Service tiers: flex, priority.",
            "- `` tick`ed ``: Done!",
            "- `bare`: ",
            "- `tiers`: Service tiers: flex.",
            "- `asks`: Why? Reasoning efforts: max.",
            "- `multibyte`: Fin\u{00e9}.",
        ]
        .join("\n")
    );
    assert_eq!(format_spawn_agent_models(&[]), "");
}

#[test]
fn optimize_normalizes_agent_message_content_only() {
    let text = r#"{"input":[{"type":"agent_message","id":"amsg_1","author":"/root","recipient":"/root/worker","content":[{"type":"input_text","text":"Payload:\n"},{"type":"encrypted_content","encrypted_content":"delegated task"}],"internal_chat_message_metadata_passthrough":{"turn_id":"turn_1"}}]}"#;
    let mut body = json(text);
    assert!(!optimize(&mut body, DESKTOP, true, String::new));
    let message = at(&body, "input.0").unwrap();
    assert_eq!(text_at(message, "type"), "agent_message");
    assert!(message.get("role").is_none());
    assert_eq!(text_at(message, "content.1.type"), "input_text");
    assert_eq!(text_at(message, "content.1.text"), "delegated task");
    assert!(at(message, "content.1.encrypted_content").is_none());
    assert_eq!(text_at(message, "author"), "/root");
    assert_eq!(text_at(message, "recipient"), "/root/worker");
    assert_eq!(
        text_at(
            message,
            "internal_chat_message_metadata_passthrough.turn_id"
        ),
        "turn_1"
    );
    // The part's `text` goes last, where sjson adds it.
    assert_eq!(
        at(message, "content.1").unwrap().to_string(),
        r#"{"type":"input_text","text":"delegated task"}"#
    );

    for (user_agent, enabled) in [(DESKTOP, false), ("curl/8.7.1", true)] {
        let mut unchanged = json(text);
        assert!(!optimize(&mut unchanged, user_agent, enabled, String::new));
        assert_eq!(unchanged, json(text), "{user_agent} {enabled}");
    }
}

#[test]
fn restore_response_cases() {
    let text = r#"{
        "type":"response.completed",
        "response":{
            "output":[
                {"type":"function_call","name":"spawn_agent","namespace":"collaboration-optimize","arguments":{"namespace":"collaboration-optimize","name":"collaboration-optimize__opaque"}},
                {"type":"function_call","name":"collaboration-optimize__send_message"},
                {"type":"message","namespace":"collaboration-optimize","name":"collaboration-optimize__plain"}
            ],
            "tools":[{"type":"namespace","name":"collaboration-optimize"}]
        }
    }"#;
    let got = restored(text);
    assert_eq!(
        text_at(&got, "response.output.0.namespace"),
        COLLABORATION_NAMESPACE
    );
    assert_eq!(
        text_at(&got, "response.output.1.name"),
        "collaboration__send_message"
    );
    assert_eq!(
        text_at(&got, "response.tools.0.name"),
        COLLABORATION_NAMESPACE
    );
    assert_eq!(
        text_at(&got, "response.output.0.arguments.namespace"),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
    assert_eq!(
        text_at(&got, "response.output.2.namespace"),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
    assert_eq!(
        text_at(&got, "response.output.2.name"),
        "collaboration-optimize__plain"
    );
    assert_eq!(restore_response(text.as_bytes(), false), text.as_bytes());
}

#[test]
fn restore_response_restores_dotted_flat_tool_name() {
    let got = restored(
        r#"{
        "type":"response.completed",
        "response":{
            "output":[{
                "type":"function_call",
                "name":"collaboration-optimize.spawn_agent",
                "namespace":null,
                "arguments":"{}",
                "call_id":"call_1"
            },{
                "type":"custom_tool_call",
                "name":"collaboration-optimize.list_agents",
                "input":"{}",
                "call_id":"call_2"
            }]
        }
    }"#,
    );
    assert_eq!(
        text_at(&got, "response.output.0.namespace"),
        COLLABORATION_NAMESPACE
    );
    assert_eq!(text_at(&got, "response.output.0.name"), "spawn_agent");
    assert_eq!(
        text_at(&got, "response.output.1.namespace"),
        COLLABORATION_NAMESPACE
    );
    assert_eq!(text_at(&got, "response.output.1.name"), "list_agents");
}

#[test]
fn restore_response_writes_go_json() {
    let text = "{\"z\":1E5,\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"collaboration-optimize__x\",\"arguments\":\"{\\\"a\\\":1}\",\"n\":-0,\"f\":1.50,\"e\":1e-7},\"html\":\"<a&b>\\u2028\",\"big\":123456789012345678901234567890,\"A\":[true,null]}";
    let out = restore_response(text.as_bytes(), true);
    assert_eq!(
        String::from_utf8(out.into_owned()).unwrap(),
        "{\"A\":[true,null],\"big\":123456789012345678901234567890,\"html\":\"\\u003ca\\u0026b\\u003e\\u2028\",\"item\":{\"arguments\":\"{\\\"a\\\":1}\",\"e\":1e-7,\"f\":1.50,\"n\":-0,\"name\":\"collaboration__x\",\"type\":\"function_call\"},\"type\":\"response.output_item.done\",\"z\":1E5}"
    );
}

#[test]
fn restore_response_reads_text_as_go_does() {
    // Invalid UTF-8 and unpaired surrogates become U+FFFD; a pair stays.
    let mut data =
        b"{\"type\":\"function_call\",\"name\":\"collaboration-optimize__a\",\"x\":\"".to_vec();
    data.extend_from_slice(b"\xff\xfe ok ");
    data.extend_from_slice(b"\\ud800 \\udc00 \\ud83d\\ude00 \\ud800\\u0041\"}");
    let out = restore_response(&data, true);
    let got: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        text_at(&got, "x"),
        "\u{fffd}\u{fffd} ok \u{fffd} \u{fffd} \u{1f600} \u{fffd}A"
    );
    assert_eq!(text_at(&got, "name"), "collaboration__a");
}

#[test]
fn restore_response_leaves_data_alone() {
    for text in [
        "",
        "[DONE]",
        "{\"type\":\"function_call\",\"name\":\"collaboration-optimize__x\"",
        r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
        r#"{"type":"function_call","name":"collaboration-optimize."}"#,
        r#"{"type":"function_call","name":" collaboration-optimize__x"}"#,
        r#"{"type":"namespace","name":"collaboration-optimize__x"}"#,
        r#"{"type":"message","name":"collaboration-optimize"}"#,
        r#"{"type":"function_call","namespace":" collaboration-optimize"}"#,
        r#"{"type":"function_call_output","output":{"type":"function_call","name":"collaboration-optimize__x"}}"#,
        r#"{"type":"custom_tool_call","input":{"type":"function_call","name":"collaboration-optimize__x"}}"#,
    ] {
        let out = restore_response(text.as_bytes(), true);
        assert!(matches!(out, Cow::Borrowed(_)), "{text}");
        assert_eq!(out, text.as_bytes(), "{text}");
    }
}

#[test]
fn restore_response_output_of_other_items_is_walked() {
    // `output` is skipped only in tool outputs.
    let got = restored(
        r#"{"type":"response.completed","output":[{"type":" namespace ","name":"collaboration-optimize"}]}"#,
    );
    assert_eq!(text_at(&got, "output.0.name"), COLLABORATION_NAMESPACE);
}

#[test]
fn rewrite_input_rewrites_agent_message() {
    let mut body = json(
        r#"{"model":"gpt-5.4","input":[{
        "type":"agent_message",
        "id":"amsg_019f92ae-84fd-76f0-aa66-5a722dee382e",
        "author":"/root",
        "recipient":"/root/arithmetic_problem",
        "content":[
            {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/arithmetic_problem\nSender: /root\nPayload:\n"},
            {"type":"encrypted_content","encrypted_content":"请出一道四则运算题，并给出答案。全程使用简体中文，题目简洁。"}
        ],
        "internal_chat_message_metadata_passthrough":{"turn_id":"019f92ae-7eae-7371-957e-8f6f734edddc"}
    }]}"#,
    );
    assert!(rewrite_input(&mut body, DESKTOP, true, false));
    assert_eq!(text_at(&body, "input.0.type"), "message");
    assert_eq!(text_at(&body, "input.0.role"), "user");
    assert_eq!(text_at(&body, "input.0.content.1.type"), "input_text");
    assert_eq!(
        text_at(&body, "input.0.content.1.text"),
        "请出一道四则运算题，并给出答案。全程使用简体中文，题目简洁。"
    );
    assert!(at(&body, "input.0.content.1.encrypted_content").is_none());
    assert_eq!(text_at(&body, "input.0.author"), "/root");
    assert_eq!(
        text_at(
            &body,
            "input.0.internal_chat_message_metadata_passthrough.turn_id"
        ),
        "019f92ae-7eae-7371-957e-8f6f734edddc"
    );
}

const ISSUE_6136: &str = r#"{"model":"gpt-5.4","input":[{
    "type":"agent_message",
    "id":"amsg_1",
    "author":"/root",
    "recipient":"/root/worker",
    "content":[
        {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\n"},
        {"type":"encrypted_content","encrypted_content":"test task"}
    ],
    "internal_chat_message_metadata_passthrough":{"turn_id":"turn_1"}
},{
    "type":"message",
    "role":"user",
    "id":"msg_2",
    "author":"/root/worker",
    "recipient":"/root",
    "content":"regular user message with author",
    "internal_chat_message_metadata_passthrough":{"turn_id":"turn_2"}
},{
    "type":"message",
    "role":"assistant",
    "content":"clean assistant message"
}]}"#;

#[test]
fn rewrite_input_compat_strips_author_and_recipient() {
    let mut body = json(ISSUE_6136);
    assert!(rewrite_input(&mut body, "curl/8.7.1", true, true));
    for index in 0..2 {
        assert_eq!(text_at(&body, &format!("input.{index}.type")), "message");
        assert_eq!(text_at(&body, &format!("input.{index}.role")), "user");
        for field in [
            "author",
            "recipient",
            "internal_chat_message_metadata_passthrough",
        ] {
            assert!(
                at(&body, &format!("input.{index}.{field}")).is_none(),
                "{index} {field}"
            );
        }
    }
    assert_eq!(text_at(&body, "input.2.content"), "clean assistant message");
}

#[test]
fn rewrite_input_without_compat_keeps_author_and_recipient() {
    let mut body = json(ISSUE_6136);
    assert!(rewrite_input(&mut body, DESKTOP, true, false));
    assert_eq!(text_at(&body, "input.0.author"), "/root");
    assert_eq!(text_at(&body, "input.0.recipient"), "/root/worker");
    assert_eq!(
        text_at(
            &body,
            "input.0.internal_chat_message_metadata_passthrough.turn_id"
        ),
        "turn_1"
    );
    assert_eq!(text_at(&body, "input.1.author"), "/root/worker");
    assert_eq!(text_at(&body, "input.1.recipient"), "/root");
}

#[test]
fn rewrite_input_compat_without_optimize() {
    let mut body = json(
        r#"{"model":"gpt-6-luna","input":[{
        "type":"agent_message",
        "id":"amsg_probe",
        "author":"/root/worker",
        "recipient":"/root",
        "content":[
            {"type":"input_text","text":"Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\ndone"},
            {"type":"encrypted_content","encrypted_content":"test task payload"}
        ],
        "internal_chat_message_metadata_passthrough":{"turn_id":"turn_probe"}
    }]}"#,
    );
    assert!(rewrite_input(
        &mut body,
        "Codex Desktop/0.158.0-alpha.2.1",
        false,
        true
    ));
    assert_eq!(text_at(&body, "input.0.type"), "message");
    assert_eq!(text_at(&body, "input.0.role"), "user");
    assert_eq!(text_at(&body, "input.0.content.1.type"), "input_text");
    assert_eq!(
        text_at(&body, "input.0.content.1.text"),
        "test task payload"
    );
    for field in [
        "author",
        "recipient",
        "internal_chat_message_metadata_passthrough",
    ] {
        assert!(at(&body, &format!("input.0.{field}")).is_none(), "{field}");
    }
}

#[test]
fn rewrite_input_conditions() {
    let text = r#"{"input":[{"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"task"}]}]}"#;
    for (enabled, user_agent, want) in [
        (true, DESKTOP, true),
        (true, TUI, true),
        (false, TUI, false),
        (true, "curl/8.7.1", false),
    ] {
        let mut body = json(text);
        assert_eq!(rewrite_input(&mut body, user_agent, enabled, false), want);
        assert_eq!(
            text_at(&body, "input.0.type") == "message",
            want,
            "{user_agent}"
        );
    }
}

#[test]
fn rewrite_input_sets_role_in_place_and_adds_it_last() {
    let mut body = json(
        r#"{"input":[{"role":"assistant","type":"agent_message","id":"a"},{"type":" agent_message ","id":"b"}]}"#,
    );
    assert!(rewrite_input(&mut body, TUI, true, false));
    assert_eq!(
        body.to_string(),
        r#"{"input":[{"role":"user","type":"message","id":"a"},{"type":"message","id":"b","role":"user"}]}"#
    );
}

#[test]
fn message_tools_are_found_in_namespaces() {
    let mut body = json(
        r#"{
        "tools":[
            {"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"unrelated_tool","parameters":{"properties":{"message":{"encrypted":true}}}}
            ]}
        ]
    }"#,
    );
    assert!(prepare_tools(&mut body, TUI, true, String::new));
    for path in ["tools.0.tools.0", "tools.0.tools.1", "tools.0.tools.2"] {
        assert!(
            at(
                &body,
                &format!("{path}.parameters.properties.message.encrypted")
            )
            .is_none(),
            "{path}"
        );
        assert_eq!(
            text_at(&body, &format!("{path}.parameters.properties.message.type")),
            "string"
        );
    }
    assert!(
        at(
            &body,
            "tools.0.tools.3.parameters.properties.message.encrypted"
        )
        .is_some()
    );
}

#[test]
fn message_tools_are_found_in_additional_tools() {
    let mut body = json(
        r#"{
        "input":[
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}},
                    {"type":"function","name":"followup_task","parameters":{"properties":{"message":{"encrypted":true}}}}
                ]}
            ]}
        ]
    }"#,
    );
    assert!(prepare_tools(&mut body, TUI, true, String::new));
    for path in ["input.0.tools.0.tools.0", "input.0.tools.0.tools.1"] {
        assert!(
            at(
                &body,
                &format!("{path}.parameters.properties.message.encrypted")
            )
            .is_none(),
            "{path}"
        );
    }
}

#[test]
fn encryption_removal_keeps_unrelated_encrypted_fields() {
    let mut body = json(
        r#"{
        "tools":[
            {"type":"function","name":"send_message","parameters":{"properties":{"message":{"type":"string","encrypted":true},"data":{"encrypted":"keep-me"}}}},
            {"type":"function","name":"unrelated_tool","parameters":{"properties":{"message":{"encrypted":true}}}}
        ]
    }"#,
    );
    assert!(prepare_tools(&mut body, TUI, true, String::new));
    assert!(at(&body, "tools.0.parameters.properties.message.encrypted").is_none());
    assert_eq!(
        text_at(&body, "tools.0.parameters.properties.data.encrypted"),
        "keep-me"
    );
    assert!(at(&body, "tools.1.parameters.properties.message.encrypted").is_some());
}

#[test]
fn optimize_removes_encryption_without_spawn_agent() {
    let mut body = json(
        r#"{
        "tools":[
            {"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
            ]}
        ]
    }"#,
    );
    assert!(!optimize(&mut body, DESKTOP, true, String::new));
    assert_eq!(text_at(&body, "tools.0.name"), COLLABORATION_NAMESPACE);
    for path in ["tools.0.tools.0", "tools.0.tools.1"] {
        assert!(
            at(
                &body,
                &format!("{path}.parameters.properties.message.encrypted")
            )
            .is_none(),
            "{path}"
        );
    }
}

#[test]
fn optimize_removes_encryption_in_additional_tools() {
    let mut body = json(
        r#"{
        "input":[
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                    {"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
                ]}
            ]}
        ]
    }"#,
    );
    optimize(&mut body, TUI, true, String::new);
    for path in ["input.0.tools.0.tools.0", "input.0.tools.0.tools.1"] {
        assert!(
            at(
                &body,
                &format!("{path}.parameters.properties.message.encrypted")
            )
            .is_none(),
            "{path}"
        );
    }
}

#[test]
fn optimize_removes_encryption_from_all_three_tools_with_spawn_agent() {
    let mut body = json(
        r#"{
        "tools":[
            {"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
            ]}
        ]
    }"#,
    );
    assert!(optimize(&mut body, DESKTOP, true, String::new));
    for path in ["tools.0.tools.0", "tools.0.tools.1", "tools.0.tools.2"] {
        assert!(
            at(
                &body,
                &format!("{path}.parameters.properties.message.encrypted")
            )
            .is_none(),
            "{path}"
        );
    }
    assert_eq!(
        text_at(&body, "tools.0.name"),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
}

#[test]
fn encryption_removal_is_a_no_op_without_encrypted() {
    let text = r#"{"tools":[{"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string"}}}}]}"#;
    let mut body = json(text);
    assert!(!prepare_tools(&mut body, TUI, true, String::new));
    assert_eq!(body.to_string(), text);
}

#[test]
fn optimize_in_additional_tools_renames_only_namespaces() {
    // A spawn_agent directly in additional_tools has no namespace to rename.
    let mut body = json(
        r#"{"input":[{"type":"additional_tools","tools":[{"type":"function","name":"spawn_agent"},{"type":"namespace","name":" collaboration ","tools":[{"type":"function","name":" spawn_agent "}]}]}]}"#,
    );
    assert!(optimize(&mut body, TUI, true, String::new));
    assert_eq!(
        text_at(&body, "input.0.tools.1.name"),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
    let mut flat = json(
        r#"{"input":[{"type":"additional_tools","tools":[{"type":"function","name":"spawn_agent"}]}]}"#,
    );
    assert!(!optimize(&mut flat, TUI, true, String::new));
}

#[test]
fn restore_response_keeps_the_last_of_repeated_keys() {
    let out = restore_response(
        br#"{"type":"function_call","name":"x","name":"collaboration-optimize__a"}"#,
        true,
    );
    assert_eq!(
        out,
        br#"{"name":"collaboration__a","type":"function_call"}"#.as_slice()
    );
}

#[test]
fn restore_response_limits_nesting() {
    let event = r#"{"type":"namespace","name":"collaboration-optimize"}"#;
    let nested = |depth: usize| format!("{}{event}{}", "[".repeat(depth), "]".repeat(depth));
    let deepest = nested(go_any::MAX_DEPTH - 1);
    assert!(matches!(
        restore_response(deepest.as_bytes(), true),
        Cow::Owned(_)
    ));
    let too_deep = nested(go_any::MAX_DEPTH);
    assert!(matches!(
        restore_response(too_deep.as_bytes(), true),
        Cow::Borrowed(_)
    ));
}
