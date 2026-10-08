import { LoaderCircle } from "lucide-react";

import { cn } from "../lib/cn";

/**
 * A spinning circle. Decorative: say what is loading in text beside it. With
 * reduced motion it doesn't turn; it fades in and out, so it still shows that
 * something is going on.
 */
export function Spinner({ className }: { className?: string }) {
  return (
    <LoaderCircle
      aria-hidden="true"
      className={cn("size-4 motion-safe:animate-spin motion-reduce:animate-pulse", className)}
    />
  );
}
