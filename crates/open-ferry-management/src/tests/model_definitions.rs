// Ported from CLIProxyAPI internal/api/handlers/management/
// model_definitions_test.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//! Static definitions and the sorted registry fallback for served providers.

use http::{Method, StatusCode};
use open_ferry_core::auth::synthesizer::{
    StableIdGenerator, SynthesisContext, synthesize_config_auths,
};
use open_ferry_core::config::Config;
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::{RegistrationRules, StaticCatalog};
use serde_json::Value;

use super::{Api, LOCAL, request_from};

#[tokio::test]
async fn configured_provider_definitions_fall_back_to_sorted_registry_models() {
    let api = Api::new();
    let models: Vec<ModelInfo> = ["z-custom", "a-custom"]
        .into_iter()
        .map(|id| ModelInfo {
            id: id.into(),
            display_name: format!("Custom {id}"),
            ..ModelInfo::default()
        })
        .collect();
    api.registry.register_client("custom-key", "acme", &models);
    for route in [
        "/v0/management/model-definitions",
        "/v8/management/routing/model-definitions",
    ] {
        let body = api
            .get(&format!("{route}/Acme"))
            .await
            .expect(StatusCode::OK);
        assert_eq!(body["channel"], "acme");
        assert_eq!(ids(&body), ["a-custom", "z-custom"]);
        assert_eq!(body["models"][0]["display_name"], "Custom a-custom");
    }
    for channel in [
        "antigravity",
        "kimi",
        "aistudio",
        "devin",
        "gemini-cli",
        "home",
    ] {
        api.get(&format!("/v0/management/model-definitions/{channel}"))
            .await
            .expect(StatusCode::BAD_REQUEST);
        api.registry.register_client(channel, channel, &models);
        let body = api
            .get(&format!("/v0/management/model-definitions/{channel}"))
            .await
            .expect(StatusCode::OK);
        assert_eq!(ids(&body), ["a-custom", "z-custom"]);
    }
    api.get("/v0/management/model-definitions/unknown")
        .await
        .expect(StatusCode::BAD_REQUEST);
}

// Use the real config synthesizer and registration rules: compat providers
// register under openai-compatible-<name>, not under their bare config name.
#[tokio::test]
async fn compat_provider_named_kimi_lists_its_registered_definitions() {
    let mut config = super::keyed_config();
    config.openai_compatibility = Config::parse(
        r#"openai-compatibility:
  - name: kimi
    base-url: https://kimi.example.test/v1
    api-key-entries:
      - api-key: dummy-key
    models:
      - name: z-upstream
        alias: z-kimi
      - name: a-upstream
        alias: a-kimi
"#,
    )
    .unwrap()
    .openai_compatibility;
    let api = Api::with(config.clone(), None);
    api.get("/v0/management/model-definitions/kimi")
        .await
        .expect(StatusCode::BAD_REQUEST);
    let auths = synthesize_config_auths(
        &config,
        &SynthesisContext::new("", chrono::Utc::now()),
        &mut StableIdGenerator::new(),
    )
    .unwrap();
    assert_eq!(auths.len(), 1);
    assert_eq!(auths[0].provider, "openai-compatible-kimi");
    api.registry
        .register_auth(&auths[0], &RegistrationRules::from(&config));
    assert!(api.registry.available_models_by_provider("kimi").is_empty());
    assert_eq!(
        api.registry
            .available_models_by_provider("openai-compatible-kimi")
            .len(),
        2
    );
    for route in [
        "/v0/management/model-definitions",
        "/v8/management/routing/model-definitions",
    ] {
        for channel in ["KiMi", "openai-compatible-kimi"] {
            let body = api
                .get(&format!("{route}/{channel}"))
                .await
                .expect(StatusCode::OK);
            assert_eq!(body["channel"], channel.to_lowercase());
            assert_eq!(ids(&body), ["a-kimi", "z-kimi"]);
            assert_eq!(body["models"][0]["owned_by"], "kimi");
        }
    }
    api.registry.unregister_client(&auths[0].id);
    api.get("/v0/management/model-definitions/kimi")
        .await
        .expect(StatusCode::BAD_REQUEST);
}

#[test]
fn an_empty_static_channel_falls_back_without_changing_catalog_priority() {
    let registry = open_ferry_core::registry::ModelRegistry::new();
    let models: Vec<ModelInfo> = ["z-registered", "a-registered"]
        .into_iter()
        .map(|id| ModelInfo {
            id: id.into(),
            ..ModelInfo::default()
        })
        .collect();
    registry.register_client("claude-key", "claude", &models);
    let empty = StaticCatalog::from_json("{}", "test").unwrap();
    let fallback = crate::model_definitions::resolved_models(&empty, &registry, "claude");
    assert_eq!(
        fallback.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["a-registered", "z-registered"]
    );
    let catalog = StaticCatalog::embedded();
    assert_eq!(
        crate::model_definitions::resolved_models(catalog, &registry, "claude"),
        catalog.models_for_channel("claude")
    );
    assert!(crate::model_definitions::resolved_models(&empty, &registry, "unknown").is_empty());
}

/// The answer's models' IDs.
fn ids(body: &Value) -> Vec<&str> {
    body["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect()
}

/// Not upstream's: each channel lists the catalog's models for it, each
/// written as upstream writes it.
#[tokio::test]
async fn channels_list_their_models() {
    let api = Api::new();
    for (channel, first) in [
        (
            "claude",
            concat!(
                r#"{"id":"claude-haiku-4-5-20251001","object":"model","created":1759276800,"#,
                r#""owned_by":"anthropic","type":"claude","display_name":"Claude 4.5 Haiku","#,
                r#""context_length":200000,"max_completion_tokens":64000,"#,
                r#""supportedInputModalities":["text","image"],"supportedOutputModalities":["text"],"#,
                r#""thinking":{"min":1024,"max":128000,"zero_allowed":true}}"#,
            ),
        ),
        (
            "gemini",
            concat!(
                r#"{"id":"gemini-2.5-pro","object":"model","created":1750118400,"#,
                r#""owned_by":"google","type":"gemini","display_name":"Gemini 2.5 Pro","#,
                r#""name":"models/gemini-2.5-pro","version":"2.5","#,
                r#""description":"Stable release (June 17th, 2025) of Gemini 2.5 Pro","#,
                r#""inputTokenLimit":1048576,"outputTokenLimit":65536,"#,
                r#""supportedGenerationMethods":["generateContent","countTokens","#,
                r#""createCachedContent","batchGenerateContent"],"#,
                r#""supportedInputModalities":["text","image","audio","video"],"#,
                r#""supportedOutputModalities":["text"],"#,
                r#""thinking":{"min":128,"max":32768,"dynamic_allowed":true}}"#,
            ),
        ),
        (
            "codex",
            concat!(
                r#"{"id":"gpt-5.5","object":"model","created":1776902400,"owned_by":"openai","#,
                r#""type":"openai","display_name":"GPT 5.5","version":"gpt-5.5","#,
                r#""description":"Frontier model for complex coding, research, and real-world work.","#,
                r#""context_length":272000,"max_completion_tokens":128000,"#,
                r#""supported_parameters":["tools"],"supportedInputModalities":["text","image"],"#,
                r#""supportedOutputModalities":["text"],"#,
                r#""thinking":{"levels":["low","medium","high","xhigh"]}}"#,
            ),
        ),
    ] {
        let answer = api
            .get(&format!("/v0/management/model-definitions/{channel}"))
            .await;
        assert_eq!(answer.status, StatusCode::OK, "{channel}");
        let start = format!(r#"{{"channel":"{channel}","models":[{first}"#);
        assert!(
            answer.body.starts_with(&start),
            "{channel}: {}",
            answer.body
        );
    }
    for channel in ["claude", "gemini", "gemini-interactions", "vertex", "codex"] {
        let body = api
            .get(&format!("/v0/management/model-definitions/{channel}"))
            .await
            .expect(StatusCode::OK);
        let want = StaticCatalog::embedded().models_for_channel(channel);
        assert!(!want.is_empty(), "{channel}");
        let want: Vec<&str> = want.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids(&body), want, "{channel}");
        assert_eq!(body["channel"], channel);
    }
}

/// Not upstream's: `xai` and `meta` list the catalog's xAI and Meta models,
/// under each of upstream's spellings of their names, written as upstream
/// writes them. The channel is named as sent, in lower case.
#[tokio::test]
async fn xai_and_meta_channels_list_their_models() {
    let api = Api::new();
    let catalog = StaticCatalog::embedded();
    let xai_first = concat!(
        r#"{"id":"grok-4.7","object":"model","created":1789948800,"owned_by":"xai","#,
        r#""type":"xai","display_name":"Grok 4.7","name":"grok-4.7","#,
        r#""description":"SpaceXAI's frontier model for coding, agentic tasks, and knowledge work.","#,
        r#""context_length":500000,"max_completion_tokens":500000,"#,
        r#""supportedInputModalities":["text","image"],"supportedOutputModalities":["text"],"#,
        r#""thinking":{"levels":["low","medium","high","xhigh"]}}"#,
    );
    let meta_first = concat!(
        r#"{"id":"muse-wonder-1.0","object":"model","created":1791763200,"owned_by":"meta","#,
        r#""type":"meta","display_name":"Muse Wonder 1.0","name":"muse-wonder-1.0","#,
        r#""description":"Meta's most capable model, built for complex software engineering and long-horizon agentic work.","#,
        r#""context_length":1007997,"max_completion_tokens":128000,"#,
        r#""supportedInputModalities":["text","image","video","audio"],"supportedOutputModalities":["text"],"#,
        r#""thinking":{"levels":["minimal","low","medium","high","xhigh","max"]}}"#,
    );
    for (names, models, first) in [
        (
            &["xai", "x-ai", "grok"][..],
            catalog.xai_models(),
            xai_first,
        ),
        (&["meta", "muse"][..], catalog.meta_models(), meta_first),
    ] {
        assert!(!models.is_empty());
        let want: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        for name in names {
            for path in [
                format!("/v0/management/model-definitions/{name}"),
                format!("/v8/management/routing/model-definitions/{name}"),
                format!(
                    "/v0/management/model-definitions/%20{}%20",
                    name.to_uppercase()
                ),
            ] {
                let answer = api.get(&path).await;
                assert_eq!(answer.status, StatusCode::OK, "{path}");
                let start = format!(r#"{{"channel":"{name}","models":[{first}"#);
                assert!(answer.body.starts_with(&start), "{path}: {}", answer.body);
                let body = answer.expect(StatusCode::OK);
                assert_eq!(ids(&body), want, "{path}");
            }
        }
    }
    // The built-in models upstream adds to xAI's list end it, the image
    // ones first, then the video and the speech ones, written as upstream
    // writes them.
    let body = api
        .get("/v0/management/model-definitions/xai")
        .await
        .expect(StatusCode::OK);
    let imagine: Vec<&str> = ids(&body)
        .into_iter()
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
    let last = body["models"]
        .as_array()
        .and_then(|models| models.last())
        .map(Value::to_string);
    assert_eq!(
        last.as_deref(),
        Some(concat!(
            r#"{"id":"grok-voice-tts-1.0","object":"model","created":1773619200,"#,
            r#""owned_by":"xai","type":"xai","display_name":"Grok Voice TTS 1.0","#,
            r#""name":"grok-voice-tts-1.0","#,
            r#""description":"xAI Grok unary text-to-speech model."}"#,
        ))
    );
}

/// Not upstream's: `gemini-interactions` lists the Gemini models, as
/// upstream's does, on both routes.
#[tokio::test]
async fn gemini_interactions_lists_the_gemini_models() {
    let api = Api::new();
    let gemini = api
        .get("/v0/management/model-definitions/gemini")
        .await
        .expect(StatusCode::OK);
    assert!(!ids(&gemini).is_empty());
    for path in [
        "/v0/management/model-definitions/gemini-interactions",
        "/v8/management/routing/model-definitions/Gemini-Interactions",
    ] {
        let body = api.get(path).await.expect(StatusCode::OK);
        assert_eq!(body["channel"], "gemini-interactions", "{path}");
        assert_eq!(body["models"], gemini["models"], "{path}");
    }
}

/// Not upstream's: the channel is trimmed and taken in any case, and named
/// in lower case; a blank one is taken from the query.
#[tokio::test]
async fn channels_are_trimmed_and_taken_in_any_case() {
    let api = Api::new();
    let codex = StaticCatalog::embedded().models_for_channel("codex");
    let codex: Vec<&str> = codex.iter().map(|model| model.id.as_str()).collect();
    for path in [
        "/v0/management/model-definitions/%20Codex%20",
        "/v0/management/model-definitions/CODEX",
        "/v0/management/model-definitions/%20?channel=codex",
    ] {
        let body = api.get(path).await.expect(StatusCode::OK);
        assert_eq!(body["channel"], "codex", "{path}");
        assert_eq!(ids(&body), codex, "{path}");
    }
    let body = api
        .get("/v0/management/model-definitions/%20?channel=Vertex")
        .await
        .expect(StatusCode::OK);
    assert_eq!(body["channel"], "vertex");
}

/// Not upstream's: a blank channel and one the catalog doesn't know are
/// answered with a 400; the unknown one is named as it was sent, trimmed.
#[tokio::test]
async fn unknown_channels_are_refused() {
    let api = Api::new();
    for path in [
        "/v0/management/model-definitions/%20",
        "/v0/management/model-definitions/%20?channel=%20",
    ] {
        let answer = api.get(path).await;
        answer.assert(
            StatusCode::BAD_REQUEST,
            r#"{"error":"channel is required"}"#,
        );
    }
    for (path, channel) in [
        ("Codex-Pro", "Codex-Pro"),
        ("gemini-cli", "gemini-cli"),
        ("xai-grok", "xai-grok"),
        ("%20Kimi%20", "Kimi"),
        ("kimi-ai", "kimi-ai"),
        ("kimi.ai", "kimi.ai"),
        ("kimi.com", "kimi.com"),
        ("aistudio", "aistudio"),
        ("antigravity", "antigravity"),
        ("devin", "devin"),
    ] {
        let answer = api
            .get(&format!("/v0/management/model-definitions/{path}"))
            .await;
        let want = format!(r#"{{"channel":"{channel}","error":"unknown channel"}}"#);
        answer.assert(StatusCode::BAD_REQUEST, &want);
    }
}

/// Not upstream's: the v8 route answers as the v0 one, both take the key,
/// and writes aren't routes.
#[tokio::test]
async fn both_routes_answer_alike() {
    let api = Api::new();
    let v0 = api.get("/v0/management/model-definitions/claude").await;
    let v8 = api
        .get("/v8/management/routing/model-definitions/claude")
        .await;
    assert_eq!(v0.status, StatusCode::OK);
    assert_eq!((v8.status, &v8.body), (v0.status, &v0.body));
    for path in [
        "/v0/management/model-definitions/claude",
        "/v8/management/routing/model-definitions/claude",
    ] {
        let request = request_from(LOCAL, Method::GET, path, "");
        assert_eq!(api.send(request).await.status, StatusCode::UNAUTHORIZED);
        let answer = api.send(super::keyed(Method::PUT, path, "{}")).await;
        assert_eq!(
            (answer.status, answer.body.as_str()),
            (StatusCode::NOT_FOUND, "")
        );
    }
}
