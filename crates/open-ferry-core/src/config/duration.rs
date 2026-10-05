// Ported from Go's time.ParseDuration (src/time/format.go, go1.27, BSD-3-Clause),
// which CLIProxyAPI internal/config/config_types.go uses (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! Go's duration syntax, such as `300ms`, `-1.5h` or `2h45m`.
//!
//! Deviations from upstream:
//! - Errors carry no message; callers only need to know the text didn't
//!   parse.

const NANOSECOND: u64 = 1;
const MICROSECOND: u64 = 1_000 * NANOSECOND;
const MILLISECOND: u64 = 1_000 * MICROSECOND;
const SECOND: u64 = 1_000 * MILLISECOND;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const LIMIT: u64 = 1 << 63;

fn unit(name: &[u8]) -> Option<u64> {
    Some(match name {
        b"ns" => NANOSECOND,
        // "µs" (micro sign) and "μs" (Greek mu) in UTF-8.
        b"us" | [0xC2, 0xB5, b's'] | [0xCE, 0xBC, b's'] => MICROSECOND,
        b"ms" => MILLISECOND,
        b"s" => SECOND,
        b"m" => MINUTE,
        b"h" => HOUR,
        _ => return None,
    })
}

/// Go's `leadingInt`: the leading digits and the rest, or `None` on overflow.
fn leading_int(text: &[u8]) -> Option<(u64, &[u8])> {
    let mut value: u64 = 0;
    let mut index = 0;
    while let Some(&byte) = text.get(index) {
        if !byte.is_ascii_digit() {
            break;
        }
        if value > LIMIT / 10 {
            return None;
        }
        value = value * 10 + u64::from(byte - b'0');
        if value > LIMIT {
            return None;
        }
        index += 1;
    }
    Some((value, text.get(index..).unwrap_or_default()))
}

/// Go's `leadingFraction`: the digits, their scale and the rest. Digits past
/// what fits are skipped.
fn leading_fraction(text: &[u8]) -> (u64, f64, &[u8]) {
    let mut value: u64 = 0;
    let mut scale = 1.0;
    let mut overflow = false;
    let mut index = 0;
    while let Some(&byte) = text.get(index) {
        if !byte.is_ascii_digit() {
            break;
        }
        index += 1;
        if overflow {
            continue;
        }
        if value > (LIMIT - 1) / 10 {
            overflow = true;
            continue;
        }
        let next = value * 10 + u64::from(byte - b'0');
        if next > LIMIT {
            overflow = true;
            continue;
        }
        value = next;
        scale *= 10.0;
    }
    (value, scale, text.get(index..).unwrap_or_default())
}

/// Parses a Go duration into nanoseconds, as `time.ParseDuration` does.
pub(crate) fn parse_go_duration(text: &str) -> Option<i64> {
    let mut rest = text.as_bytes();
    let mut negative = false;
    if let Some((&sign @ (b'-' | b'+'), tail)) = rest.split_first() {
        negative = sign == b'-';
        rest = tail;
    }
    if rest == b"0" {
        return Some(0);
    }
    if rest.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    while let Some(&first) = rest.first() {
        if first != b'.' && !first.is_ascii_digit() {
            return None;
        }
        let before = rest.len();
        let (mut value, tail) = leading_int(rest)?;
        rest = tail;
        let pre = before != rest.len();
        let mut fraction = 0;
        let mut scale = 1.0;
        let mut post = false;
        if let Some((b'.', tail)) = rest.split_first() {
            let before = tail.len();
            (fraction, scale, rest) = leading_fraction(tail);
            post = before != rest.len();
        }
        if !pre && !post {
            return None;
        }
        let length = rest
            .iter()
            .position(|&byte| byte == b'.' || byte.is_ascii_digit());
        let length = length.unwrap_or(rest.len());
        if length == 0 {
            return None;
        }
        let (name, tail) = rest.split_at(length);
        rest = tail;
        let unit = unit(name)?;
        if value > LIMIT / unit {
            return None;
        }
        value *= unit;
        if fraction > 0 {
            // Go converts through float64 to stay exact for fractions of hours.
            value += (fraction as f64 * (unit as f64 / scale)) as u64;
            if value > LIMIT {
                return None;
            }
        }
        // Go's uint64 sum wraps; it can't exceed 2^64 here.
        total = total.wrapping_add(value);
        if total > LIMIT {
            return None;
        }
    }
    if negative {
        return Some((total as i64).wrapping_neg());
    }
    i64::try_from(total).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_go() {
        for (text, nanos) in [
            ("0", Some(0)),
            ("-0", Some(0)),
            ("5s", Some(5 * SECOND as i64)),
            ("+5s", Some(5 * SECOND as i64)),
            ("-1.5h", Some(-(90 * MINUTE as i64))),
            ("2h45m", Some((2 * HOUR + 45 * MINUTE) as i64)),
            ("300ms", Some(300 * MILLISECOND as i64)),
            ("1.s", Some(SECOND as i64)),
            (".5s", Some(500 * MILLISECOND as i64)),
            ("1us", Some(1_000)),
            ("1\u{b5}s", Some(1_000)),
            ("1\u{3bc}s", Some(1_000)),
            ("9223372036854775807ns", Some(i64::MAX)),
            ("-9223372036854775808ns", Some(i64::MIN)),
            ("9223372036854775808ns", None),
            // Go's sum wraps at 2^64.
            ("9223372036854775808ns9223372036854775808ns1ns", Some(1)),
            ("9223372036854775808ns9223372036854775808ns", Some(0)),
            ("-9223372036854775808ns9223372036854775808ns5ns", Some(-5)),
            ("9223372036854775808ns1ns", None),
            ("", None),
            ("-", None),
            ("5", None),
            ("s", None),
            (".s", None),
            ("5x", None),
            ("1d", None),
            ("3000000h", None),
        ] {
            assert_eq!(parse_go_duration(text), nanos, "{text:?}");
        }
    }
}
