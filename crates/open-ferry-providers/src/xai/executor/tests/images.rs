// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_test.go
// (TestXAIExecutorExecuteImagesUsesImagesEndpointAndPublishesUsage,
// TestXAIExecutorExecuteImagesPublishesFailureUsage,
// TestXAIExecutorExecuteImagesPublishesRequestBuildFailureUsage,
// TestXAIExecutorExecuteImagesUsesEditsEndpoint,
// TestXAIExecutorExecuteImagesRewritesImageURLToURL) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Image calls against the mock xAI server. Upstream's tests sign in with
//! OAuth; these use the dummy API key, the only credential served. Their
//! usage records come from a usage queue of the test's own, fed by the
//! call's taps as a server handler's are.

use open_ferry_core::observe::usage;
use open_ferry_core::observe::{CallReport, Observation, RequestContext};

use super::*;

/// xAI's answer to a generation in upstream's tests.
const GENERATED: &str =
    r#"{"created":123,"data":[{"b64_json":"AA=="}],"usage":{"cost_in_usd_ticks":250000}}"#;

/// Options for a call from the image endpoints at `path`.
fn image_options(path: &str) -> Options {
    let mut options = Options::new(Format::OPENAI_IMAGE);
    options.metadata.request_path = path.into();
    options
}

/// A usage queue of the test's own, on.
fn usage_queue() -> usage::Usage {
    let mut config = Config::default();
    config.usage_statistics_enabled = true;
    let queue = usage::Usage::new(&config);
    usage::reconfigure(&queue, None, &config, true);
    queue
}

/// Runs the image call of `request` from `path` with `auth` on `executor`,
/// its usage reported to `queue` as a server handler's taps report it.
async fn execute_observed(
    executor: &XaiExecutor,
    queue: &usage::Usage,
    auth: Arc<Auth>,
    request: Request,
    path: &str,
) -> Result<Response, ExecError> {
    let mut options = image_options(path);
    let context = Arc::new(RequestContext::new(Method::POST, path.to_owned()));
    let tap = queue
        .tap(&context, &request, &options)
        .expect("the usage tap");
    options.observation = Some(Arc::new(Observation::new(context, vec![tap])));
    let report = CallReport::start(&options);
    let result = executor.execute(auth, request, options).await;
    report.finish(&result);
    result
}

/// The records queued so far, taken.
fn records(queue: &usage::Usage) -> Vec<Value> {
    queue
        .pop_oldest(usize::MAX)
        .iter()
        .map(|record| serde_json::from_slice(record).expect("a JSON record"))
        .collect()
}

// TestXAIExecutorExecuteImagesUsesImagesEndpointAndPublishesUsage: a
// generation posts the body as it came to /images/generations with the key
// and no Grok CLI identity, xAI's answer comes back, and the one usage
// record names the body's model and no tokens.
#[tokio::test]
async fn image_calls_post_to_generations_and_publish_usage() {
    let mock = Mock::start(Reply::json(GENERATED)).await;
    let queue = usage_queue();
    let payload = r#"{"model":"grok-imagine-image-quality","prompt":"draw"}"#;
    let response = execute_observed(
        &executor(),
        &queue,
        api_key_auth(&mock.url),
        request("image-model-alias", payload),
        "/v1/images/generations",
    )
    .await
    .unwrap();

    let seen = mock.last();
    assert_eq!(seen.path, "/images/generations");
    assert_eq!(seen.header("authorization"), Some("Bearer xai-test-key"));
    assert_eq!(seen.header("accept"), Some("application/json"));
    assert_own_identity(&seen);
    assert_eq!(seen.body, payload);
    assert_eq!(payload_json(&response)["data"][0]["b64_json"], "AA==");
    assert_eq!(
        response.headers.get("content-type").unwrap(),
        "application/json"
    );

    let records = records(&queue);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["model"], "grok-imagine-image-quality", "{record}");
    assert_eq!(record["provider"], "xai", "{record}");
    assert_eq!(record["executor_type"], "XAIExecutor", "{record}");
    assert_eq!(record["failed"], false, "{record}");
    for name in [
        "input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "cached_tokens",
        "total_tokens",
    ] {
        assert_eq!(record["tokens"][name], 0, "{name}: {record}");
    }
    assert!(record["ttft_ms"].as_i64().unwrap() >= 0, "{record}");
}

// TestXAIExecutorExecuteImagesPublishesFailureUsage: xAI's failure is an
// error with its status, and the record a failure with it, naming the
// body's model.
#[tokio::test]
async fn image_failures_publish_failure_usage() {
    let mock = Mock::start(Reply::error(429, r#"{"error":"rate limited"}"#)).await;
    let queue = usage_queue();
    let error = execute_observed(
        &executor(),
        &queue,
        api_key_auth(&mock.url),
        request(
            "image-model-alias",
            r#"{"model":"grok-imagine-image-quality","prompt":"draw"}"#,
        ),
        "/v1/images/generations",
    )
    .await
    .unwrap_err();
    assert_eq!(error.http_status(), 429, "{error:?}");
    assert_eq!(error.message, r#"{"error":"rate limited"}"#);

    let records = records(&queue);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["model"], "grok-imagine-image-quality", "{record}");
    assert_eq!(record["failed"], true, "{record}");
    assert_eq!(record["fail"]["status_code"], 429, "{record}");
}

// TestXAIExecutorExecuteImagesPublishesRequestBuildFailureUsage: a base URL
// that isn't one fails the call, and the record is a failure naming the
// request's model, the body having none.
#[tokio::test]
async fn image_request_build_failures_publish_failure_usage() {
    let mut auth = Auth {
        provider: "xai".into(),
        ..Auth::default()
    };
    auth.attributes
        .insert("base_url".into(), "://invalid".into());
    let queue = usage_queue();
    let error = execute_observed(
        &executor(),
        &queue,
        Arc::new(auth),
        request("grok-imagine-image-fallback", r#"{"prompt":"draw"}"#),
        "/v1/images/generations",
    )
    .await
    .unwrap_err();
    assert_eq!(error.http_status(), 0, "{error:?}");

    let records = records(&queue);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["model"], "grok-imagine-image-fallback", "{record}");
    assert_eq!(record["failed"], true, "{record}");
}

// TestXAIExecutorExecuteImagesUsesEditsEndpoint: an edit goes to
// /images/edits.
#[tokio::test]
async fn image_edits_post_to_edits() {
    let mock = Mock::start(Reply::json(
        r#"{"created":123,"data":[{"url":"https://x.ai/image.png"}]}"#,
    ))
    .await;
    let payload = r#"{"model":"grok-imagine-image","prompt":"edit","image":{"type":"image_url","url":"https://example.com/a.png"}}"#;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-imagine-image", payload),
            image_options("/v1/images/edits"),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.path, "/images/edits");
    assert_eq!(seen.body, payload);
}

// TestXAIExecutorExecuteImagesRewritesImageURLToURL: an image reference's
// `image_url` becomes xAI's `url`.
#[tokio::test]
async fn image_refs_get_xais_url_field() {
    let mock = Mock::start(Reply::json(
        r#"{"created":123,"data":[{"url":"https://x.ai/image.png"}]}"#,
    ))
    .await;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "grok-imagine-image",
                r#"{"model":"grok-imagine-image","prompt":"edit","image":{"image_url":"https://example.com/a.png"}}"#,
            ),
            image_options("/v1/images/edits"),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(body["image"]["url"], "https://example.com/a.png", "{body}");
    assert!(!exists(&body, "image.image_url"), "{body}");
}

// Not upstream's: the payload rules apply for the body's model (or the
// model the client asked for) under protocol `openai`; a body neither they
// nor the image references change is sent byte for byte.
#[tokio::test]
async fn image_bodies_get_payload_rules() {
    let mock = Mock::start(Reply::json(GENERATED)).await;
    let executor = executor_with(
        "payload:\n  override:\n    - models:\n        - name: grok-imagine-image-2.0\n          protocol: openai\n      params:\n        resolution: 2k\n",
    );
    generate(
        &executor,
        "image-model-alias",
        r#"{"model":"grok-imagine-image-2.0","prompt":"draw","n":1}"#,
        &mock,
    )
    .await;
    assert_eq!(
        mock.last().body,
        r#"{"model":"grok-imagine-image-2.0","prompt":"draw","n":1,"resolution":"2k"}"#
    );

    // Another model: no rule applies, and the body goes as it came.
    let payload = "{ \"model\" : \"grok-imagine-image\", \"prompt\":\"draw\" }";
    generate(&executor, "image-model-alias", payload, &mock).await;
    assert_eq!(mock.last().body, payload);

    // The model the client asked for matches too, as upstream's rules do.
    let payload = r#"{"model":"grok-imagine-image","prompt":"draw"}"#;
    generate(&executor, "grok-imagine-image-2.0", payload, &mock).await;
    assert_eq!(
        mock.last().body,
        r#"{"model":"grok-imagine-image","prompt":"draw","resolution":"2k"}"#
    );
}

/// Runs a generation of `payload` for `model` against `mock`.
async fn generate(executor: &XaiExecutor, model: &str, payload: &str, mock: &Mock) {
    executor
        .execute(
            api_key_auth(&mock.url),
            request(model, payload),
            image_options("/v1/images/generations"),
        )
        .await
        .unwrap();
}

// Not upstream's: the key the request sent is redacted from xAI's answer,
// a failure's or a success's.
#[tokio::test]
async fn image_answers_lose_the_requests_secrets() {
    let echo = format!(r#"{{"error":"bad key {API_KEY}"}}"#);
    for status in [401, 200] {
        let mock = Mock::start(Reply::error(status, &echo)).await;
        let result = executor()
            .execute(
                api_key_auth(&mock.url),
                request("grok-imagine-image", r#"{"prompt":"draw"}"#),
                image_options("/v1/images/generations"),
            )
            .await;
        let text = match result {
            Ok(response) => String::from_utf8(response.payload.to_vec()).unwrap(),
            Err(error) => error.message,
        };
        assert!(!text.contains(API_KEY), "{status}: {text}");
        assert!(text.contains("bad key"), "{status}: {text}");
    }
}
