// Ported from Go net/http header.go (ParseTime) and time format.go (Parse,
// lookup, match, getnum, cutspace, skip, atoi, leadingInt,
// parseNanoseconds, parseTimeZone, parseGMT, parseSignedOffset) (go1.26,
// BSD-3-Clause), as CLIProxyAPI
// internal/api/handlers/management/plugin_quota.go uses it (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! An HTTP date, read as Go's `http.ParseTime` reads one: in the form
//! `Mon, 02 Jan 2006 15:04:05 GMT`, else `Monday, 02-Jan-06 15:04:05 MST`,
//! else `Mon Jan _2 15:04:05 2006`, each as Go's `time.Parse` matches it.
//! Names match without regard to ASCII case, a space in the form matches
//! any run of spaces, a fraction of a second may follow the seconds, and
//! the day of the week isn't checked.
//!
//! Deviations from upstream:
//! - A zone in the second form never moves the time. Go reads it as UTC
//!   too, unless the machine's own zone has that abbreviation at that
//!   time, when it reads the time in that zone.

use chrono::{DateTime, NaiveDate, Utc};

/// One step of a form: literal text or a field.
#[derive(Clone, Copy)]
enum Step {
    Text(&'static [u8]),
    ShortWeekday,
    LongWeekday,
    /// The day of the month: two digits, or one or two after any spaces.
    Day {
        two_digits: bool,
    },
    ShortMonth,
    /// Four digits.
    LongYear,
    /// Two bytes, read as a signed number: from 69 in the 1900s, else in
    /// the 2000s.
    ShortYear,
    Hour,
    Minute,
    Second,
    Zone,
}

use Step::*;

/// `http.TimeFormat`.
const TIME_FORMAT: &[Step] = &[
    ShortWeekday,
    Text(b", "),
    Day { two_digits: true },
    Text(b" "),
    ShortMonth,
    Text(b" "),
    LongYear,
    Text(b" "),
    Hour,
    Text(b":"),
    Minute,
    Text(b":"),
    Second,
    Text(b" GMT"),
];

/// `time.RFC850`.
const RFC850: &[Step] = &[
    LongWeekday,
    Text(b", "),
    Day { two_digits: true },
    Text(b"-"),
    ShortMonth,
    Text(b"-"),
    ShortYear,
    Text(b" "),
    Hour,
    Text(b":"),
    Minute,
    Text(b":"),
    Second,
    Text(b" "),
    Zone,
];

/// `time.ANSIC`.
const ANSIC: &[Step] = &[
    ShortWeekday,
    Text(b" "),
    ShortMonth,
    Text(b" "),
    Day { two_digits: false },
    Text(b" "),
    Hour,
    Text(b":"),
    Minute,
    Text(b":"),
    Second,
    Text(b" "),
    LongYear,
];

const SHORT_WEEKDAYS: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];

const LONG_WEEKDAYS: [&[u8]; 7] = [
    b"Sunday",
    b"Monday",
    b"Tuesday",
    b"Wednesday",
    b"Thursday",
    b"Friday",
    b"Saturday",
];

const SHORT_MONTHS: [&[u8]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

/// The time `text` gives, if it is an HTTP date (`http.ParseTime`).
pub(crate) fn parse_time(text: &[u8]) -> Option<DateTime<Utc>> {
    [TIME_FORMAT, RFC850, ANSIC]
        .into_iter()
        .find_map(|form| parse(form, text))
}

/// `text` read in one form (`time.Parse`).
fn parse(form: &[Step], mut value: &[u8]) -> Option<DateTime<Utc>> {
    let (mut year, mut month, mut day) = (0, 0, 0);
    let (mut hour, mut minute, mut second, mut nanos) = (0, 0, 0, 0);
    for step in form {
        match *step {
            Text(text) => value = skip(value, text)?,
            ShortWeekday => (_, value) = lookup(&SHORT_WEEKDAYS, value)?,
            LongWeekday => (_, value) = lookup(&LONG_WEEKDAYS, value)?,
            ShortMonth => {
                let (index, rest) = lookup(&SHORT_MONTHS, value)?;
                month = index + 1;
                value = rest;
            }
            Day { two_digits } => {
                if !two_digits && value.first() == Some(&b' ') {
                    value = value.get(1..).unwrap_or_default();
                }
                (day, value) = getnum(value, two_digits)?;
            }
            LongYear => {
                if value.len() < 4 || !value.first().is_some_and(u8::is_ascii_digit) {
                    return None;
                }
                let (digits, rest) = value.split_at_checked(4)?;
                year = atoi(digits)?;
                value = rest;
            }
            ShortYear => {
                let (digits, rest) = value.split_at_checked(2)?;
                let short = atoi(digits)?;
                year = if short >= 69 {
                    short + 1900
                } else {
                    short + 2000
                };
                value = rest;
            }
            Hour => {
                (hour, value) = getnum(value, false)?;
                if hour >= 24 {
                    return None;
                }
            }
            Minute => {
                (minute, value) = getnum(value, true)?;
                if minute >= 60 {
                    return None;
                }
            }
            Second => {
                (second, value) = getnum(value, true)?;
                if second >= 60 {
                    return None;
                }
                (nanos, value) = fraction(value);
            }
            Zone => {
                let length = if value.starts_with(b"UTC") {
                    3
                } else {
                    zone_length(value)?
                };
                value = value.get(length..).unwrap_or_default();
            }
        }
    }
    if !value.is_empty() {
        return None;
    }
    let month = u32::try_from(month).ok()?;
    NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_nano_opt(hour, minute, second, nanos)
        .map(|time| time.and_utc())
}

/// Skips the literal text of a form (`skip`): a space matches any run of
/// spaces, or the end; anything else must match exactly.
fn skip<'a>(mut value: &'a [u8], mut text: &[u8]) -> Option<&'a [u8]> {
    while let Some((&first, rest)) = text.split_first() {
        if first == b' ' {
            if value.first().is_some_and(|&b| b != b' ') {
                return None;
            }
            text = cutspace(text);
            value = cutspace(value);
            continue;
        }
        let (&b, after) = value.split_first()?;
        if b != first {
            return None;
        }
        text = rest;
        value = after;
    }
    Some(value)
}

/// `s` without its leading spaces (`cutspace`).
fn cutspace(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| b != b' ').unwrap_or(s.len());
    s.get(start..).unwrap_or_default()
}

/// The first name of `table` that `value` starts with, ignoring ASCII
/// case, as its index (`lookup` and `match`).
fn lookup<'a>(table: &[&[u8]], value: &'a [u8]) -> Option<(usize, &'a [u8])> {
    table.iter().enumerate().find_map(|(index, name)| {
        let (head, rest) = value.split_at_checked(name.len())?;
        let same = head.iter().zip(name.iter()).all(|(&a, &b)| {
            a == b || {
                let (a, b) = (a | 0x20, b | 0x20);
                a == b && a.is_ascii_lowercase()
            }
        });
        same.then_some((index, rest))
    })
}

/// One or two digits, or exactly two when `fixed` (`getnum`).
fn getnum(value: &[u8], fixed: bool) -> Option<(u32, &[u8])> {
    let digit = |i: usize| {
        value
            .get(i)
            .filter(|b| b.is_ascii_digit())
            .map(|b| u32::from(b - b'0'))
    };
    let first = digit(0)?;
    match digit(1) {
        Some(second) => Some((first * 10 + second, value.get(2..).unwrap_or_default())),
        None if fixed => None,
        None => Some((first, value.get(1..).unwrap_or_default())),
    }
}

/// A fraction of a second, `.` or `,` and digits, as nanoseconds: Go
/// reads it although the forms have none.
fn fraction(value: &[u8]) -> (u32, &[u8]) {
    let separated = matches!(value.first(), Some(b'.' | b','));
    if !separated || !value.get(1).is_some_and(u8::is_ascii_digit) {
        return (0, value);
    }
    let end = value
        .iter()
        .skip(1)
        .position(|b| !b.is_ascii_digit())
        .map_or(value.len(), |n| n + 1);
    let digits = value.get(1..end).unwrap_or_default();
    let nanos = (0..9).fold(0, |n, i| {
        n * 10 + digits.get(i).map_or(0, |b| u32::from(b - b'0'))
    });
    (nanos, value.get(end..).unwrap_or_default())
}

/// A whole number with an optional sign, which must be all of `s`
/// (`atoi`).
fn atoi(s: &[u8]) -> Option<i32> {
    let (negative, digits) = match s.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, s),
    };
    let (n, rest) = leading_int(digits)?;
    if !rest.is_empty() {
        return None;
    }
    let n = i32::try_from(n).ok()?;
    Some(if negative { -n } else { n })
}

/// The digits `s` starts with, as a number, and what follows; `None` past
/// 2^63 (`leadingInt`).
fn leading_int(s: &[u8]) -> Option<(u64, &[u8])> {
    let end = s
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    let mut n: u64 = 0;
    for &b in s.get(..end).unwrap_or_default() {
        if n > (1 << 63) / 10 {
            return None;
        }
        n = n * 10 + u64::from(b - b'0');
        if n > 1 << 63 {
            return None;
        }
    }
    Some((n, s.get(end..).unwrap_or_default()))
}

/// How long the zone `value` starts with is (`parseTimeZone`).
fn zone_length(value: &[u8]) -> Option<usize> {
    if value.len() < 3 {
        return None;
    }
    if value.starts_with(b"ChST") || value.starts_with(b"MeST") {
        return Some(4);
    }
    if let Some(offset) = value.strip_prefix(b"GMT") {
        return Some(if offset.is_empty() {
            3
        } else {
            3 + signed_offset(offset)
        });
    }
    if matches!(value.first(), Some(b'+' | b'-')) {
        return Some(signed_offset(value)).filter(|&n| n > 0);
    }
    let upper = value
        .iter()
        .take(6)
        .take_while(|b| b.is_ascii_uppercase())
        .count();
    match upper {
        3 => Some(3),
        4 if value.get(3) == Some(&b'T') || value.starts_with(b"WITA") => Some(4),
        5 if value.get(4) == Some(&b'T') => Some(5),
        _ => None,
    }
}

/// How long the hour offset, a sign and at most 23, that `value` starts
/// with is, or 0 (`parseSignedOffset`).
fn signed_offset(value: &[u8]) -> usize {
    let Some((b'+' | b'-', digits)) = value.split_first() else {
        return 0;
    };
    match leading_int(digits) {
        Some((hours, rest)) if rest.len() < digits.len() && hours <= 23 => value.len() - rest.len(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: dates as Go's http.ParseTime (go1.26.4) read them,
    // as Unix seconds and nanoseconds.
    #[test]
    fn dates_read_as_go_reads_them() {
        let rfc850 = |zone: &str| format!("Monday, 02-Jan-06 15:04:05 {zone}");
        let mut cases: Vec<(String, i64, u32)> = [
            ("Mon, 02 Jan 2006 15:04:05 GMT", 1_136_214_245, 0),
            ("mon, 02 jAN 2006 15:04:05 GMT", 1_136_214_245, 0),
            ("Tue, 02 Jan 2006 15:04:05 GMT", 1_136_214_245, 0),
            ("Mon,   02   Jan 2006 5:04:05 GMT", 1_136_178_245, 0),
            (
                "Mon, 02 Jan 2006 15:04:05.123456789123 GMT",
                1_136_214_245,
                123_456_789,
            ),
            (
                "Mon, 02 Jan 2006 15:04:05,5 GMT",
                1_136_214_245,
                500_000_000,
            ),
            ("Mon, 29 Feb 2004 15:04:05 GMT", 1_078_067_045, 0),
            ("Mon, 29 Feb 2000 15:04:05 GMT", 951_836_645, 0),
            ("Mon, 02 Jan 0000 00:00:00 GMT", -62_167_132_800, 0),
            ("Mon, 02 Jan 9999 23:59:59 GMT", 253_370_937_599, 0),
            ("Monday, 02-Jan-68 15:04:05 GMT", 3_092_742_245, 0),
            ("Monday, 02-Jan-69 15:04:05 GMT", -31_395_355, 0),
            ("Monday, 02-Jan--5 15:04:05 GMT", 789_059_045, 0),
            ("Monday, 02-Jan-+5 15:04:05 GMT", 1_104_678_245, 0),
            ("Mon Jan  2 15:04:05 2006", 1_136_214_245, 0),
            ("Mon Jan 2 15:04:05 2006", 1_136_214_245, 0),
            ("Mon Jan 02 15:04:05 2006", 1_136_214_245, 0),
            ("Mon Jan 31 15:04:05 2006", 1_138_719_845, 0),
        ]
        .into_iter()
        .map(|(text, secs, nanos)| (text.to_owned(), secs, nanos))
        .collect();
        for zone in [
            "GMT", "GMT+5", "GMT-12", "UTC", "PST", "ABCDT", "WITA", "ChST", "+05",
        ] {
            cases.push((rfc850(zone), 1_136_214_245, 0));
        }
        for (text, secs, nanos) in cases {
            let time = parse_time(text.as_bytes());
            assert_eq!(
                time.map(|t| (t.timestamp(), t.timestamp_subsec_nanos())),
                Some((secs, nanos)),
                "{text}"
            );
        }
    }

    // Not upstream's: what Go's http.ParseTime (go1.26.4) rejects.
    #[test]
    fn dates_go_rejects() {
        let rfc850 = |zone: &str| format!("Monday, 02-Jan-06 15:04:05 {zone}");
        let mut cases: Vec<String> = [
            "Xyz, 02 Jan 2006 15:04:05 GMT",
            "Mon,02 Jan 2006 15:04:05 GMT",
            "Mon, 02 Jan 2006 15:04:05 gmt",
            "Mon, 02 Jan 2006 15:04:05 GMT ",
            "Mon, 2 Jan 2006 15:04:05 GMT",
            "Mon, 30 Feb 2006 15:04:05 GMT",
            "Mon, 29 Feb 1900 15:04:05 GMT",
            "Mon, 00 Jan 2006 15:04:05 GMT",
            "Mon, 02 Jan 2006 24:04:05 GMT",
            "Mon, 02 Jan 2006 15:60:05 GMT",
            "Mon, 02 Jan 2006 15:04:5 GMT",
            "Mon, 02 Jan 206 15:04:05 GMT",
            "Mon, 02 Jan 20061 15:04:05 GMT",
            "Mon, 02 Jan 2006 15:04:05 GMT\0",
            "Mon Jan _2 15:04:05 2006",
            "",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for zone in ["GMT+25", "ABCD", "AB"] {
            cases.push(rfc850(zone));
        }
        for text in cases {
            assert_eq!(parse_time(text.as_bytes()), None, "{text:?}");
        }
    }
}
