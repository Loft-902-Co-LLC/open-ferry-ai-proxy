//! The usage records as an observer takes them: open-ferry's usage ledger
//! reads every record the taps publish, beside the usage queue.
//!
//! Observing doesn't change the queue. The queue's subscribers take records
//! away from it ([`super::Usage::subscribe_usage`]), and a subscriber that
//! falls behind is dropped; an observation takes nothing away, and an
//! observer that falls behind loses records, which are counted, but stays.
//! There is one observation at a time; a new one replaces the last.
//!
//! An event is typed, not JSON, and holds no prompt or answer text. It
//! holds the client's key, for the observer to hash and mask; the key's
//! `Debug` hides it.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::accounting::TokenBreakdown;

/// How many events an observer may fall behind before events are lost.
pub const OBSERVER_BUFFER: usize = 8192;

/// One executor call's usage, as an observer gets it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageEvent {
    /// When the call started.
    pub requested_at: DateTime<Utc>,
    /// The client request's ID.
    pub request_id: String,
    /// The method and route of the client's request (`POST /v1/messages`).
    pub endpoint: String,
    /// The provider called, `unknown` when blank.
    pub provider: String,
    /// The model sent upstream, `unknown` when blank, scrubbed of the
    /// call's secrets.
    pub model: String,
    /// The model the client asked for, the model when blank.
    pub alias: String,
    /// The credential used, if any.
    pub credential: Option<EventCredential>,
    /// The key the client authenticated with, empty for none.
    pub client_key: ClientKey,
    /// The client asked for a stream.
    pub stream: bool,
    /// The call failed.
    pub failed: bool,
    /// The upstream's status for a failed call (500 when there was none),
    /// 200 otherwise.
    pub status: i64,
    /// From the call's start to its end.
    pub latency: Duration,
    /// To the first token of a streamed answer, if one came.
    pub ttft: Option<Duration>,
    /// The tokens in buckets that don't overlap.
    pub tokens: TokenBreakdown,
    /// The total the provider reported, else the breakdown's.
    pub total_tokens: i64,
}

/// The credential a call used, by what the auth files and the manager know
/// it by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventCredential {
    /// The credential's ID (its file name, or the ID the config gave it).
    pub id: String,
    /// The credential's index, as the management API shows it.
    pub auth_index: String,
    /// The account or name the credential is labelled with.
    pub label: String,
    /// `oauth` or `apikey`, or empty.
    pub auth_type: String,
}

/// A client's key, which `Debug` doesn't show.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ClientKey(String);

impl ClientKey {
    /// `key`, as a client key.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key itself, for hashing and masking.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether there is no key.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("ClientKey(none)")
        } else {
            f.write_str("ClientKey(..)")
        }
    }
}

/// The observation in force, if any.
#[derive(Default)]
pub(crate) struct Slot {
    /// Whether there is an observation, read without the lock.
    present: AtomicBool,
    inner: Mutex<SlotInner>,
    /// Events lost since the start.
    dropped: AtomicU64,
}

#[derive(Default)]
struct SlotInner {
    /// The observation's number and its sender.
    sender: Option<(u64, SyncSender<UsageEvent>)>,
    /// The last observation's number.
    generation: u64,
}

impl Slot {
    fn lock(&self) -> MutexGuard<'_, SlotInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether there is an observation.
    pub(crate) fn present(&self) -> bool {
        self.present.load(Ordering::SeqCst)
    }

    /// Starts a new observation, ending any other.
    pub(crate) fn observe(self: &Arc<Self>) -> (Receiver<UsageEvent>, Observation) {
        let (sender, receiver) = mpsc::sync_channel(OBSERVER_BUFFER);
        let mut inner = self.lock();
        inner.generation += 1;
        let generation = inner.generation;
        inner.sender = Some((generation, sender));
        self.present.store(true, Ordering::SeqCst);
        drop(inner);
        (
            receiver,
            Observation {
                slot: Arc::clone(self),
                generation,
            },
        )
    }

    /// Sends `event` to the observer, if there is one; an event the
    /// observer has no room for is counted and dropped.
    pub(crate) fn send(&self, event: UsageEvent) {
        if !self.present() {
            return;
        }
        let mut inner = self.lock();
        let Some((_, sender)) = &inner.sender else {
            return;
        };
        match sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                inner.sender = None;
                self.present.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Ends observation `generation`, unless another replaced it.
    fn end(&self, generation: u64) {
        let mut inner = self.lock();
        if inner
            .sender
            .as_ref()
            .is_some_and(|(current, _)| *current == generation)
        {
            inner.sender = None;
            self.present.store(false, Ordering::SeqCst);
        }
    }
}

/// An observation of the usage records; dropping it ends it.
pub struct Observation {
    slot: Arc<Slot>,
    generation: u64,
}

impl Observation {
    /// How many events were lost because an observer fell behind, since the
    /// statistics were made.
    pub fn dropped(&self) -> u64 {
        self.slot.dropped.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for Observation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Observation")
            .field("generation", &self.generation)
            .field("dropped", &self.dropped())
            .finish()
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        self.slot.end(self.generation);
    }
}
