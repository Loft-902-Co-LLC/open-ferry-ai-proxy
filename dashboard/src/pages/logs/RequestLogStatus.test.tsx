import { act, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { REQUEST_LOGS } from "../../api/dashboard";
import { logSearch } from "../../test/fixtures";
import { loadFirst } from "../../test/loadFirst";
import { mockApi, route } from "../../test/mockApi";
import { renderApp } from "../../test/renderApp";

loadFirst(() => import("./LogsPage"));

describe("the request-log status form", () => {
  it("refuses an out-of-range status without starting a new search", async () => {
    const api = mockApi(route("GET", REQUEST_LOGS, { json: logSearch([]) }));
    const { user, router } = renderApp("/logs");
    expect(await screen.findByText("No logs match.")).toBeVisible();
    const before = api.callsTo("GET", REQUEST_LOGS).length;
    const status = screen.getByRole("textbox", { name: "Status" });
    await user.type(status, "600");
    await act(async () => {
      await user.click(screen.getByRole("button", { name: "Search" }));
    });

    expect(api.callsTo("GET", REQUEST_LOGS)).toHaveLength(before);
    expect(
      await screen.findByText("Use a status from 100 to 599, or a class such as 4xx."),
    ).toBeVisible();
    expect(status).toHaveAttribute("aria-invalid", "true");
    expect(status).toHaveAccessibleDescription(/Use a status from 100 to 599/);
    expect(router.state.location.search).toBe("");

    await user.clear(status);
    await user.type(status, "599");
    await user.click(screen.getByRole("button", { name: "Search" }));
    await waitFor(() => {
      expect(api.callsTo("GET", REQUEST_LOGS)).toHaveLength(before + 1);
    });
    expect(api.callsTo("GET", REQUEST_LOGS).at(-1)?.url.searchParams.get("status")).toBe("599");
    expect(router.state.location.search).toBe("?status=599");
    expect(status).not.toHaveAttribute("aria-invalid", "true");
    expect(api.unhandled).toEqual([]);
  });

  it("keeps the server error for a bookmarked out-of-range status", async () => {
    const message = "status must be a status such as 502, or a class such as 5xx";
    const api = mockApi(
      route("GET", REQUEST_LOGS, { status: 400, json: { error: "invalid_request", message } }),
    );
    const { router } = renderApp("/logs?status=600");

    expect(await screen.findByText(message)).toBeVisible();
    expect(screen.getByRole("textbox", { name: "Status" })).toHaveValue("600");
    expect(api.callsTo("GET", REQUEST_LOGS)).toHaveLength(1);
    expect(api.callsTo("GET", REQUEST_LOGS)[0]?.url.searchParams.get("status")).toBe("600");
    expect(router.state.location.search).toBe("?status=600");
    expect(api.unhandled).toEqual([]);
  });
});
