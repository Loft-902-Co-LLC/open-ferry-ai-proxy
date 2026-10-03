//! Hand-written cases for a Codex client's multi-agent v2 requests and
//! orphan delegation outputs: the scenarios of upstream's tests, and the
//! edges of the `spawn_agent` model list, the sections it replaces, the
//! namespace rename and its undoing, and Go's encoder.

use serde_json::{Value, json};

use super::Case;

const TUI: &str = "codex-tui/0.154.0";
const DESKTOP: &str = "Codex Desktop/0.146.0-alpha.3";
const CLI: &str = "codex_cli_rs/0.144.1";
const CURL: &str = "curl/8.7.1";

/// The heading of the model list in `spawn_agent`'s description.
const HEADING: &str = "Available model overrides (optional; inherited parent model is preferred):";

fn case(name: &str, request: impl Into<String>, options: Value) -> Case {
    Case::new(name, "", request).with_options(options)
}

/// Options for a request from `user_agent` with the setting `enabled`.
fn options(user_agent: &str, enabled: bool, registrations: Value) -> Value {
    json!({
        "user_agent": user_agent,
        "subagent": "",
        "enabled": enabled,
        "compat": false,
        "optimized": false,
        "registrations": registrations,
    })
}

fn with(mut options: Value, key: &str, value: Value) -> Value {
    options[key] = value;
    options
}

/// One client registering `models` for the case.
fn registered(models: Value) -> Value {
    json!([{ "client": "parity-multi-agent-0", "provider": "codex", "models": models }])
}

/// The model upstream's executor test registers.
fn test_model() -> Value {
    registered(json!([{
        "id": "codex-spawn-agent-test-model",
        "description": "Test agent model.",
        "thinking": { "levels": ["low", "medium", "high"] },
    }]))
}

/// A collaboration namespace with an encrypted `spawn_agent` described by
/// `description`.
fn spawn_agent_request(description: Value) -> String {
    json!({
        "tools": [{
            "type": "namespace",
            "name": "collaboration",
            "tools": [{
                "type": "function",
                "name": "spawn_agent",
                "description": description,
                "parameters": {"type": "object", "properties": {"message": {"type": "string", "encrypted": true}}},
            }],
        }],
    })
    .to_string()
}

/// `multi-agent/prepare` cases.
pub fn prepares() -> Vec<Case> {
    let only_definitions = r#"{
		"input":[
			{"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"task"}]},
			{"type":"additional_tools","role":"developer","tools":[
				{"type":"namespace","name":"collaboration","tools":[
					{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}},
					{"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}}
				]}
			]}
		]
	}"#;
    let stale_sections = r#"{
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
	}"#;
    let alpha_beta = registered(json!([
        {
            "id": "model-alpha",
            "description": "Alpha model.",
            "thinking": { "levels": ["low", "medium", "high"] },
        },
        {
            "id": "model-beta",
            "description": "Beta model",
            "thinking": { "levels": ["low", "high"] },
        },
    ]));
    let model_a = registered(json!([{
        "id": "model-a",
        "description": "Model A.",
        "thinking": { "levels": ["medium"] },
    }]));
    let invalid_containers = r#"{
		"input":[{"type":"message","tools":[{"type":"function","name":"spawn_agent","description":"message"}]}],
		"tools":[
			{"type":"function","name":"wrapper","tools":[{"type":"function","name":"spawn_agent","description":"child"}]},
			{"type":"custom","name":"spawn_agent","description":"custom"},
			{"type":"namespace","name":"spawn_agent","description":"namespace"}
		]
	}"#;
    let all_three = r#"{
		"tools":[
			{"type":"namespace","name":"collaboration","tools":[
				{"type":"function","name":"spawn_agent","parameters":{"properties":{"message":{"encrypted":true}}}},
				{"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}},
				{"type":"function","name":"followup_task","parameters":{"properties":{"message":{"encrypted":true}}}},
				{"type":"function","name":"unrelated_tool","parameters":{"properties":{"message":{"encrypted":true}}}}
			]}
		]
	}"#;
    let additional_tools = r#"{
		"input":[
			{"type":"additional_tools","role":"developer","tools":[
				{"type":"namespace","name":"collaboration","tools":[
					{"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}},
					{"type":"function","name":"followup_task","parameters":{"properties":{"message":{"encrypted":true}}}}
				]}
			]}
		]
	}"#;
    let unrelated_encrypted = r#"{
		"tools":[
			{"type":"function","name":"send_message","parameters":{"properties":{"message":{"type":"string","encrypted":true},"data":{"encrypted":"keep-me"}}}},
			{"type":"function","name":"unrelated_tool","parameters":{"properties":{"message":{"encrypted":true}}}}
		]
	}"#;
    let conflict = json!({
        "tools": [
            {"type": "namespace", "name": "collaboration", "tools": [{
                "type": "function", "name": "spawn_agent", "description": "Spawns an agent.",
                "parameters": {"properties": {"message": {"encrypted": true}}},
            }]},
            {"type": "namespace", "name": "collaboration-optimize", "tools": []},
        ],
    })
    .to_string();
    let catalog_models = registered(json!([
        { "id": "zeta-model", "display_name": "Zeta", "description": "Last by name." },
        { "id": "gpt-5.5", "display_name": "GPT 5.5" },
        { "id": "alpha-model", "display_name": "alpha", "thinking": { "levels": ["low", "high"] } },
        { "id": "gpt-6-astra" },
        { "id": "Beta-model", "description": "  Spaced\n  description  " },
        { "id": "claude-sonnet-4-6" },
    ]));
    let odd_models = json!([
        {
            "client": "parity-multi-agent-0",
            "provider": "openai-compatibility",
            "models": [
                { "id": "back`tick", "description": "Ends with!", "thinking": { "levels": ["none"] } },
                { "id": "model  with   spaces", "description": "Asks?", "thinking": { "levels": [] } },
                {
                    "id": "levels-model",
                    "description": "<b>Levels</b> & more",
                    "thinking": { "levels": ["none", "minimal", "LOW", " High ", "unsupported", "xhigh", "max", "ultra"] },
                },
                { "id": "medium-model", "thinking": { "levels": ["high", "Medium", "low"] } },
            ],
        },
        {
            "client": "parity-multi-agent-1",
            "provider": "claude",
            "models": [
                { "id": "\u{fc}nic\u{f6}de-model", "display_name": "\u{dc}nic\u{f6}de" },
                { "id": "dotted", "description": "Ends with an ellipsis..." },
            ],
        },
    ]);
    let enabled = |registrations: Value| options(CLI, true, registrations);
    vec![
        case(
            "upstream-only-prepares-tool-definitions",
            only_definitions,
            enabled(test_model()),
        ),
        case(
            "upstream-normalizes-model-list",
            stale_sections,
            enabled(alpha_beta.clone()),
        ),
        case(
            "upstream-top-level-without-marker",
            json!({"tools": [{"type": "namespace", "name": "collaboration", "tools": [
                {"type": "function", "name": "spawn_agent", "description": "Create a worker."},
            ]}]})
            .to_string(),
            enabled(model_a.clone()),
        ),
        case(
            "upstream-without-models-still-removes-encrypted",
            json!({"tools": [{"type": "function", "name": "spawn_agent", "description": "unchanged",
                "parameters": {"properties": {"message": {"encrypted": true}}}}]})
            .to_string(),
            enabled(json!([])),
        ),
        case(
            "upstream-leaves-payload-without-tool-unchanged",
            json!({"tools": [{"type": "function", "name": "other", "description": "unchanged"}]})
                .to_string(),
            enabled(model_a.clone()),
        ),
        case(
            "upstream-ignores-invalid-containers",
            invalid_containers,
            enabled(test_model()),
        ),
        case(
            "upstream-replaces-sections-and-keeps-instructions",
            spawn_agent_request(json!(format!(
                "{HEADING}\n- `old-model`: old\nKeep this multi-agent instruction.\nSpawns an agent.\n{HEADING}"
            ))),
            enabled(json!([{ "client": "parity-multi-agent-0", "provider": "codex", "models": [
                { "id": "new-model", "description": "New model." },
            ]}])),
        ),
        case(
            "upstream-finds-all-three-tools",
            all_three,
            enabled(test_model()),
        ),
        case(
            "upstream-message-tools-in-additional-tools",
            additional_tools,
            enabled(test_model()),
        ),
        case(
            "upstream-keeps-unrelated-encrypted-fields",
            unrelated_encrypted,
            enabled(json!([])),
        ),
        case(
            "upstream-disabled",
            spawn_agent_request(json!("Spawns an agent.")),
            options(DESKTOP, false, test_model()),
        ),
        case(
            "upstream-other-user-agent",
            spawn_agent_request(json!("Spawns an agent.")),
            options(CURL, true, test_model()),
        ),
        case(
            "namespace-conflict-only-removes-encryption",
            conflict,
            enabled(test_model()),
        ),
        case(
            "catalog-models-first-then-by-display-name",
            spawn_agent_request(json!("Spawns an agent to work on a task.")),
            enabled(catalog_models),
        ),
        case(
            "model-ids-descriptions-and-levels",
            spawn_agent_request(json!("Spawns an agent to work on a task.")),
            enabled(odd_models),
        ),
        case(
            "indented-heading-keeps-its-indent",
            spawn_agent_request(json!(format!(
                "Intro.\n    {HEADING}\n    - `old`: Old.\n- also old\nMiddle.\n  {HEADING}\n  - older\nSpawns an agent."
            ))),
            enabled(model_a.clone()),
        ),
        case(
            "marker-inside-a-line",
            spawn_agent_request(json!("Use this tool.\nYou may. Spawns an agent when asked.\nMore.")),
            enabled(model_a.clone()),
        ),
        case(
            "description-ending-in-a-newline",
            spawn_agent_request(json!("Create a worker.\n")),
            enabled(model_a.clone()),
        ),
        case(
            "empty-description",
            spawn_agent_request(json!("")),
            enabled(model_a.clone()),
        ),
        case(
            "heading-alone",
            spawn_agent_request(json!(format!("{HEADING}\n"))),
            enabled(model_a.clone()),
        ),
        case(
            "description-not-a-string",
            spawn_agent_request(json!(42)),
            enabled(model_a.clone()),
        ),
        case(
            "same-list-already-in-place",
            spawn_agent_request(json!(format!(
                "{HEADING}\n- `model-a`: Model A. Reasoning efforts: medium (default).\nSpawns an agent."
            ))),
            enabled(model_a.clone()),
        ),
        case(
            "names-and-types-with-spaces",
            json!({"tools": [{"type": " namespace ", "name": " collaboration ", "tools": [
                {"type": " function ", "name": " spawn_agent ", "description": "Spawns an agent.",
                    "parameters": {"properties": {"message": {"encrypted": true}}}},
                {"type": "Function", "name": "send_message",
                    "parameters": {"properties": {"message": {"encrypted": true}}}},
            ]}]})
            .to_string(),
            enabled(model_a.clone()),
        ),
        case(
            "user-agent-with-spaces",
            spawn_agent_request(json!("Spawns an agent.")),
            options(" codex_exec/1.2.3 ", true, model_a.clone()),
        ),
        case(
            "bare-cli-user-agent",
            spawn_agent_request(json!("Spawns an agent.")),
            options("codex_cli_rs", true, model_a.clone()),
        ),
        case(
            "user-agent-without-version",
            spawn_agent_request(json!("Spawns an agent.")),
            options("codex-tui", true, model_a),
        ),
        case(
            "tools-not-an-array",
            json!({"tools": {"type": "function", "name": "spawn_agent"}, "input": "text"}).to_string(),
            enabled(test_model()),
        ),
    ]
}

/// `multi-agent/optimize` cases.
pub fn optimizes() -> Vec<Case> {
    let agent_message = r#"{"input":[{"type":"agent_message","id":"amsg_1","author":"/root","recipient":"/root/worker","content":[{"type":"input_text","text":"Payload:\n"},{"type":"encrypted_content","encrypted_content":"delegated task"}],"internal_chat_message_metadata_passthrough":{"turn_id":"turn_1"}}]}"#;
    let without_spawn_agent = r#"{
		"tools":[
			{"type":"namespace","name":"collaboration","tools":[
				{"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
				{"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
			]}
		]
	}"#;
    let in_additional_tools = r#"{
		"input":[
			{"type":"additional_tools","role":"developer","tools":[
				{"type":"namespace","name":"collaboration","tools":[
					{"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
					{"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
				]}
			]}
		]
	}"#;
    let all_three = r#"{
		"tools":[
			{"type":"namespace","name":"collaboration","tools":[
				{"type":"function","name":"spawn_agent","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
				{"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
				{"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
			]}
		]
	}"#;
    let prepared = spawn_agent_request(json!(format!(
        "{HEADING}\n- old-model: Old model.\nSpawns an agent."
    )));
    let odd_parts = json!({"input": [
        {"type": " agent_message ", "content": [
            {"type": "encrypted_content", "encrypted_content": 5},
            {"type": "encrypted_content", "encrypted_content": null},
            {"type": "encrypted_content", "encrypted_content": {"text": "nested"}},
            {"type": " encrypted_content ", "encrypted_content": "spaced type"},
            {"type": "encrypted_content"},
            "plain",
            {"type": "encrypted_content", "encrypted_content": "", "text": "kept"},
        ]},
        {"type": "agent_message", "content": "not an array"},
        {"type": "Agent_Message", "content": [{"type": "encrypted_content", "encrypted_content": "other case"}]},
        {"type": "message", "content": [{"type": "encrypted_content", "encrypted_content": "not an agent message"}]},
        7,
    ]})
    .to_string();
    let two_namespaces = json!({
        "tools": [
            {"type": "namespace", "name": "collaboration", "tools": [
                {"type": "function", "name": "spawn_agent", "description": "Spawns an agent."},
                {"type": "function", "name": "spawn_agent", "description": "Again."},
            ]},
            {"type": "namespace", "name": "agents", "tools": [
                {"type": "function", "name": "spawn_agent", "description": "Spawns an agent."},
            ]},
            {"type": "function", "name": "spawn_agent", "description": "Top level."},
        ],
        "input": [
            {"type": "additional_tools", "tools": [
                {"type": "namespace", "name": " collaboration ", "tools": [
                    {"type": "namespace", "name": "collaboration", "tools": [
                        {"type": "function", "name": "spawn_agent"},
                    ]},
                ]},
            ]},
            {"type": "additional_tools", "tools": [
                {"type": "function", "name": "spawn_agent"},
            ]},
        ],
    })
    .to_string();
    let conflict_in_input = json!({
        "tools": [{"type": "namespace", "name": "collaboration", "tools": [
            {"type": "function", "name": "spawn_agent", "parameters": {"properties": {"message": {"encrypted": true}}}},
        ]}],
        "input": [{"type": "additional_tools", "tools": [
            {"type": "namespace", "name": "other", "tools": [
                {"type": "function", "name": "collaboration-optimize__send_message"},
            ]},
        ]}],
    })
    .to_string();
    let enabled = |registrations: Value| options(TUI, true, registrations);
    vec![
        case(
            "upstream-enabled-optimizes-tool",
            r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"type":"string","encrypted":true}}}}]}]}"#,
            options(DESKTOP, true, test_model()),
        ),
        case(
            "upstream-skips-namespace-conflict",
            r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]},{"type":"namespace","name":"collaboration-optimize","tools":[]}]}"#,
            enabled(test_model()),
        ),
        case(
            "upstream-skips-dot-prefix-conflict",
            r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]},{"type":"function","name":"collaboration-optimize.tool"}]}"#,
            enabled(test_model()),
        ),
        case(
            "upstream-without-models",
            r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]}]}"#,
            enabled(json!([])),
        ),
        case(
            "upstream-normalizes-agent-message-content-only",
            agent_message,
            options(DESKTOP, true, json!([])),
        ),
        case(
            "upstream-agent-message-disabled",
            agent_message,
            options(DESKTOP, false, json!([])),
        ),
        case(
            "upstream-agent-message-unrelated-client",
            agent_message,
            options(CURL, true, json!([])),
        ),
        case(
            "upstream-removes-encryption-without-spawn-agent",
            without_spawn_agent,
            enabled(json!([])),
        ),
        case(
            "upstream-removes-encryption-in-additional-tools",
            in_additional_tools,
            enabled(json!([])),
        ),
        case(
            "upstream-removes-encryption-from-all-three-tools",
            all_three,
            enabled(test_model()),
        ),
        case(
            "prepared-list-is-made-again",
            prepared.clone(),
            options(CLI, true, test_model()),
        ),
        case(
            "prepared-list-kept-without-models",
            prepared,
            options(CLI, true, json!([])),
        ),
        case("odd-agent-message-parts", odd_parts, enabled(json!([]))),
        case(
            "namespaces-where-spawn-agent-is",
            two_namespaces,
            enabled(test_model()),
        ),
        case(
            "conflict-in-additional-tools",
            conflict_in_input,
            enabled(test_model()),
        ),
    ]
}

/// `multi-agent/input` cases.
pub fn inputs() -> Vec<Case> {
    let rewrites = r#"{"model":"gpt-5.4","input":[{
		"type":"agent_message",
		"id":"amsg_019f92ae-84fd-76f0-aa66-5a722dee382e",
		"author":"/root",
		"recipient":"/root/arithmetic_problem",
		"content":[
			{"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/arithmetic_problem\nSender: /root\nPayload:\n"},
			{"type":"encrypted_content","encrypted_content":"请出一道四则运算题，并给出答案。全程使用简体中文，题目简洁。"}
		],
		"internal_chat_message_metadata_passthrough":{"turn_id":"019f92ae-7eae-7371-957e-8f6f734edddc"}
	}]}"#;
    let issue_6136 = r#"{"model":"gpt-5.4","input":[{
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
    let issue_6233 = r#"{"model":"gpt-6-luna","input":[{
		"type":"agent_message",
		"id":"amsg_probe",
		"author":"/root/worker",
		"recipient":"/root",
		"content":[
			{"type":"input_text","text":"Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\ndone"},
			{"type":"encrypted_content","encrypted_content":"test task payload"}
		],
		"internal_chat_message_metadata_passthrough":{"turn_id":"turn_probe"}
	}]}"#;
    let conditions = r#"{"input":[{"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"task"}]}]}"#;
    let with_role = json!({"input": [
        {"role": "assistant", "type": "agent_message", "author": null, "content": []},
        {"type": "agent_message", "recipient": {"path": "/root"}},
        "text",
        [1, 2],
        {"author": "/root", "internal_chat_message_metadata_passthrough": null},
    ]})
    .to_string();
    let compat = |options: Value| with(options, "compat", json!(true));
    vec![
        case(
            "upstream-rewrites-agent-message",
            rewrites,
            options(DESKTOP, true, json!([])),
        ),
        case(
            "upstream-issue-6136-compat-strips-metadata",
            issue_6136,
            compat(options(CURL, true, json!([]))),
        ),
        case(
            "upstream-issue-6136-keeps-metadata",
            issue_6136,
            options(DESKTOP, true, json!([])),
        ),
        case(
            "upstream-issue-6233-compat-without-optimize",
            issue_6233,
            compat(options("Codex Desktop/0.158.0-alpha.2.1", false, json!([]))),
        ),
        case(
            "upstream-conditions-desktop",
            conditions,
            options(DESKTOP, true, json!([])),
        ),
        case(
            "upstream-conditions-tui",
            conditions,
            options(TUI, true, json!([])),
        ),
        case(
            "upstream-conditions-disabled",
            conditions,
            options(TUI, false, json!([])),
        ),
        case(
            "upstream-conditions-unrelated-client",
            conditions,
            options(CURL, true, json!([])),
        ),
        case(
            "agent-message-role-and-odd-items",
            with_role.clone(),
            options(TUI, true, json!([])),
        ),
        case(
            "compat-odd-items",
            with_role,
            compat(options("", false, json!([]))),
        ),
        case(
            "input-not-an-array",
            json!({"input": {"type": "agent_message", "author": "/root"}}).to_string(),
            compat(options(TUI, true, json!([]))),
        ),
    ]
}

/// Options for a sub-agent's request with the header `subagent`.
fn orphan_options(subagent: &str, enabled: bool) -> Value {
    with(options("", enabled, json!([])), "subagent", json!(subagent))
}

/// A request whose input is `items`, laid out as upstream's tests write it.
fn orphan_request(items: &str) -> String {
    format!("{{\n\t\"model\": \"deepseek-v4-pro\",\n\t\"input\": [\n{items}\n\t]\n}}")
}

/// `multi-agent/orphan` cases.
pub fn orphans() -> Vec<Case> {
    let create_thread = r#"		{
			"type": "function_call_output",
			"name": "create_thread",
			"namespace": "codex_app",
			"output": "<codex_delegation><message>handoff</message></codex_delegation>"
		}"#;
    let with_message = format!(
        "{create_thread},\n\t\t{{\"type\": \"message\", \"role\": \"user\", \"content\": [{{\"type\": \"input_text\", \"text\": \"please continue\"}}]}}"
    );
    let output = |call_id: &str, name: &str, namespace: &str, output: &str| {
        let call_id = if call_id.is_empty() {
            String::new()
        } else {
            format!("\"call_id\": {call_id}, ")
        };
        format!(
            "\t\t{{\"type\": \"function_call_output\", {call_id}\"name\": \"{name}\", \"namespace\": \"{namespace}\", \"output\": {output}}}"
        )
    };
    let call = |call_id: &str| {
        format!(
            "\t\t{{\"type\": \"function_call\", \"call_id\": {call_id}, \"name\": \"create_thread\", \"namespace\": \"codex_app\", \"arguments\": \"{{}}\"}}"
        )
    };
    let lines = |items: &[String]| orphan_request(&items.join(",\n"));
    let on = || orphan_options("collab_spawn", true);
    vec![
        case(
            "upstream-disabled",
            orphan_request(create_thread),
            orphan_options("collab_spawn", false),
        ),
        case(
            "upstream-missing-subagent-header",
            orphan_request(create_thread),
            orphan_options("", true),
        ),
        case(
            "upstream-other-subagent-header",
            orphan_request(create_thread),
            orphan_options("other_subagent", true),
        ),
        case(
            "upstream-rewrites-create-thread-without-call-id",
            orphan_request(&with_message),
            on(),
        ),
        case(
            "upstream-header-in-any-case",
            lines(&[output("", "create_thread", "codex_app", "\"msg\"")]),
            orphan_options("COLLAB_SPAWN", true),
        ),
        case(
            "upstream-send-message-with-stale-call-id",
            lines(&[output(
                "\"call_stale_123\"",
                "send_message_to_thread",
                "codex_app",
                "\"<codex_delegation>msg</codex_delegation>\"",
            )]),
            on(),
        ),
        case(
            "upstream-keeps-paired-call",
            lines(&[
                call("\"call_active_123\""),
                output(
                    "\"call_active_123\"",
                    "create_thread",
                    "codex_app",
                    "\"<codex_delegation>valid</codex_delegation>\"",
                ),
            ]),
            on(),
        ),
        case(
            "upstream-keeps-other-tools",
            lines(&[
                output("", "automation_update", "codex_app", "\"ignored\""),
                output("", "create_thread", "other_namespace", "\"ignored\""),
            ]),
            on(),
        ),
        case(
            "upstream-empty-output",
            lines(&[output("", "create_thread", "codex_app", "\"\"")]),
            on(),
        ),
        case(
            "upstream-call-id-with-spaces-is-not-paired",
            lines(&[
                call("\"call_1\""),
                output("\" call_1 \"", "create_thread", "codex_app", "\"mismatch\""),
            ]),
            on(),
        ),
        case(
            "upstream-structured-output-quoted-exactly",
            lines(&[output(
                "",
                "create_thread",
                "codex_app",
                r#"[{"type":"input_text","text":"diagram"},{"type":"input_image","image_url":"https://example.com/img.png"}]"#,
            )]),
            on(),
        ),
        case(
            "upstream-paired-in-any-order",
            lines(&[
                output("\"call_future_1\"", "create_thread", "codex_app", "\"early\""),
                call("\"call_future_1\""),
            ]),
            on(),
        ),
        case(
            "upstream-call-pairs-one-output",
            lines(&[
                call("\"call_once\""),
                output("\"call_once\"", "create_thread", "codex_app", "\"first\""),
                output("\"call_once\"", "create_thread", "codex_app", "\"second\""),
            ]),
            on(),
        ),
        case(
            "upstream-custom-tool-call-does-not-pair",
            lines(&[
                "\t\t{\"type\": \"custom_tool_call\", \"call_id\": \"call_custom\", \"name\": \"create_thread\", \"namespace\": \"codex_app\", \"input\": \"{}\"}".to_owned(),
                output("\"call_custom\"", "create_thread", "codex_app", "\"orphan\""),
            ]),
            on(),
        ),
        case(
            "upstream-message-tool-calls-do-not-pair",
            lines(&[
                "\t\t{\"type\": \"message\", \"role\": \"assistant\", \"tool_calls\": [{\"id\": \"call_in_msg\", \"type\": \"function\", \"function\": {\"name\": \"create_thread\"}}]}".to_owned(),
                output("\"call_in_msg\"", "create_thread", "codex_app", "\"orphan\""),
            ]),
            on(),
        ),
        case(
            "upstream-exact-namespace-and-name",
            lines(&[
                "\t\t{\"type\": \"function_call_output\", \"name\": \"codex_app__create_thread\", \"output\": \"orphan\"}".to_owned(),
                output("", "create_thread", " codex_app ", "\"orphan\""),
                output("", "other_tool", "codex_app", "\"orphan\""),
            ]),
            on(),
        ),
        case(
            "outputs-that-are-not-strings",
            json!({"input": [
                {"type": "function_call_output", "name": "create_thread", "namespace": "codex_app"},
                {"type": "function_call_output", "name": "create_thread", "namespace": "codex_app", "output": null},
                {"type": "function_call_output", "name": "send_message_to_thread", "namespace": "codex_app", "output": {"text": "<b>&</b>", "n": 1}},
                {"type": "function_call_output", "name": "create_thread", "namespace": "codex_app", "output": true},
            ]})
            .to_string(),
            on(),
        ),
        case(
            "call-ids-that-are-not-strings",
            json!({"input": [
                {"type": "function_call", "call_id": 5},
                {"type": "function_call_output", "call_id": "5", "name": "create_thread", "namespace": "codex_app", "output": "paired"},
                {"type": "function_call", "call_id": "  "},
                {"type": "function_call_output", "call_id": "  ", "name": "create_thread", "namespace": "codex_app", "output": "blank"},
                {"type": "function_call_output", "call_id": true, "name": "create_thread", "namespace": "codex_app", "output": "unpaired"},
            ]})
            .to_string(),
            orphan_options(" collab_spawn ", true),
        ),
        case(
            "input-not-an-array",
            json!({"input": "text"}).to_string(),
            on(),
        ),
        case(
            "pretty-structured-output",
            "{\n  \"input\": [\n    {\n      \"type\": \"function_call_output\",\n      \"name\": \"create_thread\",\n      \"namespace\": \"codex_app\",\n      \"output\": [\n        {\"type\": \"input_text\", \"text\": \"diagram\"}\n      ]\n    }\n  ]\n}",
            on(),
        )
        .known_difference(
            "an output that isn't a string is quoted as compact JSON, not as the client wrote it",
        ),
    ]
}

/// An event's data, with options saying whether the request was optimized.
fn restore(name: &str, data: impl Into<String>, optimized: bool) -> Case {
    case(name, data, json!({ "optimized": optimized }))
}

/// `multi-agent/restore` cases.
pub fn restores() -> Vec<Case> {
    let completed = r#"{
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
    let dotted = r#"{
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
	}"#;
    let backslash = '\\';
    let escapes = format!(
        r#"{{"type":"response.output_item.done","item":{{"type":"function_call","name":"collaboration-optimize.spawn_agent","text":"caf{backslash}u00e9 {backslash}/ {backslash}ud83d{backslash}ude80 lone {backslash}ud800 end","tab":"{backslash}t"}}}}"#
    );
    let mut deep = String::new();
    for _ in 0..200 {
        deep.push('[');
    }
    deep.push_str(r#"{"type":"namespace","name":"collaboration-optimize"}"#);
    for _ in 0..200 {
        deep.push(']');
    }
    vec![
        restore("upstream-restores-namespace", completed, true),
        restore("upstream-not-optimized", completed, false),
        restore("upstream-restores-dotted-flat-tool-name", dotted, true),
        restore(
            "nothing-to-restore-keeps-text",
            "{\n  \"type\": \"response.created\",\n  \"response\": {\"output\": []}\n}",
            true,
        ),
        restore(
            "html-and-separators-escaped",
            json!({"type": "function_call", "namespace": "collaboration-optimize",
                "name": "<b>&</b>", "text": "a\u{2028}b\u{2029}c \u{0}\u{1f}\u{7f}\u{85}\u{feff} \u{1f680}",
                "z": "last", "a": "first"})
            .to_string(),
            true,
        ),
        restore(
            "numbers-as-written",
            r#"{"type":"custom_tool_call","name":"collaboration-optimize__x","n":[1.50,1e3,1E+2,-0,-0.0,123456789012345678901234567890,5e-324,1e30,0.1]}"#,
            true,
        ),
        restore("escapes-decoded", escapes, true),
        restore("invalid-json", r#"{"type":"namespace","name":"collaboration-optimize""#, true),
        restore("empty", "", true),
        restore(
            "two-values",
            r#"{"type":"namespace","name":"collaboration-optimize"} {}"#,
            true,
        ),
        restore(
            "trailing-whitespace",
            "{\"type\":\"namespace\",\"name\":\"collaboration-optimize\"} \n",
            true,
        ),
        restore(
            "repeated-keys",
            r#"{"type":"function_call","namespace":"other","namespace":"collaboration-optimize","b":1,"b":2}"#,
            true,
        ),
        restore(
            "arguments-input-and-outputs-left-alone",
            json!({"type": "response.output_item.done", "output": [
                {"type": "function_call_output", "output": [{"type": "namespace", "name": "collaboration-optimize"}]},
                {"type": "custom_tool_call_output", "output": {"type": "namespace", "name": "collaboration-optimize"}},
                {"type": "message", "output": {"type": "namespace", "name": "collaboration-optimize"}},
                {"type": "function_call", "input": {"type": "namespace", "name": "collaboration-optimize"},
                    "arguments": [{"type": "namespace", "name": "collaboration-optimize"}]},
            ]})
            .to_string(),
            true,
        ),
        restore(
            "names-that-only-look-optimized",
            json!([
                {"type": "function_call", "name": "collaboration-optimize."},
                {"type": "function_call", "name": "collaboration-optimize__"},
                {"type": "custom_tool_call", "name": " collaboration-optimize.x"},
                {"type": " function_call ", "namespace": "collaboration-optimize", "name": "x"},
                {"type": "function_call", "namespace": " collaboration-optimize"},
                {"type": "namespace", "name": "collaboration-optimize."},
                {"type": "function_call_output", "name": "collaboration-optimize__x"},
                {"type": 5, "name": "collaboration-optimize"},
            ])
            .to_string(),
            true,
        ),
        restore("scalar", "\"collaboration-optimize\"", true),
        restore("nested-200-deep", deep, true).known_difference(
            "data nested more than 128 deep is left alone, where Go reads up to 10,000",
        ),
    ]
}
