// Ported from CLIProxyAPI sdk/cliproxy/auth/selector.go
// (extractConversationAlias, extractExplicitSessionIDs, extractSessionIDs,
// extractMessageHashIDs, computeSessionHash, truncateString,
// extractMessageContent, extractResponsesAPIContent and isSubagentSession)
// and isHierarchyParent in home_session_alias.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The session a call binds under: the one the client named, else the one
//! derived from the conversation's start, else a hash of its first
//! messages; with the session it may fall back to, such as its parent.
//!
//! These are local routing keys only (policy): the cache holds them
//! hashed, and nothing here is sent upstream, written into a request,
//! logged or saved.
//!
//! Deviations from upstream:
//! - The LCP matcher isn't ported, so a call that names no session goes
//!   straight to the derived identity and the message hash.
//! - Nothing is written into the call's metadata: upstream records the
//!   canonical session, the parent and the fork flag there for later
//!   stages, which aren't ported.
//! - The body is read as [`Payload`] reads it (see the session module).

use std::borrow::Cow;
use std::fmt;

use http::HeaderMap;
use serde_json::Value;

use crate::observe::usage::json::Node;
use crate::session::{
    Payload, Roots, bound_session_identity, candidate, derived_session_id, extract_session_info,
    normalize_explicit_id,
};

/// The most bytes of a message the hash reads (upstream's
/// `truncateString(s, 100)`).
const MAX_HASH_TEXT: usize = 100;

/// The keys a call's session binds under (upstream's primary and fallback
/// session IDs).
pub(crate) struct Session {
    /// The session, bounded (upstream's `BoundSessionIdentity`).
    pub(super) primary: String,
    /// The session it may fall back to, bounded, or empty.
    pub(super) fallback: String,
    /// Whether the client said the session is a fork of its parent.
    pub(super) is_fork: bool,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("fallback", &!self.fallback.is_empty())
            .field("is_fork", &self.is_fork)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// The session of a call with `headers` and `payload` (the original
    /// request), on a connection with the execution session
    /// `execution_id`, from a client in `format` whose caller scope is
    /// `caller_scope`; none when nothing names or identifies one (upstream's
    /// `extractSessionIDs` after `Enrich`).
    pub(crate) fn of(
        headers: &HeaderMap,
        payload: &Payload,
        execution_id: &str,
        format: &str,
        caller_scope: &str,
    ) -> Option<Self> {
        Self::read(headers, payload, execution_id, || {
            derived_session_id(headers, payload, execution_id, format, caller_scope)
        })
    }

    /// The session of a call with `headers` and `payload` and no metadata:
    /// the one the client named, else the message hash (upstream's
    /// `extractSessionIDs` for a selection without metadata, as Codex Alpha
    /// Search makes).
    pub(crate) fn named(headers: &HeaderMap, payload: &Payload) -> Option<Self> {
        Self::read(headers, payload, "", String::new)
    }

    /// The session as upstream's selector reads it from a call whose
    /// metadata holds only the derived session `derived`, which may be empty
    /// (`extractSessionIDs`): the one the client named, else `derived`, else
    /// the message hash.
    #[cfg(test)]
    pub(crate) fn with_derived(
        headers: &HeaderMap,
        payload: &Payload,
        execution_id: &str,
        derived: &str,
    ) -> Option<Self> {
        Self::read(headers, payload, execution_id, || derived.to_owned())
    }

    /// The session the client named, else the one `derived` gives, else the
    /// message hash, each bounded.
    fn read(
        headers: &HeaderMap,
        payload: &Payload,
        execution_id: &str,
        derived: impl FnOnce() -> String,
    ) -> Option<Self> {
        let (primary, fallback, is_fork) = explicit(headers, payload, execution_id)
            .or_else(|| {
                let derived = normalize_explicit_id(&derived());
                (!derived.is_empty()).then(|| (format!("derived:{derived}"), String::new(), false))
            })
            .or_else(|| {
                let (primary, fallback) = message_hash_ids(payload)?;
                Some((primary, fallback, false))
            })?;
        if primary.is_empty() {
            return None;
        }
        let fallback = if fallback.is_empty() {
            fallback
        } else {
            bound_session_identity(&fallback)
        };
        Some(Self {
            primary: bound_session_identity(&primary),
            fallback,
            is_fork,
        })
    }

    /// The session.
    #[cfg(test)]
    pub(crate) fn primary(&self) -> &str {
        &self.primary
    }

    /// The session it may fall back to.
    #[cfg(test)]
    pub(crate) fn fallback(&self) -> &str {
        &self.fallback
    }

    /// Whether the client said the session is a fork of its parent.
    #[cfg(test)]
    pub(crate) fn is_fork(&self) -> bool {
        self.is_fork
    }
}

/// The session the client named, its fallback, and whether it is a fork
/// (upstream's `extractExplicitSessionIDs`): the parent, else for a prompt
/// cache key the conversation.
pub(super) fn explicit(
    headers: &HeaderMap,
    payload: &Payload,
    execution_id: &str,
) -> Option<(String, String, bool)> {
    let info = extract_session_info(headers, payload, execution_id)?;
    if info.session_id.is_empty() {
        return None;
    }
    let mut fallback = info.parent_session_id;
    if fallback.is_empty() && info.session_id.starts_with("pck:") {
        fallback = conversation_alias(payload);
    }
    Some((info.session_id, fallback, info.is_fork))
}

/// The conversation the body names, as `conv:` and its ID (upstream's
/// `extractConversationAlias`).
fn conversation_alias(payload: &Payload) -> String {
    let conversation = Roots::of(payload).conversation();
    let mut id = candidate(conversation.get("id"));
    if id.is_empty() && conversation.is_string() {
        id = candidate(conversation);
    }
    if id.is_empty() {
        return String::new();
    }
    format!("conv:{id}")
}

/// The first system, user and assistant messages, each cut to 100 bytes.
#[derive(Default)]
struct Firsts {
    system: Vec<u8>,
    user: Vec<u8>,
    assistant: Vec<u8>,
}

/// `text` cut to its first 100 bytes, as Go slices a string, which may
/// split a character (upstream's `truncateString`).
fn truncate(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    bytes.get(..MAX_HASH_TEXT).unwrap_or(bytes).to_vec()
}

/// Sets `slot` to `text`, cut, if it is still empty.
fn first(slot: &mut Vec<u8>, text: &str) {
    if slot.is_empty() {
        *slot = truncate(text);
    }
}

/// The values gjson's `ForEach` visits that can hold a field: an array's
/// items or an object's values.
fn children(node: Node<'_>) -> Vec<Node<'_>> {
    match node.value() {
        Some(Value::Array(items)) => items.iter().map(Node::of).collect(),
        Some(Value::Object(object)) => object.values().map(Node::of).collect(),
        _ => Vec::new(),
    }
}

/// The text of a message's content: a string, or its `text` parts joined
/// by spaces (upstream's `extractMessageContent`).
fn message_content(content: Node<'_>) -> Cow<'_, str> {
    if content.is_string() {
        return content.string();
    }
    if content.is_array() {
        return Cow::Owned(joined_text(content, &["text"]));
    }
    Cow::Borrowed("")
}

/// The text of a Responses item's content parts, joined by spaces
/// (upstream's `extractResponsesAPIContent`).
fn responses_content(content: Node<'_>) -> String {
    if !content.is_array() {
        return String::new();
    }
    joined_text(content, &["input_text", "output_text", "text"])
}

/// The non-empty `text` of `parts` whose type is one of `types`, joined by
/// spaces.
fn joined_text(parts: Node<'_>, types: &[&str]) -> String {
    let texts: Vec<Cow<'_, str>> = children(parts)
        .into_iter()
        .filter(|part| types.contains(&part.get("type").string().as_ref()))
        .map(|part| part.get("text").string())
        .filter(|text| !text.is_empty())
        .collect();
    texts.join(" ")
}

/// The first non-empty `text` of `parts`.
fn first_text(parts: Node<'_>) -> Option<Cow<'_, str>> {
    children(parts)
        .into_iter()
        .map(|part| part.get("text").string())
        .find(|text| !text.is_empty())
}

/// The session a hash of the first messages gives, and the shorter one it
/// may fall back to: the system prompt and first user message, plus the
/// first assistant message when there is one (upstream's
/// `extractMessageHashIDs`).
pub(super) fn message_hash_ids(payload: &Payload) -> Option<(String, String)> {
    let body = payload.scan();
    let mut firsts = Firsts::default();
    // OpenAI and Claude messages.
    let messages = body.get("messages");
    if messages.is_array() {
        for message in children(messages) {
            let role = message.get("role").string();
            let content = message_content(message.get("content"));
            if content.is_empty() {
                continue;
            }
            match role.as_ref() {
                "system" => first(&mut firsts.system, &content),
                "user" => first(&mut firsts.user, &content),
                "assistant" => first(&mut firsts.assistant, &content),
                _ => {}
            }
            if !firsts.system.is_empty() && !firsts.user.is_empty() && !firsts.assistant.is_empty()
            {
                break;
            }
        }
    }
    // Claude's top-level system.
    if firsts.system.is_empty() {
        let system = body.get("system");
        if system.is_array() {
            if let Some(text) = first_text(system) {
                first(&mut firsts.system, &text);
            }
        } else if system.is_string() {
            firsts.system = truncate(&system.string());
        }
    }
    // Gemini.
    if firsts.system.is_empty() && firsts.user.is_empty() {
        let parts = body.get("systemInstruction.parts");
        if parts.is_array()
            && let Some(text) = first_text(parts)
        {
            first(&mut firsts.system, &text);
        }
        let contents = body.get("contents");
        if contents.is_array() {
            for message in children(contents) {
                let role = message.get("role").string();
                if let Some(text) = first_text(message.get("parts")) {
                    match role.as_ref() {
                        "user" => first(&mut firsts.user, &text),
                        "model" => first(&mut firsts.assistant, &text),
                        _ => {}
                    }
                }
                if !firsts.user.is_empty() && !firsts.assistant.is_empty() {
                    break;
                }
            }
        }
    }
    // OpenAI Responses.
    if firsts.system.is_empty() && firsts.user.is_empty() {
        let instructions = body.get("instructions").string();
        if !instructions.is_empty() {
            firsts.system = truncate(&instructions);
        }
        let input = body.get("input");
        if input.is_array() {
            for item in children(input) {
                let kind = item.get("type").string();
                if kind == "reasoning" || (!kind.is_empty() && kind != "message") {
                    continue;
                }
                let role = item.get("role").string();
                if kind.is_empty() && role.is_empty() {
                    continue;
                }
                let content = item.get("content");
                let text = if content.is_string() {
                    content.string()
                } else {
                    Cow::Owned(responses_content(content))
                };
                if text.is_empty() {
                    continue;
                }
                match role.as_ref() {
                    "developer" | "system" => first(&mut firsts.system, &text),
                    "user" => first(&mut firsts.user, &text),
                    "assistant" => first(&mut firsts.assistant, &text),
                    _ => {}
                }
                if !firsts.user.is_empty() && !firsts.assistant.is_empty() {
                    break;
                }
            }
        }
    }
    if firsts.user.is_empty() {
        return None;
    }
    let short = session_hash(&firsts.system, &firsts.user, &[]);
    if firsts.assistant.is_empty() {
        return Some((short, String::new()));
    }
    let full = session_hash(&firsts.system, &firsts.user, &firsts.assistant);
    Some((full, short))
}

/// `msg:` and the 64-bit FNV-1a hash of the non-empty messages, each
/// tagged, in hex (upstream's `computeSessionHash`).
fn session_hash(system: &[u8], user: &[u8], assistant: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut write = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for (tag, text) in [
        (&b"sys:"[..], system),
        (&b"usr:"[..], user),
        (&b"ast:"[..], assistant),
    ] {
        if !text.is_empty() {
            write(tag);
            write(text);
            write(b"\n");
        }
    }
    format!("msg:{hash:016x}")
}

/// Whether `primary` is a subagent's session under `fallback` (upstream's
/// `isSubagentSession`).
pub(super) fn is_subagent_session(primary: &str, fallback: &str) -> bool {
    if primary.contains(":agent:") {
        return true;
    }
    if fallback.is_empty() || primary.is_empty() || primary == fallback {
        return false;
    }
    is_hierarchy_parent(primary, fallback)
}

/// Whether `fallback` is `primary`'s parent in a session hierarchy: a
/// subagent's session, two sessions of one client (the same prefix before
/// a colon), or two without prefixes (upstream's `isHierarchyParent`).
pub(super) fn is_hierarchy_parent(primary: &str, fallback: &str) -> bool {
    if fallback.is_empty() || primary.is_empty() || primary == fallback {
        return false;
    }
    if primary.contains(":agent:") {
        return true;
    }
    match (primary.split_once(':'), fallback.split_once(':')) {
        (Some((left, _)), Some((right, _))) => !left.is_empty() && left == right,
        (None, None) => true,
        _ => false,
    }
}
