// The config-diff/details entry parses two YAML configs with upstream's
// config.ParseConfigBytes (internal/config/parse.go) and gives the change
// details upstream's diff.BuildConfigChangeDetails
// (internal/watcher/diff/config_diff.go) logs between them on reload.
//
// "options" holds:
//
//	{"old": "<yaml>", "new": "<yaml>"}
//
// The output is {"details": [line, ...]}, or {"error": "old"} or
// {"error": "new"} when that config doesn't parse: which one, not why, as
// the cases are meant to parse. The configs use only the keys open-ferry
// types, so the lines for the sections it reads and ignores never show.
package main

import (
	"encoding/json"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/diff"
)

type configDiffOptions struct {
	Old string `json:"old"`
	New string `json:"new"`
}

func init() {
	translators["config-diff/details"] = configDiffDetails
}

func configDiffDetails(in input) []byte {
	var options configDiffOptions
	if err := json.Unmarshal(in.Options, &options); err != nil {
		panic(err)
	}
	oldCfg, err := config.ParseConfigBytes([]byte(options.Old))
	if err != nil {
		return configDiffJSON(map[string]any{"error": "old"})
	}
	newCfg, err := config.ParseConfigBytes([]byte(options.New))
	if err != nil {
		return configDiffJSON(map[string]any{"error": "new"})
	}
	details := diff.BuildConfigChangeDetails(oldCfg, newCfg)
	if details == nil {
		details = []string{}
	}
	return configDiffJSON(map[string]any{"details": details})
}

func configDiffJSON(value any) []byte {
	out, err := json.Marshal(value)
	if err != nil {
		panic(err)
	}
	return out
}
