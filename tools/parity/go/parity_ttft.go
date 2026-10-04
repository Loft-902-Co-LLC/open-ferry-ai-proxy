// The ttft/token-event entry will run one of upstream's
// helps.Is{Chat,Claude,Gemini,Responses}TokenEvent
// (internal/runtime/executor/helps/*_ttft_helpers.go) on a stream event,
// and give whether it carries the first token. Not ported yet (P3 WP-C):
// it gives null, and no case is generated.
package main

func init() {
	translators["ttft/token-event"] = ttftTokenEvent
}

func ttftTokenEvent(in input) []byte {
	return []byte("null")
}
