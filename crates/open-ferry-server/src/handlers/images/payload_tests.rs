//! Original field presence at the image handler boundary (upstream 826bb167).

use serde_json::{Value, json};

use super::*;

fn value(raw: &[u8]) -> Value {
    serde_json::from_slice(raw).unwrap()
}

#[test]
fn generation_originals_do_not_include_handler_defaults() {
    for (fields, expected) in [
        (json!({}), json!({})),
        (
            json!({"response_format": " B64_JSON ", "size": "1024x1024"}),
            json!({"response_format": "b64_json", "aspect_ratio": "1:1"}),
        ),
        (
            json!({"response_format": "URL", "size": "2048x2048", "quality": " high ", "n": 2}),
            json!({"response_format": "url", "aspect_ratio": "1:1", "resolution": "2k", "quality": "high", "n": 2}),
        ),
        (
            json!({"aspect_ratio": "4:3", "resolution": "1k", "quality": "low", "n": 1}),
            json!({"aspect_ratio": "4:3", "resolution": "1k", "quality": "low", "n": 1}),
        ),
        (
            json!({"response_format": "", "size": "auto", "quality": " ", "n": 0}),
            json!({}),
        ),
        (json!({"response_format": null, "n": "2"}), json!({})),
    ] {
        let mut raw = fields;
        raw["model"] = json!("xai/grok-imagine-image");
        raw["prompt"] = json!(" draw ");
        let mut expected = expected;
        expected["model"] = json!("grok-imagine-image");
        expected["prompt"] = json!("draw");
        let bytes = raw.to_string();
        let original =
            xai::generations_original_request(bytes.as_bytes(), "xai/grok-imagine-image");
        assert_eq!(value(&original), expected);
        let prepared =
            xai::generations_request(bytes.as_bytes(), "xai/grok-imagine-image", "b64_json");
        let prepared = value(&prepared);
        assert!(prepared.get("response_format").is_some());
        assert!(prepared.get("aspect_ratio").is_some());
        assert!(prepared.get("resolution").is_some());
    }
}

#[tokio::test]
async fn handlers_propagate_un_defaulted_original_for_generations_and_json_edits() {
    for path in [GENERATIONS, EDITS] {
        for stream in [false, true] {
            for explicit in [false, true] {
                let (app, dispatcher) =
                    app(vec![Outcome::reply(r#"{"data":[{"b64_json":"AA=="}]}"#)]);
                let mut raw =
                    json!({"model": "grok-imagine-image", "prompt": "draw", "stream": stream});
                if path == EDITS {
                    raw["images"] = json!([{"image_url": "https://example.test/input.png"}]);
                }
                if explicit {
                    raw["response_format"] = json!("url");
                    raw["size"] = json!("2048x2048");
                    raw["quality"] = json!("high");
                    raw["n"] = json!(2);
                }
                let (status, _, _) = send(&app, post_json(path, &raw.to_string())).await;
                assert_eq!(status, StatusCode::OK);
                let calls = dispatcher.calls();
                let call = &calls[0];
                assert_eq!(call.method, "execute");
                assert!(!call.options.stream);
                let original = value(&call.options.original_request);
                assert_eq!(original["model"], "grok-imagine-image");
                assert_eq!(original["prompt"], "draw");
                for field in [
                    "response_format",
                    "aspect_ratio",
                    "resolution",
                    "quality",
                    "n",
                ] {
                    assert_eq!(
                        original.get(field).is_some(),
                        explicit,
                        "{path} {field}: {original}"
                    );
                }
                if explicit {
                    assert_eq!(original["response_format"], "url");
                    assert_eq!(original["aspect_ratio"], "1:1");
                    assert_eq!(original["resolution"], "2k");
                }
                if path == EDITS {
                    assert_eq!(original["image"]["url"], "https://example.test/input.png");
                } else if !explicit {
                    let prepared = value(&call.request.payload);
                    assert_eq!(prepared["response_format"], "b64_json");
                    assert_eq!(prepared["aspect_ratio"], "1:1");
                    assert_eq!(prepared["resolution"], "1k");
                }
            }
        }
    }
}

#[tokio::test]
async fn multipart_edits_preserve_original_field_presence_and_the_same_image_refs() {
    for stream in ["false", "true"] {
        for explicit in [false, true] {
            let (app, dispatcher) = app(vec![Outcome::reply(r#"{"data":[{"b64_json":"AA=="}]}"#)]);
            let mut fields = vec![
                ("model", "grok-imagine-image"),
                ("prompt", "edit"),
                ("stream", stream),
            ];
            if explicit {
                fields.extend([
                    ("response_format", "url"),
                    ("size", "2048x2048"),
                    ("quality", "high"),
                    ("n", "2"),
                ]);
            }
            let request = post_form(&fields, &[("image", "image.png", Some("image/png"), PNG)]);
            let (status, _, _) = send(&app, request).await;
            assert_eq!(status, StatusCode::OK);
            let calls = dispatcher.calls();
            let original = value(&calls[0].options.original_request);
            let prepared = value(&calls[0].request.payload);
            assert_eq!(original["image"], prepared["image"]);
            assert_eq!(
                original["image"]["url"],
                format!("data:image/png;base64,{PNG_BASE64}")
            );
            for field in [
                "response_format",
                "aspect_ratio",
                "resolution",
                "quality",
                "n",
            ] {
                assert_eq!(
                    original.get(field).is_some(),
                    explicit,
                    "{field}: {original}"
                );
            }
            assert_eq!(
                prepared["response_format"],
                if explicit { "url" } else { "b64_json" }
            );
        }
    }
}
