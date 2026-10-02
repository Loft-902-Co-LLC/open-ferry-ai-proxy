use super::*;

fn string(text: &str) -> Option<Found<'_>> {
    Some(Found::String(text.to_owned()))
}

#[test]
fn strings_are_decoded() {
    for (text, want) in [
        (r#"{"input":"a \n b"}"#, "a \n b"),
        (r#"{"input":"\u00e9 \/ \" \\"}"#, "\u{e9} / \" \\"),
        (r#"{"input":"\ud83d\ude00"}"#, "\u{1F600}"),
        // gjson's own reading of escapes JSON doesn't allow.
        (r#"{"input":"\ud800"}"#, "\u{FFFD}"),
        (r#"{"input":"\ud800\u0041"}"#, "\u{FFFD}"),
        (r#"{"input":"\uZZZZ!"}"#, "\u{0}!"),
        (r#"{"input":"\u+12a"}"#, "\u{0}"),
    ] {
        assert_eq!(get(text, "input"), string(want), "{text}");
    }
}

#[test]
fn a_string_stops_at_what_gjson_cannot_decode() {
    for (text, want) in [
        (r#"{"input":"cut.txt\+no end"}"#, "cut.txt"),
        (r#"{"input":"a\u12"}"#, "a"),
        // Only a string with an escape is decoded.
        ("{\"input\":\"a\u{1}b\\n\"}", "a"),
        ("{\"input\":\"a\u{1}b\"}", "a\u{1}b"),
    ] {
        assert_eq!(get(text, "input"), string(want), "{text}");
    }
}

#[test]
fn other_values_are_kept_as_written() {
    for (text, want) in [
        (
            r#"{"input": {"a" : [1, "]"]} }"#,
            Found::Json(r#"{"a" : [1, "]"]}"#),
        ),
        (r#"{"input":1.50}"#, Found::Number("1.50")),
        (r#"{"input":null,"x":1}"#, Found::Literal("null")),
        (r#"{"input":nan}"#, Found::Number("nan")),
        (r#"{"input":n}"#, Found::Number("n")),
        (r#"{"input":n"#, Found::Literal("n")),
        (r#"{"input":Infinity}"#, Found::Number("Infinity")),
        (r#"{"input":tru}"#, Found::Literal("tru")),
        // An object that doesn't close runs to the end.
        (r#"{"input":{"a":[1"#, Found::Json(r#"{"a":[1"#)),
    ] {
        assert_eq!(get(text, "input"), Some(want), "{text}");
    }
}

#[test]
fn into_string_matches_gjson() {
    for (value, want) in [
        ("12", "12"),
        ("-0", "-0"),
        ("-", "-"),
        ("1.50", "1.5"),
        ("+5", "5"),
        ("-0.0", "-0"),
        ("1e21", "1000000000000000000000"),
        ("1e-7", "0.0000001"),
        ("1e400", "+Inf"),
        ("-1e400", "-Inf"),
        ("Infinity", "+Inf"),
        ("nan", "NaN"),
        ("0x1p-2", "0.25"),
        ("1_0", "10"),
        ("1x", "0"),
        ("n", "0"),
        ("N", "0"),
        ("tru", "true"),
        ("fals", "false"),
        ("nul", ""),
        (r#"{"a":1}"#, r#"{"a":1}"#),
        (r#""a""#, "a"),
    ] {
        let text = format!(r#"{{"query":{value}}}"#);
        let found = get(&text, "query").map(Found::into_string);
        assert_eq!(found.as_deref(), Some(want), "{value}");
    }
    assert_eq!(get(r#"{"query":n"#, "query").unwrap().into_string(), "");
}

#[test]
fn only_the_first_top_level_key_counts() {
    for (text, want) in [
        (r#"{"input":"a","input":"b"}"#, Some("a")),
        (r#"{"a":{"input":"x"},"input":"y"}"#, Some("y")),
        (r#"{"a":"input","input":"y"}"#, Some("y")),
        (r#"{"in\u0070ut":"x"}"#, Some("x")),
        (r#"junk {"input":"x"}"#, Some("x")),
        // Whatever comes before a value is skipped.
        (r#"{"input" x "v"}"#, Some("v")),
        (r#"{"a":{"input":"x"}}"#, None),
        (r#"[{"input":"x"}]"#, None),
        (r#"{"input":"abc"#, None),
        (r#"{"input"}"#, None),
        (r#"{} {"input":"x"}"#, None),
        ("", None),
    ] {
        assert_eq!(get(text, "input"), want.and_then(string), "{text}");
    }
}

#[test]
fn a_quote_after_an_even_run_of_backslashes_ends_a_string() {
    assert_eq!(get(r#"{"input":"a\\","b":1}"#, "input"), string("a\\"));
    assert_eq!(get(r#"{"x":"\\\"}","input":"y"}"#, "input"), string("y"));
}
