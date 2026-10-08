//! The release keys a build trusts, and checking a release's signature with
//! them.
//!
//! The keys are minisign public keys, built in from `release-keys.pub` at
//! the repository's root: at most [`MAX_KEYS`], the current one and, while
//! a rotation is under way, the next. A build from a file with no key
//! trusts no release, and refuses to update itself.
//!
//! A signature is checked with `minisign-verify`: its key ID picks the key,
//! it must be of the prehashed kind minisign makes (the legacy kind is
//! refused), and its trusted comment is covered by the signature, so the
//! comment can be relied on once the signature checks out.

use std::fmt;

use minisign_verify::{Error as MinisignError, PublicKey, Signature};

/// The most keys a build trusts.
pub const MAX_KEYS: usize = 2;

/// `release-keys.pub`, as it was when this crate was built.
const BUILT_IN: &str = include_str!("../../../release-keys.pub");

/// The minisign public keys a build trusts for releases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseKeys {
    keys: Vec<PublicKey>,
}

/// Why a keys file didn't parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// The line (from 1) isn't a minisign public key.
    NotAKey { line: usize },
    /// The line (from 1) repeats a key.
    Repeated { line: usize },
    /// The file has more than [`MAX_KEYS`] keys.
    TooMany { count: usize },
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAKey { line } => {
                write!(f, "release keys: line {line} isn't a minisign public key")
            }
            Self::Repeated { line } => write!(f, "release keys: line {line} repeats a key"),
            Self::TooMany { count } => write!(
                f,
                "release keys: {count} keys, more than the {MAX_KEYS} a build may trust"
            ),
        }
    }
}

impl std::error::Error for KeyError {}

/// Why a signature wasn't accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// The build has no key to check with.
    NoTrustedKey,
    /// The signature file isn't a minisign signature.
    Malformed,
    /// The signature is by a key the build doesn't trust.
    UnknownKey,
    /// The signature is of minisign's legacy kind, which isn't accepted.
    Legacy,
    /// The data or the trusted comment doesn't match the signature.
    BadSignature,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoTrustedKey => {
                "this build of open-ferry trusts no release key, so it can't check a release's \
                 signature and won't update itself; install the update by hand (see docs/updates.md)"
            }
            Self::Malformed => "SHA256SUMS.minisig isn't a minisign signature",
            Self::UnknownKey => "SHA256SUMS is signed with a key this build doesn't trust",
            Self::Legacy => "SHA256SUMS.minisig is a legacy minisign signature, which isn't accepted",
            Self::BadSignature => "SHA256SUMS doesn't match its signature",
        })
    }
}

impl std::error::Error for VerifyError {}

impl ReleaseKeys {
    /// The keys built into this binary.
    pub fn built_in() -> Result<Self, KeyError> {
        Self::parse(BUILT_IN)
    }

    /// The keys of a keys file's text.
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        let mut keys: Vec<PublicKey> = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("untrusted comment:") {
                continue;
            }
            let number = index + 1;
            let key =
                PublicKey::from_base64(line).map_err(|_| KeyError::NotAKey { line: number })?;
            if keys.contains(&key) {
                return Err(KeyError::Repeated { line: number });
            }
            keys.push(key);
        }
        if keys.len() > MAX_KEYS {
            return Err(KeyError::TooMany { count: keys.len() });
        }
        Ok(Self { keys })
    }

    /// No keys at all.
    pub fn none() -> Self {
        Self { keys: Vec::new() }
    }

    /// Whether there is no key to trust.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// How many keys there are.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Checks `signature`, the text of a `.minisig` file, over `data`, and
    /// gives its trusted comment.
    pub fn verify(&self, data: &[u8], signature: &str) -> Result<String, VerifyError> {
        if self.keys.is_empty() {
            return Err(VerifyError::NoTrustedKey);
        }
        let signature = Signature::decode(signature).map_err(|_| VerifyError::Malformed)?;
        for key in &self.keys {
            match key.verify(data, &signature, false) {
                Ok(()) => return Ok(signature.trusted_comment().to_owned()),
                // Signed by another key: try the next.
                Err(MinisignError::UnexpectedKeyId) => {}
                Err(MinisignError::UnexpectedAlgorithm) => return Err(VerifyError::Legacy),
                Err(_) => return Err(VerifyError::BadSignature),
            }
        }
        Err(VerifyError::UnknownKey)
    }
}
