use super::*;

#[test]
fn round5_root_bool_and_null_reserve_only_key_and_node_bytes() {
    for part in [
        json!({"boolValue":true}),
        json!({"boolValue":false}),
        json!({"nullValue":null}),
    ] {
        let mut part = part;
        part["jsonPath"] = json!("$.x");
        let mut assembler = Assembler::default();
        assembler
            .push(&json!({"partialArgs":[part]}), None)
            .unwrap();
        assert_eq!(
            assembler.buffered, 5,
            "root and bool/null payloads are not reserved upstream"
        );
    }
}

#[test]
fn round5_buffered_bytes_match_upstream_for_nested_mixed_values() {
    let mut assembler = Assembler::default();
    assembler
        .push(
            &json!({"partialArgs":[
                {"jsonPath":"$.rows[0].s","stringValue":"\"\n\t\\é"},
                {"jsonPath":"$.rows[0].b","boolValue":false},
                {"jsonPath":"$.rows[0].n","nullValue":null},
                {"jsonPath":"$.rows[0].v","numberValue":"1E+2"}
            ]}),
            None,
        )
        .unwrap();
    // rows: key+4, index 0: 4, four leaf keys: 4*(1+4), raw string: 6,
    // number spelling: 4. No root or bool/null payload charge.
    assert_eq!(assembler.buffered, 8 + 4 + 20 + 6 + 4);
}

#[test]
fn round5_raw_buffer_and_emitted_output_limits_are_independent() {
    let clean = |_: &Value, _: &WorkBudget| json!({});
    for (length, succeeds) in [(MAX_BYTES - 5, true), (MAX_BYTES - 4, false)] {
        let mut assembler = Assembler::default();
        let result = assembler.push(
            &json!({"partialArgs":[{"jsonPath":"$.x","stringValue":"x".repeat(length)}]}),
            Some(&clean),
        );
        assert_eq!(result.is_ok(), succeeds, "raw bytes={length}");
        if succeeds {
            assert_eq!(assembler.buffered, MAX_BYTES);
            assert_eq!(assembler.full_arguments().unwrap(), "{}");
        }
    }
    // An escaped value can fit the raw buffer yet exceed actual emitted JSON.
    assert!(
        Assembler::default()
            .push(
                &json!({"partialArgs":[{"jsonPath":"$.x","stringValue":"\"".repeat(2_200_000)}]}),
                None
            )
            .is_err()
    );
    // Encoded path spelling and its cleaner probe can exceed 4 MiB while the
    // decoded keys fit the raw buffer and the held tree cleans to two bytes.
    let key = "\"".repeat(2_100_000);
    let path = format!("$[{}].value", serde_json::to_string(&key).unwrap());
    assert!(path.len() > MAX_BYTES);
    let request = json!({"tools":[{"name":"tool","input_schema":{"type":"object","properties":{},"additionalProperties":false}}]});
    let mut stream = crate::gemini::claude::GeminiToClaudeStream::new(&request);
    let frame = json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"tool","partialArgs":[{"jsonPath":path,"stringValue":"x"}]}}]},"finishReason":"STOP"}]});
    let mut output = stream.translate(frame.to_string().as_bytes());
    output.push_str(&stream.translate(b"[DONE]"));
    let events: Vec<Value> = output
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|json| serde_json::from_str(json).ok())
        .collect();
    let arguments: String = events
        .iter()
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(arguments, "{}");
    assert!(events.iter().any(|event| event["type"] == "message_stop"));

    // Numeric coercion reserves the resulting number, not invalid source text.
    let mut number = Assembler::default();
    number
        .push(
            &json!({"partialArgs":[{"jsonPath":"$.x","numberValue":"x".repeat(MAX_BYTES + 1)}]}),
            None,
        )
        .unwrap();
    assert_eq!(number.buffered, 6);
    assert_eq!(number.full_arguments().unwrap(), r#"{"x":0}"#);
}
