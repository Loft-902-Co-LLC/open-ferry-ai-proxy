// Ported from Go's strings (EqualFold, ToUpper), strconv (ParseBool, Atoi),
// unicode/utf8 (DecodeRune), net/textproto (CanonicalMIMEHeaderKey,
// validHeaderFieldByte) and time (Duration.String, Time.IsZero) (go1.27,
// BSD-3-Clause), as CLIProxyAPI internal/api/handlers/management uses them
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! Go standard library behaviour the management API depends on:
//! `strings.EqualFold` and `ToUpper`, `strconv.ParseBool` and `Atoi`,
//! `utf8.DecodeRune`, `textproto.CanonicalMIMEHeaderKey` and
//! `time.Duration.String`.

use std::fmt::Write as _;

use chrono::{TimeZone, Utc};
use open_ferry_core::auth::Timestamp;
pub(crate) use open_ferry_translate::go::to_upper;

/// Go's `strings.EqualFold`: whether `a` and `b` are equal under simple
/// Unicode case folding.
pub(crate) fn equal_fold(a: &str, b: &str) -> bool {
    let mut left = a.chars();
    let mut right = b.chars();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if fold_eq(x, y) => {}
            _ => return false,
        }
    }
}

/// Whether two characters are in the same simple case-folding orbit.
fn fold_eq(a: char, b: char) -> bool {
    if a == b {
        return true;
    }
    // The dotted and dotless Turkish i fold only to themselves in Go.
    if matches!(a, '\u{130}' | '\u{131}') || matches!(b, '\u{130}' | '\u{131}') {
        return false;
    }
    simple_lower(a) == simple_lower(b) || simple_upper(a) == simple_upper(b)
}

/// The lowercase of `c` when it is one character, else `c`.
fn simple_lower(c: char) -> char {
    let mut mapped = c.to_lowercase();
    match (mapped.next(), mapped.next()) {
        (Some(lower), None) => lower,
        _ => c,
    }
}

/// The uppercase of `c` when it is one character, else `c`.
fn simple_upper(c: char) -> char {
    let mut mapped = c.to_uppercase();
    match (mapped.next(), mapped.next()) {
        (Some(upper), None) => upper,
        _ => c,
    }
}

/// Go's `strconv.ParseBool`.
pub(crate) fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Go's `strconv.Atoi` on a 64-bit platform: an optional sign and decimal
/// digits, nothing else.
pub(crate) fn atoi(s: &str) -> Option<i64> {
    s.parse().ok()
}

/// Go's `utf8.DecodeRune`: the character at the start of `bytes` and its
/// length, or `None` where Go reads `RuneError` from one byte (a byte that
/// doesn't start a valid encoding).
pub(crate) fn decode_rune(bytes: &[u8]) -> Option<(char, usize)> {
    let width = match *bytes.first()? {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let encoded = bytes.get(..width)?;
    let text = std::str::from_utf8(encoded).ok()?;
    text.chars().next().map(|c| (c, width))
}

/// `bytes` as text, each byte that isn't part of a valid character read as
/// U+FFFD, as Go's `range` over a string and its JSON decoder read them.
/// Rust's `from_utf8_lossy` can read a broken sequence as one U+FFFD.
pub(crate) fn lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for _ in chunk.invalid() {
            out.push(char::REPLACEMENT_CHARACTER);
        }
    }
    out
}

/// Go's `textproto.CanonicalMIMEHeaderKey`: the first letter and each
/// letter after a hyphen upper case, the rest lower case. A name holding a
/// byte that can't be in a header name, a space or a non-ASCII byte
/// included, is returned as it is.
pub(crate) fn canonical_header_key(name: &str) -> String {
    if !name.bytes().all(is_token_byte) {
        return name.to_owned();
    }
    let mut upper = true;
    name.chars()
        .map(|c| {
            let mapped = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            mapped
        })
        .collect()
}

/// Whether `b` may be in a header name (Go's `validHeaderFieldByte`).
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Go's `time.Duration.String` for a whole number of seconds: `0s`, `59s`,
/// `29m59s`, `1h0m0s`.
pub(crate) fn duration_string(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let secs = seconds % 60;
    let mut out = String::new();
    if hours > 0 {
        let _ = write!(out, "{hours}h{minutes}m");
    } else if minutes > 0 {
        let _ = write!(out, "{minutes}m");
    }
    let _ = write!(out, "{secs}s");
    out
}

/// Go's zero `time.Time`, January 1 of year 1, which upstream's records use
/// for "never".
fn zero_time() -> Timestamp {
    Utc.with_ymd_and_hms(1, 1, 1, 0, 0, 0)
        .single()
        .unwrap_or_default()
}

/// Go's `Time.IsZero` for an optional time: unset, or Go's zero time.
pub(crate) fn is_zero(time: Option<Timestamp>) -> bool {
    time.is_none_or(|time| time == zero_time())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_fold_matches_go() {
        assert!(equal_fold("API_KEY", "api_key"));
        assert!(equal_fold("Host", "hOST"));
        assert!(equal_fold("\u{212a}", "k"));
        assert!(equal_fold("\u{17f}", "S"));
        assert!(!equal_fold("\u{131}", "i"));
        assert!(!equal_fold("\u{130}", "I"));
        assert!(!equal_fold("hosts", "host"));
    }

    #[test]
    fn to_upper_uses_simple_mappings() {
        assert_eq!(to_upper("post"), "POST");
        // The long s and the dotless i upper-case to ASCII, as in Go.
        assert_eq!(to_upper("po\u{17f}t"), "POST");
        assert_eq!(to_upper("\u{131}"), "I");
        // Sharp s has no one-character upper case, so it stays.
        assert_eq!(to_upper("\u{df}"), "\u{df}");
    }

    #[test]
    fn parse_bool_and_atoi_match_go() {
        assert_eq!(parse_bool("True"), Some(true));
        assert_eq!(parse_bool("f"), Some(false));
        assert_eq!(parse_bool("yes"), None);
        assert_eq!(atoi("+5"), Some(5));
        assert_eq!(atoi("007"), Some(7));
        assert_eq!(atoi("-1"), Some(-1));
        assert_eq!(atoi(" 1"), None);
        assert_eq!(atoi("1_0"), None);
        assert_eq!(atoi("9223372036854775808"), None);
    }

    #[test]
    fn decode_rune_reads_one_byte_of_a_broken_sequence() {
        assert_eq!(decode_rune(b"a"), Some(('a', 1)));
        assert_eq!(decode_rune("\u{e9}x".as_bytes()), Some(('\u{e9}', 2)));
        assert_eq!(decode_rune(b"\xe2\x82"), None);
        assert_eq!(decode_rune(b"\xed\xa0\x80"), None);
        assert_eq!(decode_rune(b"\xc0\x80"), None);
        assert_eq!(decode_rune(b""), None);
        // A truncated three-byte sequence is two errors in Go, one in Rust's
        // lossy conversion.
        assert_eq!(lossy(b"a\xe2\x82b"), "a\u{fffd}\u{fffd}b");
    }

    #[test]
    fn canonical_header_keys_match_go() {
        assert_eq!(canonical_header_key("content-type"), "Content-Type");
        assert_eq!(canonical_header_key("x-api-KEY"), "X-Api-Key");
        assert_eq!(canonical_header_key("www-authenticate"), "Www-Authenticate");
        assert_eq!(canonical_header_key("x--y"), "X--Y");
    }

    #[test]
    fn durations_print_as_go_prints_them() {
        assert_eq!(duration_string(0), "0s");
        assert_eq!(duration_string(59), "59s");
        assert_eq!(duration_string(60), "1m0s");
        assert_eq!(duration_string(29 * 60 + 59), "29m59s");
        assert_eq!(duration_string(3600), "1h0m0s");
        assert_eq!(duration_string(3661), "1h1m1s");
    }

    #[test]
    fn zero_times() {
        assert!(is_zero(None));
        assert!(is_zero(Some(zero_time())));
        assert!(!is_zero(Some(Utc::now())));
    }

    /// Go's `textproto.CanonicalMIMEHeaderKey`, `strings.ToUpper`,
    /// `strconv.ParseBool` and `Atoi`, and `strings.EqualFold`.
    #[test]
    fn strings_match_go() {
        let canonical = [
            ("", ""),
            ("a", "A"),
            ("content-type", "Content-Type"),
            ("CONTENT-TYPE", "Content-Type"),
            ("x-cpa-version", "X-Cpa-Version"),
            ("x_y", "X_y"),
            ("a b", "a b"),
            ("-x", "-X"),
            ("x-", "X-"),
            ("x--y", "X--Y"),
            ("\u{e9}-x", "\u{e9}-x"),
            ("user-agent", "User-Agent"),
            ("WWW-Authenticate", "Www-Authenticate"),
            ("x-123abc", "X-123abc"),
            ("1-a", "1-A"),
            ("ab\0", "ab\0"),
            ("a:b", "a:b"),
            ("Trailer", "Trailer"),
            ("transfer-encoding", "Transfer-Encoding"),
            ("x-Multi", "X-Multi"),
            ("\u{131}", "\u{131}"),
        ];
        for (name, want) in canonical {
            assert_eq!(canonical_header_key(name), want, "{name:?}");
        }
        let upper = [
            ("\u{131}", "I"),
            ("k", "K"),
            ("\u{212a}", "\u{212a}"),
            ("\u{17f}", "S"),
            ("\u{df}", "\u{df}"),
            ("\u{1e9e}", "\u{1e9e}"),
            ("\u{1c5}", "\u{1c4}"),
            ("\u{1c6}", "\u{1c4}"),
            ("\u{1c4}", "\u{1c4}"),
            ("\u{17f}trasse", "STRASSE"),
            ("STRASSE", "STRASSE"),
        ];
        for (s, want) in upper {
            assert_eq!(to_upper(s), want, "{s:?}");
        }
        let bools = [
            ("true", Some(true)),
            ("TRUE", Some(true)),
            ("True", Some(true)),
            ("tRUE", None),
            ("1", Some(true)),
            ("t", Some(true)),
            ("T", Some(true)),
            ("f", Some(false)),
            ("F", Some(false)),
            ("0", Some(false)),
            ("false", Some(false)),
            ("FALSE", Some(false)),
            ("False", Some(false)),
            ("yes", None),
            ("on", None),
            (" true", None),
            ("0", Some(false)),
        ];
        for (s, want) in bools {
            assert_eq!(parse_bool(s), want, "{s:?}");
        }
        let numbers = [
            ("-0", Some(0)),
            ("+5", Some(5)),
            ("05", Some(5)),
            ("9223372036854775807", Some(9223372036854775807)),
            ("9223372036854775808", None),
            ("-9223372036854775808", Some(-9223372036854775808)),
            ("-9223372036854775809", None),
            (" 1", None),
            ("1 ", None),
            ("1_000", None),
            ("0x10", None),
            ("\u{661}", None),
            ("12a", None),
            ("+", None),
            ("-", None),
            ("00", Some(0)),
            ("\u{1fb3}", None),
            ("\u{390}", None),
            ("\u{149}", None),
        ];
        for (s, want) in numbers {
            assert_eq!(atoi(s), want, "{s:?}");
        }
        let folds = [
            ("Go", "GO", true),
            ("k", "\u{212a}", true),
            ("s", "\u{17f}", true),
            ("S", "\u{17f}", true),
            ("\u{df}", "\u{1e9e}", true),
            ("ss", "\u{df}", false),
            ("i", "\u{131}", false),
            ("I", "\u{130}", false),
            ("i", "\u{130}", false),
            ("\u{1c5}", "\u{1c6}", true),
            ("\u{1c5}", "\u{1c4}", true),
            ("\u{3a9}", "\u{3c9}", true),
            ("\u{3a9}", "\u{3c9}", true),
            ("\u{3a9}", "\u{3a9}", true),
            ("\u{b5}", "\u{39c}", true),
            ("\u{b5}", "\u{3bc}", true),
            ("a", "ab", false),
            ("", "", true),
            ("codex", "CODEX", true),
            ("\u{3b8}", "\u{3d1}", true),
            ("\u{3d1}", "\u{398}", true),
            ("\u{3c2}", "\u{3a3}", true),
            ("\u{3c2}", "\u{3c3}", true),
            ("\u{1fb3}", "\u{1fbc}", true),
            ("\u{345}", "\u{3b9}", true),
        ];
        for (a, b, want) in folds {
            assert_eq!(equal_fold(a, b), want, "{a:?} {b:?}");
        }
    }
}
