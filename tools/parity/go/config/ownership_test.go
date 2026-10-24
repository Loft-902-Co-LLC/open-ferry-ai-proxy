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

func TestParityOwnershipTracksDroppedKeyCommentsBySurvivingValueIdentity(t *testing.T) {
	proof := ParityOwnership("openai-compatibility:\n  - name: fixture\n    base-url: https://example.invalid/v1\n    # head\n    api-key-entries:\n      - api-key: fixture\n", map[string]any{"op": "save", "migrate": true})
	if proof["unavailable"] != nil {
		t.Fatal("identity replay unavailable")
	}
	found := false
	for _, raw := range proof["moves"].([]any) {
		move := raw.(map[string]any)
		from := move["source"].(ParityOwner)
		to := move["destination"].(ParityOwner)
		if len(from.Path) == 3 && from.Path[2].(map[string]any)["key"] == "api-key-entries" && from.Node == "key" && from.Slot == "head" {
			if len(to.Path) != 4 || to.Path[3].(map[string]any)["key"] != "keys" || to.Node != "key" || to.Slot != "head" {
				t.Fatal("wrong surviving destination")
			}
			found = true
		}
	}
	if !found {
		t.Fatal("lost key comment received no surviving logical-entry destination")
	}
}
