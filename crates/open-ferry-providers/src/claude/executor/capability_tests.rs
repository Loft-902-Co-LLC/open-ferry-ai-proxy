//! Selected-credential thinking through real registration and alias routing.

use open_ferry_core::auth::Status;
use open_ferry_core::exec::Dispatcher as _;
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::registry::{ModelRegistry, RegistrationRules};

use super::*;

fn api_auth() -> Auth {
    let mut auth = (*api_key_auth()).clone();
    auth.id = "configured-api".into();
    auth.status = Status::Active;
    for (key, value) in [
        ("auth_kind", "apikey"),
        ("source", "config:claude[test]"),
        ("config_index", "0"),
    ] {
        auth.attributes.insert(key.into(), value.into());
    }
    auth
}

fn manager(mock: &Mock, config: Config, oauth_last: bool) -> Manager {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Manager::new(Settings::from(&config), registry.clone(), None);
    let rules = RegistrationRules::from(&config);
    manager.register_executor(Arc::new(
        mock.executor()
            .with_config(Arc::new(config))
            .with_models(registry.clone()),
    ));
    let mut oauth = (*oauth_auth()).clone();
    oauth.id = "oauth".into();
    oauth.status = Status::Active;
    oauth.attributes.insert("auth_kind".into(), "oauth".into());
    let api = api_auth();
    for auth in if oauth_last {
        [api, oauth]
    } else {
        [oauth, api]
    } {
        registry.register_auth(&auth, &rules);
        manager.register(auth).unwrap();
    }
    manager
}

async fn call(manager: &Manager, auth: &str, model: &str, payload: Value, format: Format) {
    let mut opts = options(format);
    opts.metadata.pinned_auth_id = Some(auth.into());
    manager
        .execute(
            &["claude".into()],
            Request {
                model: model.into(),
                payload: Bytes::from(payload.to_string()),
            },
            opts,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn selected_credential_thinking_is_independent_of_registration_order() {
    for oauth_last in [false, true] {
        let mock = Mock::start(Reply::sse(SSE)).await;
        let config = Config::parse(format!(
            "claude-api-key: [{{api-key: {API_KEY}, models: [{{name: claude-opus-4-8, thinking: {{levels: [low, medium]}}}}]}}]"
        )).unwrap();
        let manager = manager(&mock, config, oauth_last);
        for (auth, expected) in [("oauth", "xhigh"), ("configured-api", "medium")] {
            call(
                &manager,
                auth,
                "claude-opus-4-8",
                json!({
                    "model": "claude-opus-4-8", "messages": [{"role":"user", "content":"hi"}],
                    "reasoning_effort": "xhigh"
                }),
                Format::OPENAI,
            )
            .await;
            assert_eq!(
                mock.last().json()["output_config"]["effort"],
                expected,
                "oauth_last={oauth_last}, auth={auth}"
            );
        }
    }
}

#[tokio::test]
async fn registered_claude_disable_type_reaches_the_wire_after_alias_rewrite() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let config = Config::parse(format!(
        "claude-api-key: [{{api-key: {API_KEY}, models: [{{name: custom-claude, alias: fast, thinking: {{levels: [low, none], disable-type: between_tools}}}}]}}]"
    )).unwrap();
    let manager = manager(&mock, config, false);
    call(&manager, "configured-api", "fast", json!({"model":"fast", "thinking":{"type":"disabled", "display":"summarized", "block_binding":{}}, "messages":[{"role":"user", "content":"hi"}]}), Format::CLAUDE).await;
    let wire = mock.last().json();
    assert_eq!(wire["model"], "custom-claude");
    assert_eq!(wire["thinking"], json!({"type":"between_tools"}));
}

#[tokio::test]
async fn aliases_of_one_upstream_name_keep_their_own_capabilities() {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let config = Config::parse(format!(
        "claude-api-key: [{{api-key: {API_KEY}, models: [{{name: custom-claude, alias: fast, thinking: {{levels: [low]}}}}, {{name: custom-claude, alias: deep, thinking: {{levels: [high]}}}}]}}]"
    )).unwrap();
    let manager = manager(&mock, config, false);
    for (alias, expected) in [("deep", "high"), ("fast", "low")] {
        call(&manager, "configured-api", alias, json!({"model":alias, "messages":[{"role":"user", "content":"hi"}], "reasoning_effort":"high"}), Format::OPENAI).await;
        let wire = mock.last().json();
        assert_eq!(wire["model"], "custom-claude");
        assert_eq!(wire["output_config"]["effort"], expected);
    }
}

#[tokio::test]
async fn bound_unknown_model_without_thinking_strips_it_after_alias_rewrite() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let config = Config::parse(format!(
        "claude-api-key: [{{api-key: {API_KEY}, models: [{{name: private-model, alias: private}}]}}]"
    )).unwrap();
    let manager = manager(&mock, config, true);
    call(&manager, "configured-api", "private", json!({"model":"private", "thinking":{"type":"adaptive"}, "output_config":{"effort":"high"}, "messages":[{"role":"user", "content":"hi"}]}), Format::CLAUDE).await;
    let wire = mock.last().json();
    assert_eq!(wire["model"], "private-model");
    assert!(wire.get("thinking").is_none());
    assert!(wire.get("output_config").is_none());
}
