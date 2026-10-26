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
	"io"
	"os"
	"path/filepath"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	log "github.com/sirupsen/logrus"
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
	File      string           `json:"file"`
	Trace     bool             `json:"_ablation_trace"`
	Steps     []configSaveStep `json:"steps"`
	Ownership *struct {
		Source         string         `json:"source"`
		Operation      map[string]any `json:"operation"`
		SourceOwner    bool           `json:"source_owner"`
		FootSeparators bool           `json:"foot_separators"`
		Protected      *string        `json:"protected"`
	} `json:"_ownership_request"`
}

func init() {
	translators["config-save/steps"] = configSaveSteps
}

func configSaveSteps(in input) []byte {
	// The structured reply retains errors and proof data. Upstream warnings
	// can echo config values; never send those to the parity run's log.
	logger := log.StandardLogger()
	previousOutput := logger.Out
	logger.SetOutput(io.Discard)
	defer logger.SetOutput(previousOutput)
	var options configSaveOptions
	if err := json.Unmarshal(in.Options, &options); err != nil {
		panic(err)
	}
	if options.Ownership != nil {
		proof := config.ParityOwnership(options.Ownership.Source, options.Ownership.Operation)
		if options.Ownership.SourceOwner {
			identity := config.ParitySourceIdentity(options.Ownership.Source, options.Ownership.Operation)
			frame := map[string]any{"identity": identity}
			proof["retained_source_owner"] = frame
			frame["expanded_source_inventory"] = config.ParityExpandedInventory(options.Ownership.Source)
			next := in
			next.Options = configSaveJSON(map[string]any{"file": options.Ownership.Source, "steps": []any{options.Ownership.Operation}})
			var exact any
			if json.Unmarshal(configSaveSteps(next), &exact) == nil {
				frame["exact_go"] = exact
				if reply, ok := exact.(map[string]any); ok {
					if files, ok := reply["files"].([]any); ok && len(files) == 1 {
						if text, ok := files[0].(string); ok {
							frame["exact_go_inventory"] = config.ParityDocumentInventory(text)
						}
					}
				}
			}
			if options.Ownership.Protected != nil {
				frame["protected_inventory"] = config.ParityDocumentInventory(*options.Ownership.Protected)
				if options.Ownership.FootSeparators {
					frame["foot_separators"] = config.ParityFootSeparators(*options.Ownership.Protected)
				}
			}
			if identity["unavailable"] == nil {
				run := func(source string, marked bool) any {
					op := map[string]any{}
					for k, v := range options.Ownership.Operation {
						op[k] = v
					}
					if marked && op["op"] == "write" {
						op["body"] = source
					}
					next := in
					next.Options = configSaveJSON(map[string]any{"file": source, "steps": []any{op}})
					var reply any
					if json.Unmarshal(configSaveSteps(next), &reply) != nil {
						return nil
					}
					return reply
				}
				frame["go"] = frame["exact_go"]
				frame["go_inventory"] = frame["exact_go_inventory"]
				frame["marked_go"] = run(identity["masked_source"].(string), true)
				for _, name := range []string{"marked_go"} {
					if reply, ok := frame[name].(map[string]any); ok {
						if files, ok := reply["files"].([]any); ok && len(files) == 1 {
							if text, ok := files[0].(string); ok {
								frame[name+"_inventory"] = config.ParityDocumentInventory(text)
							}
						}
					}
				}
			}
		}
		return configSaveJSON(proof)
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
	rejected := [][]string{}
	current := []string{}
	if options.Trace {
		config.ParityRejected = func(data []byte) { current = append(current, string(data)) }
		defer func() { config.ParityRejected = nil }()
	}
	result := func(value map[string]any) []byte {
		if options.Trace {
			value["rejected"] = rejected
		}
		return configSaveJSON(value)
	}
	files := []string{}
	for _, step := range options.Steps {
		current = []string{}
		if message := configSaveStepRun(path, step); message != "" {
			rejected = append(rejected, current)
			return result(map[string]any{"files": files, "error": message})
		}
		data, err := os.ReadFile(path)
		if err != nil {
			panic(err)
		}
		files = append(files, string(data))
		if message := configSaveLoad(data); message != "" {
			rejected = append(rejected, current)
			return result(map[string]any{"files": files, "unloadable": message})
		}
		rejected = append(rejected, current)
	}
	return result(map[string]any{"files": files})
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
