// The config-save/steps entry runs upstream's config writer on a config file
// in a temporary directory it creates, and gives what each write left in the
// file.
//
// "options" holds:
//
//	{"file": "<yaml>", "steps": [step, ...]}
//
// "file" is written to config.yaml in the directory, then each step runs in
// turn:
//
//	{"op": "save", "config": "<yaml>", "migrate": bool}
//
// parses "config" with config.ParseConfigBytes (internal/config/parse.go),
// or the file as it stands when "config" is missing, and writes it over the
// file with config.SaveConfigPreserveComments (internal/config/config_yaml.go),
// migrating the file to the v8 layout when "migrate" is true. A config's
// plugin settings are the file's, parsed the same way, as a management
// write's are: it changes the config it read from the file, and open-ferry
// has no plugin host to change them;
//
//	{"op": "nested", "keys": ["a", "b"], "value": "<text>"}
//
// runs config.SaveConfigPreserveCommentsUpdateNestedScalar with them;
//
//	{"op": "write", "body": "<yaml>"}
//
// runs the management package's WriteConfig
// (internal/api/handlers/management/config_basic.go) on the body. Its steps
// are copied here, as importing the management package would register
// translators the registry/* entries must not see.
//
// The output is {"files": ["<file after step 1>", ...]}, with "error" added
// when a step failed: "config: <message>" when its config doesn't parse, the
// writer's message otherwise. The steps after a failed one don't run.
//
// Each file a step leaves is also read with config.LoadConfig
// (internal/config/config_load.go), from a copy in a directory of its own,
// as LoadConfig may write the file it reads. When LoadConfig refuses it,
// "unloadable" holds its message, that file is the last one, and the steps
// after it don't run: open-ferry refuses to write a file it can't load.
package main

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"gopkg.in/yaml.v3"
)

type configSaveStep struct {
	Op      string   `json:"op"`
	Config  *string  `json:"config"`
	Migrate bool     `json:"migrate"`
	Keys    []string `json:"keys"`
	Value   string   `json:"value"`
	Body    string   `json:"body"`
}

type configSaveOptions struct {
	File  string           `json:"file"`
	Steps []configSaveStep `json:"steps"`
}

func init() {
	translators["config-save/steps"] = configSaveSteps
}

func configSaveSteps(in input) []byte {
	var options configSaveOptions
	if err := json.Unmarshal(in.Options, &options); err != nil {
		panic(err)
	}
	dir, err := os.MkdirTemp("", "open-ferry-parity-config-save-")
	if err != nil {
		panic(err)
	}
	defer func() { _ = os.RemoveAll(dir) }()
	path := filepath.Join(dir, "config.yaml")
	if err := os.WriteFile(path, []byte(options.File), 0o600); err != nil {
		panic(err)
	}
	files := []string{}
	for _, step := range options.Steps {
		if message := configSaveStepRun(path, step); message != "" {
			return configSaveJSON(map[string]any{"files": files, "error": message})
		}
		data, err := os.ReadFile(path)
		if err != nil {
			panic(err)
		}
		files = append(files, string(data))
		if message := configSaveLoad(data); message != "" {
			return configSaveJSON(map[string]any{"files": files, "unloadable": message})
		}
	}
	return configSaveJSON(map[string]any{"files": files})
}

// configSaveLoad reads data with config.LoadConfig from a copy, and returns
// why LoadConfig refused it, or "".
func configSaveLoad(data []byte) string {
	dir, err := os.MkdirTemp("", "open-ferry-parity-config-load-")
	if err != nil {
		panic(err)
	}
	defer func() { _ = os.RemoveAll(dir) }()
	path := filepath.Join(dir, "config.yaml")
	if err := os.WriteFile(path, data, 0o600); err != nil {
		panic(err)
	}
	if _, err := config.LoadConfig(path); err != nil {
		return err.Error()
	}
	return ""
}

// configSaveStepRun runs one step on the file at path, and returns why it
// failed, or "".
func configSaveStepRun(path string, step configSaveStep) string {
	var err error
	switch step.Op {
	case "save":
		current, errRead := os.ReadFile(path)
		if errRead != nil {
			panic(errRead)
		}
		source := current
		if step.Config != nil {
			source = []byte(*step.Config)
		}
		cfg, errParse := config.ParseConfigBytes(source)
		if errParse != nil {
			return "config: " + errParse.Error()
		}
		if step.Config != nil {
			if file, errFile := config.ParseConfigBytes(current); errFile == nil {
				cfg.Plugins = file.Plugins
			}
		}
		err = config.SaveConfigPreserveComments(path, cfg, step.Migrate)
	case "nested":
		err = config.SaveConfigPreserveCommentsUpdateNestedScalar(path, step.Keys, step.Value)
	case "write":
		err = configSaveWrite(path, []byte(step.Body))
	default:
		panic("unknown config-save op " + step.Op)
	}
	if err != nil {
		return err.Error()
	}
	return ""
}

// configSaveWrite is the management package's WriteConfig, with its
// os.OpenFile and Write as one os.WriteFile.
func configSaveWrite(path string, data []byte) error {
	var doc yaml.Node
	if err := yaml.Unmarshal(data, &doc); err != nil {
		return err
	}
	if len(doc.Content) > 0 && config.IsV8ConfigLayout(doc.Content[0]) {
		var err error
		data, _, err = config.NormalizeConfigLayout(data, true)
		if err != nil {
			return err
		}
	}
	data = config.NormalizeCommentIndentation(data)
	return os.WriteFile(path, data, 0o644)
}

func configSaveJSON(value any) []byte {
	out, err := json.Marshal(value)
	if err != nil {
		panic(err)
	}
	return out
}
