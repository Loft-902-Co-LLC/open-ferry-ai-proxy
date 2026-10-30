// Ported from CLIProxyAPI internal/util/sanitize_test.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;

// Ported from TestCleanClaudeToolArguments, including the closed-object re-clean.
#[test]
fn clean_claude_tool_arguments() {
    let parse = |raw: &str| serde_json::from_str::<Value>(raw).unwrap();
    let original_0 = parse(
        r####"{"tools":[{"name":"EnterPlanMode","input_schema":{"type":"object","properties":{}}},{"name":"CronList","input_schema":{"type":"object"}},{"name":"Bash","input_schema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}},{"name":"CustomToolWithReason","input_schema":{"type":"object","properties":{"reason":{"type":"string"}}}},{"name":"DynamicTool","input_schema":{"type":"object","properties":{"name":{"type":"string"}},"additionalProperties":true}},{"name":"UnionTool","input_schema":{"anyOf":[{"type":"object","properties":{"reason":{"type":"string"}}}]}}]}"####,
    );
    let original_1 = parse(r####"null"####);
    let original_2 = parse(
        r####"{"tools":[{"name":"MapTool","input_schema":{"type":"object","additionalProperties":true}},{"name":"TypedMapTool","input_schema":{"type":"object","properties":{},"additionalProperties":{"type":"string"}}}]}"####,
    );
    let original_3 = parse(
        r####"{"tools":[{"name":"Read","input_schema":{"type":"object","properties":{}}},{"name":"read","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}]}"####,
    );
    let original_4 = parse(
        r####"{"tools":[{"name":"AllOfEmpty","input_schema":{"type":"object","allOf":[{"type":"object","properties":{}}]}},{"name":"AllOfArgs","input_schema":{"type":"object","allOf":[{"type":"object","properties":{"path":{"type":"string"}}}]}},{"name":"RefEmpty","input_schema":{"$ref":"#/$defs/empty","$defs":{"empty":{"type":"object","properties":{}}}}},{"name":"NestedTool","input_schema":{"type":"object","properties":{"opts":{"type":"object","properties":{}},"cfg":{"type":"object","properties":{"mode":{"type":"string"}}}}}}]}"####,
    );
    let original_5 = parse(
        r####"{"tools":[{"name":"FreeFormNested","input_schema":{"type":"object","properties":{"payload":{}},"required":["payload"]}},{"name":"AllOfMerge","input_schema":{"type":"object","properties":{"cfg":{"type":"object","properties":{"mode":{"type":"string"}}}},"allOf":[{"type":"object","properties":{"cfg":{"type":"object","properties":{"reason":{"type":"string"}}}}}]}},{"name":"SingleUnionEmpty","input_schema":{"anyOf":[{"type":"object","properties":{}}]}},{"name":"DynamicParent","input_schema":{"type":"object","properties":{"opts":{"type":"object","properties":{}}},"additionalProperties":true}},{"name":"RefArray","input_schema":{"type":"object","properties":{"items":{"$ref":"#/$defs/list"}},"$defs":{"list":{"type":"array","items":{"type":"object","properties":{"v":{"type":"string"}}}}}}},{"name":"PropRef","input_schema":{"type":"object","properties":{"template":{"type":"object","properties":{}},"copy":{"$ref":"#/properties/template"}}}}]}"####,
    );
    let original_6 = parse(
        r####"{"tools":[{"name":"AllOfDynamicConflict","input_schema":{"type":"object","properties":{},"additionalProperties":false,"allOf":[{"additionalProperties":true}]}},{"name":"MultiUnionEmpty","input_schema":{"anyOf":[{"type":"object","properties":{},"additionalProperties":false},{"type":"object","properties":{"x":{"type":"string"}},"required":["x"],"additionalProperties":false}]}},{"name":"DupTool","input_schema":{"type":"object","properties":{}}},{"name":"DupTool","input_schema":{"type":"object","properties":{}}},{"name":"ConflictingDup","input_schema":{"type":"object","properties":{}}},{"name":"ConflictingDup","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}},{"name":"MatrixTool","input_schema":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"array","items":{"type":"object","properties":{}}}}}}}]}"####,
    );
    let original_7 = parse(
        r####"{"tools":[{"name":"UnionChoice","input_schema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]},{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}]}},"required":["choice"]}},{"name":"AllOfChildMerge","input_schema":{"type":"object","properties":{"cfg":{"type":"object","properties":{"mode":{"type":"string"}}}},"allOf":[{"properties":{"cfg":{"type":"object","properties":{}}}}]}},{"name":"NestedRefTool","input_schema":{"type":"object","properties":{"opts":{"$ref":"#/$defs/Empty"}},"$defs":{"Empty":{"type":"object","properties":{},"additionalProperties":false}}}},{"name":"WrappedPlaceholder","input_schema":{"type":"object","properties":{"flag":{"type":"string"}},"additionalProperties":false}}]}"####,
    );
    let original_8 = parse(
        r####"{"tools":[{"name":"Explain","input_schema":{"type":"object","required":["reason"],"additionalProperties":true}},{"name":"Choose","input_schema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"opts":{"type":"object","properties":{},"additionalProperties":false}},"required":["opts"]},{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}]}},"required":["choice"]}},{"name":"Options","input_schema":{"type":"object","properties":{"flag":{"type":"string"}},"additionalProperties":true}}]}"####,
    );
    let original_9 = parse(
        r####"{"tools":[{"name":"PlainOptions","input_schema":{"type":"object","properties":{"flag":{"type":"string"}}}},{"name":"StrictOptions","input_schema":{"type":"object","properties":{"flag":{"type":"string"}},"additionalProperties":false}},{"name":"BranchChoice","input_schema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"cfg":{"type":"object","additionalProperties":true}},"required":["cfg"]},{"type":"object","properties":{"cfg":{"type":"object","properties":{},"additionalProperties":false}},"required":["cfg"]}]}},"required":["choice"]}}]}"####,
    );
    let original_10 = parse(
        r####"{"tools":[{"name":"Configure","input_schema":{"type":"object","properties":{"cfg":{"anyOf":[{"type":"object","properties":{},"additionalProperties":false},{"type":"object","additionalProperties":true}]}},"required":["cfg"]}}]}"####,
    );
    let original_11 = parse(
        r####"{"tools":[{"name":"ExplainPattern","input_schema":{"type":"object","properties":{},"patternProperties":{"^reason$":{"type":"string"}},"additionalProperties":false}},{"name":"PatternData","input_schema":{"type":"object","properties":{},"patternProperties":{"^x-":{"type":"string"}},"additionalProperties":false}}]}"####,
    );
    let original_12 = parse(
        r####"{"tools":[{"name":"ConfigureNested","input_schema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"cfg":{"type":"object","properties":{"mode":{"type":"string"}},"additionalProperties":false}},"required":["cfg"],"additionalProperties":false},{"type":"object","patternProperties":{"^cfg$":{"type":"object","additionalProperties":true}},"additionalProperties":false}]}},"required":["choice"]}}]}"####,
    );
    let original_13 = parse(
        r####"{"tools":[{"name":"ImplicitBranch","input_schema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"cfg":{"type":"object","properties":{"mode":{"type":"string"}},"additionalProperties":false}},"required":["cfg"],"additionalProperties":false},{"type":"object","properties":{"tag":{"type":"string"}},"required":["tag"]}]}},"required":["choice"],"additionalProperties":false}}]}"####,
    );
    let original_14 = parse(
        r####"{"tools":[{"name":"ChooseImplicit","input_schema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{},"additionalProperties":false},{"type":"object","properties":{"tag":{"type":"string"}},"required":["tag"]}]}},"required":["choice"]}}]}"####,
    );
    let original_15 = parse(
        r####"{"tools":[{"name":"ConfigureAlt","input_schema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"cfg":{"type":"object","properties":{},"additionalProperties":false}},"required":["cfg"],"additionalProperties":false},{"type":"object","properties":{"cfg":{"type":"object","properties":{"mode":{"type":"string"}},"required":["mode"]}},"required":["cfg"],"additionalProperties":false}]}},"required":["choice"],"additionalProperties":false}}]}"####,
    );
    let original_16 = parse(
        r####"{"tools":[{"name":"DeepAlt","input_schema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"cfg":{"type":"object","properties":{"opts":{"type":"object","properties":{"mode":{"type":"string"}},"additionalProperties":false}},"required":["opts"],"additionalProperties":false}},"required":["cfg"],"additionalProperties":false},{"type":"object","properties":{"cfg":{"type":"object","properties":{"tag":{"type":"string"}},"required":["tag"]}},"required":["cfg"],"additionalProperties":false}]}},"required":["choice"],"additionalProperties":false}}]}"####,
    );
    let original_17 = parse(
        r####"{"tools":[{"name":"ConfigureAltFree","input_schema":{"type":"object","anyOf":[{"type":"object","properties":{"cfg":{"type":"object","properties":{},"additionalProperties":false}},"required":["cfg"],"additionalProperties":false},{"type":"object","properties":{"cfg":{}},"required":["cfg"],"additionalProperties":false}]}}]}"####,
    );
    let original_18 = parse(
        r####"{"tools":[{"name":"Batch","input_schema":{"type":"object","anyOf":[{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{},"additionalProperties":false}}},"required":["rows"],"additionalProperties":false},{"type":"object","properties":{"rows":{}},"required":["rows"],"additionalProperties":false}]}},{"name":"BatchStrict","input_schema":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{},"additionalProperties":false}}},"required":["rows"]}}]}"####,
    );
    let original_19 = parse(
        r####"{"tools":[{"name":"1","input_schema":{"type":"object","properties":{},"required":["reason"]}},{"name":"_1","input_schema":{"type":"object","properties":{}}}]}"####,
    );
    let original_20 = parse(
        r####"{"tools":[{"name":"ExplainFixed","input_schema":{"type":"object","enum":[{"reason":"business"}]}},{"name":"ConstTool","input_schema":{"type":"object","const":{"reason":"fixed"}}}]}"####,
    );
    let original_21 = parse(
        r####"{"tools":[{"name":"ExplainFixed","input_schema":{"type":"object","enum":[{"reason":"business"}],"anyOf":[{"type":"object","properties":{}}]}}]}"####,
    );
    let original_22 = parse(
        r####"{"tools":[{"name":"BatchEnum","input_schema":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{}},"enum":[[{"reason":"business"}]]}},"required":["rows"],"additionalProperties":false}}]}"####,
    );
    let original_23 = parse(
        r####"{"tools":[{"name":"BatchContains","input_schema":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{}},"contains":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}}},"required":["rows"],"additionalProperties":false}}]}"####,
    );
    let original_24 = parse(
        r####"{"tools":[{"name":"ConfigureSibling","input_schema":{"type":"object","required":["cfg"],"anyOf":[{"type":"object","properties":{"cfg":{"type":"object","properties":{}}},"required":["cfg"],"additionalProperties":false},{"type":"object","properties":{"other":{"type":"string"}},"required":["other"],"additionalProperties":false}],"allOf":[{"properties":{"cfg":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}}}]}}]}"####,
    );
    let original_25 = parse(
        r####"{"tools":[{"name":"StoreMetadata","input_schema":{"type":"object","properties":{},"minProperties":1}}]}"####,
    );
    let original_26 = parse(
        r####"{"tools":[{"name":"BatchUnique","input_schema":{"type":"object","properties":{"rows":{"type":"array","uniqueItems":true,"items":{"type":"object","properties":{"tag":{"type":"string"}}}}},"required":["rows"],"additionalProperties":false}}]}"####,
    );
    let sent_0 = parse(r####"null"####);
    let sent_1 = parse(
        r####"{"model":"gemini-3.8-flash-high","request":{"tools":[{"functionDeclarations":[{"name":"Bash","parametersJsonSchema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}]}]}}"####,
    );
    let sent_2 = parse(
        r####"{"model":"gemini-3.8-flash-high","request":{"tools":[{"functionDeclarations":[{"name":"Bash","parametersJsonSchema":{"type":"object","properties":{"command":{"type":"string"},"reason":{"type":"string"}},"required":["command","reason"]}}]}]}}"####,
    );
    let sent_3 = parse(
        r####"{"model":"gemini-3.8-flash-high","request":{"tools":[{"functionDeclarations":[{"name":"AllOfDynamicConflict","parametersJsonSchema":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}},{"name":"MultiUnionEmpty","parametersJsonSchema":{"anyOf":[{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]},{"type":"object","properties":{"x":{"type":"string"}},"required":["x"]}]}},{"name":"MatrixTool","parametersJsonSchema":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"array","items":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}}}}}}]}]}}"####,
    );
    let sent_4 = parse(
        r####"{"model":"gemini-3.8-flash-high","request":{"tools":[{"functionDeclarations":[{"name":"UnionChoice","parametersJsonSchema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]},{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}]}},"required":["choice"]}},{"name":"AllOfChildMerge","parametersJsonSchema":{"type":"object","properties":{"cfg":{"type":"object","properties":{"mode":{"type":"string"}}}}}},{"name":"NestedRefTool","parametersJsonSchema":{"type":"object","properties":{"opts":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}}}},{"name":"WrappedPlaceholder","parametersJsonSchema":{"type":"object","properties":{"flag":{"type":"string"}}}}]}]}}"####,
    );
    let sent_5 = parse(
        r####"{"model":"gemini-3.8-flash-high","tools":[{"functionDeclarations":[{"name":"Explain","parametersJsonSchema":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}},{"name":"Choose","parametersJsonSchema":{"type":"object","properties":{"choice":{"type":"object","properties":{"opts":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}},"required":["opts"]}},"required":["choice"]}},{"name":"Options","parametersJsonSchema":{"type":"object","properties":{"flag":{"type":"string"}},"additionalProperties":true}}]}]}"####,
    );
    let sent_6 = parse(
        r####"{"model":"gemini-2.5-pro","tools":[{"functionDeclarations":[{"name":"PlainOptions","parametersJsonSchema":{"type":"object","properties":{"flag":{"type":"string"}}}},{"name":"StrictOptions","parametersJsonSchema":{"type":"object","properties":{"flag":{"type":"string"}}}},{"name":"BranchChoice","parametersJsonSchema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"cfg":{"type":"object","additionalProperties":true}},"required":["cfg"]},{"type":"object","properties":{"cfg":{"type":"object","properties":{}}},"required":["cfg"]}]}},"required":["choice"]}}]}]}"####,
    );
    let sent_7 = parse(
        r####"{"model":"gemini-3.1-pro","request":{"tools":[{"functionDeclarations":[{"name":"Configure","parametersJsonSchema":{"type":"object","properties":{"cfg":{"anyOf":[{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]},{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}]}},"required":["cfg"]}}]}]}}"####,
    );
    let sent_8 = parse(
        r####"{"model":"gemini-3.1-pro","request":{"tools":[{"functionDeclarations":[{"name":"ExplainPattern","parametersJsonSchema":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}},{"name":"PatternData","parametersJsonSchema":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}}]}]}}"####,
    );
    let sent_9 = parse(
        r####"{"model":"gemini-3.1-pro","request":{"tools":[{"functionDeclarations":[{"name":"ConfigureNested","parametersJsonSchema":{"type":"object","properties":{"choice":{"type":"object","properties":{"cfg":{"type":"object","properties":{"mode":{"type":"string"},"_":{"type":"boolean"}},"required":["_"]}},"required":["cfg"]}},"required":["choice"]}}]}]}}"####,
    );
    let sent_10 = parse(
        r####"{"model":"gemini-3.1-pro","request":{"tools":[{"functionDeclarations":[{"name":"ImplicitBranch","parametersJsonSchema":{"type":"object","properties":{"choice":{"type":"object","properties":{"cfg":{"type":"object","properties":{"mode":{"type":"string"},"_":{"type":"boolean"}},"required":["_"]}},"required":["cfg"]}},"required":["choice"]}}]}]}}"####,
    );
    let sent_11 = parse(
        r####"{"model":"gemini-3-pro-high","request":{"tools":[{"functionDeclarations":[{"name":"ChooseImplicit","parametersJsonSchema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]},{"type":"object","properties":{"tag":{"type":"string"}},"required":["tag"]}]}},"required":["choice"]}}]}]}}"####,
    );
    let sent_12 = parse(
        r####"{"model":"gemini-3-pro-high","request":{"tools":[{"functionDeclarations":[{"name":"ConfigureAlt","parametersJsonSchema":{"type":"object","properties":{"choice":{"anyOf":[{"type":"object","properties":{"cfg":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}},"required":["cfg"]},{"type":"object","properties":{"cfg":{"type":"object","properties":{"mode":{"type":"string"}},"required":["mode"]}},"required":["cfg"]}]}},"required":["choice"]}}]}]}}"####,
    );
    let sent_13 = parse(
        r####"{"model":"gemini-3-pro-high","request":{"tools":[{"functionDeclarations":[{"name":"DeepAlt","parametersJsonSchema":{"type":"object","properties":{"choice":{"type":"object","properties":{"cfg":{"type":"object","properties":{"opts":{"type":"object","properties":{"mode":{"type":"string"},"_":{"type":"boolean"}},"required":["_"]}},"required":["opts"]}},"required":["cfg"]}},"required":["choice"]}}]}]}}"####,
    );
    let sent_14 = parse(
        r####"{"model":"gemini-2.5-pro","tools":[{"functionDeclarations":[{"name":"ConfigureAltFree","parametersJsonSchema":{"type":"object","anyOf":[{"type":"object","properties":{"cfg":{"type":"object","properties":{}}},"required":["cfg"]},{"type":"object","properties":{"cfg":{}},"required":["cfg"]}]}}]}]}"####,
    );
    let sent_15 = parse(
        r####"{"model":"gemini-2.5-pro","tools":[{"functionDeclarations":[{"name":"Batch","parametersJsonSchema":{"type":"object","anyOf":[{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{}}}},"required":["rows"]},{"type":"object","properties":{"rows":{}},"required":["rows"]}]}},{"name":"BatchStrict","parametersJsonSchema":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}}},"required":["rows"]}}]}]}"####,
    );
    let sent_16 = parse(
        r####"{"model":"claude-sonnet-4-6","request":{"tools":[{"functionDeclarations":[{"name":"ExplainFixed","parametersJsonSchema":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}},{"name":"ConstTool","parametersJsonSchema":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}}]}]}}"####,
    );
    let sent_17 = parse(
        r####"{"model":"claude-sonnet-4-6","request":{"tools":[{"functionDeclarations":[{"name":"BatchEnum","parametersJsonSchema":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}}},"required":["rows"]}}]}]}}"####,
    );
    let sent_18 = parse(
        r####"{"model":"claude-sonnet-4-6","request":{"tools":[{"functionDeclarations":[{"name":"BatchContains","parametersJsonSchema":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}}},"required":["rows"]}}]}]}}"####,
    );
    let sent_19 = parse(
        r####"{"model":"gemini-2.5-pro","tools":[{"functionDeclarations":[{"name":"ConfigureSibling","parametersJsonSchema":{"type":"object","properties":{"cfg":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}},"required":["cfg"]}}]}]}"####,
    );
    let sent_20 = parse(
        r####"{"model":"claude-sonnet-4-6","request":{"tools":[{"functionDeclarations":[{"name":"StoreMetadata","parametersJsonSchema":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"],"minProperties":1}}]}]}}"####,
    );
    let sent_21 = parse(
        r####"{"model":"claude-sonnet-4-6","request":{"tools":[{"functionDeclarations":[{"name":"BatchUnique","parametersJsonSchema":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{"tag":{"type":"string"},"_":{"type":"boolean"}},"required":["_"]}}},"required":["rows"]}}]}]}}"####,
    );
    for (name, raw, tool, original, want, sent) in [
        (
            "parameterless empty properties",
            r####"{"reason":"Entering plan mode"}"####,
            "EnterPlanMode",
            &original_0,
            r####"{}"####,
            &sent_0,
        ),
        (
            "parameterless missing properties",
            r####"{"reason":"List jobs"}"####,
            "CronList",
            &original_0,
            r####"{}"####,
            &sent_0,
        ),
        (
            "declared args preserved",
            r####"{"command":"ls -la"}"####,
            "Bash",
            &original_0,
            r####"{"command":"ls -la"}"####,
            &sent_0,
        ),
        (
            "undeclared reason stripped",
            r####"{"command":"ls -la","reason":"why"}"####,
            "Bash",
            &original_0,
            r####"{"command":"ls -la"}"####,
            &sent_0,
        ),
        (
            "reason kept when translated schema lacks placeholder",
            r####"{"command":"ls -la","reason":"why"}"####,
            "Bash",
            &original_0,
            r####"{"command":"ls -la","reason":"why"}"####,
            &sent_1,
        ),
        (
            "reason stripped when translated schema marks placeholder",
            r####"{"command":"ls -la","reason":"why"}"####,
            "Bash",
            &original_0,
            r####"{"command":"ls -la"}"####,
            &sent_2,
        ),
        (
            "undeclared underscore stripped",
            r####"{"command":"ls -la","_":true}"####,
            "Bash",
            &original_0,
            r####"{"command":"ls -la"}"####,
            &sent_0,
        ),
        (
            "declared reason preserved",
            r####"{"reason":"valid"}"####,
            "CustomToolWithReason",
            &original_0,
            r####"{"reason":"valid"}"####,
            &sent_0,
        ),
        (
            "additional properties preserved",
            r####"{"name":"x","reason":"dynamic"}"####,
            "DynamicTool",
            &original_0,
            r####"{"name":"x","reason":"dynamic"}"####,
            &sent_0,
        ),
        (
            "union schema passthrough",
            r####"{"reason":"x"}"####,
            "UnionTool",
            &original_0,
            r####"{"reason":"x"}"####,
            &sent_0,
        ),
        (
            "unknown tool passthrough",
            r####"{"reason":"x"}"####,
            "EnterPlanMode",
            &original_1,
            r####"{"reason":"x"}"####,
            &sent_0,
        ),
        (
            "empty args",
            r####""####,
            "EnterPlanMode",
            &original_0,
            r####"{}"####,
            &sent_0,
        ),
        (
            "empty object args",
            r####"{}"####,
            "EnterPlanMode",
            &original_0,
            r####"{}"####,
            &sent_0,
        ),
        (
            "dynamic map tool preserved",
            r####"{"key":"value"}"####,
            "MapTool",
            &original_2,
            r####"{"key":"value"}"####,
            &sent_0,
        ),
        (
            "typed map tool preserved",
            r####"{"key":"value"}"####,
            "TypedMapTool",
            &original_2,
            r####"{"key":"value"}"####,
            &sent_0,
        ),
        (
            "case collision exact match keeps args",
            r####"{"path":"/tmp/a"}"####,
            "read",
            &original_3,
            r####"{"path":"/tmp/a"}"####,
            &sent_0,
        ),
        (
            "case collision exact match parameterless",
            r####"{"reason":"x"}"####,
            "Read",
            &original_3,
            r####"{}"####,
            &sent_0,
        ),
        (
            "allOf empty normalized",
            r####"{"reason":"x"}"####,
            "AllOfEmpty",
            &original_4,
            r####"{}"####,
            &sent_0,
        ),
        (
            "allOf declared args preserved",
            r####"{"path":"/tmp/a"}"####,
            "AllOfArgs",
            &original_4,
            r####"{"path":"/tmp/a"}"####,
            &sent_0,
        ),
        (
            "ref to empty normalized",
            r####"{"reason":"x"}"####,
            "RefEmpty",
            &original_4,
            r####"{}"####,
            &sent_0,
        ),
        (
            "nested placeholders stripped",
            r####"{"opts":{"reason":"x"},"cfg":{"mode":"fast","_":true}}"####,
            "NestedTool",
            &original_4,
            r####"{"opts":{},"cfg":{"mode":"fast"}}"####,
            &sent_0,
        ),
        (
            "free-form nested preserved",
            r####"{"payload":{"value":1}}"####,
            "FreeFormNested",
            &original_5,
            r####"{"payload":{"value":1}}"####,
            &sent_0,
        ),
        (
            "allOf deep merge keeps declared reason",
            r####"{"cfg":{"mode":"a","reason":"b"}}"####,
            "AllOfMerge",
            &original_5,
            r####"{"cfg":{"mode":"a","reason":"b"}}"####,
            &sent_0,
        ),
        (
            "single union empty normalized",
            r####"{"reason":"x"}"####,
            "SingleUnionEmpty",
            &original_5,
            r####"{}"####,
            &sent_0,
        ),
        (
            "dynamic parent still cleans declared child",
            r####"{"extra":1,"opts":{"reason":"x"}}"####,
            "DynamicParent",
            &original_5,
            r####"{"extra":1,"opts":{}}"####,
            &sent_0,
        ),
        (
            "ref array items cleaned",
            r####"{"items":[{"v":"a","_":true}]}"####,
            "RefArray",
            &original_5,
            r####"{"items":[{"v":"a"}]}"####,
            &sent_0,
        ),
        (
            "property ref cleaned",
            r####"{"copy":{"reason":"x"},"template":{}}"####,
            "PropRef",
            &original_5,
            r####"{"copy":{},"template":{}}"####,
            &sent_0,
        ),
        (
            "allOf dynamic conflict uses translated schema",
            r####"{"reason":"x"}"####,
            "AllOfDynamicConflict",
            &original_6,
            r####"{}"####,
            &sent_3,
        ),
        (
            "multi-branch union uses translated schema",
            r####"{"reason":"x"}"####,
            "MultiUnionEmpty",
            &original_6,
            r####"{}"####,
            &sent_3,
        ),
        (
            "identical duplicates cleaned",
            r####"{"reason":"x"}"####,
            "DupTool",
            &original_6,
            r####"{}"####,
            &sent_0,
        ),
        (
            "conflicting duplicates passthrough",
            r####"{"path":"/tmp/a"}"####,
            "ConflictingDup",
            &original_6,
            r####"{"path":"/tmp/a"}"####,
            &sent_0,
        ),
        (
            "conflicting duplicates keep first-declared cleaning",
            r####"{"reason":"x"}"####,
            "ConflictingDup",
            &original_6,
            r####"{"reason":"x"}"####,
            &sent_0,
        ),
        (
            "matrix nested arrays cleaned",
            r####"{"rows":[[{"reason":"x"}]]}"####,
            "MatrixTool",
            &original_6,
            r####"{"rows":[[{}]]}"####,
            &sent_3,
        ),
        (
            "union declared required reason preserved",
            r####"{"choice":{"reason":"maintenance"}}"####,
            "UnionChoice",
            &original_7,
            r####"{"choice":{"reason":"maintenance"}}"####,
            &sent_4,
        ),
        (
            "allOf child merge keeps mode",
            r####"{"cfg":{"mode":"fast"}}"####,
            "AllOfChildMerge",
            &original_7,
            r####"{"cfg":{"mode":"fast"}}"####,
            &sent_4,
        ),
        (
            "nested ref placeholder cleaned",
            r####"{"opts":{"reason":"initialize"}}"####,
            "NestedRefTool",
            &original_7,
            r####"{"opts":{}}"####,
            &sent_4,
        ),
        (
            "required-only reason preserved",
            r####"{"reason":"maintenance"}"####,
            "Explain",
            &original_8,
            r####"{"reason":"maintenance"}"####,
            &sent_5,
        ),
        (
            "union branch nested placeholder cleaned",
            r####"{"choice":{"opts":{"reason":"initialize"}}}"####,
            "Choose",
            &original_8,
            r####"{"choice":{"opts":{}}}"####,
            &sent_5,
        ),
        (
            "gemini route keeps dynamic underscore",
            r####"{"flag":"x","_":true}"####,
            "Options",
            &original_8,
            r####"{"flag":"x","_":true}"####,
            &sent_5,
        ),
        (
            "implicit additional properties keeps underscore",
            r####"{"flag":"x","_":true}"####,
            "PlainOptions",
            &original_9,
            r####"{"flag":"x","_":true}"####,
            &sent_6,
        ),
        (
            "alternative branch dynamic data preserved",
            r####"{"choice":{"cfg":{"value":1}}}"####,
            "BranchChoice",
            &original_9,
            r####"{"choice":{"cfg":{"value":1}}}"####,
            &sent_6,
        ),
        (
            "alternative branch dynamic reason preserved",
            r####"{"cfg":{"reason":"business"}}"####,
            "Configure",
            &original_10,
            r####"{"cfg":{"reason":"business"}}"####,
            &sent_7,
        ),
        (
            "pattern property reason preserved",
            r####"{"reason":"business"}"####,
            "ExplainPattern",
            &original_11,
            r####"{"reason":"business"}"####,
            &sent_8,
        ),
        (
            "pattern property non-placeholder data preserved",
            r####"{"x-note":"keep"}"####,
            "PatternData",
            &original_11,
            r####"{"x-note":"keep"}"####,
            &sent_8,
        ),
        (
            "union branch pattern child preserved",
            r####"{"choice":{"cfg":{"mode":"fast","_":true}}}"####,
            "ConfigureNested",
            &original_12,
            r####"{"choice":{"cfg":{"mode":"fast","_":true}}}"####,
            &sent_9,
        ),
        (
            "implicit open branch child preserved",
            r####"{"choice":{"tag":"keep","cfg":{"mode":"fast","_":true}}}"####,
            "ImplicitBranch",
            &original_13,
            r####"{"choice":{"tag":"keep","cfg":{"mode":"fast","_":true}}}"####,
            &sent_10,
        ),
        (
            "implicit open branch field preserved",
            r####"{"choice":{"tag":"keep","reason":"business"}}"####,
            "ChooseImplicit",
            &original_14,
            r####"{"choice":{"tag":"keep","reason":"business"}}"####,
            &sent_11,
        ),
        (
            "implicit open branch non-matching object cleaned",
            r####"{"choice":{"reason":"x"}}"####,
            "ChooseImplicit",
            &original_14,
            r####"{"choice":{}}"####,
            &sent_11,
        ),
        (
            "alternative child candidates keep matched data",
            r####"{"choice":{"cfg":{"mode":"fast","reason":"business"}}}"####,
            "ConfigureAlt",
            &original_15,
            r####"{"choice":{"cfg":{"mode":"fast","reason":"business"}}}"####,
            &sent_12,
        ),
        (
            "alternative child candidates clean unmatched placeholder",
            r####"{"choice":{"cfg":{"reason":"x"}}}"####,
            "ConfigureAlt",
            &original_15,
            r####"{"choice":{"cfg":{}}}"####,
            &sent_12,
        ),
        (
            "deep alternative candidates keep matched subtree",
            r####"{"choice":{"cfg":{"tag":"keep","opts":{"mode":"fast","_":true}}}}"####,
            "DeepAlt",
            &original_16,
            r####"{"choice":{"cfg":{"tag":"keep","opts":{"mode":"fast","_":true}}}}"####,
            &sent_13,
        ),
        (
            "deep alternative candidates clean unmatched subtree",
            r####"{"choice":{"cfg":{"opts":{"mode":"fast","_":true}}}}"####,
            "DeepAlt",
            &original_16,
            r####"{"choice":{"cfg":{"opts":{"mode":"fast"}}}}"####,
            &sent_13,
        ),
        (
            "free-form alternative child preserved",
            r####"{"cfg":{"reason":"business","value":1}}"####,
            "ConfigureAltFree",
            &original_17,
            r####"{"cfg":{"reason":"business","value":1}}"####,
            &sent_14,
        ),
        (
            "free-form array candidate preserved",
            r####"{"rows":[{"value":1}]}"####,
            "Batch",
            &original_18,
            r####"{"rows":[{"value":1}]}"####,
            &sent_15,
        ),
        (
            "strict array placeholder cleaned",
            r####"{"rows":[{"reason":"x"}]}"####,
            "BatchStrict",
            &original_18,
            r####"{"rows":[{}]}"####,
            &sent_15,
        ),
        (
            "fallback keeps required-only reason",
            r####"{"reason":"business"}"####,
            "1",
            &original_19,
            r####"{"reason":"business"}"####,
            &sent_0,
        ),
        (
            "object enum value preserved",
            r####"{"reason":"business"}"####,
            "ExplainFixed",
            &original_20,
            r####"{"reason":"business"}"####,
            &sent_16,
        ),
        (
            "object enum with union preserved",
            r####"{"reason":"business"}"####,
            "ExplainFixed",
            &original_21,
            r####"{"reason":"business"}"####,
            &sent_16,
        ),
        (
            "array enum elements preserved",
            r####"{"rows":[{"reason":"business"}]}"####,
            "BatchEnum",
            &original_22,
            r####"{"rows":[{"reason":"business"}]}"####,
            &sent_17,
        ),
        (
            "array enum elements preserved fallback",
            r####"{"rows":[{"reason":"business"}]}"####,
            "BatchEnum",
            &original_22,
            r####"{"rows":[{"reason":"business"}]}"####,
            &sent_0,
        ),
        (
            "array contains elements preserved",
            r####"{"rows":[{"reason":"business"}]}"####,
            "BatchContains",
            &original_23,
            r####"{"rows":[{"reason":"business"}]}"####,
            &sent_18,
        ),
        (
            "array contains elements preserved fallback",
            r####"{"rows":[{"reason":"business"}]}"####,
            "BatchContains",
            &original_23,
            r####"{"rows":[{"reason":"business"}]}"####,
            &sent_0,
        ),
        (
            "sibling allOf declaration preserved",
            r####"{"cfg":{"reason":"business"}}"####,
            "ConfigureSibling",
            &original_24,
            r####"{"cfg":{"reason":"business"}}"####,
            &sent_19,
        ),
        (
            "minProperties object preserved",
            r####"{"reason":"store","label":"keep"}"####,
            "StoreMetadata",
            &original_25,
            r####"{"reason":"store","label":"keep"}"####,
            &sent_20,
        ),
        (
            "minProperties object preserved fallback",
            r####"{"reason":"store","label":"keep"}"####,
            "StoreMetadata",
            &original_25,
            r####"{"reason":"store","label":"keep"}"####,
            &sent_0,
        ),
        (
            "uniqueItems elements preserved",
            r####"{"rows":[{"tag":"same","_":true},{"tag":"same","_":false}]}"####,
            "BatchUnique",
            &original_26,
            r####"{"rows":[{"tag":"same","_":true},{"tag":"same","_":false}]}"####,
            &sent_21,
        ),
        (
            "uniqueItems elements preserved fallback",
            r####"{"rows":[{"tag":"same","_":true},{"tag":"same","_":false}]}"####,
            "BatchUnique",
            &original_26,
            r####"{"rows":[{"tag":"same","_":true},{"tag":"same","_":false}]}"####,
            &sent_0,
        ),
        (
            "object enum value preserved fallback",
            r####"{"reason":"business"}"####,
            "ExplainFixed",
            &original_20,
            r####"{"reason":"business"}"####,
            &sent_0,
        ),
        (
            "object const value preserved",
            r####"{"reason":"fixed"}"####,
            "ConstTool",
            &original_20,
            r####"{"reason":"fixed"}"####,
            &sent_16,
        ),
    ] {
        let args = if raw.trim().is_empty() {
            json!({})
        } else {
            parse(raw)
        };
        assert_eq!(
            clean_arguments(&args, tool, original, sent),
            parse(want),
            "{name}"
        );
        let work = crate::common::vertex_partial_args::WorkBudget::default();
        let indexed = CallSchemas::resolve(tool, original, sent, &work);
        assert_eq!(indexed.clean(&args, &work), parse(want), "indexed {name}");
        assert!(work.check().is_ok(), "indexed budget {name}");
    }
}

// Ported from TestCleanClaudeToolArguments/disambiguated name uses translated schema.
#[test]
fn preserves_required_reason_for_disambiguated_tool_name() {
    let original = json!({"tools":[
        {"name":"1","input_schema":{"type":"object","properties":{},"required":["reason"]}},
        {"name":"_1","input_schema":{"type":"object","properties":{}}}
    ]});
    let sent = json!({"tools":[{"functionDeclarations":[{"name":"_1_ee3d96c33ea9","parametersJsonSchema":{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}}]}]});
    let args = json!({"reason":"business"});
    assert_eq!(clean_arguments(&args, "1", &original, &sent), args);
}

// Not upstream's: a disambiguated sent schema without a placeholder must
// preserve the real argument instead of falling back to schema inference.
#[test]
fn matches_disambiguated_sent_schema_before_cleaning() {
    let original = json!({"tools":[
        {"name":"1","input_schema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}},
        {"name":"_1","input_schema":{"type":"object","properties":{}}}
    ]});
    let sent = json!({"tools":[{"functionDeclarations":[{"name":"_1_ee3d96c33ea9","parametersJsonSchema":{"type":"object","properties":{"command":{"type":"string"}}}}]}]});
    let args = json!({"command":"ls","reason":"business"});
    assert_eq!(clean_arguments(&args, "1", &original, &sent), args);
}

// Not upstream's: local references use Go's integer-index parsing, including
// a sign or leading zero when traversing an array in a schema definition.
#[test]
fn local_references_follow_go_array_indices() {
    for index in ["0", "+00", "-0"] {
        let request = json!({"tools":[{"name":"Plan","input_schema":{
            "$ref":format!("#/$defs/variants/{index}"),
            "$defs":{"variants":[{"type":"object","properties":{}}]}
        }}]});
        assert_eq!(
            clean_arguments(&json!({"reason":"plan"}), "Plan", &request, &Value::Null),
            json!({})
        );
    }
}

// Ported from TestCleanClaudeToolArguments/closed object still strips executor underscore.
#[test]
fn closed_object_strips_recleaned_executor_underscore() {
    let original = json!({"tools":[{"name":"StrictOptions","input_schema":{"type":"object","properties":{"flag":{"type":"string"}},"additionalProperties":false}}]});
    let sent = json!({"tools":[{"functionDeclarations":[{"name":"StrictOptions","parametersJsonSchema":{"type":"object","properties":{"flag":{"type":"string"}}}}]}]});
    assert_eq!(
        clean_arguments(
            &json!({"flag":"x","_":true}),
            "StrictOptions",
            &original,
            &sent
        ),
        json!({"flag":"x"})
    );
}

// Not upstream's: Go 1.26.4 decisions from upstream object analysis after the
// wrapped cleaner cover
// normalization, refs, unions, required filtering and existing placeholders.
#[test]
fn reclean_projection_matches_upstream_edges() {
    let cases: Value = serde_json::from_str(r####"[
  ["type/required [\"object\", null]",{"type":"object","properties":{"flag":{"type":"string"}}},true,false],
  ["type/required [\"object\", []]",{"type":"object","properties":{"flag":{"type":"string"}},"required":[]},true,false],
  ["type/required [\"object\", [\"flag\"]]",{"type":"object","properties":{"flag":{"type":"string"}},"required":["flag"]},false,false],
  ["type/required [\"object\", [\"missing\"]]",{"type":"object","properties":{"flag":{"type":"string"}},"required":["missing"]},true,false],
  ["type/required [\"object\", \"flag\"]",{"type":"object","properties":{"flag":{"type":"string"}},"required":"flag"},true,false],
  ["type/required [\"object\", true]",{"type":"object","properties":{"flag":{"type":"string"}},"required":true},true,false],
  ["type/required [\"object\", [\"_\"]]",{"type":"object","properties":{"flag":{"type":"string"}},"required":["_"]},true,false],
  ["type/required [\"OBJECT\", null]",{"type":"OBJECT","properties":{"flag":{"type":"string"}}},false,false],
  ["type/required [\"OBJECT\", []]",{"type":"OBJECT","properties":{"flag":{"type":"string"}},"required":[]},false,false],
  ["type/required [\"OBJECT\", [\"flag\"]]",{"type":"OBJECT","properties":{"flag":{"type":"string"}},"required":["flag"]},false,false],
  ["type/required [\"OBJECT\", [\"missing\"]]",{"type":"OBJECT","properties":{"flag":{"type":"string"}},"required":["missing"]},false,false],
  ["type/required [\"OBJECT\", \"flag\"]",{"type":"OBJECT","properties":{"flag":{"type":"string"}},"required":"flag"},false,false],
  ["type/required [\"OBJECT\", true]",{"type":"OBJECT","properties":{"flag":{"type":"string"}},"required":true},false,false],
  ["type/required [\"OBJECT\", [\"_\"]]",{"type":"OBJECT","properties":{"flag":{"type":"string"}},"required":["_"]},false,false],
  ["type/required [\"string\", null]",{"type":"string","properties":{"flag":{"type":"string"}}},true,false],
  ["type/required [\"string\", []]",{"type":"string","properties":{"flag":{"type":"string"}},"required":[]},true,false],
  ["type/required [\"string\", [\"flag\"]]",{"type":"string","properties":{"flag":{"type":"string"}},"required":["flag"]},false,false],
  ["type/required [\"string\", [\"missing\"]]",{"type":"string","properties":{"flag":{"type":"string"}},"required":["missing"]},true,false],
  ["type/required [\"string\", \"flag\"]",{"type":"string","properties":{"flag":{"type":"string"}},"required":"flag"},true,false],
  ["type/required [\"string\", true]",{"type":"string","properties":{"flag":{"type":"string"}},"required":true},true,false],
  ["type/required [\"string\", [\"_\"]]",{"type":"string","properties":{"flag":{"type":"string"}},"required":["_"]},true,false],
  ["type/required [\"array\", null]",{"type":"array","properties":{"flag":{"type":"string"}}},true,false],
  ["type/required [\"array\", []]",{"type":"array","properties":{"flag":{"type":"string"}},"required":[]},true,false],
  ["type/required [\"array\", [\"flag\"]]",{"type":"array","properties":{"flag":{"type":"string"}},"required":["flag"]},false,false],
  ["type/required [\"array\", [\"missing\"]]",{"type":"array","properties":{"flag":{"type":"string"}},"required":["missing"]},true,false],
  ["type/required [\"array\", \"flag\"]",{"type":"array","properties":{"flag":{"type":"string"}},"required":"flag"},true,false],
  ["type/required [\"array\", true]",{"type":"array","properties":{"flag":{"type":"string"}},"required":true},true,false],
  ["type/required [\"array\", [\"_\"]]",{"type":"array","properties":{"flag":{"type":"string"}},"required":["_"]},true,false],
  ["type/required [null, null]",{"type":null,"properties":{"flag":{"type":"string"}}},true,false],
  ["type/required [null, []]",{"type":null,"properties":{"flag":{"type":"string"}},"required":[]},true,false],
  ["type/required [null, [\"flag\"]]",{"type":null,"properties":{"flag":{"type":"string"}},"required":["flag"]},false,false],
  ["type/required [null, [\"missing\"]]",{"type":null,"properties":{"flag":{"type":"string"}},"required":["missing"]},true,false],
  ["type/required [null, \"flag\"]",{"type":null,"properties":{"flag":{"type":"string"}},"required":"flag"},true,false],
  ["type/required [null, true]",{"type":null,"properties":{"flag":{"type":"string"}},"required":true},true,false],
  ["type/required [null, [\"_\"]]",{"type":null,"properties":{"flag":{"type":"string"}},"required":["_"]},true,false],
  ["type/required [[], null]",{"type":[],"properties":{"flag":{"type":"string"}}},true,false],
  ["type/required [[], []]",{"type":[],"properties":{"flag":{"type":"string"}},"required":[]},true,false],
  ["type/required [[], [\"flag\"]]",{"type":[],"properties":{"flag":{"type":"string"}},"required":["flag"]},false,false],
  ["type/required [[], [\"missing\"]]",{"type":[],"properties":{"flag":{"type":"string"}},"required":["missing"]},true,false],
  ["type/required [[], \"flag\"]",{"type":[],"properties":{"flag":{"type":"string"}},"required":"flag"},true,false],
  ["type/required [[], true]",{"type":[],"properties":{"flag":{"type":"string"}},"required":true},true,false],
  ["type/required [[], [\"_\"]]",{"type":[],"properties":{"flag":{"type":"string"}},"required":["_"]},true,false],
  ["type/required [[\"object\", \"null\"], null]",{"type":["object","null"],"properties":{"flag":{"type":"string"}}},true,false],
  ["type/required [[\"object\", \"null\"], []]",{"type":["object","null"],"properties":{"flag":{"type":"string"}},"required":[]},true,false],
  ["type/required [[\"object\", \"null\"], [\"flag\"]]",{"type":["object","null"],"properties":{"flag":{"type":"string"}},"required":["flag"]},false,false],
  ["type/required [[\"object\", \"null\"], [\"missing\"]]",{"type":["object","null"],"properties":{"flag":{"type":"string"}},"required":["missing"]},true,false],
  ["type/required [[\"object\", \"null\"], \"flag\"]",{"type":["object","null"],"properties":{"flag":{"type":"string"}},"required":"flag"},true,false],
  ["type/required [[\"object\", \"null\"], true]",{"type":["object","null"],"properties":{"flag":{"type":"string"}},"required":true},true,false],
  ["type/required [[\"object\", \"null\"], [\"_\"]]",{"type":["object","null"],"properties":{"flag":{"type":"string"}},"required":["_"]},true,false],
  ["type/required [[\"array\", \"object\"], null]",{"type":["array","object"],"properties":{"flag":{"type":"string"}}},true,false],
  ["type/required [[\"array\", \"object\"], []]",{"type":["array","object"],"properties":{"flag":{"type":"string"}},"required":[]},true,false],
  ["type/required [[\"array\", \"object\"], [\"flag\"]]",{"type":["array","object"],"properties":{"flag":{"type":"string"}},"required":["flag"]},false,false],
  ["type/required [[\"array\", \"object\"], [\"missing\"]]",{"type":["array","object"],"properties":{"flag":{"type":"string"}},"required":["missing"]},true,false],
  ["type/required [[\"array\", \"object\"], \"flag\"]",{"type":["array","object"],"properties":{"flag":{"type":"string"}},"required":"flag"},true,false],
  ["type/required [[\"array\", \"object\"], true]",{"type":["array","object"],"properties":{"flag":{"type":"string"}},"required":true},true,false],
  ["type/required [[\"array\", \"object\"], [\"_\"]]",{"type":["array","object"],"properties":{"flag":{"type":"string"}},"required":["_"]},true,false],
  ["type/required [[\"null\"], null]",{"type":["null"],"properties":{"flag":{"type":"string"}}},true,false],
  ["type/required [[\"null\"], []]",{"type":["null"],"properties":{"flag":{"type":"string"}},"required":[]},true,false],
  ["type/required [[\"null\"], [\"flag\"]]",{"type":["null"],"properties":{"flag":{"type":"string"}},"required":["flag"]},false,false],
  ["type/required [[\"null\"], [\"missing\"]]",{"type":["null"],"properties":{"flag":{"type":"string"}},"required":["missing"]},true,false],
  ["type/required [[\"null\"], \"flag\"]",{"type":["null"],"properties":{"flag":{"type":"string"}},"required":"flag"},true,false],
  ["type/required [[\"null\"], true]",{"type":["null"],"properties":{"flag":{"type":"string"}},"required":true},true,false],
  ["type/required [[\"null\"], [\"_\"]]",{"type":["null"],"properties":{"flag":{"type":"string"}},"required":["_"]},true,false],
  ["type/required [{\"x\": {}}, null]",{"type":{"x":{}},"properties":{"flag":{"type":"string"}}},true,false],
  ["type/required [{\"x\": {}}, []]",{"type":{"x":{}},"properties":{"flag":{"type":"string"}},"required":[]},true,false],
  ["type/required [{\"x\": {}}, [\"flag\"]]",{"type":{"x":{}},"properties":{"flag":{"type":"string"}},"required":["flag"]},false,false],
  ["type/required [{\"x\": {}}, [\"missing\"]]",{"type":{"x":{}},"properties":{"flag":{"type":"string"}},"required":["missing"]},true,false],
  ["type/required [{\"x\": {}}, \"flag\"]",{"type":{"x":{}},"properties":{"flag":{"type":"string"}},"required":"flag"},true,false],
  ["type/required [{\"x\": {}}, true]",{"type":{"x":{}},"properties":{"flag":{"type":"string"}},"required":true},true,false],
  ["type/required [{\"x\": {}}, [\"_\"]]",{"type":{"x":{}},"properties":{"flag":{"type":"string"}},"required":["_"]},true,false],
  ["properties {}",{"type":"object","properties":{}},false,true],
  ["properties {\"_\": {}}",{"type":"object","properties":{"_":{}}},true,false],
  ["properties {\"_\": null}",{"type":"object","properties":{"_":null}},true,false],
  ["properties {\"_\": false}",{"type":"object","properties":{"_":false}},true,false],
  ["properties {\"_\": true}",{"type":"object","properties":{"_":true}},true,false],
  ["properties {\"flag\": {\"required\": true}}",{"type":"object","properties":{"flag":{"required":true}}},false,false],
  ["properties {\"flag\": {\"required\": false}}",{"type":"object","properties":{"flag":{"required":false}}},true,false],
  ["properties {\"enum\": {}}",{"type":"object","properties":{"enum":{}}},false,true],
  ["properties {\"enum\": [\"x\"]}",{"type":"object","properties":{"enum":["x"]}},true,false],
  ["properties {\"$ref\": {}}",{"type":"object","properties":{"$ref":{}}},true,false],
  ["properties null",{"type":"object","properties":null},false,false],
  ["properties []",{"type":"object","properties":[]},false,false],
  ["properties true",{"type":"object","properties":true},false,false],
  ["anyOf [{}, []]",{"anyOf":[]},false,false],
  ["anyOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, []]",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":[]},false,false],
  ["anyOf [{}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}]]",{"anyOf":[{"type":"object","properties":{"flag":{"type":"string"}}}]},true,false],
  ["anyOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}]]",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":[{"type":"object","properties":{"flag":{"type":"string"}}}]},true,false],
  ["anyOf [{}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, {\"type\": \"null\"}]]",{"anyOf":[{"type":"object","properties":{"flag":{"type":"string"}}},{"type":"null"}]},true,false],
  ["anyOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, {\"type\": \"null\"}]]",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":[{"type":"object","properties":{"flag":{"type":"string"}}},{"type":"null"}]},true,false],
  ["anyOf [{}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, {\"type\": \"object\", \"properties\": {\"other\": {}}}]]",{"anyOf":[{"type":"object","properties":{"flag":{"type":"string"}}},{"type":"object","properties":{"other":{}}}]},false,false],
  ["anyOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, {\"type\": \"object\", \"properties\": {\"other\": {}}}]]",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":[{"type":"object","properties":{"flag":{"type":"string"}}},{"type":"object","properties":{"other":{}}}]},false,false],
  ["anyOf [{}, [\"bad\"]]",{"anyOf":["bad"]},false,false],
  ["anyOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [\"bad\"]]",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":["bad"]},true,false],
  ["anyOf [{}, [true]]",{"anyOf":[true]},false,false],
  ["anyOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [true]]",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":[true]},true,false],
  ["anyOf [{}, [{\"required\": [\"flag\"]}]]",{"anyOf":[{"required":["flag"]}]},false,false],
  ["anyOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [{\"required\": [\"flag\"]}]]",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":[{"required":["flag"]}]},true,false],
  ["anyOf [{}, [{\"type\": \"null\"}]]",{"anyOf":[{"type":"null"}]},false,false],
  ["anyOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [{\"type\": \"null\"}]]",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":[{"type":"null"}]},true,false],
  ["anyOf [{}, \"bad\"]",{"anyOf":"bad"},false,false],
  ["anyOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, \"bad\"]",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":"bad"},false,false],
  ["anyOf [{}, {\"properties\": {\"reason\": {}}}]",{"anyOf":{"properties":{"reason":{}}}},true,false],
  ["anyOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, {\"properties\": {\"reason\": {}}}]",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":{"properties":{"reason":{}}}},true,false],
  ["oneOf [{}, []]",{"oneOf":[]},false,false],
  ["oneOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, []]",{"type":"object","properties":{"flag":{"type":"string"}},"oneOf":[]},false,false],
  ["oneOf [{}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}]]",{"oneOf":[{"type":"object","properties":{"flag":{"type":"string"}}}]},true,false],
  ["oneOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}]]",{"type":"object","properties":{"flag":{"type":"string"}},"oneOf":[{"type":"object","properties":{"flag":{"type":"string"}}}]},true,false],
  ["oneOf [{}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, {\"type\": \"null\"}]]",{"oneOf":[{"type":"object","properties":{"flag":{"type":"string"}}},{"type":"null"}]},true,false],
  ["oneOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, {\"type\": \"null\"}]]",{"type":"object","properties":{"flag":{"type":"string"}},"oneOf":[{"type":"object","properties":{"flag":{"type":"string"}}},{"type":"null"}]},true,false],
  ["oneOf [{}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, {\"type\": \"object\", \"properties\": {\"other\": {}}}]]",{"oneOf":[{"type":"object","properties":{"flag":{"type":"string"}}},{"type":"object","properties":{"other":{}}}]},false,false],
  ["oneOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, {\"type\": \"object\", \"properties\": {\"other\": {}}}]]",{"type":"object","properties":{"flag":{"type":"string"}},"oneOf":[{"type":"object","properties":{"flag":{"type":"string"}}},{"type":"object","properties":{"other":{}}}]},false,false],
  ["oneOf [{}, [\"bad\"]]",{"oneOf":["bad"]},false,false],
  ["oneOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [\"bad\"]]",{"type":"object","properties":{"flag":{"type":"string"}},"oneOf":["bad"]},true,false],
  ["oneOf [{}, [true]]",{"oneOf":[true]},false,false],
  ["oneOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [true]]",{"type":"object","properties":{"flag":{"type":"string"}},"oneOf":[true]},true,false],
  ["oneOf [{}, [{\"required\": [\"flag\"]}]]",{"oneOf":[{"required":["flag"]}]},false,false],
  ["oneOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [{\"required\": [\"flag\"]}]]",{"type":"object","properties":{"flag":{"type":"string"}},"oneOf":[{"required":["flag"]}]},true,false],
  ["oneOf [{}, [{\"type\": \"null\"}]]",{"oneOf":[{"type":"null"}]},false,false],
  ["oneOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, [{\"type\": \"null\"}]]",{"type":"object","properties":{"flag":{"type":"string"}},"oneOf":[{"type":"null"}]},true,false],
  ["oneOf [{}, \"bad\"]",{"oneOf":"bad"},false,false],
  ["oneOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, \"bad\"]",{"type":"object","properties":{"flag":{"type":"string"}},"oneOf":"bad"},false,false],
  ["oneOf [{}, {\"properties\": {\"reason\": {}}}]",{"oneOf":{"properties":{"reason":{}}}},true,false],
  ["oneOf [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}, {\"properties\": {\"reason\": {}}}]",{"type":"object","properties":{"flag":{"type":"string"}},"oneOf":{"properties":{"reason":{}}}},true,false],
  ["structural {\"allOf\": [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}]}",{"allOf":[{"type":"object","properties":{"flag":{"type":"string"}}}]},true,false],
  ["structural {\"type\": \"object\", \"allOf\": [{\"properties\": {\"flag\": {}}}]}",{"type":"object","allOf":[{"properties":{"flag":{}}}]},true,false],
  ["structural {\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}, \"allOf\": [{\"required\": [\"flag\"]}]}",{"type":"object","properties":{"flag":{"type":"string"}},"allOf":[{"required":["flag"]}]},false,false],
  ["structural {\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}, \"allOf\": [{\"required\": [\"missing\"]}]}",{"type":"object","properties":{"flag":{"type":"string"}},"allOf":[{"required":["missing"]}]},true,false],
  ["structural {\"then\": {\"properties\": {\"flag\": {}}}}",{"then":{"properties":{"flag":{}}}},true,false],
  ["structural {\"else\": {\"properties\": {\"_\": {}}}}",{"else":{"properties":{"_":{}}}},true,false],
  ["structural {\"flag\": {\"type\": \"string\"}}",{"flag":{"type":"string"}},true,false],
  ["structural {\"flag\": {\"type\": \"string\", \"required\": true}}",{"flag":{"type":"string","required":true}},false,false],
  ["structural {\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}, \"$ref\": \"#/$defs/x\", \"$defs\": {\"x\": {\"required\": [\"flag\"]}}}",{"type":"object","properties":{"flag":{"type":"string"}},"$ref":"#/$defs/x","$defs":{"x":{"required":["flag"]}}},false,false],
  ["structural {\"$ref\": \"#/schema/$defs/x\", \"$defs\": {\"x\": {\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}}}",{"$ref":"#/schema/$defs/x","$defs":{"x":{"type":"object","properties":{"flag":{"type":"string"}}}}},false,false],
  ["structural {\"$ref\": \"#/schema/$defs/x\", \"$defs\": {\"x\": {\"$ref\": \"#/schema/$defs/x\", \"type\": \"object\", \"properties\": {\"flag\": {}}}}}",{"$ref":"#/schema/$defs/x","$defs":{"x":{"$ref":"#/schema/$defs/x","type":"object","properties":{"flag":{}}}}},false,false],
  ["structural {\"properties\": {\"$ref\": \"#/schema/$defs/x\"}, \"$defs\": {\"x\": {}}}",{"properties":{"$ref":"#/schema/$defs/x"},"$defs":{"x":{}}},true,false],
  ["structural {\"type\": [\"array\", \"object\"], \"properties\": {\"flag\": {}}, \"items\": {}}",{"type":["array","object"],"properties":{"flag":{}},"items":{}},true,false],
  ["structural {\"anyOf\": [{\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}}], \"oneOf\": [{\"type\": \"object\", \"properties\": {\"_\": {}}}]}",{"anyOf":[{"type":"object","properties":{"flag":{"type":"string"}}}],"oneOf":[{"type":"object","properties":{"_":{}}}]},true,false],
  ["structural {\"type\": \"object\", \"properties\": {\"flag\": {\"type\": \"string\"}}, \"anyOf\": [{\"required\": [\"flag\"]}]}",{"type":"object","properties":{"flag":{"type":"string"}},"anyOf":[{"required":["flag"]}]},true,false]
]"####).unwrap();
    for case in cases.as_array().unwrap() {
        for (key, index) in [("_", 2), ("reason", 3)] {
            assert_eq!(
                reclean::requires(&case[1], key),
                case[index].as_bool().unwrap(),
                "{}: {key}",
                case[0]
            );
        }
    }
}

// Not upstream's: isolate unusable refs on the two analyzer paths.
#[test]
fn unusable_refs_preserve_siblings_only_in_multi_branch_unions() {
    for reference in ["http://x", "#/$defs/missing", "#/self"] {
        let mut schema = json!({"$ref":reference,"properties":{"reason":{}},"required":"reason","additionalProperties":false,"allOf":[{"properties":{"flag":{}}}]});
        // Use an explicit visited set to isolate the already-visited case.
        let mut visited = BTreeSet::new();
        if reference == "#/self" {
            visited.insert(reference.to_owned());
        }
        let info = analyze_depth(&schema, &schema, 0, &mut visited.clone());
        assert!(info.complex);
        assert!(!info.properties.contains_key("reason"));
        schema["anyOf"] = json!([{"type":"object"},{"type":"string"}]);
        let info = analyze_depth(&schema, &schema, 0, &mut visited);
        assert!(info.properties.contains_key("reason"));
        assert!(info.properties.contains_key("flag"));
        assert!(info.required.contains("reason"));
        assert!(info.forbids);
    }
}

// Not upstream's: malformed unions remain complex but cannot enable translated cleaning.
#[test]
fn scalar_union_does_not_enable_translated_placeholder_cleaning() {
    let schema = json!({"type":"object","anyOf":true});
    assert!(analyze(&schema, &schema).complex);
    let original = json!({"tools":[{"name":"T","input_schema":schema}]});
    let sent = json!({"tools":[{"functionDeclarations":[{"name":"T","parametersJsonSchema":{"type":"object","properties":{"reason":{}},"required":["reason"]}}]}]});
    let args = json!({"reason":"keep"});
    assert_eq!(clean_arguments(&args, "T", &original, &sent), args);
}

// Not upstream's: deep declarations in opaque branches must protect client fields.
#[test]
fn deep_declarations_protect_unanalyzed_union_fields() {
    let schema = json!({"anyOf":[{"type":"object","additionalProperties":false},{"type":"object","additionalProperties":false,"unknown":true,"properties":{"reason":{}}}]});
    assert!(!analyze(&schema, &schema).properties.contains_key("reason"));
    assert!(declares_deep(
        &schema,
        &schema,
        "reason",
        0,
        &mut BTreeSet::new()
    ));
    let original = json!({"tools":[{"name":"T","input_schema":schema}]});
    let sent = json!({"tools":[{"functionDeclarations":[{"name":"T","parametersJsonSchema":{"type":"object","properties":{"reason":{}},"required":["reason"]}}]}]});
    let args = json!({"reason":"keep"});
    assert_eq!(clean_arguments(&args, "T", &original, &sent), args);
}

// Not upstream's: parsed schemas still retain property ordering for ambiguity checks.
#[test]
fn duplicate_schema_order_remains_ambiguous() {
    let original: Value = serde_json::from_str(r#"{"tools":[{"name":"T","input_schema":{"type":"object","properties":{}}},{"name":"T","input_schema":{"properties":{},"type":"object"}}]}"#).unwrap();
    assert!(original_schema(&original, "T").is_none());
}

// Not upstream's: collector precedence, recursion and raw names use the shared sanitizer.
#[test]
fn sent_names_use_raw_recursive_function_names() {
    let request = json!({"tools":[{"name":"ignored","function":{"name":" raw "}},{"tools":[{"functionDeclarations":[{"name":"a b"}],"function_declarations":[{"name":"a_b"}]}]}]});
    let mut names = Vec::new();
    for tool in request["tools"].as_array().unwrap() {
        collect_function_names(tool, &mut names);
    }
    assert_eq!(names, vec![" raw ", "a b", "a_b"]);
    let refs: Vec<_> = names.iter().map(String::as_str).collect();
    let expected = crate::responses_tools::sanitize_tool_names(&refs);
    for name in names {
        assert_eq!(
            sent_tool_name(&request, &name),
            expected.get(&name).cloned()
        );
    }
    assert!(sent_tool_name(&request, "raw").is_none());
}

// Not upstream's: no-schema fallback precedes argument type checking, except with translated schemas.
#[test]
fn no_schema_nonobjects_follow_translated_lookup_order() {
    let original = json!({"tools":[{"name":"T"}]});
    let sent = json!({"tools":[{"functionDeclarations":[{"name":"T","parametersJsonSchema":{"type":"object"}}]}]});
    for args in [json!(null), json!(7), json!([1]), json!("text")] {
        assert_eq!(
            clean_arguments(&args, "T", &original, &Value::Null),
            json!({})
        );
        assert_eq!(clean_arguments(&args, "T", &original, &sent), args);
    }
}

// Not upstream's: a tool with no name matches no call, even one whose name
// has the same canonical form, so the call isn't taken as parameterless and
// its arguments are kept.
#[test]
fn nameless_tools_match_no_call() {
    let sent = json!({"tools":[{"functionDeclarations":[{"name":" ","parametersJsonSchema":{"type":"object"}}]}]});
    for original in [
        json!({"tools":[{"name":"","input_schema":{"type":"object"}}]}),
        json!({"tools":[{"name":" "}]}),
        json!({"tools":[1]}),
    ] {
        for name in ["_", "____"] {
            assert!(!parameterless(&original, name));
            for args in [json!({"city":"Paris"}), json!(true), json!("a string")] {
                assert_eq!(clean_arguments(&args, name, &original, &Value::Null), args);
                assert_eq!(clean_arguments(&args, name, &original, &sent), args);
            }
        }
    }
    // A nameless declaration sent upstream isn't the call's either, so the
    // declared tool's missing schema still empties the arguments.
    let original = json!({"tools":[{"name":"_"}]});
    assert_eq!(clean_arguments(&json!(7), "_", &original, &sent), json!({}));
    let named = json!({"tools":[{"functionDeclarations":[{"name":"_","parametersJsonSchema":{"type":"object"}}]}]});
    assert_eq!(clean_arguments(&json!(7), "_", &original, &named), json!(7));
}

// Not upstream's: a call whose canonical name is empty keeps its name, even
// when one declared tool has the same empty canonical name.
#[test]
fn empty_canonical_names_are_not_restored() {
    let request = json!({"tools":[{"name":""},{"name":"__"}]});
    assert_eq!(legacy_client_name(&request, "_"), "_");
    assert_eq!(legacy_client_name(&request, "__"), "__");
    assert_eq!(legacy_client_name(&request, ""), "");
    let request = json!({"tools":[{"name":"Read"}]});
    assert_eq!(legacy_client_name(&request, "_read"), "Read");
}

// Not upstream's: an unbounded projection must conservatively retain arguments.
#[test]
fn reclean_expansion_bound_preserves_arguments() {
    let mut schema = json!({"type":"object","properties":{"flag":{}}});
    for _ in 0..70 {
        schema = json!({"allOf":[schema]});
    }
    assert!(!reclean::requires(&schema, "_"));
}

// Not upstream's: compact-text deduplication keeps first-seen identity and key order.
#[test]
fn cached_schema_text_keeps_first_seen_order() {
    let first = json!({"type":"object","properties":{}});
    let duplicate = first.clone();
    let reordered: Value = serde_json::from_str(r#"{"properties":{},"type":"object"}"#).unwrap();
    let mut schemas = Schemas::default();
    for schema in [&first, &duplicate, &reordered, &first] {
        unique(&mut schemas, schema);
    }
    assert_eq!(schemas.serialized.len(), 2);
    assert_eq!(schemas.len(), 2);
    assert!(std::ptr::eq(schemas[0], &first));
    assert!(std::ptr::eq(schemas[1], &reordered));
}

// Not upstream's: a large literal must exhaust the projection node budget
// even when reference inlining does not expand anything.
#[test]
fn reclean_node_budget_preserves_arguments() {
    let schema = json!({"type":"object","properties":{"p":{"type":"string"}},
        "default":vec![Value::Null; 100_001]});
    assert!(!reclean::requires(&schema, "_"));
    let original = json!({"tools":[{"name":"T","input_schema":{"type":"object","properties":{"p":{"type":"string"}},"additionalProperties":false}}]});
    let sent =
        json!({"tools":[{"functionDeclarations":[{"name":"T","parametersJsonSchema":schema}]}]});
    let args = json!({"p":"x","_":true});
    assert_eq!(clean_arguments(&args, "T", &original, &sent), args);
}

// Not upstream's: literal results from SanitizedFunctionNameMap in upstream
// v8.0.32, run with Go 1.26.4 and GOTOOLCHAIN=local in a copied review harness.
#[test]
fn sent_name_hash_suffixes_match_go() {
    let long = "a".repeat(80);
    let other_long = format!("{long}b");
    let request =
        json!({"tools":[{"name":long},{"name":other_long},{"name":"a b"},{"name":"a_b"}]});
    for (name, expected) in [
        (
            long.as_str(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_50fe35dfe06a",
        ),
        (
            other_long.as_str(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_9d18499d2077",
        ),
        ("a b", "a_b_eebd4769fc73"),
        ("a_b", "a_b_3fab944de3f4"),
    ] {
        assert_eq!(sent_tool_name(&request, name).as_deref(), Some(expected));
    }
}

// Not upstream's: manual whole-clean-call release timings for the review fixture.
// Run with cargo test -p open-ferry-translate --release schema_dedup_release_timings
// -- --ignored --nocapture. Request construction and assertion are outside timing.
#[test]
#[ignore = "manual release benchmark; no timing assertion"]
fn schema_dedup_release_timings() {
    for n in [1_000, 3_000, 6_000] {
        let branches: Vec<_> = (0..n).map(|i| json!({"type":"object","properties":{"p":{"type":"string","description":format!("d{i}")}},"additionalProperties":false})).collect();
        let schema = json!({"type":"object","anyOf":branches});
        let bytes = schema.to_string().len();
        let original = json!({"tools":[{"name":"T","input_schema":schema}]});
        let sent = json!({"tools":[{"functionDeclarations":[{"name":"T","parametersJsonSchema":{"type":"object","properties":{"p":{"type":"string"},"_":{"type":"boolean"}},"required":["_"]}}]}]});
        let args = json!({"p":"x","_":true});
        for run in 1..=3 {
            let start = std::time::Instant::now();
            let result = std::hint::black_box(clean_arguments(
                std::hint::black_box(&args),
                "T",
                std::hint::black_box(&original),
                std::hint::black_box(&sent),
            ));
            let elapsed = start.elapsed();
            assert_eq!(result, json!({"p":"x"}));
            println!(
                "N={n} bytes={bytes} run={run} elapsed_ms={:.3}",
                elapsed.as_secs_f64() * 1000.0
            );
        }
    }
}

#[test]
fn reclean_resolves_refs_against_inner_schema_and_preserves_literal_data() {
    let schema = json!({"$ref":"#/$defs/T","$defs":{"T":{"type":"object",
        "properties":{"flag":{"type":"string"}},"additionalProperties":false}}});
    assert!(reclean::requires(&schema, "_"));
    let schema = json!({"type":"object","properties":{"flag":{"type":"string"}},
        "default":{"properties":{"_":{}},"required":["_"]},"additionalProperties":false});
    assert!(reclean::requires(&schema, "_"));
    let schema = json!({"type":"object","properties":{"$ref":{"type":"string"}},
        "required":["$ref"],"additionalProperties":false});
    assert!(!reclean::requires(&schema, "_"));
}

#[test]
fn cached_name_policy_matches_legacy_scanning() {
    use crate::common::tool_names::{map_tool_name, unambiguous_tool_name_map_from_claude_request};
    let names = [
        "", "Read", "read", "READ", "_Read", "__read", " Read ", "Write", "write", "读取", "___",
        "missing", "say\"hi", "a\\b",
    ];
    let mut requests = vec![
        Value::Null,
        json!({}),
        json!({"tools":null}),
        json!({"tools":{}}),
        json!({"tools":["x", {"name":5}, {"name":null,"function":{"name":"Read"}}, {"name":"Read","function":{"name":"read"}}]}),
        json!({"tools":[{"name":"Read"},{"name":"read"},{"name":"_Read"}]}),
        json!({"tools":[{"name":"Read"},{"name":"Read"}]}),
        json!({"tools":[{"name":" ","function":{"name":"Read"}},{"function":{"name":"__Write"}},{"name":"___"}]}),
    ];
    // Deterministic randomized fixtures, as in upstream's differential test.
    let mut seed = 6816_u64;
    for _ in 0..500 {
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 32) as usize
        };
        let count = next() % 12;
        let tools: Vec<_> = (0..count)
            .map(|_| {
                let name = names[next() % names.len()];
                if next() % 2 == 0 {
                    json!({"name":name})
                } else {
                    json!({"function":{"name":name}})
                }
            })
            .collect();
        requests.push(json!({"tools":tools}));
    }
    for request in requests {
        let cached = unambiguous_tool_name_map_from_claude_request(&request);
        for name in names {
            assert_eq!(
                map_tool_name(cached.as_ref(), name),
                legacy_client_name(&request, name),
                "request={request}, name={name:?}"
            );
        }
    }
}

#[test]
fn implicit_open_objects_preserve_arguments() {
    let cases = [
        (
            "additionalProperties_omitted",
            r##"{"type":"object","properties":{}}"##,
            r##"{"user":"v"}"##,
            r##"{"user":"v"}"##,
            false,
        ),
        (
            "additionalProperties_omitted_no_properties",
            r##"{"type":"object"}"##,
            r##"{"user":"v"}"##,
            r##"{"user":"v"}"##,
            false,
        ),
        (
            "additionalProperties_true",
            r##"{"type":"object","properties":{},"additionalProperties":true}"##,
            r##"{"user":"v"}"##,
            r##"{"user":"v"}"##,
            false,
        ),
        (
            "additionalProperties_false",
            r##"{"type":"object","properties":{},"additionalProperties":false}"##,
            r##"{"user":"v"}"##,
            r##"{}"##,
            true,
        ),
        (
            "additionalProperties_false_no_properties",
            r##"{"type":"object","additionalProperties":false}"##,
            r##"{"user":"v"}"##,
            r##"{}"##,
            true,
        ),
        (
            "nested_implicit_open_preserves_user_arg",
            r##"{"type":"object","properties":{"opts":{"type":"object","properties":{}}}}"##,
            r##"{"opts":{"user":"v"}}"##,
            r##"{"opts":{"user":"v"}}"##,
            false,
        ),
        (
            "nested_explicit_closed_erases_user_arg",
            r##"{"type":"object","properties":{"opts":{"type":"object","properties":{},"additionalProperties":false}}}"##,
            r##"{"opts":{"user":"v"}}"##,
            r##"{"opts":{}}"##,
            false,
        ),
        (
            "array_item_implicit_open_preserves_user_arg",
            r##"{"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{}}}}}"##,
            r##"{"items":[{"user":"v"}]}"##,
            r##"{"items":[{"user":"v"}]}"##,
            false,
        ),
        (
            "array_item_explicit_closed_erases_user_arg",
            r##"{"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{},"additionalProperties":false}}}}"##,
            r##"{"items":[{"user":"v"}]}"##,
            r##"{"items":[{}]}"##,
            false,
        ),
        (
            "allOf_implicit_open_preserves_user_arg",
            r##"{"type":"object","allOf":[{"type":"object","properties":{}}]}"##,
            r##"{"user":"v"}"##,
            r##"{"user":"v"}"##,
            false,
        ),
        (
            "allOf_explicit_closed_erases_user_arg",
            r##"{"type":"object","allOf":[{"type":"object","properties":{},"additionalProperties":false}]}"##,
            r##"{"user":"v"}"##,
            r##"{}"##,
            true,
        ),
        (
            "local_ref_implicit_open_preserves_user_arg",
            r##"{"$ref":"#/$defs/open","$defs":{"open":{"type":"object","properties":{}}}}"##,
            r##"{"user":"v"}"##,
            r##"{"user":"v"}"##,
            false,
        ),
        (
            "local_ref_explicit_closed_erases_user_arg",
            r##"{"$ref":"#/$defs/closed","$defs":{"closed":{"type":"object","properties":{},"additionalProperties":false}}}"##,
            r##"{"user":"v"}"##,
            r##"{}"##,
            true,
        ),
    ];
    for (name, schema, args, want, no_args) in cases {
        let schema: Value = serde_json::from_str(schema).unwrap();
        let original = json!({"tools":[{"name":"echo","input_schema":schema}]});
        let cleaned = crate::gemini_schema::clean_json_schema_for_gemini_json_schema(
            &schema,
            &mut crate::gemini_schema::SchemaCopyBudget::default(),
        );
        let sent = json!({"tools":[{"functionDeclarations":[{"name":"echo","parametersJsonSchema":cleaned}]}]});
        let args = serde_json::from_str(args).unwrap();
        let want: Value = serde_json::from_str(want).unwrap();
        for translated in [&Value::Null, &sent] {
            assert_eq!(
                clean_arguments(&args, "echo", &original, translated),
                want,
                "{name}"
            );
        }
        assert_eq!(parameterless(&original, "echo"), no_args, "{name}");
    }
}

#[test]
fn open_objects_remove_only_proven_translated_placeholders() {
    let original =
        json!({"tools":[{"name":"echo","input_schema":{"type":"object","properties":{}}}]});
    let args = json!({"user":"v","reason":"Brief explanation"});
    let injected = json!({"tools":[{"functionDeclarations":[{"name":"echo","parametersJsonSchema":{
        "type":"object","properties":{"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"}},"required":["reason"]}}]}]});
    assert_eq!(
        clean_arguments(&args, "echo", &original, &Value::Null),
        json!({"user":"v"})
    );
    assert_eq!(
        clean_arguments(&args, "echo", &original, &injected),
        json!({"user":"v"})
    );
    let unproven = json!({"tools":[{"functionDeclarations":[{"name":"echo","parametersJsonSchema":{
        "type":"object","properties":{"reason":{"type":"string"}}}}]}]});
    assert_eq!(clean_arguments(&args, "echo", &original, &unproven), args);
}

#[test]
fn implicit_open_union_and_reclean_paths_preserve_unproven_placeholders() {
    let original = json!({"tools":[{"name":"echo","input_schema":{
    "type":"object","properties":{"cfg":{"anyOf":[
        {"type":"object","properties":{}},
        {"type":"object","properties":{"mode":{"type":"string"}},"required":["mode"],"additionalProperties":false}
    ]}}}}]});
    let sent = json!({"tools":[{"functionDeclarations":[{"name":"echo","parametersJsonSchema":{
    "type":"object","properties":{"cfg":{"anyOf":[
        {"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]},
        {"type":"object","properties":{"mode":{"type":"string"}},"required":["mode"]}
    ]}}}}]}]});
    let args = json!({"cfg":{"user":"v","reason":"business","_":true}});
    assert_eq!(clean_arguments(&args, "echo", &original, &sent), args);
    for closed in [false, true] {
        let mut schema = json!({"type":"object","properties":{"flag":{"type":"string"}}});
        if closed {
            schema["additionalProperties"] = json!(false);
        }
        let original = json!({"tools":[{"name":"echo","input_schema":schema}]});
        let sent = json!({"tools":[{"functionDeclarations":[{"name":"echo","parametersJsonSchema":{
            "type":"object","properties":{"flag":{"type":"string"}}}}]}]});
        let args = json!({"flag":"x","_":true,"user":"v"});
        let expected = if closed {
            json!({"flag":"x","user":"v"})
        } else {
            args.clone()
        };
        assert_eq!(clean_arguments(&args, "echo", &original, &sent), expected);
    }
}

#[test]
fn open_schema_regressions_match_uncached_analysis() {
    for uncached in [false, true] {
        let check = || {
            implicit_open_objects_preserve_arguments();
            open_objects_remove_only_proven_translated_placeholders();
            implicit_open_union_and_reclean_paths_preserve_unproven_placeholders();
        };
        if uncached {
            super::analysis_memo_tests::with_legacy(check);
        } else {
            check();
        }
    }

    let mut shared_reference_hits = 0;
    for additional_properties in [None, Some(true), Some(false)] {
        let closed = additional_properties == Some(false);
        let mut definition = json!({"type":"object","properties":{}});
        if let Some(value) = additional_properties {
            definition["additionalProperties"] = json!(value);
        }
        for keyword in ["allOf", "anyOf"] {
            let mut repeated = json!({});
            repeated[keyword] = json!([
                {"$ref":"#/$defs/Empty"},
                {"$ref":"#/$defs/Empty","description":"second reference"}
            ]);
            for location in ["root", "property", "array"] {
                let mut schema = match location {
                    "root" => repeated.clone(),
                    "property" => json!({"type":"object","properties":{"value":repeated}}),
                    "array" => json!({"type":"object","properties":{
                        "value":{"type":"array","items":repeated}
                    }}),
                    _ => unreachable!(),
                };
                schema["$defs"] = json!({"Empty":definition});
                let context = format!("{additional_properties:?}, {keyword}, {location}");
                let mut memo = AnalysisMemo::default();
                let actual = analyze_depth_memo(
                    &repeated,
                    &schema,
                    0,
                    &mut BTreeSet::new(),
                    &mut memo,
                    &mut AnalysisReferences::default(),
                );
                let expected = super::analysis_memo_tests::legacy_analyze_depth(
                    &repeated,
                    &schema,
                    0,
                    &mut BTreeSet::new(),
                );
                assert!(memo.cache_hits > 0, "{context}");
                assert_eq!(
                    super::analysis_memo_tests::snapshot(&actual),
                    super::analysis_memo_tests::snapshot(&expected),
                    "{context}"
                );

                let previous_hits = memo.cache_hits;
                let hit = memo.resolve(
                    &schema["$defs"]["Empty"],
                    &schema,
                    2,
                    &mut BTreeSet::new(),
                    &mut AnalysisReferences::default(),
                );
                assert_eq!(memo.cache_hits, previous_hits + 1, "{context}");
                assert_eq!(hit.forbids, closed, "{context}");
                assert_eq!(hit.parameterless(), closed, "{context}");
                shared_reference_hits += memo.cache_hits;

                let original = json!({"tools":[{"name":"echo","input_schema":schema}]});
                let no_args = parameterless(&original, "echo");
                assert_eq!(no_args, location == "root" && keyword == "allOf" && closed);
                assert_eq!(
                    no_args,
                    super::analysis_memo_tests::with_legacy(|| parameterless(&original, "echo")),
                    "{context}"
                );
                let cleaned = crate::gemini_schema::clean_json_schema_for_gemini_json_schema(
                    &schema,
                    &mut crate::gemini_schema::SchemaCopyBudget::default(),
                );
                let sent = json!({"tools":[{"functionDeclarations":[{
                    "name":"echo","parametersJsonSchema":cleaned
                }]}]});
                let args = match location {
                    "root" => json!({"user":"v"}),
                    "property" => json!({"value":{"user":"v"}}),
                    "array" => json!({"value":[{"user":"v"}]}),
                    _ => unreachable!(),
                };
                for translated in [&Value::Null, &sent] {
                    let actual = clean_arguments(&args, "echo", &original, translated);
                    let expected = super::analysis_memo_tests::with_legacy(|| {
                        clean_arguments(&args, "echo", &original, translated)
                    });
                    assert_eq!(actual, expected, "{context}");
                    if keyword == "allOf" && translated.is_null() {
                        let expected = match (closed, location) {
                            (true, "root") => json!({}),
                            (true, "property") => json!({"value":{}}),
                            (true, "array") => json!({"value":[{}]}),
                            _ => args.clone(),
                        };
                        assert_eq!(actual, expected, "{context}");
                    }
                }
            }
        }
    }
    eprintln!("shared-reference analysis cache hits: {shared_reference_hits}");
}
