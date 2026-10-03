// The codex-models/list entry builds the model list Codex clients fetch
// (GET /v1/models?client_version=...) with upstream's
// internal/client/codex/models, from models registered in upstream's model
// registry.
//
// "options" holds:
//
//	{"registrations": [{"client": "...", "provider": "...", "models": [model, ...]}, ...],
//	 "client_version": "...", "optimize_multi_agent_v2": bool,
//	 "providers": bool, "apply_patch": null or ["public model ID", ...]}
//
// Each model is an object with the fields of codexModelsModel; the ones
// registry.ModelInfo keeps out of JSON (metadata_model_id, the explicit_*
// flags and max_context_length) are read here. The registrations are
// registered for the case alone. The list is built from the registry's
// OpenAI model list sorted by trimmed ID, as open-ferry's registry lists
// them; upstream's order varies. "providers" passes the registry's provider
// lookup; "apply_patch" lists the model IDs that take the freeform
// apply_patch tool, or is null for no capability. No web search capability
// is given, as open-ferry doesn't port cpa_capabilities. Which models take
// apply_patch, as the HTTP handler decides it, is checked by open-ferry's
// own tests: open-ferry decides it by provider name rather than by asking
// executors.
//
// The output is {"body": ..., "bytes": n, "sha256": "..."}: the length and
// SHA-256 of MarshalCompact's output, and the output itself with every
// string longer than codexModelsLongString bytes replaced by
// "sha256:<hex of its bytes>", so a difference shows without the catalog's
// long instructions.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"sort"
	"strings"

	codexmodels "github.com/router-for-me/CLIProxyAPI/v8/internal/client/codex/models"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
)

// codexModelsLongString is the length past which a string in the output's
// body is replaced by its hash.
const codexModelsLongString = 120

type codexModelsOptions struct {
	Registrations []struct {
		Client   string             `json:"client"`
		Provider string             `json:"provider"`
		Models   []codexModelsModel `json:"models"`
	} `json:"registrations"`
	ClientVersion        string    `json:"client_version"`
	OptimizeMultiAgentV2 bool      `json:"optimize_multi_agent_v2"`
	Providers            bool      `json:"providers"`
	ApplyPatch           *[]string `json:"apply_patch"`
}

type codexModelsModel struct {
	ID                       string                    `json:"id"`
	MetadataModelID          string                    `json:"metadata_model_id"`
	Object                   string                    `json:"object"`
	Created                  int64                     `json:"created"`
	OwnedBy                  string                    `json:"owned_by"`
	Type                     string                    `json:"type"`
	DisplayName              string                    `json:"display_name"`
	Version                  string                    `json:"version"`
	Description              string                    `json:"description"`
	ContextLength            int                       `json:"context_length"`
	MaxContextLength         int                       `json:"max_context_length"`
	MaxCompletionTokens      int                       `json:"max_completion_tokens"`
	SupportedParameters      []string                  `json:"supported_parameters"`
	SupportedInputModalities []string                  `json:"supported_input_modalities"`
	Thinking                 *registry.ThinkingSupport `json:"thinking"`
	ExplicitThinking         bool                      `json:"explicit_thinking"`
	ExplicitInputModalities  bool                      `json:"explicit_input_modalities"`
}

func (m codexModelsModel) info() *registry.ModelInfo {
	return &registry.ModelInfo{
		ID:                       m.ID,
		MetadataModelID:          m.MetadataModelID,
		Object:                   m.Object,
		Created:                  m.Created,
		OwnedBy:                  m.OwnedBy,
		Type:                     m.Type,
		DisplayName:              m.DisplayName,
		Version:                  m.Version,
		Description:              m.Description,
		ContextLength:            m.ContextLength,
		MaxContextLength:         m.MaxContextLength,
		MaxCompletionTokens:      m.MaxCompletionTokens,
		SupportedParameters:      m.SupportedParameters,
		SupportedInputModalities: m.SupportedInputModalities,
		Thinking:                 m.Thinking,
		ExplicitThinking:         m.ExplicitThinking,
		ExplicitInputModalities:  m.ExplicitInputModalities,
	}
}

func init() {
	translators["codex-models/list"] = codexModelsList
}

func codexModelsList(in input) []byte {
	var options codexModelsOptions
	decodeOptions(in, &options)
	modelRegistry := registry.GetGlobalRegistry()
	for _, registration := range options.Registrations {
		models := make([]*registry.ModelInfo, 0, len(registration.Models))
		for _, model := range registration.Models {
			models = append(models, model.info())
		}
		modelRegistry.RegisterClient(registration.Client, registration.Provider, models)
		defer modelRegistry.UnregisterClient(registration.Client)
	}
	models := modelRegistry.GetAvailableModels("openai")
	modelID := func(model map[string]any) string {
		id, _ := model["id"].(string)
		return strings.TrimSpace(id)
	}
	sort.SliceStable(models, func(i, j int) bool { return modelID(models[i]) < modelID(models[j]) })
	var providers codexmodels.ProvidersForModelFunc
	if options.Providers {
		providers = modelRegistry.GetModelProviders
	}
	var applyPatch codexmodels.ApplyPatchCapabilityForModelFunc
	if options.ApplyPatch != nil {
		supported := make(map[string]bool, len(*options.ApplyPatch))
		for _, id := range *options.ApplyPatch {
			supported[id] = true
		}
		applyPatch = func(model string) bool { return supported[model] }
	}
	response := codexmodels.BuildResponseForClientWithToolCapabilities(models, providers, nil, applyPatch, options.OptimizeMultiAgentV2, options.ClientVersion)
	body, err := codexmodels.MarshalCompact(response)
	if err != nil {
		panic(err)
	}
	return codexModelsSummary(body)
}

func codexModelsSummary(body []byte) []byte {
	sum := sha256.Sum256(body)
	decoder := json.NewDecoder(bytes.NewReader(body))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		panic(err)
	}
	return marshal(map[string]any{
		"body":   codexModelsShorten(value),
		"bytes":  len(body),
		"sha256": hex.EncodeToString(sum[:]),
	})
}

func codexModelsShorten(value any) any {
	switch typed := value.(type) {
	case string:
		if len(typed) > codexModelsLongString {
			sum := sha256.Sum256([]byte(typed))
			return "sha256:" + hex.EncodeToString(sum[:])
		}
	case []any:
		for index := range typed {
			typed[index] = codexModelsShorten(typed[index])
		}
	case map[string]any:
		for key := range typed {
			typed[key] = codexModelsShorten(typed[key])
		}
	}
	return value
}
