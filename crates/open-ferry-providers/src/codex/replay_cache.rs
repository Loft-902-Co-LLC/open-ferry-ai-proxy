// Ported from CLIProxyAPI internal/cache/codex_reasoning_replay_cache.go
// (AppendCodexReasoningReplayItemsBestEffort,
// CacheCodexReasoningReplayItemsBestEffort, GetCodexReasoningReplayItems,
// GetCodexReasoningReplayItem, DeleteCodexReasoningReplayItem and their
// helpers) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The reasoning replay cache: the reasoning items and tool calls of a
//! client's recent Codex turns, kept in memory so [`super::replay`] can put
//! them back in a request whose client dropped them.
//!
//! An entry is keyed by model and session key. It holds the items of one or
//! more turns, each led by a marker item of type [`TURN_TYPE`] that says
//! where the turn belongs in a later request; items are normalized to the
//! smallest shape Codex accepts as input before they are stored, and
//! anything else is dropped. The bounds are upstream's: an entry keeps at
//! most 256 turns and 16 MiB of items, dropping its oldest turns first, and
//! the cache keeps at most 10240 entries, evicting the 128 least recently
//! used when it is full. An entry expires an hour after it was last written
//! or read.
//!
//! Deviations from upstream:
//! - Home mode's shared KV store isn't ported, so entries live in this
//!   process only and there are no `...Required` variants or their errors.
//! - Expired entries are purged as entries are written, at most once every
//!   ten minutes, rather than by a background task every ten minutes; a read
//!   of an expired entry drops it, as upstream's does.
//! - `CacheCodexReasoningReplayItem(s)` (which replaces an entry) and
//!   `GetCodexReasoningReplayItem` are only used by tests, so they are only
//!   built for tests. `ClearCodexReasoningReplayCache` isn't ported: tests
//!   that share the process's cache use sessions of their own instead.
//! - Entries are keyed by a SHA-256 hash of the model and session, so a long
//!   session key takes no more room than a short one.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use open_ferry_translate::signature::inspect_gpt_reasoning_signature;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::json::{get, str_at, str_of};

/// The type of the marker item that starts each turn of an entry.
pub(crate) const TURN_TYPE: &str = "cpa_codex_replay_turn";

/// How long an entry lives after it was last written or read.
const TTL: Duration = Duration::from_secs(60 * 60);

/// The most entries the cache keeps.
const MAX_ENTRIES: usize = 10240;

/// The most turns an entry keeps.
const MAX_TURNS_PER_ENTRY: usize = 256;

/// The most bytes of items an entry keeps.
const MAX_BYTES_PER_ENTRY: usize = 16 << 20;

/// How many entries are evicted at once when the cache is full, so a busy
/// cache doesn't rescan its entries on every write.
const EVICT_BATCH: usize = 128;

/// How often expired entries are purged (upstream's `CacheCleanupInterval`).
const PURGE_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// A stored item: its compact JSON, as upstream keeps bytes, and the ID of
/// the turn it starts if it is a marker.
#[derive(Clone)]
struct Item {
    json: String,
    turn_id: Option<String>,
}

struct Entry {
    items: Vec<Item>,
    used: Instant,
}

/// A hash of the model and session an entry is for.
type Key = [u8; 32];

#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
    last_purge: Option<Instant>,
}

/// A bounded cache of replay items. The executor uses [`ReplayCache::global`].
#[derive(Default)]
pub(crate) struct ReplayCache {
    state: Mutex<State>,
}

static GLOBAL: LazyLock<ReplayCache> = LazyLock::new(ReplayCache::default);

impl ReplayCache {
    /// The process's cache.
    pub(crate) fn global() -> &'static Self {
        &GLOBAL
    }

    /// Adds one turn's items to the entry for `model` and `session`, unless
    /// a turn with the same marker ID is already there
    /// (`AppendCodexReasoningReplayItemsBestEffort`). Whether any item was
    /// valid to keep.
    pub(crate) fn append(&self, model: &str, session: &str, items: &[&Value]) -> bool {
        self.append_at(model, session, items, Instant::now())
    }

    fn append_at(&self, model: &str, session: &str, items: &[&Value], now: Instant) -> bool {
        let Some(key) = cache_key(model, session) else {
            return false;
        };
        let Some(turn) = normalize(items) else {
            return false;
        };
        let mut state = self.lock();
        state.purge_expired(now);
        let existing = match state.entries.remove(&key) {
            Some(entry) if !expired(entry.used, now) => entry.items,
            _ => Vec::new(),
        };
        let items = append_turn(existing, turn);
        state.insert(key, Entry { items, used: now });
        true
    }

    /// Replaces the entry for `model` and `session` with `items`
    /// (`CacheCodexReasoningReplayItemsBestEffort`).
    #[cfg(test)]
    pub(crate) fn store(&self, model: &str, session: &str, items: &[&Value]) -> bool {
        let now = Instant::now();
        let Some(key) = cache_key(model, session) else {
            return false;
        };
        let Some(items) = normalize(items) else {
            return false;
        };
        let mut state = self.lock();
        state.purge_expired(now);
        state.insert(key, Entry { items, used: now });
        true
    }

    /// The items for `model` and `session`, which keeps the entry alive
    /// (`GetCodexReasoningReplayItems`).
    pub(crate) fn get(&self, model: &str, session: &str) -> Option<Vec<Value>> {
        self.get_at(model, session, Instant::now())
    }

    fn get_at(&self, model: &str, session: &str, now: Instant) -> Option<Vec<Value>> {
        let key = cache_key(model, session)?;
        let items = {
            let mut state = self.lock();
            let entry = state.entries.get_mut(&key)?;
            if expired(entry.used, now) {
                state.entries.remove(&key);
                return None;
            }
            entry.used = now;
            entry.items.clone()
        };
        Some(
            items
                .iter()
                .filter_map(|item| serde_json::from_str(&item.json).ok())
                .collect(),
        )
    }

    /// The first item for `model` and `session` that isn't a marker
    /// (`GetCodexReasoningReplayItem`).
    #[cfg(test)]
    pub(crate) fn get_item(&self, model: &str, session: &str) -> Option<Value> {
        self.get(model, session)?
            .into_iter()
            .find(|item| str_at(item, "type").trim() != TURN_TYPE)
    }

    /// Drops the entry for `model` and `session`
    /// (`DeleteCodexReasoningReplayItem`).
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

    /// `evictOldestCodexReasoningReplayEntries`.
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
    /// (`purgeExpiredCodexReasoningReplayCache`, on upstream's cleanup
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

fn expired(used: Instant, now: Instant) -> bool {
    now.saturating_duration_since(used) > TTL
}

/// A hash of `codexReasoningReplayCacheKey`: none if the model or session is
/// blank. The session is the boundary, not the Codex credential, so a
/// request that fails over to another credential still finds its turns.
fn cache_key(model: &str, session: &str) -> Option<Key> {
    let (model, session) = (model.trim(), session.trim());
    if model.is_empty() || session.is_empty() {
        return None;
    }
    let key = format!("codex-reasoning-replay\0{model}\0{session}");
    Some(Sha256::digest(key.as_bytes()).into())
}

/// `appendCodexReasoningReplayTurn`: existing items that don't start with a
/// marker are dropped, and a turn whose marker ID is already there isn't
/// added again.
fn append_turn(mut existing: Vec<Item>, turn: Vec<Item>) -> Vec<Item> {
    if existing.first().is_some_and(|item| item.turn_id.is_none()) {
        existing.clear();
    }
    let turn_id = turn.first().and_then(|item| item.turn_id.as_deref());
    if let Some(turn_id) = turn_id
        && existing
            .iter()
            .any(|item| item.turn_id.as_deref() == Some(turn_id))
    {
        return trim(existing);
    }
    existing.extend(turn);
    trim(existing)
}

/// `trimCodexReasoningReplayItems`: drops the oldest turns until the items
/// are within the turn and byte bounds. The first item always starts a
/// turn. Nothing is left if a single turn is over the bounds.
fn trim(mut items: Vec<Item>) -> Vec<Item> {
    loop {
        let mut turns = 1;
        let mut second_turn = None;
        let mut bytes = 0;
        for (index, item) in items.iter().enumerate() {
            bytes += item.json.len();
            if index > 0 && item.turn_id.is_some() {
                turns += 1;
                second_turn.get_or_insert(index);
            }
        }
        if turns <= MAX_TURNS_PER_ENTRY && bytes <= MAX_BYTES_PER_ENTRY {
            return items;
        }
        let Some(second_turn) = second_turn else {
            return Vec::new();
        };
        items.drain(..second_turn);
    }
}

/// `normalizeCodexReasoningReplayItems`: the items that can be kept, in
/// their smallest shape and within the bounds, or none.
fn normalize(items: &[&Value]) -> Option<Vec<Item>> {
    let items: Vec<Item> = items
        .iter()
        .filter_map(|item| normalize_item(item))
        .map(|value| Item {
            turn_id: (str_at(&value, "type") == TURN_TYPE).then(|| str_at(&value, "id")),
            json: value.to_string(),
        })
        .collect();
    let items = trim(items);
    (!items.is_empty()).then_some(items)
}

/// `normalizeCodexReasoningReplayItem`.
fn normalize_item(item: &Value) -> Option<Value> {
    match str_at(item, "type").trim() {
        TURN_TYPE => normalize_turn(item),
        "reasoning" => normalize_reasoning(item),
        "function_call" => normalize_function_call(item),
        "custom_tool_call" => normalize_custom_tool_call(item),
        _ => None,
    }
}

/// A marker with an ID, and its fingerprints and call IDs if it has them.
fn normalize_turn(item: &Value) -> Option<Value> {
    let id = trimmed(item, "id");
    if id.is_empty() {
        return None;
    }
    let mut marker = Map::new();
    marker.insert("type".into(), TURN_TYPE.into());
    marker.insert("id".into(), id.into());
    for field in ["assistant_fingerprint", "request_fingerprint"] {
        let fingerprint = trimmed(item, field);
        if !fingerprint.is_empty() {
            marker.insert(field.into(), fingerprint.into());
        }
    }
    if let Some(Value::Array(ids)) = item.get("call_ids") {
        let ids: Vec<Value> = ids
            .iter()
            .map(|id| str_of(Some(id)).trim().to_owned())
            .filter(|id| !id.is_empty())
            .map(Value::from)
            .collect();
        if !ids.is_empty() {
            marker.insert("call_ids".into(), ids.into());
        }
    }
    Some(marker.into())
}

/// A reasoning item whose `encrypted_content` is a GPT reasoning signature
/// with no surrounding space, with an empty summary and no content.
fn normalize_reasoning(item: &Value) -> Option<Value> {
    let Some(Value::String(encrypted)) = item.get("encrypted_content") else {
        return None;
    };
    if encrypted.trim() != encrypted || inspect_gpt_reasoning_signature(encrypted).is_err() {
        return None;
    }
    let mut reasoning = Map::new();
    reasoning.insert("type".into(), "reasoning".into());
    reasoning.insert("summary".into(), Value::Array(Vec::new()));
    reasoning.insert("content".into(), Value::Null);
    reasoning.insert("encrypted_content".into(), encrypted.clone().into());
    Some(reasoning.into())
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
    let mut call = Map::new();
    call.insert("type".into(), "function_call".into());
    call.insert("call_id".into(), call_id.into());
    call.insert("name".into(), name.into());
    call.insert("arguments".into(), arguments.clone().into());
    Some(call.into())
}

/// A custom tool call with a call ID, a name and an input, `completed`
/// unless it says otherwise.
fn normalize_custom_tool_call(item: &Value) -> Option<Value> {
    let (call_id, name) = (trimmed(item, "call_id"), trimmed(item, "name"));
    let input = get(item, "input")?;
    if call_id.is_empty() || name.is_empty() {
        return None;
    }
    let status = trimmed(item, "status");
    let mut call = Map::new();
    call.insert("type".into(), "custom_tool_call".into());
    call.insert(
        "status".into(),
        if status.is_empty() {
            "completed".into()
        } else {
            status.into()
        },
    );
    call.insert("call_id".into(), call_id.into());
    call.insert("name".into(), name.into());
    call.insert("input".into(), input.clone());
    Some(call.into())
}

/// gjson's `String()` at `field`, trimmed.
fn trimmed(item: &Value, field: &str) -> String {
    str_at(item, field).trim().to_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    //! Ported from upstream's `internal/cache/codex_reasoning_replay_cache_test.go`:
    //! `TestCodexReasoningReplayCacheRejectsInvalidItems`,
    //! `TestCodexReasoningReplayAppendBoundsTurnsPerEntry`,
    //! `TestCodexReasoningReplayCacheScopesByModelAndSession` and
    //! `TestCodexReasoningReplayCacheBatchEvictsWhenFull`. Each test has its
    //! own cache rather than clearing the process's.
    //!
    //! Dropped: the tests of Home mode's KV store, which isn't ported
    //! (`RequiredHomeReadAndSlidingExpire`, `RequiredHomeFailures`,
    //! `HomeRejectsEmptyScopeWithoutKV` and the other Home tests), and the
    //! concurrent append test, which only runs against the KV store.

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;

    use super::*;

    /// `validCodexReasoningReplayEncryptedContentForTest`.
    pub(crate) fn valid_encrypted_content(seed: u8) -> String {
        let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
        payload[0] = 0x80;
        for (index, byte) in payload.iter_mut().enumerate().skip(9) {
            *byte = seed.wrapping_add(index as u8);
        }
        URL_SAFE_NO_PAD.encode(payload)
    }

    fn reasoning(seed: u8) -> Value {
        json!({
            "type": "reasoning",
            "summary": [],
            "content": null,
            "encrypted_content": valid_encrypted_content(seed),
        })
    }

    #[test]
    fn rejects_invalid_items() {
        let cache = ReplayCache::default();
        let bad = json!({"type": "reasoning", "encrypted_content": "bad", "summary": []});
        assert!(!cache.store("gpt-5.4", "session", &[&bad]));
        assert_eq!(cache.get_item("gpt-5.4", "session"), None);
    }

    #[test]
    fn append_bounds_turns_per_entry() {
        let mut items = Vec::new();
        for turn in 0..=MAX_TURNS_PER_ENTRY {
            let marker = json!({"type": TURN_TYPE, "id": format!("turn-{turn}")});
            items.push(normalize(&[&marker]).unwrap().remove(0));
            items.extend(normalize(&[&reasoning((50 + turn) as u8)]).unwrap());
        }

        let trimmed = trim(items);
        assert_eq!(trimmed.len(), MAX_TURNS_PER_ENTRY * 2);
        assert_eq!(trimmed[0].turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn scopes_by_model_and_session() {
        let cache = ReplayCache::default();
        let encrypted = valid_encrypted_content(7);
        let item = reasoning(7);
        assert!(cache.store("gpt-5.4", "session-a", &[&item]));

        assert_eq!(cache.get_item("gpt-5.5", "session-a"), None);
        assert_eq!(cache.get_item("gpt-5.4", "session-b"), None);
        let item = cache.get_item("gpt-5.4", "session-a").unwrap();
        assert_eq!(
            item.to_string(),
            format!(
                r#"{{"type":"reasoning","summary":[],"content":null,"encrypted_content":"{encrypted}"}}"#
            )
        );
    }

    #[test]
    fn batch_evicts_when_full() {
        let cache = ReplayCache::default();
        let item = reasoning(9);
        for index in 0..=MAX_ENTRIES {
            assert!(
                cache.store("gpt-5.4", &format!("session-{index}"), &[&item]),
                "insert {index} failed"
            );
        }
        assert!(cache.len() < MAX_ENTRIES);
        assert_eq!(cache.len(), MAX_ENTRIES + 1 - EVICT_BATCH);
    }

    // Not upstream's: a long session key is hashed, and doesn't share its
    // entry with a session it starts with.
    #[test]
    fn long_session_keys_are_hashed() {
        let cache = ReplayCache::default();
        let session = "s".repeat(1 << 20);
        let item = reasoning(4);
        assert!(cache.store("m", &session, &[&item]));
        assert!(cache.get_item("m", &session).is_some());
        assert_eq!(cache.get_item("m", &session[1..]), None);
        assert_eq!(cache.len(), 1);
    }

    // Not upstream's: the shapes items are normalized to, and what is
    // dropped.
    #[test]
    fn normalizes_items_to_their_input_shape() {
        let marker = json!({
            "type": TURN_TYPE,
            "id": " turn ",
            "assistant_fingerprint": "",
            "request_fingerprint": " fp ",
            "call_ids": [" a ", "", 3],
            "extra": true,
        });
        let call = json!({
            "type": "function_call", "id": "fc_1", "status": "completed",
            "call_id": " call_1 ", "name": "lookup", "arguments": "{}",
        });
        let custom = json!({
            "type": "custom_tool_call", "call_id": "c", "name": "apply_patch",
            "input": {"patch": 1},
        });
        let custom_status = json!({
            "type": "custom_tool_call", "status": " in_progress ", "call_id": "c",
            "name": "n", "input": null,
        });
        let dropped = [
            json!({"type": "message", "role": "assistant", "content": "hi"}),
            json!({"type": TURN_TYPE, "id": " "}),
            json!({"type": "function_call", "call_id": "c", "name": "n", "arguments": {}}),
            json!({"type": "function_call", "call_id": "c", "name": " ", "arguments": "{}"}),
            json!({"type": "custom_tool_call", "call_id": "c", "name": "n"}),
            json!({"type": "reasoning", "encrypted_content": format!(" {}", valid_encrypted_content(1))}),
        ];
        let mut items = vec![&marker, &call, &custom, &custom_status];
        items.extend(&dropped);
        let normalized: Vec<String> = normalize(&items)
            .unwrap()
            .into_iter()
            .map(|item| item.json)
            .collect();
        assert_eq!(
            normalized,
            [
                r#"{"type":"cpa_codex_replay_turn","id":"turn","request_fingerprint":"fp","call_ids":["a","3"]}"#,
                r#"{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}"#,
                r#"{"type":"custom_tool_call","status":"completed","call_id":"c","name":"apply_patch","input":{"patch":1}}"#,
                r#"{"type":"custom_tool_call","status":"in_progress","call_id":"c","name":"n","input":null}"#,
            ]
        );
        assert!(normalize(&dropped.iter().collect::<Vec<_>>()).is_none());
    }

    // Not upstream's: appending turns, skipping a turn already there, and
    // dropping items that don't start with a marker.
    #[test]
    fn appends_turns_once() {
        let cache = ReplayCache::default();
        let first = json!({"type": TURN_TYPE, "id": "one"});
        let second = json!({"type": TURN_TYPE, "id": "two"});
        let (a, b) = (reasoning(1), reasoning(2));
        assert!(cache.store("m", "s", &[&a]));
        assert!(cache.append("m", "s", &[&first, &a]));
        assert!(cache.append("m", "s", &[&second, &b]));
        assert!(cache.append("m", "s", &[&first, &b]));
        let items = cache.get("m", "s").unwrap();
        let ids: Vec<String> = items.iter().map(|item| str_at(item, "id")).collect();
        assert_eq!(ids, ["one", "", "two", ""]);
        assert_eq!(items[1], a);
        assert_eq!(items[3], b);

        assert!(!cache.append("m", " ", &[&first, &a]));
        assert!(!cache.append("m", "s", &[&json!({"type": "message"})]));
        cache.delete("m", "s");
        assert_eq!(cache.get("m", "s"), None);
    }

    // Not upstream's: an entry over the byte bound keeps its newest turns,
    // and a single turn over it keeps nothing.
    #[test]
    fn bounds_bytes_per_entry() {
        let big = |id: &str| {
            let marker = json!({"type": TURN_TYPE, "id": id});
            let call = json!({
                "type": "function_call", "call_id": id, "name": "n",
                "arguments": "x".repeat(MAX_BYTES_PER_ENTRY / 3),
            });
            (marker, call)
        };
        let cache = ReplayCache::default();
        for id in ["one", "two", "three"] {
            let (marker, call) = big(id);
            assert!(cache.append("m", "s", &[&marker, &call]));
        }
        let items = cache.get("m", "s").unwrap();
        let ids: Vec<String> = items.iter().map(|item| str_at(item, "id")).collect();
        assert_eq!(ids, ["two", "", "three", ""]);

        let (marker, mut call) = big("huge");
        call["arguments"] = "x".repeat(MAX_BYTES_PER_ENTRY).into();
        assert!(!cache.store("m", "huge", &[&marker, &call]));
    }

    // Not upstream's: entries expire an hour after they were last used,
    // reads keep them alive, and writes purge expired entries.
    #[test]
    fn expires_unused_entries() {
        let cache = ReplayCache::default();
        let start = Instant::now();
        let marker = json!({"type": TURN_TYPE, "id": "one"});
        let item = reasoning(3);
        assert!(cache.append_at("m", "kept", &[&marker, &item], start));
        assert!(cache.append_at("m", "idle", &[&marker, &item], start));

        let later = start + TTL;
        assert!(cache.get_at("m", "kept", later).is_some());
        let expired_at = later + Duration::from_secs(1);
        assert!(cache.get_at("m", "kept", expired_at).is_some());
        assert_eq!(cache.len(), 2);

        // A write after the purge interval purges the idle entry.
        let next = json!({"type": TURN_TYPE, "id": "two"});
        assert!(cache.append_at("m", "new", &[&next, &item], expired_at));
        assert_eq!(cache.len(), 2);
        assert!(cache.get_at("m", "idle", expired_at).is_none());

        // An expired entry starts over when appended to.
        let much_later = expired_at + TTL + Duration::from_secs(1);
        assert!(cache.append_at("m", "kept", &[&next, &item], much_later));
        let items = cache.get_at("m", "kept", much_later).unwrap();
        assert_eq!(str_at(&items[0], "id"), "two");
        assert_eq!(items.len(), 2);
    }
}
