//! Not upstream's: what the supervisor does when a part of itself panics (lead
//! answer 1). The caller logs the panic at error level, stops every worker as a
//! service stop does, drops all state except the sockets, `supervisor.lock` and
//! the endpoint, and runs startup recovery again. At most [`PANIC_REENTRIES`]
//! times in [`PANIC_WINDOW`]; after that the supervisor exits non-zero.

use std::collections::VecDeque;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::time::Instant;

/// A future that turns a panic in `F` into `Err(message)`.
pub struct CatchUnwind<F>(pub Pin<Box<F>>);

impl<F: Future> CatchUnwind<F> {
    pub fn new(future: F) -> Self {
        Self(Box::pin(future))
    }
}

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, String>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match catch_unwind(AssertUnwindSafe(|| self.0.as_mut().poll(cx))) {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(panic_message(payload.as_ref()))),
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic with no message".to_owned()
    }
}

/// Counts re-entries in a sliding window.
#[derive(Debug, Clone)]
pub struct ReentryLimit {
    allowed: usize,
    window: Duration,
    at: VecDeque<Instant>,
}

impl ReentryLimit {
    pub fn new(allowed: usize, window: Duration) -> Self {
        Self {
            allowed,
            window,
            at: VecDeque::new(),
        }
    }

    /// Records a panic at `now`. `true` if the supervisor may re-enter, `false`
    /// if this one is over the limit and it must exit.
    pub fn allow(&mut self, now: Instant) -> bool {
        while self
            .at
            .front()
            .is_some_and(|first| now.saturating_duration_since(*first) >= self.window)
        {
            self.at.pop_front();
        }
        if self.at.len() >= self.allowed {
            return false;
        }
        self.at.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panic_becomes_an_error_and_a_normal_end_a_value() {
        let panicked = CatchUnwind::new(async {
            if true {
                panic!("the state machine broke");
            }
            1
        })
        .await;
        assert_eq!(panicked, Err("the state machine broke".to_owned()));
        assert_eq!(CatchUnwind::new(async { 7 }).await, Ok(7));
    }

    #[tokio::test(start_paused = true)]
    async fn three_re_entries_in_ten_minutes_are_allowed_and_the_fourth_is_not() {
        let mut limit = ReentryLimit::new(3, Duration::from_secs(600));
        let start = Instant::now();
        assert!(limit.allow(start));
        assert!(limit.allow(start + Duration::from_secs(100)));
        assert!(limit.allow(start + Duration::from_secs(200)));
        assert!(!limit.allow(start + Duration::from_secs(300)));
        // The first has left the window.
        assert!(limit.allow(start + Duration::from_secs(601)));
        assert!(!limit.allow(start + Duration::from_secs(602)));
    }
}
