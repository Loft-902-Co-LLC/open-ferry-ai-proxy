// Ported from CLIProxyAPI internal/logging/requestid.go (GenerateRequestID,
// ShortRequestID), internal/logging/requestmeta.go (ClientRequestMetadata,
// WithEndpoint), sdk/api/handlers/handlers.go (GetContextWithCancel's
// endpoint and client metadata) and
// sdk/cliproxy/auth/conductor_execution.go (publishSelectedAuthMetadata's
// selected index) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What the request log, the usage statistics and the access log see of a
//! request.
//!
//! The server makes a [`RequestContext`] for each request as it arrives,
//! and gives each call it makes an [`Observation`]: the context, and the
//! [`Tap`]s that want to see the call's upstream traffic. The call carries
//! it in [`Options::observation`]. Executors report each upstream attempt's
//! request, response head and body chunks to the taps; the manager records
//! the credential it picked in the context ([`SelectedAuth`]), and tells the
//! taps how each executor call ended (see [`CallReport`] and
//! [`observe_stream`]).
//!
//! With request logging and usage statistics off, a call has no taps and
//! each report costs a branch. A tap runs on the request's task, so its
//! methods must return at once: copy what they need, or hand it on with
//! `try_send`; never wait.
//!
//! [`Observability`] bundles the process-wide handles that make the taps,
//! for the server, the management API and the binary.
//!
//! Deviations from upstream:
//! - The request's metadata is one [`RequestContext`] shared through an
//!   `Arc`, where upstream keeps context values and gin keys.
//! - The context holds no session IDs: the usage statistics read the
//!   client's own session headers from the call's options, and never derive
//!   one (policy).
//! - The selected credential is the [`Auth`] snapshot the call was given,
//!   where upstream keeps its ID and index in the call's metadata.
//! - [`short_request_id`] keeps whole characters: a cut through a multibyte
//!   character moves forward to the next one, where Go cuts the bytes.
//!
//! [`Options::observation`]: crate::exec::Options::observation

pub mod client_ip;
pub mod dirs;
pub mod mask;
pub mod redact;
mod report;
pub mod request_log;
mod tap;
pub mod usage;

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use chrono::{DateTime, Utc};
use http::Method;

pub use report::{CallReport, observe_stream};
pub use tap::{AttemptKind, AttemptRequest, Observation, Outcome, Tap};

use crate::auth::Auth;

/// A request's ID: a version 7 UUID in its usual text form (upstream's
/// `GenerateRequestID`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// A new ID.
    pub fn generate() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    /// The ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The ID's last eight characters, as log lines show it (upstream's
    /// `ShortRequestID`).
    pub fn short(&self) -> &str {
        short_request_id(&self.0)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `id`'s last eight bytes once trimmed, or all of it when it is no longer
/// (upstream's `ShortRequestID`).
pub fn short_request_id(id: &str) -> &str {
    let id = id.trim();
    let Some(mut start) = id.len().checked_sub(8) else {
        return id;
    };
    while !id.is_char_boundary(start) {
        start += 1;
    }
    id.get(start..).unwrap_or(id)
}

/// What is known of a request from its arrival: its ID and timing, its
/// route and the client's address and agent (upstream's request ID, endpoint
/// and `ClientRequestMetadata`). The client's key and the credential picked
/// for it are filled in as the request goes.
///
/// `Debug` leaves the client's key out.
pub struct RequestContext {
    /// The request's ID.
    pub id: RequestId,
    /// When the request arrived, for durations.
    pub started: Instant,
    /// When the request arrived, for timestamps.
    pub started_at: DateTime<Utc>,
    /// The method.
    pub method: Method,
    /// The path, without the query.
    pub path: String,
    /// The method and the route that matched, such as
    /// `POST /v1/chat/completions`, or the method and the path when none
    /// did (upstream's endpoint).
    pub endpoint: String,
    /// The connection's address, as Go writes a request's `RemoteAddr`
    /// without its port (`ClientIP`, from upstream's `requestClientIP`).
    /// Empty when unknown.
    pub client_ip: String,
    /// The client's address as gin's `ClientIP` works it out, believing
    /// forwarded-address headers only from trusted proxies
    /// (`ResolvedClientIP`).
    pub resolved_client_ip: String,
    /// The `X-Forwarded-For` values, joined with `, ` and trimmed
    /// (`XForwardedFor`).
    pub forwarded_for: String,
    /// The trimmed `User-Agent` (`UserAgent`).
    pub user_agent: String,
    client_key: OnceLock<String>,
    selected: Mutex<Option<SelectedAuth>>,
    request_log: request_log::RequestState,
}

impl RequestContext {
    /// A context for a request with `method` to `path` arriving now, with
    /// a new ID, `path` as its endpoint, and nothing known of the client.
    pub fn new(method: Method, path: String) -> Self {
        let endpoint = format!("{method} {path}");
        Self {
            id: RequestId::generate(),
            started: Instant::now(),
            started_at: Utc::now(),
            method,
            path,
            endpoint,
            client_ip: String::new(),
            resolved_client_ip: String::new(),
            forwarded_for: String::new(),
            user_agent: String::new(),
            client_key: OnceLock::new(),
            selected: Mutex::new(None),
            request_log: request_log::RequestState::default(),
        }
    }

    /// Records the key the client authenticated with. The first one
    /// recorded stays.
    pub fn set_client_key(&self, key: &str) {
        let _ = self.client_key.set(key.to_owned());
    }

    /// The key the client authenticated with, when it gave one. It is a
    /// secret: never log it as it is.
    pub fn client_key(&self) -> Option<&str> {
        self.client_key.get().map(String::as_str)
    }

    /// The client's key as logs show it (see [`mask::hide_api_key`]).
    pub fn masked_client_key(&self) -> Option<String> {
        self.client_key().map(mask::hide_api_key)
    }

    /// Records the credential a call was given. The latest one stays, as
    /// upstream's trace callback keeps the latest.
    pub fn select(&self, selected: SelectedAuth) {
        *self.selected.lock().unwrap_or_else(PoisonError::into_inner) = Some(selected);
    }

    /// The credential the request's latest call was given, if any.
    pub fn selected(&self) -> Option<SelectedAuth> {
        self.selected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What the request log keeps for this request.
    pub fn request_log(&self) -> &request_log::RequestState {
        &self.request_log
    }
}

impl fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestContext")
            .field("id", &self.id)
            .field("method", &self.method)
            .field("endpoint", &self.endpoint)
            .field("client_ip", &self.client_ip)
            .field("resolved_client_ip", &self.resolved_client_ip)
            .field("client_key", &self.client_key.get().is_some())
            .field("selected", &self.selected())
            .finish_non_exhaustive()
    }
}

/// The credential a call was given, and when.
#[derive(Clone)]
pub struct SelectedAuth {
    /// The credential, as it was when picked. It holds secrets: never log
    /// it whole.
    pub auth: Arc<Auth>,
    /// When it was picked.
    pub selected_at: DateTime<Utc>,
}

impl SelectedAuth {
    /// `auth`, picked now.
    pub fn new(auth: Arc<Auth>) -> Self {
        Self {
            auth,
            selected_at: Utc::now(),
        }
    }

    /// The credential's trimmed index, as the trace ID and usage records
    /// show it.
    pub fn index(&self) -> &str {
        self.auth.index.trim()
    }

    /// The credential's ID.
    pub fn id(&self) -> &str {
        &self.auth.id
    }

    /// The credential's provider.
    pub fn provider(&self) -> &str {
        &self.auth.provider
    }

    /// `oauth` or `api_key`, or empty when unknown (the kind of
    /// [`Auth::account_info`]).
    pub fn auth_type(&self) -> &'static str {
        self.auth.account_info().map_or("", |(kind, _)| kind)
    }

    /// The credential's label.
    pub fn label(&self) -> &str {
        &self.auth.label
    }
}

impl fmt::Debug for SelectedAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelectedAuth")
            .field("id", &self.id())
            .field("index", &self.index())
            .field("provider", &self.provider())
            .field("selected_at", &self.selected_at)
            .finish()
    }
}

/// The process-wide observability handles: the log directory, the request
/// logger and the usage statistics. The binary makes them at start and
/// hands them to the server and the management API. Cloning gives other
/// handles to the same state.
#[derive(Clone, Debug, Default)]
pub struct Observability {
    /// Where logs go, resolved once at start as upstream resolves it (see
    /// [`dirs::resolve_log_directory`]); `None` in a state made without
    /// one, as in tests.
    pub log_dir: Option<PathBuf>,
    /// The request logger.
    pub request_log: request_log::RequestLogger,
    /// The usage statistics.
    pub usage: usage::Usage,
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn is_v7(id: &str) -> bool {
        uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.get_version_num() == 7)
    }

    // Ports TestGenerateRequestID_ValidUUIDv7.
    #[test]
    fn request_ids_are_uuid_v7() {
        for _ in 0..100 {
            let id = RequestId::generate();
            assert!(is_v7(id.as_str()), "{id}");
        }
    }

    // Ports TestGenerateRequestID_Concurrency.
    #[test]
    fn request_ids_are_unique_across_threads() {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| (0..125).map(|_| RequestId::generate()).collect::<Vec<_>>())
            })
            .collect();
        let mut seen = HashSet::new();
        for handle in handles {
            for id in handle.join().unwrap() {
                assert!(is_v7(id.as_str()), "{id}");
                assert!(seen.insert(id.clone()), "duplicate {id}");
            }
        }
        assert_eq!(seen.len(), 1000);
    }

    // Ports TestShortRequestID.
    #[test]
    fn short_request_ids() {
        for (input, want) in [
            ("", ""),
            ("   ", ""),
            ("req-1", "req-1"),
            ("00000042", "00000042"),
            ("--------", "--------"),
            ("018f3a5b-1234-7abc-def0-12345678abcd", "5678abcd"),
            (" 018f3a5b-1234-7abc-def0-12345678abcd \n", "5678abcd"),
        ] {
            assert_eq!(short_request_id(input), want, "{input:?}");
        }
        let id = RequestId("018f3a5b-1234-7abc-def0-12345678abcd".to_owned());
        assert_eq!(id.short(), "5678abcd");
    }

    // Not upstream's: a cut through a multibyte character keeps whole
    // characters.
    #[test]
    fn short_request_ids_keep_whole_characters() {
        // The last eight bytes start inside the "é".
        assert_eq!(short_request_id("abcdéfghijkl"), "fghijkl");
    }

    // Not upstream's: the context records the client's key once and the
    // latest selection, and its Debug output hides the key.
    #[test]
    fn context_keeps_the_first_key_and_the_latest_selection() {
        let context = RequestContext::new(Method::POST, "/v1/messages".to_owned());
        assert_eq!(context.endpoint, "POST /v1/messages");
        assert_eq!(context.client_key(), None);
        context.set_client_key("sk-client-secret-1");
        context.set_client_key("sk-client-secret-2");
        assert_eq!(context.client_key(), Some("sk-client-secret-1"));
        assert_eq!(context.masked_client_key().as_deref(), Some("sk-c...et-1"));
        assert!(!format!("{context:?}").contains("secret"));

        let auth = |id: &str| {
            Arc::new(Auth {
                id: id.to_owned(),
                index: format!(" {id}-index "),
                ..Auth::default()
            })
        };
        assert!(context.selected().is_none());
        context.select(SelectedAuth::new(auth("a")));
        context.select(SelectedAuth::new(auth("b")));
        let selected = context.selected().unwrap();
        assert_eq!((selected.id(), selected.index()), ("b", "b-index"));
        assert_eq!(selected.auth_type(), "");
    }
}
