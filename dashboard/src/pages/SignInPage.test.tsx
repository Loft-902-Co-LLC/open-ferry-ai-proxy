import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { readStoredKey } from "../api/keyStorage";
import { CHECK_PATH } from "../api/signIn";
import { mockApi, mockUnreachable, route, type MockReply } from "../test/mockApi";
import { renderApp } from "../test/renderApp";

const BUILD_HEADERS = {
  "x-cpa-version": "0.1.0",
  "x-cpa-commit": "abc1234",
  "x-cpa-build-date": "2026-10-05T00:00:00Z",
};

/** A server whose management key is "right-key". */
function serverWithKey(refusal?: MockReply) {
  return mockApi(
    route("GET", CHECK_PATH, (request) => {
      if (refusal !== undefined) {
        return refusal;
      }
      const auth = request.headers.get("authorization");
      if (auth === null) {
        return { status: 401, json: { error: "missing management key" } };
      }
      return auth === "Bearer right-key"
        ? { json: { "usage-statistics-enabled": true }, headers: BUILD_HEADERS }
        : { status: 401, json: { error: "invalid management key" } };
    }),
  );
}

async function submitKey(user: ReturnType<typeof renderApp>["user"], key: string) {
  const field = await screen.findByLabelText("Management key");
  await user.clear(field);
  await user.type(field, key);
  await user.click(screen.getByRole("button", { name: "Sign in" }));
}

describe("sign in", () => {
  it("sends a signed-out visit to sign in, then back", async () => {
    serverWithKey();
    const { user, router } = renderApp("/about", { key: null });
    expect(await screen.findByRole("heading", { name: "Sign in to the dashboard" })).toBeVisible();
    expect(document.title).toBe("Sign in · open-ferry");
    await submitKey(user, "right-key");
    expect(await screen.findByRole("heading", { name: "About" })).toBeVisible();
    expect(router.state.location.pathname).toBe("/about");
    expect(readStoredKey()).toBe("right-key");
  });

  it("asks for a key before calling the server", async () => {
    const api = serverWithKey();
    const { user } = renderApp("/signin", { key: null });
    await user.click(await screen.findByRole("button", { name: "Sign in" }));
    expect(await screen.findByText("Enter the management key.")).toBeVisible();
    expect(screen.getByLabelText("Management key")).toHaveAttribute("aria-invalid", "true");
    expect(api.calls).toHaveLength(0);
  });

  it("masks the key, with a button to show it", async () => {
    serverWithKey();
    const { user } = renderApp("/signin", { key: null });
    const field = await screen.findByLabelText("Management key");
    expect(field).toHaveAttribute("type", "password");
    await user.click(screen.getByRole("button", { name: "Show the key" }));
    expect(field).toHaveAttribute("type", "text");
  });

  it("explains a wrong key, and keeps it out of storage", async () => {
    serverWithKey();
    const { user } = renderApp("/signin", { key: null });
    await submitKey(user, "wrong-key");
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("That management key is wrong");
    expect(alert).toHaveTextContent("thirty minutes");
    // The local password is a key only from the server's own computer.
    expect(alert).toHaveTextContent(
      "The local password, from -password or the terminal UI's standalone mode, is taken only from the computer the server runs on.",
    );
    expect(readStoredKey()).toBeNull();
    // A refused candidate isn't a rejected session.
    expect(screen.queryByText("You were signed out")).toBeNull();
  });

  it("explains the remote-access rule", async () => {
    serverWithKey({ status: 403, json: { error: "remote management disabled" } });
    const { user } = renderApp("/signin", { key: null });
    await submitKey(user, "right-key");
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Remote management is off");
    expect(within(alert).getByText("management.allow-remote: true")).toBeVisible();
  });

  it("says how long a ban lasts", async () => {
    serverWithKey({
      status: 403,
      json: { error: "IP banned due to too many failed attempts. Try again in 29m12s" },
    });
    const { user } = renderApp("/signin", { key: null });
    await submitKey(user, "right-key");
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("This address is locked out");
    expect(alert).toHaveTextContent("Try again in 29m12s.");
  });

  it("explains a server with management switched off", async () => {
    serverWithKey({ status: 404 });
    const { user } = renderApp("/signin", { key: null });
    await submitKey(user, "right-key");
    const notice = await screen.findByText("Management is switched off on this server");
    const box = notice.closest("div[role]") ?? notice.parentElement;
    expect(box).not.toBeNull();
    expect(within(box as HTMLElement).getByText("management.secret-key")).toBeVisible();
    expect(within(box as HTMLElement).getByText("MANAGEMENT_PASSWORD")).toBeVisible();
    // A local password alone isn't a management key.
    expect(box).toHaveTextContent("doesn't turn management on by itself");
  });

  it("explains the management API's refusal while only a local password is set", async () => {
    serverWithKey({ status: 403, json: { error: "remote management key not set" } });
    const { user } = renderApp("/signin", { key: null });
    await submitKey(user, "local-password");
    expect(await screen.findByText("Management is switched off on this server")).toBeVisible();
  });

  it("names the local password as a way in", async () => {
    serverWithKey();
    renderApp("/signin", { key: null });
    const field = await screen.findByLabelText("Management key");
    expect(field).toHaveAccessibleDescription(/its local password works too/);
  });

  it("says where the key comes from, and what to do without it", async () => {
    serverWithKey();
    renderApp("/signin", { key: null });
    const field = await screen.findByLabelText("Management key");
    expect(field).toHaveAccessibleDescription(/secret-key under management: in the server.s config/);
    expect(field).toHaveAccessibleDescription(/open-ferry init prints the key/);
    expect(field).toHaveAccessibleDescription(/ask whoever runs the server/);
  });

  it("explains a server that doesn't answer", async () => {
    mockUnreachable();
    const { user } = renderApp("/signin", { key: null });
    await submitKey(user, "right-key");
    expect(await screen.findByRole("alert")).toHaveTextContent("The server didn't answer");
  });

  it("keeps the safe-mode request across sign-in", async () => {
    serverWithKey();
    const { user, router } = renderApp("/?safe-mode=configure", { key: null });
    expect(await screen.findByText("The proxy is in safe mode")).toBeVisible();
    await submitKey(user, "right-key");
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/");
    });
    expect(router.state.location.search).toBe("?safe-mode=configure");
  });

  it("opens the overview with safe mode when sent straight to sign in", async () => {
    serverWithKey();
    const { user, router } = renderApp("/signin?safe-mode=configure", { key: null });
    expect(await screen.findByText("The proxy is in safe mode")).toBeVisible();
    await submitKey(user, "right-key");
    await waitFor(() => {
      expect(router.state.location.search).toBe("?safe-mode=configure");
    });
    expect(router.state.location.pathname).toBe("/");
  });
});
