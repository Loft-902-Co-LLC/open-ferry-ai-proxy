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
    let mut api = api_auth();
    api.prefix = config
        .claude_api_key
        .first()
        .map(|key| key.prefix.clone())
        .unwrap_or_default();
    manager.register_executor(Arc::new(
        mock.executor()
            .with_config(Arc::new(config))
            .with_models(registry.clone()),
    ));
    let mut oauth = (*oauth_auth()).clone();
    oauth.id = "oauth".into();
    oauth.status = Status::Active;
    oauth.attributes.insert("auth_kind".into(), "oauth".into());
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
    // Call::routed keeps the raw client model while Request holds the route.
    opts.metadata.requested_model = payload["model"].as_str().unwrap_or(model).into();
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
async fn bound_api_capabilities_are_stable_and_unbound_oauth_follows_registry_order() {
    for oauth_last in [false, true] {
        let mock = Mock::start(Reply::sse(SSE)).await;
        let config = Config::parse(format!(
            "claude-api-key: [{{api-key: {API_KEY}, models: [{{name: claude-opus-4-8, thinking: {{levels: [low, medium]}}}}]}}]"
        )).unwrap();
        let manager = manager(&mock, config, oauth_last);
        let mut opts = options(Format::CLAUDE);
        opts.metadata.requested_model = "auto".into();
        opts.metadata.pinned_auth_id = Some("oauth".into());
        let result = manager.execute(&["claude".into()], Request {
            model: "claude-opus-4-8".into(),
            payload: Bytes::from(json!({"model":"auto", "messages":[{"role":"user", "content":"hi"}], "thinking":{"type":"adaptive"}, "output_config":{"effort":"minimal"}}).to_string()),
        }, opts).await;
        if oauth_last {
            assert_eq!(result.unwrap_err().status, 400);
        } else {
            result.unwrap();
            assert_eq!(mock.last().json()["output_config"]["effort"], "minimal");
        }
        call(&manager, "configured-api", "claude-opus-4-8", json!({"model":"auto", "messages":[{"role":"user", "content":"hi"}], "reasoning_effort":"high"}), Format::OPENAI).await;
        assert_eq!(
            mock.last().json()["output_config"]["effort"],
            "medium",
            "oauth_last={oauth_last}"
        );
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

#[tokio::test]
async fn auto_route_uses_configured_alias_capabilities() {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let config = Config::parse(format!(
        "claude-api-key: [{{api-key: {API_KEY}, models: [{{name: custom-claude, alias: fast, thinking: {{levels: [low]}}}}]}}]"
    )).unwrap();
    let manager = manager(&mock, config, false);
    // The server resolves auto to fast before Manager::execute, but retains
    // auto in requested_model and the original request body.
    call(&manager, "configured-api", "fast", json!({"model":"auto", "messages":[{"role":"user", "content":"hi"}], "reasoning_effort":"high"}), Format::OPENAI).await;
    assert_eq!(mock.last().json()["output_config"]["effort"], "low");
}

#[tokio::test]
async fn auto_route_strips_unconfigured_thinking_and_applies_disable_type() {
    for (thinking, expected) in [
        ("", None),
        (
            ", thinking: {levels: [low, none], disable-type: between_tools}",
            Some("between_tools"),
        ),
    ] {
        let mock = Mock::start(Reply::json(MESSAGE)).await;
        let config = Config::parse(format!("claude-api-key: [{{api-key: {API_KEY}, models: [{{name: private-model, alias: private{thinking}}}]}}]")).unwrap();
        let manager = manager(&mock, config, false);
        call(&manager, "configured-api", "private", json!({"model":"auto", "messages":[{"role":"user", "content":"hi"}], "thinking":{"type":"disabled"}, "output_config":{"effort":"high"}}), Format::CLAUDE).await;
        let body = mock.last().json();
        assert_eq!(body["model"], "private-model");
        assert_eq!(body["thinking"]["type"].as_str(), expected);
        assert!(body.get("output_config").is_none());
    }
}

#[tokio::test]
async fn exact_and_nested_route_suffixes_keep_first_capability_per_key() {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let config = Config::parse(format!("claude-api-key: [{{api-key: {API_KEY}, prefix: tenant, models: [{{name: custom-claude, alias: fast, thinking: {{levels: [low]}}}}, {{name: custom-claude, alias: 'fast(high)', thinking: {{levels: [high]}}}}, {{name: custom-claude, alias: 'fast(high)', thinking: {{levels: [max]}}}}]}}]")).unwrap();
    let manager = manager(&mock, config, false);
    for route in ["tenant/fast(high)", "tenant/fast(high)(low)"] {
        call(&manager, "configured-api", route, json!({"model":"models/fast(high)", "messages":[{"role":"user", "content":"hi"}], "reasoning_effort":"high"}), Format::OPENAI).await;
        assert_eq!(
            mock.last().json()["output_config"]["effort"],
            "high",
            "{route}"
        );
    }
}

#[tokio::test]
async fn capabilities_use_the_alias_snapshot_during_either_reload_order() {
    check_alias_snapshot_during_either_reload_order(false).await;
}

#[tokio::test]
async fn streaming_capabilities_use_the_alias_snapshot_during_either_reload_order() {
    check_alias_snapshot_during_either_reload_order(true).await;
}

async fn check_alias_snapshot_during_either_reload_order(streaming: bool) {
    for settings_first in [false, true] {
        let mock = Mock::start(Reply::sse(SSE)).await;
        let config = |name: &str, level: &str| {
            Config::parse(format!("claude-api-key: [{{api-key: {API_KEY}, models: [{{name: {name}, alias: fast, thinking: {{levels: [{level}]}}}}]}}]")).unwrap()
        };
        let manager = manager(&mock, config("custom-a", "low"), false);
        let next = config("custom-b", "high");
        if settings_first {
            manager.set_settings(Settings::from(&next));
        } else {
            manager.register_executor(Arc::new(mock.executor().with_config(Arc::new(next))));
        }
        if streaming {
            let mut opts = options(Format::OPENAI);
            opts.metadata.requested_model = "auto".into();
            opts.metadata.pinned_auth_id = Some("configured-api".into());
            let request = Request { model: "fast".into(), payload: Bytes::from(json!({"model":"auto", "messages":[{"role":"user", "content":"hi"}], "reasoning_effort":"high"}).to_string()) };
            for chunk in manager
                .execute_stream(&["claude".into()], request, opts)
                .await
                .unwrap()
                .chunks
                .collect::<Vec<_>>()
                .await
            {
                chunk.unwrap();
            }
        } else {
            call(&manager, "configured-api", "fast", json!({"model":"auto", "messages":[{"role":"user", "content":"hi"}], "reasoning_effort":"high"}), Format::OPENAI).await;
        }
        let wire = mock.last().json();
        assert_eq!(
            wire["model"],
            if settings_first {
                "custom-b"
            } else {
                "custom-a"
            }
        );
        assert_eq!(
            wire["output_config"]["effort"],
            if settings_first { "high" } else { "low" }
        );
    }
}

#[tokio::test]
async fn auto_route_capabilities_reach_streaming_and_token_counts() {
    for streaming in [false, true] {
        let mock = Mock::start(if streaming {
            Reply::sse(SSE)
        } else {
            Reply::json(r#"{"input_tokens":99}"#)
        })
        .await;
        let config = Config::parse(format!("claude-api-key: [{{api-key: {API_KEY}, models: [{{name: custom-claude, alias: fast, thinking: {{levels: [low]}}}}, {{name: custom-claude, alias: deep, thinking: {{levels: [high]}}}}]}}]")).unwrap();
        let manager = manager(&mock, config, false);
        let mut opts = options(Format::OPENAI);
        opts.metadata.requested_model = "auto".into();
        opts.metadata.pinned_auth_id = Some("configured-api".into());
        let request = Request { model: "deep".into(), payload: Bytes::from(json!({"model":"auto", "messages":[{"role":"user", "content":"hi"}], "reasoning_effort":"high"}).to_string()) };
        if streaming {
            for chunk in manager
                .execute_stream(&["claude".into()], request, opts)
                .await
                .unwrap()
                .chunks
                .collect::<Vec<_>>()
                .await
            {
                chunk.unwrap();
            }
        } else {
            manager
                .count_tokens(&["claude".into()], request, opts)
                .await
                .unwrap();
        }
        assert_eq!(mock.last().json()["output_config"]["effort"], "high");
    }
}

#[tokio::test]
async fn claude_55_forced_tools_follow_the_registered_upstream_alias() {
    for (upstream, alias, expected) in [
        ("claude-sonnet-5-5", "fast", "auto"),
        ("claude-sonnet-4-6", "claude-opus-5-5", "tool"),
    ] {
        let mock = Mock::start(Reply::sse(SSE)).await;
        let config = Config::parse(format!("claude-api-key: [{{api-key: {API_KEY}, models: [{{name: {upstream}, alias: {alias}}}]}}]")).unwrap();
        let manager = manager(&mock, config, false);
        call(&manager, "configured-api", alias, json!({"model":alias,"messages":[{"role":"user","content":"hi"}],"tool_choice":{"type":"tool","name":"lookup"}}), Format::CLAUDE).await;
        let body = mock.last().json();
        assert_eq!(body["model"], upstream);
        assert_eq!(body["tool_choice"]["type"], expected);
    }
}
