//! Parsed comment budgets and owners for offline proof diagnostics.
use super::{Node, unmarshal};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    Key(String, String),
    Index(usize),
}

/// One parsed comment line and the mapping key/sequence item that owns it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CommentOccurrence {
    text: String,
    owner: Vec<Part>,
}

fn inventory(text: &str, expand: bool) -> Option<Vec<CommentOccurrence>> {
    let doc = unmarshal(text.as_bytes()).ok()?;
    let root = doc.content.first()?;
    let expanded;
    let root = if expand {
        expanded = super::super::expand_config_aliases(root).ok()?;
        &expanded
    } else {
        root
    };
    fn visit(n: &Node, path: &mut Vec<Part>, out: &mut Vec<CommentOccurrence>) -> Option<()> {
        for text in [&n.head_comment, &n.line_comment, &n.foot_comment] {
            for line in text.lines() {
                let line = line.trim_start_matches([' ', '\t']);
                if !line.is_empty() {
                    if !line.starts_with('#') {
                        return None;
                    }
                    out.push(CommentOccurrence {
                        text: line.into(),
                        owner: path.clone(),
                    });
                }
            }
        }
        match n.kind {
            super::super::Kind::Mapping => {
                let mut seen = BTreeSet::new();
                for (key, value) in n.pairs() {
                    if key.kind != super::super::Kind::Scalar {
                        return None;
                    }
                    let part = Part::Key(key.tag.clone(), key.value.clone());
                    if !seen.insert(part.clone()) {
                        return None;
                    }
                    path.push(part);
                    visit(key, path, out)?;
                    visit(value, path, out)?;
                    path.pop();
                }
            }
            super::super::Kind::Sequence => {
                for (i, value) in n.content.iter().enumerate() {
                    path.push(Part::Index(i));
                    visit(value, path, out)?;
                    path.pop();
                }
            }
            _ => {}
        }
        Some(())
    }
    let mut out = Vec::new();
    visit(&doc, &mut Vec::new(), &mut out)?;
    // The document's children are traversed separately so root comments retain
    // the empty owner, rather than inventing a document index.
    visit(root, &mut Vec::new(), &mut out)?;
    Some(out)
}

/// Count parsed comments, excluding scalar data, optionally expanding aliases.
pub fn comment_occurrences(text: &str, comment: &str, expand: bool) -> Option<usize> {
    Some(
        inventory(text, expand)?
            .iter()
            .filter(|c| c.text == comment)
            .count(),
    )
}

/// Check 3's multiset budgets and placement, without certifying other checks.
/// Each protected occurrence needs its own source occurrence. Occurrences at
/// an owner absent in the source additionally consume a shared ablated slot.
pub fn retained_comment_ownership(source: &str, ablated: &str, protected: &str) -> bool {
    fn counts(items: &[CommentOccurrence]) -> BTreeMap<&str, usize> {
        let mut result = BTreeMap::new();
        for c in items {
            *result.entry(c.text.as_str()).or_default() += 1;
        }
        result
    }
    let Some((s, g, r)) = inventory(source, true)
        .zip(inventory(ablated, false))
        .zip(inventory(protected, false))
        .map(|((s, g), r)| (s, g, r))
    else {
        return false;
    };
    let (sc, gc, rc) = (counts(&s), counts(&g), counts(&r));
    if rc
        .iter()
        .any(|(c, n)| *n > sc.get(c).copied().unwrap_or_default())
        || gc
            .iter()
            .any(|(c, n)| *n > rc.get(c).copied().unwrap_or_default())
    {
        return false;
    }
    let mut owners = BTreeMap::new();
    for c in &r {
        *owners.entry((&c.text, &c.owner)).or_insert(0usize) += 1;
    }
    owners.into_iter().all(|((text, owner), n)| {
        n <= s
            .iter()
            .filter(|c| &c.text == text && &c.owner == owner)
            .count()
            + g.iter()
                .filter(|c| &c.text == text && &c.owner == owner)
                .count()
    })
}

/// An unloadable ablated file supplies counts only. Every protected occurrence
/// must reload at its source owner; an emission trace can never supply an owner.
pub fn retained_unloadable_comments(source: &str, protected: &str, ablated: &[String]) -> bool {
    let Some((s, r)) = inventory(source, true).zip(inventory(protected, false)) else {
        return false;
    };
    let mut sc = BTreeMap::new();
    let mut rc = BTreeMap::new();
    let mut gc = BTreeMap::new();
    for c in &s {
        *sc.entry((&c.text, &c.owner)).or_insert(0usize) += 1;
    }
    for c in &r {
        *rc.entry((&c.text, &c.owner)).or_insert(0usize) += 1;
    }
    for c in ablated {
        *gc.entry(c).or_insert(0usize) += 1;
    }
    r.iter().all(|c| {
        r.iter().filter(|other| other.text == c.text).count()
            <= s.iter().filter(|other| other.text == c.text).count()
    }) && rc
        .iter()
        .all(|(owner, n)| *n <= sc.get(owner).copied().unwrap_or_default())
        && gc
            .into_iter()
            .all(|(text, n)| n <= r.iter().filter(|c| &c.text == text).count())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unloadable_output_never_supplies_a_comment_owner() {
        let source = "auth-dir: /old # own\ndebug: false\n";
        let protected = "auth-dir: /x\ndebug: false # own\n";
        // Even an exact trace of invalid Go output may supply multiplicity
        // only. This placement isn't in the parsed source.
        assert!(!retained_unloadable_comments(
            source,
            protected,
            &["# own".into()]
        ));
        assert!(retained_unloadable_comments(
            source,
            "# own\nauth-dir: /x\ndebug: false\n",
            &["# own".into()]
        ));
        assert!(!retained_unloadable_comments(
            source,
            "auth-dir: /x # own\n",
            &["# own".into(), "# own".into()]
        ));
    }

    #[test]
    fn budgets_are_multisets_and_scalar_data_is_not_a_comment() {
        assert_eq!(
            comment_occurrences("x: |\n  data # c\ny: '# c'\nz: value # c\n", "# c", false),
            Some(1)
        );
        let source = "a: 1 # c\nb: 2 # c\nconfig-version: 8 # cv\n";
        assert!(!retained_comment_ownership(
            source,
            "a: 1 # c\nb: 2 # c\n",
            "a: 1 # c\nb: 2\nconfig-version: 8 # cv\n"
        ));
        assert!(!retained_comment_ownership(
            source,
            "a: 1\nb: 2\n",
            "a: 1 # invented\nb: 2\n"
        ));
        assert!(!retained_comment_ownership(
            source,
            "a: 1\nb: 2\n",
            "a: 1 # c\nb: 2 # c\nc: 3 # c\n"
        ));
    }
    #[test]
    fn owners_must_be_source_or_ablated_owners_with_occurrence_budgets() {
        let source = "debug: false\nmodels: &m # k\n  catalog: '' # v\nconfig-version: 8 # cv\n";
        let go = "models:\n  catalog: '' # v\nconfig-version: 8 # k\nobservability:\n  logs:\n    debug: false\n";
        let wrong = "models:\n  catalog: '' # v\nconfig-version: 8 # cv\nobservability: # k\n  logs:\n    debug: false\n";
        let fixed = "models:\n  catalog: '' # v\n# k\nconfig-version: 8 # cv\nobservability:\n  logs:\n    debug: false\n";
        assert!(!retained_comment_ownership(source, go, wrong));
        assert!(retained_comment_ownership(source, go, fixed));
        let v4 = "debug: false\nconfig-version:\n  # hv\n  8 # cv\nport: 1\n";
        assert!(!retained_comment_ownership(
            v4,
            "config-version: 8\nserver:\n  port: 1\n",
            "config-version: 8 # cv\n# hv\nserver:\n  port: 1\n"
        ));
        assert!(retained_comment_ownership(
            v4,
            "config-version: 8\nserver:\n  port: 1\n",
            "# hv\nconfig-version: 8 # cv\nserver:\n  port: 1\n"
        ));
    }
}
