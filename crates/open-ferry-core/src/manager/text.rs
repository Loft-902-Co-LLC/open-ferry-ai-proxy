// Ported from CLIProxyAPI internal/thinking/suffix.go (ParseSuffix), the
// canonicalModelKey in sdk/cliproxy/auth/selector.go, and parseDurationString in
// sdk/cliproxy/auth/conductor_refresh.go, with Go's time.ParseDuration
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Go standard library behaviour the manager's decisions depend on: string
//! case, number, bool and duration parsing, and gjson's string view of a
//! JSON value. The case, bool and integer helpers are the credential
//! module's.
//!
//! Deviations from upstream:
//! - JSON numbers print as serde_json writes them, where gjson keeps the raw
//!   text; the two differ only for unusual forms such as `1e3`.
//! - A number of seconds that is infinite, NaN or too large for a duration
//!   is no duration, where Go's conversion gives an arbitrary value.

use std::time::Duration;

use serde_json::Value;

pub(crate) use crate::auth::{atoi, equal_fold, parse_bool, parse_bool_any, parse_int_any};

/// Go's `strings.ToLower`.
pub(crate) fn go_lower(s: &str) -> String {
    open_ferry_translate::go::to_lower(s)
}

/// gjson's `Result.String()` of a value: a string as is, other scalars as
/// their JSON text, an object or array as JSON, and nothing as empty.
pub(crate) fn str_of(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Go's `time.ParseDuration`, in nanoseconds: `1h30m`, `1.5s`, `-2ms`,
/// `0`.
pub(crate) fn parse_go_duration(s: &str) -> Option<i64> {
    const LIMIT: u64 = 1 << 63;
    let (neg, mut rest) = match s.as_bytes().first() {
        Some(b'-') => (true, s.get(1..)?),
        Some(b'+') => (false, s.get(1..)?),
        _ => (false, s),
    };
    if rest == "0" {
        return Some(0);
    }
    if rest.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    while !rest.is_empty() {
        let bytes = rest.as_bytes();
        let unit_char_at = |i: usize| {
            bytes
                .get(i)
                .is_some_and(|b| *b != b'.' && !b.is_ascii_digit())
        };
        if unit_char_at(0) {
            return None;
        }
        // Whole part.
        let mut i = 0;
        let mut whole: u64 = 0;
        while let Some(b) = bytes.get(i).filter(|b| b.is_ascii_digit()) {
            if whole > LIMIT / 10 {
                return None;
            }
            whole = whole * 10 + u64::from(b - b'0');
            if whole > LIMIT {
                return None;
            }
            i += 1;
        }
        let pre = i > 0;
        // Fraction.
        let mut frac: u64 = 0;
        let mut scale = 1.0_f64;
        let mut post = false;
        if bytes.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            let mut overflow = false;
            while let Some(b) = bytes.get(i).filter(|b| b.is_ascii_digit()) {
                if !overflow {
                    if frac > (LIMIT - 1) / 10 {
                        overflow = true;
                    } else {
                        let next = frac * 10 + u64::from(b - b'0');
                        if next > LIMIT {
                            overflow = true;
                        } else {
                            frac = next;
                            scale *= 10.0;
                        }
                    }
                }
                i += 1;
            }
            post = i > start;
        }
        if !pre && !post {
            return None;
        }
        // Unit.
        let unit_start = i;
        while unit_char_at(i) {
            i += 1;
        }
        if i == unit_start {
            return None;
        }
        let unit: u64 = match rest.get(unit_start..i)? {
            "ns" => 1,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        rest = rest.get(i..)?;
        if whole > LIMIT / unit {
            return None;
        }
        let mut value = whole * unit;
        if frac > 0 {
            value += (frac as f64 * (unit as f64 / scale)) as u64;
            if value > LIMIT {
                return None;
            }
        }
        total += value;
        if total > LIMIT {
            return None;
        }
    }
    if neg {
        return Some((-i128::from(total)) as i64);
    }
    i64::try_from(total).ok()
}

/// A positive duration from a Go duration such as `90s`, else a number of
/// seconds (upstream's `parseDurationString`).
pub(crate) fn parse_duration_string(raw: &str) -> Option<Duration> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(nanos) = parse_go_duration(s)
        && nanos > 0
    {
        return Some(Duration::from_nanos(nanos.unsigned_abs()));
    }
    let seconds: f64 = s.parse().ok()?;
    seconds_to_duration(seconds)
}

/// A positive number of seconds as Go's `time.Duration(secs * 1e9)`.
pub(crate) fn seconds_to_duration(seconds: f64) -> Option<Duration> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    let nanos = seconds * 1e9;
    if nanos >= i64::MAX as f64 {
        return None;
    }
    let nanos = nanos as i64;
    (nanos > 0).then(|| Duration::from_nanos(nanos.unsigned_abs()))
}

/// A model name split at a trailing thinking suffix, as in `gpt-5(high)`
/// (upstream's `thinking.ParseSuffix`): the name and the raw suffix.
pub(crate) fn parse_suffix(model: &str) -> (&str, Option<&str>) {
    let Some(open) = model.rfind('(') else {
        return (model, None);
    };
    if !model.ends_with(')') {
        return (model, None);
    }
    let name = model.get(..open).unwrap_or_default();
    let raw = model.get(open + 1..model.len() - 1).unwrap_or_default();
    (name, Some(raw))
}

/// The model without its thinking suffix, trimmed (upstream's
/// `canonicalModelKey`).
pub(crate) fn canonical_model_key(model: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        return String::new();
    }
    let name = parse_suffix(model).0.trim();
    if name.is_empty() {
        return model.to_owned();
    }
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn go_parsing_matches() {
        assert_eq!(parse_bool("T"), Some(true));
        assert_eq!(parse_bool("yes"), None);
        assert_eq!(atoi("-12"), Some(-12));
        assert_eq!(atoi("+3"), Some(3));
        assert_eq!(atoi(" 3"), None);
        assert_eq!(atoi("3.0"), None);
        assert_eq!(parse_int_any(&json!(2.9)), Some(2));
        assert_eq!(parse_int_any(&json!(" 4 ")), Some(4));
        assert_eq!(parse_int_any(&json!(true)), None);
        assert_eq!(parse_bool_any(&json!(0)), Some(false));
        assert_eq!(parse_bool_any(&json!(" true ")), Some(true));
        assert_eq!(parse_bool_any(&json!("")), None);
    }

    #[test]
    fn case_folding_matches_go() {
        assert!(equal_fold("Codex", "cODEX"));
        assert!(equal_fold("k", "\u{212a}"));
        assert!(!equal_fold("ab", "a"));
        assert_eq!(go_lower("\u{130}X"), "ix");
    }

    #[test]
    fn suffixes_split_like_upstream() {
        assert_eq!(parse_suffix("gpt-5(high)"), ("gpt-5", Some("high")));
        assert_eq!(parse_suffix("gpt-5(high"), ("gpt-5(high", None));
        assert_eq!(parse_suffix("a(b)(c)"), ("a(b)", Some("c")));
        assert_eq!(canonical_model_key(" gpt-5(low) "), "gpt-5");
        assert_eq!(canonical_model_key("(x)"), "(x)");
        assert_eq!(canonical_model_key("  "), "");
        assert_eq!(str_of(Some(&json!(12))), "12");
        assert_eq!(str_of(Some(&json!({"a": 1}))), r#"{"a":1}"#);
        assert_eq!(str_of(None), "");
    }

    #[test]
    fn durations_parse_like_go() {
        assert_eq!(parse_go_duration("1h30m"), Some(5_400_000_000_000));
        assert_eq!(parse_go_duration("1.5s"), Some(1_500_000_000));
        assert_eq!(parse_go_duration("-2ms"), Some(-2_000_000));
        assert_eq!(parse_go_duration(".5us"), Some(500));
        assert_eq!(parse_go_duration("3\u{b5}s"), Some(3_000));
        assert_eq!(parse_go_duration("0"), Some(0));
        assert_eq!(parse_go_duration("1"), None);
        assert_eq!(parse_go_duration("."), None);
        assert_eq!(parse_go_duration("1d"), None);
        assert_eq!(parse_go_duration("9999999999h"), None);
        assert_eq!(parse_duration_string(" 90 "), Some(Duration::from_secs(90)));
        assert_eq!(
            parse_duration_string("2.5"),
            Some(Duration::from_millis(2500))
        );
        assert_eq!(parse_duration_string("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration_string("-1s"), None);
        assert_eq!(parse_duration_string("0"), None);
        assert_eq!(parse_duration_string("inf"), None);
    }
}
