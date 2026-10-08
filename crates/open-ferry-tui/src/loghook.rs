// Ported from CLIProxyAPI internal/tui/loghook.go (NewLogHook, Fire, Chan)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The log lines the standalone mode's embedded server writes, held for the
//! logs tab to show as they come.
//!
//! Upstream's hook is a logrus hook that formats each entry with the file
//! log's formatter; here the server's logger hands the hook each line as it
//! formats it for the file log. As upstream's buffered channel does, the
//! hook holds up to its capacity, and drops the oldest line when full.
//!
//! Deviations from upstream:
//! - Control characters other than newlines are taken out of each line,
//!   so a client can't write escape sequences to the terminal through
//!   text the server logs; tabs become spaces. Upstream shows lines as
//!   they come.
//! - A capacity of 0 holds one line. Upstream's unbuffered channel then
//!   drops every line no one is waiting for.
//! - Upstream's `SetFormatter` and `Levels` aren't ported: the lines come
//!   formatted, at every level.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Notify;

use crate::client;

/// Where the logs tab reads the embedded server's log lines from
/// (upstream's `LogHook`). Clones share the lines.
#[derive(Debug, Clone)]
pub struct LogHook {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    lines: Mutex<VecDeque<String>>,
    capacity: usize,
    ready: Notify,
}

impl LogHook {
    /// `NewLogHook`: a hook holding up to `capacity` lines.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                lines: Mutex::new(VecDeque::new()),
                capacity: capacity.max(1),
                ready: Notify::new(),
            }),
        }
    }

    /// `Fire`: takes a formatted log line, without waiting; when the hook
    /// is full, the oldest line goes.
    pub fn send(&self, line: &str) {
        let line = client::clean(line.trim_end_matches(['\n', '\r']));
        {
            let mut lines = self
                .inner
                .lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if lines.len() >= self.inner.capacity {
                lines.pop_front();
            }
            lines.push_back(line);
        }
        self.inner.ready.notify_one();
    }

    /// The next line, waiting for one (reading upstream's `Chan`).
    pub(crate) async fn recv(&self) -> String {
        loop {
            let notified = self.inner.ready.notified();
            if let Some(line) = self
                .inner
                .lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
            {
                return line;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    // Not upstream's: lines come in order, the oldest goes when the hook
    // is full, and a reader waits for a line.
    #[tokio::test]
    async fn holds_lines_as_upstream_does() {
        let hook = LogHook::new(2);
        hook.send("one\r\n");
        hook.send("two");
        hook.send("three\u{1b}[2J\tend\n");
        assert_eq!(hook.recv().await, "two");
        assert_eq!(hook.recv().await, "three[2J end");
        let reader = hook.clone();
        let waiting = tokio::spawn(async move { reader.recv().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        hook.send("four");
        assert_eq!(waiting.await.unwrap(), "four");
        let tiny = LogHook::new(0);
        tiny.send("a");
        tiny.send("b");
        assert_eq!(tiny.recv().await, "b");
    }
}
