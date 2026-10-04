// Ported from gin-gonic/gin v1.10.1 render/json.go (JSON.Render,
// WriteJSON) (MIT) and Go's encoding/json encode.go (Marshal of maps,
// structs, strings and float64s) and time's Time.MarshalJSON (go1.27,
// BSD-3-Clause), as CLIProxyAPI internal/api/handlers/management uses them
// through c.JSON (v8.0.10, MIT). Float64s are written with a port of Go's
// strconv internal/strconv/ftoa.go (bigFtoa, roundShortest, fmtE, fmtF) and
// decimal.go (decimal) (go1.26.4, BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/gin-gonic/gin
// https://github.com/golang/go

//! Response bodies, written as gin's `c.JSON` writes them with Go's
//! `encoding/json`: a map's keys sorted, a struct's fields in their order,
//! `<`, `>`, `&`, U+2028 and U+2029 escaped, each byte that isn't part of
//! valid UTF-8 written as U+FFFD, numbers decoded from JSON written as
//! float64s, and times in RFC 3339 with nanoseconds, trailing zeros
//! dropped.
//!
//! A float64 is written as `strconv` writes it, the shortest decimal that
//! reads back the same, the nearest when two are as short, ties to even.
//! Go reaches those digits with Ryu; this ports its exact path, `bigFtoa`,
//! which Go's tests hold to the same answers.
//!
//! Deviations from upstream:
//! - A value decoded into `any` comes from serde_json, which read it with
//!   the credential: an integer `-0` is written back as `0`, where Go
//!   writes `-0`, and a number beyond float64's range, which Go fails to
//!   decode, is written as it was read.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use open_ferry_core::auth::Timestamp;
use serde_json::Value;

use crate::go::decode_rune;

/// A value to write as JSON.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i64),
    /// A `uint64`.
    Uint(u64),
    /// A string.
    Str(String),
    /// A Go string, which may hold bytes that aren't UTF-8.
    Bytes(Vec<u8>),
    /// A `time.Time`.
    Time(Timestamp),
    Array(Vec<Json>),
    /// A struct: fields in declaration order.
    Struct(Vec<(&'static str, Json)>),
    /// A map, or a `gin.H`: keys in byte order.
    Map(BTreeMap<String, Json>),
    /// A value decoded into Go's `any`: objects sorted, numbers float64.
    Any(Value),
}

impl Json {
    /// A `gin.H` from its entries.
    pub(crate) fn map<const N: usize>(entries: [(&str, Json); N]) -> Self {
        Self::Map(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }

    /// The value as JSON text.
    pub(crate) fn encode(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Self::Uint(n) => {
                let _ = write!(out, "{n}");
            }
            Self::Str(s) => write_string(out, s.as_bytes()),
            Self::Bytes(bytes) => write_string(out, bytes),
            Self::Time(time) => {
                out.push('"');
                out.push_str(&go_time(*time));
                out.push('"');
            }
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Struct(fields) => {
                out.push('{');
                for (i, (name, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(out, name.as_bytes());
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
            Self::Map(entries) => {
                out.push('{');
                for (i, (key, value)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(out, key.as_bytes());
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
            Self::Any(value) => write_any(out, value),
        }
    }
}

/// Writes a value decoded into `any` as Go writes it back.
fn write_any(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(number) => match number.as_f64() {
            Some(f) => out.push_str(&format_float(f)),
            // Out of float64's range; Go wouldn't have decoded it.
            None => out.push_str(&number.to_string()),
        },
        Value::String(s) => write_string(out, s.as_bytes()),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_any(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key.as_bytes());
                out.push(':');
                write_any(out, item);
            }
            out.push('}');
        }
    }
}

/// A float64 as Go's encoder writes it: the shortest decimal that reads
/// back the same, ties to even, with an exponent below 1e-6 and from 1e21.
/// Infinities and NaN are written as `strconv.FormatFloat` writes them,
/// though Go's encoder refuses them.
pub(crate) fn format_float(f: f64) -> String {
    let bits = f.to_bits();
    let neg = bits >> 63 != 0;
    let biased = (bits >> MANT_BITS) & 0x7ff;
    let mut mant = bits & ((1 << MANT_BITS) - 1);
    if biased == 0x7ff {
        let text = match (mant != 0, neg) {
            (true, _) => "NaN",
            (false, true) => "-Inf",
            (false, false) => "+Inf",
        };
        return text.to_owned();
    }
    let mut exp = biased as i32;
    if exp == 0 {
        // Denormal.
        exp += 1;
    } else {
        mant |= 1 << MANT_BITS;
    }
    exp += BIAS;

    let mut digits = Decimal::new(mant);
    digits.shift(exp - MANT_BITS as i32);
    round_shortest(&mut digits, mant, exp);

    let mut out = String::new();
    if neg {
        out.push('-');
    }
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        digits.write_e(&mut out);
    } else {
        digits.write_f(&mut out);
    }
    out
}

/// float64's mantissa bits, less the implicit one.
const MANT_BITS: u32 = 52;

/// float64's exponent bias.
const BIAS: i32 = -1023;

/// Digits a [`Decimal`] holds, enough for any float64 exactly.
const DECIMAL_DIGITS: usize = 800;

/// The most bits a [`Decimal`] is shifted by at once, so that the
/// arithmetic fits in a `u64`: Go's `maxShift` on a 64-bit machine.
const MAX_SHIFT: u32 = 60;

/// Go's `strconv.decimal`: a number as ASCII digits, most significant
/// first, with the decimal point after `dp` of them.
struct Decimal {
    d: [u8; DECIMAL_DIGITS],
    /// Digits used.
    nd: usize,
    /// Where the decimal point is, from the first digit.
    dp: isize,
    /// Nonzero digits were dropped past `d[..nd]`.
    trunc: bool,
}

impl Decimal {
    /// `v`, as Go's `Assign` sets it.
    fn new(v: u64) -> Self {
        let mut decimal = Self {
            d: [b'0'; DECIMAL_DIGITS],
            nd: 0,
            dp: 0,
            trunc: false,
        };
        let text = v.to_string();
        for (slot, digit) in decimal.d.iter_mut().zip(text.bytes()) {
            *slot = digit;
        }
        decimal.nd = text.len().min(DECIMAL_DIGITS);
        decimal.dp = decimal.nd as isize;
        decimal.trim();
        decimal
    }

    /// The digit at `i`, or `0` when `i` is outside `d[..nd]`.
    fn digit(&self, i: isize) -> u8 {
        match usize::try_from(i) {
            Ok(i) if i < self.nd => self.d.get(i).copied().unwrap_or(b'0'),
            _ => b'0',
        }
    }

    /// Sets the digit at `i`, if it's held.
    fn set(&mut self, i: usize, digit: u8) {
        if let Some(slot) = self.d.get_mut(i) {
            *slot = digit;
        }
    }

    /// Drops trailing zeros.
    fn trim(&mut self) {
        while self.nd > 0 && self.digit(self.nd as isize - 1) == b'0' {
            self.nd -= 1;
        }
        if self.nd == 0 {
            self.dp = 0;
        }
    }

    /// Multiplies by 2^`k`, or divides by 2^-`k`, as Go's `Shift` does.
    fn shift(&mut self, k: i32) {
        if self.nd == 0 {
            return;
        }
        let mut k = k;
        if k > 0 {
            while k > MAX_SHIFT as i32 {
                self.left_shift(MAX_SHIFT);
                k -= MAX_SHIFT as i32;
            }
            self.left_shift(k.unsigned_abs());
        } else if k < 0 {
            while k < -(MAX_SHIFT as i32) {
                self.right_shift(MAX_SHIFT);
                k += MAX_SHIFT as i32;
            }
            self.right_shift(k.unsigned_abs());
        }
    }

    /// Divides by 2^`k`, as Go's `rightShift` does.
    fn right_shift(&mut self, k: u32) {
        let mut r = 0; // read index
        let mut w = 0; // write index

        // Pick up enough leading digits to cover the first shifted digit.
        let mut n: u64 = 0;
        while n >> k == 0 {
            if r >= self.nd {
                if n == 0 {
                    // The number is zero.
                    self.nd = 0;
                    return;
                }
                while n >> k == 0 {
                    n *= 10;
                    r += 1;
                }
                break;
            }
            n = n * 10 + u64::from(self.digit(r as isize) - b'0');
            r += 1;
        }
        self.dp -= r as isize - 1;

        let mask = (1 << k) - 1;

        // Pick up a digit, put down a digit.
        while r < self.nd {
            let c = u64::from(self.digit(r as isize) - b'0');
            let digit = n >> k;
            n &= mask;
            self.set(w, digit as u8 + b'0');
            w += 1;
            n = n * 10 + c;
            r += 1;
        }

        // Put down the extra digits.
        while n > 0 {
            let digit = n >> k;
            n &= mask;
            if w < DECIMAL_DIGITS {
                self.set(w, digit as u8 + b'0');
                w += 1;
            } else if digit > 0 {
                self.trunc = true;
            }
            n *= 10;
        }

        self.nd = w;
        self.trim();
    }

    /// Multiplies by 2^`k`, as Go's `leftShift` does. Go counts the new
    /// digits from a table first, to write them in place; these are
    /// counted as they're made, least significant first.
    fn left_shift(&mut self, k: u32) {
        let mut made = Vec::with_capacity(self.nd + 20);
        let mut n: u64 = 0;
        for r in (0..self.nd).rev() {
            n += u64::from(self.digit(r as isize) - b'0') << k;
            made.push((n % 10) as u8 + b'0');
            n /= 10;
        }
        while n > 0 {
            made.push((n % 10) as u8 + b'0');
            n /= 10;
        }
        let delta = made.len() - self.nd;

        // Keep the most significant digits that fit.
        let kept = made.len().min(DECIMAL_DIGITS);
        let dropped = made.len() - kept;
        if made.iter().take(dropped).any(|&digit| digit != b'0') {
            self.trunc = true;
        }
        for (w, &digit) in made.iter().rev().take(kept).enumerate() {
            self.set(w, digit);
        }
        self.nd = kept;
        self.dp += delta as isize;
        self.trim();
    }

    /// Whether rounding to `nd` digits rounds up: half to even, unless
    /// digits were dropped past the half.
    fn should_round_up(&self, nd: isize) -> bool {
        if nd < 0 || nd >= self.nd as isize {
            return false;
        }
        if self.digit(nd) == b'5' && nd + 1 == self.nd as isize {
            // Exactly halfway: round to even.
            if self.trunc {
                return true;
            }
            return nd > 0 && !(self.digit(nd - 1) - b'0').is_multiple_of(2);
        }
        self.digit(nd) >= b'5'
    }

    /// Rounds to `nd` digits, to the nearest.
    fn round(&mut self, nd: isize) {
        if nd < 0 || nd >= self.nd as isize {
            return;
        }
        if self.should_round_up(nd) {
            self.round_up(nd);
        } else {
            self.round_down(nd);
        }
    }

    /// Truncates to `nd` digits.
    fn round_down(&mut self, nd: isize) {
        if nd < 0 || nd >= self.nd as isize {
            return;
        }
        self.nd = nd.unsigned_abs();
        self.trim();
    }

    /// Rounds up to `nd` digits.
    fn round_up(&mut self, nd: isize) {
        if nd < 0 || nd >= self.nd as isize {
            return;
        }
        // Round up the last digit that isn't a 9.
        for i in (0..nd.unsigned_abs()).rev() {
            let digit = self.digit(i as isize);
            if digit < b'9' {
                self.set(i, digit + 1);
                self.nd = i + 1;
                return;
            }
        }
        // All 9s: 999 becomes 1000.
        self.set(0, b'1');
        self.nd = 1;
        self.dp += 1;
    }

    /// The digits in Go's `%e` with the fewest digits that hold them, its
    /// exponent's leading zero dropped as Go's encoder drops it.
    fn write_e(&self, out: &mut String) {
        out.push(char::from(self.digit(0)));
        if self.nd > 1 {
            out.push('.');
            for i in 1..self.nd {
                out.push(char::from(self.digit(i as isize)));
            }
        }
        let exp = if self.nd == 0 { 0 } else { self.dp - 1 };
        // Go writes at least two digits, then drops a leading zero after
        // a `-`.
        if exp < 0 {
            let _ = write!(out, "e-{}", exp.unsigned_abs());
        } else {
            let _ = write!(out, "e+{exp:02}");
        }
    }

    /// The digits in Go's `%f` with the fewest digits that hold them.
    fn write_f(&self, out: &mut String) {
        if self.dp > 0 {
            for i in 0..self.dp {
                out.push(char::from(self.digit(i)));
            }
        } else {
            out.push('0');
        }
        let fraction = self.nd as isize - self.dp;
        if fraction > 0 {
            out.push('.');
            for i in 0..fraction {
                out.push(char::from(self.digit(self.dp + i)));
            }
        }
    }
}

/// Rounds `d`, which holds mant×2^(exp-52) exactly, to the shortest
/// decimal that reads back as the same float64, as Go's `roundShortest`
/// does: the nearest one when two are as short, ties to even.
fn round_shortest(d: &mut Decimal, mant: u64, exp: i32) {
    if mant == 0 {
        d.nd = 0;
        return;
    }

    // Already shortest if the closest shorter number, 10^(dp-nd) away, is
    // farther than the bounds, at most 2^(exp-mantbits) away.
    let min_exp = BIAS + 1;
    let mant_bits = MANT_BITS as i32;
    if exp > min_exp && 332 * (d.dp - d.nd as isize) >= 100 * (exp - mant_bits) as isize {
        return;
    }

    // Halfway to the next float64 up, and to the next one down, which is
    // closer when mant-1 loses the leading bit.
    let mut upper = Decimal::new(mant * 2 + 1);
    upper.shift(exp - mant_bits - 1);
    let (mant_lo, exp_lo) = if mant > 1 << MANT_BITS || exp == min_exp {
        (mant - 1, exp)
    } else {
        (mant * 2 - 1, exp - 1)
    };
    let mut lower = Decimal::new(mant_lo * 2 + 1);
    lower.shift(exp_lo - mant_bits - 1);

    // The bounds read back as this float64, ties to even, only when its
    // mantissa is even.
    let inclusive = mant.is_multiple_of(2);

    // 0 while d and upper have the same digits; 1 once they've differed by
    // one and since only 9s in d met 0s in upper, so rounding up may fall
    // outside an exclusive bound; 2 once rounding up is within it.
    let mut upper_delta = 0;

    // Walk the digits until d differs from upper and lower. The decimal
    // points may differ, upper's being the furthest right.
    let mut ui: isize = 0;
    loop {
        let mi = ui - upper.dp + d.dp;
        if mi >= d.nd as isize {
            break;
        }
        let li = ui - upper.dp + lower.dp;
        let l = lower.digit(li);
        let m = d.digit(mi);
        let u = upper.digit(ui);

        // Truncating is fine if lower has a different digit, or if it is
        // inclusive and this is its last digit.
        let ok_down = l != m || (inclusive && li + 1 == lower.nd as isize);

        if upper_delta == 0 && m + 1 < u {
            upper_delta = 2;
        } else if upper_delta == 0 && m != u {
            upper_delta = 1;
        } else if upper_delta == 1 && (m != b'9' || u != b'0') {
            upper_delta = 2;
        }
        // Rounding up is fine if upper has a different digit and is
        // inclusive or bigger than the rounded number.
        let ok_up = upper_delta > 0 && (inclusive || upper_delta > 1 || ui + 1 < upper.nd as isize);

        match (ok_down, ok_up) {
            (true, true) => {
                d.round(mi + 1);
                return;
            }
            (true, false) => {
                d.round_down(mi + 1);
                return;
            }
            (false, true) => {
                d.round_up(mi + 1);
                return;
            }
            (false, false) => {}
        }
        ui += 1;
    }
}

/// A time as Go's `MarshalJSON` writes a UTC time: RFC 3339 with up to
/// nine digits of fraction, trailing zeros dropped.
pub(crate) fn go_time(time: Timestamp) -> String {
    let mut out = time.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = time.timestamp_subsec_nanos().min(999_999_999);
    if nanos != 0 {
        let fraction = format!("{nanos:09}");
        out.push('.');
        out.push_str(fraction.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

/// Writes `bytes` as a JSON string, escaped as Go escapes it for HTML.
pub(crate) fn write_string(out: &mut String, bytes: &[u8]) {
    out.push('"');
    let mut rest = bytes;
    while !rest.is_empty() {
        let Some((c, width)) = decode_rune(rest) else {
            push_unicode_escape(out, 0xfffd);
            rest = &rest[1..];
            continue;
        };
        rest = &rest[width..];
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' || matches!(c, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') => {
                push_unicode_escape(out, u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Writes a backslash, `u` and four lowercase hex digits.
fn push_unicode_escape(out: &mut String, code: u32) {
    out.push('\\');
    let _ = write!(out, "u{code:04x}");
}

/// A response with `body` as JSON, as gin's `c.JSON` sends it.
pub(crate) fn response(status: StatusCode, body: &Json) -> Response {
    let mut response = (status, body.encode()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// `{"error":message}` with `status`.
pub(crate) fn error(status: StatusCode, message: &str) -> Response {
    response(
        status,
        &Json::map([("error", Json::Str(message.to_owned()))]),
    )
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono::Utc;
    use serde_json::json;

    use super::*;

    /// A backslash, `u` and `hex`, built so the source holds no escape.
    fn u(hex: &str) -> String {
        format!("{}u{hex}", '\\')
    }

    #[test]
    fn strings_are_escaped_as_go_escapes_them() {
        let mut out = String::new();
        write_string(&mut out, "a\"b\\c\n\r\t\u{8}\u{c}\u{1}\u{7f}".as_bytes());
        assert_eq!(
            out,
            format!(r#""a\"b\\c\n\r\t\b\f{}{}""#, u("0001"), '\u{7f}')
        );

        let mut out = String::new();
        write_string(&mut out, "<a>&\u{2028}\u{2029}\u{e9}".as_bytes());
        assert_eq!(
            out,
            format!(
                "\"{}a{}{}{}{}\u{e9}\"",
                u("003c"),
                u("003e"),
                u("0026"),
                u("2028"),
                u("2029")
            )
        );

        // One replacement per byte that isn't valid UTF-8.
        let mut out = String::new();
        write_string(&mut out, b"x\xe2\x82y\xff");
        assert_eq!(out, format!("\"x{0}{0}y{0}\"", u("fffd")));
    }

    #[test]
    fn floats_are_written_as_go_writes_them() {
        assert_eq!(format_float(1.0), "1");
        assert_eq!(format_float(-0.0), "-0");
        assert_eq!(format_float(0.5), "0.5");
        assert_eq!(format_float(1e20), "100000000000000000000");
        assert_eq!(format_float(1e21), "1e+21");
        assert_eq!(format_float(1.5e300), "1.5e+300");
        assert_eq!(format_float(0.000001), "0.000001");
        assert_eq!(format_float(1e-7), "1e-7");
        assert_eq!(format_float(-2.5e-10), "-2.5e-10");
        assert_eq!(format_float(1e-100), "1e-100");
        assert_eq!(format_float(123456789.0), "123456789");
    }

    #[test]
    fn times_drop_trailing_zeros() {
        let time = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        assert_eq!(go_time(time), "2026-01-02T03:04:05Z");
        let time = time + chrono::TimeDelta::nanoseconds(120_000_000);
        assert_eq!(go_time(time), "2026-01-02T03:04:05.12Z");
        let time = time + chrono::TimeDelta::nanoseconds(1);
        assert_eq!(go_time(time), "2026-01-02T03:04:05.120000001Z");
    }

    #[test]
    fn maps_sort_and_structs_keep_their_order() {
        let value = Json::map([
            ("b", Json::Int(1)),
            (
                "a",
                Json::Struct(vec![("z", Json::Null), ("y", Json::Bool(true))]),
            ),
        ]);
        assert_eq!(value.encode(), r#"{"a":{"z":null,"y":true},"b":1}"#);
    }

    #[test]
    fn any_values_are_written_back_as_go_decoded_them() {
        let value: Value =
            serde_json::from_str(r#"{"b":1.50,"a":[10000000000000000000000,true,null,"<"]}"#)
                .unwrap();
        assert_eq!(
            Json::Any(value).encode(),
            format!(r#"{{"a":[1e+22,true,null,"{}"],"b":1.5}}"#, u("003c"))
        );
        assert_eq!(Json::Any(json!(7)).encode(), "7");
    }

    /// Not upstream's: Go's `json.Marshal` of float64s given by their bits,
    /// recorded from Go 1.26.4. This formatter matched Go on all 799,915
    /// floats of the recording; Rust's own formatting differed on 1,363,
    /// ties like the first fifteen here, which it rounds up. The first five
    /// are the review's.
    #[test]
    fn floats_round_ties_as_go_rounds_them() {
        let floats: &[(u64, &str)] = &[
            (0xc2baa3d487c55e10, "-29290947659102.062"),
            (0xc301ddfad8f067b2, "-628643006909686.2"),
            (0xc2e5bd5ef2b33d74, "-191224687729131.62"),
            (0x431ea413d483624d, "2156163594508435.2"),
            (0x4310930478e75a31, "1166311761237644.2"),
            (0x4310000000000001, "1125899906842624.2"),
            (0x4310000000000003, "1125899906842624.8"),
            (0x3e60000000000000, "2.9802322387695312e-8"),
            (0xbe60000000000000, "-2.9802322387695312e-8"),
            (0x42ecf0c785f6ad64, "254563721393515.12"),
            (0xc28b94da95486340, "-3790767040780.4062"),
            (0xc317e80c32287eb5, "-1682265885777837.2"),
            (0xc2ef6e8fb5084964, "-276477742957131.12"),
            (0x4307cffc5196035a, "837825883979883.2"),
            (0x42d42d41e1c86588, "88738445599126.12"),
            (0x0000000000000000, "0"),
            (0x8000000000000000, "-0"),
            (0x444b1ae4d6e2ef50, "1e+21"),
            (0x444b1ae4d6e2ef4f, "999999999999999900000"),
            (0x3eb0c6f7a0b5ed8d, "0.000001"),
            (0x3eb0c6f7a0b5ed88, "9.99999999999999e-7"),
            (0x3e7ad7f29abcaf48, "1e-7"),
            (0x7fefffffffffffff, "1.7976931348623157e+308"),
            (0x0000000000000001, "5e-324"),
            (0x000fffffffffffff, "2.225073858507201e-308"),
            (0x0010000000000000, "2.2250738585072014e-308"),
            (0x44b52d02c7e14af6, "1e+23"),
            (0x447c7e83209e90b2, "8.41e+21"),
            (0x441ac53a7e04bcda, "123456789012345680000"),
            (0x3fd3333333333334, "0.30000000000000004"),
            (0x4330000000000002, "4503599627370498"),
            (0x8001dd55d94124d0, "-2.593033205008747e-309"),
            (0x000a5a624bf469df, "1.4397705297706974e-308"),
            (0x803d058b14e571aa, "-1.6143830615127162e-307"),
            (0x7fef1e737318cbb6, "1.7481975374700147e+308"),
            (0x8003fd18aa82bd77, "-5.54691113129397e-309"),
            (0xcb60000000000001, "-1.2259964326927114e+55"),
            (0xb601b3ee02cd3263, "-1.5140977314531498e-48"),
            (0xa82b3b934b8bfd81, "-3.4557525014331025e-115"),
            (0xfc730952e9cdc9e9, "-2.9682544681547563e+291"),
            (0x5e5ee3cf70ba697e, "3.8572180286594893e+146"),
            (0x32407e5cac41eadb, "1.223563368249306e-66"),
        ];
        for &(bits, want) in floats {
            assert_eq!(format_float(f64::from_bits(bits)), want, "{bits:016x}");
        }
        let tie: f64 = "2156163594508435.25".parse().unwrap();
        assert_eq!(format_float(tie), "2156163594508435.2");
        assert_eq!(format_float(f64::NAN), "NaN");
        assert_eq!(format_float(f64::NEG_INFINITY), "-Inf");
    }

    /// Go's `json.Marshal` of a string holding these bytes.
    #[test]
    fn strings_match_go() {
        let cases: &[(&[u8], &str)] = &[
            (b"\x00", "\"\x5cu0000\""),
            (b"a\x00z", "\"a\x5cu0000z\""),
            (b"\x01", "\"\x5cu0001\""),
            (b"\x02", "\"\x5cu0002\""),
            (b"\x03", "\"\x5cu0003\""),
            (b"\x04", "\"\x5cu0004\""),
            (b"\x05", "\"\x5cu0005\""),
            (b"\x06", "\"\x5cu0006\""),
            (b"\x07", "\"\x5cu0007\""),
            (b"\x08", "\"\\b\""),
            (b"\t", "\"\\t\""),
            (b"\n", "\"\\n\""),
            (b"\x0b", "\"\x5cu000b\""),
            (b"\x0c", "\"\\f\""),
            (b"\r", "\"\\r\""),
            (b"\x0e", "\"\x5cu000e\""),
            (b"\x0f", "\"\x5cu000f\""),
            (b"\x10", "\"\x5cu0010\""),
            (b"\x11", "\"\x5cu0011\""),
            (b"\x12", "\"\x5cu0012\""),
            (b"\x13", "\"\x5cu0013\""),
            (b"\x14", "\"\x5cu0014\""),
            (b"\x15", "\"\x5cu0015\""),
            (b"\x16", "\"\x5cu0016\""),
            (b"\x17", "\"\x5cu0017\""),
            (b"\x18", "\"\x5cu0018\""),
            (b"\x19", "\"\x5cu0019\""),
            (b"\x1a", "\"\x5cu001a\""),
            (b"\x1b", "\"\x5cu001b\""),
            (b"\x1c", "\"\x5cu001c\""),
            (b"\x1d", "\"\x5cu001d\""),
            (b"\x1e", "\"\x5cu001e\""),
            (b"\x1f", "\"\x5cu001f\""),
            (b"a\x1fz", "\"a\x5cu001fz\""),
            (b" ", "\" \""),
            (b"\"", "\"\\\"\""),
            (b"&", "\"\x5cu0026\""),
            (b"<", "\"\x5cu003c\""),
            (b">", "\"\x5cu003e\""),
            (b"\\", "\"\\\\\""),
            (b"\x7f", "\"\x7f\""),
            (b"a\x7fz", "\"a\x7fz\""),
            (b"\x80", "\"\x5cufffd\""),
            (b"a\x80z", "\"a\x5cufffdz\""),
            (b"a\xc0z", "\"a\x5cufffdz\""),
            (b"\xc2", "\"\x5cufffd\""),
            (b"a\xe2z", "\"a\x5cufffdz\""),
            (b"a\xf0z", "\"a\x5cufffdz\""),
            (b"\xff", "\"\x5cufffd\""),
            (b"a\xffz", "\"a\x5cufffdz\""),
            (b"\xe2\x80\xa8", "\"\x5cu2028\""),
            (b"\xe2\x80\xa9", "\"\x5cu2029\""),
            (b"\xc3\xa9", "\"\u{e9}\""),
            (b"\xf4\x8f\xbf\xbf", "\"\u{10ffff}\""),
            (b"\xef\xbf\xbd", "\"\u{fffd}\""),
            (b"<a&b>", "\"\x5cu003ca\x5cu0026b\x5cu003e\""),
            (b"\x7f", "\"\x7f\""),
            (b"\xc2\x80", "\"\u{80}\""),
            (b"\xdf\xbf", "\"\u{7ff}\""),
            (b"\xe0\xa0\x80", "\"\u{800}\""),
            (b"\xef\xbf\xbf", "\"\u{ffff}\""),
            (b"\xf0\x90\x80\x80", "\"\u{10000}\""),
            (b"plain text", "\"plain text\""),
            (b"\"quoted\" \\ back", "\"\\\"quoted\\\" \\\\ back\""),
            (b"\xe2\x80\x8b", "\"\u{200b}\""),
            (b"\xef\xbb\xbf", "\"\u{feff}\""),
            (b"\xed\xa0\x80", "\"\x5cufffd\x5cufffd\x5cufffd\""),
            (b"\xed\xbf\xbf", "\"\x5cufffd\x5cufffd\x5cufffd\""),
            (b"\xc0\x80", "\"\x5cufffd\x5cufffd\""),
            (b"\xc1\xbf", "\"\x5cufffd\x5cufffd\""),
            (b"\xe0\x80\x80", "\"\x5cufffd\x5cufffd\x5cufffd\""),
            (b"\xe0\x9f\xbf", "\"\x5cufffd\x5cufffd\x5cufffd\""),
            (
                b"\xf0\x8f\xbf\xbf",
                "\"\x5cufffd\x5cufffd\x5cufffd\x5cufffd\"",
            ),
            (
                b"\xf4\x90\x80\x80",
                "\"\x5cufffd\x5cufffd\x5cufffd\x5cufffd\"",
            ),
            (
                b"\xf5\x80\x80\x80",
                "\"\x5cufffd\x5cufffd\x5cufffd\x5cufffd\"",
            ),
            (b"\xe2\x82", "\"\x5cufffd\x5cufffd\""),
            (b"\xe2\x82x", "\"\x5cufffd\x5cufffdx\""),
            (b"\xf0\x9f\x98", "\"\x5cufffd\x5cufffd\x5cufffd\""),
            (b"\xf0\x9f\x98\x80", "\"\u{1f600}\""),
            (b"\x80\x80", "\"\x5cufffd\x5cufffd\""),
            (b"\xc2", "\"\x5cufffd\""),
            (b"\xc2\xc2\xa9", "\"\x5cufffd\u{a9}\""),
            (b"\xff\xfe", "\"\x5cufffd\x5cufffd\""),
            (b"\xe2\x80\xa8\xe2\x80", "\"\x5cu2028\x5cufffd\x5cufffd\""),
        ];
        for &(bytes, want) in cases {
            let mut out = String::new();
            write_string(&mut out, bytes);
            assert_eq!(out, want, "{bytes:?}");
        }
    }

    /// Go's `json.Marshal` of float64s, of UTC times given in nanoseconds
    /// since 1970, and of documents decoded into `any`.
    #[test]
    fn numbers_times_and_documents_match_go() {
        let floats = [
            ("0", "0"),
            ("-0", "-0"),
            ("1", "1"),
            ("1.5", "1.5"),
            ("100", "100"),
            ("1e20", "100000000000000000000"),
            ("1e21", "1e+21"),
            ("123456789012345678901", "123456789012345680000"),
            ("1e-6", "0.000001"),
            ("1e-7", "1e-7"),
            ("0.000001", "0.000001"),
            ("0.0000001", "1e-7"),
            ("123e-9", "1.23e-7"),
            ("1.7976931348623157e308", "1.7976931348623157e+308"),
            ("5e-324", "5e-324"),
            ("2.5e-8", "2.5e-8"),
            ("-1e21", "-1e+21"),
            ("1e100", "1e+100"),
            ("0.1", "0.1"),
            ("0.30000000000000004", "0.30000000000000004"),
            ("9007199254740993", "9007199254740992"),
            ("12345678.9", "12345678.9"),
            ("1E5", "100000"),
            ("1e+30", "1e+30"),
            ("-1.5e-10", "-1.5e-10"),
            ("4.9e-324", "5e-324"),
            ("999999999999999999999", "1e+21"),
            ("1000000000000000000000", "1e+21"),
            ("0.00000099", "9.9e-7"),
            ("0.000000999999", "9.99999e-7"),
            ("-0.0", "-0"),
            ("1.0", "1"),
            ("100000000000000000000", "100000000000000000000"),
            ("1e-5", "0.00001"),
            ("12e-7", "0.0000012"),
            ("3.14159", "3.14159"),
            ("-2.5e+25", "-2.5e+25"),
            ("1.23456789e-7", "1.23456789e-7"),
            ("2e-308", "2e-308"),
            ("2.2250738585072014e-308", "2.2250738585072014e-308"),
            ("123456789", "123456789"),
            ("1e6", "1000000"),
            ("1e15", "1000000000000000"),
            ("1e16", "10000000000000000"),
            ("1e17", "100000000000000000"),
        ];
        for (text, want) in floats {
            assert_eq!(format_float(text.parse().unwrap()), want, "{text}");
        }
        let times = [
            (0, "1970-01-01T00:00:00Z"),
            (1, "1970-01-01T00:00:00.000000001Z"),
            (10, "1970-01-01T00:00:00.00000001Z"),
            (100, "1970-01-01T00:00:00.0000001Z"),
            (1000, "1970-01-01T00:00:00.000001Z"),
            (123456789, "1970-01-01T00:00:00.123456789Z"),
            (120000000, "1970-01-01T00:00:00.12Z"),
            (999999999, "1970-01-01T00:00:00.999999999Z"),
            (1700000000000000000, "2023-11-14T22:13:20Z"),
            (1700000000500000000, "2023-11-14T22:13:20.5Z"),
            (1700000000000000001, "2023-11-14T22:13:20.000000001Z"),
            (-1, "1969-12-31T23:59:59.999999999Z"),
            (1700000000010000000, "2023-11-14T22:13:20.01Z"),
            (1700000000000100000, "2023-11-14T22:13:20.0001Z"),
            (253402300799999999, "1978-01-11T21:31:40.799999999Z"),
            (-62135596800000000, "1968-01-12T20:06:43.2Z"),
        ];
        for (nanos, want) in times {
            let time = chrono::DateTime::from_timestamp_nanos(nanos);
            assert_eq!(go_time(time), want, "{nanos}");
        }
        let documents = [
            (
                "{\"b\":1,\"a\":[1.0,2.50,\"x\"],\"c\":null,\"d\":true}",
                "{\"a\":[1,2.5,\"x\"],\"b\":1,\"c\":null,\"d\":true}",
            ),
            (
                "{\"k\":\"<\x5cu2028>\"}",
                "{\"k\":\"\x5cu003c\x5cu2028\x5cu003e\"}",
            ),
            (
                "{\"\x5cu00e9\":1,\"e\":2,\"E\":3,\"_\":4,\"\":5}",
                "{\"\":5,\"E\":3,\"_\":4,\"e\":2,\"\u{e9}\":1}",
            ),
            ("[1,2]", "[1,2]"),
            (
                "{\"a\":{\"b\":{\"c\":[{\"d\":\"\x5cu0000\"}]}}}",
                "{\"a\":{\"b\":{\"c\":[{\"d\":\"\x5cu0000\"}]}}}",
            ),
            ("{\"a\":1,\"a\":2}", "{\"a\":2}"),
            (
                "{\"big\":123456789012345678901234567890}",
                "{\"big\":1.2345678901234568e+29}",
            ),
            ("{\"s\":\"\\/\"}", "{\"s\":\"/\"}"),
            (
                "{\"t\":\"\x5cu00e9\x5cu0301\"}",
                "{\"t\":\"\u{e9}\u{301}\"}",
            ),
        ];
        for (text, want) in documents {
            let value: Value = serde_json::from_str(text).unwrap();
            assert_eq!(Json::Any(value).encode(), want, "{text}");
        }
    }
}
