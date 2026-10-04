use serde_json::Value;

use super::{camel_to_snake, snake_to_camel};

/// Each input with upstream's camelCase and snake_case output.
const CASES: &[(&str, &str, &str)] = &[
    (
        r#"{"max_output_tokens":5,"thinking_config":{"thinking_budget":1.50,"include_thoughts":true},"stop_sequences":["a","b"],"empty":{},"earr":[],"n":null}"#,
        r#"{"maxOutputTokens":5,"thinkingConfig":{"thinkingBudget":1.50,"includeThoughts":true},"stopSequences":["a","b"],"n":null}"#,
        r#"{"max_output_tokens":5,"thinking_config":{"thinking_budget":1.50,"include_thoughts":true},"stop_sequences":["a","b"],"n":null}"#,
    ),
    (r#"[1,2,3]"#, r#"{"":[1,2,3]}"#, r#"{"":[1,2,3]}"#),
    (
        r#"[{"a_b":1},{"c_d":2}]"#,
        r#"{"":[{"aB":1},{"cD":2}]}"#,
        r#"{"":[{"a_b":1},{"c_d":2}]}"#,
    ),
    (r#"5"#, r#"{}"#, r#"{}"#),
    (r#""str""#, r#"{}"#, r#"{}"#),
    (r#"null"#, r#"{}"#, r#"{}"#),
    (r#"{"":{"x_y":1}}"#, r#"{"xY":1}"#, r#"{"x_y":1}"#),
    (r#"{"":1,"z":2}"#, r#"{"z":2}"#, r#"{"z":2}"#),
    (
        r#"{"a.b":1,"a":{"c":2}}"#,
        r#"{"a":{"b":1,"c":2}}"#,
        r#"{"a":{"b":1,"c":2}}"#,
    ),
    (r#"{"a\b":1}"#, r#"{"a\b":1}"#, r#"{"a\b":1}"#),
    (r#"{":0":1,":x":2}"#, r#"{"0":1,"x":2}"#, r#"{"0":1,"x":2}"#),
    (
        r#"{"x":[[1,2],[3]]}"#,
        r#"{"x":[[1],[2],[3]]}"#,
        r#"{"x":[[1],[2],[3]]}"#,
    ),
    (r#"{"m_x":1,"mX":2}"#, r#"{"mX":2}"#, r#"{"m_x":2}"#),
    (
        r#"{"m_x":{"a":1},"mX":{"b":2}}"#,
        r#"{"mX":{"a":1,"b":2}}"#,
        r#"{"m_x":{"a":1,"b":2}}"#,
    ),
    (
        r#"{"m_x":[1],"mX":[2]}"#,
        r#"{"mX":[1,2]}"#,
        r#"{"m_x":[1,2]}"#,
    ),
    (r#"{"m_x":{"a":1},"mX":5}"#, r#"{"mX":5}"#, r#"{"m_x":5}"#),
    (
        r#"{"m_x":5,"mX":{"a":1}}"#,
        r#"{"mX":{"a":1}}"#,
        r#"{"m_x":{"a":1}}"#,
    ),
    (
        r#"{"a_é":1,"b_ébc":2,"É_x":3,"aÉb":4,"ABC":5}"#,
        r#"{"a��":1,"b��bc":2,"ÉX":3,"aÉb":4,"ABC":5}"#,
        r#"{"a_é":1,"b_ébc":2,"é_x":3,"aéb":4,"a_b_c":5}"#,
    ),
    (
        r#"{"a__b_":1,"_c":2,"d_1":3}"#,
        r#"{"aB":1,"C":2,"d1":3}"#,
        r#"{"a__b_":1,"_c":2,"d_1":3}"#,
    ),
    (r#"{"0":1,"1":2}"#, r#"{"0":1,"1":2}"#, r#"{"0":1,"1":2}"#),
    (r#"{"a":{"0":1}}"#, r#"{"a":[1]}"#, r#"{"a":[1]}"#),
    (
        r#"{"a":{"5":1}}"#,
        r#"{"a":[null,null,null,null,null,1]}"#,
        r#"{"a":[null,null,null,null,null,1]}"#,
    ),
    (r#"{"a":{"-1":1,"-1x":2}}"#, r#"{"a":[1]}"#, r#"{"a":[1]}"#),
    (r#"{"a_":{"b":1}}"#, r#"{"a":{"b":1}}"#, r#"{"a_":{"b":1}}"#),
    (
        r#"{"a":{"":{"b":1}}}"#,
        r#"{"a":[{"b":1}]}"#,
        r#"{"a":[{"b":1}]}"#,
    ),
    (r#"{"a":{"":5}}"#, r#"{"a":[5]}"#, r#"{"a":[5]}"#),
    (r#"{"a":[{"":1}]}"#, r#"{"a":[[1]]}"#, r#"{"a":[[1]]}"#),
    (
        r#"{"a":[{}, {"b":1}, [], 5]}"#,
        r#"{"a":[{"b":1},5]}"#,
        r#"{"a":[{"b":1},5]}"#,
    ),
    (r#"{"a\\b":1}"#, r#"{"ab":1}"#, r#"{"ab":1}"#),
    (
        r#"{"a\\\\.b":1}"#,
        r#"{"a\\":{"b":1}}"#,
        r#"{"a\\":{"b":1}}"#,
    ),
    (r#"{"a\\":1}"#, r#"{"a":1}"#, r#"{"a":1}"#),
    (
        r#"{":":1,"x:y":2}"#,
        r#"{"":1,"x:y":2}"#,
        r#"{"":1,"x:y":2}"#,
    ),
    (r#"{"a":{":":1}}"#, r#"{"a":{"":1}}"#, r#"{"a":{"":1}}"#),
    (
        r#"{"a":{":-1":1}}"#,
        r#"{"a":{"-1":1}}"#,
        r#"{"a":{"-1":1}}"#,
    ),
    (
        r#"{"a":[1],"a_":{":-1":2}}"#,
        r#"{"a":[1]}"#,
        r#"{"a":[1],"a_":{"-1":2}}"#,
    ),
    (
        r#"{"a":[1],"a_":{"-1":2}}"#,
        r#"{"a":[1,2]}"#,
        r#"{"a":[1],"a_":[2]}"#,
    ),
    (
        r#"{"a":[1],"a_":{"3":2}}"#,
        r#"{"a":[1,null,null,2]}"#,
        r#"{"a":[1],"a_":[null,null,null,2]}"#,
    ),
    (
        r#"{"a":[1],"a_":{"0":2}}"#,
        r#"{"a":[2]}"#,
        r#"{"a":[1],"a_":[2]}"#,
    ),
    (
        r#"{"a":[1],"a_":{"x":2}}"#,
        r#"{"a":[1]}"#,
        r#"{"a":[1],"a_":{"x":2}}"#,
    ),
    (
        r#"{"a":[1],"a_":{"":2}}"#,
        r#"{"a":[1,2]}"#,
        r#"{"a":[1],"a_":[2]}"#,
    ),
    (
        r#"{"a":{"x":1},"a_":[2]}"#,
        r#"{"a":{"x":1,"-1":2}}"#,
        r#"{"a":{"x":1},"a_":[2]}"#,
    ),
    (
        r#"{"a":{"0":1},"a_":[2]}"#,
        r#"{"a":[1,2]}"#,
        r#"{"a":[1],"a_":[2]}"#,
    ),
    (
        r#"{"":[1],"x":2}"#,
        r#"{"":[1],"x":2}"#,
        r#"{"":[1],"x":2}"#,
    ),
    (
        r#"{"a":"x","a_":[2]}"#,
        r#"{"a":{"-1":2}}"#,
        r#"{"a":"x","a_":[2]}"#,
    ),
    (r#"{"a.":1}"#, r#"{"a":[1]}"#, r#"{"a":[1]}"#),
    (r#"{".a":1}"#, r#"{"":{"a":1}}"#, r#"{"":{"a":1}}"#),
    (r#"{"a..b":1}"#, r#"{"a":[{"b":1}]}"#, r#"{"a":[{"b":1}]}"#),
    (
        r#"{"a":{"b.":{"c":1}}}"#,
        r#"{"a":{"b":[{"c":1}]}}"#,
        r#"{"a":{"b":[{"c":1}]}}"#,
    ),
    (r#"{"a":"é"}"#, r#"{"a":"é"}"#, r#"{"a":"é"}"#),
    (
        r#"{"a":1.0e10,"b":-0,"c":123456789012345678901234567890}"#,
        r#"{"a":1.0e10,"b":-0,"c":123456789012345678901234567890}"#,
        r#"{"a":1.0e10,"b":-0,"c":123456789012345678901234567890}"#,
    ),
    (
        r#"{"a b":1,"a\tb":2}"#,
        r#"{"a b":1,"a\tb":2}"#,
        r#"{"a b":1,"a\tb":2}"#,
    ),
];

fn compact(text: &str) -> String {
    let value: Value = serde_json::from_str(text).unwrap();
    value.to_string()
}

// Not upstream's: checked with Go, running upstream's converters on each input.
#[test]
fn converts_keys_as_upstream_sjson_does() {
    for (input, camel, snake) in CASES {
        let value: Value = serde_json::from_str(input).unwrap();
        assert_eq!(
            snake_to_camel(&value).to_string(),
            compact(camel),
            "camel of {input}"
        );
        assert_eq!(
            camel_to_snake(&value).to_string(),
            compact(snake),
            "snake of {input}"
        );
    }
}

// Not upstream's: sjson treats these keys as gjson queries; we leave them out.
#[test]
fn leaves_out_query_keys() {
    let value: Value =
        serde_json::from_str(r#"{"a*b":1,"q?":2,"h#":3,"p|":4,"at@":5,"ok":6}"#).unwrap();
    assert_eq!(snake_to_camel(&value).to_string(), r#"{"ok":6}"#);
}

// Not upstream's: upstream runs out of memory padding the array.
#[test]
fn leaves_out_huge_indexes() {
    let value: Value =
        serde_json::from_str(r#"{"a":{"99999999999999999999999":1,"70000":2,"2":3}}"#).unwrap();
    assert_eq!(snake_to_camel(&value).to_string(), r#"{"a":[null,null,3]}"#);
}
