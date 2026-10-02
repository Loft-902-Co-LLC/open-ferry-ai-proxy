// Command open-ferry-parity runs upstream CLIProxyAPI translators on requests
// read from stdin, so open-ferry's Rust ports can be compared against them.
//
// The translators live in internal packages, so this file is compiled inside a
// CLIProxyAPI checkout (via go build -overlay) by the open-ferry-parity crate.
// See tools/parity/README.md.
//
// Input and output are JSON lines. Each input line is
//
//	{"translator":"codex/claude/request","model":"gpt-5","request":"<request JSON as a string>"}
//
// and each output line is {"output":"<base64 of the translator's raw bytes>"},
// or {"panic":"<message>"} if the translator panicked. Output is base64 so
// invalid UTF-8 survives the trip.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"

	codexclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/claude"
)

type input struct {
	Translator string `json:"translator"`
	Model      string `json:"model"`
	Request    string `json:"request"`
}

type output struct {
	Output []byte `json:"output"`
	Panic  string `json:"panic,omitempty"`
}

var translators = map[string]func(model string, request []byte) []byte{
	"codex/claude/request": func(model string, request []byte) []byte {
		return codexclaude.ConvertClaudeRequestToCodex(model, request, true)
	},
}

func main() {
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 0, 1<<20), 1<<30)
	writer := bufio.NewWriter(os.Stdout)
	encoder := json.NewEncoder(writer)

	for scanner.Scan() {
		var in input
		if err := json.Unmarshal(scanner.Bytes(), &in); err != nil {
			fail("bad input line: %v", err)
		}
		translate, ok := translators[in.Translator]
		if !ok {
			fail("unknown translator %q", in.Translator)
		}
		if err := encoder.Encode(run(translate, in)); err != nil {
			fail("write output: %v", err)
		}
	}
	if err := scanner.Err(); err != nil {
		fail("read input: %v", err)
	}
	if err := writer.Flush(); err != nil {
		fail("write output: %v", err)
	}
}

func run(translate func(string, []byte) []byte, in input) (out output) {
	defer func() {
		if r := recover(); r != nil {
			out = output{Panic: fmt.Sprint(r)}
		}
	}()
	return output{Output: translate(in.Model, []byte(in.Request))}
}

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "open-ferry-parity: "+format+"\n", args...)
	os.Exit(2)
}
