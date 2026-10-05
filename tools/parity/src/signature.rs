//! Our side of the harness's `signature/*` entries: the same calls into the
//! signature module, reported in the same JSON (see `go/main.go`).
//!
//! Upstream's structs are marshalled as they are, so their fields keep Go's
//! names, and every report lists its fields in the Go declaration order.

use open_ferry_translate::signature::{
    self, BlockKind, ClaudeCaisSignatureInfo, ClaudeMessagesSanitizeOptions, ClaudeSignatureTree,
    ClaudeValidationOptions, Decision, Error, GeminiThoughtSignatureInfo, GeminiValidationOptions,
    GptReasoningSignatureInfo, GrokEncryptedContentInfo, KimiThinkingSignatureInfo, Provider,
    SanitizeReport,
};
use serde_json::{Value, json};

use crate::translator::object;

/// Every block kind, in the order reports list per-kind results.
const BLOCK_KINDS: [BlockKind; 5] = [
    BlockKind::Unknown,
    BlockKind::ClaudeThinking,
    BlockKind::GeminiModelPart,
    BlockKind::GeminiFunctionCall,
    BlockKind::GptReasoning,
];

/// The Gemini validation options [`inspect`] tries, in order.
const GEMINI_OPTIONS: [GeminiValidationOptions; 5] = [
    GeminiValidationOptions {
        allow_bypass_sentinel: false,
        require_known_envelope: false,
        require_observed_marker: false,
    },
    GeminiValidationOptions {
        allow_bypass_sentinel: true,
        require_known_envelope: false,
        require_observed_marker: false,
    },
    GeminiValidationOptions {
        allow_bypass_sentinel: false,
        require_known_envelope: true,
        require_observed_marker: false,
    },
    GeminiValidationOptions {
        allow_bypass_sentinel: false,
        require_known_envelope: false,
        require_observed_marker: true,
    },
    GeminiValidationOptions {
        allow_bypass_sentinel: true,
        require_known_envelope: true,
        require_observed_marker: true,
    },
];

/// `signature/inspect`: every check and replay decision on one signature.
pub fn inspect(model: &str, raw: &str, options: &Value) -> Value {
    let target = provider(options.get("target"));
    let strict = ClaudeValidationOptions::STRICT;
    let default = ClaudeValidationOptions::default();
    let model_provider = Provider::from_model_name(model);
    let prefix = signature::split_signature_provider_prefix(raw)
        .map(|(provider, payload)| json!({ "provider": provider.as_str(), "payload": payload }));
    let per_kind =
        |f: &dyn Fn(BlockKind) -> Value| -> Value { BLOCK_KINDS.into_iter().map(f).collect() };

    json!({
        "detect": signature::detect_signature_provider(raw).as_str(),
        "detect_for_block": per_kind(&|kind| {
            signature::detect_signature_provider_for_block(raw, kind).as_str().into()
        }),
        "recognized": signature::is_recognized_reasoning_signature(raw),
        "prefix": prefix,
        "without_prefix": signature::signature_payload_without_provider_prefix(raw),
        "claude": {
            "has_prefix": signature::has_claude_thinking_signature_prefix(raw),
            "decodable": signature::has_decodable_claude_thinking_signature(raw),
            "valid": signature::is_valid_claude_thinking_signature(raw, default),
            "valid_strict": signature::is_valid_claude_thinking_signature(raw, strict),
            "normalized": fallible(signature::normalize_claude_thinking_signature(raw, default), Value::from),
            "normalized_strict": fallible(signature::normalize_claude_thinking_signature(raw, strict), Value::from),
            "native": fallible(
                signature::normalize_claude_provider_native_thinking_signature(raw, default),
                Value::from,
            ),
            "single_layer": fallible(signature::inspect_claude_single_layer_signature(raw), claude_tree),
            "double_layer": fallible(signature::inspect_claude_double_layer_signature(raw), claude_tree),
            "cais": fallible(signature::inspect_claude_cais_signature(raw), cais_info),
            "antigravity": signature::compatible_antigravity_claude_thinking_signature(raw),
        },
        "gemini": {
            "bypass": signature::is_gemini_thought_signature_bypass(raw),
            "inspect": GEMINI_OPTIONS
                .into_iter()
                .map(|opt| fallible(signature::inspect_gemini_thought_signature(raw, opt), gemini_info))
                .collect::<Value>(),
            "replay": per_kind(&|kind| signature::gemini_replay_signature_or_bypass(raw, kind).into()),
        },
        "gpt": fallible(signature::inspect_gpt_reasoning_signature(raw), gpt_info),
        "grok": fallible(signature::inspect_grok_encrypted_content(raw), grok_info),
        "kimi": fallible(signature::inspect_kimi_thinking_signature(raw), kimi_info),
        "model_provider": model_provider.as_str(),
        "for_model": per_kind(&|kind| {
            decision(&signature::decide_signature_compatibility_for_model(
                model_provider, model, raw, kind,
            ))
        }),
        "target": {
            "provider": target.as_str(),
            "compatible": signature::is_signature_compatible_with_provider(target, raw),
            "signature": signature::compatible_signature_for_provider(target, raw),
            "for_block": per_kind(&|kind| {
                signature::compatible_signature_for_provider_block(target, raw, kind).into()
            }),
            "decisions": per_kind(&|kind| {
                decision(&signature::decide_signature_compatibility(target, raw, kind))
            }),
        },
    })
}

/// `signature/claude-messages`: the Claude Messages strippers, validator and
/// sanitizers on one request, for target model `model`.
pub fn claude_messages(model: &str, payload: &Value, options: &Value) -> Value {
    let validation = claude_options(options.get("validation"));
    let target = options.get("target");
    let flag = |name: &str| bool_option(target, name);
    let target = ClaudeMessagesSanitizeOptions {
        target_provider: provider(target.and_then(|target| target.get("TargetProvider"))),
        target_model: model,
        drop_empty_messages: flag("DropEmptyMessages"),
        drop_tool_signatures: flag("DropToolSignatures"),
        drop_empty_thinking_placeholders: flag("DropEmptyThinkingPlaceholders"),
        preserve_empty_thinking_blocks: flag("PreserveEmptyThinkingBlocks"),
    };

    let mut strip = payload.clone();
    signature::strip_invalid_claude_thinking_blocks(&mut strip, validation);
    let mut strip_and_empty = payload.clone();
    signature::strip_invalid_claude_thinking_blocks_and_empty_messages(
        &mut strip_and_empty,
        validation,
    );
    let sanitize = |f: &dyn Fn(&mut Value) -> SanitizeReport| {
        let mut payload = payload.clone();
        let report = f(&mut payload);
        object([("payload", payload), ("report", sanitize_report(&report))])
    };

    object([
        ("strip", strip),
        ("strip_and_empty", strip_and_empty),
        (
            "validate",
            error_text(signature::validate_claude_thinking_signatures(
                payload, validation,
            )),
        ),
        (
            "for_model",
            sanitize(&|payload| {
                signature::sanitize_claude_messages_signatures_for_model(payload, model)
            }),
        ),
        (
            "claude_upstream",
            sanitize(&|payload| {
                signature::sanitize_claude_messages_for_claude_upstream(
                    payload,
                    model,
                    target.preserve_empty_thinking_blocks,
                )
            }),
        ),
        (
            "for_target",
            sanitize(&|payload| {
                signature::sanitize_claude_messages_signatures_for_target(payload, target)
            }),
        ),
    ])
}

/// `signature/gemini`: the Gemini sanitizer and validators on one request.
pub fn gemini(payload: &Value, options: &Value) -> Value {
    let contents_path = options
        .get("contents_path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let validation = options.get("validation");
    let validation = GeminiValidationOptions {
        allow_bypass_sentinel: bool_option(validation, "AllowBypassSentinel"),
        require_known_envelope: bool_option(validation, "RequireKnownEnvelope"),
        require_observed_marker: bool_option(validation, "RequireObservedMarker"),
    };
    let mut clean = payload.clone();
    signature::sanitize_gemini_request_thought_signatures(&mut clean, contents_path);

    let validate = error_text(signature::validate_gemini_thought_signatures(
        payload, validation,
    ));
    let validate_sanitized = error_text(signature::validate_gemini_thought_signatures(
        &clean, validation,
    ));
    let pairing = error_text(signature::validate_gemini_function_call_pairing(payload));
    object([
        ("sanitized", clean),
        ("validate", validate),
        ("validate_sanitized", validate_sanitized),
        ("pairing", pairing),
    ])
}

/// A provider by upstream's name. The harness sends only names upstream defines.
fn provider(name: Option<&Value>) -> Provider {
    match name.and_then(Value::as_str).unwrap_or_default() {
        "claude" => Provider::Claude,
        "gemini" => Provider::Gemini,
        "gemini_bypass" => Provider::GeminiBypass,
        "gpt" => Provider::Gpt,
        "kimi" => Provider::Kimi,
        "grok" => Provider::Grok,
        "swe" => Provider::Swe,
        _ => Provider::Unknown,
    }
}

fn bool_option(options: Option<&Value>, name: &str) -> bool {
    options
        .and_then(|options| options.get(name))
        .and_then(Value::as_bool)
        .unwrap_or_default()
}

/// `ClaudeSignatureValidationOptions`, by its Go field names.
fn claude_options(options: Option<&Value>) -> ClaudeValidationOptions {
    ClaudeValidationOptions {
        prefix_only: bool_option(options, "PrefixOnly"),
        base64_only: bool_option(options, "Base64Only"),
        allow_empty_signature_with_empty_text: bool_option(
            options,
            "AllowEmptySignatureWithEmptyText",
        ),
        strict: bool_option(options, "Strict"),
    }
}

/// A result as `{"ok": value}` or `{"error": message}`.
fn fallible<T>(result: Result<T, Error>, ok: impl FnOnce(T) -> Value) -> Value {
    match result {
        Ok(value) => json!({ "ok": ok(value) }),
        Err(err) => json!({ "error": err.to_string() }),
    }
}

/// An error as its message, or null.
fn error_text(result: Result<(), Error>) -> Value {
    result.err().map(|err| err.to_string()).into()
}

fn decision(decision: &Decision) -> Value {
    json!({
        "TargetProvider": decision.target_provider.as_str(),
        "DetectedProvider": decision.detected_provider.as_str(),
        "BlockKind": decision.block_kind.as_str(),
        "Compatible": decision.compatible,
        "Action": decision.action.as_str(),
        "ReplacementSignature": decision.replacement_signature,
        "NormalizedSignature": decision.normalized_signature,
        "Reason": decision.reason,
    })
}

fn sanitize_report(report: &SanitizeReport) -> Value {
    json!({
        "TargetProvider": report.target_provider.as_str(),
        "Preserved": report.preserved,
        "DroppedBlocks": report.dropped_blocks,
        "DroppedSignatures": report.dropped_signatures,
        "ReplacedSignatures": report.replaced_signatures,
        "Decisions": report.decisions.iter().map(decision).collect::<Value>(),
    })
}

fn claude_tree(tree: ClaudeSignatureTree) -> Value {
    json!({
        "EncodingLayers": tree.encoding_layers,
        "ChannelID": tree.channel_id,
        "Field2": tree.field2,
        "RoutingClass": tree.routing_class,
        "InfrastructureClass": tree.infrastructure_class,
        "SchemaFeatures": tree.schema_features,
        "ModelText": tree.model_text,
        "LegacyRouteHint": tree.legacy_route_hint,
        "HasField7": tree.has_field7,
    })
}

fn cais_info(info: ClaudeCaisSignatureInfo) -> Value {
    json!({
        "FirstByte": info.first_byte,
        "EnvelopeVersion": info.envelope_version,
        "ChannelID": info.channel_id,
        "ModelText": info.model_text,
        "BlockKind": info.block_kind,
        "ContextID": info.context_id,
        "SignatureLen": info.signature_len,
    })
}

fn gemini_info(info: GeminiThoughtSignatureInfo) -> Value {
    json!({
        "IsBypassSentinel": info.is_bypass_sentinel,
        "BypassSentinel": info.bypass_sentinel,
        "DecodedLen": info.decoded_len,
        "FirstByte": info.first_byte,
        "HasObservedMarker": info.has_observed_marker,
        "KnownEnvelope": info.known_envelope,
        // Upstream leaves the envelope empty for a bypass sentinel.
        "Envelope": info.envelope.map_or("", |envelope| envelope.as_str()),
        "RecordCount": info.record_count,
        "OpaquePayloadLen": info.opaque_payload_len,
    })
}

fn gpt_info(info: GptReasoningSignatureInfo) -> Value {
    json!({ "DecodedLen": info.decoded_len, "CiphertextLen": info.ciphertext_len })
}

fn grok_info(info: GrokEncryptedContentInfo) -> Value {
    json!({ "RawLen": info.raw_len, "DecodedLen": info.decoded_len })
}

fn kimi_info(info: KimiThinkingSignatureInfo) -> Value {
    json!({
        "RawLen": info.raw_len,
        "DecodedLen": info.decoded_len,
        "Mode": info.mode.as_str(),
    })
}
