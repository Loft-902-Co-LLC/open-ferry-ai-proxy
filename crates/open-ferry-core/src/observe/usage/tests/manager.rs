// Ported from CLIProxyAPI sdk/cliproxy/usage/manager_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the stream and generate flags a record carries.
//!
//! Upstream reads both from the call's Go context, which its auth manager
//! fills from the call's options; open-ferry's tap reads them from the
//! options itself, so the context tests are ported as calls made with and
//! without the option.
//!
//! Dropped: TestRecordBaseURLField, as records have no base URL (see the
//! record's module).
//!
//! Deviations from upstream: a record's `generate` is a plain flag, worked
//! out when the call is made, so an omitted flag can't reach a record.

use super::super::accounting::Detail;
use super::super::record_json::Record;
use super::super::reporter::generate_enabled;
use super::support::{ClientCall, Harness, auth, bool_at};
use crate::observe::{AttemptKind, Outcome};

/// The record of a call to OpenAI that completes with three tokens.
fn record_of(call: ClientCall) -> serde_json::Value {
    let harness = Harness::new();
    let driver = call.tap(&harness);
    let credential = auth("openai-1", "0", "openai");
    driver.attempt(AttemptKind::Execute, "openai", "gpt-5.4", &credential);
    driver.chunk(r#"{"usage":{"total_tokens":3}}"#);
    driver.finish(Outcome::Completed);
    harness.record()
}

/// Ports TestStreamFromContextDefaultsMissingToFalse.
#[test]
fn stream_defaults_missing_to_false() {
    let record = record_of(ClientCall::new("gpt-5.4"));
    assert!(!bool_at(&record, "/stream"));
}

/// Ports TestStreamFromContextHonorsExplicitTrue.
#[test]
fn stream_honors_explicit_true() {
    let record = record_of(ClientCall::new("gpt-5.4").stream());
    assert!(bool_at(&record, "/stream"));
}

/// Ports TestRecordStreamField.
#[test]
fn record_stream_field() {
    let record = Record {
        provider: "openai".to_owned(),
        model: "gpt-5.4".to_owned(),
        stream: true,
        detail: Detail::default(),
        ..Record::default()
    };
    let encoded: serde_json::Value = serde_json::from_str(&record.encode()).expect("JSON");
    assert!(bool_at(&encoded, "/stream"));
}

/// Ports TestGenerateEnabledDefaultsNilToTrue.
#[test]
fn generate_enabled_defaults_none_to_true() {
    assert!(generate_enabled(None));
}

/// Ports TestGenerateEnabledHonorsExplicitFalse.
#[test]
fn generate_enabled_honors_explicit_false() {
    assert!(!generate_enabled(Some(false)));
}

/// Ports TestGenerateEnabledHonorsExplicitTrue.
#[test]
fn generate_enabled_honors_explicit_true() {
    assert!(generate_enabled(Some(true)));
}

/// Ports TestGenerateFromContextDefaultsMissingToTrue.
#[test]
fn generate_defaults_missing_to_true() {
    let record = record_of(ClientCall::new("gpt-5.4").body(r#"{"model":"gpt-5.4"}"#));
    assert!(bool_at(&record, "/generate"));
}

/// Ports TestGenerateFromContextHonorsExplicitFalse.
#[test]
fn generate_honors_explicit_false() {
    let record = record_of(ClientCall::new("gpt-5.4").body(r#"{"generate":false}"#));
    assert!(!bool_at(&record, "/generate"));
}

/// Ports TestRecordOmittedGenerateIsEnabled: a client flag that isn't a
/// boolean counts as omitted.
#[test]
fn record_omitted_generate_is_enabled() {
    for body in [r#"{"generate":null}"#, r#"{"generate":"false"}"#, ""] {
        let record = record_of(ClientCall::new("gpt-5.4").body(body));
        assert!(bool_at(&record, "/generate"), "{body}");
    }
}
