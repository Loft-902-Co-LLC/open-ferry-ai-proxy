// Ported from CLIProxyAPI sdk/cliproxy/auth/selector_test.go
// (TestExtractSessionID*, TestExtractExplicitSessionIDs_EnhancedHarnesses
// and TestSessionAffinitySelector_LongCompositeIDBoundConsistency) and
// selector_lcp_test.go (TestSessionAffinitySelectorPromptCacheKeyCamelCase
// and TestSessionAffinitySelectorNestedAntigravityPayload) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the session a call binds under, and the session it may fall
//! back to.
//!
//! Upstream's `ExtractSessionID` is the primary of [`Session::with_derived`]
//! here, and `extractExplicitSessionIDs` is `explicit`.
//!
//! Deviations from upstream:
//! - The primary is bounded, as `Pick` bounds it; upstream's
//!   `ExtractSessionID` returns it whole. Every ID here but the long one
//!   is too short for that to matter.
//! - The newline and NUL header cases of
//!   `ExtractSessionIDRejectsInvalidExplicitSignals` send a tab, as the
//!   `http` crate refuses the others in a header value; a tab is a control
//!   character too.
//! - The execution session and the derived session are arguments, not
//!   metadata, and nothing is written to metadata: the metadata checks of
//!   `ExtractExplicitSessionIDs_EnhancedHarnesses` check the fallback and
//!   the fork flag instead.
//! - `SessionAffinitySelectorNestedAntigravityPayload` reads a nested
//!   request of no particular provider, as Antigravity is out of scope.
//! - The third part of `ExtractExplicitSessionIDs_EnhancedHarnesses` is in
//!   the manager's affinity tests. `LongCompositeIDBoundConsistency` checks
//!   the bound primary only: its `CanonicalSessionID` check is left out, as
//!   the canonical session isn't ported.
//! - Not upstream's: the exact message hashes, checked against upstream's
//!   `computeSessionHash`, and `isSubagentSession` and `isHierarchyParent`.

use super::headers;
use crate::manager::affinity::Session;
use crate::manager::affinity::keys::{explicit, is_hierarchy_parent, is_subagent_session};
use crate::session::Payload;

/// A case's name, headers, body and the session it names.
type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str, &'a str);

/// The session of a call with headers `pairs` and body `payload`, the
/// derived session `derived` and the execution session `execution`, as
/// (primary, fallback, fork); empty when there is none.
fn ids(
    pairs: &[(&str, &str)],
    payload: &str,
    execution: &str,
    derived: &str,
) -> (String, String, bool) {
    Session::with_derived(
        &headers(pairs),
        &Payload::parse(payload.as_bytes()),
        execution,
        derived,
    )
    .map(|session| {
        (
            session.primary().to_owned(),
            session.fallback().to_owned(),
            session.is_fork(),
        )
    })
    .unwrap_or_default()
}

/// Upstream's `ExtractSessionID`: the primary session of a call with
/// headers `pairs` and body `payload`, or empty.
fn extract(pairs: &[(&str, &str)], payload: &str) -> String {
    ids(pairs, payload, "", "").0
}

/// Upstream's `extractExplicitSessionIDs`: the session the client named
/// and its fallback, or two empty strings.
fn explicit_ids(pairs: &[(&str, &str)], payload: &str) -> (String, String) {
    explicit(&headers(pairs), &Payload::parse(payload.as_bytes()), "")
        .map(|(primary, fallback, _)| (primary, fallback))
        .unwrap_or_default()
}

// TestExtractSessionID.
#[test]
fn extract_session_id() {
    let cases = [
        (
            "valid_claude_code_format",
            r#"{"metadata":{"user_id":"user_3f221fe75652cf9a89a31647f16274bb8036a9b85ac4dc226a4df0efec8dc04d_account__session_ac980658-63bd-4fb3-97ba-8da64cb1e344"}}"#,
            "claude:ac980658-63bd-4fb3-97ba-8da64cb1e344",
        ),
        (
            "json_user_id_with_session_id",
            r#"{"metadata":{"user_id":"{\"device_id\":\"be82c3aee1e0c2d74535bacc85f9f559228f02dd8a17298cf522b71e6c375714\",\"account_uuid\":\"\",\"session_id\":\"e26d4046-0f88-4b09-bb5b-f863ab5fb24e\"}"}}"#,
            "claude:e26d4046-0f88-4b09-bb5b-f863ab5fb24e",
        ),
        (
            "json_user_id_without_session_id",
            r#"{"metadata":{"user_id":"{\"device_id\":\"abc123\"}"}}"#,
            r#"user:{"device_id":"abc123"}"#,
        ),
        (
            "no_session_but_user_id",
            r#"{"metadata":{"user_id":"user_abc123"}}"#,
            "user:user_abc123",
        ),
        (
            "conversation_id",
            r#"{"conversation_id":"conv-12345"}"#,
            "conv:conv-12345",
        ),
        ("no_metadata", r#"{"model":"claude-3"}"#, ""),
        ("empty_payload", "", ""),
    ];
    for (name, payload, want) in cases {
        assert_eq!(extract(&[], payload), want, "{name}");
    }
}

// TestExtractSessionID_NestedRequestSubagent.
#[test]
fn nested_request_subagent() {
    // 1. A nested request's session and metadata.agent_id.
    let agent = r#"{"request":{"sessionId":"root","metadata":{"agent_id":"worker"}}}"#;
    assert_eq!(extract(&[], agent), "session:root:agent:worker");
    assert_eq!(
        explicit_ids(&[], agent),
        ("session:root:agent:worker".into(), "session:root".into())
    );

    // 2. metadata.subagent_id.
    let subagent = r#"{"request":{"sessionId":"root","metadata":{"subagent_id":"worker"}}}"#;
    assert_eq!(extract(&[], subagent), "session:root:agent:worker");
    assert_eq!(
        explicit_ids(&[], subagent),
        ("session:root:agent:worker".into(), "session:root".into())
    );

    // 3. A parent session.
    let parent = r#"{"request":{"sessionId":"root","parentSessionId":"parent-root","metadata":{"agent_id":"worker"}}}"#;
    assert_eq!(extract(&[], parent), "session:root:agent:worker");
    assert_eq!(
        explicit_ids(&[], parent),
        (
            "session:root:agent:worker".into(),
            "session:parent-root".into()
        )
    );

    // 4. A nested promptCacheKey behind an empty top-level one.
    let shadowed = r#"{"prompt_cache_key":"","request":{"promptCacheKey":"nested-pck-valid"}}"#;
    assert_eq!(extract(&[], shadowed), "pck:nested-pck-valid");
}

// TestExtractSessionID_ClaudeCodePriorityOverHeader.
#[test]
fn claude_code_priority_over_header() {
    let payload = r#"{"metadata":{"user_id":"user_xxx_account__session_ac980658-63bd-4fb3-97ba-8da64cb1e344"}}"#;
    assert_eq!(
        extract(&[("X-Session-ID", "header-session-id")], payload),
        "claude:ac980658-63bd-4fb3-97ba-8da64cb1e344"
    );
}

// TestExtractSessionID_ClaudeCodePriorityOverIdempotencyKey: the
// idempotency key isn't read at all, so the payload alone decides.
#[test]
fn claude_code_priority_over_idempotency_key() {
    let payload = r#"{"metadata":{"user_id":"user_xxx_account__session_ac980658-63bd-4fb3-97ba-8da64cb1e344"}}"#;
    assert_eq!(
        extract(&[], payload),
        "claude:ac980658-63bd-4fb3-97ba-8da64cb1e344"
    );
}

// TestExtractSessionID_Headers.
#[test]
fn headers_session() {
    assert_eq!(
        extract(&[("X-Session-ID", "my-explicit-session")], ""),
        "header:my-explicit-session"
    );
}

// TestExtractSessionID_CodexSessionIDHeader.
#[test]
fn codex_session_id_header() {
    assert_eq!(
        extract(&[("Session_id", "codex-session-123")], ""),
        "codex:codex-session-123"
    );
}

// TestExtractSessionID_ClientRequestIDHeader.
#[test]
fn client_request_id_header() {
    assert_eq!(
        extract(&[("X-Client-Request-Id", "pi-session-123")], ""),
        "clientreq:pi-session-123"
    );
}

// TestExtractSessionID_CodexSessionIDPriorityOverClientRequestID.
#[test]
fn codex_session_id_priority_over_client_request_id() {
    assert_eq!(
        extract(
            &[
                ("X-Client-Request-Id", "pi-session-123"),
                ("Session_id", "codex-session-456"),
            ],
            ""
        ),
        "codex:codex-session-456"
    );
}

// TestExtractSessionID_IdempotencyKey: an idempotency key alone names no
// session (it isn't read), and with no body there is nothing to hash.
#[test]
fn idempotency_key() {
    assert_eq!(extract(&[("Idempotency-Key", "idem-12345")], ""), "");
}

// TestExtractSessionID_DerivedSessionAndExplicitPriority.
#[test]
fn derived_session_and_explicit_priority() {
    let derived = "ctx:v1:derived-root";
    let payload = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
    assert_eq!(
        ids(&[], payload, "", derived).0,
        "derived:ctx:v1:derived-root"
    );
    assert_eq!(
        ids(&[], payload, "execution-session", derived).0,
        "execution:execution-session"
    );

    let explicit_payload = r#"{"session_id":"explicit-session","prompt_cache_key":"explicit-cache","messages":[{"role":"user","content":"hello"}]}"#;
    assert_eq!(
        ids(&[], explicit_payload, "", derived).0,
        "session:explicit-session"
    );

    let user_payload = r#"{"metadata":{"user_id":"explicit-user"},"conversation_id":"explicit-conversation","messages":[{"role":"user","content":"hello"}]}"#;
    assert_eq!(ids(&[], user_payload, "", derived).0, "user:explicit-user");

    assert_eq!(
        ids(
            &[("x-session-id", " lowercase-session ")],
            payload,
            "",
            derived
        )
        .0,
        "header:lowercase-session"
    );
    assert_eq!(
        ids(
            &[("X-Session-ID", "header-session")],
            explicit_payload,
            "",
            derived
        )
        .0,
        "header:header-session"
    );
}

// TestExtractSessionID_MessageHashFallback.
#[test]
fn message_hash_fallback() {
    let first = r#"{"messages":[{"role":"user","content":"Hello world"}]}"#;
    let short = extract(&[], first);
    assert!(short.starts_with("msg:"), "{short}");

    let multi_turn = r#"{"messages":[
        {"role":"user","content":"Hello world"},
        {"role":"assistant","content":"Hi! How can I help?"},
        {"role":"user","content":"Tell me a joke"}
    ]}"#;
    let full = extract(&[], multi_turn);
    assert!(!full.is_empty());
    assert_ne!(full, short, "the full hash includes the assistant");
    assert_eq!(extract(&[], multi_turn), full, "not stable");
}

// TestExtractSessionID_ClaudeAPITopLevelSystem.
#[test]
fn claude_api_top_level_system() {
    let array_system = r#"{
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Hello"}]}],
        "system": [{"type": "text", "text": "You are Claude Code"}]
    }"#;
    let got1 = extract(&[], array_system);
    assert!(got1.starts_with("msg:"), "{got1}");

    let string_system = r#"{
        "messages": [{"role": "user", "content": "Hello"}],
        "system": "You are Claude Code"
    }"#;
    let got2 = extract(&[], string_system);
    assert!(got2.starts_with("msg:"), "{got2}");

    let multi_turn = r#"{
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi!"},
            {"role": "user", "content": "Help me"}
        ],
        "system": "You are Claude Code"
    }"#;
    let got3 = extract(&[], multi_turn);
    assert!(!got3.is_empty());
    assert_ne!(got3, got2, "the multi-turn hash includes the assistant");
}

// TestExtractSessionID_GeminiFormat.
#[test]
fn gemini_format() {
    let payload = r#"{
        "systemInstruction": {"parts": [{"text": "You are a helpful assistant."}]},
        "contents": [
            {"role": "user", "parts": [{"text": "Hello Gemini"}]},
            {"role": "model", "parts": [{"text": "Hi there!"}]}
        ]
    }"#;
    let got = extract(&[], payload);
    assert!(got.starts_with("msg:"), "{got}");
    assert_eq!(extract(&[], payload), got, "not stable");

    let different = r#"{
        "systemInstruction": {"parts": [{"text": "You are a helpful assistant."}]},
        "contents": [
            {"role": "user", "parts": [{"text": "Hello different"}]},
            {"role": "model", "parts": [{"text": "Hi there!"}]}
        ]
    }"#;
    assert_ne!(extract(&[], different), got);
}

// TestExtractSessionID_OpenAIResponsesAPI.
#[test]
fn openai_responses_api() {
    let first_turn = r#"{
        "instructions": "You are Codex, based on GPT-5.",
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "system instructions"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
        ]
    }"#;
    let got1 = extract(&[], first_turn);
    assert!(got1.starts_with("msg:"), "{got1}");

    let second_turn = r#"{
        "instructions": "You are Codex, based on GPT-5.",
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "system instructions"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking..."}], "encrypted_content": "xxx"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hello!"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "what can you do"}]}
        ]
    }"#;
    let got2 = extract(&[], second_turn);
    assert!(!got2.is_empty());

    let third_turn = r#"{
        "instructions": "You are Codex, based on GPT-5.",
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "system instructions"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking..."}], "encrypted_content": "xxx"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hello!"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "what can you do"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "I can help with..."}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "thanks"}]}
        ]
    }"#;
    assert_eq!(
        extract(&[], third_turn),
        got2,
        "the same first assistant message"
    );
}

// TestExtractSessionID_MultimodalContent.
#[test]
fn multimodal_content() {
    let first = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"Hello world"},{"type":"image","source":{"data":"..."}}]}]}"#;
    let short = extract(&[], first);
    assert!(short.starts_with("msg:"), "{short}");

    let multi_turn = r#"{"messages":[
        {"role":"user","content":[{"type":"text","text":"Hello world"},{"type":"image","source":{"data":"..."}}]},
        {"role":"assistant","content":"I see an image!"},
        {"role":"user","content":"What is it?"}
    ]}"#;
    let full = extract(&[], multi_turn);
    assert!(!full.is_empty());
    assert_ne!(full, short);

    let different = r#"{"messages":[
        {"role":"user","content":[{"type":"text","text":"Different content"}]},
        {"role":"assistant","content":"I see something different!"}
    ]}"#;
    assert_ne!(extract(&[], different), full);
}

// Not upstream's: the hashes are upstream's, computed by its
// `computeSessionHash` in Go: the short one, the full one with its short
// fallback, a Responses body, and a long message cut to 100 bytes.
#[test]
fn message_hashes_match_upstream() {
    let first = r#"{"messages":[{"role":"user","content":"Hello world"}]}"#;
    assert_eq!(
        ids(&[], first, "", ""),
        ("msg:6f3af82650710693".into(), String::new(), false)
    );

    let multi_turn = r#"{"messages":[
        {"role":"user","content":"Hello world"},
        {"role":"assistant","content":"Hi! How can I help?"},
        {"role":"user","content":"Tell me a joke"}
    ]}"#;
    assert_eq!(
        ids(&[], multi_turn, "", ""),
        (
            "msg:0964de51a03c0514".into(),
            "msg:6f3af82650710693".into(),
            false
        )
    );

    let responses = r#"{
        "instructions": "You are Codex, based on GPT-5.",
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "system instructions"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hello!"}]}
        ]
    }"#;
    assert_eq!(extract(&[], responses), "msg:bae4ad1b6626e0e5");

    let long = format!(
        r#"{{"messages":[{{"role":"user","content":"{}"}}]}}"#,
        "x".repeat(150)
    );
    assert_eq!(extract(&[], &long), "msg:d4bdf715107b5431");
}

// TestExtractSessionIDNativeSignals.
#[test]
fn native_signals() {
    let cases: [Case<'_>; 8] = [
        (
            "claude code header",
            &[("X-Claude-Code-Session-Id", "claude-session")],
            "",
            "claude:claude-session",
        ),
        (
            "lowercase claude code header",
            &[("x-claude-code-session-id", "lowercase-session")],
            "",
            "claude:lowercase-session",
        ),
        (
            "codex hyphen header",
            &[("Session-Id", "codex-session")],
            "",
            "codex:codex-session",
        ),
        (
            "codex underscore header",
            &[("Session_id", "legacy-codex-session")],
            "",
            "codex:legacy-codex-session",
        ),
        (
            "open code session affinity",
            &[("X-Session-Affinity", "ses_opencode")],
            "",
            "affinity:ses_opencode",
        ),
        (
            "prompt cache key",
            &[],
            r#"{"prompt_cache_key":"prompt-session"}"#,
            "pck:prompt-session",
        ),
        (
            "responses conversation object",
            &[],
            r#"{"conversation":{"id":"conv-object"}}"#,
            "conv:conv-object",
        ),
        (
            "responses conversation string",
            &[],
            r#"{"conversation":"conv-string"}"#,
            "conv:conv-string",
        ),
    ];
    for (name, pairs, payload, want) in cases {
        assert_eq!(extract(pairs, payload), want, "{name}");
    }
}

// TestExtractSessionIDNativeSignalPriority.
#[test]
fn native_signal_priority() {
    let claude_metadata = r#"{"metadata":{"user_id":"user_hash_account__session_22222222-2222-4222-8222-222222222222"}}"#;
    let cases: [Case<'_>; 6] = [
        (
            "claude header beats metadata",
            &[("X-Claude-Code-Session-Id", "header-session")],
            claude_metadata,
            "claude:header-session",
        ),
        (
            "claude metadata beats codex header",
            &[("Session-Id", "codex-session")],
            claude_metadata,
            "claude:22222222-2222-4222-8222-222222222222",
        ),
        (
            "codex header beats x session id and prompt key",
            &[
                ("Session-Id", "codex-session"),
                ("X-Session-Id", "generic-session"),
            ],
            r#"{"prompt_cache_key":"prompt-session"}"#,
            "codex:codex-session",
        ),
        (
            "x session id beats affinity",
            &[
                ("X-Session-Id", "generic-session"),
                ("X-Session-Affinity", "affinity-session"),
            ],
            "",
            "header:generic-session",
        ),
        (
            "prompt cache key beats conversation id",
            &[],
            r#"{"conversation":{"id":"conversation-session"},"prompt_cache_key":"shared-cache-bucket"}"#,
            "pck:shared-cache-bucket",
        ),
        (
            "client request id beats body fallbacks",
            &[("X-Client-Request-Id", "client-session")],
            r#"{"prompt_cache_key":"prompt-session","conversation":{"id":"conversation-session"}}"#,
            "clientreq:client-session",
        ),
    ];
    for (name, pairs, payload, want) in cases {
        assert_eq!(extract(pairs, payload), want, "{name}");
    }
}

// TestExtractSessionIDRejectsInvalidExplicitSignals, with a tab for the
// newline and the NUL.
#[test]
fn rejects_invalid_explicit_signals() {
    let too_long = "a".repeat(257);
    let cases: [Case<'_>; 6] = [
        ("whitespace", &[("X-Claude-Code-Session-Id", "   ")], "", ""),
        ("newline", &[("X-Session-Id", "bad\tsession")], "", ""),
        (
            "control character",
            &[("Session-Id", "bad\tsession")],
            "",
            "",
        ),
        ("too long", &[("X-Client-Request-Id", &too_long)], "", ""),
        (
            "invalid stronger signal falls through",
            &[
                ("X-Claude-Code-Session-Id", "bad\tsession"),
                ("Session-Id", "valid-codex"),
            ],
            "",
            "codex:valid-codex",
        ),
        (
            "invalid prompt key falls through to conversation",
            &[],
            r#"{"prompt_cache_key":"   ","conversation":{"id":"valid-conversation"}}"#,
            "conv:valid-conversation",
        ),
    ];
    for (name, pairs, payload, want) in cases {
        assert_eq!(extract(pairs, payload), want, "{name}");
    }
}

// TestExtractSessionIDClaudeMetadataParsesBeforeBoundingSessionID.
#[test]
fn claude_metadata_parses_before_bounding_session_id() {
    let session_id = "11111111-1111-4111-8111-111111111111";
    let metadata = serde_json::json!({
        "device_id": "d".repeat(64),
        "account_uuid": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "session_id": session_id,
        "organization_uuid": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        "email": "user@example.com",
    });
    let encodings = [
        (
            "rich compact json",
            serde_json::to_string(&metadata).unwrap(),
        ),
        (
            "pretty printed json",
            serde_json::to_string_pretty(&metadata).unwrap(),
        ),
    ];
    for (name, user_id) in encodings {
        let payload = serde_json::json!({"metadata": {"user_id": user_id}}).to_string();
        assert_eq!(
            extract(&[], &payload),
            format!("claude:{session_id}"),
            "{name}"
        );
    }
}

// TestExtractExplicitSessionIDs_EnhancedHarnesses, parts 1 and 2; part 3
// is in the manager's affinity tests.
#[test]
fn enhanced_harnesses() {
    // 1. Roo Code and Cline's task headers.
    let roo = [
        ("X-Task-ID", "task-abc-1"),
        ("X-Parent-Task-ID", "task-root-1"),
    ];
    assert_eq!(
        ids(&roo, "", "", ""),
        ("task:task-abc-1".into(), "task:task-root-1".into(), false)
    );

    // 2. OpenClaw's forkSource marks a fork.
    let claw = r#"{"sessionId":"claw-c-1","forkSource":{"sessionId":"claw-p-1"}}"#;
    assert_eq!(
        ids(&[], claw, "", ""),
        ("session:claw-c-1".into(), "session:claw-p-1".into(), true)
    );
}

// TestSessionAffinitySelectorPromptCacheKeyCamelCase.
#[test]
fn prompt_cache_key_camel_case() {
    assert_eq!(
        explicit_ids(&[], r#"{"promptCacheKey":"camel-pck-123","input":"hello"}"#),
        ("pck:camel-pck-123".into(), String::new())
    );
}

// TestSessionAffinitySelectorNestedAntigravityPayload, as a nested request
// of no particular provider.
#[test]
fn nested_request_payload() {
    let payload = r#"{
        "project_id": "proj-123",
        "request": {
            "parentSessionId": "parent-456",
            "sessionId": "child-789"
        }
    }"#;
    assert_eq!(
        explicit_ids(&[], payload),
        ("session:child-789".into(), "session:parent-456".into())
    );
}

// TestSessionAffinitySelector_LongCompositeIDBoundConsistency, the bound
// part: a long session is cut and hashed to at most 256 bytes.
#[test]
fn long_composite_id_is_bounded() {
    let session = "s".repeat(200);
    let agent = "a".repeat(100);
    let (primary, fallback, _) = ids(
        &[
            ("X-Claude-Code-Session-Id", &session),
            ("X-Claude-Code-Agent-Id", &agent),
        ],
        "",
        "",
        "",
    );
    assert!(primary.len() <= 256, "length {}", primary.len());
    assert!(primary.contains('#'), "no hash separator in the bound ID");
    assert_eq!(fallback, format!("claude:{session}"));
}

// Not upstream's: a pck with a conversation falls back to it, alone or
// not.
#[test]
fn prompt_cache_key_falls_back_to_the_conversation() {
    let payload = r#"{"prompt_cache_key":"shared-cache-bucket","conversation":{"id":"conversation-session"}}"#;
    assert_eq!(
        explicit_ids(&[], payload),
        (
            "pck:shared-cache-bucket".into(),
            "conv:conversation-session".into()
        )
    );
    assert_eq!(
        explicit_ids(&[], r#"{"conversation":{"id":"conversation-session"}}"#),
        ("conv:conversation-session".into(), String::new())
    );
}

// Not upstream's: isSubagentSession and isHierarchyParent.
#[test]
fn subagent_sessions_and_hierarchy_parents() {
    // An agent's session is a subagent's whatever its fallback.
    assert!(is_subagent_session("claude:root:agent:worker", ""));
    assert!(is_subagent_session(
        "claude:root:agent:worker",
        "claude:root"
    ));
    // The same client's sessions.
    assert!(is_subagent_session("codex:child", "codex:parent"));
    assert!(is_subagent_session("child", "parent"));
    // A prompt cache key and its conversation are one session.
    assert!(!is_subagent_session("pck:bucket", "conv:conversation"));
    assert!(!is_subagent_session("codex:same", "codex:same"));
    assert!(!is_subagent_session("codex:child", ""));
    assert!(!is_subagent_session("", "codex:parent"));
    assert!(!is_subagent_session(":child", ":parent"));
    assert!(!is_subagent_session("child", "codex:parent"));

    assert!(is_hierarchy_parent("session:a:agent:b", "conv:c"));
    assert!(is_hierarchy_parent("task:a", "task:b"));
    assert!(!is_hierarchy_parent("task:a", "header:b"));
    assert!(!is_hierarchy_parent("task:a", ""));
}

// Not upstream's: a session's debug output names no ID.
#[test]
fn session_debug_names_no_id() {
    let session = Session::with_derived(
        &headers(&[("X-Session-Id", "secret-session")]),
        &Payload::parse(b""),
        "",
        "",
    )
    .unwrap();
    let debug = format!("{session:?}");
    assert!(!debug.contains("secret-session"), "{debug}");
}
