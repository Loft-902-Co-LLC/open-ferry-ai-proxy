// Ported from CLIProxyAPI internal/translator/common/bytes.go (GeminiTokenCountJSON)
// (v8.0.15, MIT), and Go's `time.Unix(sec, 0).Format(time.RFC3339Nano)` as the
// translators to Gemini use it, from Go 1.26's `time/time.go` and
// `time/format_rfc3339.go` (BSD-3-Clause, see licenses/Go-LICENSE).
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
///
/// Go counts seconds from a day 292,277,022,400 years before year 1 in a
/// `uint64`, so a time before that, under about -9.2e18 seconds, wraps
/// around to one about 584 billion years later. We write the same date.
pub(crate) fn create_time(seconds: i64) -> String {
    // `unixToInternal + internalToAbsolute`.
    const UNIX_TO_ABSOLUTE: i64 = 9_223_372_028_741_760_000;
    const ABSOLUTE_YEARS: u64 = 292_277_022_400;
    const MARCH_THRU_DECEMBER: u32 = 306;
    const SECONDS_PER_DAY: u64 = 86_400;

    let abs = seconds.wrapping_add(UNIX_TO_ABSOLUTE) as u64;
    // `absDays.split`: centuries, years in the century, and the day in a
    // year that starts on March 1.
    let d = 4 * (abs / SECONDS_PER_DAY) + 3;
    let century = d / 146_097;
    let cd = u64::from((d % 146_097) as u32 | 3);
    let product = 2_939_745 * cd;
    let cyear = product >> 32;
    let ayday = (product as u32) / 2_939_745 / 4;
    // `absYday.split`, `janFeb`, `month` and `year`.
    let d = 2141 * ayday + 197_913;
    let jan_feb = u32::from(ayday >= MARCH_THRU_DECEMBER);
    let month = (d >> 16) - jan_feb * 12;
    let day = 1 + (d & 0xFFFF) / 2141;
    let year = (century.wrapping_mul(100).wrapping_sub(ABSOLUTE_YEARS) as i64)
        + cyear as i64
        + i64::from(jan_feb);
    let year = if year < 0 {
        format!("-{:04}", year.unsigned_abs())
    } else {
        format!("{year:04}")
    };
    let second_of_day = abs % SECONDS_PER_DAY;
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60,
    )
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
    }

    #[test]
    fn create_time_wraps_around_as_go_does() {
        // What Go 1.26 writes for `time.Unix(s, 0).UTC()`.
        for (seconds, want) in [
            (i64::MAX, "292277026596-12-04T15:30:07Z"),
            (i64::MAX - 1, "292277026596-12-04T15:30:06Z"),
            (8_113_015_807, "2227-02-03T15:30:07Z"),
            (8_113_015_808, "2227-02-03T15:30:08Z"),
            (9_223_372_028_741_760_000, "292277026339-11-03T00:00:00Z"),
            (4_611_686_018_427_387_904, "146138514283-06-19T07:45:04Z"),
            (-4_611_686_018_427_387_904, "-146138510344-07-14T16:14:56Z"),
            (-8_000_000_000_000_000_000, "-253509906085-07-06T09:46:40Z"),
            (-9_223_372_028_741_673_601, "-292277022400-03-01T23:59:59Z"),
            (-9_223_372_028_741_760_000, "-292277022400-03-01T00:00:00Z"),
            // Earlier than that wraps around.
            (-9_223_372_028_741_760_001, "292277026854-01-07T07:00:15Z"),
            (-9_223_372_028_741_846_400, "292277026854-01-06T07:00:16Z"),
            (-9_223_372_036_854_775_000, "292277026596-12-04T15:43:36Z"),
            (i64::MIN + 1, "292277026596-12-04T15:30:09Z"),
            (i64::MIN, "292277026596-12-04T15:30:08Z"),
        ] {
            assert_eq!(create_time(seconds), want, "{seconds}");
        }
    }
}
