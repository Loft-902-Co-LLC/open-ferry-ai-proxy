import { describe, expect, it } from "vitest";

import {
  applyKeyChanges,
  effectiveKeyChanges,
  keyChangeLabel,
  keyRows,
  withoutChange,
  type KeyChange,
} from "./keyChanges";

const A = "sk-test-client-key-aaaa-0001";
const B = "sk-test-client-key-bbbb-0002";
const C = "sk-test-client-key-cccc-0003";

const add = (key: string): KeyChange => ({ kind: "add", key });
const del = (key: string): KeyChange => ({ kind: "delete", key });

describe("effectiveKeyChanges", () => {
  it("puts new keys before deletions, so the list is never empty on the way", () => {
    expect(effectiveKeyChanges([A], [del(A), add(C)])).toEqual([add(C), del(A)]);
  });

  it("drops a new key the server has and a deletion of one it hasn't", () => {
    expect(effectiveKeyChanges([A, B], [add(B), del(C), add(C), add(C)])).toEqual([add(C)]);
  });

  it("deletes a key listed twice once for each deletion, and no more", () => {
    expect(effectiveKeyChanges([A, A], [del(A), del(A), del(A)])).toEqual([del(A), del(A)]);
  });
});

describe("keyRows", () => {
  it("marks the server's keys to delete, then lists the new ones", () => {
    expect(keyRows([A, B], [add(C), del(B)])).toEqual([
      { key: A, state: "saved" },
      { key: B, state: "deleted" },
      { key: C, state: "added" },
    ]);
  });

  it("marks only as many copies of a key as there are deletions", () => {
    expect(keyRows([A, A], [del(A)])).toEqual([
      { key: A, state: "deleted" },
      { key: A, state: "saved" },
    ]);
  });
});

describe("withoutChange", () => {
  it("undoes the last matching change only", () => {
    expect(withoutChange([del(A), add(C), del(A)], "delete", A)).toEqual([del(A), add(C)]);
    expect(withoutChange([add(C)], "delete", C)).toEqual([add(C)]);
  });
});

describe("applyKeyChanges", () => {
  it("gives the list as saving leaves it", () => {
    expect(applyKeyChanges([A, B], [add(C), del(A)])).toEqual([B, C]);
    expect(applyKeyChanges([A], [del(A)])).toEqual([]);
  });
});

describe("keyChangeLabel", () => {
  it("names a change with the key masked, or an example key in full", () => {
    expect(keyChangeLabel(add(A))).toBe("Add client key sk-...0001");
    expect(keyChangeLabel(del("your-api-key-1"))).toBe("Delete client key your-api-key-1");
  });
});
