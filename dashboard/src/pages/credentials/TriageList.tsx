import { ChevronRight } from "lucide-react";
import { useId, useState, type ReactNode } from "react";

import { Button } from "../../components/Button";
import { cn } from "../../lib/cn";
import { useAddressAnchor, useFocusAnchor } from "./anchors";
import { compareHealth, healthTally, needsAttention, type HealthOrder } from "./credentialStates";

/** A disclosure's chevron: pointing right when closed, down when open. */
function Chevron({ open }: { open: boolean }) {
  return (
    <ChevronRight
      aria-hidden="true"
      className={cn(
        "size-4 transition-transform motion-reduce:transition-none",
        open && "rotate-90",
      )}
    />
  );
}

/**
 * Whether a folded entry's details show: closed at first, unless the address
 * points at it, and opened whenever it comes to point at it.
 */
export function useOpenWhenTargeted(targeted: boolean): [boolean, (open: boolean) => void] {
  const [open, setOpen] = useState(targeted);
  const [seen, setSeen] = useState(targeted);
  if (seen !== targeted) {
    setSeen(targeted);
    if (targeted) {
      setOpen(true);
    }
  }
  return [open, setOpen];
}

/** The button that shows or hides a folded entry's details, naming the entry. */
export function DetailsButton({
  open,
  controls,
  name,
  onToggle,
}: {
  open: boolean;
  controls: string;
  name: string;
  onToggle: () => void;
}) {
  return (
    <Button
      size="sm"
      variant="ghost"
      aria-expanded={open}
      aria-controls={controls}
      onClick={onToggle}
    >
      <Chevron open={open} />
      Details <span className="sr-only">{name}</span>
    </Button>
  );
}

/** One thing in the list, with what it sorts by. */
export interface TriageEntry extends HealthOrder {
  key: string;
  /** Its element's id, which the address can point at. */
  anchor: string;
}

/** How a list shows an entry. */
export interface TriageView {
  /** In the folded-away group: a row whose details open on request. */
  compact: boolean;
  /** The address points at it: show its details. */
  targeted: boolean;
}

export interface TriageListProps<T extends TriageEntry> {
  entries: readonly T[];
  render: (entry: T, view: TriageView) => ReactNode;
}

/**
 * Entries sorted by health: the failing and resting ones in full, the rest
 * folded into one group named by its tally ("12 ready, 1 off"). An entry the
 * address points at opens, even inside the folded group, and gets focus.
 */
export function TriageList<T extends TriageEntry>({ entries, render }: TriageListProps<T>) {
  const groupId = useId();
  const anchor = useAddressAnchor();
  const sorted = [...entries].sort(compareHealth);
  const problems = sorted.filter((entry) => needsAttention(entry.health));
  const rest = sorted.filter((entry) => !needsAttention(entry.health));
  const target = sorted.find((entry) => entry.anchor === anchor);
  const targetFolded = target !== undefined && !needsAttention(target.health);

  const [open, setOpen] = useState(targetFolded);
  // The address can change while the list shows: open the group again then.
  const [seenAnchor, setSeenAnchor] = useState(anchor);
  if (seenAnchor !== anchor) {
    setSeenAnchor(anchor);
    if (targetFolded) {
      setOpen(true);
    }
  }
  useFocusAnchor(target?.anchor ?? null);

  return (
    <div className="divide-y divide-line">
      {problems.map((entry) => (
        <div key={entry.key} className="py-4 first:pt-0 last:pb-0">
          {render(entry, { compact: false, targeted: entry.anchor === anchor })}
        </div>
      ))}
      {rest.length > 0 && (
        <div className="space-y-2 py-4 first:pt-0 last:pb-0">
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
            {problems.length === 0 && <p>Nothing needs attention.</p>}
            <Button
              size="sm"
              aria-expanded={open}
              aria-controls={groupId}
              onClick={() => {
                setOpen(!open);
              }}
            >
              <Chevron open={open} />
              {healthTally(rest.map((entry) => entry.health))}
            </Button>
          </div>
          <div id={groupId} hidden={!open} className="divide-y divide-line">
            {rest.map((entry) => (
              <div key={entry.key} className="py-3 last:pb-0">
                {render(entry, { compact: true, targeted: entry.anchor === anchor })}
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
