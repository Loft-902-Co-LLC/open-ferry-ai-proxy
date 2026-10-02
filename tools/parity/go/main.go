// Command open-ferry-parity runs upstream CLIProxyAPI translators on input
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
// Response translators also take "events": the Codex event stream lines for
// the streaming translators, or the final event alone for the non-streaming
// ones. "request" is then the client's original request, and
// "translated_request", if given, the request as sent to Codex.
// codex/claude/response concatenates its output for every line.
// codex/openai-responses/response writes a JSON array with one string per
// output chunk, or "=" for a chunk identical to its input line.
//
// Each output line is {"output":"<base64 of the translator's raw bytes>"},
// or {"panic":"<message>"} if the translator panicked. Output is base64 so
// invalid UTF-8 survives the trip.
package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os"

	codexclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/claude"
	codexresponses "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/openai/responses"
)

type input struct {
	Translator string   `json:"translator"`
	Model      string   `json:"model"`
	Request    string   `json:"request"`
	Translated string   `json:"translated_request"`
	Events     []string `json:"events"`
}

type output struct {
	Output []byte `json:"output"`
	Panic  string `json:"panic,omitempty"`
}

var translators = map[string]func(in input) []byte{
	"codex/claude/request": func(in input) []byte {
		return codexclaude.ConvertClaudeRequestToCodex(in.Model, []byte(in.Request), true)
	},
	"codex/claude/response": func(in input) []byte {
		var param any
		var out []byte
		for _, event := range in.Events {
			chunks := codexclaude.ConvertCodexResponseToClaude(context.Background(), in.Model, []byte(in.Request), nil, []byte(event), &param)
			for _, chunk := range chunks {
				out = append(out, chunk...)
			}
		}
		return out
	},
	"codex/claude/response-non-stream": func(in input) []byte {
		return codexclaude.ConvertCodexResponseToClaudeNonStream(context.Background(), in.Model, []byte(in.Request), nil, finalEvent(in), nil)
	},
	"codex/openai-responses/request": func(in input) []byte {
		return codexresponses.ConvertOpenAIResponsesRequestToCodex(in.Model, []byte(in.Request), true)
	},
	"codex/openai-responses/response": func(in input) []byte {
		chunks := []string{}
		for _, event := range in.Events {
			for _, chunk := range codexresponses.ConvertCodexResponseToOpenAIResponses(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), []byte(event), nil) {
				if bytes.Equal(chunk, []byte(event)) {
					chunks = append(chunks, "=")
				} else {
					chunks = append(chunks, string(chunk))
				}
			}
		}
		out, err := json.Marshal(chunks)
		if err != nil {
			panic(err)
		}
		return out
	},
	"codex/openai-responses/response-non-stream": func(in input) []byte {
		return codexresponses.ConvertCodexResponseToOpenAIResponsesNonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	},
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
	fmt.Fprintf(os.Stderr, "open-ferry-parity: "+format+"\n", args...)
	os.Exit(2)
}
