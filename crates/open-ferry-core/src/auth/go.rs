//! Go standard library behaviour the credential code depends on:
//! `strings.EqualFold`, `strconv.ParseBool` and `strconv.Atoi`, a JSON
//! number's conversion to `int64`, and the JWT payload decoding upstream
//! does with `encoding/base64`.

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
/// added (counting any line breaks, as Go counts them) and the result decoded
/// with `base64.URLEncoding`.
pub(crate) fn decode_jwt_segment_padded(segment: &str) -> Option<Vec<u8>> {
    let padded = match segment.len() % 4 {
        2 => format!("{segment}=="),
        3 => format!("{segment}="),
        _ => segment.to_owned(),
    };
    URL_PADDED.decode(without_line_breaks(&padded)).ok()
}

/// [`decode_jwt_segment_padded`], falling back to `base64.RawURLEncoding` on
/// the segment as it is, as upstream's `parseJWTExp` does.
pub(crate) fn decode_jwt_segment(segment: &str) -> Option<Vec<u8>> {
    decode_jwt_segment_padded(segment).or_else(|| URL_RAW.decode(without_line_breaks(segment)).ok())
}

/// `text` without the `\r` and `\n` that Go's base64 decoders skip wherever
/// they are.
fn without_line_breaks(text: &str) -> String {
    text.replace(['\r', '\n'], "")
}

pub(crate) use open_ferry_translate::go::equal_fold;

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

/// A JSON number as Go's `int64(float64)` gives it once its decoder has read
/// the number as a float64: an integer past 2^53 rounds as it does there,
/// and a value outside `int64` saturates where Go's conversion is undefined.
pub(crate) fn number_to_i64(number: &serde_json::Number) -> i64 {
    number.as_f64().map_or(0, |f| f as i64)
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
        // Go's decoders skip line breaks, but the padding counts them.
        let crlf = "e3\r\n0";
        assert_eq!(decode_jwt_segment(crlf).as_deref(), Some(&b"{}"[..]));
        assert_eq!(decode_jwt_segment_padded(crlf), None);
        assert_eq!(
            decode_jwt_segment_padded("e30\r\n\r\n").as_deref(),
            Some(&b"{}"[..])
        );
        assert_eq!(decode_jwt_segment("e3\r\n0="), None);
    }

    #[test]
    fn numbers_convert_through_float64() {
        let int = |text: &str| number_to_i64(&serde_json::from_str(text).unwrap());
        assert_eq!(int("42"), 42);
        assert_eq!(int("-3.9"), -3);
        assert_eq!(int("9007199254740993"), 9_007_199_254_740_992);
        assert_eq!(int("9223372036854775807"), i64::MAX);
        assert_eq!(int("1e30"), i64::MAX);
        assert_eq!(int("-1e30"), i64::MIN);
    }
}
