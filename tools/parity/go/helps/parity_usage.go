// The usage/parse entry runs one of upstream's helps.Parse*Usage
// (internal/runtime/executor/helps/usage_helpers.go) on an upstream's
// response body or stream line, "request", and writes the usage detail it
// returned (see usageDetail), or null where the parser says it found none.
// "options" names the parser (see usageOptions): "codex" (ParseCodexUsage),
// "openai" (ParseOpenAIUsage), "openai-stream" (ParseOpenAIStreamUsage),
// "claude" (ParseClaudeUsage), "claude-stream" (ParseClaudeStreamUsage),
// "gemini" (ParseGeminiUsage) or "gemini-stream" (ParseGeminiStreamUsage).
// The Antigravity and Codex image tool parsers aren't ported, so have no
// entry. Interactions whole/stream parsers, public merging and stream
// sequences use the same entry with their parser option.
//
// It is in go/helps/main.go's harness, not go/main.go's, because importing
// helps registers a translator the registry/* entries must not see (see
// go/helps/main.go).
package main

import (
	"fmt"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
)

func init() {
	translators["usage/parse"] = usageParse
}

type usageOptions struct {
	Parser   string `json:"parser"`
	Protocol string `json:"protocol"`
	Steps    []struct {
		Parser string `json:"parser"`
		Body   string `json:"body"`
	} `json:"steps"`
	Lines []string `json:"lines"`
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
	decodeOptions(in, &options)
	if options.Parser == "merge" {
		steps := make([]usageDetail, 0, len(options.Steps))
		var merged usage.Detail
		for i, step := range options.Steps {
			update, _ := usageParsed(step.Parser, []byte(step.Body))
			if i == 0 {
				merged = update
			} else {
				merged = helps.MergeStreamUsageDetail(merged, update)
			}
			steps = append(steps, usageJSON(merged))
		}
		return marshal(steps)
	}
	if options.Parser == "sequence" {
		var buffer helps.StreamUsageBuffer
		steps := make([]map[string]interface{}, 0, len(options.Lines))
		for _, line := range options.Lines {
			switch options.Protocol {
			case "openai":
				buffer.ObserveOpenAIStream([]byte(line))
			case "claude":
				buffer.ObserveClaudeStream([]byte(line))
			case "interactions":
				detail, found := helps.ParseInteractionsStreamUsage([]byte(line))
				buffer.Observe(detail, found)
			default:
				panic(fmt.Sprintf("unknown sequence protocol %q", options.Protocol))
			}
			var value interface{}
			if detail, found := buffer.Detail(); found {
				value = usageJSON(detail)
			}
			steps = append(steps, map[string]interface{}{"detail": value, "model": buffer.ResponseModel()})
		}
		return marshal(steps)
	}
	detail, found := usageParsed(options.Parser, []byte(in.Request))
	if !found {
		return []byte("null")
	}
	return marshal(usageJSON(detail))
}

// Reads one detail without changing its accounting evidence.
func usageParsed(parser string, body []byte) (usage.Detail, bool) {
	var detail usage.Detail
	found := true
	switch parser {
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
	case "interactions":
		detail = helps.ParseInteractionsUsage(body)
	case "interactions-stream":
		detail, found = helps.ParseInteractionsStreamUsage(body)
	case "gemini":
		detail = helps.ParseGeminiUsage(body)
	case "gemini-stream":
		detail, found = helps.ParseGeminiStreamUsage(body)
	default:
		panic(fmt.Sprintf("usage/parse: unknown parser %q", parser))
	}
	return detail, found
}

// usageJSON preserves every exported accounting field.
func usageJSON(detail usage.Detail) usageDetail {
	return usageDetail{
		InputTokens:         detail.InputTokens,
		OutputTokens:        detail.OutputTokens,
		ReasoningTokens:     detail.ReasoningTokens,
		CachedTokens:        detail.CachedTokens,
		CacheReadTokens:     detail.CacheReadTokens,
		CacheCreationTokens: detail.CacheCreationTokens,
		TotalTokens:         detail.TotalTokens,
		TokenBreakdown:      detail.TokenBreakdown,
		ResponseServiceTier: detail.ResponseServiceTier,
	}
}
