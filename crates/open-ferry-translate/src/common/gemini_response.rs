// Ported from CLIProxyAPI internal/translator/common/bytes.go (GeminiTokenCountJSON)
// (v8.0.10, MIT), and Go's `time.Unix(sec, 0).Format(time.RFC3339Nano)` as the
// translators to Gemini use it.
// https://github.com/router-for-me/CLIProxyAPI

//! Helpers for translators that write Gemini responses.
//!
//! Deviations from upstream:
//! - [`create_time`] writes the time in UTC. Upstream formats it in the
//!   server's local time zone, so the same instant can read
//!   `2023-11-14T22:13:20Z` or `2023-11-14T14:13:20-08:00`.

use serde_json::{Value, json};

/// A Gemini `countTokens` response body for `count` tokens, all of them text.
pub(crate) fn gemini_token_count_json(count: i64) -> Value {
    json!({
        "totalTokens": count,
        "promptTokensDetails": [{"modality": "TEXT", "tokenCount": count}],
    })
}

/// A Unix time in seconds as an RFC 3339 timestamp in UTC, as Go's
/// `time.RFC3339Nano` layout writes a whole second: no fraction, a `Z` for the
/// zone, and a year of at least four digits.
pub(crate) fn create_time(seconds: i64) -> String {
    let seconds = i128::from(seconds);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let year = if year < 0 {
        format!("-{:04}", -year)
    } else {
        format!("{year:04}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60,
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01, by Howard Hinnant's
/// `civil_from_days`.
fn civil_from_days(days: i128) -> (i128, i128, i128) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i128::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_count_matches_upstream_bytes() {
        assert_eq!(
            serde_json::to_string(&gemini_token_count_json(42)).unwrap(),
            r#"{"totalTokens":42,"promptTokensDetails":[{"modality":"TEXT","tokenCount":42}]}"#
        );
    }

    #[test]
    fn create_time_writes_utc_rfc3339() {
        assert_eq!(create_time(0), "1970-01-01T00:00:00Z");
        assert_eq!(create_time(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(create_time(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(create_time(-1), "1969-12-31T23:59:59Z");
        assert_eq!(create_time(253_402_300_800), "10000-01-01T00:00:00Z");
        assert_eq!(create_time(-62_135_596_801), "0000-12-31T23:59:59Z");
        assert_eq!(create_time(-62_198_755_200), "-0001-01-01T00:00:00Z");
        // Extreme values don't overflow.
        assert!(create_time(i64::MAX).ends_with('Z'));
        assert!(create_time(i64::MIN).ends_with('Z'));
    }
}
