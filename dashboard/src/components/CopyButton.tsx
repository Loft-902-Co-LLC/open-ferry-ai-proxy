import { Check, Copy } from "lucide-react";
import { useEffect, useState } from "react";

import { Button, type ButtonSize } from "./Button";

export interface CopyButtonProps {
  text: string;
  /** What is copied, for the button's name: "Copy the curl command". */
  label: string;
  size?: ButtonSize;
}

/** Copies `text` to the clipboard, and says so. */
export function CopyButton({ text, label, size = "sm" }: CopyButtonProps) {
  const [state, setState] = useState<"idle" | "copied" | "failed">("idle");

  useEffect(() => {
    if (state === "idle") {
      return undefined;
    }
    const timer = window.setTimeout(() => {
      setState("idle");
    }, 2000);
    return () => {
      window.clearTimeout(timer);
    };
  }, [state]);

  const copy = () => {
    navigator.clipboard.writeText(text).then(
      () => {
        setState("copied");
      },
      () => {
        setState("failed");
      },
    );
  };

  return (
    <span className="inline-flex items-center gap-2">
      <Button size={size} onClick={copy} aria-label={label} title={label}>
        {state === "copied" ? (
          <Check aria-hidden="true" className="size-4 text-ok" />
        ) : (
          <Copy aria-hidden="true" className="size-4" />
        )}
        {state === "copied" ? "Copied" : "Copy"}
      </Button>
      <span role="status" className="sr-only">
        {state === "copied" ? "Copied" : state === "failed" ? "Couldn't copy: select the text and copy it by hand." : ""}
      </span>
      {state === "failed" && <span className="text-danger">Couldn&apos;t copy</span>}
    </span>
  );
}
