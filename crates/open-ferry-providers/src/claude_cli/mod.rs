//! `claude-cli`: Claude models served by the user's own installed Claude
//! Code, run once for each request. open-ferry's own; upstream has no such
//! provider.
//!
//! Anthropic's terms let only Claude Code itself use a Claude subscription
//! sign-in: a third-party tool may not take its credentials and call the
//! API with them. So this provider never does. It runs the `claude`
//! executable the user installed, unmodified, as `claude -p` with JSON
//! lines in and out (`stream-json`), and Claude Code signs itself in and
//! makes every request to Anthropic. open-ferry reads no credential, stores
//! none, runs no sign-in and sends no HTTP request for this provider: an
//! entry only says how to run Claude Code (see the config's `claude-cli`
//! list).
//!
//! Each call:
//! - Translates the client's request to Claude's Messages format as the
//!   Claude executor does, thinking suffixes included, and refuses what
//!   Claude Code can't do for a client: client tools, a tool choice other
//!   than `none` or `auto`, tool use or results in the messages, a last
//!   turn that isn't the user's, and no message at all. Sampling settings,
//!   metadata and the like are dropped, with a debug line. A conversation
//!   of several turns goes as one message holding a transcript of it, the
//!   same for the same turns, so each new turn only adds to the last
//!   request's.
//! - Waits for one of the entry's `max-concurrency` slots, then runs
//!   Claude Code in an empty working directory of the entry's own, with
//!   tools, MCP servers, slash commands and session files off, one turn,
//!   and the client's system prompt in a file beside that directory,
//!   removed once the process ends. The environment loses every
//!   `ANTHROPIC_*` and `CLAUDE*` variable and `MAX_THINKING_TOKENS`, but
//!   `CLAUDE_CODE_OAUTH_TOKEN` and `CLAUDE_CONFIG_DIR` for an entry
//!   without a config directory; an entry's config directory is
//!   `CLAUDE_CONFIG_DIR`.
//! - Gives Claude Code the request as one user message on standard input,
//!   and closes it.
//! - Reads Claude Code's events: Anthropic's stream events become a Claude
//!   SSE stream, translated to the client's format, or one answer for a
//!   call that isn't streamed; the result gives the usage and ends the
//!   call; Claude Code's reports of the account's rate-limit windows become
//!   Anthropic's unified rate-limit headers, which feed quota readings.
//!   Thinking the client didn't ask for is left out.
//! - Ends the process when the call is dropped or its `timeout` passes.
//!
//! [`check_version`] checks an entry's Claude Code is new enough, and
//! [`auth_status`] asks it whether it is signed in, giving back only that
//! and how.

mod aggregate;
mod events;
mod executor;
mod process;
mod prompt;
mod settings;
mod status;
mod version;

pub use executor::ClaudeCliExecutor;
pub use settings::{Entry, default_work_root};
pub use status::{AuthStatus, StatusError, auth_status};
pub use version::{MIN_VERSION, VersionCheck, check_version, warn_outdated};
