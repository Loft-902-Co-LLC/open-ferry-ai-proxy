//! Hand-written cases for the Codex client model list: the scenarios of
//! upstream's tests, and strings Go's encoder escapes.

use serde_json::{Value, json};

use super::Case;

/// The image, video and speech models the list hides.
const HIDDEN_MODELS: [&str; 12] = [
    "grok-imagine-image-quality",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
    "grok-imagine-image",
    "grok-imagine-image-2.0",
    "grok-imagine-video",
    "grok-imagine-video-1.5",
    "grok-imagine-video-1.5-preview",
    "grok-tts",
    "grok-voice-tts-1.0",
];

fn case(name: &str, registrations: Value, client_version: &str) -> Case {
    Case::new(name, "", "").with_options(json!({
        "registrations": registrations,
        "client_version": client_version,
        "optimize_multi_agent_v2": false,
        "providers": true,
        "apply_patch": null,
    }))
}

fn with(mut case: Case, key: &str, value: Value) -> Case {
    case.options[key] = value;
    case
}

fn registration(provider: &str, models: Value) -> Value {
    json!({ "client": format!("parity-{provider}"), "provider": provider, "models": models })
}

/// Upstream's `TestModelsWithClientVersionReturnsCodexCatalog` models.
fn catalog_models() -> Value {
    let mut models = vec![
        json!({
            "id": "gpt-5.5", "object": "model", "created": 1_776_902_400,
            "owned_by": "openai", "type": "openai", "display_name": "GPT 5.5",
            "description": "Frontier model for complex coding, research, and real-world work.",
            "context_length": 272_000, "max_completion_tokens": 64_000,
            "thinking": {"levels": ["low", "medium", "high", "xhigh"]},
        }),
        json!({
            "id": "custom-codex-model-test", "object": "model", "owned_by": "test",
            "type": "openai", "display_name": "Custom Codex Model",
            "description": "Custom model from registry", "context_length": 123_456,
            "thinking": {"levels": ["none", "minimal", "low", "medium", "unsupported", "high", "xhigh"]},
        }),
    ];
    models.extend(HIDDEN_MODELS.map(|id| {
        let owner = if id.starts_with("grok") {
            "xai"
        } else {
            "openai"
        };
        json!({"id": id, "object": "model", "owned_by": owner, "type": "openai"})
    }));
    Value::Array(models)
}

pub fn lists() -> Vec<Case> {
    let catalog = || json!([registration("codex", catalog_models())]);
    let all_catalog_ids: Vec<Value> = catalog_models()
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].clone())
        .collect();
    let aliases = json!([registration(
        "codex",
        json!([
            {"id": "codex-main", "metadata_model_id": "gpt-6-astra", "display_name": "GPT 6.0 Astra"},
            {"id": "codex-luna", "metadata_model_id": "gpt-5.6-luna", "display_name": "GPT 5.6 Luna"},
        ]),
    )]);
    let synthetic = json!([registration(
        "openai-compatibility",
        json!([{"id": "codex-client-multi-agent-v2-test"}]),
    )]);
    let sol = json!([registration(
        "openai-compatibility",
        json!([{"id": "gpt-5.6-sol", "object": "model", "owned_by": "openai", "display_name": "GPT-5.6-Sol"}]),
    )]);
    let routing = json!([
        registration(
            "codex",
            json!([
                {"id": "gpt-5.5"}, {"id": "gpt-reserve"}, {"id": "gpt-image-2"},
                {"id": "catalog-patch-mixed"}, {"id": "catalog-patch-partial"},
            ])
        ),
        registration(
            "openai-compatible-custom",
            json!([
                {"id": "catalog-patch-synthetic"}, {"id": "catalog-patch-mixed"},
                {"id": "catalog-patch-alias", "metadata_model_id": "gpt-5.5", "supported_input_modalities": ["text"]},
            ])
        ),
        registration(
            "gemini",
            json!([
                {"id": "catalog-patch-partial"},
                {"id": "catalog-patch-unknown", "metadata_model_id": "gpt-5.5"},
                {"id": "team/gpt-5.5"},
            ])
        ),
        registration("vertex", json!([{"id": "catalog-patch-disabled"}])),
    ]);
    let routing_patch = json!([
        "gpt-5.5",
        "gpt-reserve",
        "catalog-patch-synthetic",
        "catalog-patch-alias",
        "catalog-patch-mixed",
    ]);
    let mixed = json!([
        registration(
            "codex",
            json!([
                {"id": "mixed-model", "thinking": {"levels": ["low", "medium", "high", "xhigh", "max"]},
                 "supported_input_modalities": ["text", "image"], "explicit_input_modalities": true},
                {"id": "gpt-6-astra"},
            ])
        ),
        registration(
            "claude",
            json!([
                {"id": "mixed-model", "thinking": {"levels": ["low", "high"]}, "explicit_thinking": true,
                 "supported_input_modalities": ["TEXT"], "explicit_input_modalities": true},
                {"id": "gpt-6-astra", "thinking": {"min": 1024, "max": 32_000}, "explicit_thinking": true},
            ])
        ),
    ]);
    let static_lookup = json!([
        registration(
            "claude",
            json!([{"id": "claude-sonnet-4-6"}, {"id": "claude-opus-4-6", "display_name": "Opus"}])
        ),
        registration(
            "gemini",
            json!([{"id": "gemini-2.5-pro"}, {"id": "gemini-2.5-flash-image"}])
        ),
    ]);
    let escapes = json!([registration(
        "openai-compatibility",
        json!([
            {"id": "html-model", "display_name": "<b>Bold</b> & co", "description": "a < b > c & d"},
            {"id": "control-model", "display_name": "tab\there\nnew\u{1}\u{1f}\u{7f}",
             "description": "quote \" backslash \\ slash /"},
            {"id": "separator-model", "display_name": "line\u{2028}para\u{2029}end",
             "description": "\u{e9}\u{3b1}\u{65e5}\u{672c} \u{1d11e}"},
            {"id": "same-name-b", "display_name": "Same"},
            {"id": "same-name-a", "display_name": "Same"},
            {"id": "lower-name", "display_name": "alpha"},
            {"id": "upper-name", "display_name": "Alpha"},
        ]),
    )]);

    vec![
        case("empty", json!([]), ""),
        with(
            case("catalog", catalog(), ""),
            "apply_patch",
            Value::Array(all_catalog_ids.clone()),
        ),
        with(
            case("catalog-0.153.4", catalog(), "0.153.4"),
            "apply_patch",
            Value::Array(all_catalog_ids),
        ),
        case("catalog-without-apply-patch", catalog(), "0.137.0"),
        with(
            case("catalog-without-providers", catalog(), "0.153.4"),
            "providers",
            false.into(),
        ),
        case("oauth-aliases", aliases, "0.153.4"),
        case("multi-agent-off", synthetic.clone(), ""),
        with(
            case("multi-agent-on", synthetic, ""),
            "optimize_multi_agent_v2",
            true.into(),
        ),
        case("version-0.137.0", sol.clone(), "0.137.0"),
        case("version-0.149.1", sol, "0.149.1"),
        with(
            case("apply-patch-routing", routing.clone(), "0.153.4"),
            "apply_patch",
            routing_patch.clone(),
        ),
        with(
            case("apply-patch-routing-cpa", routing.clone(), "cpa"),
            "apply_patch",
            routing_patch,
        ),
        with(
            case("apply-patch-none", routing, ""),
            "apply_patch",
            json!([]),
        ),
        case("mixed-providers", mixed, "0.153.4"),
        case("static-lookup", static_lookup, "0.144.0"),
        case("escapes", escapes, ""),
    ]
}
