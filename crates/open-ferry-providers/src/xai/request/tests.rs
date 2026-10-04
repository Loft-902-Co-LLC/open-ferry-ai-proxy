//! The request's body, URL and headers, ported from upstream's
//! `xai_executor_test.go` and `xai_configuration_update_test.go` where they
//! test what is ported. Upstream's tests of the Grok CLI chat proxy, of xAI
//! sign-ins and of made-up sessions are turned round: they check that
//! nothing of the kind is sent.

use bytes::Bytes;
use http::{HeaderMap, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options, Request};
use open_ferry_core::models::{ModelCatalog, ModelInfo, ThinkingSupport};
use serde_json::{Value, json};

use super::*;
use crate::json::exists;

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from(payload.to_owned()),
    }
}

fn options(format: &str) -> Options {
    Options::new(Format::from(format.to_owned()))
}

fn config(yaml: &str) -> Config {
    Config::parse(yaml).expect("config parses")
}

/// [`prepare`] for a Codex body, with the config and models given.
fn prepare_with(
    config: Option<&Config>,
    models: Option<&dyn ModelCatalog>,
    request: &Request,
    options: &Options,
    stream: bool,
) -> Prepared {
    let context = Context {
        auth: None,
        config,
        models,
    };
    prepare(context, request, options, stream, Format::CODEX).expect("prepares")
}

/// The body [`prepare`] makes of an OpenAI Responses `payload` for `model`.
fn body(model: &str, payload: &str) -> Value {
    prepare_with(
        Some(&Config::default()),
        None,
        &request(model, payload),
        &options("openai-response"),
        true,
    )
    .body
}

/// The text of the value at `path`, as gjson's `Raw` gives it.
fn raw(body: &Value, path: &str) -> String {
    get(body, path).map(Value::to_string).unwrap_or_default()
}

/// An API key credential with `attributes`.
fn api_key(attributes: &[(&str, &str)]) -> Auth {
    let mut auth = Auth {
        provider: "xai".into(),
        ..Auth::default()
    };
    for (key, value) in attributes {
        auth.attributes.insert((*key).into(), (*value).into());
    }
    auth
}

/// A model registry that knows one xAI model.
struct Catalog(ModelInfo);

impl ModelCatalog for Catalog {
    fn model_providers(&self, _model: &str) -> Vec<String> {
        vec!["xai".into()]
    }
    fn first_available_model(&self) -> Option<String> {
        None
    }
    fn available_models(&self) -> Vec<ModelInfo> {
        vec![self.0.clone()]
    }
    fn model_info(&self, model: &str, provider: &str) -> Option<ModelInfo> {
        (model == self.0.id && provider == "xai").then(|| self.0.clone())
    }
}

// TestXAIExecutorPrepareResponsesRequestPreservesSupportedOutputControls.
#[test]
fn preserves_supported_output_controls() {
    struct Case {
        source: &'static str,
        payload: &'static str,
        want: &'static [(&'static str, &'static str)],
        absent: &'static [&'static str],
    }
    let cases = [
        // Chat Completions prefers max_completion_tokens.
        Case {
            source: "openai",
            payload: r#"{"model":"grok-4.5","messages":[{"role":"user","content":"hello"}],
                "max_completion_tokens":64,"max_tokens":128,"temperature":0,"top_p":0.25,
                "top_k":7,"stop":["END"]}"#,
            want: &[
                ("max_output_tokens", "64"),
                ("temperature", "0"),
                ("top_p", "0.25"),
                ("top_k", "7"),
            ],
            absent: &["max_completion_tokens", "max_tokens", "stop"],
        },
        // Chat Completions falls back to max_tokens.
        Case {
            source: "openai",
            payload: r#"{"model":"grok-4.5","messages":[{"role":"user","content":"hello"}],
                "max_completion_tokens":null,"max_tokens":128}"#,
            want: &[("max_output_tokens", "128")],
            absent: &[
                "max_completion_tokens",
                "max_tokens",
                "temperature",
                "top_p",
                "top_k",
            ],
        },
        // Responses preserves native controls.
        Case {
            source: "openai-response",
            payload: r#"{"model":"grok-4.5","input":"hello","max_output_tokens":256,
                "temperature":0.4,"top_p":0.8,"top_k":20,"stop":["END"]}"#,
            want: &[
                ("max_output_tokens", "256"),
                ("temperature", "0.4"),
                ("top_p", "0.8"),
                ("top_k", "20"),
            ],
            absent: &["stop"],
        },
        // No controls remain absent.
        Case {
            source: "openai",
            payload: r#"{"model":"grok-4.5","messages":[{"role":"user","content":"hello"}]}"#,
            want: &[],
            absent: &["max_output_tokens", "temperature", "top_p", "top_k", "stop"],
        },
    ];
    for case in cases {
        let options = Options {
            stream: true,
            ..options(case.source)
        };
        let body = prepare_with(
            Some(&Config::default()),
            None,
            &request("grok-4.5", case.payload),
            &options,
            true,
        )
        .body;
        for (path, want) in case.want {
            assert_eq!(raw(&body, path), *want, "{path}: {body}");
        }
        for path in case.absent {
            assert!(!exists(&body, path), "{path}: {body}");
        }
    }
}

// Not upstream's: only a Chat Completions or Responses client's controls
// are kept, and a null one is not.
#[test]
fn output_controls_of_other_sources_are_left_alone() {
    let source = json!({"max_output_tokens": 5, "temperature": 1, "top_k": null});
    let mut body = json!({});
    preserve_output_controls(&mut body, &source, &Format::CLAUDE);
    assert_eq!(body, json!({}));
    preserve_output_controls(&mut body, &source, &Format::OPENAI_RESPONSE);
    assert_eq!(body, json!({"max_output_tokens": 5, "temperature": 1}));
}

// TestXAIExecutorPrepareResponsesRequestDropsPayloadStopOverride.
#[test]
fn drops_payload_stop_override() {
    let config = config(
        "payload:\n  override:\n    - models:\n        - name: grok-4.5\n      params:\n        stop: [END]\n",
    );
    let prepared = prepare_with(
        Some(&config),
        None,
        &request("grok-4.5", r#"{"model":"grok-4.5","input":"hello"}"#),
        &options("openai-response"),
        true,
    );
    assert!(!exists(&prepared.body, "stop"), "{}", prepared.body);
}

// TestXAIExecutorPrepareResponsesRequestRewritesCodexAgentMessage.
#[test]
fn rewrites_codex_agent_message() {
    let config = config("client:\n  codex:\n    optimize-multi-agent-v2: true\n");
    let payload = r#"{
        "model":"grok-4.5",
        "input":[{
            "type":"agent_message",
            "id":"amsg_019f92c3-6d77-7880-a6e4-f920867dc6a0",
            "author":"/root",
            "recipient":"/root/arithmetic_question",
            "content":[
                {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/arithmetic_question\nSender: /root\nPayload:\n"},
                {"type":"encrypted_content","encrypted_content":"Pose one arithmetic question. Reply with the question only."}
            ],
            "internal_chat_message_metadata_passthrough":{"turn_id":"019f92c3-6772-7213-8aac-8bd154d528f1"}
        }]
    }"#;
    let mut options = options("openai-response");
    options.headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static("Codex Desktop/0.146.0-alpha.3.1"),
    );
    let body = prepare_with(
        Some(&config),
        None,
        &request("grok-4.5", payload),
        &options,
        true,
    )
    .body;
    let message = &body["input"][0];
    assert_eq!(message["type"], "message", "{body}");
    assert_eq!(message["role"], "user", "{body}");
    assert_eq!(message["content"][1]["type"], "input_text", "{body}");
    assert_eq!(
        message["content"][1]["text"],
        "Pose one arithmetic question. Reply with the question only."
    );
    assert!(!exists(message, "content.1.encrypted_content"), "{body}");
    assert_eq!(message["id"], "amsg_019f92c3-6d77-7880-a6e4-f920867dc6a0");
    assert_eq!(message["author"], "/root");
    assert_eq!(message["recipient"], "/root/arithmetic_question");
    assert_eq!(
        message["internal_chat_message_metadata_passthrough"]["turn_id"],
        "019f92c3-6772-7213-8aac-8bd154d528f1"
    );
}

// Not upstream's: the model loses its suffix, `stream` is set as the call
// needs, and the fields xAI refuses are dropped.
#[test]
fn sets_model_and_stream_and_drops_refused_fields() {
    let payload = r#"{"model":"grok-4.3","input":"hi","previous_response_id":"resp_1",
        "prompt_cache_retention":"24h","safety_identifier":"s","stream_options":{"include_usage":true}}"#;
    for stream in [false, true] {
        let body = prepare_with(
            Some(&Config::default()),
            None,
            &request("grok-4.3(low)", payload),
            &options("openai-response"),
            stream,
        )
        .body;
        assert_eq!(body["model"], "grok-4.3");
        assert_eq!(body["stream"], stream);
        for field in DROPPED_FIELDS {
            assert!(!exists(&body, field), "{field}: {body}");
        }
    }
}

// TestXAIResponsesPreparationStripsUnsupportedConfigurationUpdates.
#[test]
fn strips_unsupported_configuration_updates() {
    const SOURCE: &str = r#"{"model":"grok-4.5","reasoning":{"effort":"medium","summary":"auto"},"input":[{"role":"user","content":"hi"},{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","tools":[]},{"type":"configuration_update","reasoning":{"effort":"high"}},{"role":"user","content":"again"}]}"#;
    // The latest update, then a suffix that wins over it.
    for (model, effort, stream) in [("grok-4.5", "high", false), ("grok-4.5(low)", "low", true)] {
        let body = prepare_with(
            Some(&Config::default()),
            None,
            &request(model, SOURCE),
            &options("openai-response"),
            stream,
        )
        .body;
        assert_eq!(body["reasoning"]["effort"], effort, "{body}");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2, "{body}");
        assert_eq!(input[0]["content"], "hi");
        assert_eq!(input[1]["content"], "again");
        assert_eq!(body["reasoning"]["summary"], "auto", "{body}");
    }
}

// TestXAIResponsesPreparationHonorsExplicitSupportOnly, with the model in
// the executor's model registry: the model the credential manager resolves
// for an API key isn't ported.
#[test]
fn honors_explicit_configuration_update_support_only() {
    const SOURCE: &str = r#"{"model":"opaque-xai-route","reasoning":{"effort":"medium","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"high"}},{"role":"user","content":"hi"}]}"#;
    let catalog = Catalog(ModelInfo {
        id: "opaque-xai-route".into(),
        model_type: "xai".into(),
        support_configuration_update: true,
        thinking: Some(ThinkingSupport {
            levels: vec!["low".into(), "medium".into(), "high".into()],
            ..ThinkingSupport::default()
        }),
        ..ModelInfo::default()
    });
    for (model, top) in [
        ("opaque-xai-route", "medium"),
        ("opaque-xai-route(low)", "low"),
    ] {
        let body = prepare_with(
            Some(&Config::default()),
            Some(&catalog),
            &request(model, SOURCE),
            &options("openai-response"),
            false,
        )
        .body;
        assert_eq!(body["reasoning"]["effort"], top, "{body}");
        assert_eq!(body["input"][0]["reasoning"]["effort"], "high", "{body}");
    }
}

// TestXAIExecutorOmitsUnsupportedReasoningEffort, on the prepared body.
#[test]
fn omits_unsupported_reasoning_effort() {
    let catalog = Catalog(ModelInfo {
        id: "grok-4".into(),
        model_type: "xai".into(),
        ..ModelInfo::default()
    });
    let body = prepare_with(
        Some(&Config::default()),
        Some(&catalog),
        &request(
            "grok-4",
            r#"{"model":"grok-4","input":"hello","reasoning":{"effort":"high"}}"#,
        ),
        &options("openai-response"),
        true,
    )
    .body;
    assert!(!exists(&body, "reasoning"), "{body}");
}

// TestXAIExecutorThinkingPayloadOverride, for the models the built-in
// catalog has or doesn't: the model the credential manager resolves (the
// home and API key cases) isn't ported.
#[test]
fn thinking_payload_override() {
    const REMOTE: &str = "grok-home-only-thinking-test";
    let cases = [
        // A model the catalog says thinks.
        ("grok-4.5", "", "high"),
        // One it says doesn't.
        ("grok-build-0.1", "", ""),
        // One it doesn't know.
        (REMOTE, "", "high"),
        // A suffix.
        ("grok-4.5", "(low)", "low"),
    ];
    for (model, suffix, effort) in cases {
        for overridden in [false, true] {
            let (config, want) = if overridden {
                // An override may force an effort beyond the model's own.
                let yaml = format!(
                    "payload:\n  override:\n    - models:\n        - name: {model}\n      params:\n        reasoning.effort: xhigh\n"
                );
                (self::config(&yaml), "xhigh")
            } else {
                (Config::default(), effort)
            };
            let payload =
                format!(r#"{{"model":"{model}","input":"hello","reasoning":{{"effort":"high"}}}}"#);
            let body = prepare_with(
                Some(&config),
                None,
                &request(&format!("{model}{suffix}"), &payload),
                &options("openai-response"),
                false,
            )
            .body;
            assert_eq!(
                crate::json::str_at(&body, "reasoning.effort"),
                want,
                "{model}{suffix} overridden={overridden}: {body}"
            );
        }
    }
}

// TestXAIExecutorKeepsReasoningEffortForGrok45 and
// TestXAIExecutorKeepsPayloadOverrideReasoningEffortForGrok45, on the
// prepared body.
#[test]
fn keeps_reasoning_effort_for_grok_45() {
    let body = self::body(
        "grok-4.5",
        r#"{"model":"grok-4.5","input":"hello","reasoning":{"effort":"high"}}"#,
    );
    assert_eq!(body["model"], "grok-4.5");
    assert_eq!(body["reasoning"]["effort"], "high", "{body}");

    let config = config(
        "payload:\n  override:\n    - models:\n        - name: grok-4.5\n      params:\n        reasoning.effort: high\n",
    );
    let body = prepare_with(
        Some(&config),
        None,
        &request("grok-4.5", r#"{"model":"grok-4.5","input":"hello"}"#),
        &options("openai-response"),
        true,
    )
    .body;
    assert_eq!(body["reasoning"]["effort"], "high", "{body}");
}

// TestXAIExecutorAppliesThinkingSuffix, on the prepared body.
#[test]
fn applies_thinking_suffix() {
    let body = self::body("grok-4.3(low)", r#"{"model":"grok-4.3","input":"hello"}"#);
    assert_eq!(body["model"], "grok-4.3");
    assert_eq!(body["reasoning"]["effort"], "low", "{body}");
}

// TestNormalizeXAIImageRefsRewritesImageURLField.
#[test]
fn normalize_image_refs_rewrites_image_url_field() {
    let mut body = json!({
        "image": {"image_url": " https://example.com/a.png "},
        "images": [
            {"image_url": {"url": "https://example.com/b.png"}},
            {"url": "https://example.com/c.png"},
            {"url": " https://example.com/d.png "},
            "https://example.com/e.png"
        ],
        "nested": {"reference_images": [{"url": "", "image_url": "https://example.com/f.png"}]},
        "messages": [{"content": [{"type": "image_url", "image_url": {"url": "https://example.com/g.png"}}]}]
    });
    normalize_image_refs(&mut body);
    assert_eq!(
        body,
        json!({
            "image": {"url": "https://example.com/a.png"},
            "images": [
                {"url": "https://example.com/b.png"},
                {"url": "https://example.com/c.png"},
                {"url": "https://example.com/d.png"},
                "https://example.com/e.png"
            ],
            "nested": {"reference_images": [{"url": "https://example.com/f.png"}]},
            "messages": [{"content": [{"type": "image_url", "image_url": {"url": "https://example.com/g.png"}}]}]
        })
    );
}

// TestNormalizeXAIImageRefsSupportsSpecialJSONKeys: keys gjson paths
// would have to escape are found too.
#[test]
fn normalize_image_refs_supports_special_json_keys() {
    let mut body = json!({
        "a.b": {"image": {"image_url": "https://example.com/dot.png"}},
        "c*d": [{"images": [{"image_url": {"url": "https://example.com/star.png"}}]}],
        "e?f": {"reference_images": [{"image_url": "https://example.com/q.png", "keep": true}]}
    });
    normalize_image_refs(&mut body);
    assert_eq!(
        body["a.b"]["image"],
        json!({"url": "https://example.com/dot.png"})
    );
    assert_eq!(
        body["c*d"][0]["images"][0],
        json!({"url": "https://example.com/star.png"})
    );
    assert_eq!(
        body["e?f"]["reference_images"][0],
        json!({"url": "https://example.com/q.png", "keep": true})
    );
}

// Not upstream's: a reference without a usable URL is left as it is.
#[test]
fn normalize_image_refs_leaves_refs_without_a_url() {
    let original = json!({
        "image": {"image_url": "  "},
        "images": [{"image_url": {"url": 5}}, {"url": "https://example.com/x.png"}, null]
    });
    let mut body = original.clone();
    normalize_image_refs(&mut body);
    assert_eq!(body, original);
}

// TestXAIChatBaseURL and TestXAICompactBaseURL, for API keys: the
// credential's base URL, else xAI's API. The cases of xAI sign-ins and of
// `using_api` are turned round: nothing goes to Grok's CLI chat proxy
// unless the credential names it.
#[test]
fn base_urls() {
    const PROXY: &str = "https://cli-chat-proxy.grok.com/v1";
    type Case = (
        &'static [(&'static str, &'static str)],
        &'static str,
        &'static str,
    );
    let cases: [Case; 9] = [
        (&[], DEFAULT_BASE_URL, DEFAULT_BASE_URL),
        (
            &[("base_url", DEFAULT_BASE_URL)],
            DEFAULT_BASE_URL,
            DEFAULT_BASE_URL,
        ),
        (
            &[("base_url", "  https://gateway.example.com/v1  ")],
            "https://gateway.example.com/v1",
            "https://gateway.example.com/v1",
        ),
        (
            &[("base_url", "https://gateway.example.com/v1/")],
            "https://gateway.example.com/v1",
            "https://gateway.example.com/v1",
        ),
        // A sign-in's or `using_api`'s settings don't send it to the proxy.
        (
            &[("auth_kind", "oauth"), ("base_url", DEFAULT_BASE_URL)],
            DEFAULT_BASE_URL,
            DEFAULT_BASE_URL,
        ),
        (
            &[("using_api", "false")],
            DEFAULT_BASE_URL,
            DEFAULT_BASE_URL,
        ),
        (
            &[
                ("using_api", "false"),
                ("base_url", "https://gateway.example.com/v1"),
            ],
            "https://gateway.example.com/v1",
            "https://gateway.example.com/v1",
        ),
        // The proxy named outright: a compact call goes to xAI's API.
        (&[("base_url", PROXY)], PROXY, DEFAULT_BASE_URL),
        (
            &[("base_url", "https://cli-chat-proxy.grok.com/v1/")],
            PROXY,
            DEFAULT_BASE_URL,
        ),
    ];
    for (attributes, chat, compact) in cases {
        let auth = api_key(attributes);
        assert_eq!(
            endpoint(&auth, false),
            format!("{chat}/responses"),
            "{attributes:?}"
        );
        assert_eq!(
            endpoint(&auth, true),
            format!("{compact}/responses/compact"),
            "{attributes:?}"
        );
    }

    // The metadata's base URL when the attributes have none.
    let mut auth = api_key(&[("base_url", "  ")]);
    auth.metadata
        .insert("base_url".into(), " https://meta.example.com/v1 ".into());
    assert_eq!(
        endpoint(&auth, false),
        "https://meta.example.com/v1/responses"
    );
}

// Not upstream's: the token is the API key alone; a sign-in's access token
// isn't read.
#[test]
fn token_is_the_api_key_only() {
    let mut auth = api_key(&[("api_key", "  xai-key  ")]);
    auth.metadata
        .insert("access_token".into(), "oauth-token".into());
    assert_eq!(token(&auth), "xai-key");
    auth.attributes.remove("api_key");
    assert_eq!(token(&auth), "");
}

// TestApplyXAIHeaders_EmptyAPIKey_OmitsAuthorization.
#[test]
fn empty_api_key_omits_authorization() {
    let auth = api_key(&[
        ("auth_kind", "apikey"),
        ("base_url", "https://custom-xai.example.com"),
        ("header:Custom-Token", "xai-custom"),
    ]);
    let headers = build_headers(&auth, &HeaderMap::new(), false, "session-123").unwrap();
    assert!(headers.get("authorization").is_none(), "{headers:?}");
    assert_eq!(headers["x-grok-conv-id"], "session-123");
    assert_eq!(headers["custom-token"], "xai-custom");
    assert_eq!(headers["accept"], "application/json");
}

// TestApplyXAIChatHeaders, turned round: whatever the credential, no Grok
// CLI identity header nor its user agent is sent; the user agent is
// open-ferry's.
#[test]
fn chat_headers_never_pass_for_the_grok_cli() {
    for attributes in [
        &[("api_key", "xai-token"), ("base_url", DEFAULT_BASE_URL)][..],
        &[
            ("api_key", "xai-token"),
            ("auth_kind", "oauth"),
            ("base_url", DEFAULT_BASE_URL),
        ],
        &[
            ("api_key", "xai-token"),
            ("base_url", "https://cli-chat-proxy.grok.com/v1/"),
            ("using_api", "false"),
        ],
    ] {
        let auth = api_key(attributes);
        let headers = build_headers(&auth, &HeaderMap::new(), true, "conv-1").unwrap();
        assert_eq!(headers["authorization"], "Bearer xai-token");
        assert_eq!(headers["x-grok-conv-id"], "conv-1");
        assert_eq!(headers["accept"], "text/event-stream");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["user-agent"], USER_AGENT);
        for name in [
            "x-xai-token-auth",
            "x-grok-client-version",
            "x-grok-client-identifier",
            "x-authenticateresponse",
            "connection",
            "originator",
            "chatgpt-account-id",
        ] {
            assert!(headers.get(name).is_none(), "{name}: {headers:?}");
        }
        assert!(
            !headers
                .get_all("user-agent")
                .iter()
                .any(|value| value.as_bytes().starts_with(b"xai-grok-workspace")),
            "{headers:?}"
        );
    }
}

// TestApplyXAIChatHeaders' "custom headers override cli chat proxy
// defaults", turned round: a `header:` attribute can't send a Grok CLI
// identity header, nor a conversation other than the client's.
#[test]
fn custom_headers_cant_send_grok_cli_identity() {
    let auth = api_key(&[
        ("api_key", "xai-token"),
        ("base_url", "https://cli-chat-proxy.grok.com/v1"),
        ("using_api", "false"),
        ("header:X-XAI-Token-Auth", "custom-token-auth"),
        ("header:x-grok-client-version", "custom-client-version"),
        (
            "header:x-grok-client-identifier",
            "custom-client-identifier",
        ),
        ("header:X-Grok-Client-Anything", "custom"),
        (
            "header:x-authenticateresponse",
            "custom-authenticate-response",
        ),
        ("header:x-grok-conv-id", "attribute-conversation"),
        ("header:User-Agent", "xai-grok-workspace/1.0.44"),
        ("header:X-Kept", "kept"),
    ]);
    for session in ["", "client-session"] {
        let headers = build_headers(&auth, &HeaderMap::new(), true, session).unwrap();
        for name in [
            "x-xai-token-auth",
            "x-grok-client-version",
            "x-grok-client-identifier",
            "x-grok-client-anything",
            "x-authenticateresponse",
        ] {
            assert!(headers.get(name).is_none(), "{name}: {headers:?}");
        }
        let conversation: Vec<_> = headers.get_all("x-grok-conv-id").iter().collect();
        if session.is_empty() {
            assert!(conversation.is_empty(), "{headers:?}");
        } else {
            assert_eq!(conversation, [session], "{headers:?}");
        }
        let agents: Vec<_> = headers.get_all("user-agent").iter().collect();
        assert_eq!(agents, [USER_AGENT], "{headers:?}");
        assert_eq!(headers["x-kept"], "kept");
    }
}

// Not upstream's: a client's own Grok CLI headers aren't passed on either,
// and a token that isn't a valid header value fails without quoting it.
#[test]
fn client_headers_and_bad_tokens() {
    let auth = api_key(&[("api_key", "xai-token")]);
    let mut client = HeaderMap::new();
    client.insert("x-grok-client-version", HeaderValue::from_static("1.0.44"));
    client.insert("x-xai-token-auth", HeaderValue::from_static("xai-grok-cli"));
    client.insert("x-grok-conv-id", HeaderValue::from_static("client-header"));
    let headers = build_headers(&auth, &client, false, "").unwrap();
    for name in [
        "x-grok-client-version",
        "x-xai-token-auth",
        "x-grok-conv-id",
    ] {
        assert!(headers.get(name).is_none(), "{name}: {headers:?}");
    }

    let auth = api_key(&[("api_key", "bad\ntoken-secret")]);
    let error = build_headers(&auth, &HeaderMap::new(), false, "").unwrap_err();
    assert!(!error.message.contains("token-secret"), "{}", error.message);
}

// TestXAIExecutorComposerSessionIsolation, TestXAIExecutionSessionIDUsesDerivedStableUUID
// and TestXAIExecutorComposerReusesClaudeCodeSession, turned round: the
// session is the client's `prompt_cache_key` or nothing. No UUID is made
// for a `grok-composer-` model, none is derived from the request's
// metadata, a Claude Code session or a WebSocket's execution session.
#[test]
fn session_is_the_clients_prompt_cache_key_only() {
    let cases = [
        (
            "grok-composer-2.5-fast",
            "openai-response",
            r#"{"model":"grok-composer-2.5-fast","input":"hello"}"#,
            "",
        ),
        (
            "grok-build-0.1",
            "openai-response",
            r#"{"model":"grok-build-0.1","input":"hello"}"#,
            "",
        ),
        (
            "grok-composer-2.5-fast",
            "openai-response",
            r#"{"model":"grok-composer-2.5-fast","prompt_cache_key":"client-session","input":"hello"}"#,
            "client-session",
        ),
        (
            "grok-4.3",
            "openai-response",
            r#"{"prompt_cache_key":"   ","input":"hello"}"#,
            "",
        ),
        (
            "grok-4.3",
            "openai-response",
            r#"{"prompt_cache_key":"  padded  ","input":"hello"}"#,
            "padded",
        ),
        (
            "grok-composer-2.5-fast",
            "claude",
            r#"{"model":"grok-composer-2.5-fast","metadata":{"user_id":"{\"session_id\":\"cache-session-1\"}"},"messages":[{"role":"user","content":"hello"}]}"#,
            "",
        ),
        // A Chat Completions client's key too.
        (
            "grok-4.3",
            "openai",
            r#"{"prompt_cache_key":"chat-session","messages":[{"role":"user","content":"hello"}]}"#,
            "chat-session",
        ),
    ];
    let auth = api_key(&[("api_key", "xai-token")]);
    for (model, format, payload, want) in cases {
        let mut options = options(format);
        options.metadata.execution_session_id = Some("conv-xai-1".into());
        options.metadata.idempotency_key = Some("idempotent-1".into());
        let prepared = prepare_with(
            Some(&Config::default()),
            None,
            &request(model, payload),
            &options,
            true,
        );
        assert_eq!(prepared.session_id, want, "{payload}");
        // Without a session, the body keeps what the client sent, if
        // anything; nothing is put in its place.
        let sent: Value = serde_json::from_str(payload).unwrap();
        let sent = crate::json::str_at(&sent, "prompt_cache_key");
        let key = crate::json::str_at(&prepared.body, "prompt_cache_key");
        assert_eq!(
            key,
            if want.is_empty() { &sent } else { want },
            "{}",
            prepared.body
        );
        let headers = build_headers(&auth, &HeaderMap::new(), true, &prepared.session_id).unwrap();
        match want {
            "" => assert!(headers.get("x-grok-conv-id").is_none(), "{headers:?}"),
            want => assert_eq!(headers["x-grok-conv-id"], want),
        }
    }
}

// Not upstream's: the image and video handlers' formats are refused.
#[test]
fn media_requests_are_recognised() {
    for (format, media) in [
        ("openai-image", true),
        ("openai-video", true),
        ("openai-response", false),
        ("openai", false),
    ] {
        assert_eq!(is_media_request(&options(format)), media, "{format}");
    }
    let error = media_refused();
    assert_eq!(error.http_status(), 400);
    assert_eq!(error.message, MEDIA_REFUSED);
}
