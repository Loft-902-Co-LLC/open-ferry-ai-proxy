import { Ship } from "lucide-react";

import { cn } from "../lib/cn";

/** The product's name and mark. */
export function Brand({ className }: { className?: string }) {
  return (
    <span className={cn("inline-flex items-center gap-2 font-semibold text-fg", className)}>
      <span className="flex size-7 items-center justify-center rounded-md bg-accent-strong text-accent-fg">
        <Ship aria-hidden="true" className="size-4" />
      </span>
      <span>open-ferry</span>
    </span>
  );
}
