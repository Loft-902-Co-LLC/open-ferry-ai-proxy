//! Go standard library behaviour the credential code depends on:
//! `strings.EqualFold`, `strconv.ParseBool` and `strconv.Atoi`, and the JWT
//! payload decoding upstream does with `encoding/base64`.

use base64::Engine;
use base64::alphabet::URL_SAFE;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

/// Go's `base64.URLEncoding`: padded, and lenient about the unused bits of
/// the last character, as Go is.
const URL_PADDED: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

/// Go's `base64.RawURLEncoding`: unpadded.
const URL_RAW: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireNone),
);

/// Decodes a JWT segment as upstream's helpers do: the missing `=` padding is
/// added and the result decoded with `base64.URLEncoding`.
pub(crate) fn decode_jwt_segment_padded(segment: &str) -> Option<Vec<u8>> {
    let padded = match segment.len() % 4 {
        2 => format!("{segment}=="),
        3 => format!("{segment}="),
        _ => segment.to_owned(),
    };
    URL_PADDED.decode(padded).ok()
}

/// [`decode_jwt_segment_padded`], falling back to `base64.RawURLEncoding` on
/// the segment as it is, as upstream's `parseJWTExp` does.
pub(crate) fn decode_jwt_segment(segment: &str) -> Option<Vec<u8>> {
    decode_jwt_segment_padded(segment).or_else(|| URL_RAW.decode(segment).ok())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_fold_matches_go() {
        assert!(equal_fold("Gemini", "gemini"));
        assert!(equal_fold("GEMINI", "gemini"));
        assert!(!equal_fold("gemini-cli", "gemini"));
        // The Kelvin sign and the long s fold to ASCII letters.
        assert!(equal_fold("\u{212a}", "k"));
        assert!(equal_fold("\u{17f}", "S"));
        assert!(equal_fold("\u{3a3}", "\u{3c2}"));
        // Neither Turkish i folds to an ASCII i, and sharp s isn't "ss".
        assert!(!equal_fold("\u{131}", "i"));
        assert!(!equal_fold("\u{130}", "i"));
        assert!(!equal_fold("\u{df}", "ss"));
    }

    #[test]
    fn parse_bool_matches_go() {
        assert_eq!(parse_bool("True"), Some(true));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("yes"), None);
        assert_eq!(parse_bool("tRUE"), None);
    }

    #[test]
    fn atoi_matches_go() {
        assert_eq!(atoi("+12"), Some(12));
        assert_eq!(atoi("-3"), Some(-3));
        assert_eq!(atoi(" 3"), None);
        assert_eq!(atoi("1x"), None);
        assert_eq!(atoi("9223372036854775808"), None);
    }

    #[test]
    fn jwt_segments_decode_with_or_without_padding() {
        // "{}" is "e30" unpadded.
        assert_eq!(decode_jwt_segment("e30").as_deref(), Some(&b"{}"[..]));
        assert_eq!(decode_jwt_segment("e30=").as_deref(), Some(&b"{}"[..]));
        assert_eq!(
            decode_jwt_segment_padded("e30").as_deref(),
            Some(&b"{}"[..])
        );
        assert_eq!(decode_jwt_segment("e"), None);
        assert_eq!(decode_jwt_segment("e3+"), None);
    }
}
