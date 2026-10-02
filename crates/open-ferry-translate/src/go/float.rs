//! Go's `strconv.ParseFloat`, as gjson reads a number from text.
//!
//! Ported from Go's `strconv/atof.go` (BSD-3-Clause, see
//! licenses/Go-LICENSE). Rust reads decimal numbers the same way once
//! underscores are taken out; Go also takes hexadecimal mantissas with a
//! binary exponent, such as `0x1p-2`.

/// `strconv.ParseFloat(s, 64)` with its error ignored, as gjson does: the
/// nearest `f64`, or infinity or zero when out of range, or 0 if `s` isn't a
/// Go floating-point literal.
pub(crate) fn parse_float(s: &str) -> f64 {
    if let Some(special) = special(s) {
        return special;
    }
    let Some(float) = read_float(s) else {
        return 0.0;
    };
    if float.end != s.len() {
        return 0.0;
    }
    if float.hex {
        return atof_hex(float.mantissa, float.exp, float.negative, float.truncated);
    }
    // Rust reads the same literal without its underscores. Go stops adding to
    // the exponent once it passes 10000, so the exponent is written as Go
    // reads it.
    let mut text: String = s[..float.mantissa_end]
        .chars()
        .filter(|&c| c != '_')
        .collect();
    text.push('e');
    text.push_str(&float.decimal_exp.to_string());
    text.parse().unwrap_or(0.0)
}

/// `special`: infinity and NaN, ignoring case. Only infinity takes a sign.
fn special(s: &str) -> Option<f64> {
    let (negative, rest) = match s.as_bytes().first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    if rest.eq_ignore_ascii_case("inf") || rest.eq_ignore_ascii_case("infinity") {
        return Some(if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    (s.eq_ignore_ascii_case("nan")).then_some(f64::NAN)
}

/// What `readFloat` reads.
struct Float {
    negative: bool,
    hex: bool,
    /// Hexadecimal: the first 16 digits, then whether any later one wasn't 0.
    mantissa: u64,
    truncated: bool,
    /// Hexadecimal: the binary exponent of `mantissa`'s last digit.
    exp: i64,
    /// Decimal: where the mantissa ends, and the exponent as Go reads it.
    mantissa_end: usize,
    decimal_exp: i64,
    /// Where the literal ends.
    end: usize,
}

/// `readFloat`: a decimal or hexadecimal literal at the start of `s`.
fn read_float(s: &str) -> Option<Float> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let negative = match bytes.first() {
        Some(b'+') => {
            i += 1;
            false
        }
        Some(b'-') => {
            i += 1;
            true
        }
        _ => false,
    };

    let mut hex = false;
    let mut max_mantissa_digits = 19;
    let mut exp_char = b'e';
    if i + 2 < bytes.len() && bytes[i] == b'0' && bytes[i + 1].eq_ignore_ascii_case(&b'x') {
        hex = true;
        max_mantissa_digits = 16;
        exp_char = b'p';
        i += 2;
    }
    let base: u64 = if hex { 16 } else { 10 };

    let mut underscores = false;
    let mut saw_dot = false;
    let mut saw_digits = false;
    let mut digits = 0i64;
    let mut mantissa_digits = 0i64;
    let mut dp = 0i64;
    let mut mantissa = 0u64;
    let mut truncated = false;
    while let Some(&c) = bytes.get(i) {
        let lower = c.to_ascii_lowercase();
        match c {
            b'_' => underscores = true,
            b'.' if saw_dot => break,
            b'.' => {
                saw_dot = true;
                dp = digits;
            }
            b'0'..=b'9' => {
                saw_digits = true;
                if c == b'0' && digits == 0 {
                    // Leading zeros only move the point.
                    dp -= 1;
                } else {
                    digits += 1;
                    if mantissa_digits < max_mantissa_digits {
                        mantissa = mantissa * base + u64::from(c - b'0');
                        mantissa_digits += 1;
                    } else if c != b'0' {
                        truncated = true;
                    }
                }
            }
            _ if hex && (b'a'..=b'f').contains(&lower) => {
                saw_digits = true;
                digits += 1;
                if mantissa_digits < max_mantissa_digits {
                    mantissa = mantissa * 16 + u64::from(lower - b'a' + 10);
                    mantissa_digits += 1;
                } else {
                    truncated = true;
                }
            }
            _ => break,
        }
        i += 1;
    }
    if !saw_digits {
        return None;
    }
    if !saw_dot {
        dp = digits;
    }
    let mantissa_end = i;
    if hex {
        dp *= 4;
        mantissa_digits *= 4;
    }

    let mut exp_written = 0i64;
    if bytes.get(i).map(u8::to_ascii_lowercase) == Some(exp_char) {
        i += 1;
        let mut sign = 1;
        match bytes.get(i) {
            Some(b'+') => i += 1,
            Some(b'-') => {
                sign = -1;
                i += 1;
            }
            _ => {}
        }
        if !bytes.get(i).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        let mut e = 0i64;
        while let Some(&c) = bytes.get(i) {
            match c {
                b'_' => underscores = true,
                b'0'..=b'9' => {
                    if e < 10000 {
                        e = e * 10 + i64::from(c - b'0');
                    }
                }
                _ => break,
            }
            i += 1;
        }
        exp_written = e * sign;
        dp += exp_written;
    } else if hex {
        // A hexadecimal mantissa needs an exponent.
        return None;
    }

    let exp = if mantissa != 0 {
        dp - mantissa_digits
    } else {
        0
    };
    if underscores && !underscore_ok(&s[..i]) {
        return None;
    }
    Some(Float {
        negative,
        hex,
        mantissa,
        truncated,
        exp,
        mantissa_end,
        decimal_exp: exp_written,
        end: i,
    })
}

/// `underscoreOK`: underscores may only separate digits, or a base prefix
/// from a digit.
fn underscore_ok(s: &str) -> bool {
    let mut bytes = s.as_bytes();
    if let [b'+' | b'-', rest @ ..] = bytes {
        bytes = rest;
    }
    // What came last: the start, a digit (or base prefix), an underscore, or
    // anything else.
    #[derive(PartialEq)]
    enum Saw {
        Start,
        Digit,
        Underscore,
        Other,
    }
    let mut saw = Saw::Start;
    let mut hex = false;
    let mut i = 0;
    if bytes.len() >= 2
        && bytes[0] == b'0'
        && matches!(bytes[1].to_ascii_lowercase(), b'b' | b'o' | b'x')
    {
        i = 2;
        saw = Saw::Digit;
        hex = bytes[1].eq_ignore_ascii_case(&b'x');
    }
    for &c in &bytes[i..] {
        if c.is_ascii_digit() || hex && (b'a'..=b'f').contains(&c.to_ascii_lowercase()) {
            saw = Saw::Digit;
        } else if c == b'_' {
            if saw != Saw::Digit {
                return false;
            }
            saw = Saw::Underscore;
        } else if saw == Saw::Underscore {
            return false;
        } else {
            saw = Saw::Other;
        }
    }
    saw != Saw::Underscore
}

/// `atofHex` for `float64`: rounds `mantissa` × 2^`exp` to the nearest
/// `f64`, ties to even. `truncated` says that nonzero digits were cut off.
fn atof_hex(mut mantissa: u64, exp: i64, negative: bool, truncated: bool) -> f64 {
    const MANT_BITS: u32 = 52;
    const EXP_BITS: u32 = 11;
    const BIAS: i64 = -1023;
    let max_exp = (1 << EXP_BITS) + BIAS - 2;
    let min_exp = BIAS + 1;
    // The mantissa is now taken to be divided by 2^52.
    let mut exp = exp + i64::from(MANT_BITS);

    // A leading 1 bit, 52 more, and two for rounding, the last of which
    // records whether any bit after it was 1.
    while mantissa != 0 && mantissa >> (MANT_BITS + 2) == 0 {
        mantissa <<= 1;
        exp -= 1;
    }
    if truncated {
        mantissa |= 1;
    }
    while mantissa >> (1 + MANT_BITS + 2) != 0 {
        mantissa = mantissa >> 1 | mantissa & 1;
        exp += 1;
    }
    // Denormalize an exponent too small to represent.
    while mantissa > 1 && exp < min_exp - 2 {
        mantissa = mantissa >> 1 | mantissa & 1;
        exp += 1;
    }

    let mut round = mantissa & 3;
    mantissa >>= 2;
    round |= mantissa & 1;
    exp += 2;
    if round == 3 {
        mantissa += 1;
        if mantissa == 1 << (1 + MANT_BITS) {
            mantissa >>= 1;
            exp += 1;
        }
    }
    if mantissa >> MANT_BITS == 0 {
        // Subnormal or zero.
        exp = BIAS;
    }
    if exp > max_exp {
        mantissa = 1 << MANT_BITS;
        exp = max_exp + 1;
    }

    let mut bits = mantissa & ((1 << MANT_BITS) - 1);
    // `exp - BIAS` is in 0..=2047 here.
    bits |= ((exp - BIAS) as u64 & ((1 << EXP_BITS) - 1)) << MANT_BITS;
    if negative {
        bits |= 1 << (MANT_BITS + EXP_BITS);
    }
    f64::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal() {
        assert_eq!(parse_float("1"), 1.0);
        assert_eq!(parse_float("-2.5"), -2.5);
        assert_eq!(parse_float("+.5"), 0.5);
        assert_eq!(parse_float("5."), 5.0);
        assert_eq!(parse_float("1e3"), 1000.0);
        assert_eq!(parse_float("1E+3"), 1000.0);
        assert_eq!(parse_float("1_000.0"), 1000.0);
        assert_eq!(parse_float("1_0e1_0"), 1e11);
        assert_eq!(parse_float("1e400"), f64::INFINITY);
        assert_eq!(parse_float("-1e400"), f64::NEG_INFINITY);
        assert_eq!(parse_float("1e-400"), 0.0);
        assert_eq!(parse_float("0.1"), 0.1);
    }

    #[test]
    fn go_caps_the_exponent() {
        // Go reads the exponent 100005 as 10000.
        let tiny = format!("0.{}1e100005", "0".repeat(10_010));
        assert_eq!(parse_float(&tiny), 1e-11);
    }

    #[test]
    fn hexadecimal() {
        assert_eq!(parse_float("0x1p0"), 1.0);
        assert_eq!(parse_float("0X1.8P1"), 3.0);
        assert_eq!(parse_float("-0x1p-2"), -0.25);
        assert_eq!(parse_float("0x_1p0"), 1.0);
        assert_eq!(parse_float("0x.8p1"), 1.0);
        assert_eq!(parse_float("0x1p-1074"), f64::from_bits(1));
        assert_eq!(parse_float("0x1p-1075"), 0.0);
        assert_eq!(parse_float("0x1.8p-1074"), f64::from_bits(2));
        assert_eq!(parse_float("0x1p1024"), f64::INFINITY);
        assert_eq!(parse_float("0x1.fffffffffffff8p1023"), f64::INFINITY);
        assert_eq!(parse_float("0x1.fffffffffffff7ffp1023"), f64::MAX);
        assert_eq!(parse_float("0x10000000000000001p0"), 18446744073709551616.0);
        assert_eq!(parse_float("0x1p"), 0.0);
        assert_eq!(parse_float("0x1"), 0.0);
        assert_eq!(parse_float("0x"), 0.0);
    }

    #[test]
    fn special_values() {
        assert_eq!(parse_float("inf"), f64::INFINITY);
        assert_eq!(parse_float("-Infinity"), f64::NEG_INFINITY);
        assert_eq!(parse_float("+INF"), f64::INFINITY);
        assert!(parse_float("NaN").is_nan());
        assert_eq!(parse_float("+nan"), 0.0);
        assert_eq!(parse_float("infin"), 0.0);
    }

    #[test]
    fn syntax_errors_give_zero() {
        for s in [
            "", " 1", "1 ", "1e", "e1", ".", "_1", "1_", "1__0", "1_.0", "1._0", "0b1", "1x",
            "--1", "1e+", "0x1p_1", "1.2.3",
        ] {
            assert_eq!(parse_float(s), 0.0, "{s:?}");
        }
    }
}
