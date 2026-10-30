// Re-cleans the emitted tool schema through the same Claude-to-Gemini
// request translator. Both complete outputs remain in the comparison.
package main

import "encoding/json"

func repeatGeminiSchema(in input, convert func(string, []byte, bool) ([]byte, error)) []byte {
	var options struct {
		RepeatClean bool `json:"repeat_clean"`
	}
	decodeOptions(in, &options)
	first, err := convert(in.Model, []byte(in.Request), streamOption(in))
	if !options.RepeatClean {
		return refused(first, err)
	}
	if err != nil {
		panic("schema re-clean fixture was refused")
	}
	var output map[string]json.RawMessage
	mustJSON(first, &output)
	var tools []map[string]json.RawMessage
	mustJSON(output["tools"], &tools)
	var declarations []map[string]json.RawMessage
	mustJSON(tools[0]["functionDeclarations"], &declarations)
	schema := declarations[0]["parametersJsonSchema"]
	if len(schema) == 0 {
		panic("schema re-clean fixture produced no schema")
	}
	var request map[string]json.RawMessage
	mustJSON([]byte(in.Request), &request)
	tools = nil
	mustJSON(request["tools"], &tools)
	tools[0]["input_schema"] = schema
	request["tools"] = marshal(tools)
	second, err := convert(in.Model, marshal(request), streamOption(in))
	return marshal(map[string]json.RawMessage{"first": first, "second": refused(second, err)})
}

func mustJSON(raw []byte, target interface{}) {
	if err := json.Unmarshal(raw, target); err != nil {
		panic(err)
	}
}
