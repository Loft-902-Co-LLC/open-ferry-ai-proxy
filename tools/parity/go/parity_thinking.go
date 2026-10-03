// The thinking/* entries run upstream's ApplyThinkingWithModelInfo, which
// applies a model's thinking suffix, or the effort a request asks for, to a
// request already translated for its target. thinking/codex and
// thinking/openai differ only in which of open-ferry's ports they are
// compared with; the target is the "to" option either way.
//
// "request" is the translated body and "model" the model with its suffix.
// "options" holds the rest (see thinkingOptions): the client's request as it
// was sent ("source", which may be empty or not JSON), its format ("from"),
// the target ("to"), the executor's provider ("provider") and the model the
// request is bound to ("model_info", or null for one nobody registered).
// Taking the model from the case keeps the global model registry out of it.
//
// Each entry writes {"body": <the body it returned>, "error": <the error's
// message, or null>}.

package main

import (
	"encoding/json"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/thinking"
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/thinking/provider/codex"
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/thinking/provider/openai"
	log "github.com/sirupsen/logrus"
)

type thinkingOptions struct {
	Source    string         `json:"source"`
	From      string         `json:"from"`
	To        string         `json:"to"`
	Provider  string         `json:"provider"`
	ModelInfo *thinkingModel `json:"model_info"`
}

// thinkingModel holds the registry.ModelInfo fields the thinking package
// reads. Most aren't marshalled, so they can't be decoded into it directly.
type thinkingModel struct {
	ID                         string                    `json:"id"`
	Type                       string                    `json:"type"`
	UserDefined                bool                      `json:"user_defined"`
	SupportConfigurationUpdate bool                      `json:"support_configuration_update"`
	Thinking                   *registry.ThinkingSupport `json:"thinking"`
}

type thinkingReport struct {
	Body  json.RawMessage `json:"body"`
	Error *string         `json:"error"`
}

func thinkingApply(in input) []byte {
	var options thinkingOptions
	decodeOptions(in, &options)
	var info *registry.ModelInfo
	if model := options.ModelInfo; model != nil {
		info = &registry.ModelInfo{
			ID:                         model.ID,
			Type:                       model.Type,
			UserDefined:                model.UserDefined,
			SupportConfigurationUpdate: model.SupportConfigurationUpdate,
			Thinking:                   model.Thinking,
		}
	}
	// A setting the model can't take is logged as a warning; keep it off
	// the harness's stderr.
	level := log.GetLevel()
	log.SetLevel(log.ErrorLevel)
	defer log.SetLevel(level)
	body, err := thinking.ApplyThinkingWithModelInfo([]byte(in.Request), []byte(options.Source), in.Model, options.From, options.To, options.Provider, info)
	report := thinkingReport{Body: body}
	if err != nil {
		message := err.Error()
		report.Error = &message
	}
	return marshal(report)
}

func init() {
	translators["thinking/codex"] = thinkingApply
	translators["thinking/openai"] = thinkingApply
}
