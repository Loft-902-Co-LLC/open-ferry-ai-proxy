// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_execution.go
// (requestToFormat's openai-image and openai-video case) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Calls from the image and video endpoints go through the manager with
//! their source format, request path, payload and response untouched.
//!
//! Deviations from upstream:
//! - Upstream has no test of this; it keeps the two formats for its request
//!   interceptors, which aren't ported. These tests check what the image and
//!   video executors rely on instead.

use bytes::Bytes;

use super::support::*;
use crate::exec::{Dispatcher, Format, Options, Request};
use crate::manager::Settings;

/// A multipart form with a text field and a small binary file.
const FORM: &[u8] = b"--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\ndraw\r\n--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\x00\xff\r\n\r\n--b--\r\n";

/// Options for a call from `path` in `format`, as the image and video
/// handlers make them.
fn media(format: Format, path: &str, stream: bool) -> Options {
    let mut opts = Options::new(format);
    opts.stream = stream;
    opts.metadata.request_path = path.to_owned();
    opts.original_request = Bytes::from_static(FORM);
    opts.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("multipart/form-data; boundary=b"),
    );
    opts
}

fn form_request(model: &str) -> Request {
    Request {
        model: model.to_owned(),
        payload: Bytes::from_static(FORM),
    }
}

// Not upstream's: a non-streaming call in either format.
#[tokio::test(start_paused = true)]
async fn media_calls_reach_the_executor_untouched() {
    let cases = [
        (Format::OPENAI_IMAGE, "/v1/images/edits", "gpt-image-2"),
        (Format::OPENAI_VIDEO, "/v1/videos", "grok-imagine-video"),
    ];
    for (format, path, model) in cases {
        let h = Harness::new(Settings::default());
        let answer = r#"{"created":1,"data":[{"b64_json":"AAA"}]}"#;
        let executor = FakeExecutor::with("xai", move |_| Reply::ok(answer));
        h.executor(&executor);
        h.add(auth("x", "xai"), &[model]);

        let response = h
            .manager
            .execute(
                &providers(&["xai"]),
                form_request(model),
                media(format.clone(), path, false),
            )
            .await
            .unwrap();
        assert_eq!(&response.payload[..], answer.as_bytes());

        let calls = executor.calls();
        let [call] = calls.as_slice() else {
            panic!("calls = {calls:?}");
        };
        assert_eq!(call.model, model);
        assert_eq!(&call.payload[..], FORM);
        assert_eq!(call.options.source_format, format);
        assert_eq!(call.options.response_format, format);
        assert_eq!(call.options.metadata.request_path, path);
        assert_eq!(&call.options.original_request[..], FORM);
        assert_eq!(
            call.options
                .headers
                .get(http::header::CONTENT_TYPE)
                .unwrap(),
            "multipart/form-data; boundary=b"
        );
    }
}

// Not upstream's: a streaming image call, whose events come back as the
// executor gave them.
#[tokio::test(start_paused = true)]
async fn media_streams_reach_the_executor_untouched() {
    let h = Harness::new(Settings::default());
    let event = "event: image_generation.completed\ndata: {\"b64_json\":\"AAA\"}\n\n";
    let executor = FakeExecutor::with("codex", move |_| {
        Reply::chunks(vec![Ok(Bytes::from_static(event.as_bytes()))])
    });
    h.executor(&executor);
    h.add(auth("c", "codex"), &["gpt-image-2"]);

    let stream = h
        .manager
        .execute_stream(
            &providers(&["codex"]),
            form_request("gpt-image-2"),
            media(Format::OPENAI_IMAGE, "/v1/images/generations", true),
        )
        .await
        .unwrap();
    let (chunks, err) = collect(stream).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(chunks, [event]);

    let calls = executor.calls();
    let [call] = calls.as_slice() else {
        panic!("calls = {calls:?}");
    };
    assert_eq!(call.kind, Kind::Stream);
    assert_eq!(&call.payload[..], FORM);
    assert_eq!(call.options.source_format, Format::OPENAI_IMAGE);
    assert!(call.options.stream);
}
