// Ported from CLIProxyAPI internal/signature/claude_validation.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude thinking signatures.
//!
//! Classic signatures are base64 in one or two layers, told apart by the first
//! character: `E` is single-layer (the decoded payload starts with `0x12`), and
//! `R` is double-layer (it decodes to an `E` signature). Strict validation also
//! walks the protobuf tree:
//!
//! ```text
//! field 2 (bytes): container
//!   field 1 (bytes): channel block
//!     field 1 (varint): channel_id, required
//!     field 2 (varint): infrastructure (1 = AWS, 2 = Google)
//!     field 6 (bytes):  model text, UTF-8
//!     field 7 (varint)
//! ```
//!
//! Newer models wrap the channel block in a CAIS envelope instead, whose payload
//! starts with `0x08` (field 1, the envelope version), so the base64 starts
//! with `C`. Its channel block requires signature bytes in field 5 and a
//! `claude-` model text in field 6. From envelope version 4 (CAQS) the signature
//! bytes may sit in container field 5 instead, the model text is omitted, and
//! the block kind in field 8 must be `thinking` or `narration`.
//!
//! Antigravity replays Claude signatures only in R form; Claude itself takes E.

use serde_json::Value;

use super::Error;
use crate::go::base64::STD;
use crate::go::quote;
use crate::json::str_of;
use crate::protowire::{self, BYTES_TYPE, Number, Type, VARINT_TYPE};

pub const MAX_CLAUDE_THINKING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

/// How far Claude thinking signatures are inspected. By default the cache
/// prefix, the base64 layers and the decoded `0x12` marker are checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClaudeValidationOptions {
    /// Only check for an `E` or `R` signature after an optional cache prefix.
    pub prefix_only: bool,
    /// Check the prefix and that the base64 layers decode, but not the payload.
    pub base64_only: bool,
    /// When stripping blocks, keep a thinking placeholder with no signature and
    /// no text.
    pub allow_empty_signature_with_empty_text: bool,
    /// Also check the protobuf tree.
    pub strict: bool,
}

impl ClaudeValidationOptions {
    /// Strict validation, which also checks the protobuf tree.
    pub const STRICT: Self = Self {
        prefix_only: false,
        base64_only: false,
        allow_empty_signature_with_empty_text: false,
        strict: true,
    };
}

/// The protobuf fields of a classic Claude signature that describe its routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeSignatureTree {
    pub encoding_layers: u32,
    pub channel_id: u64,
    pub field2: Option<u64>,
    pub routing_class: &'static str,
    pub infrastructure_class: &'static str,
    pub schema_features: &'static str,
    pub model_text: String,
    pub legacy_route_hint: &'static str,
    pub has_field7: bool,
}

/// The structure of a Claude CAIS signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeCaisSignatureInfo {
    pub first_byte: u8,
    pub envelope_version: u64,
    pub channel_id: u64,
    pub model_text: String,
    pub block_kind: String,
    pub context_id: String,
    pub signature_len: usize,
}

/// `IsValidClaudeThinkingSignature`.
pub fn is_valid_claude_thinking_signature(raw: &str, opt: ClaudeValidationOptions) -> bool {
    if opt.prefix_only {
        return has_claude_thinking_signature_prefix(raw);
    }
    if opt.base64_only {
        return has_decodable_claude_thinking_signature(raw);
    }
    checked_signature(raw, opt).is_ok()
}

/// `HasDecodableClaudeThinkingSignature`: whether `raw` is an `E` or `R`
/// signature whose base64 layers decode.
pub fn has_decodable_claude_thinking_signature(raw: &str) -> bool {
    let sig = strip_claude_signature_prefix(raw);
    if sig.is_empty() || sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return false;
    }
    match sig.as_bytes()[0] {
        b'E' => STD.decode(sig).is_ok_and(|decoded| !decoded.is_empty()),
        b'R' => match STD.decode(sig) {
            Ok(decoded) if decoded.first() == Some(&b'E') => {
                STD.decode(&decoded).is_ok_and(|inner| !inner.is_empty())
            }
            _ => false,
        },
        _ => false,
    }
}

/// `HasClaudeThinkingSignaturePrefix`: whether `raw` starts with `E` or `R`
/// after an optional cache prefix.
pub fn has_claude_thinking_signature_prefix(raw: &str) -> bool {
    matches!(
        strip_claude_signature_prefix(raw).as_bytes().first(),
        Some(b'E' | b'R')
    )
}

/// Trims `raw` and drops anything up to the first `#`. Unlike
/// [`super::split_signature_provider_prefix`], any prefix is accepted.
fn strip_claude_signature_prefix(raw: &str) -> &str {
    let sig = raw.trim();
    match sig.find('#') {
        Some(index) => sig[index + 1..].trim(),
        None => sig,
    }
}

/// `ValidateClaudeThinkingSignatures`: checks the signature of every thinking
/// block in a Claude Messages request.
pub fn validate_claude_thinking_signatures(
    payload: &Value,
    opt: ClaudeValidationOptions,
) -> Result<(), Error> {
    let Some(Value::Array(messages)) = payload.get("messages") else {
        return Ok(());
    };
    for (i, message) in messages.iter().enumerate() {
        let Some(Value::Array(parts)) = message.get("content") else {
            continue;
        };
        for (j, part) in parts.iter().enumerate() {
            if str_of(part.get("type")) != "thinking" {
                continue;
            }
            let signature = str_of(part.get("signature"));
            let signature = signature.trim();
            if signature.is_empty() {
                return Err(error!(
                    "messages[{i}].content[{j}]: missing thinking signature"
                ));
            }
            checked_signature(signature, opt)
                .map_err(|err| error!("messages[{i}].content[{j}]: {err}"))?;
        }
    }
    Ok(())
}

/// `NormalizeClaudeThinkingSignature`: validates `raw` and returns it in the
/// double-layer R form, without its cache prefix.
pub fn normalize_claude_thinking_signature(
    raw: &str,
    opt: ClaudeValidationOptions,
) -> Result<String, Error> {
    use base64::Engine;

    let sig = checked_signature(raw, opt)?;
    Ok(if sig.starts_with('R') {
        sig.to_owned()
    } else {
        base64::engine::general_purpose::STANDARD.encode(sig)
    })
}

/// `NormalizeClaudeProviderNativeThinkingSignature`: validates `raw` and
/// returns it in the single-layer E form that Claude itself expects.
pub fn normalize_claude_provider_native_thinking_signature(
    raw: &str,
    opt: ClaudeValidationOptions,
) -> Result<String, Error> {
    let sig = checked_signature(raw, opt)?;
    if !sig.starts_with('R') {
        return Ok(sig.to_owned());
    }
    // Validation decoded both layers, so the inner layer is base64 text.
    let decoded = STD.decode(sig).unwrap_or_default();
    Ok(String::from_utf8_lossy(&decoded).into_owned())
}

/// Validates an E or R signature and returns it without its cache prefix.
fn checked_signature(raw: &str, opt: ClaudeValidationOptions) -> Result<&str, Error> {
    let sig = strip_claude_signature_prefix(raw);
    if sig.is_empty() {
        return Err(error!("empty signature"));
    }
    if sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return Err(error!(
            "signature exceeds maximum length ({MAX_CLAUDE_THINKING_SIGNATURE_LEN} bytes)"
        ));
    }
    match sig.as_bytes()[0] {
        b'R' => validate_double_layer(sig, opt)?,
        b'E' => validate_single_layer(sig.as_bytes(), 1, opt)?,
        first => {
            return Err(error!(
                "invalid signature: expected 'E' or 'R' prefix, got {}",
                quote_byte(first)
            ));
        }
    }
    Ok(sig)
}

/// `%q` of `string(b)`: Go converts the byte to the character with that code point.
fn quote_byte(b: u8) -> String {
    quote(char::from(b).encode_utf8(&mut [0; 2]))
}

fn validate_double_layer(sig: &str, opt: ClaudeValidationOptions) -> Result<(), Error> {
    let inner = decode_double_layer(sig)?;
    validate_single_layer(&inner, 2, opt)
}

/// Decodes the outer layer of an R signature, which must hold an E signature.
fn decode_double_layer(sig: &str) -> Result<Vec<u8>, Error> {
    let decoded = STD
        .decode(sig)
        .map_err(|err| error!("invalid double-layer signature: base64 decode failed: {err}"))?;
    match decoded.first() {
        None => Err(error!("invalid double-layer signature: empty after decode")),
        Some(&first) if first != b'E' => Err(error!(
            "invalid double-layer signature: inner does not start with 'E', got 0x{first:02x}"
        )),
        Some(_) => Ok(decoded),
    }
}

fn validate_single_layer(
    sig: &[u8],
    encoding_layers: u32,
    opt: ClaudeValidationOptions,
) -> Result<(), Error> {
    let decoded = decode_single_layer(sig)?;
    if decoded[0] != 0x12 {
        return Err(error!(
            "invalid Claude signature: expected first byte 0x12, got 0x{:02x}",
            decoded[0]
        ));
    }
    if opt.strict {
        inspect_claude_signature_payload(&decoded, encoding_layers)?;
    }
    Ok(())
}

/// Decodes a single base64 layer, which must not be empty.
fn decode_single_layer(sig: &[u8]) -> Result<Vec<u8>, Error> {
    let decoded = STD
        .decode(sig)
        .map_err(|err| error!("invalid single-layer signature: base64 decode failed: {err}"))?;
    if decoded.is_empty() {
        return Err(error!("invalid single-layer signature: empty after decode"));
    }
    Ok(decoded)
}

/// `InspectClaudeDoubleLayerSignature`: decodes and inspects an R signature.
pub fn inspect_claude_double_layer_signature(sig: &str) -> Result<ClaudeSignatureTree, Error> {
    let inner = decode_double_layer(sig)?;
    inspect_claude_signature_payload(&decode_single_layer(&inner)?, 2)
}

/// `InspectClaudeSingleLayerSignature`: decodes and inspects an E signature.
pub fn inspect_claude_single_layer_signature(sig: &str) -> Result<ClaudeSignatureTree, Error> {
    inspect_claude_signature_payload(&decode_single_layer(sig.as_bytes())?, 1)
}

/// `InspectClaudeSignaturePayload`: inspects the decoded protobuf payload.
pub fn inspect_claude_signature_payload(
    payload: &[u8],
    encoding_layers: u32,
) -> Result<ClaudeSignatureTree, Error> {
    match payload.first() {
        None => return Err(error!("invalid Claude signature: empty payload")),
        Some(&first) if first != 0x12 => {
            return Err(error!(
                "invalid Claude signature: expected first byte 0x12, got 0x{first:02x}"
            ));
        }
        Some(_) => {}
    }
    let container = extract_bytes_field(payload, 2, "top-level protobuf")?;
    let channel_block = extract_bytes_field(container, 1, "Claude Field 2 container")?;
    inspect_channel_block(channel_block, encoding_layers)
}

fn inspect_channel_block(
    channel_block: &[u8],
    encoding_layers: u32,
) -> Result<ClaudeSignatureTree, Error> {
    let mut channel_id = None;
    let mut field2 = None;
    let mut model_text = None;
    let mut has_field7 = false;
    walk_fields(channel_block, |num, typ, raw| {
        match num {
            1 => {
                if typ != VARINT_TYPE {
                    return Err(error!(
                        "invalid Claude signature: Field 2.1.1 channel_id must be varint"
                    ));
                }
                channel_id = Some(decode_varint(raw, "Field 2.1.1 channel_id")?);
            }
            2 => {
                if typ != VARINT_TYPE {
                    return Err(error!(
                        "invalid Claude signature: Field 2.1.2 field2 must be varint"
                    ));
                }
                field2 = Some(decode_varint(raw, "Field 2.1.2 field2")?);
            }
            6 => {
                if typ != BYTES_TYPE {
                    return Err(error!(
                        "invalid Claude signature: Field 2.1.6 model_text must be bytes"
                    ));
                }
                let bytes = decode_bytes(raw, "Field 2.1.6 model_text")?;
                let text = std::str::from_utf8(bytes).map_err(|_| {
                    error!("invalid Claude signature: Field 2.1.6 model_text is not valid UTF-8")
                })?;
                model_text = Some(text.to_owned());
            }
            7 => {
                if typ != VARINT_TYPE {
                    return Err(error!(
                        "invalid Claude signature: Field 2.1.7 must be varint"
                    ));
                }
                decode_varint(raw, "Field 2.1.7")?;
                has_field7 = true;
            }
            _ => {}
        }
        Ok(())
    })?;
    let Some(channel_id) = channel_id else {
        return Err(error!(
            "invalid Claude signature: missing Field 2.1.1 channel_id"
        ));
    };

    let routing_class = match channel_id {
        11 => "routing_class_11",
        12 => "routing_class_12",
        _ => "unknown",
    };
    let infrastructure_class = match field2 {
        None => "infra_default",
        Some(1) => "infra_aws",
        Some(2) => "infra_google",
        Some(_) => "infra_unknown",
    };
    let schema_features = if model_text.is_some() {
        "extended_model_tagged_schema"
    } else if !has_field7 && (70..=72).contains(&channel_block.len()) {
        "compact_schema"
    } else {
        "unknown_schema_features"
    };
    let legacy_route_hint = match (channel_id, field2) {
        (11, None) => "legacy_default_group",
        (11, Some(1)) => "legacy_aws_group",
        (11, Some(2)) if encoding_layers == 2 => "legacy_vertex_direct",
        (11, Some(2)) if encoding_layers == 1 => "legacy_vertex_proxy",
        _ => "",
    };
    Ok(ClaudeSignatureTree {
        encoding_layers,
        channel_id,
        field2,
        routing_class,
        infrastructure_class,
        schema_features,
        model_text: model_text.unwrap_or_default(),
        legacy_route_hint,
        has_field7,
    })
}

/// The last `field_num` field of `msg`, which must be bytes.
fn extract_bytes_field<'m>(
    msg: &'m [u8],
    field_num: Number,
    scope: &str,
) -> Result<&'m [u8], Error> {
    let mut value = None;
    walk_fields(msg, |num, typ, raw| {
        if num != field_num {
            return Ok(());
        }
        if typ != BYTES_TYPE {
            return Err(error!(
                "invalid Claude signature: {scope} field {field_num} must be bytes"
            ));
        }
        value = Some(decode_bytes(raw, &format!("{scope} field {field_num}"))?);
        Ok(())
    })?;
    value.ok_or_else(|| error!("invalid Claude signature: missing {scope} field {field_num}"))
}

/// Calls `visit` with each field's number, wire type and value bytes.
fn walk_fields<'m>(
    msg: &'m [u8],
    mut visit: impl FnMut(Number, Type, &'m [u8]) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut offset = 0;
    while offset < msg.len() {
        let (num, typ, n) = protowire::consume_tag(&msg[offset..])
            .map_err(|err| error!("invalid Claude signature: malformed protobuf tag: {err}"))?;
        offset += n;
        let len = protowire::consume_field_value(num, typ, &msg[offset..]).map_err(|err| {
            error!("invalid Claude signature: malformed protobuf field {num}: {err}")
        })?;
        visit(num, typ, &msg[offset..offset + len])?;
        offset += len;
    }
    Ok(())
}

fn decode_varint(raw: &[u8], label: &str) -> Result<u64, Error> {
    protowire::consume_varint(raw)
        .map(|(value, _)| value)
        .map_err(|err| error!("invalid Claude signature: failed to decode {label}: {err}"))
}

fn decode_bytes<'r>(raw: &'r [u8], label: &str) -> Result<&'r [u8], Error> {
    protowire::consume_bytes(raw)
        .map(|(value, _)| value)
        .map_err(|err| error!("invalid Claude signature: failed to decode {label}: {err}"))
}

/// The decoded first byte of a CAIS envelope: field 1, varint.
const CAIS_MARKER: u8 = 0x08;
/// Distinguishes a CAIS channel block from an arbitrary protobuf payload.
const CAIS_MODEL_TEXT_PREFIX: &str = "claude-";

/// `IsValidClaudeCAISSignature`.
pub fn is_valid_claude_cais_signature(raw: &str) -> bool {
    inspect_claude_cais_signature(raw).is_ok()
}

/// `InspectClaudeCAISSignature`: decodes and checks a CAIS signature. Only the
/// fields that identify the format are required, since rejecting a signature
/// drops the whole thinking block.
pub fn inspect_claude_cais_signature(raw: &str) -> Result<ClaudeCaisSignatureInfo, Error> {
    let sig = strip_claude_signature_prefix(raw);
    if sig.is_empty() {
        return Err(error!("empty signature"));
    }
    if sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return Err(error!(
            "signature exceeds maximum length ({MAX_CLAUDE_THINKING_SIGNATURE_LEN} bytes)"
        ));
    }
    // A payload starting with 0x08 always encodes to a leading `C`, so other
    // signatures are rejected without decoding.
    let first = sig.as_bytes()[0];
    if first != b'C' {
        return Err(error!(
            "invalid Claude CAIS signature: expected 'C' prefix, got {}",
            quote_byte(first)
        ));
    }

    let decoded = STD
        .decode(sig)
        .map_err(|err| error!("invalid Claude CAIS signature: base64 decode failed: {err}"))?;
    match decoded.first() {
        None => return Err(error!("invalid Claude CAIS signature: empty after decode")),
        Some(&first) if first != CAIS_MARKER => {
            return Err(error!(
                "invalid Claude CAIS signature: expected first byte 0x{CAIS_MARKER:02x}, got 0x{first:02x}"
            ));
        }
        Some(_) => {}
    }

    let mut info = ClaudeCaisSignatureInfo {
        first_byte: decoded[0],
        envelope_version: 0,
        channel_id: 0,
        model_text: String::new(),
        block_kind: String::new(),
        context_id: String::new(),
        signature_len: 0,
    };

    let mut container = None;
    walk_fields(&decoded, |num, typ, raw| {
        match num {
            1 => {
                info.envelope_version =
                    cais_varint(raw, typ, "CAIS top-level field 1 envelope version")?;
            }
            2 => container = Some(cais_bytes(raw, typ, "CAIS top-level field 2 container")?),
            3 => {
                cais_varint(raw, typ, "CAIS top-level field 3 trailer")?;
            }
            _ => {}
        }
        Ok(())
    })?;
    let Some(container) = container else {
        return Err(error!(
            "invalid Claude CAIS signature: missing top-level field 2 container"
        ));
    };

    let mut channel_block = None;
    let mut container_signature: &[u8] = &[];
    walk_fields(container, |num, typ, raw| {
        match num {
            1 => {
                channel_block = Some(cais_bytes(
                    raw,
                    typ,
                    "CAIS container field 1 channel block",
                )?);
            }
            5 => {
                container_signature =
                    cais_bytes(raw, typ, "CAIS container field 5 signature bytes")?;
            }
            _ => {}
        }
        Ok(())
    })?;
    let Some(channel_block) = channel_block else {
        return Err(error!(
            "invalid Claude CAIS signature: missing container field 1 channel block"
        ));
    };

    let mut have_channel_id = false;
    let mut have_signature = false;
    let mut have_model_text = false;
    walk_fields(channel_block, |num, typ, raw| {
        match num {
            1 => {
                info.channel_id = cais_varint(raw, typ, "CAIS channel field 1 channel_id")?;
                have_channel_id = true;
            }
            3 => {
                cais_varint(raw, typ, "CAIS channel field 3 version")?;
            }
            5 => {
                let value = cais_bytes(raw, typ, "CAIS channel field 5 signature bytes")?;
                if value.is_empty() {
                    return Err(error!(
                        "invalid Claude CAIS signature: channel field 5 signature bytes must not be empty"
                    ));
                }
                info.signature_len = value.len();
                have_signature = true;
            }
            6 => {
                let value = cais_utf8(raw, typ, "CAIS channel field 6 model_text")?;
                if !value.starts_with(CAIS_MODEL_TEXT_PREFIX) {
                    return Err(error!(
                        "invalid Claude CAIS signature: channel field 6 model_text must start with {}, got {}",
                        quote(CAIS_MODEL_TEXT_PREFIX),
                        quote(value)
                    ));
                }
                info.model_text = value.to_owned();
                have_model_text = true;
            }
            7 => {
                cais_varint(raw, typ, "CAIS channel field 7")?;
            }
            8 => {
                info.block_kind = cais_utf8(raw, typ, "CAIS channel field 8 block kind")?.to_owned()
            }
            11 => {
                let value = cais_utf8(raw, typ, "CAIS channel field 11 context id")?;
                if !is_canonical_uuid(value.as_bytes()) {
                    return Err(error!(
                        "invalid Claude CAIS signature: channel field 11 context id must be a canonical UUID, got {}",
                        quote(value)
                    ));
                }
                info.context_id = value.to_owned();
            }
            _ => {}
        }
        Ok(())
    })?;
    if !have_signature && info.envelope_version >= 4 && !container_signature.is_empty() {
        info.signature_len = container_signature.len();
        have_signature = true;
    }
    if !have_channel_id {
        return Err(error!(
            "invalid Claude CAIS signature: missing channel field 1 channel_id"
        ));
    }
    if !have_signature {
        return Err(error!(
            "invalid Claude CAIS signature: missing signature bytes"
        ));
    }
    if !have_model_text && info.envelope_version < 4 {
        return Err(error!(
            "invalid Claude CAIS signature: missing channel field 6 model_text"
        ));
    }
    if info.envelope_version >= 4 && info.block_kind != "thinking" && info.block_kind != "narration"
    {
        return Err(error!(
            "invalid Claude CAQS signature: expected block kind \"thinking\" or \"narration\", got {}",
            quote(&info.block_kind)
        ));
    }
    Ok(info)
}

fn cais_varint(raw: &[u8], typ: Type, label: &str) -> Result<u64, Error> {
    if typ != VARINT_TYPE {
        return Err(error!(
            "invalid Claude CAIS signature: {label} must be varint"
        ));
    }
    decode_varint(raw, label)
}

fn cais_bytes<'r>(raw: &'r [u8], typ: Type, label: &str) -> Result<&'r [u8], Error> {
    if typ != BYTES_TYPE {
        return Err(error!(
            "invalid Claude CAIS signature: {label} must be bytes"
        ));
    }
    decode_bytes(raw, label)
}

fn cais_utf8<'r>(raw: &'r [u8], typ: Type, label: &str) -> Result<&'r str, Error> {
    std::str::from_utf8(cais_bytes(raw, typ, label)?)
        .map_err(|_| error!("invalid Claude CAIS signature: {label} must be valid UTF-8"))
}

/// Whether `s` is a UUID in 8-4-4-4-12 hex form, in either case.
pub(super) fn is_canonical_uuid(s: &[u8]) -> bool {
    s.len() == 36
        && s.iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}
