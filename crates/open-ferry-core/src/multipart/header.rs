// Ported from Go's net/textproto/header.go (MIMEHeader) and reader.go
// (CanonicalMIMEHeaderKey, canonicalMIMEHeaderKey, validHeaderFieldByte)
// (go1.26, BSD-3-Clause, see licenses/Go-LICENSE).
// https://github.com/golang/go

//! A part's headers (Go's `textproto.MIMEHeader`).
//!
//! Deviations from Go: names are kept in name order, which is the order
//! Go's writer sorts them into.

use std::collections::BTreeMap;
use std::fmt;

use super::{ByteCount, lossy};

/// A part's headers by canonical name, each with its values in order.
/// Its `Debug` shows names and the size of each value.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Header(BTreeMap<String, Vec<Vec<u8>>>);

impl Header {
    /// No headers.
    pub fn new() -> Self {
        Self::default()
    }

    /// The first value of `name` (Go's `Get`).
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.0
            .get(&canonical_key(name))
            .and_then(|values| values.first())
            .map(Vec::as_slice)
    }

    /// The first value of `name` as text, empty when there is none, a byte
    /// that isn't part of a valid character read as U+FFFD.
    pub fn get_str(&self, name: &str) -> String {
        self.get(name).map(lossy).unwrap_or_default()
    }

    /// Sets `name` to `value` alone (Go's `Set`).
    pub fn set(&mut self, name: &str, value: impl Into<Vec<u8>>) {
        self.0.insert(canonical_key(name), vec![value.into()]);
    }

    /// Removes `name` (Go's `Del`).
    pub fn remove(&mut self, name: &str) {
        self.0.remove(&canonical_key(name));
    }

    /// Adds `value` under the name `key`, taken as it is.
    pub(crate) fn append_raw(&mut self, key: String, value: Vec<u8>) {
        self.0.entry(key).or_default().push(value);
    }

    /// Whether `key`, taken as it is, has a value.
    pub(crate) fn has_raw(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// Each name with its values, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[Vec<u8>])> {
        self.0
            .iter()
            .map(|(name, values)| (name.as_str(), values.as_slice()))
    }

    /// Whether there are no headers.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(name, values)| {
                let sizes: Vec<ByteCount> =
                    values.iter().map(|value| ByteCount(value.len())).collect();
                (name, sizes)
            }))
            .finish()
    }
}

/// Go's `textproto.CanonicalMIMEHeaderKey`: the first letter and each
/// letter after a `-` upper case, the others lower case. A name holding a
/// byte that can't be in a header name, a space or a non-ASCII byte
/// included, is returned as it is.
pub fn canonical_key(name: &str) -> String {
    if !name.bytes().all(is_field_byte) {
        return name.to_owned();
    }
    canonicalize(name.as_bytes())
}

/// `name`, all of whose bytes are header name bytes, canonicalized.
fn canonicalize(name: &[u8]) -> String {
    let mut upper = true;
    let mut out = String::with_capacity(name.len());
    for &b in name {
        let c = if upper {
            b.to_ascii_uppercase()
        } else {
            b.to_ascii_lowercase()
        };
        out.push(char::from(c));
        upper = c == b'-';
    }
    out
}

/// Go's `canonicalMIMEHeaderKey` for a name read from a header line:
/// `None` when it is empty or holds a byte that can't be in a name, except
/// that a name with a space is taken as it is.
pub(crate) fn read_key(name: &[u8]) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let mut keep = false;
    for &b in name {
        if is_field_byte(b) {
            continue;
        }
        if b == b' ' {
            keep = true;
            continue;
        }
        return None;
    }
    if keep {
        // Only ASCII bytes are left, so this is the name as it is.
        return Some(lossy(name));
    }
    Some(canonicalize(name))
}

/// Go's `validHeaderFieldByte`: a token byte of RFC 7230.
pub(crate) fn is_field_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Go's `validHeaderValueByte`: anything but a control byte other than a
/// tab.
pub(crate) fn is_value_byte(b: u8) -> bool {
    b == b'\t' || (b >= 0x20 && b != 0x7f)
}
