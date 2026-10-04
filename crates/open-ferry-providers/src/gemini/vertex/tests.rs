//! The Vertex AI executor against mock Vertex AI and token endpoints on
//! 127.0.0.1, with a service account whose key is made for the test run.
//! Ported from upstream's `gemini_vertex_executor_test.go`,
//! `vertex_proxy_token_test.go` and the Vertex tests of
//! `gemini_executor_signature_test.go`, with checks of both kinds of
//! credential, Imagen, URLs, errors and streams.
//!
//! Changed:
//! - `TestVertexAccessTokenUsesCredentialProxyNotRequestProxy` makes a call
//!   rather than fetching a token alone, and has no request proxy, since a
//!   proxy per request isn't ported. Its token endpoint is plain HTTP, so the
//!   credential's proxy sees the token request itself.
//! - `CountTokens_GeminiPayload_SanitizesClaudeCAISSignature` doesn't check
//!   the upstream-attempt tracker, which isn't ported.

use open_ferry_core::exec::Format;
use serde_json::json;

use super::*;
use crate::gemini::testing::{
    CLAUDE_SIGNATURE, Mock, OK_ANSWER, OK_STREAM, Reply, collect, function_call_payload, key_auth,
    native_gemini_signature, options, request, stream_options, test_service_account,
};

/// An executor that doesn't use the environment's proxy.
fn executor() -> VertexExecutor {
    VertexExecutor::new("direct")
}

fn auth(mock: &Mock) -> Arc<Auth> {
    key_auth("vertex", "test-vertex-key", &mock.url)
}

/// A credential with the test service account of project `proxy-test` at
/// `location`, which gets its tokens from `token_uri`.
fn service_account_auth(token_uri: &str, location: &str) -> Auth {
    let mut auth = Auth {
        provider: "vertex".into(),
        ..Auth::default()
    };
    auth.metadata.insert("type".into(), "vertex".into());
    auth.metadata
        .insert("project_id".into(), "proxy-test".into());
    auth.metadata.insert("location".into(), location.into());
    auth.metadata.insert(
        "service_account".into(),
        Value::Object(test_service_account(token_uri)),
    );
    auth
}

/// A token endpoint that hands out `sa-token`.
async fn token_endpoint() -> Mock {
    Mock::start(Reply::json(
        r#"{"access_token":"sa-token","expires_in":3600,"token_type":"Bearer"}"#,
    ))
    .await
}

/// Upstream's `geminiRequestWithThinkingSignature`: a Gemini request whose
/// model turn thought with the signature `signature`.
fn thinking_request(signature: &str) -> Request {
    let payload = json!({
        "contents": [
            {
                "role": "model",
                "parts": [
                    {"text": "Let me think...", "thought": true, "thoughtSignature": signature},
                    {"text": "Here is the response."},
                ],
            },
            {"role": "user", "parts": [{"text": "Follow up question."}]},
        ]
    });
    request("gemini-2.5-flash", &payload.to_string())
}

#[tokio::test]
async fn execute_sanitizes_claude_signature_in_gemini_request() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    executor()
        .execute(
            auth(&mock),
            thinking_request(CLAUDE_SIGNATURE),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert!(!seen.body.contains(CLAUDE_SIGNATURE), "{}", seen.body);
}

#[tokio::test]
async fn execute_stream_sanitizes_claude_signature_in_gemini_request() {
    let mock = Mock::start(Reply::sse(OK_STREAM)).await;
    let response = executor()
        .execute_stream(
            auth(&mock),
            thinking_request(CLAUDE_SIGNATURE),
            stream_options(&Format::GEMINI),
        )
        .await
        .unwrap();
    collect(response).await;
    let seen = mock.last();
    assert!(!seen.body.contains(CLAUDE_SIGNATURE), "{}", seen.body);
}

#[tokio::test]
async fn count_tokens_sanitizes_claude_signature_in_gemini_request() {
    let mock = Mock::start(Reply::json(r#"{"totalTokens": 42}"#)).await;
    executor()
        .count_tokens(
            auth(&mock),
            thinking_request(CLAUDE_SIGNATURE),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert!(!seen.body.contains(CLAUDE_SIGNATURE), "{}", seen.body);
    assert_eq!(
        seen.target(),
        "/v1/publishers/google/models/gemini-2.5-flash:countTokens"
    );
}

#[tokio::test]
async fn execute_preserves_native_gemini_signature() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let native = native_gemini_signature();
    executor()
        .execute(
            auth(&mock),
            request("gemini-2.5-flash", &function_call_payload(&native)),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(
        body["contents"][1]["parts"][0]["thoughtSignature"], native,
        "{body}"
    );
}

#[tokio::test]
async fn execute_leaves_unsigned_request_alone() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"Hello world"}]}]}"#;
    executor()
        .execute(
            auth(&mock),
            request("gemini-2.5-flash", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(
        body["contents"],
        json!([{"role": "user", "parts": [{"text": "Hello world"}]}])
    );
}

const PATCH_REQUEST: &str = r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}}]}],"input":"patch a file"}"#;
const PATCH_RESPONSE: &str = r#"{"responseId":"patch","candidates":[{"content":{"parts":[{"functionCall":{"name":"functions__apply_patch","args":{"input":"  *** Begin Patch\n*** End Patch\n "}}}]},"finishReason":"STOP"}]}"#;
const PATCH_INPUT: &str = "  *** Begin Patch\n*** End Patch\n ";

/// Upstream's `assertExecutorPatchDeclaration`.
fn assert_patch_declaration(body: &Value) {
    let declaration = &body["tools"][0]["functionDeclarations"][0];
    let description = declaration["description"].as_str().unwrap_or_default();
    let schema = &declaration["parametersJsonSchema"];
    assert!(
        declaration["name"] == "functions__apply_patch"
            && description.contains("*** Begin Patch")
            && description.contains("start: patch")
            && schema["properties"]["input"]["type"] == "string"
            && schema["additionalProperties"] == false,
        "missing standard patch declaration: {body}"
    );
}

/// Upstream's `assertExecutorPatchOutput`.
fn assert_patch_output(response: &Value) {
    let item = response["output"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["type"] == "custom_tool_call"))
        .unwrap_or(&Value::Null);
    assert!(
        item["input"] == PATCH_INPUT
            && item["name"] == "apply_patch"
            && item["namespace"] == "functions",
        "wrong patch output: {response}"
    );
}

/// Upstream's `assertExecutorPatchStream`, without the acknowledgement.
fn assert_patch_stream(chunks: &[String]) {
    let mut delta = String::new();
    let (mut done, mut item, mut last) = (Value::Null, Value::Null, Value::Null);
    for line in chunks.iter().flat_map(|chunk| chunk.split('\n')) {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let event: Value = serde_json::from_str(data.trim()).unwrap_or(Value::Null);
        match event["type"].as_str().unwrap_or_default() {
            "response.custom_tool_call_input.delta" => {
                delta.push_str(event["delta"].as_str().unwrap_or_default());
            }
            "response.custom_tool_call_input.done" => done = event,
            "response.output_item.done" if event["item"]["type"] == "custom_tool_call" => {
                item = event["item"].clone();
            }
            "response.completed" => last = event["response"].clone(),
            _ => {}
        }
    }
    assert!(
        delta == PATCH_INPUT
            && done["input"] == PATCH_INPUT
            && item["input"] == PATCH_INPUT
            && done["item_id"] == item["id"]
            && done["call_id"] == item["call_id"],
        "inconsistent stream delta={delta:?} done={done} item={item}"
    );
    assert_patch_output(&last);
}

#[tokio::test]
async fn apply_patch_reuses_the_executor() {
    let mock = Mock::answering(|seen| {
        if seen.path.contains("streamGenerateContent") {
            Reply::sse(&format!("data: {PATCH_RESPONSE}\n\n"))
        } else {
            Reply::json(PATCH_RESPONSE)
        }
    })
    .await;
    let auth = auth(&mock);
    let request = request("gemini-3.1-pro-preview", PATCH_REQUEST);
    let mut options = options(&Format::OPENAI_RESPONSE);
    options.original_request = request.payload.clone();
    let executor = executor();

    let response = executor
        .execute(Arc::clone(&auth), request.clone(), options.clone())
        .await
        .unwrap();
    assert_patch_declaration(&mock.last().json());
    assert_patch_output(&serde_json::from_slice(&response.payload).unwrap());

    options.stream = true;
    let stream = executor
        .execute_stream(auth, request, options)
        .await
        .unwrap();
    let (chunks, error) = collect(stream).await;
    assert!(error.is_none(), "{error:?}");
    assert_patch_declaration(&mock.last().json());
    assert_patch_stream(&chunks);
}

#[tokio::test]
async fn strips_tool_call_ids_from_responses_requests() {
    let mock = Mock::start(Reply::json(r#"{"totalTokens":3}"#)).await;
    let payload = json!({
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "run it"}]},
            {"type": "function_call", "call_id": "call_1", "name": "Bash", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
        ]
    })
    .to_string();
    let executor = executor();
    executor
        .execute(
            auth(&mock),
            request("gemini-2.5-flash", &payload),
            options(&Format::OPENAI_RESPONSE),
        )
        .await
        .ok();
    executor
        .count_tokens(
            auth(&mock),
            request("gemini-2.5-flash", &payload),
            options(&Format::OPENAI_RESPONSE),
        )
        .await
        .unwrap();
    for seen in mock.requests() {
        let body = seen.json();
        for content in body["contents"].as_array().unwrap() {
            for part in content["parts"].as_array().into_iter().flatten() {
                assert!(part["functionCall"].get("id").is_none(), "{body}");
                assert!(part["functionResponse"].get("id").is_none(), "{body}");
            }
        }
    }
}

#[test]
fn location_endpoints() {
    assert_eq!(
        vertex_base_url(""),
        "https://us-central1-aiplatform.googleapis.com"
    );
    assert_eq!(
        vertex_base_url(" global "),
        "https://aiplatform.googleapis.com"
    );
    assert_eq!(
        vertex_base_url(" europe-west4 "),
        "https://europe-west4-aiplatform.googleapis.com"
    );
}

#[test]
fn imagen_models_and_actions() {
    assert!(is_imagen("imagen-4.0-generate-001"));
    assert!(is_imagen("Publishers-IMAGEN-3"));
    assert!(!is_imagen("gemini-2.5-flash-image"));
    assert_eq!(action("imagen-3.0-generate-002", true), "predict");
    assert_eq!(action("imagen-3.0-generate-002", false), "predict");
    assert_eq!(action("gemini-2.5-pro", true), "streamGenerateContent");
    assert_eq!(action("gemini-2.5-pro", false), "generateContent");
}

/// The message of `auth`'s target error.
fn target_error(auth: &Auth) -> String {
    match target(auth) {
        Ok(_) => panic!("the credential was taken"),
        Err(error) => {
            assert_eq!(error.kind, ErrorKind::Upstream);
            error.message
        }
    }
}

#[test]
fn reads_the_credential() {
    // An API key attribute wins over a metadata token, which also counts
    // as a key.
    let mut auth = Auth::clone(&key_auth("vertex", "attribute-key", ""));
    auth.metadata
        .insert("access_token".into(), "metadata-key".into());
    assert!(matches!(
        target(&auth),
        Ok(Target::ApiKey {
            key: "attribute-key",
            base_url: ""
        })
    ));
    auth.attributes.remove("api_key");
    assert!(matches!(
        target(&auth),
        Ok(Target::ApiKey {
            key: "metadata-key",
            ..
        })
    ));

    // A service account takes its project and location, trimmed.
    let mut auth = service_account_auth("http://127.0.0.1:9/token", " europe-west4 ");
    let Ok(Target::ServiceAccount {
        project, location, ..
    }) = target(&auth)
    else {
        panic!("no service account");
    };
    assert_eq!(
        (project.as_str(), location.as_str()),
        ("proxy-test", "europe-west4")
    );
    auth.metadata.remove("project_id");
    auth.metadata.insert("project".into(), " other ".into());
    auth.metadata.remove("location");
    let Ok(Target::ServiceAccount {
        project, location, ..
    }) = target(&auth)
    else {
        panic!("no service account");
    };
    assert_eq!(
        (project.as_str(), location.as_str()),
        ("other", "us-central1")
    );
}

#[test]
fn reports_unusable_credentials() {
    assert_eq!(
        target_error(&Auth::default()),
        "vertex executor: missing auth metadata"
    );
    let mut auth = service_account_auth("http://127.0.0.1:9/token", "");
    auth.metadata.insert("project_id".into(), "  ".into());
    assert_eq!(
        target_error(&auth),
        "vertex executor: missing project_id in credentials"
    );
    let mut auth = service_account_auth("http://127.0.0.1:9/token", "");
    auth.metadata
        .insert("service_account".into(), "not an object".into());
    assert_eq!(
        target_error(&auth),
        "vertex executor: missing service_account in credentials"
    );

    // A broken account names the problem but not the key.
    let mut auth = service_account_auth("http://127.0.0.1:9/token", "");
    let key = auth.metadata["service_account"]["private_key"]
        .as_str()
        .unwrap()
        .to_owned();
    let broken = key.replace("MII", "!!!");
    auth.metadata["service_account"]["private_key"] = Value::from(broken);
    let message = target_error(&auth);
    assert!(message.starts_with("vertex executor: "), "{message}");
    assert!(!message.contains("!!!"), "{message}");
    assert!(!message.contains(&key[40..80]), "{message}");
}

#[tokio::test]
async fn api_key_calls_go_to_the_global_endpoint_or_base_url() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
    let mut alt = options(&Format::GEMINI);
    alt.alt = "json".into();
    executor()
        .execute(auth(&mock), request("gemini-2.5-pro(low)", payload), alt)
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(
        seen.target(),
        "/v1/publishers/google/models/gemini-2.5-pro:generateContent?$alt=json"
    );
    assert_eq!(seen.header("x-goog-api-key"), Some("test-vertex-key"));
    assert!(seen.header("authorization").is_none());
    assert!(seen.header("x-goog-api-client").is_none());
    assert_eq!(
        seen.header("user-agent"),
        Some(crate::codex::client::USER_AGENT)
    );
    assert_eq!(seen.json()["model"], "gemini-2.5-pro");

    let target = Target::ApiKey {
        key: "key",
        base_url: "",
    };
    assert_eq!(
        executor().url(&target, "gemini-2.5-pro", "generateContent"),
        "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.5-pro:generateContent"
    );
}

#[tokio::test]
async fn errors_hide_the_key_or_token() {
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
    let mock = Mock::start(Reply::error(
        403,
        r#"{"error":{"message":"key test-vertex-key is not allowed"}}"#,
    ))
    .await;
    let error = executor()
        .execute(
            auth(&mock),
            request("gemini-2.5-pro", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.status, error.message.as_str()),
        (
            403,
            r#"{"error":{"message":"key [redacted] is not allowed"}}"#
        )
    );

    let tokens = token_endpoint().await;
    let model = Mock::start(Reply::error(
        401,
        r#"{"error":{"message":"token sa-token expired"}}"#,
    ))
    .await;
    let executor = executor().with_service_account_base_url(model.url.clone());
    let auth = Arc::new(service_account_auth(&format!("{}/token", tokens.url), ""));
    let error = executor
        .execute(
            auth,
            request("gemini-2.5-pro", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.status, error.message.as_str()),
        (401, r#"{"error":{"message":"token [redacted] expired"}}"#)
    );
}

#[tokio::test]
async fn service_accounts_call_their_project_with_a_cached_token() {
    let tokens = token_endpoint().await;
    let model = Mock::answering(|seen| {
        if seen.path.ends_with(":countTokens") {
            Reply::json(r#"{"totalTokens":5}"#)
        } else if seen.path.ends_with(":streamGenerateContent") {
            Reply::sse(OK_STREAM)
        } else {
            Reply::json(OK_ANSWER)
        }
    })
    .await;
    let executor = executor().with_service_account_base_url(model.url.clone());
    let auth = Arc::new(service_account_auth(
        &format!("{}/token", tokens.url),
        "europe-west4",
    ));
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;

    let response = executor
        .execute(
            Arc::clone(&auth),
            request("gemini-2.5-pro", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let answer: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(answer["candidates"][0]["content"]["parts"][0]["text"], "ok");
    let seen = model.last();
    assert_eq!(
        seen.target(),
        "/v1/projects/proxy-test/locations/europe-west4/publishers/google/models/gemini-2.5-pro:generateContent"
    );
    assert_eq!(seen.header("authorization"), Some("Bearer sa-token"));
    assert!(seen.header("x-goog-api-key").is_none());
    assert!(seen.header("x-goog-api-client").is_none());

    let stream = executor
        .execute_stream(
            Arc::clone(&auth),
            request("gemini-2.5-pro", payload),
            stream_options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let (chunks, error) = collect(stream).await;
    assert!(error.is_none(), "{error:?}");
    assert!(
        chunks.iter().any(|chunk| chunk.contains("\"chunk\"")),
        "{chunks:?}"
    );
    assert_eq!(
        model.last().target(),
        "/v1/projects/proxy-test/locations/europe-west4/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
    );

    let count = executor
        .count_tokens(
            Arc::clone(&auth),
            request("gemini-2.5-pro", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    assert_eq!(
        count.payload,
        r#"{"totalTokens":5,"promptTokensDetails":[{"modality":"TEXT","tokenCount":5}]}"#
    );
    assert_eq!(
        model.last().target(),
        "/v1/projects/proxy-test/locations/europe-west4/publishers/google/models/gemini-2.5-pro:countTokens"
    );

    // One token served all three calls.
    assert_eq!(tokens.hits(), 1);
    assert!(
        model
            .requests()
            .iter()
            .all(|seen| seen.header("authorization") == Some("Bearer sa-token"))
    );

    // Without a test endpoint, the location's own is used.
    let target = target(&auth).unwrap();
    assert_eq!(
        VertexExecutor::new("direct").url(&target, "gemini-2.5-pro", "generateContent"),
        "https://europe-west4-aiplatform.googleapis.com/v1/projects/proxy-test/locations/europe-west4/publishers/google/models/gemini-2.5-pro:generateContent"
    );
}

#[tokio::test]
async fn token_failures_are_internal_errors() {
    let tokens = Mock::start(Reply::error(
        400,
        r#"{"error":"invalid_grant","error_description":"Invalid JWT"}"#,
    ))
    .await;
    let model = Mock::start(Reply::json(OK_ANSWER)).await;
    let executor = executor().with_service_account_base_url(model.url.clone());
    let auth = Arc::new(service_account_auth(&format!("{}/token", tokens.url), ""));
    let error = executor
        .execute(
            auth,
            request("gemini-2.5-pro", r#"{"contents":[]}"#),
            options(&Format::GEMINI),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 500);
    assert_eq!(error.message, "internal server error");
    assert_eq!(model.hits(), 0);
}

#[tokio::test]
async fn access_tokens_use_the_credential_proxy() {
    let auth_proxy = Mock::start(Reply::error(502, "auth proxy")).await;
    let global_proxy = Mock::start(Reply::error(502, "global proxy")).await;
    let executor = VertexExecutor::new(global_proxy.url.clone());
    let mut auth = service_account_auth("http://oauth.test/token", "");
    auth.proxy_url = auth_proxy.url.clone();
    let error = executor
        .execute(
            Arc::new(auth),
            request("gemini-2.5-pro", r#"{"contents":[]}"#),
            options(&Format::GEMINI),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.status, error.message.as_str()),
        (500, "internal server error")
    );
    assert!(
        auth_proxy.hits() > 0,
        "the token exchange skipped the credential proxy"
    );
    assert_eq!(auth_proxy.last().uri, "http://oauth.test/token");
    assert_eq!(
        global_proxy.hits(),
        0,
        "the token exchange used the global proxy"
    );
}

#[test]
fn builds_imagen_requests() {
    let request = |payload: Value| convert_to_imagen_request(payload.to_string().as_bytes());

    let body = request(json!({
        "contents": [{"role": "user", "parts": [{"text": "a red fox"}]}],
        "aspectRatio": "16:9",
        "sampleCount": 2,
        "negativePrompt": "blur & <noise>",
    }))
    .unwrap();
    assert_eq!(
        body.to_string(),
        r#"{"instances":[{"negativePrompt":"blur & <noise>","prompt":"a red fox"}],"parameters":{"aspectRatio":"16:9","sampleCount":2}}"#
    );

    // The first message with content, else a prompt field.
    let body = request(json!({"messages": [{"role": "system"}, {"role": "user", "content": ""}, {"role": "user", "content": "a cat"}]})).unwrap();
    assert_eq!(
        body,
        json!({"instances": [{"prompt": "a cat"}], "parameters": {"sampleCount": 1}})
    );
    let body = request(json!({"prompt": "a dog", "sampleCount": "3"})).unwrap();
    assert_eq!(
        body,
        json!({"instances": [{"prompt": "a dog"}], "parameters": {"sampleCount": 3}})
    );

    for payload in [json!({}), json!({"contents": [{"parts": [{"text": ""}]}]})] {
        let error = request(payload).unwrap_err();
        assert_eq!(error.message, "imagen: no prompt found in request");
    }
}

#[test]
fn turns_imagen_answers_into_gemini_answers() {
    let data = br#"{"predictions":[{"bytesBase64Encoded":"aGVsbG8=","mimeType":"image/jpeg"},{"mimeType":"image/png"},{"bytesBase64Encoded":"d29ybGQ="}]}"#;
    let answer: Value = serde_json::from_slice(&convert_imagen_to_gemini_response(
        data.to_vec(),
        "imagen-4",
    ))
    .unwrap();
    assert_eq!(
        answer["candidates"],
        json!([{
            "content": {
                "parts": [
                    {"inlineData": {"data": "aGVsbG8=", "mimeType": "image/jpeg"}},
                    {"inlineData": {"data": "d29ybGQ=", "mimeType": "image/png"}},
                ],
                "role": "model",
            },
            "finishReason": "STOP",
        }])
    );
    assert_eq!(answer["modelVersion"], "imagen-4");
    assert!(
        answer["responseId"]
            .as_str()
            .unwrap()
            .starts_with("imagen-")
    );
    assert_eq!(
        answer["usageMetadata"],
        json!({"candidatesTokenCount": 0, "promptTokenCount": 0, "totalTokenCount": 0})
    );

    // Anything else is left as it is.
    for data in [
        &br#"{"error":"x"}"#[..],
        b"not json",
        br#"{"predictions":{}}"#,
    ] {
        assert_eq!(
            convert_imagen_to_gemini_response(data.to_vec(), "imagen-4"),
            data
        );
    }
}

#[tokio::test]
async fn service_accounts_call_imagen_with_its_own_requests() {
    let tokens = token_endpoint().await;
    let model = Mock::start(Reply::json(
        r#"{"predictions":[{"bytesBase64Encoded":"aGVsbG8=","mimeType":"image/png"}]}"#,
    ))
    .await;
    let executor = executor().with_service_account_base_url(model.url.clone());
    let auth = Arc::new(service_account_auth(
        &format!("{}/token", tokens.url),
        "us-east5",
    ));
    let payload =
        r#"{"contents":[{"role":"user","parts":[{"text":"a red fox"}]}],"aspectRatio":"1:1"}"#;
    let response = executor
        .execute(
            Arc::clone(&auth),
            request("imagen-4.0-generate-001", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let seen = model.last();
    assert_eq!(
        seen.target(),
        "/v1/projects/proxy-test/locations/us-east5/publishers/google/models/imagen-4.0-generate-001:predict"
    );
    assert_eq!(seen.header("authorization"), Some("Bearer sa-token"));
    assert_eq!(
        seen.json(),
        json!({"instances": [{"prompt": "a red fox"}], "parameters": {"aspectRatio": "1:1", "sampleCount": 1}})
    );
    let answer: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(
        answer["candidates"][0]["content"]["parts"][0]["inlineData"]["data"],
        "aGVsbG8="
    );

    // A request without a prompt doesn't reach Imagen.
    let error = executor
        .execute(
            Arc::clone(&auth),
            request("imagen-4.0-generate-001", "{}"),
            options(&Format::GEMINI),
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, "imagen: no prompt found in request");
    assert_eq!(model.hits(), 1);

    // Streaming Imagen gets no SSE parameters.
    let stream = executor
        .execute_stream(
            auth,
            request("imagen-4.0-generate-001", payload),
            stream_options(&Format::GEMINI),
        )
        .await
        .unwrap();
    collect(stream).await;
    assert_eq!(
        model.last().target(),
        "/v1/projects/proxy-test/locations/us-east5/publishers/google/models/imagen-4.0-generate-001:predict"
    );
}

#[tokio::test]
async fn api_keys_call_imagen_with_the_gemini_request() {
    let predictions = r#"{"predictions":[{"bytesBase64Encoded":"aGVsbG8="}]}"#;
    let mock = Mock::start(Reply::json(predictions)).await;
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"a red fox"}]}]}"#;
    let response = executor()
        .execute(
            auth(&mock),
            request("imagen-4.0-generate-001", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(
        seen.target(),
        "/v1/publishers/google/models/imagen-4.0-generate-001:predict"
    );
    assert_eq!(seen.json()["contents"][0]["parts"][0]["text"], "a red fox");
    let answer: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(answer["predictions"][0]["bytesBase64Encoded"], "aGVsbG8=");
}

#[tokio::test]
async fn streams_lines_as_they_come() {
    let mock = Mock::start(Reply::sse(OK_STREAM)).await;
    let mut alt = stream_options(&Format::GEMINI);
    let response = executor()
        .execute_stream(
            auth(&mock),
            request(
                "gemini-2.5-flash",
                r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
            ),
            alt.clone(),
        )
        .await
        .unwrap();
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert!(
        chunks.iter().any(|chunk| chunk.contains("\"chunk\"")),
        "{chunks:?}"
    );
    assert_eq!(
        mock.last().target(),
        "/v1/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );

    alt.alt = "json".into();
    let response = executor()
        .execute_stream(auth(&mock), request("gemini-2.5-flash", "{}"), alt)
        .await
        .unwrap();
    collect(response).await;
    assert_eq!(
        mock.last().target(),
        "/v1/publishers/google/models/gemini-2.5-flash:streamGenerateContent?$alt=json"
    );
}

#[tokio::test]
async fn streams_to_claude_clients() {
    // Each line reaches the translator as it came, `data:` and all.
    let answer = r#"data: {"responseId":"r1","modelVersion":"gemini-2.5-pro","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"Hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1}}"#;
    let mock = Mock::start(Reply::sse(&format!("{answer}\n\n"))).await;
    let response = executor()
        .execute_stream(
            auth(&mock),
            request(
                "gemini-2.5-pro",
                r#"{"model":"gemini-2.5-pro","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
            ),
            stream_options(&Format::CLAUDE),
        )
        .await
        .unwrap();
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    let streamed = chunks.concat();
    for expected in [
        r#""id":"r1","type":"message","role":"assistant","content":[],"model":"gemini-2.5-pro""#,
        r#""delta":{"type":"text_delta","text":"Hello"}"#,
        r#""delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":2,"output_tokens":1}"#,
        "event: message_stop\ndata: {\"type\":\"message_stop\"}",
    ] {
        assert!(streamed.contains(expected), "{expected} not in {streamed}");
    }
}

#[tokio::test]
async fn stream_errors_hide_the_key_and_token() {
    let error = |secret: &str| {
        let error = format!(r#"{{"error":{{"code":401,"message":"bad credential {secret}"}}}}"#);
        Reply::sse(&format!("data: {error}\n\n"))
    };
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;

    // An express API key.
    let mock = Mock::start(error("test-vertex-key")).await;
    let response = executor()
        .execute_stream(
            auth(&mock),
            request("gemini-2.5-flash", payload),
            stream_options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let streamed = collect(response).await.0.concat();
    assert!(streamed.contains("bad credential [redacted]"), "{streamed}");
    assert!(!streamed.contains("test-vertex-key"), "{streamed}");

    // A service account's token.
    let tokens = Mock::start(Reply::json(
        r#"{"access_token":"sa-token-secret","expires_in":3600,"token_type":"Bearer"}"#,
    ))
    .await;
    let model = Mock::start(error("sa-token-secret")).await;
    let executor = executor().with_service_account_base_url(model.url.clone());
    let auth = Arc::new(service_account_auth(
        &format!("{}/token", tokens.url),
        "us-central1",
    ));
    let response = executor
        .execute_stream(
            auth,
            request("gemini-2.5-pro", payload),
            stream_options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let streamed = collect(response).await.0.concat();
    assert_eq!(
        model.last().header("authorization"),
        Some("Bearer sa-token-secret")
    );
    assert!(streamed.contains("bad credential [redacted]"), "{streamed}");
    assert!(!streamed.contains("sa-token-secret"), "{streamed}");
}

#[tokio::test]
async fn rejects_compact_calls_and_returns_upstream_errors() {
    let body = r#"{"error":{"code":403,"message":"denied"}}"#;
    let mock = Mock::start(Reply::error(403, body)).await;
    let mut compact = options(&Format::OPENAI_RESPONSE);
    compact.alt = "responses/compact".into();
    let error = executor()
        .execute(auth(&mock), request("gemini-2.5-flash", "{}"), compact)
        .await
        .unwrap_err();
    assert_eq!(error.status, 501);
    assert_eq!(mock.hits(), 0);

    let error = executor()
        .execute(
            auth(&mock),
            request("gemini-2.5-flash", r#"{"contents":[]}"#),
            options(&Format::GEMINI),
        )
        .await
        .unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (403, body));
}

#[tokio::test]
async fn refreshes_to_the_same_credential() {
    assert_eq!(executor().id(), "vertex");
    let auth = Arc::new(service_account_auth("http://127.0.0.1:9/token", ""));
    let refreshed = executor().refresh(Arc::clone(&auth)).await.unwrap();
    assert_eq!(refreshed.metadata, auth.metadata);
}

// Not upstream's: an error quotes none of the secrets the request sent (the
// credential headers after the custom ones, each cookie, the URL's
// credentials), nor the password of a proxy that answers 407.
#[tokio::test]
async fn errors_hide_every_secret_sent() {
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
    for case in crate::secret_echo::cases(|base_url| {
        (*key_auth("vertex", "test-vertex-key", base_url)).clone()
    })
    .await
    {
        for stream in [false, true] {
            let options = Options {
                headers: case.headers.clone(),
                ..if stream {
                    stream_options(&Format::GEMINI)
                } else {
                    options(&Format::GEMINI)
                }
            };
            let auth = Arc::clone(&case.auth);
            let error = if stream {
                executor()
                    .execute_stream(auth, request("gemini-2.5-flash", payload), options)
                    .await
                    .err()
            } else {
                executor()
                    .execute(auth, request("gemini-2.5-flash", payload), options)
                    .await
                    .err()
            };
            case.check(&error.expect("the call went through"));
        }
    }
}
