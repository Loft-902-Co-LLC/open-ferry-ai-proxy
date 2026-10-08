// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_videos_handlers.go
// (videoAuthBindingStore: setWithModel, getBinding, cleanupExpiredLocked)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which credential made each video, so the calls about it go back to that
//! credential: xAI only knows a video by the account that asked for it.
//!
//! Each video ID is held with the credential's ID and the model it was
//! made with, for a time (the config's `video-result-auth-cache-ttl`, else
//! three hours) and only in memory. Nothing here is logged.
//!
//! Deviations from upstream:
//! - The store holds at most [`CAP`] videos, dropping the one set longest
//!   ago to make room; upstream's grows until its entries expire.
//! - A video ID over [`MAX_ID_LEN`] bytes isn't held.
//! - The store belongs to the server and is kept across config reloads;
//!   upstream's is global to the process.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How many videos the store holds.
pub(crate) const CAP: usize = 10_000;
/// The longest video ID the store holds, in bytes.
pub(crate) const MAX_ID_LEN: usize = 512;
/// How long a video is held when the config doesn't say
/// (`defaultVideoAuthBindingTTL`).
pub(crate) const DEFAULT_TTL: Duration = Duration::from_secs(3 * 60 * 60);

/// The credential that made a video, and the model it was made with. It
/// isn't `Debug` outside tests, so it can't be logged by mistake.
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(Debug))]
pub(crate) struct Binding {
    pub(crate) auth_id: String,
    /// The model a credential is picked by, or empty.
    pub(crate) model: String,
}

/// A held video.
struct Entry {
    binding: Binding,
    /// When it is dropped; never, where the time can't be counted to.
    expires_at: Option<Instant>,
    /// Its place in [`Store::order`].
    seq: u64,
}

impl Entry {
    fn expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|at| now > at)
    }
}

#[derive(Default)]
struct Store {
    entries: HashMap<String, Entry>,
    /// The held videos, by when they were last set.
    order: BTreeMap<u64, String>,
    next: u64,
}

impl Store {
    fn remove(&mut self, video_id: &str) {
        if let Some(entry) = self.entries.remove(video_id) {
            self.order.remove(&entry.seq);
        }
    }
}

/// The videos the server made, by ID, with the credential that made each
/// (upstream's `videoAuthBindingStore`).
#[derive(Default)]
pub(crate) struct VideoBindings {
    store: Mutex<Store>,
}

impl VideoBindings {
    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Holds `video_id` as made by `auth_id` with `model`, for `ttl`, or
    /// three hours if it is zero (`setWithModel`). An ID or credential
    /// that is empty once trimmed isn't held.
    pub(crate) fn set(&self, video_id: &str, auth_id: &str, model: &str, ttl: Duration) {
        self.set_at(video_id, auth_id, model, ttl, Instant::now());
    }

    pub(crate) fn set_at(
        &self,
        video_id: &str,
        auth_id: &str,
        model: &str,
        ttl: Duration,
        now: Instant,
    ) {
        let video_id = video_id.trim();
        let auth_id = auth_id.trim();
        if video_id.is_empty() || auth_id.is_empty() || video_id.len() > MAX_ID_LEN {
            return;
        }
        let ttl = if ttl.is_zero() { DEFAULT_TTL } else { ttl };
        let mut store = self.lock();
        // `cleanupExpiredLocked`.
        let expired: Vec<String> = store
            .entries
            .iter()
            .filter(|(_, entry)| entry.expired(now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            store.remove(&id);
        }
        store.remove(video_id);
        while store.entries.len() >= CAP {
            let Some((_, oldest)) = store.order.pop_first() else {
                break;
            };
            store.entries.remove(&oldest);
        }
        let seq = store.next;
        store.next = seq.wrapping_add(1);
        store.order.insert(seq, video_id.to_owned());
        store.entries.insert(
            video_id.to_owned(),
            Entry {
                binding: Binding {
                    auth_id: auth_id.to_owned(),
                    model: model.trim().to_owned(),
                },
                expires_at: now.checked_add(ttl),
                seq,
            },
        );
    }

    /// The credential and model `video_id` was made with, unless it isn't
    /// held or has expired, when it is dropped (`getBinding`).
    pub(crate) fn get(&self, video_id: &str) -> Option<Binding> {
        self.get_at(video_id, Instant::now())
    }

    pub(crate) fn get_at(&self, video_id: &str, now: Instant) -> Option<Binding> {
        let video_id = video_id.trim();
        if video_id.is_empty() {
            return None;
        }
        let mut store = self.lock();
        let entry = store.entries.get(video_id)?;
        if entry.expired(now) {
            store.remove(video_id);
            return None;
        }
        Some(entry.binding.clone())
    }

    /// How many videos are held.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(auth_id: &str, model: &str) -> Option<Binding> {
        Some(Binding {
            auth_id: auth_id.to_owned(),
            model: model.to_owned(),
        })
    }

    #[test]
    fn video_auth_binding_store_expires_entries() {
        // TestVideoAuthBindingStoreExpiresEntries.
        let store = VideoBindings::default();
        let start = Instant::now();
        store.set_at(
            "video-expired",
            "auth-expired",
            "",
            Duration::from_secs(1),
            start,
        );
        let later = start + Duration::from_secs(2);
        assert_eq!(store.get_at("video-expired", later), None);
        assert_eq!(store.len(), 0, "the expired binding was not removed");
    }

    // Not upstream's: a binding is held, trimmed, until just after its
    // time; setting one sweeps out the expired.
    #[test]
    fn bindings_are_held_for_their_time() {
        let store = VideoBindings::default();
        let start = Instant::now();
        store.set_at(" v1 ", " a ", " m ", Duration::from_secs(10), start);
        let at_expiry = start + Duration::from_secs(10);
        assert_eq!(store.get_at("v1", at_expiry), binding("a", "m"));
        assert_eq!(store.get_at(" v1\t", at_expiry), binding("a", "m"));
        store.set_at("v2", "b", "", Duration::from_secs(60), start);
        store.set_at(
            "v3",
            "c",
            "",
            Duration::from_secs(60),
            at_expiry + Duration::from_nanos(1),
        );
        assert_eq!(store.len(), 2);
        assert_eq!(store.get_at("v2", at_expiry), binding("b", ""));
    }

    // Not upstream's: a zero time is three hours; one too long to count
    // never ends.
    #[test]
    fn zero_ttls_default_and_huge_ones_never_end() {
        let store = VideoBindings::default();
        let start = Instant::now();
        store.set_at("v", "a", "", Duration::ZERO, start);
        assert_eq!(store.get_at("v", start + DEFAULT_TTL), binding("a", ""));
        assert_eq!(
            store.get_at("v", start + DEFAULT_TTL + Duration::from_secs(1)),
            None
        );
        store.set_at("w", "a", "", Duration::MAX, start);
        assert_eq!(
            store.get_at("w", start + Duration::from_secs(100 * 365 * 24 * 3600)),
            binding("a", "")
        );
    }

    // Not upstream's: what isn't held.
    #[test]
    fn empty_and_long_ids_are_not_held() {
        let store = VideoBindings::default();
        let ttl = Duration::from_secs(60);
        store.set(" ", "a", "", ttl);
        store.set("v", " ", "", ttl);
        store.set(&"x".repeat(MAX_ID_LEN + 1), "a", "", ttl);
        assert_eq!(store.len(), 0);
        let longest = "x".repeat(MAX_ID_LEN);
        store.set(&longest, "a", "", ttl);
        assert_eq!(store.get(&longest), binding("a", ""));
        assert_eq!(store.get(""), None);
    }

    // Not upstream's: the store holds at most `CAP` videos, dropping the
    // one set longest ago; setting a video again makes it the newest.
    #[test]
    fn the_oldest_binding_makes_room() {
        let store = VideoBindings::default();
        let ttl = Duration::from_secs(60);
        for i in 0..CAP {
            store.set(&format!("v{i}"), "a", "", ttl);
        }
        store.set("v0", "b", "", ttl);
        store.set("new", "c", "", ttl);
        assert_eq!(store.len(), CAP);
        assert_eq!(store.get("v1"), None);
        assert_eq!(store.get("v0"), binding("b", ""));
        assert_eq!(store.get("v2"), binding("a", ""));
        assert_eq!(store.get("new"), binding("c", ""));
    }
}
