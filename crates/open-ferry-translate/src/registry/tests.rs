//! Ported from upstream's registry_test.go, registry_bytes_test.go and
//! registry_summary_test.go. Tests of plugin hooks are not ported, nor is the
//! part of `TestRequestEnvelopePreservesRegisteredTransformDispatch` that
//! passes `ModelInfo` through the pipeline.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use super::*;
use crate::json::path;

fn identity() -> RequestTransform {
    Arc::new(|_, body, _| body)
}

fn returning(body: Value) -> RequestTransform {
    Arc::new(move |_, _, _| body.clone())
}

const NO_REQUEST: Value = Value::Null;

fn context() -> ResponseContext<'static> {
    ResponseContext {
        model: "model",
        original_request: &NO_REQUEST,
        request: &NO_REQUEST,
    }
}

struct Echo;

impl StreamTranslator for Echo {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        vec![chunk.to_vec()]
    }
}

struct Silent;

impl StreamTranslator for Silent {
    fn translate(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

#[derive(Debug)]
struct Invalid;

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid")
    }
}

impl Error for Invalid {}

/// Fails on its first chunk, as upstream's test translator does.
struct Failing(bool);

impl StreamTranslator for Failing {
    fn translate(&mut self, _: &[u8]) -> Vec<Vec<u8>> {
        self.0 = true;
        Vec::new()
    }

    fn tool_input_error(&self) -> Option<&(dyn Error + 'static)> {
        self.0.then_some(&Invalid as &(dyn Error + 'static))
    }
}

fn stream_of<T: StreamTranslator + 'static>(make: fn() -> T) -> Option<StreamTransform> {
    Some(Arc::new(move |_| Box::new(make())))
}

#[test]
fn fallback_normalizes_model() {
    let registry = Registry::new();
    let cases = [
        (
            "gpt-5-mini",
            json!({"model": "copilot/gpt-5-mini", "input": "ping"}),
            json!({"model": "gpt-5-mini", "input": "ping"}),
        ),
        (
            "gpt-5-mini",
            json!({"model": "gpt-5-mini", "input": "ping"}),
            json!({"model": "gpt-5-mini", "input": "ping"}),
        ),
        (
            "",
            json!({"model": "copilot/gpt-5-mini", "input": "ping"}),
            json!({"model": "copilot/gpt-5-mini", "input": "ping"}),
        ),
        (
            "gpt-5.3-codex",
            json!({"model": "team/gpt-5.3-codex", "stream": true}),
            json!({"model": "gpt-5.3-codex", "stream": true}),
        ),
    ];
    for (model, body, want) in cases {
        let got = registry.translate_request(&"a".into(), &"b".into(), model, body, false);
        assert_eq!(got, want);
    }
}

#[test]
fn fallback_sets_model_as_sjson_does() {
    let registry = Registry::new();
    let translate = |body| registry.translate_request(&"a".into(), &"b".into(), "m", body, false);
    assert_eq!(translate(json!({"model": 5})), json!({"model": "m"}));
    // A model that reads as the same text, as gjson reads it, is left alone.
    assert_eq!(
        registry.translate_request(&"a".into(), &"b".into(), "5", json!({"model": 5}), false),
        json!({"model": 5})
    );
    // A missing model goes last.
    assert_eq!(translate(json!({"a": 1})), json!({"a": 1, "model": "m"}));
    // sjson replaces a body that isn't an object, except an array.
    assert_eq!(translate(json!("s")), json!({"model": "m"}));
    assert_eq!(translate(Value::Null), json!({"model": "m"}));
    assert_eq!(translate(json!([1])), json!([1]));
    // An object or array model reads as compact JSON. Upstream reads the text
    // as the client wrote it, so `{ }` would be replaced there.
    for model in [json!({}), json!([1])] {
        let name = model.to_string();
        let body = json!({ "model": model });
        let got = registry.translate_request(&"a".into(), &"b".into(), &name, body.clone(), false);
        assert_eq!(got, body);
    }
}

#[test]
fn fallback_moves_format_to_target() {
    let registry = Registry::new();
    let req = RequestEnvelope {
        format: Format::OPENAI,
        model: String::new(),
        stream: true,
        body: json!({}),
    };
    let got = registry.translate_request_envelope(&Format::OPENAI, &Format::CODEX, req);
    assert_eq!(got.format, Format::CODEX);
    assert!(got.stream);
}

#[test]
fn registered_transform_takes_precedence() {
    let registry = Registry::new();
    let format = Format::OPENAI_RESPONSE;
    registry.register(
        format.clone(),
        format.clone(),
        Some(returning(json!({"model": "from-transform"}))),
        ResponseTransform::default(),
    );
    let got = registry.translate_request(
        &format,
        &format,
        "gpt-5-mini",
        json!({"model": "copilot/gpt-5-mini", "input": "ping"}),
        false,
    );
    assert_eq!(got["model"], "from-transform");
}

#[test]
fn has_request_transformer() {
    let registry = Registry::new();
    let (from, to) = (Format::from("from"), Format::from("to"));
    assert!(!registry.has_request_transformer(&from, &to));
    registry.register(
        from.clone(),
        to.clone(),
        Some(identity()),
        ResponseTransform::default(),
    );
    assert!(registry.has_request_transformer(&from, &to));
}

#[test]
fn registering_without_a_request_transform_keeps_the_old_one() {
    let registry = Registry::new();
    let (from, to) = (Format::from("from"), Format::from("to"));
    registry.register(
        from.clone(),
        to.clone(),
        Some(identity()),
        ResponseTransform::default(),
    );
    registry.register(
        from.clone(),
        to.clone(),
        None,
        ResponseTransform {
            stream: stream_of(|| Echo),
            ..Default::default()
        },
    );
    assert!(registry.has_request_transformer(&from, &to));
    assert!(registry.has_stream_response_transformer(&from, &to));
}

#[test]
fn has_response_transformer_ignores_empty_registration() {
    let registry = Registry::new();
    let (from, to) = (Format::from("from"), Format::from("to"));
    registry.register(
        from.clone(),
        to.clone(),
        Some(identity()),
        ResponseTransform::default(),
    );
    assert!(!registry.has_response_transformer(&from, &to));
    assert!(!registry.has_stream_response_transformer(&from, &to));
    assert!(!registry.has_non_stream_response_transformer(&from, &to));
}

#[test]
fn has_response_transformer_checks_concrete_response_kinds() {
    let registry = Registry::new();
    let from = Format::from("from");
    let stream_only = Format::from("stream-to");
    let non_stream_only = Format::from("non-stream-to");
    let token_count_only = Format::from("token-count-to");
    registry.register(
        from.clone(),
        stream_only.clone(),
        None,
        ResponseTransform {
            stream: stream_of(|| Echo),
            ..Default::default()
        },
    );
    registry.register(
        from.clone(),
        non_stream_only.clone(),
        None,
        ResponseTransform {
            non_stream: Some(Arc::new(|_, body| Some(body.to_vec()))),
            ..Default::default()
        },
    );
    registry.register(
        from.clone(),
        token_count_only.clone(),
        None,
        ResponseTransform {
            token_count: Some(Arc::new(|_| Vec::new())),
            ..Default::default()
        },
    );

    assert!(registry.has_response_transformer(&from, &stream_only));
    assert!(registry.has_stream_response_transformer(&from, &stream_only));
    assert!(!registry.has_non_stream_response_transformer(&from, &stream_only));

    assert!(registry.has_response_transformer(&from, &non_stream_only));
    assert!(!registry.has_stream_response_transformer(&from, &non_stream_only));
    assert!(registry.has_non_stream_response_transformer(&from, &non_stream_only));

    assert!(registry.has_response_transformer(&from, &token_count_only));
    assert!(!registry.has_stream_response_transformer(&from, &token_count_only));
    assert!(!registry.has_non_stream_response_transformer(&from, &token_count_only));

    let mut stream = registry.response_stream(&stream_only, &from, &context());
    assert!(stream.is_translated());
    assert_eq!(
        stream.translate(br#"data: {"ok":true}"#),
        [br#"data: {"ok":true}"#]
    );
}

#[test]
fn response_lookups_go_from_provider_to_client() {
    let registry = Registry::new();
    let (client, provider) = (Format::from("client"), Format::from("provider"));
    registry.register(
        client.clone(),
        provider.clone(),
        None,
        ResponseTransform {
            stream: stream_of(|| Silent),
            non_stream: Some(Arc::new(|_, _| Some(b"native".to_vec()))),
            token_count: Some(Arc::new(|count| count.to_string().into_bytes())),
        },
    );
    assert!(
        registry
            .response_stream(&provider, &client, &context())
            .is_translated()
    );
    assert!(
        !registry
            .response_stream(&client, &provider, &context())
            .is_translated()
    );
    assert_eq!(
        registry.translate_non_stream(&provider, &client, &context(), b"raw".to_vec()),
        Some(b"native".to_vec())
    );
    assert_eq!(
        registry.translate_non_stream(&client, &provider, &context(), b"raw".to_vec()),
        Some(b"raw".to_vec())
    );
    assert_eq!(
        registry.translate_token_count(&provider, &client, 7, b"raw".to_vec()),
        b"7"
    );
    assert_eq!(
        registry.translate_token_count(&client, &provider, 7, b"raw".to_vec()),
        b"raw"
    );
}

#[test]
fn stream_without_translator_passes_chunks_on() {
    let registry = Registry::new();
    let mut stream = registry.response_stream(&"a".into(), &"b".into(), &context());
    assert!(!stream.is_translated());
    assert_eq!(stream.translate(b"data: {}"), [b"data: {}"]);
    assert!(stream.translate(b"").is_empty());
    assert!(stream.finish().is_empty());
    assert!(stream.tool_input_error().is_none());
}

#[test]
fn native_empty_stream_output_suppresses_raw_fallback() {
    let registry = Registry::new();
    let (client, provider) = (Format::from("client"), Format::from("upstream"));
    registry.register(
        provider.clone(),
        client.clone(),
        None,
        ResponseTransform {
            stream: stream_of(|| Silent),
            ..Default::default()
        },
    );
    let mut stream = registry.response_stream(&client, &provider, &context());
    assert!(stream.translate(br#"data: {"raw":true}"#).is_empty());
}

#[test]
fn request_envelope_transform_is_used_until_replaced() {
    let registry = Registry::new();
    let (from, to) = (Format::OPENAI_RESPONSE, Format::ANTIGRAVITY);
    registry.register_request_envelope(
        from.clone(),
        to.clone(),
        Arc::new(|mut req: RequestEnvelope| {
            req.body = json!({"source": "envelope", "model": req.model});
            req
        }),
    );
    let got =
        registry.translate_request(&from, &to, "home-model", json!({"input": "hello"}), false);
    assert_eq!(got, json!({"source": "envelope", "model": "home-model"}));

    // A custom registration replaces the envelope transform.
    registry.register(
        from.clone(),
        to.clone(),
        Some(returning(json!({"source": "custom"}))),
        ResponseTransform::default(),
    );
    for body in [
        json!({"input": "hello"}),
        json!({"input": "weather", "tools": [{"type": "web_search"}]}),
    ] {
        let req = RequestEnvelope {
            format: from.clone(),
            model: "home-model".into(),
            stream: false,
            body,
        };
        let got = registry.translate_request_envelope(&from, &to, req);
        assert_eq!(got.body["source"], "custom");
        assert_eq!(got.format, to);
    }
}

#[test]
fn unregister_restores_format_pair() {
    let registry = Registry::global();
    let from = Format::from("unregister-default-from");
    let to = Format::from("unregister-default-to");
    let other = Format::from("unregister-default-other");
    assert!(!registry.has_request_transformer(&from, &to));
    assert!(!registry.has_request_transformer(&from, &other));
    registry.register(
        from.clone(),
        to.clone(),
        Some(identity()),
        ResponseTransform {
            non_stream: Some(Arc::new(|_, _| Some(br#"{"removed":true}"#.to_vec()))),
            ..Default::default()
        },
    );
    registry.register(
        from.clone(),
        other.clone(),
        Some(identity()),
        ResponseTransform::default(),
    );
    assert!(registry.has_request_transformer(&from, &to));
    assert!(registry.has_non_stream_response_transformer(&from, &to));
    assert!(registry.has_request_transformer(&from, &other));

    registry.unregister(&from, &to);
    assert!(!registry.has_request_transformer(&from, &to));
    assert!(!registry.has_non_stream_response_transformer(&from, &to));
    assert!(!registry.has_response_transformer(&from, &to));
    assert!(registry.has_request_transformer(&from, &other));

    registry.unregister(&from, &other);
    assert!(!registry.has_request_transformer(&from, &other));
    assert!(!format!("{registry:?}").contains("unregister-default"));
}

#[test]
fn retained_tool_input_failure_is_not_recovered() {
    let registry = Registry::new();
    registry.register(
        Format::OPENAI,
        Format::OPENAI_RESPONSE,
        None,
        ResponseTransform {
            stream: stream_of(|| Failing(false)),
            non_stream: Some(Arc::new(|_, _| None)),
            ..Default::default()
        },
    );
    let mut stream =
        registry.response_stream(&Format::OPENAI_RESPONSE, &Format::OPENAI, &context());
    assert!(stream.translate(br#"{"RAW_SECRET":true}"#).is_empty());
    assert_eq!(
        stream
            .tool_input_error()
            .map(ToString::to_string)
            .as_deref(),
        Some("invalid")
    );
    assert!(stream.translate(br#"{"RAW_SECRET":true}"#).is_empty());
    let got = registry.translate_non_stream(
        &Format::OPENAI_RESPONSE,
        &Format::OPENAI,
        &context(),
        br#"{"RAW_SECRET":true}"#.to_vec(),
    );
    assert_eq!(got, None);
}

#[test]
fn translators_run_outside_the_lock() {
    // A translator may use the registry it is registered in.
    let registry = Arc::new(Registry::new());
    let inner = Arc::downgrade(&registry);
    let ran = Arc::new(AtomicBool::new(false));
    let seen = ran.clone();
    registry.register(
        "a".into(),
        "b".into(),
        Some(Arc::new(move |_, body, _| {
            let registry = inner.upgrade().expect("registry is alive");
            registry.register(
                "c".into(),
                "d".into(),
                Some(identity()),
                ResponseTransform::default(),
            );
            seen.store(true, Ordering::SeqCst);
            body
        })),
        ResponseTransform::default(),
    );
    registry.translate_request(&"a".into(), &"b".into(), "", json!({}), false);
    assert!(ran.load(Ordering::SeqCst));
    assert!(registry.has_request_transformer(&"c".into(), &"d".into()));
}

#[test]
fn translate_stream_returns_byte_chunks() {
    let registry = Registry::new();
    registry.register(
        Format::OPENAI,
        Format::GEMINI,
        None,
        ResponseTransform {
            stream: stream_of(|| Echo),
            ..Default::default()
        },
    );
    let mut stream = registry.response_stream(&Format::GEMINI, &Format::OPENAI, &context());
    assert_eq!(
        stream.translate(br#"{"chunk":true}"#),
        [br#"{"chunk":true}"#]
    );
}

#[test]
fn translate_non_stream_returns_bytes() {
    let registry = Registry::new();
    registry.register(
        Format::OPENAI,
        Format::GEMINI,
        None,
        ResponseTransform {
            non_stream: Some(Arc::new(|_, body| Some(body.to_vec()))),
            ..Default::default()
        },
    );
    let got = registry.translate_non_stream(
        &Format::GEMINI,
        &Format::OPENAI,
        &context(),
        br#"{"done":true}"#.to_vec(),
    );
    assert_eq!(got.as_deref(), Some(&br#"{"done":true}"#[..]));
}

#[test]
fn translate_token_count_returns_bytes() {
    let registry = Registry::new();
    registry.register(
        Format::OPENAI,
        Format::GEMINI,
        None,
        ResponseTransform {
            token_count: Some(Arc::new(|_| br#"{"totalTokens":7}"#.to_vec())),
            ..Default::default()
        },
    );
    let got = registry.translate_token_count(
        &Format::GEMINI,
        &Format::OPENAI,
        7,
        br#"{"fallback":true}"#.to_vec(),
    );
    assert_eq!(got, br#"{"totalTokens":7}"#);
}

#[test]
fn translate_request_applies_summary_intent() {
    let cases = [
        // Chat effort leaves Claude's display unspecified.
        (
            Format::OPENAI,
            Format::CLAUDE,
            json!({"reasoning_effort": "high"}),
            json!({"thinking": {"type": "adaptive"}}),
            "thinking.display",
            None,
        ),
        // Chat's explicit exclusion hides Claude's summary.
        (
            Format::OPENAI,
            Format::CLAUDE,
            json!({"reasoning_effort": "high", "reasoning": {"exclude": true}}),
            json!({"thinking": {"type": "adaptive"}}),
            "thinking.display",
            Some(json!("omitted")),
        ),
        // Responses effort alone leaves Claude's display absent.
        (
            Format::OPENAI_RESPONSE,
            Format::CLAUDE,
            json!({"reasoning": {"effort": "high"}}),
            json!({"thinking": {"type": "adaptive"}}),
            "thinking.display",
            None,
        ),
        // A Responses summary shows Claude's.
        (
            Format::OPENAI_RESPONSE,
            Format::CLAUDE,
            json!({"reasoning": {"effort": "high", "summary": "auto"}}),
            json!({"thinking": {"type": "adaptive"}}),
            "thinking.display",
            Some(json!("summarized")),
        ),
        // A null Responses summary hides Gemini's.
        (
            Format::OPENAI_RESPONSE,
            Format::GEMINI,
            json!({"reasoning": {"effort": "high", "summary": null}}),
            json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "high"}}}),
            "generationConfig.thinkingConfig.includeThoughts",
            Some(json!(false)),
        ),
        // Google's Chat extension overrides effort.
        (
            Format::OPENAI,
            Format::GEMINI,
            json!({
                "reasoning_effort": "high",
                "extra_body": {"google": {"thinking_config": {"include_thoughts": false}}}
            }),
            json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "high", "includeThoughts": true}}}),
            "generationConfig.thinkingConfig.includeThoughts",
            Some(json!(false)),
        ),
    ];
    for (from, to, input, translated, at, want) in cases {
        let registry = Registry::new();
        registry.register(
            from.clone(),
            to.clone(),
            Some(returning(translated)),
            ResponseTransform::default(),
        );
        let out = registry.translate_request(&from, &to, "model", input.clone(), false);
        assert_eq!(path(&out, at), want.as_ref(), "{from} -> {to}: {input}");
    }
}

#[test]
fn translate_request_activates_claude_for_enabled_summary() {
    let registry = Registry::new();
    registry.register(
        Format::OPENAI_RESPONSE,
        Format::CLAUDE,
        Some(returning(
            json!({"model": "claude-opus-5", "max_tokens": 32000}),
        )),
        ResponseTransform::default(),
    );
    let out = registry.translate_request(
        &Format::OPENAI_RESPONSE,
        &Format::CLAUDE,
        "claude-opus-5",
        json!({"reasoning": {"summary": "auto"}, "input": "hi"}),
        false,
    );
    assert_eq!(out["thinking"]["type"], "adaptive");
    assert_eq!(out["thinking"]["display"], "summarized");
}

#[test]
fn translate_request_does_not_activate_claude_for_disabled_summary() {
    let registry = Registry::new();
    registry.register(
        Format::OPENAI_RESPONSE,
        Format::CLAUDE,
        Some(returning(
            json!({"model": "claude-opus-5", "max_tokens": 32000}),
        )),
        ResponseTransform::default(),
    );
    let out = registry.translate_request(
        &Format::OPENAI_RESPONSE,
        &Format::CLAUDE,
        "claude-opus-5",
        json!({"reasoning": {"summary": null}, "input": "hi"}),
        false,
    );
    assert!(out.get("thinking").is_none(), "{out}");
}

#[test]
fn translate_request_preserves_native_claude_missing_display() {
    let registry = Registry::new();
    let body = json!({"model": "claude-opus-5", "thinking": {"type": "adaptive"}});
    let out = registry.translate_request(
        &Format::CLAUDE,
        &Format::CLAUDE,
        "claude-opus-5",
        body.clone(),
        true,
    );
    assert_eq!(out, body);
}

#[test]
fn translate_request_does_not_mix_summary_into_fallback() {
    let registry = Registry::new();
    let body =
        json!({"model": "gemini-3.6-flash", "reasoning": {"summary": "auto"}, "input": "hi"});
    let out = registry.translate_request(
        &Format::OPENAI_RESPONSE,
        &Format::GEMINI,
        "gemini-3.6-flash",
        body.clone(),
        false,
    );
    assert_eq!(out, body);
}

#[test]
fn format_names() {
    assert_eq!(Format::OPENAI_RESPONSE.as_str(), "openai-response");
    assert_eq!(Format::from("codex"), Format::CODEX);
    assert_eq!(Format::from(String::from("claude")), Format::CLAUDE);
    assert_eq!(Format::new("gemini").to_string(), "gemini");
}

mod builtin;
