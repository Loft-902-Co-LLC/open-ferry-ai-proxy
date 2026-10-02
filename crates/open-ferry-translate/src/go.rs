//! Go standard library behaviour that upstream's output depends on.

/// Go's `strings.ToLower`: maps each character on its own by its simple Unicode
/// mapping. Rust's `str::to_lowercase` differs for `İ` (to `i` plus a combining
/// dot) and for a word-final `Σ` (to `ς`); Go gives `i` and `σ`.
pub(crate) fn to_lower(s: &str) -> String {
    // `char::to_lowercase` yields the full mapping. Only `İ` has more than one
    // character, and the first is its simple mapping.
    s.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_lower_matches_go() {
        assert_eq!(to_lower("PRIORITY"), "priority");
        assert_eq!(to_lower("OPENAİ"), "openai");
        assert_eq!(to_lower("ΑΣ"), "ασ");
        assert_eq!(to_lower("Straße ÀÉ"), "straße àé");
    }
}
