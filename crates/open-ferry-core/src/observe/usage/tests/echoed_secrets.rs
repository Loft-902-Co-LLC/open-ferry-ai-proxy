//! Not upstream's: the models a record and the substitution warning name
//! come from the upstream's answer and the client's request, and the record
//! is served to the management API and written to disk and the warning to
//! main.log, so a credential's secret they quote is scrubbed from both, as
//! a file is.
//!
//! Upstream writes the model as it is. Each test runs a call through the
//! usage tap and reads what it queues and what it logs.

use super::support::{ClientCall, Harness, Warnings, auth, str_field};
use crate::auth::Auth;
use crate::exec::Format;
use crate::observe::redact::REDACTED;
use crate::observe::{AttemptKind, Outcome};

const SECRET: &str = "echoed-dummy-meta-secret";

/// The line that tells each provider's served model, and the format its
/// executor sends.
fn served_line(provider: &str, served: &str) -> (Format, String) {
    match provider {
        "meta" | "codex" => (
            Format::CODEX,
            format!(r#"data: {{"type":"response.created","response":{{"model":"{served}"}}}}"#),
        ),
        "claude" => (
            Format::CLAUDE,
            format!(
                r#"data: {{"type":"message_start","message":{{"id":"msg_1","model":"{served}"}}}}"#
            ),
        ),
        "gemini" => (
            Format::GEMINI,
            format!(
                r#"data: {{"candidates":[{{"content":{{"parts":[{{"text":"hi"}}]}}}}],"modelVersion":"{served}"}}"#
            ),
        ),
        "gemini-interactions" => (
            Format::OPENAI,
            format!(
                r#"data: {{"event_type":"interaction.completed","interaction":{{"id":"i1","status":"completed","model":"{served}"}}}}"#
            ),
        ),
        _ => (
            Format::OPENAI,
            format!(
                r#"data: {{"id":"chatcmpl-1","model":"{served}","choices":[{{"index":0,"delta":{{"content":"hi"}}}}]}}"#
            ),
        ),
    }
}

/// Streams `served` as the model of an answer to a call for `requested` to
/// `provider` with `credential`, which sent `secrets`, and returns the one
/// record and the warnings logged.
fn substituted_call(
    provider: &str,
    credential: &Auth,
    secrets: &[&str],
    requested: &str,
    served: &str,
) -> (serde_json::Value, Warnings) {
    let warnings = Warnings::capture();
    let harness = Harness::new();
    let driver = ClientCall::new("client-alias").stream().tap(&harness);
    let (format, line) = served_line(provider, served);
    driver.attempt_with(
        AttemptKind::Stream,
        provider,
        requested,
        &format,
        credential,
        secrets,
        "{}",
    );
    driver.chunk(&format!("{line}\n"));
    if provider == "claude" {
        driver.chunk(
            "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}\n",
        );
    }
    driver.finish(Outcome::Completed);
    (harness.record(), warnings)
}

/// Checks that `record` and the warnings name the models with `secret`
/// replaced, and that nothing of `secret` is left in either.
#[track_caller]
fn assert_scrubbed(
    what: &str,
    record: &serde_json::Value,
    warnings: &Warnings,
    secret: &str,
    requested: &str,
    served: &str,
) {
    let requested = requested.replace(secret, REDACTED);
    let served = served.replace(secret, REDACTED);
    assert_eq!(str_field(record, "response_model"), served, "{what}");
    assert_eq!(str_field(record, "model"), requested, "{what}");
    // The record's `source` is the credential, which for an API key is the
    // key, as upstream's is; it is not what is under test.
    let mut rest = record.clone();
    if let Some(fields) = rest.as_object_mut() {
        fields.remove("source");
    }
    assert!(!rest.to_string().contains(secret), "{what}: {rest}");
    let warned = warnings.substitutions();
    assert_eq!(warned.len(), 1, "{what}: {warned:?}");
    assert!(
        warned[0].contains(&format!(
            r#"upstream served model "{served}" for requested model "{requested}""#
        )),
        "{what}: {}",
        warned[0]
    );
    for line in warnings.all() {
        assert!(!line.contains(secret), "{what}: logged {line}");
    }
}

/// An upstream that echoes the token it was sent in the model it names, and
/// a request model that carries it, reach neither the record nor main.log,
/// whichever executor served the call.
#[test]
fn models_are_scrubbed_of_the_attempts_secrets() {
    let requested = format!("requested-{SECRET}");
    let served = format!("served-{SECRET}");
    for provider in [
        "meta",
        "codex",
        "claude",
        "gemini",
        "gemini-interactions",
        "openai",
    ] {
        let (record, warnings) = substituted_call(
            provider,
            &auth("auth-1", "0", provider),
            &[SECRET],
            &requested,
            &served,
        );
        assert_scrubbed(provider, &record, &warnings, SECRET, &requested, &served);
    }
}

/// The credential's own key is scrubbed as well when the attempt names no
/// secret of its own, and however short it is: the record is scrubbed as a
/// file is, which hides a secret of any length.
#[test]
fn models_are_scrubbed_of_the_credentials_keys_however_short() {
    for key in [SECRET, "k3y!"] {
        let credential = Auth {
            attributes: [("api_key".to_owned(), key.to_owned())].into(),
            ..auth("auth-1", "0", "meta")
        };
        let requested = format!("requested-{key}");
        let served = format!("served-{key}");
        let (record, warnings) = substituted_call("meta", &credential, &[], &requested, &served);
        assert_scrubbed(key, &record, &warnings, key, &requested, &served);
    }
}

/// A model that quotes nothing secret is recorded and logged as it is.
#[test]
fn models_without_a_secret_are_left_alone() {
    let (record, warnings) = substituted_call(
        "meta",
        &auth("auth-1", "0", "meta"),
        &[SECRET],
        "muse-spark",
        "muse-spark-lite",
    );
    assert_eq!(str_field(&record, "response_model"), "muse-spark-lite");
    assert_eq!(str_field(&record, "model"), "muse-spark");
    let warned = warnings.substitutions();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(
        warned[0].contains(
            r#"upstream served model "muse-spark-lite" for requested model "muse-spark""#
        ),
        "{}",
        warned[0]
    );
}
