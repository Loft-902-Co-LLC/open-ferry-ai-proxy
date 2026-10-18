// Ported from CLIProxyAPI internal/util/sanitize_test.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;

// Ported from TestCleanClaudeToolArguments. Antigravity executor-only
// post-translation injection is excluded because that executor isn't ported.
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
