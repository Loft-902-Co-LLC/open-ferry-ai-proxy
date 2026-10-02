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
// codex/openai-chat/response writes a JSON array with one string per chunk.
//
// The signature/* entries run upstream's reasoning-signature package and write
// a JSON report of what it returned. "options", a JSON object, holds inputs
// other than the request; each entry documents its own. signature/inspect
// takes the raw signature as "request".
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

	"github.com/router-for-me/CLIProxyAPI/v8/internal/signature"
	codexclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/claude"
	codexchat "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/openai/chat-completions"
	codexresponses "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/openai/responses"
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

var translators = map[string]func(in input) []byte{
	"codex/claude/request": func(in input) []byte {
		return codexclaude.ConvertClaudeRequestToCodex(in.Model, []byte(in.Request), true)
	},
	"codex/claude/request-compat": func(in input) []byte {
		return codexclaude.ConvertClaudeRequestToCodexWithCompat(in.Model, []byte(in.Request), true)
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
	"codex/openai-chat/request": func(in input) []byte {
		return codexchat.ConvertOpenAIRequestToCodex(in.Model, []byte(in.Request), true)
	},
	"codex/openai-chat/response": func(in input) []byte {
		var param any
		chunks := []string{}
		for _, event := range in.Events {
			for _, chunk := range codexchat.ConvertCodexResponseToOpenAI(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), []byte(event), &param) {
				chunks = append(chunks, string(chunk))
			}
		}
		out, err := json.Marshal(chunks)
		if err != nil {
			panic(err)
		}
		return out
	},
	"codex/openai-chat/response-non-stream": func(in input) []byte {
		return codexchat.ConvertCodexResponseToOpenAINonStream(context.Background(), in.Model, []byte(in.Request), translatedRequest(in), finalEvent(in), nil)
	},
	"signature/inspect":         inspectSignature,
	"signature/claude-messages": sanitizeClaudeMessages,
	"signature/gemini":          sanitizeGemini,
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

// Every block kind, in the order reports list per-kind results.
var blockKinds = []signature.SignatureBlockKind{
	signature.SignatureBlockKindUnknown,
	signature.SignatureBlockKindClaudeThinking,
	signature.SignatureBlockKindGeminiModelPart,
	signature.SignatureBlockKindGeminiFunctionCall,
	signature.SignatureBlockKindGPTReasoning,
}

// The Gemini validation options signature/inspect tries, in order.
var geminiOptions = []signature.GeminiThoughtSignatureValidationOptions{
	{},
	{AllowBypassSentinel: true},
	{RequireKnownEnvelope: true},
	{RequireObservedMarker: true},
	{AllowBypassSentinel: true, RequireKnownEnvelope: true, RequireObservedMarker: true},
}

// signature/inspect: every check and replay decision upstream makes on one
// signature. "model" is a target model; options are {"target": provider},
// the target for the provider-specific decisions.
func inspectSignature(in input) []byte {
	var options struct {
		Target signature.SignatureProvider `json:"target"`
	}
	decodeOptions(in, &options)
	raw, target := in.Request, options.Target

	type prefixReport struct {
		Provider signature.SignatureProvider `json:"provider"`
		Payload  string                      `json:"payload"`
	}
	var prefix *prefixReport
	if provider, payload, ok := signature.SplitSignatureProviderPrefix(raw); ok {
		prefix = &prefixReport{Provider: provider, Payload: payload}
	}
	strict := signature.ClaudeSignatureValidationOptions{Strict: true}
	modelProvider := signature.SignatureProviderFromModelName(in.Model)

	type claudeReport struct {
		HasPrefix        bool `json:"has_prefix"`
		Decodable        bool `json:"decodable"`
		Valid            bool `json:"valid"`
		ValidStrict      bool `json:"valid_strict"`
		Normalized       any  `json:"normalized"`
		NormalizedStrict any  `json:"normalized_strict"`
		Native           any  `json:"native"`
		SingleLayer      any  `json:"single_layer"`
		DoubleLayer      any  `json:"double_layer"`
		CAIS             any  `json:"cais"`
		Antigravity      any  `json:"antigravity"`
	}
	type geminiReport struct {
		Bypass  bool     `json:"bypass"`
		Inspect []any    `json:"inspect"`
		Replay  []string `json:"replay"`
	}
	type targetReport struct {
		Provider   signature.SignatureProvider                `json:"provider"`
		Compatible bool                                       `json:"compatible"`
		Signature  any                                        `json:"signature"`
		ForBlock   []any                                      `json:"for_block"`
		Decisions  []signature.SignatureCompatibilityDecision `json:"decisions"`
	}
	inspections := make([]any, 0, len(geminiOptions))
	for _, opts := range geminiOptions {
		inspections = append(inspections, fallible(signature.InspectGeminiThoughtSignature(raw, opts)))
	}

	return marshal(struct {
		Detect         signature.SignatureProvider                `json:"detect"`
		DetectForBlock []signature.SignatureProvider              `json:"detect_for_block"`
		Recognized     bool                                       `json:"recognized"`
		Prefix         *prefixReport                              `json:"prefix"`
		WithoutPrefix  string                                     `json:"without_prefix"`
		Claude         claudeReport                               `json:"claude"`
		Gemini         geminiReport                               `json:"gemini"`
		GPT            any                                        `json:"gpt"`
		Grok           any                                        `json:"grok"`
		Kimi           any                                        `json:"kimi"`
		ModelProvider  signature.SignatureProvider                `json:"model_provider"`
		ForModel       []signature.SignatureCompatibilityDecision `json:"for_model"`
		Target         targetReport                               `json:"target"`
	}{
		Detect: signature.DetectSignatureProvider(raw),
		DetectForBlock: perKind(func(kind signature.SignatureBlockKind) signature.SignatureProvider {
			return signature.DetectSignatureProviderForBlock(raw, kind)
		}),
		Recognized:    signature.IsRecognizedReasoningSignature(raw),
		Prefix:        prefix,
		WithoutPrefix: signature.SignaturePayloadWithoutProviderPrefix(raw),
		Claude: claudeReport{
			HasPrefix:        signature.HasClaudeThinkingSignaturePrefix(raw),
			Decodable:        signature.HasDecodableClaudeThinkingSignature(raw),
			Valid:            signature.IsValidClaudeThinkingSignature(raw),
			ValidStrict:      signature.IsValidClaudeThinkingSignature(raw, strict),
			Normalized:       fallible(signature.NormalizeClaudeThinkingSignature(raw)),
			NormalizedStrict: fallible(signature.NormalizeClaudeThinkingSignature(raw, strict)),
			Native:           fallible(signature.NormalizeClaudeProviderNativeThinkingSignature(raw)),
			SingleLayer:      fallible(signature.InspectClaudeSingleLayerSignature(raw)),
			DoubleLayer:      fallible(signature.InspectClaudeDoubleLayerSignature(raw)),
			CAIS:             fallible(signature.InspectClaudeCAISSignature(raw)),
			Antigravity:      optional(signature.CompatibleAntigravityClaudeThinkingSignature(raw)),
		},
		Gemini: geminiReport{
			Bypass:  signature.IsGeminiThoughtSignatureBypass(raw),
			Inspect: inspections,
			Replay: perKind(func(kind signature.SignatureBlockKind) string {
				return signature.GeminiReplaySignatureOrBypass(raw, kind)
			}),
		},
		GPT:           fallible(signature.InspectGPTReasoningSignature(raw)),
		Grok:          fallible(signature.InspectGrokEncryptedContent(raw)),
		Kimi:          fallible(signature.InspectKimiThinkingSignature(raw)),
		ModelProvider: modelProvider,
		ForModel: perKind(func(kind signature.SignatureBlockKind) signature.SignatureCompatibilityDecision {
			return signature.DecideSignatureCompatibilityForModel(modelProvider, in.Model, raw, kind)
		}),
		Target: targetReport{
			Provider:   target,
			Compatible: signature.IsSignatureCompatibleWithProvider(target, raw),
			Signature:  optional(signature.CompatibleSignatureForProvider(target, raw)),
			ForBlock: perKind(func(kind signature.SignatureBlockKind) any {
				return optional(signature.CompatibleSignatureForProviderBlock(target, raw, kind))
			}),
			Decisions: perKind(func(kind signature.SignatureBlockKind) signature.SignatureCompatibilityDecision {
				return signature.DecideSignatureCompatibility(target, raw, kind)
			}),
		},
	})
}

// signature/claude-messages: the Claude Messages strippers, validator and
// sanitizers on one request, for target model "model". Options are
// {"validation": ClaudeSignatureValidationOptions, "target":
// ClaudeMessagesSignatureSanitizeOptions}; TargetModel is set from "model".
func sanitizeClaudeMessages(in input) []byte {
	var options struct {
		Validation signature.ClaudeSignatureValidationOptions       `json:"validation"`
		Target     signature.ClaudeMessagesSignatureSanitizeOptions `json:"target"`
	}
	decodeOptions(in, &options)
	options.Target.TargetModel = in.Model
	// Each call gets its own copy, in case one writes to its input.
	payload := func() []byte { return []byte(in.Request) }

	return marshal(struct {
		Strip          json.RawMessage `json:"strip"`
		StripAndEmpty  json.RawMessage `json:"strip_and_empty"`
		Validate       any             `json:"validate"`
		ForModel       any             `json:"for_model"`
		ClaudeUpstream any             `json:"claude_upstream"`
		ForTarget      any             `json:"for_target"`
	}{
		Strip:          signature.StripInvalidClaudeThinkingBlocks(payload(), options.Validation),
		StripAndEmpty:  signature.StripInvalidClaudeThinkingBlocksAndEmptyMessages(payload(), options.Validation),
		Validate:       errorText(signature.ValidateClaudeThinkingSignatures(payload(), options.Validation)),
		ForModel:       sanitized(signature.SanitizeClaudeMessagesSignaturesForModel(payload(), in.Model)),
		ClaudeUpstream: sanitized(signature.SanitizeClaudeMessagesForClaudeUpstream(payload(), in.Model, options.Target.PreserveEmptyThinkingBlocks)),
		ForTarget:      sanitized(signature.SanitizeClaudeMessagesSignaturesForTarget(payload(), options.Target)),
	})
}

// signature/gemini: the Gemini sanitizer and validators on one request.
// Options are {"contents_path": string, "validation":
// GeminiThoughtSignatureValidationOptions}.
func sanitizeGemini(in input) []byte {
	var options struct {
		ContentsPath string                                            `json:"contents_path"`
		Validation   signature.GeminiThoughtSignatureValidationOptions `json:"validation"`
	}
	decodeOptions(in, &options)
	clean := signature.SanitizeGeminiRequestThoughtSignatures([]byte(in.Request), options.ContentsPath)

	return marshal(struct {
		Sanitized         json.RawMessage `json:"sanitized"`
		Validate          any             `json:"validate"`
		ValidateSanitized any             `json:"validate_sanitized"`
		Pairing           any             `json:"pairing"`
	}{
		Sanitized:         clean,
		Validate:          errorText(signature.ValidateGeminiThoughtSignatures([]byte(in.Request), options.Validation)),
		ValidateSanitized: errorText(signature.ValidateGeminiThoughtSignatures(clean, options.Validation)),
		Pairing:           errorText(signature.ValidateGeminiFunctionCallPairing([]byte(in.Request))),
	})
}

func perKind[T any](f func(signature.SignatureBlockKind) T) []T {
	out := make([]T, 0, len(blockKinds))
	for _, kind := range blockKinds {
		out = append(out, f(kind))
	}
	return out
}

// fallible reports a result as {"ok": value} or {"error": message}.
func fallible[T any](value T, err error) any {
	if err != nil {
		return map[string]string{"error": err.Error()}
	}
	return map[string]T{"ok": value}
}

// optional reports a (value, ok) pair as the value or null.
func optional(value string, ok bool) any {
	if !ok {
		return nil
	}
	return value
}

// errorText reports an error as its message, or null.
func errorText(err error) any {
	if err != nil {
		return err.Error()
	}
	return nil
}

// sanitized reports a sanitizer's output and report. No decisions read as [].
func sanitized(payload []byte, report signature.SignatureSanitizeReport) any {
	if report.Decisions == nil {
		report.Decisions = []signature.SignatureCompatibilityDecision{}
	}
	return struct {
		Payload json.RawMessage                   `json:"payload"`
		Report  signature.SignatureSanitizeReport `json:"report"`
	}{payload, report}
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
	fmt.Fprintf(os.Stderr, "open-ferry-parity: "+format+"\n", args...)
	os.Exit(2)
}
