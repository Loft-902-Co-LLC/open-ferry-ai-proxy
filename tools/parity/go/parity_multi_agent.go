// The multi-agent/* entries run upstream's
// internal/client/codex/optimize-multi-agent-v2 on a Codex client's
// Responses request, or on an event the upstream sent back.
//
// "request" is the request, or for multi-agent/restore the event's data.
// "options" holds:
//
//	{"user_agent": "...", "subagent": "...", "enabled": bool,
//	 "compat": bool, "optimized": bool,
//	 "registrations": [{"client": "...", "provider": "...",
//	   "models": [{"id": "...", "display_name": "...", "description": "...",
//	     "thinking": null or {"levels": [...]}}, ...]}, ...]}
//
// "user_agent" and "subagent" are the client's User-Agent and
// X-Openai-Subagent headers, sent when they aren't empty. "enabled" is
// client.codex.optimize-multi-agent-v2, or for multi-agent/orphan
// codex.orphan-delegation-compatibility. "compat" says the request is for a
// credential's compatibility model (multi-agent/input), and "optimized"
// that the request's namespace was renamed (multi-agent/restore). The
// registrations are registered in upstream's model registry for the case
// alone, and give the models spawn_agent's description lists.
//
// multi-agent/prepare runs PrepareCodexMultiAgentV2Tools without Home, and
// multi-agent/input RewriteCodexMultiAgentV2Input, multi-agent/orphan
// RewriteCodexOrphanDelegationInput; each writes the request it returns.
// multi-agent/optimize runs OptimizeCodexMultiAgentV2Request without a Gin
// context, so the tools aren't marked prepared, and writes {"body": the
// request, "optimized": bool}. multi-agent/restore runs
// RestoreCodexMultiAgentV2Response and writes {"output": its bytes as a
// string}, so that their key order, escapes and number text are compared.
package main

import (
	"context"
	"encoding/json"
	"net/http"

	multiagentv2 "github.com/router-for-me/CLIProxyAPI/v8/internal/client/codex/optimize-multi-agent-v2"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
)

type multiAgentOptions struct {
	UserAgent     string `json:"user_agent"`
	Subagent      string `json:"subagent"`
	Enabled       bool   `json:"enabled"`
	Compat        bool   `json:"compat"`
	Optimized     bool   `json:"optimized"`
	Registrations []struct {
		Client   string `json:"client"`
		Provider string `json:"provider"`
		Models   []struct {
			ID          string `json:"id"`
			DisplayName string `json:"display_name"`
			Description string `json:"description"`
			Thinking    *struct {
				Levels []string `json:"levels"`
			} `json:"thinking"`
		} `json:"models"`
	} `json:"registrations"`
}

func init() {
	translators["multi-agent/prepare"] = func(in input) []byte {
		options, headers, unregister := multiAgentSetup(in)
		defer unregister()
		out, _ := multiagentv2.PrepareCodexMultiAgentV2Tools(context.Background(), headers, []byte(in.Request), options.Enabled, false)
		return out
	}
	translators["multi-agent/optimize"] = func(in input) []byte {
		options, headers, unregister := multiAgentSetup(in)
		defer unregister()
		out, optimized := multiagentv2.OptimizeCodexMultiAgentV2Request(context.Background(), headers, []byte(in.Request), multiAgentConfig(options))
		return marshal(struct {
			Body      json.RawMessage `json:"body"`
			Optimized bool            `json:"optimized"`
		}{out, optimized})
	}
	translators["multi-agent/input"] = func(in input) []byte {
		options, headers, unregister := multiAgentSetup(in)
		defer unregister()
		return multiagentv2.RewriteCodexMultiAgentV2Input(context.Background(), headers, []byte(in.Request), multiAgentConfig(options), options.Compat)
	}
	translators["multi-agent/orphan"] = func(in input) []byte {
		options, headers, unregister := multiAgentSetup(in)
		defer unregister()
		return multiagentv2.RewriteCodexOrphanDelegationInput(context.Background(), headers, []byte(in.Request), options.Enabled)
	}
	translators["multi-agent/restore"] = func(in input) []byte {
		var options multiAgentOptions
		decodeOptions(in, &options)
		out := multiagentv2.RestoreCodexMultiAgentV2Response([]byte(in.Request), options.Optimized)
		return marshal(map[string]string{"output": string(out)})
	}
}

// multiAgentSetup reads the case's options and registers its models. It
// returns the options, the client's headers, and a function that
// unregisters the models.
func multiAgentSetup(in input) (multiAgentOptions, http.Header, func()) {
	var options multiAgentOptions
	decodeOptions(in, &options)
	modelRegistry := registry.GetGlobalRegistry()
	clients := make([]string, 0, len(options.Registrations))
	for _, registration := range options.Registrations {
		models := make([]*registry.ModelInfo, 0, len(registration.Models))
		for _, model := range registration.Models {
			info := &registry.ModelInfo{
				ID:          model.ID,
				DisplayName: model.DisplayName,
				Description: model.Description,
			}
			if model.Thinking != nil {
				info.Thinking = &registry.ThinkingSupport{Levels: model.Thinking.Levels}
			}
			models = append(models, info)
		}
		modelRegistry.RegisterClient(registration.Client, registration.Provider, models)
		clients = append(clients, registration.Client)
	}
	headers := http.Header{}
	if options.UserAgent != "" {
		headers.Set("User-Agent", options.UserAgent)
	}
	if options.Subagent != "" {
		headers.Set("X-Openai-Subagent", options.Subagent)
	}
	return options, headers, func() {
		for _, client := range clients {
			modelRegistry.UnregisterClient(client)
		}
	}
}

func multiAgentConfig(options multiAgentOptions) *config.Config {
	return &config.Config{SDKConfig: config.SDKConfig{Client: config.ClientConfig{Codex: config.CodexClientConfig{OptimizeMultiAgentV2: options.Enabled}}}}
}
