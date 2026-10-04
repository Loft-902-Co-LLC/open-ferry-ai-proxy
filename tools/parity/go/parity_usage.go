// The usage/parse entry will run one of upstream's
// helps.Parse{Codex,OpenAI,OpenAIStream,Claude,ClaudeStream,Gemini,GeminiStream}Usage
// (internal/runtime/executor/helps/usage_helpers.go) on a response body
// or stream line, and give the usage detail as JSON. Not ported yet (P3
// WP-C): it gives null, and no case is generated.
package main

func init() {
	translators["usage/parse"] = usageParse
}

func usageParse(in input) []byte {
	return []byte("null")
}
