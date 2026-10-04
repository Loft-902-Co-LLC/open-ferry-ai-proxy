//! Go standard library behaviour that upstream's output depends on.
//!
//! A few of these are public for the crates that build on this one.

pub(crate) mod base64;
mod float;
mod ftoa;
mod printable;

pub use float::parse_float;
pub(crate) use float::parse_float_checked;
pub use ftoa::{format_float, format_float_g, json_float};

use std::cmp::Ordering;
use std::fmt::Write as _;

/// Go's `strings.ToLower`: maps each character on its own by its simple Unicode
/// mapping. Rust's `str::to_lowercase` differs for `İ` (to `i` plus a combining
/// dot) and for a word-final `Σ` (to `ς`); Go gives `i` and `σ`.
pub fn to_lower(s: &str) -> String {
    s.chars().map(simple_lower).collect()
}

/// Go's `strings.ToUpper`: maps each character on its own by its simple Unicode
/// mapping. Rust's `str::to_uppercase` uses the full mapping, which turns `ß`
/// into `SS`; Go leaves a character with no one-character upper case as it is.
pub fn to_upper(s: &str) -> String {
    s.chars().map(simple_upper).collect()
}

/// The simple uppercase mapping of `c`, as Go's `unicode.ToUpper` gives it.
fn simple_upper(c: char) -> char {
    let mut mapped = c.to_uppercase();
    if let (Some(upper), None) = (mapped.next(), mapped.next()) {
        return upper;
    }
    // Of the characters whose full mapping is longer, only the Greek letters
    // with a subscript iota have a simple one: the capital with the iota
    // beside it.
    let offset = match u32::from(c) {
        0x1f80..=0x1f87 | 0x1f90..=0x1f97 | 0x1fa0..=0x1fa7 => 8,
        0x1fb3 | 0x1fc3 | 0x1ff3 => 9,
        _ => 0,
    };
    char::from_u32(u32::from(c) + offset).unwrap_or(c)
}

/// Go's `strings.EqualFold`: whether `a` and `b` are equal under simple
/// Unicode case folding, each character against the one in the same place.
pub fn equal_fold(a: &str, b: &str) -> bool {
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

/// Whether `a` and `b` are in the same case-folding orbit, as `EqualFold`
/// decides it for one pair of characters.
fn fold_eq(a: char, b: char) -> bool {
    if a == b {
        return true;
    }
    let (low, high) = if a < b { (a, b) } else { (b, a) };
    if high.is_ascii() {
        return low.is_ascii_uppercase() && high == low.to_ascii_lowercase();
    }
    // Go walks the orbit from the lower character until it passes the higher.
    let mut next = simple_fold(low);
    while next != low && next < high {
        next = simple_fold(next);
    }
    next == high
}

/// Go's `unicode.SimpleFold`: the next character after `c` in its orbit of
/// characters equal under simple case folding, wrapping round to the
/// lowest; `c` itself when it has no other case.
pub fn simple_fold(c: char) -> char {
    if let Ok(index) = CASE_ORBIT.binary_search_by_key(&u32::from(c), |&(from, _)| from) {
        return char::from_u32(CASE_ORBIT[index].1).unwrap_or(c);
    }
    let lower = simple_lower(c);
    if lower != c {
        return lower;
    }
    simple_upper(c)
}

/// Go's `caseOrbit` (Unicode 15.0): the orbits of more than two characters,
/// and the two Turkish i's, each character with the next in its orbit.
/// Every other character's orbit is itself and its lower or upper case.
const CASE_ORBIT: [(u32, u32); 88] = [
    (0x004b, 0x006b),
    (0x0053, 0x0073),
    (0x006b, 0x212a),
    (0x0073, 0x017f),
    (0x00b5, 0x039c),
    (0x00c5, 0x00e5),
    (0x00df, 0x1e9e),
    (0x00e5, 0x212b),
    (0x0130, 0x0130),
    (0x0131, 0x0131),
    (0x017f, 0x0053),
    (0x01c4, 0x01c5),
    (0x01c5, 0x01c6),
    (0x01c6, 0x01c4),
    (0x01c7, 0x01c8),
    (0x01c8, 0x01c9),
    (0x01c9, 0x01c7),
    (0x01ca, 0x01cb),
    (0x01cb, 0x01cc),
    (0x01cc, 0x01ca),
    (0x01f1, 0x01f2),
    (0x01f2, 0x01f3),
    (0x01f3, 0x01f1),
    (0x0345, 0x0399),
    (0x0392, 0x03b2),
    (0x0395, 0x03b5),
    (0x0398, 0x03b8),
    (0x0399, 0x03b9),
    (0x039a, 0x03ba),
    (0x039c, 0x03bc),
    (0x03a0, 0x03c0),
    (0x03a1, 0x03c1),
    (0x03a3, 0x03c2),
    (0x03a6, 0x03c6),
    (0x03a9, 0x03c9),
    (0x03b2, 0x03d0),
    (0x03b5, 0x03f5),
    (0x03b8, 0x03d1),
    (0x03b9, 0x1fbe),
    (0x03ba, 0x03f0),
    (0x03bc, 0x00b5),
    (0x03c0, 0x03d6),
    (0x03c1, 0x03f1),
    (0x03c2, 0x03c3),
    (0x03c3, 0x03a3),
    (0x03c6, 0x03d5),
    (0x03c9, 0x2126),
    (0x03d0, 0x0392),
    (0x03d1, 0x03f4),
    (0x03d5, 0x03a6),
    (0x03d6, 0x03a0),
    (0x03f0, 0x039a),
    (0x03f1, 0x03a1),
    (0x03f4, 0x0398),
    (0x03f5, 0x0395),
    (0x0412, 0x0432),
    (0x0414, 0x0434),
    (0x041e, 0x043e),
    (0x0421, 0x0441),
    (0x0422, 0x0442),
    (0x042a, 0x044a),
    (0x0432, 0x1c80),
    (0x0434, 0x1c81),
    (0x043e, 0x1c82),
    (0x0441, 0x1c83),
    (0x0442, 0x1c84),
    (0x044a, 0x1c86),
    (0x0462, 0x0463),
    (0x0463, 0x1c87),
    (0x1c80, 0x0412),
    (0x1c81, 0x0414),
    (0x1c82, 0x041e),
    (0x1c83, 0x0421),
    (0x1c84, 0x1c85),
    (0x1c85, 0x0422),
    (0x1c86, 0x042a),
    (0x1c87, 0x0462),
    (0x1c88, 0xa64a),
    (0x1e60, 0x1e61),
    (0x1e61, 0x1e9b),
    (0x1e9b, 0x1e60),
    (0x1e9e, 0x00df),
    (0x1fbe, 0x0345),
    (0x2126, 0x03a9),
    (0x212a, 0x004b),
    (0x212b, 0x00c5),
    (0xa64a, 0xa64b),
    (0xa64b, 0x1c88),
];

/// The simple lowercase mapping of `c`, as Go's `unicode.ToLower` gives it.
fn simple_lower(c: char) -> char {
    // `char::to_lowercase` yields the full mapping. Only `İ` has more than one
    // character, and the first is its simple mapping.
    c.to_lowercase().next().unwrap_or(c)
}

/// Go's `strconv.Quote`, which `%q` uses: wraps `s` in double quotes and
/// escapes `"`, `\` and every character `strconv.IsPrint` rejects.
pub fn quote(s: &str) -> String {
    quote_bytes(s.as_bytes())
}

/// [`quote`] for bytes that may not be UTF-8, as `%q` quotes a `[]byte`:
/// each byte that isn't part of a valid character is written `\xNN`.
pub fn quote_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            push_quoted(&mut out, c);
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    out.push('"');
    out
}

/// Writes `c` as `strconv.Quote` does.
fn push_quoted(out: &mut String, c: char) {
    match c {
        '"' | '\\' => {
            out.push('\\');
            out.push(c);
        }
        c if is_print(c) => out.push(c),
        '\u{7}' => out.push_str("\\a"),
        '\u{8}' => out.push_str("\\b"),
        '\u{c}' => out.push_str("\\f"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{b}' => out.push_str("\\v"),
        c if c < ' ' || c == '\u{7f}' => {
            let _ = write!(out, "\\x{:02x}", u32::from(c));
        }
        c if u32::from(c) < 0x10000 => {
            let _ = write!(out, "\\u{:04x}", u32::from(c));
        }
        c => {
            let _ = write!(out, "\\U{:08x}", u32::from(c));
        }
    }
}

/// Go's `bytes.TrimSpace`: `bytes` without the white space characters at
/// either end. Like Go, it stops at a byte that isn't part of a valid
/// character.
pub fn trim_space(bytes: &[u8]) -> &[u8] {
    let mut chunks = bytes.utf8_chunks();
    let Some(first) = chunks.next() else {
        return bytes;
    };
    let start = first.valid().len() - first.valid().trim_start().len();
    let last = chunks.last().unwrap_or(first);
    let end = if last.invalid().is_empty() {
        bytes.len() - (last.valid().len() - last.valid().trim_end().len())
    } else {
        bytes.len()
    };
    bytes.get(start..end).unwrap_or_default()
}

/// How deep Go's `encoding/json` lets arrays and objects nest
/// (`maxNestingDepth`).
pub const MAX_NESTING_DEPTH: usize = 10_000;

/// Go's `json.Valid`: whether `bytes` is one JSON value, nested at most
/// [`MAX_NESTING_DEPTH`] deep. Like Go, it doesn't check that strings are
/// UTF-8.
pub fn json_valid(bytes: &[u8]) -> bool {
    crate::json::raw::valid_within(bytes, MAX_NESTING_DEPTH)
}

/// gjson's `Valid`: [`json_valid`] without a limit on nesting.
pub fn gjson_valid(bytes: &[u8]) -> bool {
    crate::json::raw::valid_bytes(bytes)
}

/// Go's `json.Marshal` of a string: wrapped in double quotes, with the escapes
/// JSON requires and, as Go adds for HTML, `<`, `>`, `&`, U+2028 and U+2029
/// written as `\u` escapes too.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' || matches!(c, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Go's `strconv.IsPrint`.
fn is_print(c: char) -> bool {
    let c = u32::from(c);
    printable::PRINTABLE
        .binary_search_by(|&(start, end)| {
            if end < c {
                Ordering::Less
            } else if start > c {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// Go's `math.Log2`. Exact powers of two give exact results.
pub(crate) fn log2(x: f64) -> f64 {
    let (frac, exp) = frexp(x);
    if frac == 0.5 {
        return f64::from(exp - 1);
    }
    frac.ln() * std::f64::consts::LOG2_E + f64::from(exp)
}

/// Go's `math.Frexp`: `x == frac * 2^exp` with `frac` in `[0.5, 1)`.
fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let (x, mut exp) = if x.abs() < f64::MIN_POSITIVE {
        // Normalize a subnormal number.
        (x * (1u64 << 52) as f64, -52)
    } else {
        (x, 0)
    };
    let bits = x.to_bits();
    exp += ((bits >> 52) & 0x7ff) as i32 - 1022;
    let frac = f64::from_bits(bits & !(0x7ff << 52) | 1022 << 52);
    (frac, exp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_lower_matches_go() {
        assert_eq!(to_lower("PRIORITY"), "priority");
        assert_eq!(to_lower("OPENAİ"), "openai");
        assert_eq!(to_lower("ΑΣ"), "ασ");
        assert_eq!(to_lower("Straße ÀÉ"), "straße àé");
    }

    #[test]
    fn to_upper_matches_go() {
        assert_eq!(to_upper("post"), "POST");
        // The long s and the dotless i upper-case to ASCII.
        assert_eq!(to_upper("po\u{17f}t \u{131}"), "POST I");
        // Sharp s and a ligature have no one-character upper case.
        assert_eq!(to_upper("stra\u{df}e \u{fb00}"), "STRA\u{df}E \u{fb00}");
        // Letters with a subscript iota take the iota beside them.
        assert_eq!(
            to_upper("\u{1f80}\u{1fa7}\u{1fb3}\u{1fc3}\u{1ff3}"),
            "\u{1f88}\u{1faf}\u{1fbc}\u{1fcc}\u{1ffc}"
        );
        // Titlecase forms stay.
        assert_eq!(to_upper("\u{1f88}\u{1fbc}"), "\u{1f88}\u{1fbc}");
    }

    #[test]
    fn json_valid_limits_nesting_as_go_does() {
        let nested = |depth: usize, inner: &str| {
            format!("{}{inner}{}", "[".repeat(depth), "]".repeat(depth))
        };
        assert!(json_valid(nested(MAX_NESTING_DEPTH, "0").as_bytes()));
        assert!(!json_valid(nested(MAX_NESTING_DEPTH + 1, "0").as_bytes()));
        // An empty array or object is a level too.
        assert!(json_valid(nested(MAX_NESTING_DEPTH - 1, "[]").as_bytes()));
        assert!(!json_valid(nested(MAX_NESTING_DEPTH, "[]").as_bytes()));
        assert!(!json_valid(nested(MAX_NESTING_DEPTH, "{}").as_bytes()));
        let objects = format!(
            "{}0{}",
            r#"{"a":"#.repeat(MAX_NESTING_DEPTH + 1),
            "}".repeat(MAX_NESTING_DEPTH + 1)
        );
        assert!(!json_valid(objects.as_bytes()));
        // gjson has no limit.
        assert!(gjson_valid(nested(MAX_NESTING_DEPTH + 1, "[]").as_bytes()));
        assert!(gjson_valid(objects.as_bytes()));
    }

    #[test]
    fn equal_fold_matches_go() {
        assert!(equal_fold("Gemini", "gemini"));
        assert!(equal_fold("", ""));
        assert!(!equal_fold("gemini-cli", "gemini"));
        assert!(!equal_fold("ab", "a"));
        // The Kelvin and Angstrom signs, the long s and the micro sign are in
        // orbits of three.
        assert!(equal_fold("\u{212a}\u{212b}", "k\u{e5}"));
        assert!(equal_fold("\u{17f}\u{b5}", "S\u{39c}"));
        // Every theta is one orbit, though no one mapping joins the symbol
        // and the capital symbol.
        for (a, b) in [
            ('\u{398}', '\u{3d1}'),
            ('\u{3d1}', '\u{3f4}'),
            ('\u{3b8}', '\u{3f4}'),
        ] {
            assert!(fold_eq(a, b) && fold_eq(b, a), "{a} {b}");
        }
        // Sharp s and capital sharp s; the titlecase digraphs.
        assert!(equal_fold("\u{df}", "\u{1e9e}"));
        assert!(equal_fold("\u{1c4}\u{1c5}", "\u{1c6}\u{1c6}"));
        // The combining iota is an iota; the old Cyrillic forms fold too.
        assert!(equal_fold("\u{345}\u{1fbe}", "\u{399}\u{3b9}"));
        assert!(equal_fold("\u{1c80}\u{1c88}", "\u{432}\u{a64b}"));
        // Neither Turkish i folds to an ASCII one.
        assert!(!equal_fold("\u{130}", "i") && !equal_fold("\u{130}", "I"));
        assert!(!equal_fold("\u{131}", "I") && !equal_fold("\u{131}", "i"));
        // Ligatures with no one-character case are only themselves, though
        // both upper-case in full to "ST".
        assert!(!equal_fold("\u{fb05}", "\u{fb06}"));
        assert!(!equal_fold("\u{fb05}", "st"));
        // Upper case past ASCII against an ASCII letter.
        assert!(!equal_fold("\u{c0}", "a"));
    }

    #[test]
    fn simple_fold_matches_go() {
        // unicode.SimpleFold's documented examples.
        assert_eq!(simple_fold('A'), 'a');
        assert_eq!(simple_fold('a'), 'A');
        assert_eq!(simple_fold('K'), 'k');
        assert_eq!(simple_fold('k'), '\u{212a}');
        assert_eq!(simple_fold('\u{212a}'), 'K');
        assert_eq!(simple_fold('1'), '1');
        assert_eq!(simple_fold('\u{10ffff}'), '\u{10ffff}');
        assert_eq!(simple_fold('\u{130}'), '\u{130}');
    }

    #[test]
    fn quote_matches_go() {
        assert_eq!(quote("abc"), r#""abc""#);
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(quote("\n\t\u{7}\u{0}\u{7f}"), r#""\n\t\a\x00\x7f""#);
        assert_eq!(quote("\u{e9}\u{4e16} "), "\"\u{e9}\u{4e16} \"");
        // Not printable: a soft hyphen, a line separator and a private-use character.
        assert_eq!(
            quote("\u{ad}\u{2028}\u{f0000}"),
            "\"\u{5c}u00ad\u{5c}u2028\u{5c}U000f0000\""
        );
        assert_eq!(quote("\u{80}"), "\"\u{5c}u0080\"");
        // A letter from Unicode 16, which Go 1.26 (Unicode 15) doesn't know.
        assert_eq!(quote("\u{10d50}"), "\"\u{5c}U00010d50\"");
    }

    #[test]
    fn trim_space_matches_go() {
        assert_eq!(trim_space(b" \t\r\n a b \x0b\x0c"), b"a b");
        assert_eq!(trim_space(b"   "), b"");
        assert_eq!(trim_space(b""), b"");
        // U+00A0, U+0085 and U+3000 are white space.
        assert_eq!(trim_space("\u{a0}\u{85}x\u{3000}".as_bytes()), b"x");
        // Trimming stops at an invalid byte.
        assert_eq!(trim_space(b" \xff x \xff "), b"\xff x \xff");
        assert_eq!(trim_space(b" \xe2\x80 "), b"\xe2\x80");
        assert_eq!(trim_space(b" x\xe2\x80\x80\x80"), b"x\xe2\x80\x80\x80");
    }

    #[test]
    fn json_string_matches_go() {
        assert_eq!(json_string("a\"b\\c"), r#""a\"b\\c""#);
        // DEL is left as it is.
        assert_eq!(
            json_string("\u{0}\u{8}\u{c}\n\r\t\u{1f}\u{7f}"),
            "\"\\u0000\\b\\f\\n\\r\\t\\u001f\u{7f}\""
        );
        assert_eq!(
            json_string("<a>&\u{2028}\u{2029}é🙂"),
            r#""\u003ca\u003e\u0026\u2028\u2029é🙂""#
        );
    }

    #[test]
    fn log2_matches_go() {
        assert_eq!(log2(0.25), -2.0);
        assert_eq!(log2(256.0), 8.0);
        assert_eq!(log2(1.0), 0.0);
        assert!((log2(3.0) - 1.584_962_500_721_156).abs() < 1e-15);
        assert_eq!(frexp(f64::MIN_POSITIVE / 4.0), (0.5, -1023));
    }
}
