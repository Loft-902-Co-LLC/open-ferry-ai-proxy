//! Hand-written cases for session reading: the sessions of upstream's
//! ExtractSessionInfo tests (sdk/cliproxy/session/info_test.go and
//! info_duplicate_test.go) and the bodies of its DeriveID tests
//! (identity_test.go), with, not upstream's, the corners those don't
//! reach. Upstream's tests compare identities with each other; here each
//! identity is compared with upstream's.

use serde_json::{Value, json};

use super::{Case, escaped};

/// Why an object or array where an ID goes reads differently.
const COMPACT_CONTAINER: &str = "the text of an object or array where an ID goes is its compact JSON; gjson gives it as written";

/// A `session/info` case: `headers` as name and value pairs, and `body`.
fn info(name: &str, headers: &[(&str, &str)], body: &str) -> Case {
    executed(name, headers, body, "")
}

/// A `session/info` case on a connection with the execution session
/// `execution_id`.
fn executed(name: &str, headers: &[(&str, &str)], body: &str, execution_id: &str) -> Case {
    let headers: Vec<Value> = headers
        .iter()
        .map(|(name, value)| json!([name, value]))
        .collect();
    Case::new(name, "", body).with_options(json!({
        "headers": headers,
        "execution_id": execution_id,
    }))
}

/// A `session/derive` case for a body from a client in `format`, for the
/// caller `caller_scope`.
fn derive(name: &str, format: &str, caller_scope: &str, body: &str) -> Case {
    Case::new(name, "", body).with_options(json!({
        "format": format,
        "caller_scope": caller_scope,
    }))
}

/// The hand-written cases for `session/info`.
pub fn infos() -> Vec<Case> {
    let mut cases = all_clients();
    cases.extend(hierarchies());
    cases.extend(enhanced_harnesses());
    cases.extend(not_upstreams_infos());
    cases
}

/// Upstream's TestExtractSessionInfoAllClients, without its Antigravity
/// header (out of scope).
fn all_clients() -> Vec<Case> {
    vec![
        info(
            "claude-code-subagent",
            &[
                ("X-Claude-Code-Session-Id", "claude-root-123"),
                ("X-Claude-Code-Agent-Id", "subagent-checker"),
            ],
            "",
        ),
        info(
            "codex-parent-thread",
            &[
                ("Session-Id", "codex-child-555"),
                ("x-codex-parent-thread-id", "codex-parent-111"),
            ],
            "",
        ),
        info(
            "codex-fork-turn-metadata",
            &[
                ("Session-Id", "codex-fork-666"),
                (
                    "X-Codex-Turn-Metadata",
                    r#"{"session_id":"codex-fork-666","forked_from_thread_id":"codex-parent-111","request_kind":"turn"}"#,
                ),
            ],
            "",
        ),
        info(
            "codex-multi-agent-v2",
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
        ),
        info("pi-slot", &[("X-Slot-Session-Id", "pi-slot-777")], ""),
        info(
            "opencode-affinity-parent",
            &[
                ("X-Session-Affinity", "oc-child-999"),
                ("X-Parent-Session-Affinity", "oc-parent-333"),
            ],
            "",
        ),
        info(
            "body-only-thread-fork",
            &[],
            r#"{"thread_id":"child-thread-01","forked_from_thread_id":"parent-thread-00"}"#,
        ),
        info(
            "codex-fork-session-and-thread",
            &[
                ("Session-Id", "parent-thread-00"),
                ("Thread-Id", "child-thread-01"),
                (
                    "X-Codex-Turn-Metadata",
                    r#"{"session_id":"parent-thread-00","thread_id":"child-thread-01","forked_from_thread_id":"parent-thread-00"}"#,
                ),
            ],
            "",
        ),
        info(
            "nested-metadata-fork",
            &[],
            r#"{"thread_id":"child-t-99","metadata":{"forked_from_thread_id":"parent-t-88"}}"#,
        ),
        info(
            "codex-header-body-fork",
            &[("Session-Id", "parent-sess-uuid")],
            r#"{"thread_id":"child-thread-uuid","metadata":{"forked_from_thread_id":"parent-sess-uuid"}}"#,
        ),
        info(
            "payload-agent-and-parent",
            &[],
            "{\n\t\t\"session_id\": \"payload-child-10\",\n\t\t\"parent_session_id\": \"payload-parent-01\",\n\t\t\"metadata\": {\n\t\t\t\"agent_id\": \"analyzer\"\n\t\t}\n\t}",
        ),
    ]
}

/// Upstream's tests of parents, forks, threads, conversations and nested
/// requests, from TestExtractSessionInfoCanonicalizesPayloadParentForHeaderSessions
/// to TestExtractSessionInfoNestedSubagentIDAndUserID, and
/// TestDuplicateMetadataKeysPreserveSessionLookup.
fn hierarchies() -> Vec<Case> {
    let parent = r#"{"parent_session_id":"parent"}"#;
    let claude_user_id = r#"{"metadata":{"user_id":"{\"session_id\":\"child-session-123\",\"parent_session_id\":\"parent-session-456\",\"agent_id\":\"subagent-worker\"}"}}"#;
    vec![
        // TestExtractSessionInfoCanonicalizesPayloadParentForHeaderSessions.
        info(
            "payload-parent-generic-header",
            &[("X-Session-ID", "child")],
            parent,
        ),
        info(
            "payload-parent-codex-header",
            &[("Session-Id", "child")],
            parent,
        ),
        info(
            "payload-parent-claude-header",
            &[("X-Claude-Code-Session-Id", "child")],
            parent,
        ),
        // TestExtractSessionInfoRejectsControlCharacters.
        info(
            "session-id-with-newline",
            &[],
            r#"{"session_id": "test\nsession"}"#,
        ),
        info(
            "session-id-with-null",
            &[],
            &format!(r#"{{"session_id": "test{}session"}}"#, escaped('\0')),
        ),
        // TestExtractSessionInfoClaudeNestedParent.
        info("claude-user-id-parent-and-agent", &[], claude_user_id),
        info(
            "claude-header-body-parent",
            &[("X-Claude-Code-Session-Id", "header-child-123")],
            r#"{"parent_session_id":"parent-session-789"}"#,
        ),
        // TestExtractSessionInfoGeminiAndAntigravityHierarchy, without its
        // Antigravity headers (out of scope).
        info(
            "gemini-cached-content-parent",
            &[],
            r#"{"cachedContent":"cache-child-1","parent_session_id":"cache-parent-1"}"#,
        ),
        // TestExtractSessionInfoNestedAntigravityRequest: a body nesting its
        // request, which is read the same whatever the client.
        info(
            "nested-request-session-and-parent",
            &[],
            r#"{"project_id": "proj-123","request": {"parentSessionId": "parent-sess-456","sessionId": "child-sess-789"}}"#,
        ),
        // TestExtractSessionInfoThreadAndConversation.
        info("thread-header", &[("X-Thread-Id", "thread-abc-123")], ""),
        info(
            "conversation-header",
            &[("X-Conversation-Id", "conv-xyz-789")],
            "",
        ),
        info(
            "payload-thread-parent",
            &[],
            r#"{"thread_id": "thread-child-1", "parent_thread_id": "thread-parent-1"}"#,
        ),
        info(
            "payload-conversation-parent",
            &[],
            r#"{"conversation_id": "conv-child-2", "parent_conversation_id": "conv-parent-2"}"#,
        ),
        // TestExtractSessionInfoSelfReferentialParentFiltered.
        info(
            "self-referential-parent",
            &[("X-Session-ID", "self-session-123")],
            r#"{"parent_session_id": "self-session-123"}"#,
        ),
        // TestExtractSessionInfoClaudePayloadOutranksGenericHeader.
        info(
            "claude-payload-outranks-generic-header",
            &[("X-Session-ID", "generic-fallback-session")],
            r#"{"metadata": {"user_id": "{\"session_id\":\"claude-real-session\",\"agent_id\":\"reviewer\"}"}}"#,
        ),
        // TestExtractSessionInfoPromptCacheKeyAndClientReqAndMetadata.
        info(
            "prompt-cache-key-conversation-alias",
            &[],
            r#"{"prompt_cache_key":"prompt-key-123","conversation":{"id":"conv-456"}}"#,
        ),
        info(
            "prompt-cache-key-parent",
            &[],
            r#"{"prompt_cache_key":"prompt-key-123","conversation":{"id":"conv-456"},"parent_session_id":"pck-parent-789"}"#,
        ),
        info(
            "plain-user-id",
            &[],
            r#"{"metadata":{"user_id":"user-999"}}"#,
        ),
        info(
            "client-request-id",
            &[("X-Client-Request-Id", "client-req-001")],
            "",
        ),
        executed("execution-session", &[], "", "exec-777"),
        // TestExtractSessionInfoNestedRequestAgent.
        info(
            "nested-request-agent",
            &[],
            r#"{"request": {"sessionId": "child-sess-1", "metadata": {"agent_id": "worker-sub"}}}"#,
        ),
        // TestExtractSessionInfoClaudeMetadataUserIDWithAgentHeader.
        info(
            "claude-user-id-agent-header",
            &[("X-Claude-Code-Agent-Id", "subagent-uuid-123")],
            r#"{"metadata": {"user_id": "{\"device_id\":\"dev-1\",\"session_id\":\"main-sess-456\"}"}}"#,
        ),
        // TestExtractSessionInfoNestedSubagentIDAndUserID.
        info(
            "nested-subagent-id",
            &[],
            r#"{"request": {"sessionId": "main-sess-999", "metadata": {"subagent_id": "worker-sub-999"}}}"#,
        ),
        info(
            "nested-plain-user-id",
            &[],
            r#"{"request": {"metadata": {"user_id": "nested-user-123"}}}"#,
        ),
        info(
            "nested-prompt-cache-key",
            &[],
            r#"{"request": {"promptCacheKey": "nested-pck-456"}}"#,
        ),
        info(
            "nested-prompt-cache-key-shadowed",
            &[],
            r#"{"prompt_cache_key": "", "request": {"promptCacheKey": "nested-pck-valid"}}"#,
        ),
        // TestDuplicateMetadataKeysPreserveSessionLookup.
        info(
            "duplicate-metadata-session",
            &[],
            r#"{"metadata":{},"metadata":{"session_id":"child"}}"#,
        ),
        info(
            "duplicate-nested-metadata-session",
            &[],
            r#"{"request":{"metadata":{},"metadata":{"session_id":"child"}}}"#,
        ),
        info(
            "duplicate-metadata-parent",
            &[("X-Session-Id", "child")],
            r#"{"metadata":{},"metadata":{"parent_session_id":"parent"}}"#,
        ),
    ]
}

/// Upstream's TestExtractSessionInfoEnhancedHarnesses.
fn enhanced_harnesses() -> Vec<Case> {
    vec![
        info(
            "roo-task-headers",
            &[
                ("X-Task-ID", "task-child-001"),
                ("X-Parent-Task-ID", "task-parent-001"),
            ],
            "",
        ),
        info(
            "roo-task-payload",
            &[],
            r#"{"taskId":"task-child-002","parentTaskId":"task-parent-002"}"#,
        ),
        info(
            "opencode-parent-id",
            &[],
            r#"{"session_id":"opencode-child-100","parent_id":"opencode-parent-100"}"#,
        ),
        info(
            "opencode-parent-id-camel",
            &[],
            r#"{"sessionID":"opencode-child-200","parentID":"opencode-parent-200"}"#,
        ),
        info(
            "openclaw-fork-source",
            &[],
            r#"{"sessionId":"claw-child-1","forkSource":{"sessionId":"claw-parent-1"}}"#,
        ),
        info(
            "openclaw-previous-session",
            &[],
            r#"{"sessionId":"claw-child-2","previousSessionId":"claw-parent-2"}"#,
        ),
        info(
            "hermes-child-session",
            &[],
            r#"{"child_session_id":"hermes-child-1","parent_subagent_id":"hermes-parent-1"}"#,
        ),
        info(
            "openhands-action",
            &[],
            r#"{"action_id":"openhands-action-1","parent_action_id":"openhands-parent-action"}"#,
        ),
        info(
            "pi-slot-parent",
            &[
                ("X-Slot-Session-Id", "slot-child-001"),
                ("X-Parent-Slot-Session-Id", "slot-parent-001"),
            ],
            "",
        ),
        info(
            "pi-parent-session-payload",
            &[],
            r#"{"prompt_cache_key":"pi-pck-001","parent_session":"pi-parent-001"}"#,
        ),
        info(
            "claude-legacy-user-id-parent-agent",
            &[],
            r#"{"metadata": {"user_id": "user_123_acc__session_01a06a06-e830-7da9-a866-98470a94389c", "agent_id": "worker-reviewer", "parent_agent_id": "orchestrator-main"}}"#,
        ),
        info(
            "generic-parent-id-header",
            &[
                ("X-Session-ID", "gen-child-001"),
                ("X-Parent-ID", "gen-parent-001"),
            ],
            "",
        ),
        info(
            "conversation-parent-header",
            &[
                ("X-Conversation-Id", "conv-child-001"),
                ("X-Parent-Conversation-Id", "conv-parent-001"),
            ],
            "",
        ),
        info(
            "thread-parent-header",
            &[
                ("X-Thread-Id", "thread-child-001"),
                ("X-Parent-Thread-Id", "thread-parent-001"),
            ],
            "",
        ),
    ]
}

/// Not upstream's: the corners its tests don't reach.
fn not_upstreams_infos() -> Vec<Case> {
    let long = "s".repeat(257);
    let longest = "s".repeat(256);
    vec![
        info("nothing", &[], ""),
        info("empty-object", &[], "{}"),
        info("array-body", &[], r#"[{"session_id":"in-array"}]"#),
        info(
            "text-before-body",
            &[],
            r#"data: {"session_id":"after-text"}"#,
        ),
        info(
            "white-space-before-body",
            &[],
            " \r\n\t{\"session_id\":\"after-space\"}",
        ),
        info(
            "header-value-trimmed",
            &[("X-Session-Id", "  padded  ")],
            "",
        ),
        info(
            "second-header-value",
            &[("X-Session-Id", " "), ("X-Session-Id", "second")],
            "",
        ),
        info("header-over-256-bytes", &[("X-Session-Id", &long)], ""),
        info("header-of-256-bytes", &[("X-Session-Id", &longest)], ""),
        info(
            "header-with-tab",
            &[("X-Session-Id", "a\tb"), ("X-Thread-Id", "next")],
            "",
        ),
        executed(
            "explicit-session-outranks-execution",
            &[("X-Session-Id", "explicit")],
            "",
            "exec-1",
        ),
        executed("execution-session-blank", &[], "", "  "),
        executed("execution-session-control", &[], "", "exec\u{1}id"),
        info(
            "turn-metadata-not-json",
            &[("X-Codex-Turn-Metadata", "not json")],
            "",
        ),
        info(
            "turn-metadata-session-only",
            &[(
                "X-Codex-Turn-Metadata",
                r#"{"session_id":"turn-sid","agent_name":"main"}"#,
            )],
            "",
        ),
        info(
            "openai-subagent-false",
            &[("Session-Id", "codex-1"), ("X-Openai-Subagent", "FALSE")],
            "",
        ),
        info(
            "openai-subagent-review",
            &[("Session-Id", "codex-1"), ("X-Openai-Subagent", "review")],
            "",
        ),
        info(
            "codex-agent-root",
            &[(
                "X-Codex-Turn-Metadata",
                r#"{"session_id":"s","thread_id":"t","agent_name":"root"}"#,
            )],
            "",
        ),
        info(
            "claude-legacy-user-id-upper-hex",
            &[],
            r#"{"metadata":{"user_id":"user_1_account__session_ABC-123"}}"#,
        ),
        info(
            "claude-user-id-not-json-object",
            &[],
            r#"{"metadata":{"user_id":"{not json"}}"#,
        ),
        info(
            "claude-agent-main",
            &[
                ("X-Claude-Code-Session-Id", "root"),
                ("X-Claude-Code-Agent-Id", "main"),
                ("X-Claude-Code-Parent-Agent-Id", "lead"),
            ],
            "",
        ),
        info(
            "claude-parent-agent",
            &[
                ("X-Claude-Code-Session-Id", "root"),
                ("X-Claude-Code-Agent-Id", "worker"),
                ("X-Claude-Code-Parent-Agent-Id", "lead"),
            ],
            "",
        ),
        info(
            "conversation-string",
            &[],
            r#"{"conversation":"conv-as-text"}"#,
        ),
        info(
            "conversation-object-number-id",
            &[],
            r#"{"conversation":{"id":12}}"#,
        ),
        info("session-id-number", &[], r#"{"session_id":1.50}"#),
        info("session-id-exponent", &[], r#"{"session_id":1e3}"#),
        info("session-id-bool", &[], r#"{"session_id":true}"#),
        info("session-id-object", &[], r#"{"session_id":{"a": 1}}"#)
            .known_difference(COMPACT_CONTAINER),
        info(
            "session-id-object-over-lines",
            &[],
            "{\"session_id\":{\n\"a\": 1\n},\"prompt_cache_key\":\"p\"}",
        )
        .known_difference(COMPACT_CONTAINER),
        info(
            "session-id-null",
            &[],
            r#"{"session_id":null,"thread_id":"t"}"#,
        ),
        info(
            "session-id-escaped",
            &[],
            &format!(
                r#"{{"session_id":"caf{}-{}"}}"#,
                escaped('\u{e9}'),
                escaped('\u{1f680}')
            ),
        ),
        info(
            "session-id-c1-control",
            &[],
            &format!(r#"{{"session_id":"a{}b"}}"#, escaped('\u{85}')),
        ),
        info(
            "nested-request-with-contents",
            &[],
            r#"{"contents":[],"request":{"session_id":"nested"}}"#,
        ),
        info(
            "nested-request-not-object",
            &[],
            r#"{"request":"text","session_id":"top"}"#,
        ),
        info(
            "gemini-cached-content-nested",
            &[],
            r#"{"request":{"cached_content":"cache-1"}}"#,
        ),
        info(
            "agent-id-header-on-body-session",
            &[("X-Agent-Id", "helper")],
            r#"{"session_id":"body-session"}"#,
        ),
        info(
            "extra-body-session-and-parent",
            &[],
            r#"{"extra_body":{"session_id":"x-child","parent_id":"x-parent"}}"#,
        ),
        info(
            "metadata-task-and-parent",
            &[],
            r#"{"metadata":{"task_id":"m-task","parent_task_id":"m-parent"}}"#,
        ),
        info(
            "legacy-chat-id",
            &[],
            r#"{"chat_id":"chat-1","parentConversationId":"chat-0"}"#,
        ),
    ]
}

/// The hand-written cases for `session/derive`.
pub fn derives() -> Vec<Case> {
    let mut cases = conversation_growth();
    cases.extend(roots());
    cases.extend(not_upstreams_derives());
    cases
}

/// Upstream's TestDeriveIDStableAcrossConversationGrowth: each format's
/// first request and a later one.
fn conversation_growth() -> Vec<Case> {
    [
        (
            "openai",
            r#"{"messages":[{"role":"system","content":"system prompt"},{"role":"developer","content":"developer prompt"},{"role":"user","content":"complete first user prompt"}]}"#,
            r#"{"messages":[{"role":"system","content":"system prompt"},{"role":"developer","content":"developer prompt"},{"role":"user","content":"complete first user prompt"},{"role":"assistant","content":"answer"},{"role":"developer","content":"later instruction"},{"role":"user","content":"next"}]}"#,
        ),
        (
            "claude",
            r#"{"system":[{"type":"text","text":"system prompt"}],"messages":[{"role":"user","content":[{"type":"text","text":"complete first user prompt"}]}]}"#,
            r#"{"system":[{"type":"text","text":"system prompt"}],"messages":[{"role":"user","content":[{"type":"text","text":"complete first user prompt"}]},{"role":"assistant","content":"answer"},{"role":"user","content":"next"}]}"#,
        ),
        (
            "openai-response",
            r#"{"instructions":"system prompt","input":[{"type":"message","role":"developer","content":[{"type":"input_text","text":"developer prompt"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"complete first user prompt"}]}]}"#,
            r#"{"instructions":"system prompt","input":[{"type":"message","role":"developer","content":[{"type":"input_text","text":"developer prompt"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"complete first user prompt"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#,
        ),
        (
            "gemini",
            r#"{"systemInstruction":{"parts":[{"text":"system prompt"}]},"contents":[{"role":"user","parts":[{"text":"complete first user prompt"}]}]}"#,
            r#"{"systemInstruction":{"parts":[{"text":"system prompt"}]},"contents":[{"role":"user","parts":[{"text":"complete first user prompt"}]},{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":"next"}]}]}"#,
        ),
        (
            "interactions",
            r#"{"system_instruction":"system prompt","input":[{"type":"developer_instruction","text":"developer prompt"},{"type":"user_input","content":[{"type":"text","text":"complete first user prompt"}]}]}"#,
            r#"{"system_instruction":"system prompt","input":[{"type":"developer_instruction","text":"developer prompt"},{"type":"user_input","content":[{"type":"text","text":"complete first user prompt"}]},{"type":"model_output","content":[{"type":"text","text":"answer"}]},{"type":"user_input","content":[{"type":"text","text":"next"}]}]}"#,
        ),
    ]
    .into_iter()
    .flat_map(|(format, first, later)| {
        [
            derive(&format!("{format}-first"), format, "caller-a", first),
            derive(&format!("{format}-later"), format, "caller-a", later),
        ]
    })
    .collect()
}

/// Upstream's TestDeriveIDInstructionPrefixAndFullUser,
/// TestDeriveIDCallerIsolationAndGeminiCachedContent (with the caller
/// scopes passed as they are, where upstream hashes them first),
/// TestDeriveIDRequiresFirstUser and
/// TestDeriveIDAntigravityNestedRequestAndEmptyFirstUser (for Gemini:
/// Antigravity's format is out of scope).
fn roots() -> Vec<Case> {
    let prefix = "\u{754c}".repeat(50);
    let user = "u".repeat(120);
    let chat = |system: &str, last: &str| {
        format!(
            r#"{{"messages":[{{"role":"system","content":"{prefix}{system}"}},{{"role":"user","content":"{user}{last}"}}]}}"#
        )
    };
    let same = r#"{"messages":[{"role":"user","content":"same prompt"}]}"#;
    let nested = r#"{"project_id": "test-project", "request": {"systemInstruction": {"parts":[{"text":"system prompt"}]}, "contents": [{"role":"user","parts":[{"text":""}]}, {"role":"user","parts":[{"text":"actual user prompt"}]}]}}"#;
    let direct = r#"{"systemInstruction": {"parts":[{"text":"system prompt"}]}, "contents": [{"role":"user","parts":[{"text":"actual user prompt"}]}]}"#;
    vec![
        derive(
            "instruction-prefix-first",
            "openai",
            "caller-a",
            &chat("timestamp-a", "a"),
        ),
        derive(
            "instruction-prefix-same-root",
            "openai",
            "caller-a",
            &chat("timestamp-b", "a"),
        ),
        derive(
            "instruction-prefix-different-user",
            "openai",
            "caller-a",
            &chat("timestamp-b", "b"),
        ),
        derive("caller-a", "openai", "api-key-a", same),
        derive("caller-b", "openai", "api-key-b", same),
        derive(
            "gemini-cached-first",
            "gemini",
            "caller-a",
            r#"{"cachedContent":"cachedContents/abc","contents":[{"role":"user","parts":[{"text":"first"}]}]}"#,
        ),
        derive(
            "gemini-cached-grown",
            "gemini",
            "caller-a",
            r#"{"cachedContent":"cachedContents/abc","contents":[{"role":"user","parts":[{"text":"first"}]},{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":"next"}]}]}"#,
        ),
        derive(
            "gemini-cached-different",
            "gemini",
            "caller-a",
            r#"{"cachedContent":"cachedContents/abc","contents":[{"role":"user","parts":[{"text":"different"}]}]}"#,
        ),
        derive(
            "requires-first-user",
            "openai",
            "caller-a",
            r#"{"messages":[{"role":"system","content":"shared system"}]}"#,
        ),
        derive("gemini-nested-request", "gemini", "caller-a", nested),
        derive("gemini-direct-request", "gemini", "caller-a", direct),
        derive(
            "antigravity-nested-request",
            "antigravity",
            "caller-a",
            nested,
        )
        .known_difference("Antigravity's format isn't read as Gemini's (out of scope)"),
    ]
}

/// Not upstream's: formats, bodies and parts its tests don't reach.
fn not_upstreams_derives() -> Vec<Case> {
    let user = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
    vec![
        derive("format-spaced-and-cased", " Claude ", "", user),
        derive(
            "format-codex",
            "codex",
            "scope",
            r#"{"input":"plain text input"}"#,
        ),
        derive("format-unknown", "openai-chat", "scope", user),
        derive("format-empty", "", "scope", user),
        derive("caller-scope-trimmed", "openai", "  scope  ", user),
        derive("empty-body", "openai", "scope", ""),
        derive("array-body", "openai", "scope", "[1]"),
        derive("text-before-body", "openai", "scope", &format!("x{user}")),
        derive(
            "white-space-around-body",
            "openai",
            "scope",
            &format!(" \n{user}\t "),
        ),
        derive("text-after-body", "openai", "scope", &format!("{user} x")),
        derive(
            "number-out-of-range",
            "openai",
            "scope",
            r#"{"n":1e400,"messages":[{"role":"user","content":"hello"}]}"#,
        ),
        derive(
            "numbers-in-json-part",
            "openai",
            "scope",
            r#"{"messages":[{"role":"user","content":[{"type":"other","n":1.50,"m":1e21,"k":100000000000000000000,"z":0.000001}]}]}"#,
        ),
        derive(
            "cache-control-left-out",
            "claude",
            "scope",
            r#"{"system":"sys","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","Cache_Control ":{"type":"ephemeral"},"cache_control":{"type":"ephemeral"},"is_error":false}]}]}"#,
        ),
        derive(
            "html-characters-escaped",
            "openai",
            "scope",
            &format!(
                r#"{{"messages":[{{"role":"user","content":[{{"type":"x","v":"<a> & {}"}}]}}]}}"#,
                escaped('\u{2028}')
            ),
        ),
        derive(
            "media-parts",
            "openai",
            "scope",
            r#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/a.png"}},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}},{"inlineData":{"mimeType":"image/jpeg","data":"BBBB"}},{"file_data":{"file_uri":"gs://bucket/file"}},{"image_url":"data:image/png;base64,CCCC"}]}]}"#,
        ),
        derive(
            "media-part-without-data",
            "gemini",
            "scope",
            r#"{"contents":[{"role":"user","parts":[{"fileData":{"mimeType":"text/plain"}},{"text":"after"}]}]}"#,
        ),
        derive(
            "system-role-cased",
            "openai",
            "scope",
            r#"{"messages":[{"role":" SYSTEM ","content":"sys"},{"role":"User","content":"hi"}]}"#,
        ),
        derive(
            "first-user-empty",
            "openai",
            "scope",
            r#"{"messages":[{"role":"user","content":""},{"role":"user","content":[]},{"role":"user","content":"second"}]}"#,
        ),
        derive(
            "claude-system-not-read-for-openai",
            "openai",
            "scope",
            r#"{"system":"sys","messages":[{"role":"user","content":"hi"}]}"#,
        ),
        derive(
            "responses-input-items-with-system",
            "openai-response",
            "scope",
            r#"{"input":[{"role":"system","content":"sys"},{"role":"user","content":[{"type":"input_text","text":"hi"},{"type":"input_image","image_url":"https://example.invalid/i.png"}]}]}"#,
        ),
        derive(
            "interactions-steps",
            "interactions",
            "scope",
            r#"{"systemInstruction":{"text":"sys"},"input":[{"role":"user","steps":[{"type":"text","text":"in a step"}]}]}"#,
        ),
        derive(
            "interactions-string-input",
            "interactions",
            "scope",
            r#"{"input":"just text"}"#,
        ),
        derive(
            "interactions-message-without-role",
            "interactions",
            "scope",
            r#"{"input":[{"type":"model_output","text":"x"},{"type":"message","content":"hello"}]}"#,
        ),
        derive(
            "long-instruction-cut",
            "openai-response",
            "scope",
            &format!(
                r#"{{"instructions":"{}","input":"hi"}}"#,
                "\u{1f680}".repeat(60)
            ),
        ),
    ]
}
