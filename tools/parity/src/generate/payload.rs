//! Random cases for the config's payload rules. None yet (P3 WP-D).

use crate::cases::Case;

/// `count` random cases for `payload/apply`, each depending only on `seed` and
/// its index. None yet.
pub fn apply_cases(seed: u64, count: usize) -> Vec<Case> {
    let _ = (seed, count);
    Vec::new()
}
