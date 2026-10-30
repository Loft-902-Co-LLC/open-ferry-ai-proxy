use super::*;

#[test]
fn round5_probe_shape_sweep() {
    let mut failures = Vec::new();
    for shape in ["root", "object", "array", "array_array", "object_object"] {
        for translated in [false, true] {
            let properties: serde_json::Map<_, _> = (0..500)
                .map(|index| {
                    (
                        format!("k{index:03}"),
                        json!({"type":"object","properties":{"value":{"type":"string"}}}),
                    )
                })
                .collect();
            let wide = json!({"type":"object","properties":properties});
            let (schema, prefix) = match shape {
                "root" => (wide, "$"),
                "object" => (
                    json!({"type":"object","properties":{"opts":wide}}),
                    "$.opts",
                ),
                "array" => (
                    json!({"type":"object","properties":{"rows":{"type":"array","items":wide}}}),
                    "$.rows[0]",
                ),
                "array_array" => (
                    json!({"type":"object","properties":{"rows":{"type":"array","items":{"type":"array","items":wide}}}}),
                    "$.rows[0][0]",
                ),
                "object_object" => (
                    json!({"type":"object","properties":{"opts":{"type":"object","properties":{"inner":wide}}}}),
                    "$.opts.inner",
                ),
                _ => unreachable!(),
            };
            let request = json!({"tools":[{"name":"wide","input_schema":schema}]});
            let sent = json!({"tools":[{"functionDeclarations":[{"name":"wide","parametersJsonSchema":schema}]}]});
            let mut stream = GeminiToClaudeStream::new(&request);
            if translated {
                stream = stream.with_translated_request(&sent);
            }
            let mut all = Vec::new();
            let mut probes = 0;
            for index in 0..500 {
                all.extend(events(
                    &mut stream,
                    &frame(
                        json!([{"functionCall":{
                            "name":"wide","willContinue":true,"partialArgs":[{
                                "jsonPath":format!("{prefix}.k{index:03}.value"),"stringValue":"x"
                            }]
                        }}]),
                        false,
                    ),
                ));
                probes += 1;
                if stream.incomplete_call {
                    break;
                }
            }
            all.extend(events(
                &mut stream,
                &frame(json!([{"functionCall":{}}]), true),
            ));
            all.extend(parse(&stream.translate(b"[DONE]")));
            let complete =
                !stream.incomplete_call && all.iter().any(|event| event["type"] == "message_stop");
            eprintln!(
                "R5_SWEEP shape={shape} translated={translated} complete={complete} probes={probes}"
            );
            if !complete {
                failures.push(format!("{shape}/{translated}: complete={complete},"));
            }
            if complete {
                let args: Value = serde_json::from_str(&arguments(&all)).unwrap();
                let wide = match shape {
                    "root" => &args,
                    "object" => &args["opts"],
                    "array" => &args["rows"][0],
                    "array_array" => &args["rows"][0][0],
                    "object_object" => &args["opts"]["inner"],
                    _ => unreachable!(),
                };
                assert_eq!(wide.as_object().unwrap().len(), 500);
                for index in 0..500 {
                    assert_eq!(wide[format!("k{index:03}")]["value"], "x");
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn round5_parameterless_tool_accepts_large_escaped_string() {
    let request = json!({"tools":[{"name":"ping","input_schema":{"type":"object","properties":{},"additionalProperties":false}}]});
    let mut stream = GeminiToClaudeStream::new(&request);
    let mut all = events(
        &mut stream,
        &frame(
            json!([{"functionCall":{"name":"ping","partialArgs":[{
                "jsonPath":"$.reason","stringValue":"\"".repeat(2_200_000)
            }]}}]),
            true,
        ),
    );
    all.extend(parse(&stream.translate(b"[DONE]")));
    assert!(!stream.incomplete_call);
    assert_eq!(arguments(&all), "{}");
    assert!(all.iter().any(|event| event["type"] == "message_stop"));
}
