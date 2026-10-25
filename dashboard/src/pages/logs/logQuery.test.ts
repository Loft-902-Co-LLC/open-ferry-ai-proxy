import { describe, expect, it } from "vitest";

import { logSearchForm, readLogQuery, searchParams, writeLogQuery } from "./logQuery";

describe("the request-log status filter", () => {
  it.each([
    "100",
    "299",
    "599",
    "1xx",
    "5XX",
    " 200 ",
    " 4XX ",
    "",
    "   ",
  ])("accepts %j and trims it", (status) => {
    const query = readLogQuery(new URLSearchParams());
    const parsed = logSearchForm.parse({ ...query, status });
    expect(parsed.status).toBe(status.trim());
  });

  it.each(["000", "099", "600", "999", "4x", "12", "1000", "letters", "0xx", "6xx"])(
    "refuses %j",
    (status) => {
      const query = readLogQuery(new URLSearchParams());
      const parsed = logSearchForm.safeParse({ ...query, status });
      expect(parsed.success).toBe(false);
      if (!parsed.success) {
        expect(parsed.error.issues[0]?.path).toEqual(["status"]);
      }
    },
  );

  it("normalizes padded uppercase classes for the search", () => {
    const query = readLogQuery(new URLSearchParams());
    const parsed = logSearchForm.parse({ ...query, status: " 5XX " });
    expect(searchParams({ ...query, ...parsed }, 0).status).toBe("5xx");
  });

  it.each(["000", "099", "600", "999"])(
    "keeps a bookmarked status of %j for the server to validate",
    (status) => {
      const query = readLogQuery(new URLSearchParams({ status }));
      expect(query.status).toBe(status);
      expect(writeLogQuery(query).get("status")).toBe(status);
      expect(searchParams(query, 0).status).toBe(status);
    },
  );
});
