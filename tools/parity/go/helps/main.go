// Command open-ferry-parity-helps runs upstream CLIProxyAPI's executor
// helpers (internal/runtime/executor/helps) on input read from stdin, so
// open-ferry's Rust ports can be compared against them.
//
// The helpers have a harness of their own because of what they import:
// importing helps registers translators in sdk/translator's default
// registry (today the one from Claude to the Gemini Interactions API, through
// internal/translator/interactions/claude). go/main.go's registry/* entries
// must see only the pairs its go/parity_registry_*.go files import, the ones
// the Rust registry has. So no file of go/main.go's harness may import helps,
// and a pair is added to the registry suites by a blank import there, as in
// go/parity_registry_claude.go, never by removing what an import registered.
// When the Rust registry gains the pair from Claude to Interactions (P4
// WP4-A), its blank import goes in go/parity_registry_claude.go; nothing here
// changes.
//
// Like go/main.go, this file is compiled inside a CLIProxyAPI checkout via go
// build -overlay, as cmd/open-ferry-parity-helps, along with each
// go/helps/parity_*.go, which adds its entries to translators from init().
// See tools/parity/README.md.
//
// Input and output are JSON lines in go/main.go's format. The
// open-ferry-parity crate sends a key here when its package, the part before
// the first "/", is one of the helpers' (see is_helps in src/upstream.rs):
// payload, usage or ttft, as in payload/apply, usage/parse and
// ttft/token-event. Each file documents its entries.
package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"os"
)

type input struct {
	Translator string          `json:"translator"`
	Model      string          `json:"model"`
	Request    string          `json:"request"`
	Translated string          `json:"translated_request"`
	Events     []string        `json:"events"`
	Options    json.RawMessage `json:"options"`
}

type output struct {
	Output []byte `json:"output"`
	Panic  string `json:"panic,omitempty"`
}

// translators is filled by the go/helps/parity_*.go files.
var translators = map[string]func(in input) []byte{}

func decodeOptions(in input, options any) {
	if len(in.Options) == 0 {
		return
	}
	if err := json.Unmarshal(in.Options, options); err != nil {
		panic(fmt.Sprintf("bad options: %v", err))
	}
}

// marshal encodes a report without escaping <, > and &, so strings read
// back unchanged.
func marshal(value any) []byte {
	var out bytes.Buffer
	encoder := json.NewEncoder(&out)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(value); err != nil {
		panic(err)
	}
	return bytes.TrimSuffix(out.Bytes(), []byte{'\n'})
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

func run(translate func(input) []byte, in input) (out output) {
	defer func() {
		if r := recover(); r != nil {
			out = output{Panic: fmt.Sprint(r)}
		}
	}()
	return output{Output: translate(in)}
}

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "open-ferry-parity-helps: "+format+"\n", args...)
	os.Exit(2)
}
