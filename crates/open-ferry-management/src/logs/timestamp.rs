// Ported from CLIProxyAPI internal/api/handlers/management/logs.go
// (parseTimestamp, and timestampRotationOrder's parse) (v8.0.15, MIT), with
// Go's time/format.go (Parse, for the two layouts upstream uses) and
// time.go (Date's zone offset) (go1.27, BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! The local times at the start of log lines and in rotated logs' names,
//! read as Go's `time.ParseInLocation` reads them in the local zone with
//! the layouts `2006-01-02 15:04:05` and `2006-01-02T15-04-05`: a
//! four-digit year; a two-digit month, day, minute and second; a one- or
//! two-digit hour; each in its range. A space in the layout matches one or
//! more spaces.
//!
//! A local time the clocks skip, or pass twice, is placed as Go's
//! `time.Date` places it: with the offset in force at the instant the
//! offset found first gives.
//!
//! Deviations from upstream: none.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, Offset, TimeZone};

/// A part of a layout.
#[derive(Clone, Copy)]
pub(super) enum Part {
    /// `2006`: four digits.
    Year,
    /// `01`: two digits.
    Month,
    /// `02`: two digits.
    Day,
    /// `15`: one or two digits.
    Hour,
    /// `04`: two digits.
    Minute,
    /// `05`: two digits.
    Second,
    /// A byte that must be there; a space matches one or more.
    Literal(u8),
}

use Part::{Day, Hour, Literal, Minute, Month, Second, Year};

/// `2006-01-02 15:04:05`: a log line's time.
const LINE_LAYOUT: [Part; 11] = [
    Year,
    Literal(b'-'),
    Month,
    Literal(b'-'),
    Day,
    Literal(b' '),
    Hour,
    Literal(b':'),
    Minute,
    Literal(b':'),
    Second,
];

/// `2006-01-02T15-04-05`: the time in a rotated log's name.
pub(super) const ROTATION_LAYOUT: [Part; 11] = [
    Year,
    Literal(b'-'),
    Month,
    Literal(b'-'),
    Day,
    Literal(b'T'),
    Hour,
    Literal(b'-'),
    Minute,
    Literal(b'-'),
    Second,
];

/// The Unix time of the local time a log line starts with, after an
/// optional `[`, or 0 (upstream's `parseTimestamp`).
pub(crate) fn parse_timestamp(line: &[u8]) -> i64 {
    let line = line.strip_prefix(b"[").unwrap_or(line);
    line.get(..19)
        .and_then(|candidate| parse_local(candidate, &LINE_LAYOUT))
        .unwrap_or(0)
}

/// The Unix time `value` names in the local zone, read with `layout`; `None`
/// where Go's parse fails.
pub(super) fn parse_local(value: &[u8], layout: &[Part]) -> Option<i64> {
    let mut rest = value;
    let (mut year, mut month, mut day) = (0, 0, 0);
    let (mut hour, mut minute, mut second) = (0, 0, 0);
    for part in layout {
        match *part {
            Literal(b' ') => {
                if rest.first().is_some_and(|&b| b != b' ') {
                    return None;
                }
                while let Some(tail) = rest.strip_prefix(b" ") {
                    rest = tail;
                }
            }
            Literal(byte) => rest = rest.strip_prefix(&[byte])?,
            Year => {
                let (digits, tail) = rest.split_at_checked(4)?;
                if !digits.iter().all(u8::is_ascii_digit) {
                    return None;
                }
                year = digits
                    .iter()
                    .fold(0, |n, digit| n * 10 + i32::from(digit - b'0'));
                rest = tail;
            }
            Month => (month, rest) = number(rest, true).filter(|&(m, _)| (1..=12).contains(&m))?,
            Day => (day, rest) = number(rest, true)?,
            Hour => (hour, rest) = number(rest, false).filter(|&(h, _)| h < 24)?,
            Minute => (minute, rest) = number(rest, true).filter(|&(m, _)| m < 60)?,
            Second => (second, rest) = number(rest, true).filter(|&(s, _)| s < 60)?,
        }
    }
    if !rest.is_empty() {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    Some(local_unix(date.and_hms_opt(hour, minute, second)?))
}

/// Go's `getnum`: two digits, or with `fixed` false one where no second
/// follows; and what is left.
fn number(s: &[u8], fixed: bool) -> Option<(u32, &[u8])> {
    let (&first, rest) = s.split_first()?;
    if !first.is_ascii_digit() {
        return None;
    }
    let first = u32::from(first - b'0');
    match rest.split_first() {
        Some((&second, tail)) if second.is_ascii_digit() => {
            Some((first * 10 + u32::from(second - b'0'), tail))
        }
        _ if fixed => None,
        _ => Some((first, rest)),
    }
}

/// The Unix time of `local` in the local zone, as Go's `time.Date` finds
/// it: the offset in force at `local` read as UTC gives a guess, and the
/// offset in force at the guess is the one used.
fn local_unix(local: NaiveDateTime) -> i64 {
    let unix = local.and_utc().timestamp();
    unix - offset_at(unix - offset_at(unix))
}

/// The local zone's offset from UTC, in seconds, at Unix time `unix`.
fn offset_at(unix: i64) -> i64 {
    DateTime::from_timestamp(unix, 0).map_or(0, |time| {
        i64::from(
            Local
                .offset_from_utc_datetime(&time.naive_utc())
                .fix()
                .local_minus_utc(),
        )
    })
}
