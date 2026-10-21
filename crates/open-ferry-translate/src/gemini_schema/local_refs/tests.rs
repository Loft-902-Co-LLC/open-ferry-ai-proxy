// Ported from CLIProxyAPI internal/util/gemini_schema_test.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use crate::gemini_schema::{SchemaCopyBudget, clean_json_schema_for_gemini_json_schema};
use serde_json::json;

// Only the measuring thread counts allocations, so parallel tests cannot affect
// the root-transfer regression. The unusual size identifies its literal string.
mod sibling_allocations {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    pub(super) const BYTES: usize = 4093;

    thread_local! {
        static MEASUREMENT: Cell<(bool, usize)> = const { Cell::new((false, 0)) };
    }

    struct Allocator;

    fn record(bytes: usize) {
        if bytes == BYTES {
            let _ = MEASUREMENT.try_with(|state| {
                let (enabled, count) = state.get();
                if enabled {
                    state.set((true, count + 1));
                }
            });
        }
    }

    unsafe impl GlobalAlloc for Allocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record(layout.size());
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) }
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            record(size);
            unsafe { System.realloc(pointer, layout, size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Allocator = Allocator;

    pub(super) fn measure<T>(operation: impl FnOnce() -> T) -> (T, usize) {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                MEASUREMENT.with(|state| state.set((false, 0)));
            }
        }

        MEASUREMENT.with(|state| {
            assert!(!state.get().0);
            state.set((true, 0));
        });
        let _reset = Reset;
        let result = operation();
        let count = MEASUREMENT.with(|state| state.get().1);
        (result, count)
    }
}

fn clean(schema: &Value) -> Value {
    clean_json_schema_for_gemini_json_schema(schema, &mut SchemaCopyBudget::default())
}

fn inline_for_clean(schema: &Value) -> Value {
    super::inline_for_clean(schema, &mut SchemaCopyBudget::default())
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

#[test]
fn repeated_references_lookup_and_size_the_target_once_but_each_spends_budget() {
    let properties: Map<_, _> = (0..1000)
        .map(|i| (format!("p{i}"), json!({"$ref":"#/$defs/Large"})))
        .collect();
    let root = json!({"properties":properties,"$defs":{"Large":{"type":"string","description":"x".repeat(32*1024)}}});
    let mut budget = SchemaCopyBudget::default();
    let mut resolver = Resolver {
        root: &root,
        remaining: COPY_BYTES,
        max_depth: MAX_OUTPUT_DEPTH,
        depth_exhausted: false,
        budget: &mut budget,
        budget_exhausted: false,
        active: HashSet::new(),
        targets: HashMap::new(),
        lookups: HashMap::new(),
        target_size_scans: 0,
    };
    let out = resolver.root();
    assert_eq!(resolver.target_size_scans, 1);
    assert!(resolver.remaining < 32 * 1024);
    assert!(out.to_string().len() < 512 * 1024);
    assert_eq!(out["properties"].as_object().unwrap().len(), 1000);
    assert!(
        out["properties"]["p999"]["description"]
            .as_str()
            .unwrap()
            .starts_with("See:")
    );
}

#[test]
fn no_ref_fast_path_retains_definitions_for_the_later_cleaning_passes() {
    let schema = json!({"properties":{"p":{"type":"string"}},"$defs":{"unused":{"type":"string"}}});
    assert_eq!(inline_for_clean(&schema), schema);
    let mut schema = schema;
    schema["example"] = json!("$ref");
    assert!(inline_for_clean(&schema).get("$defs").is_none());
}

#[test]
fn escaped_reference_keys_inline_after_the_request_has_been_parsed() {
    let escaped: Value=serde_json::from_str(r##"{"type":"object","properties":{"p":{"\u0024ref":"#/$defs/S"}},"$defs":{"S":{"type":"string"}}}"##).unwrap();
    let literal = json!({"type":"object","properties":{"p":{"$ref":"#/$defs/S"}},"$defs":{"S":{"type":"string"}}});
    assert_eq!(escaped, literal);
    assert_eq!(clean(&escaped), clean(&literal));
    assert_eq!(clean(&escaped)["properties"]["p"]["type"], "string");
    // Go's raw-token fast path instead yields object + "See: S" for the escape.
}

#[test]
fn aliased_array_pointer_spellings_size_one_target_once() {
    let mut target = json!({"type":"string","description":"x".repeat(32*1024)});
    for _ in 0..4 {
        target = json!([target]);
    }
    let aliases = ["0", "00", "+0", "-0"];
    let properties: Map<_, _> = (0..256)
        .map(|i| {
            let pointer = (0..4)
                .map(|shift| aliases[(i >> (2 * shift)) & 3])
                .collect::<Vec<_>>()
                .join("/");
            (
                format!("p{i}"),
                json!({"$ref":format!("#/$defs/S/{pointer}")}),
            )
        })
        .collect();
    let root = json!({"properties":properties,"$defs":{"S":target}});
    let mut budget = SchemaCopyBudget::default();
    let mut resolver = Resolver {
        root: &root,
        remaining: COPY_BYTES,
        max_depth: MAX_OUTPUT_DEPTH,
        depth_exhausted: false,
        budget: &mut budget,
        budget_exhausted: false,
        active: HashSet::new(),
        targets: HashMap::new(),
        lookups: HashMap::new(),
        target_size_scans: 0,
    };
    let out = resolver.root();
    assert_eq!(resolver.target_size_scans, 1);
    assert_eq!(resolver.targets.len(), 1);
    assert_eq!(resolver.lookups.len(), 256);
    assert_eq!(out["properties"].as_object().unwrap().len(), 256);
}

#[test]
fn size_scans_stop_before_unaffordable_schema_and_literal_tails() {
    let value = json!({"description":"x".repeat(COPY_BYTES), "tail":vec![Value::Null;100_000]});
    SIZE_NODES_VISITED.with(|n| n.set(0));
    assert_eq!(expansion_size(&value, false), COPY_BYTES + 1);
    SIZE_NODES_VISITED.with(|n| assert!(n.get() <= 3));
    SIZE_NODES_VISITED.with(|n| n.set(0));
    assert_eq!(literal_size(&value), COPY_BYTES + 1);
    SIZE_NODES_VISITED.with(|n| assert!(n.get() <= 4));
    let array = json!(["x".repeat(COPY_BYTES), value]);
    SIZE_NODES_VISITED.with(|n| n.set(0));
    assert_eq!(literal_size(&array), COPY_BYTES + 1);
    SIZE_NODES_VISITED.with(|n| assert!(n.get() <= 4));
}

pub(crate) fn nested_reference_chain(links: usize, levels: usize) -> Value {
    let mut defs = Map::new();
    for i in 0..links {
        let mut value = if i + 1 == links {
            json!({"type":"string"})
        } else {
            json!({"$ref":format!("#/$defs/N{}",i+1)})
        };
        for _ in 0..levels {
            value = json!({"items":value});
        }
        defs.insert(format!("N{i}"), value);
    }
    json!({"$ref":"#/$defs/N0","$defs":defs})
}

pub(crate) const NESTED_CHAINS: [(usize, usize); 4] = [(12, 118), (16, 118), (64, 60), (1024, 100)];

#[test]
fn materialized_unions_cannot_deepen_inlined_output_past_the_cap() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            let mut property = json!({"type":"string"});
            for _ in 0..119 {
                property = json!({"items":property});
            }
            let target = json!({"type":"object","properties":{"p":property},
            "anyOf":[{"required":["p"]},{"required":["p"]}]});
            let mut body = json!({"$ref":"#/$defs/S"});
            for _ in 0..5 {
                body = json!({"items":body});
            }
            body["$defs"] = json!({"S":target});
            let out = clean(&body);
            assert!(structural_depth(&out) <= MAX_OUTPUT_DEPTH);
            assert!(!out.to_string().contains("anyOf"));
        })
        .unwrap()
        .join()
        .unwrap();
}

fn translate_diamonds(chat: bool, count: usize, levels: usize) -> Value {
    let schema = diamond(levels);
    let tools: Vec<_> = (0..count)
        .map(|i| {
            if chat {
                json!({"type":"function","function":{"name":format!("T{i}"),"parameters":schema}})
            } else {
                json!({"name":format!("T{i}"),"input_schema":schema})
            }
        })
        .collect();
    let request = json!({"messages":[{"role":"user","content":"hi"}],"tools":tools});
    let (out, error) = if chat {
        crate::gemini::openai::chat_completions::convert_openai_request_to_gemini(
            "gemini-2.5-pro",
            &request,
            false,
        )
    } else {
        crate::gemini::claude::convert_claude_request_to_gemini(
            "gemini-2.5-pro",
            &request,
            false,
            crate::models::ModelCatalog::embedded(),
        )
    };
    assert!(error.is_none());
    out
}

fn check_many_diamonds(chat: bool) {
    let out = translate_diamonds(chat, 100, 14);
    let tools = out["tools"][0]["functionDeclarations"].as_array().unwrap();
    assert_eq!(tools.len(), 100);
    assert!(
        serde_json::to_vec(&out).unwrap().len()
            <= crate::gemini_schema::SCHEMA_COPY_BYTES_PER_REQUEST
    );
    assert!(
        tools[0]["parametersJsonSchema"]
            .to_string()
            .contains("properties")
    );
    assert!(
        tools[99]["parametersJsonSchema"]
            .to_string()
            .contains("See:")
    );
    assert!(tools[99]["parametersJsonSchema"].to_string().len() < 256);
}

#[test]
fn many_diamond_tools_share_claude_request_allowance() {
    check_many_diamonds(false);
}

#[test]
fn many_diamond_tools_share_chat_request_allowance() {
    check_many_diamonds(true);
}

#[test]
fn under_allowance_shared_budget_matches_unlimited_request_budget() {
    let schema = diamond(8);
    let expected =
        clean_json_schema_for_gemini_json_schema(&schema, &mut SchemaCopyBudget::unlimited());
    for chat in [false, true] {
        let out = translate_diamonds(chat, 2, 8);
        for tool in out["tools"][0]["functionDeclarations"].as_array().unwrap() {
            assert_eq!(tool["parametersJsonSchema"], expected);
        }
    }
}

#[test]
fn root_fallback_preserves_resolved_siblings_without_copying_them() {
    let literal = "x".repeat(sibling_allocations::BYTES);
    let root = json!({"$ref":"#/$defs/Large","title":"kept",
        "properties":{"v":{"$ref":"#/$defs/S"}},"default":{"note":literal},
        "$defs":{"S":{"type":"string"},"Large":{"type":"object","nullable":true,
            "description":"target","default":"x".repeat(COPY_BYTES + 1)}}});
    let expected = json!({"type":"object","nullable":true,"description":"target (See: Large)",
        "title":"kept","properties":{"v":{"type":"string"}},"default":{"note":literal}});
    let mut budget = SchemaCopyBudget::default();
    let mut resolver = Resolver {
        root: &root,
        remaining: COPY_BYTES,
        max_depth: MAX_OUTPUT_DEPTH,
        depth_exhausted: false,
        budget: &mut budget,
        budget_exhausted: false,
        active: HashSet::new(),
        targets: HashMap::new(),
        lookups: HashMap::new(),
        target_size_scans: 0,
    };
    let (out, copies) = sibling_allocations::measure(|| resolver.root());
    assert_eq!(out.to_string(), expected.to_string());
    // entries() copies the source literal once. Re-resolving the sibling map or
    // cloning it before fallback would allocate a second copy of this string.
    assert_eq!(
        copies, 1,
        "root fallback must transfer the already-resolved literal allocation"
    );
}

#[test]
fn projection_preflights_source_nodes_and_bytes_before_inlining() {
    assert!(inline_for_projection(&json!({"default":vec![Value::Null;100_001]})).is_none());
    assert!(
        inline_for_projection(
            &json!({"default":"x".repeat(super::super::SCHEMA_COPY_BYTES_PER_REQUEST)})
        )
        .is_none()
    );
    let mut budget = SchemaCopyBudget {
        remaining: 0,
        nodes: 0,
        path_work: 0,
    };
    let schema =
        json!({"$ref":"#/$defs/S","$defs":{"S":{"type":"string","description":"x".repeat(512)}}});
    let out = super::inline_for_clean(&schema, &mut budget);
    assert!(out["description"].as_str().unwrap().starts_with("See:"));
    assert!(!out.to_string().contains(&"x".repeat(512)));
}

#[test]
fn nested_refs_claude_to_gemini_fit_two_mib_stack() {
    std::thread::Builder::new().stack_size(2 << 20).spawn(|| {
        for (links, levels) in NESTED_CHAINS {
            let schema = nested_reference_chain(links, levels);
            let request = json!({"model":"claude-test","messages":[{"role":"user","content":"hi"}],
                "tools":[{"name":"T","input_schema":schema}]});
            let request: Value = serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
            let (out, error) = crate::gemini::claude::convert_claude_request_to_gemini(
                "gemini-2.5-pro", &request, false, crate::models::ModelCatalog::embedded());
            assert!(error.is_none());
            let parameters = &out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
            assert!(parameters.is_object(), "{out}");
            assert!(structural_depth(parameters) <= MAX_OUTPUT_DEPTH);
            assert!(!serde_json::to_vec(&out).unwrap().is_empty());
        }
    }).unwrap().join().unwrap();
}

#[test]
fn nested_refs_chat_to_gemini_fit_two_mib_stack() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            for (links, levels) in NESTED_CHAINS {
                let schema = nested_reference_chain(links, levels);
                let request = json!({"messages":[{"role":"user","content":"hi"}],
                "tools":[{"type":"function","function":{"name":"T","parameters":schema}}]});
                let request: Value =
                    serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
                let (out, error) =
                    crate::gemini::openai::chat_completions::convert_openai_request_to_gemini(
                        "gemini-2.5-pro",
                        &request,
                        false,
                    );
                assert!(error.is_none());
                let parameters =
                    &out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
                assert!(parameters.is_object(), "{out}");
                assert!(structural_depth(parameters) <= MAX_OUTPUT_DEPTH);
                assert!(!serde_json::to_vec(&out).unwrap().is_empty());
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
