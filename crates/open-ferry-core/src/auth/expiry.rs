// Ported from CLIProxyAPI sdk/cliproxy/auth/types.go (ExpirationTime,
// AccessTokenExpirationTime, HasValidAccessToken and their helpers) and
// authAccessToken in sdk/cliproxy/auth/conductor_refresh.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! When a credential's tokens expire, read from its metadata.
//!
//! An access token that is a JWT says when it expires in its `exp` claim,
//! and that wins. Otherwise the metadata may hold an expiry time under one
//! of several names, a lifetime in seconds plus the time it was issued, or
//! either of those in a nested `token` object, as older files do.
//!
//! JWTs are decoded without checking their signature; the expiry is only a
//! hint for when to refresh.
//!
//! Deviations from upstream:
//! - Go's zero time ("year 1") stands for a time of zero or less, as
//!   upstream returns; times beyond what [`Timestamp`] holds are clamped to
//!   its range.
//! - Numbers too large for an integer saturate, where Go's conversion is
//!   undefined.

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use serde_json::{Map, Value};

use super::go::{atoi, decode_jwt_segment};
use super::json::fold_field;
use super::metadata::parse_int_any;
use super::{Auth, Timestamp};

/// The metadata keys that may hold an absolute expiry time, in the order
/// they're tried.
const EXPIRE_KEYS: [&str; 6] = [
    "expired",
    "expire",
    "expires_at",
    "expiresAt",
    "expiry",
    "expires",
];

impl Auth {
    /// When the credential expires: the access token's JWT `exp` claim, or
    /// else an expiry in the metadata.
    pub fn expiration_time(&self) -> Option<Timestamp> {
        let token = self.access_token();
        if !token.is_empty()
            && let Some(exp) = parse_jwt_exp(token)
        {
            return Some(exp);
        }
        expiration_from_map(&self.metadata)
    }

    /// When the access token expires: its JWT `exp` claim, or else
    /// [`expiration_time`](Self::expiration_time). `None` without an access
    /// token.
    pub fn access_token_expiration_time(&self) -> Option<Timestamp> {
        let token = self.access_token();
        if token.is_empty() {
            return None;
        }
        parse_jwt_exp(token).or_else(|| self.expiration_time())
    }

    /// Whether the credential has an access token that is still good at
    /// `now`. A token with no known expiry counts as good.
    pub fn has_valid_access_token(&self, now: Timestamp) -> bool {
        if self.access_token().is_empty() {
            return false;
        }
        self.access_token_expiration_time()
            .is_none_or(|exp| exp > now)
    }

    /// The access token in the metadata, trimmed, or empty.
    pub(crate) fn access_token(&self) -> &str {
        let token = self.trimmed_metadata_str("access_token");
        if token.is_empty() {
            self.trimmed_metadata_str("accessToken")
        } else {
            token
        }
    }
}

/// Go's zero `time.Time`, 0001-01-01 00:00:00 UTC.
pub(crate) fn zero_time() -> Timestamp {
    DateTime::from_timestamp(-62_135_596_800, 0).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

fn expiration_from_map(metadata: &Map<String, Value>) -> Option<Timestamp> {
    for key in EXPIRE_KEYS {
        if let Some(ts) = metadata.get(key).and_then(parse_time_value) {
            return Some(ts);
        }
    }
    if let Some(seconds) = relative_expiry_seconds(metadata)
        && let Some(issued) = relative_expiry_timestamp(metadata)
    {
        // Go multiplies into a time.Duration of nanoseconds, which wraps.
        let nanos = seconds.wrapping_mul(1_000_000_000);
        return Some(
            issued
                .checked_add_signed(TimeDelta::nanoseconds(nanos))
                .unwrap_or(if nanos < 0 {
                    DateTime::<Utc>::MIN_UTC
                } else {
                    DateTime::<Utc>::MAX_UTC
                }),
        );
    }
    for key in ["token", "Token"] {
        if let Some(Value::Object(nested)) = metadata.get(key)
            && let Some(ts) = expiration_from_map(nested)
        {
            return Some(ts);
        }
    }
    None
}

fn relative_expiry_seconds(metadata: &Map<String, Value>) -> Option<i64> {
    ["expires_in", "expiresIn"].into_iter().find_map(|key| {
        metadata
            .get(key)
            .and_then(parse_int_any)
            .filter(|seconds| *seconds > 0)
    })
}

fn relative_expiry_timestamp(metadata: &Map<String, Value>) -> Option<Timestamp> {
    let zero = zero_time();
    ["timestamp", "issued_at", "issuedAt"]
        .into_iter()
        .find_map(|key| {
            metadata
                .get(key)
                .and_then(parse_time_value)
                .filter(|ts| *ts != zero)
        })
}

/// The `exp` claim of a JWT, read without checking the signature.
pub(crate) fn parse_jwt_exp(token: &str) -> Option<Timestamp> {
    let token = token.trim();
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let bytes = decode_jwt_segment(payload)?;
    let claims: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).ok()?;
    match fold_field(claims.as_object()?, "exp")? {
        Value::Number(number) => {
            let exp = number.as_f64()?;
            (exp > 0.0).then(|| normalise_unix(exp as i64))
        }
        Value::String(text) => atoi(text.trim()).filter(|exp| *exp > 0).map(normalise_unix),
        _ => None,
    }
}

/// Upstream's `parseTimeValue`: a time in one of a few layouts, or Unix
/// seconds or milliseconds as a number or a string.
pub(crate) fn parse_time_value(value: &Value) -> Option<Timestamp> {
    match value {
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            parse_layouts(text).or_else(|| text.parse::<i64>().ok().map(normalise_unix))
        }
        Value::Number(number) => number.as_f64().map(|n| normalise_unix(n as i64)),
        _ => None,
    }
}

/// Upstream's `normaliseUnix`: zero or less is the zero time, above 1e12 is
/// milliseconds, the rest is seconds.
pub(crate) fn normalise_unix(raw: i64) -> Timestamp {
    if raw <= 0 {
        return zero_time();
    }
    let ts = if raw > 1_000_000_000_000 {
        DateTime::from_timestamp_millis(raw)
    } else {
        DateTime::from_timestamp(raw, 0)
    };
    ts.unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// Go's `time.Parse` with the layouts upstream tries, in order:
/// `2006-01-02T15:04:05Z07:00` (RFC 3339, with or without fractional
/// seconds), `2006-01-02 15:04:05` and `2006-01-02 15:04` (both UTC).
fn parse_layouts(text: &str) -> Option<Timestamp> {
    parse_go_time(text, Layout::Rfc3339)
        .or_else(|| parse_go_time(text, Layout::Seconds))
        .or_else(|| parse_go_time(text, Layout::Minutes))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// `2006-01-02T15:04:05Z07:00`.
    Rfc3339,
    /// `2006-01-02 15:04:05`.
    Seconds,
    /// `2006-01-02 15:04`.
    Minutes,
}

/// A cursor over the text being parsed, mirroring the pieces of Go's
/// `time.parse` these layouts use.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn digit(&self, i: usize) -> Option<u32> {
        self.0
            .get(i)
            .filter(|c| c.is_ascii_digit())
            .map(|c| u32::from(c - b'0'))
    }

    /// Go's `getnum`: two digits, or one when `fixed` is false and only one
    /// is there.
    fn num(&mut self, fixed: bool) -> Option<u32> {
        let first = self.digit(0)?;
        match self.digit(1) {
            Some(second) => {
                self.0 = self.0.get(2..)?;
                Some(first * 10 + second)
            }
            None if !fixed => {
                self.0 = self.0.get(1..)?;
                Some(first)
            }
            None => None,
        }
    }

    fn literal(&mut self, c: u8) -> Option<()> {
        if self.0.first() == Some(&c) {
            self.0 = self.0.get(1..)?;
            Some(())
        } else {
            None
        }
    }

    /// A space in a Go layout matches one or more spaces.
    fn spaces(&mut self) -> Option<()> {
        if self.0.first().is_some_and(|c| *c != b' ') {
            return None;
        }
        while self.0.first() == Some(&b' ') {
            self.0 = self.0.get(1..)?;
        }
        Some(())
    }

    /// Fractional seconds after the seconds: `.` or `,` and any number of
    /// digits, of which the first nine count.
    fn fraction(&mut self) -> u32 {
        if !matches!(self.0.first(), Some(b'.' | b',')) || self.digit(1).is_none() {
            return 0;
        }
        let digits = self
            .0
            .get(1..)
            .unwrap_or_default()
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        let mut nanos = 0;
        for i in 0..9 {
            nanos = nanos * 10 + self.digit(1 + i).filter(|_| i < digits).unwrap_or(0);
        }
        self.0 = self.0.get(1 + digits..).unwrap_or_default();
        nanos
    }
}

fn parse_go_time(text: &str, layout: Layout) -> Option<Timestamp> {
    let mut cur = Cursor(text.as_bytes());
    let year_digits = cur.0.get(..4)?;
    if !year_digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year: i32 = std::str::from_utf8(year_digits).ok()?.parse().ok()?;
    cur.0 = cur.0.get(4..)?;
    cur.literal(b'-')?;
    let month = cur.num(true)?;
    cur.literal(b'-')?;
    let day = cur.num(true)?;
    match layout {
        Layout::Rfc3339 => cur.literal(b'T')?,
        Layout::Seconds | Layout::Minutes => cur.spaces()?,
    }
    let hour = cur.num(false)?;
    cur.literal(b':')?;
    let minute = cur.num(true)?;
    let (second, nanos) = if layout == Layout::Minutes {
        (0, 0)
    } else {
        cur.literal(b':')?;
        let second = cur.num(true)?;
        (second, cur.fraction())
    };
    if !(1..=12).contains(&month) || hour >= 24 || minute >= 60 || second >= 60 {
        return None;
    }
    let offset = if layout == Layout::Rfc3339 {
        zone_offset(&mut cur)?
    } else {
        0
    };
    if !cur.0.is_empty() {
        return None;
    }
    let naive =
        NaiveDate::from_ymd_opt(year, month, day)?.and_hms_nano_opt(hour, minute, second, nanos)?;
    naive
        .and_utc()
        .checked_sub_signed(TimeDelta::seconds(offset))
}

/// Go's `Z07:00`: `Z`, or a sign and two-digit hours and minutes, in
/// seconds east of UTC. Go accepts up to 24 hours and 60 minutes.
fn zone_offset(cur: &mut Cursor<'_>) -> Option<i64> {
    if cur.literal(b'Z').is_some() {
        return Some(0);
    }
    let zone = cur.0.get(..6)?;
    if zone.get(3) != Some(&b':') {
        return None;
    }
    let mut hours = Cursor(zone.get(1..3)?);
    let mut minutes = Cursor(zone.get(4..6)?);
    let hours = i64::from(hours.num(true)?);
    let minutes = i64::from(minutes.num(true)?);
    if hours > 24 || minutes > 60 {
        return None;
    }
    let offset = (hours * 60 + minutes) * 60;
    let offset = match zone.first() {
        Some(b'+') => offset,
        Some(b'-') => -offset,
        _ => return None,
    };
    cur.0 = cur.0.get(6..)?;
    Some(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;

    fn make_test_jwt(exp_unix: i64) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(format!(
            r#"{{"exp":{exp_unix},"email":"test@example.com"}}"#
        ));
        format!("{header}.{payload}.sig")
    }

    fn auth(metadata: Value) -> Auth {
        Auth {
            metadata: metadata.as_object().cloned().unwrap(),
            ..Auth::default()
        }
    }

    fn at(text: &str) -> Timestamp {
        DateTime::parse_from_rfc3339(text).unwrap().to_utc()
    }

    #[test]
    fn expiration_time_prefers_jwt_exp() {
        let now = Utc::now();
        let future = DateTime::from_timestamp((now + TimeDelta::hours(48)).timestamp(), 0).unwrap();
        let past = DateTime::from_timestamp((now - TimeDelta::hours(24)).timestamp(), 0).unwrap();
        let future_jwt = make_test_jwt(future.timestamp());
        let past_jwt = make_test_jwt(past.timestamp());

        // 1. Without an expiry in the metadata, the JWT's exp is used.
        let only_jwt = auth(json!({"access_token": future_jwt}));
        assert_eq!(only_jwt.expiration_time(), Some(future));

        // 2. A future JWT exp wins over a stale `expired`.
        let stale = auth(json!({"expired": past.to_rfc3339(), "access_token": future_jwt}));
        assert_eq!(stale.expiration_time(), Some(future));

        // 3. Both in the past: still a time, in the past.
        let past_both = auth(json!({"expired": past.to_rfc3339(), "access_token": past_jwt}));
        assert!(past_both.expiration_time().unwrap() < Utc::now());

        // 4. An expired access token isn't saved by a future ID token.
        let expired_access = auth(json!({"access_token": past_jwt, "id_token": future_jwt}));
        assert!(!expired_access.has_valid_access_token(Utc::now()));
        assert!(expired_access.access_token_expiration_time().unwrap() < Utc::now());

        // 5. No access token, no valid access token.
        let no_access = auth(json!({"id_token": future_jwt, "expired": future.to_rfc3339()}));
        assert!(!no_access.has_valid_access_token(Utc::now()));
        assert_eq!(no_access.access_token_expiration_time(), None);
    }

    #[test]
    fn token_without_expiry_counts_as_valid() {
        let opaque = auth(json!({"accessToken": " opaque "}));
        assert!(opaque.has_valid_access_token(Utc::now()));
        assert_eq!(opaque.expiration_time(), None);
    }

    #[test]
    fn metadata_expiry_keys() {
        let ts = at("2026-01-02T03:04:05Z");
        assert_eq!(
            auth(json!({"expire": "2026-01-02T03:04:05Z"})).expiration_time(),
            Some(ts)
        );
        assert_eq!(
            auth(json!({"expiry": 1_767_323_045})).expiration_time(),
            Some(ts)
        );
        assert_eq!(
            auth(json!({"expires": "1767323045000"})).expiration_time(),
            Some(ts)
        );
        // An unparseable key falls through to the next.
        assert_eq!(
            auth(json!({"expired": "soon", "expires_at": "2026-01-02 03:04:05"})).expiration_time(),
            Some(ts)
        );
        // Zero is Go's zero time, not "unknown".
        assert_eq!(
            auth(json!({"expired": 0})).expiration_time(),
            Some(zero_time())
        );
    }

    #[test]
    fn relative_and_nested_expiry() {
        let issued = at("2026-01-02T03:04:05Z");
        let relative = auth(json!({"expires_in": "3600", "issued_at": "2026-01-02T03:04:05Z"}));
        assert_eq!(
            relative.expiration_time(),
            Some(issued + TimeDelta::hours(1))
        );
        // A lifetime without an issue time says nothing.
        assert_eq!(auth(json!({"expires_in": 3600})).expiration_time(), None);
        assert_eq!(
            auth(json!({"expires_in": 0, "timestamp": 1})).expiration_time(),
            None
        );

        let nested = auth(json!({"token": {"expiry": "2026-01-02T03:04:05Z"}}));
        assert_eq!(nested.expiration_time(), Some(issued));
        let nested =
            auth(json!({"Token": {"token": {"expiresIn": 60, "timestamp": 1_767_323_045}}}));
        assert_eq!(
            nested.expiration_time(),
            Some(issued + TimeDelta::minutes(1))
        );
    }

    #[test]
    fn jwt_exp_forms() {
        let payload = |claims: &str| format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims));
        assert_eq!(
            parse_jwt_exp(&payload(r#"{"EXP":"1767323045"}"#)),
            Some(at("2026-01-02T03:04:05Z"))
        );
        assert_eq!(
            parse_jwt_exp(&payload(r#"{"exp":1767323045000}"#)),
            Some(at("2026-01-02T03:04:05Z"))
        );
        assert_eq!(parse_jwt_exp(&payload(r#"{"exp":0.5}"#)), Some(zero_time()));
        assert_eq!(parse_jwt_exp(&payload(r#"{"exp":0}"#)), None);
        assert_eq!(parse_jwt_exp(&payload(r#"{"exp":"-1"}"#)), None);
        assert_eq!(parse_jwt_exp(&payload(r#"{"exp":true}"#)), None);
        assert_eq!(parse_jwt_exp(&payload("null")), None);
        assert_eq!(parse_jwt_exp(&payload("[1]")), None);
        assert_eq!(parse_jwt_exp("a.b"), None);
        assert_eq!(parse_jwt_exp("a.b.c.d"), None);
        // Go's base64 decoders skip line breaks inside the payload.
        assert_eq!(
            parse_jwt_exp("h.eyJl\r\neHAiOiAxNzAwMDAwMDAwfQ.s"),
            Some(at("2023-11-14T22:13:20Z"))
        );
    }

    #[test]
    fn time_layouts_match_go() {
        let parse = |text: &str| parse_time_value(&Value::from(text));
        let ts = at("2026-01-02T03:04:05Z");
        assert_eq!(parse("2026-01-02T03:04:05Z"), Some(ts));
        assert_eq!(parse("2026-01-02T05:04:05+02:00"), Some(ts));
        assert_eq!(parse("2026-01-02T3:04:05Z"), Some(ts));
        assert_eq!(parse("2026-01-02 03:04:05"), Some(ts));
        assert_eq!(parse("2026-01-02   03:04:05"), Some(ts));
        assert_eq!(parse("2026-01-02 03:04"), Some(at("2026-01-02T03:04:00Z")));
        assert_eq!(
            parse("2026-01-02T03:04:05,1234567891Z"),
            Some(at("2026-01-02T03:04:05.123456789Z"))
        );
        assert_eq!(
            parse("2026-01-02 03:04:05.5"),
            Some(at("2026-01-02T03:04:05.5Z"))
        );
        assert_eq!(
            parse("2026-01-02T03:04:05+24:00"),
            Some(ts - TimeDelta::hours(24))
        );
        assert_eq!(
            parse("2024-02-29T00:00:00Z"),
            Some(at("2024-02-29T00:00:00Z"))
        );
        for bad in [
            "2026-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-01-02T24:00:00Z",
            "2026-01-02T03:04:05",
            "2026-01-02T03:04:05z",
            "2026-01-02T03:04:05+0200",
            "2026-01-02T03:04:05+25:00",
            "2026-1-02T03:04:05Z",
            "2026-01-02 03:04:05Z",
            "2026-01-02 03:04.5",
            "+026-01-02T03:04:05Z",
            "x",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(parse("1767323045"), Some(ts));
        assert_eq!(parse("-5"), Some(zero_time()));
        assert_eq!(parse(" "), None);
    }
}
