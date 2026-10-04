// Ported from CLIProxyAPI sdk/api/handlers/model_execution_test.go
// (TestExecuteProtocolWithAuthManagerUsesForcedProvider and
// TestExecuteProtocol[Stream]WithAuthManagerAgentUsesSelectionModelForAuth)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A call with a forced provider: only that provider is tried, and with an
//! `auth_selection_model` the credential is picked by that model while the
//! executor gets the request's.
//!
//! Deviations from upstream:
//! - Upstream's tests go through the handler's `ExecuteProtocolWithAuthManager`,
//!   which resolves the forced provider before the manager sees the call.
//!   Here the forced provider is in the call's metadata and the manager
//!   honours it, so the tests call the manager directly, with the options
//!   the server's Interactions handler gives it.

use bytes::Bytes;

use super::support::*;
use crate::exec::{Dispatcher, Format, Options};
use crate::manager::Settings;

const SELECTION_MODEL: &str = "gemini-2.5-flash";
const AGENT_MODEL: &str = "agents/test-agent";

/// Options for an Interactions call forced to `provider`.
fn forced(provider: &str) -> Options {
    let mut opts = Options::new(Format::from("interactions"));
    opts.metadata.forced_provider = Some(provider.to_owned());
    opts
}

/// Options for an `agent` call, as the Interactions handler makes them.
fn agent(stream: bool) -> Options {
    let mut opts = forced("gemini-interactions");
    opts.metadata.auth_selection_model = Some(SELECTION_MODEL.to_owned());
    opts.stream = stream;
    opts
}

// Ports TestExecuteProtocolWithAuthManagerUsesForcedProvider. The call names
// no providers: the forced one is enough.
#[tokio::test(start_paused = true)]
async fn uses_the_forced_provider() {
    let model = "interactions-agent-target";
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::with("gemini", |_| Reply::ok(r#"{"id":"interaction_1"}"#));
    h.executor(&executor);
    h.add(auth("a", "gemini"), &[model]);

    let body = r#"{"agent":"agents/test-agent","input":"hi"}"#;
    let response = h
        .manager
        .execute(&[], request_with(model, body), forced("gemini"))
        .await
        .unwrap();
    assert_eq!(&response.payload[..], br#"{"id":"interaction_1"}"#);

    let calls = executor.calls();
    let [call] = calls.as_slice() else {
        panic!("calls = {calls:?}");
    };
    assert_eq!(call.model, model);
    assert_eq!(call.options.source_format, Format::from("interactions"));
    assert_eq!(call.options.response_format, Format::from("interactions"));
    assert_eq!(call.options.metadata.requested_model, model);
}

// Not upstream's: the providers a call names don't widen a forced call.
#[tokio::test(start_paused = true)]
async fn tries_no_provider_but_the_forced_one() {
    let h = Harness::new(Settings::default());
    let gemini = FakeExecutor::with("gemini", |_| Reply::status(500, "down"));
    let interactions = FakeExecutor::with("gemini-interactions", |_| Reply::status(500, "down"));
    h.executor(&gemini);
    h.executor(&interactions);
    h.add(auth("g", "gemini"), &["m"]);
    h.add(auth("i", "gemini-interactions"), &["m"]);

    let err = h
        .manager
        .execute(
            &providers(&["gemini", "gemini-interactions"]),
            request("m"),
            forced("gemini-interactions"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 500);
    assert!(gemini.calls().is_empty());
    assert_eq!(interactions.ids(Kind::Execute), ["i"]);
}

// Not upstream's: the manager's side of upstream's router conflict.
#[tokio::test(start_paused = true)]
async fn a_forced_provider_the_call_leaves_out_is_a_conflict() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("claude");
    h.executor(&executor);
    h.add(auth("c", "claude"), &[AGENT_MODEL]);

    let err = h
        .manager
        .execute(&providers(&["claude"]), request(AGENT_MODEL), agent(false))
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 400);
    assert_eq!(
        err.to_string(),
        "agent is only supported for native interactions execution"
    );
    let err = h
        .manager
        .execute_stream(&providers(&["claude"]), request(AGENT_MODEL), agent(true))
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), 400);
    assert!(executor.calls().is_empty());
}

// Ports TestExecuteProtocolWithAuthManagerAgentUsesSelectionModelForAuth.
#[tokio::test(start_paused = true)]
async fn agent_uses_the_selection_model_for_auth() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::with("gemini-interactions", |_| {
        Reply::ok(r#"{"id":"interaction_1"}"#)
    });
    h.executor(&executor);
    h.add(
        auth("model-execution-agent-selection", "gemini-interactions"),
        &[SELECTION_MODEL, AGENT_MODEL],
    );

    let body = r#"{"agent":"agents/test-agent","input":"hi"}"#;
    let response = h
        .manager
        .execute(
            &providers(&["gemini-interactions"]),
            request_with(AGENT_MODEL, body),
            agent(false),
        )
        .await
        .unwrap();
    assert_eq!(&response.payload[..], br#"{"id":"interaction_1"}"#);

    let calls = executor.calls();
    let [call] = calls.as_slice() else {
        panic!("calls = {calls:?}");
    };
    assert_eq!(call.model, AGENT_MODEL);
    assert_eq!(call.payload, Bytes::from(body));
    assert_eq!(
        call.options.metadata.auth_selection_model.as_deref(),
        Some(SELECTION_MODEL)
    );
}

// Ports TestExecuteProtocolStreamWithAuthManagerAgentUsesSelectionModelForAuth.
#[tokio::test(start_paused = true)]
async fn agent_stream_uses_the_selection_model_for_auth() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::with("gemini-interactions", |_| {
        Reply::chunks(vec![Ok(Bytes::from_static(br#"{"id":"interaction_1"}"#))])
    });
    h.executor(&executor);
    h.add(
        auth(
            "model-execution-agent-stream-selection",
            "gemini-interactions",
        ),
        &[SELECTION_MODEL, AGENT_MODEL],
    );

    let body = r#"{"agent":"agents/test-agent","input":"hi","stream":true}"#;
    let stream = h
        .manager
        .execute_stream(
            &providers(&["gemini-interactions"]),
            request_with(AGENT_MODEL, body),
            agent(true),
        )
        .await
        .unwrap();
    let (chunks, err) = collect(stream).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(chunks, [r#"{"id":"interaction_1"}"#]);

    let calls = executor.calls();
    let [call] = calls.as_slice() else {
        panic!("calls = {calls:?}");
    };
    assert_eq!(call.model, AGENT_MODEL);
    assert_eq!(call.payload, Bytes::from(body));
    assert_eq!(
        call.options.metadata.auth_selection_model.as_deref(),
        Some(SELECTION_MODEL)
    );
}

// Not upstream's: the credential is picked by the selection model alone, so
// one that serves only `gemini-2.5-flash` still runs the agent.
#[tokio::test(start_paused = true)]
async fn agent_runs_on_a_credential_for_the_selection_model() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini-interactions");
    h.executor(&executor);
    h.add(auth("flash", "gemini-interactions"), &[SELECTION_MODEL]);

    h.manager
        .execute(
            &providers(&["gemini-interactions"]),
            request(AGENT_MODEL),
            agent(false),
        )
        .await
        .unwrap();
    assert_eq!(executor.ids(Kind::Execute), ["flash"]);
    assert_eq!(executor.models(Kind::Execute), [AGENT_MODEL]);
}
