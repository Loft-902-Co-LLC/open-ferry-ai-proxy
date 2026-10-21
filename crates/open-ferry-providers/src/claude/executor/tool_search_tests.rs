// Ported from CLIProxyAPI internal/runtime/executor/claude_executor_tool_search_test.go,
// claude_executor_tool_search_disabled_test.go, helps/responses_tool_search_test.go
// and helps/claude_tool_search_model_test.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use crate::claude::executor::tool_search::{model_lacks_tool_search, strip_responses_tool_search};
use open_ferry_core::config::ClaudeKey;

fn search_payload() -> Value {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../open-ferry-translate/src/claude/openai/responses/fixtures/tool_search_disabled/cases.json"
    )).unwrap();
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "codex_session")
        .unwrap()["request"]
        .clone()
}

fn search_config(enabled: bool) -> Arc<Config> {
    let mut config = Config::default();
    config.claude.tool_search = enabled;
    Arc::new(config)
}

fn search_sse() -> String {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../open-ferry-translate/src/claude/openai/responses/fixtures/tool_search_disabled/cases.json"
    )).unwrap();
    fixture["stream"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| {
            let mut event: Value =
                serde_json::from_str(line.as_str().unwrap().strip_prefix("data: ").unwrap())
                    .unwrap();
            if event["type"] == "message_start" {
                event["message"]["model"] = "claude-sonnet-4-5".into();
            }
            format!("data: {event}\n\n")
        })
        .collect::<Vec<_>>()
        .concat()
}

#[test]
fn native_search_support_checks_origin_model_flag_and_switch() {
    let executor = ClaudeExecutor::new("direct").with_config(search_config(true));
    assert!(executor.supports_native_tool_search(&api_key_auth(), "claude-sonnet-4-5"));
    assert!(executor.supports_native_tool_search(&oauth_auth(), "unknown-model"));
    for base in [
        "https://api.anthropic.com",
        "https://api.anthropic.com:443/v1",
    ] {
        assert!(executor.supports_native_tool_search(&gateway_auth(base), "new-model"));
    }
    for base in [
        "https://gateway.example.com",
        "https://api.anthropic.com.evil",
        "http://api.anthropic.com",
        "https://user@api.anthropic.com",
        "https://api.anthropic.com:444",
    ] {
        assert!(!executor.supports_native_tool_search(&gateway_auth(base), "new-model"));
    }
    let mut auth = (*gateway_auth("https://gateway.example.com")).clone();
    auth.attributes
        .insert("native_tool_search".into(), " TRUE ".into());
    assert!(executor.supports_native_tool_search(&auth, "new-model"));
    for model in [
        "claude-opus-4",
        "claude-opus-4-1-20250805",
        "CLAUDE-SONNET-4(high)",
        "team/claude-3-7-sonnet[free]",
        "claude-2.1",
        "claude-instant-1.2",
    ] {
        assert!(model_lacks_tool_search(model), "{model}");
        assert!(!executor.supports_native_tool_search(&auth, model));
    }
    for model in [
        "",
        "claude-opus-4-5",
        "claude-haiku-4-5",
        "unknown",
        "third-party/model",
    ] {
        assert!(!model_lacks_tool_search(model), "{model}");
    }
    let mut config = (*search_config(true)).clone();
    config.claude_api_key.push(ClaudeKey {
        api_key: API_KEY.into(),
        base_url: "https://gateway.example.com".into(),
        native_tool_search: true,
        ..ClaudeKey::default()
    });
    let configured = ClaudeExecutor::new("direct").with_config(Arc::new(config));
    assert!(
        configured
            .supports_native_tool_search(&gateway_auth("https://gateway.example.com"), "new-model")
    );
    assert!(
        !configured
            .supports_native_tool_search(&gateway_auth("https://other.example.com"), "new-model")
    );
    assert!(
        !ClaudeExecutor::new("direct")
            .with_config(search_config(false))
            .supports_native_tool_search(&auth, "new-model")
    );
}

#[test]
fn stripping_search_preserves_hosted_declarations_and_deferred_definitions() {
    let mut body = json!({"tools":[{"type":"tool_search","execution":"client"},
        {"type":"tool_search","execution":"server"},{"type":"function","name":"later","defer_loading":true}],
        "input":[{"type":"tool_search_call"},{"type":"tool_search_output","tools":[{"type":"function","name":"found"}]},
        {"type":"additional_tools","tools":[{"type":"tool_search","execution":"client"},{"type":"function","name":"extra"}]},
        {"role":"user","content":"hi"}]});
    strip_responses_tool_search(&mut body);
    assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    assert_eq!(body["tools"][0]["execution"], "server");
    assert_eq!(body["tools"][1]["defer_loading"], true);
    assert_eq!(body["input"].as_array().unwrap().len(), 2);
    assert_eq!(body["input"][0]["tools"].as_array().unwrap().len(), 1);
    let once = body.clone();
    strip_responses_tool_search(&mut body);
    assert_eq!(body, once);
}

#[test]
fn enabled_search_keeps_translation_refusals_request_scoped() {
    let request = request(
        json!({"input":[{"role":"user","content":[{"type":"input_audio","data":"unsupported"}]}],
        "tools":[{"type":"tool_search","execution":"client"}]}),
    );
    let error = translate_request(
        Some(&search_config(true)),
        &request,
        &options(Format::OPENAI_RESPONSE),
        "claude-sonnet-4-5",
        true,
    )
    .unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.request_scoped);
}

#[tokio::test]
async fn execute_stream_and_count_use_native_search_or_credential_fallback() {
    for enabled in [false, true] {
        for native in [false, true] {
            for path in ["execute", "stream", "count_tokens"] {
                let mock = Mock::start(if path == "count_tokens" {
                    Reply::json(r#"{"input_tokens":7}"#)
                } else {
                    Reply::sse(&search_sse())
                })
                .await;
                let executor = mock.executor().with_config(search_config(enabled));
                let mut auth = (*api_key_auth()).clone();
                // The mock is the first-party test seam. A legacy model makes
                // its credential ineligible without requiring a real gateway.
                let mut request = request(search_payload());
                if !native {
                    request.model = "claude-opus-4-1".into();
                }
                let mut options = options(Format::OPENAI_RESPONSE);
                options.original_request = request.payload.clone();
                let original_request = request.payload.clone();
                let original_options = options.original_request.clone();
                match path {
                    "execute" => {
                        let out = executor
                            .execute(Arc::new(auth.clone()), request.clone(), options.clone())
                            .await
                            .unwrap();
                        let body: Value = serde_json::from_slice(&out.payload).unwrap();
                        assert_eq!(
                            body["output"][0]["type"],
                            if enabled && native {
                                "tool_search_call"
                            } else {
                                "function_call"
                            }
                        );
                    }
                    "stream" => {
                        options.stream = true;
                        let out = executor
                            .execute_stream(
                                Arc::new(auth.clone()),
                                request.clone(),
                                options.clone(),
                            )
                            .await
                            .unwrap();
                        let chunks = out.chunks.collect::<Vec<_>>().await;
                        let data = chunks.into_iter().map(Result::unwrap).fold(
                            Vec::new(),
                            |mut data, chunk| {
                                data.extend(chunk);
                                data
                            },
                        );
                        let raw = String::from_utf8(data).unwrap();
                        assert_eq!(
                            raw.contains("\"type\":\"tool_search_call\""),
                            enabled && native
                        );
                    }
                    _ => {
                        let out = executor
                            .count_tokens(Arc::new(auth.clone()), request.clone(), options.clone())
                            .await
                            .unwrap();
                        assert!(serde_json::from_slice::<Value>(&out.payload).is_ok());
                    }
                }
                let body = mock.last().json();
                let tools = body["tools"].as_array().unwrap();
                assert_eq!(
                    tools.iter().any(|t| t["name"] == "tool_search"),
                    enabled && native,
                    "{enabled}/{native}/{path}"
                );
                assert_eq!(
                    tools.iter().any(|t| t["defer_loading"] == true),
                    enabled && native
                );
                assert_eq!(
                    body.to_string().contains("tool_reference"),
                    enabled && native
                );
                for tool in tools.iter().filter(|tool| tool["defer_loading"] == true) {
                    assert!(tool.get("cache_control").is_none());
                }
                assert_eq!(request.payload, original_request);
                assert_eq!(options.original_request, original_options);
                auth.attributes.insert("base_url".into(), mock.url.clone());
                if !native && enabled {
                    let (request, options) =
                        executor.tool_search_fallback(&auth, &request, &options);
                    assert!(
                        !String::from_utf8_lossy(&request.payload).contains("tool_search_call")
                    );
                    assert!(
                        !String::from_utf8_lossy(&options.original_request)
                            .contains("tool_search_output")
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn executor_switch_is_retained_when_config_is_replaced_during_a_stream() {
    for enabled in [true, false] {
        let mock = Mock::start(Reply::sse(&search_sse())).await;
        let executor = mock.executor().with_config(search_config(enabled));
        let request = request(search_payload());
        let mut options = stream_options(Format::OPENAI_RESPONSE);
        options.original_request = request.payload.clone();
        let stream = executor
            .execute_stream(api_key_auth(), request, options)
            .await
            .unwrap();
        // Reload replaces the immutable executor/config snapshot. The existing
        // stream owns its translator and must retain its original switch.
        let replacement = mock.executor().with_config(search_config(!enabled));
        assert_eq!(
            replacement.supports_native_tool_search(&api_key_auth(), "new-model"),
            !enabled
        );
        let data = stream
            .chunks
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .fold(Vec::new(), |mut data, chunk| {
                data.extend(chunk);
                data
            });
        assert_eq!(
            String::from_utf8(data)
                .unwrap()
                .contains("\"type\":\"tool_search_call\""),
            enabled
        );
    }
}

#[tokio::test]
async fn custom_gateway_requires_native_search_opt_in_on_execute_and_stream() {
    for native in [true, false] {
        for streaming in [true, false] {
            let mock = Mock::start(Reply::sse(&search_sse())).await;
            let executor = mock.executor().with_config(search_config(true));
            let mut auth = (*gateway_auth(&mock.url)).clone();
            if native {
                auth.attributes
                    .insert("native_tool_search".into(), "true".into());
            }
            let mut options = options(Format::OPENAI_RESPONSE);
            let request = request(search_payload());
            options.original_request = request.payload.clone();
            if streaming {
                options.stream = true;
                let out = executor
                    .execute_stream(Arc::new(auth), request, options)
                    .await
                    .unwrap();
                for chunk in out.chunks.collect::<Vec<_>>().await {
                    chunk.unwrap();
                }
            } else {
                executor
                    .execute(Arc::new(auth), request, options)
                    .await
                    .unwrap();
            }
            let body = mock.last().json();
            assert_eq!(body.to_string().contains("tool_reference"), native);
            assert_eq!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == "tool_search"),
                native
            );
        }
    }
}

#[test]
fn reload_between_fallback_and_translation_retains_the_executor_snapshot() {
    let enabled = ClaudeExecutor::new("direct").with_config(search_config(true));
    let request = request(search_payload());
    let mut options = options(Format::OPENAI_RESPONSE);
    options.original_request = request.payload.clone();
    let auth = gateway_auth("https://gateway.example.com");
    let (fallback_request, fallback_options) =
        enabled.tool_search_fallback(&auth, &request, &options);
    let replacement = ClaudeExecutor::new("direct").with_config(search_config(false));
    assert!(!replacement.tool_search_enabled());
    let body = translate_request(
        enabled.config.as_deref(),
        &fallback_request,
        &fallback_options,
        "claude-sonnet-4-5",
        true,
    )
    .unwrap();
    assert!(!body.to_string().contains("tool_reference"));
    assert!(
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["name"] != "mcp__codex_apps__calendar_create_event")
    );
    let (fallback_request, fallback_options) =
        replacement.tool_search_fallback(&auth, &request, &options);
    assert_eq!(fallback_request.payload, request.payload);
    assert_eq!(fallback_options.original_request, options.original_request);
    let body = translate_request(
        replacement.config.as_deref(),
        &fallback_request,
        &fallback_options,
        "claude-sonnet-4-5",
        true,
    )
    .unwrap();
    assert!(!body.to_string().contains("tool_reference"));
}

#[test]
fn disabled_executor_bodies_match_upstream_golden_without_cloaking() {
    let golden: Value = serde_json::from_str(include_str!(
        "fixtures/tool_search_disabled/dev_executor_golden.json"
    ))
    .unwrap();
    let mut payload = json!({"model":"claude-sonnet-4-5","input":[
        {"type":"message","role":"user","content":[{"type":"input_text","text":"create a calendar event"}]},
        {"type":"tool_search_call","call_id":"call_search_1","status":"completed","execution":"client","arguments":{"query":"calendar"}},
        {"type":"tool_search_output","call_id":"call_search_1","status":"completed","execution":"client","tools":[
            {"type":"namespace","name":"mcp__codex_apps__","tools":[{"type":"function","name":"calendar_create_event","defer_loading":true,"parameters":{"type":"object","properties":{"title":{"type":"string"}},"required":["title"]}}]}]}],
        "tools":[{"type":"tool_search","execution":"client","name":"ClientSearch","description":"Named search."},
            {"type":"function","name":"exec_command","description":"Run a command.","strict":false,"parameters":{"type":"object","properties":{"cmd":{"type":"string"}},"required":["cmd"]}},
            {"type":"function","name":"lookup_order","description":"Look up an order.","strict":false,"defer_loading":true,"parameters":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}},
            {"type":"tool_search","execution":"client","description":"Search deferred tools.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}],"stream":false});
    // The golden predates wave 1's registered output ceiling change. An
    // explicit limit preserves its 32000 ceiling without changing search.
    payload["max_output_tokens"] = 32000.into();
    let request = request(payload);
    let mut options = options(Format::OPENAI_RESPONSE);
    options.original_request = request.payload.clone();
    let executor = ClaudeExecutor::new("direct").with_config(search_config(false));
    for (name, recorded) in golden.as_object().unwrap() {
        let recorded = recorded.as_str().unwrap();
        if recorded.is_empty() {
            // Upstream estimates gateway counts locally. This port's existing
            // 501 is outside the search feature and remains unchanged.
            assert_eq!(name, "gateway/count_tokens");
            continue;
        }
        let count = name.ends_with("count_tokens");
        let mut expected: Value = serde_json::from_str(recorded).unwrap();
        if name.starts_with("oauth/") {
            // These recording-only fields are client disguise. Native search
            // must not reintroduce tool aliases, a synthetic date reminder,
            // billing fingerprints, CLI prompts, or diagnostics identities.
            // Cloaking also adds the 1h cache TTL, moves the automatic tool
            // cache marker away from tools[2], and changes tool_search's
            // type through OAuth aliasing. Undo those recording side effects.
            expected.as_object_mut().unwrap().remove("system");
            expected.as_object_mut().unwrap().remove("diagnostics");
            if !count {
                let content = expected["messages"][0]["content"].as_array_mut().unwrap();
                assert!(
                    content[0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("# currentDate")
                );
                content.remove(0);
                content[0]["cache_control"]
                    .as_object_mut()
                    .unwrap()
                    .remove("ttl");
                expected["tools"][2]["cache_control"] = json!({"type":"ephemeral"});
            }
            for (index, name) in ["ClientSearch", "exec_command", "lookup_order"]
                .into_iter()
                .enumerate()
            {
                expected["tools"][index]["name"] = name.into();
            }
            expected["tools"][0]["type"] = "tool_search".into();
        }
        let actual = if count {
            let mut body = translate_request(
                executor.config.as_deref(),
                &request,
                &options,
                "claude-sonnet-4-5",
                true,
            )
            .unwrap();
            sanitize_for_upstream(&mut body, "claude-sonnet-4-5");
            body.as_object_mut().unwrap().remove("metadata");
            body
        } else {
            executor
                .prepare_messages(&api_key_auth(), &request, &options, true, true, false)
                .unwrap()
                .upstream
        };
        assert_eq!(actual, expected, "{name}");
    }
}

// The default rule checks the original with the same search-aware tool order
// as the sent request; search-off translation omits the unnamed search tool.
#[test]
fn payload_defaults_see_the_search_aware_original_tool_list() {
    let config = Config::parse(
        r#"
claude:
  tool-search: true
payload:
  default:
    - models:
        - name: '*'
          protocol: claude
      params:
        tools.1.description: default description
"#,
    )
    .unwrap();
    let executor = ClaudeExecutor::new("direct").with_config(Arc::new(config));
    let request = request(json!({"input":"hello", "tools":[
        {"type":"tool_search", "execution":"client"},
        {"type":"function", "name":"lookup",
         "description":"caller description", "parameters":{"type":"object"}}
    ]}));
    let mut options = options(Format::OPENAI_RESPONSE);
    options.original_request = request.payload.clone();
    let prepared = executor
        .prepare_messages(&api_key_auth(), &request, &options, true, true, false)
        .unwrap();
    assert_eq!(prepared.upstream["tools"][1]["name"], "lookup");
    assert_eq!(
        prepared.upstream["tools"][1]["description"],
        "caller description"
    );
}

// Ported from nativeToolSearchResponsesPayload and nativeToolSearchCalendar: a
// Codex Responses turn after one client tool_search discovered a namespaced MCP
// tool.
const NATIVE_TOOL_SEARCH_CALENDAR: &str = "mcp__codex_apps__calendar_create_event";

fn native_tool_search_responses_payload() -> Value {
    json!({"model":"claude-sonnet-4-5","input":[
        {"type":"message","role":"user","content":[{"type":"input_text","text":"create a calendar event"}]},
        {"type":"tool_search_call","call_id":"call_search_1","status":"completed","execution":"client","arguments":{"query":"calendar"}},
        {"type":"tool_search_output","call_id":"call_search_1","status":"completed","execution":"client","tools":[
            {"type":"namespace","name":"mcp__codex_apps__","description":"Apps.","tools":[
                {"type":"function","name":"calendar_create_event","description":"Create a calendar event.","strict":false,"defer_loading":true,"parameters":{"type":"object","properties":{"title":{"type":"string"}},"required":["title"]}}]}]}
        ],"tools":[
        {"type":"function","name":"exec_command","description":"Run a command.","strict":false,"parameters":{"type":"object","properties":{"cmd":{"type":"string"}},"required":["cmd"]}},
        {"type":"tool_search","execution":"client","description":"Search deferred tools.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}
        ],"stream":false})
}

// Ported from TestClaudeExecutor_NativeToolSearchPayloadRuleIsFinal, with the
// tool index moved for the constant deferred anchor that precedes the
// discovered tools (97ba1e89). The rule runs after native tool search shapes
// the tools, so it still has the last word over `defer_loading`.
#[tokio::test]
async fn native_tool_search_payload_rule_is_final() {
    let sent = |rules: &str| {
        let config = Config::parse(format!(
            "claude:
  tool-search: true
{rules}"
        ))
        .unwrap();
        async move {
            let mock = Mock::start(Reply::sse(&search_sse())).await;
            let mut request = request(native_tool_search_responses_payload());
            request.model = "claude-sonnet-4-5".into();
            let mut options = options(Format::OPENAI_RESPONSE);
            options.original_request = request.payload.clone();
            mock.executor()
                .with_config(Arc::new(config))
                .execute(api_key_auth(), request, options)
                .await
                .unwrap();
            mock.last().json()
        }
    };
    let plain = sent("").await;
    let tools = plain["tools"].as_array().unwrap();
    let anchor = tools
        .iter()
        .position(|tool| tool["name"] == "tool_search_deferred_anchor")
        .expect("the anchor is declared with native tool search");
    let discovered = anchor + 1;
    assert_eq!(tools[discovered]["defer_loading"], true, "{tools:?}");
    assert_eq!(tools[discovered]["name"], NATIVE_TOOL_SEARCH_CALENDAR);
    let body = sent(&format!(
        "payload:
  override:
    - models:
        - name: '*'
          protocol: claude
      params:
        tools.{discovered}.defer_loading: false
"
    ))
    .await;
    assert_eq!(
        body["tools"][discovered]["name"],
        NATIVE_TOOL_SEARCH_CALENDAR
    );
    assert_eq!(
        body["tools"][discovered]["defer_loading"], false,
        "payload rule did not win over native tool search: {}",
        body["tools"][discovered]
    );
}
