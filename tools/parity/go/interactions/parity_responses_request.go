// The responses family's request entries in the Interactions harness (see
// main.go and parity_responses.go), added to translators from init(). Both
// run tr/openai/interactions/responses, with the "stream" option as the
// stream flag:
//   - interactions/openai-responses/request: an OpenAI Responses client's
//     request for an Interactions upstream
//     (ConvertOpenAIResponsesRequestToInteractions);
//   - openai-responses/interactions/request: an Interactions client's
//     request for a Responses upstream
//     (ConvertInteractionsRequestToOpenAIResponses).
package main

import (
	responsesinteractions "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/openai/interactions/responses"
)

func init() {
	translators["interactions/openai-responses/request"] = func(in input) []byte {
		return responsesinteractions.ConvertOpenAIResponsesRequestToInteractions(in.Model, []byte(in.Request), streamOption(in))
	}
	translators["openai-responses/interactions/request"] = func(in input) []byte {
		return responsesinteractions.ConvertInteractionsRequestToOpenAIResponses(in.Model, []byte(in.Request), streamOption(in))
	}
}
