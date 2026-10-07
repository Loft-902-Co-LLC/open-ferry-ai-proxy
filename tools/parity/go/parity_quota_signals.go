// The quota-signals/observe entry runs upstream's
// QuotaState.ObserveResponseHeadersForProvider
// (sdk/cliproxy/auth/quota_signals.go) on one response's headers.
//
// "options" holds:
//
//	{"provider": "...",
//	 "headers": [{"name": "...", "value": "..."} or {"name": "...", "hex": "..."}, ...],
//	 "prior": {"observed_at": unix seconds or 0, "signals": {"name": "value", ...}}}
//
// The headers are added in order with http.Header.Add, so names in other
// cases join one canonical name, as net/http reads a response's. "hex"
// gives a value's bytes, for those that aren't UTF-8. "prior" is the
// snapshot before the response; an observed_at of 0 is the zero time. The
// response comes at Unix time 1000.
//
// The output is {"changed": bool, "observed_at": unix seconds or 0,
// "signals": {"name": "value", ...}}, the snapshot after the response.
package main

import (
	"encoding/hex"
	"net/http"
	"time"

	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

type quotaSignalsHeader struct {
	Name  string  `json:"name"`
	Value *string `json:"value"`
	Hex   *string `json:"hex"`
}

type quotaSignalsOptions struct {
	Provider string               `json:"provider"`
	Headers  []quotaSignalsHeader `json:"headers"`
	Prior    struct {
		ObservedAt int64             `json:"observed_at"`
		Signals    map[string]string `json:"signals"`
	} `json:"prior"`
}

func init() {
	translators["quota-signals/observe"] = quotaSignalsObserve
}

func quotaSignalsObserve(in input) []byte {
	var options quotaSignalsOptions
	decodeOptions(in, &options)
	headers := http.Header{}
	for _, header := range options.Headers {
		value := ""
		switch {
		case header.Hex != nil:
			raw, err := hex.DecodeString(*header.Hex)
			if err != nil {
				panic(err)
			}
			value = string(raw)
		case header.Value != nil:
			value = *header.Value
		}
		headers.Add(header.Name, value)
	}
	quota := coreauth.QuotaState{Signals: options.Prior.Signals}
	if options.Prior.ObservedAt != 0 {
		quota.ObservedAt = time.Unix(options.Prior.ObservedAt, 0)
	}
	changed := quota.ObserveResponseHeadersForProvider(options.Provider, headers, time.Unix(1000, 0))
	observedAt := int64(0)
	if !quota.ObservedAt.IsZero() {
		observedAt = quota.ObservedAt.Unix()
	}
	signals := quota.Signals
	if signals == nil {
		signals = map[string]string{}
	}
	return marshal(struct {
		Changed    bool              `json:"changed"`
		ObservedAt int64             `json:"observed_at"`
		Signals    map[string]string `json:"signals"`
	}{changed, observedAt, signals})
}
