// The ttft/token-event entry runs one of upstream's helps.Is*TokenEvent
// (internal/runtime/executor/helps/*_ttft_helpers.go) on an upstream's
// stream event, "request", and writes whether it carries the first token:
// true or false. "options" names the protocol (see ttftOptions):
// "responses" (IsResponsesTokenEvent), "chat" (IsChatTokenEvent), "claude"
// (IsClaudeTokenEvent) or "gemini" (IsGeminiTokenEvent).
//
// It is in go/helps/main.go's harness, not go/main.go's, because importing
// helps registers a translator the registry/* entries must not see (see
// go/helps/main.go).
package main

import (
	"fmt"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
)

func init() {
	translators["ttft/token-event"] = ttftTokenEvent
}

type ttftOptions struct {
	Format string `json:"format"`
}

func ttftTokenEvent(in input) []byte {
	var options ttftOptions
	decodeOptions(in, &options)
	payload := []byte(in.Request)
	var token bool
	switch options.Format {
	case "responses":
		token = helps.IsResponsesTokenEvent(payload)
	case "chat":
		token = helps.IsChatTokenEvent(payload)
	case "claude":
		token = helps.IsClaudeTokenEvent(payload)
	case "gemini":
		token = helps.IsGeminiTokenEvent(payload)
	default:
		panic(fmt.Sprintf("ttft/token-event: unknown format %q", options.Format))
	}
	if token {
		return []byte("true")
	}
	return []byte("false")
}
