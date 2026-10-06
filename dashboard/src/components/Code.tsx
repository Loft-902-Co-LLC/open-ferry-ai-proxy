import type { ReactNode } from "react";

/** Inline code: a config key, a command, an environment variable. */
export function Code({ children }: { children: ReactNode }) {
  return (
    <code className="rounded bg-raised px-1 py-0.5 font-mono text-[0.85em] break-words">
      {children}
    </code>
  );
}
