// Command open-ferry-parity-interactions runs upstream CLIProxyAPI's Gemini
// Interactions translators on input read from stdin, so open-ferry's Rust
// ports can be compared against them.
//
// Importing the Interactions translator packages registers their pairs in
// sdk/translator's default registry, which go/main.go's registry/* entries
// run. Those entries gain each family's pairs only when the family imports
// its packages in go/parity_registry_*.go, in the same commit as the Rust
// registry gains them, so the families' own entries get a harness of their
// own. Like go/main.go, this file is compiled inside a CLIProxyAPI checkout
// via go build -overlay, as cmd/open-ferry-parity-interactions, along with
// each go/interactions/parity_*.go, which adds one family's entries to
// translators from init(). See tools/parity/README.md.
//
// Input and output are JSON lines in go/main.go's format. An entry's key is
// "<package>/<format>/<kind>" with interactions as its package or its format,
// such as interactions/claude/request or codex/interactions/response; the
// open-ferry-parity crate sends a key here when either is interactions. Each
// family's file documents its entries.
package main

import (
	"bufio"
	"bytes"
	"context"
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

// translators is filled by the go/interactions/parity_*.go files.
var translators = map[string]func(in input) []byte{}

// streamTranslator is the signature of upstream's stream response
// translators.
type streamTranslator func(ctx context.Context, modelName string, originalRequestRawJSON, requestRawJSON, rawJSON []byte, param *any) [][]byte

// streamChunks runs translate on each event in turn, with one state for the
// whole stream, and returns every chunk it gave, in order.
func streamChunks(in input, translate streamTranslator) [][]byte {
	var param any
	var chunks [][]byte
	for _, event := range in.Events {
		chunks = append(chunks, translate(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), []byte(event), &param)...)
	}
	return chunks
}

// joined is the chunks concatenated.
func joined(chunks [][]byte) []byte {
	return bytes.Join(chunks, nil)
}

// chunkList is a JSON array with one string per chunk, empty ones included.
func chunkList(chunks [][]byte) []byte {
	list := make([]string, 0, len(chunks))
	for _, chunk := range chunks {
		list = append(list, string(chunk))
	}
	return marshal(list)
}

func finalEvent(in input) []byte {
	if len(in.Events) == 0 {
		return nil
	}
	return []byte(in.Events[0])
}

func translatedRequest(in input) []byte {
	if in.Translated == "" {
		return nil
	}
	return []byte(in.Translated)
}

// streamOption is the "stream" option, false if not given.
func streamOption(in input) bool {
	var options struct {
		Stream bool `json:"stream"`
	}
	decodeOptions(in, &options)
	return options.Stream
}

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
	fmt.Fprintf(os.Stderr, "open-ferry-parity-interactions: "+format+"\n", args...)
	os.Exit(2)
}
