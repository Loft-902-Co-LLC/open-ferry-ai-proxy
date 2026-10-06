import { screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { readStoredKey } from "../api/keyStorage";
import { CHECK_PATH } from "../api/signIn";
import { mockApi, route } from "../test/mockApi";
import { TEST_KEY, renderApp } from "../test/renderApp";

describe("the signed-in app", () => {
  it("shows the shell with navigation and a skip link", async () => {
    mockApi(route("GET", CHECK_PATH, { json: { "usage-statistics-enabled": true } }));
    renderApp("/");
    expect(await screen.findByRole("heading", { name: "Overview" })).toBeVisible();
    expect(screen.getByRole("navigation", { name: "Main" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Overview" })).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("link", { name: "Skip to content" })).toHaveAttribute("href", "#main");
    expect(document.title).toBe("Overview · open-ferry");
  });

  it("signs out, forgetting the key", async () => {
    mockApi();
    const { user } = renderApp("/");
    await user.click(await screen.findByRole("button", { name: "Sign out" }));
    expect(await screen.findByText("You signed out.")).toBeVisible();
    expect(readStoredKey()).toBeNull();
  });

  it("signs out when the server stops taking the key", async () => {
    mockApi(route("GET", CHECK_PATH, { status: 401, json: { error: "invalid management key" } }));
    const { router } = renderApp("/about");
    expect(await screen.findByText("You were signed out")).toBeVisible();
    expect(router.state.location.pathname).toBe("/signin");
    expect(readStoredKey()).toBeNull();
  });

  it("shows the server's build on the About page, with the credits", async () => {
    const api = mockApi(
      route("GET", CHECK_PATH, {
        json: { "usage-statistics-enabled": false },
        headers: { "x-cpa-version": "0.1.0", "x-cpa-commit": "abc1234", "x-cpa-build-date": "today" },
      }),
    );
    renderApp("/about");
    expect(await screen.findByText("abc1234")).toBeVisible();
    expect(screen.getByText("0.1.0")).toBeVisible();
    expect(screen.getByRole("link", { name: "CLIProxyAPI" })).toHaveAttribute(
      "href",
      "https://github.com/router-for-me/CLIProxyAPI",
    );
    expect(screen.getByRole("link", { name: "third-party-licenses.txt" })).toHaveAttribute(
      "href",
      `${import.meta.env.BASE_URL}third-party-licenses.txt`,
    );
    expect(api.calls[0]?.headers.get("authorization")).toBe(`Bearer ${TEST_KEY}`);
  });

  it("has a page for unknown addresses", async () => {
    mockApi();
    renderApp("/no-such-page");
    expect(await screen.findByRole("heading", { name: "Page not found" })).toBeVisible();
  });

  it("never calls anything but the two APIs", async () => {
    const api = mockApi(route("GET", CHECK_PATH, { json: {} }));
    renderApp("/about");
    await waitFor(() => {
      expect(api.calls.length).toBeGreaterThan(0);
    });
    for (const call of api.calls) {
      expect(call.url.origin).toBe("http://127.0.0.1:4173");
      expect(call.url.pathname).toMatch(/^\/(v0\/management|v8\/management|open-ferry\/api\/v1)\//);
    }
  });
});
