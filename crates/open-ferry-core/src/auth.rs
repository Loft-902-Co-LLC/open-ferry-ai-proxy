// Ported from CLIProxyAPI sdk/cliproxy/auth/types.go, status.go and the Error
// type in errors.go, and the Store interface in sdk/cliproxy/auth/store.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Credentials: an [`Auth`] record for each account or API key, and the
//! [`AuthStore`] that keeps them.
//!
//! A record's [`Auth::metadata`] is the credential's file as JSON, tokens
//! included, and is what gets saved; [`Auth::attributes`] are fixed settings
//! from the config, such as an API key and its base URL. The rest is runtime
//! state the credential manager keeps.
//!
//! Deviations from upstream:
//! - Times are `Option`s, where upstream uses Go's zero time for "never".
//! - `Debug` leaves out metadata and attribute values, which hold secrets.
//! - Upstream's recent-request ring, registration epoch, generation and
//!   plugin fields aren't ported; nor is `Runtime`.

use std::collections::BTreeMap;
use std::fmt;
use std::io;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

/// A point in time, in UTC.
pub type Timestamp = DateTime<Utc>;

/// Where a credential is in its life (upstream's `Status`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Status {
    /// Not known yet.
    #[default]
    Unknown,
    /// Ready for calls.
    Active,
    /// Waiting on something outside the proxy, such as a sign-in.
    Pending,
    /// Being refreshed.
    Refreshing,
    /// Unavailable for now, after errors.
    Error,
    /// Turned off by the operator.
    Disabled,
}

impl Status {
    /// Upstream's name for the status.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Active => "active",
            Self::Pending => "pending",
            Self::Refreshing => "refreshing",
            Self::Error => "error",
            Self::Disabled => "disabled",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failure recorded on a credential (upstream's `auth.Error`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthError {
    /// A short code for machines, or empty.
    pub code: String,
    /// What went wrong.
    pub message: String,
    /// Whether trying again may work.
    pub retryable: bool,
    /// The HTTP status behind it, or 0.
    pub http_status: u16,
}

/// What the credential manager knows of a credential's quota (upstream's
/// `QuotaState`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuotaState {
    /// Whether the credential hit its quota recently.
    pub exceeded: bool,
    /// Why, in the provider's words, or empty.
    pub reason: String,
    /// When the credential may be used again.
    pub next_recover_at: Option<Timestamp>,
    /// How many times in a row the cooldown has grown.
    pub backoff_level: u32,
}

/// A credential's state for one model (upstream's `ModelState`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelState {
    /// Where the credential is for this model.
    pub status: Status,
    /// Why, or empty.
    pub status_message: String,
    /// Whether the model is blocked for this credential for now.
    pub unavailable: bool,
    /// When to try this model again.
    pub next_retry_after: Option<Timestamp>,
    /// The last failure for this model.
    pub last_error: Option<AuthError>,
    /// Quota for this model.
    pub quota: QuotaState,
    /// When this state last changed.
    pub updated_at: Option<Timestamp>,
}

/// One credential and its runtime state (upstream's `Auth`).
#[derive(Clone, Default)]
pub struct Auth {
    /// Unique across restarts. For a credential file, its path relative to
    /// the auth directory.
    pub id: String,
    /// The provider, such as `codex` or `claude`.
    pub provider: String,
    /// Namespaces the credential's models, as in `team-a/gpt-5`, or empty.
    pub prefix: String,
    /// The credential's file, or empty when it comes from the config.
    pub file_name: String,
    /// A name for logs, or empty.
    pub label: String,
    /// Where it is in its life.
    pub status: Status,
    /// Why, or empty.
    pub status_message: String,
    /// Turned off by the operator.
    pub disabled: bool,
    /// Unavailable for now, as after a quota error.
    pub unavailable: bool,
    /// A proxy for this credential's calls, or empty for the global one.
    pub proxy_url: String,
    /// Fixed settings for executors, such as `api_key` and `base_url`.
    pub attributes: BTreeMap<String, String>,
    /// The credential's saved state, tokens included.
    pub metadata: Map<String, Value>,
    /// Quota across models.
    pub quota: QuotaState,
    /// The last failure, from a call or a refresh.
    pub last_error: Option<AuthError>,
    /// When the record was made.
    pub created_at: Option<Timestamp>,
    /// When it last changed.
    pub updated_at: Option<Timestamp>,
    /// When its tokens were last refreshed.
    pub last_refreshed_at: Option<Timestamp>,
    /// The earliest time to refresh again.
    pub next_refresh_after: Option<Timestamp>,
    /// The earliest time to call with it again.
    pub next_retry_after: Option<Timestamp>,
    /// State per model.
    pub model_states: BTreeMap<String, ModelState>,
}

impl Auth {
    /// The metadata value at `key`, if it is a string.
    pub fn metadata_str(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).and_then(Value::as_str)
    }

    /// The attribute at `key`, if set.
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("prefix", &self.prefix)
            .field("file_name", &self.file_name)
            .field("label", &self.label)
            .field("status", &self.status)
            .field("disabled", &self.disabled)
            .field("unavailable", &self.unavailable)
            .field("attributes", &self.attributes.keys().collect::<Vec<_>>())
            .field("metadata", &self.metadata.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Keeps credentials (upstream's `auth.Store`).
pub trait AuthStore: Send + Sync + 'static {
    /// Every credential the store holds.
    fn list(&self) -> io::Result<Vec<Auth>>;

    /// Saves `auth`, and returns where it went, such as a file path.
    fn save(&self, auth: &Auth) -> io::Result<String>;

    /// Removes the credential with `id`. Removing one that isn't there is
    /// not an error.
    fn delete(&self, id: &str) -> io::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_leaves_out_secrets() {
        let mut auth = Auth {
            id: "codex-a.json".into(),
            provider: "codex".into(),
            ..Auth::default()
        };
        auth.metadata
            .insert("access_token".into(), Value::from("secret-token"));
        auth.attributes
            .insert("api_key".into(), "secret-key".into());
        let debug = format!("{auth:?}");
        assert!(debug.contains("access_token"), "{debug}");
        assert!(debug.contains("api_key"), "{debug}");
        assert!(!debug.contains("secret"), "{debug}");
    }
}
