//! open-ferry's own updates: finding the latest signed release, checking
//! it, staging it, and switching the installed binary to it.
//!
//! Upstream updates only its management panel's web page; this crate is
//! open-ferry's, and nothing in it is a port. See `docs/updates.md`.
//!
//! [`keys`] holds the release keys built in from `release-keys.pub` and
//! checks a minisign signature of `SHA256SUMS`; [`release`] reads the
//! release for this target from the list, as `install.sh` does;
//! [`fetch`] downloads over HTTPS only, through the config's proxy;
//! [`archive`] takes the binary out of a release archive, refusing unsafe
//! entries. [`data_dir`] is where versions, the state ([`state`]), the
//! installers' receipt ([`receipt`]) and the lock live; [`install`] tells
//! whether this install replaces its own binary; [`settings`] reads the
//! `self-update` section and the environment. [`updater`] puts these
//! together: a check, staging, a switch ([`switch`]) and a rollback, each
//! staged binary first run with `--version` ([`runner`]). [`background`]
//! is the server's periodic check.
//!
//! Deviations from upstream: all of it is open-ferry's own.

pub mod archive;
pub mod background;
pub mod data_dir;
pub mod fetch;
pub mod install;
pub mod keys;
pub mod receipt;
pub mod release;
pub mod runner;
pub mod settings;
pub mod state;
pub mod switch;
pub mod updater;

#[cfg(test)]
mod tests;

pub use background::{CheckNow, UpdateService};
pub use data_dir::DataDir;
pub use fetch::{Fetch, FetchError, HttpFetch};
pub use install::{Install, NotSelfUpdating};
pub use keys::{ReleaseKeys, VerifyError};
pub use open_ferry_core::config::{SelfUpdate, SelfUpdateMode};
pub use settings::{ModeSource, Settings};
pub use state::State;
pub use switch::{ReplaceOnDisk, Switch, SwitchPlan};
pub use updater::{CheckResult, Report, Status, UpdateError, Updater};

/// The version of this binary.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The target triple this binary was built for, whose archive it updates
/// from.
pub const TARGET: &str = env!("OPEN_FERRY_TARGET");

/// Where releases are downloaded from: `<base>/latest/download/SHA256SUMS`
/// and `<base>/download/v<version>/<archive>`.
pub const DEFAULT_BASE_URL: &str =
    "https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases";

/// The environment variable that replaces [`DEFAULT_BASE_URL`], as
/// `OPEN_FERRY_INSTALL_BASE_URL` does for the installers.
pub const BASE_URL_ENV: &str = "OPEN_FERRY_UPDATE_BASE_URL";

/// The environment variable that can lower the `self-update` mode, never
/// raise it.
pub const MODE_ENV: &str = "OPEN_FERRY_SELF_UPDATE";

/// The User-Agent of every update request.
pub const USER_AGENT: &str = concat!("open-ferry/", env!("CARGO_PKG_VERSION"));
