import { useId, useRef, type KeyboardEvent, type ReactNode } from "react";

import { cn } from "../lib/cn";

export interface TabItem {
  id: string;
  label: string;
}

export interface TabsProps {
  /** The tab list's name. */
  label: string;
  items: readonly TabItem[];
  selected: string;
  onSelect: (id: string) => void;
  /** The selected tab's panel. */
  children: ReactNode;
}

/**
 * Tabs as the ARIA authoring practices have them: arrow keys, Home and End
 * move between tabs and select them; Tab moves into the panel.
 */
export function Tabs({ label, items, selected, onSelect, children }: TabsProps) {
  const baseId = useId();
  const tabRefs = useRef(new Map<string, HTMLButtonElement>());

  const move = (event: KeyboardEvent<HTMLDivElement>) => {
    const index = items.findIndex((item) => item.id === selected);
    let next: number;
    switch (event.key) {
      case "ArrowRight":
        next = (index + 1) % items.length;
        break;
      case "ArrowLeft":
        next = (index - 1 + items.length) % items.length;
        break;
      case "Home":
        next = 0;
        break;
      case "End":
        next = items.length - 1;
        break;
      default:
        return;
    }
    event.preventDefault();
    const item = items[next];
    if (item !== undefined) {
      onSelect(item.id);
      tabRefs.current.get(item.id)?.focus();
    }
  };

  return (
    <div>
      <div
        role="tablist"
        aria-label={label}
        onKeyDown={move}
        className="flex flex-wrap gap-x-1 border-b border-line"
      >
        {items.map((item) => {
          const active = item.id === selected;
          return (
            <button
              key={item.id}
              ref={(element) => {
                if (element === null) {
                  tabRefs.current.delete(item.id);
                } else {
                  tabRefs.current.set(item.id, element);
                }
              }}
              type="button"
              role="tab"
              id={`${baseId}-tab-${item.id}`}
              aria-selected={active}
              aria-controls={`${baseId}-panel`}
              tabIndex={active ? 0 : -1}
              onClick={() => {
                onSelect(item.id);
              }}
              className={cn(
                "-mb-px border-b-2 px-3 py-2 font-medium whitespace-nowrap pointer-coarse:min-h-11",
                active
                  ? "border-accent text-fg"
                  : "border-transparent text-muted hover:border-line hover:text-fg",
              )}
            >
              {item.label}
            </button>
          );
        })}
      </div>
      <div
        role="tabpanel"
        id={`${baseId}-panel`}
        aria-labelledby={`${baseId}-tab-${selected}`}
        tabIndex={0}
        className="pt-4"
      >
        {children}
      </div>
    </div>
  );
}
