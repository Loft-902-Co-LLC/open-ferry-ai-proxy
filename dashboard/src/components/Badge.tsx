import type { ReactNode } from "react";

import { cn } from "../lib/cn";

export type BadgeTone = "neutral" | "info" | "ok" | "warn" | "danger";

const TONES: Record<BadgeTone, string> = {
  neutral: "border-line bg-raised text-fg",
  info: "border-accent/40 bg-accent-soft text-fg",
  ok: "border-ok/40 bg-ok-soft text-fg",
  warn: "border-warn/50 bg-warn-soft text-fg",
  danger: "border-danger/40 bg-danger-soft text-fg",
};

/** A short label for a state; its words carry the meaning, not its colour. */
export function Badge({
  tone = "neutral",
  children,
  className,
}: {
  tone?: BadgeTone;
  children: ReactNode;
  className?: string;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-full border px-2 py-px text-xs font-medium whitespace-nowrap",
        TONES[tone],
        className,
      )}
    >
      {children}
    </span>
  );
}
