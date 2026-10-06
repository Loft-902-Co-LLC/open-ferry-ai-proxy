//! `GET /client-setup`, from the config and the model registry.

use http::StatusCode;
use open_ferry_core::auth::compat::OPENAI_COMPATIBILITY;
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::registration::OPENAI_IMAGE_MODEL_TYPE;
use serde_json::{Value, json};

use super::{Dash, keyed_config};

const SETUP: &str = "/open-ferry/api/v1/client-setup";

/// A model `id` of `owned_by`.
fn info(id: &str, owned_by: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        owned_by: owned_by.to_owned(),
        object: "model".to_owned(),
        ..ModelInfo::default()
    }
}

/// The routes of `setup` with `models` on each, in `id` order.
fn routes(models: &[&str]) -> Value {
    json!([
        {"id": "claude-messages", "protocol": "claude", "method": "POST", "path": "/v1/messages", "base_path": "", "models": models},
        {"id": "codex-responses", "protocol": "codex", "method": "POST", "path": "/backend-api/codex/responses", "base_path": "/backend-api/codex", "models": models},
        {"id": "gemini-generate-content", "protocol": "gemini", "method": "POST", "path": "/v1beta/models/{model}:generateContent", "base_path": "", "models": models},
        {"id": "openai-chat-completions", "protocol": "openai", "method": "POST", "path": "/v1/chat/completions", "base_path": "/v1", "models": models},
        {"id": "openai-responses", "protocol": "openai-responses", "method": "POST", "path": "/v1/responses", "base_path": "/v1", "models": models},
    ])
}

/// Not upstream's: each route lists the models with a credential, less
/// those only the image endpoints serve, and each model is described with
/// its providers in order of preference, when it came out and its limits
/// where known.
#[tokio::test]
async fn routes_and_models_come_from_the_registry() {
    let dash = Dash::new();
    let gpt = ModelInfo {
        created: 1_754_524_800,
        context_length: 400_000,
        max_completion_tokens: 128_000,
        ..info("gpt-5", "openai")
    };
    let claude = ModelInfo {
        display_name: "Claude Sonnet 4.5".to_owned(),
        created: 1_759_104_000,
        input_token_limit: 200_000,
        output_token_limit: 64_000,
        ..info("claude-sonnet-4-5", "anthropic")
    };
    let gemini = ModelInfo {
        max_context_length: 1_000_000,
        context_length: 2_000_000,
        ..info("gemini-2.5-pro", "google")
    };
    let registry = dash.registry();
    registry.register_client("codex-1", "codex", &[gpt, info("gpt-image-2", "openai")]);
    registry.register_client("claude-1", "claude", std::slice::from_ref(&claude));
    registry.register_client("claude-2", "claude", std::slice::from_ref(&claude));
    registry.register_client("vertex-1", "antigravity", &[claude]);
    registry.register_client("gemini-1", "gemini", &[gemini]);

    let setup = dash.get(SETUP).await.json(StatusCode::OK);
    assert_eq!(
        setup["routes"],
        routes(&["claude-sonnet-4-5", "gemini-2.5-pro", "gpt-5"])
    );
    assert_eq!(
        setup["models"],
        json!([
            {
                "id": "claude-sonnet-4-5",
                "display_name": "Claude Sonnet 4.5",
                "owned_by": "anthropic",
                "providers": ["claude", "antigravity"],
                "created": 1_759_104_000,
                "chat": true,
                "context_length": 200_000,
                "max_output_tokens": 64_000,
            },
            {
                "id": "gemini-2.5-pro",
                "display_name": "gemini-2.5-pro",
                "owned_by": "google",
                "providers": ["gemini"],
                "created": null,
                "chat": true,
                "context_length": 1_000_000,
                "max_output_tokens": null,
            },
            {
                "id": "gpt-5",
                "display_name": "gpt-5",
                "owned_by": "openai",
                "providers": ["codex"],
                "created": 1_754_524_800,
                "chat": true,
                "context_length": 400_000,
                "max_output_tokens": 128_000,
            },
        ])
    );
}

/// Not upstream's: a model has the catalog's `created`, but none when it
/// is defined in the config, where `created` is the time the config was
/// loaded; and only a model that answers in text through `generateContent`,
/// and isn't an image or video model, is a chat model.
#[tokio::test]
async fn models_say_when_they_came_out_and_whether_they_chat() {
    let dash = Dash::new();
    let strings = |items: &[&str]| items.iter().map(|&item| item.to_owned()).collect();
    let dated = |id: &str, owned_by: &str| ModelInfo {
        created: 1_790_000_000,
        ..info(id, owned_by)
    };
    let gemini = |id: &str, outputs: &[&str], methods: &[&str]| ModelInfo {
        supported_output_modalities: strings(outputs),
        supported_generation_methods: strings(methods),
        ..dated(id, "google")
    };
    let registry = dash.registry();
    registry.register_client(
        "gemini-1",
        "gemini",
        &[
            gemini(
                "gemini-3.6-flash",
                &["TEXT"],
                &["generateContent", "countTokens"],
            ),
            gemini(
                "gemini-3-pro-image-preview",
                &["text", "image"],
                &["generateContent", "countTokens"],
            ),
            gemini("imagen-4.0-generate-001", &["image"], &["predict"]),
            gemini("gemini-embedding-001", &["text"], &["embedContent"]),
        ],
    );
    registry.register_client(
        "claude-1",
        "claude",
        &[
            dated("claude-sonnet-5-5", "anthropic"),
            ModelInfo {
                user_defined: true,
                model_type: "claude".to_owned(),
                ..dated("my-sonnet", "anthropic")
            },
        ],
    );
    registry.register_client(
        "compat-1",
        "openrouter",
        &[
            ModelInfo {
                model_type: OPENAI_COMPATIBILITY.to_owned(),
                ..dated("router-chat", "openrouter")
            },
            ModelInfo {
                model_type: OPENAI_IMAGE_MODEL_TYPE.to_owned(),
                ..dated("router-draw", "openrouter")
            },
        ],
    );
    registry.register_client("xai-1", "xai", &[dated("grok-imagine-video", "xai")]);

    let setup = dash.get(SETUP).await.json(StatusCode::OK);
    let Some(models) = setup["models"].as_array() else {
        panic!("models: {setup}");
    };
    let described: Vec<(&str, &Value, &Value)> = models
        .iter()
        .map(|model| {
            (
                model["id"].as_str().unwrap_or_default(),
                &model["created"],
                &model["chat"],
            )
        })
        .collect();
    let dated = json!(1_790_000_000);
    let (yes, no, null) = (json!(true), json!(false), Value::Null);
    assert_eq!(
        described,
        vec![
            ("claude-sonnet-5-5", &dated, &yes),
            ("gemini-3-pro-image-preview", &dated, &no),
            ("gemini-3.6-flash", &dated, &yes),
            ("gemini-embedding-001", &dated, &no),
            ("grok-imagine-video", &dated, &no),
            ("imagen-4.0-generate-001", &dated, &no),
            ("my-sonnet", &null, &yes),
            ("router-chat", &null, &yes),
            ("router-draw", &null, &no),
        ]
    );
}

/// Not upstream's: the base URLs, `tls` and `safe_mode` come from the
/// config; without models, each route lists none.
#[tokio::test]
async fn the_config_gives_the_roots_and_safe_mode() {
    let mut config = keyed_config();
    config.port = 8317;
    let dash = Dash::with_config(config);
    let setup = dash.get(SETUP).await.json(StatusCode::OK);
    assert_eq!(
        setup,
        json!({
            "base_urls": [
                {"url": "http://127.0.0.1:8317", "source": "listen"},
                {"url": "http://[::1]:8317", "source": "listen"},
                {"url": "http://localhost:8317", "source": "listen"},
            ],
            "tls": false,
            "safe_mode": false,
            "routes": routes(&[]),
            "models": [],
        })
    );

    let mut config = keyed_config();
    config.host = "0.0.0.0".to_owned();
    config.port = 9443;
    config.tls.enable = true;
    config.remote_management.base_url = "https://proxy.example.com/".to_owned();
    config.api_keys = vec!["your-api-key-1".to_owned(), "sk-real".to_owned()];
    let dash = Dash::with_config(config);
    let setup = dash.get(SETUP).await.json(StatusCode::OK);
    assert_eq!(
        setup["base_urls"],
        json!([
            {"url": "https://127.0.0.1:9443", "source": "listen"},
            {"url": "https://localhost:9443", "source": "listen"},
            {"url": "https://proxy.example.com", "source": "config"},
        ])
    );
    assert_eq!(setup["tls"], true);
    assert_eq!(setup["safe_mode"], true);
}
