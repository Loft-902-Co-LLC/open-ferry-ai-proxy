//! Tests of the usage observer, which the usage ledger reads. Not
//! upstream's: the observer is open-ferry's own.

use std::sync::Arc;
use std::sync::mpsc::TryRecvError;

use super::super::{OBSERVER_BUFFER, UsageEvent};
use super::support::{ClientCall, Harness, auth};
use crate::auth::Auth;
use crate::exec::ExecError;
use crate::observe::{AttemptKind, Outcome};

/// An OpenAI answer counting 10 tokens in, 4 of them cached, and 6 out, 5
/// of them reasoning.
const ANSWER: &str = r#"{"usage":{"prompt_tokens":10,"completion_tokens":6,"total_tokens":16,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":5}}}"#;

fn labelled_auth() -> Auth {
    Auth {
        label: "user@example.com".to_owned(),
        ..auth("openai-1.json", "7", "openai")
    }
}

/// Runs a completed call for `model` with `key` as the client's key.
fn completed_call(harness: &Harness, call: ClientCall, key: &str) {
    let driver = call.tap(harness);
    driver.context.set_client_key(key);
    driver.attempt(AttemptKind::Execute, "openai", "gpt-5.4", &labelled_auth());
    driver.head(200, &[]);
    driver.chunk(ANSWER);
    driver.finish(Outcome::Completed);
}

#[track_caller]
fn one_event(receiver: &std::sync::mpsc::Receiver<UsageEvent>) -> UsageEvent {
    let event = receiver.try_recv().expect("an observed event");
    assert_eq!(receiver.try_recv().err(), Some(TryRecvError::Empty));
    event
}

/// Not upstream's: an observer gets every record, and the queue keeps it
/// too, unlike a subscriber, which takes it.
#[test]
fn observing_leaves_records_in_the_queue() {
    let harness = Harness::new();
    let (receiver, _observation) = harness.usage.observe();
    completed_call(&harness, ClientCall::new("gpt-5.4"), "sk-client-key-1234");

    let event = one_event(&receiver);
    assert_eq!(event.provider, "openai");
    assert_eq!(event.model, "gpt-5.4");
    assert_eq!(event.alias, "gpt-5.4");
    assert_eq!(event.endpoint, "POST /v1/chat/completions");
    assert_eq!(event.status, 200);
    assert!(!event.failed);
    assert!(!event.stream);
    assert_eq!(event.ttft, None);
    assert_eq!(event.client_key.expose(), "sk-client-key-1234");
    let credential = event.credential.expect("the credential");
    assert_eq!(credential.id, "openai-1.json");
    assert_eq!(credential.auth_index, "7");
    assert_eq!(credential.label, "user@example.com");
    assert_eq!(event.tokens.input.total_tokens, 10);
    assert_eq!(event.tokens.input.cache_read_tokens, 4);
    assert_eq!(event.tokens.output.total_tokens, 6);
    assert_eq!(event.tokens.output.reasoning_tokens, 5);
    assert_eq!(event.total_tokens, 16);

    assert_eq!(harness.records().len(), 1, "the queue keeps the record");
}

/// Not upstream's: with the queue off (no management key), records are
/// still made for an observer, and none is queued.
#[test]
fn an_observer_gets_records_while_the_queue_is_off() {
    let harness = Harness::new();
    harness.usage.inner.queue.set_enabled(false);
    let (receiver, _observation) = harness.usage.observe();
    completed_call(&harness, ClientCall::new("gpt-5.4"), "");

    let event = one_event(&receiver);
    assert!(event.client_key.is_empty());
    assert!(harness.records().is_empty());
}

/// Not upstream's: with records off, an observer gets nothing, and no tap
/// is made.
#[test]
fn nothing_is_observed_while_records_are_off() {
    let harness = Harness::new();
    harness
        .usage
        .inner
        .queue
        .set_usage_statistics_enabled(false);
    let (_receiver, _observation) = harness.usage.observe();
    let call = ClientCall::new("gpt-5.4");
    let context = Arc::new(call.context);
    assert!(
        harness
            .usage
            .tap(&context, &call.request, &call.options)
            .is_none()
    );
}

/// Not upstream's: once the observation is dropped, nothing more is sent,
/// and with the queue off no tap is made.
#[test]
fn dropping_the_observation_ends_it() {
    let harness = Harness::new();
    let (receiver, observation) = harness.usage.observe();
    drop(observation);
    completed_call(&harness, ClientCall::new("gpt-5.4"), "");
    assert_eq!(receiver.try_recv().err(), Some(TryRecvError::Disconnected));

    harness.usage.inner.queue.set_enabled(false);
    let call = ClientCall::new("gpt-5.4");
    let context = Arc::new(call.context);
    assert!(
        harness
            .usage
            .tap(&context, &call.request, &call.options)
            .is_none()
    );
}

/// Not upstream's: a new observation replaces the last, and dropping the
/// old one's handle doesn't end the new one.
#[test]
fn a_new_observation_replaces_the_last() {
    let harness = Harness::new();
    let (first, first_observation) = harness.usage.observe();
    let (second, _second_observation) = harness.usage.observe();
    drop(first_observation);
    completed_call(&harness, ClientCall::new("gpt-5.4"), "");
    assert_eq!(first.try_recv().err(), Some(TryRecvError::Disconnected));
    one_event(&second);
}

/// Not upstream's: an observer that falls behind loses events, which are
/// counted, and stays observing.
#[test]
fn a_full_observer_loses_events_and_counts_them() {
    let harness = Harness::new();
    harness.usage.inner.queue.set_enabled(false);
    let (receiver, observation) = harness.usage.observe();
    for _ in 0..OBSERVER_BUFFER + 2 {
        completed_call(&harness, ClientCall::new("gpt-5.4"), "");
    }
    assert_eq!(observation.dropped(), 2);
    let mut received = 0;
    while receiver.try_recv().is_ok() {
        received += 1;
    }
    assert_eq!(received, OBSERVER_BUFFER);
    completed_call(&harness, ClientCall::new("gpt-5.4"), "");
    one_event(&receiver);
}

/// Not upstream's: a failed streamed call is observed with its status, and
/// a stream's first token gives a TTFT.
#[test]
fn failures_and_streams_are_observed() {
    let harness = Harness::new();
    let (receiver, _observation) = harness.usage.observe();
    let driver = ClientCall::new("gpt-5.4").stream().tap(&harness);
    driver.attempt(AttemptKind::Stream, "openai", "gpt-5.4", &labelled_auth());
    harness.advance_ms(5);
    driver.fail(&ExecError::upstream(502, "bad gateway"));
    let event = one_event(&receiver);
    assert!(event.failed);
    assert_eq!(event.status, 502);
    assert!(event.stream);

    let driver = ClientCall::new("gpt-5.4").stream().tap(&harness);
    driver.attempt(AttemptKind::Stream, "openai", "gpt-5.4", &labelled_auth());
    harness.advance_ms(40);
    driver.chunk("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
    driver.chunk("data: {\"usage\":{\"total_tokens\":3}}\n\n");
    driver.finish(Outcome::Completed);
    let event = one_event(&receiver);
    assert_eq!(event.ttft, Some(std::time::Duration::from_millis(40)));
}

/// Not upstream's: the client's key doesn't show in an event's `Debug`.
#[test]
fn the_client_key_is_hidden_from_debug() {
    let harness = Harness::new();
    let (receiver, _observation) = harness.usage.observe();
    completed_call(&harness, ClientCall::new("gpt-5.4"), "sk-very-secret-key");
    let event = one_event(&receiver);
    let debug = format!("{event:?}");
    assert!(!debug.contains("sk-very-secret-key"), "{debug}");
    assert!(debug.contains("ClientKey(..)"), "{debug}");
}
