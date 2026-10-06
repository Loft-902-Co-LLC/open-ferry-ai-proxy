//! The usage routes, over a ledger with a few calls in it.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use http::{Method, StatusCode};
use open_ferry_core::observe::usage::{ClientKey, EventCredential};
use serde_json::{Value, json};

use super::{APP, Dash, event, keyed_config, tokens};
use crate::LEDGER_FILE;
use crate::assets::Assets;

const SUMMARY: &str = "/open-ferry/api/v1/usage/summary";
const SERIES: &str = "/open-ferry/api/v1/usage/series";
const REQUESTS: &str = "/open-ferry/api/v1/usage/requests";
const LEDGER: &str = "/open-ferry/api/v1/usage/ledger";
const RECORDS: &str = "/open-ferry/api/v1/usage/records";
const PRICES: &str = "/open-ferry/api/v1/usage/prices";

/// The day the calls are on.
const DAY: &str = "from=2026-10-05T00:00:00Z&to=2026-10-06T00:00:00Z";

/// Two client keys.
const KEY_1: &str = "sk-live-aaaaaaaaaaaaaaaaaaaaaaaa1111";
const KEY_2: &str = "sk-live-bbbbbbbbbbbbbbbbbbbbbbbb2222";

/// A dashboard whose ledger has four calls: on 2026-10-05, two to `gpt-5`
/// of `codex` with one credential and two client keys, and a failed one to
/// `claude-x` of `claude` with neither; and one the day before, to
/// `gemini-2.5-pro` of `gemini`.
fn dash_with_calls() -> Dash {
    let dash = Dash::new();
    let credential = EventCredential {
        id: "codex-a.json".to_owned(),
        auth_index: "1".to_owned(),
        label: "a@example.com".to_owned(),
        auth_type: "oauth".to_owned(),
    };

    let mut first = event("2026-10-05T10:00:00Z", "codex", "gpt-5");
    first.tokens = tokens(1000, 400, 100, 200, 50);
    first.total_tokens = 1200;
    first.credential = Some(credential.clone());
    first.client_key = ClientKey::new(KEY_1);

    let mut second = event("2026-10-05T10:30:00Z", "codex", "gpt-5");
    second.alias = "fast".to_owned();
    second.tokens = tokens(2000, 0, 0, 300, 0);
    second.total_tokens = 2300;
    second.latency = Duration::from_millis(300);
    second.ttft = Some(Duration::from_millis(50));
    second.stream = true;
    second.credential = Some(credential);
    second.client_key = ClientKey::new(KEY_2);

    let mut failed = event("2026-10-05T11:15:00Z", "claude", "claude-x");
    failed.failed = true;
    failed.status = 529;
    failed.latency = Duration::from_millis(200);

    let before = event("2026-10-04T09:00:00Z", "gemini", "gemini-2.5-pro");

    dash.record(&[before, first, second, failed]);
    dash
}

/// The metrics of the calls of 2026-10-05, without prices.
fn day_totals() -> Value {
    json!({
        "requests": 3,
        "errors": 1,
        "input_tokens": 3000,
        "cache_read_tokens": 400,
        "cache_write_tokens": 100,
        "output_tokens": 500,
        "reasoning_tokens": 50,
        "total_tokens": 3500,
        "latency_ms": {"p50": 200, "p95": 300, "p99": 300},
        "ttft_ms": {"p50": 50, "p95": 50, "p99": 50},
        "cost": null,
        "unpriced_requests": 3,
    })
}

/// Metrics with no calls.
fn empty() -> Value {
    json!({
        "requests": 0,
        "errors": 0,
        "input_tokens": 0,
        "cache_read_tokens": 0,
        "cache_write_tokens": 0,
        "output_tokens": 0,
        "reasoning_tokens": 0,
        "total_tokens": 0,
        "latency_ms": null,
        "ttft_ms": null,
        "cost": null,
        "unpriced_requests": 0,
    })
}

/// The client key IDs the ledger gave [`KEY_1`] and [`KEY_2`].
async fn client_key_ids(dash: &Dash) -> (String, String) {
    let page = dash
        .get(&format!("{REQUESTS}?{DAY}&provider=codex"))
        .await
        .json(StatusCode::OK);
    let id = |index: usize| {
        page["requests"][index]["client_key"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    // Newest first: the second call's key, then the first's.
    (id(1), id(0))
}

/// Not upstream's: the summary sums the calls in the range, and with
/// `group_by` per group, most calls first.
#[tokio::test]
async fn the_summary_sums_the_calls() {
    let dash = dash_with_calls();
    let summary = dash
        .get(&format!("{SUMMARY}?{DAY}"))
        .await
        .json(StatusCode::OK);
    assert_eq!(
        summary,
        json!({
            "from": "2026-10-05T00:00:00.000Z",
            "to": "2026-10-06T00:00:00.000Z",
            "currency": "USD",
            "totals": day_totals(),
            "group_by": null,
            "groups": [],
            "more_groups": false,
        })
    );

    let by_model = dash
        .get(&format!("{SUMMARY}?{DAY}&group_by=model"))
        .await
        .json(StatusCode::OK);
    assert_eq!(by_model["group_by"], "model");
    let groups = by_model["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["key"], "gpt-5");
    assert_eq!(groups[0]["label"], "gpt-5");
    assert_eq!(groups[0]["metrics"]["requests"], 2);
    assert_eq!(groups[0]["metrics"]["input_tokens"], 3000);
    assert_eq!(
        groups[0]["metrics"]["latency_ms"],
        json!({"p50": 100, "p95": 300, "p99": 300})
    );
    assert_eq!(groups[1]["key"], "claude-x");
    assert_eq!(groups[1]["metrics"]["errors"], 1);
    assert_eq!(groups[1]["metrics"]["ttft_ms"], Value::Null);
    assert!(groups[0].get("credential").is_none());

    let limited = dash
        .get(&format!("{SUMMARY}?{DAY}&group_by=model&limit=1"))
        .await
        .json(StatusCode::OK);
    assert_eq!(limited["groups"].as_array().unwrap().len(), 1);
    assert_eq!(limited["more_groups"], true);
    assert_eq!(limited["totals"]["requests"], 3);

    // Without a range, the day before now.
    let before = Utc::now();
    let recent = dash.get(SUMMARY).await.json(StatusCode::OK);
    let time = |name: &str| {
        DateTime::parse_from_rfc3339(recent[name].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc)
    };
    let (from, to) = (time("from"), time("to"));
    assert_eq!(to - from, TimeDelta::hours(24));
    assert!(
        to >= before - TimeDelta::milliseconds(1) && to <= Utc::now(),
        "{to}"
    );
}

/// Not upstream's: a credential group is labelled with its label and
/// carries the credential; a client-key group with the key masked; the
/// calls without either are the group keyed `""`.
#[tokio::test]
async fn credential_and_client_key_groups() {
    let dash = dash_with_calls();
    let by_credential = dash
        .get(&format!("{SUMMARY}?{DAY}&group_by=credential"))
        .await
        .json(StatusCode::OK);
    let groups = by_credential["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["key"], "codex-a.json");
    assert_eq!(groups[0]["label"], "a@example.com");
    assert_eq!(
        groups[0]["credential"],
        json!({"id": "codex-a.json", "auth_index": "1", "label": "a@example.com", "auth_type": "oauth"})
    );
    assert_eq!(groups[1]["key"], "");
    assert_eq!(groups[1]["label"], "");
    assert_eq!(groups[1]["credential"], Value::Null);
    assert!(groups[0].get("client_key").is_none());

    let (first, second) = client_key_ids(&dash).await;
    assert_ne!(first, second);
    let by_key = dash
        .get(&format!("{SUMMARY}?{DAY}&group_by=client_key"))
        .await
        .json(StatusCode::OK);
    let groups = by_key["groups"].as_array().unwrap();
    // One call each: by key, so the calls without one first.
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0]["key"], "");
    assert_eq!(groups[0]["label"], "");
    assert_eq!(groups[0]["client_key"], Value::Null);
    let mut keyed: Vec<(String, Value)> = groups[1..]
        .iter()
        .map(|group| (group["key"].as_str().unwrap().to_owned(), group.clone()))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![(first, "sk-...1111"), (second, "sk-...2222")];
    expected.sort();
    for ((key, group), (id, masked)) in keyed.iter().zip(&expected) {
        assert_eq!(key, id);
        assert_eq!(group["label"], *masked);
        assert_eq!(group["client_key"], json!({"id": id, "masked": masked}));
    }

    // The keys filter as they group.
    let first_only = dash
        .get(&format!("{SUMMARY}?{DAY}&client_key={}", expected[0].0))
        .await
        .json(StatusCode::OK);
    assert_eq!(first_only["totals"]["requests"], 1);
    let without = dash
        .get(&format!("{SUMMARY}?{DAY}&client_key=&credential="))
        .await
        .json(StatusCode::OK);
    assert_eq!(without["totals"]["requests"], 1);
    assert_eq!(without["totals"]["errors"], 1);
    let codex = dash
        .get(&format!(
            "{SUMMARY}?{DAY}&provider=codex&credential=codex-a.json&model=gpt-5"
        ))
        .await
        .json(StatusCode::OK);
    assert_eq!(codex["totals"]["requests"], 2);
}

/// Not upstream's: a series has every bucket of the range in order, empty
/// ones with zeros, and `auto` takes the smallest bucket giving at most
/// 200 of them.
#[tokio::test]
async fn series_have_every_bucket() {
    let dash = dash_with_calls();
    let range = "from=2026-10-05T10:00:00Z&to=2026-10-05T12:00:00Z";
    let hourly = dash
        .get(&format!("{SERIES}?{range}&bucket=hour"))
        .await
        .json(StatusCode::OK);
    assert_eq!(hourly["bucket"], "hour");
    assert_eq!(hourly["bucket_seconds"], 3600);
    assert_eq!(hourly["currency"], "USD");
    assert_eq!(hourly["group_by"], Value::Null);
    assert_eq!(hourly["more_groups"], false);
    let series = hourly["series"].as_array().unwrap();
    assert_eq!(series.len(), 1);
    assert_eq!(series[0]["key"], Value::Null);
    assert_eq!(series[0]["label"], Value::Null);
    let points = series[0]["points"].as_array().unwrap();
    assert_eq!(points.len(), 2);
    assert_eq!(points[0]["start"], "2026-10-05T10:00:00.000Z");
    assert_eq!(points[0]["metrics"]["requests"], 2);
    assert_eq!(points[1]["start"], "2026-10-05T11:00:00.000Z");
    assert_eq!(points[1]["metrics"]["requests"], 1);

    let auto = dash
        .get(&format!("{SERIES}?{range}"))
        .await
        .json(StatusCode::OK);
    assert_eq!(auto["bucket"], "minute");
    assert_eq!(auto["bucket_seconds"], 60);
    let points = auto["series"][0]["points"].as_array().unwrap();
    assert_eq!(points.len(), 120);
    for (index, point) in points.iter().enumerate() {
        let requests = if [0, 30, 75].contains(&index) { 1 } else { 0 };
        assert_eq!(point["metrics"]["requests"], requests, "{index}");
    }
    assert_eq!(points[1]["metrics"], empty());
    assert_eq!(points[75]["start"], "2026-10-05T11:15:00.000Z");

    let weeks = dash
        .get(&format!(
            "{SERIES}?from=2026-09-01T00:00:00Z&to=2026-10-06T00:00:00Z"
        ))
        .await
        .json(StatusCode::OK);
    assert_eq!(weeks["bucket"], "day");
    assert_eq!(weeks["series"][0]["points"].as_array().unwrap().len(), 35);
}

/// Not upstream's: day buckets start at midnight at `utc_offset`, the
/// first may start before the range, and a range of over 1,500 buckets is
/// refused.
#[tokio::test]
async fn series_follow_the_offset() {
    let dash = dash_with_calls();
    let days = dash
        .get(&format!(
            "{SERIES}?from=2026-10-04T00:00:00Z&to=2026-10-06T00:00:00Z&bucket=day&utc_offset=-300"
        ))
        .await
        .json(StatusCode::OK);
    let points = days["series"][0]["points"].as_array().unwrap();
    let starts: Vec<&str> = points
        .iter()
        .map(|point| point["start"].as_str().unwrap())
        .collect();
    assert_eq!(
        starts,
        [
            "2026-10-03T05:00:00.000Z",
            "2026-10-04T05:00:00.000Z",
            "2026-10-05T05:00:00.000Z",
        ]
    );
    let requests: Vec<&Value> = points
        .iter()
        .map(|point| &point["metrics"]["requests"])
        .collect();
    assert_eq!(requests, [&json!(0), &json!(1), &json!(3)]);

    for (query, needle) in [
        (
            "from=2026-10-01T00:00:00Z&to=2026-10-05T00:00:00Z&bucket=minute",
            "over 1500 buckets",
        ),
        (
            "from=2020-01-01T00:00:00Z&to=2026-01-01T00:00:00Z",
            "over 1500 buckets",
        ),
        ("bucket=week", "bucket must be"),
        ("utc_offset=900", "utc_offset must be"),
        ("group_by=model&groups=21", "groups must be"),
    ] {
        let message = dash
            .get(&format!("{SERIES}?{query}"))
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_request");
        assert!(message.contains(needle), "{query}: {message}");
    }
}

/// Not upstream's: with `group_by`, a series per group, the groups with
/// the most calls.
#[tokio::test]
async fn grouped_series() {
    let dash = dash_with_calls();
    let grouped = dash
        .get(&format!(
            "{SERIES}?{DAY}&bucket=day&group_by=provider&groups=1"
        ))
        .await
        .json(StatusCode::OK);
    assert_eq!(grouped["group_by"], "provider");
    assert_eq!(grouped["more_groups"], true);
    let series = grouped["series"].as_array().unwrap();
    assert_eq!(series.len(), 1);
    assert_eq!(series[0]["key"], "codex");
    assert_eq!(series[0]["label"], "codex");
    assert_eq!(series[0]["points"][0]["metrics"]["requests"], 2);

    let by_credential = dash
        .get(&format!("{SERIES}?{DAY}&bucket=day&group_by=credential"))
        .await
        .json(StatusCode::OK);
    let series = by_credential["series"].as_array().unwrap();
    assert_eq!(series.len(), 2);
    assert_eq!(series[0]["label"], "a@example.com");
    assert_eq!(series[0]["credential"]["auth_index"], "1");
    assert_eq!(series[1]["key"], "");
    assert_eq!(series[1]["points"][0]["metrics"]["requests"], 1);
}

/// Not upstream's: the calls are listed newest first, a page at a time,
/// and filtered by outcome and request ID.
#[tokio::test]
async fn calls_are_listed_and_paged() {
    let dash = dash_with_calls();
    let page = dash
        .get(&format!("{REQUESTS}?{DAY}&limit=2"))
        .await
        .json(StatusCode::OK);
    let calls = page["requests"].as_array().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["model"], "claude-x");
    assert_eq!(calls[0]["failed"], true);
    assert_eq!(calls[0]["status"], 529);
    assert_eq!(calls[0]["credential"], Value::Null);
    assert_eq!(calls[0]["client_key"], Value::Null);
    assert_eq!(calls[0]["cost"], Value::Null);

    let second = &calls[1];
    let id = second["id"].as_i64().unwrap();
    let key = second["client_key"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        *second,
        json!({
            "id": id,
            "time": "2026-10-05T10:30:00.000Z",
            "request_id": "req-2026-10-05T10:30:00Z",
            "endpoint": "POST /v1/chat/completions",
            "provider": "codex",
            "model": "gpt-5",
            "alias": "fast",
            "stream": true,
            "failed": false,
            "status": 200,
            "latency_ms": 300,
            "ttft_ms": 50,
            "credential": {"id": "codex-a.json", "auth_index": "1", "label": "a@example.com", "auth_type": "oauth"},
            "client_key": {"id": key, "masked": "sk-...2222"},
            "tokens": {"input": 2000, "cache_read": 0, "cache_write": 0, "output": 300, "reasoning": 0, "total": 2300},
            "cost": null,
        })
    );

    let cursor = page["next_cursor"].as_str().unwrap();
    let next = dash
        .get(&format!("{REQUESTS}?{DAY}&limit=2&cursor={cursor}"))
        .await
        .json(StatusCode::OK);
    let calls = next["requests"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["time"], "2026-10-05T10:00:00.000Z");
    assert_eq!(calls[0]["ttft_ms"], Value::Null);
    assert_eq!(next["next_cursor"], Value::Null);

    let everything = dash
        .get(&format!(
            "{REQUESTS}?from=2026-01-01T00:00:00Z&to=2026-10-06T00:00:00Z"
        ))
        .await
        .json(StatusCode::OK);
    assert_eq!(everything["requests"].as_array().unwrap().len(), 4);
    assert_eq!(everything["next_cursor"], Value::Null);

    for (query, count) in [
        ("failed=true", 1),
        ("failed=false", 2),
        ("request_id=req-2026-10-05T10:00:00Z", 1),
        ("request_id=req-none", 0),
        ("model=gpt-5&failed=true", 0),
    ] {
        let page = dash
            .get(&format!("{REQUESTS}?{DAY}&{query}"))
            .await
            .json(StatusCode::OK);
        assert_eq!(page["requests"].as_array().unwrap().len(), count, "{query}");
    }

    dash.get(&format!("{REQUESTS}?cursor=not-a-cursor"))
        .await
        .error(StatusCode::BAD_REQUEST, "invalid_cursor");
    let message = dash
        .get(&format!("{REQUESTS}?failed=maybe"))
        .await
        .error(StatusCode::BAD_REQUEST, "invalid_request");
    assert!(message.contains("failed"), "{message}");
    dash.get(&format!("{REQUESTS}?limit=501"))
        .await
        .error(StatusCode::BAD_REQUEST, "invalid_request");
}

/// Not upstream's: the ledger's state, and its settings changed and
/// checked.
#[tokio::test]
async fn the_ledger_has_a_state_and_settings() {
    let dash = dash_with_calls();
    let state = dash.get(LEDGER).await.json(StatusCode::OK);
    let file = dash.logs().join(LEDGER_FILE).display().to_string();
    assert_eq!(state["file"], file.as_str());
    assert!(state["size_bytes"].as_u64().unwrap() > 0);
    let mut expected = state.clone();
    for (field, value) in [
        ("available", json!(true)),
        ("unavailable_reason", Value::Null),
        ("recording", json!(true)),
        ("usage_statistics_enabled", json!(true)),
        ("rows", json!(4)),
        ("oldest", json!("2026-10-04T09:00:00.000Z")),
        ("newest", json!("2026-10-05T11:15:00.000Z")),
        ("retention_days", json!(90)),
        ("max_rows", json!(1_000_000)),
        ("currency", json!("USD")),
        ("dropped_records", json!(0)),
    ] {
        expected[field] = value;
    }
    assert_eq!(state, expected);

    let changed = dash
        .call(
            Method::PATCH,
            LEDGER,
            r#"{"retention_days": 30, "currency": "EUR"}"#,
        )
        .await
        .json(StatusCode::OK);
    assert_eq!(changed["retention_days"], 30);
    assert_eq!(changed["max_rows"], 1_000_000);
    assert_eq!(changed["currency"], "EUR");
    let changed = dash
        .call(Method::PATCH, LEDGER, r#"{"max_rows": 10000}"#)
        .await
        .json(StatusCode::OK);
    assert_eq!(changed["retention_days"], 30);
    assert_eq!(changed["max_rows"], 10_000);
    assert_eq!(dash.get(LEDGER).await.json(StatusCode::OK), changed);
    let summary = dash
        .get(&format!("{SUMMARY}?{DAY}"))
        .await
        .json(StatusCode::OK);
    assert_eq!(summary["currency"], "EUR");

    for (body, needle) in [
        (r#"{"retention_days": 0}"#, "retention_days"),
        (r#"{"retention_days": 3651}"#, "retention_days"),
        (r#"{"max_rows": 9999}"#, "max_rows"),
        (r#"{"max_rows": 10000001}"#, "max_rows"),
        (r#"{"currency": ""}"#, "currency"),
        (r#"{"currency": "US$"}"#, "currency"),
        (r#"{"currency": "ABCDEFGHI"}"#, "currency"),
    ] {
        let message = dash
            .call(Method::PATCH, LEDGER, body)
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_request");
        assert!(message.contains(needle), "{body}: {message}");
    }
    assert_eq!(dash.get(LEDGER).await.json(StatusCode::OK), changed);
}

/// Not upstream's: while `usage-statistics-enabled` is off, the ledger is
/// open but records nothing.
#[tokio::test]
async fn the_ledger_records_only_with_statistics_on() {
    let mut config = keyed_config();
    config.usage_statistics_enabled = false;
    let dash = Dash::with_config(config);
    let state = dash.get(LEDGER).await.json(StatusCode::OK);
    assert_eq!(state["available"], true);
    assert_eq!(state["recording"], false);
    assert_eq!(state["usage_statistics_enabled"], false);
    assert_eq!(state["rows"], 0);
    assert_eq!(state["oldest"], Value::Null);
    assert_eq!(state["newest"], Value::Null);
}

/// Not upstream's: a ledger that couldn't be opened says why, and every
/// other usage route answers `ledger_unavailable`.
#[tokio::test]
async fn an_unavailable_ledger() {
    let dash = Dash::build(keyed_config(), Assets::fixture(APP), false);
    let state = dash.get(LEDGER).await.json(StatusCode::OK);
    assert_eq!(
        state,
        json!({
            "available": false,
            "unavailable_reason": "the test has none",
            "recording": false,
            "usage_statistics_enabled": true,
            "file": null,
            "size_bytes": null,
            "rows": null,
            "oldest": null,
            "newest": null,
            "retention_days": null,
            "max_rows": null,
            "currency": null,
            "dropped_records": null,
        })
    );
    for (method, path, body) in [
        (Method::GET, SUMMARY, ""),
        (Method::GET, SERIES, ""),
        (Method::GET, REQUESTS, ""),
        (Method::PATCH, LEDGER, r#"{"retention_days": 30}"#),
        (Method::DELETE, RECORDS, ""),
        (Method::GET, PRICES, ""),
        (
            Method::PUT,
            PRICES,
            r#"{"model": "gpt-5", "input": 1, "output": 2}"#,
        ),
        (
            Method::DELETE,
            "/open-ferry/api/v1/usage/prices?model=gpt-5",
            "",
        ),
    ] {
        let message = dash
            .call(method.clone(), path, body)
            .await
            .error(StatusCode::SERVICE_UNAVAILABLE, "ledger_unavailable");
        assert!(message.contains("the test has none"), "{method} {path}");
    }
}

/// Not upstream's: prices are set, replaced and removed; costs are worked
/// out from them when asked, a missing cache price being the input price;
/// the models without one are listed.
#[tokio::test]
async fn prices_make_costs() {
    let dash = dash_with_calls();
    let prices = dash.get(PRICES).await.json(StatusCode::OK);
    assert_eq!(
        prices,
        json!({
            "currency": "USD",
            "prices": [],
            "unpriced_models": ["claude-x", "gemini-2.5-pro", "gpt-5"],
        })
    );

    let entry = dash
        .call(
            Method::PUT,
            PRICES,
            r#"{"model": "gpt-5", "input": 1.25, "cache_read": 0.125, "output": 10}"#,
        )
        .await
        .json(StatusCode::OK);
    let updated = entry["updated"].as_str().unwrap().to_owned();
    assert!(updated.ends_with('Z'), "{updated}");
    assert_eq!(
        entry,
        json!({
            "model": "gpt-5",
            "input": 1.25,
            "cache_read": 0.125,
            "cache_write": null,
            "output": 10.0,
            "updated": updated,
        })
    );
    let prices = dash.get(PRICES).await.json(StatusCode::OK);
    assert_eq!(prices["prices"], json!([entry]));
    assert_eq!(
        prices["unpriced_models"],
        json!(["claude-x", "gemini-2.5-pro"])
    );

    // The first call: 500 uncached, 400 read and 100 written (at the input
    // price) and 200 out; the second 2000 in and 300 out.
    let first = (500.0 * 1.25 + 400.0 * 0.125 + 100.0 * 1.25 + 200.0 * 10.0) / 1e6;
    let second = (2000.0 * 1.25 + 300.0 * 10.0) / 1e6;
    let close = |value: &Value, expected: f64| {
        let value = value.as_f64().unwrap();
        assert!((value - expected).abs() < 1e-12, "{value} != {expected}");
    };
    let summary = dash
        .get(&format!("{SUMMARY}?{DAY}&group_by=model"))
        .await
        .json(StatusCode::OK);
    close(&summary["totals"]["cost"], first + second);
    assert_eq!(summary["totals"]["unpriced_requests"], 1);
    close(&summary["groups"][0]["metrics"]["cost"], first + second);
    assert_eq!(summary["groups"][0]["metrics"]["unpriced_requests"], 0);
    assert_eq!(summary["groups"][1]["metrics"]["cost"], Value::Null);
    let calls = dash
        .get(&format!("{REQUESTS}?{DAY}&model=gpt-5"))
        .await
        .json(StatusCode::OK);
    close(&calls["requests"][0]["cost"], second);
    close(&calls["requests"][1]["cost"], first);

    // Replaced, the new price applies to past calls too.
    dash.call(
        Method::PUT,
        PRICES,
        r#"{"model": "gpt-5", "input": 2, "cache_read": null, "output": 0}"#,
    )
    .await
    .json(StatusCode::OK);
    let summary = dash
        .get(&format!("{SUMMARY}?{DAY}&model=gpt-5"))
        .await
        .json(StatusCode::OK);
    close(&summary["totals"]["cost"], 3000.0 * 2.0 / 1e6);

    for body in [
        r#"{"model": "", "input": 1, "output": 1}"#,
        r#"{"model": "m", "input": -1, "output": 1}"#,
        r#"{"model": "m", "input": 1, "output": 1000001}"#,
        r#"{"model": "m", "input": 1, "cache_write": -0.5, "output": 1}"#,
        r#"{"model": "m", "input": 1}"#,
        r#"{"model": "m", "input": 1, "output": 1, "currency": "EUR"}"#,
    ] {
        dash.call(Method::PUT, PRICES, body)
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let long = format!(
        r#"{{"model": "{}", "input": 1, "output": 1}}"#,
        "m".repeat(257)
    );
    dash.call(Method::PUT, PRICES, &long)
        .await
        .error(StatusCode::BAD_REQUEST, "invalid_request");
    let longest = format!(
        r#"{{"model": "{}", "input": 1, "output": 1}}"#,
        "é".repeat(256)
    );
    dash.call(Method::PUT, PRICES, &longest)
        .await
        .json(StatusCode::OK);

    let delete = format!("{PRICES}?model=gpt-5");
    let deleted = dash
        .call(Method::DELETE, &delete, "")
        .await
        .json(StatusCode::OK);
    assert_eq!(deleted, json!({"deleted": true}));
    let deleted = dash
        .call(Method::DELETE, &delete, "")
        .await
        .json(StatusCode::OK);
    assert_eq!(deleted, json!({"deleted": false}));
    dash.call(Method::DELETE, PRICES, "")
        .await
        .error(StatusCode::BAD_REQUEST, "invalid_request");
    let summary = dash
        .get(&format!("{SUMMARY}?{DAY}"))
        .await
        .json(StatusCode::OK);
    assert_eq!(summary["totals"]["cost"], Value::Null);
}

/// Not upstream's: deleting the records deletes every row, and keeps the
/// settings and prices.
#[tokio::test]
async fn records_are_deleted() {
    let dash = dash_with_calls();
    dash.call(Method::PATCH, LEDGER, r#"{"currency": "EUR"}"#)
        .await
        .json(StatusCode::OK);
    dash.call(
        Method::PUT,
        PRICES,
        r#"{"model": "gpt-5", "input": 1, "output": 1}"#,
    )
    .await
    .json(StatusCode::OK);

    let deleted = dash
        .call(Method::DELETE, RECORDS, "")
        .await
        .json(StatusCode::OK);
    assert_eq!(deleted, json!({"deleted": 4}));
    let state = dash.get(LEDGER).await.json(StatusCode::OK);
    assert_eq!(state["rows"], 0);
    assert_eq!(state["oldest"], Value::Null);
    assert_eq!(state["currency"], "EUR");
    let prices = dash.get(PRICES).await.json(StatusCode::OK);
    assert_eq!(prices["prices"][0]["model"], "gpt-5");
    assert_eq!(prices["unpriced_models"], json!([]));
    let summary = dash
        .get(&format!("{SUMMARY}?{DAY}"))
        .await
        .json(StatusCode::OK);
    assert_eq!(summary["totals"], empty());
    let deleted = dash
        .call(Method::DELETE, RECORDS, "")
        .await
        .json(StatusCode::OK);
    assert_eq!(deleted, json!({"deleted": 0}));
}
