// Ported from CLIProxyAPI internal/registry/model_definitions_test.go and
// model_updater_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests for the static catalog.
//!
//! `TestValidateModelsCatalog_Meta` runs through [`StaticCatalog::from_json`],
//! as the checks aren't public, and
//! `TestGetStaticModelDefinitionsByChannelSupportsGeminiInteractions` is
//! part of `the_embedded_catalog_loads`. Dropped: the Gemini, Vertex, Kimi,
//! Antigravity and Devin tests (those providers aren't ported) and
//! `TestModelOverrideHeadersFromEmbeddedModels` (left out by policy). The
//! check that `support_configuration_update` stays out of a model's JSON has
//! no counterpart: `ModelInfo` isn't serialized.

use super::*;

#[test]
fn codex_configuration_update_capability() {
    let catalog = StaticCatalog::embedded();
    let tiers: [(CodexPlan, &[&str]); 4] = [
        (CodexPlan::Free, &["gpt-6-luna"]),
        (CodexPlan::Team, &["gpt-6-astra", "gpt-6-sol", "gpt-6-luna"]),
        (CodexPlan::Plus, &["gpt-6-astra", "gpt-6-sol", "gpt-6-luna"]),
        (CodexPlan::Pro, &["gpt-6-astra", "gpt-6-sol", "gpt-6-luna"]),
    ];
    for (plan, capable) in tiers {
        let models = catalog.codex_models(plan);
        for id in std::iter::once(&"gpt-5.5").chain(capable) {
            let model = models
                .iter()
                .find(|model| model.id == *id)
                .unwrap_or_else(|| panic!("{plan:?}: {id} is missing"));
            assert_eq!(
                model.support_configuration_update,
                *id != "gpt-5.5",
                "{plan:?}: {id}"
            );
        }
    }

    for (raw, want) in [
        (r#"{"id":"test","support_configuration_update":true}"#, true),
        (
            r#"{"id":"test","support_configuration_update":false}"#,
            false,
        ),
        (r#"{"id":"test"}"#, false),
    ] {
        let catalog =
            StaticCatalog::from_json(&format!(r#"{{"codex-pro":[{raw}]}}"#), "test").unwrap();
        let model = &catalog.codex_models(CodexPlan::Pro)[0];
        assert_eq!(model.support_configuration_update, want, "{raw}");
    }
}

#[test]
fn validate_models_catalog() {
    let load = |meta: &str| StaticCatalog::from_json(&format!(r#"{{"meta":{meta}}}"#), "test");
    assert!(load(r#"[{"id":"muse-spark-1.3"}]"#).is_ok());
    assert_eq!(
        load("[null]").unwrap_err().to_string(),
        "test: validate models catalog: meta[0] is null"
    );
    assert_eq!(
        load(r#"[{"id":" "}]"#).unwrap_err().to_string(),
        "test: validate models catalog: meta[0] has empty id"
    );
    assert_eq!(
        load(r#"[{"id":"muse-spark-1.3"},{"id":" muse-spark-1.3 "}]"#)
            .unwrap_err()
            .to_string(),
        "test: validate models catalog: meta contains duplicate model id \"muse-spark-1.3\""
    );
    // Upstream doesn't check the Devin section, and allows empty sections.
    assert!(StaticCatalog::from_json(r#"{"devin":[null,{"id":""}],"claude":[]}"#, "test").is_ok());
}

#[test]
fn with_codex_builtins_includes_image_25_models() {
    let models = with_codex_builtins(Vec::new());
    for (id, display_name) in [
        ("gpt-image-2.5-flare", "GPT Image 2.5 Flare"),
        ("gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst"),
        ("gpt-image-2.5", "GPT Image 2.5"),
    ] {
        let model = models
            .iter()
            .find(|model| model.id == id)
            .unwrap_or_else(|| panic!("{id} is missing"));
        assert_eq!(model.display_name, display_name);
        assert_eq!(model.object, "model");
        assert_eq!(model.owned_by, "openai");
        assert_eq!(model.model_type, "openai");
        assert_eq!(model.version, id);
        assert_eq!(model.created, 1_704_067_200);
    }
}

#[test]
fn with_codex_builtins_replaces_models_of_the_same_id() {
    let models = with_codex_builtins(vec![
        ModelInfo {
            id: " GPT-IMAGE-2 ".into(),
            display_name: "old".into(),
            ..ModelInfo::default()
        },
        ModelInfo {
            id: " ".into(),
            ..ModelInfo::default()
        },
        ModelInfo {
            id: "gpt-5".into(),
            ..ModelInfo::default()
        },
    ]);
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "gpt-5",
            "gpt-image-1.5",
            "gpt-image-2",
            "gpt-image-2.5-flare",
            "gpt-image-2.5-sunburst",
            "gpt-image-2.5",
        ]
    );
}

#[test]
fn with_xai_builtins_includes_image_20() {
    let models = with_xai_builtins(Vec::new());
    let model = models
        .iter()
        .find(|model| model.id == "grok-imagine-image-2.0")
        .expect("grok-imagine-image-2.0 is missing");
    assert_eq!(model.created, 1_786_060_800, "2026-08-07");
}

#[test]
fn with_xai_builtins_includes_video_15_ga_and_preview_alias() {
    let models = with_xai_builtins(Vec::new());
    for id in ["grok-imagine-video-1.5", "grok-imagine-video-1.5-preview"] {
        assert!(models.iter().any(|model| model.id == id), "{id}");
    }
}

// Ported from TestWithXAIBuiltinsIncludesSpeechModels; not upstream's: the
// display names, descriptions and dates.
#[test]
fn with_xai_builtins_includes_speech_models() {
    let models = with_xai_builtins(Vec::new());
    for (id, display_name) in [
        ("grok-tts", "Grok TTS"),
        ("grok-voice-tts-1.0", "Grok Voice TTS 1.0"),
    ] {
        let model = models
            .iter()
            .find(|model| model.id == id)
            .unwrap_or_else(|| panic!("{id}"));
        assert_eq!(model.owned_by, "xai", "{id}");
        assert_eq!(model.model_type, "xai", "{id}");
        assert_eq!(model.display_name, display_name);
        assert_eq!(model.name, id);
        assert_eq!(model.description, "xAI Grok unary text-to-speech model.");
        assert_eq!(model.created, 1_773_619_200, "{id}");
    }
}

// Not upstream's: the image and video models replace models of the same
// ID, in any case, and drop models without one, as upstream's
// `upsertModelInfos` does, and are written as upstream writes them.
#[test]
fn with_xai_builtins_replaces_models_of_the_same_id() {
    let models = with_xai_builtins(vec![
        ModelInfo {
            id: "grok-4.5".to_owned(),
            ..ModelInfo::default()
        },
        ModelInfo {
            id: " GROK-Imagine-Video ".to_owned(),
            display_name: "stale".to_owned(),
            ..ModelInfo::default()
        },
        ModelInfo {
            id: "  ".to_owned(),
            ..ModelInfo::default()
        },
        ModelInfo {
            id: "Grok-Imagine-Image-Quality".to_owned(),
            display_name: "stale".to_owned(),
            ..ModelInfo::default()
        },
    ]);
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "grok-4.5",
            "grok-imagine-image",
            "grok-imagine-image-quality",
            "grok-imagine-image-2.0",
            "grok-imagine-video",
            "grok-imagine-video-1.5",
            "grok-imagine-video-1.5-preview",
            "grok-tts",
            "grok-voice-tts-1.0"
        ]
    );
    assert_eq!(
        models[2],
        ModelInfo {
            id: "grok-imagine-image-quality".to_owned(),
            object: "model".to_owned(),
            created: 1_735_689_600,
            owned_by: "xai".to_owned(),
            model_type: "xai".to_owned(),
            display_name: "Grok Imagine Image Quality".to_owned(),
            name: "grok-imagine-image-quality".to_owned(),
            description: "xAI Grok higher-fidelity image generation model.".to_owned(),
            ..ModelInfo::default()
        }
    );
    assert_eq!(
        models[6],
        ModelInfo {
            id: "grok-imagine-video-1.5-preview".to_owned(),
            object: "model".to_owned(),
            created: 1_735_689_600,
            owned_by: "xai".to_owned(),
            model_type: "xai".to_owned(),
            display_name: "Grok Imagine Video 1.5 Preview".to_owned(),
            name: "grok-imagine-video-1.5-preview".to_owned(),
            description: "Compatibility alias for the xAI Grok video generation model.".to_owned(),
            ..ModelInfo::default()
        }
    );
    assert_eq!(models[1].display_name, "Grok Imagine Image");
    assert_eq!(models[4].display_name, "Grok Imagine Video");
}

#[test]
fn the_embedded_catalog_loads() {
    let text = embedded_catalog_json();
    let catalog = StaticCatalog::from_json(text, "embed").unwrap();
    assert_eq!(&catalog, StaticCatalog::embedded());
    assert!(!catalog.claude_models().is_empty());
    for plan in [
        CodexPlan::Free,
        CodexPlan::Team,
        CodexPlan::Plus,
        CodexPlan::Pro,
    ] {
        let models = catalog.codex_models(plan);
        assert!(models.len() > CODEX_BUILTINS.len(), "{plan:?}");
        assert!(models.iter().any(|model| model.id == "gpt-image-2"));
    }
    assert_eq!(
        catalog.models_for_channel(" CODEX "),
        catalog.codex_models(CodexPlan::Pro)
    );
    assert_eq!(
        catalog.models_for_channel("claude"),
        catalog.claude_models()
    );
    assert_eq!(
        catalog.models_for_channel("gemini"),
        catalog.gemini_models()
    );
    assert_eq!(
        catalog.models_for_channel("Gemini-Interactions"),
        catalog.gemini_models()
    );
    assert_eq!(
        catalog.models_for_channel(" vertex"),
        catalog.vertex_models()
    );
    assert!(catalog.models_for_channel("aistudio").is_empty());
}

// Not upstream's: the xAI and Meta sections, the xAI one followed by
// upstream's image and video built-ins.
#[test]
fn xai_and_meta_models_come_from_their_sections() {
    let catalog = StaticCatalog::embedded();
    let xai = catalog.xai_models();
    assert!(xai.iter().any(|model| model.id == "grok-4.5"));
    assert!(xai.iter().all(|model| model.owned_by == "xai"), "{xai:?}");
    let imagine: Vec<&str> = xai
        .iter()
        .map(|model| model.id.as_str())
        .filter(|id| id.contains("imagine"))
        .collect();
    assert_eq!(
        imagine,
        [
            "grok-imagine-image",
            "grok-imagine-image-quality",
            "grok-imagine-image-2.0",
            "grok-imagine-video",
            "grok-imagine-video-1.5",
            "grok-imagine-video-1.5-preview"
        ]
    );
    let meta = catalog.meta_models();
    assert!(meta.iter().any(|model| model.id == "muse-spark-1.3"));
    assert!(meta.iter().all(|model| model.owned_by == "meta"));
    let only = StaticCatalog::from_json(
        r#"{"xai":[{"id":"grok-x"}],"meta":[{"id":"muse-x"}]}"#,
        "test",
    )
    .unwrap();
    assert_eq!(only.xai_models().len(), 9);
    assert_eq!(only.xai_models()[0].id, "grok-x");
    assert_eq!(only.meta_models()[0].id, "muse-x");
}

#[test]
fn gemini_models_keep_their_gemini_fields() {
    let catalog = StaticCatalog::embedded();
    let gemini = catalog.gemini_models();
    assert!(!gemini.is_empty());
    assert!(!catalog.vertex_models().is_empty());
    let pro = gemini
        .iter()
        .find(|model| model.id == "gemini-2.5-pro")
        .expect("gemini-2.5-pro in the catalog");
    assert_eq!(pro.name, "models/gemini-2.5-pro");
    assert!(pro.input_token_limit > 0);
    assert!(pro.output_token_limit > 0);
    assert!(
        pro.supported_generation_methods
            .iter()
            .any(|method| method == "generateContent")
    );
    let catalog = StaticCatalog::from_json(
        r#"{"gemini": [{"id": "g", "inputTokenLimit": -1, "outputTokenLimit": 8,
            "supportedGenerationMethods": ["countTokens"]}]}"#,
        "test",
    )
    .expect("catalog");
    let model = catalog.gemini_models().remove(0);
    assert_eq!(model.input_token_limit, 0);
    assert_eq!(model.output_token_limit, 8);
    assert_eq!(model.supported_generation_methods, ["countTokens"]);
}

// LookupStaticModelInfo: the first section that has the model wins, and
// sections no provider serves here are searched too.
#[test]
fn lookup_searches_sections_in_upstream_order() {
    let catalog = StaticCatalog::from_json(
        r#"{
            "codex-free": [{"id": "free-only", "type": "openai"}],
            "kimi": [{"id": "shared", "type": "kimi"}, {"id": "k", "type": "kimi"}],
            "vertex": [{"id": "shared", "type": "gemini"}]
        }"#,
        "test",
    )
    .expect("catalog");
    assert_eq!(catalog.lookup("shared").unwrap().model_type, "gemini");
    assert_eq!(catalog.lookup("k").unwrap().model_type, "kimi");
    assert!(catalog.lookup("free-only").is_none());
    assert!(catalog.lookup("").is_none());
    assert!(catalog.lookup(" k").is_none());
    let embedded = StaticCatalog::embedded();
    assert_eq!(
        embedded.lookup("gemini-2.5-pro").unwrap().model_type,
        "gemini"
    );
    assert!(embedded.lookup("imagen-4.0-generate-001").is_some());
}

#[test]
fn plan_types_pick_codex_plans() {
    for (plan_type, plan) in [
        ("pro", CodexPlan::Pro),
        ("PLUS", CodexPlan::Plus),
        ("team", CodexPlan::Team),
        ("Business", CodexPlan::Team),
        ("go", CodexPlan::Team),
        ("free", CodexPlan::Free),
        ("", CodexPlan::Pro),
        ("enterprise", CodexPlan::Pro),
    ] {
        assert_eq!(CodexPlan::from_plan_type(plan_type), plan, "{plan_type}");
    }
}

#[test]
fn models_decode_as_go_decodes_them() {
    let catalog = StaticCatalog::from_json(
        r#"{
            "CLAUDE": [{
                "ID": "c1",
                "object": "model",
                "created": 5,
                "owned_by": null,
                "type": "claude",
                "context_length": -1,
                "max_completion_tokens": 8192,
                "supported_parameters": ["a", null],
                "thinking": {"MIN": 1024, "levels": ["low"], "zero_allowed": true},
                "config": {"override_header": {"user-agent": "ignored"}},
                "native_capabilities": {"web_search": null},
                "unknown": [1, 2]
            }],
            "codex-pro": null
        }"#,
        "test",
    )
    .unwrap();
    let models = catalog.claude_models();
    assert_eq!(models.len(), 1);
    let model = &models[0];
    assert_eq!(model.id, "c1");
    assert_eq!(model.created, 5);
    assert_eq!(model.owned_by, "");
    assert_eq!(model.model_type, "claude");
    assert_eq!(model.context_length, 0);
    assert_eq!(model.max_completion_tokens, 8192);
    assert_eq!(model.supported_parameters, ["a", ""]);
    assert_eq!(
        model.thinking,
        Some(ThinkingSupport {
            min: 1024,
            zero_allowed: true,
            levels: vec!["low".into()],
            ..ThinkingSupport::default()
        })
    );
    assert_eq!(
        StaticCatalog::from_json("null", "test"),
        Ok(StaticCatalog::default())
    );
}

#[test]
fn decode_errors_name_the_value() {
    let error = |text: &str| {
        StaticCatalog::from_json(text, "remote")
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        error("[]"),
        "remote: decode models catalog: catalog: want an object, found an array"
    );
    assert_eq!(
        error(r#"{"claude": {}}"#),
        "remote: decode models catalog: claude: want an array, found an object"
    );
    assert_eq!(
        error(r#"{"claude": [1]}"#),
        "remote: decode models catalog: claude[0]: want an object, found a number"
    );
    assert_eq!(
        error(r#"{"claude": [{"id": 1}]}"#),
        "remote: decode models catalog: claude[0].id: want a string, found a number"
    );
    assert_eq!(
        error(r#"{"gemini": [{"id": "g", "inputTokenLimit": 1.5}]}"#),
        "remote: decode models catalog: gemini[0].inputTokenLimit: 1.5 is not a 64-bit integer"
    );
    assert_eq!(
        error(r#"{"claude": [{"id": "c", "config": {"override_header": {"a": 1}}}]}"#),
        "remote: decode models catalog: claude[0].config.override_header.a: want a string, found a number"
    );
    assert_eq!(
        error(r#"{"claude": [{"id": "c", "thinking": {"levels": "high"}}]}"#),
        "remote: decode models catalog: claude[0].thinking.levels: want an array, found a string"
    );
    assert!(error("{").starts_with("remote: decode models catalog: "));
}

// Upstream's TestDetectChangedProviders_CodexConfigurationUpdate.
#[test]
fn detect_changed_providers_codex_configuration_update() {
    let old = StaticCatalog::from_json(r#"{"codex-free":[{"id":"gpt-6-luna"}]}"#, "test").unwrap();
    let new = StaticCatalog::from_json(
        r#"{"codex-free":[{"id":"gpt-6-luna","support_configuration_update":true}]}"#,
        "test",
    )
    .unwrap();
    assert_eq!(old.changed_providers(&new), ["codex"]);
}

// Upstream's TestDetectChangedProviders_KimiAliases.
#[test]
fn detect_changed_providers_kimi_aliases() {
    let old = StaticCatalog::from_json(r#"{"kimi":[{"id":"kimi-k2"}]}"#, "test").unwrap();
    let new = StaticCatalog::from_json(r#"{"kimi":[{"id":"kimi-k2"},{"id":"kimi-k3"}]}"#, "test")
        .unwrap();
    let changed = old.changed_providers(&new);
    for provider in ["kimi", "kimi-ai", "kimi.ai", "kimi.com"] {
        assert!(
            changed.iter().any(|seen| seen == provider),
            "{provider}: {changed:?}"
        );
    }
}

// Not upstream's: each provider is named once, in upstream's order, and the
// same catalog changes none.
#[test]
fn changed_providers_come_once_in_upstreams_order() {
    let old = StaticCatalog::embedded();
    assert!(old.changed_providers(old).is_empty());
    let new = StaticCatalog::from_json(
        r#"{"meta":[{"id":"m"}],"codex-pro":[{"id":"c"}],"codex-team":[{"id":"c"}],"gemini":[{"id":"g"}]}"#,
        "test",
    )
    .unwrap();
    let changed = StaticCatalog::default().changed_providers(&new);
    assert_eq!(changed, ["gemini", "gemini-interactions", "codex", "meta"]);
}

// Not upstream's: a model's web search fields decode as upstream's
// `ModelInfo` reads them, the last of a repeated key standing, and reach
// the translators' catalog.
#[test]
fn web_search_fields_decode() {
    let catalog = StaticCatalog::from_json(
        r#"{"claude":[
            {"id":"a","supports_web_search":true,"native_capabilities":{"web_search":false}},
            {"id":"b","native_capabilities":null},
            {"id":"c","native_capabilities":{"web_search":null,"WEB_SEARCH":true}},
            {"id":"d","native_capabilities":{"web_search":true,"web_search":null}}
        ]}"#,
        "test",
    )
    .unwrap();
    let fields = |id: &str| {
        let model = catalog.lookup(id).unwrap();
        (model.supports_web_search, model.native_web_search)
    };
    assert_eq!(fields("a"), (true, Some(false)));
    assert_eq!(fields("b"), (false, None));
    assert_eq!(fields("c"), (false, Some(true)));
    assert_eq!(fields("d"), (false, None));
    let translators = catalog.translator_catalog();
    let a = translators.lookup("a").unwrap();
    assert!(a.supports_web_search);
    assert_eq!(a.native_web_search, Some(false));
    assert!(
        StaticCatalog::from_json(
            r#"{"claude":[{"id":"x","native_capabilities":{"web_search":1}}]}"#,
            "test"
        )
        .is_err()
    );
}

// Not upstream's: the translators' catalog made from the built-in catalog
// is the one the translators build in.
#[test]
fn the_translators_catalog_matches_the_built_in_one() {
    assert_eq!(
        StaticCatalog::embedded().translator_catalog(),
        *open_ferry_translate::models::ModelCatalog::embedded()
    );
}
