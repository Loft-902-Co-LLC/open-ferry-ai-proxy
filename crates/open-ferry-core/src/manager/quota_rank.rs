//! Routing by quota: open-ferry's own `quota` strategy, which picks among
//! the ready credentials of a priority by the quota their provider last
//! reported (see `quota_signals`).
//!
//! The windows read are Claude's (and `claude-cli`'s) 5-hour and 7-day
//! ones, from `Anthropic-Ratelimit-Unified-{5h,7d}-Utilization` (a
//! fraction) with `-Reset` (Unix seconds or RFC 3339) and `-Status`
//! (`rejected` counts as full), and Codex's primary and secondary ones,
//! from `X-Codex-{Primary,Secondary}-Used-Percent` with `-Reset-At` (Unix
//! seconds) or `-Reset-After-Seconds` (from when the reading came). Other
//! windows (Claude's overage and per-model ones, Codex's per-model limits)
//! aren't read.
//!
//! A window counts while its reset is ahead; one with no reset, or whose
//! reset has passed, says nothing of today's use and is ignored. A
//! credential's binding window is its fullest current one (the later reset
//! when two are as full). It has room when every current window is below
//! `100 - reserve-percent` percent used; a credential with no current
//! window (an API key, a provider that sends no quota headers, a credential
//! not used since the start, or a reading whose windows have all reset) has
//! room, with all of its allowance left.
//!
//! The order, best first:
//! - Credentials with room come before those without: the reserve is kept
//!   while another credential has room.
//! - `soonest-reset` (the default) among those with room: the earliest
//!   binding reset first, then the most left; those with no reading come
//!   last, since no allowance of theirs is known to be about to go unused,
//!   and they stay spare.
//! - `most-left` among those with room: the most left in the binding window
//!   first, then the earliest reset; one with no reading counts as having
//!   all of it left, so a credential is read on its first pick.
//! - When none has room, the most left in the binding window, then the
//!   earliest reset: the call is still served, by the credential likeliest
//!   to take it, rather than refused while the provider would still answer;
//!   a credential the provider refuses rests as with any strategy.
//! - Credentials that rank the same take turns, as round-robin does.
//!
//! Percentages compare to a tenth of a percent and resets to the second.
//! Credential priority still comes first and an established session
//! affinity binding still wins, since this only orders the ready
//! credentials of one priority; weights are ignored. A pick's debug line
//! holds numbers only, never a credential's ID, key or email.
//!
//! Deviations from upstream:
//! - The whole strategy is open-ferry's own: CLIProxyAPI has no `quota`
//!   strategy and runs one as round-robin, so there is no parity suite for
//!   it. Tests use credentials with set readings and loopback mock
//!   upstreams only.

use std::cmp::Ordering;

use chrono::DateTime;

use super::select::successor_index;
use super::text::go_lower;
use crate::auth::{Auth, QuotaState, Timestamp};
use crate::config::RoutingQuota;

/// What the `quota` strategy prefers among credentials with room.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum QuotaPreference {
    /// The credential whose binding window resets soonest
    /// (`soonest-reset`), so allowance that would be lost is used first.
    #[default]
    SoonestReset,
    /// The credential with the most left in its binding window
    /// (`most-left`).
    MostLeft,
}

/// The `quota` strategy's preferences (`routing.quota`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct QuotaPrefs {
    /// What it prefers among credentials with room.
    pub prefer: QuotaPreference,
    /// The share of each window kept back, in percent, 0 to 100.
    pub reserve_percent: u8,
}

impl QuotaPrefs {
    /// The preferences `routing.quota` gives: `most-left` (any case, with
    /// spaces around) or else `soonest-reset`, and the reserve clamped to 0
    /// to 100.
    pub fn of(quota: &RoutingQuota) -> Self {
        let prefer = match go_lower(quota.prefer.trim()).as_str() {
            "most-left" => QuotaPreference::MostLeft,
            _ => QuotaPreference::SoonestReset,
        };
        Self {
            prefer,
            reserve_percent: u8::try_from(quota.reserve_percent.clamp(0, 100)).unwrap_or(0),
        }
    }
}

/// One quota window of a reading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Window {
    /// How much of it is used, in percent, 0 to 100.
    pub(crate) used: f64,
    /// When it resets.
    pub(crate) reset: Timestamp,
}

/// The current windows of `quota`'s reading at `now`: those with a use and
/// a reset still ahead.
pub(crate) fn windows(quota: &QuotaState, now: Timestamp) -> Vec<Window> {
    let get = |name: &str| {
        quota
            .signals
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let mut out = Vec::new();
    for window in ["5h", "7d"] {
        let prefix = format!("anthropic-ratelimit-unified-{window}-");
        let mut used = get(&format!("{prefix}utilization"))
            .and_then(number)
            .map(|fraction| fraction * 100.0);
        if get(&format!("{prefix}status"))
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("rejected"))
        {
            used = Some(100.0);
        }
        let reset = get(&format!("{prefix}reset")).and_then(timestamp);
        out.extend(current(used, reset, now));
    }
    for window in ["primary", "secondary"] {
        let prefix = format!("x-codex-{window}-");
        let used = get(&format!("{prefix}used-percent")).and_then(number);
        let reset = get(&format!("{prefix}reset-at"))
            .and_then(unix_seconds)
            .or_else(|| {
                let after = get(&format!("{prefix}reset-after-seconds")).and_then(number)?;
                let delta = chrono::TimeDelta::try_milliseconds((after * 1000.0) as i64)?;
                quota.observed_at?.checked_add_signed(delta)
            });
        out.extend(current(used, reset, now));
    }
    out
}

/// The window of `used` and `reset` when it is current at `now`.
fn current(used: Option<f64>, reset: Option<Timestamp>, now: Timestamp) -> Option<Window> {
    let (used, reset) = (used?, reset?);
    (reset > now).then_some(Window {
        used: used.clamp(0.0, 100.0),
        reset,
    })
}

/// A finite number in `text`.
fn number(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|n| n.is_finite())
}

/// A time in `text`: Unix seconds, or RFC 3339.
fn timestamp(text: &str) -> Option<Timestamp> {
    unix_seconds(text).or_else(|| {
        DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .map(|time| time.to_utc())
    })
}

/// A time in `text` as Unix seconds, with a fraction or not.
fn unix_seconds(text: &str) -> Option<Timestamp> {
    let seconds = number(text).filter(|n| *n > 0.0)?;
    DateTime::from_timestamp_millis((seconds * 1000.0) as i64)
}

/// Where one credential stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Standing {
    /// Whether every current window is below the reserve.
    room: bool,
    /// What is left in the binding window, in tenths of a percent.
    left: i64,
    /// When the binding window resets, in Unix seconds, or `None` with no
    /// current window.
    reset: Option<i64>,
}

impl Standing {
    fn of(auth: &Auth, reserve_percent: u8, now: Timestamp) -> Self {
        let windows = windows(&auth.quota, now);
        let limit = 100.0 - f64::from(reserve_percent);
        let binding = windows.iter().max_by(|a, b| {
            a.used
                .total_cmp(&b.used)
                .then_with(|| a.reset.cmp(&b.reset))
        });
        Self {
            room: windows.iter().all(|w| w.used < limit),
            left: binding.map_or(1000, |w| ((100.0 - w.used) * 10.0).round() as i64),
            reset: binding.map(|w| w.reset.timestamp()),
        }
    }
}

/// How `a` ranks against `b`, `Less` being better.
fn order(a: &Standing, b: &Standing, prefer: QuotaPreference) -> Ordering {
    let most_left = || b.left.cmp(&a.left);
    let soonest = || match (a.reset, b.reset) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    b.room.cmp(&a.room).then_with(|| {
        if a.room && prefer == QuotaPreference::SoonestReset {
            soonest().then_with(most_left)
        } else {
            most_left().then_with(soonest)
        }
    })
}

/// The pick among `candidates`, the ready credentials the call may take,
/// by `prefs` at `now`; those that rank the same take turns after `last`,
/// the credential the same pick took last time, which this updates.
pub(crate) fn pick<T>(
    candidates: Vec<T>,
    auth: impl Fn(&T) -> &Auth,
    prefs: QuotaPrefs,
    now: Timestamp,
    last: &mut String,
) -> Option<T> {
    let standings: Vec<Standing> = candidates
        .iter()
        .map(|c| Standing::of(auth(c), prefs.reserve_percent, now))
        .collect();
    let best = *standings.iter().min_by(|a, b| order(a, b, prefs.prefer))?;
    let mut tied: Vec<(&str, usize)> = candidates
        .iter()
        .zip(&standings)
        .enumerate()
        .filter(|(_, (_, standing))| order(standing, &best, prefs.prefer) == Ordering::Equal)
        .map(|(index, (c, _))| (auth(c).id.as_str(), index))
        .collect();
    tied.sort_unstable();
    let ids: Vec<&str> = tied.iter().map(|(id, _)| *id).collect();
    let (id, index) = tied.get(successor_index(&ids, last)).copied()?;
    id.clone_into(last);
    tracing::debug!(
        prefer = ?prefs.prefer,
        reserve_percent = prefs.reserve_percent,
        candidates = standings.len(),
        tied = tied.len(),
        room = best.room,
        left_percent = best.left as f64 / 10.0,
        resets_in_seconds = best.reset.map(|reset| reset - now.timestamp()),
        "quota routing pick"
    );
    candidates.into_iter().nth(index)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone, Utc};

    use super::*;

    fn now() -> Timestamp {
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("time")
    }

    fn reading(pairs: &[(&str, String)]) -> QuotaState {
        QuotaState {
            observed_at: Some(now() - TimeDelta::minutes(5)),
            signals: pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            ..QuotaState::default()
        }
    }

    fn secs(offset: i64) -> String {
        (now().timestamp() + offset).to_string()
    }

    // Not upstream's: the windows read from Claude's and Codex's headers.
    #[test]
    fn reads_claude_and_codex_windows() {
        let claude = reading(&[
            ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.25".into()),
            ("Anthropic-Ratelimit-Unified-5h-Reset", secs(3600)),
            ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.5".into()),
            (
                "Anthropic-Ratelimit-Unified-7d-Reset",
                (now() + TimeDelta::days(3)).to_rfc3339(),
            ),
            // Not read: overage and per-model windows.
            (
                "Anthropic-Ratelimit-Unified-7d_oi-Utilization",
                "0.9".into(),
            ),
            ("Anthropic-Ratelimit-Unified-7d_oi-Reset", secs(3600)),
        ]);
        assert_eq!(
            windows(&claude, now()),
            [
                Window {
                    used: 25.0,
                    reset: now() + TimeDelta::hours(1)
                },
                Window {
                    used: 50.0,
                    reset: now() + TimeDelta::days(3)
                },
            ]
        );
        let codex = reading(&[
            ("X-Codex-Primary-Used-Percent", "12.5".into()),
            ("X-Codex-Primary-Reset-At", secs(600)),
            ("X-Codex-Secondary-Used-Percent", "40".into()),
            // From when the reading came, five minutes ago.
            ("X-Codex-Secondary-Reset-After-Seconds", "900".into()),
            ("X-Codex-Bengalfox-Primary-Used-Percent", "99".into()),
        ]);
        assert_eq!(
            windows(&codex, now()),
            [
                Window {
                    used: 12.5,
                    reset: now() + TimeDelta::minutes(10)
                },
                Window {
                    used: 40.0,
                    reset: now() + TimeDelta::minutes(10)
                },
            ]
        );
    }

    // Not upstream's: a rejected window is full, a window that has reset or
    // has no reset is ignored, and use is clamped.
    #[test]
    fn rejected_stale_and_odd_windows() {
        let quota = reading(&[
            ("Anthropic-Ratelimit-Unified-5h-Status", "Rejected".into()),
            ("Anthropic-Ratelimit-Unified-5h-Reset", secs(60)),
            ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.5".into()),
            ("Anthropic-Ratelimit-Unified-7d-Reset", secs(-60)),
            ("X-Codex-Primary-Used-Percent", "140".into()),
            ("X-Codex-Primary-Reset-At", secs(60)),
            ("X-Codex-Secondary-Used-Percent", "10".into()),
        ]);
        assert_eq!(
            windows(&quota, now()),
            [
                Window {
                    used: 100.0,
                    reset: now() + TimeDelta::minutes(1)
                },
                Window {
                    used: 100.0,
                    reset: now() + TimeDelta::minutes(1)
                },
            ]
        );
        assert!(windows(&QuotaState::default(), now()).is_empty());
    }

    // Not upstream's: the preferences `routing.quota` gives.
    #[test]
    fn prefs_from_config() {
        let prefs = |prefer: &str, reserve_percent| {
            QuotaPrefs::of(&RoutingQuota {
                prefer: prefer.into(),
                reserve_percent,
                ..RoutingQuota::default()
            })
        };
        assert_eq!(prefs("", 0), QuotaPrefs::default());
        assert_eq!(prefs("MOST-LEFT", 10).prefer, QuotaPreference::MostLeft);
        assert_eq!(prefs("x", 101).reserve_percent, 100);
        assert_eq!(prefs("x", -1).reserve_percent, 0);
    }
}
