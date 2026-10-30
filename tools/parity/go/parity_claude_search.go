// Pins the native tool-search switch for one request case. The main
// harness processes cases sequentially; restore the setting before the next.
package main

import clauderesponses "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/claude/openai/responses"

func claudeSearchRequest(in input, compat bool) []byte {
	var options struct {
		NativeToolSearch *bool `json:"native_tool_search"`
	}
	decodeOptions(in, &options)
	if options.NativeToolSearch != nil {
		previous := clauderesponses.ClientToolSearchEnabled()
		clauderesponses.SetClientToolSearchEnabled(*options.NativeToolSearch)
		defer clauderesponses.SetClientToolSearchEnabled(previous)
	}
	if compat {
		return refused(clauderesponses.ConvertOpenAIResponsesRequestToClaudeWithCompat(in.Model, []byte(in.Request), true))
	}
	return refused(clauderesponses.ConvertOpenAIResponsesRequestToClaude(in.Model, []byte(in.Request), true))
}
