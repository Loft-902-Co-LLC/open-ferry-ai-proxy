// Ported from CLIProxyAPI sdk/cliproxy/session/identity_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the explicit session checks and the derived identity. Not
//! ported:
//! - `TestEnrich_MetadataOnlyCanonicalAndParentSessionPreserved`: no caller
//!   puts a session in a call's metadata here.
//! - `TestNormalizeToCanonicalUUID`: `NormalizeToCanonicalUUID` isn't
//!   ported.
//! - `TestEnrich_DoesNotClonePayloadWhenPopulatingOriginalRequest_Issue6101`:
//!   it checks that Go shares the body's bytes; nothing here copies them.
//!
//! Deviations from upstream:
//! - The `Enrich` tests check [`derived_session_id`], which returns what
//!   `Enrich` leaves in the metadata for the selector; their cases with a
//!   derived identity already in the metadata are left out, as there is no
//!   such metadata. A header can't hold a line feed, so the header with a
//!   control character holds a tab.
//! - The execution session is passed as an argument, not in metadata.
//! - `TestDeriveIDAntigravityNestedRequestAndEmptyFirstUser` reads with
//!   Gemini's format, which reads a nested request as Antigravity's does;
//!   Antigravity's isn't read.

use super::{extract, headers};
use crate::session::{
    Payload, caller_scope, claude_metadata_identities, derive_id, derived_session_id,
};

/// A case's name, headers, body and execution session.
type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str, &'a str);

/// [`derive_id`] of `payload`.
fn derive(format: &str, payload: &str, scope: &str) -> String {
    derive_id(format, &Payload::parse(payload.as_bytes()), scope)
}

/// [`derived_session_id`] of the request with `pairs` and `payload`.
fn derived(
    pairs: &[(&str, &str)],
    payload: &str,
    execution: &str,
    format: &str,
    scope: &str,
) -> String {
    derived_session_id(
        &headers(pairs),
        &Payload::parse(payload.as_bytes()),
        execution,
        format,
        scope,
    )
}

// TestDeriveIDStableAcrossConversationGrowth.
#[test]
fn derive_id_stable_across_conversation_growth() {
    for (name, format, first, later) in [
        (
            "openai chat",
            "openai",
            r#"{"messages":[{"role":"system","content":"system prompt"},{"role":"developer","content":"developer prompt"},{"role":"user","content":"complete first user prompt"}]}"#,
            r#"{"messages":[{"role":"system","content":"system prompt"},{"role":"developer","content":"developer prompt"},{"role":"user","content":"complete first user prompt"},{"role":"assistant","content":"answer"},{"role":"developer","content":"later instruction"},{"role":"user","content":"next"}]}"#,
        ),
        (
            "claude messages",
            "claude",
            r#"{"system":[{"type":"text","text":"system prompt"}],"messages":[{"role":"user","content":[{"type":"text","text":"complete first user prompt"}]}]}"#,
            r#"{"system":[{"type":"text","text":"system prompt"}],"messages":[{"role":"user","content":[{"type":"text","text":"complete first user prompt"}]},{"role":"assistant","content":"answer"},{"role":"user","content":"next"}]}"#,
        ),
        (
            "openai responses",
            "openai-response",
            r#"{"instructions":"system prompt","input":[{"type":"message","role":"developer","content":[{"type":"input_text","text":"developer prompt"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"complete first user prompt"}]}]}"#,
            r#"{"instructions":"system prompt","input":[{"type":"message","role":"developer","content":[{"type":"input_text","text":"developer prompt"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"complete first user prompt"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#,
        ),
        (
            "gemini",
            "gemini",
            r#"{"systemInstruction":{"parts":[{"text":"system prompt"}]},"contents":[{"role":"user","parts":[{"text":"complete first user prompt"}]}]}"#,
            r#"{"systemInstruction":{"parts":[{"text":"system prompt"}]},"contents":[{"role":"user","parts":[{"text":"complete first user prompt"}]},{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":"next"}]}]}"#,
        ),
        (
            "interactions",
            "interactions",
            r#"{"system_instruction":"system prompt","input":[{"type":"developer_instruction","text":"developer prompt"},{"type":"user_input","content":[{"type":"text","text":"complete first user prompt"}]}]}"#,
            r#"{"system_instruction":"system prompt","input":[{"type":"developer_instruction","text":"developer prompt"},{"type":"user_input","content":[{"type":"text","text":"complete first user prompt"}]},{"type":"model_output","content":[{"type":"text","text":"answer"}]},{"type":"user_input","content":[{"type":"text","text":"next"}]}]}"#,
        ),
    ] {
        let first_id = derive(format, first, "caller-a");
        assert!(!first_id.is_empty(), "{name}");
        assert_eq!(derive(format, later, "caller-a"), first_id, "{name}");
    }
}

// TestDeriveIDInstructionPrefixAndFullUser.
#[test]
fn derive_id_instruction_prefix_and_full_user() {
    let prefix = "界".repeat(50);
    let user = "u".repeat(120);
    let body = |stamp: &str, last: &str| {
        format!(
            r#"{{"messages":[{{"role":"system","content":"{prefix}timestamp-{stamp}"}},{{"role":"user","content":"{user}{last}"}}]}}"#
        )
    };
    let first_id = derive("openai", &body("a", "a"), "caller-a");
    assert!(!first_id.is_empty());
    assert_eq!(derive("openai", &body("b", "a"), "caller-a"), first_id);
    assert_ne!(derive("openai", &body("b", "b"), "caller-a"), first_id);
}

// TestDeriveIDCallerIsolationAndGeminiCachedContent.
#[test]
fn derive_id_caller_isolation_and_gemini_cached_content() {
    let payload = r#"{"messages":[{"role":"user","content":"same prompt"}]}"#;
    let caller_a = derive("openai", payload, &caller_scope("api-key-a"));
    let caller_b = derive("openai", payload, &caller_scope("api-key-b"));
    assert!(!caller_a.is_empty() && !caller_b.is_empty());
    assert_ne!(caller_a, caller_b);

    let first = derive(
        "gemini",
        r#"{"cachedContent":"cachedContents/abc","contents":[{"role":"user","parts":[{"text":"first"}]}]}"#,
        "caller-a",
    );
    let grown = derive(
        "gemini",
        r#"{"cachedContent":"cachedContents/abc","contents":[{"role":"user","parts":[{"text":"first"}]},{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":"next"}]}]}"#,
        "caller-a",
    );
    let different = derive(
        "gemini",
        r#"{"cachedContent":"cachedContents/abc","contents":[{"role":"user","parts":[{"text":"different"}]}]}"#,
        "caller-a",
    );
    assert!(!first.is_empty());
    assert_eq!(first, grown);
    assert_ne!(different, first);
}

// TestDeriveIDRequiresFirstUser.
#[test]
fn derive_id_requires_first_user() {
    let payload = r#"{"messages":[{"role":"system","content":"shared system"}]}"#;
    assert_eq!(derive("openai", payload, "caller-a"), "");
}

// TestEnrichSkipsDerivationForExplicitSessions, without the case with a
// stale derived identity in the metadata.
#[test]
fn enrich_skips_derivation_for_explicit_sessions() {
    let hello = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
    let long_legacy = format!(
        r#"{{"metadata":{{"user_id":"{}_session_ac980658-63bd-4fb3-97ba-8da64cb1e344"}},"messages":[{{"role":"user","content":"hello"}}]}}"#,
        "x".repeat(300)
    );
    let cases: [Case<'_>; 15] = [
        (
            "session header avoids malformed body parsing",
            &[("X-Session-ID", "header-session")],
            "not-json",
            "",
        ),
        (
            "Claude Code session header",
            &[("X-Claude-Code-Session-Id", "claude-session")],
            hello,
            "",
        ),
        (
            "later valid multi-value session header",
            &[
                ("X-Session-Affinity", ""),
                ("X-Session-Affinity", "later-valid-session"),
            ],
            hello,
            "",
        ),
        (
            "OpenCode affinity header",
            &[("X-Session-Affinity", "opencode-session")],
            hello,
            "",
        ),
        (
            "Responses conversation object",
            &[],
            r#"{"conversation":{"id":"conversation-session"},"messages":[{"role":"user","content":"hello"}]}"#,
            "",
        ),
        (
            "Responses conversation string",
            &[],
            r#"{"conversation":"conversation-session","messages":[{"role":"user","content":"hello"}]}"#,
            "",
        ),
        (
            "metadata user id",
            &[],
            r#"{"metadata":{"user_id":"explicit-user"},"messages":[{"role":"user","content":"hello"}]}"#,
            "",
        ),
        ("long legacy Claude metadata session", &[], &long_legacy, ""),
        (
            "JSON metadata user id without nested session",
            &[],
            r#"{"metadata":{"user_id":"{\"device_id\":\"abc123\"}"},"messages":[{"role":"user","content":"hello"}]}"#,
            "",
        ),
        (
            "body session id",
            &[],
            r#"{"session_id":"body-session","messages":[{"role":"user","content":"hello"}]}"#,
            "",
        ),
        (
            "prompt cache key",
            &[],
            r#"{"prompt_cache_key":"cache-session","input":"hello"}"#,
            "",
        ),
        ("execution session", &[], hello, "execution-session"),
        (
            "explicit header",
            &[("x-session-id", "header-session")],
            hello,
            "",
        ),
        (
            "nested request sessionId",
            &[],
            r#"{"request":{"sessionId":"nested-session"},"messages":[{"role":"user","content":"hello"}]}"#,
            "",
        ),
        (
            "nested request subagent",
            &[],
            r#"{"request":{"sessionId":"nested-session","metadata":{"agent_id":"worker"}},"messages":[{"role":"user","content":"hello"}]}"#,
            "",
        ),
    ];
    for (name, pairs, payload, execution) in cases {
        assert_eq!(
            derived(pairs, payload, execution, "openai", ""),
            "",
            "{name}"
        );
    }
    assert!(!derived(&[], hello, "", "openai", "").is_empty());
}

// TestEnrichDerivesAfterInvalidSessionIdentity, without the cases with a
// derived identity already in the metadata.
#[test]
fn enrich_derives_after_invalid_session_identity() {
    let oversized = format!(
        r#"{{"prompt_cache_key":"{}","input":"hello"}}"#,
        "x".repeat(257)
    );
    let long_execution = "x".repeat(257);
    let cases: [Case<'_>; 6] = [
        ("oversized prompt cache key", &[], &oversized, ""),
        (
            "trailing control character prompt cache key",
            &[],
            r#"{"prompt_cache_key":"tenant\n","input":"hello"}"#,
            "",
        ),
        (
            "leading control character prompt cache key",
            &[],
            r#"{"prompt_cache_key":"\ttenant","input":"hello"}"#,
            "",
        ),
        (
            "control character session header",
            &[("X-Session-Affinity", "bad\tsession")],
            r#"{"input":"hello"}"#,
            "",
        ),
        (
            "oversized execution session",
            &[],
            r#"{"input":"hello"}"#,
            &long_execution,
        ),
        (
            "control character execution session",
            &[],
            r#"{"input":"hello"}"#,
            "bad\nsession",
        ),
    ];
    for (name, pairs, payload, execution) in cases {
        let want = derive("openai-response", payload, "");
        assert!(!want.is_empty(), "{name}");
        assert_eq!(
            derived(pairs, payload, execution, "openai-response", ""),
            want,
            "{name}"
        );
    }
}

// TestEnrichCopiesDerivedIdentityToRequestAndOptions, as the identity
// derived under the caller's scope: nothing is copied into metadata.
#[test]
fn enrich_derives_identity_under_caller_scope() {
    let payload = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
    let id = derived(&[], payload, "", "openai", "caller-a");
    assert!(!id.is_empty());
    assert_eq!(id, derive("openai", payload, "caller-a"));
    assert_ne!(id, derive("openai", payload, ""));
}

// TestDeriveIDAntigravityNestedRequestAndEmptyFirstUser, with Gemini's
// format.
#[test]
fn derive_id_nested_request_and_empty_first_user() {
    let nested = r#"{
        "project_id": "test-project",
        "request": {
            "systemInstruction": {"parts":[{"text":"system prompt"}]},
            "contents": [
                {"role":"user","parts":[{"text":""}]},
                {"role":"user","parts":[{"text":"actual user prompt"}]}
            ]
        }
    }"#;
    let id = derive("gemini", nested, "caller-a");
    assert!(!id.is_empty());
    let direct = r#"{
        "systemInstruction": {"parts":[{"text":"system prompt"}]},
        "contents": [
            {"role":"user","parts":[{"text":"actual user prompt"}]}
        ]
    }"#;
    assert_eq!(derive("gemini", direct, "caller-a"), id);
}

// TestClaudeMetadataIdentitiesNormalizesAgentID.
#[test]
fn claude_metadata_identities_normalizes_agent_id() {
    let valid = Payload::parse(
        br#"{
            "metadata": {
                "user_id": "{\"session_id\":\"sess-123\",\"parent_session_id\":\"parent-456\",\"agent_id\":\"  subagent-alpha  \"}"
            }
        }"#,
    );
    let (session, parent, agent) = claude_metadata_identities(&valid);
    assert_eq!(session, "sess-123");
    assert_eq!(parent, "parent-456");
    assert_eq!(agent, "subagent-alpha");

    let invalid = Payload::parse(
        br#"{
            "metadata": {
                "user_id": "{\"session_id\":\"sess-123\",\"agent_id\":\"bad\nagent\"}"
            }
        }"#,
    );
    let (_, _, agent) = claude_metadata_identities(&invalid);
    assert_eq!(agent, "");
}

// TestEnrich_ExplicitSessionPreferredOverExecutionSession, as the session
// read and the identity derived.
#[test]
fn enrich_explicit_session_preferred_over_execution_session() {
    let payload = r#"{"session_id":"explicit-ws-session"}"#;
    let info = extract(&[], payload, "conn-ws-uuid-123").unwrap();
    assert_eq!(info.session_id, "session:explicit-ws-session");
    assert_eq!(derived(&[], payload, "conn-ws-uuid-123", "codex", ""), "");

    let payload = r#"{"model":"test"}"#;
    let info = extract(&[], payload, "conn-ws-uuid-456").unwrap();
    assert_eq!(info.session_id, "execution:conn-ws-uuid-456");
    assert_eq!(derived(&[], payload, "conn-ws-uuid-456", "codex", ""), "");
}

// Not upstream's: Antigravity's session header isn't an explicit session
// here, so a request with only it still gets a derived identity.
#[test]
fn antigravity_session_header_is_not_read() {
    let payload = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
    assert_eq!(
        derived(&[("X-Http-Session-Id", "agy")], payload, "", "openai", ""),
        derive("openai", payload, "")
    );
    assert!(extract(&[("X-Http-Session-Id", "agy")], "", "").is_none());
}

// Not upstream's: the caller scope, as Go 1.26.4 computes it for upstream
// at v8.0.15.
#[test]
fn caller_scope_matches_upstream() {
    assert_eq!(
        caller_scope(" api-key-a "),
        "b2c45f7e2a92f31846c1ea2952de1b21659c5d6009125ef6623ab836be30a93b"
    );
    assert_eq!(caller_scope(" \t "), "");
}

/// Format, caller scope, body, and the identity upstream's `DeriveID`
/// derives, as Go 1.26.4 computes it for upstream at v8.0.15.
const DERIVE_GOLDENS: &[(&str, &str, &str, &str)] = &[
    // chat basic
    (
        "openai",
        "",
        r#"{"messages":[{"role":"user","content":"hello"}]}"#,
        "ctx:v1:d4577c88ea75a15ee504445cf3b6293f3b31266eaeb01efd438959ef84bc6a8b",
    ),
    // chat scope trimmed
    (
        "openai",
        "  caller-a  ",
        r#"{"messages":[{"role":"user","content":"hello"}]}"#,
        "ctx:v1:45d873f969582206f77b1f0bad393102b64fe49ef910a1d0f116e8fd5f63ce7d",
    ),
    // chat instructions
    (
        "openai",
        "7d0c1e5f",
        r#"{"messages":[{"role":"system","content":"界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},{"role":"developer","content":[{"type":"text","text":"a"},{"type":"text","text":""},{"type":"image_url","image_url":"x"},{"type":"text","text":"b"}]},{"role":"assistant","content":"skip"},{"role":"user","content":"q"}]}"#,
        "ctx:v1:612fa213b88b059aa93fb4b02f77d09528730d7f29459cfadafb93e826eb67b1",
    ),
    // chat escapes
    (
        "openai",
        "",
        r#"{"messages":[{"role":"user","content":"<b>&amp;</b> \u2028 \u2029 é 界 \u0001 \b \f \t \n \r \u007f \"q\" \\ \/ 😀"}]}"#,
        "ctx:v1:dee1dfca2ffe096a9643125d53f01fd9add74980ad1cc690459e88901b19dc46",
    ),
    // chat json parts
    (
        "openai",
        "",
        r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":false,"n":100000000000000000000000,"f":1.5e300,"g":1e-7,"h":0.000001,"i":-0.0,"i2":-0e5,"j":123456789012345678,"k":1E21,"l":2.50,"m":1e20,"o":-1.25e-10,"cache_control":{"type":"ephemeral"},"z":[1,true,null,{" CACHE_CONTROL ":1,"b":"<x>","a":"y","B":[]}]},42,true,1.0e2,{"text":5}]}]}"#,
        "ctx:v1:68650067e9eff612988a364b79a68a4cf5a660b1afa511c6d36143561dcbe86e",
    ),
    // claude media
    (
        "claude",
        "",
        r#"{"system":[{"type":"text","text":"sys","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}},{"type":" Document ","media_type":" Application/PDF ","source":"https://example.test/a.pdf"},{"type":"image_url","image_url":{"url":" https://example.test/i.png "}},{"image_url":"data:x"},{"inlineData":{"mimeType":"image/jpeg","data":"BBBB"}},{"inline_data":{"mime_type":"audio/wav","data":""}},{"fileData":{"fileUri":"gs://b/f","mimeType":"video/mp4"}},{"file_data":"plain-string"},{"source":{"url":"u"}},{"type":"x","source":[{"text":"in-array"}]},{"source":null},{"image_url":{"uri":"v","media_type":"m"}},{"source":7}]}]}"#,
        "ctx:v1:a5308106b6d9058f59e0e5f66e010083464071aa111923d919299c73cfba3742",
    ),
    // claude skips empty users
    (
        "claude",
        "",
        r#"{"system":"s","messages":[{"role":"user","content":""},{"role":"user","content":[]},{"role":"assistant","content":"a"},"stray",{"role":"user","content":[{"type":"text","text":"real"}]}]}"#,
        "ctx:v1:818140c51cfc6dcb62cce9e198532a51425d55225fd92e8d17872e9c7ea49164",
    ),
    // responses string
    (
        "openai-response",
        "",
        r#"{"instructions":"ins","input":"hello"}"#,
        "ctx:v1:414d450222028773fa295f3ebf25878315cc14484867ae19c2c7ebde4d962954",
    ),
    // codex items
    (
        "codex",
        "",
        r#"{"instructions":[{"type":"input_text","text":"x"}],"input":[{"role":"developer","content":"dev"},{"type":"function_call_output","output":"o"},"str",{"role":"USER ","content":[{"type":"input_text","text":"q"},{"type":"input_image","image_url":"https://example.test/p.png"}]}]}"#,
        "ctx:v1:d09bc01a9147a17166e2b399256866265bb160762f1548de915f7e48dde460e2",
    ),
    // responses object input
    (
        "openai-response",
        "",
        r#"{"instructions":"ins","input":{"role":"user","content":"x"}}"#,
        "",
    ),
    // gemini nested
    (
        "gemini",
        "",
        r#"{"project":"p","request":{"cached_content":" cachedContents/x ","system_instruction":{"parts":[{"text":"sys"}]},"contents":[{"role":"user","parts":[{"text":""}]},{"role":"user","parts":[{"text":"q"},{"inlineData":{"mimeType":"image/png","data":"CCC"}}]}]}}"#,
        "ctx:v1:a5113c0d6711e867813c662834fc546bc3dd0ffa29f3f27edcb0391f228b1efb",
    ),
    // gemini direct
    (
        "gemini",
        "",
        r#"{"systemInstruction":{"parts":[{"text":"sys"}]},"contents":[{"role":"user","parts":[{"text":"q"},{"inlineData":{"mimeType":"image/png","data":"CCC"}}]}],"cachedContent":"cachedContents/x"}"#,
        "ctx:v1:a5113c0d6711e867813c662834fc546bc3dd0ffa29f3f27edcb0391f228b1efb",
    ),
    // gemini text content
    (
        "gemini",
        "",
        r#"{"systemInstruction":"plain","contents":[{"role":"model","parts":[{"text":"m"}]},{"role":"user","text":"t"}]}"#,
        "ctx:v1:93a233fdbecc5165c815b9f3d35282798dcee242313a6d46b7d86a71ab7aa3e9",
    ),
    // interactions steps
    (
        "interactions",
        "",
        r#"{"systemInstruction":{"content":"sys"},"input":[{"type":"system_instruction","text":"s2"},{"role":"model","steps":[{"type":"text","text":"m"}]},{"role":"user","steps":[{"type":"text","text":"u1"},{"type":"text","text":"u2"}]}]}"#,
        "ctx:v1:8d43992e6c9929553447ad5d1cdb86c3de6ae3a45c77d88b0b3f7fed1edb03c1",
    ),
    // interactions string entry
    (
        "interactions",
        "",
        r#"{"input":[{"type":"model_output","content":"x"},"plain user"]}"#,
        "ctx:v1:a783b79ed03cf641059462f37a6db72c220f4d6eddb25aa5c115ffc07fa212c9",
    ),
    // interactions untyped
    (
        "interactions",
        "",
        r#"{"input":[[{"content":[{"type":"text","text":"hi"}]}]]}"#,
        "ctx:v1:50f40ff090573d922a41698341526bdc93c6d4c5056809c8ca8bc16327d9ddb7",
    ),
    // interactions input string
    (
        "interactions",
        "",
        r#"{"system_instruction":"s","input":"hello"}"#,
        "ctx:v1:6b7d2afa0ea3ac2092a3b534a05c9cf4d28202c0cfc68727a033f855ba5193e6",
    ),
    // interactions role steps string
    (
        "interactions",
        "",
        r#"{"input":[{"role":"user","steps":"x","text":"t"}]}"#,
        "ctx:v1:d2e4861799960a43bcda90cd85ae76a622aa3db4ca66993932b315ded68d72c8",
    ),
    // interactions developer role
    (
        "interactions",
        "",
        r#"{"input":[{"role":"developer","content":"d"},{"type":"user_input","parts":[{"text":"u"}]}]}"#,
        "ctx:v1:a89b90c9d67c0218afe25e1b81d6a53181bf883c483928eca79b0c8d2b045da7",
    ),
    // format case
    (
        " Claude ",
        "",
        r#"{"system":"sys","messages":[{"role":"user","content":"q"}]}"#,
        "ctx:v1:035d947bb7e8b863f4571d65b8b16c55c014c0c45a46480df4a76c7ea46cb1c2",
    ),
    // unknown format
    (
        "something",
        "",
        r#"{"system":"ignored","messages":[{"role":"user","content":"q"}]}"#,
        "ctx:v1:3167a8e3902bde1f5a9faa1325ed03f63c5f36d41eb8c1fa346ed2653c28375f",
    ),
    // huge number
    (
        "openai",
        "",
        r#"{"messages":[{"role":"user","content":"x"}],"n":1e400}"#,
        "",
    ),
    // not object
    (
        "openai",
        "",
        r#"[{"messages":[{"role":"user","content":"x"}]}]"#,
        "",
    ),
    // trailing text
    (
        "openai",
        "",
        r#"{"messages":[{"role":"user","content":"x"}]} x"#,
        "",
    ),
    // whitespace around
    (
        "openai",
        "",
        " \n{\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}]}\t\r\n",
        "ctx:v1:d4577c88ea75a15ee504445cf3b6293f3b31266eaeb01efd438959ef84bc6a8b",
    ),
    // duplicate key
    (
        "openai",
        "",
        r#"{"messages":[{"role":"user","content":"a","content":"b"}]}"#,
        "ctx:v1:23e94a91edea9ee876666ee24526d52800715767fe9d802930485fd146e26050",
    ),
    // no user
    (
        "openai",
        "",
        r#"{"messages":[{"role":"system","content":"s"}]}"#,
        "",
    ),
    // null body
    ("openai", "", r#"null"#, ""),
    // null content
    (
        "openai",
        "",
        r#"{"messages":[{"role":"user","content":null},{"role":"user","content":{"text":5}}]}"#,
        "ctx:v1:61161be59376a52c00b3d0f4b3c1463e9454d71cec49bc545b911b9c8eac83dd",
    ),
];

// Not upstream's: identities derived from each request format, with
// media, parts given as JSON, numbers, escapes, nested steps and bodies
// that derive none, as upstream derives them.
#[test]
fn derive_id_matches_upstream() {
    for (format, scope, payload, want) in DERIVE_GOLDENS {
        assert_eq!(derive(format, payload, scope), *want, "{payload}");
    }
}
