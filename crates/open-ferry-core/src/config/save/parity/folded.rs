//! A direct scalar-loss proof, using both emitter modes and unchanged source
//! values. Unlike a counterfactual, this keeps folded style in both outputs.
use super::*;
use crate::config::yaml3::encode;

struct Scalar {
    start: usize,
    end: usize,
    style: ScalarStyle,
    value: String,
    line: usize,
    column: usize,
}

fn scalars(text: &str) -> Option<Vec<Scalar>> {
    if text.contains(['\r', '\u{85}', '\u{2028}', '\u{2029}']) {
        return None;
    }
    unmarshal(text.as_bytes()).ok()?;
    let offset = |line, column| {
        let start: usize = text.split_inclusive('\n').take(line).map(str::len).sum();
        let rest = text.get(start..)?;
        Some(start + rest.char_indices().nth(column).map_or(rest.len(), |p| p.0))
    };
    let mut parser = Parser::new(text.as_bytes());
    let mut out = Vec::new();
    loop {
        let token = parser.scan().ok()?;
        if matches!(token.typ, TokenType::No | TokenType::StreamEnd) {
            return Some(out);
        }
        if token.typ == TokenType::Scalar {
            out.push(Scalar {
                start: offset(token.start_mark.line, token.start_mark.column)?,
                end: offset(token.end_mark.line, token.end_mark.column)?,
                style: token.style,
                value: String::from_utf8(token.value).ok()?,
                line: token.start_mark.line + 1,
                column: token.start_mark.column + 1,
            });
        }
    }
}

/// Decoded folded strings retained with the complete raw-output evidence.
pub fn folded_scalar_values(text: &str) -> Option<Vec<String>> {
    Some(
        scalars(text)?
            .into_iter()
            .filter(|s| s.style == ScalarStyle::Folded)
            .map(|s| s.value)
            .collect(),
    )
}

fn root(text: &str) -> Option<Node> {
    let doc = unmarshal(text.as_bytes()).ok()?;
    let node = doc.content.first()?;
    let node = super::super::expand_config_aliases(node).ok()?;
    super::super::flatten_v8(&node).ok()
}

#[derive(Clone, PartialEq, Eq)]
enum Part {
    Key(String),
    Index(usize),
}

fn at_path<'a>(node: &'a Node, path: &[Part], out: &mut Vec<(Vec<Part>, &'a Node)>) {
    match node.kind {
        super::super::Kind::Mapping => {
            for [key, value] in node.content.as_chunks::<2>().0 {
                let mut child = path.to_vec();
                child.push(Part::Key(key.value.clone()));
                at_path(value, &child, out);
            }
        }
        super::super::Kind::Sequence => {
            for (i, value) in node.content.iter().enumerate() {
                let mut child = path.to_vec();
                child.push(Part::Index(i));
                at_path(value, &child, out);
            }
        }
        _ => out.push((path.into(), node)),
    }
}

fn body_indent(text: &str) -> Option<usize> {
    text.lines()
        .skip(1)
        .filter(|s| !s.trim_matches(' ').is_empty())
        .map(|s| s.len() - s.trim_start_matches(' ').len())
        .min()
}

/// Put a bare emitter's scalar at the saved scalar's body indent, keeping
/// the exact original header comment. No other formatting can change.
fn positioned(bare: &str, reference: &str, indent: usize) -> Option<String> {
    let (header, body) = bare.split_once('\n')?;
    let original_header = reference.split_once('\n')?.0;
    let indicator = original_header
        .bytes()
        .take_while(|b| matches!(b, b'>' | b'+' | b'-' | b'1'..=b'9'))
        .count();
    let suffix = original_header.get(indicator..)?;
    if !suffix.trim_start_matches([' ', '\t']).is_empty()
        && !suffix.trim_start_matches([' ', '\t']).starts_with('#')
    {
        return None;
    }
    let old_indent = body_indent(bare)?;
    let mut out = format!("{header}{suffix}\n");
    for line in body.split_inclusive('\n') {
        if line.trim_matches([' ', '\n']).is_empty() {
            out.push_str(line);
        } else {
            out.push_str(&" ".repeat(indent));
            out.push_str(line.get(old_indent..)?);
        }
    }
    Some(out)
}

/// Prove every changed span is one source-preserved folded scalar. The old
/// emitter must reproduce its Go bytes, the protected emitter its Rust bytes,
/// and replacing only those scalar spans must make the whole text identical.
pub fn folded_scalar_reencode(
    sources: &[&str],
    go: &str,
    rust: &str,
    configs: &[&str],
    paths: &[Vec<String>],
    passes: usize,
) -> bool {
    fn prove(
        sources: &[&str],
        go: &str,
        rust: &str,
        configs: &[&str],
        paths: &[Vec<String>],
        passes: usize,
    ) -> Option<bool> {
        if !(1..=2).contains(&passes) {
            return None;
        }
        let nodes = sources
            .iter()
            .map(|s| root(s))
            .collect::<Option<Vec<_>>>()?;
        let mut source_values = Vec::new();
        for node in &nodes {
            at_path(node, &[], &mut source_values);
        }
        let rust_node = root(rust)?;
        let mut rust_values = Vec::new();
        at_path(&rust_node, &[], &mut rust_values);
        let mut forbidden: Vec<Vec<Part>> = paths
            .iter()
            .map(|p| p.iter().cloned().map(Part::Key).collect())
            .collect();
        for config in configs {
            let config = root(config)?;
            forbidden.extend(
                config
                    .pairs()
                    .map(|(key, _)| vec![Part::Key(key.value.clone())]),
            );
        }
        let (gs, rs) = (scalars(go)?, scalars(rust)?);
        if gs.len() != rs.len() {
            return None;
        }
        let mut fixed = String::new();
        let mut last = 0;
        let mut changed = false;
        for (g, r) in gs.iter().zip(&rs) {
            let (graw, rraw) = (go.get(g.start..g.end)?, rust.get(r.start..r.end)?);
            if graw == rraw {
                continue;
            }
            let (path, value) = rust_values
                .iter()
                .find(|(_, n)| n.line == r.line && n.column == r.column)?;
            // Defense in depth: for this comment-free bare-node replay, the
            // repaired clip emitter already makes every lossless pair byte
            // identical. The only remaining folded differences change values.
            if g.value == r.value
                || g.style != ScalarStyle::Folded
                || r.style != ScalarStyle::Folded
                || value.tag != STR_TAG
                || forbidden.iter().any(|p| path.starts_with(p))
                || !source_values.iter().any(|(p, n)| {
                    p == path
                        && n.tag == STR_TAG
                        && n.style.has(Style::FOLDED)
                        && n.value == r.value
                })
                || g.start < last
            {
                return None;
            }
            let indent = body_indent(rraw)?;
            let mut node = Node::scalar(STR_TAG, &r.value);
            node.style = Style::FOLDED;
            let matches = [2, 4].into_iter().any(|width| {
                let mut old_node = node.clone();
                let mut old = Vec::new();
                for _ in 0..passes {
                    let Ok(bytes) = encode::faithful_node(&old_node, width) else {
                        return false;
                    };
                    let Ok(parsed) = unmarshal(&bytes) else {
                        return false;
                    };
                    let Some(value) = parsed.content.first() else {
                        return false;
                    };
                    old_node.value.clone_from(&value.value);
                    old = bytes;
                }
                let Ok((new, _)) = encode::protected_node(&node, width) else {
                    return false;
                };
                let (Ok(old), Ok(new)) = (std::str::from_utf8(&old), std::str::from_utf8(&new))
                else {
                    return false;
                };
                positioned(old, rraw, indent).as_deref() == Some(graw)
                    && positioned(new, rraw, indent).as_deref() == Some(rraw)
                    && scalars(old).is_some_and(|s| s.len() == 1 && s[0].value == g.value)
            });
            if !matches {
                return None;
            }
            fixed.push_str(go.get(last..g.start)?);
            fixed.push_str(rraw);
            last = g.end;
            changed = true;
        }
        fixed.push_str(go.get(last..)?);
        Some(changed && fixed == rust)
    }
    prove(sources, go, rust, configs, paths, passes) == Some(true)
}

/// A separate known difference, never an equivalence: only extra trailing LF
/// bytes in keep-chomped scalars may differ from the source-preserving output.
pub fn extra_keep_chomped_newlines(source: &str, go: &str, rust: &str) -> bool {
    fn prove(source: &str, go: &str, rust: &str) -> Option<bool> {
        let source_node = root(source)?;
        let rust_node = root(rust)?;
        let mut source_values = Vec::new();
        let mut rust_values = Vec::new();
        at_path(&source_node, &[], &mut source_values);
        at_path(&rust_node, &[], &mut rust_values);
        let (ss, gs, rs) = (scalars(source)?, scalars(go)?, scalars(rust)?);
        if gs.len() != rs.len() {
            return None;
        }
        let mut fixed = String::new();
        let mut last = 0;
        let mut changed = false;
        for (g, r) in gs.iter().zip(&rs) {
            let (graw, rraw) = (go.get(g.start..g.end)?, rust.get(r.start..r.end)?);
            if graw == rraw {
                continue;
            }
            let (path, value) = rust_values
                .iter()
                .find(|(_, n)| n.line == r.line && n.column == r.column)?;
            let original = source_values.iter().find(|(p, _)| p == path)?.1;
            let source_scalar = ss
                .iter()
                .find(|s| s.line == original.line && s.column == original.column)?;
            let keep = |text: &str| {
                text.lines().next().is_some_and(|header| {
                    let indicator = header
                        .bytes()
                        .take_while(|b| matches!(b, b'>' | b'|' | b'+' | b'-' | b'1'..=b'9'))
                        .collect::<Vec<_>>();
                    indicator.contains(&b'+')
                })
            };
            let extra = g.value.strip_prefix(&original.value)?;
            let raw_extra = graw.strip_prefix(rraw)?;
            if original.tag != STR_TAG
                || value.tag != STR_TAG
                || original.value != r.value
                || !matches!(g.style, ScalarStyle::Literal | ScalarStyle::Folded)
                || g.style != r.style
                || source_scalar.style != r.style
                || !keep(source.get(source_scalar.start..source_scalar.end)?)
                || !keep(graw)
                || !keep(rraw)
                || extra.is_empty()
                || !extra.bytes().all(|b| b == b'\n')
                || raw_extra != extra
                || g.start < last
            {
                return None;
            }
            fixed.push_str(go.get(last..g.start)?);
            fixed.push_str(rraw);
            last = g.end;
            changed = true;
        }
        fixed.push_str(go.get(last..)?);
        Some(changed && fixed == rust)
    }
    prove(source, go, rust) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lossless_bare_folded_replays_are_already_byte_identical() {
        let lines = ["ordinary", " indented", "\tindented", "", "long words "];
        let mut checked = 0;
        for a in lines {
            for b in lines {
                for c in lines {
                    for trailing in 0..4 {
                        for width in [2, 4] {
                            let value = format!("{a}\n{b}\n{c}{}", "\n".repeat(trailing));
                            let mut node = Node::scalar(STR_TAG, &value);
                            node.style = Style::FOLDED;
                            let Ok((protected, _)) = encode::protected_node(&node, width) else {
                                continue;
                            };
                            let Ok(parsed) = unmarshal(&protected) else {
                                continue;
                            };
                            if parsed.content[0].value != value {
                                continue;
                            }
                            let mut faithful_node = node.clone();
                            for _pass in 1..=2 {
                                let Ok(faithful) = encode::faithful_node(&faithful_node, width)
                                else {
                                    break;
                                };
                                let Ok(parsed) = unmarshal(&faithful) else {
                                    break;
                                };
                                let Some(parsed) = parsed.content.first() else {
                                    break;
                                };
                                if parsed.value == value {
                                    assert_eq!(faithful, protected, "{value:?}");
                                    checked += 1;
                                }
                                faithful_node.value.clone_from(&parsed.value);
                            }
                        }
                    }
                }
            }
        }
        assert!(checked > 100);
    }

    #[test]
    fn clip_trailing_break_difference_is_not_a_value_preservation_proof() {
        let rust = "v: >\n  text\nnext: ok\n";
        let go = "v: >\n  text\n\nnext: ok\n";
        assert!(!folded_scalar_reencode(&[rust], go, rust, &[], &[], 1));
    }

    #[test]
    fn exact_folded_emitter_replay_requires_source_value_and_every_other_byte() {
        for (rust, go) in [
            (
                "v: >+\n  text\n\nnext: ok\n",
                "v: >+\n  text\n\n\nnext: ok\n",
            ),
            (
                "v: >2-\n   indented\n  ordinary\n\n  next\nnext: ok\n",
                "v: >2-\n   indented\n  ordinary\n  next\nnext: ok\n",
            ),
        ] {
            assert!(
                folded_scalar_reencode(&[rust], go, rust, &[], &[], 1),
                "{go:?} {rust:?}"
            );
            assert!(!folded_scalar_reencode(
                &[rust
                    .replace("text", "other")
                    .replace("ordinary", "other")
                    .as_str()],
                go,
                rust,
                &[],
                &[],
                1
            ));
            assert!(!folded_scalar_reencode(
                &[rust],
                &go.replace("next: ok", "next: bad"),
                rust,
                &[],
                &[],
                1
            ));
            assert!(!folded_scalar_reencode(
                &[rust],
                &go.replace("text", "bad").replace("ordinary", "bad"),
                rust,
                &[],
                &[],
                1
            ));
            assert!(!folded_scalar_reencode(
                &[rust],
                &(go.to_owned() + "# extra\n"),
                rust,
                &[],
                &[],
                1
            ));
            assert!(!folded_scalar_reencode(
                &[rust],
                go,
                rust,
                &["v: requested\n"],
                &[],
                1
            ));
            assert!(!folded_scalar_reencode(
                &[rust],
                go,
                rust,
                &[],
                &[vec!["v".into()]],
                1
            ));
        }
    }

    #[test]
    fn archive_pass_count_source_path_and_header_comments_are_exact() {
        let rust = "v: >+ # own\n  text\n\nnext: ok\n";
        let go = "v: >+ # own\n  text\n\n\n\nnext: ok\n";
        assert!(folded_scalar_reencode(&[rust], go, rust, &[], &[], 2));
        assert!(!folded_scalar_reencode(&[rust], go, rust, &[], &[], 1));
        assert!(!folded_scalar_reencode(
            &[rust],
            &go.replace("# own", "# other"),
            rust,
            &[],
            &[],
            2
        ));
        assert!(!folded_scalar_reencode(
            &[rust],
            &go.replace("\nnext:", "\n\nnext:"),
            rust,
            &[],
            &[],
            2
        ));
        let wrong_path = rust.replace("v:", "other:");
        assert!(!folded_scalar_reencode(
            &[&wrong_path],
            go,
            rust,
            &[],
            &[],
            2
        ));
    }
}
