import { Fragment } from "react";

/** The characters a line may break after, by the kind of text. */
const BREAK_AFTER = {
  /** A URL path: after each "/", and after a query's "?" and "&". */
  path: "/?&",
  /** A model or file name: after each "-" or "/". */
  name: "-/",
} as const;

export interface BreakableTextProps {
  text: string;
  /** What the text is, which says where it may wrap. */
  kind?: keyof typeof BREAK_AFTER;
}

/**
 * Text that wraps only at its natural joints, never mid-word: a long path
 * breaks after a "/", a model name after a "-". Each joint gets a `<wbr>`,
 * so copying the text copies it whole.
 */
export function BreakableText({ text, kind = "path" }: BreakableTextProps) {
  const after = BREAK_AFTER[kind];
  const parts: string[] = [];
  let start = 0;
  for (let index = 0; index < text.length - 1; index += 1) {
    if (after.includes(text.charAt(index))) {
      parts.push(text.slice(start, index + 1));
      start = index + 1;
    }
  }
  parts.push(text.slice(start));
  return parts.map((part, index) => (
    // The parts are fixed for a given text; their place is their identity.
    <Fragment key={index}>
      {index > 0 && <wbr />}
      {part}
    </Fragment>
  ));
}
