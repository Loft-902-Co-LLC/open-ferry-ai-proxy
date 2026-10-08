// Ported from CLIProxyAPI internal/cache/antigravity_reasoning_replay_cache.go
// (CacheAntigravityReasoningReplayItems, GetAntigravityReasoningReplayItems,
// antigravityReasoningReplayCacheKey,
// normalizeAntigravityReasoningReplayItems, normalizeAntigravityReasoningReplayItem,
// normalizeAntigravityThoughtSignatureReplayItem,
// normalizeAntigravityFunctionCallPartReplayItem,
// evictOldestAntigravityReasoningReplayEntries,
// purgeExpiredAntigravityReasoningReplayCache), with the cleanup interval from
// internal/cache/signature_cache.go (CacheCleanupInterval) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The in-process reasoning replay cache: items a response wants back on the
//! next turn, by model and session.
//!
//! The Gemini Responses translator keeps the signatures of text parts here,
//! so they don't show in the client's reasoning timeline. Entries last an
//! hour from their last use; at most 10240 are kept, and past that the 128
//! oldest go. One entry holds at most 4096 items and 16 MiB; a larger chain
//! isn't cached at all, since a partial one would break Gemini's signature
//! order. Only `thought_signature` and `function_call_part` items are kept,
//! each cut down to the fields replay reads.
//!
//! Deviations from upstream:
//! - Only the in-process store is ported. Upstream can keep entries in its
//!   Home key-value store instead, and has snapshots, revisions and branches
//!   so a request can replace or delete an entry only if no other request
//!   changed it. The Gemini Responses translator uses none of that.
//! - Upstream purges expired entries every 10 minutes from a background
//!   task. Here a purge runs on a call that comes 10 minutes or more after
//!   the last one. A read checks the age of what it finds either way.
//! - An entry's size is that of its items written by serde_json. Upstream
//!   measures sjson's text, whose escaping differs slightly.
//! - Entries are kept as JSON values, so an item's `args` is kept as a value
//!   rather than as the text it was written as.
//! - A read that finds nothing leaves nothing behind. Upstream marks the miss
//!   for an hour so a later conditional write can tell; with no conditional
//!   writes the mark has no use, and it would let a client fill the cache
//!   with session IDs of its choosing.
//! - Entries are keyed by a SHA-256 hash of the model and session, each with
//!   its length, so a long session ID takes no more room than a short one,
//!   and a model or session holding a NUL can't stand for another pair.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::json::{int_of, str_of};

/// `AntigravityReasoningReplayCacheTTL`: how long an entry lasts after its
/// last use.
pub(super) const TTL: Duration = Duration::from_secs(60 * 60);
/// `AntigravityReasoningReplayCacheMaxEntries`.
pub(super) const MAX_ENTRIES: usize = 10240;
/// `AntigravityReasoningReplayCacheEvictBatchSize`: how many of the oldest
/// entries go once there are too many.
pub(super) const EVICT_BATCH: usize = 128;
/// `minAntigravityThoughtSignatureReplayLen`.
const MIN_SIGNATURE_LEN: usize = 16;
/// `AntigravityReasoningReplayCacheMaxItemsPerEntry`.
pub(super) const MAX_ITEMS_PER_ENTRY: usize = 4096;
/// `AntigravityReasoningReplayCacheMaxBytesPerEntry`.
pub(super) const MAX_BYTES_PER_ENTRY: usize = 16 << 20;
/// `CacheCleanupInterval`.
const PURGE_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// The bypass sentinel, which is never worth replaying.
const BYPASS: &str = "skip_thought_signature_validator";

struct Entry {
    items: Vec<Value>,
    /// When the entry was written or last read.
    timestamp: Instant,
}

/// A hash of the model and session an entry is for.
type Key = [u8; 32];

/// One replay cache. The translator uses the process-wide one.
#[derive(Default)]
pub(super) struct ReplayCache {
    entries: HashMap<Key, Entry>,
    last_purge: Option<Instant>,
}

impl ReplayCache {
    /// `CacheAntigravityReasoningReplayItems`: stores `items`, cut down, in
    /// place of what the model and session had. Returns whether anything was
    /// stored.
    pub(super) fn cache(
        &mut self,
        model: &str,
        session: &str,
        items: &[Value],
        now: Instant,
    ) -> bool {
        let Some(key) = cache_key(model, session) else {
            return false;
        };
        let Some(items) = normalize_items(items) else {
            return false;
        };
        self.purge_if_due(now);
        self.entries.insert(
            key,
            Entry {
                items,
                timestamp: now,
            },
        );
        if self.entries.len() > MAX_ENTRIES {
            self.evict_oldest(EVICT_BATCH);
        }
        true
    }

    /// `GetAntigravityReasoningReplayItems`: the model and session's items,
    /// if there are any and they haven't expired. A read keeps them another
    /// hour.
    pub(super) fn get(&mut self, model: &str, session: &str, now: Instant) -> Option<Vec<Value>> {
        let key = cache_key(model, session)?;
        self.purge_if_due(now);
        let entry = self.entries.get_mut(&key)?;
        if now.saturating_duration_since(entry.timestamp) > TTL {
            self.entries.remove(&key);
            return None;
        }
        entry.timestamp = now;
        Some(entry.items.clone())
    }

    /// `evictOldestAntigravityReasoningReplayEntries`.
    fn evict_oldest(&mut self, count: usize) {
        let mut candidates: Vec<(Instant, Key)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.timestamp, *key))
            .collect();
        candidates.sort_by_key(|(timestamp, _)| *timestamp);
        for (_, key) in candidates.into_iter().take(count) {
            self.entries.remove(&key);
        }
    }

    /// `purgeExpiredAntigravityReasoningReplayCache`, at most once per
    /// [`PURGE_INTERVAL`].
    fn purge_if_due(&mut self, now: Instant) {
        match self.last_purge {
            None => self.last_purge = Some(now),
            Some(last) if now.saturating_duration_since(last) >= PURGE_INTERVAL => {
                self.last_purge = Some(now);
                self.entries
                    .retain(|_, entry| now.saturating_duration_since(entry.timestamp) <= TTL);
            }
            Some(_) => {}
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

static CACHE: LazyLock<Mutex<ReplayCache>> = LazyLock::new(Mutex::default);

/// [`ReplayCache::cache`] on the process-wide cache.
pub(super) fn cache_items(model: &str, session: &str, items: &[Value]) -> bool {
    CACHE.lock().unwrap_or_else(PoisonError::into_inner).cache(
        model,
        session,
        items,
        Instant::now(),
    )
}

/// [`ReplayCache::get`] on the process-wide cache.
pub(super) fn get_items(model: &str, session: &str) -> Option<Vec<Value>> {
    CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(model, session, Instant::now())
}

/// A hash of `antigravityReasoningReplayCacheKey`, or `None` if the model or
/// session is blank.
fn cache_key(model: &str, session: &str) -> Option<Key> {
    let (model, session) = (model.trim(), session.trim());
    if model.is_empty() || session.is_empty() {
        return None;
    }
    Some(hash_parts(&[
        "antigravity-reasoning-replay",
        model,
        session,
    ]))
}

/// A SHA-256 hash of `parts`, each led by its length, so that no two lists
/// of parts hash alike whatever they hold.
fn hash_parts(parts: &[&str]) -> Key {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().into()
}

/// `normalizeAntigravityReasoningReplayItems`: the items worth keeping, or
/// `None` if there are too many, they are too large, or none is.
fn normalize_items(items: &[Value]) -> Option<Vec<Value>> {
    if items.len() > MAX_ITEMS_PER_ENTRY {
        return None;
    }
    let mut normalized = Vec::with_capacity(items.len());
    let mut total_bytes = 0;
    for item in items {
        if let Some(item) = normalize_item(item) {
            total_bytes += serde_json::to_string(&item).map_or(0, |text| text.len());
            if total_bytes > MAX_BYTES_PER_ENTRY {
                return None;
            }
            normalized.push(item);
        }
    }
    (!normalized.is_empty()).then_some(normalized)
}

/// `normalizeAntigravityReasoningReplayItem`.
fn normalize_item(item: &Value) -> Option<Value> {
    match str_of(item.get("type")).trim() {
        "thought_signature" => normalize_thought_signature(item),
        "function_call_part" => normalize_function_call_part(item),
        _ => None,
    }
}

fn trimmed(value: Option<&Value>) -> String {
    str_of(value).trim().to_owned()
}

/// Copies `contentIndex` and `partIndex` if they are numbers.
fn copy_indexes(from: &Value, to: &mut Map<String, Value>) {
    for key in ["contentIndex", "partIndex"] {
        if let Some(index @ Value::Number(_)) = from.get(key) {
            to.insert(key.to_owned(), int_of(index).into());
        }
    }
}

/// Copies `targetOccurrence` if it is a number that isn't negative, then
/// `contextHash` if it isn't blank.
fn copy_occurrence_and_context(from: &Value, to: &mut Map<String, Value>) {
    if let Some(occurrence @ Value::Number(_)) = from.get("targetOccurrence")
        && int_of(occurrence) >= 0
    {
        to.insert("targetOccurrence".to_owned(), int_of(occurrence).into());
    }
    let context_hash = trimmed(from.get("contextHash"));
    if !context_hash.is_empty() {
        to.insert("contextHash".to_owned(), Value::String(context_hash));
    }
}

/// `normalizeAntigravityThoughtSignatureReplayItem`.
fn normalize_thought_signature(item: &Value) -> Option<Value> {
    let mut signature = trimmed(item.get("thoughtSignature"));
    if signature.is_empty() {
        signature = trimmed(item.get("thought_signature"));
    }
    if signature.is_empty() || signature == BYPASS || signature.len() < MIN_SIGNATURE_LEN {
        return None;
    }
    let mut out = Map::new();
    out.insert("type".to_owned(), "thought_signature".into());
    out.insert("thoughtSignature".to_owned(), Value::String(signature));
    copy_indexes(item, &mut out);
    let target_kind = trimmed(item.get("targetKind"));
    if target_kind == "text" || target_kind == "thought" {
        out.insert("targetKind".to_owned(), Value::String(target_kind));
    }
    let target_hash = trimmed(item.get("targetHash"));
    if !target_hash.is_empty() {
        out.insert("targetHash".to_owned(), Value::String(target_hash));
    }
    copy_occurrence_and_context(item, &mut out);
    Some(Value::Object(out))
}

/// `normalizeAntigravityFunctionCallPartReplayItem`.
fn normalize_function_call_part(item: &Value) -> Option<Value> {
    let mut call_id = trimmed(item.get("call_id"));
    if call_id.is_empty() {
        call_id = trimmed(item.get("id"));
    }
    let mut name = trimmed(item.get("name"));
    let mut args = item.get("args");
    if (name.is_empty() || args.is_none())
        && let Some(function_call) = item.get("functionCall")
    {
        if call_id.is_empty() {
            call_id = trimmed(function_call.get("id"));
        }
        if name.is_empty() {
            name = trimmed(function_call.get("name"));
        }
        if args.is_none() {
            args = function_call.get("args");
        }
    }
    let args = args?;
    if name.is_empty() {
        return None;
    }
    let mut out = Map::new();
    out.insert("type".to_owned(), "function_call_part".into());
    if !call_id.is_empty() {
        out.insert("call_id".to_owned(), Value::String(call_id));
    }
    out.insert("name".to_owned(), Value::String(name));
    out.insert("args".to_owned(), args.clone());
    let signature = trimmed(item.get("thoughtSignature"));
    if !signature.is_empty() && signature != BYPASS {
        out.insert("thoughtSignature".to_owned(), Value::String(signature));
    }
    copy_indexes(item, &mut out);
    copy_occurrence_and_context(item, &mut out);
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests;
