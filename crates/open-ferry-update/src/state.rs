//! `update-state.json`: what the last check found and what updates did.
//!
//! The file is JSON, written whole to a temporary file and renamed over the
//! old, so a reader never sees half of it. One that is missing or doesn't
//! parse reads as empty: the state is only a record, and the next check
//! writes it again. It holds no secrets: versions, times, hashes and
//! messages.
//!
//! ```json
//! {
//!   "format": 1,
//!   "last_check": "2026-10-08T12:00:00Z",
//!   "last_result": "staged",
//!   "last_error": null,
//!   "latest": "0.2.0",
//!   "notified": "0.2.0",
//!   "staged": "0.2.0",
//!   "staged_sha256": "<64 hex digits of the staged binary>",
//!   "previous": "0.1.0",
//!   "failed": ["0.1.5"],
//!   "rolled_back": null,
//!   "last_switch": {
//!     "from": "0.1.0", "to": "0.2.0", "at": "2026-10-08T12:05:00Z",
//!     "how": "update", "binary": "/home/me/.local/bin/open-ferry",
//!     "sha256": "<64 hex digits>", "size": 31457280,
//!     "modified_ms": 1791460800000, "restart_needed": true
//!   }
//! }
//! ```

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The state file's format.
pub const FORMAT: u32 = 1;

/// How many failed versions are remembered.
const MAX_FAILED: usize = 10;

/// What updates know between runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// [`FORMAT`].
    pub format: u32,
    /// When the last check ended, RFC 3339.
    pub last_check: Option<String>,
    /// How it ended: `up-to-date`, `update-available`, `staged`,
    /// `skipped`, `cannot-update` or `error`.
    pub last_result: Option<String>,
    /// Its error, when it failed.
    pub last_error: Option<String>,
    /// The latest release's version, when one was found.
    pub latest: Option<String>,
    /// The last version a check reported, so each is reported once.
    pub notified: Option<String>,
    /// The version staged for the next switch.
    pub staged: Option<String>,
    /// The staged binary's SHA-256, checked again before a switch.
    pub staged_sha256: Option<String>,
    /// The version before the last switch, kept for a rollback.
    pub previous: Option<String>,
    /// Versions whose binary failed its `--version` run; skipped until a
    /// newer release.
    pub failed: Vec<String>,
    /// A version rolled back from; automatic updates skip it until a newer
    /// release, a manual one may go to it again.
    pub rolled_back: Option<String>,
    /// The last switch.
    pub last_switch: Option<SwitchRecord>,
}

/// A switch from one binary to another.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SwitchRecord {
    /// The version switched from.
    pub from: String,
    /// The version switched to.
    pub to: String,
    /// When, RFC 3339.
    pub at: String,
    /// `update` or `rollback`.
    pub how: String,
    /// The binary replaced.
    pub binary: String,
    /// The SHA-256 of the binary put in its place.
    pub sha256: String,
    /// That binary's size, and its modification time in milliseconds
    /// since the epoch, to tell whether it is still the one installed.
    pub size: u64,
    pub modified_ms: i64,
    /// Whether open-ferry must be restarted to run it.
    pub restart_needed: bool,
}

impl State {
    /// The state in `path`; empty when there is none or it doesn't parse.
    pub fn load(path: &Path) -> Self {
        match fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|error| {
                tracing::warn!(
                    "update state {} doesn't parse, so it starts over: {error}",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Writes the state to `path`, whole.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut state = self.clone();
        state.format = FORMAT;
        let mut data = serde_json::to_vec_pretty(&state).map_err(io::Error::other)?;
        data.push(b'\n');
        write_atomically(path, &data)
    }

    /// Whether `version` failed its check and is skipped.
    pub fn has_failed(&self, version: &str) -> bool {
        self.failed.iter().any(|failed| failed == version)
    }

    /// Remembers that `version` failed its check.
    pub fn mark_failed(&mut self, version: &str) {
        if !self.has_failed(version) {
            self.failed.push(version.to_owned());
        }
        let extra = self.failed.len().saturating_sub(MAX_FAILED);
        self.failed.drain(..extra);
    }
}

/// Writes `data` to `path` through a temporary file beside it.
pub fn write_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&temp, data)?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}
