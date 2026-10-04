// The responses family's response entries in the Interactions harness (see
// main.go), added to translators from init(). They run
// tr/openai/interactions/responses:
//
// interactions/openai-responses/response takes Interactions event lines, as
// upstream's Gemini executor passes them, for an OpenAI Responses client,
// and concatenates every chunk the translator gives; response-non-stream
// takes a whole Interactions response as its one event. "request" is the
// client's original request and "translated_request", if given, the request
// as sent upstream. interactions/openai-responses/tool-input-error runs the
// same stream, then FinalizeToolInput, and writes {"events": the stream's
// chunks concatenated, "finalize": what FinalizeToolInput gave,
// concatenated, "failed": whether ToolInputError is set}.
//
// openai-responses/interactions/response takes OpenAI Responses event lines
// for an Interactions client and concatenates every chunk the translator
// gives; response-non-stream takes a whole Responses response as its one
// event.
//
// The request entries (interactions/openai-responses/request and
// openai-responses/interactions/request) are WP4-C1's, in a file of their
// own.
package main

import (
	"bytes"
	"context"

	responses "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/openai/interactions/responses"
)

func init() {
	translators["interactions/openai-responses/response"] = func(in input) []byte {
		return joined(streamChunks(in, responses.ConvertInteractionsResponseToOpenAIResponses))
	}
	translators["interactions/openai-responses/response-non-stream"] = func(in input) []byte {
		return responses.ConvertInteractionsResponseToOpenAIResponsesNonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	}
	translators["interactions/openai-responses/tool-input-error"] = toolInputError
	translators["openai-responses/interactions/response"] = func(in input) []byte {
		return joined(streamChunks(in, responses.ConvertOpenAIResponsesResponseToInteractions))
	}
	translators["openai-responses/interactions/response-non-stream"] = func(in input) []byte {
		return responses.ConvertOpenAIResponsesResponseToInteractionsNonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	}
}

// toolInputError is interactions/openai-responses/tool-input-error. The
// stream's state type is unexported; its FinalizeToolInput and
// ToolInputError methods aren't.
func toolInputError(in input) []byte {
	var param any
	var events, finalize bytes.Buffer
	for _, event := range in.Events {
		for _, chunk := range responses.ConvertInteractionsResponseToOpenAIResponses(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), []byte(event), &param) {
			events.Write(chunk)
		}
	}
	if state, ok := param.(interface{ FinalizeToolInput() [][]byte }); ok {
		for _, chunk := range state.FinalizeToolInput() {
			finalize.Write(chunk)
		}
	}
	failed := false
	if state, ok := param.(interface{ ToolInputError() error }); ok {
		failed = state.ToolInputError() != nil
	}
	return marshal(struct {
		Events   string `json:"events"`
		Finalize string `json:"finalize"`
		Failed   bool   `json:"failed"`
	}{events.String(), finalize.String(), failed})
}
