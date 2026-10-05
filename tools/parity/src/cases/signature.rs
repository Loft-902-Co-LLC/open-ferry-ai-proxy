//! Hand-written cases for the signature checks and sanitizers: one signature
//! of each provider's real layout, which the generator only damages, and
//! Antigravity's Q form behind cache prefixes.

use serde_json::json;

use super::Case;

const CLAUDE: &str = "EhkKFwgMEAIyEWNsYXVkZS1zb25uZXQtNC02GAE=";
const CLAUDE_CAIS: &str = "CAISigEKhwEIEBgCKkAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMg1jbGF1ZGUtb3B1cy01OAFCCHRoaW5raW5nWiQwZjhjMmQxZS00YjZhLTRjM2UtOWE3ZC0yZTVmNmI4YzlkMGEYAQ==";
const GEMINI: &str = "EiwKKgEMAAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJw==";
const GPT: &str = "gAAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
const GROK: &str = "HWav+EGK0xxlrvdAidIbZK32P4jRGmOs9T6H0Bliq/Q9hs8YYarzPIXOF2Cp8juEzRZfqPE6g8wVXqfwOYLLFF2m7ziByhNcpe43gMkSW6TtNn/IEVqj7DV+xxBZous0";
const GEMINI_BYPASS: &str = "skip_thought_signature_validator";

/// One signature per provider, each checked against its own provider and one
/// other.
pub fn inspect() -> Vec<Case> {
    let caqs = antigravity_caqs();
    let signatures = [
        ("claude", "claude-sonnet-4-6", CLAUDE.to_owned()),
        ("claude-r", "claude-sonnet-4-6", base64_again(CLAUDE)),
        (
            "claude-cache-prefix",
            "claude-sonnet-4-6",
            format!("claude#{CLAUDE}"),
        ),
        ("claude-cais", "claude-opus-5", CLAUDE_CAIS.to_owned()),
        ("claude-q", "claude-opus-5-5", caqs.clone()),
        (
            "claude-q-cache-prefix",
            "claude-opus-5-5",
            format!("anthropic#{caqs}"),
        ),
        (
            "claude-q-two-cache-prefixes",
            "claude-opus-5-5",
            format!("claude#claude#{caqs}"),
        ),
        ("gemini", "gemini-3.1-pro", GEMINI.to_owned()),
        ("gemini-bypass", "gemini-3.1-pro", GEMINI_BYPASS.to_owned()),
        ("gpt", "gpt-5.6-luna", GPT.to_owned()),
        (
            "gpt-cache-prefix",
            "gpt-5.6-luna",
            format!(" OpenAI # {GPT}"),
        ),
        ("grok", "grok-4.5", GROK.to_owned()),
        ("swe", "swe-1.5", "sealed.v1.abc".to_owned()),
        ("empty", "claude-sonnet-4-6", String::new()),
    ];
    let mut cases: Vec<Case> = signatures
        .into_iter()
        .flat_map(|(name, model, signature)| {
            ["unknown", "claude", "gemini", "gpt"].map(|target| {
                Case::new(format!("{name}-for-{target}"), model, signature.clone())
                    .with_options(json!({ "target": target }))
            })
        })
        .collect();
    cases.push(
        Case::new(
            "claude-q-after-a-second-prefix-for-claude",
            "claude-opus-5-5",
            format!("claude#Qjunk#{caqs}"),
        )
        .with_options(json!({ "target": "claude" }))
        .known_difference(
            "upstream's Claude validators strip a second cache prefix from a Q signature; \
             ours check it as it is (see UPSTREAM.md)",
        ),
    );
    cases
}

/// A conversation with thinking from every provider, under each target.
pub fn claude_messages() -> Vec<Case> {
    let caqs = antigravity_caqs();
    let request = json!({
        "messages": [
            { "role": "user", "content": "hi" },
            {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "claude", "signature": CLAUDE },
                    { "type": "thinking", "thinking": "cais", "signature": CLAUDE_CAIS },
                    { "type": "thinking", "thinking": "antigravity", "signature": caqs },
                    { "type": "thinking", "thinking": "gemini", "signature": GEMINI },
                    { "type": "thinking", "thinking": "gpt", "signature": GPT },
                    { "type": "thinking", "thinking": "grok", "signature": GROK },
                    { "type": "thinking", "thinking": "", "signature": "" },
                    { "type": "thinking", "thinking": "" },
                    { "type": "redacted_thinking", "data": "EmwKAhgB" },
                    { "type": "text", "text": "Let me check." },
                    {
                        "type": "tool_use", "id": "toolu_1", "name": "get_weather",
                        "input": { "city": "Paris" },
                        "signature": GPT,
                        "extra_content": { "google": { "thought_signature": GEMINI } },
                        "model": "gemini-3.1-pro"
                    }
                ]
            },
            {
                "role": "user",
                "content": [{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "sunny" }]
            },
            { "role": "assistant", "content": [{ "type": "thinking", "thinking": "", "signature": "" }] }
        ]
    })
    .to_string();
    let targets = [
        ("claude-sonnet-4-6", "claude"),
        ("gemini-3.1-pro", "gemini"),
        ("gpt-5.6-luna", "gpt"),
        ("grok-4.5", "grok"),
        ("kimi-k3", "kimi"),
        ("deepseek-v4", "unknown"),
    ];
    targets
        .into_iter()
        .flat_map(|(model, provider)| {
            [false, true].map(|drop| {
                let options = json!({
                    "validation": { "Strict": drop },
                    "target": {
                        "TargetProvider": provider,
                        "DropEmptyMessages": drop,
                        "DropToolSignatures": drop,
                        "DropEmptyThinkingPlaceholders": drop,
                        "PreserveEmptyThinkingBlocks": !drop,
                    },
                });
                let name = format!(
                    "every-provider-for-{provider}{}",
                    if drop { "-drop" } else { "" }
                );
                Case::new(name, model, request.clone()).with_options(options)
            })
        })
        .collect()
}

/// Function calls and responses with signatures from Gemini, another
/// provider, and the bypass sentinel.
pub fn gemini() -> Vec<Case> {
    let contents = json!([
        { "role": "user", "parts": [{ "text": "Weather in Paris and Rome?" }] },
        {
            "role": "model",
            "parts": [
                { "text": "Thinking.", "thought": true, "thoughtSignature": GEMINI },
                { "functionCall": { "name": "get_weather", "args": { "city": "Paris" } }, "thoughtSignature": GEMINI },
                { "functionCall": { "name": "get_weather", "args": { "city": "Rome" } }, "thought_signature": GPT },
                { "toolCall": { "toolType": "GOOGLE_SEARCH_WEB" }, "thoughtSignature": CLAUDE },
                { "functionCall": { "name": "lookup", "args": {} }, "thoughtSignature": GEMINI_BYPASS }
            ]
        },
        {
            "role": "user",
            "parts": [
                { "functionResponse": { "name": "get_weather", "response": { "result": "sunny" } } },
                { "functionResponse": { "name": "get_weather", "response": { "result": "rain" } } },
                { "functionResponse": { "name": "lookup", "response": {} }, "thoughtSignature": GEMINI }
            ]
        }
    ]);
    let requests = [
        ("contents", json!({ "contents": contents })),
        (
            "request.contents",
            json!({ "request": { "contents": contents } }),
        ),
    ];
    let validations = [
        ("default", json!({})),
        (
            "strict",
            json!({ "RequireKnownEnvelope": true, "RequireObservedMarker": true }),
        ),
        ("bypass", json!({ "AllowBypassSentinel": true })),
    ];
    requests
        .iter()
        .flat_map(|(path, request)| {
            validations.iter().map(move |(name, validation)| {
                Case::new(format!("calls-at-{path}-{name}"), "", request.to_string())
                    .with_options(json!({ "contents_path": path, "validation": validation }))
            })
        })
        .collect()
}

/// Encodes `data` in standard base64, as Antigravity wraps Claude signatures.
fn base64_again(data: impl AsRef<[u8]>) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Antigravity's Q form, laid out as upstream's `signaturetest.AntigravityCAQS`
/// with a shorter signature: a CAQS protobuf for Google's thinking channel in
/// two layers of base64. Every opaque field is zeros.
fn antigravity_caqs() -> String {
    /// A length-delimited protobuf field.
    fn bytes(field: u8, value: &[u8]) -> Vec<u8> {
        let mut out = vec![field << 3 | 2];
        let mut len = value.len();
        while len >= 0x80 {
            out.push(len as u8 | 0x80);
            len >>= 7;
        }
        out.push(len as u8);
        out.extend_from_slice(value);
        out
    }
    // Channel 18, Google infrastructure (field 2), version 2, field 7, and
    // a thinking block.
    let channel = [
        &[0x08, 18, 0x10, 2, 0x18, 2, 0x38, 1][..],
        &bytes(8, b"thinking"),
    ]
    .concat();
    let container = [
        bytes(1, &channel),
        bytes(2, &[0; 12]),
        bytes(3, &[0; 12]),
        bytes(4, &[0; 48]),
        bytes(5, &[0; 64]),
    ]
    .concat();
    let payload = [&[0x08, 4][..], &bytes(2, &container), &[0x18, 1]].concat();
    base64_again(base64_again(payload))
}

#[cfg(test)]
mod tests {
    use open_ferry_translate::signature::inspect_antigravity_claude_caqs_signature;

    use super::*;

    #[test]
    fn antigravity_caqs_is_well_formed() {
        let info = inspect_antigravity_claude_caqs_signature(&antigravity_caqs())
            .expect("a valid Q-form signature");
        assert_eq!(info.signature_len, 64);
    }
}
