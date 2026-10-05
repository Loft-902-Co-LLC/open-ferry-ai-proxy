// Ported from CLIProxyAPI internal/cache/xai_reasoning_replay_cache.go
// (StoreXAIReasoningReplayItems, GetXAIReasoningReplayItems,
// DeleteXAIReasoningReplayItem and their helpers) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The xAI reasoning replay cache: the reasoning, message and tool-call
//! items of a client's last completed Grok response, kept in memory so
//! [`crate::xai`]'s replay can put them back in a request whose client
//! dropped them.
//!
//! It sits beside [`super::replay_cache`], the Codex one, and shares its
//! bounds and its key hash, but not its shape: upstream's xAI cache holds one
//! batch per entry and every write replaces it, where Codex's appends turns
//! and keeps markers for them. An entry is keyed by model and session key,
//! never by the credential, so a request that fails over to another one still
//! finds it. Items are normalized to the smallest shape xAI accepts as input
//! before they are stored, and anything else is dropped:
//! - a `reasoning` item must carry Grok's own `encrypted_content`, which
//!   [`inspect_grok_encrypted_content`] checks (a Codex blob is refused);
//! - an assistant `message` keeps its `output_text` and `refusal` parts and
//!   loses its ID and status;
//! - a `function_call` or `custom_tool_call` keeps what a call needs.
//!
//! A batch is only kept if it holds a reasoning item or a tool call: an
//! assistant message alone has nothing to replay. The bounds are upstream's:
//! at most 10240 entries, the 128 least recently used evicted when it is
//! full, and an entry expires an hour after it was last written or read.
//!
//! Deviations from upstream:
//! - Home mode's shared KV store isn't ported, so entries live in this
//!   process only and there are no `...Required` variants, backend errors or
//!   `StoreBackendError` status.
//! - Expired entries are purged as entries are written, at most once every
//!   ten minutes, rather than by a background task every ten minutes; a read
//!   of an expired entry drops it, as upstream's does.
//! - `CacheXAIReasoningReplayItem(s)` (and its `BestEffort` form) and
//!   `GetXAIReasoningReplayItem` are only used by tests, so [`Store`] is the
//!   one write and `get_item` is only built for tests.
//!   `ClearXAIReasoningReplayCache` isn't ported: tests that share the
//!   process's cache use sessions of their own instead.
//! - Entries are keyed by a SHA-256 hash of the model and session, each with
//!   its length, so a long session key takes no more room than a short one,
//!   and a model or session holding a NUL can't stand for another pair.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Instant;

use open_ferry_translate::signature::inspect_grok_encrypted_content;
use serde_json::{Value, json};

use super::replay_cache::{EVICT_BATCH, Key, MAX_ENTRIES, PURGE_INTERVAL, expired, hash_parts};
use crate::json::{eq_fold, get, str_at};

/// How a write ended (`XAIReasoningReplayStoreStatus`, less its backend
/// error).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Store {
    /// The model or session was blank: nothing was written.
    InvalidArgs,
    /// A replayable batch was written.
    Stored,
    /// The items held no reasoning item or tool call to replay (for example
    /// when reasoning is off): nothing was written.
    NoReplayableState,
}

struct Entry {
    items: Vec<Value>,
    used: Instant,
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
    last_purge: Option<Instant>,
}

/// A bounded cache of replay batches. The executor uses
/// [`XaiReplayCache::global`].
#[derive(Default)]
pub(crate) struct XaiReplayCache {
    state: Mutex<State>,
}

static GLOBAL: LazyLock<XaiReplayCache> = LazyLock::new(XaiReplayCache::default);

impl XaiReplayCache {
    /// The process's cache.
    pub(crate) fn global() -> &'static Self {
        &GLOBAL
    }

    /// Replaces the batch for `model` and `session` with `items`, normalized
    /// (`StoreXAIReasoningReplayItems`).
    pub(crate) fn store(&self, model: &str, session: &str, items: &[&Value]) -> Store {
        self.store_at(model, session, items, Instant::now())
    }

    fn store_at(&self, model: &str, session: &str, items: &[&Value], now: Instant) -> Store {
        let Some(key) = cache_key(model, session) else {
            return Store::InvalidArgs;
        };
        let Some(items) = normalize(items) else {
            return Store::NoReplayableState;
        };
        let mut state = self.lock();
        state.purge_expired(now);
        state.insert(key, Entry { items, used: now });
        Store::Stored
    }

    /// The batch for `model` and `session`, which keeps the entry alive
    /// (`GetXAIReasoningReplayItems`).
    pub(crate) fn get(&self, model: &str, session: &str) -> Option<Vec<Value>> {
        self.get_at(model, session, Instant::now())
    }

    fn get_at(&self, model: &str, session: &str, now: Instant) -> Option<Vec<Value>> {
        let key = cache_key(model, session)?;
        let mut state = self.lock();
        let entry = state.entries.get_mut(&key)?;
        if expired(entry.used, now) {
            state.entries.remove(&key);
            return None;
        }
        entry.used = now;
        Some(entry.items.clone())
    }

    /// The first item of the batch for `model` and `session`
    /// (`GetXAIReasoningReplayItem`).
    #[cfg(test)]
    pub(crate) fn get_item(&self, model: &str, session: &str) -> Option<Value> {
        self.get(model, session)?.into_iter().next()
    }

    /// Drops the batch for `model` and `session`
    /// (`DeleteXAIReasoningReplayItem`).
    pub(crate) fn delete(&self, model: &str, session: &str) {
        if let Some(key) = cache_key(model, session) {
            self.lock().entries.remove(&key);
        }
    }

    /// The number of entries.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().entries.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// Stores an entry and evicts the oldest if the cache is over its bound.
    fn insert(&mut self, key: Key, entry: Entry) {
        self.entries.insert(key, entry);
        if self.entries.len() > MAX_ENTRIES {
            self.evict_oldest(EVICT_BATCH);
        }
    }

    /// `evictOldestXAIReasoningReplayEntriesLocked`.
    fn evict_oldest(&mut self, count: usize) {
        let mut candidates: Vec<(Instant, Key)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.used, *key))
            .collect();
        candidates.sort_unstable();
        for (_, key) in candidates.into_iter().take(count) {
            self.entries.remove(&key);
        }
    }

    /// Drops expired entries if they haven't been purged for a while
    /// (`purgeExpiredXAIReasoningReplayCache`, on upstream's cleanup
    /// interval).
    fn purge_expired(&mut self, now: Instant) {
        if self
            .last_purge
            .is_some_and(|last| now.saturating_duration_since(last) < PURGE_INTERVAL)
        {
            return;
        }
        self.last_purge = Some(now);
        self.entries.retain(|_, entry| !expired(entry.used, now));
    }
}

/// A hash of `xaiReasoningReplayCacheKey`: none if the model or session is
/// blank. The session is the boundary, not the xAI credential.
fn cache_key(model: &str, session: &str) -> Option<Key> {
    let (model, session) = (model.trim(), session.trim());
    if model.is_empty() || session.is_empty() {
        return None;
    }
    Some(hash_parts(&["xai-reasoning-replay", model, session]))
}

/// `normalizeXAIReasoningReplayItems`: the items that can be kept, in their
/// smallest shape, if at least one is reasoning or a tool call.
fn normalize(items: &[&Value]) -> Option<Vec<Value>> {
    let mut normalized = Vec::with_capacity(items.len());
    let mut replayable = false;
    for item in items {
        let Some(item) = normalize_item(item) else {
            continue;
        };
        replayable |= matches!(
            str_at(&item, "type").as_str(),
            "reasoning" | "function_call" | "custom_tool_call"
        );
        normalized.push(item);
    }
    replayable.then_some(normalized)
}

/// `normalizeXAIReasoningReplayItem`.
fn normalize_item(item: &Value) -> Option<Value> {
    match str_at(item, "type").trim() {
        "reasoning" => normalize_reasoning(item),
        "message" => normalize_message(item),
        "function_call" => normalize_function_call(item),
        "custom_tool_call" => normalize_custom_tool_call(item),
        _ => None,
    }
}

/// A reasoning item whose `encrypted_content` is Grok's, with no surrounding
/// space, with an empty summary and no content.
fn normalize_reasoning(item: &Value) -> Option<Value> {
    let Some(Value::String(encrypted)) = item.get("encrypted_content") else {
        return None;
    };
    if encrypted.trim() != encrypted || inspect_grok_encrypted_content(encrypted).is_err() {
        return None;
    }
    Some(json!({
        "type": "reasoning",
        "summary": [],
        "content": null,
        "encrypted_content": encrypted,
    }))
}

/// An assistant message with at least one `output_text` or `refusal` part;
/// parts of any other kind, or that lack their string, are skipped.
fn normalize_message(item: &Value) -> Option<Value> {
    if !eq_fold(str_at(item, "role").trim(), "assistant") {
        return None;
    }
    let Some(Value::Array(content)) = item.get("content") else {
        return None;
    };
    let parts: Vec<Value> = content.iter().filter_map(normalize_part).collect();
    if parts.is_empty() {
        return None;
    }
    Some(json!({"type": "message", "role": "assistant", "content": parts}))
}

/// An `output_text` part with its text, or a `refusal` part with its refusal
/// (not a `text`, as the Responses API names it).
fn normalize_part(part: &Value) -> Option<Value> {
    match str_at(part, "type").trim() {
        "output_text" => {
            let Some(Value::String(text)) = part.get("text") else {
                return None;
            };
            Some(json!({"type": "output_text", "text": text}))
        }
        "refusal" => {
            let Some(Value::String(refusal)) = part.get("refusal") else {
                return None;
            };
            Some(json!({"type": "refusal", "refusal": refusal}))
        }
        _ => None,
    }
}

/// A function call with a call ID, a name and string arguments.
fn normalize_function_call(item: &Value) -> Option<Value> {
    let (call_id, name) = (trimmed(item, "call_id"), trimmed(item, "name"));
    let Some(Value::String(arguments)) = item.get("arguments") else {
        return None;
    };
    if call_id.is_empty() || name.is_empty() {
        return None;
    }
    Some(json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    }))
}

/// A custom tool call with a call ID, a name and an input (a string or any
/// JSON), `completed` unless it says otherwise.
fn normalize_custom_tool_call(item: &Value) -> Option<Value> {
    let (call_id, name) = (trimmed(item, "call_id"), trimmed(item, "name"));
    let input = get(item, "input")?;
    if call_id.is_empty() || name.is_empty() {
        return None;
    }
    let status = trimmed(item, "status");
    Some(json!({
        "type": "custom_tool_call",
        "status": if status.is_empty() { "completed" } else { status.as_str() },
        "call_id": call_id,
        "name": name,
        "input": input,
    }))
}

/// gjson's `String()` at `field`, trimmed.
fn trimmed(item: &Value, field: &str) -> String {
    str_at(item, field).trim().to_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    //! Ported from upstream's `internal/cache/xai_reasoning_replay_cache_test.go`:
    //! `TestXAIReasoningReplayCacheRejectsCodexEncryptedContent`,
    //! `...StoresGrokEncryptedContent`, `...StoresAssistantMessageWithReasoning`,
    //! `...RejectsAssistantMessageWithoutReasoning`,
    //! `...StoresToolCallWithoutReasoning` and
    //! `...StoresRefusalMessagePart`. Each test has its own cache rather than
    //! clearing the process's.
    //!
    //! Dropped: `TestXAIReasoningReplayRequiredHomeExpireFailureReturnsItems`,
    //! which tests Home mode's KV store, which isn't ported.

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD_NO_PAD;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::codex::replay_cache::TTL;

    /// `testValidGrokEncryptedContentForSeed`: 256 bytes that pass for
    /// Grok's `encrypted_content`, which differ with the seed.
    pub(crate) fn grok_content(seed: u8) -> String {
        let mut buffer = Vec::with_capacity(256);
        for index in 0_u32..8 {
            let [low, middle, high, _] = index.to_le_bytes();
            buffer.extend_from_slice(&Sha256::digest([seed, low, middle, high]));
        }
        STANDARD_NO_PAD.encode(buffer)
    }

    /// `validGrokEncryptedContentForReplayCacheTest`.
    fn cache_test_content() -> String {
        let mut buffer = Vec::with_capacity(256);
        for index in 0_u32..8 {
            let [low, middle, high, _] = index.to_le_bytes();
            buffer.extend_from_slice(&Sha256::digest([low, middle, high, 99]));
        }
        STANDARD_NO_PAD.encode(buffer)
    }

    // TestXAIReasoningReplayCacheRejectsCodexEncryptedContent.
    #[test]
    fn rejects_codex_encrypted_content() {
        let cache = XaiReplayCache::default();
        let item = json!({"type": "reasoning", "summary": [], "content": null, "encrypted_content": "gAAAAABinvalid-gpt-shape"});
        assert_eq!(
            cache.store("grok-4.3", "claude:xai-cache-test", &[&item]),
            Store::NoReplayableState
        );
        assert_eq!(cache.get_item("grok-4.3", "claude:xai-cache-test"), None);
    }

    // TestXAIReasoningReplayCacheStoresGrokEncryptedContent.
    #[test]
    fn stores_grok_encrypted_content() {
        let cache = XaiReplayCache::default();
        let encrypted = cache_test_content();
        let item = json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "visible"}], "content": null, "encrypted_content": encrypted});
        assert_eq!(
            cache.store("grok-4.3", "claude:xai-cache-test", &[&item]),
            Store::Stored
        );
        let item = cache
            .get_item("grok-4.3", "claude:xai-cache-test")
            .expect("the item is cached");
        assert_eq!(item["encrypted_content"], encrypted);
        assert_eq!(item["summary"], json!([]));
        assert_eq!(
            item.to_string(),
            format!(
                r#"{{"type":"reasoning","summary":[],"content":null,"encrypted_content":"{encrypted}"}}"#
            )
        );
    }

    // TestXAIReasoningReplayCacheStoresAssistantMessageWithReasoning.
    #[test]
    fn stores_assistant_message_with_reasoning() {
        let cache = XaiReplayCache::default();
        let encrypted = cache_test_content();
        let reasoning = json!({"id": "rs_1", "type": "reasoning", "summary": [{"type": "summary_text", "text": "visible"}], "encrypted_content": encrypted});
        let message = json!({"id": "msg_1", "type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "answer", "annotations": [], "logprobs": []}]});
        assert_eq!(
            cache.store("grok-4.5", "prompt-cache:session", &[&reasoning, &message]),
            Store::Stored
        );

        let items = cache
            .get("grok-4.5", "prompt-cache:session")
            .expect("the items are cached");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["encrypted_content"], encrypted);
        assert_eq!(items[1]["content"][0]["text"], "answer");
        assert!(get(&items[1], "id").is_none() && get(&items[1], "status").is_none());
        assert_eq!(
            items[1].to_string(),
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}"#
        );
    }

    // TestXAIReasoningReplayCacheRejectsAssistantMessageWithoutReasoning.
    #[test]
    fn rejects_assistant_message_without_reasoning() {
        let cache = XaiReplayCache::default();
        let message = json!({"id": "msg_1", "type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "answer"}]});
        assert_eq!(
            cache.store("grok-4.5", "prompt-cache:message-only", &[&message]),
            Store::NoReplayableState
        );
        assert_eq!(cache.get("grok-4.5", "prompt-cache:message-only"), None);
    }

    // TestXAIReasoningReplayCacheStoresToolCallWithoutReasoning.
    #[test]
    fn stores_tool_call_without_reasoning() {
        let cases = [
            (
                "function call",
                "prompt-cache:function-call-only",
                json!({"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"weather\"}"}),
                "function_call",
                "arguments",
                json!("{\"q\":\"weather\"}"),
            ),
            (
                "custom tool call",
                "prompt-cache:custom-tool-call-only",
                json!({"type": "custom_tool_call", "call_id": "call_2", "name": "shell", "input": "pwd"}),
                "custom_tool_call",
                "input",
                json!("pwd"),
            ),
        ];
        let cache = XaiReplayCache::default();
        for (name, session, item, want_type, payload_path, want_payload) in cases {
            assert_eq!(
                cache.store("grok-4.3", session, &[&item]),
                Store::Stored,
                "{name}"
            );
            let items = cache.get("grok-4.3", session).expect("the item is cached");
            assert_eq!(items.len(), 1, "{name}");
            assert_eq!(items[0]["type"], want_type, "{name}");
            assert_eq!(items[0][payload_path], want_payload, "{name}");
        }
    }

    // TestXAIReasoningReplayCacheStoresRefusalMessagePart.
    #[test]
    fn stores_refusal_message_part() {
        let cache = XaiReplayCache::default();
        let reasoning =
            json!({"type": "reasoning", "summary": [], "encrypted_content": cache_test_content()});
        let message = json!({"type": "message", "role": "assistant", "content": [{"type": "refusal", "refusal": "I cannot help with that"}]});
        assert_eq!(
            cache.store("grok-4.5", "prompt-cache:refusal", &[&reasoning, &message]),
            Store::Stored
        );
        let items = cache
            .get("grok-4.5", "prompt-cache:refusal")
            .expect("the items are cached");
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["content"][0]["type"], "refusal");
        assert_eq!(items[1]["content"][0]["refusal"], "I cannot help with that");
        assert!(get(&items[1], "content.0.text").is_none());
    }

    // Not upstream's: the status of a write by what it was given.
    #[test]
    fn store_reports_why_nothing_was_written() {
        let cache = XaiReplayCache::default();
        let reasoning = json!({"type": "reasoning", "encrypted_content": grok_content(1)});
        assert_eq!(cache.store("", "s", &[&reasoning]), Store::InvalidArgs);
        assert_eq!(cache.store("m", "  ", &[&reasoning]), Store::InvalidArgs);
        assert_eq!(cache.store("m", "s", &[]), Store::NoReplayableState);
        assert_eq!(cache.len(), 0);
        // The model and session are trimmed, and a write replaces the batch.
        assert_eq!(cache.store(" m ", " s ", &[&reasoning]), Store::Stored);
        let call = json!({"type": "function_call", "call_id": "c", "name": "n", "arguments": "{}"});
        assert_eq!(cache.store("m", "s", &[&call]), Store::Stored);
        let items = cache.get("m", "s").expect("the batch is cached");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(cache.len(), 1);
        // A batch with nothing to replay leaves what was there, for the
        // caller to delete.
        let message = json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "x"}]});
        assert_eq!(cache.store("m", "s", &[&message]), Store::NoReplayableState);
        assert!(cache.get("m", "s").is_some());
        cache.delete("m", " s");
        assert_eq!(cache.get("m", "s"), None);
        cache.delete("m", "");
    }

    // Not upstream's: the shapes items are normalized to, and what is
    // dropped.
    #[test]
    fn normalizes_items_to_their_input_shape() {
        let reasoning = json!({"id": "rs", "type": " reasoning ", "status": "completed", "encrypted_content": grok_content(2), "content": [1]});
        let message = json!({"type": "message", "role": " Assistant ", "id": "m", "content": [
            {"type": "output_text", "text": "kept", "annotations": [1]},
            {"type": "output_text", "text": 3},
            {"type": "refusal", "text": "wrong field"},
            {"type": "refusal", "refusal": "no"},
            {"type": "input_text", "text": "other"},
            "bare",
        ]});
        let call = json!({"type": "function_call", "id": "fc", "status": "completed", "call_id": " call_1 ", "name": " lookup ", "arguments": "{}"});
        let custom = json!({"type": "custom_tool_call", "call_id": "c", "name": "apply_patch", "input": {"patch": 1}});
        let custom_status = json!({"type": "custom_tool_call", "status": " in_progress ", "call_id": "c", "name": "n", "input": null});
        let normalized: Vec<String> =
            normalize(&[&reasoning, &message, &call, &custom, &custom_status])
                .expect("replayable")
                .iter()
                .map(Value::to_string)
                .collect();
        assert_eq!(
            normalized,
            [
                format!(
                    r#"{{"type":"reasoning","summary":[],"content":null,"encrypted_content":"{}"}}"#,
                    grok_content(2)
                ),
                r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"kept"},{"type":"refusal","refusal":"no"}]}"#.to_owned(),
                r#"{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}"#.to_owned(),
                r#"{"type":"custom_tool_call","status":"completed","call_id":"c","name":"apply_patch","input":{"patch":1}}"#.to_owned(),
                r#"{"type":"custom_tool_call","status":"in_progress","call_id":"c","name":"n","input":null}"#.to_owned(),
            ]
        );

        let dropped = [
            json!({"type": "reasoning", "encrypted_content": format!(" {}", grok_content(1))}),
            json!({"type": "reasoning", "encrypted_content": 7}),
            json!({"type": "reasoning"}),
            json!({"type": "message", "role": "user", "content": [{"type": "output_text", "text": "x"}]}),
            json!({"type": "message", "role": "assistant", "content": "text"}),
            json!({"type": "message", "role": "assistant", "content": []}),
            json!({"type": "message", "role": "assistant", "content": [{"type": "input_text", "text": "x"}]}),
            json!({"type": "function_call", "call_id": "c", "name": "n", "arguments": {}}),
            json!({"type": "function_call", "call_id": "c", "name": " ", "arguments": "{}"}),
            json!({"type": "function_call", "call_id": " ", "name": "n", "arguments": "{}"}),
            json!({"type": "custom_tool_call", "call_id": "c", "name": "n"}),
            json!({"type": "web_search_call", "id": "ws"}),
            json!("not an object"),
        ];
        for item in &dropped {
            assert!(normalize_item(item).is_none(), "{item}");
        }
        // A batch of only an assistant message is dropped whole.
        assert!(normalize(&[&message]).is_none());
        assert!(normalize(&dropped.iter().collect::<Vec<_>>()).is_none());
    }

    // Not upstream's: a long session key is hashed, and doesn't share its
    // entry with a session it starts with.
    #[test]
    fn long_session_keys_are_hashed() {
        let cache = XaiReplayCache::default();
        let session = "s".repeat(1 << 20);
        let item = json!({"type": "reasoning", "encrypted_content": grok_content(4)});
        assert_eq!(cache.store("m", &session, &[&item]), Store::Stored);
        assert!(cache.get_item("m", &session).is_some());
        assert_eq!(cache.get_item("m", &session[1..]), None);
        assert_eq!(cache.len(), 1);
    }

    // Not upstream's: a NUL in the model or session can't make two pairs
    // share an entry, and the Codex cache's entries are a different space.
    #[test]
    fn keys_keep_model_and_session_apart() {
        assert_ne!(cache_key("m\0a", "s"), cache_key("m", "a\0s"));
        assert_ne!(
            cache_key("m", "s"),
            Some(hash_parts(&["codex-reasoning-replay", "m", "s"]))
        );
        let cache = XaiReplayCache::default();
        let item = json!({"type": "reasoning", "encrypted_content": grok_content(5)});
        assert_eq!(cache.store("m\0a", "s", &[&item]), Store::Stored);
        assert_eq!(cache.get_item("m", "a\0s"), None);
        assert_eq!(cache.get_item("other", "s"), None);
    }

    // Not upstream's (upstream's xAI cache tests don't reach the bounds):
    // the cache evicts its oldest batch of entries when it is over
    // MAX_ENTRIES.
    #[test]
    fn batch_evicts_when_full() {
        let cache = XaiReplayCache::default();
        let item = json!({"type": "reasoning", "encrypted_content": grok_content(9)});
        for index in 0..=MAX_ENTRIES {
            assert_eq!(
                cache.store("grok-4.3", &format!("session-{index}"), &[&item]),
                Store::Stored,
                "insert {index} failed"
            );
        }
        assert_eq!(cache.len(), MAX_ENTRIES + 1 - EVICT_BATCH);
        assert_eq!(cache.get_item("grok-4.3", "session-0"), None);
        assert!(
            cache
                .get_item("grok-4.3", &format!("session-{MAX_ENTRIES}"))
                .is_some()
        );
    }

    // Not upstream's: entries expire an hour after they were last used,
    // reads keep them alive, and writes purge expired entries.
    #[test]
    fn expires_unused_entries() {
        let cache = XaiReplayCache::default();
        let start = Instant::now();
        let item = json!({"type": "reasoning", "encrypted_content": grok_content(3)});
        assert_eq!(cache.store_at("m", "kept", &[&item], start), Store::Stored);
        assert_eq!(cache.store_at("m", "idle", &[&item], start), Store::Stored);

        // A read within the TTL slides the entry's clock.
        let later = start + TTL;
        assert!(cache.get_at("m", "kept", later).is_some());
        let expired_at = later + std::time::Duration::from_secs(1);
        assert!(cache.get_at("m", "kept", expired_at).is_some());
        assert_eq!(cache.len(), 2);

        // A write after the purge interval purges the idle entry.
        assert_eq!(
            cache.store_at("m", "new", &[&item], expired_at),
            Store::Stored
        );
        assert_eq!(cache.len(), 2);
        assert!(cache.get_at("m", "idle", expired_at).is_none());

        // Reading an expired entry drops it.
        let much_later = expired_at + TTL + std::time::Duration::from_secs(1);
        assert!(cache.get_at("m", "kept", much_later).is_none());
        assert_eq!(cache.len(), 1);
    }
}
