// The gemini family's entries in the Interactions harness (see main.go),
// added to translators from init(), with their Rust side in
// tools/parity/src/interactions/gemini.rs. All run
// internal/translator/gemini/interactions:
//
//   - gemini/interactions/request, response and response-non-stream:
//     Interactions clients to a Gemini upstream. The request entry's
//     "stream" option is the stream flag. The stream entry writes its SSE
//     frames joined; the non-streaming entry reads the first event as the
//     Gemini response.
//   - interactions/gemini/request, response and response-non-stream: Gemini
//     clients to an Interactions upstream. The stream entry writes a JSON
//     array of its chunks.
//   - interactions/interactions/request, response and response-non-stream:
//     Interactions passed through. The stream entry writes a JSON array of
//     its chunks.
package main

import (
	"context"

	geminiinteractions "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/gemini/interactions"
)

func init() {
	translators["gemini/interactions/request"] = func(in input) []byte {
		return refused(geminiinteractions.ConvertInteractionsRequestToGemini(in.Model, []byte(in.Request), streamOption(in)))
	}
	translators["gemini/interactions/response"] = func(in input) []byte {
		return joined(streamChunks(in, geminiinteractions.ConvertGeminiResponseToInteractions))
	}
	translators["gemini/interactions/response-non-stream"] = func(in input) []byte {
		return geminiinteractions.ConvertGeminiResponseToInteractionsNonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	}
	translators["interactions/gemini/request"] = func(in input) []byte {
		return refused(geminiinteractions.ConvertGeminiRequestToInteractions(in.Model, []byte(in.Request), streamOption(in)))
	}
	translators["interactions/gemini/response"] = func(in input) []byte {
		return chunkList(streamChunks(in, geminiinteractions.ConvertInteractionsResponseToGemini))
	}
	translators["interactions/gemini/response-non-stream"] = func(in input) []byte {
		return geminiinteractions.ConvertInteractionsResponseToGeminiNonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	}
	translators["interactions/interactions/request"] = func(in input) []byte {
		return refused(geminiinteractions.ConvertInteractionsRequestToInteractions(in.Model, []byte(in.Request), streamOption(in)))
	}
	translators["interactions/interactions/response"] = func(in input) []byte {
		return chunkList(streamChunks(in, geminiinteractions.ConvertInteractionsResponsePassthrough))
	}
	translators["interactions/interactions/response-non-stream"] = func(in input) []byte {
		return geminiinteractions.ConvertInteractionsResponsePassthroughNonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	}
}
