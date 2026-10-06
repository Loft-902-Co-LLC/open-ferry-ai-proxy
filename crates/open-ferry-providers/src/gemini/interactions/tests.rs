// Ported from CLIProxyAPI internal/runtime/executor/gemini_executor_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Interactions executor against a mock Gemini API on 127.0.0.1, ported
//! from the Interactions tests of upstream's `gemini_executor_test.go`, with
//! checks of the request headers, errors and the calls it hands to the
//! Gemini executor.
//!
//! Changed:
//! - `InteractionsWithGeminiAPIKeyUsesGeminiEndpoint` calls both this
//!   executor, with a `gemini` credential, and the Gemini executor, which
//!   upstream's shared executor stands for.
//! - The stream tests read each chunk as the frame or event it is, where
//!   upstream reads them through `geminiInteractionsSSEPayload`.

use futures_util::StreamExt as _;
use http::HeaderValue;
use open_ferry_core::exec::{ErrorKind, Format};
use serde_json::json;

use super::stream::sse_payload;
use super::*;
use crate::gemini::testing::{Mock, Reply, collect, key_auth, options, request, stream_options};

/// An Interactions answer.
const ANSWER: &str = r#"{"id":"interaction_1","object":"interaction","status":"completed","steps":[{"type":"model_output","content":[{"text":"ok"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#;

/// A Gemini answer.
const GEMINI_ANSWER: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#;

/// An executor that doesn't use the environment's proxy.
fn executor() -> InteractionsExecutor {
    InteractionsExecutor::new("direct")
}

fn auth(mock: &Mock) -> Arc<Auth> {
    key_auth("gemini-interactions", "test-key", &mock.url)
}

/// An executor applying the payload rules in `yaml`.
fn with_rules(yaml: &str) -> InteractionsExecutor {
    executor().with_config(Arc::new(Config::parse(yaml).expect("config parses")))
}

/// The answer to a call that should succeed.
async fn execute(
    executor: &InteractionsExecutor,
    auth: Arc<Auth>,
    model: &str,
    payload: &str,
    options: Options,
) -> Response {
    executor
        .execute(auth, request(model, payload), options)
        .await
        .expect("the call succeeds")
}

/// The chunks of a stream that should succeed.
async fn stream(
    executor: &InteractionsExecutor,
    auth: Arc<Auth>,
    model: &str,
    payload: &str,
    options: Options,
) -> Vec<String> {
    let response = executor
        .execute_stream(auth, request(model, payload), options)
        .await
        .expect("the stream starts");
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "stream error: {error:?}");
    chunks
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// The JSON a translated chunk carries, after any `event:` line and its
/// `data: ` prefix.
fn event(chunk: &str) -> Value {
    sse_payload(chunk.as_bytes())
        .map(|payload| parse(&String::from_utf8_lossy(&payload)))
        .unwrap_or(Value::Null)
}

/// Ports TestGeminiExecutorInteractionsWithGeminiAPIKeyUsesGeminiEndpoint.
#[tokio::test]
async fn a_gemini_key_uses_the_gemini_endpoint() {
    let mock = Mock::start(Reply::json(GEMINI_ANSWER)).await;
    let gemini = GeminiExecutor::new("direct");
    let interactions = executor();
    let executors: [&dyn ProviderExecutor; 2] = [&interactions, &gemini];
    for executor in executors {
        executor
            .execute(
                key_auth("gemini", "test-key", &mock.url),
                request(
                    "gemini-3.5-flash",
                    r#"{"model":"gemini-3.5-flash","input":"hi"}"#,
                ),
                options(&Format::INTERACTIONS),
            )
            .await
            .expect("the call succeeds");
        let seen = mock.last();
        assert_eq!(seen.path, "/v1beta/models/gemini-3.5-flash:generateContent");
        assert!(seen.header("api-revision").is_none());
        let body = seen.json();
        assert!(body.pointer("/contents/0/parts/0/text").is_some(), "{body}");
        assert!(body.get("input").is_none(), "{body}");
    }
    assert_eq!(mock.hits(), 2);
}

/// Ports TestGeminiExecutorNativeInteractionsUsesInteractionsEndpoint.
#[tokio::test]
async fn native_requests_use_the_interactions_endpoint() {
    let mock = Mock::start(Reply::json(ANSWER)).await;
    let response = execute(
        &executor(),
        auth(&mock),
        "agents/test-agent",
        r#"{"agent":"agents/test-agent","input":"hi"}"#,
        options(&Format::INTERACTIONS),
    )
    .await;
    let seen = mock.last();
    assert_eq!(seen.target(), "/v1beta/interactions");
    assert_eq!(seen.header("api-revision"), Some("2026-05-20"));
    assert!(seen.json().get("model").is_none(), "{}", seen.body);
    assert_eq!(
        parse(&String::from_utf8_lossy(&response.payload))["id"],
        "interaction_1"
    );
}

// Not upstream's: a request carries the key, a JSON content type, the
// client's user agent or this project's, the credential's custom headers
// and the revision, and never `x-goog-api-client`, whoever asks for it.
#[tokio::test]
async fn sends_only_the_headers_it_should() {
    let mock = Mock::start(Reply::json(ANSWER)).await;
    let mut credential = Auth::clone(&auth(&mock));
    credential
        .attributes
        .insert("header:X-Custom".into(), "custom-value".into());
    credential
        .attributes
        .insert("header:X-Goog-Api-Client".into(), "gl-python/3.12".into());
    let credential = Arc::new(credential);
    let payload = r#"{"model":"gemini-3.5-flash","input":"hi"}"#;
    let mut client = options(&Format::INTERACTIONS);
    client.headers.insert(
        "x-goog-api-client",
        HeaderValue::from_static("gl-node/22.0.0"),
    );
    execute(
        &executor(),
        Arc::clone(&credential),
        "gemini-3.5-flash",
        payload,
        client,
    )
    .await;
    let seen = mock.last();
    assert_eq!(seen.header("x-goog-api-key"), Some("test-key"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(
        seen.header("user-agent"),
        Some(crate::codex::client::USER_AGENT)
    );
    assert_eq!(seen.header("x-custom"), Some("custom-value"));
    assert_eq!(seen.header("api-revision"), Some("2026-05-20"));
    assert!(seen.header("x-goog-api-client").is_none());
    assert!(seen.header("authorization").is_none());

    let mut client = stream_options(&Format::INTERACTIONS);
    client
        .headers
        .insert("user-agent", HeaderValue::from_static(" my-client/1.0 "));
    let mock_stream = Mock::start(Reply::sse(
        "event: interaction.completed\ndata: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"status\":\"completed\"}}\n\n",
    ))
    .await;
    let mut credential = Auth::clone(&credential);
    credential
        .attributes
        .insert("base_url".into(), mock_stream.url.clone());
    stream(
        &executor(),
        Arc::new(credential),
        "gemini-3.5-flash",
        payload,
        client,
    )
    .await;
    let seen = mock_stream.last();
    assert_eq!(seen.header("user-agent"), Some("my-client/1.0"));
    assert!(seen.header("x-goog-api-client").is_none());
    assert_eq!(seen.json()["stream"], true, "{}", seen.body);
}

/// Ports TestGeminiExecutorNativeInteractionsTranslatesOpenAIResponsesRequest.
#[tokio::test]
async fn native_translates_an_openai_responses_request() {
    let mock = Mock::start(Reply::json(ANSWER)).await;
    let response = execute(
        &executor(),
        auth(&mock),
        "gemini-3.1-flash-lite",
        r#"{
            "model":"gemini-3.1-flash-lite",
            "instructions":"be brief",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "reasoning":{"effort":"high","summary":"auto"}
        }"#,
        options(&Format::OPENAI_RESPONSE),
    )
    .await;
    let seen = mock.last();
    assert_eq!(seen.path, "/v1beta/interactions");
    let body = seen.json();
    assert_eq!(body["input"][0]["type"], "user_input", "{body}");
    assert_eq!(
        body["generation_config"]["thinking_level"], "high",
        "{body}"
    );
    let answer = parse(&String::from_utf8_lossy(&response.payload));
    assert_eq!(answer["output"][0]["content"][0]["text"], "ok", "{answer}");
}

/// Ports TestGeminiExecutorNativeInteractionsPayloadRulesUseResponsesFromProtocol.
#[tokio::test]
async fn native_payload_rules_use_the_responses_from_protocol() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"interaction_1","object":"interaction","status":"completed","steps":[{"type":"model_output","content":[{"text":"ok"}]}]}"#,
    ))
    .await;
    let executor = with_rules(
        r#"
payload:
  override:
    - models:
        - name: gemini-3.1-flash-lite
          protocol: interactions
          from-protocol: openai
      params:
        generation_config.thinking_summaries: wrong
    - models:
        - name: gemini-3.1-flash-lite
          protocol: interactions
          from-protocol: responses
      params:
        generation_config.thinking_summaries: detailed
"#,
    );
    execute(
        &executor,
        auth(&mock),
        "gemini-3.1-flash-lite",
        r#"{
            "model":"gemini-3.1-flash-lite",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]
        }"#,
        options(&Format::OPENAI_RESPONSE),
    )
    .await;
    let body = mock.last().json();
    assert_eq!(
        body["generation_config"]["thinking_summaries"], "detailed",
        "{body}"
    );
}

/// Ports TestGeminiExecutorNativeInteractionsTranslatesOpenAIChatRequest.
#[tokio::test]
async fn native_translates_an_openai_chat_request() {
    let mock = Mock::start(Reply::sse(concat!(
        "event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"i1\",\"model\":\"gemini-3.1-flash-lite\"}}\n\n",
        "event: step.start\ndata: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"function_call\",\"id\":\"call_1\",\"name\":\"get_weather\",\"arguments\":{}}}\n\n",
        "event: step.delta\ndata: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"arguments_delta\",\"arguments\":\"{\\\"location\\\":\\\"北京\\\"}\"}}\n\n",
        "event: step.stop\ndata: {\"event_type\":\"step.stop\",\"index\":0}\n\n",
        "event: interaction.completed\ndata: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"status\":\"requires_action\",\"usage\":{\"total_input_tokens\":2,\"total_output_tokens\":3,\"total_tokens\":5}}}\n\n",
    )))
    .await;
    let chunks = stream(
        &executor(),
        auth(&mock),
        "gemini-3.1-flash-lite",
        r#"{
            "model":"gemini-3.1-flash-lite",
            "stream":true,
            "messages":[{"role":"user","content":"今天北京的天气怎么样？"}],
            "tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object","properties":{"location":{"type":"string"}}}}}],
            "tool_choice":"auto"
        }"#,
        stream_options(&Format::OPENAI),
    )
    .await;
    let tool_start = chunks
        .iter()
        .map(|chunk| event(chunk))
        .rfind(|chunk| {
            chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["name"] == "get_weather"
        })
        .expect("the OpenAI tool call chunk");
    let seen = mock.last();
    assert_eq!(seen.path, "/v1beta/interactions");
    let body = seen.json();
    assert_eq!(
        body["input"][0]["content"][0]["text"], "今天北京的天气怎么样？",
        "{body}"
    );
    assert!(body.get("messages").is_none(), "{body}");
    assert_eq!(body["tools"][0]["type"], "function", "{body}");
    assert_eq!(body["generation_config"]["tool_choice"], "auto", "{body}");
    assert_eq!(
        tool_start["choices"][0]["delta"]["tool_calls"][0]["id"], "call_1",
        "{tool_start}"
    );
}

/// Ports TestGeminiExecutorNativeInteractionsPayloadDefaultsUseTranslatedOpenAIChatSource.
#[tokio::test]
async fn native_payload_defaults_use_the_translated_openai_chat_source() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"interaction_1","object":"interaction","status":"completed","steps":[{"type":"model_output","content":[{"text":"ok"}]}]}"#,
    ))
    .await;
    let executor = with_rules(
        r#"
payload:
  default:
    - models:
        - name: gemini-3.1-flash-lite
          protocol: interactions
          from-protocol: openai
      params:
        generation_config.temperature: 0.9
        generation_config.top_p: 0.8
"#,
    );
    execute(
        &executor,
        auth(&mock),
        "gemini-3.1-flash-lite",
        r#"{
            "model":"gemini-3.1-flash-lite",
            "messages":[{"role":"user","content":"hi"}],
            "temperature":0.2
        }"#,
        options(&Format::OPENAI),
    )
    .await;
    let body = mock.last().json();
    assert_eq!(body["generation_config"]["temperature"], 0.2, "{body}");
    assert_eq!(body["generation_config"]["top_p"], 0.8, "{body}");
}

/// Ports TestGeminiExecutorNativeInteractionsTranslatesGeminiStreamResponse.
#[tokio::test]
async fn native_translates_a_gemini_stream_response() {
    let mock = Mock::start(Reply::sse(concat!(
        "event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"i1\",\"model\":\"gemini-3.1-flash-lite\"}}\n\n",
        "event: step.start\ndata: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"function_call\",\"id\":\"call_1\",\"signature\":\"sig_1\",\"name\":\"get_weather\",\"arguments\":{}}}\n\n",
        "event: step.delta\ndata: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"arguments_delta\",\"arguments\":\"{\\\"location\\\":\\\"北京\\\"}\"}}\n\n",
        "event: step.stop\ndata: {\"event_type\":\"step.stop\",\"index\":0}\n\n",
        "event: interaction.completed\ndata: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"status\":\"requires_action\",\"usage\":{\"total_input_tokens\":2,\"total_output_tokens\":3,\"total_tokens\":5,\"total_cached_tokens\":1},\"service_tier\":\"standard\",\"model\":\"gemini-3.1-flash-lite\"}}\n\n",
        "event: done\ndata: [DONE]\n\n",
    )))
    .await;
    let chunks = stream(
        &executor(),
        auth(&mock),
        "gemini-3.1-flash-lite",
        r#"{
            "contents":[{"role":"user","parts":[{"text":"今天北京的天气怎么样？"}]}],
            "tools":[{"functionDeclarations":[{"name":"get_weather","parameters":{"type":"OBJECT","properties":{"location":{"type":"STRING"}},"required":["location"]}}]}]
        }"#,
        stream_options(&Format::GEMINI),
    )
    .await;
    let mut call = Value::Null;
    let mut finish = Value::Null;
    for chunk in &chunks {
        let chunk = event(chunk);
        assert!(chunk.get("event_type").is_none(), "{chunk}");
        if chunk
            .pointer("/candidates/0/content/parts/0/functionCall")
            .is_some()
        {
            call = chunk.clone();
        }
        if chunk.pointer("/candidates/0/finishReason").is_some() {
            finish = chunk;
        }
    }
    let seen = mock.last();
    assert_eq!(seen.path, "/v1beta/interactions");
    let body = seen.json();
    assert!(body.get("contents").is_none(), "{body}");
    assert_eq!(
        body["input"][0]["content"][0]["text"], "今天北京的天气怎么样？",
        "{body}"
    );
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    let part = &call["candidates"][0]["content"]["parts"][0];
    assert_eq!(part["functionCall"]["name"], "get_weather", "{call}");
    assert_eq!(part["functionCall"]["args"]["location"], "北京", "{call}");
    assert_eq!(part["thoughtSignature"], "sig_1", "{call}");
    assert_eq!(finish["candidates"][0]["finishReason"], "STOP", "{finish}");
    assert_eq!(finish["usageMetadata"]["promptTokenCount"], 2, "{finish}");
    assert_eq!(
        finish["usageMetadata"]["candidatesTokenCount"], 3,
        "{finish}"
    );
    assert_eq!(finish["usageMetadata"]["totalTokenCount"], 5, "{finish}");
}

/// Ports TestNativeInteractionsSourceFormatAllowsSupportedEntryProtocols,
/// with `RequestToFormat`.
#[test]
fn native_source_formats_are_the_supported_entry_protocols() {
    for format in [
        Format::INTERACTIONS,
        Format::OPENAI,
        Format::OPENAI_RESPONSE,
        Format::CLAUDE,
        Format::GEMINI,
    ] {
        assert!(native_source_format(&format), "{format:?}");
        assert_eq!(
            executor().request_to_format(&options(&format)),
            Format::INTERACTIONS
        );
    }
    for format in [Format::CODEX, Format::ANTIGRAVITY] {
        assert!(!native_source_format(&format), "{format:?}");
        assert_eq!(
            executor().request_to_format(&options(&format)),
            Format::GEMINI
        );
    }
}

/// Ports TestGeminiExecutorNativeInteractionsTranslatesClaudeRequest.
#[tokio::test]
async fn native_translates_a_claude_request() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"interaction_1","object":"interaction","status":"completed","model":"gemini-3.1-flash-lite","steps":[{"type":"model_output","content":[{"type":"text","text":"ok"}]}],"usage":{"total_input_tokens":1,"total_output_tokens":1}}"#,
    ))
    .await;
    let response = execute(
        &executor(),
        auth(&mock),
        "gemini-3.1-flash-lite",
        r#"{
            "model":"gemini-3.1-flash-lite",
            "max_tokens":1024,
            "tools":[{"name":"get_weather","description":"weather","input_schema":{"type":"object","properties":{"location":{"type":"string"}}}}],
            "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]
        }"#,
        options(&Format::CLAUDE),
    )
    .await;
    let seen = mock.last();
    assert_eq!(seen.path, "/v1beta/interactions");
    let body = seen.json();
    assert_eq!(body["input"][0]["content"][0]["text"], "hi", "{body}");
    assert!(body.get("messages").is_none(), "{body}");
    assert_eq!(body["tools"][0]["type"], "function", "{body}");
    let answer = parse(&String::from_utf8_lossy(&response.payload));
    assert_eq!(answer["content"][0]["text"], "ok", "{answer}");
    assert_eq!(answer["usage"]["output_tokens"], 1, "{answer}");
}

/// Checks the IDs of `body`'s input: a `function_call` keeps its `id`
/// without a `call_id`, a `function_result` its `call_id` without an `id`,
/// and nothing else, content parts included, has an `id`. Says whether
/// both a call and a result are there.
fn check_input_ids(body: &Value) -> (bool, bool) {
    let items = body["input"].as_array().expect("input");
    let (mut call, mut result) = (false, false);
    for item in items {
        for part in item["content"].as_array().into_iter().flatten() {
            assert!(part.get("id").is_none(), "{body}");
        }
        match item["type"].as_str() {
            Some("function_call") => {
                call = true;
                assert_eq!(item["id"], "toolu_1", "{body}");
                assert!(item.get("call_id").is_none(), "{body}");
            }
            Some("function_result") => {
                result = true;
                assert_eq!(item["call_id"], "toolu_1", "{body}");
                assert!(item.get("id").is_none(), "{body}");
            }
            _ => assert!(item.get("id").is_none(), "{body}"),
        }
    }
    (call, result)
}

/// Ports TestGeminiExecutorNativeInteractionsStripsClaudeToolIDsBeforeUpstream.
#[tokio::test]
async fn native_strips_claude_tool_ids() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"interaction_1","object":"interaction","status":"completed","model":"gemini-3.6-flash","steps":[{"type":"model_output","content":[{"type":"text","text":"ok"}]}],"usage":{"total_input_tokens":1,"total_output_tokens":1}}"#,
    ))
    .await;
    execute(
        &executor(),
        auth(&mock),
        "gemini-3.6-flash",
        r#"{
            "model":"gemini-3.6-flash",
            "messages":[
                {"role":"user","content":[{"type":"text","text":"weather?","id":"txt_user"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"location":"北京"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"晴"}]}
            ]
        }"#,
        options(&Format::CLAUDE),
    )
    .await;
    let body = mock.last().json();
    assert_eq!(check_input_ids(&body), (true, true), "{body}");
}

/// Ports TestGeminiExecutorNativeInteractionsStripsPassthroughInputIDsBeforeUpstream.
#[tokio::test]
async fn native_strips_passthrough_input_ids() {
    let payload = r#"{
        "model":"gemini-3.6-flash",
        "input":[
            {"type":"user_input","id":"msg_1","content":[{"type":"text","id":"txt_1","text":"hi"}]},
            {"type":"function_call","id":"toolu_1","call_id":"toolu_1","name":"lookup","arguments":{"q":"x"}},
            {"type":"function_result","id":"toolu_1","call_id":"toolu_1","result":"ok"}
        ]
    }"#;
    for streaming in [false, true] {
        let mock = Mock::start(if streaming {
            Reply::sse(
                "event: interaction.completed\ndata: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"status\":\"completed\"}}\n\n",
            )
        } else {
            Reply::json(
                r#"{"id":"interaction_1","object":"interaction","status":"completed","model":"gemini-3.6-flash","steps":[],"usage":{"total_input_tokens":1,"total_output_tokens":1}}"#,
            )
        })
        .await;
        if streaming {
            let options = stream_options(&Format::INTERACTIONS);
            stream(
                &executor(),
                auth(&mock),
                "gemini-3.6-flash",
                payload,
                options,
            )
            .await;
        } else {
            let options = options(&Format::INTERACTIONS);
            execute(
                &executor(),
                auth(&mock),
                "gemini-3.6-flash",
                payload,
                options,
            )
            .await;
        }
        let body = mock.last().json();
        assert_eq!(body["input"].as_array().map(Vec::len), Some(3), "{body}");
        assert_eq!(check_input_ids(&body), (true, true), "{body}");
        assert_eq!(body["input"][0]["content"][0]["text"], "hi", "{body}");
    }
}

// Not upstream's: a `function_call` without an `id` takes its `call_id`.
#[test]
fn a_function_call_takes_its_call_id() {
    let mut body = json!({"input": [
        {"type": "function_call", "call_id": "call_7", "name": "lookup"},
        {"type": "function_call", "id": "kept", "call_id": "dropped"},
        {"type": "model_output", "id": "out_1", "content": [{"type": "text", "id": "p", "text": "x"}]},
        "not an object"
    ]});
    sanitize_input_ids(&mut body);
    assert_eq!(
        body,
        json!({"input": [
            {"type": "function_call", "id": "call_7", "name": "lookup"},
            {"type": "function_call", "id": "kept"},
            {"type": "model_output", "content": [{"type": "text", "text": "x"}]},
            "not an object"
        ]})
    );
    let mut text = json!({"input": "hi"});
    sanitize_input_ids(&mut text);
    assert_eq!(text, json!({"input": "hi"}));
}

/// Ports TestGeminiExecutorNativeInteractionsAppliesThinkingSuffix.
#[tokio::test]
async fn native_applies_the_thinking_suffix() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"interaction_1","status":"completed","steps":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#,
    ))
    .await;
    execute(
        &executor(),
        auth(&mock),
        "gemini-3.1-flash-lite(high)",
        r#"{"model":"gemini-3.1-flash-lite(high)","generation_config":{"max_output_tokens":32},"input":"hi"}"#,
        options(&Format::INTERACTIONS),
    )
    .await;
    let body = mock.last().json();
    assert_eq!(body["model"], "gemini-3.1-flash-lite", "{body}");
    assert!(body.get("generationConfig").is_none(), "{body}");
    let config = &body["generation_config"];
    assert!(config.get("thinking_config").is_none(), "{body}");
    assert_eq!(config["thinking_level"], "high", "{body}");
    assert!(config.get("thinking_summaries").is_none(), "{body}");
}

/// Ports TestGeminiExecutorNativeInteractionsPreservesThinkingProtocolFields.
#[tokio::test]
async fn native_preserves_thinking_protocol_fields() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"interaction_1","status":"completed","steps":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#,
    ))
    .await;
    execute(
        &executor(),
        auth(&mock),
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","generation_config":{"tool_choice":"auto","thinking_level":"high","thinking_summaries":"auto"},"input":"hi"}"#,
        options(&Format::INTERACTIONS),
    )
    .await;
    let body = mock.last().json();
    assert!(body.get("generationConfig").is_none(), "{body}");
    let config = &body["generation_config"];
    assert!(config.get("thinking_config").is_none(), "{body}");
    assert_eq!(config["thinking_level"], "high", "{body}");
    assert_eq!(config["thinking_summaries"], "auto", "{body}");
}

/// The `Api-Revision` a native call sent, with the credential's
/// `header:Api-Revision` attribute and the client's header.
async fn revision_sent(credential: Option<&str>, client: Option<&'static str>) -> Option<String> {
    let mock = Mock::start(Reply::json(
        r#"{"id":"interaction_1","status":"completed","steps":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#,
    ))
    .await;
    let mut auth = Auth::clone(&auth(&mock));
    if let Some(revision) = credential {
        auth.attributes
            .insert("header:Api-Revision".into(), revision.into());
    }
    let mut options = options(&Format::INTERACTIONS);
    if let Some(revision) = client {
        options
            .headers
            .insert("api-revision", HeaderValue::from_static(revision));
    }
    execute(
        &executor(),
        Arc::new(auth),
        "agents/test-agent",
        r#"{"agent":"agents/test-agent","input":"hi"}"#,
        options,
    )
    .await;
    mock.last().header("api-revision").map(str::to_owned)
}

/// Ports TestGeminiExecutorNativeInteractionsPreservesApiRevision.
#[tokio::test]
async fn native_preserves_the_credential_api_revision() {
    assert_eq!(
        revision_sent(Some("2026-06-01"), None).await.as_deref(),
        Some("2026-06-01")
    );
}

/// Ports TestGeminiExecutorNativeInteractionsUsesRequestApiRevision.
#[tokio::test]
async fn native_uses_the_client_api_revision() {
    assert_eq!(
        revision_sent(None, Some("2026-06-01")).await.as_deref(),
        Some("2026-06-01")
    );
}

/// Ports TestGeminiExecutorNativeInteractionsRequestApiRevisionDoesNotOverrideAuthHeader.
#[tokio::test]
async fn the_client_api_revision_does_not_override_the_credential() {
    assert_eq!(
        revision_sent(Some("2026-06-01"), Some("2026-07-01"))
            .await
            .as_deref(),
        Some("2026-06-01")
    );
}

/// Ports TestGeminiExecutorNativeInteractionsStreamParsesUsage.
#[tokio::test]
async fn native_stream_passes_frames_with_their_usage() {
    let mock = Mock::start(Reply::sse(concat!(
        "event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"i1\"}}\n\n",
        "event: interaction.completed\ndata: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"status\":\"completed\",\"usage\":{\"total_input_tokens\":2,\"total_output_tokens\":3,\"total_tokens\":5}}}\n\n",
    )))
    .await;
    let chunks = stream(
        &executor(),
        auth(&mock),
        "gemini-3.5-flash",
        r#"{"model":"gemini-3.5-flash","input":"hi","stream":true}"#,
        stream_options(&Format::INTERACTIONS),
    )
    .await;
    assert!(!chunks.is_empty());
    let mut completed = Value::Null;
    for chunk in &chunks {
        assert!(
            chunk.contains("event:") && chunk.contains("data:"),
            "{chunk:?}"
        );
        assert!(chunk.ends_with("}\n\n"), "{chunk:?}");
        let payload = event(chunk);
        if payload["event_type"] == "interaction.completed" {
            completed = payload;
        }
    }
    let usage = &completed["interaction"]["usage"];
    assert_eq!(usage["total_input_tokens"], 2, "{completed}");
    assert_eq!(usage["total_output_tokens"], 3, "{completed}");
    assert_eq!(usage["total_tokens"], 5, "{completed}");
}

// Not upstream's: an Interactions client gets the frames as they came, the
// last one even with no blank line after it, its line endings normalized.
#[tokio::test]
async fn native_stream_passes_the_last_frame() {
    let mock = Mock::start(Reply::sse(
        ": keep-alive\r\n\r\nevent: step.delta\r\ndata: {\"a\":1}\r\n\r\n\n\nevent: done\ndata: [DONE]",
    ))
    .await;
    let chunks = stream(
        &executor(),
        auth(&mock),
        "gemini-3.5-flash",
        r#"{"model":"gemini-3.5-flash","input":"hi"}"#,
        stream_options(&Format::INTERACTIONS),
    )
    .await;
    assert_eq!(
        chunks,
        [
            ": keep-alive\n\n",
            "event: step.delta\ndata: {\"a\":1}\n\n",
            "event: done\ndata: [DONE]\n\n",
        ]
    );
}

/// Ports TestGeminiExecutorNativeInteractionsClaudeStreamPreservesToolSignature.
#[tokio::test]
async fn native_claude_stream_preserves_the_tool_signature() {
    let mock = Mock::start(Reply::sse(concat!(
        "event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"i1\",\"model\":\"gemini-3.1-flash-lite\"}}\n\n",
        "event: step.start\ndata: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"function_call\",\"id\":\"toolu_1\",\"signature\":\"sig_1\",\"name\":\"get_weather\",\"arguments\":{}}}\n\n",
        "event: step.delta\ndata: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"arguments_delta\",\"arguments\":\"{\\\"location\\\":\\\"北京\\\"}\"}}\n\n",
        "event: step.stop\ndata: {\"event_type\":\"step.stop\",\"index\":0}\n\n",
        "event: interaction.completed\ndata: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"status\":\"requires_action\",\"usage\":{\"total_input_tokens\":1,\"total_output_tokens\":2}}}\n\n",
        "event: done\ndata: [DONE]\n\n",
    )))
    .await;
    let chunks = stream(
        &executor(),
        auth(&mock),
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","stream":true,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
        stream_options(&Format::CLAUDE),
    )
    .await;
    let (mut tool_start, mut tool_delta, mut message_stop) = (Value::Null, Value::Null, false);
    for chunk in &chunks {
        let payload = event(chunk);
        match payload["type"].as_str() {
            Some("content_block_start") if payload["content_block"]["type"] == "tool_use" => {
                tool_start = payload;
            }
            Some("content_block_delta") if payload["delta"]["type"] == "input_json_delta" => {
                tool_delta = payload;
            }
            Some("message_stop") => message_stop = true,
            _ => {}
        }
    }
    assert_eq!(
        tool_start["content_block"]["signature"], "sig_1",
        "{tool_start}"
    );
    assert_eq!(
        tool_delta["delta"]["partial_json"], r#"{"location":"北京"}"#,
        "{tool_delta}"
    );
    assert!(message_stop, "{chunks:?}");
}

/// Ports TestGeminiExecutorNativeInteractionsResponsesStreamEmitsDone.
#[tokio::test]
async fn native_responses_stream_emits_done_once() {
    let mock = Mock::start(Reply::sse(concat!(
        "event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"i1\",\"model\":\"gemini-3.1-flash-lite\"}}\n\n",
        "event: interaction.completed\ndata: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"status\":\"completed\",\"usage\":{\"total_input_tokens\":1,\"total_output_tokens\":2}}}\n\n",
        "event: done\ndata: [DONE]\n\n",
        "event: done\ndata: [DONE]\n\n",
        "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"late\"}}\n\n",
    )))
    .await;
    let chunks = stream(
        &executor(),
        auth(&mock),
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","stream":true,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        stream_options(&Format::OPENAI_RESPONSE),
    )
    .await;
    let completed = chunks
        .iter()
        .filter(|chunk| chunk.contains(r#""type":"response.completed""#))
        .count();
    let done = chunks
        .iter()
        .filter(|chunk| chunk.trim() == "data: [DONE]")
        .count();
    assert!(
        chunks.iter().all(|chunk| !chunk.contains(r#""late""#)),
        "{chunks:?}"
    );
    assert_eq!((done, completed), (1, 1), "{chunks:?}");
}

// Not upstream's: a Codex client's request, which Interactions isn't called
// natively for, goes to the Gemini executor's `generateContent`, without a
// revision.
#[tokio::test]
async fn a_codex_client_goes_to_generate_content() {
    let mock = Mock::start(Reply::json(GEMINI_ANSWER)).await;
    execute(
        &executor(),
        auth(&mock),
        "gemini-3.5-flash",
        r#"{"model":"gemini-3.5-flash","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        options(&Format::CODEX),
    )
    .await;
    let seen = mock.last();
    assert_eq!(seen.path, "/v1beta/models/gemini-3.5-flash:generateContent");
    assert!(seen.header("api-revision").is_none());
    assert_eq!(seen.header("x-goog-api-key"), Some("test-key"));
}

// Not upstream's: tokens are counted with the Gemini API's `countTokens`.
#[tokio::test]
async fn counts_tokens_with_gemini() {
    let mock = Mock::start(Reply::json(r#"{"totalTokens":7}"#)).await;
    let response = executor()
        .count_tokens(
            auth(&mock),
            request(
                "gemini-3.5-flash",
                r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
            ),
            options(&Format::GEMINI),
        )
        .await
        .expect("the count succeeds");
    assert_eq!(
        mock.last().path,
        "/v1beta/models/gemini-3.5-flash:countTokens"
    );
    assert!(
        String::from_utf8_lossy(&response.payload).contains('7'),
        "{:?}",
        response.payload
    );
}

// Not upstream's: a `/responses/compact` call fails with a 501 before
// anything is sent, natively or not.
#[tokio::test]
async fn rejects_compact_calls() {
    let mock = Mock::start(Reply::json(ANSWER)).await;
    for format in [Format::OPENAI_RESPONSE, Format::CODEX] {
        let mut compact = options(&format);
        compact.alt = "responses/compact".into();
        let error = executor()
            .execute(
                auth(&mock),
                request("gemini-3.5-flash", "{}"),
                compact.clone(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            (error.status, error.message.as_str()),
            (501, "/responses/compact not supported")
        );
        let Err(error) = executor()
            .execute_stream(auth(&mock), request("gemini-3.5-flash", "{}"), compact)
            .await
        else {
            panic!("the stream was opened");
        };
        assert_eq!(error.status, 501);
    }
    assert_eq!(mock.hits(), 0);
}

// Not upstream's: an error status comes back with its body, without the
// key the request carried, for a call and a stream.
#[tokio::test]
async fn errors_keep_their_status_without_the_key() {
    let body = r#"{"error":{"code":429,"message":"quota for test-key-secret","status":"RESOURCE_EXHAUSTED"}}"#;
    let mock = Mock::start(Reply::error(429, body)).await;
    let payload = r#"{"model":"gemini-3.5-flash","input":"hi"}"#;
    let auth = key_auth("gemini-interactions", "test-key-secret", &mock.url);
    let error = executor()
        .execute(
            Arc::clone(&auth),
            request("gemini-3.5-flash", payload),
            options(&Format::INTERACTIONS),
        )
        .await
        .unwrap_err();
    assert_eq!((error.kind, error.status), (ErrorKind::Upstream, 429));
    assert_eq!(
        error.message,
        r#"{"error":{"code":429,"message":"quota for [redacted]","status":"RESOURCE_EXHAUSTED"}}"#
    );
    let Err(error) = executor()
        .execute_stream(
            auth,
            request("gemini-3.5-flash", payload),
            stream_options(&Format::INTERACTIONS),
        )
        .await
    else {
        panic!("the stream was opened");
    };
    assert_eq!(error.status, 429);
    assert!(
        !error.message.contains("test-key-secret"),
        "{}",
        error.message
    );
}

// Not upstream's: a stream's frames don't quote the key the request
// carried.
#[tokio::test]
async fn stream_frames_hide_the_key() {
    let mock = Mock::start(Reply::sse(
        "event: error\ndata: {\"error\":{\"message\":\"invalid API key test-key-secret\"}}\n\n",
    ))
    .await;
    let response = executor()
        .execute_stream(
            key_auth("gemini-interactions", "test-key-secret", &mock.url),
            request("gemini-3.5-flash", r#"{"input":"hi"}"#),
            stream_options(&Format::INTERACTIONS),
        )
        .await
        .expect("the stream starts");
    let chunks: Vec<_> = response.chunks.collect().await;
    let text: String = chunks
        .into_iter()
        .map(|chunk| String::from_utf8_lossy(&chunk.expect("a chunk")).into_owned())
        .collect();
    assert!(!text.contains("test-key-secret"), "{text}");
    assert!(text.contains("[redacted]"), "{text}");
}

// Not upstream's: an error quotes none of the secrets the request sent (the
// credential headers after the custom ones, each cookie, the URL's
// credentials), nor the password of a proxy that answers 407.
// The error log, with `request-log` off, has the answer's status and
// body, scrubbed.
#[tokio::test]
async fn errors_hide_every_secret_sent() {
    let payload = r#"{"model":"gemini-3.5-flash","input":"hi"}"#;
    for case in crate::secret_echo::cases(|base_url| {
        (*key_auth("gemini-interactions", "test-key", base_url)).clone()
    })
    .await
    {
        for streaming in [false, true] {
            let log = crate::secret_echo::ErrorLog::start();
            let options = log.tapped(Options {
                headers: case.headers.clone(),
                ..if streaming {
                    stream_options(&Format::INTERACTIONS)
                } else {
                    options(&Format::INTERACTIONS)
                }
            });
            let auth = Arc::clone(&case.auth);
            let error = if streaming {
                executor()
                    .execute_stream(auth, request("gemini-3.5-flash", payload), options)
                    .await
                    .err()
            } else {
                executor()
                    .execute(auth, request("gemini-3.5-flash", payload), options)
                    .await
                    .err()
            };
            case.check_logged(&error.expect("the call went through"), log);
        }
    }
}

// Not upstream's: an API key needs no refresh.
#[tokio::test]
async fn refreshes_to_the_same_credential() {
    let auth = key_auth("gemini-interactions", "test-key", "http://127.0.0.1:9");
    let refreshed = executor().refresh(Arc::clone(&auth)).await.unwrap();
    assert_eq!(refreshed.attributes, auth.attributes);
    assert_eq!(executor().id(), "gemini-interactions");
}

/// A mock whose answer, an Interactions one, says the API key the request
/// carried as the model's output.
async fn saying_the_key() -> Mock {
    Mock::answering(|seen| {
        let key = seen.header("x-goog-api-key").unwrap_or_default();
        Reply::json(
            &json!({
                "id": "interaction_1",
                "object": "interaction",
                "status": "completed",
                "model": "gemini-3.1-flash-lite",
                "steps": [{
                    "type": "model_output",
                    "content": [{"type": "text", "text": format!("your key is {key}")}],
                }],
                "usage": {"total_input_tokens": 1, "total_output_tokens": 1},
            })
            .to_string(),
        )
    })
    .await
}

// Not upstream's: a model that says the key the request carried doesn't hand
// it on, in the answer to a call that isn't streamed, whether it is returned
// as it is or translated for another client.
#[tokio::test]
async fn answers_hide_the_key() {
    let mock = saying_the_key().await;
    let auth = key_auth("gemini-interactions", "test-key-secret", &mock.url);
    let native = r#"{"model":"gemini-3.1-flash-lite","input":"hi"}"#;
    let chat = r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"hi"}]}"#;
    let claude = r#"{"model":"gemini-3.1-flash-lite","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;
    for (format, payload) in [
        (Format::INTERACTIONS, native),
        (Format::OPENAI, chat),
        (Format::CLAUDE, claude),
    ] {
        let response = execute(
            &executor(),
            Arc::clone(&auth),
            "gemini-3.1-flash-lite",
            payload,
            options(&format),
        )
        .await;
        let text = String::from_utf8_lossy(&response.payload);
        assert!(!text.contains("test-key-secret"), "{format:?}: {text}");
        assert!(
            text.contains("your key is [redacted]"),
            "{format:?}: {text}"
        );
    }
    assert_eq!(mock.hits(), 3);
}

// Not upstream's: an interaction that failed with a 200, quoting the key in
// its error, doesn't hand it on either.
#[tokio::test]
async fn a_failed_interaction_hides_the_key() {
    let mock = Mock::answering(|seen| {
        let key = seen.header("x-goog-api-key").unwrap_or_default();
        Reply::json(
            &json!({
                "id": "interaction_1",
                "object": "interaction",
                "status": "failed",
                "error": {"code": "invalid_key", "message": format!("API key {key} is not valid")},
            })
            .to_string(),
        )
    })
    .await;
    let response = execute(
        &executor(),
        key_auth("gemini-interactions", "test-key-secret", &mock.url),
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","input":"hi"}"#,
        options(&Format::INTERACTIONS),
    )
    .await;
    let text = String::from_utf8_lossy(&response.payload);
    assert!(!text.contains("test-key-secret"), "{text}");
    assert!(text.contains("API key [redacted] is not valid"), "{text}");
}
