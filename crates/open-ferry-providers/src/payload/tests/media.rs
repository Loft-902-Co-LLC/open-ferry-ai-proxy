// Ported from CLIProxyAPI internal/runtime/executor/helps/payload_finalizer_test.go
// (TestPayloadFinalizerMultipartPreservesFiles) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The rules applied to an image or video body: a form's view, written
//! back only when a rule changes it, and a JSON body.

use bytes::Bytes;
use http::header::{CONTENT_TYPE, HeaderValue};
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options, Request};
use open_ferry_core::multipart::Writer;
use serde_json::Value;

use super::json;
use crate::payload::media::{MediaTarget, View, apply_media_rules, view};
use crate::payload::{MediaError, Rules, apply_media};

/// A form of a `model` field and an `image` file, as the upstream test
/// writes it, and its content type.
fn upstream_form() -> (Bytes, String) {
    let mut writer = Writer::new();
    writer.write_field("model", b"original");
    writer.write_file("image", "input.png", &[0, 255, 13, 10]);
    let content_type = writer.form_data_content_type();
    (Bytes::from(writer.finish()), content_type)
}

/// A request for `model` whose body is `payload`, sent as `content_type`.
fn call(model: &str, payload: &Bytes, content_type: &str) -> (Request, Options) {
    let request = Request {
        model: model.to_owned(),
        payload: payload.clone(),
    };
    let mut options = Options::new(Format::OPENAI_IMAGE);
    if let Ok(value) = HeaderValue::from_str(content_type) {
        options.headers.insert(CONTENT_TYPE, value);
    }
    (request, options)
}

/// The format of every body here.
static OPENAI: Format = Format::OPENAI;

/// The target of every call here.
fn target(model: &str) -> MediaTarget<'_> {
    MediaTarget {
        executor: "openai",
        model,
        protocol: &OPENAI,
    }
}

/// The view of a body: a form's, or a JSON body.
fn view_of(body: &Bytes, content_type: &str) -> Value {
    match view(body, content_type.as_bytes()) {
        Ok(View::Json(value) | View::Form(value)) => value,
        Ok(View::Other { .. }) => panic!("neither JSON nor a form"),
        Err(error) => panic!("{error}"),
    }
}

/// The payload rules of a config document.
fn rules(yaml: &str) -> Rules {
    super::rules(yaml)
}

/// `TestPayloadFinalizerMultipartPreservesFiles`: an override of a form
/// field writes the form again, its file intact.
#[test]
fn multipart_preserves_files() {
    let config = Config::parse(
        "payload:\n  override:\n    - models:\n        - name: original\n      params:\n        model: configured\n",
    )
    .unwrap();
    let (form, content_type) = upstream_form();
    let (request, options) = call("original", &form, &content_type);
    let (body, out_type) = apply_media(
        Some(&config),
        &target("original"),
        &request,
        &options,
        form.clone(),
        &content_type,
    )
    .unwrap();
    assert_ne!(out_type, content_type);
    let view = view_of(&body, &out_type);
    assert_eq!(view["model"], "configured");
    assert_eq!(view["image"]["data"], "AP8NCg==");
    assert_eq!(view["image"]["filename"], "input.png");
    assert_eq!(view["image"]["content_type"], "application/octet-stream");
}

/// Not upstream's: a form no rule changes is sent byte for byte, with its
/// own content type.
#[test]
fn unchanged_form_is_sent_as_it_came() {
    let (form, content_type) = upstream_form();
    let (request, options) = call("original", &form, &content_type);
    let rules = rules(
        "payload:\n  override:\n    - models:\n        - name: other\n      params:\n        model: configured\n",
    );
    for rules in [None, Some(&rules)] {
        let (body, out_type) = apply_media_rules(
            rules,
            &target("original"),
            &request,
            &options,
            form.clone(),
            &content_type,
        )
        .unwrap();
        assert_eq!(body, form);
        assert_eq!(out_type, content_type);
    }
}

/// Not upstream's: a name sent twice is an array in the view, and becomes
/// two parts again; a field a rule adds is written as gjson's `String()`;
/// a file part's own content type is kept.
#[test]
fn repeated_fields_and_added_fields() {
    let mut writer = Writer::with_boundary("b").unwrap();
    writer.write_field("prompt", b"a cat");
    let mut header = open_ferry_core::multipart::Header::new();
    header.set(
        "Content-Disposition",
        "form-data; name=\"image[]\"; filename=\"a.png\"",
    );
    header.set("Content-Type", "image/png");
    writer.write_part(&header, b"one");
    header.set(
        "Content-Disposition",
        "form-data; name=\"image[]\"; filename=\"dir/b.png\"",
    );
    writer.write_part(&header, b"two");
    let form = Bytes::from(writer.finish());
    let content_type = "multipart/form-data; boundary=b";
    assert_eq!(
        view_of(&form, content_type),
        json(
            r#"{"image[]":[{"content_type":"image/png","data":"b25l","filename":"a.png"},{"content_type":"image/png","data":"dHdv","filename":"b.png"}],"prompt":"a cat"}"#
        )
    );

    let rules = rules(
        "payload:\n  override:\n    - models:\n        - name: gpt-image-2\n      params:\n        n: 2\n        quality: 1.50\n        extra:\n          k: v\n",
    );
    let (request, options) = call("gpt-image-2", &form, content_type);
    let (body, out_type) = apply_media_rules(
        Some(&rules),
        &target("gpt-image-2"),
        &request,
        &options,
        form.clone(),
        content_type,
    )
    .unwrap();
    assert!(out_type.starts_with("multipart/form-data; boundary="));
    assert_eq!(
        view_of(&body, &out_type),
        json(
            r#"{"extra":"{\"k\":\"v\"}","image[]":[{"content_type":"image/png","data":"b25l","filename":"a.png"},{"content_type":"image/png","data":"dHdv","filename":"b.png"}],"n":"2","prompt":"a cat","quality":"1.5"}"#
        )
    );
}

/// Not upstream's: defaults check the client's form as it came, and are
/// written where it has no such field.
#[test]
fn defaults_check_the_clients_form() {
    let rules = rules(
        "payload:\n  default:\n    - models:\n        - name: gpt-image-2\n      params:\n        quality: high\n        prompt: not applied\n",
    );
    let mut writer = Writer::with_boundary("b").unwrap();
    writer.write_field("prompt", b"a cat");
    let form = Bytes::from(writer.finish());
    let content_type = "multipart/form-data; boundary=b";
    let (request, options) = call("gpt-image-2", &form, content_type);
    let (body, out_type) = apply_media_rules(
        Some(&rules),
        &target("gpt-image-2"),
        &request,
        &options,
        form.clone(),
        content_type,
    )
    .unwrap();
    assert_eq!(
        view_of(&body, &out_type),
        json(r#"{"prompt":"a cat","quality":"high"}"#)
    );

    // A body rebuilt from the client's form is checked against the form.
    let mut writer = Writer::with_boundary("c").unwrap();
    writer.write_field("prompt", b"rebuilt");
    let rebuilt = Bytes::from(writer.finish());
    let (body, out_type) = apply_media_rules(
        Some(&rules),
        &target("gpt-image-2"),
        &request,
        &options,
        rebuilt,
        "multipart/form-data; boundary=c",
    )
    .unwrap();
    assert_eq!(
        view_of(&body, &out_type),
        json(r#"{"prompt":"rebuilt","quality":"high"}"#)
    );
}

/// Not upstream's: a JSON body is written compactly when a rule changes
/// it, and sent as it came when none does.
#[test]
fn json_bodies() {
    let rules = rules(
        "payload:\n  override:\n    - models:\n        - name: gpt-image-2\n      params:\n        size: 1024x1024\n",
    );
    let raw = Bytes::from_static(b"{ \"model\": \"gpt-image-2\", \"n\": 1.0 }");
    let (request, options) = call("gpt-image-2", &raw, "application/json");
    let (body, out_type) = apply_media_rules(
        Some(&rules),
        &target("gpt-image-2"),
        &request,
        &options,
        raw.clone(),
        "application/json",
    )
    .unwrap();
    assert_eq!(
        body.as_ref(),
        br#"{"model":"gpt-image-2","n":1.0,"size":"1024x1024"}"#
    );
    assert_eq!(out_type, "application/json");
    let (request, options) = call("other", &raw, "application/json");
    let (body, _) = apply_media_rules(
        Some(&rules),
        &target("other"),
        &request,
        &options,
        raw.clone(),
        "application/json",
    )
    .unwrap();
    assert_eq!(body, raw);
}

/// Not upstream's: a body that is neither JSON nor a form goes as it came;
/// one of a form content type that doesn't read fails with Go's error, as
/// does a file whose data a rule broke.
#[test]
fn errors_and_other_bodies() {
    let rules = rules(
        "payload:\n  override:\n    - models:\n        - name: m\n      params:\n        image.data: \"!!\"\n",
    );
    let text = Bytes::from_static(b"not json");
    let (request, options) = call("m", &text, "text/plain");
    let (body, out_type) = apply_media_rules(
        Some(&rules),
        &target("m"),
        &request,
        &options,
        text.clone(),
        "text/plain",
    )
    .unwrap();
    assert_eq!((body, out_type.as_str()), (text.clone(), "text/plain"));

    let error = apply_media_rules(
        Some(&rules),
        &target("m"),
        &request,
        &options,
        text.clone(),
        "multipart/form-data; boundary=b",
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "multipart: NextPart: EOF");

    let (form, content_type) = upstream_form();
    let (request, options) = call("m", &form, &content_type);
    let error = apply_media_rules(
        Some(&rules),
        &target("m"),
        &request,
        &options,
        form,
        &content_type,
    )
    .unwrap_err();
    assert!(matches!(error, MediaError::Data { .. }));
    assert_eq!(
        error.to_string(),
        "multipart payload image: illegal base64 data at input byte 0"
    );
}
