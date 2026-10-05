// Ported from CLIProxyAPI internal/api/handlers/management/usage_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The usage queue route.
//!
//! Deviations from upstream: the tests drive the router with the key, and
//! each has its own queue, turned on as the binary turns it on while the
//! management API is available.

use bytes::Bytes;
use http::StatusCode;
use open_ferry_core::config::Config;
use open_ferry_core::observe::usage::{Usage, reconfigure};
use serde_json::{Value, json};

use super::Api;

/// The API and its usage statistics, with the queue on and `records`
/// queued.
fn api_with_queue(records: &[&str]) -> (Api, Usage) {
    let api = Api::new();
    let usage = api.state.observability().usage.clone();
    reconfigure(&usage, None, &Config::default(), true);
    for record in records {
        usage.enqueue(Bytes::copy_from_slice(record.as_bytes()));
    }
    (api, usage)
}

/// What is left in `usage`'s queue.
fn remaining(usage: &Usage) -> Vec<Bytes> {
    usage.pop_oldest(10)
}

/// Ports TestGetUsageQueuePopsRequestedRecords.
#[tokio::test]
async fn usage_queue_pops_requested_records() {
    let (api, usage) = api_with_queue(&[r#"{"id":1}"#, r#"{"id":2}"#, r#"{"id":3}"#]);

    let payload = api
        .get("/v0/management/usage-queue?count=2")
        .await
        .expect(StatusCode::OK);
    assert_eq!(payload, json!([{ "id": 1 }, { "id": 2 }]));
    assert_eq!(remaining(&usage), [Bytes::from_static(br#"{"id":3}"#)]);
}

/// Ports TestGetUsageQueueInvalidCountDoesNotPop.
#[tokio::test]
async fn usage_queue_invalid_count_does_not_pop() {
    let (api, usage) = api_with_queue(&[r#"{"id":1}"#]);

    api.get("/v0/management/usage-queue?count=0").await.assert(
        StatusCode::BAD_REQUEST,
        r#"{"error":"count must be a positive integer"}"#,
    );
    assert_eq!(remaining(&usage), [Bytes::from_static(br#"{"id":1}"#)]);
}

/// Not upstream's: a blank `count` takes one record, white space around it
/// is ignored, a sign is allowed, and anything else that isn't a positive
/// integer is refused; the v8 path answers the same.
#[tokio::test]
async fn usage_queue_count_is_read_as_go_reads_it() {
    let (api, usage) = api_with_queue(&["1", "2", "3", "4", "5", "6"]);
    let ids = |payload: Value| payload.as_array().map(Vec::len).unwrap_or_default();

    let one = api.get("/v0/management/usage-queue").await;
    assert_eq!(one.expect(StatusCode::OK), json!([1]));
    assert_eq!(
        one.header("content-type"),
        Some("application/json; charset=utf-8")
    );
    let blank = api.get("/v0/management/usage-queue?count=%20").await;
    assert_eq!(blank.expect(StatusCode::OK), json!([2]));
    let spaced = api.get("/v0/management/usage-queue?count=+2+").await;
    assert_eq!(spaced.expect(StatusCode::OK), json!([3, 4]));
    let signed = api
        .get("/v8/management/observability/usage/queue?count=%2B1")
        .await;
    assert_eq!(ids(signed.expect(StatusCode::OK)), 1);

    for count in ["-1", "abc", "1.5", "0x2", "99999999999999999999", "%FF"] {
        api.get(&format!("/v0/management/usage-queue?count={count}"))
            .await
            .assert(
                StatusCode::BAD_REQUEST,
                r#"{"error":"count must be a positive integer"}"#,
            );
    }
    let all = api
        .get("/v0/management/usage-queue?count=99")
        .await
        .expect(StatusCode::OK);
    assert_eq!(all, json!([6]));
    assert!(remaining(&usage).is_empty());
}

/// Not upstream's: a record that is valid JSON is written compacted and
/// escaped for HTML, as gin writes upstream's records; any other is written
/// as a JSON string; an empty queue, or one that is off, answers `[]`.
#[tokio::test]
async fn usage_queue_records_are_written_as_go_writes_them() {
    let (api, usage) = api_with_queue(&[
        "{ \"a\" : \"<b>&\u{2028}\" ,\n \"n\" : [1, 2] }",
        "not json <x>",
    ]);

    let answer = api.get("/v0/management/usage-queue?count=5").await;
    answer.assert(
        StatusCode::OK,
        concat!(
            r#"[{"a":""#,
            "\\u003cb\\u003e\\u0026\\u2028",
            r#"","n":[1,2]},"not json "#,
            "\\u003cx\\u003e",
            r#""]"#,
        ),
    );

    api.get("/v0/management/usage-queue")
        .await
        .assert(StatusCode::OK, "[]");
    reconfigure(&usage, None, &Config::default(), false);
    usage.enqueue(Bytes::from_static(b"{}"));
    api.get("/v0/management/usage-queue")
        .await
        .assert(StatusCode::OK, "[]");
}
