// The chat family's entries in the Interactions harness (see main.go),
// added to translators from init(), with their Rust side in
// tools/parity/src/interactions/chat.rs. All run
// tr/openai/interactions/chat-completions:
//
//   - interactions/openai-chat/request, response and response-non-stream,
//     for Chat Completions clients to an Interactions upstream;
//   - openai/interactions/request, response and response-non-stream, for
//     Interactions clients to a Chat Completions upstream.
//
// A request entry takes the "stream" option as the translator's stream
// argument. The Interactions to Chat Completions stream is written as a
// JSON array of its chunks, and the Chat Completions to Interactions one as
// its SSE frames joined.
package main

import (
	"context"

	chat "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/openai/interactions/chat-completions"
)

func init() {
	translators["interactions/openai-chat/request"] = func(in input) []byte {
		return chat.ConvertOpenAIRequestToInteractions(in.Model, []byte(in.Request), streamOption(in))
	}
	translators["interactions/openai-chat/response"] = func(in input) []byte {
		return chunkList(streamChunks(in, chat.ConvertInteractionsResponseToOpenAI))
	}
	translators["interactions/openai-chat/response-non-stream"] = func(in input) []byte {
		return chat.ConvertInteractionsResponseToOpenAINonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	}
	translators["openai/interactions/request"] = func(in input) []byte {
		return chat.ConvertInteractionsRequestToOpenAI(in.Model, []byte(in.Request), streamOption(in))
	}
	translators["openai/interactions/response"] = func(in input) []byte {
		return joined(streamChunks(in, chat.ConvertOpenAIResponseToInteractions))
	}
	translators["openai/interactions/response-non-stream"] = func(in input) []byte {
		return chat.ConvertOpenAIResponseToInteractionsNonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	}
}
