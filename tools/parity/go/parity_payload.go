// The payload/apply entry will run upstream's
// helps.ApplyPayloadConfigWithTrackedPathsForExecutor
// (internal/runtime/executor/helps/payload_helpers.go) on a translated
// body, with the case's rules, model, protocol, headers and tracked
// paths, and give the body and the paths touched. Not ported yet (P3
// WP-D): it gives null, and no case is generated.
package main

func init() {
	translators["payload/apply"] = payloadApply
}

func payloadApply(in input) []byte {
	return []byte("null")
}
