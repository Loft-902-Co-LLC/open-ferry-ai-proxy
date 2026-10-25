use super::*;
use std::cell::Cell;

thread_local! {
    static LEGACY: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn legacy_enabled() -> bool {
    LEGACY.get()
}

pub(super) fn with_legacy<T>(operation: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            LEGACY.set(self.0);
        }
    }
    let _reset = Reset(LEGACY.replace(true));
    operation()
}

// The pre-memo analysis is kept unchanged as the differential oracle.
pub(super) fn legacy_analyze<'a>(schema: &'a Value, root: &'a Value) -> Info<'a> {
    legacy_analyze_depth(schema, root, 0, &mut BTreeSet::new())
}
pub(super) fn legacy_analyze_depth<'a>(
    schema: &'a Value,
    root: &'a Value,
    depth: usize,
    visited: &mut BTreeSet<String>,
) -> Info<'a> {
    let mut info = Info::default();
    let Some(map) = schema.as_object().filter(|_| depth <= 16) else {
        info.complex = true;
        return info;
    };
    info.object = schema.get("type") == Some(&json!("object"))
        || array(schema, "type").any(|v| v == "object");
    const KNOWN: &str = "type description title properties required items additionalProperties patternProperties anyOf oneOf allOf $ref $defs definitions nullable minLength maxLength minimum maximum exclusiveMinimum exclusiveMaximum multipleOf pattern format default examples minItems maxItems maxProperties $schema $id id $comment deprecated readOnly writeOnly example contentEncoding contentMediaType";
    if map
        .keys()
        .any(|k| !k.starts_with("x-") && !KNOWN.split_whitespace().any(|known| known == k))
    {
        info.complex = true;
        info.unsafe_complex = true;
        return info;
    }
    let has_union = schema.get("anyOf").is_some() || schema.get("oneOf").is_some();
    let mut multi_branch_union = false;
    if has_union {
        let Some(branches) = branches(schema).filter(|b| !b.is_empty()) else {
            info.complex = true;
            return info;
        };
        let multiple = branches.len() > 1;
        multi_branch_union = multiple;
        for branch in branches {
            let branch_info = legacy_analyze_depth(branch, root, depth + 1, visited);
            if multiple {
                info.branches.push(branch_info.clone());
            }
            info.merge(branch_info);
        }
        info.complex |= multiple;
    }
    if let Some(reference) = schema.get("$ref") {
        let reference = str_of(Some(reference)).into_owned();
        let target = (!visited.contains(&reference))
            .then(|| local_ref(root, &reference))
            .flatten();
        if let Some(target) = target {
            visited.insert(reference.clone());
            info.merge(legacy_analyze_depth(target, root, depth + 1, visited));
            visited.remove(&reference);
        } else if !multi_branch_union {
            info.complex = true;
            return info;
        }
    }
    if let Some(all) = schema.get("allOf") {
        if let Some(all) = all.as_array() {
            for branch in all {
                info.merge(legacy_analyze_depth(branch, root, depth + 1, visited));
            }
        } else {
            info.complex = true;
        }
    }
    info.pattern |= schema
        .get("patternProperties")
        .and_then(Value::as_object)
        .is_some_and(|p| !p.is_empty());
    info.allows |= info.pattern
        || schema
            .get("additionalProperties")
            .is_some_and(|v| v != &Value::Bool(false));
    info.forbids |= schema.get("additionalProperties") == Some(&Value::Bool(false));
    if let Some(properties) = schema.get("properties") {
        if let Some(properties) = properties.as_object() {
            info.object = true;
            for (key, value) in properties {
                unique(info.properties.entry(key.clone()).or_default(), value);
            }
        } else {
            info.complex = true;
        }
    }
    info.required
        .extend(gjson_array(schema.get("required")).map(|v| str_of(Some(v)).into_owned()));
    info
}

pub(super) fn snapshot(info: &Info<'_>) -> Value {
    let properties: Vec<_> = info
        .properties
        .iter()
        .map(|(name, schemas)| {
            let values: Vec<_> = schemas
                .iter()
                .map(|value| {
                    json!({"identity": std::ptr::from_ref(*value) as usize,
                        "text": value.to_string()})
                })
                .collect();
            let serialized: BTreeSet<_> = schemas.serialized.iter().collect();
            json!({"name":name,"values":values,"serialized":serialized})
        })
        .collect();
    let branches: Vec<_> = info.branches.iter().map(snapshot).collect();
    json!({"properties":properties,"required":info.required,"branches":branches,
        "allows":info.allows,"forbids":info.forbids,"pattern":info.pattern,
        "complex":info.complex,"unsafe_complex":info.unsafe_complex,"object":info.object})
}

pub(super) fn reference_graph(levels: usize, width: usize) -> Value {
    let mut definitions = serde_json::Map::new();
    definitions.insert(
        "D0".into(),
        json!({"type":"object","properties":{"p":{"type":"string"}},"additionalProperties":false}),
    );
    for level in 1..=levels {
        let branches: Vec<_> = (0..width)
            .map(|_| json!({"$ref":format!("#/$defs/D{}",level-1)}))
            .collect();
        definitions.insert(format!("D{level}"), json!({"allOf":branches}));
    }
    json!({"$ref":format!("#/$defs/D{levels}"),"$defs":definitions})
}

pub(super) fn cyclic_reference_graph(levels: usize, width: usize, kind: &str) -> Value {
    let mut schema = reference_graph(levels, width);
    match kind {
        "self" => schema["$defs"]["D0"]["anyOf"] = json!([{"$ref":"#/$defs/D0"}]),
        "mutual" => {
            schema["$defs"]["D0"]["anyOf"] = json!([{"$ref":"#/$defs/M"}]);
            schema["$defs"]["M"] = json!({"$ref":"#/$defs/D0"});
        }
        "deep" => schema["$defs"]["D3"]["anyOf"] = json!([{"$ref":"#/$defs/D3"}]),
        _ => panic!("unknown cycle fixture"),
    }
    schema
}

#[test]
fn analysis_memo_cost_is_bounded_by_distinct_targets() {
    for levels in [6, 7] {
        let schema = reference_graph(levels, 7);
        let mut memo = AnalysisMemo::default();
        let info = analyze_depth_memo(
            &schema,
            &schema,
            0,
            &mut BTreeSet::new(),
            &mut memo,
            &mut AnalysisReferences::default(),
        );
        assert_eq!(memo.target_analyses, levels + 1);
        assert_eq!(memo.targets.len(), levels + 1);
        assert_eq!(info.properties.keys().collect::<Vec<_>>(), [&"p"]);
        assert!(info.object && info.forbids && !info.complex);
        eprintln!(
            "levels={levels} width=7 target_analyses={} distinct_targets={}",
            memo.target_analyses,
            memo.targets.len()
        );
    }
}

#[test]
fn analysis_memo_cyclic_cost_is_bounded_by_distinct_targets() {
    for (levels, kind, expected) in [
        (6, "self", 13),
        (7, "self", 14),
        (6, "mutual", 20),
        (6, "deep", 13),
        (7, "deep", 14),
    ] {
        let schema = cyclic_reference_graph(levels, 7, kind);
        let mut memo = AnalysisMemo::default();
        let info = analyze_depth_memo(
            &schema,
            &schema,
            0,
            &mut BTreeSet::new(),
            &mut memo,
            &mut AnalysisReferences::default(),
        );
        assert!(info.complex && info.object && info.forbids);
        assert_eq!(info.properties.keys().collect::<Vec<_>>(), [&"p"]);
        assert_eq!(
            memo.target_analyses, expected,
            "levels={levels} kind={kind}"
        );
        let targets = schema["$defs"].as_object().unwrap().len();
        assert!(memo.target_analyses <= 3 * targets);
        assert!(memo.targets.len() <= targets);
        eprintln!(
            "levels={levels} kind={kind} target_analyses={} distinct_targets={targets}",
            memo.target_analyses
        );
    }
}

#[test]
fn analysis_memo_cache_hits_propagate_followed_references() {
    let schema = json!({"x":{"allOf":[{"allOf":[{"$ref":"#/y"}]},{"$ref":"#/t"}]},
        "t":{"$ref":"#/y"},"y":{"$ref":"#/z"},
        "z":{"type":"object","properties":{"z":{}}}});
    let mut memo = AnalysisMemo::default();
    memo.resolve(
        &schema["x"],
        &schema,
        0,
        &mut BTreeSet::new(),
        &mut AnalysisReferences::default(),
    );
    let mut visited = BTreeSet::from(["#/z".to_owned()]);
    let expected = legacy_analyze_depth(&schema["t"], &schema, 2, &mut visited.clone());
    let actual = memo.resolve(
        &schema["t"],
        &schema,
        2,
        &mut visited,
        &mut AnalysisReferences::default(),
    );
    assert!(expected.complex);
    assert_eq!(
        snapshot(&actual).to_string(),
        snapshot(&expected).to_string()
    );
}

#[test]
fn analysis_memo_keeps_depth_and_cycle_contexts_separate() {
    let schema = json!({"$defs":{
        "A":{"allOf":[{"$ref":"#/$defs/B"}],"properties":{"a":{}}},
        "B":{"$ref":"#/$defs/A","type":"object","additionalProperties":false,
            "properties":{"b":{}}},
        "Leaf":{"type":"object","properties":{"p":{}},"additionalProperties":false},
        "Link":{"$ref":"#/$defs/Leaf"}
    }});
    let mut memo = AnalysisMemo::default();
    for name in ["A", "B", "Leaf", "Link"] {
        let target = &schema["$defs"][name];
        for depth in [0, 1, 8, 15, 16, 17] {
            for active in [
                None,
                Some("#/$defs/A"),
                Some("#/$defs/B"),
                Some("#/$defs/Leaf"),
            ] {
                let visited: BTreeSet<_> = active.map(str::to_owned).into_iter().collect();
                let expected = legacy_analyze_depth(target, &schema, depth, &mut visited.clone());
                let actual = memo.resolve(
                    target,
                    &schema,
                    depth,
                    &mut visited.clone(),
                    &mut AnalysisReferences::default(),
                );
                assert_eq!(
                    snapshot(&actual).to_string(),
                    snapshot(&expected).to_string(),
                    "name={name} depth={depth} active={active:?}"
                );
            }
        }
    }
    assert!(memo.targets.len() <= 4);
    for name in ["A", "B"] {
        let identity = std::ptr::from_ref(&schema["$defs"][name]);
        if let Some(depths) = memo.targets.get(&identity) {
            assert!(
                depths
                    .iter()
                    .flatten()
                    .all(|entry| entry.references.cut.is_subset(&entry.references.followed))
            );
        }
    }
}

pub(super) struct Generator(pub(super) u64);

impl Generator {
    pub(super) fn next(&mut self, bound: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.0 >> 32) as usize % bound
    }

    fn reference(&mut self, count: usize) -> Value {
        match self.next(8) {
            0 => json!("#/$defs/missing"),
            1 => json!("https://example.invalid/schema"),
            2 => Value::Null,
            _ => json!(format!("#/$defs/D{}", self.next(count))),
        }
    }

    fn schema(&mut self, case: usize) -> Value {
        if case.is_multiple_of(4) {
            let levels = 2 + self.next(3);
            let mut schema = reference_graph(levels, 2 + self.next(2));
            let cycle = if case.is_multiple_of(8) { 0 } else { 1 };
            schema["$defs"][format!("D{cycle}")]["anyOf"] =
                json!([{"$ref":format!("#/$defs/D{cycle}")}]);
            return schema;
        }
        let count = 2 + self.next(4);
        let mut defs = serde_json::Map::new();
        for index in 0..count {
            let mut definition = json!({"type":"object","properties":{
                format!("p{}", self.next(4)):{"type":"string"}}});
            if self.next(2) == 0 {
                definition["additionalProperties"] = json!(self.next(2) == 0);
            }
            match self.next(6) {
                0 => definition["$ref"] = self.reference(count),
                1 => {
                    definition["allOf"] = json!([
                        {"$ref":self.reference(count)},
                        {"allOf":[{"$ref":self.reference(count)},
                            {"type":"object","additionalProperties":false}]}
                    ])
                }
                2 => {
                    definition["anyOf"] = json!([
                        {"$ref":self.reference(count)},
                        {"type":"object","properties":{"_":{"type":"boolean"}},
                            "additionalProperties":true}, {"type":"null"}
                    ])
                }
                3 => {
                    definition["properties"]["p"] = json!({"type":"object",
                    "properties":{"kept":{}},"additionalProperties":false})
                }
                4 => {
                    definition["required"] = match self.next(3) {
                        0 => json!(["p", "reason"]),
                        1 => json!("_"),
                        _ => Value::Null,
                    }
                }
                _ => definition["patternProperties"] = json!({"^x":{"type":"string"}}),
            }
            if self.next(16) == 0 {
                definition["enum"] = json!([1, 2]);
            }
            defs.insert(format!("D{index}"), definition);
        }
        let mut schema = json!({"$ref":"#/$defs/D0","$defs":defs});
        match case % 8 {
            0 => schema["$defs"]["D0"]["$ref"] = json!("#/$defs/D0"),
            1 => {
                schema["$defs"]["D0"]["$ref"] = json!("#/$defs/D1");
                schema["$defs"]["D1"]["$ref"] = json!("#/$defs/D0");
                schema["properties"] = json!({"reason":{"type":"string"}});
            }
            2 => {
                schema["allOf"] = json!([
                    {"$ref":"#/$defs/D1"}, {"$ref":"#/$defs/D1"},
                    {"allOf":[{"$ref":"#/$defs/D0"}]}
                ])
            }
            3 => {
                let mut deep = json!({"$ref":"#/$defs/D1"});
                for _ in 0..15 {
                    deep = json!({"allOf":[deep]});
                }
                schema["allOf"] = json!([deep, {"$ref":"#/$defs/D1"}]);
            }
            4 => {
                schema["allOf"] = json!([
                    {"type":"object","additionalProperties":true},
                    {"allOf":[{"type":"object","additionalProperties":false,
                        "properties":{"p":{"type":"string"}}}, {"$ref":"#/$defs/D1"}]}
                ])
            }
            5 => schema["$ref"] = self.reference(count),
            6 => {
                schema["oneOf"] = json!([
                    {"$ref":"#/$defs/D1"}, {"type":"object","required":["_"],
                        "properties":{"_":{"type":"boolean"}}}
                ])
            }
            _ => {
                schema["$defs"]["a/b~c"] = json!({"type":"object","properties":{
                    "p":{"type":"string"}},"additionalProperties":false});
                schema["$ref"] = json!("#/$defs/a~1b~0c");
                schema["allOf"] = json!([{"$ref":"#/$defs/D1"}]);
            }
        }
        schema
    }

    pub(super) fn spelling(&mut self, count: usize) -> String {
        let index = self.next(count);
        match self.next(8) {
            0 => format!("#/x-d/{index}"),
            1 => format!("#/x-d/0{index}"),
            2 => format!("#/x-d/+{index}"),
            3 if index == 0 => "#/x-d/-0".into(),
            3 => format!("#/x-d/00{index}"),
            4 => format!("#/x-k/a~0{index}"),
            5 => format!("#/x-k/a~{index}"),
            6 => "#/$defs/missing".into(),
            _ => "https://example.invalid/schema".into(),
        }
    }

    fn wide_node(&mut self, count: usize, depth: usize) -> Value {
        let mut schema = json!({});
        if self.next(2) == 0 {
            schema["type"] = json!("object");
        }
        if self.next(2) == 0 {
            schema["$ref"] = json!(self.spelling(count));
        }
        if depth > 0 && self.next(3) == 0 {
            let branches: Vec<_> = (0..1 + self.next(2))
                .map(|_| self.wide_node(count, depth - 1))
                .collect();
            schema["allOf"] = json!(branches);
        }
        if depth > 0 && self.next(4) == 0 {
            let mut branches: Vec<_> = (0..1 + self.next(2))
                .map(|_| self.wide_node(count, depth - 1))
                .collect();
            if self.next(3) == 0 {
                branches.push(json!({"type":"null"}));
            }
            let keyword = if self.next(2) == 0 { "anyOf" } else { "oneOf" };
            schema[keyword] = json!(branches);
        }
        let name = ["p", "reason", "_", "a"][self.next(4)];
        if self.next(2) == 0 {
            schema["properties"] = json!({name:{"type":"string"}});
        }
        if self.next(3) == 0 {
            schema["additionalProperties"] = json!(self.next(2) == 0);
        }
        if self.next(3) == 0 {
            schema["required"] = json!([name]);
        }
        if self.next(3) == 0 {
            schema["items"] = json!({"type":"object","properties":{name:{}}});
        }
        if self.next(32) == 0 {
            schema["enum"] = json!([1]);
        }
        schema
    }

    pub(super) fn wide_schema(&mut self) -> Value {
        let count = 2 + self.next(5);
        let definitions: Vec<_> = (0..count).map(|_| self.wide_node(count, 1)).collect();
        let aliases: serde_json::Map<_, _> = definitions
            .iter()
            .enumerate()
            .map(|(index, value)| (format!("a~{index}"), value.clone()))
            .collect();
        json!({"x-d":definitions,"x-k":aliases,"$ref":self.spelling(count)})
    }
}

#[test]
fn analysis_memo_matches_legacy_on_generated_schemas_and_tool_inputs() {
    let mut generator = Generator(0x51c4_3a87_d209_be6f);
    let args = [
        json!({"p":{"_":true,"reason":"why","kept":1},"_":true,"reason":"why","extra":2}),
        json!({"p":[{"_":true},{"reason":"why"}],"_":false}),
        json!({"_":true,"reason":"why"}),
        json!({}),
    ];
    let sent = json!({"tools":[{"functionDeclarations":[{"name":"T",
        "parametersJsonSchema":{"type":"object","properties":{
            "p":{"type":"object","properties":{"_":{"type":"boolean"}},"required":["_"]},
            "_":{"type":"boolean"},"reason":{"type":"string"}},"required":["_"]}}]}]});
    for case in 0..2_048 {
        let schema = generator.schema(case);
        let expected = legacy_analyze(&schema, &schema);
        let actual = analyze(&schema, &schema);
        assert_eq!(
            snapshot(&actual).to_string(),
            snapshot(&expected).to_string(),
            "analysis case={case} schema={schema}"
        );
        let mut memo = AnalysisMemo::default();
        for depth in [0, 15, 16] {
            for active in [None, Some("#/$defs/D0"), Some("#/$defs/D1")] {
                let visited: BTreeSet<_> = active.map(str::to_owned).into_iter().collect();
                let target = &schema["$defs"]["D0"];
                let expected = legacy_analyze_depth(target, &schema, depth, &mut visited.clone());
                let actual = memo.resolve(
                    target,
                    &schema,
                    depth,
                    &mut visited.clone(),
                    &mut AnalysisReferences::default(),
                );
                assert_eq!(
                    snapshot(&actual).to_string(),
                    snapshot(&expected).to_string(),
                    "context case={case} depth={depth} active={active:?} schema={schema}"
                );
            }
        }
        assert!(memo.targets.len() <= schema["$defs"].as_object().unwrap().len());
        let original = json!({"tools":[{"name":"T","input_schema":schema}]});
        for args in &args {
            let actual = clean_arguments(args, "T", &original, &sent);
            let expected = with_legacy(|| clean_arguments(args, "T", &original, &sent));
            assert_eq!(
                actual.to_string(),
                expected.to_string(),
                "tool input case={case}"
            );
        }
    }
}
