// Ported from CLIProxyAPI sdk/cliproxy/session (info.go and identity.go)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The session a request belongs to, for session affinity: read from what
//! the client sent, or derived from the conversation's start when it sent
//! nothing.
//!
//! [`extract_session_info`] reads the session a client named, in its
//! headers or its body, with the parent it came from: the session IDs of
//! Claude Code, Codex, OpenCode, Pi, Roo Code, Cline, OpenHands, Hermes,
//! OpenClaw and other clients, Gemini's cached content, a prompt cache key
//! or a conversation. [`derived_session_id`] derives one for a request that
//! names none, from its leading instructions and first user message, so a
//! conversation keeps its identity as it grows.
//!
//! Both are local routing keys only (policy): the manager hashes them into
//! its affinity bindings, and nothing here is ever sent upstream, written
//! into a request, logged or saved.
//!
//! Deviations from upstream:
//! - Only what session affinity reads is ported: [`SessionInfo`] holds no
//!   caller scope, credential, provider, model, node kind or metadata, and
//!   nothing is copied into a request's metadata (upstream's `Enrich`
//!   writes the IDs into it).
//! - Antigravity's session header (`X-Http-Session-Id`) isn't read, nor
//!   its request format when deriving an identity (out of scope).
//! - Not ported: the LCP fingerprints and their sessions,
//!   `NormalizeToCanonicalUUID`, and the deprecated session tree store,
//!   which only the plugin host and Home read.
//! - The body is read as [`Payload`] reads it (see its module): a key given
//!   twice reads as its last value, where upstream's reads take the first;
//!   bytes that aren't UTF-8 read as U+FFFD; a body that doesn't parse, or
//!   nests more than 128 deep, names no session.
//! - The text of an object or array where a session ID was expected is its
//!   compact JSON; gjson gives it as it was written.

mod identity;
mod info;
mod payload;

pub use identity::{
    MAX_SESSION_ID_LENGTH, caller_scope, claude_metadata_identities, derive_id, derived_session_id,
    has_explicit_session, normalize_explicit_id,
};
pub use info::{SessionInfo, bound_session_identity, extract_session_info};
pub use payload::Payload;

#[cfg(test)]
mod tests;
