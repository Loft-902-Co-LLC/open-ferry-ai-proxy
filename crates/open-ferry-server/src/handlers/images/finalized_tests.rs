//! Final payload formats through real image executors and handler rendering.
use super::*;
use futures_util::future::BoxFuture;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{
    Dispatcher, Options as ExecOptions, ProviderId, Request as ExecRequest,
    Response as ExecResponse, StreamResponse,
};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_providers::{openai_compat::OpenAiCompatExecutor, xai::XaiExecutor};
use std::sync::Mutex;

struct ImageExecutor {
    executors: Vec<Box<dyn ProviderExecutor>>,
    auth: Arc<Auth>,
    formats: Arc<Mutex<Vec<String>>>,
}
impl Dispatcher for ImageExecutor {
    fn execute<'a>(
        &'a self,
        _: &'a [ProviderId],
        request: ExecRequest,
        mut options: ExecOptions,
    ) -> BoxFuture<'a, Result<ExecResponse, ExecError>> {
        Box::pin(async move {
            let callback = options
                .metadata
                .finalized_response_format
                .take()
                .expect("handler callback");
            let formats = self.formats.clone();
            options.metadata.finalized_response_format = Some(Arc::new(move |format| {
                formats.lock().unwrap().push(format.into());
                callback(format);
            }));
            let mut result = None;
            for executor in &self.executors {
                let attempt = executor
                    .execute(self.auth.clone(), request.clone(), options.clone())
                    .await;
                let done = attempt.is_ok();
                result = Some(attempt);
                if done {
                    break;
                }
            }
            result.unwrap()
        })
    }
    fn count_tokens<'a>(
        &'a self,
        p: &'a [ProviderId],
        r: ExecRequest,
        o: ExecOptions,
    ) -> BoxFuture<'a, Result<ExecResponse, ExecError>> {
        self.execute(p, r, o)
    }
    fn execute_stream<'a>(
        &'a self,
        _: &'a [ProviderId],
        _: ExecRequest,
        _: ExecOptions,
    ) -> BoxFuture<'a, Result<StreamResponse, ExecError>> {
        panic!("synthesized image calls execute without a provider stream")
    }
}

fn executor(compat: bool, yaml: &str) -> Box<dyn ProviderExecutor> {
    let mut config = open_ferry_core::config::Config::parse(yaml).unwrap();
    config.proxy_url = "direct".into();
    if compat {
        config
            .openai_compatibility
            .push(open_ferry_core::config::OpenAiCompatibility {
                name: "compat".into(),
                ..Default::default()
            });
        Box::new(OpenAiCompatExecutor::new("compat", Arc::new(config)))
    } else {
        Box::new(XaiExecutor::new("direct").with_config(Arc::new(config)))
    }
}

// Each finalizer fires once per provider attempt, after all rules, for JSON
// generations, JSON edits and multipart edits; rendering uses that last value.
#[tokio::test]
async fn finalized_image_formats_follow_rules_through_real_executors() {
    for compat in [false, true] {
        for kind in 0..3 {
            for mode in 0..8 {
                for phase in 0..if compat { 1 } else { 3 } {
                    let (rule, explicit, want) = match mode {
                        0 => ("default", None, "url"),
                        1 => ("default", Some("b64_json"), "b64_json"),
                        2 => ("override", Some("url"), "b64_json"),
                        3 => ("filter", Some("url"), ""),
                        4 => ("", Some("url"), "url"),
                        5 => ("default-raw", None, "url"),
                        6 => ("override-raw", Some("b64_json"), "url"),
                        _ => ("", None, if compat { "" } else { "b64_json" }),
                    };
                    let params = if rule == "filter" {
                        "params: [response_format]".into()
                    } else {
                        let value = if mode == 2 { "b64_json" } else { "url" };
                        if rule.ends_with("-raw") {
                            format!("params:\n        response_format: '\"{value}\"'")
                        } else {
                            format!("params:\n        response_format: {value}")
                        }
                    };
                    let yaml = if rule.is_empty() {
                        "{}".into()
                    } else {
                        format!(
                            "payload:\n  {rule}:\n    - models:\n        - name: '*'\n      {params}\n"
                        )
                    };
                    let release = Arc::new(tokio::sync::Notify::new());
                    let gate = release.clone();
                    let peer = Router::new().fallback(move || {
                        let gate = gate.clone();
                        async move {
                            if phase == 2 {
                                gate.notified().await;
                            }
                            // A base64-only reply must turn into a data URL when url wins.
                            (
                                [(header::CONTENT_TYPE, "application/json")],
                                r#"{"data":[{"b64_json":"AA=="}]}"#,
                            )
                        }
                    });
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let url = format!("http://{}/v1", listener.local_addr().unwrap());
                    let task =
                        tokio::spawn(async move { axum::serve(listener, peer).await.unwrap() });
                    let model = if compat {
                        "compat-image"
                    } else {
                        "grok-imagine-image"
                    };
                    let mut auth = Auth::default();
                    auth.attributes.insert("base_url".into(), url);
                    auth.attributes
                        .insert("api_key".into(), "image-test-key".into());
                    auth.attributes
                        .insert("compat_name".into(), "compat".into());
                    let formats = Arc::new(Mutex::new(Vec::new()));
                    let dispatcher = Arc::new(ImageExecutor {
                        executors: vec![executor(compat, &yaml)],
                        auth: Arc::new(auth),
                        formats: formats.clone(),
                    });
                    let catalog = FakeCatalog::new()
                        .serve(model, &[if compat { "compat" } else { "xai" }])
                        .info(image_model(model));
                    let mut config = keepalive(1);
                    config.streaming.keepalive = Some(if phase == 2 {
                        Duration::from_millis(1)
                    } else {
                        Duration::from_secs(1)
                    });
                    config.api_keys = vec!["sk-test".into()];
                    let app = router(crate::AppState::new(config, dispatcher, Arc::new(catalog)));
                    let stream = phase > 0;
                    let request = if kind == 2 {
                        let mut fields = vec![
                            ("model", model),
                            ("prompt", "cat"),
                            ("stream", if stream { "true" } else { "false" }),
                        ];
                        if let Some(format) = explicit {
                            fields.push(("response_format", format));
                        }
                        post_form(&fields, &[("image", "a.png", Some("image/png"), PNG)])
                    } else {
                        let mut body =
                            serde_json::json!({"model":model,"prompt":"cat","stream":stream});
                        if kind == 1 {
                            body["image"] = "data:image/png;base64,AA==".into();
                        }
                        if let Some(format) = explicit {
                            body["response_format"] = format.into();
                        }
                        post_json(
                            if kind == 0 { GENERATIONS } else { EDITS },
                            &body.to_string(),
                        )
                    };
                    let (status, text) = if phase == 2 {
                        let response = tokio::time::timeout(
                            Duration::from_secs(5),
                            app.clone().oneshot(request),
                        )
                        .await
                        .unwrap()
                        .unwrap();
                        let status = response.status();
                        let mut body = response.into_body();
                        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
                        assert_eq!(&first[..], b": keep-alive\n\n");
                        release.notify_one();
                        let rest = body.collect().await.unwrap().to_bytes();
                        (
                            status,
                            format!(
                                "{}{}",
                                String::from_utf8_lossy(&first),
                                String::from_utf8_lossy(&rest)
                            ),
                        )
                    } else {
                        let (status, _, text) = send(&app, request).await;
                        (status, text)
                    };
                    assert_eq!(
                        status,
                        StatusCode::OK,
                        "compat={compat} kind={kind} mode={mode} phase={phase}: {text}"
                    );
                    assert_eq!(*formats.lock().unwrap(), vec![want.to_owned()]);
                    if phase == 2 {
                        assert!(text.contains(": keep-alive"), "{text}");
                    }
                    if want == "url" {
                        assert!(text.contains("\"url\":\"data:"), "{text}");
                        assert!(!text.contains("\"b64_json\""), "{text}");
                    } else {
                        assert!(text.contains("\"b64_json\":\"AA==\""), "{text}");
                        assert!(!text.contains("\"url\""), "{text}");
                    }
                    task.abort();
                }
            }
        }
    }
}

#[tokio::test]
async fn finalized_image_retry_renders_the_last_attempt() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = attempts.clone();
    let peer = Router::new().fallback(move || {
        let seen = seen.clone();
        async move {
            let status = if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            };
            (
                status,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"data":[{"b64_json":"AA==","url":"https://image.test/a.png"}]}"#,
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, peer).await.unwrap() });
    let formats = Arc::new(Mutex::new(Vec::new()));
    let mut auth = Auth::default();
    auth.attributes.insert("base_url".into(), url);
    auth.attributes
        .insert("api_key".into(), "image-retry-test".into());
    let executors=["url","b64_json"].map(|format|executor(false,&format!("payload:\n  override:\n    - models:\n        - name: '*'\n      params:\n        response_format: {format}\n"))).into_iter().collect();
    let dispatcher = Arc::new(ImageExecutor {
        executors,
        auth: Arc::new(auth),
        formats: formats.clone(),
    });
    let catalog = FakeCatalog::new().serve("grok-imagine-image", &["xai"]);
    let config = ServerConfig {
        api_keys: vec!["sk-test".into()],
        ..Default::default()
    };
    let app = router(crate::AppState::new(config, dispatcher, Arc::new(catalog)));
    let (status, _, text) = send(
        &app,
        post_json(
            GENERATIONS,
            r#"{"model":"grok-imagine-image","prompt":"cat","response_format":"url"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains("\"b64_json\":\"AA==\""), "{text}");
    assert!(!text.contains("\"url\""), "{text}");
    assert_eq!(*formats.lock().unwrap(), ["url", "b64_json"]);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    task.abort();
}

#[test]
fn finalized_image_callback_clones_and_debug_shows_only_presence() {
    let seen = Arc::new(Mutex::new(String::new()));
    let output = seen.clone();
    let metadata = open_ferry_core::exec::Metadata {
        finalized_response_format: Some(Arc::new(move |format| {
            *output.lock().unwrap() = format.into()
        })),
        ..Default::default()
    };
    let cloned = metadata.clone();
    cloned.finalized_response_format.as_ref().unwrap()("private-format-test");
    assert_eq!(*seen.lock().unwrap(), "private-format-test");
    let debug = format!("{cloned:?}");
    assert!(debug.contains("finalized_response_format: true"));
    assert!(!debug.contains("private-format-test"));
    assert!(
        format!("{:?}", open_ferry_core::exec::Metadata::default())
            .contains("finalized_response_format: false")
    );
}
