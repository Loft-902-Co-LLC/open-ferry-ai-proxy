import { useId, type ReactNode } from "react";

import { cn } from "../lib/cn";

export interface CardProps {
  title?: ReactNode;
  description?: ReactNode;
  /** Controls shown at the right of the title. */
  actions?: ReactNode;
  children?: ReactNode;
  className?: string;
}

/** A titled section of a page. */
export function Card({ title, description, actions, children, className }: CardProps) {
  const titleId = useId();
  return (
    <section
      aria-labelledby={title === undefined ? undefined : titleId}
      className={cn("rounded-lg border border-line bg-surface", className)}
    >
      {(title !== undefined || actions !== undefined) && (
        <header className="flex flex-wrap items-start justify-between gap-3 border-b border-line px-4 py-3">
          <div className="min-w-0">
            {title !== undefined && (
              <h2 id={titleId} className="text-base font-semibold">
                {title}
              </h2>
            )}
            {description !== undefined && <p className="text-muted">{description}</p>}
          </div>
          {actions !== undefined && <div className="flex flex-wrap gap-2">{actions}</div>}
        </header>
      )}
      {children !== undefined && <div className="space-y-4 px-4 py-4">{children}</div>}
    </section>
  );
}
