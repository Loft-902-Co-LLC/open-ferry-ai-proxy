//! `GET /client-setup`, from the config and the model registry.

use http::StatusCode;
use open_ferry_core::models::ModelInfo;
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
/// its providers in order of preference and its limits where known.
#[tokio::test]
async fn routes_and_models_come_from_the_registry() {
    let dash = Dash::new();
    let gpt = ModelInfo {
        context_length: 400_000,
        max_completion_tokens: 128_000,
        ..info("gpt-5", "openai")
    };
    let claude = ModelInfo {
        display_name: "Claude Sonnet 4.5".to_owned(),
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
                "context_length": 200_000,
                "max_output_tokens": 64_000,
            },
            {
                "id": "gemini-2.5-pro",
                "display_name": "gemini-2.5-pro",
                "owned_by": "google",
                "providers": ["gemini"],
                "context_length": 1_000_000,
                "max_output_tokens": null,
            },
            {
                "id": "gpt-5",
                "display_name": "gpt-5",
                "owned_by": "openai",
                "providers": ["codex"],
                "context_length": 400_000,
                "max_output_tokens": 128_000,
            },
        ])
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
