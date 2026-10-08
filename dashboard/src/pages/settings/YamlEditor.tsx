// CodeMirror 6, for config.yaml. It styles itself through style-mod, which
// puts its rules in a <style> element when it mounts in a document: the
// dashboard's Content-Security-Policy refuses that. Mounted in a shadow
// root, style-mod uses a constructed stylesheet (adoptedStyleSheets)
// instead, which the policy allows. Everything else it styles through the
// CSSOM. Colours come from the page's custom properties, which inherit into
// the shadow root, so the editor follows the light and dark themes.

import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { yaml } from "@codemirror/lang-yaml";
import { HighlightStyle, bracketMatching, indentOnInput, syntaxHighlighting } from "@codemirror/language";
import { highlightSelectionMatches, searchKeymap } from "@codemirror/search";
import { EditorState } from "@codemirror/state";
import {
  EditorView,
  drawSelection,
  highlightActiveLine,
  highlightActiveLineGutter,
  keymap,
  lineNumbers,
} from "@codemirror/view";
import { tags } from "@lezer/highlight";
import { useEffect, useRef } from "react";

const MONO =
  'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace';

const theme = EditorView.theme({
  "&": {
    height: "100%",
    color: "var(--of-fg)",
    backgroundColor: "var(--of-surface)",
    fontSize: "13px",
  },
  "&.cm-focused": { outline: "2px solid var(--of-accent)", outlineOffset: "-2px" },
  ".cm-scroller": { fontFamily: MONO, lineHeight: "1.55" },
  ".cm-content": { caretColor: "var(--of-fg)" },
  ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--of-fg)" },
  ".cm-selectionBackground": { backgroundColor: "var(--of-raised)" },
  "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground": {
    backgroundColor: "var(--of-accent-soft)",
  },
  ".cm-activeLine": { backgroundColor: "transparent" },
  "&.cm-focused .cm-activeLine": { backgroundColor: "var(--of-canvas)" },
  ".cm-gutters": {
    backgroundColor: "var(--of-raised)",
    color: "var(--of-muted)",
    borderRight: "1px solid var(--of-line)",
  },
  ".cm-activeLineGutter": { backgroundColor: "var(--of-line)", color: "var(--of-fg)" },
  ".cm-selectionMatch": { backgroundColor: "var(--of-accent-soft)" },
  ".cm-searchMatch": {
    backgroundColor: "var(--of-warn-soft)",
    outline: "1px solid var(--of-warn)",
  },
  ".cm-searchMatch.cm-searchMatch-selected": { backgroundColor: "var(--of-accent-soft)" },
  ".cm-matchingBracket, &.cm-focused .cm-matchingBracket": {
    backgroundColor: "var(--of-accent-soft)",
    outline: "1px solid var(--of-accent)",
  },
  ".cm-panels": {
    backgroundColor: "var(--of-raised)",
    color: "var(--of-fg)",
    fontFamily: "inherit",
  },
  ".cm-panels.cm-panels-top": { borderBottom: "1px solid var(--of-line)" },
  ".cm-panels.cm-panels-bottom": { borderTop: "1px solid var(--of-line)" },
  ".cm-textfield": {
    backgroundColor: "var(--of-surface)",
    color: "var(--of-fg)",
    border: "1px solid var(--of-line)",
    borderRadius: "4px",
  },
  ".cm-button": {
    backgroundImage: "none",
    backgroundColor: "var(--of-surface)",
    color: "var(--of-fg)",
    border: "1px solid var(--of-line)",
    borderRadius: "4px",
  },
  ".cm-panel.cm-search label": { color: "var(--of-fg)" },
  ".cm-panel button[name=close]": { color: "var(--of-muted)" },
});

// Each colour meets WCAG AA on the editor's surface in both themes.
const highlight = HighlightStyle.define([
  { tag: [tags.definition(tags.propertyName), tags.propertyName], color: "var(--of-accent)" },
  { tag: tags.string, color: "var(--of-ok)" },
  { tag: [tags.lineComment, tags.comment], color: "var(--of-muted)", fontStyle: "italic" },
  { tag: [tags.labelName, tags.typeName], color: "var(--of-warn)" },
  { tag: [tags.meta, tags.separator, tags.punctuation], color: "var(--of-muted)" },
]);

export interface YamlEditorProps {
  /** The text it starts with. A change of it starts the editor afresh. */
  initial: string;
  /** The editor's accessible name. */
  label: string;
  onChange: (text: string) => void;
}

/** A YAML editor. Tab moves focus on, as elsewhere on the page; indent with spaces. */
export function YamlEditor({ initial, label, onChange }: YamlEditorProps) {
  const host = useRef<HTMLDivElement>(null);
  const changed = useRef(onChange);
  useEffect(() => {
    changed.current = onChange;
  }, [onChange]);

  useEffect(() => {
    const element = host.current;
    if (element === null) {
      return;
    }
    const shadow = element.shadowRoot ?? element.attachShadow({ mode: "open" });
    const parent = document.createElement("div");
    parent.style.height = "100%";
    shadow.append(parent);
    const view = new EditorView({
      root: shadow,
      parent,
      state: EditorState.create({
        doc: initial,
        extensions: [
          // Keep the file's line endings: CodeMirror joins lines with "\n"
          // unless told otherwise.
          initial.includes("\r\n") ? EditorState.lineSeparator.of("\r\n") : [],
          lineNumbers(),
          highlightActiveLineGutter(),
          history(),
          drawSelection(),
          indentOnInput(),
          bracketMatching(),
          highlightActiveLine(),
          highlightSelectionMatches(),
          yaml(),
          syntaxHighlighting(highlight),
          theme,
          keymap.of([...defaultKeymap, ...historyKeymap, ...searchKeymap]),
          EditorState.tabSize.of(2),
          EditorView.contentAttributes.of({
            "aria-label": label,
            spellcheck: "false",
            autocapitalize: "off",
            autocorrect: "off",
          }),
          EditorView.updateListener.of((update) => {
            if (update.docChanged) {
              changed.current(update.state.doc.toString());
            }
          }),
        ],
      }),
    });
    return () => {
      view.destroy();
      parent.remove();
    };
  }, [initial, label]);

  return (
    <div
      ref={host}
      className="block h-[min(65vh,40rem)] min-h-64 overflow-hidden rounded-md border border-control"
    />
  );
}
