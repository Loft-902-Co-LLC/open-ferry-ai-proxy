// Ported from CLIProxyAPI internal/signature/gemini_validation.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini thought signatures.
//!
//! Gemini 3 returns `thoughtSignature` on model parts, and checks that a
//! replayed model `functionCall` still carries the signature it was issued
//! with. Other parts may carry one too. History that Gemini didn't produce has
//! no real signature, so Gemini documents two bypass sentinels for it; this
//! repo sends `skip_thought_signature_validator` on the first function call of
//! such a turn.
//!
//! The signatures are opaque provider state, so only the transport envelope is
//! checked. The one replay-safe envelope is protobuf field 2 holding field 1,
//! whose value is versioned Tink ciphertext (first byte `0x01`), a provider
//! UUID, or a server-side tool invocation wrapping Tink ciphertext. Gemini
//! 2.5's repeated field-1 form is no longer recognized. A bare base64 UUID is
//! classified separately and should be replaced by the bypass sentinel.

use serde_json::Value;

use super::gemini_sanitize::{has_normalized_part_thought_signature, part_thought_signature};
use super::{Error, claude_validation::is_canonical_uuid, is_valid_claude_cais_signature};
use crate::go::base64::{RAW_STD, STD};
use crate::go::{self, quote};
use crate::json::{self, str_of};
use crate::protowire::{
    self, BYTES_TYPE, FIXED32_TYPE, FIXED64_TYPE, VARINT_TYPE, consume_bytes, consume_tag,
};

pub const MAX_GEMINI_THOUGHT_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
/// Gemini's documented sentinel for history it didn't produce.
pub const GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";
/// Gemini's other documented bypass sentinel.
pub const GEMINI_CONTEXT_ENGINEERING_BYPASS: &str = "context_engineering_is_the_way_to_go";

/// How much of a Gemini thought signature is checked. None of it proves the
/// signature came from Gemini.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GeminiValidationOptions {
    /// Accept the documented bypass sentinels.
    pub allow_bypass_sentinel: bool,
    /// Require the replay-safe protobuf envelope. This rejects opaque base64
    /// such as a base64 UUID.
    pub require_known_envelope: bool,
    /// Require the decoded payload to start with `0x12`. A weaker check than
    /// `require_known_envelope`.
    pub require_observed_marker: bool,
}

/// The envelope a decoded Gemini thought signature was found to have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GeminiEnvelope {
    Unknown,
    /// Field 2 holding field 1: the only replay-safe envelope.
    ProtobufField2,
    AsciiUuid,
}

impl GeminiEnvelope {
    /// Upstream's name for the envelope.
    pub fn as_str(self) -> &'static str {
        match self {
            GeminiEnvelope::Unknown => "unknown",
            GeminiEnvelope::ProtobufField2 => "protobuf_field_2",
            GeminiEnvelope::AsciiUuid => "ascii_uuid",
        }
    }
}

/// What can be checked locally about an opaque Gemini thought signature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GeminiThoughtSignatureInfo {
    pub is_bypass_sentinel: bool,
    pub bypass_sentinel: String,
    pub decoded_len: usize,
    pub first_byte: u8,
    pub has_observed_marker: bool,
    pub known_envelope: bool,
    /// `None` for a bypass sentinel, which isn't decoded.
    pub envelope: Option<GeminiEnvelope>,
    pub record_count: usize,
    pub opaque_payload_len: usize,
}

/// `IsGeminiThoughtSignatureBypass`: whether `raw` is one of Gemini's
/// documented bypass sentinels.
pub fn is_gemini_thought_signature_bypass(raw: &str) -> bool {
    matches!(
        raw.trim(),
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR | GEMINI_CONTEXT_ENGINEERING_BYPASS
    )
}

/// `IsValidGeminiThoughtSignature`.
pub fn is_valid_gemini_thought_signature(raw: &str, opt: GeminiValidationOptions) -> bool {
    inspect_gemini_thought_signature(raw, opt).is_ok()
}

/// `InspectGeminiThoughtSignature`: checks the transport shape of a Gemini
/// thought signature, treating the payload as opaque.
pub fn inspect_gemini_thought_signature(
    raw: &str,
    opt: GeminiValidationOptions,
) -> Result<GeminiThoughtSignatureInfo, Error> {
    let sig = raw.trim();
    if sig.is_empty() {
        return Err(error!("empty Gemini thought signature"));
    }
    if is_valid_claude_cais_signature(sig) {
        return Err(error!(
            "invalid Gemini thought signature: detected Claude CAIS signature"
        ));
    }
    if is_gemini_thought_signature_bypass(sig) {
        if !opt.allow_bypass_sentinel {
            return Err(error!(
                "Gemini thought signature bypass sentinel is not allowed"
            ));
        }
        return Ok(GeminiThoughtSignatureInfo {
            is_bypass_sentinel: true,
            bypass_sentinel: sig.to_owned(),
            ..GeminiThoughtSignatureInfo::default()
        });
    }

    let decoded = decode(sig)?;
    let Some(&first_byte) = decoded.first() else {
        return Err(error!(
            "invalid Gemini thought signature: empty decoded payload"
        ));
    };
    let (envelope, known_envelope) = classify_envelope(&decoded);
    let opaque_payload_len = match envelope {
        GeminiEnvelope::ProtobufField2 => field2_envelope_payload_len(&decoded).unwrap_or(0),
        _ => 0,
    };
    let info = GeminiThoughtSignatureInfo {
        is_bypass_sentinel: false,
        bypass_sentinel: String::new(),
        decoded_len: decoded.len(),
        first_byte,
        has_observed_marker: first_byte == 0x12,
        known_envelope,
        envelope: Some(envelope),
        record_count: usize::from(opaque_payload_len > 0),
        opaque_payload_len,
    };
    if opt.require_known_envelope && !known_envelope {
        return Err(error!(
            "invalid Gemini thought signature: unknown envelope {}",
            quote(envelope.as_str())
        ));
    }
    if opt.require_observed_marker && !info.has_observed_marker {
        return Err(error!(
            "invalid Gemini thought signature: expected observed marker 0x12, got 0x{first_byte:02x}"
        ));
    }
    Ok(info)
}

/// `ValidateGeminiThoughtSignatures`: checks the `thoughtSignature` fields of a
/// Gemini request. The first `functionCall` of each model turn needs a valid
/// signature or the bypass sentinel. Later parallel calls may be unsigned, but
/// a signature they carry must be valid.
pub fn validate_gemini_thought_signatures(
    payload: &Value,
    opt: GeminiValidationOptions,
) -> Result<(), Error> {
    let (contents, contents_path) = gemini_contents(payload);
    let Some(Value::Array(contents)) = contents else {
        return Ok(());
    };
    for (i, content) in contents.iter().enumerate() {
        let Some(Value::Array(parts)) = content.get("parts") else {
            continue;
        };
        let is_model_turn = str_of(content.get("role"))
            .trim()
            .eq_ignore_ascii_case("model");
        let mut first_function_call_seen = false;
        for (j, part) in parts.iter().enumerate() {
            let has_function_call = part.get("functionCall").is_some();
            let is_first_function_call =
                is_model_turn && has_function_call && !first_function_call_seen;
            if is_model_turn && has_function_call {
                first_function_call_seen = true;
            }
            let signature = part_thought_signature(part);
            if !has_function_call && signature.is_none() {
                continue;
            }

            let part_path = format!("{contents_path}[{i}].parts[{j}]");
            let has_signature = signature.is_some();
            let signature = signature.unwrap_or_default();
            let signature = signature.trim();
            if part.get("functionResponse").is_some() && has_signature {
                return Err(error!(
                    "{part_path}: functionResponse must not carry thoughtSignature"
                ));
            }
            if signature.is_empty() {
                if is_first_function_call {
                    return Err(error!(
                        "{part_path}: missing thoughtSignature on first functionCall"
                    ));
                }
                if has_signature {
                    return Err(error!("{part_path}: empty thoughtSignature"));
                }
                continue;
            }
            if is_gemini_thought_signature_bypass(signature) && !is_first_function_call {
                return Err(error!(
                    "{part_path}: Gemini bypass sentinel is allowed only on the first model functionCall"
                ));
            }
            if !has_normalized_part_thought_signature(part, signature) {
                return Err(error!(
                    "{part_path}: thoughtSignature must use one canonical top-level field"
                ));
            }
            inspect_gemini_thought_signature(signature, opt)
                .map_err(|err| error!("{part_path}: {err}"))?;
        }
    }
    Ok(())
}

/// A `functionCall` waiting for its `functionResponse`.
struct PendingCall {
    id: String,
    name: String,
    path: String,
}

/// `ValidateGeminiFunctionCallPairing`: checks that each group of model
/// `functionCall` parts is answered by matching `functionResponse` parts in a
/// later content, never interleaved with the calls. A final unanswered group is
/// allowed, since a freshly returned model step has no tool output yet.
pub fn validate_gemini_function_call_pairing(payload: &Value) -> Result<(), Error> {
    let (contents, contents_path) = gemini_contents(payload);
    let Some(Value::Array(contents)) = contents else {
        return Ok(());
    };

    let mut pending: Vec<PendingCall> = Vec::new();
    for (i, content) in contents.iter().enumerate() {
        let parts = match content.get("parts") {
            Some(Value::Array(parts)) if !parts.is_empty() => parts,
            _ => {
                if !pending.is_empty() {
                    return Err(error!(
                        "{contents_path}[{i}]: content appears before {} pending functionResponse part(s)",
                        pending.len()
                    ));
                }
                continue;
            }
        };

        let mut calls = Vec::new();
        let mut responses = Vec::new();
        for (j, part) in parts.iter().enumerate() {
            let part_path = format!("{contents_path}[{i}].parts[{j}]");
            if let Some(call) = part.get("functionCall") {
                let name = str_of(call.get("name"));
                if name.is_empty() {
                    return Err(error!("{part_path}: missing functionCall.name"));
                }
                calls.push(PendingCall {
                    id: str_of(call.get("id")).into_owned(),
                    name: name.into_owned(),
                    path: part_path.clone(),
                });
            }
            if let Some(response) = part.get("functionResponse") {
                responses.push((response, part_path));
            }
        }

        if !calls.is_empty() && !responses.is_empty() {
            return Err(error!(
                "{contents_path}[{i}]: functionCall and functionResponse parts must not be interleaved in the same content"
            ));
        }
        if !calls.is_empty() && !pending.is_empty() {
            return Err(error!(
                "{contents_path}[{i}]: functionCall appears before {} pending functionResponse part(s)",
                pending.len()
            ));
        }
        if !calls.is_empty() {
            pending = calls;
            continue;
        }
        if responses.is_empty() {
            // Other content, such as a system reminder or a user turn, may come
            // before the responses. A model turn may not.
            let role = go::to_lower(str_of(content.get("role")).trim());
            if !pending.is_empty() && role == "model" {
                return Err(error!(
                    "{contents_path}[{i}]: model content appears before {} pending functionResponse part(s)",
                    pending.len()
                ));
            }
            continue;
        }
        if pending.is_empty() {
            return Err(error!(
                "{contents_path}[{i}]: functionResponse without preceding functionCall"
            ));
        }
        if responses.len() != pending.len() {
            return Err(error!(
                "{contents_path}[{i}]: functionResponse count {} does not match pending functionCall count {}",
                responses.len(),
                pending.len()
            ));
        }

        for ((response, part_path), call) in responses.iter().zip(&pending) {
            let id = str_of(response.get("id"));
            let name = str_of(response.get("name"));
            if !call.id.is_empty() && id.is_empty() {
                return Err(error!(
                    "{part_path}: missing functionResponse.id for {}",
                    call.path
                ));
            }
            if !call.id.is_empty() && id != call.id {
                return Err(error!(
                    "{part_path}: functionResponse.id {} does not match functionCall.id {} at {}",
                    quote(&id),
                    quote(&call.id),
                    call.path
                ));
            }
            if name.is_empty() {
                return Err(error!("{part_path}: missing functionResponse.name"));
            }
            if !call.name.is_empty() && name != call.name {
                return Err(error!(
                    "{part_path}: functionResponse.name {} does not match functionCall.name {} at {}",
                    quote(&name),
                    quote(&call.name),
                    call.path
                ));
            }
        }
        pending.clear();
    }
    Ok(())
}

/// Standard base64, padded or not.
fn decode(sig: &str) -> Result<Vec<u8>, Error> {
    if sig.len() > MAX_GEMINI_THOUGHT_SIGNATURE_LEN {
        return Err(error!(
            "Gemini thought signature exceeds maximum length ({MAX_GEMINI_THOUGHT_SIGNATURE_LEN} bytes)"
        ));
    }
    STD.decode(sig).or_else(|err| {
        RAW_STD
            .decode(sig)
            .map_err(|_| error!("invalid Gemini thought signature: base64 decode failed: {err}"))
    })
}

fn classify_envelope(decoded: &[u8]) -> (GeminiEnvelope, bool) {
    if is_canonical_uuid(decoded) {
        (GeminiEnvelope::AsciiUuid, false)
    } else if field2_envelope_payload_len(decoded).is_some() {
        (GeminiEnvelope::ProtobufField2, true)
    } else {
        (GeminiEnvelope::Unknown, false)
    }
}

/// The length of the payload in a field-2 envelope, if `decoded` is one.
fn field2_envelope_payload_len(decoded: &[u8]) -> Option<usize> {
    let value = field2_field1_value(decoded)?;
    let recognized = is_likely_tink_payload(value)
        || is_canonical_uuid(value)
        || is_likely_tool_invocation_payload(value);
    // Every recognized payload is non-empty.
    recognized.then_some(value.len())
}

/// The value of field 1 inside field 2, when those are the only records.
fn field2_field1_value(decoded: &[u8]) -> Option<&[u8]> {
    let container = only_bytes_field(decoded, 2)?;
    only_bytes_field(container, 1)
}

/// The value of `msg` when it is exactly one bytes field numbered `num`.
fn only_bytes_field(msg: &[u8], num: protowire::Number) -> Option<&[u8]> {
    let (field, typ, n) = consume_tag(msg).ok()?;
    if field != num || typ != BYTES_TYPE {
        return None;
    }
    let (value, m) = consume_bytes(&msg[n..]).ok()?;
    (n + m == msg.len()).then_some(value)
}

/// A Google Tink primitive output: the prefix-type byte `0x01`, then a key id
/// and ciphertext. Only the format byte is checked; the key id rotates.
fn is_likely_tink_payload(value: &[u8]) -> bool {
    value.first() == Some(&0x01)
}

/// A server-side tool block (`toolCall` or `toolResponse`): a protobuf message
/// with at least one bytes field holding a Tink payload.
fn is_likely_tool_invocation_payload(value: &[u8]) -> bool {
    let mut offset = 0;
    let mut has_tink_field = false;
    while offset < value.len() {
        let Ok((_, typ, n)) = consume_tag(&value[offset..]) else {
            return false;
        };
        offset += n;
        let rest = &value[offset..];
        let consumed = match typ {
            VARINT_TYPE => protowire::consume_varint(rest).map(|(_, n)| n),
            BYTES_TYPE => consume_bytes(rest).map(|(bytes, n)| {
                has_tink_field |= is_likely_tink_payload(bytes);
                n
            }),
            FIXED32_TYPE => protowire::consume_fixed32(rest).map(|(_, n)| n),
            FIXED64_TYPE => protowire::consume_fixed64(rest).map(|(_, n)| n),
            _ => return false,
        };
        let Ok(n) = consumed else {
            return false;
        };
        offset += n;
    }
    has_tink_field
}

/// The request's `contents`, or `request.contents` when there is none, with
/// the path used in error messages.
fn gemini_contents(payload: &Value) -> (Option<&Value>, &'static str) {
    match payload.get("contents") {
        Some(contents) => (Some(contents), "contents"),
        None => (json::path(payload, "request.contents"), "request.contents"),
    }
}
