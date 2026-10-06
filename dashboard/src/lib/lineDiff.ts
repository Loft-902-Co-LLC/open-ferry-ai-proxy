// A line diff of two texts, for showing what a save will change: the
// longest common subsequence of their lines, with the lines both share at
// the start and end set aside first, as a config edit usually leaves most of
// the file alone.

/** One line of a diff. */
export interface DiffLine {
  kind: "same" | "added" | "removed";
  text: string;
  /** Its number in the text before, from 1; null for an added line. */
  before: number | null;
  /** Its number in the text after, from 1; null for a removed line. */
  after: number | null;
}

/** A line, or a run of unchanged lines left out. */
export type DiffRow = { kind: "line"; line: DiffLine } | { kind: "gap"; count: number };

/**
 * The most cells the subsequence table may have. Past it, the middle of the
 * diff shows every line before as removed and every line after as added.
 */
const MAX_CELLS = 4_000_000;

/** The lines of `text`, without their endings. A last line ending ends no further line. */
export function splitLines(text: string): string[] {
  if (text === "") {
    return [];
  }
  const lines = text.split("\n");
  if (lines.at(-1) === "") {
    lines.pop();
  }
  return lines.map((line) => (line.endsWith("\r") ? line.slice(0, -1) : line));
}

/** The lines of `before` and `after`, each marked same, removed or added. */
export function diffLines(before: string, after: string): DiffLine[] {
  const a = splitLines(before);
  const b = splitLines(after);
  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) {
    start += 1;
  }
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA -= 1;
    endB -= 1;
  }

  const lines: DiffLine[] = [];
  const same = (i: number, j: number) => {
    lines.push({ kind: "same", text: a[i] ?? "", before: i + 1, after: j + 1 });
  };
  const removed = (i: number) => {
    lines.push({ kind: "removed", text: a[i] ?? "", before: i + 1, after: null });
  };
  const added = (j: number) => {
    lines.push({ kind: "added", text: b[j] ?? "", before: null, after: j + 1 });
  };

  for (let i = 0; i < start; i += 1) {
    same(i, i);
  }

  const n = endA - start;
  const m = endB - start;
  if (n > 0 && m > 0 && (n + 1) * (m + 1) <= MAX_CELLS) {
    // table[i * width + j]: the longest common subsequence of a[start + i..]
    // and b[start + j..]. min(n, m) is at most 2,000 here, so 16 bits hold it.
    const width = m + 1;
    const table = new Uint16Array((n + 1) * width);
    for (let i = n - 1; i >= 0; i -= 1) {
      for (let j = m - 1; j >= 0; j -= 1) {
        table[i * width + j] =
          a[start + i] === b[start + j]
            ? (table[(i + 1) * width + j + 1] ?? 0) + 1
            : Math.max(table[(i + 1) * width + j] ?? 0, table[i * width + j + 1] ?? 0);
      }
    }
    let i = 0;
    let j = 0;
    while (i < n && j < m) {
      if (a[start + i] === b[start + j]) {
        same(start + i, start + j);
        i += 1;
        j += 1;
      } else if ((table[(i + 1) * width + j] ?? 0) >= (table[i * width + j + 1] ?? 0)) {
        removed(start + i);
        i += 1;
      } else {
        added(start + j);
        j += 1;
      }
    }
    for (; i < n; i += 1) {
      removed(start + i);
    }
    for (; j < m; j += 1) {
      added(start + j);
    }
  } else {
    for (let i = start; i < endA; i += 1) {
      removed(i);
    }
    for (let j = start; j < endB; j += 1) {
      added(j);
    }
  }

  for (let i = endA, j = endB; i < a.length; i += 1, j += 1) {
    same(i, j);
  }
  return lines;
}

/** How many lines `lines` adds and removes. */
export function diffCounts(lines: readonly DiffLine[]): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const line of lines) {
    if (line.kind === "added") {
      added += 1;
    } else if (line.kind === "removed") {
      removed += 1;
    }
  }
  return { added, removed };
}

/**
 * The changed lines of `lines` with up to `context` unchanged lines around
 * each change; the unchanged lines further away become gaps.
 */
export function diffRows(lines: readonly DiffLine[], context = 3): DiffRow[] {
  const keep = new Array<boolean>(lines.length).fill(false);
  lines.forEach((line, index) => {
    if (line.kind !== "same") {
      const from = Math.max(0, index - context);
      const to = Math.min(lines.length - 1, index + context);
      for (let at = from; at <= to; at += 1) {
        keep[at] = true;
      }
    }
  });
  const rows: DiffRow[] = [];
  let hidden = 0;
  lines.forEach((line, index) => {
    if (keep[index] === true) {
      if (hidden > 0) {
        rows.push({ kind: "gap", count: hidden });
        hidden = 0;
      }
      rows.push({ kind: "line", line });
    } else {
      hidden += 1;
    }
  });
  if (hidden > 0) {
    rows.push({ kind: "gap", count: hidden });
  }
  return rows;
}
