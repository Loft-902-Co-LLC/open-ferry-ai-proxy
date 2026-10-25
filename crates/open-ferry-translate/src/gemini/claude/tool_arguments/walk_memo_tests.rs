use super::analysis_memo_tests::{
    Generator, cyclic_reference_graph, legacy_analyze_depth, reference_graph, snapshot,
};
use super::*;

// The pre-memo walkers are retained, including items' failure-side visited set.
pub(super) fn legacy_items<'a>(
    schema: &'a Value,
    root: &'a Value,
    depth: usize,
    visited: &mut BTreeSet<String>,
) -> Option<Vec<&'a Value>> {
    if depth > 16 || !schema.is_object() {
        return None;
    }
    let mut result = Schemas::default();
    if let Some(reference) = schema.get("$ref") {
        let reference = str_of(Some(reference)).into_owned();
        if !reference.starts_with("#/") || !visited.insert(reference.clone()) {
            return None;
        }
        for item in legacy_items(local_ref(root, &reference)?, root, depth + 1, visited)? {
            unique(&mut result, item);
        }
        visited.remove(&reference);
    }
    if schema.get("anyOf").is_some() || schema.get("oneOf").is_some() {
        let branches = branches(schema)?;
        if branches.len() != 1 {
            return None;
        }
        for item in legacy_items(branches[0], root, depth + 1, visited)? {
            unique(&mut result, item);
        }
    }
    if let Some(all) = schema.get("allOf") {
        for branch in all.as_array()? {
            for item in legacy_items(branch, root, depth + 1, visited)? {
                unique(&mut result, item);
            }
        }
    }
    if let Some(item) = schema.get("items") {
        unique(&mut result, item);
    }
    Some(result.values)
}

pub(super) fn legacy_declares_deep(
    schema: &Value,
    root: &Value,
    key: &str,
    depth: usize,
    visited: &mut BTreeSet<String>,
) -> bool {
    if depth > 16 || !schema.is_object() {
        return false;
    }
    if let Some(reference) = schema.get("$ref") {
        let reference = str_of(Some(reference)).into_owned();
        if visited.insert(reference.clone()) {
            let declared = local_ref(root, &reference)
                .is_some_and(|target| legacy_declares_deep(target, root, key, depth + 1, visited));
            visited.remove(&reference);
            if declared {
                return true;
            }
        }
    }
    if schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|props| props.contains_key(key))
        || gjson_array(schema.get("required")).any(|required| str_of(Some(required)) == key)
    {
        return true;
    }
    ["allOf", "anyOf", "oneOf"].iter().any(|keyword| {
        gjson_array(schema.get(*keyword))
            .any(|branch| legacy_declares_deep(branch, root, key, depth + 1, visited))
    })
}

fn item_snapshot(items: Option<Vec<&Value>>) -> Option<Vec<(usize, String)>> {
    items.map(|items| {
        items
            .into_iter()
            .map(|value| (std::ptr::from_ref(value) as usize, value.to_string()))
            .collect()
    })
}

#[test]
fn walk_memo_cost_is_bounded_by_distinct_targets() {
    for levels in [6, 7] {
        let mut schema = reference_graph(levels, 7);
        schema["$defs"]["D0"]["items"] = json!({"type":"object","properties":{}});
        let mut item_memo = AnalysisMemo::default();
        let found = items_memo(
            &schema,
            &schema,
            0,
            &mut BTreeSet::new(),
            &mut item_memo,
            &mut AnalysisReferences::default(),
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert!(std::ptr::eq(found[0], &schema["$defs"]["D0"]["items"]));
        assert_eq!(item_memo.target_analyses, levels + 1);
        assert_eq!(item_memo.targets.len(), levels + 1);
        for key in ["reason", "_"] {
            let mut memo = AnalysisMemo::default();
            assert!(!declares_deep_memo(
                &schema,
                &schema,
                key,
                0,
                &mut BTreeSet::new(),
                &mut memo,
                &mut AnalysisReferences::default()
            ));
            assert_eq!(memo.target_analyses, levels + 1);
            assert_eq!(memo.targets.len(), levels + 1);
        }
        eprintln!(
            "levels={levels} item/declaration target_analyses={}",
            levels + 1
        );
    }
    for (levels, kind, expected) in [
        (6, "self", 13),
        (7, "self", 14),
        (6, "mutual", 20),
        (6, "deep", 13),
        (7, "deep", 14),
    ] {
        let schema = cyclic_reference_graph(levels, 7, kind);
        let mut memo = AnalysisMemo::default();
        assert!(!declares_deep_memo(
            &schema,
            &schema,
            "_",
            0,
            &mut BTreeSet::new(),
            &mut memo,
            &mut AnalysisReferences::default()
        ));
        assert_eq!(
            memo.target_analyses, expected,
            "levels={levels} kind={kind}"
        );
        assert!(memo.target_analyses <= 3 * schema["$defs"].as_object().unwrap().len());
    }
}

#[test]
fn item_memo_preserves_failure_visited_sets() {
    for schema in [
        json!({"$ref":"#/missing"}),
        json!({"$ref":"#/$defs/A","$defs":{"A":{"$ref":"#/$defs/A"}}}),
        json!({"$ref":"#/$defs/A","$defs":{"A":{"allOf":[
            {"items":{"type":"string"}}, {"$ref":"#/missing"}]}}}),
        json!({"$ref":"#/$defs/A","$defs":{"A":{"$ref":"#/$defs/B"},"B":false}}),
    ] {
        let mut expected_visited = BTreeSet::new();
        let expected = legacy_items(&schema, &schema, 0, &mut expected_visited);
        let mut visited = BTreeSet::new();
        let actual = items(&schema, &schema, 0, &mut visited);
        assert_eq!(item_snapshot(actual), item_snapshot(expected));
        assert_eq!(visited, expected_visited, "schema={schema}");
        assert!(!visited.is_empty());
    }
}

#[test]
fn reference_memos_match_legacy_in_wide_shared_contexts() {
    let mut generator = Generator(0x3a91_6bf2_74c8_50de);
    for case in 0..2_500 {
        let schema = generator.wide_schema();
        let count = schema["x-d"].as_array().unwrap().len();
        let mut analysis_memo = AnalysisMemo::default();
        let mut item_memo = AnalysisMemo::default();
        let mut declaration_memos: [_; 3] = std::array::from_fn(|_| AnalysisMemo::default());
        for context in 0..12 {
            let index = generator.next(count);
            let target = if generator.next(2) == 0 {
                &schema["x-d"][index]
            } else {
                &schema["x-k"][format!("a~{index}")]
            };
            let depth = if context < 5 {
                [0, 1, 15, 16, 17][context]
            } else {
                generator.next(18)
            };
            let visited: BTreeSet<_> = (0..generator.next(4))
                .map(|_| generator.spelling(count))
                .collect();
            let mut expected_visited = visited.clone();
            let expected = legacy_analyze_depth(target, &schema, depth, &mut expected_visited);
            let mut actual_visited = visited.clone();
            let actual = analysis_memo.resolve(
                target,
                &schema,
                depth,
                &mut actual_visited,
                &mut AnalysisReferences::default(),
            );
            assert_eq!(
                snapshot(&actual).to_string(),
                snapshot(&expected).to_string(),
                "analysis case={case} context={context} depth={depth} visited={visited:?} schema={schema}"
            );
            assert_eq!(actual_visited, expected_visited);

            let mut expected_visited = visited.clone();
            let expected = legacy_items(target, &schema, depth, &mut expected_visited);
            let mut actual_visited = visited.clone();
            let actual = item_memo.resolve_with(
                target,
                depth,
                &mut actual_visited,
                &mut AnalysisReferences::default(),
                |memo, visited, own| items_memo(target, &schema, depth, visited, memo, own),
                Option::is_some,
            );
            assert_eq!(
                item_snapshot(actual),
                item_snapshot(expected),
                "items case={case} context={context} depth={depth} visited={visited:?} schema={schema}"
            );
            assert_eq!(
                actual_visited, expected_visited,
                "items visited case={case}"
            );

            for (key, memo) in ["p", "reason", "_"].into_iter().zip(&mut declaration_memos) {
                let mut expected_visited = visited.clone();
                let expected =
                    legacy_declares_deep(target, &schema, key, depth, &mut expected_visited);
                let mut actual_visited = visited.clone();
                let actual = memo.resolve_with(
                    target,
                    depth,
                    &mut actual_visited,
                    &mut AnalysisReferences::default(),
                    |memo, visited, own| {
                        declares_deep_memo(target, &schema, key, depth, visited, memo, own)
                    },
                    |_| true,
                );
                assert_eq!(
                    actual, expected,
                    "declaration case={case} context={context} key={key} depth={depth} visited={visited:?} schema={schema}"
                );
                assert_eq!(actual_visited, expected_visited);
            }
        }
        assert!(analysis_memo.targets.len() <= 2 * count);
        assert!(item_memo.targets.len() <= 2 * count);
        assert!(
            declaration_memos
                .iter()
                .all(|memo| memo.targets.len() <= 2 * count)
        );
    }
}
