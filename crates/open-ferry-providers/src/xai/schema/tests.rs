//! Schema clean-up, ported from upstream's `xai_executor_test.go`
//! (`TestXAIFunctionParametersNeedSimplification`). Upstream tests
//! `InlineLocalRefs` only through the Gemini schema cleaner, so those tests
//! here are new, after `gemini_schema_test.go`.

use serde_json::{Value, json};

use super::*;

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("valid JSON")
}

/// `schema` inlined, or `None` when it comes out unchanged.
fn inlined(schema: &Value) -> Option<Value> {
    match inline_local_refs(schema) {
        Inlined::Schema(inlined) => Some(inlined),
        Inlined::Unchanged => None,
        Inlined::TooLarge => panic!("too large to inline: {schema}"),
    }
}

/// A schema whose property `a` refers to definition `n<levels>`, each
/// definition past `n0` holding two references to the one before: inlined,
/// `n<levels>` holds 2^levels copies of `n0`.
fn doubling(levels: usize) -> Value {
    let mut defs = serde_json::Map::new();
    defs.insert("n0".to_owned(), json!({"type": "string"}));
    for level in 1..=levels {
        let previous = format!("#/$defs/n{}", level - 1);
        defs.insert(
            format!("n{level}"),
            json!({
                "type": "object",
                "properties": {"a": {"$ref": previous}, "b": {"$ref": previous}},
            }),
        );
    }
    json!({
        "type": "object",
        "properties": {"a": {"$ref": format!("#/$defs/n{levels}")}},
        "$defs": defs,
    })
}

/// How many times `n0`'s schema appears in `value`.
fn leaves(value: &Value) -> usize {
    match value {
        Value::Object(fields) if fields.len() == 1 && value["type"] == "string" => 1,
        Value::Object(fields) => fields.values().map(leaves).sum(),
        Value::Array(items) => items.iter().map(leaves).sum(),
        _ => 0,
    }
}

// TestXAIFunctionParametersNeedSimplification.
#[test]
fn function_parameters_need_simplification() {
    let auto =
        parse(r#"{"type":"function","name":"automation_update","parameters":{"type":"object"}}"#);
    for namespace in [
        "codex_app",
        "mcp__codex_app",
        "codex_apps",
        "mcp__codex_apps",
    ] {
        assert!(needs_simplification(&auto, namespace), "{namespace}");
    }
    for namespace in ["calendar", ""] {
        assert!(!needs_simplification(&auto, namespace), "{namespace:?}");
    }
    for name in [
        "codex_app__automation_update",
        "mcp__codex_app__automation_update",
        "codex_apps__automation_update",
        "mcp__codex_apps__automation_update",
    ] {
        let flattened = json!({"type": "function", "name": name, "parameters": {"type": "object"}});
        assert!(needs_simplification(&flattened, ""), "{name}");
    }
    let custom =
        parse(r#"{"type":"custom","name":"automation_update","parameters":{"type":"object"}}"#);
    assert!(
        !needs_simplification(&custom, "codex_app"),
        "a custom automation_update with an object schema is left alone"
    );

    let needs = [
        // A custom tool sent as a function, with an invalid root union.
        r#"{"type":"custom","name":"nullable_lookup","parameters":{"oneOf":[{"type":"object"},{"type":"null"}]}}"#,
        r#"{"type":"function","name":"nullable_lookup","parameters":{"oneOf":[{"type":"object"},{"type":"null"}]}}"#,
        r#"{"type":"function","name":"nullable_lookup","parameters":{"anyOf":[{"type":"object"},{"type":["object","null"]}]}}"#,
        // An untyped branch.
        r#"{"type":"function","name":"nullable_lookup","parameters":{"oneOf":[{"type":"object"},{"const":null}]}}"#,
        // A $ref branch.
        r##"{"type":"function","name":"ref_tool","parameters":{"oneOf":[{"$ref":"#/$defs/schema0"}]}}"##,
    ];
    for tool in needs {
        assert!(needs_simplification(&parse(tool), ""), "{tool}");
    }
    let fine = [
        r#"{"type":"function","name":"lookup","parameters":{"oneOf":[{"type":"object"},{"type":"object"}]}}"#,
        // A nested union isn't the root's.
        r#"{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"value":{"oneOf":[{"type":"string"},{"type":"null"}]}}}}"#,
    ];
    for tool in fine {
        assert!(!needs_simplification(&parse(tool), ""), "{tool}");
    }
    let safe = parse(
        r#"{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"cmd":{"type":"string"}}}}"#,
    );
    assert!(!needs_simplification(&safe, "codex_app"));
}

// Not upstream's: an object-only type is matched in any case, with spaces
// around, and a list of it; nothing else is.
#[test]
fn object_only_types() {
    for schema_type in [
        json!("object"),
        json!(" OBJECT "),
        json!(["object", "Object"]),
    ] {
        assert!(type_is_object_only(Some(&schema_type)), "{schema_type}");
    }
    for schema_type in [json!("null"), json!([]), json!(["object", 1]), json!(1)] {
        assert!(!type_is_object_only(Some(&schema_type)), "{schema_type}");
    }
    assert!(!type_is_object_only(None));
}

// Not upstream's, after TestCleanJSONSchemaForAntigravityResponseInlinesLocalRef
// (gemini_schema_test.go): each use of a definition gets its own copy, with
// the referring schema's keywords on top.
#[test]
fn inline_local_refs_copies_definitions() {
    let schema = parse(
        r##"{"type":"object","properties":{"home":{"$ref":"#/$defs/Address","description":"Home"},"work":{"$ref":"#/$defs/Address"}},"$defs":{"Address":{"type":"object","description":"An address","properties":{"city":{"type":"string"}}}}}"##,
    );
    let inlined = inlined(&schema).expect("references are inlined");
    assert_eq!(inlined["properties"]["home"]["description"], "Home");
    assert_eq!(
        inlined["properties"]["home"]["properties"]["city"]["type"],
        "string"
    );
    assert_eq!(inlined["properties"]["work"]["description"], "An address");
    assert!(inlined["properties"]["work"].get("$ref").is_none());
    // The definitions themselves stay; the caller drops them.
    assert!(inlined.get("$defs").is_some());
    // Keys come out sorted, as Go's encoder writes them.
    let keys: Vec<&String> = inlined.as_object().expect("object").keys().collect();
    assert_eq!(keys, ["$defs", "properties", "type"]);
}

// Not upstream's: a schema without a "$ref" is left alone, and so is one
// whose references point nowhere when it is already in sorted order.
#[test]
fn inline_local_refs_leaves_schemas_without_references() {
    let plain = parse(r#"{"type":"object","properties":{"a":{"type":"string"}}}"#);
    assert_eq!(inline_local_refs(&plain), Inlined::Unchanged);
    let dangling = parse(r##"{"properties":{"a":{"$ref":"#/$defs/Missing"}},"type":"object"}"##);
    assert_eq!(inline_local_refs(&dangling), Inlined::Unchanged);
    // A remote reference isn't followed either.
    let remote =
        parse(r#"{"properties":{"a":{"$ref":"https://example.invalid/a.json"}},"type":"object"}"#);
    assert_eq!(inline_local_refs(&remote), Inlined::Unchanged);
}

// Not upstream's, after TestCleanJSONSchemaForAntigravity_CyclicRefDefaults
// (gemini_schema_test.go): a reference inside its own target stops at a
// typed hint.
#[test]
fn inline_local_refs_ends_cycles_in_a_hint() {
    let schema = parse(
        r##"{"$ref":"#/$defs/Node","$defs":{"Node":{"type":"object","description":"A node","properties":{"next":{"$ref":"#/$defs/Node"}}}}}"##,
    );
    let inlined = inlined(&schema).expect("references are inlined");
    assert_eq!(inlined["type"], "object");
    let next = &inlined["properties"]["next"];
    assert_eq!(next["type"], "object");
    assert_eq!(next["description"], "A node (See: Node)");
    assert!(next.get("properties").is_none());
}

// Not upstream's: JSON Pointer escapes and array indices are followed.
#[test]
fn inline_local_refs_follows_pointer_escapes_and_indices() {
    let schema = parse(
        r##"{"properties":{"a":{"$ref":"#/$defs/a~1b"},"b":{"$ref":"#/list/1"}},"$defs":{"a/b":{"type":"string"}},"list":[{"type":"null"},{"type":"integer"}]}"##,
    );
    let inlined = inlined(&schema).expect("references are inlined");
    assert_eq!(inlined["properties"]["a"], json!({"type": "string"}));
    assert_eq!(inlined["properties"]["b"], json!({"type": "integer"}));
}

// Not upstream's: references shared at each of a few levels are all
// inlined, each use getting its own copy.
#[test]
fn inline_local_refs_copies_shared_references_within_the_allowance() {
    let schema = doubling(6);
    let inlined = inlined(&schema).expect("references are inlined");
    assert!(
        !inlined["properties"].to_string().contains("$ref"),
        "{inlined}"
    );
    assert_eq!(leaves(&inlined["properties"]), 1 << 6);
}

// Not upstream's: references whose copies would outgrow the schema by more
// than the allowance aren't inlined. Upstream inlines them, doubling the
// copies with each level.
#[test]
fn inline_local_refs_stops_past_the_allowance() {
    // Some 4 KB, which upstream would make terabytes.
    let schema = doubling(40);
    assert!(schema.to_string().len() < 8 << 10);
    assert_eq!(inline_local_refs(&schema), Inlined::TooLarge);
    // Each level's copy is some 37 units more than twice the last, and the
    // definitions are inlined as well as the property: nine levels come to
    // some 57,000 units, and fourteen to some 1.8 million.
    assert!(matches!(
        inline_local_refs(&doubling(9)),
        Inlined::Schema(_)
    ));
    assert_eq!(inline_local_refs(&doubling(14)), Inlined::TooLarge);
}

// Not upstream's: a large string is paid for by its bytes, so copies of it
// stop sooner than copies of small values.
#[test]
fn inline_local_refs_counts_string_bytes() {
    // Inlined, `n0` is copied 23 times: 8 times into the property, and 1,
    // 2, 4 and 8 times into the definitions.
    let mut schema = doubling(3);
    schema["$defs"]["n0"]["description"] = Value::String("d".repeat(8 << 10));
    assert!(matches!(inline_local_refs(&schema), Inlined::Schema(_)));
    schema["$defs"]["n0"]["description"] = Value::String("d".repeat(64 << 10));
    assert_eq!(inline_local_refs(&schema), Inlined::TooLarge);
}

// Not upstream's: a value's size counts its values and the bytes of its
// keys and strings, and stops once past the limit.
#[test]
fn size_counts_values_and_bytes() {
    assert_eq!(size(&json!(1), usize::MAX), 1);
    assert_eq!(size(&json!("abc"), usize::MAX), 4);
    assert_eq!(
        size(&json!({"ab": [null, "c"]}), usize::MAX),
        1 + 2 + 1 + 1 + 2
    );
    assert!(size(&json!([[1, 2], [3, 4]]), 2) <= 4);
}

// Not upstream's: the hint is added once.
#[test]
fn merge_hint_adds_once() {
    assert_eq!(merge_hint("", "See: A"), "See: A");
    assert_eq!(merge_hint("See: A", "See: A"), "See: A");
    assert_eq!(merge_hint("See: A (x)", "See: A"), "See: A (x)");
    assert_eq!(merge_hint("Thing (See: A)", "See: A"), "Thing (See: A)");
    assert_eq!(merge_hint("Thing", "See: A"), "Thing (See: A)");
    assert_eq!(ref_name("#/$defs/a~1b~0c"), "a/b~c");
    assert_eq!(ref_name("#/"), "#/");
}

// Not upstream's (`normalizeXAIObjectRootUnionBranchTypes` is tested through
// `normalizeXAITools` there): only untyped object branches of an object
// root get the type.
#[test]
fn types_root_union_branches_of_object_roots_only() {
    let mut tool = json!({"parameters": {"type": "object", "oneOf": [
        {"required": ["a"]}, {"type": "string"}, {"$ref": "#/x"}, "text"
    ]}});
    assert!(type_root_union_branches(&mut tool));
    assert_eq!(tool["parameters"]["oneOf"][0]["type"], "object");
    assert_eq!(tool["parameters"]["oneOf"][1]["type"], "string");
    assert!(tool["parameters"]["oneOf"][2].get("type").is_none());

    let mut untyped_root = json!({"parameters": {"anyOf": [{"required": ["a"]}]}});
    assert!(!type_root_union_branches(&mut untyped_root));
    let mut list_root = json!({"parameters": {"type": ["object"], "anyOf": [{"required": ["a"]}]}});
    assert!(!type_root_union_branches(&mut list_root));
    assert!(list_root["parameters"]["anyOf"][0].get("type").is_none());
}
