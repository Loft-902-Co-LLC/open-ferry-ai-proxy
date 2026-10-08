//! The install receipt: `install-receipt.json` in the data directory,
//! written by `install.sh` and `install.ps1` when they install a binary.
//!
//! ```json
//! {"format":1,"installer":"install.sh","version":"0.1.0",
//!  "binary":"/home/me/.local/bin/open-ferry",
//!  "target":"x86_64-unknown-linux-gnu","installed_at":"2026-10-08T12:00:00Z"}
//! ```
//!
//! `installer` is `install.sh` or `install.ps1`; `binary` is the absolute
//! path the installer wrote; `installed_at` is UTC, RFC 3339. Updates
//! replace a binary only when the receipt names the running one (see
//! [`crate::install`]); they never write the receipt.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The most bytes a receipt may have.
const MAX_SIZE: u64 = 16 * 1024;

/// What an installer recorded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// The receipt's format: 1.
    pub format: u32,
    /// `install.sh` or `install.ps1`.
    pub installer: String,
    /// The version installed.
    pub version: String,
    /// The installed binary's absolute path.
    pub binary: String,
    /// The target triple installed.
    pub target: String,
    /// When, RFC 3339.
    pub installed_at: String,
}

/// Why a receipt couldn't be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptError {
    /// Reading it failed.
    Unreadable(String),
    /// It isn't a receipt.
    Malformed(String),
}

impl std::fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(message) => write!(f, "the install receipt can't be read: {message}"),
            Self::Malformed(message) => write!(f, "the install receipt isn't valid: {message}"),
        }
    }
}

impl std::error::Error for ReceiptError {}

impl Receipt {
    /// The receipt at `path`: `Ok(None)` when there is none.
    pub fn load(path: &Path) -> Result<Option<Self>, ReceiptError> {
        let size = match fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ReceiptError::Unreadable(error.to_string())),
        };
        if size > MAX_SIZE {
            return Err(ReceiptError::Malformed(format!(
                "it is over {MAX_SIZE} bytes"
            )));
        }
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ReceiptError::Unreadable(error.to_string())),
        };
        let receipt: Self = serde_json::from_slice(&data)
            .map_err(|error| ReceiptError::Malformed(error.to_string()))?;
        if receipt.format != 1 {
            return Err(ReceiptError::Malformed(format!(
                "format {} isn't 1",
                receipt.format
            )));
        }
        if receipt.binary.is_empty() {
            return Err(ReceiptError::Malformed("it names no binary".into()));
        }
        Ok(Some(receipt))
    }
}
