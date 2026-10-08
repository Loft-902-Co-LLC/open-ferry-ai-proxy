// Ported from the tests in CLIProxyAPI internal/signature (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Each submodule ports the upstream test file of the same name. This module
//! holds the fixtures they share.
//!
//! Tests that need upstream's captured corpora, which aren't in its repository,
//! are not ported, and neither are tests of its debug logging.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};

use super::*;
use crate::protowire;

mod claude;
mod claude_antigravity_boundaries;
mod claude_antigravity_validation;
mod claude_messages_sanitize;
mod gemini_sanitize;
mod gemini_validation;
mod gpt_validation;
mod grok_validation;
mod kimi_validation;
mod observed;
mod provider_compatibility;
mod signaturetest;

pub(crate) use observed::*;

/// Builds a protobuf message field by field, as upstream's tests do with
/// protowire's `Append*` functions.
#[derive(Clone, Default)]
struct Pb(Vec<u8>);

impl Pb {
    fn new() -> Self {
        Self::default()
    }

    /// Appends a raw varint.
    fn uvarint(mut self, mut value: u64) -> Self {
        while value >= 0x80 {
            self.0.push(value as u8 | 0x80);
            value >>= 7;
        }
        self.0.push(value as u8);
        self
    }

    fn tag(self, field: u64, wire_type: protowire::Type) -> Self {
        self.uvarint(field << 3 | u64::from(wire_type))
    }

    /// Appends raw bytes.
    fn raw(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    fn varint(self, field: u64, value: u64) -> Self {
        self.tag(field, protowire::VARINT_TYPE).uvarint(value)
    }

    fn bytes(self, field: u64, value: &[u8]) -> Self {
        self.tag(field, protowire::BYTES_TYPE)
            .uvarint(value.len() as u64)
            .raw(value)
    }

    fn string(self, field: u64, value: &str) -> Self {
        self.bytes(field, value.as_bytes())
    }

    fn build(self) -> Vec<u8> {
        self.0
    }
}

/// The smallest well-formed GPT reasoning signature, padded.
pub(crate) fn valid_codex_reasoning_signature() -> String {
    let mut raw = [0u8; 1 + 8 + 16 + 16 + 32];
    raw[0] = 0x80;
    raw[8] = 1;
    URL_SAFE.encode(raw)
}

/// `testGPTReasoningSignature`: a GPT reasoning signature, unpadded.
fn test_gpt_reasoning_signature() -> String {
    let mut payload = [0u8; 1 + 8 + 16 + 16 + 32];
    payload[0] = 0x80;
    for (i, byte) in payload.iter_mut().enumerate().skip(9) {
        *byte = i as u8;
    }
    URL_SAFE_NO_PAD.encode(payload)
}

/// The channel block of a classic Claude signature from claude-sonnet-4-6.
fn claude_channel_payload() -> Pb {
    let channel_block = Pb::new()
        .varint(1, 12)
        .varint(2, 2)
        .string(6, "claude-sonnet-4-6")
        .build();
    let container = Pb::new().bytes(1, &channel_block).build();
    Pb::new().bytes(2, &container).varint(3, 1)
}

/// `testClaudeThinkingSignature`: a single-layer E signature.
fn test_claude_thinking_signature() -> String {
    STANDARD.encode(claude_channel_payload().build())
}

/// `testClaudeThinkingSignatureWithOpaqueLen`: a single-layer E signature with
/// `opaque_len` bytes of opaque data in field 4, which sets the padding.
fn test_claude_thinking_signature_with_opaque_len(opaque_len: usize) -> String {
    let opaque: Vec<u8> = (0..opaque_len)
        .map(|i| ((i * 41 + 17) % 251) as u8)
        .collect();
    STANDARD.encode(claude_channel_payload().bytes(4, &opaque).build())
}

/// `testUnpaddedClaudeThinkingSignature`.
fn test_unpadded_claude_thinking_signature() -> String {
    test_claude_thinking_signature_with_opaque_len(35)
}

/// `testUnpaddedAntigravityClaudeThinkingSignature`: a double-layer R
/// signature with no padding.
fn test_unpadded_antigravity_claude_thinking_signature() -> String {
    STANDARD.encode(test_claude_thinking_signature_with_opaque_len(41))
}

/// `claudeCAISParts`: builds a Claude CAIS signature field by field, so tests
/// can cover both the observed layout and drift from it.
#[derive(Clone)]
struct ClaudeCaisParts {
    include_top_envelope: bool,
    top_envelope: u64,
    include_top_trailer: bool,
    include_container: bool,
    include_channel_block: bool,
    include_channel_id: bool,
    channel_id: u64,
    channel_id_as_bytes: bool,
    include_channel_version: bool,
    include_signature: bool,
    signature_len: usize,
    include_model_text: bool,
    model_text: Vec<u8>,
    include_field7: bool,
    block_kind: String,
    context_id: String,
}

impl ClaudeCaisParts {
    /// `defaultClaudeCAISParts`: the layout observed on claude-fable-5 and
    /// claude-opus-5.
    fn new(model: &str) -> Self {
        Self {
            include_top_envelope: true,
            top_envelope: 2,
            include_top_trailer: true,
            include_container: true,
            include_channel_block: true,
            include_channel_id: true,
            channel_id: 16,
            channel_id_as_bytes: false,
            include_channel_version: true,
            include_signature: true,
            signature_len: 64,
            include_model_text: true,
            model_text: model.as_bytes().to_vec(),
            include_field7: true,
            block_kind: "thinking".to_owned(),
            context_id: OBSERVED_CONTEXT_ID.to_owned(),
        }
    }

    fn encode(&self) -> String {
        let mut channel_block = Pb::new();
        if self.include_channel_id {
            channel_block = if self.channel_id_as_bytes {
                channel_block.bytes(1, &[0x10])
            } else {
                channel_block.varint(1, self.channel_id)
            };
        }
        if self.include_channel_version {
            channel_block = channel_block.varint(3, 2);
        }
        if self.include_signature {
            channel_block = channel_block.bytes(5, &vec![0; self.signature_len]);
        }
        if self.include_model_text {
            channel_block = channel_block.bytes(6, &self.model_text);
        }
        if self.include_field7 {
            channel_block = channel_block.varint(7, 1);
        }
        if !self.block_kind.is_empty() {
            channel_block = channel_block.string(8, &self.block_kind);
        }
        if !self.context_id.is_empty() {
            channel_block = channel_block.string(11, &self.context_id);
        }

        let mut container = Pb::new();
        if self.include_channel_block {
            container = container.bytes(1, &channel_block.build());
        }

        let mut payload = Pb::new();
        if self.include_top_envelope {
            payload = payload.varint(1, self.top_envelope);
        }
        if self.include_container {
            payload = payload.bytes(2, &container.build());
        }
        if self.include_top_trailer {
            payload = payload.varint(3, 1);
        }
        STANDARD.encode(payload.build())
    }
}

/// `testClaudeCAISSignature`.
fn test_claude_cais_signature(model: &str) -> String {
    ClaudeCaisParts::new(model).encode()
}

/// `testUnpaddedClaudeCAISSignature`: a CAIS signature with no `=` padding,
/// found by varying the length of the model text.
fn test_unpadded_claude_cais_signature() -> String {
    (0..8)
        .map(|suffix| test_claude_cais_signature(&format!("claude-opus-5{}", "x".repeat(suffix))))
        .find(|sample| !sample.contains('='))
        .expect("an unpadded Claude CAIS fixture")
}

/// `testGeminiThoughtSignature`.
fn test_gemini_thought_signature(payload: &[u8]) -> String {
    STANDARD.encode(payload)
}

/// `testGemini25ThoughtSignature`: the retired Gemini 2.5 envelope, a repeated
/// field 1.
fn test_gemini25_thought_signature(records: &[&[u8]]) -> String {
    let payload = records
        .iter()
        .fold(Pb::new(), |payload, record| payload.bytes(1, record));
    test_gemini_thought_signature(&payload.build())
}

/// `testGemini3ThoughtSignature`: the Gemini 3 envelope, field 2 holding field 1.
fn test_gemini3_thought_signature(payload: &[u8]) -> String {
    let inner = Pb::new().bytes(1, payload).build();
    test_gemini_thought_signature(&Pb::new().bytes(2, &inner).build())
}

/// `testGeminiThoughtSignatureEnvelope`: an unpadded Gemini 3 envelope.
fn test_gemini_thought_signature_envelope() -> String {
    let payload: Vec<u8> = [0x01, 0x0c].into_iter().chain(0..97).collect();
    let inner = Pb::new().bytes(1, &payload).build();
    STANDARD_NO_PAD.encode(Pb::new().bytes(2, &inner).build())
}

/// `testGemini25Field1ThoughtSignatureEnvelope`: an unpadded Gemini 2.5
/// envelope.
fn test_gemini25_field1_thought_signature_envelope() -> String {
    let payload: Vec<u8> = std::iter::once(0x01)
        .chain((0..127).map(|i| ((i * 37 + 11) % 251) as u8))
        .collect();
    STANDARD_NO_PAD.encode(Pb::new().bytes(1, &payload).build())
}

/// `synthesizeKimiSignature`: `decoded_len` pseudo-random bytes from `seed`,
/// unpadded. Upstream uses Go's `math/rand`; any uniform source will do.
fn synthesize_kimi_signature(decoded_len: usize, seed: u64) -> String {
    // SplitMix64.
    let mut state = seed;
    let buf: Vec<u8> = (0..decoded_len)
        .map(|_| {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) as u8
        })
        .collect();
    STANDARD_NO_PAD.encode(buf)
}

/// Parses a JSON fixture.
fn json(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).expect("valid JSON fixture")
}
