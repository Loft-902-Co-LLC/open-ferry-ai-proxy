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

/// [`prepare`] for a Codex body, with the config and models given, and
/// with the config's payload rules applied as a call applies them
/// ([`Prepared::finalize`]).
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
    let mut prepared = prepare(context, request, options, stream, Format::CODEX).expect("prepares");
    prepared.finalize(config, request, options);
    prepared
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

// TestXAIExecutorPrepareResponsesRequestDropsPayloadStopOverride, which,
// like upstream's, checks the prepared body before the rules apply. Then,
// not upstream's: the rules apply last, so a rule's `stop` is sent.
#[test]
fn drops_payload_stop_override() {
    let config = config(
        "payload:\n  override:\n    - models:\n        - name: grok-4.5\n      params:\n        stop: [END]\n",
    );
    let request = request(
        "grok-4.5",
        r#"{"model":"grok-4.5","input":"hello","stop":["X"]}"#,
    );
    let options = options("openai-response");
    let context = Context {
        auth: None,
        config: Some(&config),
        models: None,
    };
    let mut prepared = prepare(context, &request, &options, true, Format::CODEX).expect("prepares");
    assert!(!exists(&prepared.body, "stop"), "{}", prepared.body);
    prepared.finalize(Some(&config), &request, &options);
    assert_eq!(prepared.body["stop"], json!(["END"]), "{}", prepared.body);
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
        // A numeric key, as gjson's `String` gives it: an integer as
        // written, anything else as a plain decimal.
        (
            "grok-4.3",
            "openai-response",
            r#"{"prompt_cache_key":-0,"input":"hello"}"#,
            "-0",
        ),
        (
            "grok-4.3",
            "openai-response",
            r#"{"prompt_cache_key":1E20,"input":"hello"}"#,
            "100000000000000000000",
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

// Not upstream's: the image, video and speech handlers' formats are
// refused.
#[test]
fn media_requests_are_recognised() {
    for (format, media) in [
        ("openai-image", true),
        ("openai-video", true),
        ("openai-speech", true),
        ("openai-response", false),
        ("openai", false),
    ] {
        assert_eq!(is_media_request(&options(format)), media, "{format}");
    }
    let error = media_refused();
    assert_eq!(error.http_status(), 400);
    assert_eq!(error.message, MEDIA_REFUSED);
}

/// A config that adds Grok's X search tool.
fn inject_x_search() -> Config {
    config("xai:\n  inject-x-search: true\n")
}

/// [`prepare`] for an OpenAI Responses `payload` for `model`, not streaming.
fn prepared(config: &Config, model: &str, payload: &str) -> Prepared {
    prepare_with(
        Some(config),
        None,
        &request(model, payload),
        &options("openai-response"),
        false,
    )
}

/// [`prepare`] for a streamed OpenAI Responses `payload` for Grok 4.6.
fn prepared_streaming(payload: &str) -> Prepared {
    prepare_with(
        Some(&Config::default()),
        None,
        &request("grok-4.6", payload),
        &options("openai-response"),
        true,
    )
}

/// The types of the body's tools.
fn tool_types(body: &Value) -> Vec<&str> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool["type"].as_str().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}

/// The tool named `name`.
fn tool_named<'b>(body: &'b Value, name: &str) -> &'b Value {
    body["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == name))
        .unwrap_or_else(|| panic!("{name} missing: {body}"))
}

/// `count` namespaces `mcp__app_<i>` of ten functions `tool_<j>`, each with
/// `extra` in it.
fn namespaces(count: usize, extra: &str) -> Vec<String> {
    (0..count)
        .map(|index| {
            let children: Vec<String> = (0..10)
                .map(|child| {
                    format!(
                        r#"{{"type":"function","name":"tool_{child}",{extra}"parameters":{{"type":"object"}}}}"#
                    )
                })
                .collect();
            format!(
                r#"{{"type":"namespace","name":"mcp__app_{index}","tools":[{}]}}"#,
                children.join(",")
            )
        })
        .collect()
}

// TestXAIExecutorPrepareHonorsInjectXSearchConfig.
#[test]
fn honors_inject_x_search_config() {
    let payload = r#"{"model":"grok-4.5","input":"search the web",
        "tools":[{"type":"function","name":"web_search","parameters":{"type":"object"}}],
        "tool_choice":{"type":"allowed_tools","tools":[{"type":"function","name":"web_search"}]}}"#;
    for (config, inject) in [(Config::default(), false), (inject_x_search(), true)] {
        let prepared = prepared(&config, "grok-4.5", payload);
        let body = &prepared.body;
        let want = usize::from(inject);
        assert_eq!(
            body["tools"].as_array().map(Vec::len),
            Some(1 + want),
            "{body}"
        );
        assert_eq!(body["tools"][0]["name"], "clientfn_web_search", "{body}");
        let x_search = |path: &str| {
            get(body, path)
                .and_then(Value::as_array)
                .map_or(0, |tools| {
                    tools
                        .iter()
                        .filter(|tool| tool["type"] == "x_search")
                        .count()
                })
        };
        assert_eq!(x_search("tools"), want, "{body}");
        assert_eq!(x_search("tool_choice.tools"), want, "{body}");
        assert_eq!(prepared.filter_internal_x_search, inject);
    }
}

// TestXAIExecutorPrepareNormalizesClaudeWebSearchToolChoice and its _Grok46
// variant.
#[test]
fn normalizes_claude_web_search_tool_choice() {
    let cases = [
        (
            "grok-4.5",
            r#"{"model":"grok-4.5","max_tokens":4096,"stream":true,"output_config":{"effort":"high"},
                "thinking":{"type":"disabled"},
                "messages":[{"role":"user","content":[{"type":"text","text":"Perform a web search"}]}],
                "tool_choice":{"type":"tool","name":"web_search"},
                "tools":[{"type":"web_search_20250305","name":"web_search","max_uses":8}]}"#,
        ),
        (
            "grok-4.6",
            r#"{"model":"grok-4.6","max_tokens":64000,"stream":true,"output_config":{"effort":"high"},
                "thinking":{"type":"disabled"},
                "messages":[{"role":"user","content":[{"type":"text","text":"Perform a web search"}]}],
                "tool_choice":{"type":"tool","name":"web_search"},
                "tools":[{"type":"web_search_20250305","name":"web_search","max_uses":8,"allowed_domains":["github.com"]}]}"#,
        ),
    ];
    for (model, payload) in cases {
        let body = prepare_with(
            Some(&Config::default()),
            None,
            &request(model, payload),
            &options("claude"),
            true,
        )
        .body;
        assert_eq!(body["tool_choice"], "required", "{body}");
        assert_eq!(tool_types(&body), ["web_search"], "{body}");
        if model == "grok-4.6" {
            assert_eq!(
                raw(&body, "tools.0.filters.allowed_domains.0"),
                r#""github.com""#
            );
        }
    }
}

// TestXAIExecutorPrepareKeepsNativeImageGenerationForGrok46.
#[test]
fn keeps_native_image_generation_for_grok_46() {
    let body = prepared(
        &Config::default(),
        "grok-4.6",
        r#"{"model":"grok-4.6","input":"draw a red circle",
            "tools":[{"type":"image_generation","action":"generate"}],
            "tool_choice":{"type":"image_generation"}}"#,
    )
    .body;
    assert_eq!(
        body["tools"],
        json!([{"type": "image_generation", "action": "generate"}])
    );
    assert_eq!(body["tool_choice"], "required");
}

// TestXAIExecutorPrepareRewritesImageGenerationAllowedToolsToRequired,
// TestXAIExecutorPrepareForcedImageGenerationDropsOtherToolsAndSkipsXSearchInject,
// TestXAIExecutorPrepareRewritesWebSearchAllowedToolsToRequired,
// TestXAIExecutorPrepareForcedWebSearchDropsOtherToolsAndSkipsXSearchInject
// and TestXAIExecutorPrepareRewritesWebSearchOnlyAllowedToolsAutoToAuto: a
// choice of one hosted tool becomes a string choice with that tool alone,
// and X search isn't added next to it.
#[test]
fn forced_hosted_tool_choices() {
    let cases = [
        (
            Config::default(),
            r#""tools":[{"type":"image_generation","action":"generate"},{"type":"web_search"}],
                "tool_choice":{"type":"allowed_tools","mode":"required","tools":[{"type":"image_generation"}]}"#,
            "required",
            "image_generation",
        ),
        (
            inject_x_search(),
            r#""tools":[{"type":"image_generation","action":"generate"},{"type":"web_search"},{"type":"function","name":"lookup","parameters":{"type":"object"}}],
                "tool_choice":{"type":"image_generation"}"#,
            "required",
            "image_generation",
        ),
        (
            Config::default(),
            r#""tools":[{"type":"web_search"},{"type":"function","name":"lookup","parameters":{"type":"object"}}],
                "tool_choice":{"type":"allowed_tools","mode":"required","tools":[{"type":"web_search"}]}"#,
            "required",
            "web_search",
        ),
        (
            inject_x_search(),
            r#""tools":[{"type":"web_search"},{"type":"function","name":"lookup","parameters":{"type":"object"}}],
                "tool_choice":{"type":"web_search"}"#,
            "required",
            "web_search",
        ),
        (
            Config::default(),
            r#""tools":[{"type":"web_search"},{"type":"function","name":"lookup","parameters":{"type":"object"}}],
                "tool_choice":{"type":"allowed_tools","mode":"auto","tools":[{"type":"web_search"}]}"#,
            "auto",
            "web_search",
        ),
    ];
    for (config, tools, choice, kept) in cases {
        let payload = format!(r#"{{"model":"grok-4.6","input":"go",{tools}}}"#);
        let body = prepared(&config, "grok-4.6", &payload).body;
        assert_eq!(body["tool_choice"], choice, "{body}");
        assert_eq!(tool_types(&body), [kept], "{body}");
    }
}

// TestXAIExecutorPrepareRewritesImageOnlyAllowedToolsAutoToAuto.
#[test]
fn image_only_allowed_tools_auto_stays_auto() {
    let body = prepared(
        &Config::default(),
        "grok-4.6",
        r#"{"model":"grok-4.6","input":"draw a red circle",
            "tools":[{"type":"image_generation"},{"type":"web_search"}],
            "tool_choice":{"type":"allowed_tools","mode":"auto","tools":[{"type":"image_generation"}]}}"#,
    )
    .body;
    assert_eq!(body["tool_choice"], "auto", "{body}");
}

// TestXAIExecutorPrepareStripsImageGenerationFromMixedAllowedTools and
// TestXAIExecutorPrepareStripsWebSearchFromMixedAllowedTools.
#[test]
fn strips_hosted_tools_from_mixed_allowed_tools() {
    for hosted in ["image_generation", "web_search"] {
        let payload = format!(
            r#"{{"model":"grok-4.6","input":"draw or search",
                "tools":[{{"type":"image_generation"}},{{"type":"web_search"}},{{"type":"function","name":"lookup","parameters":{{"type":"object"}}}}],
                "tool_choice":{{"type":"allowed_tools","mode":"required","tools":[{{"type":"{hosted}"}},{{"type":"function","name":"lookup"}}]}}}}"#
        );
        let body = prepared(&Config::default(), "grok-4.6", &payload).body;
        assert_eq!(body["tool_choice"]["type"], "allowed_tools", "{body}");
        assert_eq!(
            body["tool_choice"]["tools"],
            json!([{"type": "function", "name": "lookup"}]),
            "{body}"
        );
    }
}

// TestXAIExecutorPrepareDropsOrphanedToolChoiceBeforeXSearchInject.
#[test]
fn drops_orphaned_tool_choice_before_x_search_inject() {
    let body = prepared(
        &inject_x_search(),
        "grok-4.5",
        r#"{"model":"grok-4.5","input":"draw something",
            "tools":[{"type":"image_generation"}],"tool_choice":{"type":"image_generation"}}"#,
    )
    .body;
    assert_eq!(tool_types(&body), ["x_search"], "{body}");
    assert!(!exists(&body, "tool_choice"), "{body}");
}

// TestXAIExecutorPrepareResponsesRequestAddsObjectTypeToRootUnionBranches.
#[test]
fn adds_object_type_to_root_union_branches() {
    let parameters = r#"{"type":"object","additionalProperties":false,"required":["imagePath","point"],
        "oneOf":[{"required":["radius"],"not":{"required":["size"]}},{"required":["size"],"not":{"required":["radius"]}}],
        "properties":{"imagePath":{"type":"string"},"point":{"type":"array"},"radius":{"type":"number"},"size":{"type":"object"}}}"#;
    let cases = [
        (
            "openai-response",
            format!(
                r#"{{"model":"grok-4.5","input":"crop a region","tools":[{{"type":"function","name":"crop_around_point","parameters":{parameters}}}]}}"#
            ),
        ),
        (
            "openai",
            format!(
                r#"{{"model":"grok-4.5","messages":[{{"role":"user","content":"crop a region"}}],"tools":[{{"type":"function","function":{{"name":"crop_around_point","parameters":{parameters}}}}}]}}"#
            ),
        ),
    ];
    for (format, payload) in cases {
        let body = prepare_with(
            Some(&Config::default()),
            None,
            &request("grok-4.5", &payload),
            &options(format),
            true,
        )
        .body;
        let tool = tool_named(&body, "crop_around_point");
        assert_eq!(tool["type"], "function", "{format}");
        let parameters = &tool["parameters"];
        let branches = parameters["oneOf"].as_array().expect("branches");
        assert_eq!(branches.len(), 2, "{format}");
        for branch in branches {
            assert_eq!(branch["type"], "object", "{format}: {parameters}");
            assert!(exists(branch, "not.required"), "{format}: {parameters}");
        }
        for property in ["imagePath", "point", "radius", "size"] {
            assert!(
                exists(parameters, &format!("properties.{property}")),
                "{format}"
            );
        }
        assert_eq!(parameters["additionalProperties"], false, "{format}");
    }
}

// TestXAIExecutorPrepareAllowedToolsSyncsInjectedXSearch.
#[test]
fn allowed_tools_sync_injected_x_search() {
    let body = prepared(
        &inject_x_search(),
        "grok-4.5",
        r#"{"model":"grok-4.5","input":"search X",
            "tools":[{"type":"image_generation"},{"type":"function","name":"lookup","parameters":{"type":"object"}}],
            "tool_choice":{"type":"allowed_tools","tools":[{"type":"image_generation"},{"type":"function","name":"lookup"}]}}"#,
    )
    .body;
    assert_eq!(tool_types(&body), ["function", "x_search"], "{body}");
    assert_eq!(body["tools"][0]["name"], "lookup");
    assert_eq!(
        body["tool_choice"]["tools"],
        json!([{"type": "function", "name": "lookup"}, {"type": "x_search"}]),
        "{body}"
    );
}

// TestXAIExecutorPrepareResponsesRequest_SimplifiesMCPCodexAppAutomationUpdate.
#[test]
fn simplifies_mcp_codex_app_automation_update() {
    let parameters = r##"{"type":"object","properties":{},"oneOf":[{"$ref":"#/$defs/__schema0"},{"$ref":"#/$defs/__schema3"}],"$defs":{"__schema0":{"type":"object","properties":{"id":{"type":"string"},"mode":{"type":"string","enum":["view"]}},"required":["mode","id"],"additionalProperties":false},"__schema3":{"oneOf":[{"$ref":"#/$defs/__schema4"}]},"__schema4":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}}}"##;
    let body = prepared_streaming(&format!(
        r#"{{"model":"grok-4.6","input":[{{"type":"message","role":"user","content":"help"}}],
            "tools":[{{"type":"namespace","name":"mcp__codex_app","description":"Tools provided by the Codex app.",
                "tools":[{{"type":"function","name":"automation_update","description":"recurring automations","strict":false,"parameters":{parameters}}}]}}]}}"#
    ))
    .body;
    let tool = tool_named(&body, "mcp__codex_app__automation_update");
    assert_eq!(tool["parameters"]["type"], "object");
    assert!(tool["parameters"].get("oneOf").is_none(), "{tool}");
    assert_eq!(tool["parameters"]["additionalProperties"], true);
}

// TestXAIExecutorPrepareResponsesRequestPreservesCodexNumberToolSchemas.
#[test]
fn preserves_codex_number_tool_schemas() {
    let mut options = options("codex");
    options.headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static("codex_cli_rs/0.1"),
    );
    let body = prepare_with(
        Some(&Config::default()),
        None,
        &request(
            "grok-4",
            r#"{"tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"yield_time_ms":{"type":"number"}}}}],
                "input":[{"type":"additional_tools","tools":[{"type":"function","name":"functions__exec_command","parameters":{"type":"object","properties":{"yield_time_ms":{"type":"number"}}}}]}]}"#,
        ),
        &options,
        false,
    )
    .body;
    for path in [
        "tools.0.parameters.properties.yield_time_ms.type",
        "tools.1.parameters.properties.yield_time_ms.type",
    ] {
        assert_eq!(raw(&body, path), r#""integer""#, "{path}: {body}");
    }
}

// TestPrepareResponsesRequest_CapsAt200WithInjectXSearch: 200 flattened
// tools and X search make 201, so the namespaces fold.
#[test]
fn caps_at_200_with_inject_x_search() {
    let payload = format!(
        r#"{{"model":"grok-4.6","tools":[{}],"input":[{{"role":"user","content":"hi"}}]}}"#,
        namespaces(20, "").join(",")
    );
    let config = inject_x_search();
    let context = Context {
        auth: None,
        config: Some(&config),
        models: None,
    };
    let prepared = prepare(
        context,
        &request("grok-4.6", &payload),
        &options("openai-response"),
        false,
        Format::OPENAI_RESPONSE,
    )
    .expect("prepares");
    let types = tool_types(&prepared.body);
    assert_eq!(types.len(), 21, "{}", prepared.body);
    assert_eq!(types.iter().filter(|kind| **kind == "x_search").count(), 1);
    assert!(prepared.namespace_tools["mcp__app_0"].is_dispatcher);
}

// TestXAIExecutorAliasesClientWebSearchFunctionInRequest.
#[test]
fn aliases_client_web_search_function_in_request() {
    let prepared = prepared_streaming(
        r#"{"model":"grok-4.6",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"search ranking"}]},
                {"type":"function_call","name":"web_search","call_id":"call_1","arguments":"{\"query\":\"ranking\"}"}
            ],
            "tools":[
                {"type":"function","name":"web_search","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}},
                {"type":"function","name":"read","parameters":{"type":"object"}}
            ],
            "tool_choice":{"type":"function","name":"web_search"}}"#,
    );
    let body = &prepared.body;
    assert_eq!(prepared.web_search_alias, "clientfn_web_search");
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["tools"][0]["name"], "clientfn_web_search");
    assert_eq!(body["tools"][1]["name"], "read");
    assert_eq!(body["tool_choice"]["name"], "clientfn_web_search", "{body}");
    assert_eq!(body["input"][1]["name"], "clientfn_web_search", "{body}");

    let allowed = prepared_streaming(
        r#"{"model":"grok-4.6","tools":[{"type":"function","name":"web_search","parameters":{"type":"object"}}],
            "tool_choice":{"type":"allowed_tools","tools":[{"type":"function","name":"web_search"}]}}"#,
    )
    .body;
    assert_eq!(
        allowed["tool_choice"]["tools"][0]["name"], "clientfn_web_search",
        "{allowed}"
    );
}

// TestXAIExecutorPreservesHostedWebSearchToolType.
#[test]
fn preserves_hosted_web_search_tool_type() {
    let prepared = prepared_streaming(
        r#"{"model":"grok-4.6","input":"search ranking","tools":[{"type":"web_search"}]}"#,
    );
    assert_eq!(prepared.body["tools"], json!([{"type": "web_search"}]));
    assert_eq!(prepared.web_search_alias, "");
}

// TestXAIExecutorAliasesClientWebSearchWithExistingAliasCollision, the
// request half; restoring the name is in the response's tests.
#[test]
fn aliases_client_web_search_past_an_existing_alias() {
    let prepared = prepared_streaming(
        r#"{"model":"grok-4.6",
            "tools":[
                {"type":"function","name":"clientfn_web_search","parameters":{"type":"object"}},
                {"type":"function","name":"web_search","parameters":{"type":"object"}}
            ],
            "input":[
                {"type":"function_call","name":"clientfn_web_search","call_id":"call_1","arguments":"{}"},
                {"type":"function_call","name":"web_search","call_id":"call_2","arguments":"{}"}
            ]}"#,
    );
    let body = &prepared.body;
    assert_eq!(prepared.web_search_alias, "clientfn_web_search_1");
    assert_eq!(body["tools"][0]["name"], "clientfn_web_search");
    assert_eq!(body["tools"][1]["name"], "clientfn_web_search_1");
    assert_eq!(body["input"][0]["name"], "clientfn_web_search");
    assert_eq!(body["input"][1]["name"], "clientfn_web_search_1");
}

// TestXAIExecutorDoesNotAliasNamespacedWebSearchToolChoice.
#[test]
fn does_not_alias_namespaced_web_search_tool_choice() {
    let body = prepared_streaming(
        r#"{"model":"grok-4.6",
            "tools":[
                {"type":"function","name":"web_search","parameters":{"type":"object"}},
                {"type":"namespace","name":"acme","tools":[{"type":"function","name":"web_search","parameters":{"type":"object"}}]}
            ],
            "tool_choice":{"type":"allowed_tools","tools":[
                {"type":"function","name":"web_search","namespace":"acme"},
                {"type":"function","name":"web_search"}
            ]}}"#,
    )
    .body;
    assert_eq!(
        body["tool_choice"]["tools"],
        json!([
            {"type": "function", "name": "acme__web_search"},
            {"type": "function", "name": "clientfn_web_search"}
        ]),
        "{body}"
    );
}

// TestXAIExecutorAliasesClientWebSearchBeyond100Collisions.
#[test]
fn aliases_client_web_search_beyond_100_collisions() {
    let mut tools = vec![
        r#"{"type":"function","name":"web_search","parameters":{"type":"object"}}"#.to_owned(),
        r#"{"type":"function","name":"clientfn_web_search","parameters":{"type":"object"}}"#
            .to_owned(),
    ];
    tools.extend((1..=100).map(|index| {
        format!(
            r#"{{"type":"function","name":"clientfn_web_search_{index}","parameters":{{"type":"object"}}}}"#
        )
    }));
    let prepared = prepared_streaming(&format!(
        r#"{{"model":"grok-4.6","tools":[{}]}}"#,
        tools.join(",")
    ));
    assert_eq!(prepared.web_search_alias, "clientfn_web_search_101");
}

// TestXAIExecutorFoldsNamespaceNamedWebSearchWithoutAliasing, the request
// half (upstream checks the body a mock server got; the restored call is in
// the response's tests): a folded namespace named web_search keeps its
// dispatcher's name.
#[test]
fn folds_namespace_named_web_search_without_aliasing() {
    let mut list = namespaces(46, r#""description":"child tool","#);
    list.push(
        r#"{"type":"namespace","name":"web_search","tools":[{"type":"function","name":"query_web","parameters":{"type":"object"}}]}"#
            .to_owned(),
    );
    let prepared = prepared_streaming(&format!(
        r#"{{"model":"grok-4.6","tools":[{}],"input":[{{"role":"user","content":"search query"}}]}}"#,
        list.join(",")
    ));
    let names: Vec<&Value> = prepared.body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| &tool["name"])
        .collect();
    assert!(names.contains(&&json!("web_search")), "{}", prepared.body);
    assert!(!names.contains(&&json!("clientfn_web_search")));
    assert_eq!(prepared.web_search_alias, "");
    assert!(prepared.namespace_tools["web_search"].is_dispatcher);
}

// TestXAIExecutorExecuteNormalizesCustomToolCallHistory, adapted: the
// prepared body is checked rather than the body a mock server got. Past
// custom calls become function calls; ones without a call ID go.
#[test]
fn normalizes_custom_tool_call_history() {
    let body = prepared(
        &Config::default(),
        "grok-4.5",
        r#"{"model":"grok-4.5",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"search"}]},
                {"type":"custom_tool_call","name":"missing_call_id","input":"invalid"},
                {"type":"custom_tool_call_output","output":"missing call id"},
                {"type":"custom_tool_call","status":"completed","call_id":"xs_call-1","name":"x_semantic_search","input":"{\"query\":\"US stocks\",\"limit\":\"10\"}","internal_chat_message_metadata_passthrough":{"turn_id":"turn-1"}},
                {"type":"custom_tool_call_output","call_id":"xs_call-1","output":"unsupported custom tool call: x_semantic_search","internal_chat_message_metadata_passthrough":{"turn_id":"turn-1"}},
                {"type":"custom_tool_call","call_id":"call-2","name":"apply_patch","input":"*** Begin Patch"},
                {"type":"custom_tool_call_output","call_id":"call-2","output":[{"type":"input_text","text":"done"}]}
            ],
            "tools":[{"type":"x_search"}],
            "tool_choice":"auto"}"#,
    )
    .body;
    let input = body["input"].as_array().expect("input");
    assert_eq!(input.len(), 5, "{body}");
    assert!(
        input.iter().all(|item| !item["type"]
            .as_str()
            .unwrap_or_default()
            .starts_with("custom_tool_call")),
        "{body}"
    );
    let arguments = |item: &Value| -> Value {
        serde_json::from_str(item["arguments"].as_str().expect("arguments")).expect("JSON")
    };
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(arguments(&input[1])["query"], "US stocks");
    assert!(input[1].get("input").is_none());
    assert!(
        input[1]
            .get("internal_chat_message_metadata_passthrough")
            .is_none()
    );
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(
        input[2]["output"],
        "unsupported custom tool call: x_semantic_search"
    );
    assert_eq!(arguments(&input[3])["input"], "*** Begin Patch");
    // gjson's String() of the output, which the apply_patch bridge leaves an
    // array, as upstream's does.
    assert_eq!(
        crate::json::str_of(Some(&input[4]["output"])),
        r#"[{"type":"input_text","text":"done"}]"#
    );
    assert_eq!(body["tools"][0]["type"], "x_search");
}

// TestXAIExecutorReMergesReasoningAfterDroppingInvalidEncryptedContent,
// checked on the prepared body rather than the one a mock server got:
// another provider's encrypted_content goes, and the reasoning item left
// with only its summary joins the one before.
#[test]
fn re_merges_reasoning_after_dropping_invalid_encrypted_content() {
    let body = prepared(
        &Config::default(),
        "grok-4.3",
        r#"{"model":"grok-4.3","input":[
            {"type":"reasoning","summary":[{"type":"summary_text","text":"first"}]},
            {"type":"reasoning","summary":[{"type":"summary_text","text":"second"}],"encrypted_content":"gAAAAABforeign-codex-replay"},
            {"role":"user","content":"hi"}
        ]}"#,
    )
    .body;
    let input = body["input"].as_array().expect("input");
    assert_eq!(input[0]["summary"][0]["text"], "first", "{body}");
    assert_eq!(input[0]["summary"][1]["text"], "second", "{body}");
    assert_eq!(input[1]["role"], "user", "{body}");
    assert_eq!(input.len(), 2, "{body}");
}

// TestXAIExecutorDropsInvalidCompactionItem, checked on the prepared body
// rather than the one a mock server got.
#[test]
fn drops_invalid_compaction_item() {
    let body = prepared(
        &Config::default(),
        "grok-4.3",
        r#"{"model":"grok-4.3","input":[{"type":"compaction","encrypted_content":"gAAAAABforeign-codex-replay"},{"role":"user","content":"hi"}]}"#,
    )
    .body;
    let input = body["input"].as_array().expect("input");
    assert!(
        input.iter().all(|item| item["type"] != "compaction"),
        "{body}"
    );
    assert_eq!(input[0]["role"], "user", "{body}");
    assert_eq!(input.len(), 1, "{body}");
}
