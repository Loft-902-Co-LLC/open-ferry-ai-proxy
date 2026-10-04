//! Ported from upstream's
//! internal/runtime/executor/helps/payload_helpers_codex_integer_test.go.

use super::{Args, headers};

/// `TestApplyPayloadConfigWithTrackedPathsForExecutorCodexIntegerNormalizationUsesExecutor`:
/// with no config, a Codex client's whole-number tool parameter is declared
/// `integer` unless the body goes to a Codex executor, whatever its format.
#[test]
fn codex_integer_normalization_uses_executor() {
    let input = r#"{"tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"yield_time_ms":{"type":"number"}}}}]}"#;
    for (name, executor, protocol, want) in [
        (
            "Codex executor with another protocol keeps number",
            "codex",
            "openai-response",
            "number",
        ),
        (
            "xAI executor with Codex protocol normalizes",
            "xai",
            "codex",
            "integer",
        ),
    ] {
        let args = Args {
            executor,
            model: "model",
            protocol,
            headers: headers(&[("User-Agent", "codex_cli_rs/0.1")]),
            ..Args::default()
        };
        let (out, _) = args.apply(None, input);
        assert_eq!(
            out["tools"][0]["parameters"]["properties"]["yield_time_ms"]["type"], want,
            "{name}: {out}"
        );
    }
}

/// Not upstream's: every Codex executor name, in any case and with spaces
/// around it, skips the pass, and a client that isn't Codex is left alone.
#[test]
fn codex_target_names_and_other_clients() {
    let input = r#"{"tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"yield_time_ms":{"type":"number"}}}}]}"#;
    let path = |out: &serde_json::Value| {
        out["tools"][0]["parameters"]["properties"]["yield_time_ms"]["type"].clone()
    };
    for executor in [" Codex ", "codex-websockets", "CODEX_WEBSOCKETS"] {
        let args = Args {
            executor,
            headers: headers(&[("User-Agent", "codex_cli_rs/0.1")]),
            ..Args::default()
        };
        assert_eq!(path(&args.apply(None, input).0), "number", "{executor}");
    }
    let args = Args {
        executor: "claude",
        headers: headers(&[("User-Agent", "curl/8")]),
        ..Args::default()
    };
    assert_eq!(path(&args.apply(None, input).0), "number");
}
