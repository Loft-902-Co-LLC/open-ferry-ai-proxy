// Command open-ferry-parity-completions runs upstream CLIProxyAPI's legacy
// Completions conversions on input read from stdin, so open-ferry's Rust port
// can be compared against them.
//
// The conversions live in sdk/api/handlers/openai, which is compiled with
// go/openai/export.go added to export them. That package's imports register
// translators that go/main.go doesn't import, which would change what its
// registry/* entries see, so these entries get a harness of their own. Like
// go/main.go, this file is compiled inside a CLIProxyAPI checkout via
// go build -overlay. See tools/parity/README.md.
//
// Input and output are JSON lines in go/main.go's format.
// completions/request converts "request". completions/response converts the
// Chat Completions response given as the one event. completions/stream-chunk
// converts each event as one chunk's JSON, and writes a JSON array with one
// string per chunk, or null for a chunk upstream skips.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"

	"github.com/router-for-me/CLIProxyAPI/v8/sdk/api/handlers/openai"
)

type input struct {
	Translator string   `json:"translator"`
	Request    string   `json:"request"`
	Events     []string `json:"events"`
}

type output struct {
	Output []byte `json:"output"`
	Panic  string `json:"panic,omitempty"`
}

var translators = map[string]func(in input) []byte{
	"completions/request": func(in input) []byte {
		return openai.OpenFerryConvertCompletionsRequest([]byte(in.Request))
	},
	"completions/response": func(in input) []byte {
		var body []byte
		if len(in.Events) > 0 {
			body = []byte(in.Events[0])
		}
		return openai.OpenFerryConvertCompletionsResponse(body)
	},
	"completions/stream-chunk": func(in input) []byte {
		chunks := []any{}
		for _, event := range in.Events {
			if chunk := openai.OpenFerryConvertCompletionsStreamChunk([]byte(event)); chunk != nil {
				chunks = append(chunks, string(chunk))
			} else {
				chunks = append(chunks, nil)
			}
		}
		out, err := json.Marshal(chunks)
		if err != nil {
			panic(err)
		}
		return out
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

func run(translate func(input) []byte, in input) (out output) {
	defer func() {
		if r := recover(); r != nil {
			out = output{Panic: fmt.Sprint(r)}
		}
	}()
	return output{Output: translate(in)}
}

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "open-ferry-parity-completions: "+format+"\n", args...)
	os.Exit(2)
}
