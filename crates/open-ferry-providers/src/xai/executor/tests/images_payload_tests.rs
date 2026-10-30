//! Image default rules against the un-defaulted original (upstream 826bb167).

use super::*;

const DEFAULTS: &str = "payload:\n  default:\n    - models:\n        - name: grok-imagine-*\n      params:\n        response_format: url\n        aspect_ratio: '16:9'\n        resolution: 2k\n        quality: high\n        n: 2\n";

async fn execute_with_original(config: &str, prepared: &str, original: &str, path: &str) -> Value {
    let mock = Mock::start(Reply::json(GENERATED)).await;
    let mut options = image_options(path);
    options.original_request = Bytes::copy_from_slice(original.as_bytes());
    executor_with(config)
        .execute(
            api_key_auth(&mock.url),
            request("grok-imagine-image", prepared),
            options,
        )
        .await
        .unwrap();
    assert_eq!(mock.last().path, path.strip_prefix("/v1").unwrap());
    mock.last().json()
}

#[tokio::test]
async fn image_payload_defaults_use_original_not_prepared_handler_defaults() {
    for path in ["/v1/images/generations", "/v1/images/edits"] {
        let prepared = r#"{"model":"grok-imagine-image","prompt":"draw","response_format":"b64_json","aspect_ratio":"1:1","resolution":"1k"}"#;
        let original = r#"{"model":"grok-imagine-image","prompt":"draw"}"#;
        let body = execute_with_original(DEFAULTS, prepared, original, path).await;
        assert_eq!(body["response_format"], "url");
        assert_eq!(body["aspect_ratio"], "16:9");
        assert_eq!(body["resolution"], "2k");
        assert_eq!(body["quality"], "high");
        assert_eq!(body["n"], 2);
    }
}

#[tokio::test]
async fn image_payload_defaults_preserve_explicit_fields_and_normalized_image_refs() {
    let explicit = r#"{"model":"grok-imagine-image","prompt":"edit","response_format":"b64_json","aspect_ratio":"4:3","resolution":"1k","quality":"low","n":1,"image":{"image_url":"https://example.test/input.png"}}"#;
    let config = format!("{DEFAULTS}        image.url: https://example.test/replaced.png\n");
    let body = execute_with_original(&config, explicit, explicit, "/v1/images/edits").await;
    assert_eq!(body["response_format"], "b64_json");
    assert_eq!(body["aspect_ratio"], "4:3");
    assert_eq!(body["resolution"], "1k");
    assert_eq!(body["quality"], "low");
    assert_eq!(body["n"], 1);
    assert_eq!(body["image"]["url"], "https://example.test/input.png");
    assert!(body["image"].get("image_url").is_none());

    let body = execute_with_original(DEFAULTS, explicit, "", "/v1/images/edits").await;
    assert_eq!(
        body["response_format"], "b64_json",
        "no original falls back to the prepared payload"
    );
}

#[tokio::test]
async fn image_default_raw_then_override_then_filter_remain_the_final_barrier() {
    let config = "payload:\n  default-raw:\n    - models:\n        - name: grok-imagine-*\n      params:\n        response_format: '\"url\"'\n        n: '3'\n  override:\n    - models:\n        - name: grok-imagine-*\n      params:\n        response_format: b64_json\n        aspect_ratio: '9:16'\n  filter:\n    - models:\n        - name: grok-imagine-*\n      params:\n        - resolution\n";
    let prepared = r#"{"model":"grok-imagine-image","prompt":"draw","response_format":"url","aspect_ratio":"1:1","resolution":"1k"}"#;
    let body = execute_with_original(
        config,
        prepared,
        r#"{"prompt":"draw"}"#,
        "/v1/images/generations",
    )
    .await;
    assert_eq!(body["response_format"], "b64_json");
    assert_eq!(body["aspect_ratio"], "9:16");
    assert_eq!(body["n"], 3);
    assert!(body.get("resolution").is_none());

    let raw_only = "payload:\n  default-raw:\n    - models:\n        - name: grok-imagine-*\n      params:\n        response_format: '\"url\"'\n";
    let body = execute_with_original(
        raw_only,
        prepared,
        r#"{"prompt":"draw"}"#,
        "/v1/images/generations",
    )
    .await;
    assert_eq!(body["response_format"], "url");
}
