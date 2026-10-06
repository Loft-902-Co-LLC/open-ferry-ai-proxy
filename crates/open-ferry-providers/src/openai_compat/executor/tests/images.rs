// Ported from CLIProxyAPI
// internal/runtime/executor/openai_compat_executor_images_test.go
// (TestOpenAICompatExecutor_ImageStreamChunkBoundaryObservability,
// TestOpenAICompatExecutor_ImageEndpointPath_HonorsOverriddenRequestPath_Issue6196)
// and openai_compat_executor_compact_test.go
// (TestOpenAICompatExecutorImagesGenerationsPassthrough,
// TestOpenAICompatExecutorImagesGenerationsStreamsUpstream,
// TestOpenAICompatExecutorImagesEditsMultipartRewritesModel) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The executor's calls from the OpenAI Images endpoints, with checks of
//! their headers, errors and payload rules, and of which calls they are.
//!
//! Changed: the chunk boundary test gives no requested model alias, which
//! upstream's record takes the place of the model from; the record's
//! served model is checked all the same.

use http::Method;
use open_ferry_core::multipart::{Header, Writer, file_content_disposition};
use open_ferry_core::observe::usage;
use open_ferry_core::observe::{CallReport, Observation, RequestContext};

use super::*;
use crate::images::{multipart_boundary, read_form};

/// Options for a call from the Images endpoint `path`, sent as
/// `content_type` unless it is empty.
fn image_options(path: &str, content_type: &str, stream: bool) -> Options {
    let mut options = Options {
        stream,
        ..options(&Format::OPENAI_IMAGE)
    };
    options.metadata.request_path = path.into();
    if !content_type.is_empty() {
        options.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(content_type).unwrap(),
        );
    }
    options
}

/// A credential with a base URL, an API key and the `header:` attributes
/// of `extra`.
fn image_auth(base_url: &str, key: &str, extra: &[(&str, &str)]) -> Arc<Auth> {
    let mut auth = Auth::default();
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), key.into());
    for (name, value) in extra {
        auth.attributes
            .insert(format!("header:{name}"), (*value).to_owned());
    }
    Arc::new(auth)
}

/// A usage queue of the test's own, on.
fn usage_queue() -> usage::Usage {
    let mut config = Config::default();
    config.usage_statistics_enabled = true;
    let queue = usage::Usage::new(&config);
    usage::reconfigure(&queue, None, &config, true);
    queue
}

/// Has the call of `request` with `options` report to `queue`, as a
/// server handler's taps do.
fn observe(queue: &usage::Usage, request: &Request, options: &mut Options) {
    let context = Arc::new(RequestContext::new(
        Method::POST,
        "/v1/images/generations".to_owned(),
    ));
    let tap = queue
        .tap(&context, request, options)
        .expect("the usage tap");
    options.observation = Some(Arc::new(Observation::new(context, vec![tap])));
}

/// The records queued so far, taken.
fn records(queue: &usage::Usage) -> Vec<Value> {
    queue
        .pop_oldest(usize::MAX)
        .iter()
        .map(|record| serde_json::from_slice(record).expect("a JSON record"))
        .collect()
}

// TestOpenAICompatExecutor_ImageStreamChunkBoundaryObservability: the
// stream is passed on as it came, however the provider's writes split it,
// and the usage record names the model the answer names.
#[tokio::test]
async fn image_stream_chunk_boundary_observability() {
    let cases: [(&str, &[&str]); 3] = [
        (
            "chunk split across json boundaries",
            &[
                r#"data: {"id":"img_split","cre"#,
                r#"ated":123,"mo"#,
                "del\":\"dall-e-3\",\"data\":[{\"url\":\"http://example.com/1.png\"}]}\n\n",
            ],
        ),
        (
            "multiple events in single network chunk",
            &[
                "event: ping\ndata: {}\n\nevent: completion\ndata: {\"model\":\"dall-e-3\",\"status\":\"done\"}\n\n",
            ],
        ),
        (
            "chunk starting with event: prefix",
            &[
                "event: image_event\n",
                "data: {\"model\":\"dall-e-3\",\"data\":[]}\n\n",
            ],
        ),
    ];
    for (name, chunks) in cases {
        let mock = Mock::start(Reply::chunked(chunks)).await;
        let executor = executor(vec![entry("compat", false)]);
        let mut auth = compat_auth(&mock.url, "compat");
        auth.attributes.insert("api_key".into(), "test-key".into());
        let queue = usage_queue();
        let request = request(
            "dall-e-3",
            r#"{"model":"dall-e-3","prompt":"a sunset over mountains"}"#,
        );
        let mut options = Options {
            stream: true,
            ..options(&Format::OPENAI_IMAGE)
        };
        observe(&queue, &request, &mut options);
        let report = CallReport::start(&options);
        let result = executor
            .execute_stream(Arc::new(auth), request, options)
            .await;
        let (out, error) = collect(report.stream(result).unwrap()).await;
        assert!(error.is_none(), "{name}: {error:?}");
        assert_eq!(out.concat(), chunks.concat(), "{name}");
        assert_eq!(mock.last().path, "/images/generations", "{name}");
        let records = records(&queue);
        assert_eq!(records.len(), 1, "{name}: {records:?}");
        assert_eq!(records[0]["response_model"], "dall-e-3", "{name}");
    }
}

// TestOpenAICompatExecutor_ImageEndpointPath_HonorsOverriddenRequestPath_Issue6196:
// the call goes where the path the client called says, edits or
// generations.
#[tokio::test]
async fn image_endpoint_path_honors_overridden_request_path() {
    let mock = Mock::start(Reply::json(
        r#"{"created":123,"data":[{"b64_json":"img"}]}"#,
    ))
    .await;
    let executor = executor(vec![entry("compat", false)]);
    let auth = Arc::new(compat_auth(&mock.url, "compat"));
    let payload = r#"{"model":"gpt-image-2.5","prompt":"a red apple"}"#;
    executor
        .execute(
            auth.clone(),
            request("gpt-image-2.5", payload),
            image_options("/v1/images/edits", "", false),
        )
        .await
        .unwrap();
    assert_eq!(mock.last().path, "/images/edits");

    let response = executor
        .execute(
            auth,
            request("gpt-image-2.5", payload),
            image_options("/v1/images/generations", "", false),
        )
        .await
        .unwrap();
    assert!(!response.payload.is_empty());
    assert_eq!(mock.last().path, "/images/generations");
}

// TestOpenAICompatExecutorImagesGenerationsPassthrough: a generation goes
// to `<base_url>/images/generations` with the call's model and no
// `prompt_cache_key`, and its answer comes back as it came.
#[tokio::test]
async fn images_generations_passthrough() {
    let answer = r#"{"created":123,"data":[{"b64_json":"AA=="}],"usage":{"total_tokens":1}}"#;
    let mock = Mock::start(Reply::json(answer)).await;
    let executor = executor(vec![entry("compat", true)]);
    let auth = Arc::new(compat_auth(&mock.base_url(), "compat"));
    let response = executor
        .execute(
            auth,
            request(
                "upstream-image",
                r#"{"model":"compat-image","prompt":"draw"}"#,
            ),
            image_options("/v1/images/generations", "application/json", false),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.path, "/v1/images/generations");
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.json()["model"], "upstream-image", "{}", seen.body);
    assert!(!exists(&seen.json(), "prompt_cache_key"), "{}", seen.body);
    assert_eq!(&response.payload[..], answer.as_bytes());
}

// TestOpenAICompatExecutorImagesGenerationsStreamsUpstream: a stream asks
// for one, with `"stream": true`, and is passed on.
#[tokio::test]
async fn images_generations_streams_upstream() {
    let mock = Mock::start(Reply::chunked(&[
        "event: image_generation.partial\ndata: {\"type\":\"image_generation.partial\"}\n\n",
        "data: [DONE]\n\n",
    ]))
    .await;
    let response = executor(Vec::new())
        .execute_stream(
            plain_auth(&mock.base_url()),
            request(
                "upstream-image",
                r#"{"model":"compat-image","prompt":"draw","stream":true}"#,
            ),
            image_options("/v1/images/generations", "application/json", true),
        )
        .await
        .unwrap();
    let (out, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    let streamed = out.concat();
    let seen = mock.last();
    assert_eq!(seen.path, "/v1/images/generations");
    assert_eq!(seen.header("accept"), Some("text/event-stream"));
    assert_eq!(seen.json()["model"], "upstream-image", "{}", seen.body);
    assert_eq!(seen.json()["stream"], true, "{}", seen.body);
    assert!(
        streamed.contains("event: image_generation.partial") && streamed.contains("data: [DONE]"),
        "{streamed}"
    );
}

// TestOpenAICompatExecutorImagesEditsMultipartRewritesModel: a form edit
// goes as a form with the call's model, its other fields and its file
// with the file's type.
#[tokio::test]
async fn images_edits_multipart_rewrites_model() {
    let mut writer = Writer::new();
    writer.write_field("model", b"compat-image");
    writer.write_field("prompt", b"edit");
    let mut part = Header::new();
    part.set(
        "Content-Disposition",
        file_content_disposition("image", "image.png"),
    );
    part.set("Content-Type", "image/png");
    writer.write_part(&part, b"png-data");
    let content_type = writer.form_data_content_type();
    let body = Bytes::from(writer.finish());

    let mock = Mock::start(Reply::json(
        r#"{"created":123,"data":[{"b64_json":"AA=="}]}"#,
    ))
    .await;
    executor(Vec::new())
        .execute(
            plain_auth(&mock.base_url()),
            Request {
                model: "upstream-image".into(),
                payload: body,
            },
            image_options("/v1/images/edits", &content_type, false),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.path, "/v1/images/edits");
    let boundary = multipart_boundary(seen.header("content-type").unwrap()).unwrap();
    let form = read_form(&Bytes::from(seen.body.clone()), &boundary).unwrap();
    assert_eq!(&form.value("model").unwrap()[..], b"upstream-image");
    assert_eq!(&form.value("prompt").unwrap()[..], b"edit");
    let image = &form.files_of("image")[0];
    assert_eq!(&image.data[..], b"png-data");
    assert_eq!(image.header.get_str("Content-Type"), "image/png");
}

// Not upstream's: only a call from the Images endpoints, named exactly,
// takes this path, and the path the client called decides where it goes.
#[test]
fn endpoints() {
    let at = |format: &Format, path: &str| {
        let mut options = options(format);
        options.metadata.request_path = path.into();
        super::super::images::endpoint(&options)
    };
    let image = Format::OPENAI_IMAGE;
    assert_eq!(at(&image, ""), Some("/images/generations"));
    assert_eq!(at(&image, "/v1/images/edits"), Some("/images/edits"));
    assert_eq!(at(&image, " /v1/images/edits \n"), Some("/images/edits"));
    assert_eq!(at(&image, "/v1/images/edits/"), Some("/images/generations"));
    assert_eq!(
        at(&image, "/v1/images/variations"),
        Some("/images/generations")
    );
    assert_eq!(at(&Format::OPENAI, "/v1/images/edits"), None);
    assert_eq!(at(&Format::new("OpenAI-Image"), "/v1/images/edits"), None);
    assert_eq!(at(&Format::new("openai-video"), "/v1/images/edits"), None);
}

// Not upstream's: an image call sends its body's content type, the API key
// and the client's `User-Agent` or open-ferry's; a stream asks for one,
// and the credential's custom headers come last, over those. Neither has
// the model's suffix, and only a stream has `stream`.
#[tokio::test]
async fn image_request_headers() {
    let mock = Mock::start(Reply::sse("data: {}\n\n")).await;
    let executor = executor(Vec::new());
    let payload = r#"{"prompt":"draw","stream":false}"#;
    let auth = image_auth(
        &mock.url,
        "test",
        &[("Accept", "text/plain"), ("X-Extra", "yes")],
    );
    let response = executor
        .execute_stream(
            auth,
            request("m(high)", payload),
            image_options("", "application/json; charset=utf-8", true),
        )
        .await
        .unwrap();
    collect(response).await;
    let seen = mock.last();
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("authorization"), Some("Bearer test"));
    assert_eq!(seen.header("user-agent"), Some(USER_AGENT));
    assert_eq!(seen.header("accept"), Some("text/plain"));
    assert_eq!(seen.header("cache-control"), Some("no-cache"));
    assert_eq!(seen.header("x-extra"), Some("yes"));
    assert_eq!(seen.body, r#"{"prompt":"draw","stream":true,"model":"m"}"#);

    let mut options = image_options("/v1/images/generations", "", false);
    options.headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("my-client/1.0"),
    );
    executor
        .execute(
            image_auth(&mock.url, "", &[]),
            request("m(high)", payload),
            options,
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("authorization"), None, "no API key, no token");
    assert_eq!(seen.header("user-agent"), Some("my-client/1.0"));
    assert_eq!(seen.header("cache-control"), None);
    assert_eq!(seen.body, r#"{"prompt":"draw","model":"m"}"#);
}

// Not upstream's: a body neither JSON nor a form goes as it came, with the
// client's content type; one with no content type goes as JSON.
#[tokio::test]
async fn other_bodies_go_as_they_came() {
    let mock = Mock::start(Reply::json("{}")).await;
    let executor = executor(Vec::new());
    for (content_type, sent) in [("text/plain", "text/plain"), ("", "application/json")] {
        executor
            .execute(
                image_auth(&mock.url, "test", &[]),
                request("m", "not json"),
                image_options("", content_type, false),
            )
            .await
            .unwrap();
        let seen = mock.last();
        assert_eq!(seen.header("content-type"), Some(sent));
        assert_eq!(seen.body, "not json");
    }
    let error = executor
        .execute(
            image_auth(&mock.url, "test", &[]),
            request("m", "not json"),
            image_options("", "multipart/form-data", false),
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, "multipart boundary is missing");
    assert_eq!(mock.requests().len(), 2, "nothing more is sent");
}

// Not upstream's: the config's payload rules apply to an image body, for
// the model without its suffix.
#[tokio::test]
async fn payload_rules_apply() {
    let mock = Mock::start(Reply::json("{}")).await;
    let mut config = Config::parse(
        "payload:\n  override:\n    - models:\n        - name: m\n      params:\n        size: 1024x1024\n",
    )
    .unwrap();
    config.proxy_url = "direct".into();
    let executor = OpenAiCompatExecutor::new("openai-compatibility", Arc::new(config));
    executor
        .execute(
            image_auth(&mock.url, "test", &[]),
            request("m(high)", r#"{"prompt":"draw","size":"256x256"}"#),
            image_options("/v1/images/generations", "", false),
        )
        .await
        .unwrap();
    assert_eq!(
        mock.last().body,
        r#"{"prompt":"draw","size":"1024x1024","model":"m"}"#
    );
}

// Not upstream's: an error status fails the call with the body, its key
// redacted. A 429 that doesn't stream waits as `Retry-After` says; a
// stream's names no wait. A successful answer has the key redacted too,
// and a credential without a base URL fails before anything is sent.
#[tokio::test]
async fn image_errors() {
    let key = "sk-compat-secret";
    let body = r#"{"error":{"message":"slow down, sk-compat-secret"}}"#;
    let redacted = r#"{"error":{"message":"slow down, [redacted]"}}"#;
    let mock = Mock::start(Reply::error(429, body).with_header("retry-after", "7")).await;
    let executor = executor(Vec::new());
    let auth = image_auth(&mock.url, key, &[]);
    let payload = r#"{"prompt":"draw"}"#;
    let error = executor
        .execute(
            auth.clone(),
            request("m", payload),
            image_options("", "", false),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 429, "{error:?}");
    assert_eq!(error.message, redacted);
    assert_eq!(error.retry_after, Some(Duration::from_secs(7)));

    let Err(error) = executor
        .execute_stream(
            auth.clone(),
            request("m", payload),
            image_options("", "", true),
        )
        .await
    else {
        panic!("the stream started");
    };
    assert_eq!(error.status, 429, "{error:?}");
    assert_eq!(error.message, redacted);
    assert_eq!(error.retry_after, None);

    let mock = Mock::start(Reply::json(r#"{"echo":"sk-compat-secret"}"#)).await;
    let response = executor
        .execute(
            image_auth(&mock.url, key, &[]),
            request("m", payload),
            image_options("", "", false),
        )
        .await
        .unwrap();
    assert_eq!(&response.payload[..], br#"{"echo":"[redacted]"}"#);

    let error = executor
        .execute(
            image_auth(" ", key, &[]),
            request("m", payload),
            image_options("", "", false),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.status, error.message.as_str()),
        (401, "missing provider baseURL")
    );
}
