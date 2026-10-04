// The payload/apply entry runs upstream's
// helps.ApplyPayloadConfigWithTrackedPathsForExecutor
// (internal/runtime/executor/helps/payload_helpers.go) on a translated
// body, with the case's rules, model, protocol, headers and tracked
// paths.
//
// "request" is the body and "model" the model sent upstream. "options"
// holds:
//
//	{"config": "...", "no_config": bool, "executor": "...",
//	 "protocol": "...", "from": "...", "root": "...",
//	 "original": null or "...", "requested_model": "...",
//	 "request_path": "...", "headers": [["name", "value"], ...],
//	 "tracked": ["...", ...]}
//
// "config" is a YAML config, read by config.ParseConfigBytes; with
// "no_config" there is none. "original" is the client's request as the
// executor translated it, or null for none. The headers are added in
// order.
//
// It writes {"body": the body, "touched": the tracked paths touched,
// sorted}, {"config_error": true} when the config can't be read, or
// {"invalid_body": the body as a string} when the body isn't valid JSON.
//
// It is in go/helps/main.go's harness, not go/main.go's, because importing
// helps registers a translator the registry/* entries must not see (see
// go/helps/main.go).
package main

import (
	"encoding/json"
	"net/http"
	"sort"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
)

type payloadOptions struct {
	Config         string      `json:"config"`
	NoConfig       bool        `json:"no_config"`
	Executor       string      `json:"executor"`
	Protocol       string      `json:"protocol"`
	From           string      `json:"from"`
	Root           string      `json:"root"`
	Original       *string     `json:"original"`
	RequestedModel string      `json:"requested_model"`
	RequestPath    string      `json:"request_path"`
	Headers        [][2]string `json:"headers"`
	Tracked        []string    `json:"tracked"`
}

func init() {
	translators["payload/apply"] = payloadApply
}

func payloadApply(in input) []byte {
	var options payloadOptions
	decodeOptions(in, &options)
	var cfg *config.Config
	if !options.NoConfig {
		parsed, err := config.ParseConfigBytes([]byte(options.Config))
		if err != nil {
			return marshal(map[string]bool{"config_error": true})
		}
		cfg = parsed
	}
	headers := make(http.Header)
	for _, header := range options.Headers {
		headers.Add(header[0], header[1])
	}
	var original []byte
	if options.Original != nil {
		original = []byte(*options.Original)
	}
	out, touched := helps.ApplyPayloadConfigWithTrackedPathsForExecutor(cfg, options.Executor, in.Model, options.Protocol, options.From, options.Root, []byte(in.Request), original, options.RequestedModel, options.RequestPath, headers, options.Tracked...)
	if !json.Valid(out) {
		return marshal(map[string]string{"invalid_body": string(out)})
	}
	paths := make([]string, 0, len(touched))
	for path, ok := range touched {
		if ok {
			paths = append(paths, path)
		}
	}
	sort.Strings(paths)
	return marshal(struct {
		Body    json.RawMessage `json:"body"`
		Touched []string        `json:"touched"`
	}{out, paths})
}
