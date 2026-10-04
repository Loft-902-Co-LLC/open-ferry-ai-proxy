// The ttft/token-event entry runs one of upstream's helps.Is*TokenEvent
// (internal/runtime/executor/helps/*_ttft_helpers.go) on an upstream's
// stream event, "request", and writes whether it carries the first token:
// true or false. "options" names the protocol (see ttftOptions):
// "responses" (IsResponsesTokenEvent), "chat" (IsChatTokenEvent), "claude"
// (IsClaudeTokenEvent) or "gemini" (IsGeminiTokenEvent).
//
// The entry imports helps, and keeps to this file so it can move with it.

package main

import (
	"encoding/json"
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
	if err := json.Unmarshal(in.Options, &options); err != nil {
		panic(fmt.Sprintf("ttft/token-event options: %v", err))
	}
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
