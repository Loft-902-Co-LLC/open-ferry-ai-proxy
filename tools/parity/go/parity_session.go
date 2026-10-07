// The session/* entries run upstream's sdk/cliproxy/session on a request
// body, "request", as session affinity reads it.
//
// session/info runs ExtractSessionInfo (info.go) with the headers and the
// connection's execution session in "options" (see sessionOptions), and
// writes the session it found (see sessionInfo), or null when it found
// none. The caller scope, credential, provider, model, node kind and
// metadata it may fill aren't written: open-ferry's SessionInfo doesn't
// hold them.
//
// session/derive runs DeriveID (identity.go) for the client format and
// caller scope in "options", and writes the identity as a JSON string,
// empty when there is none.
package main

import (
	"encoding/json"
	"net/http"

	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/session"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

func init() {
	translators["session/info"] = sessionInfoEntry
	translators["session/derive"] = sessionDeriveEntry
}

// sessionOptions holds the inputs other than the body: the request's
// headers as name and value pairs, added in order; the connection's
// execution session, put in the metadata when it isn't empty; and, for
// session/derive, the client format and the caller scope.
type sessionOptions struct {
	Headers     [][2]string `json:"headers"`
	ExecutionID string      `json:"execution_id"`
	Format      string      `json:"format"`
	CallerScope string      `json:"caller_scope"`
}

// sessionInfo is the part of session.SessionInfo open-ferry ports.
type sessionInfo struct {
	SessionID       string `json:"session_id"`
	ParentSessionID string `json:"parent_session_id"`
	AgentName       string `json:"agent_name"`
	ClientType      string `json:"client_type"`
	IsFork          bool   `json:"is_fork"`
	IsSubagent      bool   `json:"is_subagent"`
}

func sessionInfoEntry(in input) []byte {
	var options sessionOptions
	decodeOptions(in, &options)
	headers := http.Header{}
	for _, pair := range options.Headers {
		headers.Add(pair[0], pair[1])
	}
	var metadata map[string]any
	if options.ExecutionID != "" {
		metadata = map[string]any{cliproxyexecutor.ExecutionSessionMetadataKey: options.ExecutionID}
	}
	info, ok := session.ExtractSessionInfo(headers, []byte(in.Request), metadata)
	if !ok {
		return []byte("null")
	}
	return marshal(sessionInfo{
		SessionID:       info.SessionID,
		ParentSessionID: info.ParentSessionID,
		AgentName:       info.AgentName,
		ClientType:      info.ClientType,
		IsFork:          info.IsFork,
		IsSubagent:      info.IsSubagent,
	})
}

func sessionDeriveEntry(in input) []byte {
	var options sessionOptions
	decodeOptions(in, &options)
	id := session.DeriveID(sdktranslator.FromString(options.Format), []byte(in.Request), options.CallerScope)
	out, err := json.Marshal(id)
	if err != nil {
		panic(err)
	}
	return out
}
