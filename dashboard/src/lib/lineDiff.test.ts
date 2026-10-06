import { describe, expect, it } from "vitest";

import { diffCounts, diffLines, diffRows, splitLines, type DiffLine } from "./lineDiff";

const NL = String.fromCharCode(10);
const CR = String.fromCharCode(13);

function text(...lines: string[]): string {
  return lines.map((line) => line + NL).join("");
}

/** A diff as " line", "-line" and "+line". */
function marked(lines: readonly DiffLine[]): string[] {
  return lines.map(
    (line) => (line.kind === "same" ? " " : line.kind === "added" ? "+" : "-") + line.text,
  );
}

describe("splitLines", () => {
  it("reads a last line with or without its ending, and drops carriage returns", () => {
    expect(splitLines("")).toEqual([]);
    expect(splitLines(text("a", "b"))).toEqual(["a", "b"]);
    expect(splitLines(`a${NL}b`)).toEqual(["a", "b"]);
    expect(splitLines(`a${CR}${NL}b${CR}${NL}`)).toEqual(["a", "b"]);
    expect(splitLines(text("a", "", ""))).toEqual(["a", "", ""]);
  });
});

describe("diffLines", () => {
  it("marks nothing in equal texts", () => {
    const lines = diffLines(text("a", "b"), text("a", "b"));
    expect(marked(lines)).toEqual([" a", " b"]);
    expect(diffCounts(lines)).toEqual({ added: 0, removed: 0 });
  });

  it("finds a changed line among unchanged ones", () => {
    const lines = diffLines(
      text("port: 8317", "debug: false", "request-retry: 3"),
      text("port: 8317", "debug: true", "request-retry: 3"),
    );
    expect(marked(lines)).toEqual([
      " port: 8317",
      "-debug: false",
      "+debug: true",
      " request-retry: 3",
    ]);
    expect(lines[1]).toEqual({ kind: "removed", text: "debug: false", before: 2, after: null });
    expect(lines[2]).toEqual({ kind: "added", text: "debug: true", before: null, after: 2 });
    expect(lines[3]).toEqual({ kind: "same", text: "request-retry: 3", before: 3, after: 3 });
  });

  it("keeps the lines both share in the middle of a change", () => {
    const lines = diffLines(text("a", "b", "c", "d", "e"), text("a", "x", "c", "y", "e"));
    expect(marked(lines)).toEqual([" a", "-b", "+x", " c", "-d", "+y", " e"]);
  });

  it("numbers lines on each side through additions and removals", () => {
    const lines = diffLines(text("a", "b", "c"), text("z", "a", "c", "d"));
    expect(marked(lines)).toEqual(["+z", " a", "-b", " c", "+d"]);
    expect(lines.map((line) => [line.before, line.after])).toEqual([
      [null, 1],
      [1, 2],
      [2, null],
      [3, 3],
      [null, 4],
    ]);
    expect(diffCounts(lines)).toEqual({ added: 2, removed: 1 });
  });

  it("diffs from and to an empty text", () => {
    expect(marked(diffLines("", text("a", "b")))).toEqual(["+a", "+b"]);
    expect(marked(diffLines(text("a"), "")).join()).toBe("-a");
  });

  it("ignores line endings in the lines it compares", () => {
    const crlf = ["a", "b"].map((line) => line + CR + NL).join("");
    expect(marked(diffLines(crlf, text("a", "c")))).toEqual([" a", "-b", "+c"]);
  });

  it("shows a change too large to compare as a removal and an addition", () => {
    const before = Array.from({ length: 2500 }, (_, index) => `before ${String(index)}`);
    const after = Array.from({ length: 2500 }, (_, index) => `after ${String(index)}`);
    const lines = diffLines(text("head", ...before, "tail"), text("head", ...after, "tail"));
    expect(diffCounts(lines)).toEqual({ added: 2500, removed: 2500 });
    expect(lines[0]?.kind).toBe("same");
    expect(lines[1]?.text).toBe("before 0");
    expect(lines[2501]?.text).toBe("after 0");
    expect(lines.at(-1)).toEqual({ kind: "same", text: "tail", before: 2502, after: 2502 });
  });
});

describe("diffRows", () => {
  it("keeps three lines around each change and counts the rest as gaps", () => {
    const before = Array.from({ length: 20 }, (_, index) => `line ${String(index + 1)}`);
    const after = [...before];
    after[9] = "line ten";
    const rows = diffRows(diffLines(text(...before), text(...after)));
    expect(
      rows.map((row) => (row.kind === "gap" ? `(${String(row.count)} lines)` : marked([row.line])[0])),
    ).toEqual([
      "(6 lines)",
      " line 7",
      " line 8",
      " line 9",
      "-line 10",
      "+line ten",
      " line 11",
      " line 12",
      " line 13",
      "(7 lines)",
    ]);
  });

  it("joins changes whose context meets", () => {
    const rows = diffRows(diffLines(text("a", "b", "c", "d"), text("x", "b", "c", "y")), 1);
    expect(rows.every((row) => row.kind === "line")).toBe(true);
    expect(rows).toHaveLength(6);
  });

  it("shows no rows for equal texts but one gap", () => {
    expect(diffRows(diffLines(text("a", "b"), text("a", "b")))).toEqual([{ kind: "gap", count: 2 }]);
    expect(diffRows([])).toEqual([]);
  });
});
