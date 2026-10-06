import { LoaderCircle } from "lucide-react";

import { cn } from "../lib/cn";

/** A spinning circle. Decorative: say what is loading in text beside it. */
export function Spinner({ className }: { className?: string }) {
  return <LoaderCircle aria-hidden="true" className={cn("size-4 animate-spin", className)} />;
}
