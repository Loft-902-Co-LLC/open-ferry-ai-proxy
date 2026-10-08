// Ported from CLIProxyAPI internal/runtime/executor/gemini_interactions_translate_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! How the body and the payload rules' baseline are translated.
//!
//! Dropped:
//! - `InvokesPluginOncePerInput` and `NativeCopyIgnoresHooks`: plugins and
//!   their hooks aren't ported.
//!
//! Changed:
//! - The `compat` cases are gone: the model the credential manager resolved
//!   for an API key (`APIKeyModelIsCompat`) isn't ported.
//! - `assertIndependentGeminiInteractionsBuffers` is gone: the body and the
//!   baseline are owned values that can't share a buffer with each other or
//!   the payload.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use open_ferry_core::exec::{Format, Options, Request};
use serde_json::Value;

use super::*;

fn request(model: &str, payload: &Bytes) -> Request {
    Request {
        model: model.into(),
        payload: payload.clone(),
    }
}

fn parse(payload: &[u8]) -> Value {
    serde_json::from_slice(payload).unwrap_or(Value::Null)
}

/// Ports TestTranslateGeminiInteractionsRequestPairReusesSameSlice.
#[test]
fn the_same_bytes_are_translated_alike() {
    let model = "gemini-3.1-flash-lite";
    let cases = [
        (
            Format::OPENAI,
            r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"hi"}]}"#,
        ),
        (
            Format::CLAUDE,
            r#"{"model":"gemini-3.1-flash-lite","max_tokens":128,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
        ),
    ];
    for (format, payload) in cases {
        let payload = Bytes::from_static(payload.as_bytes());
        for stream in [false, true] {
            let options = Options::new(format.clone());
            let (base, work) =
                translate_pair(None, &request(model, &payload), &options, model, stream);
            let want = translate_body(None, &options, model, parse(&payload), stream);
            assert_eq!(base, want, "{format:?} stream={stream}");
            assert_eq!(work, want, "{format:?} stream={stream}");
            assert!(want.get("input").is_some(), "{want}");
        }
    }
}

/// Removes a test translator when the test ends, however it ends.
struct Unregister(Format);

impl Drop for Unregister {
    fn drop(&mut self) {
        Registry::global().unregister(&self.0, &Format::INTERACTIONS);
    }
}

/// Ports TestTranslateGeminiInteractionsRequestPairTranslatesSameSliceOnce:
/// it counts the translator's calls, since equal translations would pass
/// if the same bytes were translated twice.
#[test]
fn the_same_bytes_are_translated_once() {
    let from = Format::new("gemini-interactions-translate-count");
    let registry = Registry::global();
    assert!(!registry.has_request_transformer(&from, &Format::INTERACTIONS));
    let model = "gemini-interactions-count";
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&calls);
    registry.register(
        from.clone(),
        Format::INTERACTIONS,
        Some(Arc::new(move |model: &str, body: Value, stream: bool| {
            recorder
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((model.to_owned(), stream));
            (body, None)
        })),
        Default::default(),
    );
    let _unregister = Unregister(from.clone());
    let take = || std::mem::take(&mut *calls.lock().unwrap_or_else(PoisonError::into_inner));

    let payload = Bytes::from_static(
        br#"{"model":"gemini-interactions-count","messages":[{"role":"user","content":"hi"}]}"#,
    );
    let want = parse(&payload);
    for stream in [false, true] {
        let once = vec![(model.to_owned(), stream)];
        let mut options = Options::new(from.clone());
        let (base, work) = translate_pair(None, &request(model, &payload), &options, model, stream);
        assert_eq!(take(), once, "no original, stream={stream}");
        assert_eq!((&base, &work), (&want, &want));

        options.original_request = payload.clone();
        let (base, work) = translate_pair(None, &request(model, &payload), &options, model, stream);
        assert_eq!(take(), once, "the same bytes, stream={stream}");
        assert_eq!((&base, &work), (&want, &want));

        options.original_request = Bytes::copy_from_slice(&payload);
        let (base, work) = translate_pair(None, &request(model, &payload), &options, model, stream);
        assert_eq!(take().len(), 2, "equal bytes elsewhere, stream={stream}");
        assert_eq!((&base, &work), (&want, &want));
    }
}

/// Ports TestTranslateGeminiInteractionsRequestPairTranslatesDistinctInputs.
#[test]
fn distinct_inputs_are_translated_apart() {
    let model = "gemini-3.1-flash-lite";
    let payload = Bytes::from_static(
        br#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"working"}]}"#,
    );
    let baseline = Bytes::from_static(
        br#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"baseline"}]}"#,
    );
    // The same bytes elsewhere are still translated on their own.
    let detached = Bytes::copy_from_slice(&payload);
    for original in [baseline, detached] {
        let options = Options {
            original_request: original.clone(),
            ..Options::new(Format::OPENAI)
        };
        let (base, work) = translate_pair(None, &request(model, &payload), &options, model, true);
        assert_eq!(
            work,
            translate_body(None, &options, model, parse(&payload), true)
        );
        assert_eq!(
            base,
            translate_body(None, &options, model, parse(&original), true)
        );
    }
}

/// Ports TestTranslateGeminiInteractionsRequestPairNativeCopy.
#[test]
fn an_interactions_request_is_copied() {
    let model = "gemini-3.1-flash-lite";
    let payload = Bytes::from_static(br#"{"model":"gemini-3.1-flash-lite","input":"hi"}"#);
    let original = Bytes::from_static(br#"{"model":"gemini-3.1-flash-lite","input":"original"}"#);
    for format in [Format::new(""), Format::INTERACTIONS] {
        let mut options = Options::new(format.clone());
        let (base, work) = translate_pair(None, &request(model, &payload), &options, model, false);
        assert_eq!(base, parse(&payload), "{format:?}");
        assert_eq!(work, parse(&payload), "{format:?}");

        options.original_request = original.clone();
        let (base, work) = translate_pair(None, &request(model, &payload), &options, model, true);
        assert_eq!(work, parse(&payload), "{format:?}");
        assert_eq!(base, parse(&original), "{format:?}");
    }
}

// Not upstream's: the revision is the one the headers already carry, else
// the client's, else the default; an empty one counts as none.
#[test]
fn picks_the_api_revision() {
    let revision = |sent: Option<&'static str>, client: Option<&'static str>| {
        let mut headers = HeaderMap::new();
        if let Some(value) = sent {
            headers.insert(API_REVISION_HEADER, HeaderValue::from_static(value));
        }
        let mut client_headers = HeaderMap::new();
        if let Some(value) = client {
            client_headers.insert("Api-Revision", HeaderValue::from_static(value));
        }
        apply_api_revision(&mut headers, &client_headers);
        let values: Vec<_> = headers
            .get_all(API_REVISION_HEADER)
            .iter()
            .map(|value| value.to_str().unwrap_or_default().to_owned())
            .collect();
        values
    };
    assert_eq!(revision(None, None), [API_REVISION]);
    assert_eq!(revision(None, Some("2026-06-01")), ["2026-06-01"]);
    assert_eq!(revision(None, Some("")), [API_REVISION]);
    assert_eq!(
        revision(Some("2026-06-01"), Some("2026-07-01")),
        ["2026-06-01"]
    );
    assert_eq!(revision(Some(""), Some("2026-07-01")), ["2026-07-01"]);
}

// Not upstream's: the endpoint is under the credential's base URL, without
// a trailing slash, else Google's.
#[test]
fn builds_the_interactions_url() {
    let url = |base_url: Option<&str>| {
        let mut auth = Auth::default();
        if let Some(base_url) = base_url {
            auth.attributes.insert("base_url".into(), base_url.into());
        }
        interactions_url(&auth)
    };
    assert_eq!(
        url(None),
        "https://generativelanguage.googleapis.com/v1beta/interactions"
    );
    assert_eq!(
        url(Some(" http://127.0.0.1:9/ ")),
        "http://127.0.0.1:9/v1beta/interactions"
    );
    assert_eq!(
        url(Some(" / ")),
        "https://generativelanguage.googleapis.com/v1beta/interactions"
    );
}
