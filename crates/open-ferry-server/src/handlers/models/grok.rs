// Ported from CLIProxyAPI internal/client/grokbuild/grokbuild.go
// (IsGrokShellUserAgent, BuildResponse) and internal/api/server_routes.go
// (the Grok branch of unifiedModelsHandler, grokModelsFromRegistryInfos and
// handleGrokModels) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The model list the Grok shell gets from `GET /v1/models`.
//!
//! A request whose `User-Agent` contains `grok-shell`, in any case, gets this
//! list instead of the OpenAI, Anthropic or Codex one. The Grok pager's agent
//! names the shell too, so it gets the list as well; an agent that names the
//! pager alone doesn't. The format is chosen by reading the client's own
//! agent, and nothing about it is sent to any upstream. The list comes from
//! the models the registry serves, sorted by ID:
//!
//! ```text
//! {"object":"list","data":[{"id":"grok-4","model":"grok-4","name":"Grok 4",
//!   "context_window":256000,"api_backend":"responses","supported_in_api":true,
//!   "reasoning_efforts":[{"value":"high"}]}]}
//! ```
//!
//! `model` repeats `id`, and `name` is the display name, or the ID for a
//! model without one. `context_window` is the model's context length, left
//! out when that is unknown (0). `reasoning_efforts` has the model's named
//! thinking levels, trimmed and without the blank ones, and is left out when
//! there are none. `api_backend` and `supported_in_api` are the same for
//! every model.
//!
//! The Grok shell's `keepalive` comments on a Codex stream are not made
//! here: they are part of the Codex executor's stream, which is the only
//! place upstream makes them (`codex::stream` in `open-ferry-providers`).
//!
//! Deviations from upstream:
//! - Home mode isn't ported, so there is no list made from Home's models.
//! - The list doesn't pass through plugin response interceptors, which
//!   aren't ported either (upstream's `WriteModelListResponse`).

use http::{HeaderMap, header};
use open_ferry_core::models::ModelInfo;
use open_ferry_translate::go;
use serde_json::{Map, Value, json};

/// Whether the client is a Grok shell, by its user agent
/// (`grokbuild.IsGrokShellUserAgent`).
pub(super) fn is_grok_shell(headers: &HeaderMap) -> bool {
    headers.get(header::USER_AGENT).is_some_and(|agent| {
        go::to_lower(&String::from_utf8_lossy(agent.as_bytes())).contains("grok-shell")
    })
}

/// The list for `models`, in the order given (`grokbuild.BuildResponse`).
pub(super) fn list(models: Vec<ModelInfo>) -> Value {
    let data: Vec<Value> = models.into_iter().map(entry).collect();
    json!({"object": "list", "data": data})
}

/// One model's entry, with the keys in the order upstream's struct writes
/// them.
fn entry(model: ModelInfo) -> Value {
    let efforts: Vec<Value> = model
        .thinking
        .iter()
        .flat_map(|thinking| &thinking.levels)
        .map(|level| level.trim())
        .filter(|level| !level.is_empty())
        .map(|level| json!({"value": level}))
        .collect();
    let name = if model.display_name.is_empty() {
        model.id.clone()
    } else {
        model.display_name
    };
    let mut entry = Map::new();
    entry.insert("id".into(), model.id.clone().into());
    entry.insert("model".into(), model.id.into());
    entry.insert("name".into(), name.into());
    if model.context_length > 0 {
        entry.insert("context_window".into(), model.context_length.into());
    }
    entry.insert("api_backend".into(), "responses".into());
    entry.insert("supported_in_api".into(), true.into());
    if !efforts.is_empty() {
        entry.insert("reasoning_efforts".into(), efforts.into());
    }
    Value::Object(entry)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use http::{HeaderValue, Request, StatusCode};
    use http_body_util::BodyExt;
    use open_ferry_core::models::ThinkingSupport;
    use open_ferry_core::registry::ModelRegistry;
    use tower::ServiceExt;

    use super::*;
    use crate::config::ServerConfig;
    use crate::state::AppState;
    use crate::testing::FakeDispatcher;

    const SHELL_AGENT: &str = "grok-shell/0.2.119 (macos; aarch64)";
    const PAGER_AND_SHELL_AGENT: &str = "grok-pager/0.2.119 grok-shell/0.2.119 (macos; aarch64)";

    fn agent(user_agent: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static(user_agent));
        headers
    }

    fn levels(levels: &[&str]) -> Option<ThinkingSupport> {
        Some(ThinkingSupport {
            levels: levels.iter().map(|&level| level.to_owned()).collect(),
            ..ThinkingSupport::default()
        })
    }

    /// A server serving `registry`'s models, with the key `test-key`.
    fn state(registry: &Arc<ModelRegistry>) -> AppState {
        let config = ServerConfig {
            api_keys: vec!["test-key".into()],
            ..ServerConfig::default()
        };
        AppState::new(config, FakeDispatcher::new([]), Arc::clone(registry) as _)
    }

    /// The parsed body of a successful `GET uri` sent with the key, the
    /// agent and `extra` headers.
    async fn get(state: &AppState, uri: &str, user_agent: &str, extra: &[(&str, &str)]) -> Value {
        let mut request = Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer test-key")
            .header(header::USER_AGENT, user_agent);
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = crate::router(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json; charset=utf-8"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// The entry for `id` in a list's `data`.
    fn find<'a>(list: &'a Value, id: &str) -> &'a Value {
        list["data"]
            .as_array()
            .expect("data is an array")
            .iter()
            .find(|entry| entry["id"] == id)
            .unwrap_or_else(|| panic!("registered model {id} missing: {list}"))
    }

    // TestIsGrokShellUserAgent.
    #[test]
    fn detects_the_grok_shell_by_its_agent() {
        for (name, agent_text, want) in [
            ("shell", SHELL_AGENT, true),
            ("pager", PAGER_AND_SHELL_AGENT, true),
            ("case insensitive", "GROK-PAGER/1.0 GROK-SHELL/1.0", true),
            ("ordinary client", "curl/8.7.1", false),
            // Not upstream's: the pager alone isn't the shell, and a request
            // with no agent is no client of either.
            ("pager alone", "grok-pager/1.0.5", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::USER_AGENT,
                HeaderValue::from_str(agent_text).unwrap(),
            );
            assert_eq!(is_grok_shell(&headers), want, "{name}: {agent_text}");
        }
        assert!(!is_grok_shell(&HeaderMap::new()));
        assert!(is_grok_shell(&agent("Mozilla/5.0 grok-shell/1")));
    }

    // TestBuildResponse, which also checks the keys' order.
    #[test]
    fn builds_the_grok_list() {
        let list = list(vec![
            ModelInfo {
                id: "grok-4".into(),
                display_name: "Grok 4".into(),
                context_length: 256_000,
                thinking: levels(&["high"]),
                ..ModelInfo::default()
            },
            ModelInfo {
                id: "plain-model".into(),
                ..ModelInfo::default()
            },
        ]);
        assert_eq!(list["object"], "list");
        assert_eq!(list["data"].as_array().map(Vec::len), Some(2));
        let entry = &list["data"][0];
        assert_eq!(entry["id"], "grok-4");
        assert_eq!(entry["model"], "grok-4");
        assert_eq!(entry["name"], "Grok 4");
        assert_eq!(entry["context_window"], 256_000);
        assert_eq!(entry["api_backend"], "responses");
        assert_eq!(entry["supported_in_api"], true);
        assert_eq!(entry["reasoning_efforts"], json!([{"value": "high"}]));
        // The name falls back to the ID; a model without a context length or
        // levels has neither key.
        let plain = list["data"][1].as_object().expect("an object");
        assert_eq!(plain["name"], "plain-model");
        assert!(!plain.contains_key("context_window"));
        assert!(!plain.contains_key("reasoning_efforts"));

        assert_eq!(
            list.to_string(),
            concat!(
                r#"{"object":"list","data":["#,
                r#"{"id":"grok-4","model":"grok-4","name":"Grok 4","context_window":256000,"api_backend":"responses","supported_in_api":true,"reasoning_efforts":[{"value":"high"}]},"#,
                r#"{"id":"plain-model","model":"plain-model","name":"plain-model","api_backend":"responses","supported_in_api":true}"#,
                r#"]}"#
            )
        );
    }

    // Not upstream's: BuildResponse trims each reasoning level, drops the
    // blank ones and leaves the key out when none are left.
    #[test]
    fn trims_reasoning_levels_and_drops_blank_ones() {
        let list = list(vec![
            ModelInfo {
                id: "a".into(),
                thinking: levels(&[" low ", "", "  ", "\thigh\n"]),
                ..ModelInfo::default()
            },
            ModelInfo {
                id: "b".into(),
                thinking: levels(&["", " "]),
                ..ModelInfo::default()
            },
            ModelInfo {
                id: "c".into(),
                thinking: Some(ThinkingSupport::default()),
                ..ModelInfo::default()
            },
        ]);
        assert_eq!(
            list["data"][0]["reasoning_efforts"],
            json!([{"value": "low"}, {"value": "high"}])
        );
        assert!(list["data"][1].get("reasoning_efforts").is_none());
        assert!(list["data"][2].get("reasoning_efforts").is_none());
        assert_eq!(
            super::list(Vec::new()).to_string(),
            r#"{"object":"list","data":[]}"#
        );
    }

    // TestModelsDispatchByGrokShellUserAgent.
    #[tokio::test]
    async fn dispatches_by_grok_shell_user_agent() {
        let registry = Arc::new(ModelRegistry::new());
        registry.register_client(
            "test-grok-shell-model-list",
            "openai",
            &[ModelInfo {
                id: "grok-shell-openai-model".into(),
                display_name: "Grok Shell Model".into(),
                context_length: 256_000,
                thinking: levels(&["high"]),
                ..ModelInfo::default()
            }],
        );
        registry.register_client(
            "test-grok-shell-model-list-claude",
            "claude",
            &[ModelInfo {
                id: "grok-shell-claude-model".into(),
                display_name: "Claude Catalog Model".into(),
                context_length: 200_000,
                ..ModelInfo::default()
            }],
        );
        let state = state(&registry);
        for user_agent in [SHELL_AGENT, PAGER_AND_SHELL_AGENT] {
            let list = get(&state, "/v1/models?client_version", user_agent, &[]).await;
            assert_eq!(list["object"], "list", "{user_agent}");

            let openai = find(&list, "grok-shell-openai-model");
            assert_eq!(openai["model"], openai["id"], "{openai}");
            assert_eq!(openai["name"], "Grok Shell Model");
            assert_eq!(openai["context_window"], 256_000);
            assert_eq!(openai["api_backend"], "responses");
            assert_eq!(openai["supported_in_api"], true);
            assert_eq!(openai["reasoning_efforts"], json!([{"value": "high"}]));

            let claude = find(&list, "grok-shell-claude-model");
            assert_eq!(claude["model"], claude["id"], "{claude}");
            assert_eq!(claude["name"], "Claude Catalog Model");
            assert_eq!(claude["context_window"], 200_000);
            assert!(claude.get("reasoning_efforts").is_none(), "{claude}");
        }
    }

    // TestModelsDispatchKeepsOrdinaryOpenAIResponse.
    #[tokio::test]
    async fn keeps_the_ordinary_openai_list() {
        let registry = Arc::new(ModelRegistry::new());
        registry.register_client(
            "test-ordinary-model-list-after-grok",
            "openai",
            &[ModelInfo {
                id: "ordinary-model".into(),
                ..ModelInfo::default()
            }],
        );
        let list = get(&state(&registry), "/v1/models", "curl/8.7.1", &[]).await;
        assert_eq!(list["object"], "list");
        for entry in list["data"].as_array().expect("data is an array") {
            assert!(
                entry.get("api_backend").is_none(),
                "ordinary response contains a Grok field: {entry}"
            );
        }
        find(&list, "ordinary-model");
    }

    // Not upstream's: the Grok branch comes before the others, so the
    // shell's list wins over an Anthropic client's and a Codex client's,
    // which the pager alone, with no shell in its agent, still gets.
    #[tokio::test]
    async fn the_grok_list_comes_before_the_other_formats() {
        let registry = Arc::new(ModelRegistry::new());
        registry.register_client(
            "test-grok-shell-dispatch-order",
            "openai",
            &[ModelInfo {
                id: "ordered-model".into(),
                owned_by: "openai".into(),
                ..ModelInfo::default()
            }],
        );
        let state = state(&registry);

        let anthropic = [("anthropic-version", "2023-06-01")];
        let list = get(&state, "/v1/models", SHELL_AGENT, &anthropic).await;
        assert_eq!(find(&list, "ordered-model")["api_backend"], "responses");
        assert!(list.get("first_id").is_none(), "{list}");
        let list = get(&state, "/v1/models", "grok-pager/1.0.5", &anthropic).await;
        assert!(list.get("first_id").is_some(), "{list}");

        let list = get(
            &state,
            "/v1/models?client_version=0.149.1",
            "grok-pager/1.0.5",
            &[],
        )
        .await;
        assert!(list.get("models").is_some(), "{list}");
        assert!(list.get("object").is_none(), "{list}");
        let list = get(
            &state,
            "/v1/models?client_version=0.149.1",
            SHELL_AGENT,
            &[],
        )
        .await;
        assert!(list.get("models").is_none(), "{list}");
        assert_eq!(list["object"], "list");
    }
}
