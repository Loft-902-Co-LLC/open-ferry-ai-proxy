//! Random cases for config change details. None yet (P3 WP-B).

use crate::cases::Case;

/// `count` random cases for `config-diff/details`, each depending only on `seed` and
/// its index. None yet.
pub fn detail_cases(seed: u64, count: usize) -> Vec<Case> {
    let _ = (seed, count);
    Vec::new()
}
