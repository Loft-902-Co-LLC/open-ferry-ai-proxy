// The codex family's entries in the Interactions harness (see main.go),
// added to translators from init(). All run tr/codex/interactions, for
// Interactions clients to a Codex upstream:
//   - codex/interactions/request: the client's request for Codex
//     (ConvertInteractionsRequestToCodex), with the "stream" option as the
//     stream flag;
//   - codex/interactions/response: Codex's event stream for the client,
//     one event at a time, its chunks joined
//     (ConvertCodexResponseToInteractions);
//   - codex/interactions/response-non-stream: Codex's final event for the
//     client (ConvertCodexResponseToInteractionsNonStream).
package main

import (
	"context"

	codexinteractions "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/interactions"
)

func init() {
	translators["codex/interactions/request"] = func(in input) []byte {
		return codexinteractions.ConvertInteractionsRequestToCodex(in.Model, []byte(in.Request), streamOption(in))
	}
	translators["codex/interactions/response"] = func(in input) []byte {
		return joined(streamChunks(in, codexinteractions.ConvertCodexResponseToInteractions))
	}
	translators["codex/interactions/response-non-stream"] = func(in input) []byte {
		return codexinteractions.ConvertCodexResponseToInteractionsNonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	}
}
