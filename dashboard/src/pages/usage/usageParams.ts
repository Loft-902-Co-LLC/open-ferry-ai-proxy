// The Usage page's choices live in its URL, so a view can be bookmarked,
// shared and restored with Back.

import { useCallback, useMemo } from "react";
import { useSearchParams } from "react-router";

import type { GroupBy, UsageFilters } from "../../api/dashboard";
import { DEFAULT_RANGE, presetById, type RangePreset } from "../../lib/timeRange";

export const GROUP_BY_LABELS: Record<GroupBy, string> = {
  model: "Model",
  provider: "Provider",
  credential: "Credential",
  client_key: "Client key",
};

export const GROUP_BYS = Object.keys(GROUP_BY_LABELS) as GroupBy[];

/** The filters a group's key can set, by dimension. */
export type FilterName = GroupBy;

export interface ActiveFilter {
  name: FilterName;
  value: string;
  /** What to show for it: a credential's label, a client key masked. */
  label: string;
}

export interface UsageView {
  range: RangePreset;
  groupBy: GroupBy | null;
  filters: ActiveFilter[];
  failedOnly: boolean;
}

function isGroupBy(value: string | null): value is GroupBy {
  return value !== null && (GROUP_BYS as string[]).includes(value);
}

export function readUsageView(params: URLSearchParams): UsageView {
  const group = params.get("group");
  const filters: ActiveFilter[] = [];
  for (const name of GROUP_BYS) {
    const value = params.get(name);
    if (value !== null) {
      filters.push({ name, value, label: params.get(`${name}_label`) ?? value });
    }
  }
  return {
    range: presetById(params.get("range") ?? DEFAULT_RANGE),
    groupBy: isGroupBy(group) ? group : null,
    filters,
    failedOnly: params.get("failed") === "true",
  };
}

/** The filters as the usage routes take them. */
export function filterQuery(filters: readonly ActiveFilter[]): Omit<UsageFilters, "from" | "to"> {
  const query: Omit<UsageFilters, "from" | "to"> = {};
  for (const filter of filters) {
    query[filter.name] = filter.value;
  }
  return query;
}

export function useUsageView() {
  const [params, setParams] = useSearchParams();
  const view = useMemo(() => readUsageView(params), [params]);

  const update = useCallback(
    (change: (next: URLSearchParams) => void) => {
      setParams(
        (current) => {
          const next = new URLSearchParams(current);
          change(next);
          return next;
        },
        { replace: true },
      );
    },
    [setParams],
  );

  return {
    view,
    setRange: (id: string) => {
      update((next) => {
        next.set("range", id);
      });
    },
    setGroupBy: (groupBy: GroupBy | null) => {
      update((next) => {
        if (groupBy === null) {
          next.delete("group");
        } else {
          next.set("group", groupBy);
        }
      });
    },
    addFilter: (filter: ActiveFilter) => {
      update((next) => {
        next.set(filter.name, filter.value);
        if (filter.label === filter.value) {
          next.delete(`${filter.name}_label`);
        } else {
          next.set(`${filter.name}_label`, filter.label);
        }
      });
    },
    removeFilter: (name: FilterName) => {
      update((next) => {
        next.delete(name);
        next.delete(`${name}_label`);
      });
    },
    setFailedOnly: (failedOnly: boolean) => {
      update((next) => {
        if (failedOnly) {
          next.set("failed", "true");
        } else {
          next.delete("failed");
        }
      });
    },
  };
}
