//! Comparison cases, and hand-written ones for inputs the generator is unlikely to build.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use serde_json::json;

pub struct Case {
    pub name: String,
    /// The model name passed to the translator.
    pub model: String,
    /// The request body exactly as sent, so formatting and escapes are tested too.
    pub request: String,
    /// Why the outputs are expected to differ: behaviour not ported yet, or a
    /// difference between Go and Rust we accept (see UPSTREAM.md).
    pub known_difference: Option<&'static str>,
}

impl Case {
    pub fn new(
        name: impl Into<String>,
        model: impl Into<String>,
        request: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            model: model.into(),
            request: request.into(),
            known_difference: None,
        }
    }

    fn known_difference(mut self, reason: &'static str) -> Self {
        self.known_difference = Some(reason);
        self
    }
}

/// A Grok reasoning signature, from upstream's tests.
const GROK_SIGNATURE: &str = "HmlYdr2aCAqCYP/m9mr8PS6KOsdMs72FGDigmydR+Jsmuv8KX97yWPlbOwmXJgWn0CbHaCacdQD3+n5EvpgLfPNmafS3kdICBjRuDf4bzHy7uBiUhNVhqPtp/ee1y9q4imPE4LYgD1VZ4J+bp9mTeqA1+nC9Oue58CiNEMV9SVaGenCD+aBnVuSTzQhD32Y+68i6HLJW0Dx6ifaRfb8hxYtA/sPM+/FTvAMW11nRho5a2BBSkpnzfqqAz/e/vGJ77/bygpXM823QA9wL9i0X";

/// The smallest well-formed GPT reasoning signature, built as upstream's tests build it.
fn gpt_signature() -> String {
    let mut raw = [0u8; 1 + 8 + 16 + 16 + 32];
    raw[0] = 0x80;
    raw[8] = 1;
    URL_SAFE.encode(raw)
}

fn with_thinking_signature(signature: &str) -> String {
    json!({
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "summary", "signature": signature},
                {"type": "text", "text": "answer"}
            ]},
            {"role": "user", "content": "next"}
        ]
    })
    .to_string()
}

pub fn hand_written() -> Vec<Case> {
    let deep_schema = (0..100).fold(
        json!({ "type": "string" }),
        |schema, _| json!({ "type": "array", "items": schema }),
    );
    let large_image = "A".repeat(2 << 20);

    vec![
        Case::new(
            "minimal",
            "gpt-5",
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
        ),
        Case::new("empty-object", "gpt-5", "{}"),
        Case::new("array-body", "gpt-5", "[]"),
        Case::new("null-body", "gpt-5", "null"),
        Case::new(
            "escaped-keys",
            "gpt-5",
            r#"{"messages":[{"role":"user","content":"café 🚀"}]}"#,
        ),
        Case::new(
            "duplicate-keys",
            "gpt-5",
            r#"{"thinking":{"type":"enabled","budget_tokens":1024},"thinking":{"type":"disabled"}}"#,
        )
        .known_difference("gjson reads the first duplicate key; serde_json keeps the last"),
        Case::new(
            "pretty-tool-input",
            "gpt-5",
            serde_json::to_string_pretty(&json!({
                "tools": [{"name": "get_weather", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}],
                "messages": [
                    {"role": "user", "content": "Weather?"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Zürich", "days": 1.50}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "document"}]}
                    ]}
                ]
            }))
            .expect("serializable"),
        ),
        Case::new(
            "huge-float-budget",
            "gpt-5",
            r#"{"thinking":{"type":"enabled","budget_tokens":1e30}}"#,
        )
        .known_difference("Go's int64(1e30) depends on the CPU; Rust saturates"),
        Case::new(
            "infinite-number-as-text",
            "gpt-5",
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":1e400}]}]}"#,
        ),
        // Go lowercases İ to i and has no final-sigma rule; Rust does both.
        Case::new(
            "dotted-capital-i-effort",
            "gpt-5",
            r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"MAXİMUM"}}"#,
        ),
        Case::new(
            "final-sigma-effort",
            "gpt-5",
            r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"ΑΣ"}}"#,
        ),
        Case::new(
            "dotted-capital-i-service-tier",
            "gpt-5",
            r#"{"service_tier":"PRİORİTY"}"#,
        ),
        Case::new(
            "dotted-capital-i-signature-prefix",
            "gpt-5",
            with_thinking_signature(&format!("OPENAİ#{}", gpt_signature())),
        ),
        Case::new(
            "deep-schema",
            "gpt-5",
            json!({ "tools": [{ "name": "deep", "input_schema": deep_schema }] }).to_string(),
        ),
        Case::new(
            "large-image",
            "gpt-5",
            json!({ "messages": [{ "role": "user", "content": [
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": large_image } }
            ]}]})
            .to_string(),
        ),
        Case::new("grok-signature", "grok-4", with_thinking_signature(GROK_SIGNATURE))
            .known_difference("Grok signature replay is not ported"),
    ]
}
