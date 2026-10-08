// Ported from CLIProxyAPI
// sdk/api/handlers/openai/openai_speech_handlers_test.go
// (TestBuildXAISpeechPayloadMapsOpenAIVoice,
// TestBuildXAISpeechPayloadWavAndSpeed,
// TestBuildXAISpeechPayloadNativePCMKeepsSampleRate,
// TestBuildXAISpeechPayloadRejects,
// TestBuildXAISpeechPayloadRejectsLongInput, TestSpeechRoutingModel,
// TestSpeechResponseContentType) and openai_speech_routing_test.go
// (TestSpeechEndpointsRouteSpeechOnlyModelsToExecutor,
// TestChatAndResponsesRejectSpeechOnlyModels) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The speech endpoints against a scripted dispatcher.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use open_ferry_core::exec::{ExecError, Format, Response as ExecResponse};
use tower::ServiceExt;

use super::*;
use crate::config::ServerConfig;
use crate::router;
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome, state};

const KEY: &str = "sk-test";

/// The body built for `raw`, parsed, and its format.
fn built(raw: &str) -> (serde_json::Value, &'static str) {
    let (payload, format) =
        build_payload(raw.as_bytes()).unwrap_or_else(|message| panic!("{raw}: {message}"));
    let parsed = serde_json::from_str(&payload).unwrap_or_else(|err| panic!("{err}: {payload}"));
    (parsed, format)
}

// TestBuildXAISpeechPayloadMapsOpenAIVoice. Not upstream's: the body as
// written, its keys sorted as Go's `json.Marshal` sorts a map's.
#[test]
fn payload_maps_openai_voices() {
    let raw = r#"{"model":"tts-1","input":" Hello ","voice":"Alloy","response_format":"mp3"}"#;
    let (payload, format) = build_payload(raw.as_bytes()).unwrap();
    assert_eq!(format, "mp3");
    assert_eq!(
        payload,
        r#"{"language":"auto","text":"Hello","voice_id":"ara"}"#
    );
    let (body, _) = built(raw);
    assert!(body.get("output_format").is_none(), "{body}");
    assert!(body.get("model").is_none(), "{body}");
}

// TestBuildXAISpeechPayloadWavAndSpeed.
#[test]
fn payload_keeps_wav_and_speed() {
    let raw =
        r#"{"text":"你好","voice_id":"Luna","language":"zh","speed":1.25,"response_format":"wav"}"#;
    let (payload, format) = build_payload(raw.as_bytes()).unwrap();
    assert_eq!(format, "wav");
    assert_eq!(
        payload,
        r#"{"language":"zh","output_format":{"codec":"wav","sample_rate":24000},"speed":1.25,"text":"你好","voice_id":"luna"}"#
    );
}

// TestBuildXAISpeechPayloadNativePCMKeepsSampleRate.
#[test]
fn payload_keeps_a_native_pcm_sample_rate() {
    let (body, format) =
        built(r#"{"input":"pcm","output_format":{"codec":"pcm","sample_rate":16000}}"#);
    assert_eq!(format, "pcm");
    assert_eq!(body["voice_id"], "eve");
    assert_eq!(body["output_format"]["codec"], "pcm");
    assert_eq!(body["output_format"]["sample_rate"], 16000);
}

// TestBuildXAISpeechPayloadRejects and
// TestBuildXAISpeechPayloadRejectsLongInput.
#[test]
fn payload_rejects_bad_requests() {
    let cases = [
        (r#"{"voice":"eve"}"#, "input is required"),
        (
            r#"{"input":"hi","response_format":"flac"}"#,
            "response_format",
        ),
        ("{", "valid JSON"),
    ];
    for (raw, want) in cases {
        let err = build_payload(raw.as_bytes()).unwrap_err();
        assert!(err.contains(want), "{raw}: {err}");
    }
    let (body, _) = built(r#"{"input":"hi","speed":0}"#);
    assert!(body.get("speed").is_none(), "{body}");

    let long = format!(r#"{{"input":"{}"}}"#, "a".repeat(60_001));
    let err = build_payload(long.as_bytes()).unwrap_err();
    assert!(err.contains("60000"), "{err}");
    // Not upstream's: 60,000 characters, not bytes, are taken.
    let most = format!(r#"{{"input":"{}"}}"#, "é".repeat(60_000));
    assert!(build_payload(most.as_bytes()).is_ok());
}

// Not upstream's: the edges of the fields as upstream reads them.
#[test]
fn payload_reads_fields_as_upstream() {
    // `text` when `input` is blank; `voice_id` when `voice` is; a voice
    // that isn't OpenAI's is passed on, lower case.
    let (body, _) = built(r#"{"input":"  ","text":"x","voice":" ","voice_id":" Rex2 "}"#);
    assert_eq!(body["text"], "x");
    assert_eq!(body["voice_id"], "rex2");
    // A speed that isn't a positive number is dropped; a format names its
    // own default sample rate.
    for speed in ["\"1.5\"", "-1", "true", "null"] {
        let (body, _) = built(&format!(r#"{{"input":"x","speed":{speed}}}"#));
        assert!(body.get("speed").is_none(), "{speed}: {body}");
    }
    let (payload, _) = build_payload(br#"{"input":"x","speed":1e-7}"#).unwrap();
    assert!(payload.contains(r#""speed":1e-7"#), "{payload}");
    let err = build_payload(br#"{"input":"x","speed":1e999}"#).unwrap_err();
    assert_eq!(err, "json: unsupported value: +Inf");
    // `response_format` wins over `output_format`, which must be an object.
    let (body, format) =
        built(r#"{"input":"x","response_format":" PCM ","output_format":{"codec":"wav"}}"#);
    assert_eq!(format, "pcm");
    assert_eq!(body["output_format"]["sample_rate"], 24000);
    let err = build_payload(br#"{"input":"x","output_format":"wav"}"#).unwrap_err();
    assert_eq!(err, "output_format must be an object");
    // A native codec of MP3, or none, sends no format.
    let (body, format) = built(r#"{"input":"x","output_format":{"sample_rate":8000}}"#);
    assert_eq!(format, "mp3");
    assert!(body.get("output_format").is_none(), "{body}");
    // A sample rate that isn't a positive number, or doesn't fit, is
    // xAI's default; a fraction is cut.
    for (rate, want) in [
        ("0", 24000),
        ("\"8000\"", 24000),
        ("1e30", 24000),
        ("8000.9", 8000),
    ] {
        let raw =
            format!(r#"{{"input":"x","output_format":{{"codec":"wav","sample_rate":{rate}}}}}"#);
        let (body, _) = built(&raw);
        assert_eq!(body["output_format"]["sample_rate"], want, "{rate}");
    }
    // Text is escaped as Go escapes it, HTML included.
    let (payload, _) = build_payload(br#"{"input":"<a & b>"}"#).unwrap();
    assert!(
        payload.contains(r#""text":"\u003ca \u0026 b\u003e""#),
        "{payload}"
    );
}

// TestSpeechRoutingModel.
#[test]
fn speech_models_route_to_grok() {
    let cases = [
        ("", Some("grok-tts")),
        ("tts-1-hd", Some("grok-tts")),
        ("gpt-4o-mini-tts", Some("grok-tts")),
        ("xai/grok-tts", Some("grok-tts")),
        ("grok-voice-tts-1.0", Some("grok-voice-tts-1.0")),
        ("x-ai/grok-voice-tts-1.0(high)", Some("grok-voice-tts-1.0")),
        ("grok-4.7", None),
        ("openai/tts-1", None),
        // Not upstream's.
        ("  TTS-1  ", Some("grok-tts")),
        ("GROK/Grok-TTS", Some("grok-tts")),
        ("(grok-tts)", None),
    ];
    for (model, want) in cases {
        assert_eq!(routing_model(model), want, "{model:?}");
    }
}

// TestSpeechResponseContentType.
#[test]
fn audio_content_type_falls_back_to_the_format() {
    let value = HeaderValue::from_static;
    assert_eq!(response_content_type("mp3", None), "audio/mpeg");
    assert_eq!(
        response_content_type("wav", Some(&value("application/json"))),
        "audio/wav"
    );
    assert_eq!(
        response_content_type("pcm", Some(&value("audio/L16; rate=24000"))),
        "audio/L16; rate=24000"
    );
    // Not upstream's: the other generic types, with parameters.
    for generic in [
        "",
        "Text/Plain; charset=utf-8",
        " application/octet-stream ",
    ] {
        assert_eq!(
            response_content_type("pcm", Some(&value(generic))),
            "audio/pcm",
            "{generic:?}"
        );
    }
}

struct Server {
    app: Router,
    dispatcher: Arc<FakeDispatcher>,
}

/// A server with `config` and the client key `sk-test`, serving the speech
/// models and a chat model through `xai`, its dispatcher giving `outcomes`.
fn server_with(config: ServerConfig, outcomes: Vec<Outcome>) -> Server {
    let catalog = FakeCatalog::new()
        .serve("grok-tts", &["xai"])
        .serve("xai/grok-tts", &["xai"])
        .serve("grok-voice-tts-1.0", &["xai"])
        .serve("grok-4", &["xai"]);
    let dispatcher = FakeDispatcher::new(outcomes);
    let config = ServerConfig {
        api_keys: vec![KEY.into()],
        ..config
    };
    Server {
        app: router(state(config, catalog, &dispatcher)),
        dispatcher,
    }
}

fn server(outcomes: Vec<Outcome>) -> Server {
    server_with(ServerConfig::default(), outcomes)
}

fn post(uri: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.into())
        .unwrap()
}

/// The status, headers and body of the response to `request`.
async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// An answer of MP3 audio, as upstream's test executor gives.
fn audio(content_type: &'static str) -> Outcome {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    Outcome::Reply(ExecResponse {
        payload: Bytes::from_static(b"ID3audio"),
        headers,
    })
}

// TestSpeechEndpointsRouteSpeechOnlyModelsToExecutor: both endpoints send
// the speech models on, in the speech format, and answer with the audio.
#[tokio::test]
async fn speech_endpoints_route_speech_models_to_the_executor() {
    let cases = [
        (
            "/v1/audio/speech",
            r#"{"model":"tts-1","input":"hello","voice":"nova"}"#,
            "grok-tts",
        ),
        (
            "/v1/audio/speech",
            r#"{"model":"xai/grok-tts","input":"hello"}"#,
            "grok-tts",
        ),
        (
            "/v1/audio/speech",
            r#"{"model":"grok-voice-tts-1.0","input":"hello"}"#,
            "grok-voice-tts-1.0",
        ),
        (
            "/v1/tts",
            r#"{"text":"hello","voice_id":"eve"}"#,
            "grok-tts",
        ),
    ];
    for (path, body, model) in cases {
        let server = server(vec![audio("audio/mpeg")]);
        let (status, headers, answer) = send(&server.app, post(path, body)).await;
        assert_eq!(status, StatusCode::OK, "{path} {body}: {answer}");
        assert_eq!(answer, "ID3audio");
        assert_eq!(headers[header::CONTENT_TYPE], "audio/mpeg");
        assert!(
            headers
                .get(header::CONTENT_LENGTH)
                .is_none_or(|len| len == "8")
        );
        let calls = server.dispatcher.calls();
        let [call] = calls.as_slice() else {
            panic!("{path} {body}: calls {calls:?}");
        };
        assert_eq!(call.method, "execute");
        assert_eq!(call.request.model, model);
        assert_eq!(call.options.source_format, Format::OPENAI_SPEECH);
        assert!(!call.options.stream);
        assert_eq!(call.providers, ["xai"]);
    }
}

// Not upstream's: the body sent on is xAI's, built from the request, and a
// WAV request's audio is labelled WAV without xAI's headers.
#[tokio::test]
async fn speech_sends_xais_body() {
    let server = server(vec![audio("audio/mpeg")]);
    let body = r#"{"model":"tts-1","input":"hi","voice":"shimmer","response_format":"wav"}"#;
    let (status, headers, _) = send(&server.app, post("/v1/audio/speech", body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "audio/wav");
    let call = &server.dispatcher.calls()[0];
    assert_eq!(
        &call.request.payload[..],
        br#"{"language":"auto","output_format":{"codec":"wav","sample_rate":24000},"text":"hi","voice_id":"aurora"}"#
    );
}

// Not upstream's: with `passthrough-headers`, xAI's content type is kept
// unless it is generic, and its other headers are passed on.
#[tokio::test]
async fn speech_keeps_xais_content_type_with_passthrough() {
    let config = ServerConfig {
        passthrough_headers: true,
        ..ServerConfig::default()
    };
    let mut reply_headers = HeaderMap::new();
    reply_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("audio/L16; rate=24000"),
    );
    reply_headers.insert("x-request-id", HeaderValue::from_static("req-1"));
    let reply = Outcome::Reply(ExecResponse {
        payload: Bytes::from_static(b"pcm"),
        headers: reply_headers,
    });
    let server = server_with(config, vec![reply, audio("application/octet-stream")]);
    let body = r#"{"input":"hi","response_format":"pcm"}"#;
    let (_, headers, answer) = send(&server.app, post("/v1/tts", body)).await;
    assert_eq!(answer, "pcm");
    assert_eq!(headers[header::CONTENT_TYPE], "audio/L16; rate=24000");
    assert_eq!(headers["x-request-id"], "req-1");
    let (_, headers, _) = send(&server.app, post("/v1/tts", body)).await;
    assert_eq!(headers[header::CONTENT_TYPE], "audio/pcm");
}

// Not upstream's: the handler's own refusals, before any call.
#[tokio::test]
async fn speech_refuses_bad_requests_unsent() {
    let server = server(vec![]);
    let cases = [
        (
            r#"{"model":"grok-4","input":"hi"}"#.to_owned(),
            "Model grok-4 is not supported on /v1/audio/speech. Use grok-tts.",
        ),
        (r#"{"model":"tts-1"}"#.to_owned(), "input is required"),
        ("not json".to_owned(), "body must be valid JSON"),
        (
            format!(r#"{{"input":"{}"}}"#, "a".repeat(1 << 20)),
            "request body is larger than 1MB",
        ),
    ];
    for (body, want) in cases {
        let (status, headers, answer) = send(&server.app, post("/v1/audio/speech", body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{want}");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json"),
            "{want}"
        );
        let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(answer["error"]["message"], want);
        assert_eq!(answer["error"]["type"], "invalid_request_error");
    }
    assert!(server.dispatcher.calls().is_empty());
}

// Not upstream's: xAI's error comes back as on the other OpenAI routes.
#[tokio::test]
async fn speech_passes_xais_error_on() {
    let server = server(vec![Outcome::Fail(ExecError::upstream(
        404,
        r#"{"error":"voice not found"}"#,
    ))]);
    let body = r#"{"input":"hi","voice":"nope"}"#;
    let (status, _, answer) = send(&server.app, post("/v1/audio/speech", body)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(answer.contains("voice not found"), "{answer}");
}

// TestChatAndResponsesRejectSpeechOnlyModels: the chat and Responses routes
// turn the speech models away with a 4xx naming the speech endpoint, before
// any call, streamed or not.
#[tokio::test]
async fn chat_and_responses_reject_speech_only_models() {
    let server = server(vec![]);
    for model in ["grok-tts", "xai/grok-tts", "grok-voice-tts-1.0"] {
        for stream in [false, true] {
            let cases = [
                (
                    "/v1/chat/completions",
                    format!(
                        r#"{{"model":"{model}","stream":{stream},"messages":[{{"role":"user","content":"hi"}}]}}"#
                    ),
                ),
                (
                    "/v1/responses",
                    format!(r#"{{"model":"{model}","stream":{stream},"input":"hi"}}"#),
                ),
            ];
            for (path, body) in cases {
                let (status, _, answer) = send(&server.app, post(path, body)).await;
                assert!(
                    status.is_client_error(),
                    "{path} {model} stream={stream}: {status} {answer}"
                );
                assert!(
                    answer.contains("/v1/audio/speech"),
                    "{path} {model} stream={stream}: {answer}"
                );
            }
        }
    }
    assert!(server.dispatcher.calls().is_empty());
}
