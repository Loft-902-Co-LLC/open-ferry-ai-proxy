// Ported from CLIProxyAPI internal/runtime/executor/helps/apply_patch_test.go
// (TestApplyPatchTranslationError, TestApplyPatchFinalizeOptionalCanonicalState,
// TestApplyPatchTokenUsageHelperRetainsFailureState,
// TestApplyPatchInteractionsOnlyValidSourceDoneClosesTransport,
// TestApplyPatchCanceledEOFStillRecordsFailure) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Upstream's `helps` stream helpers, through what holds their state here:
//! a [`ResponseStream`] from the translator registry, whose translator
//! keeps the bridge's state and its failure, and the executor streams that
//! read one.
//!
//! Upstream's executors share the state between helpers as an untyped
//! `param`: `InitializeApplyPatchStream` prepares it, `FinalizeApplyPatchStream`
//! fails it at the end of the stream, and `ApplyPatchTranslationError` reads
//! its failure. Here the translator is made with the client's request
//! ([`Registry::response_stream`]) and an empty chunk starts it, as
//! `InitializeApplyPatchStream`'s empty chunk does upstream: the Chat
//! Completions and Interactions translators, like upstream's, fail nothing
//! at the end of a stream they were never given a chunk of.
//! [`ResponseStream::finish`] fails it and
//! [`ResponseStream::tool_input_error`] reads its failure.
//!
//! The OpenAI-compatible and Gemini Interactions executors didn't start
//! their translators, so a patch request whose answer was an empty stream
//! ended with another 502 (OpenAI-compatible) or with nothing at all
//! (Interactions), where upstream's fail it with one `response.failed`
//! and the `apply_patch` 502. They now start them;
//! `empty_eof_fails_canonically_through_each_executor` checks it.
//!
//! Deviations from upstream:
//! - `TestApplyPatchTranslationError` and the unrelated states of
//!   `TestApplyPatchFinalizeOptionalCanonicalState` give the helpers a
//!   state that isn't the bridge's (`struct{}{}`, `nil`). A stream here
//!   always has its translator's state, so the unrelated states are a
//!   stream with no translator for its pair and a stream for a request
//!   without the patch tool.
//! - `TestApplyPatchTokenUsageHelperRetainsFailureState` calls
//!   `TranslateStreamWithClaudeInputTokens` alone; here that step is part of
//!   each executor's stream, so the test reads the OpenAI-compatible
//!   executor's.
//! - `TestApplyPatchCanceledEOFStillRecordsFailure` checks that the usage
//!   upstream's executor publishes itself is a failure even when the
//!   context is canceled at the end of the stream. Here the manager's usage
//!   tap records the call from the error the stream ends with, and a
//!   dropped stream records an error it already has ready but waits for
//!   nothing, where upstream watches its context, so the test checks that
//!   the Gemini executor's stream, at the end of an empty answer, gives the
//!   failure frame with the 502 ready right behind it, nothing to wait for
//!   between them.

use bytes::Bytes;
use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use open_ferry_core::exec::{Format, Options, Request};
use open_ferry_translate::registry::{Registry, ResponseContext, ResponseStream};
use serde_json::Value;

use super::{
    TASK6_PATCH_REQUEST, Upstream, WAIT, assert_patch_error, auth, collect, events, executor, json,
    payloads, sse,
};
use crate::json::str_at;

/// The client's request in upstream's helper tests: the patch tool alone.
const ORIGINAL: &str = r#"{"tools":[{"type":"custom","name":"apply_patch"}]}"#;

/// A Chat Completions chunk calling `apply_patch` with arguments that
/// can't be a patch.
const INVALID_CHUNK: &str = r#"data: {"id":"r","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"apply_patch","arguments":"{\"input\":7}"}}]}}]}"#;

/// A stream from the provider's format `from` to the Responses API, for
/// the client's request `original` (upstream's `InitializeApplyPatchStream`
/// with `original` as both requests).
fn responses_stream(from: &Format, original: &Value) -> ResponseStream {
    Registry::global().response_stream(
        from,
        &Format::OPENAI_RESPONSE,
        &ResponseContext {
            model: "model",
            original_request: original,
            request: original,
        },
    )
}

/// Whether one of `chunks` is a `response.failed` event.
fn has_failure(chunks: &[Vec<u8>]) -> bool {
    chunks
        .iter()
        .any(|chunk| String::from_utf8_lossy(chunk).contains(r#""type":"response.failed""#))
}

// TestApplyPatchTranslationError: the failure a bridged stream retains is
// read back from it; a state that isn't the bridge's has none. Upstream
// sets the error on the state by hand; here the translator retains it from
// arguments that can't be a patch.
#[test]
fn translation_error() {
    let original = json(ORIGINAL);
    let mut stream = responses_stream(&Format::OPENAI, &original);
    assert!(
        stream.tool_input_error().is_none(),
        "failed before any call"
    );
    let chunks = stream.translate(INVALID_CHUNK.as_bytes());
    assert!(
        stream.tool_input_error().is_some(),
        "the stream didn't retain its failure: {chunks:?}"
    );

    // The unrelated states: no translator for the pair, and a request
    // without the patch tool, given the same call.
    let mut untranslated = Registry::global().response_stream(
        &Format::OPENAI_RESPONSE,
        &Format::OPENAI_RESPONSE,
        &ResponseContext {
            model: "model",
            original_request: &original,
            request: &original,
        },
    );
    assert!(!untranslated.is_translated(), "a translator for no pair");
    untranslated.translate(INVALID_CHUNK.as_bytes());
    let plain = json(r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#);
    let mut unrelated = responses_stream(&Format::OPENAI, &plain);
    unrelated.translate(INVALID_CHUNK.as_bytes());
    assert!(
        untranslated.tool_input_error().is_none() && unrelated.tool_input_error().is_none(),
        "an unrelated stream reported a patch failure"
    );
}

// TestApplyPatchFinalizeOptionalCanonicalState: a stream for the patch tool
// that ends before any event fails with one `response.failed` event, once,
// and nothing that comes after reopens it or forwards the provider's
// `[DONE]`. Upstream prepares the state with `InitializeApplyPatchStream`;
// here the translator is made ready with the request.
#[test]
fn finalize_optional_canonical_state() {
    let original = json(ORIGINAL);
    let late = br#"data: {"type":"message_stop","event_type":"interaction.completed","candidates":[{"finishReason":"STOP"}],"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
    for source in [
        Format::OPENAI,
        Format::CLAUDE,
        Format::GEMINI,
        Format::INTERACTIONS,
    ] {
        let mut stream = responses_stream(&source, &original);
        assert!(
            stream.translate(b"").is_empty(),
            "{source}: starting the stream gave something"
        );
        let frames = stream.finish();
        assert!(
            frames.len() == 1 && stream.tool_input_error().is_some() && has_failure(&frames),
            "{source}: EOF did not fail canonically: {frames:?}"
        );
        assert!(
            stream.finish().is_empty(),
            "{source}: EOF failure emitted twice"
        );
        assert!(
            stream.translate(late).is_empty(),
            "{source}: EOF failure reopened into success"
        );
        assert!(
            stream.translate(b"[DONE]").is_empty(),
            "{source}: failure forwarded transport success"
        );
    }

    // The unrelated states: no translator for the pair, and a request
    // without the patch tool.
    let mut untranslated = Registry::global().response_stream(
        &Format::OPENAI_RESPONSE,
        &Format::OPENAI_RESPONSE,
        &ResponseContext {
            model: "model",
            original_request: &original,
            request: &original,
        },
    );
    let plain = json(r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#);
    for source in [
        Format::OPENAI,
        Format::CLAUDE,
        Format::GEMINI,
        Format::INTERACTIONS,
    ] {
        let mut unrelated = responses_stream(&source, &plain);
        let frames = unrelated.finish();
        assert!(
            !has_failure(&frames) && unrelated.tool_input_error().is_none(),
            "{source}: an unrelated state received an EOF failure: {frames:?}"
        );
    }
    assert!(
        untranslated.finish().is_empty(),
        "a stream with no translator received an EOF failure"
    );
}

// Not upstream's: the executors half of
// TestApplyPatchFinalizeOptionalCanonicalState. Each executor upstream
// starts with `InitializeApplyPatchStream` (OpenAI-compatible, Claude,
// Gemini, Vertex AI and Gemini Interactions) fails a Responses client's
// patch request whose answer is an empty stream with one
// `response.failed` and the 502, not a success or another error.
#[tokio::test]
async fn empty_eof_fails_canonically_through_each_executor() {
    for provider in [
        "custom-compat",
        "claude",
        "gemini",
        "vertex",
        "gemini-interactions",
    ] {
        let upstream = Upstream::answering("").await;
        let base_url = if provider == "custom-compat" {
            format!("{}/v1", upstream.url)
        } else {
            upstream.url.clone()
        };
        let executor = executor(provider);
        let auth = auth(provider, executor.as_ref(), &base_url, false);
        let request = Request {
            model: "model".into(),
            payload: Bytes::from_static(TASK6_PATCH_REQUEST.as_bytes()),
        };
        let options = Options {
            stream: true,
            original_request: Bytes::from_static(TASK6_PATCH_REQUEST.as_bytes()),
            ..Options::new(Format::OPENAI_RESPONSE)
        };
        let response = tokio::time::timeout(WAIT, executor.execute_stream(auth, request, options))
            .await
            .unwrap_or_else(|_| panic!("{provider}: the call didn't start"))
            .unwrap_or_else(|error| panic!("{provider}: the call failed: {error:?}"));
        let (chunks, errors) = collect(response).await;
        let failed = events(&chunks)
            .iter()
            .filter(|event| str_at(event, "type") == "response.failed")
            .count();
        assert!(
            failed == 1 && errors.len() == 1,
            "{provider}: EOF did not fail canonically: {chunks:?} {errors:?}"
        );
        assert_patch_error(&errors[0]);
    }
}

// TestApplyPatchTokenUsageHelperRetainsFailureState: the step that fills
// in a Responses answer's usage (upstream's
// `TranslateStreamWithClaudeInputTokens`) keeps the bridge's failure and
// leaves its frame alone. Here that step is part of the executor's stream,
// so the OpenAI-compatible executor reads upstream's chunk.
#[tokio::test]
async fn token_usage_helper_retains_failure_state() {
    let chunk = INVALID_CHUNK
        .strip_prefix("data: ")
        .unwrap_or(INVALID_CHUNK)
        .to_owned();
    let upstream = Upstream::answering(&sse(&[chunk])).await;
    let executor = executor("custom-compat");
    let auth = auth(
        "custom-compat",
        executor.as_ref(),
        &format!("{}/v1", upstream.url),
        false,
    );
    let request = Request {
        model: "model".into(),
        payload: Bytes::from_static(ORIGINAL.as_bytes()),
    };
    let options = Options {
        stream: true,
        original_request: Bytes::from_static(ORIGINAL.as_bytes()),
        ..Options::new(Format::OPENAI_RESPONSE)
    };
    let response = tokio::time::timeout(WAIT, executor.execute_stream(auth, request, options))
        .await
        .expect("the call didn't start")
        .unwrap_or_else(|error| panic!("the call failed: {error:?}"));
    let (chunks, errors) = collect(response).await;
    assert!(
        errors.len() == 1,
        "the stream discarded its retained failure: {errors:?}\n{chunks:?}"
    );
    assert_patch_error(&errors[0]);
    let failures: Vec<&String> = chunks
        .iter()
        .filter(|chunk| chunk.contains(r#""type":"response.failed""#))
        .collect();
    assert!(failures.len() == 1, "failure frames: {chunks:?}");
    assert!(
        !failures[0].contains(r#""usage""#),
        "usage normalization altered a failure frame: {}",
        failures[0]
    );
}

// TestApplyPatchInteractionsOnlyValidSourceDoneClosesTransport: after an
// Interactions answer completes, only the first well-formed `[DONE]` closes
// the client's stream; a malformed event and a second sentinel give
// nothing.
#[test]
fn interactions_only_valid_source_done_closes_transport() {
    let mut stream = responses_stream(&Format::INTERACTIONS, &Value::Null);
    stream.translate(br#"{"event_type":"interaction.completed","interaction":{"id":"r"}}"#);
    let chunks = stream.translate(br#"{"event_type":"done","#);
    assert!(
        chunks.is_empty(),
        "malformed post-terminal event was accepted as a sentinel: {chunks:?}"
    );
    let chunks = stream.translate(b"[DONE]");
    assert!(
        chunks.len() == 1 && chunks[0] == b"data: [DONE]",
        "first real source sentinel was swallowed: {chunks:?}"
    );
    assert!(
        stream.translate(b"[DONE]").is_empty()
            && stream.translate(br#"{"event_type":"done"}"#).is_empty(),
        "duplicate source sentinel was forwarded"
    );
}

// TestApplyPatchCanceledEOFStillRecordsFailure, adapted: see the module's
// deviations. Upstream's Gemini source ends with nothing for a request with
// the patch tool, and its context is canceled before the failure goes out;
// the failure must still be recorded. Here the manager's usage tap records
// it from the stream's error, so the error must be ready as soon as the
// failure frame is taken: a client gone after that frame can't leave the
// call looking like a success because the error was still to come.
#[tokio::test]
async fn canceled_eof_still_records_failure() {
    let upstream = Upstream::answering("").await;
    let executor = executor("gemini");
    let auth = auth("gemini", executor.as_ref(), &upstream.url, false);
    let request = Request {
        model: "m".into(),
        payload: Bytes::from_static(TASK6_PATCH_REQUEST.as_bytes()),
    };
    let options = Options {
        stream: true,
        original_request: Bytes::from_static(ORIGINAL.as_bytes()),
        ..Options::new(Format::OPENAI_RESPONSE)
    };
    let response = tokio::time::timeout(WAIT, executor.execute_stream(auth, request, options))
        .await
        .expect("the call didn't start")
        .unwrap_or_else(|error| panic!("the call failed: {error:?}"));
    let mut chunks = response.chunks;
    let first = tokio::time::timeout(WAIT, chunks.next())
        .await
        .expect("the stream didn't end")
        .expect("EOF failure was discarded")
        .unwrap_or_else(|error| panic!("an error before the failure frame: {error:?}"));
    let first = String::from_utf8_lossy(&first).into_owned();
    let failed: Vec<Value> = payloads(&first)
        .into_iter()
        .filter(|event| str_at(event, "type") == "response.failed")
        .collect();
    assert!(failed.len() == 1, "no EOF failure frame: {first}");
    let error = chunks
        .next()
        .now_or_never()
        .expect("the error wasn't ready behind the failure frame")
        .expect("the stream ended without its error")
        .expect_err("cancellation would recover the EOF failure as a success");
    assert_patch_error(&error);
    let (rest, errors) = collect(open_ferry_core::exec::StreamResponse {
        headers: http::HeaderMap::new(),
        chunks,
    })
    .await;
    assert!(
        events(&rest).is_empty() && errors.is_empty(),
        "more after the failure: {rest:?} {errors:?}"
    );
}
