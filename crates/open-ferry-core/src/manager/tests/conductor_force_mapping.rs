// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_force_mapping_test.go
// and the fixtures in force_mapping_live_fixtures_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Force-mapped aliases end to end: the executor is asked for the upstream
//! model, and the response, whole or streamed, names the alias the client
//! asked for. Covers OAuth aliases and API-key and OpenAI-compatible
//! aliases, with response shapes taken from live providers.
//!
//! Deviations from upstream:
//! - `TestManagerExecute_AntigravityCreditsFallbackForceMappingRewritesResponse`
//!   and `TestManagerExecuteStream_AntigravityCreditsFallbackForceMappingRewritesResponse`
//!   are dropped: the Antigravity credits fallback isn't ported.
//! - The OAuth aliases don't set upstream's `fork`: the port's
//!   [`ModelAlias`] has no fork flag, which only changes what the registry
//!   lists, not how a call resolves.

use std::collections::BTreeMap;

use bytes::Bytes;

use super::support::*;
use crate::auth::Status;
use crate::exec::Dispatcher;
use crate::manager::{ApiKeyEntry, ModelAlias, OpenAiCompat, Settings};

const LIVE_CODEX_RESPONSES_CREATED_UPSTREAM: &str = r#"{"type":"response.created","response":{"id":"resp_live","object":"response","created_at":1782272843,"status":"in_progress","model":"gpt-5.4","output":[],"parallel_tool_calls":true}}"#;

const LIVE_CODEX_RESPONSES_COMPLETED_UPSTREAM: &str = r#"{"type":"response.completed","response":{"id":"resp_live","object":"response","created_at":1782272843,"status":"completed","model":"gpt-5.4","output":[{"type":"message","content":[{"type":"output_text","text":"Hi!"}]}]}}"#;

const LIVE_ANTIGRAVITY_MESSAGES_START_UPSTREAM: &str = r#"{"type": "message_start", "message": {"id": "UVM7aqirB-npz7IP8rfZuQQ", "type": "message", "role": "assistant", "content": [], "model": "gemini-3-flash", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 2, "output_tokens": 1}}}"#;

const LIVE_KIMI_CHAT_CHUNK_UPSTREAM: &str = r#"{"id":"chatcmpl-McAG6QS2WmxRKmMxSjWvbgWB","object":"chat.completion.chunk","created":1782272842,"model":"kimi-k2.5","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}],"system_fingerprint":"fpv0_b30801d4"}"#;

const LIVE_KIMI_MESSAGES_START_UPSTREAM: &str = r#"{"type":"message_start","message":{"id":"msg_iFEkPDty2KtvlbdThqOBsN25","type":"message","role":"assistant","content":[],"model":"kimi-k2.5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1263,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0,"service_tier":"standard","inference_geo":"not_available","prompt_tokens":1263,"cached_tokens":0}}}"#;

const LIVE_XAI_MESSAGES_START_UPSTREAM: &str = r#"{"type":"message_start","message":{"id":"4aeb964a-1190-98f6-9978-a8a7548848d8","type":"message","role":"assistant","model":"grok-4.3","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0},"content":[],"stop_reason":null}}"#;

const LIVE_CODEX_RESPONSES_NON_STREAM_UPSTREAM: &str = r#"{"model":"gpt-5.4","output":[{"type":"message","content":[{"type":"output_text","text":"Hi!"}]}]}"#;

const LIVE_KIMI_MESSAGES_NON_STREAM_UPSTREAM: &str = r#"{"type":"message","role":"assistant","model":"kimi-k2.5","content":[{"type":"text","text":"hi"}]}"#;

/// One provider, its upstream model and the alias clients use.
struct Case {
    provider: &'static str,
    upstream_model: &'static str,
    alias_model: &'static str,
}

const KIMI_AND_XAI: [Case; 2] = [
    Case {
        provider: "kimi",
        upstream_model: "kimi-k2.5",
        alias_model: "k2.5",
    },
    Case {
        provider: "xai",
        upstream_model: "grok-4.3",
        alias_model: "grok-latest",
    },
];

const LIVE_DERIVED: [Case; 4] = [
    Case {
        provider: "codex",
        upstream_model: "gpt-5.4",
        alias_model: "gpt-5.4-fast",
    },
    Case {
        provider: "antigravity",
        upstream_model: "gemini-3-flash",
        alias_model: "claude-haiku-4-5-20251001",
    },
    Case {
        provider: "kimi",
        upstream_model: "kimi-k2.5",
        alias_model: "k2.5",
    },
    Case {
        provider: "xai",
        upstream_model: "grok-4.3",
        alias_model: "grok-latest",
    },
];

const API_KEY_CASES: [Case; 5] = [
    Case {
        provider: "claude",
        upstream_model: "glm-5.2",
        alias_model: "claude-sonnet-latest",
    },
    Case {
        provider: "codex",
        upstream_model: "gpt-5.5",
        alias_model: "claude-sonnet-4-5",
    },
    Case {
        provider: "xai",
        upstream_model: "grok-4.5",
        alias_model: "grok-latest",
    },
    Case {
        provider: "vertex",
        upstream_model: "gemini-3-pro",
        alias_model: "claude-opus-4-5",
    },
    Case {
        provider: "openai-compatibility",
        upstream_model: "deepseek-v3.1",
        alias_model: "claude-opus-4.66",
    },
];

/// Whether `payload` still names the upstream model in a model field
/// (upstream's `forceMappingPayloadLeaksUpstream`).
fn payload_leaks_upstream(payload: &str, upstream_model: &str) -> bool {
    if upstream_model.is_empty() {
        return false;
    }
    payload.contains(&format!(r#""model":"{upstream_model}""#))
        || payload.contains(&format!(r#""model": "{upstream_model}""#))
        || payload.contains(&format!(r#""modelVersion":"{upstream_model}""#))
}

/// The provider's non-streaming body naming `upstream_model` (upstream's
/// `forceMappingNonStreamUpstreamPayload`).
fn non_stream_upstream_payload(provider: &str, upstream_model: &str) -> String {
    match provider {
        "codex" => LIVE_CODEX_RESPONSES_NON_STREAM_UPSTREAM.replacen("gpt-5.4", upstream_model, 1),
        "kimi" => LIVE_KIMI_MESSAGES_NON_STREAM_UPSTREAM.replacen("kimi-k2.5", upstream_model, 1),
        "xai" => format!(
            r#"{{"type":"message","role":"assistant","model":"{upstream_model}","content":[{{"type":"text","text":"hi"}}]}}"#
        ),
        "antigravity" => {
            LIVE_ANTIGRAVITY_MESSAGES_START_UPSTREAM.replacen("gemini-3-flash", upstream_model, 1)
        }
        _ => format!(r#"{{"model":"{upstream_model}","message":{{"model":"{upstream_model}"}}}}"#),
    }
}

/// The provider's stream chunks naming `upstream_model` (upstream's
/// `forceMappingStreamUpstreamChunks`).
fn stream_upstream_chunks(provider: &str, upstream_model: &str) -> Vec<String> {
    match provider {
        "codex" => {
            let created = LIVE_CODEX_RESPONSES_CREATED_UPSTREAM.replace("gpt-5.4", upstream_model);
            let completed =
                LIVE_CODEX_RESPONSES_COMPLETED_UPSTREAM.replace("gpt-5.4", upstream_model);
            vec![
                "event: response.created\n".to_owned(),
                format!("data: {created}\n"),
                "\n".to_owned(),
                "event: response.completed\n".to_owned(),
                format!("data: {completed}\n"),
                "\n".to_owned(),
            ]
        }
        "kimi" => {
            let msg = LIVE_KIMI_MESSAGES_START_UPSTREAM.replacen("kimi-k2.5", upstream_model, 1);
            let chat = LIVE_KIMI_CHAT_CHUNK_UPSTREAM.replacen("kimi-k2.5", upstream_model, 1);
            vec![
                "event:message_start\n".to_owned(),
                format!("data:{msg}\n\n"),
                format!("data: {chat}\n\n"),
            ]
        }
        "xai" => {
            let msg = LIVE_XAI_MESSAGES_START_UPSTREAM.replacen("grok-4.3", upstream_model, 1);
            vec![
                "event: message_start\n".to_owned(),
                format!("data: {msg}\n\n"),
            ]
        }
        "antigravity" => {
            let msg = LIVE_ANTIGRAVITY_MESSAGES_START_UPSTREAM.replacen(
                "gemini-3-flash",
                upstream_model,
                1,
            );
            vec![
                "event: message_start\n".to_owned(),
                format!("data: {msg}\n\n"),
            ]
        }
        _ => vec![format!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"model\":\"{upstream_model}\"}}}}\n\n"
        )],
    }
}

/// An executor answering with the provider's live-shaped responses for
/// whatever model it is asked (upstream's `forceMappingExecutor`).
fn force_mapping_executor(provider: &'static str) -> std::sync::Arc<FakeExecutor> {
    FakeExecutor::with(provider, move |call| match call.kind {
        Kind::Stream => Reply::chunks(
            stream_upstream_chunks(provider, &call.model)
                .into_iter()
                .map(|chunk| Ok(Bytes::from(chunk)))
                .collect(),
        ),
        Kind::Execute => Reply::ok(non_stream_upstream_payload(provider, &call.model)),
        Kind::Count => Reply::status(501, "CountTokens not implemented"),
    })
}

/// A manager with one OAuth credential of `provider` whose alias force-maps
/// to `upstream_model` (upstream's `setupForceMappingManager`).
fn setup_force_mapping_manager(case: &Case) -> (Harness, std::sync::Arc<FakeExecutor>) {
    let mut oauth_model_alias = BTreeMap::new();
    oauth_model_alias.insert(
        case.provider.to_owned(),
        vec![ModelAlias {
            name: case.upstream_model.to_owned(),
            alias: case.alias_model.to_owned(),
            force_mapping: true,
        }],
    );
    let h = Harness::new(Settings {
        oauth_model_alias,
        ..Settings::default()
    });
    let executor = force_mapping_executor(case.provider);
    h.executor(&executor);
    let mut credential = auth(
        &format!("{}-force-mapping-auth", case.provider),
        case.provider,
    );
    credential.status = Status::Active;
    h.add(credential, &[case.alias_model, case.upstream_model]);
    (h, executor)
}

/// A manager with one API-key (or OpenAI-compatible) credential of
/// `provider` whose configured alias force-maps to `upstream_model`
/// (upstream's `setupAPIKeyForceMappingManager`).
fn setup_api_key_force_mapping_manager(case: &Case) -> (Harness, std::sync::Arc<FakeExecutor>) {
    let api_key = format!("{}-key", case.provider);
    let models = vec![ModelAlias {
        name: case.upstream_model.to_owned(),
        alias: case.alias_model.to_owned(),
        force_mapping: true,
    }];
    let mut settings = Settings::default();
    match case.provider {
        "claude" | "codex" | "xai" | "vertex" => {
            settings.api_keys.insert(
                case.provider.to_owned(),
                vec![ApiKeyEntry {
                    api_key: api_key.clone(),
                    models,
                    ..ApiKeyEntry::default()
                }],
            );
        }
        "openai-compatibility" => settings.openai_compatibility.push(OpenAiCompat {
            name: case.provider.to_owned(),
            models,
            ..OpenAiCompat::default()
        }),
        other => panic!("unsupported provider {other:?}"),
    }
    let h = Harness::new(settings);
    let executor = force_mapping_executor(case.provider);
    h.executor(&executor);
    let mut credential = auth(
        &format!("{}-api-key-force-mapping-auth", case.provider),
        case.provider,
    );
    credential.attributes.insert("api_key".into(), api_key);
    if case.provider == "openai-compatibility" {
        credential
            .attributes
            .insert("compat_name".into(), case.provider.to_owned());
        credential
            .attributes
            .insert("provider_key".into(), case.provider.to_owned());
    }
    h.add(credential, &[case.alias_model, case.upstream_model]);
    (h, executor)
}

/// Executes the alias and checks the executor got the upstream model and
/// the body names the alias only.
async fn assert_execute_rewrites(h: &Harness, executor: &FakeExecutor, case: &Case) {
    let resp = h
        .manager
        .execute(
            &providers(&[case.provider]),
            request(case.alias_model),
            options(),
        )
        .await
        .unwrap_or_else(|err| panic!("{}: execute error = {err}, want success", case.provider));
    assert_eq!(
        executor.models(Kind::Execute),
        [case.upstream_model],
        "{}: execute models",
        case.provider
    );
    let got = String::from_utf8_lossy(&resp.payload);
    assert!(
        got.contains(case.alias_model) && !payload_leaks_upstream(&got, case.upstream_model),
        "{}: response payload = {got}, want alias {:?} without upstream {:?}",
        case.provider,
        case.alias_model,
        case.upstream_model
    );
}

/// Streams the alias and checks the executor got the upstream model and
/// the stream names the alias only.
async fn assert_stream_rewrites(h: &Harness, executor: &FakeExecutor, case: &Case) {
    let stream = h
        .manager
        .execute_stream(
            &providers(&[case.provider]),
            request(case.alias_model),
            options(),
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "{}: execute stream error = {err}, want success",
                case.provider
            )
        });
    assert_eq!(
        executor.models(Kind::Stream),
        [case.upstream_model],
        "{}: stream models",
        case.provider
    );
    let (chunks, err) = collect(stream).await;
    assert!(
        err.is_none(),
        "{}: unexpected stream error: {err:?}",
        case.provider
    );
    let got = chunks.concat();
    assert!(
        got.contains(case.alias_model),
        "{}: stream payload missing alias {:?}: {got}",
        case.provider,
        case.alias_model
    );
    assert!(
        !payload_leaks_upstream(&got, case.upstream_model),
        "{}: stream payload leaked upstream {:?}: {got}",
        case.provider,
        case.upstream_model
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_oauth_alias_force_mapping_rewrites_non_stream_response() {
    let case = Case {
        provider: "antigravity",
        upstream_model: "gemini-3-flash-preview",
        alias_model: "claude-haiku-4-5-20251001",
    };
    let (h, executor) = setup_force_mapping_manager(&case);
    assert_execute_rewrites(&h, &executor, &case).await;
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_oauth_alias_force_mapping_rewrites_stream_response() {
    let case = Case {
        provider: "antigravity",
        upstream_model: "gemini-3-flash-preview",
        alias_model: "claude-haiku-4-5-20251001",
    };
    let (h, executor) = setup_force_mapping_manager(&case);
    assert_stream_rewrites(&h, &executor, &case).await;
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_oauth_alias_force_mapping_rewrites_codex_style_line_chunks() {
    let case = Case {
        provider: "codex",
        upstream_model: "gpt-5.4",
        alias_model: "gpt-5.4-fast",
    };
    let (h, _) = setup_force_mapping_manager(&case);
    let stream = h
        .manager
        .execute_stream(
            &providers(&[case.provider]),
            request(case.alias_model),
            options(),
        )
        .await
        .unwrap_or_else(|err| panic!("execute stream error = {err}, want success"));
    let (chunks, err) = collect(stream).await;
    assert!(err.is_none(), "unexpected stream error: {err:?}");
    let got = chunks.concat();
    assert!(
        got.contains(case.alias_model) && !payload_leaks_upstream(&got, case.upstream_model),
        "stream payload = {got}, want alias {:?} without upstream {:?}",
        case.alias_model,
        case.upstream_model
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_oauth_alias_force_mapping_rewrites_kimi_and_xai_responses() {
    for case in &KIMI_AND_XAI {
        let (h, executor) = setup_force_mapping_manager(case);
        assert_execute_rewrites(&h, &executor, case).await;
    }
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_oauth_alias_force_mapping_rewrites_kimi_and_xai_responses() {
    for case in &KIMI_AND_XAI {
        let (h, executor) = setup_force_mapping_manager(case);
        assert_stream_rewrites(&h, &executor, case).await;
    }
}

#[tokio::test(start_paused = true)]
async fn manager_execute_live_derived_force_mapping_all_providers() {
    for case in &LIVE_DERIVED {
        let (h, executor) = setup_force_mapping_manager(case);
        assert_execute_rewrites(&h, &executor, case).await;
    }
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_live_derived_force_mapping_all_providers() {
    for case in &LIVE_DERIVED {
        let (h, executor) = setup_force_mapping_manager(case);
        assert_stream_rewrites(&h, &executor, case).await;
    }
}

#[tokio::test(start_paused = true)]
async fn manager_execute_api_key_alias_force_mapping_rewrites_response() {
    for case in &API_KEY_CASES {
        let (h, executor) = setup_api_key_force_mapping_manager(case);
        assert_execute_rewrites(&h, &executor, case).await;
    }
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_api_key_alias_force_mapping_rewrites_response() {
    for case in &API_KEY_CASES {
        let (h, executor) = setup_api_key_force_mapping_manager(case);
        assert_stream_rewrites(&h, &executor, case).await;
    }
}
