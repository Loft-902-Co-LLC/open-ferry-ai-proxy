// Ported from CLIProxyAPI internal/signature/claude_antigravity_boundaries_test.go
// (v8.0.20, MIT). https://github.com/router-for-me/CLIProxyAPI

use base64::engine::general_purpose::STANDARD;

use super::claude_antigravity_validation::validation_modes;
use super::signaturetest::antigravity_caqs;
use super::*;
use crate::signature::claude_validation::extract_bytes_field;

// Ports TestAntigravityCAQSNativeSanitizerRejectsPrefixedWrappers.
#[test]
fn antigravity_caqs_native_sanitizer_rejects_prefixed_wrappers() {
    let sig = antigravity_caqs();
    for (name, prefix, want_detected, want_antigravity) in [
        ("bare", "", Provider::Claude, true),
        ("single prefix", "claude#", Provider::Claude, true),
        ("alias prefix", "anthropic#", Provider::Claude, true),
        (
            "duplicate prefix",
            "claude#claude#",
            Provider::Unknown,
            false,
        ),
        (
            "nested unknown prefix",
            "claude#junk#",
            Provider::Unknown,
            false,
        ),
        (
            "nested alias prefix",
            "anthropic#cais#",
            Provider::Unknown,
            false,
        ),
        ("unknown prefix", "junk#", Provider::Unknown, false),
    ] {
        let raw = format!("{prefix}{sig}");
        assert_eq!(
            detect_signature_provider(&raw),
            want_detected,
            "{name}: detected"
        );
        let decision =
            decide_signature_compatibility(Provider::Claude, &raw, BlockKind::ClaudeThinking);
        assert!(
            !decision.compatible
                && decision.action == Action::DropBlock
                && decision.normalized_signature.is_empty(),
            "{name}: native Claude must reject Q wrapper: {decision:?}"
        );
        let normalized = compatible_antigravity_claude_thinking_signature(&raw);
        assert_eq!(
            normalized.is_some(),
            want_antigravity,
            "{name}: Antigravity compatibility"
        );
        if let Some(normalized) = normalized {
            assert_eq!(normalized, sig, "{name}: Antigravity normalization");
        }

        let mut payload = serde_json::json!({"messages": [{"role": "assistant", "content": [
            {"type": "thinking", "thinking": "reasoning", "signature": raw},
            {"type": "text", "text": "answer"},
        ]}]});
        let report =
            sanitize_claude_messages_for_claude_upstream(&mut payload, "claude-opus-5-5", false);
        let parts = payload["messages"][0]["content"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        assert!(
            parts.len() == 1
                && parts[0]["type"] == "text"
                && parts[0]["text"] == "answer"
                && report.dropped_blocks == 1
                && report.preserved == 0,
            "{name}: native sanitizer retained Google thinking wrapper: report={report:?}"
        );
    }
}

// Ports TestAntigravityCAQSRejectsDuplicateSubmessages.
#[test]
fn antigravity_caqs_rejects_duplicate_submessages() {
    let inner = STANDARD.decode(antigravity_caqs()).expect("outer layer");
    let raw = STANDARD.decode(&inner).expect("inner layer");
    let container = extract_bytes_field(&raw, 2, "container").expect("container");
    let channel = extract_bytes_field(container, 1, "channel").expect("channel");
    let bytes_field = |field: u64, value: &[u8]| Pb::new().bytes(field, value).build();
    let payload = |containers: &[&[u8]]| {
        containers
            .iter()
            .fold(Pb::new().raw(&[8, 4]), |pb, value| pb.bytes(2, value))
            .raw(&[24, 1])
            .build()
    };

    let first_channels: [(&str, &[u8]); 5] = [
        ("malformed tag", &[0x80]),
        ("infrastructure wire type", &[0x12, 0x01, 0x02]),
        ("old signature slot", &[0x2a, 0x01, 0x01]),
        ("empty", &[]),
        ("valid duplicate", channel),
    ];
    for (name, first_channel) in first_channels {
        for scope in ["channel", "container"] {
            let first = bytes_field(1, first_channel);
            let duplicated = if scope == "channel" {
                payload(&[&[first.as_slice(), container].concat()])
            } else {
                payload(&[&first, container])
            };
            assert_antigravity_caqs_duplicate_rejected(&format!("{scope}/{name}"), &duplicated);
        }
    }

    let first_containers: [(&str, &[u8]); 3] = [
        ("malformed container", &[0x80]),
        ("empty container", &[]),
        ("valid container duplicate", container),
    ];
    for (name, first) in first_containers {
        assert_antigravity_caqs_duplicate_rejected(name, &payload(&[first, container]));
    }
}

/// `assertAntigravityCAQSDuplicateRejected`.
fn assert_antigravity_caqs_duplicate_rejected(name: &str, raw: &[u8]) {
    let inner = STANDARD.encode(raw);
    // Native CAIS and CAQS parsing keeps its behavior outside the Q check.
    if let Err(err) = inspect_claude_cais_signature(&inner) {
        panic!("{name}: native parser behavior changed: {err}");
    }
    let sig = STANDARD.encode(&inner);
    assert!(
        inspect_antigravity_claude_caqs_signature(&sig).is_err(),
        "{name}: duplicate submessage passed Q structural validation"
    );
    for opts in validation_modes() {
        assert!(
            !is_valid_claude_thinking_signature(&sig, opts),
            "{name}: duplicate submessage accepted in mode {opts:?}"
        );
        assert!(
            normalize_claude_thinking_signature(&sig, opts).is_err(),
            "{name}: duplicate submessage normalized in mode {opts:?}"
        );
    }
    assert_eq!(
        detect_signature_provider(&sig),
        Provider::Unknown,
        "{name}: duplicate submessage classified as a valid signature"
    );
    assert_eq!(
        compatible_antigravity_claude_thinking_signature(&sig),
        None,
        "{name}: duplicate submessage allowed for Antigravity replay"
    );
}
