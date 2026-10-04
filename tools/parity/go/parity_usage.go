// The usage/parse entry runs one of upstream's helps.Parse*Usage
// (internal/runtime/executor/helps/usage_helpers.go) on an upstream's
// response body or stream line, "request", and writes the usage detail it
// returned (see usageDetail), or null where the parser says it found none.
// "options" names the parser (see usageOptions): "codex" (ParseCodexUsage),
// "openai" (ParseOpenAIUsage), "openai-stream" (ParseOpenAIStreamUsage),
// "claude" (ParseClaudeUsage), "claude-stream" (ParseClaudeStreamUsage),
// "gemini" (ParseGeminiUsage) or "gemini-stream" (ParseGeminiStreamUsage).
// The Antigravity, Interactions and Codex image tool parsers aren't ported,
// so have no entry.
//
// The entry imports helps, and keeps to this file so it can move with it.

package main

import (
	"encoding/json"
	"fmt"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
)

func init() {
	translators["usage/parse"] = usageParse
}

type usageOptions struct {
	Parser string `json:"parser"`
}

// usageDetail is usage.Detail with its fields named for JSON.
type usageDetail struct {
	InputTokens         int64                `json:"input_tokens"`
	OutputTokens        int64                `json:"output_tokens"`
	ReasoningTokens     int64                `json:"reasoning_tokens"`
	CachedTokens        int64                `json:"cached_tokens"`
	CacheReadTokens     int64                `json:"cache_read_tokens"`
	CacheCreationTokens int64                `json:"cache_creation_tokens"`
	TotalTokens         int64                `json:"total_tokens"`
	TokenBreakdown      usage.TokenBreakdown `json:"token_breakdown"`
	ResponseServiceTier string               `json:"response_service_tier"`
}

func usageParse(in input) []byte {
	var options usageOptions
	if err := json.Unmarshal(in.Options, &options); err != nil {
		panic(fmt.Sprintf("usage/parse options: %v", err))
	}
	body := []byte(in.Request)
	var detail usage.Detail
	found := true
	switch options.Parser {
	case "codex":
		detail, found = helps.ParseCodexUsage(body)
	case "openai":
		detail = helps.ParseOpenAIUsage(body)
	case "openai-stream":
		detail, found = helps.ParseOpenAIStreamUsage(body)
	case "claude":
		detail = helps.ParseClaudeUsage(body)
	case "claude-stream":
		detail, found = helps.ParseClaudeStreamUsage(body)
	case "gemini":
		detail = helps.ParseGeminiUsage(body)
	case "gemini-stream":
		detail, found = helps.ParseGeminiStreamUsage(body)
	default:
		panic(fmt.Sprintf("usage/parse: unknown parser %q", options.Parser))
	}
	if !found {
		return []byte("null")
	}
	out, err := json.Marshal(usageDetail{
		InputTokens:         detail.InputTokens,
		OutputTokens:        detail.OutputTokens,
		ReasoningTokens:     detail.ReasoningTokens,
		CachedTokens:        detail.CachedTokens,
		CacheReadTokens:     detail.CacheReadTokens,
		CacheCreationTokens: detail.CacheCreationTokens,
		TotalTokens:         detail.TotalTokens,
		TokenBreakdown:      detail.TokenBreakdown,
		ResponseServiceTier: detail.ResponseServiceTier,
	})
	if err != nil {
		panic(err)
	}
	return out
}
