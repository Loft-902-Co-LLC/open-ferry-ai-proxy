// Ported from Go's encoding/base64 (go1.27, BSD-3-Clause, see licenses/Go-LICENSE).
// https://go.dev/src/encoding/base64/base64.go

//! Go's base64 decoder.
//!
//! Signature validation depends on details that Rust base64 crates handle
//! differently. Go skips `\r` and `\n` anywhere in the input, accepts non-zero
//! trailing bits (outside [`Encoding::strict`] mode), and reports where
//! decoding failed.

use std::fmt;

/// A base64 variant: the alphabet, whether output is padded with `=`, and
/// whether non-zero trailing bits are rejected.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Encoding {
    url_safe: bool,
    padded: bool,
    strict: bool,
}

/// `base64.StdEncoding`.
pub(crate) const STD: Encoding = Encoding {
    url_safe: false,
    padded: true,
    strict: false,
};
/// `base64.RawStdEncoding`.
pub(crate) const RAW_STD: Encoding = Encoding {
    url_safe: false,
    padded: false,
    strict: false,
};
/// `base64.URLEncoding`.
pub(crate) const URL: Encoding = Encoding {
    url_safe: true,
    padded: true,
    strict: false,
};
/// `base64.RawURLEncoding`.
pub(crate) const RAW_URL: Encoding = Encoding {
    url_safe: true,
    padded: false,
    strict: false,
};

/// Go's `base64.CorruptInputError`: the input byte offset where decoding failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CorruptInputError(pub(crate) usize);

impl fmt::Display for CorruptInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "illegal base64 data at input byte {}", self.0)
    }
}

impl Encoding {
    /// `Strict`: the same encoding, rejecting non-zero trailing bits. Newlines
    /// are still skipped.
    pub(crate) const fn strict(self) -> Self {
        Self {
            strict: true,
            ..self
        }
    }

    /// `DecodeString`.
    pub(crate) fn decode(self, src: impl AsRef<[u8]>) -> Result<Vec<u8>, CorruptInputError> {
        let src = src.as_ref();
        let mut out = Vec::with_capacity(src.len() / 4 * 3 + 3);
        let mut si = 0;
        while si < src.len() {
            si = self.decode_quantum(src, si, &mut out)?;
        }
        Ok(out)
    }

    fn value(self, c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' if !self.url_safe => Some(62),
            b'/' if !self.url_safe => Some(63),
            b'-' if self.url_safe => Some(62),
            b'_' if self.url_safe => Some(63),
            _ => None,
        }
    }

    /// Decodes up to four characters starting at `si`, skipping newlines, and
    /// returns where the next quantum starts.
    fn decode_quantum(
        self,
        src: &[u8],
        mut si: usize,
        out: &mut Vec<u8>,
    ) -> Result<usize, CorruptInputError> {
        let is_newline = |c: u8| c == b'\n' || c == b'\r';
        let mut dbuf = [0u8; 4];
        let mut len = 0;
        let mut trailing_garbage = None;
        while len < dbuf.len() {
            if si == src.len() {
                if len == 0 {
                    return Ok(si);
                }
                if len == 1 || self.padded {
                    return Err(CorruptInputError(si - len));
                }
                break;
            }
            let c = src[si];
            si += 1;
            if let Some(value) = self.value(c) {
                dbuf[len] = value;
                len += 1;
                continue;
            }
            if is_newline(c) {
                continue;
            }
            if !self.padded || c != b'=' {
                return Err(CorruptInputError(si - 1));
            }

            // Padding ends the input.
            match len {
                0 | 1 => return Err(CorruptInputError(si - 1)),
                2 => {
                    // A second `=` must follow.
                    while si < src.len() && is_newline(src[si]) {
                        si += 1;
                    }
                    if si == src.len() {
                        return Err(CorruptInputError(src.len()));
                    }
                    if src[si] != b'=' {
                        return Err(CorruptInputError(si - 1));
                    }
                    si += 1;
                }
                _ => {}
            }
            while si < src.len() && is_newline(src[si]) {
                si += 1;
            }
            if si < src.len() {
                trailing_garbage = Some(CorruptInputError(si));
            }
            break;
        }

        let value = u32::from(dbuf[0]) << 18
            | u32::from(dbuf[1]) << 12
            | u32::from(dbuf[2]) << 6
            | u32::from(dbuf[3]);
        let bytes = [(value >> 16) as u8, (value >> 8) as u8, value as u8];
        if self.strict {
            // The bits after the last whole byte must be zero. This check comes
            // before any trailing-garbage error, as in Go.
            match len {
                3 if bytes[2] != 0 => return Err(CorruptInputError(si.saturating_sub(1))),
                2 if bytes[1] != 0 || bytes[2] != 0 => {
                    return Err(CorruptInputError(si.saturating_sub(2)));
                }
                _ => {}
            }
        }
        out.extend_from_slice(&bytes[..len - 1]);
        match trailing_garbage {
            Some(error) => Err(error),
            None => Ok(si),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_like_go() {
        assert_eq!(STD.decode("aGVsbG8="), Ok(b"hello".to_vec()));
        assert_eq!(STD.decode("aGVs\r\nbG8="), Ok(b"hello".to_vec()));
        assert_eq!(STD.decode("aGVsbG8"), Err(CorruptInputError(4)));
        assert_eq!(RAW_STD.decode("aGVsbG8"), Ok(b"hello".to_vec()));
        assert_eq!(RAW_STD.decode("aGVsbG8="), Err(CorruptInputError(7)));
        // Non-zero trailing bits are accepted.
        assert_eq!(STD.decode("aGVsbG9="), Ok(b"hello".to_vec()));
        assert_eq!(STD.decode("aGVsbA=="), Ok(b"hell".to_vec()));
        assert_eq!(STD.decode("aGVsbA=\n="), Ok(b"hell".to_vec()));
        assert_eq!(STD.decode("aGVsbA="), Err(CorruptInputError(7)));
        assert_eq!(STD.decode("aGVsbA=x"), Err(CorruptInputError(6)));
        assert_eq!(STD.decode("aGVsbA==aGVs"), Err(CorruptInputError(8)));
        assert_eq!(STD.decode("a==="), Err(CorruptInputError(1)));
        assert_eq!(STD.decode("aGV-"), Err(CorruptInputError(3)));
        assert_eq!(URL.decode("-_-_"), Ok(vec![0xfb, 0xff, 0xbf]));
        assert_eq!(RAW_URL.decode("a"), Err(CorruptInputError(0)));
        assert_eq!(STD.decode(""), Ok(Vec::new()));
        assert_eq!(STD.decode("\n\n"), Ok(Vec::new()));
    }

    // Not upstream's: checks Strict() against Go's answers.
    #[test]
    fn strict_decodes_like_go() {
        let strict = STD.strict();
        assert_eq!(strict.decode("aGVsbG8="), Ok(b"hello".to_vec()));
        assert_eq!(strict.decode("aGVsbG9="), Err(CorruptInputError(7)));
        assert_eq!(strict.decode("aGVsbA=="), Ok(b"hell".to_vec()));
        assert_eq!(strict.decode("aGVsbB=="), Err(CorruptInputError(6)));
        assert_eq!(strict.decode("aGVs\r\nbG8="), Ok(b"hello".to_vec()));
        assert_eq!(strict.decode("aGVsbG9=x"), Err(CorruptInputError(7)));
        assert_eq!(strict.decode("aGVsbG8=x"), Err(CorruptInputError(8)));
        assert_eq!(strict.decode("aGVsbB=\n="), Err(CorruptInputError(7)));
        assert_eq!(strict.decode("aGVsbB==\n"), Err(CorruptInputError(7)));
        let raw = RAW_STD.strict();
        assert_eq!(raw.decode("aGVsbG8"), Ok(b"hello".to_vec()));
        assert_eq!(raw.decode("aGVsbG9"), Err(CorruptInputError(6)));
        assert_eq!(raw.decode("aGVsbB"), Err(CorruptInputError(4)));
    }
}
