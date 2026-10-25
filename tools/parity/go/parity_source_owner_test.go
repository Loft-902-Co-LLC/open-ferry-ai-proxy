package main

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"
)

func TestSourceOwnerBundleReplaysTheRealOperationWithOpaqueCommentIdentities(t *testing.T) {
	const source = "auth-dir: /a\nproxy-url: http://p\noauth-model-alias:\n  antigravity:\n    - {name: a, alias: b}\n    # f0\n\n    - {name: c, alias: d}\n    # f1\n\noauth-request-scoped-errors: {}\n"
	op := map[string]any{"op": "nested", "keys": []any{"auth-dir"}, "value": "/x"}
	protected := strings.Replace(source, "auth-dir: /a", "auth-dir: /x", 1)
	request := input{Options: configSaveJSON(map[string]any{"_ownership_request": map[string]any{"source": source, "operation": op, "source_owner": true, "protected": protected}})}
	var proof map[string]any
	if json.Unmarshal(configSaveSteps(request), &proof) != nil {
		t.Fatal("source-owner bundle did not return JSON")
	}
	frame, ok := proof["retained_source_owner"].(map[string]any)
	if !ok {
		t.Fatal("source-owner bundle is missing")
	}
	identity := frame["identity"].(map[string]any)
	if identity["unavailable"] != nil || identity["source"] != source {
		t.Fatal("source occurrences were not bound to the exact input")
	}
	actual := frame["go"].(map[string]any)
	marked := frame["marked_go"].(map[string]any)
	if actual["error"] != nil || actual["unloadable"] != nil || marked["error"] != nil || marked["unloadable"] != nil {
		t.Fatal("a real one-step operation did not complete and reload")
	}
	goText := actual["files"].([]any)[0].(string)
	unmasked := marked["files"].([]any)[0].(string)
	for _, raw := range identity["occurrences"].([]any) {
		occurrence := raw.(map[string]any)
		unmasked = strings.ReplaceAll(unmasked, occurrence["id"].(string), occurrence["text"].(string))
	}
	if goText != unmasked {
		t.Fatal("opaque replay did not reproduce the real Go bytes")
	}
	goInventory := frame["go_inventory"].(map[string]any)
	protectedInventory := frame["protected_inventory"].(map[string]any)
	if !reflect.DeepEqual(goInventory["values"], protectedInventory["values"]) || len(protectedInventory["comments"].([]any)) != 2 {
		t.Fatal("pinned-parser values or source-owner inventory did not match the control")
	}
}
