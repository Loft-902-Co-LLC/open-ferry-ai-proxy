package config

import (
	"encoding/json"
	"testing"
)

func TestParityOwnershipSourceIdentities(t *testing.T) {
	operation := map[string]any{"op": "save", "migrate": true}
	for _, source := range []string{
		"disable-cooling: false # same\nrequest-retry: 0 # same\n",
		"gemini-api-key:\n  - api-key: fixture # own\n",
	} {
		proof := ParityOwnership(source, operation)
		if proof["unavailable"] != nil {
			t.Fatal("tree oracle unavailable")
		}
		moves := proof["moves"].([]any)
		if len(moves) == 0 {
			t.Fatal("no surviving source identities")
		}
		moved := false
		for _, raw := range moves {
			move := raw.(map[string]any)
			from := move["source"].(ParityOwner)
			to := move["destination"].(ParityOwner)
			a, _ := json.Marshal(from.Path)
			b, _ := json.Marshal(to.Path)
			if string(a) != string(b) {
				moved = true
			}
		}
		if !moved {
			t.Fatal("upstream tree operations did not relocate source identities")
		}
		if proof["source"] != source {
			t.Fatal("source binding lost")
		}
	}
	// The field discarded by the operation has no slot, even though an equal
	// comment on a surviving field can travel elsewhere.
	proof := ParityOwnership("disable-cooling: false # same\nrouting:\n  cooldown:\n    disable-cooling: true # same\n", operation)
	for _, move := range proof["moves"].([]any) {
		encoded, _ := json.Marshal(move.(map[string]any)["source"])
		if string(encoded) == `{"path":[{"key":"disable-cooling","tag":"!!str"}],"node":"value","slot":"line"}` {
			t.Fatal("deleted identity received a destination")
		}
	}
}

func TestParityOwnershipDeletedAncestorHasNoSlot(t *testing.T) {
	proof := ParityOwnership("codex: # parent\n  disable-codex-cloaking: false\nconfig-version: 8\n", map[string]any{"op": "write"})
	if proof["unavailable"] != nil {
		t.Fatal("tree oracle unavailable")
	}
	for _, move := range proof["moves"].([]any) {
		owner := move.(map[string]any)["source"].(ParityOwner)
		if len(owner.Path) == 1 && owner.Path[0].(map[string]any)["key"] == "codex" {
			t.Fatal("deleted ancestor identity received a relocation slot")
		}
	}
}
