// Independent archive construction from source entries and the pinned edits.
package config

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strings"

	"gopkg.in/yaml.v3"
)

var parityArchiveObserver func(*yaml.Node, string, string)

// Inserted immediately after the pin computes an archive block, before it
// deletes the source entry. The ordinary harness has no observer installed.
func parityArchiveEntry(key *yaml.Node, section, text string) {
	if parityArchiveObserver != nil {
		parityArchiveObserver(key, section, text)
	}
}

func parityEntrySignature(n *yaml.Node) any {
	kind := map[yaml.Kind]string{yaml.MappingNode: "Mapping", yaml.SequenceNode: "Sequence", yaml.ScalarNode: "Scalar", yaml.AliasNode: "Alias", yaml.DocumentNode: "Document"}[n.Kind]
	children := []any{}
	for _, child := range n.Content {
		children = append(children, parityEntrySignature(child))
	}
	return []any{kind, n.ShortTag(), n.Value, n.HeadComment, n.LineComment, n.FootComment, children}
}

// The real save encodes and reparses before normalization. Archive text must
// include that stage, even when it makes the archive proof unavailable. Node
// IDs cross this boundary by typed value paths, never by comment text.
func parityRoundTrip(root *yaml.Node, source []byte) (*yaml.Node, error) {
	ids := map[string]int{}
	parityWalk(root, []any{}, "root", func(n *yaml.Node, p []any, side string, _ *yaml.Node) {
		b, _ := json.Marshal([]any{p, side, n.Kind, n.ShortTag(), n.Value})
		ids[string(b)] = n.Line
	})
	var doc yaml.Node
	if err := yaml.Unmarshal(source, &doc); err != nil {
		return nil, err
	}
	if len(doc.Content) != 1 {
		return nil, fmt.Errorf("archive source document")
	}
	doc.Content[0] = root
	var buf bytes.Buffer
	enc := yaml.NewEncoder(&buf)
	enc.SetIndent(2)
	if err := enc.Encode(&doc); err != nil {
		return nil, err
	}
	if err := enc.Close(); err != nil {
		return nil, err
	}
	if err := yaml.Unmarshal(NormalizeCommentIndentation(buf.Bytes()), &doc); err != nil {
		return nil, err
	}
	if len(doc.Content) != 1 {
		return nil, fmt.Errorf("archive emitted document")
	}
	root = doc.Content[0]
	parityWalk(root, []any{}, "root", func(n *yaml.Node, p []any, side string, _ *yaml.Node) {
		b, _ := json.Marshal([]any{p, side, n.Kind, n.ShortTag(), n.Value})
		n.Line = ids[string(b)]
	})
	return root, nil
}

type parityArchiveLine struct {
	Binding map[string]any
	Ordinal int
	Text    string
	Section string
}
type parityCommentBlock struct {
	Owner ParityOwner
	Lines []string
}

func parityBlocks(root *yaml.Node) map[string]parityCommentBlock {
	out := map[string]parityCommentBlock{}
	visit := func(n *yaml.Node, path []any, side string, _ *yaml.Node) {
		for slot, text := range map[string]string{"head": n.HeadComment, "line": n.LineComment, "foot": n.FootComment} {
			owner := ParityOwner{path, side, slot}
			b, _ := json.Marshal(owner)
			lines := []string{}
			for _, line := range strings.Split(text, "\n") {
				line = strings.TrimLeft(line, " \t")
				if line != "" {
					lines = append(lines, line)
				}
			}
			if len(lines) > 0 {
				out[string(b)] = parityCommentBlock{owner, lines}
			}
		}
	}
	if root.Kind == yaml.DocumentNode {
		parityWalk(root, []any{}, "document", visit)
		if len(root.Content) == 1 {
			parityWalk(root.Content[0], []any{}, "root", visit)
		}
	} else {
		parityWalk(root, []any{}, "root", visit)
	}
	return out
}

func parityArchives(source string, operation map[string]any, root *yaml.Node) ([]any, error) {
	bindings := map[int]map[string]any{}
	parityWalk(root, []any{}, "root", func(n *yaml.Node, path []any, side string, value *yaml.Node) {
		if side == "key" {
			bindings[n.Line] = map[string]any{"path": path, "entry": []any{parityEntrySignature(n), parityEntrySignature(value)}}
		}
	})
	lines := []parityArchiveLine{}
	parityArchiveObserver = func(key *yaml.Node, section, text string) {
		for i, line := range strings.Split(text, "\n") {
			lines = append(lines, parityArchiveLine{bindings[key.Line], i, line, section})
		}
	}
	defer func() { parityArchiveObserver = nil }()
	var err error
	root, err = parityRunTree(source, operation, root, true)
	if err != nil {
		return nil, err
	}
	if len(lines) == 0 {
		return []any{}, nil
	}
	// Two independently emitted layouts bind each computed line by ordinal.
	// Opaque markers establish owners; actual archive text supplies bytes only.
	// Neither layout reads a Go/protected output or matches equal comments.
	parityClearGenerated(root)
	texts, markers := []string{}, []string{}
	markerIDs := map[string]int{}
	for i, line := range lines {
		marker := fmt.Sprintf("# parity-archive-%d", i)
		markers = append(markers, marker)
		markerIDs[marker] = i
		texts = append(texts, line.Text)
	}
	emit := func(comments []string) (*yaml.Node, error) {
		root.FootComment = strings.TrimSpace(strings.Join(comments, "\n"))
		data, e := yaml.Marshal(root)
		if e != nil {
			return nil, e
		}
		var doc yaml.Node
		if e = yaml.Unmarshal(data, &doc); e != nil {
			return nil, e
		}
		if len(doc.Content) != 1 {
			return nil, fmt.Errorf("archive layout document")
		}
		return &doc, nil
	}
	actual, err := emit(texts)
	if err != nil {
		return nil, err
	}
	tagged, err := emit(markers)
	if err != nil {
		return nil, err
	}
	a, m := parityBlocks(actual), parityBlocks(tagged)
	if len(a) != len(m) {
		return nil, fmt.Errorf("archive layout shape")
	}
	result := []any{}
	seen := map[int]bool{}
	for key, block := range m {
		other, ok := a[key]
		if !ok || len(other.Lines) != len(block.Lines) {
			return nil, fmt.Errorf("archive layout occurrence shape")
		}
		for i, marker := range block.Lines {
			id, ok := markerIDs[marker]
			if !ok || seen[id] {
				return nil, fmt.Errorf("archive layout identity")
			}
			seen[id] = true
			line := lines[id]
			if line.Binding == nil {
				continue
			}
			result = append(result, map[string]any{"source_key": line.Binding["path"], "binding": line.Binding, "ordinal": line.Ordinal, "owner": block.Owner, "text": other.Lines[i], "section": line.Section})
		}
	}
	if len(seen) != len(lines) {
		return nil, fmt.Errorf("archive layout lost identity")
	}
	return result, nil
}
