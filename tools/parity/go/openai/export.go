// Exports upstream's legacy Completions conversions to open-ferry's parity
// harness (go/completions/main.go). They are unexported, so go build -overlay
// adds this file to upstream's sdk/api/handlers/openai package, leaving the
// checkout as it is. See tools/parity/README.md.

package openai

// OpenFerryConvertCompletionsRequest runs convertCompletionsRequestToChatCompletions.
func OpenFerryConvertCompletionsRequest(rawJSON []byte) []byte {
	return convertCompletionsRequestToChatCompletions(rawJSON)
}

// OpenFerryConvertCompletionsResponse runs convertChatCompletionsResponseToCompletions.
func OpenFerryConvertCompletionsResponse(rawJSON []byte) []byte {
	return convertChatCompletionsResponseToCompletions(rawJSON)
}

// OpenFerryConvertCompletionsStreamChunk runs convertChatCompletionsStreamChunkToCompletions.
func OpenFerryConvertCompletionsStreamChunk(chunkData []byte) []byte {
	return convertChatCompletionsStreamChunkToCompletions(chunkData)
}
