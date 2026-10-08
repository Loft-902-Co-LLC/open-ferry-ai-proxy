// Ported from CLIProxyAPI sdk/cliproxy/session/identity.go
// (NormalizeExplicitID, ClaudeMetadataIdentities, ClaudeMetadataSessionID,
// ClaudeMetadataParentSessionID, CallerScope, Enrich, hasExplicitSession,
// headerValue, DeriveID, messagesRoot, responsesRoot, geminiRoot,
// interactionsRoot, flattenInteractionEntries, appendInstruction,
// canonicalParts, appendCanonicalParts, appendMediaPart, contentValue,
// normalizeJSONValue, hashRoot, firstField, stringField, normalizedString,
// truncateRunes, sourceFormatEqual) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Explicit session IDs, Claude Code's metadata, and the identity derived
//! for a request that names no session.
//!
//! [`derive_id`] hashes a conversation's root: its format, the caller's
//! scope, the first 50 characters of each leading instruction, and the
//! whole first user message, so the identity holds as the conversation
//! grows, and differs between callers and between conversations. It is
//! `ctx:v1:` and the SHA-256 of the root as Go's `json.Marshal` writes it.
//!
//! Deviations from upstream:
//! - [`derived_session_id`] is what upstream's `Enrich` leaves in the
//!   call's metadata for the selector, returned instead: nothing is
//!   written into a request. The canonical and LCP sessions an SDK caller
//!   may put in the metadata aren't read (there is no such caller).
//! - Antigravity's session header isn't an explicit session, and its
//!   format isn't read as Gemini's (out of scope).
//! - Not ported: `NormalizeToCanonicalUUID`, `CandidateSessionPrefixes`
//!   and the metadata helpers, which only the plugin host and Home read.

use std::borrow::Cow;
use std::fmt::Write as _;

use http::HeaderMap;
use open_ferry_translate::go;
use serde_json::{Map, Value};

use super::info::{Roots, candidate, first_in, header};
use super::payload::Payload;
use crate::auth::synthesizer::sha256_hex;

/// The longest explicit session ID taken, in bytes once trimmed.
pub const MAX_SESSION_ID_LENGTH: usize = 256;

/// The version a derived identity's root names.
const IDENTITY_VERSION: &str = "cpa-session-root-v1";

/// What a derived identity starts with.
const IDENTITY_PREFIX: &str = "ctx:v1:";

/// How many characters of each instruction a derived identity keeps.
const INSTRUCTION_RUNE_LIMIT: usize = 50;

/// The headers that name a session, or a parent, explicitly (upstream's
/// list in `hasExplicitSession`, without Antigravity's).
const EXPLICIT_HEADERS: &[&str] = &[
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
    "session-id",
    "session_id",
    "x-codex-parent-thread-id",
    "x-codex-turn-metadata",
    "x-openai-subagent",
    "x-session-id",
    "x-session-affinity",
    "x-parent-session-id",
    "x-parent-session-affinity",
    "x-parent-id",
    "x-slot-session-id",
    "x-parent-slot-session-id",
    "x-task-id",
    "x-parent-task-id",
    "x-conversation-id",
    "x-parent-conversation-id",
    "x-thread-id",
    "x-parent-thread-id",
    "thread-id",
    "x-client-request-id",
];

/// The body keys that name a session, or a parent, explicitly (upstream's
/// list in `hasExplicitSession`).
const EXPLICIT_PATHS: &[&str] = &[
    "session_id",
    "sessionId",
    "sessionID",
    "child_session_id",
    "childSessionId",
    "task_id",
    "taskId",
    "taskID",
    "action_id",
    "actionId",
    "cachedContent",
    "cached_content",
    "thread_id",
    "threadId",
    "conversation_id",
    "conversationId",
    "chat_id",
    "chatId",
    "prompt_cache_key",
    "promptCacheKey",
    "parent_session_id",
    "parentSessionId",
    "parent_thread_id",
    "parentThreadId",
    "parent_id",
    "parentId",
    "parentID",
    "parent_task_id",
    "parentTaskId",
    "parent_action_id",
    "parentActionId",
    "parent_session",
    "parentSession",
    "parent_subagent_id",
    "forkSource.sessionId",
    "previousSessionId",
    "forked_from_thread_id",
    "forked_from_id",
    "metadata.session_id",
    "metadata.sessionId",
    "metadata.task_id",
    "metadata.taskId",
    "metadata.thread_id",
    "metadata.conversation_id",
    "metadata.parent_id",
    "metadata.parent_task_id",
    "metadata.parent_agent_id",
    "extra_body.session_id",
    "extra_body.task_id",
    "extra_body.parent_id",
    "extra_body.parent_task_id",
];

/// `raw` trimmed, or empty when it holds a control character, or is empty
/// or longer than [`MAX_SESSION_ID_LENGTH`] bytes once trimmed (upstream's
/// `NormalizeExplicitID`).
pub fn normalize_explicit_id(raw: &str) -> String {
    if raw.chars().any(char::is_control) {
        return String::new();
    }
    let trimmed = raw.trim();
    if trimmed.len() > MAX_SESSION_ID_LENGTH {
        return String::new();
    }
    trimmed.to_owned()
}

/// The session, parent and agent in Claude Code's `metadata.user_id`.
#[derive(Debug, Default)]
pub(crate) struct ClaudeIdentities {
    pub(crate) session: String,
    pub(crate) parent: String,
    pub(crate) agent: String,
}

impl ClaudeIdentities {
    /// Read from the body's `metadata.user_id`, else its nested request's
    /// (upstream's `ClaudeMetadataIdentities`): a JSON object's
    /// `session_id`, parent and agent, or the legacy form's `_session_`
    /// suffix with the parent and agent in the body's metadata.
    pub(crate) fn of(roots: Roots<'_>) -> Self {
        let mut user_id = roots
            .root
            .get("metadata.user_id")
            .string()
            .trim()
            .to_owned();
        if user_id.is_empty()
            && let Some(nested) = roots.nested
        {
            user_id = nested.get("metadata.user_id").string().trim().to_owned();
        }
        if user_id.is_empty() {
            return Self::default();
        }
        if user_id.starts_with('{') {
            let parsed = Payload::parse(user_id.as_bytes());
            let parsed = parsed.root();
            return Self {
                session: candidate(parsed.get("session_id")),
                parent: first_in(
                    parsed,
                    &["parent_session_id", "parent_agent_id", "parent_id"],
                ),
                agent: first_in(parsed, &["agent_id", "subagent_id"]),
            };
        }
        let Some(session) = legacy_session(&user_id) else {
            return Self::default();
        };
        let metadata = roots.root.get("metadata");
        Self {
            session: normalize_explicit_id(session),
            parent: first_in(
                metadata,
                &["parent_agent_id", "parent_session_id", "parent_id"],
            ),
            agent: first_in(metadata, &["agent_id", "subagent_id"]),
        }
    }
}

/// The session ID of a legacy Claude Code `user_id`, the hex digits and
/// dashes ending it after `_session_` (upstream's
/// `_session_([a-f0-9-]+)$`). `_session_` holds characters the ID can't,
/// so the ID is the whole run of them at the end.
fn legacy_session(user_id: &str) -> Option<&str> {
    let run = user_id
        .bytes()
        .rev()
        .take_while(|b| matches!(b, b'a'..=b'f' | b'0'..=b'9' | b'-'))
        .count();
    let start = user_id.len() - run;
    let session = user_id.get(start..).filter(|session| !session.is_empty())?;
    user_id
        .get(..start)
        .is_some_and(|head| head.ends_with("_session_"))
        .then_some(session)
}

/// The session, parent and agent in Claude Code's `metadata.user_id`
/// (upstream's `ClaudeMetadataIdentities`).
pub fn claude_metadata_identities(payload: &Payload) -> (String, String, String) {
    let ClaudeIdentities {
        session,
        parent,
        agent,
    } = ClaudeIdentities::of(Roots::of(payload));
    (session, parent, agent)
}

/// An irreversible namespace for a downstream caller: the SHA-256 of
/// `value`, trimmed, under a fixed prefix, in hex; empty for an empty
/// `value` (upstream's `CallerScope`).
pub fn caller_scope(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    sha256_hex(format!("cli-proxy-api:caller-scope:v1\0{value}").as_bytes())
}

/// Whether the request names a session, or a parent, in its headers or
/// body (upstream's `hasExplicitSession`).
pub fn has_explicit_session(headers: &HeaderMap, payload: &Payload) -> bool {
    if EXPLICIT_HEADERS
        .iter()
        .any(|name| !header(headers, name).is_empty())
    {
        return true;
    }
    let roots = Roots::of(payload);
    if !roots.first_by_path(EXPLICIT_PATHS).is_empty() {
        return true;
    }
    if !ClaudeIdentities::of(roots).session.is_empty() {
        return true;
    }
    let mut user_id = roots
        .root
        .get("metadata.user_id")
        .string()
        .trim()
        .to_owned();
    if user_id.is_empty()
        && let Some(nested) = roots.nested
    {
        user_id = nested.get("metadata.user_id").string().trim().to_owned();
    }
    if !normalize_explicit_id(&user_id).is_empty() {
        return true;
    }
    let conversation = roots.conversation();
    !candidate(conversation.get("id")).is_empty()
        || (conversation.is_string() && !candidate(conversation).is_empty())
}

/// The identity derived for a request that names no session, or empty
/// (upstream's `Enrich`, for the selector's derived ID): none when the
/// request names a session or the connection has an execution session,
/// else [`derive_id`]'s.
pub fn derived_session_id(
    headers: &HeaderMap,
    payload: &Payload,
    execution_id: &str,
    format: &str,
    caller_scope: &str,
) -> String {
    if has_explicit_session(headers, payload) || !normalize_explicit_id(execution_id).is_empty() {
        return String::new();
    }
    derive_id(format, payload, caller_scope)
}

/// A piece of the first user message, or of an instruction (upstream's
/// `canonicalPart`).
#[derive(Debug)]
struct Part {
    kind: String,
    mime: String,
    value: String,
}

/// A stable identity from the leading instructions and the first complete
/// user message of a request in `format`, for the caller `caller_scope`,
/// or empty when the body isn't a JSON object or has no user message
/// (upstream's `DeriveID`).
pub fn derive_id(format: &str, payload: &Payload, caller_scope: &str) -> String {
    let Some(body) = payload.object() else {
        return String::new();
    };
    // Go reads every number as a float64, and refuses a body with one out
    // of its range.
    if !numbers_fit(body.values()) {
        return String::new();
    }
    let format_is = |name: &str| go::equal_fold(format.trim(), name);
    let mut resource = String::new();
    let (instructions, user) = if format_is("gemini") {
        let request = body
            .get("request")
            .and_then(Value::as_object)
            .unwrap_or(body);
        resource = string_field(request, &["cachedContent", "cached_content"]);
        gemini_root(body)
    } else if format_is("interactions") {
        interactions_root(body)
    } else if format_is("openai-response") || format_is("codex") {
        responses_root(body)
    } else if format_is("claude") {
        messages_root(body, true)
    } else {
        messages_root(body, false)
    };
    if user.is_empty() {
        return String::new();
    }
    hash_root(format, caller_scope.trim(), &instructions, &user, &resource)
}

/// Whether every number in `values` reads as a float64.
fn numbers_fit<'a>(mut values: impl Iterator<Item = &'a Value>) -> bool {
    values.all(|value| match value {
        Value::Number(number) => go::parse_float_checked(&number.to_string()).is_some(),
        Value::Array(items) => numbers_fit(items.iter()),
        Value::Object(object) => numbers_fit(object.values()),
        _ => true,
    })
}

/// The instructions before the first user message with content, and that
/// message's parts, of a Chat Completions or Claude Messages body; the
/// top-level `system` is read only for Claude (upstream's `messagesRoot`).
fn messages_root(body: &Map<String, Value>, top_level_system: bool) -> (Vec<String>, Vec<Part>) {
    let mut instructions = Vec::new();
    if top_level_system && let Some(system) = body.get("system") {
        append_instruction(&mut instructions, system);
    }
    let user = role_items(body.get("messages"), &mut instructions);
    (instructions, user)
}

/// The parts of the first user item with content in `items`, an array of
/// items with a role and content, after adding each system or developer
/// item before it to `instructions`.
fn role_items(items: Option<&Value>, instructions: &mut Vec<String>) -> Vec<Part> {
    let Some(Value::Array(items)) = items else {
        return Vec::new();
    };
    for item in items {
        let Some(item) = item.as_object() else {
            continue;
        };
        let content = item.get("content").unwrap_or(&Value::Null);
        match normalized_string(item.get("role")).as_str() {
            "system" | "developer" => append_instruction(instructions, content),
            "user" => {
                let parts = canonical_parts(content);
                if !parts.is_empty() {
                    return parts;
                }
            }
            _ => {}
        }
    }
    Vec::new()
}

/// The root of a Responses body: its `instructions`, and its `input` as a
/// string or as items (upstream's `responsesRoot`).
fn responses_root(body: &Map<String, Value>) -> (Vec<String>, Vec<Part>) {
    let mut instructions = Vec::new();
    if let Some(value) = body.get("instructions") {
        append_instruction(&mut instructions, value);
    }
    let user = match body.get("input") {
        None => Vec::new(),
        Some(input @ Value::String(_)) => canonical_parts(input),
        Some(input) => role_items(Some(input), &mut instructions),
    };
    (instructions, user)
}

/// The root of a Gemini body, or of the request it nests: its system
/// instruction and its first user content with parts (upstream's
/// `geminiRoot`).
fn gemini_root(body: &Map<String, Value>) -> (Vec<String>, Vec<Part>) {
    let body = body
        .get("request")
        .and_then(Value::as_object)
        .unwrap_or(body);
    let mut instructions = Vec::new();
    if let Some(value) = first_field(body, &["systemInstruction", "system_instruction"]) {
        append_instruction(&mut instructions, content_value(value));
    }
    if let Some(Value::Array(contents)) = body.get("contents") {
        for content in contents {
            let Some(object) = content.as_object() else {
                continue;
            };
            if normalized_string(object.get("role")) != "user" {
                continue;
            }
            let parts = canonical_parts(content_value(content));
            if !parts.is_empty() {
                return (instructions, parts);
            }
        }
    }
    (instructions, Vec::new())
}

/// The root of a Gemini Interactions body: its system instruction, the
/// system and developer steps before the first user step, and that step's
/// parts (upstream's `interactionsRoot`).
fn interactions_root(body: &Map<String, Value>) -> (Vec<String>, Vec<Part>) {
    let mut instructions = Vec::new();
    if let Some(value) = first_field(body, &["system_instruction", "systemInstruction"]) {
        append_instruction(&mut instructions, content_value(value));
    }
    let Some(input) = body.get("input") else {
        return (instructions, Vec::new());
    };
    if input.is_string() {
        return (instructions, canonical_parts(input));
    }
    let mut entries = Vec::new();
    flatten_interaction_entries(input, "", &mut entries);
    for entry in &entries {
        let entry: &Value = entry;
        if entry.is_string() {
            return (instructions, canonical_parts(entry));
        }
        let Some(step) = entry.as_object() else {
            continue;
        };
        let role = normalized_string(step.get("role"));
        let step_type = normalized_string(step.get("type"));
        if matches!(role.as_str(), "system" | "developer")
            || matches!(
                step_type.as_str(),
                "system_instruction" | "developer_instruction"
            )
        {
            append_instruction(&mut instructions, content_value(entry));
            continue;
        }
        if role == "user"
            || step_type == "user_input"
            || ((step_type == "message" || step_type.is_empty()) && role.is_empty())
        {
            return (instructions, canonical_parts(content_value(entry)));
        }
    }
    (instructions, Vec::new())
}

/// `value`'s steps in order: arrays and `steps` lists opened, and each
/// step without a role of its own given its enclosing one (upstream's
/// `flattenInteractionEntries`).
fn flatten_interaction_entries<'a>(
    value: &'a Value,
    inherited_role: &str,
    entries: &mut Vec<Cow<'a, Value>>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                flatten_interaction_entries(item, inherited_role, entries);
            }
        }
        Value::Object(object) => {
            let own_role = normalized_string(object.get("role"));
            let role = if own_role.is_empty() {
                inherited_role
            } else {
                &own_role
            };
            if let Some(Value::Array(steps)) = object.get("steps") {
                for step in steps {
                    flatten_interaction_entries(step, role, entries);
                }
                return;
            }
            if !role.is_empty() && own_role.is_empty() {
                let mut cloned = object.clone();
                cloned.insert("role".to_owned(), Value::String(role.to_owned()));
                entries.push(Cow::Owned(Value::Object(cloned)));
            } else {
                entries.push(Cow::Borrowed(value));
            }
        }
        other => entries.push(Cow::Borrowed(other)),
    }
}

/// Adds the text of `value` to `instructions`, its text parts joined by
/// line feeds and cut to [`INSTRUCTION_RUNE_LIMIT`] characters, when it has
/// any (upstream's `appendInstruction`).
fn append_instruction(instructions: &mut Vec<String>, value: &Value) {
    let mut text = String::new();
    for part in canonical_parts(value) {
        if part.kind != "text" || part.value.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&part.value);
    }
    if text.is_empty() {
        return;
    }
    instructions.push(text.chars().take(INSTRUCTION_RUNE_LIMIT).collect());
}

/// `value`'s parts (upstream's `canonicalParts`).
fn canonical_parts(value: &Value) -> Vec<Part> {
    let mut parts = Vec::new();
    append_canonical_parts(&mut parts, value);
    parts
}

/// Adds `value`'s parts: a string's text, an array's items, an object's
/// text, content, parts or media, else the object as JSON (upstream's
/// `appendCanonicalParts`).
fn append_canonical_parts(parts: &mut Vec<Part>, value: &Value) {
    match value {
        Value::Null => {}
        Value::String(text) => {
            if !text.is_empty() {
                parts.push(Part {
                    kind: "text".to_owned(),
                    mime: String::new(),
                    value: text.clone(),
                });
            }
        }
        Value::Array(items) => {
            for item in items {
                append_canonical_parts(parts, item);
            }
        }
        Value::Object(object) => {
            if let Some(text @ Value::String(_)) = object.get("text") {
                append_canonical_parts(parts, text);
            } else if let Some(nested) = object.get("content") {
                append_canonical_parts(parts, nested);
            } else if let Some(nested) = object.get("parts") {
                append_canonical_parts(parts, nested);
            } else if let Some(image) = object.get("image_url") {
                append_media_part(parts, "image", image, "");
            } else if let Some(inline) = first_field(object, &["inlineData", "inline_data"]) {
                append_media_part(parts, "inline_data", inline, "");
            } else if let Some(file) = first_field(object, &["fileData", "file_data"]) {
                append_media_part(parts, "file", file, "");
            } else if let Some(source) = object.get("source") {
                let kind = normalized_string(object.get("type"));
                let mime = normalized_string(object.get("media_type"));
                append_media_part(parts, &kind, source, &mime);
            } else {
                parts.push(json_part(value));
            }
        }
        other => parts.push(json_part(other)),
    }
}

/// `value` as a part of kind `json`, as Go's `json.Marshal` writes it
/// with every `cache_control` key left out (upstream's
/// `normalizeJSONValue`).
fn json_part(value: &Value) -> Part {
    let mut json = String::new();
    marshal(value, &mut json);
    Part {
        kind: "json".to_owned(),
        mime: String::new(),
        value: json,
    }
}

/// Adds a media part of `kind`: a string as it is, or an object's URL, URI
/// or data with its MIME type (upstream's `appendMediaPart`).
fn append_media_part(parts: &mut Vec<Part>, kind: &str, value: &Value, fallback_mime: &str) {
    let kind = match kind.trim() {
        "" => "media",
        kind => kind,
    };
    match value {
        Value::String(text) => {
            if !text.is_empty() {
                parts.push(Part {
                    kind: kind.to_owned(),
                    mime: fallback_mime.to_owned(),
                    value: text.clone(),
                });
            }
        }
        Value::Object(object) => {
            let mut mime = string_field(object, &["mimeType", "mime_type", "media_type"]);
            if mime.is_empty() {
                fallback_mime.clone_into(&mut mime);
            }
            let media = string_field(object, &["url", "uri", "fileUri", "file_uri", "data"]);
            if !media.is_empty() {
                parts.push(Part {
                    kind: kind.to_owned(),
                    mime,
                    value: media,
                });
            }
        }
        other => append_canonical_parts(parts, other),
    }
}

/// An object's `content`, else its `parts`, else its `text`, else the
/// value itself (upstream's `contentValue`).
fn content_value(value: &Value) -> &Value {
    let Some(object) = value.as_object() else {
        return value;
    };
    first_field(object, &["content", "parts", "text"]).unwrap_or(value)
}

/// Writes `value` as Go's `json.Marshal` writes what `json.Unmarshal`
/// read: keys sorted, strings with Go's HTML escapes, numbers as float64s,
/// and every key that is `cache_control` once trimmed, in any case, left
/// out (upstream's `normalizeJSONValue`).
fn marshal(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            out.push_str(&go::json_float(go::parse_float(&number.to_string())));
        }
        Value::String(text) => out.push_str(&go::json_string(text)),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                marshal(item, out);
            }
            out.push(']');
        }
        Value::Object(object) => {
            let mut entries: Vec<(&String, &Value)> = object
                .iter()
                .filter(|(key, _)| !go::equal_fold(key.trim(), "cache_control"))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&go::json_string(key));
                out.push(':');
                marshal(item, out);
            }
            out.push('}');
        }
    }
}

/// `ctx:v1:` and the SHA-256, in hex, of the root as Go's `json.Marshal`
/// writes upstream's `canonicalRoot` (upstream's `hashRoot`).
fn hash_root(
    format: &str,
    caller_scope: &str,
    instructions: &[String],
    user: &[Part],
    resource: &str,
) -> String {
    let mut root = String::new();
    let _ = write!(
        root,
        "{{\"version\":{},\"format\":{},\"caller_scope\":{}",
        go::json_string(IDENTITY_VERSION),
        go::json_string(format),
        go::json_string(caller_scope),
    );
    if !instructions.is_empty() {
        root.push_str(",\"instructions\":[");
        for (index, instruction) in instructions.iter().enumerate() {
            if index > 0 {
                root.push(',');
            }
            root.push_str(&go::json_string(instruction));
        }
        root.push(']');
    }
    if !user.is_empty() {
        root.push_str(",\"user\":[");
        for (index, part) in user.iter().enumerate() {
            if index > 0 {
                root.push(',');
            }
            let _ = write!(root, "{{\"kind\":{}", go::json_string(&part.kind));
            if !part.mime.is_empty() {
                let _ = write!(root, ",\"mime\":{}", go::json_string(&part.mime));
            }
            let _ = write!(root, ",\"value\":{}}}", go::json_string(&part.value));
        }
        root.push(']');
    }
    if !resource.is_empty() {
        let _ = write!(root, ",\"resource\":{}", go::json_string(resource));
    }
    root.push('}');
    format!("{IDENTITY_PREFIX}{}", sha256_hex(root.as_bytes()))
}

/// The value of the first of `keys` that `object` has, even `null`
/// (upstream's `firstField`).
fn first_field<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| object.get(*key))
}

/// The first of `keys` that `object` has, trimmed when it is a string,
/// else empty (upstream's `stringField`).
fn string_field(object: &Map<String, Value>, keys: &[&str]) -> String {
    first_field(object, keys)
        .and_then(Value::as_str)
        .map(|text| text.trim().to_owned())
        .unwrap_or_default()
}

/// `value` trimmed and in lower case when it is a string, else empty
/// (upstream's `normalizedString`).
fn normalized_string(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .map(|text| go::to_lower(text.trim()))
        .unwrap_or_default()
}
