//! Not upstream's: the supervisor's time limits, in one place (v3 1.1, review 3).
//! The supervisor tells a worker its budgets in `welcome`. Tests shorten them
//! by building a [`Timings`] value, never through the environment.

use std::time::Duration;

use crate::protocol::Budgets;

pub const LOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an active worker that has greeted may take to report `loaded`.
/// The worker's own 60 s budget covers only the lease wait; preparing and
/// activating come after it with no deadline of their own, so the supervisor's
/// deadline covers all three and is three times `LOAD_TIMEOUT`: a healthy load
/// on a slow machine must not walk the ladder, a hung one is still ended.
pub const ACTIVE_LOAD_TIMEOUT: Duration = Duration::from_secs(3 * 60);
pub const HANDOVER_BUDGET: Duration = Duration::from_secs(60);
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
pub const RESUME_TIMEOUT: Duration = Duration::from_secs(10);
pub const WATCH_WINDOW: Duration = Duration::from_secs(10 * 60);
pub const DRAIN_CAP: Duration = Duration::from_secs(30 * 60);
pub const SIGNIN_WAIT: Duration = Duration::from_secs(5 * 60);
pub const STOP_BUDGET: Duration = Duration::from_secs(50);
pub const PIN_TTL: Duration = Duration::from_secs(10 * 60);
pub const INSTALL_RETRY: Duration = Duration::from_secs(60);
pub const INSTALL_RETRY_MAX: Duration = Duration::from_secs(60 * 60);
pub const RENAME_TRIES: u32 = 5;
pub const DETECT_TIMEOUT: Duration = Duration::from_secs(3);
pub const STATUS_EVERY: Duration = Duration::from_secs(5);
pub const JOURNAL_WRITE_TRIES: u32 = 3;
/// The first wait before an active worker that exited is started again.
pub const RESTART_FIRST: Duration = Duration::from_secs(5);
pub const RESTART_MAX: Duration = Duration::from_secs(60);
/// A worker that stayed up this long starts the backoff over.
pub const RESTART_RESET: Duration = Duration::from_secs(10 * 60);
/// The supervisor's own panic re-entries allowed in a window (lead answer 1).
pub const PANIC_REENTRIES: usize = 3;
pub const PANIC_WINDOW: Duration = Duration::from_secs(10 * 60);
/// How often the supervisor looks at a running worker's process.
pub const EXIT_POLL: Duration = Duration::from_millis(20);
/// Warning interval while the update lock is waited for at startup (T6).
pub const LOCK_WARN_EVERY: Duration = Duration::from_secs(30);
/// How often that wait looks at the lock again.
pub const LOCK_POLL: Duration = Duration::from_millis(250);

/// The limits one supervisor runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    pub load: Duration,
    /// The wait for an active worker's `loaded` after its handshake (a
    /// standby's is `load`).
    pub active_load: Duration,
    pub handover: Duration,
    pub ready: Duration,
    pub resume: Duration,
    pub watch: Duration,
    pub drain: Duration,
    pub signin_wait: Duration,
    pub stop: Duration,
    pub pin_ttl: Duration,
    pub install_retry: Duration,
    pub install_retry_max: Duration,
    pub status_every: Duration,
    pub restart_first: Duration,
    pub restart_max: Duration,
    pub restart_reset: Duration,
    pub exit_poll: Duration,
    pub lock_warn_every: Duration,
    pub lock_poll: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            load: LOAD_TIMEOUT,
            active_load: ACTIVE_LOAD_TIMEOUT,
            handover: HANDOVER_BUDGET,
            ready: READY_TIMEOUT,
            resume: RESUME_TIMEOUT,
            watch: WATCH_WINDOW,
            drain: DRAIN_CAP,
            signin_wait: SIGNIN_WAIT,
            stop: STOP_BUDGET,
            pin_ttl: PIN_TTL,
            install_retry: INSTALL_RETRY,
            install_retry_max: INSTALL_RETRY_MAX,
            status_every: STATUS_EVERY,
            restart_first: RESTART_FIRST,
            restart_max: RESTART_MAX,
            restart_reset: RESTART_RESET,
            exit_poll: EXIT_POLL,
            lock_warn_every: LOCK_WARN_EVERY,
            lock_poll: LOCK_POLL,
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Timings {
    /// What a worker is told in `welcome`.
    pub fn budgets(&self) -> Budgets {
        Budgets {
            load_ms: millis(self.load),
            handover_ms: millis(self.handover),
            ready_ms: millis(self.ready),
            resume_ms: millis(self.resume),
            watch_ms: millis(self.watch),
            drain_ms: millis(self.drain),
            stop_ms: millis(self.stop_message_budget()),
        }
    }

    /// The budget a `stop` carries: the supervisor keeps five seconds to kill.
    pub fn stop_message_budget(&self) -> Duration {
        self.stop.saturating_sub(Duration::from_secs(5))
    }
}

/// The wait before an active worker that exited is started again (5 s doubling
/// to 60 s, starting over after 10 minutes up).
#[derive(Debug, Clone)]
pub struct Backoff {
    first: Duration,
    max: Duration,
    reset: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(timings: &Timings) -> Self {
        Self {
            first: timings.restart_first,
            max: timings.restart_max,
            reset: timings.restart_reset,
            next: timings.restart_first,
        }
    }

    /// The wait after a worker that ran for `alive` exited.
    pub fn after(&mut self, alive: Duration) -> Duration {
        if alive >= self.reset {
            self.next = self.first;
        }
        let wait = self.next;
        self.next = (self.next * 2).min(self.max);
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_the_cap_and_starts_over_after_a_long_run() {
        let mut backoff = Backoff::new(&Timings::default());
        let quick = Duration::from_secs(1);
        let waits: Vec<u64> = (0..6).map(|_| backoff.after(quick).as_secs()).collect();
        assert_eq!(waits, [5, 10, 20, 40, 60, 60]);
        assert_eq!(backoff.after(WATCH_WINDOW).as_secs(), 5);
        assert_eq!(backoff.after(quick).as_secs(), 10);
    }

    #[test]
    fn budgets_are_the_documented_ones_in_milliseconds() {
        let budgets = Timings::default().budgets();
        assert_eq!(budgets.load_ms, 60_000);
        assert_eq!(budgets.ready_ms, 30_000);
        assert_eq!(budgets.drain_ms, 1_800_000);
        assert_eq!(
            budgets.stop_ms, 45_000,
            "the stop message's budget, not the kill deadline"
        );
        assert_eq!(Timings::default().stop_message_budget().as_secs(), 45);
    }
}
