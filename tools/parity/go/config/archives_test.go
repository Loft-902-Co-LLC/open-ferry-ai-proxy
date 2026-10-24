package config

import (
	"encoding/json"
	"gopkg.in/yaml.v3"
	"os"
	"path/filepath"
	"testing"
)

func TestParityArchiveBudgetsAreComputedFromSourceKeys(t *testing.T) {
	for index, source := range []string{
		"x-note: fixture\nconfig-version: 8\n",
		"x-note:\n  nested: fixture # own\nconfig-version: 8\n",
		"x-note: |\n  fixture\nconfig-version: 8\n",
		"x-note: {nested: [fixture]}\nconfig-version: 8\n",
		"? x-note # own key\n: fixture # own value\nconfig-version: 8\n",
		"debug: false # parity-archive-0\nx-note: fixture\nconfig-version: 8\n",
	} {
		proof := ParityOwnership(source, map[string]any{"op": "save", "migrate": true})
		if proof["unavailable"] != nil || proof["archive_unavailable"] != nil {
			t.Fatalf("archive construction unavailable in fixture %d: %v", index, proof["archive_failure"])
		}
		slots := proof["archives"].([]any)
		if len(slots) == 0 {
			t.Fatal("computed archive has no budget")
		}
		for _, raw := range slots {
			slot := raw.(map[string]any)
			path := slot["source_key"].([]any)
			if len(path) != 1 || path[0].(map[string]any)["key"] != "x-note" {
				t.Fatal("archive budget lost its source key")
			}
			if slot["binding"] == nil || slot["owner"] == nil {
				t.Fatal("unbound archive budget")
			}
		}
		// Independent positive control against the actual pinned file write.
		// The output is never used to construct the budgets above.
		path := filepath.Join(t.TempDir(), "config.yaml")
		if err := os.WriteFile(path, []byte(source), 0600); err != nil {
			t.Fatal("fixture write")
		}
		cfg, err := ParseConfigBytes([]byte(source))
		if err != nil {
			t.Fatal("fixture config")
		}
		if err = SaveConfigPreserveComments(path, cfg, true); err != nil {
			t.Fatal("pinned save")
		}
		data, err := os.ReadFile(path)
		if err != nil {
			t.Fatal("fixture read")
		}
		var doc yaml.Node
		if yaml.Unmarshal(data, &doc) != nil {
			t.Fatal("pinned output parse")
		}
		actual := parityBlocks(&doc)
		for _, raw := range slots {
			slot := raw.(map[string]any)
			key, _ := json.Marshal(slot["owner"])
			block, ok := actual[string(key)]
			found := false
			if ok {
				for _, line := range block.Lines {
					if line == slot["text"] {
						found = true
					}
				}
			}
			if !found {
				t.Fatal("independent archive line/owner differs from pinned save")
			}
		}
	}
}

func TestParityArchiveRefusesAnUnboundDocumentFootLayout(t *testing.T) {
	proof := ParityOwnership("# top\nx-note: fixture\nconfig-version: 8\n\n# tail\n", map[string]any{"op": "save", "migrate": true})
	if proof["archive_unavailable"] != true || len(proof["archives"].([]any)) != 0 {
		t.Fatal("ambiguous document-foot layout received an archive slot")
	}
}

func TestParityArchiveDoesNotTreatSourceCommentOrKnownKeyAsGenerated(t *testing.T) {
	for _, source := range []string{
		"# x-note: fixture\nconfig-version: 8\n",
		"debug: false # x-note: fixture\nconfig-version: 8\n",
	} {
		proof := ParityOwnership(source, map[string]any{"op": "save", "migrate": true})
		if proof["unavailable"] != nil || len(proof["archives"].([]any)) != 0 {
			t.Fatal("source comment or unarchived key received an archive slot")
		}
	}
	proof := ParityOwnership("x-note: fixture\n", map[string]any{"op": "nested", "keys": []any{"debug"}})
	if proof["unavailable"] != nil || len(proof["archives"].([]any)) != 0 {
		t.Fatal("wrong operation received an archive slot")
	}
}

func TestParityArchiveDoesNotFundAnUpstreamValueChangingReencode(t *testing.T) {
	proof := ParityOwnership("x-note: >+\n  fixture\n\nconfig-version: 8\n", map[string]any{"op": "save", "migrate": true})
	if proof["unavailable"] != nil {
		t.Fatal("tree replay unavailable")
	}
	if len(proof["archives"].([]any)) != 0 {
		t.Fatal("value-changing archive received a budget")
	}
}
