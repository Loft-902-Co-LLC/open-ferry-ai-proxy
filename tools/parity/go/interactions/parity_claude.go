// The claude family's entries in the Interactions harness (see main.go),
// added to translators from init(), with their Rust side in
// tools/parity/src/interactions/claude.rs.
//
// interactions/claude/request, request-compat, response and
// response-non-stream run tr/interactions/claude, for Claude clients to an
// Interactions upstream; claude/interactions/request, response and
// response-non-stream run tr/claude/interactions, for Interactions clients
// to a Claude upstream. The request entries take the "stream" option. The
// response entries write each chunk as an entry of a JSON array, and the
// non-streaming ones take the final event as the whole response.
package main

import (
	"context"

	claudeinteractions "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/claude/interactions"
	interactionsclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/interactions/claude"
)

func init() {
	translators["interactions/claude/request"] = func(in input) []byte {
		return interactionsclaude.ConvertClaudeRequestToInteractions(in.Model, []byte(in.Request), streamOption(in))
	}
	translators["interactions/claude/request-compat"] = func(in input) []byte {
		return interactionsclaude.ConvertClaudeRequestToInteractionsWithCompat(in.Model, []byte(in.Request), streamOption(in))
	}
	translators["interactions/claude/response"] = func(in input) []byte {
		return chunkList(streamChunks(in, interactionsclaude.ConvertInteractionsResponseToClaude))
	}
	translators["interactions/claude/response-non-stream"] = func(in input) []byte {
		return interactionsclaude.ConvertInteractionsResponseToClaudeNonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	}
	translators["claude/interactions/request"] = func(in input) []byte {
		return claudeinteractions.ConvertInteractionsRequestToClaude(in.Model, []byte(in.Request), streamOption(in))
	}
	translators["claude/interactions/response"] = func(in input) []byte {
		return chunkList(streamChunks(in, claudeinteractions.ConvertClaudeResponseToInteractions))
	}
	translators["claude/interactions/response-non-stream"] = func(in input) []byte {
		return claudeinteractions.ConvertClaudeResponseToInteractionsNonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	}
}
