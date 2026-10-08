//! The early-stop rule for upstream connections, for the OS the run is on.
//!
//! Each connection a proxy closes to the fake upstream holds a port of the
//! machine in TIME_WAIT for a while, and the ports for outgoing connections
//! are shared by every program on the machine. A proxy that opens a
//! connection per request could run them out, which would fail its own
//! requests and other programs' too. So a load level stops once a proxy has
//! opened a quarter of those ports within the TIME_WAIT time, rounded down
//! to hundreds, and the next level waits until fewer than half that many
//! were opened within it. The rule is the same for both proxies; only a
//! proxy that opens many connections meets it.
//!
//! - Windows: 16,384 ports (49152 to 65535) and two minutes, its defaults.
//! - Linux: the range in `/proc/sys/net/ipv4/ip_local_port_range` (32768
//!   to 60999, 28,232 ports, by default) and a minute, which Linux doesn't
//!   let one change. Linux can reuse a port in TIME_WAIT for a new
//!   connection on loopback after a second (`tcp_tw_reuse`, on for
//!   loopback by default), so the rule is cautious there.
//! - macOS: 16,384 ports (49152 to 65535) and 30 seconds, its defaults.

use std::time::Duration;

/// The rule on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortRule {
    /// How long a closed connection's port stays taken.
    pub window: Duration,
    /// The ports for outgoing connections.
    pub ports: usize,
    /// Where `ports` and `window` come from, for the report.
    pub source: String,
    /// New connections a proxy may open within `window`.
    pub budget: usize,
}

impl PortRule {
    /// The rule for `ports` ports and a TIME_WAIT of `window`.
    pub fn new(ports: usize, window: Duration, source: String) -> Self {
        Self {
            window,
            ports,
            source,
            budget: (ports / 4 / 100 * 100).max(100),
        }
    }

    /// The rule for the OS this runs on.
    pub fn for_this_os() -> Self {
        if cfg!(target_os = "linux") {
            let range = std::fs::read_to_string(LINUX_RANGE).ok();
            Self::linux(range.as_deref())
        } else if cfg!(target_os = "macos") {
            Self::new(
                16_384,
                Duration::from_secs(30),
                "macOS's defaults: ports 49152 to 65535, and TIME_WAIT for 30 seconds".to_owned(),
            )
        } else {
            Self::new(
                16_384,
                Duration::from_secs(120),
                "Windows' defaults: ports 49152 to 65535, and TIME_WAIT for two minutes".to_owned(),
            )
        }
    }

    /// The rule on Linux, given the text of [`LINUX_RANGE`] if it could be
    /// read.
    fn linux(range: Option<&str>) -> Self {
        let window = Duration::from_secs(60);
        match range.and_then(parse_range) {
            Some((low, high)) => Self::new(
                usize::from(high - low) + 1,
                window,
                format!(
                    "Linux's ip_local_port_range: ports {low} to {high}, and TIME_WAIT for a \
                     minute"
                ),
            ),
            None => Self::new(
                28_232,
                window,
                "Linux's defaults, as ip_local_port_range couldn't be read: ports 32768 to \
                 60999, and TIME_WAIT for a minute"
                    .to_owned(),
            ),
        }
    }
}

/// Where Linux keeps the range of ports for outgoing connections.
const LINUX_RANGE: &str = "/proc/sys/net/ipv4/ip_local_port_range";

/// The first and last port of `ip_local_port_range`'s text, such as
/// `32768\t60999\n`.
fn parse_range(text: &str) -> Option<(u16, u16)> {
    let mut parts = text.split_whitespace();
    let low: u16 = parts.next()?.parse().ok()?;
    let high: u16 = parts.next()?.parse().ok()?;
    (parts.next().is_none() && low <= high).then_some((low, high))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the range as Linux writes it, and texts that aren't
    // one.
    #[test]
    fn parses_linux_ranges() {
        assert_eq!(parse_range("32768\t60999\n"), Some((32768, 60999)));
        assert_eq!(parse_range("1024 65535"), Some((1024, 65535)));
        assert_eq!(parse_range("60999 32768"), None);
        assert_eq!(parse_range("32768"), None);
        assert_eq!(parse_range("32768 60999 1"), None);
        assert_eq!(parse_range("a b"), None);
    }

    // Not upstream's: a quarter of the ports, in hundreds, within each OS's
    // TIME_WAIT time.
    #[test]
    fn budgets() {
        let windows = PortRule::new(16_384, Duration::from_secs(120), String::new());
        assert_eq!(windows.budget, 4_000);
        let linux = PortRule::linux(Some("32768\t60999\n"));
        assert_eq!(
            (linux.ports, linux.budget, linux.window),
            (28_232, 7_000, Duration::from_secs(60))
        );
        assert_eq!(
            linux.source,
            "Linux's ip_local_port_range: ports 32768 to 60999, and TIME_WAIT for a minute"
        );
        let wide = PortRule::linux(Some("1024 65535"));
        assert_eq!((wide.ports, wide.budget), (64_512, 16_100));
        let unread = PortRule::linux(None);
        assert_eq!((unread.ports, unread.budget), (28_232, 7_000));
        assert!(unread.source.starts_with("Linux's defaults"));
        // A tiny range still lets a level run.
        assert_eq!(PortRule::linux(Some("40000 40099")).budget, 100);
    }
}
