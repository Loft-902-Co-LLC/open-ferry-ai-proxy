// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_sse_events_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Split SSE events keep their physical-line budgets and every fragment
//! across bootstrap release. The clock and body writer are the parent's seams.

use super::*;

/// Hold the rest of the event until the executor releases bootstrap.
async fn release_before_tail(
    executor: &CodexExecutor,
    prefix: String,
    tail: String,
) -> StreamResponse {
    let resume = Arc::new(Notify::new());
    let server_resume = Arc::clone(&resume);
    let url = serve(200, move |writer| {
        let (prefix, tail, resume) = (prefix.clone(), tail.clone(), Arc::clone(&server_resume));
        async move {
            writer.write(&prefix);
            resume.notified().await;
            writer.write(&tail);
        }
    })
    .await;
    let response = tokio::time::timeout(Duration::from_secs(5), start(executor, &url))
        .await
        .expect("bootstrap must release before reading the event's tail");
    resume.notify_one();
    started(response)
}

fn in_progress(body: &str) -> Value {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["type"] == "response.in_progress")
        .expect("one valid assembled in_progress event")
}

// Ported from TestCodexExecuteStreamEventExceedsBudgetWhileAssemblingReleasesStream.
#[tokio::test]
async fn event_exceeds_line_budget_while_assembling() {
    let (logs, _capture) = crate::secret_echo::Logs::capture();
    let prefix = format!(
        "event: response.created\ndata: {CREATED_EVENT}\n\ndata: {{\"type\":\"response.in_progress\",\"response\":{{\"items\":[\n{}data: {{\"key_at_boundary\":123}},\n",
        "data: \n".repeat(43)
    );
    let tail = format!(
        "data: {{\"key_after_boundary\":456}}\ndata: ]}}}}\nevent: error\ndata: {OVERLOAD_EVENT}\n\n"
    );
    let response = release_before_tail(&buffering(true), prefix, tail).await;
    let (body, error) = drain(response).await;
    assert!(error.is_some(), "the overload must arrive in-stream");
    let event = in_progress(&body);
    assert_eq!(event["response"]["items"][0]["key_at_boundary"], 123);
    assert_eq!(event["response"]["items"][1]["key_after_boundary"], 456);
    assert!(
        logs.text().contains("bootstrap frame budget exhausted"),
        "{}",
        logs.text()
    );
}

// Ported from TestCodexExecuteStreamEventExceedsTimeBudgetWhileAssemblingReleasesStream.
#[tokio::test]
async fn event_exceeds_time_budget_while_assembling() {
    let (logs, _capture) = crate::secret_echo::Logs::capture();
    let clock = MockClock::new();
    let server_clock = clock.clone();
    let header_read = Arc::new(Notify::new());
    let server_header_read = Arc::clone(&header_read);
    let executor_clock = clock.clone();
    let reads = std::sync::atomic::AtomicUsize::new(0);
    let executor = timed("10s", &clock).with_bootstrap_clock(Arc::new(move || {
        let now = executor_clock.now();
        // Not upstream's: synchronize after the first held-line check, so
        // the timeout cannot accidentally release an earlier created frame.
        // The start and three created-event lines consume the first four reads.
        if reads.fetch_add(1, Ordering::SeqCst) == 4 {
            header_read.notify_one();
        }
        now
    }));
    let resume = Arc::new(Notify::new());
    let server_resume = Arc::clone(&resume);
    let url = serve(200, move |writer| {
        let (clock, resume, header_read) = (
            server_clock.clone(), Arc::clone(&server_resume), Arc::clone(&server_header_read));
        async move {
            writer.write(&format!("event: response.created\ndata: {CREATED_EVENT}\n\ndata: {{\"type\":\"response.in_progress\",\"response\":{{\"items\":[\n"));
            header_read.notified().await;
            clock.advance(Duration::from_secs(11));
            writer.write("data: {\"t_item\":1,\"key_at_boundary\":123},\n");
            resume.notified().await;
            writer.write(&format!("data: {{\"key_after_boundary\":456}}\ndata: ]}}}}\n\nevent: error\ndata: {OVERLOAD_EVENT}\n\n"));
        }
    }).await;
    let response = tokio::time::timeout(Duration::from_secs(5), start(&executor, &url))
        .await
        .expect("time budget must release during assembly");
    resume.notify_one();
    let (body, error) = drain(started(response)).await;
    assert!(error.is_some(), "the overload must arrive in-stream");
    let event = in_progress(&body);
    assert_eq!(event["response"]["items"][0]["t_item"], 1);
    assert_eq!(event["response"]["items"][0]["key_at_boundary"], 123);
    assert_eq!(event["response"]["items"][1]["key_after_boundary"], 456);
    assert!(
        logs.text()
            .contains("bootstrap time budget exhausted after 3 lines /"),
        "{}",
        logs.text()
    );
}

// Ported from TestCodexExecuteStreamSplitCompletedAfterOutputAndClockAdvance.
#[tokio::test]
async fn split_completed_after_output_and_clock_advance() {
    let clock = MockClock::new();
    let resume = Arc::new(Notify::new());
    let server_resume = Arc::clone(&resume);
    let url = serve(200, move |writer| {
        let resume = Arc::clone(&server_resume);
        async move {
            writer.write(&format!("event: response.created\ndata: {CREATED_EVENT}\n\nevent: response.output_text.delta\ndata: {OUTPUT_DELTA_EVENT}\n\n"));
            resume.notified().await;
            let completed = COMPLETED_EVENT.replace("resp_1", "resp_clock");
            let cut = completed.find("\"output\":").unwrap();
            writer.write(&format!("event: response.completed\ndata: {}\ndata: {}\n\n", &completed[..cut], &completed[cut..]));
        }
    }).await;
    let mut response = started(start(&timed("10s", &clock), &url).await);
    loop {
        let chunk = response
            .chunks
            .next()
            .await
            .expect("output before completion")
            .unwrap();
        if String::from_utf8_lossy(&chunk).contains("response.output_text.delta") {
            break;
        }
    }
    clock.advance(Duration::from_secs(20));
    resume.notify_one();
    let (body, error) = drain(response).await;
    assert!(error.is_none(), "{error:?}");
    let completed = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("valid split completion");
    assert_eq!(completed["response"]["id"], "resp_clock");
    assert_eq!(completed["response"]["usage"]["total_tokens"], 2);
}

// Ported from TestCodexExecuteStreamMixedFramingCommentInsideSplitEventCountsBudget.
#[tokio::test]
async fn mixed_framing_comments_count_toward_budget() {
    let (logs, _capture) = crate::secret_echo::Logs::capture();
    let prefix = format!(
        "data: {{\"type\":\"response.in_progress\",\"items\":[\n{}: keepalive-1\n: keepalive-2\n",
        "data: \n".repeat(46)
    );
    let tail = format!("event: error\ndata: {OVERLOAD_EVENT}\n\ndata: ]}}}}\n\n");
    let response = release_before_tail(&buffering(true), prefix, tail).await;
    let (_, error) = drain(response).await;
    assert!(error.is_some(), "the stream must release before the error");
    // Not upstream's: the second comment takes buffered + held + raw above
    // 48, while buffered alone is only one. Pin the frame-budget label.
    assert!(
        logs.text()
            .contains("bootstrap frame budget exhausted after 1 lines /"),
        "{}",
        logs.text()
    );
}

// Ported from TestCodexExecuteStreamEventExceedsByteBudgetWhileAssemblingReleasesStream.
#[tokio::test]
async fn event_exceeds_byte_budget_while_assembling() {
    let (logs, _capture) = crate::secret_echo::Logs::capture();
    let prefix = format!(
        "data: {{\"type\":\"response.in_progress\",\"items\":[\n{}",
        format!("data: \"{}\",\n", "x".repeat(250 * 1024)).repeat(5)
    );
    let tail = format!("data: \"last\"]}}\n\nevent: error\ndata: {OVERLOAD_EVENT}\n\n");
    let response = release_before_tail(&buffering(true), prefix, tail).await;
    let (body, error) = drain(response).await;
    assert!(error.is_some(), "the overload must arrive in-stream");
    let event = in_progress(&body);
    assert_eq!(event["items"].as_array().unwrap().len(), 6);
    for item in &event["items"].as_array().unwrap()[..5] {
        assert_eq!(item.as_str().unwrap().len(), 250 * 1024);
    }
    assert_eq!(event["items"][5], "last");
    assert!(
        logs.text().contains("bootstrap byte budget exhausted"),
        "{}",
        logs.text()
    );
}
