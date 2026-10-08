// Ported from CLIProxyAPI internal/translator/codex/claude/codex_claude_request.go,
// codex/gemini/codex_gemini_request.go and
// codex/openai/chat-completions/codex_openai_request.go (the makeUnique closures
// of buildShortNameMap) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Giving each tool a name no other tool has, for the translators that
//! shorten tool names to fit the Responses limit.
//!
//! Deviations from upstream:
//! - Upstream tries every suffix from `_1` on for each name that's taken, so a
//!   request declaring the same tool name thousands of times took quadratic
//!   time. [`UniqueNames`] remembers where each search stopped instead; the
//!   names it gives out are the same.
//! - A name is cut at a character boundary, where upstream cuts at a byte and
//!   can leave half a UTF-8 character.

use std::collections::{HashMap, HashSet};

/// The names given out so far, and where to resume each search for a free
/// suffix.
#[derive(Default)]
pub(super) struct UniqueNames {
    used: HashSet<String>,
    /// For a prefix and a suffix width in digits, the first suffix not known
    /// to be taken. Names are only ever added, so one found taken stays so.
    next: HashMap<(String, u32), u64>,
}

impl UniqueNames {
    /// `makeUnique`: `candidate` if it's free, or else the first free one of
    /// `candidate_1`, `candidate_2` and so on, each with `candidate` cut so
    /// the name fits in `limit` bytes. The name is then taken.
    pub(super) fn claim(&mut self, candidate: &str, limit: usize) -> String {
        let name = if self.used.contains(candidate) {
            self.suffixed(candidate, limit)
        } else {
            candidate.to_owned()
        };
        self.used.insert(name.clone());
        name
    }

    fn suffixed(&mut self, candidate: &str, limit: usize) -> String {
        // Every suffix of a width comes after the same prefix, so the search
        // for that width can resume where the last one with the prefix left
        // off.
        for width in 1..=u64::MAX.ilog10() + 1 {
            let prefix = truncate_bytes(candidate, limit.saturating_sub(width as usize + 1));
            let end = 10u64.checked_pow(width).unwrap_or(u64::MAX);
            let next = self
                .next
                .entry((prefix.to_owned(), width))
                .or_insert(10u64.pow(width - 1));
            while *next < end {
                let name = format!("{prefix}_{next}");
                *next += 1;
                if !self.used.contains(&name) {
                    return name;
                }
            }
        }
        unreachable!("fewer names than suffixes")
    }
}

/// Cuts `s` to at most `max` bytes without splitting a character.
pub(super) fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upstream's search, which starts over at `_1` every time.
    fn naive(used: &mut HashSet<String>, candidate: &str, limit: usize) -> String {
        let name = if used.contains(candidate) {
            (1u64..)
                .map(|n| {
                    let suffix = format!("_{n}");
                    let prefix = truncate_bytes(candidate, limit.saturating_sub(suffix.len()));
                    format!("{prefix}{suffix}")
                })
                .find(|name| !used.contains(name))
                .unwrap()
        } else {
            candidate.to_owned()
        };
        used.insert(name.clone());
        name
    }

    fn check(candidates: &[&str], limit: usize) {
        let mut names = UniqueNames::default();
        let mut used = HashSet::new();
        for candidate in candidates {
            assert_eq!(
                names.claim(candidate, limit),
                naive(&mut used, candidate, limit),
                "{candidate:?}"
            );
        }
    }

    #[test]
    fn matches_upstream() {
        check(&["f", "f", "f", "g", "f"], 64);
        check(&["f_1", "f", "f", "f_3", "f", "f", "f_2"], 64);
        check(&["f_1_1", "f_1", "f", "f", "f_1", "f_1"], 64);
        check(&["abcd", "abcd", "abc_1", "abcd", "abcd"], 5);
        check(&["ab", "abc", "abc", "ab", "abc", "ab"], 4);
        check(&["x"; 120], 64);
        check(&["x"; 120], 3);
        check(&["x"; 30], 1);
        check(&["éé", "éé", "éé"], 4);
        let long = "n".repeat(70);
        check(&[&long, &long, &long[..64], &long, &long[..62]], 64);
        let mut mixed: Vec<String> = (1..=150).map(|n| format!("t_{}", n * 7 % 151)).collect();
        mixed.extend(std::iter::repeat_n("t".to_owned(), 160));
        mixed.extend((1..=30).map(|n| format!("t_{n}")));
        mixed.extend(std::iter::repeat_n("t".to_owned(), 40));
        check(&mixed.iter().map(String::as_str).collect::<Vec<_>>(), 64);
    }

    #[test]
    fn many_duplicates_are_fast() {
        let mut names = UniqueNames::default();
        for _ in 0..100_000 {
            names.claim("f", 64);
        }
        assert_eq!(names.claim("f", 64), "f_100000");
        assert_eq!(names.claim("f_100001", 64), "f_100001");
        assert_eq!(names.claim("f", 64), "f_100002");
        assert_eq!(names.claim("f_100002", 64), "f_100002_1");
    }
}
