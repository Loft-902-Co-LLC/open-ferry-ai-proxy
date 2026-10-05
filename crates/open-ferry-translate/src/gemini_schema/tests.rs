//! Ports internal/util/gemini_schema_test.go (v8.0.15).
//!
//! Upstream runs most of these tests against several of its cleaners. Only
//! `CleanJSONSchemaForGeminiJSONSchema` is ported, so they run against it. A
//! test that ran only the legacy `CleanJSONSchemaForGemini` runs against it
//! too: the two cleaners differ only in `additionalProperties` and the standard
//! constraints, which those inputs don't use or don't assert on.
//! `compareJSON` becomes `Value` equality, which ignores key order as
//! `reflect.DeepEqual` on Go maps does.
//!
//! Dropped:
//! - every `TestCleanJSONSchemaForAntigravity*` test, and the Antigravity
//!   cleaners in the tests that run several: Antigravity is out of scope.
//! - TestCleanJSONSchema_ResponseArrayMissingItemsUnchanged: Antigravity's
//!   response cleaner only.
//!
//! Changed:
//! - TestCleanJSONSchemaStripsPropertyNamesUnderPropertyNamedProperties: the
//!   assertion that `additionalProperties` is removed is dropped, since this
//!   cleaner keeps it (upstream's own Issue5959 test checks it is kept).
//! - TestCleanJSONSchema_RemovesDraft04IdAndSchemaIdentifierKeywords: the `$ref`
//!   half is dropped. It tests Antigravity's local `$ref` inlining.
//! - TestCleanJSONSchema_PreservesLargeNumberPrecision ran Antigravity's
//!   response cleaner, which moves `minimum` into the description. Here
//!   `minimum` stays, and the test checks its digits survive the re-encoding.
//! - TestCleanJSONSchema_SingleKeySchemaWrapper ran Antigravity's tool cleaner,
//!   and TestCleanJSONSchema_PreservesHTMLCharactersWithoutEscaping its response
//!   cleaner. Both assertions hold for this cleaner.
//! - The two `TestCleanJSONSchemaForGeminiJSONSchema_*` tests drop their
//!   comparisons with the legacy cleaner.
//! - TestCleanJSONSchema_ArrayItemsRequireArrayType_Issue6011: case 7 is
//!   dropped (Antigravity's response cleaner). Idempotency compares the
//!   encoded output, as upstream compares strings.
//!
//! The tests after those check exact output against upstream's, for inputs
//! that take the less obvious paths, and the gjson and sjson path emulation.

use serde_json::{Value, json};

use super::*;

fn clean(text: &str) -> Value {
    let schema: Value = serde_json::from_str(text).expect("test schema is JSON");
    clean_json_schema_for_gemini_json_schema(&schema)
}

/// gjson `Get(path).String()`.
fn text(value: &Value, path: &str) -> String {
    str_of(get(value, path)).into_owned()
}

fn exists(value: &Value, path: &str) -> bool {
    get(value, path).is_some()
}

/// The strings of the array at `path`, as upstream collects `.Array()`.
fn list(value: &Value, path: &str) -> Vec<String> {
    get_strings(value, path)
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("test JSON is valid")
}

#[test]
fn removes_gemini_unsupported_metadata_fields() {
    let input = r##"{
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$id": "root-schema",
        "$comment": "root comment should be removed",
        "type": "object",
        "properties": {
            "payload": {
                "type": "object",
                "$comment": "nested comment should be removed",
                "prefill": "hello",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["a", "b"],
                        "enumDescriptions": ["Alpha", "Beta"],
                        "enumTitles": ["A", "B"]
                    }
                },
                "patternProperties": {
                    "^x-": {"type": "string"}
                }
            },
            "$id": {
                "type": "string",
                "description": "property name should not be removed"
            },
            "$comment": {
                "type": "string",
                "description": "property name should not be removed"
            },
            "enumDescriptions": {
                "type": "array",
                "description": "property name should not be removed"
            }
        }
    }"##;
    let expected = r#"{
        "type": "object",
        "properties": {
            "payload": {
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["a", "b"],
                        "description": "Allowed: a, b"
                    }
                }
            },
            "$id": {
                "type": "string",
                "description": "property name should not be removed"
            },
            "$comment": {
                "type": "string",
                "description": "property name should not be removed"
            },
            "enumDescriptions": {
                "type": "array",
                "items": {"type": "string"},
                "description": "property name should not be removed"
            }
        }
    }"#;
    assert_eq!(clean(input), parse(expected));
}

#[test]
fn remove_extension_fields_cases() {
    let cases = [
        (
            "removes x- fields at root",
            r#"{"type": "object", "x-custom-meta": "value", "properties": {"foo": {"type": "string"}}}"#,
            r#"{"type": "object", "properties": {"foo": {"type": "string"}}}"#,
        ),
        (
            "removes x- fields in nested properties",
            r#"{"type": "object", "properties": {"foo": {"type": "string", "x-internal-id": 123}}}"#,
            r#"{"type": "object", "properties": {"foo": {"type": "string"}}}"#,
        ),
        (
            "does NOT remove properties named x-",
            r#"{"type": "object", "properties": {"x-data": {"type": "string"},
                "normal": {"type": "number", "x-meta": "remove"}}, "required": ["x-data"]}"#,
            r#"{"type": "object", "properties": {"x-data": {"type": "string"},
                "normal": {"type": "number"}}, "required": ["x-data"]}"#,
        ),
        (
            "does NOT remove $schema and other meta fields",
            r##"{"$schema": "http://json-schema.org/draft-07/schema#", "$id": "test",
                "type": "object", "properties": {"foo": {"type": "string"}}}"##,
            r##"{"$schema": "http://json-schema.org/draft-07/schema#", "$id": "test",
                "type": "object", "properties": {"foo": {"type": "string"}}}"##,
        ),
        (
            "handles properties named $schema",
            r#"{"type": "object", "properties": {"$schema": {"type": "string"}}}"#,
            r#"{"type": "object", "properties": {"$schema": {"type": "string"}}}"#,
        ),
        (
            "handles escaping in paths",
            r#"{"type": "object", "properties": {"foo.bar": {"type": "string", "x-meta": "remove"}},
                "x-root.meta": "remove"}"#,
            r#"{"type": "object", "properties": {"foo.bar": {"type": "string"}}}"#,
        ),
    ];
    for (name, input, expected) in cases {
        let mut doc = parse(input);
        remove_extension_fields(&mut doc);
        assert_eq!(doc, parse(expected), "{name}");
    }
}

#[test]
fn is_property_definition_distinguishes_property_named_properties() {
    for (path, want) in [
        ("", false),
        ("properties", true),
        ("properties.properties", false),
        ("properties.properties.properties", true),
        ("properties.records.items.properties", true),
        ("properties.records.items", false),
        // Any prefix the caller nests the schema under must not change the answer.
        ("schema.properties", true),
        ("request.tools.0.functionDeclarations.0.parameters", false),
        (
            "request.tools.0.functionDeclarations.0.parameters.properties",
            true,
        ),
        (
            "request.tools.0.functionDeclarations.0.parameters.properties.properties",
            false,
        ),
        // $defs and patternProperties are name maps for the same reason as properties.
        ("$defs", true),
        ("$defs.properties", false),
        ("properties.$defs", false),
        ("properties.a.patternProperties", true),
        ("properties.patternProperties", false),
    ] {
        assert_eq!(is_property_definition(path), want, "{path}");
    }
}

#[test]
fn strips_property_names_under_property_named_properties() {
    let shapes = [
        (
            "arrayItem",
            r#"{"type":"object","properties":{"records":{"type":"array","items":{"type":"object",
            "properties":{"name":{"type":"string"}},"propertyNames":{"type":"string"}}}}}"#,
        ),
        (
            "propertyNamedProperties",
            r#"{"type":"object","properties":{"properties":{"type":"object",
            "propertyNames":{"type":"string"}}}}"#,
        ),
        (
            "combined",
            r#"{"type":"object","properties":{"pages":{"type":"array","items":{"type":"object",
            "properties":{"properties":{"type":"object","propertyNames":{"type":"string"},
            "additionalProperties":true}},"propertyNames":{"type":"string"}}}}}"#,
        ),
    ];
    for (name, schema) in shapes {
        let got = clean(schema).to_string();
        assert!(
            !got.contains(r#""propertyNames""#),
            "{name}: propertyNames survived cleaning: {got}"
        );
    }
}

#[test]
fn keeps_properties_named_like_keywords() {
    let got = clean(
        r#"{"type":"object","properties":{
            "propertyNames":{"type":"string"},
            "patternProperties":{"type":"string"},
            "properties":{"type":"object","properties":{"propertyNames":{"type":"string"}}}
        }}"#,
    );
    for path in [
        "properties.propertyNames",
        "properties.patternProperties",
        "properties.properties.properties.propertyNames",
    ] {
        assert!(exists(&got, path), "property {path} was removed: {got}");
    }
}

#[test]
fn conditional_keywords() {
    // 1. Root-level if/then/else.
    let res = clean(
        r#"{
        "type": "object",
        "properties": { "kind": { "type": "string", "enum": ["buy", "sell"] } },
        "required": ["kind"],
        "if":   { "properties": { "kind": { "const": "sell" } } },
        "then": { "properties": { "sell_reason": { "type": "string", "description": "why the position is being sold" } }, "required": ["sell_reason"] },
        "else": { "properties": { "buy_reason": { "type": "string" } } }
    }"#,
    );
    assert!(!exists(&res, "if"), "root 'if' was not removed: {res}");
    assert!(!exists(&res, "then"), "root 'then' was not removed: {res}");
    assert!(!exists(&res, "else"), "root 'else' was not removed: {res}");
    assert!(exists(&res, "properties.sell_reason"), "{res}");
    assert!(exists(&res, "properties.buy_reason"), "{res}");
    assert_eq!(
        text(&res, "properties.sell_reason.description"),
        "why the position is being sold"
    );

    // 2. allOf with if/then.
    let res = clean(
        r#"{
        "type": "object",
        "properties": { "kind": { "type": "string", "enum": ["buy", "sell"] } },
        "required": ["kind"],
        "allOf": [
            {
                "if":   { "properties": { "kind": { "const": "sell" } } },
                "then": {
                    "properties": { "sell_reason": { "type": "string", "description": "why the position is being sold" } },
                    "required": ["sell_reason"]
                }
            }
        ]
    }"#,
    );
    assert!(!exists(&res, "allOf"), "'allOf' was not removed: {res}");
    assert!(
        !exists(&res, "if") && !res.to_string().contains(r#""if":"#),
        "'if' keyword present: {res}"
    );
    assert!(exists(&res, "properties.sell_reason"), "{res}");
    assert_eq!(
        text(&res, "properties.sell_reason.description"),
        "why the position is being sold"
    );

    // 3. Nested property with if/then.
    let res = clean(
        r#"{
        "type": "object",
        "properties": {
            "trade": {
                "type": "object",
                "properties": { "kind": { "type": "string" } },
                "if":   { "properties": { "kind": { "const": "sell" } } },
                "then": { "properties": { "sell_reason": { "type": "string" } } }
            }
        }
    }"#,
    );
    assert!(!exists(&res, "properties.trade.if"), "{res}");
    assert!(!exists(&res, "properties.trade.then"), "{res}");
    assert!(
        exists(&res, "properties.trade.properties.sell_reason"),
        "{res}"
    );
}

#[test]
fn sort_by_depth_uses_segments_and_is_stable() {
    let mut paths = ["root.verylong", "root.x.y", "first.same", "later.same"].map(String::from);
    sort_by_depth(&mut paths);
    assert_eq!(
        paths,
        ["root.x.y", "root.verylong", "first.same", "later.same"]
    );
}

#[test]
fn strips_encrypted_metadata() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "api_key": { "type": "string", "description": "API credential", "encrypted": true },
            "timeout": { "type": "integer", "encrypted": false },
            "nested": {
                "type": "object",
                "properties": { "secret": { "type": "string", "encrypted": true } }
            }
        },
        "required": ["api_key"]
    }"#,
    );
    assert!(!got.to_string().contains(r#""encrypted""#), "{got}");
    assert!(exists(&got, "properties.api_key.type"), "{got}");
    assert_eq!(
        text(&got, "properties.api_key.description"),
        "API credential"
    );
    assert!(
        exists(&got, "properties.nested.properties.secret.type"),
        "{got}"
    );
}

#[test]
fn keeps_property_named_encrypted() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "encrypted": { "type": "boolean", "description": "Whether the payload is encrypted", "encrypted": true },
            "data": { "type": "string" }
        },
        "required": ["encrypted"]
    }"#,
    );
    assert!(exists(&got, "properties.encrypted"), "{got}");
    assert_eq!(text(&got, "properties.encrypted.type"), "boolean");
    assert!(!exists(&got, "properties.encrypted.encrypted"), "{got}");
}

#[test]
fn bare_property_map_normalized() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "data": {
                "parent": { "type": "string", "required": true },
                "insert_after": { "type": "string" },
                "insert_before": { "type": "string" }
            },
            "opts": {
                "opt_fields": { "type": "string" }
            }
        }
    }"#,
    );
    assert_eq!(text(&got, "properties.data.type"), "object", "{got}");
    for name in ["parent", "insert_after", "insert_before"] {
        assert_eq!(
            text(&got, &format!("properties.data.properties.{name}.type")),
            "string",
            "{got}"
        );
    }
    assert!(
        list(&got, "properties.data.required").contains(&"parent".to_owned()),
        "{got}"
    );
    assert!(
        !exists(&got, "properties.data.properties.parent.required"),
        "{got}"
    );
    assert_eq!(text(&got, "properties.opts.type"), "object", "{got}");
    assert_eq!(
        text(&got, "properties.opts.properties.opt_fields.type"),
        "string",
        "{got}"
    );
}

#[test]
fn nested_bare_property_map() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "data": {
                "workspace": { "type": "string", "required": true },
                "task": {
                    "name": { "type": "string", "required": true },
                    "notes": { "type": "string" }
                }
            }
        }
    }"#,
    );
    assert_eq!(text(&got, "properties.data.type"), "object", "{got}");
    assert_eq!(
        text(&got, "properties.data.properties.workspace.type"),
        "string"
    );
    assert_eq!(text(&got, "properties.data.properties.task.type"), "object");
    assert_eq!(
        text(&got, "properties.data.properties.task.properties.name.type"),
        "string"
    );
    assert!(list(&got, "properties.data.required").contains(&"workspace".to_owned()));
    assert!(
        list(&got, "properties.data.properties.task.required").contains(&"name".to_owned()),
        "{got}"
    );
}

#[test]
fn bare_property_map_with_keyword_names() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "data": {
                "title": { "type": "string", "required": true },
                "description": { "type": "string" },
                "format": { "type": "string" },
                "type": { "type": "string" }
            }
        }
    }"#,
    );
    assert_eq!(text(&got, "properties.data.type"), "object", "{got}");
    for name in ["title", "description", "type"] {
        assert_eq!(
            text(&got, &format!("properties.data.properties.{name}.type")),
            "string",
            "{got}"
        );
    }
    assert!(list(&got, "properties.data.required").contains(&"title".to_owned()));
}

#[test]
fn array_items_bare_property_map() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "tasks": {
                "type": "array",
                "items": {
                    "id": { "type": "string", "required": true },
                    "label": { "type": "string" }
                }
            }
        }
    }"#,
    );
    assert_eq!(text(&got, "properties.tasks.items.type"), "object", "{got}");
    assert_eq!(
        text(&got, "properties.tasks.items.properties.id.type"),
        "string"
    );
    assert!(list(&got, "properties.tasks.items.required").contains(&"id".to_owned()));
}

#[test]
fn tool_arrays_missing_items() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "params": { "type": "array" },
            "values": { "type": ["array", "null"], "description": "no items" },
            "existing": { "type": "array", "items": { "type": "number" } }
        }
    }"#,
    );
    for path in [
        "properties.params.items.type",
        "properties.values.items.type",
    ] {
        assert_eq!(text(&got, path), "string", "{path}: {got}");
    }
    assert_eq!(text(&got, "properties.existing.items.type"), "number");
    assert_eq!(text(&clean(r#"{"type":"array"}"#), "items.type"), "string");
}

#[test]
fn boolean_required_promoted() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "existing": { "type": "string" },
            "name": { "type": "string", "required": true },
            "age": { "type": "integer", "required": false },
            "tag": { "type": "string" }
        },
        "required": ["existing"]
    }"#,
    );
    let required = list(&got, "required");
    for name in ["existing", "name"] {
        assert!(required.contains(&name.to_owned()), "{got}");
    }
    for name in ["age", "tag"] {
        assert!(!required.contains(&name.to_owned()), "{got}");
    }
    assert!(!exists(&got, "properties.name.required"), "{got}");
    assert!(!exists(&got, "properties.age.required"), "{got}");
}

#[test]
fn preserves_large_number_precision() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "big_int": { "type": "integer", "minimum": 9007199254740993 },
            "bare_child": { "sub": { "type": "string" } }
        }
    }"#,
    );
    assert!(
        got.to_string().contains("9007199254740993"),
        "large integer precision was lost: {got}"
    );
}

#[test]
fn bare_property_map_with_request_and_tools_names() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "data": {
                "request": {
                    "method": { "type": "string", "required": true },
                    "url": { "type": "string" }
                },
                "headers": { "authorization": { "type": "string" } },
                "tools": { "name": { "type": "string" } }
            }
        }
    }"#,
    );
    for path in [
        "properties.data.type",
        "properties.data.properties.headers.type",
        "properties.data.properties.tools.type",
        "properties.data.properties.request.type",
    ] {
        assert_eq!(text(&got, path), "object", "{path}: {got}");
    }
    assert_eq!(
        text(
            &got,
            "properties.data.properties.request.properties.method.type"
        ),
        "string"
    );
    assert!(
        list(&got, "properties.data.properties.request.required").contains(&"method".to_owned())
    );
}

#[test]
fn bare_property_map_with_sibling_description() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "data": {
                "description": "Task payload",
                "parent": { "type": "string", "required": true },
                "insert_after": { "type": "string" }
            }
        }
    }"#,
    );
    assert_eq!(text(&got, "properties.data.type"), "object", "{got}");
    assert_eq!(text(&got, "properties.data.description"), "Task payload");
    assert_eq!(
        text(&got, "properties.data.properties.parent.type"),
        "string"
    );
    assert_eq!(
        text(&got, "properties.data.properties.insert_after.type"),
        "string"
    );
    assert!(list(&got, "properties.data.required").contains(&"parent".to_owned()));
}

#[test]
fn single_key_schema_wrapper() {
    let got = clean(
        r#"{"schema": {
        "type": "object",
        "properties": {
            "data": {
                "parent": { "type": "string", "required": true }
            }
        }
    }}"#,
    );
    assert!(
        exists(&got, "schema"),
        "wrapper key 'schema' was lost: {got}"
    );
    assert_eq!(text(&got, "schema.properties.data.type"), "object", "{got}");
    assert_eq!(
        text(&got, "schema.properties.data.properties.parent.type"),
        "string"
    );
    assert!(list(&got, "schema.properties.data.required").contains(&"parent".to_owned()));
}

#[test]
fn bare_property_map_with_explicit_type_object() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "data": {
                "type": "object",
                "parent": { "type": "string", "required": true },
                "insert_after": { "type": "string" }
            }
        }
    }"#,
    );
    assert_eq!(text(&got, "properties.data.type"), "object", "{got}");
    assert_eq!(
        text(&got, "properties.data.properties.parent.type"),
        "string"
    );
    assert_eq!(
        text(&got, "properties.data.properties.insert_after.type"),
        "string"
    );
    assert!(list(&got, "properties.data.required").contains(&"parent".to_owned()));
}

#[test]
fn bare_property_map_with_nullable() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "data": {
                "nullable": true,
                "description": "Task payload",
                "parent": { "type": "string" }
            }
        }
    }"#,
    );
    assert_eq!(text(&got, "properties.data.type"), "object", "{got}");
    assert_eq!(
        text(&got, "properties.data.properties.parent.type"),
        "string"
    );
}

#[test]
fn preserves_html_characters_without_escaping() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "data": {
                "description": "Uses <tag> & symbols > threshold",
                "parent": { "type": "string" }
            }
        }
    }"#,
    )
    .to_string();
    for code in ["003c", "003e", "0026"] {
        let escaped = format!("{}u{code}", '\\');
        assert!(
            !got.contains(&escaped),
            "HTML characters were escaped: {got}"
        );
    }
    assert!(
        got.contains("<tag>") && got.contains("& symbols >"),
        "{got}"
    );
}

#[test]
fn vendor_extension_on_enum_not_wrapped_into_properties() {
    let got = clean(
        r#"{
        "type": "string",
        "enum": ["FOO", "BAR"],
        "x-google-enum-descriptions": { "FOO": "Foo option", "BAR": "Bar option" }
    }"#,
    );
    assert!(!exists(&got, "properties"), "{got}");
    assert_eq!(text(&got, "type"), "string");
}

#[test]
fn object_default_not_wrapped_into_properties() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "settings": { "type": "object", "default": { "theme": "dark", "lang": "en" } }
        }
    }"#,
    );
    assert!(
        !exists(&got, "properties.settings.properties.default"),
        "{got}"
    );
}

#[test]
fn mixed_properties_and_orphan_bare_property() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": { "foo": { "type": "string" } },
        "bar": { "type": "integer", "required": true }
    }"#,
    );
    assert_eq!(text(&got, "properties.foo.type"), "string", "{got}");
    assert_eq!(text(&got, "properties.bar.type"), "integer", "{got}");
    assert!(list(&got, "required").contains(&"bar".to_owned()), "{got}");
    assert!(!exists(&got, "bar"), "{got}");
}

#[test]
fn preserves_additional_properties_object_schema() {
    let got = clean(r#"{"additionalProperties": {"type": "string"}}"#);
    assert!(!exists(&got, "properties.additionalProperties"), "{got}");
}

#[test]
fn removes_draft04_id_and_schema_identifier_keywords() {
    let got = clean(
        r##"{
        "id": "http://example.com/root.json",
        "$anchor": "rootAnchor",
        "$vocabulary": {"https://json-schema.org/draft/2020-12/vocab/core": true},
        "type": "object",
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["short", "video"],
                "id": "ContentType",
                "$anchor": "contentTypeAnchor",
                "$dynamicAnchor": "dynAnchor",
                "$dynamicRef": "#dynAnchor",
                "description": "Kind"
            },
            "id": {
                "type": "string",
                "description": "Property legitimately named id should survive"
            }
        },
        "required": ["kind"]
    }"##,
    );
    for path in [
        "id",
        "$anchor",
        "$vocabulary",
        "properties.kind.id",
        "properties.kind.$anchor",
        "properties.kind.$dynamicAnchor",
        "properties.kind.$dynamicRef",
    ] {
        assert!(!exists(&got, path), "{path} was not removed: {got}");
    }
    assert!(exists(&got, "properties.id"), "{got}");
    assert_eq!(text(&got, "properties.id.type"), "string");
}

#[test]
fn true_boolean_subschemas() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "screenshot_id": true,
            "filename": true,
            "file_size": true,
            "source_file_checksum": true,
            "disabled_field": false
        },
        "additionalProperties": true
    }"#,
    );
    for name in [
        "screenshot_id",
        "filename",
        "file_size",
        "source_file_checksum",
    ] {
        assert_eq!(
            get(&got, &format!("properties.{name}")),
            Some(&json!({})),
            "{got}"
        );
    }
    assert_eq!(
        get(&got, "properties.disabled_field"),
        Some(&Value::Bool(false))
    );
}

#[test]
fn nested_true_boolean_subschemas() {
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "tags": { "type": "array", "items": true },
            "tuple": { "type": "array", "items": [true, {"type": "string"}], "additionalItems": true },
            "union": { "anyOf": [true, {"type": "string"}] },
            "combination": { "allOf": [true, {"type": "object", "properties": {"opt": true}}] },
            "metadata": {
                "type": "object",
                "properties": { "nested_true": true, "nested_false": false }
            },
            "large_int": 9007199254740993
        },
        "$defs": { "custom_schema": true }
    }"#,
    );
    assert_ne!(get(&got, "properties.tags.items"), Some(&Value::Bool(true)));
    assert_eq!(
        get(&got, "properties.metadata.properties.nested_true"),
        Some(&json!({}))
    );
    assert_eq!(
        get(&got, "properties.metadata.properties.nested_false"),
        Some(&Value::Bool(false))
    );
    assert_ne!(
        get(&got, "properties.tuple.items.0"),
        Some(&Value::Bool(true))
    );
    assert_ne!(
        get(&got, "properties.combination.properties.opt"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        get(&got, "properties.large_int").map(Value::to_string),
        Some("9007199254740993".to_owned())
    );
}

#[test]
fn root_and_wrapped_true() {
    assert_eq!(clean("true").to_string(), "{}");
    let wrapped = clean(r#"{"schema": true}"#);
    assert_ne!(
        get(&wrapped, "schema"),
        Some(&Value::Bool(true)),
        "{wrapped}"
    );
}

#[test]
fn preserves_additional_properties_and_pattern_issue_5959() {
    let got = clean(
        r#"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "SubmitTool",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "recipient": { "type": "string", "pattern": "^(alice|bob)$", "minLength": 3, "maxLength": 10 },
            "amount": { "type": "number", "minimum": 1 },
            "nested": {
                "type": "object",
                "additionalProperties": false,
                "properties": { "tag": { "type": "string", "pattern": "^[a-z]+$" } }
            },
            "items_list": {
                "type": "array",
                "items": { "type": "string", "pattern": "^[0-9]+$" }
            }
        },
        "required": ["recipient", "amount", "non_existent"]
    }"#,
    );
    assert_eq!(get(&got, "additionalProperties"), Some(&Value::Bool(false)));
    assert_eq!(text(&got, "properties.recipient.pattern"), "^(alice|bob)$");
    assert_eq!(get(&got, "properties.recipient.minLength"), Some(&json!(3)));
    assert_eq!(
        get(&got, "properties.recipient.maxLength"),
        Some(&json!(10))
    );
    assert_eq!(get(&got, "properties.amount.minimum"), Some(&json!(1)));
    assert_eq!(
        get(&got, "properties.nested.additionalProperties"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        text(&got, "properties.nested.properties.tag.pattern"),
        "^[a-z]+$"
    );
    assert_eq!(
        text(&got, "properties.items_list.items.pattern"),
        "^[0-9]+$"
    );
    assert!(!exists(&got, "title"), "{got}");
    assert!(!exists(&got, "$schema"), "{got}");
    assert_ne!(text(&got, "description"), "No extra properties allowed");
    for path in [
        "properties.recipient.description",
        "properties.nested.properties.tag.description",
        "properties.items_list.items.description",
    ] {
        assert!(!text(&got, path).contains("pattern:"), "{path}: {got}");
    }
    assert_eq!(list(&got, "required"), ["recipient", "amount"]);
}

#[test]
fn preserves_schema_valued_additional_properties() {
    let got = clean(
        r#"{
        "type": "object",
        "additionalProperties": { "type": "string", "pattern": "^[a-z]+$", "minLength": 2 }
    }"#,
    );
    let additional = get(&got, "additionalProperties").expect("additionalProperties kept");
    assert!(additional.is_object(), "{got}");
    assert_eq!(text(additional, "type"), "string");
    assert_eq!(text(additional, "pattern"), "^[a-z]+$");
    assert_eq!(get(additional, "minLength"), Some(&json!(2)));
    assert!(!text(additional, "description").contains("pattern:"));
}

#[test]
fn array_items_require_array_type_issue_6011() {
    // Case 1: a property declares "items" but omits "type": "array".
    let missing_type = r#"{
        "type": "object",
        "properties": {
            "revision_reasons": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "evidence_reference": {
                            "description": "references to evidence",
                            "items": { "type": "string" }
                        }
                    }
                }
            }
        }
    }"#;
    let got = clean(missing_type);
    let target = get(
        &got,
        "properties.revision_reasons.items.properties.evidence_reference",
    )
    .expect("evidence_reference kept");
    assert_eq!(text(target, "type"), "array", "{got}");
    assert_eq!(text(target, "items.type"), "string", "{got}");

    // Case 2: a union type with items.
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "revision_reasons": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "evidence_reference": { "type": ["string", "array"], "items": { "type": "string" } }
                    }
                }
            }
        }
    }"#,
    );
    assert_eq!(
        text(
            &got,
            "properties.revision_reasons.items.properties.evidence_reference.type"
        ),
        "array",
        "{got}"
    );

    // Case 3: an explicit non-array type with extraneous items.
    let got = clean(
        r#"{
        "type": "object",
        "properties": {
            "label": { "type": "string", "items": { "type": "string" } },
            "config": {
                "type": "object",
                "properties": { "key": { "type": "string" } },
                "items": { "type": "string" }
            }
        }
    }"#,
    );
    assert!(!exists(&got, "properties.label.items"), "{got}");
    assert!(!exists(&got, "properties.config.items"), "{got}");
    assert_eq!(text(&got, "properties.label.type"), "string");

    // Case 4: a property named "items" is kept.
    let got = clean(
        r#"{
        "type": "object",
        "properties": { "items": { "type": "string", "description": "a field named items" } }
    }"#,
    );
    assert_eq!(text(&got, "properties.items.type"), "string", "{got}");

    // Case 5: a root array whose type is missing.
    let got = clean(r#"{"items": {"type": "string"}}"#);
    assert_eq!(text(&got, "type"), "array", "{got}");
    assert_eq!(text(&got, "items.type"), "string", "{got}");

    // Case 6: cleaning is idempotent.
    let once = clean(missing_type).to_string();
    assert_eq!(clean(&once).to_string(), once);
}

#[test]
fn sanitize_array_items_preserves_items_for_uppercase_array_type() {
    let got = clean(
        r#"{
        "type": "OBJECT",
        "properties": {
            "summary": {"type": "STRING"},
            "brands": { "type": "ARRAY", "items": {"type": "STRING"} },
            "catalog": {
                "type": "OBJECT",
                "properties": {
                    "items": {
                        "type": "ARRAY",
                        "items": { "type": "OBJECT", "properties": {"id": {"type": "STRING"}} }
                    }
                }
            }
        }
    }"#,
    );
    assert!(exists(&got, "properties.brands.items"), "{got}");
    assert!(
        text(&got, "properties.brands.type").eq_ignore_ascii_case("array"),
        "{got}"
    );
    assert!(
        exists(&got, "properties.catalog.properties.items.items"),
        "{got}"
    );
}

/// Inputs that take the less obvious paths, with the output upstream gives.
#[test]
fn matches_upstream_output() {
    let cases = [
        (
            r#"{"type":"object","properties":{"a":{"type":"object","properties":{"_":{"type":"boolean"},"b":{"type":"string"}},"required":["_","b"]}}}"#,
            r#"{"type":"object","properties":{"a":{"type":"object","properties":{"b":{"type":"string"}},"required":["b"]}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"o":{"type":"object","properties":{"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"}},"required":["reason"]}}}"#,
            r#"{"type":"object","properties":{"o":{"type":"object","properties":{}}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"description":"d","anyOf":[{"type":"string"},{"type":"null"},{"type":"object","properties":{"a":{"type":"string"}}}]}}}"#,
            r#"{"type":"object","properties":{"v":{"type":"object","properties":{"a":{"type":"string"}},"description":"d (Accepts: string | null | object)"}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"type":["string","null","integer"]}},"required":["v","w"]}"#,
            r#"{"type":"object","properties":{"v":{"type":"string","description":"Accepts: string | integer ((nullable))"}}}"#,
        ),
        (
            r##"{"type":"object","properties":{"p":{"$ref":"#/$defs/Foo~1Bar","description":"x"}},"$defs":{"Foo/Bar":{"type":"string"}}}"##,
            r#"{"type":"object","properties":{"p":{"type":"object","description":"x (See: Foo/Bar)"}}}"#,
        ),
        (
            r#"{"allOf":[{"properties":{"a.b":{"type":"string"}},"required":["a.b"]},{"required":["c"],"if":{}}],"type":"object","properties":{"c":{"type":"integer"}}}"#,
            r#"{"type":"object","properties":{"c":{"type":"integer"},"a.b":{"type":"string"}},"required":["a.b","c"]}"#,
        ),
        (
            r#"{"type":"object","properties":{"k":{"type":"integer","enum":[1,2,3]},"c":{"const":true}}}"#,
            r#"{"type":"object","properties":{"k":{"type":"string","enum":["1","2","3"],"description":"Allowed: 1, 2, 3"},"c":{"enum":["true"],"type":"string"}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"x":{"type":"object","properties":{"y":{"type":"string"}},"oneOf":[{"properties":{"z":{"type":"number"}}},{"type":"null"}]}}}"#,
            r#"{"type":"object","properties":{"x":{"type":"object","properties":{"y":{"type":"string"},"z":{"type":"number"}}}}}"#,
        ),
        // A key that is an index makes upstream build an array.
        (
            r#"{"type":"object","then":{"properties":{"3":{"type":"string"}}}}"#,
            r#"{"type":"object","properties":[null,null,null,{"type":"string"}]}"#,
        ),
        (
            r##"{"$ref":"#/definitions/Root","description":"top"}"##,
            r#"{"type":"object","description":"top (See: Root)"}"#,
        ),
        (
            r#"{"type":["array","null"],"items":{"type":"string"},"x-a":1,"properties":{"x-b":{"x-c":2}}}"#,
            r#"{"type":"array","items":{"type":"string"},"properties":{"x-b":{}}}"#,
        ),
        // A root const: upstream's path ".enum" names a key under "".
        (
            r#"{"const":5,"type":"integer"}"#,
            r#"{"type":"integer","":{"enum":[5]}}"#,
        ),
        (r#"{"const":1e21}"#, r#"{"":{"enum":[1e+21]}}"#),
        (
            r#"{"properties":{"c":{"const":1.50}},"type":"object"}"#,
            r#"{"properties":{"c":{"enum":["1.5"],"type":"string"}},"type":"object"}"#,
        ),
        // A repaired schema comes out with sorted keys.
        (
            r#"{"type":"object","properties":{"a":{"type":"array"}},"x":{"type":"string","required":true}}"#,
            r#"{"properties":{"a":{"items":{"type":"string"},"type":"array"},"x":{"type":"string"}},"required":["x"],"type":"object"}"#,
        ),
        (
            r#"{"schema":{"type":"object","properties":{"a":{"type":"string","required":true}}}}"#,
            r#"{"schema":{"properties":{"a":{"type":"string"}},"required":["a"],"type":"object"}}"#,
        ),
        (
            r#"{"type":"object","allOf":[{"required":[]}]}"#,
            r#"{"type":"object","required":null}"#,
        ),
        (
            r#"{"type":"object","properties":{"e":{"enum":[]}}}"#,
            r#"{"type":"object","properties":{"e":{"enum":null,"type":"string"}}}"#,
        ),
        ("true", "{}"),
        (
            r#"{"type":"object","properties":{"_":{"type":"string"}}}"#,
            r#"{"type":"object","properties":{"_":{"type":"string"}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"":{"type":["string","null"]}},"required":[""]}"#,
            r#"{"type":"object","properties":{"":{"type":"string","description":"(nullable)"}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"a.b":{"type":["string","null"]},"c":{"type":"string"}},"required":["a.b","c"]}"#,
            r#"{"type":"object","properties":{"a.b":{"type":"string","description":"(nullable)"},"c":{"type":"string"}},"required":["c"]}"#,
        ),
        (
            r#"{"type":"object","properties":{"a*":{"x-foo":1,"type":"string"}},"required":["a*"]}"#,
            r#"{"type":"object","properties":{"a*":{"type":"string"}},"required":["a*"]}"#,
        ),
        // gjson reads the path `a\b` as the key "ab", so upstream drops the
        // name from `required`.
        (
            r#"{"type":"object","properties":{"a\\b":{"type":"string"},"c":{"type":"string"}},"required":["a\\b","c"]}"#,
            r#"{"type":"object","properties":{"a\\b":{"type":"string"},"c":{"type":"string"}},"required":["c"]}"#,
        ),
        // A trailing backslash escapes the dot after it, so upstream never
        // finds the type array to flatten.
        (
            r#"{"type":"object","properties":{"a\\":{"type":["string","null"]}},"required":["a\\"]}"#,
            r#"{"type":"object","properties":{"a\\":{"type":["string","null"]}}}"#,
        ),
        (
            r#"{"anyOf":[{"type":"string"},{"type":"integer"}],"description":"root"}"#,
            r#"{"type":"string","description":"root (Accepts: string | integer)"}"#,
        ),
        (
            r#"{"type":"object","properties":{"v":{"anyOf":["x",{"type":"null"}]}}}"#,
            r#"{"type":"object","properties":{"v":{}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"e":{"enum":[{"b":1,"a":2},[1,2],null,1.50,true]}}}"#,
            r#"{"type":"object","properties":{"e":{"enum":["{\"b\":1,\"a\":2}","[1,2]","","1.5","true"],"type":"string","description":"Allowed: {\"b\":1,\"a\":2}, [1,2], , 1.5, true"}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"p":{"type":["null"]}}}"#,
            r#"{"type":"object","properties":{"p":{"type":"string","description":"(nullable)"}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"a":{"type":"object","properties":{"x":{"type":"string"}}}},"allOf":[{"properties":{"a":{"properties":{"y":{"type":"integer"}}}}}]}"#,
            r#"{"type":"object","properties":{"a":{"type":"object","properties":{"x":{"type":"string"},"y":{"type":"integer"}}}}}"#,
        ),
        (
            r#"{"allOf":["s",{"type":"string"}]}"#,
            r#"{"type":"string"}"#,
        ),
        (
            r#"{"type":"object","properties":{"t":{"type":["integer","array"],"items":{"type":"string"}},"u":{"type":["integer","string"],"items":{"type":"string"}}}}"#,
            r#"{"type":"object","properties":{"t":{"type":"array","items":{"type":"string"},"description":"Accepts: integer | array"},"u":{"type":"integer","description":"Accepts: integer | string"}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"o":{"oneOf":[{"type":"array"},{"items":{"type":"string"}},{"description":"d"}]}}}"#,
            r#"{"properties":{"o":{"items":{"type":"string"},"type":"array","description":"Accepts: array | array"}},"type":"object"}"#,
        ),
        (
            r#"{"type":"object","properties":{"n":{"type":"object","properties":{"a":{"type":"string"}},"anyOf":[{"type":"null"}]}}}"#,
            r#"{"type":"object","properties":{"n":{"type":"object","properties":{"a":{"type":"string"}}}}}"#,
        ),
        (
            r#"{"type":"object","properties":{"properties":{"type":"object","title":"t","properties":{"title":{"type":"string"}}}}}"#,
            r#"{"type":"object","properties":{"properties":{"type":"object","properties":{"title":{"type":"string"}}}}}"#,
        ),
        (
            r#"{"type":"object","description":"x","properties":{"a":{"type":"string"}},"else":{"properties":{"a":{"type":"integer"},"b":{"type":"boolean"}}}}"#,
            r#"{"type":"object","description":"x","properties":{"a":{"type":"string"},"b":{"type":"boolean"}}}"#,
        ),
        (
            r##"{"type":"object","properties":{"r":{"$ref":"#/x/"}}}"##,
            r##"{"type":"object","properties":{"r":{"type":"object","description":"See: #/x/"}}}"##,
        ),
        (
            r#"{"type":"string","enum":["a"],"x-google":{"A":"a"}}"#,
            r#"{"type":"string","enum":["a"]}"#,
        ),
        (
            r#"{"type":"array","items":[true,{"type":"string"}]}"#,
            r#"{"items":[{},{"type":"string"}],"type":"array"}"#,
        ),
        (
            r#"{"type":"object","properties":{"a":{"type":"object","properties":{"_":{"type":"string"}}},"b":{"type":"object","properties":{"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"},"other":{"type":"string"}}}}}"#,
            r#"{"type":"object","properties":{"a":{"type":"object","properties":{}},"b":{"type":"object","properties":{"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"},"other":{"type":"string"}}}}}"#,
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(clean(input).to_string(), expected, "{input}");
    }
}

#[test]
fn paths_split_and_escape_as_gjson_does() {
    assert_eq!(escape_key("plain"), "plain");
    assert_eq!(escape_key("a.b*c?"), r"a\.b\*c\?");
    assert_eq!(keys(r"a\.b.c"), ["a.b", "c"]);
    assert_eq!(keys(""), [""]);
    assert_eq!(keys(r"a\"), ["a"]);
    assert_eq!(keys("a..b"), ["a", "", "b"]);
    assert_eq!(segments(r"a\.b.c"), [r"a\.b", "c"]);
    assert!(segments("").is_empty());
    assert_eq!(unescape_segment(r"a\.b"), "a.b");
    assert_eq!(unescape_segment(r"a\"), r"a\");
    assert_eq!(trim_suffix("enum", ".enum"), "");
    assert_eq!(trim_suffix("properties.a.enum", ".enum"), "properties.a");
    assert_eq!(join_path("", "type"), "type");
    assert_eq!(description_path(""), "description");
    assert_eq!(description_path("properties.a"), "properties.a.description");
}

#[test]
fn set_and_delete_edit_as_sjson_does() {
    // Each expected value was checked against sjson v1.2.5.
    let set_text = |doc: &str, path: &str, value: Value| {
        let mut doc = parse(doc);
        set(&mut doc, path, value);
        doc.to_string()
    };
    for (doc, path, expected) in [
        (r#"{"a":[1,2]}"#, "a.x", r#"{"a":[1,2]}"#),
        (r#"{"a":"s"}"#, "a.0.b", r#"{"a":[{"b":1}]}"#),
        (r#"{"a":[1]}"#, "a.3", r#"{"a":[1,null,null,1]}"#),
        ("{}", "a.2.b", r#"{"a":[null,null,{"b":1}]}"#),
        (r#""str""#, "a", r#"{"a":1}"#),
        (r#"{"a":[]}"#, "a.", r#"{"a":[1]}"#),
        ("{}", "a..b", r#"{"a":[{"b":1}]}"#),
        (r#"{"a":5}"#, "a.-1", r#"{"a":{"-1":1}}"#),
        (r#"{"a":[1]}"#, "a.-1.b", r#"{"a":[1,{"b":1}]}"#),
        (r#"{"a":{}}"#, "a.-1", r#"{"a":{"-1":1}}"#),
        (r#"{"a":5}"#, "a.", r#"{"a":[1]}"#),
        (r#"{"a":1}"#, "", r#"{"a":1}"#),
        // Past the padding limit nothing changes; see the module docs.
        (r#"{"a":[]}"#, "a.2000", r#"{"a":[]}"#),
        (r#"{"a":5}"#, "a.2000.b", r#"{"a":5}"#),
    ] {
        assert_eq!(set_text(doc, path, json!(1)), expected, "set {doc} {path}");
    }

    let delete_text = |doc: &str, path: &str| {
        let mut doc = parse(doc);
        delete(&mut doc, path);
        doc.to_string()
    };
    for (doc, path, expected) in [
        (r#"{"a":[1,2,3]}"#, "a.-1", r#"{"a":[1,2]}"#),
        ("[1,2]", "5", "[1,2]"),
        (r#"{"a":[1,2]}"#, "a.0", r#"{"a":[2]}"#),
        (r#"{"a":{"b":1}}"#, "a.b.c", r#"{"a":{"b":1}}"#),
        (r#"{"x":1,"a":2,"y":3}"#, "a", r#"{"x":1,"y":3}"#),
    ] {
        assert_eq!(delete_text(doc, path), expected, "delete {doc} {path}");
    }
}

/// `depth` levels of `wrap` around a schema with a `$ref`.
fn nested(depth: usize, wrap: fn(Value) -> Value) -> Value {
    (0..depth).fold(json!({"type": "string", "$ref": "x"}), |schema, _| {
        wrap(schema)
    })
}

#[test]
fn json_written_into_strings_is_cut() {
    // Each level escapes the text of the level below once more, so upstream's
    // doubles with each.
    let wraps: [fn(Value) -> Value; 4] = [
        |schema| json!({"type": "string", "$ref": schema}),
        |schema| json!({"type": [schema, "integer"]}),
        |schema| json!({"anyOf": [{"type": schema}, {"type": "string"}]}),
        |schema| json!({"description": schema, "anyOf": [{"type": "string"}, {"type": "integer"}]}),
    ];
    for wrap in wraps {
        let cleaned = clean_json_schema_for_gemini_json_schema(&nested(24, wrap)).to_string();
        assert!(
            cleaned.len() < 4 * MAX_JSON_TEXT,
            "{} bytes: {cleaned}",
            cleaned.len()
        );
    }

    // Short of the limit, all of it is written.
    assert_eq!(
        clean(r##"{"type":"string","$ref":{"type":"string","$ref":"#/x"}}"##),
        json!({"type": "object", "description": r#"See: {"type":"object","description":"See: x"}"#})
    );

    // Past it, the text ends at the last whole character.
    let long = json!({"type": "string", "$ref": {"ab": "é".repeat(1000)}});
    let description = text(
        &clean_json_schema_for_gemini_json_schema(&long),
        "description",
    );
    assert_eq!(description.len(), "See: ".len() + MAX_JSON_TEXT - 1);
    assert!(
        description.starts_with(r#"See: {"ab":"éé"#),
        "{description}"
    );
}

#[test]
fn nested_conditionals_copy_within_a_budget() {
    // Each branch keeps what it gives its parent, so upstream's copies double
    // with each level.
    let schema = (0..40).fold(
        json!({"type": "string"}),
        |schema, _| json!({"if": {}, "then": {"properties": {"a": schema}}}),
    );
    let cleaned = clean_json_schema_for_gemini_json_schema(&schema).to_string();
    assert!(cleaned.len() < 1 << 16, "{} bytes", cleaned.len());

    // A few levels are all copied.
    let schema = (0..3).fold(
        json!({"type": "string"}),
        |schema, _| json!({"if": {}, "then": {"properties": {"a": schema}}}),
    );
    assert_eq!(
        clean_json_schema_for_gemini_json_schema(&schema),
        json!({"properties": {"a": {"properties": {"a": {"properties": {"a": {"type": "string"}}}}}}})
    );
}

#[test]
fn all_of_adds_required_names_across_branches() {
    let cleaned = clean(
        r#"{"type":"object","properties":{"a":{},"b":{},"c":{},"1":{}},"required":["a"],"allOf":[{"required":["b","a"]},{"required":"x"},{"required":["c","b",1]},{"description":"d"},{"required":[]}]}"#,
    );
    assert_eq!(list(&cleaned, "required"), ["a", "b", "c", "1"]);
    assert_eq!(text(&cleaned, "description"), "d");

    // Branches that add nothing to an empty list write upstream's `null`.
    let cleaned = clean(r#"{"allOf":[{"required":[]},{"required":[]}]}"#);
    assert_eq!(get(&cleaned, "required"), Some(&Value::Null), "{cleaned}");
}

#[test]
fn deleting_object_keys_together_matches_one_at_a_time() {
    let doc = parse(
        r#"{"title":1,"a":{"title":2,"x-b":3,"properties":{"title":{"title":4,"x-c":[{"x-d":5,"title":6}]}}},"items":[{"title":7},{"nullable":true,"x-e":{"x-f":1}}],"x-g":{"title":8},"":{"title":9},"a.b":{"title":10,"c":{"nullable":1}},"-1":{"title":11}}"#,
    );
    let one_at_a_time = |paths: &[String]| {
        let mut doc = doc.clone();
        for path in paths {
            delete(&mut doc, path);
        }
        doc
    };
    let together = |paths: &[String]| {
        let mut doc = doc.clone();
        delete_object_keys(&mut doc, paths);
        doc
    };
    for fields in [
        &["title"][..],
        &["title", "nullable"],
        &["nullable", "x-c", "title"],
    ] {
        let mut paths: Vec<String> = fields
            .iter()
            .flat_map(|field| find_paths(&doc, field))
            .collect();
        assert_eq!(together(&paths), one_at_a_time(&paths), "{fields:?}");
        paths.reverse();
        assert_eq!(
            together(&paths),
            one_at_a_time(&paths),
            "{fields:?} reversed"
        );
        sort_by_depth(&mut paths);
        assert_eq!(
            together(&paths),
            one_at_a_time(&paths),
            "{fields:?} by depth"
        );
    }
    let mut paths = Vec::new();
    walk_for_extensions(&doc, "", &mut paths);
    assert_eq!(together(&paths), one_at_a_time(&paths), "extensions");
}
