// Ported from CLIProxyAPI internal/util/gemini_schema_ref_test.go (MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use serde_json::json;

fn clean(schema: &Value) -> Value {
    super::super::clean_json_schema_for_gemini_json_schema(schema, &mut SchemaCopyBudget::default())
}

#[test]
fn ref_sibling_conjunctions_retain_assertion_scopes() {
    let cases = [
        (
            "target_minimum",
            r##"{"type":"string","minLength":5}"##,
            r##"{"minLength":1}"##,
            vec![
                (r##"/allOf/0/minLength"##, r##"5"##),
                (r##"/minLength"##, r##"1"##),
            ],
        ),
        (
            "sibling_minimum",
            r##"{"type":"string","minLength":1}"##,
            r##"{"minLength":5}"##,
            vec![
                (r##"/allOf/0/minLength"##, r##"1"##),
                (r##"/minLength"##, r##"5"##),
            ],
        ),
        (
            "maximum",
            r##"{"type":"string","maxLength":5}"##,
            r##"{"maxLength":10}"##,
            vec![
                (r##"/allOf/0/maxLength"##, r##"5"##),
                (r##"/maxLength"##, r##"10"##),
            ],
        ),
        (
            "patterns",
            r##"{"type":"string","pattern":"^a"}"##,
            r##"{"pattern":"z$"}"##,
            vec![
                (r##"/allOf/0/pattern"##, r##""^a""##),
                (r##"/pattern"##, r##""z$""##),
            ],
        ),
        (
            "disjoint_types",
            r##"{"type":"string"}"##,
            r##"{"type":"number"}"##,
            vec![
                (r##"/allOf/0/type"##, r##""string""##),
                (r##"/type"##, r##""number""##),
            ],
        ),
        (
            "overlapping_types",
            r##"{"type":["string","number"]}"##,
            r##"{"type":"number"}"##,
            vec![
                (r##"/allOf/0/type"##, r##"["string","number"]"##),
                (r##"/type"##, r##""number""##),
            ],
        ),
        (
            "overlapping_nullable_types",
            r##"{"type":["string","null"]}"##,
            r##"{"type":"null"}"##,
            vec![
                (r##"/allOf/0/type"##, r##"["string","null"]"##),
                (r##"/type"##, r##""null""##),
            ],
        ),
        (
            "overlapping_array_types",
            r##"{"type":["array","null"],"items":{"type":"integer"}}"##,
            r##"{"type":"array"}"##,
            vec![
                (r##"/allOf/0/type"##, r##"["array","null"]"##),
                (r##"/allOf/0/items/type"##, r##""integer""##),
                (r##"/type"##, r##""array""##),
            ],
        ),
        (
            "overlapping_object_types",
            r##"{"type":["object","string"],"properties":{"a":{"type":"integer"}}}"##,
            r##"{"type":"string"}"##,
            vec![
                (r##"/allOf/0/type"##, r##"["object","string"]"##),
                (r##"/allOf/0/properties/a/type"##, r##""integer""##),
                (r##"/type"##, r##""string""##),
            ],
        ),
        (
            "enums",
            r##"{"type":"string","enum":["a","b"]}"##,
            r##"{"enum":["b","c"]}"##,
            vec![
                (r##"/allOf/0/enum"##, r##"["a","b"]"##),
                (r##"/enum"##, r##"["b","c"]"##),
            ],
        ),
        (
            "required_without_local_properties",
            r##"{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}},"required":["a"]}"##,
            r##"{"required":["b"]}"##,
            vec![
                (r##"/allOf/0/required"##, r##"["a"]"##),
                (r##"/required"##, r##"["b"]"##),
                (r##"/allOf/0/properties/b/type"##, r##""string""##),
            ],
        ),
        (
            "closed_object_scopes",
            r##"{"type":"object","properties":{"a":{"type":"string"}},"additionalProperties":false}"##,
            r##"{"properties":{"b":{"type":"string"}},"additionalProperties":false}"##,
            vec![],
        ),
        (
            "equal_closed_object_constraints",
            r##"{"type":"object","properties":{"a":{"type":"string"}},"additionalProperties":false}"##,
            r##"{"additionalProperties":false}"##,
            vec![
                (r##"/allOf/0/properties/a/type"##, r##""string""##),
                (r##"/allOf/0/additionalProperties"##, r##"false"##),
                (r##"/additionalProperties"##, r##"false"##),
            ],
        ),
        (
            "sibling_closed_object_scope",
            r##"{"type":"object","properties":{"a":{"type":"string"}}}"##,
            r##"{"additionalProperties":false}"##,
            vec![
                (r##"/allOf/0/properties/a/type"##, r##""string""##),
                (r##"/additionalProperties"##, r##"false"##),
            ],
        ),
        (
            "target_closed_object_scope",
            r##"{"type":"object","additionalProperties":false}"##,
            r##"{"properties":{"a":{"type":"string"}}}"##,
            vec![
                (r##"/allOf/0/additionalProperties"##, r##"false"##),
                (r##"/properties/a/type"##, r##""string""##),
            ],
        ),
        (
            "equal_contains_constraints",
            r##"{"type":"array","items":{},"contains":{"type":"integer"},"minContains":0}"##,
            r##"{"contains":{"type":"integer"}}"##,
            vec![
                (r##"/allOf/0/minContains"##, r##"0"##),
                (r##"/allOf/0/contains/type"##, r##""integer""##),
                (r##"/contains/type"##, r##""integer""##),
            ],
        ),
        (
            "contains_count_scope",
            r##"{"type":"array","items":{},"contains":{"type":"integer"}}"##,
            r##"{"minContains":0}"##,
            vec![
                (r##"/allOf/0/contains/type"##, r##""integer""##),
                (r##"/minContains"##, r##"0"##),
            ],
        ),
        (
            "equal_items_with_different_prefix_scope",
            r##"{"type":"array","prefixItems":[{"type":"string"}],"items":{"type":"integer"}}"##,
            r##"{"items":{"type":"integer"}}"##,
            vec![
                (r##"/allOf/0/prefixItems/0/type"##, r##""string""##),
                (r##"/allOf/0/items/type"##, r##""integer""##),
                (r##"/items/type"##, r##""integer""##),
            ],
        ),
        (
            "existing_allof",
            r##"{"type":"string","allOf":[{"minLength":5}]}"##,
            r##"{"allOf":[{"maxLength":8}]}"##,
            vec![
                (r##"/allOf/0/allOf/0/minLength"##, r##"5"##),
                (r##"/allOf/1/maxLength"##, r##"8"##),
            ],
        ),
    ];
    for (name, target, sibling, checks) in cases {
        for root_ref in [false, true] {
            let mut node: Value = serde_json::from_str(sibling).unwrap();
            node["$ref"] = json!("#/$defs/A");
            let target: Value = serde_json::from_str(target).unwrap();
            let mut schema = if root_ref {
                node
            } else {
                json!({"type":"object","properties":{"x":node}})
            };
            schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
            schema["$defs"] = json!({"A":target});
            for out in [
                inline_for_clean(&schema, &mut SchemaCopyBudget::default()),
                clean(&schema),
            ] {
                let node = if root_ref {
                    &out
                } else {
                    &out["properties"]["x"]
                };
                for (pointer, expected) in &checks {
                    let expected: Value = serde_json::from_str(expected).unwrap();
                    assert_eq!(
                        node.pointer(pointer),
                        Some(&expected),
                        "{name}, root={root_ref}: {out}"
                    );
                }
                assert_eq!(clean(&out), clean(&clean(&out)), "{name}: {out}");
            }
        }
    }
}

#[test]
fn conjunction_cleanup_keeps_cross_branch_constraints() {
    let cases = [
        (
            r##"{"type":"object","allOf":[{"properties":{"x":{"type":"string"}}}],"required":["x"]}"##,
            "/required/0",
            r##""x""##,
        ),
        (
            r##"{"type":"array","allOf":[{"items":{"type":"integer"}}]}"##,
            "/allOf/0/items/type",
            r##""integer""##,
        ),
        (
            r##"{"type":"object","allOf":[{"properties":{"x":{"type":"string"}},"required":["x"]}],"anyOf":[{"type":"object"}]}"##,
            "/allOf/0/required/0",
            r##""x""##,
        ),
        (
            r##"{"type":"object","allOf":[{"properties":{"a":{"type":"string"},"b":{"type":"string"}}},{"anyOf":[{"required":["a"]},{"required":["b"]}]}]}"##,
            "/allOf/1/anyOf/0/required/0",
            r##""a""##,
        ),
        (
            r##"{"$defs":{"A":{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}},"minProperties":1}},"type":"object","properties":{"x":{"$ref":"#/$defs/A","minProperties":0,"anyOf":[{"required":["a"]},{"required":["b"]}]}}}"##,
            "/properties/x/anyOf/1/required/0",
            r##""b""##,
        ),
        (
            r##"{"type":"object","properties":{"x":{"type":"string"}},"anyOf":[{"allOf":[{"properties":{"x":{"minLength":5}}}]}]}"##,
            "/anyOf/0/allOf/0/properties/x/minLength",
            r##"5"##,
        ),
        (
            r##"{"$defs":{"A":{"type":"object","properties":{"x":{"minLength":5}},"minProperties":1}},"type":"object","properties":{"x":{"type":"string"}},"anyOf":[{"$ref":"#/$defs/A","minProperties":0}]}"##,
            "/anyOf/0/allOf/0/properties/x/minLength",
            r##"5"##,
        ),
        (
            r##"{"required":["x"],"anyOf":[{"allOf":[{"properties":{"x":{"type":"string"}}}]}]}"##,
            "/required/0",
            r##""x""##,
        ),
        (
            r##"{"schema":{"$defs":{"A":{"type":"string","minLength":5}},"$ref":"#/$defs/A","minLength":1}}"##,
            "/schema/allOf/0/minLength",
            r##"5"##,
        ),
    ];
    for (input, pointer, want) in cases {
        let schema = serde_json::from_str(input).unwrap();
        let out = clean(&schema);
        assert_eq!(
            out.pointer(pointer),
            Some(&serde_json::from_str::<Value>(want).unwrap()),
            "{out}"
        );
        assert_eq!(out, clean(&out));
    }
}

#[test]
fn unevaluated_constraints_keep_their_reference_scope() {
    let cases = [
        (
            "target_properties_scope",
            r##"{"type":"object","unevaluatedProperties":false}"##,
            r##"{"properties":{"x":{"type":"string"}}}"##,
            r##"{"properties":{"x":{"type":"string"}},"allOf":[{"type":"object","unevaluatedProperties":false}]}"##,
        ),
        (
            "target_items_scope",
            r##"{"type":"array","unevaluatedItems":false}"##,
            r##"{"prefixItems":[{"type":"integer"}]}"##,
            r##"{"prefixItems":[{"type":"integer"}],"allOf":[{"type":"array","unevaluatedItems":false}]}"##,
        ),
        (
            "properties",
            r##"{"type":"object","properties":{"x":{"type":"string"}},"minProperties":1}"##,
            r##"{"minProperties":0,"unevaluatedProperties":false}"##,
            r##"{"minProperties":0,"unevaluatedProperties":false,"allOf":[{"type":"object","properties":{"x":{"type":"string"}},"minProperties":1}]}"##,
        ),
        (
            "items",
            r##"{"type":"array","prefixItems":[{"type":"integer"}],"minItems":1}"##,
            r##"{"minItems":0,"unevaluatedItems":false}"##,
            r##"{"minItems":0,"unevaluatedItems":false,"allOf":[{"type":"array","prefixItems":[{"type":"integer"}],"minItems":1}]}"##,
        ),
    ];
    for (name, target, sibling, want) in cases {
        for root_ref in [false, true] {
            let mut node: Value = serde_json::from_str(sibling).unwrap();
            node["$ref"] = json!("#/$defs/A");
            let mut schema = if root_ref {
                node
            } else {
                json!({"properties":{"value":node}})
            };
            schema["$defs"] = json!({"A":serde_json::from_str::<Value>(target).unwrap()});
            let out = inline_for_clean(&schema, &mut SchemaCopyBudget::default());
            let node = if root_ref {
                &out
            } else {
                &out["properties"]["value"]
            };
            assert_eq!(
                node,
                &serde_json::from_str::<Value>(want).unwrap(),
                "{name}: {out}"
            );
            assert_eq!(clean(&out), clean(&clean(&out)));
        }
    }
}

#[test]
fn annotations_equal_assertions_and_literals_do_not_create_conjunctions() {
    let literal = json!({"$ref":"literal","allOf":[{"type":["object","null"]}]});
    let schema = json!({"$defs":{"A":{"type":"string","minLength":5,"description":"base",
        "default":"base","examples":["base"]}},"properties":{"x":{"$ref":"#/$defs/A",
        "type":"string","description":"local","default":literal,"examples":[literal]}}});
    let out = clean(&schema);
    assert_eq!(
        out["properties"]["x"],
        json!({"type":"string","minLength":5,
        "description":"local","default":literal,"examples":[literal]})
    );
}

#[test]
fn root_dialect_stays_outside_generated_conjunction() {
    let schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema",
        "$defs":{"A":{"type":"string","minLength":5}},"$ref":"#/$defs/A","minLength":1});
    let out = inline_for_clean(&schema, &mut SchemaCopyBudget::default());
    assert_eq!(out["$schema"], schema["$schema"]);
    assert!(out["allOf"][0].get("$schema").is_none());
    assert!(out.get("$defs").is_none()); // This private inliner discards declarations for cleaning.
    assert!(clean(&schema).get("$schema").is_none());
}

#[test]
fn conjunctions_do_not_synthesize_container_types_or_array_items() {
    for root_ref in [false, true] {
        for (target, siblings, expected) in [
            (
                json!({"type":"array","items":{"type":"integer"},"minItems":2}),
                json!({"type":"array","minItems":1}),
                json!({"type":"array","minItems":1,"allOf":[{"type":"array","items":{"type":"integer"},"minItems":2}]}),
            ),
            (
                json!({"type":"array","minItems":2}),
                json!({"minItems":1,"items":{"type":"integer"}}),
                json!({"minItems":1,"items":{"type":"integer"},"allOf":[{"type":"array","minItems":2}]}),
            ),
            (
                json!({"type":"number","minimum":5}),
                json!({"minimum":1,"items":{"type":"integer"}}),
                json!({"minimum":1,"items":{"type":"integer"},"allOf":[{"type":"number","minimum":5}]}),
            ),
            (
                json!({"type":"number","minimum":5}),
                json!({"minimum":1,"properties":{"ignored":{"type":"integer"}}}),
                json!({"minimum":1,"properties":{"ignored":{"type":"integer"}},"allOf":[{"type":"number","minimum":5}]}),
            ),
        ] {
            let mut node = siblings;
            node["$ref"] = json!("#/$defs/A");
            let mut schema = if root_ref {
                node
            } else {
                json!({"properties":{"x":node}})
            };
            schema["$defs"] = json!({"A":target});
            let out = clean(&schema);
            assert_eq!(
                if root_ref {
                    &out
                } else {
                    &out["properties"]["x"]
                },
                &expected
            );
            assert_eq!(out, clean(&out));
        }
    }
}

#[test]
fn conjunction_cycles_and_wrapper_depth_exhaustion_stay_bounded() {
    let schema = json!({"$ref":"#/$defs/A","minProperties":0,"$defs":{"A":{
        "type":"object","minProperties":1,"properties":{"next":{"$ref":"#/$defs/A"}}}}});
    let out = clean(&schema);
    assert_eq!(out["allOf"][0]["minProperties"], 1);
    assert_eq!(
        out["allOf"][0]["properties"]["next"]["description"],
        "See: A"
    );
    assert!(structural_depth(&out) <= MAX_OUTPUT_DEPTH);
    let schema = json!({"$ref":"#/$defs/A","minItems":0,"$defs":{"A":{
        "type":"array","minItems":1,"items":{"type":"integer"}}}});
    let (out, truncated) = inline_with_depth(&schema, 3, &mut SchemaCopyBudget::default());
    assert!(truncated);
    assert_eq!(out["minItems"], 0);
    assert!(out.get("allOf").is_none());
    assert!(structural_depth(&out) <= 3);
}

#[test]
fn conjunction_wrapper_spends_copy_and_path_allowances_before_allocating() {
    let schema = json!({"$ref":"#/$defs/A","minLength":1,"$defs":{"A":{
        "type":"string","minLength":5}}});
    let cost = expansion_size(&schema["$defs"]["A"], false);
    for mut budget in [
        SchemaCopyBudget {
            remaining: cost,
            nodes: 100,
            path_work: 100,
        },
        SchemaCopyBudget {
            remaining: 1000,
            nodes: node_count(&schema["$defs"]["A"], 100, false),
            path_work: 100,
        },
        SchemaCopyBudget {
            remaining: 1000,
            nodes: 100,
            path_work: 0,
        },
    ] {
        let (out, truncated) = inline_with_depth(&schema, MAX_OUTPUT_DEPTH, &mut budget);
        assert!(truncated, "{out}");
        assert!(out.get("allOf").is_none());
        assert_eq!(out["minLength"], 1);
        assert!(out["description"].as_str().unwrap().contains("See: A"));
    }
}
