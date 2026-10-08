// Ported from gopkg.in/yaml.v3 v3.0.1 yamlprivateh.go (the buffer sizes and
// the character classes: is_alpha, is_digit, as_digit, is_hex, as_hex,
// is_ascii, is_printable, is_z, is_bom, is_space, is_tab, is_blank, is_break,
// is_crlf, is_breakz, is_blankz, width) (MIT, from libyaml), the
// YAML library CLIProxyAPI v8.0.20 (MIT) reads and writes its config with.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml
//
// Copyright (c) 2006-2010 Kirill Simonov
// Copyright (c) 2006-2011 Kirill Simonov
// Copyright (c) 2011-2019 Canonical Ltd
// Licensed under the MIT License; see licenses/go-yaml-LICENSE.

//! Character classes over a byte buffer, as the scanner and emitter test
//! the bytes at a position.
//!
//! Deviations from upstream:
//! - A position past the end of the buffer reads as a NUL byte where
//!   yaml.v3 would index out of range. The scanner keeps its buffer padded
//!   with NULs at the end of the input, so it never reads past it there.

/// The size of the input raw buffer (`input_raw_buffer_size`).
pub(crate) const INPUT_RAW_BUFFER_SIZE: usize = 512;
/// The size of the input buffer (`input_buffer_size`); it holds what the
/// raw buffer decodes to, at most three bytes per raw byte.
pub(crate) const INPUT_BUFFER_SIZE: usize = INPUT_RAW_BUFFER_SIZE * 3;
/// The size of the output buffer (`output_buffer_size`).
pub(crate) const OUTPUT_BUFFER_SIZE: usize = 128;

/// The byte at `i`, or NUL past the end.
pub(crate) fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

/// Whether the byte is an ASCII letter, a digit, `_` or `-`.
pub(crate) fn is_alpha(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// Whether the byte is an ASCII digit.
pub(crate) fn is_digit(b: &[u8], i: usize) -> bool {
    at(b, i).is_ascii_digit()
}

/// The value of an ASCII digit.
pub(crate) fn as_digit(b: &[u8], i: usize) -> i32 {
    i32::from(at(b, i)) - i32::from(b'0')
}

/// Whether the byte is a hex digit.
pub(crate) fn is_hex(b: &[u8], i: usize) -> bool {
    at(b, i).is_ascii_hexdigit()
}

/// The value of a hex digit.
pub(crate) fn as_hex(b: &[u8], i: usize) -> i32 {
    let c = at(b, i);
    if c.is_ascii_uppercase() && c <= b'F' {
        return i32::from(c) - i32::from(b'A') + 10;
    }
    if c.is_ascii_lowercase() && c <= b'f' {
        return i32::from(c) - i32::from(b'a') + 10;
    }
    i32::from(c) - i32::from(b'0')
}

/// Whether the byte is ASCII.
pub(crate) fn is_ascii(b: &[u8], i: usize) -> bool {
    at(b, i) <= 0x7F
}

/// Whether the character at `i` may appear unescaped in YAML.
pub(crate) fn is_printable(b: &[u8], i: usize) -> bool {
    let (c0, c1, c2) = (at(b, i), at(b, i + 1), at(b, i + 2));
    c0 == 0x0A // . == #x0A
        || (0x20..=0x7E).contains(&c0) // #x20 <= . <= #x7E
        || (c0 == 0xC2 && c1 >= 0xA0) // #0xA0 <= . <= #xD7FF
        || (c0 > 0xC2 && c0 < 0xED)
        || (c0 == 0xED && c1 < 0xA0)
        || c0 == 0xEE
        || (c0 == 0xEF // #xE000 <= . <= #xFFFD
            && !(c1 == 0xBB && c2 == 0xBF) // && . != #xFEFF
            && !(c1 == 0xBF && (c2 == 0xBE || c2 == 0xBF)))
}

/// Whether the byte is NUL.
pub(crate) fn is_z(b: &[u8], i: usize) -> bool {
    at(b, i) == 0x00
}

/// Whether the buffer *starts* with a UTF-8 byte order mark, whatever the
/// position `_i`, as yaml.v3's `is_bom` tests it. Its callers pass the
/// current position, so a U+FEFF at the start of the buffer makes the
/// scanner skip the first character of a later line and the emitter escape
/// every character of a double-quoted scalar that starts with one.
pub(crate) fn is_bom(b: &[u8], _i: usize) -> bool {
    at(b, 0) == 0xEF && at(b, 1) == 0xBB && at(b, 2) == 0xBF
}

/// Whether the byte is a space.
pub(crate) fn is_space(b: &[u8], i: usize) -> bool {
    at(b, i) == b' '
}

/// Whether the byte is a tab.
pub(crate) fn is_tab(b: &[u8], i: usize) -> bool {
    at(b, i) == b'\t'
}

/// Whether the byte is a space or a tab.
pub(crate) fn is_blank(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c == b' ' || c == b'\t'
}

/// Whether a line break starts at `i`: CR, LF, NEL, LS or PS.
pub(crate) fn is_break(b: &[u8], i: usize) -> bool {
    let (c0, c1, c2) = (at(b, i), at(b, i + 1), at(b, i + 2));
    c0 == b'\r' // CR (#xD)
        || c0 == b'\n' // LF (#xA)
        || (c0 == 0xC2 && c1 == 0x85) // NEL (#x85)
        || (c0 == 0xE2 && c1 == 0x80 && c2 == 0xA8) // LS (#x2028)
        || (c0 == 0xE2 && c1 == 0x80 && c2 == 0xA9) // PS (#x2029)
}

/// Whether CR LF starts at `i`.
pub(crate) fn is_crlf(b: &[u8], i: usize) -> bool {
    at(b, i) == b'\r' && at(b, i + 1) == b'\n'
}

/// Whether a line break or NUL is at `i`.
pub(crate) fn is_breakz(b: &[u8], i: usize) -> bool {
    is_break(b, i) || at(b, i) == 0
}

/// Whether a space, a tab, a line break or NUL is at `i`.
pub(crate) fn is_blankz(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c == b' ' || c == b'\t' || is_breakz(b, i)
}

/// The width of the UTF-8 character that starts with `b`, or 0 if `b`
/// can't start one.
pub(crate) fn width(b: u8) -> usize {
    if b & 0x80 == 0x00 {
        return 1;
    }
    if b & 0xE0 == 0xC0 {
        return 2;
    }
    if b & 0xF0 == 0xE0 {
        return 3;
    }
    if b & 0xF8 == 0xF0 {
        return 4;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: classes read past the end as NUL instead of
    // panicking.
    #[test]
    fn past_the_end_reads_as_nul() {
        let b = b"a";
        assert!(is_z(b, 1));
        assert!(is_blankz(b, 5));
        assert!(!is_break(b, 1));
        assert!(is_breakz(b, 1));
    }

    // Not upstream's: the multi-byte breaks and the printable ranges.
    #[test]
    fn multi_byte_classes() {
        assert!(is_break("\u{85}".as_bytes(), 0));
        assert!(is_break("\u{2028}".as_bytes(), 0));
        assert!(is_break("\u{2029}".as_bytes(), 0));
        assert!(!is_printable("\u{FEFF}".as_bytes(), 0));
        assert!(!is_printable("\u{FFFE}".as_bytes(), 0));
        assert!(is_printable("\u{E9}".as_bytes(), 0));
        assert!(!is_printable("\u{9F}".as_bytes(), 0));
        assert!(is_bom("\u{FEFF}x".as_bytes(), 0));
        assert!(is_bom("\u{FEFF}x".as_bytes(), 3));
        assert!(!is_bom("x\u{FEFF}".as_bytes(), 1));
        assert_eq!(width("\u{1F600}".as_bytes()[0]), 4);
        assert_eq!(as_hex(b"F", 0), 15);
        assert_eq!(as_hex(b"a", 0), 10);
        assert_eq!(as_digit(b"7", 0), 7);
    }
}
