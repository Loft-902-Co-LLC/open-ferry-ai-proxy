//! Random cases for first-token events. None yet (P3 WP-C).

use crate::cases::Case;

/// `count` random cases for `ttft/token-event`, each depending only on `seed` and
/// its index. None yet.
pub fn token_event_cases(seed: u64, count: usize) -> Vec<Case> {
    let _ = (seed, count);
    Vec::new()
}
