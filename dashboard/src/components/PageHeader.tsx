import { useEffect, type ReactNode } from "react";

/** Sets the tab's title while a page shows. */
export function usePageTitle(title: string) {
  useEffect(() => {
    document.title = `${title} · open-ferry`;
  }, [title]);
}

export interface PageHeaderProps {
  title: string;
  description?: ReactNode;
  actions?: ReactNode;
}

/** A page's heading, which also names the tab. */
export function PageHeader({ title, description, actions }: PageHeaderProps) {
  usePageTitle(title);
  return (
    <div className="mb-5 flex flex-wrap items-end justify-between gap-3">
      <div className="min-w-0">
        <h1 className="text-xl font-semibold">{title}</h1>
        {description !== undefined && <p className="text-muted">{description}</p>}
      </div>
      {actions !== undefined && <div className="flex flex-wrap gap-2">{actions}</div>}
    </div>
  );
}
