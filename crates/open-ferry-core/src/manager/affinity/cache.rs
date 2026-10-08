// Ported from CLIProxyAPI sdk/cliproxy/auth/session_cache.go (SessionCache,
// NewSessionCacheWithCapacity, Get, GetAndRefresh, Set, SetAliases,
// replaceAliasGroupsLocked, evictExcessLocked, removeAliasGroupLocked,
// compactSessionAliases, isLocalPromptCacheSessionAlias,
// mergeSessionAliases, Touch, CompareAndDelete, InvalidateAuth, Len and
// cleanup) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The session bindings: which credential each session key is bound to,
//! until when.
//!
//! The keys of one logical session (a session and its parent, say) form a
//! group that is bound, refreshed, moved and dropped together. A binding
//! lives for the TTL after it was last bound or refreshed. When the cache
//! holds more keys than its cap, the oldest groups go first.
//!
//! Deviations from upstream:
//! - A key is held as the SHA-256 of its text, never as the text (policy:
//!   session IDs stay out of memory dumps and debug output), with whether
//!   it is a prompt cache key, which compaction needs.
//! - There is no lock: the cache lives in the manager's state, behind its
//!   lock. The time is passed in, and expired groups are swept by the
//!   first call at least half a TTL after the last sweep, where upstream
//!   runs a goroutine on a ticker; so there is no `Stop`.
//! - A group is known by a sequence number, where upstream compares its
//!   credential, expiry and keys; each key points at the group it belongs
//!   to, so the two agree.
//! - `Invalidate` isn't ported: nothing calls it but the plugin host.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::auth::Timestamp;
use crate::manager::cooldown::add;

/// The most keys that aren't prompt cache keys one group keeps (upstream's
/// `maxStableSessionAliases`).
const MAX_STABLE_SESSION_ALIASES: usize = 64;

/// The most keys the cache holds (upstream's `defaultMaxSessionEntries`).
pub(crate) const DEFAULT_MAX_SESSION_ENTRIES: usize = 65536;

/// The TTL of a cache made with none (upstream's 30 minutes).
const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);

/// The shortest time between two sweeps.
const MIN_SWEEP_INTERVAL: Duration = Duration::from_millis(1);

/// A session key, hashed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    digest: [u8; 32],
    /// Whether the key is a prompt cache key (upstream's
    /// `isLocalPromptCacheSessionAlias`).
    prompt_cache: bool,
}

impl Key {
    /// The key for `raw`, or none for an empty one.
    fn of(raw: &str) -> Option<Self> {
        if raw.is_empty() {
            return None;
        }
        Some(Self {
            digest: Sha256::digest(raw.as_bytes()).into(),
            prompt_cache: is_local_prompt_cache_session_alias(raw),
        })
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(..)")
    }
}

/// Whether `alias` is a prompt cache key, alone or inside a cache key
/// (upstream's `isLocalPromptCacheSessionAlias`).
fn is_local_prompt_cache_session_alias(alias: &str) -> bool {
    if alias.starts_with("pck:") {
        return true;
    }
    alias
        .split_once("::")
        .is_some_and(|(_, session_and_model)| session_and_model.starts_with("pck:"))
}

/// The keys of `existing` then `candidates`, each once, in order (upstream's
/// `mergeSessionAliases`; empty keys are already gone).
fn merge(existing: &[Key], candidates: &[Key]) -> Vec<Key> {
    let mut out: Vec<Key> = Vec::with_capacity(existing.len() + candidates.len());
    for key in existing.iter().chain(candidates) {
        if !out.contains(key) {
            out.push(*key);
        }
    }
    out
}

/// `aliases` with only the first prompt cache key and the first 64 others
/// (upstream's `compactSessionAliases`).
fn compact(aliases: Vec<Key>) -> Vec<Key> {
    let mut has_prompt_cache = false;
    let mut stable = 0usize;
    aliases
        .into_iter()
        .filter(|key| {
            if key.prompt_cache {
                if has_prompt_cache {
                    return false;
                }
                has_prompt_cache = true;
            } else {
                if stable >= MAX_STABLE_SESSION_ALIASES {
                    return false;
                }
                stable += 1;
            }
            true
        })
        .collect()
}

/// The keys of one logical session, the credential they are bound to, and
/// until when (upstream's `sessionEntry`).
#[derive(Debug)]
struct Group {
    auth: String,
    expires_at: Timestamp,
    /// The keys; the first is the primary.
    aliases: Vec<Key>,
}

/// Session keys bound to credentials, for a TTL, with a cap on the keys
/// held (upstream's `SessionCache`).
#[derive(Debug)]
pub(crate) struct SessionCache {
    ttl: Duration,
    max_entries: usize,
    /// The groups by sequence number, oldest first (upstream's `groups` and
    /// its eviction order).
    groups: BTreeMap<u64, Group>,
    /// The group whose primary each key is.
    primaries: HashMap<Key, u64>,
    /// The group each key belongs to (upstream's `entries`).
    entries: HashMap<Key, u64>,
    next_seq: u64,
    next_sweep: Option<Timestamp>,
}

impl SessionCache {
    /// A cache whose bindings live for `ttl`, or 30 minutes for none
    /// (upstream's `NewSessionCache`).
    pub(crate) fn new(ttl: Duration) -> Self {
        Self::with_capacity(ttl, DEFAULT_MAX_SESSION_ENTRIES)
    }

    /// A cache holding at most `max_entries` keys, or 65536 for 0
    /// (upstream's `NewSessionCacheWithCapacity`).
    pub(crate) fn with_capacity(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl: if ttl.is_zero() { DEFAULT_TTL } else { ttl },
            max_entries: if max_entries == 0 {
                DEFAULT_MAX_SESSION_ENTRIES
            } else {
                max_entries
            },
            groups: BTreeMap::new(),
            primaries: HashMap::new(),
            entries: HashMap::new(),
            next_seq: 0,
            next_sweep: None,
        }
    }

    /// The live group `key` belongs to; an expired one is dropped.
    fn live(&mut self, key: Key, now: Timestamp) -> Option<u64> {
        let seq = *self.entries.get(&key)?;
        let expires_at = self.groups.get(&seq)?.expires_at;
        if now < expires_at {
            return Some(seq);
        }
        self.remove_group(seq);
        None
    }

    /// The credential `session_id` is bound to, without refreshing it
    /// (upstream's `Get`).
    pub(crate) fn get(&mut self, session_id: &str, now: Timestamp) -> Option<String> {
        self.sweep(now);
        let seq = self.live(Key::of(session_id)?, now)?;
        self.groups.get(&seq).map(|group| group.auth.clone())
    }

    /// The credential `session_id` is bound to, refreshing every key of its
    /// session (upstream's `GetAndRefresh`).
    pub(crate) fn get_and_refresh(&mut self, session_id: &str, now: Timestamp) -> Option<String> {
        self.sweep(now);
        let key = Key::of(session_id)?;
        let seq = self.live(key, now)?;
        let group = self.groups.get(&seq)?;
        let auth = group.auth.clone();
        let aliases = compact(merge(&[key], &group.aliases));
        self.replace(&auth, add(now, self.ttl), aliases, &[seq]);
        Some(auth)
    }

    /// Binds `session_id` to `auth`, with the keys of its session
    /// (upstream's `Set`).
    pub(crate) fn set(&mut self, session_id: &str, auth: &str, now: Timestamp) {
        self.set_aliases(auth, &[session_id], now);
    }

    /// Binds every one of `session_ids`, and the keys of their sessions, to
    /// `auth` as one session (upstream's `SetAliases`).
    pub(crate) fn set_aliases(&mut self, auth: &str, session_ids: &[&str], now: Timestamp) {
        if auth.is_empty() {
            return;
        }
        self.sweep(now);
        let keys: Vec<Key> = session_ids.iter().filter_map(|id| Key::of(id)).collect();
        let mut aliases = merge(&[], &keys);
        let mut previous: Vec<u64> = Vec::with_capacity(keys.len());
        for key in &keys {
            let Some(seq) = self.live(*key, now) else {
                continue;
            };
            previous.push(seq);
            if let Some(group) = self.groups.get(&seq) {
                aliases = merge(&aliases, &group.aliases);
            }
        }
        let aliases = compact(aliases);
        if aliases.is_empty() {
            return;
        }
        self.replace(auth, add(now, self.ttl), aliases, &previous);
    }

    /// Refreshes `session_id`'s session if it is bound to `expected`
    /// (upstream's `Touch`).
    pub(crate) fn touch(&mut self, session_id: &str, expected: &str, now: Timestamp) -> bool {
        if expected.is_empty() {
            return false;
        }
        self.sweep(now);
        let Some(key) = Key::of(session_id) else {
            return false;
        };
        let Some(seq) = self.entries.get(&key).copied() else {
            return false;
        };
        let Some(group) = self.groups.get(&seq) else {
            return false;
        };
        if group.auth != expected || now >= group.expires_at {
            return false;
        }
        let aliases = compact(merge(&[key], &group.aliases));
        self.replace(expected, add(now, self.ttl), aliases, &[seq]);
        true
    }

    /// Unbinds `session_id` if it is bound to `expected`; the other keys of
    /// its session stay bound (upstream's `CompareAndDelete`).
    pub(crate) fn compare_and_delete(
        &mut self,
        session_id: &str,
        expected: &str,
        now: Timestamp,
    ) -> bool {
        if expected.is_empty() {
            return false;
        }
        self.sweep(now);
        let Some(key) = Key::of(session_id) else {
            return false;
        };
        let Some(seq) = self.entries.get(&key).copied() else {
            return false;
        };
        if self
            .groups
            .get(&seq)
            .is_none_or(|group| group.auth != expected)
        {
            return false;
        }
        let Some(group) = self.remove_group(seq) else {
            return false;
        };
        let surviving: Vec<Key> = group.aliases.into_iter().filter(|k| *k != key).collect();
        if !surviving.is_empty() {
            self.replace(&group.auth, group.expires_at, surviving, &[]);
        }
        true
    }

    /// Drops every session bound to `auth` (upstream's `InvalidateAuth`).
    pub(crate) fn invalidate_auth(&mut self, auth: &str) {
        if auth.is_empty() {
            return;
        }
        let bound: Vec<u64> = self
            .groups
            .iter()
            .filter(|(_, group)| group.auth == auth)
            .map(|(seq, _)| *seq)
            .collect();
        for seq in bound {
            self.remove_group(seq);
        }
    }

    /// How many keys the cache holds (upstream's `Len`).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether `session_id` is held, live or not.
    #[cfg(test)]
    pub(crate) fn contains(&self, session_id: &str) -> bool {
        Key::of(session_id).is_some_and(|key| self.entries.contains_key(&key))
    }

    /// How many keys `session_id`'s session has, or 0 when it isn't held.
    #[cfg(test)]
    pub(crate) fn group_len(&self, session_id: &str) -> usize {
        Key::of(session_id)
            .and_then(|key| self.entries.get(&key))
            .and_then(|seq| self.groups.get(seq))
            .map_or(0, |group| group.aliases.len())
    }

    /// Drops the `previous` groups, then binds `aliases` to `auth` until
    /// `expires_at` as the newest group, and drops the oldest groups while
    /// the cache holds too many keys (upstream's
    /// `replaceAliasGroupsLocked` and `evictExcessLocked`).
    fn replace(&mut self, auth: &str, expires_at: Timestamp, aliases: Vec<Key>, previous: &[u64]) {
        for seq in previous {
            self.remove_group(*seq);
        }
        let Some(primary) = aliases.first().copied() else {
            return;
        };
        if let Some(existing) = self.primaries.get(&primary).copied() {
            self.remove_group(existing);
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.primaries.insert(primary, seq);
        for key in &aliases {
            self.entries.insert(*key, seq);
        }
        self.groups.insert(
            seq,
            Group {
                auth: auth.to_owned(),
                expires_at,
                aliases,
            },
        );
        while self.entries.len() > self.max_entries {
            let Some(oldest) = self.groups.keys().next().copied() else {
                break;
            };
            self.remove_group(oldest);
        }
    }

    /// Drops group `seq` and the keys that still point at it (upstream's
    /// `removeAliasGroupLocked`).
    fn remove_group(&mut self, seq: u64) -> Option<Group> {
        let group = self.groups.remove(&seq)?;
        if let Some(primary) = group.aliases.first()
            && self.primaries.get(primary) == Some(&seq)
        {
            self.primaries.remove(primary);
        }
        for key in &group.aliases {
            if self.entries.get(key) == Some(&seq) {
                self.entries.remove(key);
            }
        }
        Some(group)
    }

    /// Drops the expired groups once half a TTL has passed since the last
    /// sweep (upstream's `cleanupLoop` and `cleanup`).
    fn sweep(&mut self, now: Timestamp) {
        let interval = (self.ttl / 2).max(MIN_SWEEP_INTERVAL);
        match self.next_sweep {
            Some(due) if now >= due => {}
            Some(_) => return,
            None => {
                self.next_sweep = Some(add(now, interval));
                return;
            }
        }
        self.next_sweep = Some(add(now, interval));
        let expired: Vec<u64> = self
            .groups
            .iter()
            .filter(|(_, group)| now >= group.expires_at)
            .map(|(seq, _)| *seq)
            .collect();
        for seq in expired {
            self.remove_group(seq);
        }
    }
}
