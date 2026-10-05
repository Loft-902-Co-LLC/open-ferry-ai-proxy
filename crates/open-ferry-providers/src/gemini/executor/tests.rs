//! The Gemini executor against a mock Gemini API on 127.0.0.1, ported from
//! upstream's `gemini_executor_test.go` and the Gemini tests of
//! `gemini_executor_signature_test.go`, with checks of the request URLs,
//! headers, errors and streams.
//!
//! Dropped:
//! - `AppliesPayloadRulesAfterLeadingUserNormalization`: it belongs to
//!   the payload rules' tests (see `crate::payload`'s).
//! - `InteractionsWithGeminiAPIKeyUsesGeminiEndpoint`, every
//!   `NativeInteractions` test and
//!   `NativeInteractionsSourceFormatAllowsSupportedEntryProtocols`: they
//!   are the Interactions executor's (see `interactions`' tests).
//!
//! Changed:
//! - `CountTokensPrependsLeadingUser` doesn't check the upstream-attempt
//!   tracker, which isn't ported, or call `Execute` with a `countTokens`
//!   action, which isn't either.
//! - `PrepareRequest_EmptyAPIKey_OmitsAuthHeaders` checks a call, since
//!   `PrepareRequest` isn't ported.
//! - The Claude signature tests don't pass the resolved model info of an
//!   API key, which isn't ported.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use http::HeaderValue;
use open_ferry_core::exec::{ErrorKind, Format};
use open_ferry_translate::signature::GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR;
use serde_json::json;

use super::*;
use crate::gemini::testing::{
    CLAUDE_SIGNATURE, Mock, OK_ANSWER, OK_STREAM, Reply, collect, function_call_payload, key_auth,
    native_gemini_signature, options, request, stream_options,
};

/// An executor that doesn't use the environment's proxy.
fn executor() -> GeminiExecutor {
    GeminiExecutor::new("direct")
}

fn auth(mock: &Mock) -> Arc<Auth> {
    key_auth("gemini", "test-key", &mock.url)
}

/// The roles of the request's turns.
fn roles(body: &Value) -> Vec<&str> {
    body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|content| content["role"].as_str().unwrap_or_default())
        .collect()
}

#[test]
fn caps_max_output_tokens_to_the_output_limit() {
    let mut body = json!({"generationConfig": {"maxOutputTokens": 500000, "temperature": 0.2}, "contents": []});
    cap_max_output_tokens(&mut body, "gemini-3.1-pro-preview", None);
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 65536);
    assert_eq!(body["generationConfig"]["temperature"], 0.2);
}

#[test]
fn leaves_allowed_or_unknown_max_output_tokens() {
    for (model, requested) in [
        ("gemini-3.1-pro-preview", 64000),
        ("custom-gemini-model", 500000),
    ] {
        let mut body = json!({"generationConfig": {"maxOutputTokens": requested}});
        cap_max_output_tokens(&mut body, model, None);
        assert_eq!(
            body["generationConfig"]["maxOutputTokens"], requested,
            "{model}"
        );
    }

    // Only a number is capped.
    let mut body = json!({"generationConfig": {"maxOutputTokens": "500000"}});
    let want = body.clone();
    cap_max_output_tokens(&mut body, "gemini-3.1-pro-preview", None);
    assert_eq!(body, want);
}

#[tokio::test]
async fn execute_caps_max_output_tokens_before_upstream() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"maxOutputTokens":500000}}"#;
    executor()
        .execute(
            auth(&mock),
            request("gemini-3.1-pro-preview", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    assert_eq!(
        mock.last().json()["generationConfig"]["maxOutputTokens"],
        65536
    );
}

#[tokio::test]
async fn execute_prepends_leading_user() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let payload = r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"key":"value"}}}]},{"role":"user","parts":[{"functionResponse":{"name":"lookup","response":{"result":"ok"}}}]}]}"#;
    executor()
        .execute(
            auth(&mock),
            request("gemini-3.7-flash", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(roles(&body), ["user", "model", "user"], "{body}");
    assert_eq!(body["contents"][0]["parts"][0]["text"], "");
}

/// Not upstream's: upstream sends the client's Gemini body as its bytes,
/// edited in place with sjson, so a call's arguments and the settings keep
/// each number as the client wrote it.
#[tokio::test]
async fn execute_sends_the_clients_numbers_as_written() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let payload = concat!(
        r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]},"#,
        r#"{"role":"model","parts":[{"functionCall":{"name":"f","args":{"x":-0,"y":1E20,"z":[1e5,-0.0]}}}]},"#,
        r#"{"role":"user","parts":[{"functionResponse":{"name":"f","response":{"n":-0}}}]}],"#,
        r#""generationConfig":{"temperature":-0,"topP":1E-1}}"#
    );
    executor()
        .execute(
            auth(&mock),
            request("gemini-3.7-flash", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let body = mock.last().body;
    for kept in [
        r#""args":{"x":-0,"y":1E20,"z":[1e5,-0.0]}"#,
        r#""response":{"n":-0}"#,
        r#""temperature":-0,"topP":1E-1"#,
    ] {
        assert!(body.contains(kept), "{kept} in {body}");
    }
}

/// Upstream's `issue4959ResponsesModelFirstPayload`: a Responses history
/// that starts with a reasoning carrier and a function call.
fn issue4959_payload() -> String {
    let signature = "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA";
    let carrier = format!(
        "cpa-gemini-responses-carrier-v1:next:function:{}",
        STANDARD_NO_PAD.encode(signature)
    );
    json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {"type": "reasoning", "id": "rs_resp_test_detached_before_0", "summary": [], "encrypted_content": carrier},
            {"type": "function_call", "call_id": "call_bash_1", "name": "Bash", "arguments": "{\"command\":\"true\"}"},
            {"type": "function_call_output", "call_id": "call_bash_1", "output": "ok"},
            {"role": "assistant", "content": [{"type": "output_text", "text": "first"}]},
            {"role": "assistant", "content": [{"type": "output_text", "text": "second"}]},
        ]
    })
    .to_string()
}

/// Whether `content` has a `kind` part named `name`.
fn has_named_part(content: &Value, kind: &str, name: &str) -> bool {
    content["parts"]
        .as_array()
        .is_some_and(|parts| parts.iter().any(|part| part[kind]["name"] == name))
}

#[tokio::test]
async fn execute_prepends_leading_user_for_issue_4959_responses_history() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    executor()
        .execute(
            auth(&mock),
            request("gemini-3.7-flash", &issue4959_payload()),
            options(&Format::OPENAI_RESPONSE),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    let contents = body["contents"].as_array().unwrap();
    assert!(contents.len() >= 3, "{body}");
    assert_eq!(contents[0]["role"], "user", "{body}");
    assert_eq!(contents[0]["parts"][0]["text"], "", "{body}");
    assert_eq!(contents[1]["role"], "model", "{body}");
    assert!(
        has_named_part(&contents[1], "functionCall", "Bash"),
        "{body}"
    );
    assert!(
        has_named_part(&contents[2], "functionResponse", "Bash"),
        "{body}"
    );
}

#[tokio::test]
async fn execute_appends_trailing_user_for_trailing_model_turn() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"text":"answer"}]}]}"#;
    executor()
        .execute(
            auth(&mock),
            request("gemini-3.7-flash", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(roles(&body), ["user", "model", "user"], "{body}");
    assert_eq!(body["contents"][2]["parts"][0]["text"], "");
}

#[tokio::test]
async fn count_tokens_prepends_leading_user() {
    let mock = Mock::start(Reply::json(r#"{"totalTokens":7}"#)).await;
    let payload = r#"{"contents":[{"role":"model","parts":[{"text":"prior output"}]}],"tools":[{"functionDeclarations":[]}],"generationConfig":{"temperature":1},"safetySettings":[]}"#;
    let response = executor()
        .count_tokens(
            auth(&mock),
            request("gemini-2.5-flash(1024)", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.target(), "/v1beta/models/gemini-2.5-flash:countTokens");
    let body = seen.json();
    assert_eq!(roles(&body), ["user", "model"], "{body}");
    assert_eq!(body["contents"][0]["parts"][0]["text"], "");
    assert_eq!(body["contents"][1]["parts"][0]["text"], "prior output");
    // Only the contents are counted.
    for field in ["tools", "generationConfig", "safetySettings"] {
        assert!(body.get(field).is_none(), "{field}: {body}");
    }
    // Written again by the Gemini translator, as upstream's is.
    assert_eq!(
        response.payload,
        r#"{"totalTokens":7,"promptTokensDetails":[{"modality":"TEXT","tokenCount":7}]}"#
    );
}

/// Upstream's `claudeRequestWithThinkingSignature`: a Claude request whose
/// assistant turn thought with the signature `signature`.
fn claude_request(signature: &str) -> Request {
    let payload = json!({
        "model": "claude-3-7-sonnet-20250219",
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Let me think...", "signature": signature},
                    {"type": "text", "text": "Here is the response."},
                ],
            },
            {"role": "user", "content": [{"type": "text", "text": "Follow up question."}]},
        ]
    });
    request("gemini-2.5-flash", &payload.to_string())
}

/// Asserts that the request body `seen` doesn't carry the Claude signature.
fn assert_no_claude_signature(seen: &str) {
    assert!(!seen.contains(CLAUDE_SIGNATURE), "{seen}");
}

#[tokio::test]
async fn execute_sanitizes_claude_signature() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    executor()
        .execute(
            auth(&mock),
            claude_request(CLAUDE_SIGNATURE),
            options(&Format::CLAUDE),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_no_claude_signature(&seen.body);
    for content in seen.json()["contents"].as_array().unwrap() {
        for part in content["parts"].as_array().into_iter().flatten() {
            assert_ne!(part["thoughtSignature"], CLAUDE_SIGNATURE);
        }
    }
}

#[tokio::test]
async fn execute_stream_sanitizes_claude_signature() {
    let mock = Mock::start(Reply::sse(OK_STREAM)).await;
    let response = executor()
        .execute_stream(
            auth(&mock),
            claude_request(CLAUDE_SIGNATURE),
            stream_options(&Format::CLAUDE),
        )
        .await
        .unwrap();
    collect(response).await;
    assert_no_claude_signature(&mock.last().body);
}

#[tokio::test]
async fn count_tokens_sanitizes_claude_signature() {
    let mock = Mock::start(Reply::json(r#"{"totalTokens": 42}"#)).await;
    executor()
        .count_tokens(
            auth(&mock),
            claude_request(CLAUDE_SIGNATURE),
            options(&Format::CLAUDE),
        )
        .await
        .unwrap();
    assert_no_claude_signature(&mock.last().body);
}

#[tokio::test]
async fn execute_replaces_claude_signature_of_function_call_with_bypass() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    executor()
        .execute(
            auth(&mock),
            request("gemini-2.5-flash", &function_call_payload(CLAUDE_SIGNATURE)),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(
        body["contents"][1]["parts"][0]["thoughtSignature"],
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "{body}"
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
    let response = executor()
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
    let answer: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(answer["candidates"][0]["content"]["parts"][0]["text"], "ok");
}

#[tokio::test]
async fn sends_the_key_and_no_google_client_headers() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
    let mut auth = Auth::clone(&auth(&mock));
    auth.attributes
        .insert("header:X-Custom".into(), "custom-value".into());
    auth.attributes
        .insert("header:X-Goog-Api-Client".into(), "gl-python/3.12".into());
    executor()
        .execute(
            Arc::new(auth.clone()),
            request("gemini-2.5-flash", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(
        seen.target(),
        "/v1beta/models/gemini-2.5-flash:generateContent"
    );
    assert_eq!(seen.header("x-goog-api-key"), Some("test-key"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(
        seen.header("user-agent"),
        Some(crate::codex::client::USER_AGENT)
    );
    assert_eq!(seen.header("x-custom"), Some("custom-value"));
    assert!(seen.header("x-goog-api-client").is_none());
    assert!(seen.header("authorization").is_none());

    // The client's own user agent, trimmed, goes through.
    let mut client = options(&Format::GEMINI);
    client
        .headers
        .insert("user-agent", HeaderValue::from_static(" my-client/1.0 "));
    executor()
        .execute(Arc::new(auth), request("gemini-2.5-flash", payload), client)
        .await
        .unwrap();
    assert_eq!(mock.last().header("user-agent"), Some("my-client/1.0"));
}

#[tokio::test]
async fn sends_no_key_when_it_is_empty() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let mut auth = Auth::clone(&key_auth("gemini", "", &mock.url));
    auth.attributes
        .insert("header:Custom-Token".into(), "gemini-secret".into());
    executor()
        .execute(
            Arc::new(auth),
            request("gemini-2.5-flash", r#"{"contents":[]}"#),
            options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert!(seen.header("x-goog-api-key").is_none());
    assert!(seen.header("authorization").is_none());
    assert_eq!(seen.header("custom-token"), Some("gemini-secret"));
}

#[test]
fn keys_are_sensitive_headers() {
    let auth = key_auth("gemini", "secret-key", "http://127.0.0.1:9");
    let headers = build_headers(
        &auth,
        &options(&Format::GEMINI),
        &Credential::ApiKey("secret-key"),
        NAME,
    )
    .unwrap();
    assert!(headers["x-goog-api-key"].is_sensitive());
    assert!(!format!("{headers:?}").contains("secret-key"));

    // A key that can't be a header fails without showing it.
    let error = build_headers(
        &auth,
        &options(&Format::GEMINI),
        &Credential::ApiKey("bad\nkey"),
        NAME,
    )
    .unwrap_err();
    assert_eq!(
        error.message,
        "gemini executor: the credential isn't a valid header value"
    );
}

#[test]
fn resolves_the_base_url() {
    let with = |base_url: &str| key_auth("gemini", "key", base_url);
    assert_eq!(base_url(&with("")), DEFAULT_BASE_URL);
    assert_eq!(base_url(&with("  ")), DEFAULT_BASE_URL);
    assert_eq!(base_url(&with("/")), DEFAULT_BASE_URL);
    assert_eq!(
        base_url(&with(" https://proxy.test/gemini/ ")),
        "https://proxy.test/gemini"
    );
    assert_eq!(
        model_url(&with(""), "gemini-2.5-pro", "generateContent"),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
    );
}

#[tokio::test]
async fn passes_alt_and_strips_the_thinking_suffix() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let mut alt = options(&Format::GEMINI);
    alt.alt = "json".into();
    executor()
        .execute(
            key_auth("gemini", "key", &format!("{}/", mock.url)),
            request(
                "gemini-2.5-flash(1024)",
                r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
            ),
            alt,
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(
        seen.target(),
        "/v1beta/models/gemini-2.5-flash:generateContent?$alt=json"
    );
    let body = seen.json();
    assert_eq!(body["model"], "gemini-2.5-flash");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 1024,
        "{body}"
    );
}

#[tokio::test]
async fn rejects_compact_calls() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let mut compact = options(&Format::OPENAI_RESPONSE);
    compact.alt = "responses/compact".into();
    let error = executor()
        .execute(
            auth(&mock),
            request("gemini-2.5-flash", "{}"),
            compact.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 501);
    assert_eq!(error.message, "/responses/compact not supported");
    let Err(error) = executor()
        .execute_stream(auth(&mock), request("gemini-2.5-flash", "{}"), compact)
        .await
    else {
        panic!("the stream was opened");
    };
    assert_eq!(error.status, 501);
    assert_eq!(mock.hits(), 0);
}

#[tokio::test]
async fn returns_upstream_errors() {
    let body = r#"{"error":{"code":429,"message":"quota","status":"RESOURCE_EXHAUSTED"}}"#;
    let mock = Mock::start(Reply::error(429, body)).await;
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
    let error = executor()
        .execute(
            auth(&mock),
            request("gemini-2.5-flash", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap_err();
    assert_eq!((error.kind, error.status), (ErrorKind::Upstream, 429));
    assert_eq!(error.message, body);
    let Err(error) = executor()
        .execute_stream(
            auth(&mock),
            request("gemini-2.5-flash", payload),
            stream_options(&Format::GEMINI),
        )
        .await
    else {
        panic!("the stream was opened");
    };
    assert_eq!((error.status, error.message.as_str()), (429, body));
    let error = executor()
        .count_tokens(
            auth(&mock),
            request("gemini-2.5-flash", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 429);

    // A provider that can't be reached is a connection error, without the
    // key.
    let error = executor()
        .execute(
            key_auth("gemini", "secret-key", "http://127.0.0.1:9"),
            request("gemini-2.5-flash", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 0);
    assert!(!error.message.contains("secret-key"));
}

#[tokio::test]
async fn errors_hide_the_key() {
    let body = r#"{"error":{"code":400,"message":"API key not valid: test-key-secret","status":"INVALID_ARGUMENT"}}"#;
    let mock = Mock::start(Reply::error(400, body)).await;
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
    let error = executor()
        .execute(
            key_auth("gemini", "test-key-secret", &mock.url),
            request("gemini-2.5-flash", payload),
            options(&Format::GEMINI),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(
        error.message,
        r#"{"error":{"code":400,"message":"API key not valid: [redacted]","status":"INVALID_ARGUMENT"}}"#
    );
}

#[tokio::test]
async fn stream_errors_hide_the_key() {
    let error = r#"{"error":{"code":400,"message":"invalid API key test-key-secret"}}"#;
    let mock = Mock::start(Reply::sse(&format!("data: {error}\n\n"))).await;
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
    let response = executor()
        .execute_stream(
            key_auth("gemini", "test-key-secret", &mock.url),
            request("gemini-2.5-flash", payload),
            stream_options(&Format::GEMINI),
        )
        .await
        .unwrap();
    let (chunks, _) = collect(response).await;
    let text = chunks.concat();
    assert!(text.contains("invalid API key [redacted]"), "{chunks:?}");
    assert!(!text.contains("test-key-secret"), "{chunks:?}");
}

#[tokio::test]
async fn streams_the_answer() {
    let mock = Mock::start(Reply::sse(OK_STREAM)).await;
    let response = executor()
        .execute_stream(
            auth(&mock),
            request(
                "gemini-2.5-flash",
                r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
            ),
            stream_options(&Format::GEMINI),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers.get("content-type").unwrap(),
        "text/event-stream"
    );
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert!(
        chunks.iter().any(|chunk| chunk.contains("\"chunk\"")),
        "{chunks:?}"
    );
    assert_eq!(
        mock.last().target(),
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );

    // With an `alt`, that goes instead.
    let mut alt = stream_options(&Format::GEMINI);
    alt.alt = "json".into();
    let response = executor()
        .execute_stream(auth(&mock), request("gemini-2.5-flash", "{}"), alt)
        .await
        .unwrap();
    collect(response).await;
    assert_eq!(
        mock.last().target(),
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?$alt=json"
    );
}

#[tokio::test]
async fn refreshes_to_the_same_credential() {
    let auth = key_auth("gemini", "key", "http://127.0.0.1:9");
    let refreshed = executor().refresh(Arc::clone(&auth)).await.unwrap();
    assert_eq!(refreshed.attributes, auth.attributes);
    assert_eq!(executor().id(), "gemini");
}

// Not upstream's: an error quotes none of the secrets the request sent (the
// credential headers after the custom ones, each cookie, the URL's
// credentials), nor the password of a proxy that answers 407.
#[tokio::test]
async fn errors_hide_every_secret_sent() {
    let payload = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
    for case in
        crate::secret_echo::cases(|base_url| (*key_auth("gemini", "test-key", base_url)).clone())
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

// Not upstream's: a model that says the key the request carried doesn't hand
// it on, in the answer to a call translated for any client or in a count
// that is handed on as it came (a Gemini count is made again from its
// total, so it couldn't say anything).
#[tokio::test]
async fn answers_hide_the_key() {
    let mock = Mock::answering(|seen| {
        let key = seen.header("x-goog-api-key").unwrap_or_default();
        let answer = if seen.path.ends_with(":countTokens") {
            json!({"totalTokens": 7, "note": format!("counted with {key}")})
        } else {
            json!({
                "candidates": [{
                    "content": {"role": "model", "parts": [{"text": format!("your key is {key}")}]},
                    "finishReason": "STOP",
                }],
                "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2},
            })
        };
        Reply::json(&answer.to_string())
    })
    .await;
    let auth = key_auth("gemini", "test-key-secret", &mock.url);
    let gemini = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
    let chat = r#"{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}"#;
    let claude = r#"{"model":"gemini-2.5-flash","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;
    for (format, payload) in [
        (Format::GEMINI, gemini),
        (Format::OPENAI, chat),
        (Format::CLAUDE, claude),
    ] {
        let response = executor()
            .execute(
                Arc::clone(&auth),
                request("gemini-2.5-flash", payload),
                options(&format),
            )
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&response.payload);
        assert!(!text.contains("test-key-secret"), "{format:?}: {text}");
        assert!(
            text.contains("your key is [redacted]"),
            "{format:?}: {text}"
        );
    }
    let response = executor()
        .count_tokens(
            auth,
            request(
                "gemini-2.5-flash",
                r#"{"model":"gemini-2.5-flash","input":"hi"}"#,
            ),
            options(&Format::OPENAI_RESPONSE),
        )
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&response.payload);
    assert!(!text.contains("test-key-secret"), "{text}");
    assert!(text.contains("counted with [redacted]"), "{text}");
    assert_eq!(mock.hits(), 4);
}
