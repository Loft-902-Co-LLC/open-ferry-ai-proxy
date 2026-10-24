// Independent archive construction from source entries and the pinned edits.
package config

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strings"

	"gopkg.in/yaml.v3"
)

var parityArchiveObserver func(*yaml.Node, *yaml.Node, string, string)
var parityArchiveAppendObserver func(string)
var parityArchiveDocumentObserver func(*yaml.Node)

// Inserted immediately after the pin computes an archive block, before it
// deletes the source entry. The ordinary harness has no observer installed.
func parityArchiveEntry(key, value *yaml.Node, section, text string) {
	if parityArchiveObserver != nil {
		parityArchiveObserver(key, value, section, text)
	}
}

func parityArchiveValues(n *yaml.Node) string {
	var value func(*yaml.Node) any
	value = func(n *yaml.Node) any {
		children := []any{}
		for _, child := range n.Content {
			children = append(children, value(child))
		}
		return []any{n.Kind, n.ShortTag(), n.Value, children}
	}
	data, _ := json.Marshal(value(n))
	return string(data)
}

func parityArchiveAppended(previous string) {
	if parityArchiveAppendObserver != nil {
		parityArchiveAppendObserver(previous)
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
	if parityArchiveDocumentObserver != nil {
		parityArchiveDocumentObserver(&doc)
	}
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
	var template yaml.Node
	if err := yaml.Unmarshal([]byte(source), &template); err != nil {
		return nil, err
	}
	allText := source
	parityWalk(root, []any{}, "root", func(n *yaml.Node, _ []any, _ string, _ *yaml.Node) {
		allText += n.Value + n.HeadComment + n.LineComment + n.FootComment
	})
	allText += template.HeadComment + template.LineComment + template.FootComment
	prefix := "# parity-archive-"
	for strings.Contains(allText, prefix) {
		prefix += "-"
	}
	bindings := map[int]map[string]any{}
	values := map[int]string{}
	parityWalk(root, []any{}, "root", func(n *yaml.Node, path []any, side string, value *yaml.Node) {
		if side == "key" {
			bindings[n.Line] = map[string]any{"path": path, "entry": []any{parityEntrySignature(n), parityEntrySignature(value)}}
			values[n.Line] = parityArchiveValues(value)
		}
	})
	lines := []parityArchiveLine{}
	markers := []string{}
	markerIDs := map[string]int{}
	footer := ""
	footerBound := false
	batchStart := 0
	parityArchiveObserver = func(key, value *yaml.Node, section, text string) {
		binding := bindings[key.Line]
		if values[key.Line] != parityArchiveValues(value) {
			binding = nil
		}
		// The archive's YAML text must also decode to the original values.
		// An upstream scalar re-encode that changes data receives no budget.
		body := []string{}
		for _, line := range strings.Split(text, "\n") {
			if !strings.HasPrefix(line, "# ") {
				binding = nil
			}
			body = append(body, strings.TrimPrefix(line, "# "))
		}
		var archived yaml.Node
		if yaml.Unmarshal([]byte(strings.Join(body, "\n")+"\n"), &archived) != nil || len(archived.Content) != 1 || archived.Content[0].Kind != yaml.MappingNode || len(archived.Content[0].Content) != 2 {
			binding = nil
		} else if parityArchiveValues(archived.Content[0].Content[1]) != values[key.Line] {
			binding = nil
		}
		for i, line := range strings.Split(text, "\n") {
			marker := fmt.Sprintf("%s%d", prefix, len(lines))
			markerIDs[marker] = len(lines)
			markers = append(markers, marker)
			lines = append(lines, parityArchiveLine{binding, i, line, section})
		}
	}
	parityArchiveAppendObserver = func(previous string) {
		if !footerBound {
			footer = previous
			footerBound = true
		}
		footer = strings.TrimSpace(footer + "\n" + strings.Join(markers[batchStart:], "\n"))
		batchStart = len(markers)
	}
	parityArchiveDocumentObserver = func(doc *yaml.Node) { template = *deepCopyNode(doc) }
	defer func() {
		parityArchiveObserver = nil
		parityArchiveAppendObserver = nil
		parityArchiveDocumentObserver = nil
	}()
	var err error
	root, err = parityRunTree(source, operation, root, true)
	if err != nil {
		return nil, err
	}
	if len(lines) == 0 {
		return []any{}, nil
	}
	if !footerBound || batchStart != len(lines) {
		return nil, fmt.Errorf("archive footer binding")
	}
	// Two independently emitted layouts bind each computed line by ordinal.
	// Opaque markers establish owners; actual archive text supplies bytes only.
	// Neither layout reads a Go/protected output or matches equal comments.
	template.Content = []*yaml.Node{root}
	taggedTemplate := deepCopyNode(&template)
	taggedTemplate.Content[0].FootComment = footer
	emit := func(model *yaml.Node) (*yaml.Node, error) {
		data, e := yaml.Marshal(model)
		if e != nil {
			return nil, e
		}
		if operation["op"] == "write" {
			data = NormalizeCommentIndentation(data)
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
	actual, err := emit(&template)
	if err != nil {
		return nil, err
	}
	tagged, err := emit(taggedTemplate)
	if err != nil {
		return nil, err
	}
	a, m := parityBlocks(actual), parityBlocks(tagged)
	for _, block := range a {
		for _, line := range block.Lines {
			if _, collision := markerIDs[line]; collision {
				return nil, fmt.Errorf("archive identity collision")
			}
		}
	}
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
			if !ok {
				if marker != other.Lines[i] {
					return nil, fmt.Errorf("archive layout changed source comment")
				}
				continue
			}
			if seen[id] {
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
