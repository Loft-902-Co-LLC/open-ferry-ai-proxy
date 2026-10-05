// Ported from CLIProxyAPI internal/signature (v8.0.15, MIT); this file from
// provider_compatibility.go. https://github.com/router-for-me/CLIProxyAPI

//! Reasoning-signature validation and replay decisions.
//!
//! Clients send back the signed reasoning a model returned: Claude thinking
//! signatures, Gemini thought signatures, GPT and Grok `encrypted_content`, and
//! Kimi thinking signatures. Each provider verifies only its own, so before
//! history is replayed to a target provider every signature is classified and
//! kept, dropped, or swapped for Gemini's documented bypass sentinel.
//!
//! Validation is structural. It checks each provider's transport envelope
//! (base64 layers, protobuf framing, lengths, byte entropy) and cannot prove a
//! signature verifies. Error messages match upstream's.
//!
//! Upstream logs debug lines when it sanitizes Gemini signatures. They are not
//! ported.

use std::fmt;

/// Builds an [`Error`] from a format string.
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::signature::Error(format!($($arg)*))
    };
}

mod claude;
mod claude_antigravity_validation;
mod claude_messages_sanitize;
mod claude_validation;
mod gemini_sanitize;
mod gemini_validation;
mod gpt_validation;
mod grok_validation;
mod kimi_validation;

pub use claude::{
    strip_invalid_claude_thinking_blocks, strip_invalid_claude_thinking_blocks_and_empty_messages,
};
pub use claude_antigravity_validation::inspect_antigravity_claude_caqs_signature;
pub use claude_messages_sanitize::{
    ClaudeMessagesSanitizeOptions, SanitizeReport, sanitize_claude_messages_for_claude_upstream,
    sanitize_claude_messages_signatures_for_model, sanitize_claude_messages_signatures_for_target,
};
pub use claude_validation::{
    ClaudeCaisSignatureInfo, ClaudeSignatureTree, ClaudeValidationOptions,
    MAX_CLAUDE_THINKING_SIGNATURE_LEN, has_claude_thinking_signature_prefix,
    has_decodable_claude_thinking_signature, inspect_claude_cais_signature,
    inspect_claude_double_layer_signature, inspect_claude_signature_payload,
    inspect_claude_single_layer_signature, is_valid_claude_cais_signature,
    is_valid_claude_thinking_signature, normalize_claude_provider_native_thinking_signature,
    normalize_claude_thinking_signature, validate_claude_thinking_signatures,
};
pub use gemini_sanitize::{
    gemini_replay_signature_or_bypass, sanitize_gemini_request_thought_signatures,
};
pub use gemini_validation::{
    GEMINI_CONTEXT_ENGINEERING_BYPASS, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, GeminiEnvelope,
    GeminiThoughtSignatureInfo, GeminiValidationOptions, MAX_GEMINI_THOUGHT_SIGNATURE_LEN,
    inspect_gemini_thought_signature, is_gemini_thought_signature_bypass,
    is_valid_gemini_thought_signature, validate_gemini_function_call_pairing,
    validate_gemini_thought_signatures,
};
pub use gpt_validation::{
    GptReasoningSignatureInfo, MAX_GPT_REASONING_SIGNATURE_LEN, inspect_gpt_reasoning_signature,
    is_valid_gpt_reasoning_signature,
};
pub use grok_validation::{
    GrokEncryptedContentInfo, MAX_GROK_ENCRYPTED_CONTENT_LEN,
    MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN, MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO,
    inspect_grok_encrypted_content, is_valid_grok_encrypted_content,
};
pub use kimi_validation::{
    KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN, KIMI_THINKING_SIGNATURE_STREAMING_LEN,
    KimiSignatureMode, KimiThinkingSignatureInfo, MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO,
    inspect_kimi_thinking_signature, is_valid_kimi_thinking_signature,
};

/// A validation failure, with upstream's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// The provider family that issued a signature, or that history is replayed to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Provider {
    #[default]
    Unknown,
    Claude,
    Gemini,
    /// One of Gemini's documented bypass sentinels rather than a signature.
    GeminiBypass,
    Gpt,
    /// Identified by length alone, which is an observed regularity rather than
    /// a contract. See [`inspect_kimi_thinking_signature`].
    Kimi,
    /// Only ever a target. xAI's `encrypted_content` has no envelope and looks
    /// like uniform random bytes, so detection never claims it. Whether a Grok
    /// target can replay a value is decided from the target model plus
    /// [`inspect_grok_encrypted_content`].
    Grok,
    /// Cognition's SWE models, which emit `sealed.v1.` envelopes.
    Swe,
}

impl Provider {
    /// Upstream's name for the provider.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Unknown => "unknown",
            Provider::Claude => "claude",
            Provider::Gemini => "gemini",
            Provider::GeminiBypass => "gemini_bypass",
            Provider::Gpt => "gpt",
            Provider::Kimi => "kimi",
            Provider::Grok => "grok",
            Provider::Swe => "swe",
        }
    }

    /// `SignatureProviderFromModelName`: the provider whose signed history can
    /// be replayed to `model`.
    pub fn from_model_name(model: &str) -> Self {
        let lower = crate::go::to_lower(model.trim());
        let has = |needle| lower.contains(needle);
        let starts = |prefix| lower.starts_with(prefix);
        if has("claude") {
            Provider::Claude
        } else if has("gemini") {
            Provider::Gemini
        } else if has("gpt")
            || has("openai")
            || has("codex")
            || starts("o1")
            || starts("o3")
            || starts("o4")
        {
            Provider::Gpt
        } else if has("kimi") || has("moonshot") || starts("k2") || starts("k3") {
            Provider::Kimi
        } else if has("grok") {
            Provider::Grok
        } else if has("swe-") {
            Provider::Swe
        } else {
            Provider::Unknown
        }
    }

    /// `SignatureProviderFromCachePrefix`: the provider named by a `prefix#`
    /// on a cached signature. Stricter than [`Provider::from_model_name`], so a
    /// model name such as `claude-cache` isn't taken as provenance.
    pub fn from_cache_prefix(prefix: &str) -> Self {
        match crate::go::to_lower(prefix.trim()).as_str() {
            "claude" | "anthropic" | "cais" | "claude-cais" | "claude_cais" | "ccmax"
            | "claude-code-max" | "claude_code_max" => Provider::Claude,
            "gemini" | "google" => Provider::Gemini,
            "openai" | "gpt" | "codex" => Provider::Gpt,
            "swe" | "sealed" => Provider::Swe,
            _ => Provider::Unknown,
        }
    }
}

/// The kind of block a signature was found on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BlockKind {
    #[default]
    Unknown,
    ClaudeThinking,
    GeminiModelPart,
    GeminiFunctionCall,
    GptReasoning,
}

impl BlockKind {
    /// Upstream's name for the block kind.
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Unknown => "unknown",
            BlockKind::ClaudeThinking => "claude_thinking",
            BlockKind::GeminiModelPart => "gemini_model_part",
            BlockKind::GeminiFunctionCall => "gemini_function_call",
            BlockKind::GptReasoning => "gpt_reasoning",
        }
    }
}

/// What to do with a signed block when replaying it to a target provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Preserve,
    DropBlock,
    DropSignature,
    ReplaceWithGeminiBypass,
    NoCompatibleReplacement,
}

impl Action {
    /// Upstream's name for the action.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Preserve => "preserve",
            Action::DropBlock => "drop_block",
            Action::DropSignature => "drop_signature",
            Action::ReplaceWithGeminiBypass => "replace_with_gemini_bypass",
            Action::NoCompatibleReplacement => "no_compatible_replacement",
        }
    }
}

/// How to replay one signed block, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub target_provider: Provider,
    pub detected_provider: Provider,
    pub block_kind: BlockKind,
    pub compatible: bool,
    pub action: Action,
    /// The bypass sentinel, for [`Action::ReplaceWithGeminiBypass`].
    pub replacement_signature: String,
    /// The signature in the form the target expects, for [`Action::Preserve`].
    pub normalized_signature: String,
    pub reason: String,
}

/// Base64 characters a self-describing envelope can start with. The first
/// character is the first decoded byte shifted right by two:
///
/// - `C`: `0x08..=0x0b`, Claude CAIS (`0x08`)
/// - `E`: `0x10..=0x13`, Claude single-layer and Gemini field-2 (`0x12`)
/// - `Q`: `0x40..=0x43`, Antigravity double-layer CAQS (`0x43`, an inner `C`)
/// - `R`: `0x44..=0x47`, Claude double-layer (`0x45`, an inner `E`)
/// - `g`: `0x80..=0x83`, GPT Fernet reasoning (`0x80`)
///
/// Gemini's ASCII UUID payload is left out because it is never replay-safe.
const SELF_DESCRIBING_FIRST_CHARS: &[u8] = b"CEQRg";

/// Whether `sig` could be a self-describing provider envelope. `false` is
/// conclusive; `true` only narrows the candidates.
fn maybe_self_describing_envelope(sig: &str) -> bool {
    sig.as_bytes()
        .first()
        .is_some_and(|b| SELF_DESCRIBING_FIRST_CHARS.contains(b))
}

/// The first byte of `sig` outside the alphanumeric base64 core plus `extra`,
/// with its offset and the character there.
fn first_invalid_base64_char(sig: &str, extra: &[u8]) -> Option<(usize, char)> {
    let index = sig
        .bytes()
        .position(|b| !b.is_ascii_alphanumeric() && !extra.contains(&b))?;
    // Every byte before it is ASCII, so `index` is a character boundary.
    Some((index, sig[index..].chars().next()?))
}

/// `DetectSignatureProvider`.
pub fn detect_signature_provider(raw: &str) -> Provider {
    detect_signature_provider_for_block(raw, BlockKind::Unknown)
}

/// `DetectSignatureProviderForBlock`: classifies the provider family that can
/// replay `raw`. A Gemini ASCII UUID payload is never classified as replay-safe;
/// a Gemini target should replace it with the bypass sentinel.
pub fn detect_signature_provider_for_block(raw: &str, block_kind: BlockKind) -> Provider {
    // Upstream passes the block kind through to Gemini detection, which ignores it.
    let _ = block_kind;
    let sig = raw.trim();
    if sig.is_empty() {
        return Provider::Unknown;
    }

    if let Some((prefixed, unprefixed)) = split_signature_provider_prefix(sig) {
        // The validators strip a cache prefix themselves, so a second one could
        // make detection judge a different payload from the one replayed.
        if unprefixed.contains('#') {
            return Provider::Unknown;
        }
        let matches = match prefixed {
            Provider::Gemini if is_gemini_thought_signature_bypass(unprefixed) => {
                return Provider::GeminiBypass;
            }
            Provider::Gemini => is_recognized_gemini_provider_signature(unprefixed),
            Provider::Claude => {
                is_valid_claude_thinking_signature(unprefixed, ClaudeValidationOptions::STRICT)
                    || is_valid_claude_cais_signature(unprefixed)
            }
            Provider::Gpt => is_valid_gpt_reasoning_signature(unprefixed),
            Provider::Swe => unprefixed.starts_with("sealed.v1."),
            _ => false,
        };
        return if matches { prefixed } else { Provider::Unknown };
    }
    if sig.contains('#') {
        return Provider::Unknown;
    }

    // The bypass sentinel is a literal, not an envelope, so it is matched
    // before the envelope pre-filter would reject it.
    if is_gemini_thought_signature_bypass(sig) {
        return Provider::GeminiBypass;
    }
    if sig.starts_with("sealed.v1.") {
        return Provider::Swe;
    }
    // From the strongest marker to the weakest: GPT's literal `gAAAA`, Claude
    // CAIS, classic Claude, then Gemini, which has only its wire shape.
    if maybe_self_describing_envelope(sig) {
        if is_valid_gpt_reasoning_signature(sig) {
            return Provider::Gpt;
        }
        if is_valid_claude_cais_signature(sig)
            || is_valid_claude_thinking_signature(sig, ClaudeValidationOptions::STRICT)
        {
            return Provider::Claude;
        }
        if is_recognized_gemini_provider_signature(sig) {
            return Provider::Gemini;
        }
    }
    // Kimi has no envelope, so it is claimed only after every envelope probe
    // declined. Its base64 starts with one of the envelope characters about 8%
    // of the time, which is why the pre-filter above doesn't return early.
    if is_valid_kimi_thinking_signature(sig) {
        return Provider::Kimi;
    }
    Provider::Unknown
}

/// `IsSignatureCompatibleWithProvider`.
pub fn is_signature_compatible_with_provider(target: Provider, raw: &str) -> bool {
    decide_signature_compatibility(target, raw, BlockKind::Unknown).compatible
}

/// `DecideSignatureCompatibility`: how to replay a signed block to `target`.
pub fn decide_signature_compatibility(
    target: Provider,
    raw: &str,
    block_kind: BlockKind,
) -> Decision {
    decide_signature_compatibility_for_model(target, "", raw, block_kind)
}

/// `DecideSignatureCompatibilityForModel`: how to replay a signed block to
/// `target`, naming `target_model` in the reason.
pub fn decide_signature_compatibility_for_model(
    target: Provider,
    target_model: &str,
    raw: &str,
    block_kind: BlockKind,
) -> Decision {
    let target = normalize_target_provider(target);
    let detected = detect_signature_provider_for_block(raw, block_kind);
    let mut decision = Decision {
        target_provider: target,
        detected_provider: detected,
        block_kind,
        compatible: false,
        action: Action::NoCompatibleReplacement,
        replacement_signature: String::new(),
        normalized_signature: String::new(),
        reason: String::new(),
    };

    // A Claude envelope in Google's wrapper is for Antigravity's replay, not
    // Claude's own endpoints.
    if target == Provider::Claude
        && detected == Provider::Claude
        && signature_payload_without_provider_prefix(raw).starts_with('Q')
    {
        decision.action = Action::DropBlock;
        decision.reason = "Antigravity CAQS wrapper requires Antigravity replay".to_owned();
        return decision;
    }

    if provider_matches_target(target, detected) {
        // A matching family isn't enough: the signature must also normalize,
        // or a sanitizer would keep the client's text as it is.
        let normalized = normalize_compatible_signature(target, raw);
        if !normalized.is_empty() {
            decision.compatible = true;
            decision.action = Action::Preserve;
            decision.normalized_signature = normalized;
            decision.reason = compatible_signature_reason(target, raw, target_model);
            return decision;
        }
    }

    let (action, reason) = match target {
        Provider::Gemini
            if matches!(
                block_kind,
                BlockKind::GeminiFunctionCall | BlockKind::GeminiModelPart | BlockKind::Unknown
            ) =>
        {
            decision.replacement_signature = GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_owned();
            (
                Action::ReplaceWithGeminiBypass,
                "missing or incompatible signature",
            )
        }
        Provider::Gemini => (
            Action::DropBlock,
            "signature is not compatible with Gemini and this block is not a bypass-safe Gemini model part",
        ),
        Provider::Claude => (
            Action::DropBlock,
            "Claude has no cross-provider bypass sentinel for thinking blocks",
        ),
        Provider::Gpt => (
            Action::DropBlock,
            "GPT reasoning encrypted_content cannot be synthesized from another provider signature",
        ),
        Provider::Swe => (
            Action::DropBlock,
            "SWE requires sealed.v1 signature from its own backend",
        ),
        // Kimi never reads the signature back, so the thinking text can stay.
        Provider::Kimi => (
            Action::DropSignature,
            "Kimi does not validate replayed thinking signatures, so the block survives without one",
        ),
        // xAI rejects a foreign or mutated blob with 400 "Could not decrypt".
        Provider::Grok => (
            Action::DropBlock,
            "xAI verifies encrypted_content on replay and rejects foreign or mutated blobs",
        ),
        _ => (Action::NoCompatibleReplacement, "unknown target provider"),
    };
    decision.action = action;
    decision.reason = reason.to_owned();
    decision
}

/// `SplitSignatureProviderPrefix`: the provider named by a known `prefix#`
/// and the trimmed signature after it.
pub fn split_signature_provider_prefix(raw: &str) -> Option<(Provider, &str)> {
    let (prefix, rest) = raw.trim().split_once('#')?;
    match Provider::from_cache_prefix(prefix) {
        Provider::Unknown => None,
        provider => Some((provider, rest.trim())),
    }
}

/// `SignaturePayloadWithoutProviderPrefix`: the value to replay upstream, with
/// any known `prefix#` removed.
pub fn signature_payload_without_provider_prefix(raw: &str) -> &str {
    match split_signature_provider_prefix(raw) {
        Some((_, unprefixed)) => unprefixed,
        None => raw.trim(),
    }
}

/// `CompatibleSignatureForProvider`.
pub fn compatible_signature_for_provider(target: Provider, raw: &str) -> Option<String> {
    compatible_signature_for_provider_block(target, raw, BlockKind::Unknown)
}

/// `CompatibleSignatureForProviderBlock`: the signature to replay to `target`,
/// without its cache prefix and in the form the target expects.
pub fn compatible_signature_for_provider_block(
    target: Provider,
    raw: &str,
    block_kind: BlockKind,
) -> Option<String> {
    let decision = decide_signature_compatibility(target, raw, block_kind);
    (decision.compatible && !decision.normalized_signature.is_empty())
        .then_some(decision.normalized_signature)
}

/// `CompatibleAntigravityClaudeThinkingSignature`: the double-layer R or Q form
/// that Antigravity's Claude replay requires. Only signatures strictly
/// identified as Claude qualify, so a Gemini envelope that also starts with `E`
/// cannot.
pub fn compatible_antigravity_claude_thinking_signature(raw: &str) -> Option<String> {
    if detect_signature_provider_for_block(raw, BlockKind::ClaudeThinking) != Provider::Claude {
        return None;
    }
    normalize_claude_thinking_signature(
        signature_payload_without_provider_prefix(raw),
        ClaudeValidationOptions::STRICT,
    )
    .ok()
}

/// `IsRecognizedReasoningSignature`: whether `raw` is a well-formed signature
/// from any known provider, Grok included.
pub fn is_recognized_reasoning_signature(raw: &str) -> bool {
    let sig = raw.trim();
    !sig.is_empty()
        && (detect_signature_provider(sig) != Provider::Unknown
            || is_valid_grok_encrypted_content(sig))
}

/// Why a matching signature is replayable. A Claude CAIS signature names the
/// model that issued it, so that and the target model are both reported.
fn compatible_signature_reason(target: Provider, raw: &str, target_model: &str) -> String {
    const GENERIC: &str = "signature provider matches target provider";
    if target != Provider::Claude {
        return GENERIC.to_owned();
    }
    let Ok(info) = inspect_claude_cais_signature(signature_payload_without_provider_prefix(raw))
    else {
        return GENERIC.to_owned();
    };
    let mut reason = if !info.model_text.is_empty() {
        format!(
            "valid Claude CAIS signature with embedded model {} is compatible with any Claude target",
            info.model_text
        )
    } else if info.envelope_version >= 4 {
        "valid Claude CAQS signature is compatible with any Claude target".to_owned()
    } else {
        "valid Claude CAIS signature is compatible with any Claude target".to_owned()
    };
    let target_model = target_model.trim();
    if !target_model.is_empty() {
        reason.push_str(", including target model ");
        reason.push_str(target_model);
    }
    reason
}

fn normalize_target_provider(provider: Provider) -> Provider {
    match provider {
        Provider::GeminiBypass => Provider::Gemini,
        provider => provider,
    }
}

fn provider_matches_target(target: Provider, detected: Provider) -> bool {
    match target {
        Provider::Gemini => matches!(detected, Provider::Gemini | Provider::GeminiBypass),
        Provider::Claude | Provider::Gpt | Provider::Swe | Provider::Kimi => detected == target,
        // Detection never yields Grok, so a Grok target never matches.
        _ => false,
    }
}

/// The signature in the form `target` expects, or empty if it can't be replayed.
fn normalize_compatible_signature(target: Provider, raw: &str) -> String {
    let payload = signature_payload_without_provider_prefix(raw);
    let keep = match normalize_target_provider(target) {
        Provider::Claude => {
            if is_valid_claude_cais_signature(payload) {
                true
            } else {
                return normalize_claude_provider_native_thinking_signature(
                    payload,
                    ClaudeValidationOptions::default(),
                )
                .unwrap_or_default();
            }
        }
        Provider::Gemini => {
            is_gemini_thought_signature_bypass(payload)
                || is_recognized_gemini_provider_signature(payload)
        }
        Provider::Gpt => is_valid_gpt_reasoning_signature(payload),
        Provider::Swe => payload.starts_with("sealed.v1."),
        Provider::Kimi => is_valid_kimi_thinking_signature(payload),
        _ => false,
    };
    if keep {
        payload.to_owned()
    } else {
        String::new()
    }
}

fn is_recognized_gemini_provider_signature(raw: &str) -> bool {
    !is_valid_claude_cais_signature(raw)
        && is_valid_gemini_thought_signature(
            raw,
            GeminiValidationOptions {
                require_known_envelope: true,
                ..GeminiValidationOptions::default()
            },
        )
}

#[cfg(test)]
pub(crate) mod tests;
