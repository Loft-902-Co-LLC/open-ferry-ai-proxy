import type { ReactNode, TdHTMLAttributes, ThHTMLAttributes } from "react";

import { cn } from "../lib/cn";

/** A dense data table that scrolls sideways on narrow screens. */
export function Table({
  caption,
  children,
  className,
}: {
  /** Names the table for screen readers; shown only to them. */
  caption: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("-mx-4 overflow-x-auto px-4", className)}>
      <table className="w-full border-collapse text-left tabular-nums">
        <caption className="sr-only">{caption}</caption>
        {children}
      </table>
    </div>
  );
}

export function Th({ className, ...props }: ThHTMLAttributes<HTMLTableCellElement>) {
  return (
    <th
      scope="col"
      className={cn(
        "border-b border-line px-2 py-1.5 text-xs font-semibold whitespace-nowrap text-muted first:pl-0 last:pr-0",
        className,
      )}
      {...props}
    />
  );
}

export function Td({ className, ...props }: TdHTMLAttributes<HTMLTableCellElement>) {
  return (
    <td
      className={cn(
        "border-b border-line/60 px-2 py-1.5 align-top first:pl-0 last:pr-0",
        className,
      )}
      {...props}
    />
  );
}
