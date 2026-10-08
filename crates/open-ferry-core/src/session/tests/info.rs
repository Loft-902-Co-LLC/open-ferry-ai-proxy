// Ported from CLIProxyAPI sdk/cliproxy/session/info_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of reading the session a request names. All of upstream's are
//! ported but `TestDeprecatedInMemorySessionTreeStoreCompatibility`, as the
//! tree store isn't ported.
//!
//! Deviations from upstream:
//! - The Antigravity cases of `TestExtractSessionInfoAllClients` and
//!   `TestExtractSessionInfoGeminiAndAntigravityHierarchy` are left out
//!   (out of scope), and so is the caller scope check, which
//!   [`SessionInfo`](crate::session::SessionInfo) doesn't hold.
//! - The execution session is passed as an argument, not in metadata.

use super::extract;
use crate::session::bound_session_identity;

// TestExtractSessionInfoAllClients, without the Antigravity case and the
// caller scope.
#[test]
fn extract_session_info_all_clients() {
    // 1. Claude Code with a subagent.
    let info = extract(
        &[
            ("X-Claude-Code-Session-Id", "claude-root-123"),
            ("X-Claude-Code-Agent-Id", "subagent-checker"),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.client_type, "claude");
    assert_eq!(
        info.session_id,
        "claude:claude-root-123:agent:subagent-checker"
    );
    assert_eq!(info.parent_session_id, "claude:claude-root-123");
    assert_eq!(info.agent_name, "subagent-checker");

    // 2. Codex CLI with a parent thread.
    let info = extract(
        &[
            ("Session-Id", "codex-child-555"),
            ("x-codex-parent-thread-id", "codex-parent-111"),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.client_type, "codex");
    assert_eq!(info.session_id, "codex:codex-child-555");
    assert_eq!(info.parent_session_id, "codex:codex-parent-111");

    // 2b. A Codex CLI fork, from X-Codex-Turn-Metadata.
    let info = extract(
        &[
            ("Session-Id", "codex-fork-666"),
            (
                "X-Codex-Turn-Metadata",
                r#"{"session_id":"codex-fork-666","forked_from_thread_id":"codex-parent-111","request_kind":"turn"}"#,
            ),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.client_type, "codex");
    assert_eq!(info.session_id, "codex:codex-fork-666");
    assert_eq!(info.parent_session_id, "codex:codex-parent-111");
    assert!(info.is_fork);

    // 2c. Codex CLI's Multi-Agent v2 (collab_spawn).
    let info = extract(
        &[
            ("Session-Id", "codex-root-001"),
            ("Thread-Id", "codex-sub-thread-002"),
            ("X-Openai-Subagent", "collab_spawn"),
            (
                "X-Codex-Turn-Metadata",
                r#"{"session_id":"codex-root-001","thread_id":"codex-sub-thread-002","agent_name":"/root/check_readme","parent_thread_id":"codex-root-001","subagent_kind":"thread_spawn"}"#,
            ),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.client_type, "codex");
    assert_eq!(info.session_id, "codex:codex-root-001:agent:check_readme");
    assert_eq!(info.parent_session_id, "codex:codex-root-001");
    assert_eq!(info.agent_name, "check_readme");
    assert!(info.is_subagent);

    // 3. A Pi slot session.
    let info = extract(&[("X-Slot-Session-Id", "pi-slot-777")], "", "").unwrap();
    assert_eq!(info.client_type, "pi");
    assert_eq!(info.session_id, "slot:pi-slot-777");

    // 4. OpenCode's session affinity and parent.
    let info = extract(
        &[
            ("X-Session-Affinity", "oc-child-999"),
            ("X-Parent-Session-Affinity", "oc-parent-333"),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.client_type, "opencode");
    assert_eq!(info.session_id, "affinity:oc-child-999");
    assert_eq!(info.parent_session_id, "affinity:oc-parent-333");

    // 4b. A fork in the body's thread_id alone.
    let info = extract(
        &[],
        r#"{"thread_id":"child-thread-01","forked_from_thread_id":"parent-thread-00"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "thread:child-thread-01");
    assert_eq!(info.parent_session_id, "thread:parent-thread-00");
    assert!(info.is_fork);
    assert!(!info.is_subagent);

    // 4c. A Codex fork with both Session-Id and Thread-Id.
    let info = extract(
        &[
            ("Session-Id", "parent-thread-00"),
            ("Thread-Id", "child-thread-01"),
            (
                "X-Codex-Turn-Metadata",
                r#"{"session_id":"parent-thread-00","thread_id":"child-thread-01","forked_from_thread_id":"parent-thread-00"}"#,
            ),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "codex:child-thread-01");
    assert_eq!(info.parent_session_id, "codex:parent-thread-00");
    assert!(info.is_fork);

    // 4d. The body's metadata.forked_from_thread_id.
    let info = extract(
        &[],
        r#"{"thread_id":"child-t-99","metadata":{"forked_from_thread_id":"parent-t-88"}}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "thread:child-t-99");
    assert_eq!(info.parent_session_id, "thread:parent-t-88");
    assert!(info.is_fork);

    // 4e. A Codex fork with Session-Id in a header, and thread_id and
    // metadata.forked_from_thread_id in the body.
    let info = extract(
        &[("Session-Id", "parent-sess-uuid")],
        r#"{"thread_id":"child-thread-uuid","metadata":{"forked_from_thread_id":"parent-sess-uuid"}}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "codex:child-thread-uuid");
    assert_eq!(info.parent_session_id, "codex:parent-sess-uuid");
    assert!(info.is_fork);

    // 6. A body with metadata.agent_id and parent_session_id.
    let info = extract(
        &[],
        r#"{
            "session_id": "payload-child-10",
            "parent_session_id": "payload-parent-01",
            "metadata": {
                "agent_id": "analyzer"
            }
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(info.client_type, "generic");
    assert_eq!(info.session_id, "session:payload-child-10:agent:analyzer");
    assert_eq!(info.parent_session_id, "session:payload-parent-01");
}

// TestExtractSessionInfoCanonicalizesPayloadParentForHeaderSessions.
#[test]
fn extract_session_info_canonicalizes_payload_parent_for_header_sessions() {
    let payload = r#"{"parent_session_id":"parent"}"#;
    for (name, header, want_parent) in [
        ("generic header", "X-Session-ID", "header:parent"),
        ("codex header", "Session-Id", "codex:parent"),
        ("claude header", "X-Claude-Code-Session-Id", "claude:parent"),
    ] {
        let info = extract(&[(header, "child")], payload, "").unwrap();
        assert_eq!(info.parent_session_id, want_parent, "{name}");
    }
}

// TestExtractSessionInfoRejectsControlCharacters.
#[test]
fn extract_session_info_rejects_control_characters() {
    assert!(extract(&[], r#"{"session_id": "test\nsession"}"#, "").is_none());
    let null = r#"{"session_id": "test?u0000session"}"#.replace('?', "\\");
    assert!(extract(&[], &null, "").is_none());
}

// TestExtractSessionInfoClaudeNestedParent.
#[test]
fn extract_session_info_claude_nested_parent() {
    let info = extract(
        &[],
        r#"{
            "metadata": {
                "user_id": "{\"session_id\":\"child-session-123\",\"parent_session_id\":\"parent-session-456\",\"agent_id\":\"subagent-worker\"}"
            }
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(
        info.session_id,
        "claude:child-session-123:agent:subagent-worker"
    );
    assert_eq!(info.parent_session_id, "claude:parent-session-456");

    let info = extract(
        &[("X-Claude-Code-Session-Id", "header-child-123")],
        r#"{"parent_session_id":"parent-session-789"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "claude:header-child-123");
    assert_eq!(info.parent_session_id, "claude:parent-session-789");
}

// TestExtractSessionInfoGeminiAndAntigravityHierarchy, without the
// Antigravity half.
#[test]
fn extract_session_info_gemini_hierarchy() {
    let info = extract(
        &[],
        r#"{"cachedContent":"cache-child-1","parent_session_id":"cache-parent-1"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "geminicache:cache-child-1");
    assert_eq!(info.parent_session_id, "geminicache:cache-parent-1");
    assert_eq!(info.agent_name, "subagent");
}

// TestExtractSessionInfoNestedAntigravityRequest: a body that nests its
// request, as Antigravity's does, read as any such body is.
#[test]
fn extract_session_info_nested_request() {
    let info = extract(
        &[],
        r#"{
            "project_id": "proj-123",
            "request": {
                "parentSessionId": "parent-sess-456",
                "sessionId": "child-sess-789"
            }
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "session:child-sess-789");
    assert_eq!(info.parent_session_id, "session:parent-sess-456");
}

// TestExtractSessionInfoThreadAndConversation.
#[test]
fn extract_session_info_thread_and_conversation() {
    let info = extract(&[("X-Thread-Id", "thread-abc-123")], "", "").unwrap();
    assert_eq!(info.client_type, "openai-thread");
    assert_eq!(info.session_id, "thread:thread-abc-123");

    let info = extract(&[("X-Conversation-Id", "conv-xyz-789")], "", "").unwrap();
    assert_eq!(info.client_type, "conv");
    assert_eq!(info.session_id, "conv:conv-xyz-789");

    let info = extract(
        &[],
        r#"{
            "thread_id": "thread-child-1",
            "parent_thread_id": "thread-parent-1"
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(info.client_type, "openai-thread");
    assert_eq!(info.session_id, "thread:thread-child-1");
    assert_eq!(info.parent_session_id, "thread:thread-parent-1");

    let info = extract(
        &[],
        r#"{
            "conversation_id": "conv-child-2",
            "parent_conversation_id": "conv-parent-2"
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(info.client_type, "conv");
    assert_eq!(info.session_id, "conv:conv-child-2");
    assert_eq!(info.parent_session_id, "conv:conv-parent-2");
}

// TestExtractSessionInfoSelfReferentialParentFiltered.
#[test]
fn extract_session_info_self_referential_parent_filtered() {
    let info = extract(
        &[("X-Session-ID", "self-session-123")],
        r#"{"parent_session_id": "self-session-123"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "header:self-session-123");
    assert_eq!(info.parent_session_id, "");
}

// TestExtractSessionInfoClaudePayloadOutranksGenericHeader.
#[test]
fn extract_session_info_claude_payload_outranks_generic_header() {
    let info = extract(
        &[("X-Session-ID", "generic-fallback-session")],
        r#"{
            "metadata": {
                "user_id": "{\"session_id\":\"claude-real-session\",\"agent_id\":\"reviewer\"}"
            }
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(info.client_type, "claude");
    assert_eq!(info.session_id, "claude:claude-real-session:agent:reviewer");
}

// TestExtractSessionInfoPromptCacheKeyAndClientReqAndMetadata.
#[test]
fn extract_session_info_prompt_cache_key_and_client_req_and_metadata() {
    // A conversation beside a prompt cache key is an alias, not a parent.
    let info = extract(
        &[],
        r#"{"prompt_cache_key":"prompt-key-123","conversation":{"id":"conv-456"}}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "pck:prompt-key-123");
    assert_eq!(info.parent_session_id, "");

    let info = extract(
        &[],
        r#"{"prompt_cache_key":"prompt-key-123","conversation":{"id":"conv-456"},"parent_session_id":"pck-parent-789"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "pck:prompt-key-123");
    assert_eq!(info.parent_session_id, "pck:pck-parent-789");

    let info = extract(&[], r#"{"metadata":{"user_id":"user-999"}}"#, "").unwrap();
    assert_eq!(info.session_id, "user:user-999");

    let info = extract(&[("X-Client-Request-Id", "client-req-001")], "", "").unwrap();
    assert_eq!(info.session_id, "clientreq:client-req-001");

    let info = extract(&[], "", "exec-777").unwrap();
    assert_eq!(info.session_id, "execution:exec-777");
}

// TestExtractSessionInfoNestedRequestAgent.
#[test]
fn extract_session_info_nested_request_agent() {
    let info = extract(
        &[],
        r#"{
            "request": {
                "sessionId": "child-sess-1",
                "metadata": {
                    "agent_id": "worker-sub"
                }
            }
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "session:child-sess-1:agent:worker-sub");
    assert_eq!(info.parent_session_id, "session:child-sess-1");
    assert_eq!(info.agent_name, "worker-sub");
}

// TestExtractSessionInfoClaudeMetadataUserIDWithAgentHeader.
#[test]
fn extract_session_info_claude_metadata_user_id_with_agent_header() {
    let info = extract(
        &[("X-Claude-Code-Agent-Id", "subagent-uuid-123")],
        r#"{
            "metadata": {
                "user_id": "{\"device_id\":\"dev-1\",\"session_id\":\"main-sess-456\"}"
            }
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(info.client_type, "claude");
    assert_eq!(
        info.session_id,
        "claude:main-sess-456:agent:subagent-uuid-123"
    );
    assert_eq!(info.parent_session_id, "claude:main-sess-456");
    assert_eq!(info.agent_name, "subagent-uuid-123");
}

// TestExtractSessionInfoNestedSubagentIDAndUserID.
#[test]
fn extract_session_info_nested_subagent_id_and_user_id() {
    let info = extract(
        &[],
        r#"{
            "request": {
                "sessionId": "main-sess-999",
                "metadata": {
                    "subagent_id": "worker-sub-999"
                }
            }
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(
        info.session_id,
        "session:main-sess-999:agent:worker-sub-999"
    );
    assert_eq!(info.parent_session_id, "session:main-sess-999");
    assert_eq!(info.agent_name, "worker-sub-999");

    let info = extract(
        &[],
        r#"{"request": {"metadata": {"user_id": "nested-user-123"}}}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "user:nested-user-123");

    let info = extract(
        &[],
        r#"{"request": {"promptCacheKey": "nested-pck-456"}}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "pck:nested-pck-456");

    // An empty top-level prompt_cache_key doesn't hide the nested one.
    let info = extract(
        &[],
        r#"{
            "prompt_cache_key": "",
            "request": {
                "promptCacheKey": "nested-pck-valid"
            }
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "pck:nested-pck-valid");
}

// TestBoundSessionIdentitySafetyAndUniqueness, with the bound identities
// as Go 1.26.4 gives them for upstream at v8.0.15.
#[test]
fn bound_session_identity_safety_and_uniqueness() {
    let short = "session:normal-length-session";
    assert_eq!(bound_session_identity(short), short);

    let long_cjk = "会话测试超长标识符".repeat(20);
    let bounded = bound_session_identity(&long_cjk);
    assert!(bounded.len() <= 256);
    assert_eq!(
        bounded,
        format!(
            "{}#e8ceacab05585fc45412f0b1fb36551896b65b0fc0cc8afed8dcacaf3e8efba2",
            "会话测试超长标识符".repeat(7)
        )
    );

    let long_a = format!("{}-worker-1", "a".repeat(250));
    let long_b = format!("{}-worker-2", "a".repeat(250));
    let bounded_a = bound_session_identity(&long_a);
    let bounded_b = bound_session_identity(&long_b);
    assert_ne!(bounded_a, bounded_b);
    assert!(bounded_a.len() <= 256 && bounded_b.len() <= 256);
    assert_eq!(
        bounded_a,
        format!(
            "{}#a30da60cdf452ab439a76318b3081fc3e2c7798c6ca5185b4a2490a616964bfa",
            "a".repeat(190)
        )
    );

    let at_bound = "b".repeat(256);
    assert_eq!(bound_session_identity(&at_bound), at_bound);
    assert_eq!(
        bound_session_identity(&"b".repeat(257)),
        format!(
            "{}#cd9c5059c6de0a0e2f1781b2c902b4155ccf8b81c18bc68f3553d5a9be38f1c2",
            "b".repeat(190)
        )
    );
}

// TestExtractSessionInfoEnhancedHarnesses.
#[test]
fn extract_session_info_enhanced_harnesses() {
    // 1. Roo Code and Cline's task headers.
    let info = extract(
        &[
            ("X-Task-ID", "task-child-001"),
            ("X-Parent-Task-ID", "task-parent-001"),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "task:task-child-001");
    assert_eq!(info.parent_session_id, "task:task-parent-001");
    assert_eq!(info.client_type, "task");
    assert_eq!(info.agent_name, "subagent");

    // 2. Roo Code and Cline's body.
    let info = extract(
        &[],
        r#"{"taskId":"task-child-002","parentTaskId":"task-parent-002"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "task:task-child-002");
    assert_eq!(info.parent_session_id, "task:task-parent-002");
    assert_eq!(info.client_type, "task");
    assert_eq!(info.agent_name, "subagent");

    // 3. OpenCode's parent_id and parentID.
    let info = extract(
        &[],
        r#"{"session_id":"opencode-child-100","parent_id":"opencode-parent-100"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "session:opencode-child-100");
    assert_eq!(info.parent_session_id, "session:opencode-parent-100");
    assert_eq!(info.agent_name, "subagent");

    let info = extract(
        &[],
        r#"{"sessionID":"opencode-child-200","parentID":"opencode-parent-200"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "session:opencode-child-200");
    assert_eq!(info.parent_session_id, "session:opencode-parent-200");

    // 4. OpenClaw's forkSource.sessionId and previousSessionId.
    let info = extract(
        &[],
        r#"{"sessionId":"claw-child-1","forkSource":{"sessionId":"claw-parent-1"}}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "session:claw-child-1");
    assert_eq!(info.parent_session_id, "session:claw-parent-1");
    assert!(info.is_fork);

    let info = extract(
        &[],
        r#"{"sessionId":"claw-child-2","previousSessionId":"claw-parent-2"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "session:claw-child-2");
    assert_eq!(info.parent_session_id, "session:claw-parent-2");
    assert!(info.is_fork);

    // 5. Hermes Agent's child_session_id and parent_subagent_id.
    let info = extract(
        &[],
        r#"{"child_session_id":"hermes-child-1","parent_subagent_id":"hermes-parent-1"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "session:hermes-child-1");
    assert_eq!(info.parent_session_id, "session:hermes-parent-1");

    // 6. OpenHands' action_id and parent_action_id.
    let info = extract(
        &[],
        r#"{"action_id":"openhands-action-1","parent_action_id":"openhands-parent-action"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "task:openhands-action-1");
    assert_eq!(info.parent_session_id, "task:openhands-parent-action");
    assert_eq!(info.client_type, "task");

    // 7. Pi Coding Agent's slot parent, and parent_session in the body.
    let info = extract(
        &[
            ("X-Slot-Session-Id", "slot-child-001"),
            ("X-Parent-Slot-Session-Id", "slot-parent-001"),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "slot:slot-child-001");
    assert_eq!(info.parent_session_id, "slot:slot-parent-001");
    assert_eq!(info.client_type, "pi");
    assert_eq!(info.agent_name, "subagent");

    let info = extract(
        &[],
        r#"{"prompt_cache_key":"pi-pck-001","parent_session":"pi-parent-001"}"#,
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "pck:pi-pck-001");
    assert_eq!(info.parent_session_id, "pck:pi-parent-001");

    // 8. Claude Code's metadata.parent_agent_id in the body.
    let info = extract(
        &[],
        r#"{
            "metadata": {
                "user_id": "user_123_acc__session_01a06a06-e830-7da9-a866-98470a94389c",
                "agent_id": "worker-reviewer",
                "parent_agent_id": "orchestrator-main"
            }
        }"#,
        "",
    )
    .unwrap();
    assert_eq!(
        info.session_id,
        "claude:01a06a06-e830-7da9-a866-98470a94389c:agent:worker-reviewer"
    );
    assert_eq!(
        info.parent_session_id,
        "claude:01a06a06-e830-7da9-a866-98470a94389c:agent:orchestrator-main"
    );
    assert_eq!(info.agent_name, "worker-reviewer");

    // 9. The generic parent headers.
    let info = extract(
        &[
            ("X-Session-ID", "gen-child-001"),
            ("X-Parent-ID", "gen-parent-001"),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "header:gen-child-001");
    assert_eq!(info.parent_session_id, "header:gen-parent-001");

    let info = extract(
        &[
            ("X-Conversation-Id", "conv-child-001"),
            ("X-Parent-Conversation-Id", "conv-parent-001"),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "conv:conv-child-001");
    assert_eq!(info.parent_session_id, "conv:conv-parent-001");

    let info = extract(
        &[
            ("X-Thread-Id", "thread-child-001"),
            ("X-Parent-Thread-Id", "thread-parent-001"),
        ],
        "",
        "",
    )
    .unwrap();
    assert_eq!(info.session_id, "thread:thread-child-001");
    assert_eq!(info.parent_session_id, "thread:thread-parent-001");
}
