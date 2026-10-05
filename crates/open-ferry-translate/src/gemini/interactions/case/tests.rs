use serde_json::Value;

use super::{camel_to_snake, snake_to_camel};
use crate::json::exact;

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
    (
        r#"{"x":-0,"y":1E20,"z":1e-7,"w":0.10,"v":123456789012345678901234567890,"u":1E+2,"t":-0.0,"s":1e5,"r":-1.5E-3,"q":[-0,1E2]}"#,
        r#"{"x":-0,"y":1E20,"z":1e-7,"w":0.10,"v":123456789012345678901234567890,"u":1E+2,"t":-0.0,"s":1e5,"r":-1.5E-3,"q":[-0,1E2]}"#,
        r#"{"x":-0,"y":1E20,"z":1e-7,"w":0.10,"v":123456789012345678901234567890,"u":1E+2,"t":-0.0,"s":1e5,"r":-1.5E-3,"q":[-0,1E2]}"#,
    ),
    // gjson finds an array's element at digits after a `:` too; the `:` only
    // makes sjson build an object where nothing is there.
    (r#"{"a":[1],"a.:0":2}"#, r#"{"a":[2]}"#, r#"{"a":[2]}"#),
    (
        r#"{"a":{"x":1},"a.:0":2}"#,
        r#"{"a":{"x":1,"0":2}}"#,
        r#"{"a":{"x":1,"0":2}}"#,
    ),
    (
        r#"{"a":[1,[5]],"a.:1.:0":2}"#,
        r#"{"a":[1,[2]]}"#,
        r#"{"a":[1,[2]]}"#,
    ),
    (
        r#"{"a":[[1]],"a.:0.:0":2}"#,
        r#"{"a":[[2]]}"#,
        r#"{"a":[[2]]}"#,
    ),
    (r#"{"a":[1],"a.:3":2}"#, r#"{"a":[1]}"#, r#"{"a":[1]}"#),
    (r#"{"a":[1],"a.:-1":2}"#, r#"{"a":[1]}"#, r#"{"a":[1]}"#),
    (r#"{"a":[1],"a.:":2}"#, r#"{"a":[1]}"#, r#"{"a":[1]}"#),
    (
        r#"{"a":[1],"a.:0.b":2}"#,
        r#"{"a":[{"b":2}]}"#,
        r#"{"a":[{"b":2}]}"#,
    ),
    (
        r#"{"a":[{"b":1}],"a.:0.b":2}"#,
        r#"{"a":[{"b":2}]}"#,
        r#"{"a":[{"b":2}]}"#,
    ),
    (
        r#"{"a":1,"a.:0":2}"#,
        r#"{"a":{"0":2}}"#,
        r#"{"a":{"0":2}}"#,
    ),
    (r#"{"a":[1],"a.:00":2}"#, r#"{"a":[2]}"#, r#"{"a":[2]}"#),
    (
        r#"{"a":[1,2],"a.:01":3}"#,
        r#"{"a":[1,3]}"#,
        r#"{"a":[1,3]}"#,
    ),
];

// Not upstream's: checked with Go, running upstream's converters on each input.
// Each output is compared as text, so a number keeps the text it was sent with,
// as upstream copies it.
#[test]
fn converts_keys_as_upstream_sjson_does() {
    for (input, camel, snake) in CASES {
        let value = exact::from_str(input).unwrap();
        assert_eq!(
            snake_to_camel(&value).to_string(),
            *camel,
            "camel of {input}"
        );
        assert_eq!(
            camel_to_snake(&value).to_string(),
            *snake,
            "snake of {input}"
        );
    }
}

/// Runs `f` on a thread with a small stack, which the converters' recursion
/// would overflow on a deep path.
fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

/// `{key: 1}` nested under `depth` objects keyed `n`.
fn nested(depth: usize, key: &str) -> Value {
    let mut value = serde_json::json!({ key: 1 });
    for _ in 0..depth {
        value = serde_json::json!({ "n": value });
    }
    value
}

// Not upstream's: upstream nests a value for each key of the path, 5,000
// here (checked with Go); we leave out a path of more than 128 keys, so
// neither converting nor writing out nor dropping the value goes deep.
#[test]
fn leaves_out_paths_of_more_than_128_keys() {
    on_small_stack(|| {
        let deep = vec!["a"; 5_000].join(".");
        let value = nested(0, &deep);
        assert_eq!(snake_to_camel(&value).to_string(), "{}");
        assert_eq!(camel_to_snake(&value).to_string(), "{}");

        let kept = vec!["a"; 128].join(".");
        let want = format!("{}1{}", r#"{"a":"#.repeat(128), "}".repeat(128));
        assert_eq!(snake_to_camel(&nested(0, &kept)).to_string(), want);
        assert_eq!(camel_to_snake(&nested(0, &kept)).to_string(), want);
        let left_out = vec!["a"; 129].join(".");
        assert_eq!(snake_to_camel(&nested(0, &left_out)).to_string(), "{}");
        assert_eq!(camel_to_snake(&nested(0, &left_out)).to_string(), "{}");

        // The keys a leaf is nested under count too, and an escaped dot
        // doesn't part keys.
        let kept = vec!["a"; 28].join(".");
        let converted = snake_to_camel(&nested(100, &kept)).to_string();
        assert_eq!(converted.matches('{').count(), 128);
        let left_out = vec!["a"; 29].join(".");
        assert_eq!(snake_to_camel(&nested(100, &left_out)).to_string(), "{}");
        assert_eq!(camel_to_snake(&nested(100, &left_out)).to_string(), "{}");
        assert_eq!(snake_to_camel(&nested(129, "a")).to_string(), "{}");
        let escaped = vec!["a"; 200].join("\\.");
        let want = format!(r#"{{"{}":1}}"#, vec!["a"; 200].join("."));
        assert_eq!(snake_to_camel(&nested(0, &escaped)).to_string(), want);
    });
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
