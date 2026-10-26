use super::*;
use serde_json::Value;

fn rows() -> Vec<Value> {
    serde_json::from_str(include_str!("../testdata/writer/owner_round6.json")).unwrap()
}

fn values_styles_tags(node: &Node) -> Value {
    serde_json::json!([
        format!("{:?}", node.kind),
        format!("{:?}", node.style),
        node.tag,
        node.value,
        node.content
            .iter()
            .map(values_styles_tags)
            .collect::<Vec<_>>()
    ])
}

#[test]
fn owner_round6_unchanged_sequence_matching_skips_consumed_prefixes() {
    fn work(n: usize) -> usize {
        let mut original = Node {
            kind: Kind::Sequence,
            tag: "!!seq".into(),
            ..Node::default()
        };
        for i in 0..n {
            let mut item = Node::scalar(STR_TAG, "same");
            item.line_comment = format!("# original {i}");
            original.content.push(item);
        }
        let generated = Node {
            kind: Kind::Sequence,
            tag: "!!seq".into(),
            content: (0..n).map(|_| Node::scalar(STR_TAG, "same")).collect(),
            ..Node::default()
        };
        let (_, work) = tree::count_comment_work(|| {
            merge::merge_node_preserve(&mut original, &generated, &mut Vec::new());
        });
        for (i, item) in original.content.iter().enumerate() {
            assert!(item.line_comment == format!("# original {i}"));
        }
        work
    }
    let small = work(1_000);
    let large = work(4_000);
    assert!(
        large <= 8_000,
        "sequence matching rescanned consumed items: {large}"
    );
    assert!(large <= small * 5);
}

fn raw_history(name: &str) {
    let row = rows().into_iter().find(|row| row["case"] == name).unwrap();
    let data = row["source"].as_str().unwrap().as_bytes();
    let output = render_write_file(data).unwrap();
    let writer = tree::without_owner_protection(|| render_write_file(data)).unwrap();
    let comment = if name == "fff" { b"# f1" } else { b"# c1" };
    if name == "fff" {
        assert!(output != writer, "flow-first-foot repair was rejected");
    } else {
        assert!(
            output == writer,
            "unsafe raw proposal did not keep writer bytes"
        );
    }
    for migrate in [false, true] {
        let cfg = Config::parse(&output).unwrap();
        let saved = render_preserving_comments(&output, &cfg, migrate).unwrap();
        assert!(
            tree::physical_comment_counts(&saved)
                .unwrap()
                .get(comment.as_slice())
                .copied()
                .unwrap_or(0)
                >= 1,
            "{name}: next save lost text"
        );
    }
    assert!(
        render_write_file(&output).unwrap() == output,
        "{name}: raw repeat changed bytes"
    );
}

#[test]
fn owner_round6_raw_duplicate_credential() {
    raw_history("A-dup-key");
}
#[test]
fn owner_round6_raw_empty_alias() {
    raw_history("A-empty-alias");
}
#[test]
fn owner_round6_raw_rewritten_scalar() {
    raw_history("B-upper-foot");
}
#[test]
fn owner_round6_raw_rewritten_scalar_without_blank() {
    raw_history("B-upper-foot-nonl");
}
#[test]
fn owner_round6_raw_reencoded_subtree() {
    raw_history("C-params-foot");
}
#[test]
fn owner_round6_raw_reencoded_second_subtree() {
    raw_history("C-params-foot2");
}
#[test]
fn owner_round6_raw_flow_first_foot_still_repairs() {
    raw_history("fff");
}

fn source_count_cap(name: &str) {
    for row in rows().into_iter().filter(|row| row["case"] == name) {
        let data = row["source"].as_str().unwrap().as_bytes();
        let writer = tree::without_owner_protection(|| render_write_file(data)).unwrap();
        let output = render_write_file(data).unwrap();
        assert!(output != writer, "source cap rejected a safe raw repair");
        assert!(raw_comments_survive_following_saves(data, &writer, &output));
        assert!(render_write_file(&output).unwrap() == output);
    }
}

#[test]
fn owner_round6_source_count_cap_seed30() {
    source_count_cap("random-30-2159/1");
}

#[test]
fn owner_round6_source_count_cap_seed38() {
    source_count_cap("random-38-4656/1");
}

#[test]
fn owner_round6_repeated_item_and_explicit_item_keys_are_tag_free() {
    for row in rows().into_iter().filter(|row| row["kind"] == "tag-free") {
        let data = row["source"].as_str().unwrap().as_bytes();
        for save in [false, true] {
            let run = |bytes: &[u8]| {
                if save {
                    render_preserving_comments(bytes, &Config::parse(bytes).unwrap(), false)
                        .unwrap()
                } else {
                    render_nested_scalar(bytes, &["auth-dir"], "/x").unwrap()
                }
            };
            let output = run(data);
            let writer = tree::without_owner_protection(|| run(data));
            assert!(
                !output.windows(3).any(|bytes| bytes == b" ! "),
                "avoidable tag remains"
            );
            assert!(
                independent_owners(data) == independent_owners(&output),
                "tag-free owners changed"
            );
            assert!(run(&output) == output, "tag-free repeat changed bytes");
            assert!(Config::load_bytes(&output).is_ok());
            assert!(
                values_styles_tags(&unmarshal(&output).unwrap())
                    == values_styles_tags(&unmarshal(&writer).unwrap())
            );
        }
    }
}

fn removed_comment_line(name: &str) {
    for row in rows().into_iter().filter(|row| row["case"] == name) {
        let data = row["source"].as_str().unwrap().as_bytes();
        let run = |bytes: &[u8]| {
            render_nested_scalar(bytes, &["observability", "pprof", "addr"], "localhost:7070")
                .unwrap()
        };
        let output = run(data);
        assert!(independent_owners(data) == independent_owners(&output));
        let text = std::str::from_utf8(&output).unwrap();
        assert!(
            !text
                .lines()
                .zip(text.lines().skip(1))
                .any(|(a, b)| a.trim().is_empty()
                    && (b.trim_start().starts_with("? ") || b.trim_start().starts_with("- "))),
            "empty comment line remains"
        );
        assert!(run(&output) == output);
    }
}

#[test]
fn owner_round6_removed_key_comment_line_leaves_no_empty_line() {
    removed_comment_line("blank-explicit");
}

#[test]
fn owner_round6_key_feet_need_the_blank_before_a_less_indented_key() {
    for row in rows().into_iter().filter(|row| row["kind"] == "foot-blank") {
        let correct = row["source"].as_str().unwrap().as_bytes();
        let without_blank = row["without_blank"].as_str().unwrap().as_bytes();
        assert!(Config::load_bytes(correct).is_ok());
        assert!(Config::load_bytes(without_blank).is_ok());
        assert!(independent_owners(correct) != independent_owners(without_blank));
        assert!(
            values_styles_tags(&unmarshal(correct).unwrap())
                == values_styles_tags(&unmarshal(without_blank).unwrap())
        );
    }
}
