import { Eye, EyeOff } from "lucide-react";
import { useState } from "react";

import { maskKey } from "../lib/mask";
import { CopyButton } from "./CopyButton";

export interface SecretTextProps {
  value: string;
  /** What it is, for the buttons' names: "the API key". */
  label: string;
}

/** A secret shown masked, with a button to show it and one to copy it. */
export function SecretText({ value, label }: SecretTextProps) {
  const [shown, setShown] = useState(false);
  // A toggle keeps its name; aria-pressed says whether it is on.
  const revealLabel = `Show ${label}`;
  return (
    <span className="inline-flex flex-wrap items-center gap-1.5">
      <code className="rounded bg-raised px-1 py-0.5 font-mono text-[0.85em] break-all">
        {shown ? value : maskKey(value)}
      </code>
      <button
        type="button"
        aria-label={revealLabel}
        aria-pressed={shown}
        title={revealLabel}
        onClick={() => {
          setShown((value) => !value);
        }}
        className="inline-flex size-7 items-center justify-center rounded-md text-muted hover:bg-raised hover:text-fg"
      >
        {shown ? (
          <EyeOff aria-hidden="true" className="size-4" />
        ) : (
          <Eye aria-hidden="true" className="size-4" />
        )}
      </button>
      <CopyButton text={value} label={`Copy ${label}`} />
    </span>
  );
}
