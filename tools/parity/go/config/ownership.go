// Offline source-identity oracle. No protected output enters this helper.
package config

import (
	"fmt"
	"reflect"
	"strings"

	"gopkg.in/yaml.v3"
)

type ParityOwner struct {
	Path []any  `json:"path"`
	Node string `json:"node"`
	Slot string `json:"slot"`
}

type parityOrigin struct {
	Owner    ParityOwner
	Ordinal  int
	Identity int
	Text     string
}

// ParityOwnership runs tree edits extracted from the pinned implementation at
// harness build time. Line is private identity metadata on this isolated tree;
// it is cleared on generated nodes. Opaque per-occurrence IDs travel through
// upstream's comment-copy operations, never through matching comment text.
// A destination exists only if the enclosing source value identity survives.
func ParityOwnership(source string, operation map[string]any) map[string]any {
	result := map[string]any{"source": source, "operation": operation, "moves": []any{}}
	fail := func() map[string]any { result["unavailable"] = true; return result }
	var doc yaml.Node
	if yaml.Unmarshal([]byte(source), &doc) != nil || len(doc.Content) != 1 {
		return fail()
	}
	root := expandConfigAliases(doc.Content[0])
	origins := map[string]parityOrigin{}
	identity := 0
	parityWalk(root, []any{}, "root", func(n *yaml.Node, path []any, side string, value *yaml.Node) {
		identity++
		n.Line = identity
	})
	raw := deepCopyNode(root)
	parityWalk(root, []any{}, "root", func(n *yaml.Node, path []any, side string, value *yaml.Node) {
		for slot, field := range map[string]*string{"head": &n.HeadComment, "line": &n.LineComment, "foot": &n.FootComment} {
			lines := strings.Split(*field, "\n")
			ordinal := 0
			for i, line := range lines {
				if strings.TrimSpace(line) == "" {
					continue
				}
				id := fmt.Sprintf("# parity-origin-%d-%s-%d", n.Line, slot, ordinal)
				origins[id] = parityOrigin{ParityOwner{path, side, slot}, ordinal, value.Line, strings.TrimLeft(line, " \t")}
				lines[i] = id
				ordinal++
			}
			*field = strings.Join(lines, "\n")
		}
	})
	root, err := parityRunTree(source, operation, root, false)
	if err != nil {
		return fail()
	}
	result["moves"] = parityRelocations(root, origins)
	archives, archiveErr := parityArchives(source, operation, raw)
	if archiveErr != nil {
		result["archives"] = []any{}
		result["archive_unavailable"] = true
		result["archive_failure"] = archiveErr.Error()
	} else {
		result["archives"] = archives
	}
	return result
}

func parityRelocations(root *yaml.Node, origins map[string]parityOrigin) []any {
	surviving := map[int]bool{}
	destinations := map[int][]ParityOwner{}
	parityWalk(root, []any{}, "root", func(n *yaml.Node, path []any, side string, _ *yaml.Node) {
		surviving[n.Line] = true
		destinations[n.Line] = append(destinations[n.Line], ParityOwner{path, side, ""})
	})
	observed := map[string]bool{}
	moves := []any{}
	parityWalk(root, []any{}, "root", func(n *yaml.Node, path []any, side string, _ *yaml.Node) {
		for slot, text := range map[string]string{"head": n.HeadComment, "line": n.LineComment, "foot": n.FootComment} {
			for _, line := range strings.Split(text, "\n") {
				origin, ok := origins[strings.TrimSpace(line)]
				if !ok || !surviving[origin.Identity] {
					continue
				}
				bound := false
				for _, target := range destinations[origin.Identity] {
					if reflect.DeepEqual(target.Path, path) {
						bound = true
					}
				}
				if !bound {
					continue
				}
				observed[strings.TrimSpace(line)] = true
				moves = append(moves, map[string]any{"source": origin.Owner, "ordinal": origin.Ordinal, "text": origin.Text, "via": "observed", "destination": ParityOwner{path, side, slot}})
			}
		}
	})
	// A transfer defines a destination only at the comment's own surviving
	// node or the key holding it. A transfer to any other path is ignored.
	// For a key, value or item comment with no bound transfer (including one
	// Go misplaced), use its own node's surviving place in its original slot.
	// Only a bound transfer takes precedence; no equal text is read.
	for id, origin := range origins {
		if observed[id] {
			continue
		}
		for _, target := range destinations[origin.Identity] {
			if origin.Owner.Node == "key" {
				if target.Node != "value" {
					continue
				}
			} else if origin.Owner.Node != target.Node {
				continue
			}
			moves = append(moves, map[string]any{"source": origin.Owner, "ordinal": origin.Ordinal, "text": origin.Text, "via": "fallback", "destination": ParityOwner{target.Path, origin.Owner.Node, origin.Owner.Slot}})
		}
	}
	return moves
}

func parityWalk(n *yaml.Node, path []any, side string, visit func(*yaml.Node, []any, string, *yaml.Node)) {
	if n == nil {
		return
	}
	visit(n, path, side, n)
	switch n.Kind {
	case yaml.MappingNode:
		for i := 0; i+1 < len(n.Content); i += 2 {
			k, v := n.Content[i], n.Content[i+1]
			p := append(append([]any{}, path...), map[string]any{"key": k.Value, "tag": k.ShortTag()})
			visit(k, p, "key", v)
			parityWalk(v, p, "value", visit)
		}
	case yaml.SequenceNode:
		for i, v := range n.Content {
			parityWalk(v, append(append([]any{}, path...), map[string]any{"index": i}), "item", visit)
		}
	}
}

func parityClearGenerated(n *yaml.Node) {
	if n == nil {
		return
	}
	n.Line = 0
	n.HeadComment, n.LineComment, n.FootComment = "", "", ""
	for _, c := range n.Content {
		parityClearGenerated(c)
	}
}

func parityRunTree(source string, operation map[string]any, root *yaml.Node, roundtrip bool) (*yaml.Node, error) {
	var err error
	switch operation["op"] {
	case "save":
		configSource := source
		if text, ok := operation["config"].(string); ok {
			configSource = text
		}
		cfg, parseErr := ParseConfigBytes([]byte(configSource))
		if parseErr != nil {
			return nil, fmt.Errorf("invalid ownership operation")
		}
		if _, ok := operation["config"].(string); ok {
			if file, e := ParseConfigBytes([]byte(source)); e == nil {
				cfg.Plugins = file.Plugins
			}
		}
		root, err = paritySaveTree(root, cfg, []byte(source), operation["migrate"] == true, roundtrip)
	case "write":
		if IsV8ConfigLayout(root) {
			root, err = parityNormalizeTree(root, true)
		}
	case "nested":
		keys, ok := operation["keys"].([]any)
		if !ok {
			return nil, fmt.Errorf("invalid ownership operation")
		}
		cur := root
		for i, key := range keys {
			name, ok := key.(string)
			if !ok {
				return nil, fmt.Errorf("invalid ownership operation")
			}
			cur = getOrCreateMapValue(cur, name)
			if i == len(keys)-1 {
				cur.Kind, cur.Tag, cur.Value, cur.Content = yaml.ScalarNode, "!!str", "", nil
			}
		}
	default:
		return nil, fmt.Errorf("invalid ownership operation")
	}
	return root, err
}

func parityClearIdentities(n *yaml.Node) {
	if n == nil {
		return
	}
	n.Line = 0
	for _, c := range n.Content {
		parityClearIdentities(c)
	}
}
