// Ported from CLIProxyAPI internal/util/gemini_schema_test.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use crate::gemini_schema::{SchemaCopyBudget, clean_json_schema_for_gemini_json_schema};
use serde_json::json;

fn clean(schema: &Value) -> Value {
    clean_json_schema_for_gemini_json_schema(schema, &mut SchemaCopyBudget::default())
}

#[test]
fn local_definitions_property_reuse_and_sibling_overrides() {
    let schema = json!({"type":"object","properties":{
        "billing":{"$ref":"#/$defs/Address"},
        "shipping":{"$ref":"#/properties/billing","description":"Shipping"},
        "default":{"$ref":"#/$defs/Address"},
        "$ref":{"type":"string"}, "const":{"type":"string"},
        "type":{"type":"string","title":"Kind"}
    },"$defs":{"Address":{"type":"object","description":"Address",
        "properties":{"zip":{"type":"string"}},"required":["zip"]}}});
    let out = clean(&schema);
    for key in ["billing", "shipping", "default"] {
        assert_eq!(
            out["properties"][key]["properties"]["zip"]["type"], "string",
            "{out}"
        );
        assert_eq!(out["properties"][key]["required"], json!(["zip"]));
    }
    assert_eq!(out["properties"]["shipping"]["description"], "Shipping");
    assert_eq!(out["properties"]["$ref"]["type"], "string");
    assert_eq!(out["properties"]["const"]["type"], "string");
    assert!(out["properties"]["type"].get("title").is_none());
    assert!(out.get("enum").is_none());
    assert!(out.get("$defs").is_none());
}

#[test]
fn literal_defaults_examples_const_and_enum_are_not_walked() {
    let literal = json!({"$ref":"#/$defs/S","title":"Report","format":"pdf","id":7,
        "x-owner":"ops","properties":{"readOnly":true},"const":{"type":"array"}});
    let schema = json!({"type":"object","properties":{"a":{"type":"object",
        "default":literal,"example":literal,"examples":[literal],"const":literal}},
        "$defs":{"S":{"type":"string"}}});
    let inlined = inline_for_clean(&schema);
    for key in ["default", "example", "const"] {
        assert_eq!(inlined["properties"]["a"][key], literal);
    }
    let out = clean(&schema);
    for key in ["default", "example"] {
        assert_eq!(out["properties"]["a"][key], literal, "{out}");
    }
    assert_eq!(out["properties"]["a"]["examples"], json!([literal]));
    assert_eq!(
        serde_json::from_str::<Value>(out["properties"]["a"]["enum"][0].as_str().unwrap()).unwrap(),
        literal
    );
}

#[test]
fn name_maps_and_json_pointer_escapes_keep_schema_semantics() {
    let schema = json!({"dependencies":{"default":{"properties":{"x":{"$ref":"#/definitions/S~1~0"}}}},
        "definitions":{"S/~":{"type":"string"}}});
    let out = inline_for_clean(&schema);
    assert_eq!(
        out["dependencies"]["default"]["properties"]["x"]["type"],
        "string"
    );
    let schema = json!({"properties":{"p":{"$ref":"#/$defs/default"}},"$defs":{
        "default":{"type":"object","properties":{"x":{"$ref":"#/$defs/properties"}}},
        "properties":{"type":"object","default":{"$ref":"#/literal"}}}});
    let out = inline_for_clean(&schema);
    assert_eq!(
        out["properties"]["p"]["properties"]["x"]["default"]["$ref"],
        "#/literal"
    );
}

#[test]
fn root_refs_wrappers_cycles_and_array_repairs() {
    let schema = json!({"$ref":"#/$defs/O","items":{"type":"integer"},"$defs":{
        "O":{"type":"object","properties":{"x":{"type":"string"}}}}});
    let out = clean(&schema);
    assert_eq!(out["type"], "object");
    assert_eq!(out["properties"]["x"]["type"], "string");
    assert!(out.get("items").is_none());
    assert_eq!(clean(&out), out);
    let out = inline_for_clean(&json!({"schema":schema}));
    assert_eq!(out["schema"]["properties"]["x"]["type"], "string");
    for target in [
        json!({"description":"Rows"}),
        json!({"type":"array","items":{
        "type":"object","properties":{"name":{"type":"string"}}}}),
    ] {
        let out = clean(
            &json!({"properties":{"rows":{"type":"array","$ref":"#/$defs/A"}},"$defs":{"A":target}}),
        );
        assert!(out["properties"]["rows"].get("items").is_some(), "{out}");
        if target.get("items").is_some() {
            assert_eq!(
                out["properties"]["rows"]["items"]["properties"]["name"]["type"],
                "string"
            );
        }
        assert_eq!(clean(&out), out);
    }
    let out = clean(&json!({"properties":{"node":{"$ref":"#/$defs/N"}},"$defs":{
        "N":{"type":"object","properties":{"next":{"$ref":"#/$defs/N"}}}}}));
    assert_eq!(
        out["properties"]["node"]["properties"]["next"]["description"],
        "See: N"
    );
}

fn diamond(levels: usize) -> Value {
    let mut defs = Map::new();
    defs.insert(
        "leaf".into(),
        json!({"type":"string","description":"leaf ".repeat(10)}),
    );
    for i in 0..levels {
        let target = if i == 0 {
            "leaf".into()
        } else {
            format!("D{}", i - 1)
        };
        let reference = format!("#/$defs/{target}");
        defs.insert(
            format!("D{i}"),
            json!({"type":"object","properties":{
            "a":{"$ref":reference},"b":{"$ref":reference}}}),
        );
    }
    json!({"type":"object","properties":{"leaf":{"$ref":format!("#/$defs/D{}",levels-1)}},"$defs":defs})
}

#[test]
fn expansion_is_bounded_and_unused_definitions_never_spend_budget() {
    let schema = diamond(13);
    let out = clean(&schema);
    assert!(out.to_string().len() < 512 * 1024);
    assert!(
        out["properties"]["leaf"]["properties"]["a"]["properties"]
            .get("a")
            .is_some()
    );
    assert!(out.get("$defs").is_none());
    let mut schema = schema;
    schema["properties"] = json!({"person":{"$ref":"#/$defs/Person"},"a":{
        "type":"object","$defs":{"Unused":{"description":"x".repeat(280*1024)}}}});
    schema["$defs"]["Person"] = json!({"type":"object","properties":{"name":{"type":"string"}},
        "$defs":{"Unused":{"description":"x".repeat(280*1024)}}});
    let first = inline_for_clean(&schema);
    assert!(first.to_string().len() < 64 * 1024);
    assert_eq!(
        first["properties"]["person"]["properties"]["name"]["type"],
        "string"
    );
    assert!(first["properties"]["a"].get("$defs").is_none());
    for _ in 0..3 {
        assert_eq!(inline_for_clean(&schema), first);
    }
}

#[test]
fn fallback_bounds_descriptions_and_resolves_siblings_without_spending_omitted_bytes() {
    for (description, count) in [
        ("x".repeat(32 * 1024), 200),
        ("x".repeat(512), 2000),
        ("<".repeat(32 * 1024), 8),
        ("x".repeat(280 * 1024), 2),
    ] {
        let properties: Map<_, _> = (0..count)
            .map(|i| (format!("p{i}"), json!({"$ref":"#/$defs/Big"})))
            .collect();
        let out = clean(&json!({"type":"object","properties":properties,"$defs":{
            "Big":{"type":"object","description":description,"properties":{"name":{"type":"string"}}}}}));
        assert!(out.to_string().len() < 512 * 1024);
        assert_eq!(out["properties"].as_object().unwrap().len(), count);
    }
    let out = clean(&json!({"properties":{
        "a":{"$ref":"#/$defs/Big","properties":{"x":{"$ref":"#/$defs/S"}}},
        "b":{"$ref":"#/$defs/S"}},"$defs":{
        "Big":{"type":"object","description":"x".repeat(280*1024)},"S":{"type":"string"}}}));
    assert_eq!(out["properties"]["a"]["properties"]["x"]["type"], "string");
    assert_eq!(out["properties"]["b"]["type"], "string");
    assert!(!out.to_string().contains("$ref"));
}

#[test]
fn long_reference_chains_stop_at_the_stack_bound() {
    let mut defs = Map::new();
    for i in 0..200 {
        defs.insert(format!("N{i}"), json!({"$ref":format!("#/$defs/N{}", i+1)}));
    }
    let out = inline_for_clean(&json!({"$ref":"#/$defs/N0","$defs":defs}));
    assert!(out.to_string().contains("See:"));
    assert!(out.to_string().len() < 128);
}
