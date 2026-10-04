// Ported from Go's internal/strconv/ftoa.go (genericFtoa, bigFtoa,
// roundShortest, formatDigits, fmtE, fmtF) and decimal.go (decimal), and
// encoding/json's encode.go (floatEncoder.encode) (go1.26.4, BSD-3-Clause,
// see licenses/Go-LICENSE).
// https://github.com/golang/go

//! Go's shortest formatting of a float64: `strconv.FormatFloat(f, 'f', -1,
//! 64)`, `strconv.FormatFloat(f, 'g', -1, 64)` (also `fmt`'s `%v`), and
//! `encoding/json`'s.
//!
//! The digits are the shortest decimal that reads back as the same float64,
//! the nearest when two are as short, ties to even. Go reaches them with
//! Dragonbox; this ports its exact path, `bigFtoa`, which Go's tests hold to
//! the same answers. Rust's own formatting finds as many digits, but rounds
//! a tie up: Go writes `2156163594508435.25` as `2156163594508435.2`, Rust
//! as `2156163594508435.3`.
//!
//! Deviations from upstream: none.

/// Go's `strconv.FormatFloat(f, 'f', -1, 64)`: the shortest decimal that
/// reads back as `f`, with no exponent.
pub fn format_float(f: f64) -> String {
    format(f, Fmt::F)
}

/// Go's `strconv.FormatFloat(f, 'g', -1, 64)`, which `fmt` prints for `%v`:
/// as [`format_float`], but as `%e` writes it, with an exponent of at least
/// two digits, when the exponent is below -4 or from 6.
pub fn format_float_g(f: f64) -> String {
    format(f, Fmt::G)
}

/// A float64 as Go's `encoding/json` writes it: as [`format_float`], but
/// with an exponent below 1e-6 and from 1e21, written without a leading
/// zero. Infinities and NaN are written as [`format_float`] writes them,
/// though Go's encoder refuses them.
pub fn json_float(f: f64) -> String {
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let mut out = format(f, Fmt::E);
        // Clean up e-09 to e-9.
        if let Some(at) = out.len().checked_sub(4)
            && out.get(at..at + 3) == Some("e-0")
        {
            let last = out.pop();
            out.pop();
            out.extend(last);
        }
        out
    } else {
        format(f, Fmt::F)
    }
}

/// A `strconv` format.
#[derive(Clone, Copy)]
enum Fmt {
    /// `'e'`: `-d.ddde±dd`.
    E,
    /// `'f'`: `-ddd.ddd`.
    F,
    /// `'g'`: `'e'` for large and small exponents, else `'f'`.
    G,
}

/// `genericFtoa` with a precision of -1 and a bit size of 64, as `bigFtoa`
/// computes it.
fn format(f: f64, fmt: Fmt) -> String {
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
    let nd = digits.nd as isize;

    let mut out = String::new();
    if neg {
        out.push('-');
    }
    match fmt {
        Fmt::E => digits.write_e(&mut out, nd - 1),
        Fmt::F => digits.write_f(&mut out, (nd - digits.dp).max(0)),
        Fmt::G => {
            // `formatDigits` for the shortest digits: `%e` when the
            // exponent is under -4 or from 6, with as many digits as there
            // are.
            let exp = digits.dp - 1;
            if !(-4..6).contains(&exp) {
                digits.write_e(&mut out, nd - 1);
            } else {
                digits.write_f(&mut out, (nd - digits.dp).max(0));
            }
        }
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

    /// Go's `fmtE`: the first digit, then `.` and `prec` more if `prec` is
    /// positive, then the exponent, with at least two digits.
    fn write_e(&self, out: &mut String, prec: isize) {
        out.push(char::from(self.digit(0)));
        if prec > 0 {
            out.push('.');
            for i in 1..=prec {
                out.push(char::from(self.digit(i)));
            }
        }
        // Zero has exponent 0.
        let exp = if self.nd == 0 { 0 } else { self.dp - 1 };
        let sign = if exp < 0 { '-' } else { '+' };
        out.push('e');
        out.push(sign);
        let exp = exp.unsigned_abs();
        if exp < 10 {
            out.push('0');
        }
        out.push_str(&exp.to_string());
    }

    /// Go's `fmtF`: the integer part, padded with zeros, then `.` and
    /// `prec` digits of fraction if `prec` is positive.
    fn write_f(&self, out: &mut String, prec: isize) {
        if self.dp > 0 {
            for i in 0..self.dp {
                out.push(char::from(self.digit(i)));
            }
        } else {
            out.push('0');
        }
        if prec > 0 {
            out.push('.');
            for i in 0..prec {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Not upstream's: Go's `json.Marshal` of float64s given by their bits,
    /// recorded from Go 1.26.4. This formatter matched Go on all 799,915
    /// floats of the recording; Rust's own formatting differed on 1,363,
    /// ties like the first fifteen here, which it rounds up. The first five
    /// are the review's.
    #[test]
    fn json_floats_round_ties_as_go_rounds_them() {
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
            assert_eq!(json_float(f64::from_bits(bits)), want, "{bits:016x}");
        }
        let tie: f64 = "2156163594508435.25".parse().unwrap();
        assert_eq!(json_float(tie), "2156163594508435.2");
        assert_eq!(json_float(f64::NAN), "NaN");
        assert_eq!(json_float(f64::NEG_INFINITY), "-Inf");
    }

    /// Not upstream's: Go 1.26.4's `strconv.FormatFloat(f, 'f', -1, 64)` and
    /// `strconv.FormatFloat(f, 'g', -1, 64)`, which `fmt.Sprint` matched, of
    /// floats parsed from text. From `2156163594508435.25` on, each is
    /// halfway between two shortest decimals.
    #[test]
    fn f_and_g_match_go() {
        let max = format!("17976931348623157{}", "0".repeat(292));
        let min_normal = format!("0.{}22250738585072014", "0".repeat(307));
        let min = format!("0.{}5", "0".repeat(323));
        let floats: &[(&str, &str, &str)] = &[
            ("0", "0", "0"),
            ("-0", "-0", "-0"),
            ("1", "1", "1"),
            ("-1", "-1", "-1"),
            ("0.5", "0.5", "0.5"),
            ("1.5", "1.5", "1.5"),
            ("100", "100", "100"),
            ("123456", "123456", "123456"),
            ("999999", "999999", "999999"),
            ("1e6", "1000000", "1e+06"),
            ("1234567", "1234567", "1.234567e+06"),
            ("0.0001", "0.0001", "0.0001"),
            ("0.000123", "0.000123", "0.000123"),
            ("0.00001", "0.00001", "1e-05"),
            ("0.00001234", "0.00001234", "1.234e-05"),
            ("0.1", "0.1", "0.1"),
            (
                "0.30000000000000004",
                "0.30000000000000004",
                "0.30000000000000004",
            ),
            ("1e20", "100000000000000000000", "1e+20"),
            ("1e21", "1000000000000000000000", "1e+21"),
            ("1e22", "10000000000000000000000", "1e+22"),
            ("1e-6", "0.000001", "1e-06"),
            ("1e-7", "0.0000001", "1e-07"),
            ("1e-10", "0.0000000001", "1e-10"),
            ("123456789", "123456789", "1.23456789e+08"),
            (
                "9007199254740993",
                "9007199254740992",
                "9.007199254740992e+15",
            ),
            ("1.7976931348623157e308", &max, "1.7976931348623157e+308"),
            (
                "2.2250738585072014e-308",
                &min_normal,
                "2.2250738585072014e-308",
            ),
            ("5e-324", &min, "5e-324"),
            (
                "2156163594508435.25",
                "2156163594508435.2",
                "2.1561635945084352e+15",
            ),
            (
                "-628643006909686.25",
                "-628643006909686.2",
                "-6.286430069096862e+14",
            ),
            (
                "-191224687729131.625",
                "-191224687729131.62",
                "-1.9122468772913162e+14",
            ),
            (
                "-29290947659102.0625",
                "-29290947659102.062",
                "-2.9290947659102062e+13",
            ),
            (
                "1166311761237644.25",
                "1166311761237644.2",
                "1.1663117612376442e+15",
            ),
            (
                "1125899906842624.25",
                "1125899906842624.2",
                "1.1258999068426242e+15",
            ),
            (
                "1125899906842624.75",
                "1125899906842624.8",
                "1.1258999068426248e+15",
            ),
            (
                "2.98023223876953125e-8",
                "0.000000029802322387695312",
                "2.9802322387695312e-08",
            ),
            (
                "-2.98023223876953125e-8",
                "-0.000000029802322387695312",
                "-2.9802322387695312e-08",
            ),
            (
                "254563721393515.125",
                "254563721393515.12",
                "2.5456372139351512e+14",
            ),
            (
                "-3790767040780.40625",
                "-3790767040780.4062",
                "-3.7907670407804062e+12",
            ),
            (
                "837825883979883.25",
                "837825883979883.2",
                "8.378258839798832e+14",
            ),
            (
                "88738445599126.125",
                "88738445599126.12",
                "8.873844559912612e+13",
            ),
        ];
        for &(text, f, g) in floats {
            let float: f64 = text.parse().unwrap();
            assert_eq!(format_float(float), f, "{text}");
            assert_eq!(format_float_g(float), g, "{text}");
        }
        for (float, text) in [
            (f64::INFINITY, "+Inf"),
            (f64::NEG_INFINITY, "-Inf"),
            (f64::NAN, "NaN"),
        ] {
            assert_eq!(format_float(float), text);
            assert_eq!(format_float_g(float), text);
            assert_eq!(json_float(float), text);
        }
    }

    /// `%e` text as its digits, without the sign or point, and its exponent.
    fn digits_and_exponent(text: &str) -> (String, i32) {
        let (mantissa, exponent) = text.split_once('e').unwrap();
        let digits = mantissa.trim_start_matches('-').replace('.', "");
        (digits, exponent.parse().unwrap())
    }

    /// Not upstream's: on floats from a fixed generator, the digits read
    /// back as the float and are as many as Rust's shortest. Where they
    /// differ from Rust's, the float is exactly halfway between the two,
    /// and these are even.
    #[test]
    fn digits_are_shortest_and_ties_go_to_even() {
        // splitmix64.
        let mut state: u64 = 13;
        let mut next = move || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        let mut ties = 0;
        for i in 0..60_000 {
            let float = if i % 2 == 0 {
                f64::from_bits(next())
            } else {
                // A binary fraction near 2^53, so that its exact decimal is
                // often one digit longer than the shortest, ending in 5.
                let mant = next() >> 11;
                let scale = (next() % 80) as i32 - 70;
                mant as f64 * 2f64.powi(scale)
            };
            if !float.is_finite() || float == 0.0 {
                continue;
            }
            let ours = format(float, Fmt::E);
            assert_eq!(ours.parse::<f64>().unwrap(), float, "{ours}");
            let (digits, exp) = digits_and_exponent(&ours);
            let (rust, rust_exp) = digits_and_exponent(&format!("{float:e}"));
            assert_eq!(digits.len(), rust.len(), "{ours}");
            assert_eq!(exp, rust_exp, "{ours}");
            if digits == rust {
                continue;
            }
            ties += 1;
            let ours_n: u64 = digits.parse().unwrap();
            let rust_n: u64 = rust.parse().unwrap();
            assert_eq!(rust_n, ours_n + 1, "{ours}");
            assert!(ours_n.is_multiple_of(2), "{ours}");
            // The float's exact decimal is ours with a 5 after it.
            let bits = float.abs().to_bits();
            let biased = (bits >> MANT_BITS) as i32;
            let mut mant = bits & ((1 << MANT_BITS) - 1);
            let exponent = if biased == 0 {
                BIAS + 1
            } else {
                mant |= 1 << MANT_BITS;
                biased + BIAS
            };
            let mut exact = Decimal::new(mant);
            exact.shift(exponent - MANT_BITS as i32);
            assert!(!exact.trunc);
            let exact_digits: String = (0..exact.nd as isize)
                .map(|i| char::from(exact.digit(i)))
                .collect();
            assert_eq!(exact_digits, format!("{digits}5"), "{ours}");
            assert_eq!(exact.dp - 1, exp as isize, "{ours}");
        }
        assert!(ties > 100, "{ties} ties");
    }
}
