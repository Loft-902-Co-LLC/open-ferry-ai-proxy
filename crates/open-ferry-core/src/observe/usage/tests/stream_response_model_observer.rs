// Ported from CLIProxyAPI
// internal/runtime/executor/helps/stream_response_model_observer_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the stream observer that reads the served model wherever the
//! chunks split it. All of upstream's are ported.
//!
//! Deviations from upstream: the observer keeps the model itself, where
//! upstream's sets it on a usage reporter.

use super::super::response_model::{
    MAX_LINES_PER_STREAM_EVENT, STREAM_MODEL_BUFFER_BOUND, StreamResponseModelObserver,
};

fn observer() -> StreamResponseModelObserver {
    StreamResponseModelObserver::new("openai-compat")
}

/// Ports TestStreamResponseModelObserver_ChunkSplit.
#[test]
fn chunk_split() {
    let mut observer = observer();
    for chunk in [
        r#"data: {"id":"img_1","cre"#,
        r#"ated":1700000000,"mo"#,
        "del\":\"dall-e-3\",\"data\":[]}\n\n",
    ] {
        observer.feed(chunk.as_bytes());
    }
    observer.finish();
    assert_eq!(observer.response_model(), "dall-e-3");
}

/// Ports TestStreamResponseModelObserver_MultiEventChunk.
#[test]
fn multi_event_chunk() {
    let mut observer = observer();
    observer.feed(
        b"event: ping\ndata: {}\n\nevent: progress\ndata: {\"percent\":50}\n\nevent: completion\ndata: {\"model\":\"dall-e-3\",\"status\":\"completed\"}\n\n",
    );
    observer.finish();
    assert_eq!(observer.response_model(), "dall-e-3");
}

/// Ports TestStreamResponseModelObserver_EventPrefix.
#[test]
fn event_prefix() {
    let mut observer = observer();
    observer.feed(b"event: image_generation\n");
    observer.feed(b"data: {\"model\":\"flux-pro\"}\n\n");
    observer.finish();
    assert_eq!(observer.response_model(), "flux-pro");
}

/// Ports TestStreamResponseModelObserver_MultiLineDataEvent.
#[test]
fn multi_line_data_event() {
    let mut observer = observer();
    observer.feed(b"event: message\ndata: {\ndata: \"model\": \"dall-e-3\"\ndata: }\n\n");
    observer.finish();
    assert_eq!(observer.response_model(), "dall-e-3");
}

/// Ports TestStreamResponseModelObserver_OverflowProtection.
#[test]
fn overflow_protection() {
    let mut observer = observer();
    let huge = format!("data: {}", "A".repeat(STREAM_MODEL_BUFFER_BOUND + 1000));
    observer.feed(huge.as_bytes());
    observer.feed(b"\n\nevent: result\ndata: {\"model\":\"dall-e-3\"}\n\n");
    observer.finish();
    assert_eq!(observer.response_model(), "dall-e-3");
}

/// Ports TestStreamResponseModelObserver_FinishFlushesIncompleteLine.
#[test]
fn finish_flushes_incomplete_line() {
    let mut observer = observer();
    observer.feed(br#"{"created":123,"mo"#);
    observer.feed(br#"del":"dall-e-3"}"#);
    observer.finish();
    assert_eq!(observer.response_model(), "dall-e-3");
}

/// Ports TestStreamResponseModelObserver_RepeatedEmptyDataBounded.
#[test]
fn repeated_empty_data_bounded() {
    let mut observer = observer();
    let chunk = "data:\n".repeat(100);
    for _ in 0..50 {
        observer.feed(chunk.as_bytes());
    }
    assert!(observer.frame_len() <= MAX_LINES_PER_STREAM_EVENT);
    assert_eq!(observer.frame_len(), 0);
    observer.feed(b"\n\nevent: completion\ndata: {\"model\":\"dall-e-3\"}\n\n");
    observer.finish();
    assert_eq!(observer.response_model(), "dall-e-3");
}

/// Ports TestStreamResponseModelObserver_EventOverflowDropsEventUntilBoundary.
#[test]
fn event_overflow_drops_event_until_boundary() {
    let mut observer = observer();
    let line = format!("data: {}\n", "x".repeat(1024));
    for _ in 0..100 {
        observer.feed(line.as_bytes());
    }
    assert_eq!(observer.frame_len(), 0);
    observer.feed(b"\n\ndata: {\"model\":\"dall-e-3\"}\n\n");
    observer.finish();
    assert_eq!(observer.response_model(), "dall-e-3");
}
